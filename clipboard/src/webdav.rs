// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

//! On-demand **loopback WebDAV** file server (Linux) — the **fallback for FUSE**.
//!
//! FUSE ([`crate::fuse`]) is the preferred files backend: it exposes a real
//! `file://` path usable by any application and any syscall. But FUSE needs
//! `/dev/fuse` plus the setuid `fusermount3` helper, and some environments
//! (hardened containers, sandboxes, hosts without the `fuse3` package) have
//! neither. There, a GNOME/GVfs (or KDE/KIO) desktop can still paste a
//! `dav://localhost:PORT/…` URI as readily as a `file://` one, streaming the
//! bytes on demand through `g_file_copy` — no `/dev/fuse` required. This module
//! is that middle ground.
//!
//! Its reach is narrower than FUSE (only GVfs/KIO file managers understand
//! `dav://`; a terminal or a `file://`-only app does not), so it is wired as a
//! strict fallback: the X11 backend tries FUSE first and only reaches here when
//! FUSE is unavailable (see `x11.rs`).
//!
//! ## Mechanics
//!
//! A tiny pure-`std` HTTP/1.1 server (no C dependency, no async) listens on an
//! ephemeral `127.0.0.1` port and serves the frozen manifest tree:
//! - `PROPFIND` (Depth 0/1) returns a `207 Multi-Status` declaring each file's
//!   size (the file exists only in the manifest, never on disk) and each
//!   directory's nature;
//! - `GET` (with an optional `Range`) streams a file by pulling `SCRATCH`-sized
//!   ranges from the [`FileFetcher`] seam straight to the socket — nothing is
//!   ever spilled to disk or held whole in RAM, and the bounded fetcher throttles
//!   the source. Because the fetcher is range-native (one call returns the whole
//!   requested slice, short only at genuine EOF), there is no reader cache and no
//!   skip-and-discard.
//!
//! GNOME does not auto-mount on paste (its copy path returns
//! `G_IO_ERROR_NOT_MOUNTED`), so [`WebDavMount`] establishes the GVfs mount
//! itself with `gio mount` at offer time and tears it down (`gio mount -u`) on
//! drop.
//!
//! ## Capability-path secret (access control)
//!
//! A loopback DAV server is reachable by any local process, so every served path
//! lives under a per-offer `/<secret>/` prefix — 128 random bits from
//! `/dev/urandom`, lowercase-hex. A request whose first path segment is not the
//! secret gets a flat 404. This is the access control the private reference
//! lacked. Its limit: the secret rides in the `gio mount` argv, so it is briefly
//! visible in `/proc/<pid>/cmdline` (world-readable by default) for the ~seconds
//! the mount call runs. So the prefix stops a passive loopback reader but not a
//! process actively polling `/proc` during that window — which is the second
//! reason (with the reach limit) a sensitive clip never uses this path.
//!
//! ## Never for sensitive clips
//!
//! WebDAV is **never** used for a `sensitive` clip: a loopback DAV server is
//! weaker than the uid-private FUSE mount, so a sensitive files clip stays
//! FUSE-only (or is refused). That refusal is enforced at the X11 call site
//! (`x11.rs`), not here — this module carries no sensitivity logic.

use std::ffi::OsStr;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::backend::{FileFetcher, RemoteFile};
use crate::files::{self, FileTree, NodeKind};

/// Streaming grain (bytes). Each `GET` pulls the file in slices of this size and
/// writes each straight to the socket, so an arbitrarily large file is served
/// without ever being held whole in RAM.
const SCRATCH: usize = 256 * 1024;

/// Cap of a request body. The only legitimate body is the small XML of a
/// `PROPFIND`; anything larger is refused (and the connection closed) WITHOUT
/// ever pre-allocating a client-declared size — an unbounded `Content-Length`
/// would otherwise be an OOM lever against the whole daemon.
const MAX_BODY: usize = 64 * 1024;

/// Fixed `Last-Modified` (a valid HTTP-date). No reliable timestamp travels here
/// and the paste does not depend on one, so a constant avoids embedding an
/// RFC 1123 formatter.
const HTTP_DATE: &str = "Mon, 01 Jan 2024 00:00:00 GMT";

/// Socket timeouts: bound a stuck client without hampering a slow but legitimate
/// copy. The read timeout closes an idle keep-alive connection; the write timeout
/// bounds a blocked consumer.
const READ_TIMEOUT: Duration = Duration::from_secs(120);
const WRITE_TIMEOUT: Duration = Duration::from_secs(600);

