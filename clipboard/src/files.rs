// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

//! Pure file-copy helpers shared by the Linux backend: the exact clipboard byte
//! formats file managers parse (`text/uri-list`, `x-special/gnome-copied-files`,
//! `x-special/KDE-copied-files`), and the manifest→inode tree the FUSE mount
//! walks. Everything here is X-free and FUSE-free, so it compiles and unit-tests
//! everywhere the crate builds on Linux (no `/dev/fuse`, no X server).
//!
//! The byte formats are load-bearing: a wrong separator or a missing trailing
//! CRLF makes a paste silently drop files, so they are ported verbatim from a
//! private POC and pinned by exact-byte tests. File *contents* never travel
//! through these targets — only `file://` URIs pointing into the FUSE mount do.

use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::fmt::Write as _;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};

use crate::backend::RemoteFile;

/// The FUSE root inode. Equals `fuser::FUSE_ROOT_ID`; kept as a plain constant so
/// this module never has to pull FUSE in (it is unit-tested in CI without it).
pub const ROOT_INO: u64 = 1;

// ---------------------------------------------------------------------------
// Percent-encoding (RFC 3986) for `file://` URIs.
// ---------------------------------------------------------------------------

/// Percent-encode raw bytes for a `file://` URI: keep the RFC 3986 unreserved
/// set (`A-Z a-z 0-9 - . _ ~`) plus the path separator `/` literal; everything
/// else (space, `%`, `:`, non-ASCII…) becomes `%XX` with uppercase hex. The path
/// is already split into components, so keeping `/` literal is safe and readable.
pub fn percent_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len());
    for &b in bytes {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~' | b'/') {
            out.push(b as char);
        } else {
            let _ = write!(out, "%{b:02X}");
        }
    }
    out
}

/// Decode percent-encoding to raw bytes. Tolerant: a malformed or truncated
/// `%XX` sequence is left literal (these bytes come from other applications,
/// which are sometimes imperfect).
pub fn percent_decode(s: &str) -> Vec<u8> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%'
            && i + 2 < b.len()
            && let (Some(hi), Some(lo)) = (
                (b[i + 1] as char).to_digit(16),
                (b[i + 2] as char).to_digit(16),
            )
        {
            out.push((hi * 16 + lo) as u8);
            i += 3;
            continue;
        }
        out.push(b[i]);
        i += 1;
    }
    out
}

/// The `file://` URI of an absolute path (empty authority): an absolute path
/// starts with `/`, so the result is `file:///encoded/path`.
pub fn file_uri(path: &Path) -> String {
    format!("file://{}", percent_encode(path.as_os_str().as_bytes()))
}

/// Parse one `text/uri-list` / `x-special/…` line into a local path. `None` for
/// an empty line, a `#` comment, or any line that is not a `file://` URI with an
/// absolute path (which naturally drops the `copy`/`cut` first line of the
/// GNOME/KDE formats, and any non-`file` URI). The authority (host) between
/// `file://` and the first `/` is ignored.
pub fn parse_file_uri(line: &str) -> Option<PathBuf> {
    let line = line.trim();
    if line.is_empty() || line.starts_with('#') {
        return None;
    }
    let rest = line.strip_prefix("file://")?;
    // The authority (host) sits between `file://` and the first `/`; skip it.
    let idx = rest.find('/')?;
    let bytes = percent_decode(&rest[idx..]);
    // Defensive: a decoded path we would hand to the OS must be absolute and
    // free of a NUL (never representable as a filename). Reject otherwise.
    if !bytes.starts_with(b"/") || bytes.contains(&0) {
        return None;
    }
    Some(PathBuf::from(OsString::from_vec(bytes)))
}

/// Parse a whole URI-list blob into local paths. Splits on `\n` (tolerating a
/// trailing `\r`), parses each line, and keeps the `file://` ones. Works for both
/// `text/uri-list` (CRLF) and `x-special/gnome-copied-files` (LF, first line
/// `copy`/`cut` — dropped by [`parse_file_uri`]).
pub fn parse_uri_list(raw: &[u8]) -> Vec<PathBuf> {
    let text = String::from_utf8_lossy(raw);
    text.split('\n').filter_map(parse_file_uri).collect()
}

