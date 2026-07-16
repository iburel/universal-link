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
//!
//! A transaction is either **local** (announced by a backend on this device —
//! inline pulled from it, files read from this disk) or **remote** (learned
//! from a peer's `clip_announce` — everything relayed to the source device over
//! the data plane, see `clipnet`). Which one is the transaction's `origin`; a
//! remote manifest carries no local backing, only the metadata a paste needs.

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
/// (`MANIFEST_TOO_LARGE`) rather than freezing an unbounded manifest in memory.
/// It bounds the LOCAL offer and the IPC notification (16 MiB); it does NOT by
/// itself fit the network `clip_announce` frame (`dataplane::MAX_FRAME`, 1 MiB —
/// a few tens of thousands of entries), so a very large clip still serves
/// locally and to a local paste but fails to PROPAGATE (best-effort, logged) —
/// a known v1 limit that lazy enumeration (shared folders) will lift.
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

/// What a source Core needs to serve a manifest file's bytes from its own disk:
/// the canonical path and the frozen identity. Absent on a remote transaction
/// (the bytes live on the source device; reads are relayed to it, never served
/// from here) — its presence is exactly "this file is local".
#[derive(Clone, Debug)]
pub struct LocalBacking {
    /// Canonical absolute path on the local disk. Resolved once at the announce
    /// (symlinks followed); reads are bounded to it.
    pub source: PathBuf,
    pub identity: Identity,
}

/// A manifest entry: what the destination sees (`file_id`, relative `path`,
/// size, `dir`) plus, for a local file, the `backing` the Core reads from.
#[derive(Clone, Debug)]
pub struct FileEntry {
    pub file_id: String,
    /// Relative, `/`-separated, unique within the manifest — what
    /// `clipboard.remote_updated` carries and a destination joins onto its
    /// paste target.
    pub rel_path: String,
    pub size: u64,
    pub is_dir: bool,
    /// `Some` on the source device (bytes served from disk), `None` on a
    /// destination that learned the manifest over the network.
    pub backing: Option<LocalBacking>,
}

/// Where a transaction's bytes come from — and therefore how the Core serves a
/// paste of it.
#[derive(Clone, Debug)]
pub enum Origin {
    /// Announced by a backend on this device: inline formats are pulled from
    /// `announcer` (`clipboard.get_data`), files are read from the local disk.
    Local { announcer: ConnId },
    /// Learned from a peer's `clip_announce`: every paste is relayed to the
    /// source device over the data plane (a `clip_session` stream). `node_id`
    /// authenticates the source (iroh identity); `device_id` reaches it through
    /// the directory (`resolve_peer`).
    Remote { node_id: String, device_id: String },
}

/// A transaction: a frozen offer, addressable by its unguessable `tx_id`.
pub struct Transaction {
    pub tx_id: String,
    /// The source device (own device on the announcing side, the peer's on a
    /// remote clip); omitted when a local Core is not logged in.
    pub device_id: Option<String>,
    /// Ordering key for the global last-copier-wins: a best-effort millisecond
    /// timestamp, floored above the current clip's so a fresh local copy always
    /// wins locally. Every Core elects the highest `(seq, device_id)`.
    pub seq: u64,
    pub formats: Vec<Format>,
    /// The file manifest (empty unless a `files` format was announced).
    pub files: Vec<FileEntry>,
    pub sensitive: bool,
    /// Where the bytes live and how a paste reaches them.
    pub origin: Origin,
    /// Superseded by a newer announce: refuses new sessions (`TX_STALE`), but a
    /// session already open runs to completion.
    pub superseded: bool,
    /// Open consumer channels / fills reading it. A superseded transaction is
    /// deleted once this reaches zero.
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
                    let mut fv =
                        json!({ "file_id": f.file_id, "path": f.rel_path, "size": f.size });
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
    /// fails: we never serve bytes we cannot vouch for (`FILE_CHANGED`). A
    /// remote entry (no backing) never matches: its bytes are not served from
    /// here.
    pub fn still_matches(&self) -> bool {
        let Some(backing) = &self.backing else {
            return false;
        };
        let Ok(meta) = std::fs::metadata(&backing.source) else {
            return false;
        };
        !meta.is_dir() && meta.len() == self.size && identity_of(&meta) == backing.identity
    }