/// Poll period of `stop` by the accept loop (the listener is non-blocking). Short
/// enough for a responsive drop, long enough not to spin (a file manager opens
/// only a handful of connections per paste).
const ACCEPT_POLL: Duration = Duration::from_millis(50);

/// Bound of the `gio mount` wait on the critical path (the X11 thread). A healthy
/// mount is sub-second; this cap only covers the degraded case (gvfsd-dav cold,
/// D-Bus contention). `stdin` is closed so any auth prompt fails fast (a bare
/// `dav://` triggers none in practice).
const GIO_MOUNT_TIMEOUT: Duration = Duration::from_secs(5);

// ===================== Shared server state =====================

/// State shared (behind an `Arc`) by the accept thread and every detached
/// connection thread. Immutable but for `stop`, so no locking is needed.
struct Shared {
    /// The frozen manifest tree (reused verbatim from the FUSE side).
    tree: FileTree,
    /// The pull seam: one call returns the whole requested range (short only at
    /// EOF).
    fetcher: Arc<dyn FileFetcher>,
    /// The 128-bit capability secret (lowercase hex). Every served path lives
    /// under `/<secret>/`.
    secret: String,
    /// Set by [`WebDavServer::drop`] to unwind the accept loop and connections.
    stop: AtomicBool,
}

impl Shared {
    /// Whether the request path's first segment is the capability secret. Every
    /// served path lives under `/<secret>/`; anything else is a flat 404.
    fn path_has_secret(&self, path: &str) -> bool {
        path.split('/').find(|s| !s.is_empty()) == Some(self.secret.as_str())
    }

    /// Resolve a normalized request path (`/<secret>/a/b`) to a tree inode.
    /// `None` if the first segment is not the secret (access control) or if the
    /// remaining path does not exist — both surface as 404. The mount root path
    /// `/<secret>` resolves to [`files::ROOT_INO`].
    fn resolve(&self, path: &str) -> Option<u64> {
        let mut segs = path.split('/').filter(|s| !s.is_empty());
        if segs.next()? != self.secret {
            return None;
        }
        let mut ino = files::ROOT_INO;
        for name in segs {
            ino = self.tree.lookup(ino, OsStr::new(name))?;
        }
        Some(ino)
    }
}

/// Generate the 128-bit capability secret: 16 bytes from `/dev/urandom`,
/// lowercase-hex-encoded. `Err` if `/dev/urandom` cannot be read (never guess a
/// weak secret — refuse the offer instead). Pure `std`, no crate.
fn make_secret() -> io::Result<String> {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut buf = [0u8; 16];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut buf)?;
    let mut s = String::with_capacity(32);
    for b in buf {
        s.push(HEX[(b >> 4) as usize] as char);
        s.push(HEX[(b & 0x0f) as usize] as char);
    }
    Ok(s)
}

// ===================== Manifest XML (PROPFIND) =====================

/// Serialize one `<D:response>` for a node. `href` is the node's absolute HTTP
/// path, percent-encoded (so free of raw `&`/`<`/`>` → XML-safe, no extra
/// escaping needed).
fn push_response(out: &mut String, href: &str, kind: NodeKind, size: u64) {
    out.push_str("<D:response><D:href>");
    out.push_str(href);
    out.push_str("</D:href><D:propstat><D:prop>");
    match kind {
        NodeKind::Dir => out.push_str("<D:resourcetype><D:collection/></D:resourcetype>"),
        NodeKind::File => {
            out.push_str("<D:resourcetype/><D:getcontentlength>");
            out.push_str(&size.to_string());
            out.push_str("</D:getcontentlength>");
            out.push_str("<D:getcontenttype>application/octet-stream</D:getcontenttype>");
        }
    }
    out.push_str("<D:getlastmodified>");
    out.push_str(HTTP_DATE);
    out.push_str(
        "</D:getlastmodified></D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response>",
    );
}

// ===================== Minimal HTTP parsing =====================

/// A parsed HTTP request (only the fields we act on).
struct Req {
    method: String,
    /// The **decoded, normalized** path (no query/fragment, no trailing `/` bar
    /// the root).
    path: String,
    /// `Depth:` header ≠ `0` (PROPFIND) → also list a directory's children.
    depth_one: bool,
    /// `Range: bytes=…` → optional `(start, end)` bounds.
    range: Option<(Option<u64>, Option<u64>)>,
    keep_alive: bool,
}

