// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

//! Loopback integration tests for the Linux WebDAV fallback. They drive the bare
//! [`WebDavServer`] directly over a raw `TcpStream` — NO gio, NO GVFS, NO
//! `/dev/fuse` — so they run in CI on any box (this one has neither gio nor
//! gvfs). Each test binds its own ephemeral port, so nothing needs serializing;
//! the client sockets use short timeouts so a server bug cannot hang CI.

#![cfg(target_os = "linux")]

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::sync::Arc;
use std::time::Duration;

use universallink_clipboard::{FileFetcher, RemoteFile, WebDavServer};

/// Deterministic byte at absolute offset `k` (same formula as `tests/linux_files.rs`).
fn byte_at(k: u64) -> u8 {
    (k % 251) as u8
}

/// An in-process fetcher serving deterministic bytes for a fixed set of files,
/// truncating at each file's declared size (fewer than `len` only at EOF).
struct FakeFetcher {
    sizes: std::collections::HashMap<String, u64>,
}

impl FileFetcher for FakeFetcher {
    fn read(&self, file_id: &str, offset: u64, len: u64) -> std::io::Result<Vec<u8>> {
        let size = *self
            .sizes
            .get(file_id)
            .ok_or_else(|| std::io::Error::other(format!("unknown file_id {file_id}")))?;
        if offset >= size {
            return Ok(Vec::new());
        }
        let end = (offset + len).min(size);
        Ok((offset..end).map(byte_at).collect())
    }
}

const TOP_SIZE: u64 = 1_000;
const INNER_SIZE: u64 = 250_003;

/// A top-level file plus a file nested in a directory (the "bigger file").
fn manifest() -> Vec<RemoteFile> {
    vec![
        RemoteFile {
            file_id: "f-top".into(),
            path: "top.bin".into(),
            size: TOP_SIZE,
            dir: false,
        },
        RemoteFile {
            file_id: "f-inner".into(),
            path: "dir/inner.bin".into(),
            size: INNER_SIZE,
            dir: false,
        },
    ]
}

fn fetcher() -> Arc<dyn FileFetcher> {
    let mut sizes = std::collections::HashMap::new();
    sizes.insert("f-top".to_string(), TOP_SIZE);
    sizes.insert("f-inner".to_string(), INNER_SIZE);
    Arc::new(FakeFetcher { sizes })
}

fn server() -> WebDavServer {
    WebDavServer::bind(&manifest(), fetcher()).expect("bind webdav server")
}

