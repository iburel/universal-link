// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

//! The clipboard's transactions: the capability at the heart of the pull-at-
//! paste model (doc/core-api.md, "Transactions").
//!
//! A **transaction** is the right to read a frozen set of resources, minted by
//! the source Core at the announce (`clipboard.updated`). The clipboard is its
//! first producer — a future shared folder will be a long-lived one. This
//! module owns the table (born / superseded / deleted) and the freezing of the
//! file manifest; serving the bytes lives in `datachannel`, and the source→peer
//! relay will come with the network plane.
//!
//! What travels: at the announce, only metadata (formats, and for files a
//! manifest of paths + sizes + identity). No byte is read here. Inline formats
//! (text, image/png) are re-read from the backend at paste time; files are
//! served by the Core from the disk, bounded to the frozen manifest.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::time::SystemTime;

use serde_json::{Value, json};

use crate::rpc::RpcErr;
use crate::state::ConnId;

/// v1 normalized formats (doc/core-api.md). The backend converts from/to the OS
/// formats; the Core only transports these.
const FORMATS: [&str; 3] = ["text", "image/png", "files"];

/// Upper bound on a v1 manifest: a runaway copy is refused at the announce
/// (`MANIFEST_TOO_LARGE`) rather than later killing a connection with an
/// oversized notification frame. Lazy enumeration (shared folders) will lift it.
const MANIFEST_MAX: usize = 65_536;

/// An offered format and its advisory size (bytes). For inline formats the size
/// is a hint — the content is re-serialized at paste time and the stream is
/// authoritative; for `files` the per-file sizes live in the manifest.
#[derive(Clone)]
pub struct Format {
    pub format: String,
    pub size: Option<u64>,
}

/// The frozen identity of a manifest file, captured at the announce and
/// re-checked at open: a file whose bytes may have changed underneath must fail
/// the read (`FILE_CHANGED`) rather than serve different content silently. Size
/// is compared separately (it is also reported in the manifest).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Identity {
    pub mtime: Option<SystemTime>,
    #[cfg(unix)]
    pub dev: u64,
    #[cfg(unix)]
    pub ino: u64,
}

/// A manifest entry: what the destination sees (`file_id`, relative `path`,
/// size, `dir`) plus what the source Core needs to serve it (the canonical
/// on-disk `source` and the frozen `identity`).
#[derive(Clone, Debug)]
pub struct FileEntry {
    pub file_id: String,
    /// Relative, `/`-separated, unique within the manifest — what
    /// `clipboard.remote_updated` carries and a destination joins onto its
    /// paste target.
    pub rel_path: String,
    /// Canonical absolute path on the local disk. Resolved once at the announce
    /// (symlinks followed); reads are bounded to it.
    pub source: PathBuf,
    pub size: u64,
    pub is_dir: bool,
    pub identity: Identity,
}

/// A transaction: a frozen offer, addressable by its unguessable `tx_id`.
pub struct Transaction {
    pub tx_id: String,
    /// The source device (own device on the announcing side); omitted when the
    /// Core is not logged in.
    pub device_id: Option<String>,
    pub formats: Vec<Format>,
    /// The file manifest (empty unless a `files` format was announced).
    pub files: Vec<FileEntry>,
    pub sensitive: bool,
    /// The control connection that announced it — where `clipboard.get_data` is
    /// addressed for inline formats. If it is gone, inline pulls fail
    /// `CLIP_STALE`; files keep being served from the disk regardless.
    pub announcer: ConnId,
    /// Superseded by a newer announce: refuses new sessions (`TX_STALE`), but a
    /// session already open runs to completion.
    pub superseded: bool,
    /// Open consumer channels reading it. A superseded transaction is deleted
    /// once this reaches zero.
    pub sessions: u32,
}

impl Transaction {
    /// The metadata view (`clipboard.current`, and the payload a destination is
    /// told about). Never exposes the on-disk `source` paths — only the
    /// relative manifest paths.
    pub fn record(&self) -> Value {
        let formats: Vec<Value> = self
            .formats
            .iter()
            .map(|f| {
                let mut v = json!({ "format": f.format });
                if let Some(size) = f.size {
                    v["size"] = json!(size);
                }
                v
            })
            .collect();
        let mut v = json!({ "tx_id": self.tx_id, "formats": formats });
        if let Some(device_id) = &self.device_id {
            v["device_id"] = json!(device_id);
        }
        if !self.files.is_empty() {
            let files: Vec<Value> = self
                .files
                .iter()
                .map(|f| {
                    let mut fv = json!({ "file_id": f.file_id, "path": f.rel_path, "size": f.size });
                    if f.is_dir {
                        fv["dir"] = json!(true);
                    }
                    fv
                })
                .collect();
            v["files"] = json!(files);
        }
        if self.sensitive {
            v["sensitive"] = json!(true);
        }
        v
    }
}