/// Read and parse **one** request off the connection. `Ok(None)` means the client
/// closed (EOF) or sent a request we cannot frame safely (over-large or unknown
/// body framing) → the caller closes without responding.
fn read_request<R: BufRead>(reader: &mut R) -> io::Result<Option<Req>> {
    let mut line = String::new();
    if reader.read_line(&mut line)? == 0 {
        return Ok(None);
    }
    let line = line.trim_end();
    if line.is_empty() {
        return Ok(None);
    }
    let mut parts = line.split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let target = parts.next().unwrap_or("/").to_string();
    let version = parts.next().unwrap_or("HTTP/1.0").to_string();

    let mut depth_one = false;
    let mut range = None;
    let mut content_length = 0usize;
    let mut conn_close = false;
    let mut conn_keep = false;
    // A request we cannot frame (unreadable Content-Length, any Transfer-Encoding,
    // an over-large body): we cannot reliably drain the body → close rather than
    // desynchronize the stream.
    let mut unframed = false;
    loop {
        let mut h = String::new();
        if reader.read_line(&mut h)? == 0 {
            // EOF before the terminating blank line = truncated request → close.
            return Ok(None);
        }
        let h = h.trim_end();
        if h.is_empty() {
            break; // end of headers
        }
        if let Some((k, v)) = h.split_once(':') {
            let k = k.trim().to_ascii_lowercase();
            let v = v.trim();
            match k.as_str() {
                "depth" => depth_one = v != "0",
                "range" => range = parse_range_header(v),
                "content-length" => match v.parse::<usize>() {
                    Ok(n) => content_length = n,
                    Err(_) => unframed = true, // unreadable length: framing unknown
                },
                // Chunked bodies are not handled (no GVfs/KIO client uses one);
                // seeing one makes the body undrainable → close.
                "transfer-encoding" => unframed = true,
                "connection" => {
                    let v = v.to_ascii_lowercase();
                    conn_close = v.contains("close");
                    conn_keep = v.contains("keep-alive");
                }
                _ => {}
            }
        }
    }
    // Over-large body (the only legitimate one is a small PROPFIND XML): NEVER
    // pre-allocate a client-declared size (an unbounded `Content-Length` → OOM).
    if unframed || content_length > MAX_BODY {
        return Ok(None); // close without responding
    }
    // Drain a legitimate small body (a PROPFIND request XML) to stay keep-alive
    // aligned.
    if content_length > 0 {
        let mut body = vec![0u8; content_length];
        reader.read_exact(&mut body)?;
    }
    let keep_alive = if version.eq_ignore_ascii_case("HTTP/1.0") {
        conn_keep
    } else {
        !conn_close
    };
    Ok(Some(Req {
        method,
        path: normalize_path(&target),
        depth_one,
        range,
        keep_alive,
    }))
}

/// `bytes=START-END` → optional bounds. `None` if unparseable (the `Range` is
/// then ignored → whole resource).
fn parse_range_header(v: &str) -> Option<(Option<u64>, Option<u64>)> {
    let v = v.strip_prefix("bytes=")?;
    let (s, e) = v.split_once('-')?;
    let start = if s.trim().is_empty() {
        None
    } else {
        s.trim().parse().ok()
    };
    let end = if e.trim().is_empty() {
        None
    } else {
        e.trim().parse().ok()
    };
    if start.is_none() && end.is_none() {
        return None;
    }
    Some((start, end))
}

/// Decode/normalize a request target into a tree path: drop query/fragment,
/// percent-decode, force a leading `/`, drop the trailing `/` (bar the root).
fn normalize_path(target: &str) -> String {
    let raw = target.split(['?', '#']).next().unwrap_or(target);
    let decoded = String::from_utf8_lossy(&files::percent_decode(raw)).into_owned();
    let mut p = if decoded.starts_with('/') {
        decoded
    } else {
        format!("/{decoded}")
    };
    while p.len() > 1 && p.ends_with('/') {
        p.pop();
    }
    p
}

