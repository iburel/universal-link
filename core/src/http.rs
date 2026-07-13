// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

//! The Core's minimal HTTP/1.1 client: OIDC discovery and token endpoint. It
//! runs on a stream opened by the [`Connector`], so over TLS as soon as the
//! binary wires one in.
//!
//! Enough for a real IdP, not a byte more: `Connection: close`, a body
//! delimited by `Content-Length` OR by `Transfer-Encoding: chunked` — Google's
//! token endpoint ALWAYS replies chunked, which is what motivated this module.
//! No redirects (OIDC endpoints do not do any), no compression (we announce no
//! `Accept-Encoding`, hence `identity`), no keep-alive.
//!
//! Everything is read as BYTES: `Content-Length` counts bytes, and a non-UTF-8
//! body (a proxy's Latin-1 error page…) must not be able to land an index on a
//! character boundary.

use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};

use crate::connector::{Connector, parse_url};

/// Each outbound HTTP call (discovery, token) is bounded.
const HTTP_TIMEOUT: Duration = Duration::from_secs(10);
/// Beyond this, the header is not a header.
const HEAD_MAX: usize = 16 * 1024;
/// No legitimate OIDC body comes close to this cap; a peer that writes without
/// end will not fill our memory.
const BODY_MAX: usize = 1024 * 1024;

/// GET (body `None`) or POST form.
pub(crate) async fn request(
    connector: &dyn Connector,
    url: &str,
    form_body: Option<String>,
) -> Result<(u16, String), String> {
    let location = parse_url(url).ok_or_else(|| format!("unreadable URL: {url}"))?;
    tokio::time::timeout(HTTP_TIMEOUT, async move {
        let mut stream = connector
            .connect(&location.target)
            .await
            .map_err(|e| format!("connecting to {}: {e}", location.authority))?;
        let (authority, path) = (&location.authority, &location.path);
        let head = match &form_body {
            Some(body) => format!(
                "POST {path} HTTP/1.1\r\nHost: {authority}\r\n\
                 Content-Type: application/x-www-form-urlencoded\r\n\
                 Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            ),
            None => {
                format!("GET {path} HTTP/1.1\r\nHost: {authority}\r\nConnection: close\r\n\r\n")
            }
        };
        stream
            .write_all(head.as_bytes())
            .await
            .map_err(|e| format!("HTTP write: {e}"))?;
        read_response(&mut stream).await
    })
    .await
    .map_err(|_| format!("HTTP timeout on {url}"))?
}

/// Reads a complete response: status line, headers, delimited body.
async fn read_response<R: AsyncRead + Unpin>(stream: &mut R) -> Result<(u16, String), String> {
    let mut reader = Reader::new(stream);
    let head = reader.head().await?;
    let status = head
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|s| s.parse().ok())
        .ok_or("unreadable status line")?;
    let body = if header(&head, "transfer-encoding").is_some_and(|v| {
        v.to_ascii_lowercase()
            .split(',')
            .any(|t| t.trim() == "chunked")
    }) {
        reader.chunked_body().await?
    } else if let Some(length) = header(&head, "content-length").and_then(|v| v.parse().ok()) {
        // The peer announced a length: hold to it, otherwise the response is
        // truncated and a partial body that parses is worse than an error.
        reader.exactly(length).await?
    } else {
        reader.until_close().await?
    };
    Ok((status, String::from_utf8_lossy(&body).into_owned()))
}

/// A header's value, case-insensitive. The first one wins.
fn header<'a>(head: &'a str, name: &str) -> Option<&'a str> {
    head.lines()
        .filter_map(|l| l.split_once(':'))
        .find(|(n, _)| n.trim().eq_ignore_ascii_case(name))
        .map(|(_, v)| v.trim())
}

/// Buffered reading of the stream: the body is delimited by the headers, never
/// by the end of the stream — except when the peer says nothing (last resort).
struct Reader<'a, R> {
    stream: &'a mut R,
    buf: Vec<u8>,
    /// First unconsumed byte of `buf`.
    pos: usize,
}