/// Bytes of a `text/uri-list` target (RFC 2483): each URI on its own line,
/// terminated by CRLF — including a trailing CRLF after the last URI.
pub fn uri_list_bytes(uris: &[String]) -> Vec<u8> {
    let mut out = Vec::new();
    for u in uris {
        out.extend_from_slice(u.as_bytes());
        out.extend_from_slice(b"\r\n");
    }
    out
}

/// Bytes of an `x-special/gnome-copied-files` / `x-special/KDE-copied-files`
/// target: the literal first line `copy` (never `cut` — we never delete the
/// source, least of all on another machine), then each URI on its own line
/// joined by `\n`, with NO trailing `\n`. e.g. `copy\nfile:///a\nfile:///b`.
pub fn copied_files_bytes(uris: &[String]) -> Vec<u8> {
    let mut s = String::from("copy");
    for u in uris {
        s.push('\n');
        s.push_str(u);
    }
    s.into_bytes()
}

// ---------------------------------------------------------------------------
// Manifest → inode tree for the FUSE mount.
// ---------------------------------------------------------------------------

/// The kind of a tree node.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NodeKind {
    Dir,
    File,
}

/// One node of the exposed tree, addressed by FUSE inode.
struct Node {
    /// Parent inode (the root's parent is itself), used for `..` in `readdir`.
    parent: u64,
    kind: NodeKind,
    /// Children `(name, inode)` in insertion order — empty for a file.
    children: Vec<(String, u64)>,
    /// Manifest `file_id` — empty for a directory.
    file_id: String,
    /// Declared size (files only; 0 for a directory).
    size: u64,
}

/// The inode tree of a remote FILES clip, synthesized from the frozen manifest.
/// Immutable once built (a new clip builds a fresh tree). Malformed manifest
/// entries are skipped, never panicked on.
pub struct FileTree {
    nodes: HashMap<u64, Node>,
    /// Top-level element names (first path component of each entry), ordered and
    /// de-duplicated — these become the clipboard `file://` URIs.
    roots: Vec<String>,
}

/// Whether a manifest path is safe to place in the tree. The receiving Core has
/// already re-validated it, but a files backend must still never join an
/// absolute path, a `..`/`.` component, or a `:`-bearing component (defensive:
/// treat it as ignorable). `\0` cannot be a filename either.
fn path_is_malformed(path: &str, comps: &[&str]) -> bool {
    path.starts_with('/')
        || comps
            .iter()
            .any(|c| *c == ".." || *c == "." || c.contains(':') || c.contains('\0'))
}

impl FileTree {
    /// Build the tree from a manifest. Intermediate directories implied by a
    /// `path` are synthesized and shared; a `dir:true` entry becomes a directory
    /// node; malformed and prefix-colliding entries are skipped (an existing
    /// inode always wins — a file inode never grows children).
    pub fn build(files: &[RemoteFile]) -> FileTree {
        let mut nodes: HashMap<u64, Node> = HashMap::new();
        nodes.insert(
            ROOT_INO,
            Node {
                parent: ROOT_INO,
                kind: NodeKind::Dir,
                children: Vec::new(),
                file_id: String::new(),
                size: 0,
            },
        );
        // Cumulative relative path → inode, to share intermediate directories.
        let mut path_to_ino: HashMap<String, u64> = HashMap::new();
        path_to_ino.insert(String::new(), ROOT_INO);
        let mut next_ino: u64 = ROOT_INO + 1;
        let mut roots: Vec<String> = Vec::new();

        for entry in files {
            let comps: Vec<&str> = entry.path.split('/').filter(|c| !c.is_empty()).collect();
            if comps.is_empty() || path_is_malformed(&entry.path, &comps) {
                continue;
            }
            let mut cum = String::new();
            let mut parent = ROOT_INO;
            let last = comps.len() - 1;
            for (depth, comp) in comps.iter().enumerate() {
                let is_leaf = depth == last;
                if !cum.is_empty() {
                    cum.push('/');
                }
                cum.push_str(comp);
                if let Some(&existing) = path_to_ino.get(&cum) {
                    // Path already created. Normal: a shared intermediate dir, or
                    // an explicit dir entry coinciding with an implied one. A
                    // prefix collision (same path is both file and dir — a
                    // malformed manifest) keeps the existing node: never an
                    // inconsistent tree (a file with children), never a panic.
                    let existing_is_dir =
                        matches!(nodes.get(&existing), Some(n) if n.kind == NodeKind::Dir);
                    if is_leaf {
                        continue;
                    }
                    if !existing_is_dir {
                        break; // an intermediate component names a file: cannot descend.
                    }
                    parent = existing;
                    continue;
                }
                let ino = next_ino;
                next_ino += 1;
                let (kind, file_id, size) = if is_leaf && !entry.dir {
                    (NodeKind::File, entry.file_id.clone(), entry.size)
                } else {
                    // An intermediate directory, or an explicit `dir:true` entry.
                    (NodeKind::Dir, String::new(), 0)
                };
                nodes.insert(
                    ino,
                    Node {
                        parent,
                        kind,
                        children: Vec::new(),
                        file_id,
                        size,
                    },
                );
                if let Some(p) = nodes.get_mut(&parent) {
                    p.children.push(((*comp).to_string(), ino));
                }
                path_to_ino.insert(cum.clone(), ino);
                if depth == 0 && !roots.iter().any(|r| r == comp) {
                    roots.push((*comp).to_string());
                }
                parent = ino;
            }
        }

        FileTree { nodes, roots }
    }