impl FileEntry {
    /// Re-`stat`s the source and confirms it is still the frozen file: same
    /// size AND same identity (mtime, and dev+inode on unix). Any change — a
    /// swap, a same-size rewrite, a vanished file, or an unreadable stat —
    /// fails: we never serve bytes we cannot vouch for (`FILE_CHANGED`).
    pub fn still_matches(&self) -> bool {
        let Ok(meta) = std::fs::metadata(&self.source) else {
            return false;
        };
        !meta.is_dir() && meta.len() == self.size && identity_of(&meta) == self.identity
    }
}

/// The clipboard state: the current global clip plus any superseded
/// transactions still draining active sessions. LEAF lock (see `state`): taken
/// alone, never across an await, never while holding another lock.
pub struct ClipboardState {
    /// `tx_id` of the current clip — the last announce. `None` before any.
    current: Option<String>,
    /// All live transactions, keyed by `tx_id`: the current one plus the
    /// superseded-but-still-read ones.
    transactions: HashMap<String, Transaction>,
}

impl ClipboardState {
    pub fn new() -> ClipboardState {
        ClipboardState {
            current: None,
            transactions: HashMap::new(),
        }
    }

    /// Opens `tx` and makes it the current clip, superseding the previous one
    /// (last copier wins). Returns the `tx_id`.
    pub fn announce(&mut self, tx: Transaction) -> String {
        let tx_id = tx.tx_id.clone();
        self.supersede_current();
        self.transactions.insert(tx_id.clone(), tx);
        self.current = Some(tx_id.clone());
        tx_id
    }

    /// Marks the current transaction superseded; drops it at once if no session
    /// reads it, otherwise leaves it alive until its last session ends (an
    /// in-flight paste is never cut by a new copy).
    fn supersede_current(&mut self) {
        if let Some(prev) = self.current.take()
            && let Some(t) = self.transactions.get_mut(&prev)
        {
            t.superseded = true;
            if t.sessions == 0 {
                self.transactions.remove(&prev);
            }
        }
    }

    /// The current clip's metadata, or `{}` if there is none — the
    /// `clipboard.current` snapshot.
    pub fn current_record(&self) -> Value {
        match self.current.as_ref().and_then(|id| self.transactions.get(id)) {
            Some(t) => t.record(),
            None => json!({}),
        }
    }

    /// Drops every transaction — the non-graceful exit (Core stop, logout, and
    /// later an explicit revocation): the right to read ends *now*. Active
    /// sessions are cut separately (a `TX_STALE` pushed on their channels); once
    /// the table is empty their next `READ`/`FETCH` resolves to gone anyway.
    pub fn clear_all(&mut self) {
        self.current = None;
        self.transactions.clear();
    }

    /// May a NEW session open on `tx_id`? It must exist and not be superseded —
    /// a superseded clip lets its live sessions finish but accepts no new one.
    pub fn is_openable(&self, tx_id: &str) -> bool {
        self.transactions
            .get(tx_id)
            .is_some_and(|t| !t.superseded)
    }

    /// Opens a session on `tx_id`, reserving it against deletion. `false` if the
    /// transaction is already gone (deleted between `transactions.open` and the
    /// channel attach). Accepts a superseded-but-alive transaction: the grant
    /// was minted while it was openable.
    pub fn start_session(&mut self, tx_id: &str) -> bool {
        match self.transactions.get_mut(tx_id) {
            Some(t) => {
                t.sessions += 1;
                true
            }
            None => false,
        }
    }

    /// Closes a session; deletes the transaction if it was superseded and this
    /// was its last reader.
    pub fn end_session(&mut self, tx_id: &str) {
        if let Some(t) = self.transactions.get_mut(tx_id) {
            t.sessions = t.sessions.saturating_sub(1);
            if t.superseded && t.sessions == 0 {
                self.transactions.remove(tx_id);
            }
        }
    }

    /// Resolves a `(tx_id, file_id)` for a `READ`, distinguishing a vanished
    /// transaction (`TX_STALE`, ends the session) from an unknown file id
    /// (`FILE_UNKNOWN`, request-scoped).
    pub fn lookup_file(&self, tx_id: &str, file_id: &str) -> Lookup {
        match self.transactions.get(tx_id) {
            None => Lookup::Gone,
            Some(t) => match t.files.iter().find(|f| f.file_id == file_id) {
                Some(f) => Lookup::File(f.clone()),
                None => Lookup::NoSuchFile,
            },
        }
    }
}