/// `Range` + size → the **inclusive** `(start, end)` to serve, or `None` if the
/// range is **unsatisfiable** (RFC 7233 → 416): inverted (`end < start`), start
/// past the resource (`start >= size`), or any range on an empty resource.
/// Without this guard `length = end - start + 1` would underflow. A request
/// **without** a `Range` always returns the whole resource.
fn resolve_range(range: Option<(Option<u64>, Option<u64>)>, size: u64) -> Option<(u64, u64)> {
    let last = size.saturating_sub(1);
    let (start, end) = match range {
        None | Some((None, None)) => return Some((0, last)), // whole resource (200)
        Some((Some(s), Some(e))) => (s, e.min(last)),
        Some((Some(s), None)) => (s, last),
        // `bytes=-N`: the last N bytes.
        Some((None, Some(n))) => (size.saturating_sub(n), last),
    };
    // Unsatisfiable: empty resource, start past the end, or an inverted range.
    if size == 0 || start >= size || end < start {
        return None;
    }
    Some((start, end))
}

// ===================== Connection loop + responses =====================

/// Serve one connection (keep-alive loop). The secret gate runs before method
/// dispatch: a path not under `/<secret>/` is a flat 404 whatever the method.
fn handle_conn(stream: TcpStream, shared: Arc<Shared>) {
    let _ = stream.set_read_timeout(Some(READ_TIMEOUT));
    let _ = stream.set_write_timeout(Some(WRITE_TIMEOUT));
    let read_half = match stream.try_clone() {
        Ok(s) => s,
        Err(_) => return,
    };
    let mut reader = BufReader::new(read_half);
    let mut writer = stream;
    loop {
        if shared.stop.load(Ordering::Relaxed) {
            break;
        }
        let req = match read_request(&mut reader) {
            Ok(Some(r)) => r,
            _ => break, // EOF / error / timeout / unframable → close
        };
        let keep = req.keep_alive && !shared.stop.load(Ordering::Relaxed);
        let res = match req.method.as_str() {
            // OPTIONS is a capability probe that leaks no file data, so it is
            // answered regardless of the capability path: a GVfs/KIO client may
            // send `OPTIONS *` (or probe before the secret is in play), and a 404
            // there would break the mount handshake for no security gain.
            "OPTIONS" => write_options(&mut writer, keep),
            // Access control for every data-bearing method: a path not under the
            // capability secret is a flat 404, revealing nothing about the tree.
            _ if !shared.path_has_secret(&req.path) => {
                write_status(&mut writer, 404, "Not Found", keep)
            }
            "PROPFIND" => write_propfind(&mut writer, &shared, &req, keep),
            "HEAD" => write_head(&mut writer, &shared, &req, keep),
            "GET" => write_get(&mut writer, &shared, &req, keep),
            _ => write_405(&mut writer, keep),
        };
        match res {
            Ok(true) => continue,
            _ => break, // error, broken framing, or Connection: close → close
        }
    }
}

fn write_options<W: Write>(w: &mut W, keep: bool) -> io::Result<bool> {
    let conn = if keep { "keep-alive" } else { "close" };
    write!(
        w,
        "HTTP/1.1 200 OK\r\nDAV: 1\r\nAllow: OPTIONS, GET, HEAD, PROPFIND\r\n\
         MS-Author-Via: DAV\r\nContent-Length: 0\r\nConnection: {conn}\r\n\r\n"
    )?;
    w.flush()?;
    Ok(keep)
}

fn write_405<W: Write>(w: &mut W, keep: bool) -> io::Result<bool> {
    let conn = if keep { "keep-alive" } else { "close" };
    write!(
        w,
        "HTTP/1.1 405 Method Not Allowed\r\nAllow: OPTIONS, GET, HEAD, PROPFIND\r\n\
         Content-Length: 0\r\nConnection: {conn}\r\n\r\n"
    )?;
    w.flush()?;
    Ok(keep)
}

fn write_status<W: Write>(w: &mut W, code: u16, reason: &str, keep: bool) -> io::Result<bool> {
    let conn = if keep { "keep-alive" } else { "close" };
    write!(
        w,
        "HTTP/1.1 {code} {reason}\r\nContent-Length: 0\r\nConnection: {conn}\r\n\r\n"
    )?;
    w.flush()?;
    Ok(keep)
}