    /// The on-disk path to read this file's bytes from, if it is local.
    pub fn source(&self) -> Option<&std::path::Path> {
        self.backing.as_ref().map(|b| b.source.as_path())
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

    /// Opens a LOCAL `tx` and makes it the current clip, superseding the
    /// previous one — a fresh local copy always wins on its own device. Assigns
    /// its ordering `seq`: `now_ms`, floored above the current clip's so it also
    /// wins the global election even against a peer with a fast clock. Returns
    /// the `tx_id`.
    pub fn announce_local(&mut self, mut tx: Transaction, now_ms: u64) -> String {
        tx.seq = now_ms.max(self.current_seq().saturating_add(1));
        let tx_id = tx.tx_id.clone();
        self.supersede_current();
        self.transactions.insert(tx_id.clone(), tx);
        self.current = Some(tx_id.clone());
        tx_id
    }

    /// Adopts a REMOTE `tx` learned from a peer, but ONLY if it is strictly
    /// newer than the current clip by `(seq, device_id)` — the global
    /// last-copier-wins. Returns `Some(tx_id)` when adopted (the caller then
    /// notifies the local backend), `None` when the announce is stale or a
    /// duplicate (ignored, never made current). Every Core applying the same
    /// total order converges on the same winner.
    pub fn announce_remote(&mut self, tx: Transaction) -> Option<String> {
        // Every announce mints a FRESH unguessable `tx_id`; a remote one reusing
        // an id already in the table is a duplicate delivery or a collision from
        // a misbehaving account device — refuse it rather than overwrite a live
        // transaction (which could be draining a paste of its own).
        if self.transactions.contains_key(&tx.tx_id) {
            return None;
        }
        if let Some(cur) = self
            .current
            .as_ref()
            .and_then(|id| self.transactions.get(id))
            && (tx.seq, &tx.device_id) <= (cur.seq, &cur.device_id)
        {
            return None;
        }
        let tx_id = tx.tx_id.clone();
        self.supersede_current();
        self.transactions.insert(tx_id.clone(), tx);
        self.current = Some(tx_id.clone());
        Some(tx_id)
    }

    /// The `seq` of the current clip (0 if none) — the floor for the next local
    /// announce.
    fn current_seq(&self) -> u64 {
        self.current
            .as_ref()
            .and_then(|id| self.transactions.get(id))
            .map_or(0, |t| t.seq)
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
        match self
            .current
            .as_ref()
            .and_then(|id| self.transactions.get(id))
        {
            Some(t) => t.record(),
            None => json!({}),
        }
    }

    /// The `clip_announce` payload for a transaction: its metadata record plus
    /// the ordering `seq` (the network-internal field peers converge on). Used
    /// right after `announce_local` to broadcast the copy.
    pub fn network_announce_of(&self, tx_id: &str) -> Option<Value> {
        let t = self.transactions.get(tx_id)?;
        let mut v = t.record();
        v["seq"] = json!(t.seq);
        Some(v)
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
        self.transactions.get(tx_id).is_some_and(|t| !t.superseded)
    }

    /// The origin of `tx_id`, if it exists — to decide, at `transactions.open`,
    /// whether reaching the source device is even possible (`DEVICE_OFFLINE`).
    pub fn origin_of(&self, tx_id: &str) -> Option<Origin> {
        self.transactions.get(tx_id).map(|t| t.origin.clone())
    }

    /// Opens a session on `tx_id`, reserving it against deletion, and returns
    /// its `origin` (how to serve the paste). `None` if the transaction is
    /// already gone (deleted between `transactions.open` and the channel
    /// attach). Accepts a superseded-but-alive transaction: the grant was minted
    /// while it was openable, and an in-flight paste runs to completion.
    pub fn begin_session(&mut self, tx_id: &str) -> Option<Origin> {
        let t = self.transactions.get_mut(tx_id)?;
        t.sessions += 1;
        Some(t.origin.clone())
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

/// A resolved `transactions.fill`: the files to write, checked against the
/// frozen manifest, plus the total and the source device (for `transfer.*`).
pub struct FillPlan {
    pub items: Vec<FillItem>,
    pub total: u64,
    pub device_id: Option<String>,
}

/// One target of a fill: the manifest `file_id` to read, its `name` (relative
/// manifest path, for the `transfer.started` event), `size`, and the
/// backend-chosen `dest_path` the Core writes to.
pub struct FillItem {
    pub file_id: String,
    pub name: String,
    pub size: u64,
    pub dest_path: PathBuf,
}

impl ClipboardState {
    /// Validates a `transactions.fill` against the current manifest: the
    /// transaction must be openable (a superseded clip accepts no new session,
    /// `TX_STALE`), and every `file_id` must be a non-`dir` manifest entry
    /// (`-32602` otherwise — a directory has no bytes to fill). Returns the plan
    /// to run; the background task then reserves the session with
    /// `begin_session`.
    pub fn fill_plan(&self, tx_id: &str, entries: &[(String, String)]) -> Result<FillPlan, RpcErr> {
        let t = self
            .transactions
            .get(tx_id)
            .filter(|t| !t.superseded)
            .ok_or_else(|| RpcErr::app("TX_STALE"))?;
        let mut items = Vec::with_capacity(entries.len());
        let mut total = 0u64;
        for (file_id, dest_path) in entries {
            let fe = t
                .files
                .iter()
                .find(|f| &f.file_id == file_id)
                .filter(|f| !f.is_dir)
                .ok_or_else(|| RpcErr::invalid_params(&format!("file_id: {file_id}")))?;
            total = total.saturating_add(fe.size);
            items.push(FillItem {
                file_id: fe.file_id.clone(),
                name: fe.rel_path.clone(),
                size: fe.size,
                dest_path: PathBuf::from(dest_path),
            });
        }
        Ok(FillPlan {
            items,
            total,
            device_id: t.device_id.clone(),
        })
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
    /// distinguishing a vanished transaction from an absent format. Only
    /// meaningful for a LOCAL transaction — a remote clip's inline pulls are
    /// relayed to the source device by the consumer pipe, never resolved here
    /// (a remote origin therefore reads as `Gone`, defensively).
    pub fn inline_source(&self, tx_id: &str, format: &str) -> InlineSource {
        match self.transactions.get(tx_id) {
            None => InlineSource::Gone,
            Some(t) if !t.formats.iter().any(|f| f.format == format) => InlineSource::NoFormat,
            Some(t) => match &t.origin {
                Origin::Local { announcer } => InlineSource::Announcer(*announcer),
                Origin::Remote { .. } => InlineSource::Gone,
            },
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
            size: meta.len(),
            is_dir: false,
            backing: Some(LocalBacking {
                identity: identity_of(&meta),
                source,
            }),
        });
    }
    Ok(files)
}

/// Re-validates a manifest received over the network (`clip_announce`) and
/// rebuilds it as a set of backing-less entries. Fail-closed, exactly like the
/// transfer receiver: a relative `/`-separated `path` only — no `..`, no rooted
/// or absolute segment, no `\`, no `:` or control character, no duplicate — so
/// a backend that naively joins `path` onto its paste target cannot be turned
/// into a confused deputy by a compromised peer. Returns `None` (drop the
/// announce) on any violation. `MANIFEST_MAX` bounds the count here too.
pub fn validate_remote_manifest(files: &[Value]) -> Option<Vec<FileEntry>> {
    if files.len() > MANIFEST_MAX {
        return None;
    }
    let mut used = HashSet::new();
    let mut out = Vec::with_capacity(files.len());
    for f in files {
        let file_id = f.get("file_id").and_then(Value::as_str)?;
        let rel_path = f.get("path").and_then(Value::as_str)?;
        let size = f.get("size").and_then(Value::as_u64)?;
        let is_dir = match f.get("dir") {
            None | Some(Value::Null) => false,
            Some(Value::Bool(b)) => *b,
            Some(_) => return None,
        };
        if !is_safe_rel_path(rel_path) || !used.insert(rel_path.to_string()) {
            return None;
        }
        out.push(FileEntry {
            file_id: file_id.to_string(),
            rel_path: rel_path.to_string(),
            size,
            is_dir,
            backing: None,
        });
    }
    Some(out)
}

/// A manifest `path` a destination may safely join onto its paste target:
/// relative, `/`-separated, no traversal. Reasoning on the raw string (never
/// `Path`, whose splitting diverges across platforms) — the same OS-independent
/// discipline as `safe_base_name` and the transfer receiver.
fn is_safe_rel_path(raw: &str) -> bool {
    if raw.is_empty() || raw.starts_with('/') {
        return false;
    }
    // `\` is never a separator on the wire (paths are `/`-separated); a
    // backslash, colon, or control character is refused outright, as is any
    // `.`/`..`/empty segment.
    if raw
        .chars()
        .any(|c| matches!(c, '\\' | ':') || c.is_control())
    {
        return false;
    }
    raw.split('/')
        .all(|seg| !seg.is_empty() && seg != "." && seg != "..")
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
    if base.chars().any(|c| matches!(c, ':') || c.is_control()) {
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

    #[test]
    fn validate_remote_manifest_accepts_relative_paths() {
        let files = json!([
            { "file_id": "f0", "path": "a.txt", "size": 3 },
            { "file_id": "f1", "path": "sub/b.txt", "size": 5, "dir": false },
            { "file_id": "f2", "path": "sub", "size": 0, "dir": true },
        ]);
        let out = validate_remote_manifest(files.as_array().unwrap()).expect("valid manifest");
        assert_eq!(out.len(), 3);
        assert_eq!(out[1].rel_path, "sub/b.txt");
        assert!(out[2].is_dir);
        // A remote manifest carries no local backing.
        assert!(out.iter().all(|f| f.backing.is_none()));
    }

    #[test]
    fn validate_remote_manifest_refuses_traversal_and_duplicates() {
        // Absolute, `..`, backslash, colon, control char, empty segment.
        for bad in [
            "/etc/passwd",
            "../escape",
            "a/../b",
            "a/./b",
            r"a\b",
            "c:evil",
            "with\nnewline",
            "a//b",
            "",
        ] {
            let files = json!([{ "file_id": "f0", "path": bad, "size": 1 }]);
            assert!(
                validate_remote_manifest(files.as_array().unwrap()).is_none(),
                "must refuse {bad:?}"
            );
        }
        // Duplicate relative paths.
        let dup = json!([
            { "file_id": "f0", "path": "same.txt", "size": 1 },
            { "file_id": "f1", "path": "same.txt", "size": 1 },
        ]);
        assert!(validate_remote_manifest(dup.as_array().unwrap()).is_none());
        // A missing field.
        let missing = json!([{ "file_id": "f0", "size": 1 }]);
        assert!(validate_remote_manifest(missing.as_array().unwrap()).is_none());
    }
}