/// The outcome of resolving a manifest file for a data-channel `READ`.
pub enum Lookup {
    /// The transaction is gone — `TX_STALE`.
    Gone,
    /// No such `file_id` in the manifest — `FILE_UNKNOWN`.
    NoSuchFile,
    File(FileEntry),
}

/// Where to obtain an inline format for a `FETCH`: the connection that announced
/// the transaction (the only one that can re-read the OS clipboard).
pub enum InlineSource {
    /// The transaction is gone — `TX_STALE`.
    Gone,
    /// The format is not part of this transaction — `FORMAT_UNKNOWN`.
    NoFormat,
    /// Ask this connection (`clipboard.get_data`).
    Announcer(ConnId),
}

impl ClipboardState {
    /// Resolves an inline `FETCH`: the announcer to ask for `format`,
    /// distinguishing a vanished transaction from an absent format.
    pub fn inline_source(&self, tx_id: &str, format: &str) -> InlineSource {
        match self.transactions.get(tx_id) {
            None => InlineSource::Gone,
            Some(t) if t.formats.iter().any(|f| f.format == format) => {
                InlineSource::Announcer(t.announcer)
            }
            Some(_) => InlineSource::NoFormat,
        }
    }
}

/// Parses and validates the `formats` field of `clipboard.updated`: a required
/// array (possibly empty — an empty announce means the clipboard was cleared)
/// of `{ format, size? }` with `format` in the normalized set.
pub fn parse_formats(params: &Value) -> Result<Vec<Format>, RpcErr> {
    let items = params
        .get("formats")
        .and_then(Value::as_array)
        .ok_or_else(|| RpcErr::invalid_params("formats"))?;
    let mut formats = Vec::with_capacity(items.len());
    for item in items {
        let format = item
            .get("format")
            .and_then(Value::as_str)
            .filter(|f| FORMATS.contains(f))
            .ok_or_else(|| RpcErr::invalid_params("format"))?
            .to_string();
        let size = match item.get("size") {
            None | Some(Value::Null) => None,
            Some(v) => Some(v.as_u64().ok_or_else(|| RpcErr::invalid_params("size"))?),
        };
        formats.push(Format { format, size });
    }
    Ok(formats)
}

/// Freezes the file manifest from the backend-supplied `paths`: canonicalizes
/// each path, `stat`s it (no byte read), captures its identity, and assigns a
/// unique relative name. v1 "flat files": a directory is refused (`-32602`) —
/// tree freezing is a follow-up, the manifest shape (`dir`) already anticipates
/// it. Beyond `MANIFEST_MAX` entries, `MANIFEST_TOO_LARGE`.
pub fn freeze_manifest(paths: &[String]) -> Result<Vec<FileEntry>, RpcErr> {
    if paths.len() > MANIFEST_MAX {
        return Err(RpcErr::app("MANIFEST_TOO_LARGE"));
    }
    let mut used = HashSet::new();
    let mut files = Vec::with_capacity(paths.len());
    for (index, raw) in paths.iter().enumerate() {
        // Canonicalize first: it both resolves the real target (symlinks
        // followed once, here) and proves the path exists. The bytes are later
        // served strictly from this canonical path.
        let source = std::fs::canonicalize(raw)
            .map_err(|e| RpcErr::invalid_params(&format!("{raw} — {e}")))?;
        let meta = std::fs::metadata(&source)
            .map_err(|e| RpcErr::invalid_params(&format!("{raw} — {e}")))?;
        if meta.is_dir() {
            return Err(RpcErr::invalid_params(&format!(
                "folders are not supported yet (v1 files only): {raw}"
            )));
        }
        // The displayed name comes from the ORIGINAL path (what the user
        // copied), not the canonical target: a copied `link.txt` stays
        // `link.txt` even though we read its target's bytes.
        let base = safe_base_name(raw)
            .ok_or_else(|| RpcErr::invalid_params(&format!("path without a usable name: {raw}")))?;
        files.push(FileEntry {
            file_id: format!("f{index}"),
            rel_path: unique_rel(&mut used, &base),
            source,
            size: meta.len(),
            is_dir: false,
            identity: identity_of(&meta),
        });
    }
    Ok(files)
}

/// The identity to freeze: modification time everywhere, plus the (device,
/// inode) pair on unix — the strongest "same file" signal the OS offers.
fn identity_of(meta: &std::fs::Metadata) -> Identity {
    Identity {
        mtime: meta.modified().ok(),
        #[cfg(unix)]
        dev: std::os::unix::fs::MetadataExt::dev(meta),
        #[cfg(unix)]
        ino: std::os::unix::fs::MetadataExt::ino(meta),
    }
}