fn write_propfind<W: Write>(w: &mut W, shared: &Shared, req: &Req, keep: bool) -> io::Result<bool> {
    let ino = match shared.resolve(&req.path) {
        Some(ino) => ino,
        None => return write_status(w, 404, "Not Found", keep),
    };
    let Some((kind, size)) = shared.tree.attr(ino) else {
        return write_status(w, 404, "Not Found", keep);
    };
    // The self href is the request path re-encoded (it is already the canonical
    // `/<secret>/…` path, so percent-encoding it is idempotent on the secret).
    let self_href = files::percent_encode(req.path.as_bytes());
    let mut body = String::from(r#"<?xml version="1.0" encoding="utf-8"?>"#);
    body.push_str(r#"<D:multistatus xmlns:D="DAV:">"#);
    push_response(&mut body, &self_href, kind, size);
    // Depth 1 on a directory: one response per immediate child. Each child href is
    // the parent href + "/" + percent-encoded name.
    if req.depth_one
        && kind == NodeKind::Dir
        && let Some(children) = shared.tree.children(ino)
    {
        for (name, child_ino) in children {
            if let Some((c_kind, c_size)) = shared.tree.attr(*child_ino) {
                let href = format!("{self_href}/{}", files::percent_encode(name.as_bytes()));
                push_response(&mut body, &href, c_kind, c_size);
            }
        }
    }
    body.push_str("</D:multistatus>");
    let conn = if keep { "keep-alive" } else { "close" };
    write!(
        w,
        "HTTP/1.1 207 Multi-Status\r\nContent-Type: application/xml; charset=\"utf-8\"\r\n\
         Content-Length: {}\r\nConnection: {conn}\r\n\r\n",
        body.len()
    )?;
    w.write_all(body.as_bytes())?;
    w.flush()?;
    Ok(keep)
}

fn write_head<W: Write>(w: &mut W, shared: &Shared, req: &Req, keep: bool) -> io::Result<bool> {
    let file = shared
        .resolve(&req.path)
        .and_then(|ino| shared.tree.attr(ino))
        .filter(|(kind, _)| *kind == NodeKind::File);
    match file {
        Some((_, size)) => {
            let conn = if keep { "keep-alive" } else { "close" };
            write!(
                w,
                "HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\n\
                 Content-Length: {size}\r\nAccept-Ranges: bytes\r\nConnection: {conn}\r\n\r\n"
            )?;
            w.flush()?;
            Ok(keep)
        }
        None => write_status(w, 404, "Not Found", keep), // dir / unknown → 404
    }
}

/// Serve a `GET` (on-demand streaming). Returns `Ok(false)` — forcing the
/// connection closed — the moment the promised `Content-Length` cannot be met
/// (a short pull, a pull error, a broken socket, or a stop): a truncated stream
/// the client can detect, never a silent short body.
fn write_get<W: Write>(w: &mut W, shared: &Shared, req: &Req, keep: bool) -> io::Result<bool> {
    // Directory or unknown path → 404 (no GET body). The connection gate already
    // 404'd a wrong secret; `resolve` re-checks it defensively.
    let ino = match shared.resolve(&req.path) {
        Some(ino) => ino,
        None => return write_status(w, 404, "Not Found", keep),
    };
    let Some((kind, size)) = shared.tree.attr(ino) else {
        return write_status(w, 404, "Not Found", keep);
    };
    let file_id = match (kind, shared.tree.file_id(ino)) {
        (NodeKind::File, Some(id)) => id,
        _ => return write_status(w, 404, "Not Found", keep),
    };

    // Unsatisfiable range → 416 (rather than an underflowing length). A GET
    // without a Range stays the whole resource (200).
    let (start, end) = match resolve_range(req.range, size) {
        Some(se) => se,
        None => {
            let conn = if keep { "keep-alive" } else { "close" };
            write!(
                w,
                "HTTP/1.1 416 Range Not Satisfiable\r\nContent-Range: bytes */{size}\r\n\
                 Content-Length: 0\r\nConnection: {conn}\r\n\r\n"
            )?;
            w.flush()?;
            return Ok(keep);
        }
    };
    let length = if size == 0 { 0 } else { end - start + 1 };
    let partial = req.range.is_some();
    let conn = if keep { "keep-alive" } else { "close" };
    let status = if partial {
        "206 Partial Content"
    } else {
        "200 OK"
    };
    let mut head = format!(
        "HTTP/1.1 {status}\r\nContent-Type: application/octet-stream\r\n\
         Content-Length: {length}\r\nAccept-Ranges: bytes\r\n"
    );
    if partial {
        head.push_str(&format!("Content-Range: bytes {start}-{end}/{size}\r\n"));
    }
    head.push_str(&format!("Connection: {conn}\r\n\r\n"));
    w.write_all(head.as_bytes())?;

    // Stream exactly `length` bytes, pulling SCRATCH-sized ranges on demand. The
    // range-native fetcher returns the whole requested slice in one call, so each
    // pull goes straight to the socket — the file is never held whole in RAM. A
    // short pull before the length is met, a pull error, or a broken socket
    // aborts: a Content-Length was already promised, so we force-close rather than
    // send a silently truncated body.
    let mut remaining = length;
    let mut pos = start;
    let mut ok = true;
    while remaining > 0 {
        if shared.stop.load(Ordering::Relaxed) {
            ok = false;
            break;
        }
        let want = remaining.min(SCRATCH as u64);
        match shared.fetcher.read(file_id, pos, want) {
            Ok(chunk) if !chunk.is_empty() => {
                if w.write_all(&chunk).is_err() {
                    ok = false; // peer gone / paste cancelled → broken pipe
                    break;
                }
                let n = chunk.len() as u64;
                remaining -= n;
                pos += n;
                if n < want {
                    // Fewer bytes than asked while more were promised: the source
                    // is shorter than the manifest declared (a truncation).
                    warn(&format!(
                        "webdav GET {}: short pull ({n} < {want}) before Content-Length met",
                        req.path
                    ));
                    ok = false;
                    break;
                }
            }
            Ok(_) => {
                warn(&format!("webdav GET {}: empty pull before EOF", req.path));
                ok = false; // EOF before the promised length
                break;
            }
            Err(e) => {
                warn(&format!("webdav GET {}: pull failed ({e})", req.path));
                ok = false;
                break;
            }
        }
    }
    let _ = w.flush();
    // On success honor keep-alive; on abort force-close (broken framing).
    if ok { Ok(keep) } else { Ok(false) }
}

// ===================== Bare server (no gio) =====================

/// A live loopback WebDAV server for one clip, independent of gio (so tests can
/// drive it over loopback with no GVFS). Its lifetime is the offer's; dropping it
/// stops accepting and unwinds the connection threads.
pub struct WebDavServer {
    port: u16,
    shared: Arc<Shared>,
    accept_join: Option<JoinHandle<()>>,
}

impl WebDavServer {
    /// Build the manifest tree, mint a capability secret, bind an ephemeral
    /// loopback port, and spawn the accept thread. `Err` (`InvalidInput`) if the
    /// manifest yields no usable root, or on an I/O failure (secret, bind, thread).
    pub fn bind(files: &[RemoteFile], fetcher: Arc<dyn FileFetcher>) -> io::Result<WebDavServer> {
        let tree = FileTree::build(files);
        if tree.roots().is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "files manifest yields no usable root",
            ));
        }
        let secret = make_secret()?;
        let listener = TcpListener::bind(("127.0.0.1", 0))?;
        let port = listener.local_addr()?.port();
        // Non-blocking accept: the loop polls `stop` every ACCEPT_POLL and exits
        // without needing a wake-up "dummy connection" whose failure could leave
        // `accept()` blocked and wedge the drop's join.
        listener.set_nonblocking(true)?;
        let shared = Arc::new(Shared {
            tree,
            fetcher,
            secret,
            stop: AtomicBool::new(false),
        });

        let accept_shared = shared.clone();
        let accept_join = thread::Builder::new()
            .name("universallink-webdav".into())
            .spawn(move || {
                while !accept_shared.stop.load(Ordering::Relaxed) {
                    match listener.accept() {
                        Ok((stream, _)) => {
                            // The accepted stream inherits the listener's
                            // non-blocking mode: put it back to BLOCKING (the
                            // handler reads/writes blocking, bounded by the socket
                            // timeouts).
                            let _ = stream.set_nonblocking(false);
                            let conn_shared = accept_shared.clone();
                            // Detached connection thread: ends via stop / EOF
                            // (unmount) / end of copy.
                            let _ = thread::Builder::new()
                                .name("universallink-webdav-conn".into())
                                .spawn(move || handle_conn(stream, conn_shared));
                        }
                        Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                            thread::sleep(ACCEPT_POLL)
                        }
                        Err(e) => {
                            warn(&format!("webdav accept failed: {e}"));
                            thread::sleep(ACCEPT_POLL);
                        }
                    }
                }
            })
            .map_err(|e| io::Error::other(format!("webdav accept thread: {e}")))?;

        Ok(WebDavServer {
            port,
            shared,
            accept_join: Some(accept_join),
        })
    }

    /// The ephemeral loopback port the server listens on.
    pub fn port(&self) -> u16 {
        self.port
    }

    /// The capability secret every served path lives under.
    pub fn secret(&self) -> &str {
        &self.shared.secret
    }

    /// The top-level element names (one published URI each).
    pub fn roots(&self) -> &[String] {
        self.shared.tree.roots()
    }
}

