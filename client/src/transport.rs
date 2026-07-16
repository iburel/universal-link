// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

//! The per-platform IPC stream: a Unix-domain socket on unix, a named pipe on
//! Windows. Shared by the managed control connection ([`crate::conn`]) and the
//! data channels ([`crate::channel`]) — both dial the same endpoint, the
//! control plane with a `hello`, a channel with a single-use `channel_token`.

use std::path::Path;

#[cfg(unix)]
pub(crate) type Stream = tokio::net::UnixStream;
#[cfg(windows)]
pub(crate) type Stream = tokio::net::windows::named_pipe::NamedPipeClient;

#[cfg(unix)]
pub(crate) async fn connect(path: &Path) -> std::io::Result<Stream> {
    tokio::net::UnixStream::connect(path).await
}

#[cfg(windows)]
pub(crate) async fn connect(path: &Path) -> std::io::Result<Stream> {
    use tokio::net::windows::named_pipe::ClientOptions;
    let name = path
        .to_str()
        .ok_or_else(|| std::io::Error::other("pipe name not UTF-8"))?;
    // All instances busy (ERROR_PIPE_BUSY): a failure like any other — the
    // caller retries (the control cycle's backoff; a channel open surfaces it).
    ClientOptions::new().open(name)
}