/// The basename of `raw`, refused if it carries any path structure — the same
/// OS-independent rule as the transfer receiver (`/`, `\`, `:`, control chars,
/// `.`/`..`). Reasoning on the raw string, never `Path::file_name`, whose
/// splitting diverges across platforms.
fn safe_base_name(raw: &str) -> Option<String> {
    let base = raw.rsplit(['/', '\\']).next().unwrap_or(raw);
    if base.is_empty() || base == "." || base == ".." {
        return None;
    }
    if base.chars().any(|c| matches!(c, ':' ) || c.is_control()) {
        return None;
    }
    Some(base.to_string())
}

/// A relative name unique within the manifest: `name` as-is if free, otherwise
/// suffixed " (n)" before the extension — same rule as the received-files
/// collision handling, applied in memory across the manifest.
fn unique_rel(used: &mut HashSet<String>, name: &str) -> String {
    if used.insert(name.to_string()) {
        return name.to_string();
    }
    let path = std::path::Path::new(name);
    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or(name);
    let ext = path.extension().and_then(|s| s.to_str());
    for n in 1..=9999 {
        let candidate = match ext {
            Some(ext) => format!("{stem} ({n}).{ext}"),
            None => format!("{stem} ({n})"),
        };
        if used.insert(candidate.clone()) {
            return candidate;
        }
    }
    // Implausible (10k collisions of one name): a random suffix rather than a
    // duplicate.
    let fallback = format!("{stem}-{}", rpc_random());
    used.insert(fallback.clone());
    fallback
}

fn rpc_random() -> String {
    crate::state::random_hex(4)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_formats_validates_the_normalized_set() {
        assert!(parse_formats(&json!({ "formats": [] })).unwrap().is_empty());
        let ok = parse_formats(&json!({ "formats": [
            { "format": "text" },
            { "format": "image/png", "size": 42 },
        ] }))
        .unwrap();
        assert_eq!(ok.len(), 2);
        assert_eq!(ok[1].size, Some(42));
        // Unknown format, missing field, bad size type.
        assert!(parse_formats(&json!({ "formats": [{ "format": "video/mp4" }] })).is_err());
        assert!(parse_formats(&json!({ "formats": [{}] })).is_err());
        assert!(parse_formats(&json!({})).is_err());
    }

    #[test]
    fn unique_rel_suffixes_collisions() {
        let mut used = HashSet::new();
        assert_eq!(unique_rel(&mut used, "a.txt"), "a.txt");
        assert_eq!(unique_rel(&mut used, "a.txt"), "a (1).txt");
        assert_eq!(unique_rel(&mut used, "a.txt"), "a (2).txt");
        assert_eq!(unique_rel(&mut used, "noext"), "noext");
        assert_eq!(unique_rel(&mut used, "noext"), "noext (1)");
    }

    #[test]
    fn safe_base_name_strips_dirs_and_refuses_structure() {
        assert_eq!(safe_base_name("/a/b/c.txt").as_deref(), Some("c.txt"));
        // A Windows path keeps only its last segment (both separators split).
        assert_eq!(safe_base_name(r"C:\dir\c.txt").as_deref(), Some("c.txt"));
        assert_eq!(safe_base_name("plain.txt").as_deref(), Some("plain.txt"));
        assert_eq!(safe_base_name(".."), None);
        assert_eq!(safe_base_name("a/.."), None);
        // Colon / control char in the resulting basename itself is refused
        // (ADS/drive, framing).
        assert_eq!(safe_base_name("stream:ads"), None);
        assert_eq!(safe_base_name("with\nnewline"), None);
    }

    #[test]
    fn freeze_manifest_freezes_flat_files() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.txt");
        let b = dir.path().join("a.txt.bak");
        std::fs::write(&a, b"hello").unwrap();
        std::fs::write(&b, b"backup!!").unwrap();
        let files = freeze_manifest(&[
            a.to_string_lossy().into_owned(),
            b.to_string_lossy().into_owned(),
        ])
        .unwrap();
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].rel_path, "a.txt");
        assert_eq!(files[0].size, 5);
        assert_eq!(files[1].size, 8);
        assert!(!files[0].is_dir);
    }

    #[test]
    fn freeze_manifest_refuses_a_directory() {
        let dir = tempfile::tempdir().unwrap();
        let err = freeze_manifest(&[dir.path().to_string_lossy().into_owned()]).unwrap_err();
        assert_eq!(err.code, -32602);
    }
}