    /// Top-level element names, ordered and de-duplicated.
    pub fn roots(&self) -> &[String] {
        &self.roots
    }

    /// `(kind, size)` of an inode, or `None` if it does not exist.
    pub fn attr(&self, ino: u64) -> Option<(NodeKind, u64)> {
        self.nodes.get(&ino).map(|n| (n.kind, n.size))
    }

    /// The parent inode of `ino` (for `..`), or `None` if it does not exist.
    pub fn parent(&self, ino: u64) -> Option<u64> {
        self.nodes.get(&ino).map(|n| n.parent)
    }

    /// The `(name, inode)` children of a directory, or `None` if `ino` is not a
    /// directory.
    pub fn children(&self, ino: u64) -> Option<&[(String, u64)]> {
        match self.nodes.get(&ino) {
            Some(n) if n.kind == NodeKind::Dir => Some(&n.children),
            _ => None,
        }
    }

    /// The inode of `name` under directory `parent`, or `None`.
    pub fn lookup(&self, parent: u64, name: &OsStr) -> Option<u64> {
        self.children(parent)?
            .iter()
            .find(|(n, _)| OsStr::new(n) == name)
            .map(|(_, ino)| *ino)
    }

    /// The manifest `file_id` of an inode, `Some` only for a file node.
    pub fn file_id(&self, ino: u64) -> Option<&str> {
        match self.nodes.get(&ino) {
            Some(n) if n.kind == NodeKind::File => Some(&n.file_id),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(file_id: &str, path: &str, size: u64) -> RemoteFile {
        RemoteFile {
            file_id: file_id.into(),
            path: path.into(),
            size,
            dir: false,
        }
    }

    fn dir(path: &str) -> RemoteFile {
        RemoteFile {
            file_id: String::new(),
            path: path.into(),
            size: 0,
            dir: true,
        }
    }

    #[test]
    fn percent_round_trips_space_and_multibyte() {
        for s in ["/a b/c", "/tmp/café.md", "/plain/path", "/100%/x:y"] {
            let enc = percent_encode(s.as_bytes());
            assert_eq!(percent_decode(&enc), s.as_bytes(), "round-trip {s}");
        }
        assert_eq!(
            file_uri(Path::new("/home/u/a b.txt")),
            "file:///home/u/a%20b.txt"
        );
        // `café` encodes byte-by-byte; `/` and `.` stay literal.
        assert_eq!(
            file_uri(Path::new("/tmp/café.md")),
            "file:///tmp/caf%C3%A9.md"
        );
    }

    #[test]
    fn parse_strips_authority_and_tolerates_comments() {
        assert_eq!(parse_file_uri("file://host/x"), Some(PathBuf::from("/x")));
        assert_eq!(parse_file_uri("file:///x"), Some(PathBuf::from("/x")));
        assert_eq!(parse_file_uri("  #comment"), None);
        assert_eq!(parse_file_uri(""), None);
        assert_eq!(parse_file_uri("http://x/y"), None);
        // A non-absolute decoded path is rejected.
        assert_eq!(parse_file_uri("file://"), None);
    }

    #[test]
    fn parse_uri_list_handles_crlf_comments_and_nonfile() {
        let blob = b"#comment\r\nfile:///home/u/a%20b.txt\r\nhttp://x/y\r\nfile:///x\r\n";
        assert_eq!(
            parse_uri_list(blob),
            vec![PathBuf::from("/home/u/a b.txt"), PathBuf::from("/x")]
        );
    }

    #[test]
    fn parse_uri_list_of_a_gnome_copied_blob() {
        // First line `copy`, `\n` separators, no CRLF, no trailing newline.
        let blob = b"copy\nfile:///a\nfile:///b%20c";
        assert_eq!(
            parse_uri_list(blob),
            vec![PathBuf::from("/a"), PathBuf::from("/b c")]
        );
    }

    #[test]
    fn uri_list_bytes_are_crlf_terminated() {
        let uris = vec!["file:///a".to_string(), "file:///b".to_string()];
        assert_eq!(uri_list_bytes(&uris), b"file:///a\r\nfile:///b\r\n");
        assert!(uri_list_bytes(&uris).ends_with(b"\r\n"));
    }

    #[test]
    fn copied_files_bytes_are_copy_prefixed_no_trailing_newline() {
        let uris = vec!["file:///a".to_string(), "file:///b".to_string()];
        let bytes = copied_files_bytes(&uris);
        assert_eq!(bytes, b"copy\nfile:///a\nfile:///b");
        assert!(bytes.starts_with(b"copy\n"));
        assert!(!bytes.ends_with(b"\n"));
    }

    #[test]
    fn tree_from_a_nested_manifest() {
        // `a/b.txt` (file), `a/c/` (explicit dir), `d.txt` (top-level file), plus
        // malformed entries that must be ignored.
        let files = vec![
            file("f0", "a/b.txt", 7),
            dir("a/c"),
            file("f1", "d.txt", 3),
            file("bad-abs", "/etc/passwd", 1),
            file("bad-dotdot", "a/../escape", 1),
            file("bad-colon", "c:/win", 1),
            file("bad-empty", "", 1),
        ];
        let tree = FileTree::build(&files);

        // Roots: first path component of each kept entry, de-duplicated & ordered.
        assert_eq!(tree.roots(), &["a".to_string(), "d.txt".to_string()]);

        // `a` is a synthesized directory reachable from the root.
        let a = tree.lookup(ROOT_INO, OsStr::new("a")).expect("a exists");
        assert_eq!(tree.attr(a).map(|(k, _)| k), Some(NodeKind::Dir));

        // `a/b.txt` is a file with the right size and file_id.
        let b = tree.lookup(a, OsStr::new("b.txt")).expect("a/b.txt exists");
        assert_eq!(tree.attr(b), Some((NodeKind::File, 7)));
        assert_eq!(tree.file_id(b), Some("f0"));

        // `a/c` is the synthesized/explicit directory.
        let c = tree.lookup(a, OsStr::new("c")).expect("a/c exists");
        assert_eq!(tree.attr(c).map(|(k, _)| k), Some(NodeKind::Dir));
        assert_eq!(tree.parent(c), Some(a));

        // `d.txt` is a top-level file.
        let d = tree
            .lookup(ROOT_INO, OsStr::new("d.txt"))
            .expect("d.txt exists");
        assert_eq!(tree.attr(d), Some((NodeKind::File, 3)));

        // None of the malformed entries leaked into the tree.
        assert!(tree.lookup(ROOT_INO, OsStr::new("etc")).is_none());
        assert!(tree.lookup(ROOT_INO, OsStr::new("escape")).is_none());
        assert!(tree.lookup(ROOT_INO, OsStr::new("c:")).is_none());
    }

    #[test]
    fn tree_prefix_collision_never_grows_a_file() {
        // `a/b` is a file, then `a/b/c` needs `a/b` as a directory: a malformed
        // manifest. The existing file node must never gain children.
        let files = vec![file("f0", "a/b", 10), file("f1", "a/b/c", 20)];
        let tree = FileTree::build(&files);
        let a = tree.lookup(ROOT_INO, OsStr::new("a")).unwrap();
        let b = tree.lookup(a, OsStr::new("b")).unwrap();
        assert_eq!(tree.attr(b).map(|(k, _)| k), Some(NodeKind::File));
        assert!(tree.children(b).is_none(), "a file node has no children");
    }

    #[test]
    fn tree_from_empty_or_all_malformed_has_no_roots() {
        assert!(FileTree::build(&[]).roots().is_empty());
        let files = vec![file("x", "/abs", 1), file("y", "..", 1)];
        assert!(FileTree::build(&files).roots().is_empty());
    }
}
