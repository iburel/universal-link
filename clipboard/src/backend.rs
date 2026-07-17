// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

//! The seam between the OS-agnostic orchestrator and a platform clipboard
//! backend. A backend reports what the OS did through [`BackendEvent`] (upcalls,
//! pushed on an `mpsc::Sender<BackendEvent>` it is handed at construction) and
//! is driven by the orchestrator through [`ClipboardBackend`] (downcalls). Real
//! backends bind the OS event loop to the main thread and expose a cheap,
//! `Clone` handle here (channel senders to that thread); the test double
//! implements it directly.

use std::future::Future;
use std::path::PathBuf;

/// One normalized clipboard format. `id` is a Core-normalized identifier
/// (`text`, `image/png`, `files`); `size` is the advisory inline size hint,
/// absent for `files` and omitted for a `sensitive` clip.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Format {
    pub id: String,
    pub size: Option<u64>,
}

/// A local copy the OS backend observed — the metadata the orchestrator
/// announces (`clipboard.updated`). The bytes are not carried: inline formats
/// are pulled back from the backend at paste time, files are read by the Core
/// from `paths`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LocalClip {
    pub formats: Vec<Format>,
    /// Absolute local paths for a `files` copy; empty for an inline-only copy.
    pub paths: Vec<PathBuf>,
    /// The OS confidentiality markers were detected on the source.
    pub sensitive: bool,
}

/// One entry of a remote copy's frozen manifest (`clipboard.remote_updated`'s
/// `files`). `path` is relative, `/`-separated and already de-duplicated by the
/// announcing Core; the receiving Core has re-validated it fail-closed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemoteFile {
    pub file_id: String,
    pub path: String,
    pub size: u64,
    pub dir: bool,
}

/// A remote copy the orchestrator asks the backend to promise on the OS
/// clipboard. The backend takes ownership without the bytes; the orchestrator
/// pulls them from the source at paste time (a consumer channel).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemoteClip {
    pub tx_id: String,
    pub formats: Vec<Format>,
    pub files: Vec<RemoteFile>,
    pub sensitive: bool,
}

/// Serves on-demand byte ranges of a promised remote FILES clip. A files
/// backend (FUSE) calls this from a synchronous OS filesystem thread, so it is
/// BLOCKING and must be safe to call off any thread; the implementor bridges to
/// the IPC runtime internally.
pub trait FileFetcher: Send + Sync {
    /// Bytes [offset, offset+len) of manifest file `file_id`. Returns the
    /// intersection with the file (fewer than `len`, or empty, only at/after
    /// EOF). `Err` on a failed pull (TX_STALE / FILE_CHANGED / PEER_GONE / …):
    /// the OS read then fails cleanly, never a silent truncation.
    fn read(&self, file_id: &str, offset: u64, len: u64) -> std::io::Result<Vec<u8>>;
}

/// What a platform backend reports to the orchestrator.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BackendEvent {
    /// A local copy happened → announce it. `generation` is the backend's own
    /// monotonic id for this copy; the orchestrator hands it back to
    /// [`ClipboardBackend::provide`] so the backend can refuse to vouch for a
    /// clipboard that has since moved on.
    Copied { generation: u64, clip: LocalClip },
    /// The local clipboard was cleared → announce empty (supersedes).
    Cleared,
    /// A local paste needs the promised `format`. `token` correlates the
    /// eventual [`ClipboardBackend::deliver`]/[`ClipboardBackend::paste_failed`]
    /// back to this exact paste, so a delayed-rendering backend answers the
    /// precise request and drops stale replies.
    Paste { format: String, token: u64 },
}

/// The orchestrator's downcalls into a platform backend. A cheap-to-`Clone`
/// handle (the orchestrator clones it into per-paste tasks); real backends make
/// the methods forward to the main-thread OS loop.
pub trait ClipboardBackend: Clone + Send + Sync + 'static {
    /// Bytes for `format` of the local copy announced as `generation`, or
    /// `None` if the backend can no longer vouch for it (the OS clipboard moved
    /// on). `None` makes the orchestrator answer the paste with `CLIP_STALE`.
    fn provide(
        &self,
        generation: u64,
        format: &str,
    ) -> impl Future<Output = Option<Vec<u8>>> + Send;

    /// Take ownership of the OS clipboard, promising `clip` (a remote copy). If
    /// `clip.sensitive`, re-apply the OS confidentiality markers.
    fn offer(&self, clip: RemoteClip);

    /// Take ownership of the OS clipboard for a remote FILES clip, serving its
    /// bytes on demand through `fetcher`. DEFAULT: the platform has no files
    /// backend yet → make NO promise (clean refusal), leaving the OS clipboard
    /// untouched.
    fn offer_files(&self, _clip: RemoteClip, _fetcher: std::sync::Arc<dyn FileFetcher>) {}

    /// Deliver fetched bytes to the OS for the pending paste `token`.
    fn deliver(&self, token: u64, format: &str, bytes: Vec<u8>);

    /// The pending paste `token` could not be satisfied — release the promise
    /// cleanly (the paste is refused, never silently truncated).
    fn paste_failed(&self, token: u64, format: &str);

    /// Release OS ownership: the promise was withdrawn, superseded, or the
    /// component is shutting down.
    fn release(&self);
}