impl Drop for WebDavServer {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Relaxed);
        // Join the accept thread: it polls `stop` every ACCEPT_POLL and exits
        // fast. Detached connection threads exit when their sockets close / time
        // out.
        if let Some(j) = self.accept_join.take() {
            let _ = j.join();
        }
    }
}

// ===================== gio probe + mount =====================

/// Non-destructive probe: is a loopback WebDAV backend plausible here? Checks
/// (a) the `gio` binary (which establishes/removes the GVfs mount), (b) a
/// reachable session D-Bus (else gvfsd-dav is unreachable), (c) the `gvfsd-dav`
/// backend present. Best-effort/conservative: `x11.rs` only reaches this fallback
/// when it has a chance to work.
pub fn webdav_available() -> bool {
    if !in_path("gio") {
        return false;
    }
    let has_bus = std::env::var_os("DBUS_SESSION_BUS_ADDRESS").is_some()
        || std::env::var_os("XDG_RUNTIME_DIR")
            .map(|d| std::path::Path::new(&d).join("bus").exists())
            .unwrap_or(false);
    if !has_bus {
        return false;
    }
    gvfsd_dav_present()
}

/// Is an executable on the `PATH`?
fn in_path(bin: &str) -> bool {
    std::env::var_os("PATH")
        .map(|paths| std::env::split_paths(&paths).any(|d| d.join(bin).exists()))
        .unwrap_or(false)
}