/// A parsed HTTP response: status code, headers, and body. `status == 0` means
/// the server closed the connection without a valid status line (EOF).
struct Resp {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Resp {
    /// The first header matching `name` (case-insensitive).
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

/// Send one raw request to the loopback server and read the response. `want_body`
/// is false for HEAD (whose response declares a `Content-Length` but sends no
/// body — reading it would block until timeout).
fn http(port: u16, raw: &[u8], want_body: bool) -> Resp {
    let stream = TcpStream::connect(("127.0.0.1", port)).expect("connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("read timeout");
    stream
        .set_write_timeout(Some(Duration::from_secs(5)))
        .expect("write timeout");
    let mut wr = stream.try_clone().expect("clone stream");
    let mut rd = BufReader::new(stream);
    wr.write_all(raw).expect("write request");
    wr.flush().expect("flush request");

    let mut line = String::new();
    if rd.read_line(&mut line).expect("read status line") == 0 {
        // The server closed without responding (e.g. an over-large body).
        return Resp {
            status: 0,
            headers: Vec::new(),
            body: Vec::new(),
        };
    }
    let status: u16 = line
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);

    let mut headers = Vec::new();
    let mut clen = 0usize;
    loop {
        let mut h = String::new();
        if rd.read_line(&mut h).expect("read header") == 0 {
            break;
        }
        let h = h.trim_end();
        if h.is_empty() {
            break;
        }
        if let Some((k, v)) = h.split_once(':') {
            let (k, v) = (k.trim().to_string(), v.trim().to_string());
            if k.eq_ignore_ascii_case("content-length") {
                clen = v.parse().unwrap_or(0);
            }
            headers.push((k, v));
        }
    }
    let body = if want_body && clen > 0 {
        let mut b = vec![0u8; clen];
        rd.read_exact(&mut b).expect("read body");
        b
    } else {
        Vec::new()
    };
    Resp {
        status,
        headers,
        body,
    }
}

#[test]
fn options_advertises_dav() {
    let srv = server();
    let (port, secret) = (srv.port(), srv.secret().to_string());
    let req = format!(
        "OPTIONS /{secret}/ HTTP/1.1\r\nHost: x\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
    );
    let r = http(port, req.as_bytes(), true);
    assert_eq!(r.status, 200);
    assert_eq!(r.header("DAV"), Some("1"));
}

#[test]
fn options_is_answered_without_the_secret() {
    // OPTIONS is a capability probe carrying no file data: a GVfs/KIO client may
    // send `OPTIONS *` (or probe before the secret is in play), so it must be
    // answered regardless of the capability path — never a 404.
    let srv = server();
    let port = srv.port();
    let req = "OPTIONS * HTTP/1.1\r\nHost: x\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
    let r = http(port, req.as_bytes(), true);
    assert_eq!(r.status, 200);
    assert_eq!(r.header("DAV"), Some("1"));
}

#[test]
fn propfind_depth0_root_is_a_collection() {
    let srv = server();
    let (port, secret) = (srv.port(), srv.secret().to_string());
    let req = format!(
        "PROPFIND /{secret}/ HTTP/1.1\r\nHost: x\r\nDepth: 0\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
    );
    let r = http(port, req.as_bytes(), true);
    assert_eq!(r.status, 207);
    let body = String::from_utf8_lossy(&r.body);
    assert!(body.contains("<D:collection/>"), "root is a collection");
}

#[test]
fn propfind_depth1_lists_each_root() {
    let srv = server();
    let (port, secret) = (srv.port(), srv.secret().to_string());
    let req = format!(
        "PROPFIND /{secret}/ HTTP/1.1\r\nHost: x\r\nDepth: 1\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
    );
    let r = http(port, req.as_bytes(), true);
    assert_eq!(r.status, 207);
    let body = String::from_utf8_lossy(&r.body);
    // Each root's href is the percent-encoded absolute path under the secret.
    assert!(
        body.contains(&format!("/{secret}/top.bin")),
        "lists top.bin"
    );
    assert!(body.contains(&format!("/{secret}/dir")), "lists dir");
    assert!(
        body.contains(&format!(
            "<D:getcontentlength>{TOP_SIZE}</D:getcontentlength>"
        )),
        "top.bin size declared"
    );
}

#[test]
fn propfind_file_reports_size() {
    let srv = server();
    let (port, secret) = (srv.port(), srv.secret().to_string());
    let req = format!(
        "PROPFIND /{secret}/dir/inner.bin HTTP/1.1\r\nHost: x\r\nDepth: 0\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
    );
    let r = http(port, req.as_bytes(), true);
    assert_eq!(r.status, 207);
    let body = String::from_utf8_lossy(&r.body);
    assert!(body.contains(&format!(
        "<D:getcontentlength>{INNER_SIZE}</D:getcontentlength>"
    )));
}

#[test]
fn get_whole_file_streams_exact_bytes() {
    let srv = server();
    let (port, secret) = (srv.port(), srv.secret().to_string());
    let req = format!("GET /{secret}/top.bin HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n");
    let r = http(port, req.as_bytes(), true);
    assert_eq!(r.status, 200);
    assert_eq!(r.body.len() as u64, TOP_SIZE);
    assert!(
        r.body
            .iter()
            .enumerate()
            .all(|(i, &b)| b == byte_at(i as u64)),
        "whole-file bytes"
    );
}

#[test]
fn get_range_is_206_with_content_range() {
    let srv = server();
    let (port, secret) = (srv.port(), srv.secret().to_string());
    let req = format!(
        "GET /{secret}/dir/inner.bin HTTP/1.1\r\nHost: x\r\nRange: bytes=100-199\r\nConnection: close\r\n\r\n"
    );
    let r = http(port, req.as_bytes(), true);
    assert_eq!(r.status, 206);
    assert_eq!(
        r.header("Content-Range"),
        Some(format!("bytes 100-199/{INNER_SIZE}").as_str())
    );
    assert_eq!(r.body.len(), 100);
    assert!(
        r.body
            .iter()
            .enumerate()
            .all(|(i, &b)| b == byte_at(100 + i as u64)),
        "range bytes"
    );
}

#[test]
fn get_suffix_range_returns_last_bytes() {
    let srv = server();
    let (port, secret) = (srv.port(), srv.secret().to_string());
    let req = format!(
        "GET /{secret}/dir/inner.bin HTTP/1.1\r\nHost: x\r\nRange: bytes=-50\r\nConnection: close\r\n\r\n"
    );
    let r = http(port, req.as_bytes(), true);
    assert_eq!(r.status, 206);
    let first = INNER_SIZE - 50;
    assert_eq!(
        r.header("Content-Range"),
        Some(format!("bytes {first}-{}/{INNER_SIZE}", INNER_SIZE - 1).as_str())
    );
    assert_eq!(r.body.len(), 50);
    assert!(
        r.body
            .iter()
            .enumerate()
            .all(|(i, &b)| b == byte_at(first + i as u64)),
        "suffix bytes"
    );
}

#[test]
fn get_out_of_range_is_416() {
    let srv = server();
    let (port, secret) = (srv.port(), srv.secret().to_string());
    let req = format!(
        "GET /{secret}/dir/inner.bin HTTP/1.1\r\nHost: x\r\nRange: bytes=999999999-\r\nConnection: close\r\n\r\n"
    );
    let r = http(port, req.as_bytes(), true);
    assert_eq!(r.status, 416);
    assert_eq!(
        r.header("Content-Range"),
        Some(format!("bytes */{INNER_SIZE}").as_str())
    );
}

#[test]
fn head_file_reports_size_with_no_body() {
    let srv = server();
    let (port, secret) = (srv.port(), srv.secret().to_string());
    let req = format!("HEAD /{secret}/top.bin HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n");
    // HEAD declares Content-Length but sends no body: do NOT read the body.
    let r = http(port, req.as_bytes(), false);
    assert_eq!(r.status, 200);
    assert_eq!(
        r.header("Content-Length"),
        Some(TOP_SIZE.to_string().as_str())
    );
    assert!(r.body.is_empty());
}

#[test]
fn wrong_secret_is_404() {
    let srv = server();
    let port = srv.port();
    // "deadbeef" (8 chars) can never equal the 32-hex-char secret.
    let req = "GET /deadbeef/top.bin HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n";
    let r = http(port, req.as_bytes(), true);
    assert_eq!(r.status, 404);
}

#[test]
fn unknown_path_under_secret_is_404() {
    let srv = server();
    let (port, secret) = (srv.port(), srv.secret().to_string());
    let req = format!("GET /{secret}/nope.bin HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n");
    let r = http(port, req.as_bytes(), true);
    assert_eq!(r.status, 404);
}

#[test]
fn put_is_405() {
    let srv = server();
    let (port, secret) = (srv.port(), srv.secret().to_string());
    let req = format!(
        "PUT /{secret}/top.bin HTTP/1.1\r\nHost: x\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
    );
    let r = http(port, req.as_bytes(), true);
    assert_eq!(r.status, 405);
    assert!(r.header("Allow").is_some(), "405 carries an Allow header");
}

#[test]
fn oversized_body_closes_without_responding() {
    let srv = server();
    let (port, secret) = (srv.port(), srv.secret().to_string());
    // A Content-Length far above MAX_BODY (64 KiB): the server must close the
    // connection without responding — never pre-allocate a client-declared size.
    let req = format!(
        "PROPFIND /{secret}/ HTTP/1.1\r\nHost: x\r\nDepth: 0\r\nContent-Length: 200000\r\n\r\n"
    );
    let r = http(port, req.as_bytes(), true);
    assert_eq!(r.status, 0, "server closes with no valid HTTP status line");
}