impl<'a, R: AsyncRead + Unpin> Reader<'a, R> {
    fn new(stream: &'a mut R) -> Reader<'a, R> {
        Reader {
            stream,
            buf: Vec::new(),
            pos: 0,
        }
    }

    fn pending(&self) -> &[u8] {
        &self.buf[self.pos..]
    }

    /// Reads one more block. `false`: the peer closed.
    async fn fill(&mut self) -> Result<bool, String> {
        if self.buf.len() > HEAD_MAX + BODY_MAX {
            return Err("oversized HTTP response".to_string());
        }
        let mut chunk = [0u8; 4096];
        match self.stream.read(&mut chunk).await {
            Ok(0) => Ok(false),
            Ok(n) => {
                self.buf.extend_from_slice(&chunk[..n]);
                Ok(true)
            }
            // TLS without `close_notify`: rustls signals it this way. The body
            // is delimited by the headers, so this abrupt end deprives us of
            // nothing — the delimiters are what check completeness.
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => Ok(false),
            Err(e) => Err(format!("HTTP read: {e}")),
        }
    }

    /// Status line + headers, without the trailing `\r\n\r\n`.
    async fn head(&mut self) -> Result<String, String> {
        loop {
            if let Some(i) = find(self.pending(), b"\r\n\r\n") {
                let end = self.pos + i;
                let head = String::from_utf8_lossy(&self.buf[self.pos..end]).into_owned();
                self.pos = end + 4;
                return Ok(head);
            }
            if self.pending().len() > HEAD_MAX {
                return Err("oversized HTTP headers".to_string());
            }
            if !self.fill().await? {
                return Err("HTTP response without headers".to_string());
            }
        }
    }

    /// A line terminated by CRLF, without the CRLF.
    async fn line(&mut self) -> Result<String, String> {
        loop {
            if let Some(i) = find(self.pending(), b"\r\n") {
                let line = String::from_utf8_lossy(&self.buf[self.pos..self.pos + i]).into_owned();
                self.pos += i + 2;
                return Ok(line);
            }
            if self.pending().len() > HEAD_MAX {
                return Err("oversized HTTP line".to_string());
            }
            if !self.fill().await? {
                return Err("truncated HTTP response".to_string());
            }
        }
    }

    /// Exactly `n` bytes, otherwise an error: a truncation must not pass for a
    /// valid body.
    async fn exactly(&mut self, n: usize) -> Result<Vec<u8>, String> {
        if n > BODY_MAX {
            return Err("oversized HTTP body".to_string());
        }
        while self.pending().len() < n {
            if !self.fill().await? {
                return Err("truncated HTTP body".to_string());
            }
        }
        let body = self.buf[self.pos..self.pos + n].to_vec();
        self.pos += n;
        Ok(body)
    }

    /// A `chunked` body (RFC 9112 §7.1): a sequence of hexadecimal sizes, an
    /// empty chunk terminates. Chunk extensions and trailers are read and
    /// ignored.
    async fn chunked_body(&mut self) -> Result<Vec<u8>, String> {
        let mut body = Vec::new();
        loop {
            let line = self.line().await?;
            // `1a;ext=1`: the size stops at the first `;`.
            let size_text = line.split(';').next().unwrap_or("").trim();
            let size = usize::from_str_radix(size_text, 16)
                .map_err(|_| format!("unreadable chunk size: {size_text:?}"))?;
            if size == 0 {
                // Trailers until the blank line (or end of stream).
                while let Ok(trailer) = self.line().await {
                    if trailer.is_empty() {
                        break;
                    }
                }
                return Ok(body);
            }
            if body.len() + size > BODY_MAX {
                return Err("oversized HTTP body".to_string());
            }
            body.extend_from_slice(&self.exactly(size).await?);
            if !self.line().await?.is_empty() {
                return Err("malformed chunk terminator".to_string());
            }
        }
    }

    /// Last resort: neither length nor chunked, the body runs until the close.
    /// This is the only case where a truncation is undetectable.
    async fn until_close(&mut self) -> Result<Vec<u8>, String> {
        while self.fill().await? {
            if self.pending().len() > BODY_MAX {
                return Err("oversized HTTP body".to_string());
            }
        }
        Ok(self.pending().to_vec())
    }
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn parse(raw: &str) -> Result<(u16, String), String> {
        read_response(&mut raw.as_bytes()).await
    }

    #[tokio::test]
    async fn reads_a_content_length_body() {
        let (status, body) = parse("HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nhi")
            .await
            .expect("response");
        assert_eq!((status, body.as_str()), (200, "hi"));
    }

    #[tokio::test]
    async fn a_content_length_body_that_is_short_is_an_error_not_a_body() {
        // A RST in the middle of the body: better to fail than to serve `{"id_`
        // to serde_json, which would fail anyway — but not always (a truncated
        // JSON can stay valid: `{"a":1}` then noise).
        let err = parse("HTTP/1.1 200 OK\r\nContent-Length: 10\r\n\r\nhi")
            .await
            .expect_err("truncated body");
        assert!(err.contains("truncated"), "{err}");
    }

    #[tokio::test]
    async fn reads_a_chunked_body() {
        // The exact shape of Google's token endpoint.
        let (status, body) = parse(
            "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n\
             5\r\n{\"a\":\r\n3\r\n1}\n\r\n0\r\n\r\n",
        )
        .await
        .expect("response");
        assert_eq!((status, body.as_str()), (200, "{\"a\":1}\n"));
    }

    #[tokio::test]
    async fn chunked_survives_extensions_and_trailers() {
        let (_, body) = parse(
            "HTTP/1.1 200 OK\r\ntransfer-encoding: CHUNKED\r\n\r\n\
             2;name=value\r\nok\r\n0\r\nTrailer: x\r\n\r\n",
        )
        .await
        .expect("response");
        assert_eq!(body, "ok");
    }

    #[tokio::test]
    async fn chunked_wins_over_content_length() {
        // A peer that sends both: the RFC says chunked prevails. Reading the 99
        // announced bytes would wait for nothing.
        let (_, body) = parse(
            "HTTP/1.1 200 OK\r\nContent-Length: 99\r\nTransfer-Encoding: chunked\r\n\r\n\
             2\r\nok\r\n0\r\n\r\n",
        )
        .await
        .expect("response");
        assert_eq!(body, "ok");
    }

    #[tokio::test]
    async fn a_body_without_delimiter_runs_to_the_close() {
        let (status, body) = parse("HTTP/1.1 502 Bad Gateway\r\n\r\nboom")
            .await
            .expect("response");
        assert_eq!((status, body.as_str()), (502, "boom"));
    }

    #[tokio::test]
    async fn a_non_utf8_body_does_not_panic() {
        // A proxy's Latin-1 error page: the indices are in bytes.
        let mut raw = b"HTTP/1.1 500 x\r\nContent-Length: 3\r\n\r\n".to_vec();
        raw.extend_from_slice(&[0xE9, 0xE8, 0xEA]);
        let (status, body) = read_response(&mut raw.as_slice()).await.expect("response");
        assert_eq!(status, 500);
        assert_eq!(body.chars().count(), 3, "three replacement characters");
    }

    #[tokio::test]
    async fn a_bad_chunk_size_is_refused() {
        let err = parse("HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\nzz\r\n")
            .await
            .expect_err("unreadable size");
        assert!(err.contains("chunk"), "{err}");
    }

    #[tokio::test]
    async fn headers_without_terminator_are_refused() {
        let err = parse("HTTP/1.1 200 OK\r\nContent-Length: 2\r\n")
            .await
            .expect_err("incomplete headers");
        assert!(err.contains("without headers"), "{err}");
    }
}