/// Is the `gvfsd-dav` backend installed? Usual fixed locations plus a scan of
/// `/usr/lib/*/gvfs` (multiarch). Best-effort: if not found, prefer refusing to a
/// mount doomed to fail.
fn gvfsd_dav_present() -> bool {
    const FIXED: &[&str] = &[
        "/usr/libexec/gvfsd-dav",
        "/usr/libexec/gvfs/gvfsd-dav",
        "/usr/lib/gvfs/gvfsd-dav",
        "/usr/local/libexec/gvfsd-dav",
    ];
    if FIXED.iter().any(|p| std::path::Path::new(p).exists()) {
        return true;
    }
    if let Ok(entries) = std::fs::read_dir("/usr/lib") {
        for e in entries.flatten() {
            if e.path().join("gvfs/gvfsd-dav").exists() {
                return true;
            }
        }
    }
    false
}

/// Run `gio mount <url>` with a bounded wait ([`GIO_MOUNT_TIMEOUT`]) and `stdin`
/// closed. On non-zero exit, the captured stderr is surfaced; on timeout the
/// child is killed.
fn gio_mount(url: &str) -> io::Result<()> {
    let mut child = Command::new("gio")
        .arg("mount")
        .arg(url)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| io::Error::other(format!("spawn `gio mount`: {e}")))?;
    let started = Instant::now();
    loop {
        match child.try_wait()? {
            Some(status) if status.success() => return Ok(()),
            Some(_) => {
                let mut err = String::new();
                if let Some(mut e) = child.stderr.take() {
                    let _ = e.read_to_string(&mut err);
                }
                return Err(io::Error::other(format!(
                    "`gio mount {url}` failed: {}",
                    err.trim()
                )));
            }
            None => {
                if started.elapsed() > GIO_MOUNT_TIMEOUT {
                    let _ = child.kill();
                    // Reap the SIGKILLed child (returns at once): `Child` does not
                    // reap on drop, so without this the killed `gio` lingers as a
                    // zombie for the daemon's lifetime — one per degraded mount.
                    let _ = child.wait();
                    return Err(io::Error::other(format!(
                        "`gio mount {url}` timed out ({GIO_MOUNT_TIMEOUT:?})"
                    )));
                }
                thread::sleep(Duration::from_millis(50));
            }
        }
    }
}

// ===================== Mounted server (gio) =====================

/// A [`WebDavServer`] plus the live GVfs mount that makes GNOME/KDE paste its
/// URIs. Dropping it unmounts (non-blocking) and stops the inner server. Its
/// lifetime is the offer's, exactly like [`crate::fuse::FuseMount`].
pub struct WebDavMount {
    server: WebDavServer,
    /// The URL passed to `gio mount` (`dav://localhost:PORT/<secret>/`).
    mount_url: String,
}

