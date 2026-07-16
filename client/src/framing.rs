// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

//! LSP-style framing: `Name: value\r\n`…, empty line, then `Content-Length`
//! bytes of UTF-8 JSON. Line endings `\r\n` (a lone `\n` is tolerated),
//! `Content-Length` case-insensitive, unknown headers ignored.
//!
//! Any violation is an error: the caller closes the connection —
//! fail-closed, and nothing is allocated beyond the ceilings.
//!
//! Deliberate copy of `core/src/framing.rs` (2nd) — a single framing grammar
//! in the project. To be extracted into a shared crate if a third copy
//! threatens.

use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt};

/// The control plane carries JSON-RPC only — payloads (clipboard blobs, file
/// ranges) ride the data channel (doc/core-api.md). The ceiling bounds
/// metadata; its heaviest legitimate frame is a full clipboard manifest,
/// itself capped well below this.
pub const MAX_FRAME_BYTES: usize = 16 * 1024 * 1024;
/// The header section has no reason to exceed a few lines.
pub const MAX_HEADER_BYTES: usize = 8 * 1024;

fn violation(what: &str) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, what.to_string())
}

/// Reads a frame. `Ok(None)` = clean EOF between two frames; `Err` =
/// framing violation or I/O error (the caller closes).
pub async fn read_frame<R: AsyncBufRead + Unpin>(
    reader: &mut R,
) -> std::io::Result<Option<String>> {
    let mut content_length: Option<usize> = None;
    let mut header_bytes: usize = 0;
    let mut line: Vec<u8> = Vec::new();

    loop {
        line.clear();
        // Bounded read: at most what the header ceiling still allows
        // (+1 to distinguish "exactly at the ceiling" from "overflows").
        let budget = (MAX_HEADER_BYTES - header_bytes + 1) as u64;
        let n = (&mut *reader)
            .take(budget)
            .read_until(b'\n', &mut line)
            .await?;
        if n == 0 {
            if header_bytes == 0 {
                return Ok(None); // clean EOF, between two frames
            }
            return Err(violation("EOF in the middle of the headers"));
        }
        header_bytes += n;
        if header_bytes > MAX_HEADER_BYTES {
            return Err(violation("header section too long"));
        }
        if !line.ends_with(b"\n") {
            return Err(violation("header line without line ending"));
        }

        let text = str::from_utf8(&line).map_err(|_| violation("header not UTF-8"))?;
        let text = text.trim_end_matches(['\r', '\n']);
        if text.is_empty() {
            break; // end of headers
        }
        let Some((name, value)) = text.split_once(':') else {
            return Err(violation("header line without a colon"));
        };
        if name.eq_ignore_ascii_case("content-length") {
            let len: usize = value
                .trim()
                .parse()
                .map_err(|_| violation("unreadable Content-Length"))?;
            if len > MAX_FRAME_BYTES {
                return Err(violation("Content-Length beyond the ceiling"));
            }
            content_length = Some(len);
        }
        // Unknown header: ignored (additive extensions).
    }

    let len = content_length.ok_or_else(|| violation("frame without Content-Length"))?;
    let mut payload = vec![0u8; len];
    reader.read_exact(&mut payload).await?;
    String::from_utf8(payload)
        .map(Some)
        .map_err(|_| violation("payload not UTF-8"))
}

/// Encodes `text` into a frame ready to write.
pub fn encode(text: &str) -> Vec<u8> {
    let mut bytes = format!("Content-Length: {}\r\n\r\n", text.len()).into_bytes();
    bytes.extend_from_slice(text.as_bytes());
    bytes
}