impl WebDavMount {
    /// Bind the server, then establish the GVfs mount. On a `gio` failure the
    /// server is dropped (its Drop stops it) and the error is returned.
    pub fn mount(files: &[RemoteFile], fetcher: Arc<dyn FileFetcher>) -> io::Result<WebDavMount> {
        let server = WebDavServer::bind(files, fetcher)?;
        let mount_url = format!("dav://localhost:{}/{}/", server.port(), server.secret());
        // On a `gio` failure `server` drops as this returns early → its Drop stops
        // it, so no bare server is left running.
        gio_mount(&mount_url)?;
        Ok(WebDavMount { server, mount_url })
    }

    /// The URIs to publish for a paste: scheme `webdav` for KDE/KIO (Dolphin
    /// rejects `dav://`, KDE bug 365356), `dav` for GNOME and `text/uri-list`.
    /// One per top-level element, under the capability path.
    pub fn uris(&self, kde: bool) -> Vec<String> {
        let scheme = if kde { "webdav" } else { "dav" };
        let (port, secret) = (self.server.port(), self.server.secret());
        self.server
            .roots()
            .iter()
            .map(|r| {
                format!(
                    "{scheme}://localhost:{port}/{secret}/{}",
                    files::percent_encode(r.as_bytes())
                )
            })
            .collect()
    }
}

impl Drop for WebDavMount {
    fn drop(&mut self) {
        // Unmount on a DETACHED thread so the X11 thread never blocks. Unmounting
        // closes gvfsd-dav's connections → the connection threads end. The inner
        // `server` then drops (fields drop after this method), stopping it.
        let url = std::mem::take(&mut self.mount_url);
        thread::spawn(move || {
            let _ = Command::new("gio")
                .arg("mount")
                .arg("-u")
                .arg(&url)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        });
    }
}

fn warn(message: &str) {
    eprintln!("[universallink-clipboard] {message}");
}

#[cfg(test)]
mod tests {
    //! Pure-helper tests only (no socket, no gio): path normalization and the
    //! Range→interval resolution. The bare server itself is driven end-to-end
    //! over loopback in `tests/linux_webdav.rs`.
    use super::*;

    #[test]
    fn normalize_strips_query_trailing_slash_and_decodes() {
        assert_eq!(normalize_path("/dir/a%20b.bin"), "/dir/a b.bin");
        assert_eq!(normalize_path("/dir/"), "/dir");
        assert_eq!(normalize_path("/"), "/");
        assert_eq!(normalize_path("/x?y=1"), "/x");
        assert_eq!(normalize_path("relative"), "/relative");
    }

    #[test]
    fn range_header_parsing() {
        assert_eq!(
            parse_range_header("bytes=100-199"),
            Some((Some(100), Some(199)))
        );
        assert_eq!(parse_range_header("bytes=50-"), Some((Some(50), None)));
        assert_eq!(parse_range_header("bytes=-30"), Some((None, Some(30))));
        assert_eq!(parse_range_header("bytes=-"), None);
        assert_eq!(parse_range_header("nonsense"), None);
    }

    #[test]
    fn range_resolution_is_inclusive_and_rejects_unsatisfiable() {
        // Satisfiable → Some.
        assert_eq!(
            resolve_range(Some((Some(100), Some(199))), 1000),
            Some((100, 199))
        );
        assert_eq!(
            resolve_range(Some((Some(900), None)), 1000),
            Some((900, 999))
        );
        assert_eq!(
            resolve_range(Some((None, Some(30))), 1000),
            Some((970, 999))
        );
        assert_eq!(resolve_range(None, 1000), Some((0, 999)));
        // End clamped to the last byte.
        assert_eq!(
            resolve_range(Some((Some(0), Some(9999))), 1000),
            Some((0, 999))
        );
        // Unsatisfiable → None (→ 416), never an underflowing length.
        assert_eq!(resolve_range(Some((Some(500), Some(100))), 1000), None); // inverted
        assert_eq!(resolve_range(Some((Some(2000), Some(3000))), 1000), None); // past resource
        assert_eq!(resolve_range(Some((None, Some(0))), 1000), None); // `bytes=-0`
        assert_eq!(resolve_range(Some((Some(0), None)), 0), None); // empty + Range
        // Empty resource without Range → whole resource (length 0, handled by GET).
        assert_eq!(resolve_range(None, 0), Some((0, 0)));
    }

    #[test]
    fn webdav_available_does_not_panic() {
        let _ = webdav_available();
    }
}
