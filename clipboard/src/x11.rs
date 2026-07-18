// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

//! The X11 / XWayland clipboard backend: an owner-driven, deferred-render
//! selection state machine that plugs into the frozen [`crate::backend`] seam.
//!
//! Ported from a private POC and adapted onto this crate's seam. Inline formats
//! (`text`, `image/png`) render deferred: the offer takes ownership, a paste
//! emits `Paste`, and `deliver` writes the bytes. `files` clips are different —
//! X11 has no paste event, so a promised files clip is exposed on demand and the
//! clipboard advertises URIs into it; the file manager then reads those files,
//! each read pulling one byte range on demand. The preferred backend is a FUSE
//! mount (see [`crate::fuse`]) advertising `file://` URIs; when FUSE is
//! unavailable, a non-sensitive clip falls back to a loopback WebDAV server (see
//! [`crate::webdav`]) advertising `dav://`/`webdav://` URIs. No INCR for inline
//! formats (large transfers are refused cleanly, marked `TODO(INCR)`).
//!
//! Two threads meet here. The non-`Send` [`xcb::Connection`] is pinned to the
//! MAIN thread inside [`Backend`], pumped by [`X11Loop::run`] (a `mio::Poll`
//! over the X socket fd and a `Waker`-backed command queue). The SIDE thread
//! runs the async orchestrator; it drives the OS through the cheap, `Clone`
//! [`X11Backend`] handle (each downcall pushes a [`Cmd`] then wakes the loop)
//! and observes local activity through the `BackendEvent` channel the loop
//! pushes on (`try_send`, never blocking the pump).
//!
//! Anti-echo: applying a paste writes onto the *requestor*'s window, not the
//! clipboard, so there is no apply→resurface loop. The only self-event is our
//! own `SetSelectionOwner` at offer time, ignored via `owner == our window`.

use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::os::unix::io::AsRawFd;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use mio::unix::SourceFd;
use mio::{Events, Interest, Poll, Token, Waker};
use tokio::sync::{mpsc, oneshot};
use xcb::{Xid, x, xfixes};

use crate::backend::{BackendEvent, ClipboardBackend, FileFetcher, Format, LocalClip, RemoteClip};
use crate::{files, fuse, webdav};

const XCB_TOKEN: Token = Token(0);
const CMD_TOKEN: Token = Token(1);

/// Deadline of a pending paste: past it, refuse cleanly (the app never freezes).
/// A safety net independent of `release()` when a disconnection is silent.
const PASTE_TIMEOUT: Duration = Duration::from_secs(30);

/// Deadline of a synchronous selection read (TARGETS / bytes) on the source side.
const READ_TIMEOUT: Duration = Duration::from_secs(2);

/// Cap of the timeout so `shutdown` and the paste deadline are always observed
/// even without a wake (a lost `Waker::wake` must not wedge the loop).
const SHUTDOWN_POLL_CAP: Duration = Duration::from_millis(250);

/// Cap of one eager read / one deferred write (a single `GetProperty` /
/// `ChangeProperty`). The X server already caps a request at ~16 MiB
/// (BIG-REQUESTS); past it INCR would be required — a later brick. A copy or a
/// paste larger than this is skipped/refused rather than truncated or fatal.
const MAX_EAGER_READ: usize = 16 * 1024 * 1024;

/// Bounded capacity of the upcall channel. Generous; the orchestrator drains
/// promptly, and a full queue only ever means it has stalled or gone.
const BACKEND_EVENT_CAPACITY: usize = 256;

/// Core v1 format strings.
const FORMAT_TEXT: &str = "text";
const FORMAT_PNG: &str = "image/png";
const FORMAT_FILES: &str = "files";

/// A downcall from the orchestrator, queued for the main-thread loop.
enum Cmd {
    /// Answer `provide(generation, format)` from the eager-read cache.
    Provide {
        generation: u64,
        format: String,
        reply: oneshot::Sender<Option<Vec<u8>>>,
    },
    /// Take ownership of CLIPBOARD, promising a remote clip (bytes come later).
    Offer(RemoteClip),
    /// Take ownership of CLIPBOARD for a remote FILES clip: expose the tree on
    /// demand (FUSE, else the WebDAV fallback) serving `fetcher`'s bytes and
    /// advertise the corresponding URIs.
    OfferFiles {
        clip: RemoteClip,
        fetcher: Arc<dyn FileFetcher>,
    },
    /// Fetched bytes for the pending paste `token`: complete the request.
    Deliver {
        token: u64,
        format: String,
        bytes: Vec<u8>,
    },
    /// The pending paste `token` could not be satisfied: refuse cleanly.
    PasteFailed { token: u64, format: String },
    /// Drop OS ownership (promise withdrawn / superseded / shutting down).
    Release,
    /// Stop the loop with this process exit code (dropping ownership first).
    Exit(i32),
}

/// The cheap, `Clone` handle the orchestrator holds: a command queue and the
/// loop's `Waker`. Carries no X11 resource. Each downcall pushes a [`Cmd`] then
/// wakes the loop (push BEFORE wake, or a coalesced wake could drop the
/// command; a poisoned mutex is recovered so `Exit`/`Release` are never lost).
#[derive(Clone)]
pub struct X11Backend {
    cmds: Arc<Mutex<VecDeque<Cmd>>>,
    waker: Arc<Waker>,
}

impl X11Backend {
    fn push(&self, cmd: Cmd) {
        match self.cmds.lock() {
            Ok(mut q) => q.push_back(cmd),
            Err(p) => p.into_inner().push_back(cmd),
        }
        // A lost wake leaves a queued command, but the loop caps its timeout at
        // SHUTDOWN_POLL_CAP and re-checks, so it never wedges indefinitely.
        if let Err(e) = self.waker.wake() {
            warn(&format!("waking the X11 loop failed: {e}"));
        }
    }

    /// Queue a request to stop the loop with `code` (from another thread).
    pub fn request_exit(&self, code: i32) {
        self.push(Cmd::Exit(code));
    }
}

impl ClipboardBackend for X11Backend {
    fn provide(
        &self,
        generation: u64,
        format: &str,
    ) -> impl Future<Output = Option<Vec<u8>>> + Send {
        // Push synchronously (before returning the future) so the ordering with
        // other downcalls is preserved even if the future is polled later.
        let (reply, rx) = oneshot::channel();
        self.push(Cmd::Provide {
            generation,
            format: format.to_string(),
            reply,
        });
        // A dropped sender (loop gone) resolves to `None` → CLIP_STALE.
        async move { rx.await.unwrap_or(None) }
    }

    fn offer(&self, clip: RemoteClip) {
        self.push(Cmd::Offer(clip));
    }

    fn offer_files(&self, clip: RemoteClip, fetcher: Arc<dyn FileFetcher>) {
        self.push(Cmd::OfferFiles { clip, fetcher });
    }

    fn deliver(&self, token: u64, format: &str, bytes: Vec<u8>) {
        self.push(Cmd::Deliver {
            token,
            format: format.to_string(),
            bytes,
        });
    }

    fn paste_failed(&self, token: u64, format: &str) {
        self.push(Cmd::PasteFailed {
            token,
            format: format.to_string(),
        });
    }

    fn release(&self) {
        self.push(Cmd::Release);
    }
}

/// Owns the pinned [`Backend`]; [`run`](Self::run) is the blocking main-thread
/// pump. Returns the process exit code once the loop stops.
pub struct X11Loop {
    backend: Backend,
}

impl X11Loop {
    /// Pumps the X11 event loop on the calling (main) thread until a
    /// [`Cmd::Exit`] (or a vanished orchestrator) stops it, dropping selection
    /// ownership on the way out. Returns the requested exit code.
    pub fn run(mut self) -> i32 {
        self.backend.run()
    }
}

/// The interned atoms this backend uses, plus the atom↔format mapping. The map
/// helpers are pure (only compare atom fields), so they are unit-tested against
/// a hand-built `Atoms` without any `Connection`.
struct Atoms {
    clipboard: x::Atom,
    targets: x::Atom,
    timestamp: x::Atom,
    utf8_string: x::Atom,
    string: x::Atom,
    text: x::Atom,
    image_png: x::Atom,
    /// Scratch property we read our own `ConvertSelection` results into.
    prop: x::Atom,
    /// KDE confidentiality hint: advertised for a sensitive offer, and answered
    /// with `"secret"` so Klipper/KWallet keep it out of history.
    kde_password_hint: x::Atom,
    /// File-manager copy targets. `uri_list` is the RFC 2483 `text/uri-list`;
    /// `gnome_copied`/`kde_copied` are the `copy\n…` variants GNOME/KDE prefer.
    uri_list: x::Atom,
    gnome_copied: x::Atom,
    kde_copied: x::Atom,
}

fn intern(conn: &xcb::Connection, name: &[u8]) -> Result<x::Atom, String> {
    let c = conn.send_request(&x::InternAtom {
        only_if_exists: false,
        name,
    });
    conn.wait_for_reply(c)
        .map(|r| r.atom())
        .map_err(|e| format!("InternAtom {}: {e:?}", String::from_utf8_lossy(name)))
}

impl Atoms {
    fn intern_all(conn: &xcb::Connection) -> Result<Self, String> {
        Ok(Self {
            clipboard: intern(conn, b"CLIPBOARD")?,
            targets: intern(conn, b"TARGETS")?,
            timestamp: intern(conn, b"TIMESTAMP")?,
            utf8_string: intern(conn, b"UTF8_STRING")?,
            string: intern(conn, b"STRING")?,
            text: intern(conn, b"TEXT")?,
            image_png: intern(conn, b"image/png")?,
            prop: intern(conn, b"UNIVERSALLINK_SELECTION")?,
            kde_password_hint: intern(conn, b"x-kde-passwordManagerHint")?,
            uri_list: intern(conn, b"text/uri-list")?,
            gnome_copied: intern(conn, b"x-special/gnome-copied-files")?,
            kde_copied: intern(conn, b"x-special/KDE-copied-files")?,
        })
    }

    /// Detect: which Core format an advertised target atom maps to, if any.
    /// Text priority is expressed by the read order, not here.
    fn format_for_target(&self, target: x::Atom) -> Option<&'static str> {
        if target == self.utf8_string || target == self.string || target == self.text {
            Some(FORMAT_TEXT)
        } else if target == self.image_png {
            Some(FORMAT_PNG)
        } else if target == self.uri_list
            || target == self.gnome_copied
            || target == self.kde_copied
        {
            Some(FORMAT_FILES)
        } else {
            None
        }
    }

    /// Advertise: the target atoms to publish for a Core format. Unknown formats
    /// publish nothing (the paste is refused cleanly).
    fn targets_for_format(&self, format: &str) -> Vec<x::Atom> {
        match format {
            FORMAT_TEXT => vec![self.utf8_string, self.string, self.text],
            FORMAT_PNG => vec![self.image_png],
            FORMAT_FILES => vec![self.uri_list, self.gnome_copied, self.kde_copied],
            _ => vec![],
        }
    }

    /// Answer a paste: the property TYPE atom for a Core format. Text is ALWAYS
    /// UTF8_STRING (our bytes are UTF-8; typing them STRING/ISO-8859-1 would
    /// mojibake), image is image/png.
    fn type_atom_for_format(&self, format: &str) -> x::Atom {
        match format {
            FORMAT_PNG => self.image_png,
            _ => self.utf8_string,
        }
    }
}

/// A paste request awaiting bytes from the orchestrator (ICCCM: at most one in
/// flight). Deferred until `deliver`/`paste_failed`/timeout.
struct PendingPaste {
    requestor: x::Window,
    property: x::Atom,
    /// The target the requestor asked for. Echoed verbatim in the answering
    /// `SelectionNotify` (ICCCM §2.2 requires it), even when the property TYPE
    /// differs — e.g. a STRING/TEXT request whose bytes we render UTF8_STRING.
    target: x::Atom,
    /// Property TYPE, derived from the Core format (not the requested target).
    type_atom: x::Atom,
    time: x::Timestamp,
    deadline: Instant,
    /// Correlation token minted from `next_paste_token`.
    token: u64,
}

/// The per-generation eager-read cache: the bytes of the local copy announced
/// as `generation`, keyed by Core format. A pure struct (no xcb) so its
/// generation-gate is unit-tested directly.
#[derive(Default)]
struct Cache {
    generation: u64,
    bytes_by_format: HashMap<String, Vec<u8>>,
}

impl Cache {
    fn store(&mut self, generation: u64, bytes_by_format: HashMap<String, Vec<u8>>) {
        self.generation = generation;
        self.bytes_by_format = bytes_by_format;
    }

    /// Bytes for `format` iff the cache still holds exactly `generation`.
    fn get(&self, generation: u64, format: &str) -> Option<Vec<u8>> {
        if self.generation == generation {
            self.bytes_by_format.get(format).cloned()
        } else {
            None
        }
    }

    /// Drop the local capture (a remote promise supersedes it).
    fn invalidate(&mut self) {
        self.bytes_by_format.clear();
    }

    /// Whether the cache holds any bytes — i.e. a live local capture we could
    /// still vouch for.
    fn is_empty(&self) -> bool {
        self.bytes_by_format.is_empty()
    }
}

/// The live on-demand backend serving a promised remote FILES clip. FUSE is
/// preferred (a real `file://` path any app can read); WebDAV is the fallback
/// when FUSE is unavailable. Dropping either variant tears the backend down, so
/// `Backend` needs no explicit teardown beyond clearing this field. The payload
/// is a pure RAII guard — its bytes are served through the URIs captured at offer
/// time, so it is only ever "read" by its own `Drop`.
#[allow(dead_code)]
enum FilesMount {
    Fuse(fuse::FuseMount),
    WebDav(webdav::WebDavMount),
}

/// Backend state, living on the X11 (main) thread. Owns the non-`Send`
/// connection.
struct Backend {
    conn: xcb::Connection,
    window: x::Window,
    atoms: Atoms,
    poll: Poll,
    cmds: Arc<Mutex<VecDeque<Cmd>>>,
    /// Bounded upcall channel to the orchestrator (never blocks the loop).
    events_tx: mpsc::Sender<BackendEvent>,
    /// Content targets currently published (derived from the last offer).
    advertised: Vec<x::Atom>,
    /// The live on-demand backend of a promised remote FILES clip (if any): a
    /// FUSE mount, or the WebDAV fallback. Held for the offer's lifetime; dropping
    /// it unmounts / stops the server.
    current_files: Option<FilesMount>,
    /// URIs advertised for the GNOME / `text/uri-list` file-manager targets: a
    /// `file://` list for FUSE, a `dav://` list for WebDAV.
    offer_uris: Vec<String>,
    /// URIs advertised for the KDE (`x-special/KDE-copied-files`) target:
    /// identical to `offer_uris` for FUSE, a `webdav://` list for WebDAV (Dolphin
    /// rejects `dav://`, KDE bug 365356).
    offer_uris_kde: Vec<String>,
    pending: Option<PendingPaste>,
    /// Whether we own CLIPBOARD (an offer is live).
    owned: bool,
    /// Whether the previous CLIPBOARD owner was foreign — gates the `Cleared`
    /// upcall so our own `release()` does not masquerade as a foreign clear.
    last_owner_foreign: bool,
    /// Monotonic id per local copy (announce generation).
    next_generation: u64,
    /// Monotonic id per deferred paste (correlation token). SEPARATE from
    /// `next_generation`: conflating them breaks the seam.
    next_paste_token: u64,
    cache: Cache,
    /// A CLIPBOARD owner change observed *during* a synchronous read; the main
    /// loop reconciles it once the read returns (never nested inside a read).
    pending_owner_change: Option<x::Window>,
    shutdown: bool,
    exit_code: Option<i32>,
}

impl Drop for Backend {
    /// Safety net at thread death: drop ownership and destroy the window, so an
    /// orphaned offer never wedges other apps' pastes.
    fn drop(&mut self) {
        if self.owned {
            self.conn.send_request(&x::SetSelectionOwner {
                owner: x::Window::none(),
                selection: self.atoms.clipboard,
                time: x::CURRENT_TIME,
            });
        }
        self.conn.send_request(&x::DestroyWindow {
            window: self.window,
        });
        let _ = self.conn.flush();
    }
}

impl Backend {
    fn run(&mut self) -> i32 {
        let mut events = Events::with_capacity(8);
        loop {
            if let Err(e) = self.conn.flush() {
                warn(&format!("flush (main loop) failed, stopping: {e:?}"));
                return self.exit_code.unwrap_or(1);
            }
            let timeout = self
                .pending
                .as_ref()
                .map(|p| p.deadline.saturating_duration_since(Instant::now()))
                .map_or(SHUTDOWN_POLL_CAP, |d| d.min(SHUTDOWN_POLL_CAP));
            if let Err(e) = self.poll.poll(&mut events, Some(timeout)) {
                if e.kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                warn(&format!("mio poll (main loop) failed, stopping: {e}"));
                return self.exit_code.unwrap_or(1);
            }
            // Reconcile unconditionally rather than per-token. Under mio's
            // edge-triggered epoll a readiness can be consumed without the work
            // being done — a command queued during a synchronous read, or X
            // events libxcb buffered while `take_ownership` awaited a reply — so
            // it would not re-fire. Draining both queues every iteration (cheap
            // when empty, and the poll is capped at SHUTDOWN_POLL_CAP) closes
            // those gaps. Commands run BEFORE X events on purpose: a `deliver`/
            // `paste_failed` for the current pending paste must land before a
            // new `SelectionRequest` could replace it.
            self.process_cmds();
            self.drain_xcb();
            // An owner change captured during a read (which must not re-enter a
            // read) is acted on here, at the top level, so a concurrent clear is
            // not lost.
            while let Some(owner) = self.pending_owner_change.take() {
                self.on_clipboard_update(owner);
            }
            self.check_paste_timeout();
            if self.shutdown {
                self.on_release();
                let _ = self.conn.flush();
                return self.exit_code.unwrap_or(1);
            }
        }
    }

    /// Push a `BackendEvent` upcall without ever blocking the loop. A closed
    /// channel means the orchestrator is gone → stop.
    fn emit(&mut self, event: BackendEvent) {
        use mpsc::error::TrySendError;
        match self.events_tx.try_send(event) {
            Ok(()) => {}
            Err(TrySendError::Closed(_)) => self.shutdown = true,
            Err(TrySendError::Full(_)) => warn("backend event queue full; dropping an event"),
        }
    }

    /// Drain all buffered X events (edge-triggered epoll → empty fully).
    fn drain_xcb(&mut self) {
        loop {
            match self.conn.poll_for_event() {
                Ok(Some(xcb::Event::XFixes(xfixes::Event::SelectionNotify(e)))) => {
                    if e.selection() == self.atoms.clipboard {
                        self.on_clipboard_update(e.owner());
                    }
                }
                Ok(Some(other)) => self.dispatch_xcb_event(other),
                Ok(None) => break,
                Err(xcb::Error::Protocol(e)) => {
                    // Recoverable (e.g. BadWindow from a SendEvent/ChangeProperty
                    // to a requestor that vanished): the connection is fine —
                    // log and keep draining the rest of the queue.
                    warn(&format!("X protocol error (ignored): {e:?}"));
                }
                Err(xcb::Error::Connection(e)) => {
                    // The connection is gone: stop so the supervisor restarts us.
                    warn(&format!("X connection lost, stopping: {e:?}"));
                    self.shutdown = true;
                    break;
                }
            }
        }
    }

    /// Non-XFixes X events (reused by the read sub-loop, which deliberately
    /// skips XFixes so it never re-enters a `ConvertSelection`).
    fn dispatch_xcb_event(&mut self, ev: xcb::Event) {
        match ev {
            xcb::Event::X(x::Event::SelectionRequest(e)) => self.on_selection_request(&e),
            xcb::Event::X(x::Event::SelectionClear(e))
                if e.selection() == self.atoms.clipboard && self.owned =>
            {
                // Another app took the selection (or an echo of our release):
                // supersede — drop ownership state and any live files offer.
                self.owned = false;
                self.advertised.clear();
                self.drop_files_offer();
            }
            _ => {}
        }
    }

    // ----- Source side: a local copy → eager read → Copied/Cleared -----

    fn on_clipboard_update(&mut self, owner: x::Window) {
        if owner == self.window {
            // Our own SetSelectionOwner (an offer): anti-echo.
            self.last_owner_foreign = false;
            return;
        }
        if owner == x::Window::none() {
            // Suppressed unless the cleared owner was foreign — our own
            // release() also lands here (owner none) but must not announce.
            if self.last_owner_foreign {
                self.emit(BackendEvent::Cleared);
            }
            self.last_owner_foreign = false;
            return;
        }
        self.last_owner_foreign = true;
        self.read_and_announce_copy();
    }

    /// Eager-read EVERY inline format the foreign owner offers, cache the bytes
    /// under a fresh generation, and announce the metadata.
    fn read_and_announce_copy(&mut self) {
        let Some(targets) = self.query_targets() else {
            // The new owner would not (or could not) tell us its targets; we can
            // no longer vouch for our previous capture.
            self.supersede_local();
            return;
        };
        let sensitive = targets.contains(&self.atoms.kde_password_hint);

        // Files take priority: a files copy often ALSO offers text (the dropped
        // paths) that must not be mistaken for content. Read the URIs, and if any
        // parse, announce a files copy and stop — the Core reads the paths, so no
        // inline bytes are cached (a sentinel keeps the local capture "live" so a
        // later unsupported foreign copy still supersedes it).
        if (targets.contains(&self.atoms.uri_list) || targets.contains(&self.atoms.gnome_copied))
            && let Some(paths) = self.read_file_uris(&targets)
        {
            let generation = self.next_generation;
            self.next_generation += 1;
            let mut sentinel: HashMap<String, Vec<u8>> = HashMap::new();
            sentinel.insert(FORMAT_FILES.to_string(), Vec::new());
            self.cache.store(generation, sentinel);
            self.emit(BackendEvent::Copied {
                generation,
                clip: LocalClip {
                    formats: vec![Format {
                        id: FORMAT_FILES.to_string(),
                        size: None,
                    }],
                    paths,
                    sensitive,
                },
            });
            return;
        }

        let mut bytes_by_format: HashMap<String, Vec<u8>> = HashMap::new();
        let mut formats: Vec<Format> = Vec::new();

        if let Some(bytes) = self.read_text(&targets) {
            formats.push(Format {
                id: FORMAT_TEXT.to_string(),
                size: size_hint(sensitive, bytes.len()),
            });
            bytes_by_format.insert(FORMAT_TEXT.to_string(), bytes);
        }
        if targets.contains(&self.atoms.image_png)
            && let Some(bytes) = self.convert_and_read(self.atoms.image_png)
        {
            formats.push(Format {
                id: FORMAT_PNG.to_string(),
                size: size_hint(sensitive, bytes.len()),
            });
            bytes_by_format.insert(FORMAT_PNG.to_string(), bytes);
        }

        if formats.is_empty() {
            // The new owner offers nothing we support (or all over the cap):
            // supersede the stale capture instead of continuing to vouch for it.
            self.supersede_local();
            return;
        }
        let generation = self.next_generation;
        self.next_generation += 1;
        self.cache.store(generation, bytes_by_format);
        self.emit(BackendEvent::Copied {
            generation,
            clip: LocalClip {
                formats,
                paths: Vec::new(),
                sensitive,
            },
        });
    }

    /// A foreign owner took CLIPBOARD but we could not capture a usable clip
    /// (unreadable, or only unsupported formats). Drop the stale local capture
    /// and announce a clear, so the orchestrator stops vouching for a generation
    /// the OS clipboard has already moved past — otherwise `provide` would keep
    /// serving bytes the user has replaced (a staleness + confidentiality bug).
    fn supersede_local(&mut self) {
        if !self.cache.is_empty() {
            self.cache.invalidate();
            self.emit(BackendEvent::Cleared);
        }
    }

    /// Read the text bytes as Core `text` (UTF-8). UTF8_STRING is declared
    /// UTF-8, so it is taken as-is. STRING (ISO-8859-1) and TEXT (possibly
    /// COMPOUND_TEXT) are only usable when their bytes happen to be valid UTF-8
    /// (e.g. pure ASCII); otherwise they are skipped rather than announced as
    /// mojibake — transcoding legacy encodings is out of scope. (`text == UTF-8`
    /// is the seam invariant; the delivery side always types text UTF8_STRING.)
    fn read_text(&mut self, targets: &[x::Atom]) -> Option<Vec<u8>> {
        if targets.contains(&self.atoms.utf8_string)
            && let Some(bytes) = self.convert_and_read(self.atoms.utf8_string)
        {
            return Some(bytes);
        }
        for atom in [self.atoms.string, self.atoms.text] {
            if targets.contains(&atom)
                && let Some(bytes) = self.convert_and_read(atom)
                && std::str::from_utf8(&bytes).is_ok()
            {
                return Some(bytes);
            }
        }
        None
    }

    /// Read the copied `file://` URIs as a files copy: convert `text/uri-list`
    /// (preferred) else `x-special/gnome-copied-files`, parse the URIs into
    /// absolute local paths. `None` if the list is empty or unreadable (the
    /// caller then falls through to text/image). The Core enumerates and reads
    /// the paths itself; only the top-level paths are announced.
    fn read_file_uris(&mut self, targets: &[x::Atom]) -> Option<Vec<PathBuf>> {
        let raw = if targets.contains(&self.atoms.uri_list) {
            self.convert_and_read(self.atoms.uri_list)
        } else {
            self.convert_and_read(self.atoms.gnome_copied)
        }?;
        let paths = files::parse_uri_list(&raw);
        if paths.is_empty() { None } else { Some(paths) }
    }

    fn query_targets(&mut self) -> Option<Vec<x::Atom>> {
        self.conn.send_request(&x::ConvertSelection {
            requestor: self.window,
            selection: self.atoms.clipboard,
            target: self.atoms.targets,
            property: self.atoms.prop,
            time: x::CURRENT_TIME,
        });
        if self.conn.flush().is_err() {
            return None;
        }
        if !self.wait_selection_notify() {
            return None;
        }
        let cookie = self.conn.send_request(&x::GetProperty {
            delete: true,
            window: self.window,
            property: self.atoms.prop,
            r#type: x::ATOM_ATOM,
            long_offset: 0,
            long_length: 1024,
        });
        let reply = self.conn.wait_for_reply(cookie).ok()?;
        if reply.format() != 32 {
            return None;
        }
        Some(reply.value::<x::Atom>().to_vec())
    }

    /// `ConvertSelection` a target then read (bounded) the resulting property.
    fn convert_and_read(&mut self, target: x::Atom) -> Option<Vec<u8>> {
        self.conn.send_request(&x::ConvertSelection {
            requestor: self.window,
            selection: self.atoms.clipboard,
            target,
            property: self.atoms.prop,
            time: x::CURRENT_TIME,
        });
        if self.conn.flush().is_err() {
            return None;
        }
        if !self.wait_selection_notify() {
            return None;
        }
        let cookie = self.conn.send_request(&x::GetProperty {
            delete: true,
            window: self.window,
            property: self.atoms.prop,
            r#type: x::ATOM_ANY,
            long_offset: 0,
            long_length: (MAX_EAGER_READ / 4) as u32,
        });
        let reply = self.conn.wait_for_reply(cookie).ok()?;
        if reply.format() != 8 {
            return None;
        }
        if reply.bytes_after() > 0 {
            // TODO(INCR): later brick — a copy larger than one GetProperty is
            // skipped rather than announced/served truncated.
            warn(&format!(
                "local copy > {MAX_EAGER_READ} bytes — not announced (TODO(INCR))"
            ));
            return None;
        }
        Some(reply.value::<u8>().to_vec())
    }

    /// Sub-loop: wait for a `SelectionNotify` on OUR property, with a deadline.
    /// `true` if the property is set, `false` on refusal/timeout/disconnect.
    /// Processes commands while waiting (so `release`/`Exit` are not starved)
    /// but skips XFixes to avoid a re-entrant `ConvertSelection`.
    fn wait_selection_notify(&mut self) -> bool {
        let deadline = Instant::now() + READ_TIMEOUT;
        let mut events = Events::with_capacity(8);
        loop {
            loop {
                match self.conn.poll_for_event() {
                    Ok(Some(xcb::Event::X(x::Event::SelectionNotify(e)))) => {
                        if e.property() == x::ATOM_NONE {
                            return false; // owner refused
                        }
                        if e.property() == self.atoms.prop {
                            return true;
                        }
                        // A SelectionNotify from another read: ignored.
                    }
                    Ok(Some(xcb::Event::XFixes(xfixes::Event::SelectionNotify(e))))
                        if e.selection() == self.atoms.clipboard =>
                    {
                        // A concurrent CLIPBOARD owner change during our read.
                        // Remember the latest owner and reconcile once the read
                        // returns (never re-enter a read here); otherwise the
                        // change — e.g. a clear — would be silently dropped.
                        self.pending_owner_change = Some(e.owner());
                    }
                    Ok(Some(other)) => self.dispatch_xcb_event(other),
                    Ok(None) => break,
                    Err(xcb::Error::Protocol(e)) => {
                        warn(&format!("X protocol error during read (ignored): {e:?}"));
                    }
                    Err(xcb::Error::Connection(e)) => {
                        warn(&format!("X connection lost during read: {e:?}"));
                        self.shutdown = true;
                        return false;
                    }
                }
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return false;
            }
            if self.conn.flush().is_err() {
                return false;
            }
            if let Err(e) = self.poll.poll(&mut events, Some(remaining)) {
                if e.kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                return false;
            }
            // Commands are deliberately NOT processed during a read: running an
            // `offer`/`deliver` here would mutate selection state under the
            // in-flight ConvertSelection. They stay queued (their wake edge is
            // consumed harmlessly) and the main loop runs them once the read
            // returns, within SHUTDOWN_POLL_CAP.
        }
    }

    // ----- Destination side: we own the selection, answer deferred -----

    fn on_selection_request(&mut self, e: &x::SelectionRequestEvent) {
        let target = e.target();
        if target == self.atoms.targets {
            self.respond_targets(e);
        } else if target == self.atoms.timestamp {
            self.respond_timestamp(e);
        } else if target == self.atoms.kde_password_hint && self.advertised.contains(&target) {
            self.respond_secret(e);
        } else if self.advertised.contains(&target)
            && self.current_files.is_some()
            && (target == self.atoms.uri_list
                || target == self.atoms.gnome_copied
                || target == self.atoms.kde_copied)
        {
            // A files offer: the URIs are known at offer time (they point into
            // the FUSE mount), so answer synchronously — no deferred render.
            self.respond_files(e, target);
        } else if self.advertised.contains(&target) {
            self.start_deferred(e, target);
        } else {
            self.notify(e.requestor(), e.target(), x::ATOM_NONE, e.time());
        }
    }

    /// Answer a files paste synchronously. The property TYPE is the REQUESTED
    /// target atom itself (not UTF8_STRING): `text/uri-list` bytes for the
    /// uri-list target, `copy\n…` bytes for the GNOME/KDE variants. The KDE
    /// variant draws on the KDE URI list (a `webdav://` scheme for a WebDAV
    /// offer), every other target on the default list.
    fn respond_files(&self, e: &x::SelectionRequestEvent, target: x::Atom) {
        let uris = if target == self.atoms.kde_copied {
            &self.offer_uris_kde
        } else {
            &self.offer_uris
        };
        let bytes = if target == self.atoms.uri_list {
            files::uri_list_bytes(uris)
        } else {
            files::copied_files_bytes(uris)
        };
        let property = property_or_target(e);
        self.conn.send_request(&x::ChangeProperty {
            mode: x::PropMode::Replace,
            window: e.requestor(),
            property,
            r#type: target,
            data: &bytes,
        });
        self.notify(e.requestor(), e.target(), property, e.time());
    }

    fn respond_targets(&self, e: &x::SelectionRequestEvent) {
        let mut targets = vec![self.atoms.targets, self.atoms.timestamp];
        targets.extend_from_slice(&self.advertised);
        let property = property_or_target(e);
        self.conn.send_request(&x::ChangeProperty {
            mode: x::PropMode::Replace,
            window: e.requestor(),
            property,
            r#type: x::ATOM_ATOM,
            data: &targets,
        });
        self.notify(e.requestor(), e.target(), property, e.time());
    }

    fn respond_timestamp(&self, e: &x::SelectionRequestEvent) {
        // No stored ownership timestamp: CURRENT_TIME is enough in practice.
        let t: x::Timestamp = x::CURRENT_TIME;
        let property = property_or_target(e);
        self.conn.send_request(&x::ChangeProperty {
            mode: x::PropMode::Replace,
            window: e.requestor(),
            property,
            r#type: x::ATOM_INTEGER,
            data: &[t],
        });
        self.notify(e.requestor(), e.target(), property, e.time());
    }

    /// Answer the KDE confidentiality hint with `"secret"` (a sensitive offer).
    fn respond_secret(&self, e: &x::SelectionRequestEvent) {
        let property = property_or_target(e);
        self.conn.send_request(&x::ChangeProperty {
            mode: x::PropMode::Replace,
            window: e.requestor(),
            property,
            r#type: self.atoms.utf8_string,
            data: "secret".as_bytes(),
        });
        self.notify(e.requestor(), e.target(), property, e.time());
    }

    /// Owner-driven core: do NOT answer now. Record the request, mint a token,
    /// and emit `Paste`; the reply leaves later from `deliver`/`paste_failed`.
    fn start_deferred(&mut self, e: &x::SelectionRequestEvent, target: x::Atom) {
        let Some(format) = self.atoms.format_for_target(target) else {
            self.notify(e.requestor(), e.target(), x::ATOM_NONE, e.time());
            return;
        };
        // Files never render deferred: they are answered synchronously from a
        // live FUSE mount (`respond_files`). Reaching here for a file target
        // means the mount is gone (a torn-down offer whose atoms still linger in
        // `advertised`) — refuse cleanly rather than emit a spurious `Paste`.
        if format == FORMAT_FILES {
            self.notify(e.requestor(), e.target(), x::ATOM_NONE, e.time());
            return;
        }
        // ICCCM: at most one paste in flight — refuse and replace the previous.
        if let Some(prev) = self.pending.take() {
            self.refuse(&prev);
        }
        let token = self.next_paste_token;
        self.next_paste_token += 1;
        let property = property_or_target(e);
        let type_atom = self.atoms.type_atom_for_format(format);
        self.pending = Some(PendingPaste {
            requestor: e.requestor(),
            property,
            target,
            type_atom,
            time: e.time(),
            deadline: Instant::now() + PASTE_TIMEOUT,
            token,
        });
        self.emit(BackendEvent::Paste {
            format: format.to_string(),
            token,
        });
    }

    fn on_deliver(&mut self, token: u64, _format: &str, bytes: Vec<u8>) {
        // Only honor the current paste; a stale deliver (paste replaced) is
        // dropped so we never write the wrong bytes.
        let pending = match &self.pending {
            Some(p) if p.token == token => self.pending.take().unwrap(),
            _ => return,
        };
        if bytes.len() > MAX_EAGER_READ {
            // TODO(INCR): later brick — refuse cleanly rather than have libxcb
            // close the connection on an over-length ChangeProperty.
            warn(&format!(
                "paste bytes ({}) over the {MAX_EAGER_READ} cap — refusing (TODO(INCR))",
                bytes.len()
            ));
            self.refuse(&pending);
            return;
        }
        self.conn.send_request(&x::ChangeProperty {
            mode: x::PropMode::Replace,
            window: pending.requestor,
            property: pending.property,
            r#type: pending.type_atom,
            data: &bytes,
        });
        self.notify(
            pending.requestor,
            pending.target,
            pending.property,
            pending.time,
        );
    }

    fn on_paste_failed(&mut self, token: u64, _format: &str) {
        if matches!(&self.pending, Some(p) if p.token == token) {
            let p = self.pending.take().unwrap();
            self.refuse(&p);
        }
    }

    /// Clean refusal: `SelectionNotify` with property NONE (the app never
    /// freezes, the paste is never truncated).
    fn refuse(&self, p: &PendingPaste) {
        self.notify(p.requestor, p.target, x::ATOM_NONE, p.time);
    }

    fn notify(&self, requestor: x::Window, target: x::Atom, property: x::Atom, time: x::Timestamp) {
        self.conn.send_request(&x::SendEvent {
            propagate: false,
            destination: x::SendEventDest::Window(requestor),
            event_mask: x::EventMask::empty(),
            event: &x::SelectionNotifyEvent::new(
                time,
                requestor,
                self.atoms.clipboard,
                target,
                property,
            ),
        });
        // A dead requestor must not kill the loop: log and carry on.
        if let Err(e) = self.conn.flush() {
            warn(&format!("flush (notify) failed: {e:?}"));
        }
    }

    fn check_paste_timeout(&mut self) {
        let expired = self
            .pending
            .as_ref()
            .is_some_and(|p| Instant::now() >= p.deadline);
        if expired {
            let p = self.pending.take().unwrap();
            self.refuse(&p);
        }
    }

    // ----- Orchestrator commands -----

    fn process_cmds(&mut self) {
        // Drain the WHOLE queue under one lock (a coalesced wake may carry
        // several). Recover a poisoned mutex so Exit/Release are never dropped.
        let drained: Vec<Cmd> = {
            let mut guard = match self.cmds.lock() {
                Ok(g) => g,
                Err(p) => p.into_inner(),
            };
            guard.drain(..).collect()
        };
        for cmd in drained {
            match cmd {
                Cmd::Provide {
                    generation,
                    format,
                    reply,
                } => {
                    let _ = reply.send(self.cache.get(generation, &format));
                }
                Cmd::Offer(clip) => self.on_offer(clip),
                Cmd::OfferFiles { clip, fetcher } => self.on_offer_files(clip, fetcher),
                Cmd::Deliver {
                    token,
                    format,
                    bytes,
                } => self.on_deliver(token, &format, bytes),
                Cmd::PasteFailed { token, format } => self.on_paste_failed(token, &format),
                Cmd::Release => self.on_release(),
                Cmd::Exit(code) => {
                    self.exit_code = Some(code);
                    self.shutdown = true;
                }
            }
        }
    }

    fn on_offer(&mut self, clip: RemoteClip) {
        // A new offer refuses any in-flight paste and supersedes the local
        // capture (a remote promise wins convergence). It also tears down any
        // prior files offer (unmounts the FUSE tree).
        if let Some(p) = self.pending.take() {
            self.refuse(&p);
        }
        self.cache.invalidate();
        self.drop_files_offer();
        let mut advertised: Vec<x::Atom> = clip
            .formats
            .iter()
            .flat_map(|f| self.atoms.targets_for_format(&f.id))
            .collect();
        if clip.sensitive {
            advertised.push(self.atoms.kde_password_hint);
        }
        self.advertised = advertised;
        self.take_ownership();
    }

    /// Promise a remote FILES clip: expose the tree on demand and advertise its
    /// URIs, taking CLIPBOARD ownership. Any prior offer is torn down first. The
    /// backend cascade is FUSE first (universal `file://` path), then the WebDAV
    /// fallback for a NON-sensitive clip on a GVfs/KIO desktop. A sensitive clip
    /// is FUSE-only: a loopback DAV server is weaker than the uid-private mount,
    /// so it is never used for confidential content. Every refusal path releases
    /// ownership so we never keep owning the selection while advertising a promise
    /// we cannot honor.
    fn on_offer_files(&mut self, clip: RemoteClip, fetcher: Arc<dyn FileFetcher>) {
        if let Some(p) = self.pending.take() {
            self.refuse(&p);
        }
        self.cache.invalidate();
        self.drop_files_offer();

        // 1. FUSE (preferred). Clone the `Arc` so the fetcher survives a FUSE
        //    failure and can still be handed to the WebDAV fallback.
        if fuse::fuse_available() {
            match fuse::FuseMount::mount(&clip.files, fetcher.clone()) {
                Ok(mount) => {
                    let uris: Vec<String> = mount
                        .root_paths()
                        .iter()
                        .map(|p| files::file_uri(p))
                        .collect();
                    self.offer_uris = uris.clone();
                    self.offer_uris_kde = uris;
                    self.current_files = Some(FilesMount::Fuse(mount));
                    self.advertise_files(clip.sensitive);
                    self.take_ownership();
                    return;
                }
                Err(e) => warn(&format!("FUSE mount failed ({e}); trying WebDAV fallback")),
            }
        }

        // 2. WebDAV fallback — only for a non-sensitive clip on a GVfs/KIO desktop.
        if !clip.sensitive && webdav::webdav_available() {
            match webdav::WebDavMount::mount(&clip.files, fetcher) {
                Ok(mount) => {
                    self.offer_uris = mount.uris(false);
                    self.offer_uris_kde = mount.uris(true);
                    self.current_files = Some(FilesMount::WebDav(mount));
                    self.advertise_files(clip.sensitive);
                    self.take_ownership();
                    return;
                }
                Err(e) => {
                    warn(&format!("WebDAV mount failed ({e}); refusing files paste"));
                    self.release_ownership();
                    return;
                }
            }
        }

        // 3. No backend: refuse cleanly (release ownership, make no promise).
        if clip.sensitive {
            warn("no FUSE and a sensitive files clip stays FUSE-only; refusing files paste");
        } else {
            warn("no FUSE and no WebDAV files backend; refusing files paste");
        }
        self.release_ownership();
    }

    /// Publish the files targets, plus the KDE confidentiality hint for a
    /// sensitive clip (only ever reached via FUSE now).
    fn advertise_files(&mut self, sensitive: bool) {
        let mut advertised = self.atoms.targets_for_format(FORMAT_FILES);
        if sensitive {
            advertised.push(self.atoms.kde_password_hint);
        }
        self.advertised = advertised;
    }

    /// Tear down any live files offer: unmount/stop the backend and forget its
    /// URIs (both the default and the KDE list).
    fn drop_files_offer(&mut self) {
        self.current_files = None;
        self.offer_uris.clear();
        self.offer_uris_kde.clear();
    }

    /// Take CLIPBOARD ownership for the already-set `advertised` targets, and
    /// confirm the acquisition.
    fn take_ownership(&mut self) {
        self.conn.send_request(&x::SetSelectionOwner {
            owner: self.window,
            selection: self.atoms.clipboard,
            time: x::CURRENT_TIME,
        });
        if self.conn.flush().is_err() {
            return;
        }
        let c = self.conn.send_request(&x::GetSelectionOwner {
            selection: self.atoms.clipboard,
        });
        match self.conn.wait_for_reply(c) {
            Ok(r) if r.owner() == self.window => self.owned = true,
            _ => {
                self.owned = false;
                warn("failed to acquire CLIPBOARD ownership");
            }
        }
    }

    /// Relinquish CLIPBOARD ownership if held, and forget the advertised
    /// targets. Used both on a full release and on a clean files refusal: once we
    /// can no longer honor a promise, we must not keep owning the selection while
    /// advertising a superseded one (a paste of it would only be refused later).
    fn release_ownership(&mut self) {
        if self.owned {
            self.conn.send_request(&x::SetSelectionOwner {
                owner: x::Window::none(),
                selection: self.atoms.clipboard,
                time: x::CURRENT_TIME,
            });
            let _ = self.conn.flush();
            self.owned = false;
        }
        self.advertised.clear();
    }

    fn on_release(&mut self) {
        if let Some(p) = self.pending.take() {
            self.refuse(&p);
        }
        self.drop_files_offer();
        self.release_ownership();
    }
}

/// The inline `size` hint for an announced format: absent for a sensitive clip,
/// the byte length otherwise.
fn size_hint(sensitive: bool, len: usize) -> Option<u64> {
    if sensitive { None } else { Some(len as u64) }
}

/// The property to write into, falling back to the target atom for old-ICCCM
/// requestors that leave `property == NONE`.
fn property_or_target(e: &x::SelectionRequestEvent) -> x::Atom {
    if e.property() == x::ATOM_NONE {
        e.target()
    } else {
        e.property()
    }
}

fn warn(message: &str) {
    eprintln!("[universallink-clipboard] {message}");
}

/// Connects to X, sets up the selection window/atoms/poll, and builds the pinned
/// backend plus the `Clone` handle and the upcall channel. A connect failure
/// (no X server) surfaces as `Err` → the caller treats it as `Unsupported`.
pub fn create() -> Result<crate::os::Created, String> {
    let cmds: Arc<Mutex<VecDeque<Cmd>>> = Arc::new(Mutex::new(VecDeque::new()));

    let (conn, screen_num) =
        xcb::Connection::connect_with_extensions(None, &[xcb::Extension::XFixes], &[])
            .map_err(|e| format!("X connection: {e:?}"))?;

    let c = conn.send_request(&xfixes::QueryVersion {
        client_major_version: 5,
        client_minor_version: 0,
    });
    conn.wait_for_reply(c)
        .map_err(|e| format!("XFixes QueryVersion: {e:?}"))?;

    let screen = conn
        .get_setup()
        .roots()
        .nth(screen_num as usize)
        .ok_or_else(|| "X screen not found".to_string())?;
    let root = screen.root();
    let visual = screen.root_visual();

    let window: x::Window = conn.generate_id();
    conn.send_request(&x::CreateWindow {
        depth: x::COPY_FROM_PARENT as u8,
        wid: window,
        parent: root,
        x: 0,
        y: 0,
        width: 1,
        height: 1,
        border_width: 0,
        class: x::WindowClass::InputOutput,
        visual,
        value_list: &[],
    });
    conn.flush().map_err(|e| format!("flush init: {e:?}"))?;

    let atoms = Atoms::intern_all(&conn)?;

    // XFixes: be notified of CLIPBOARD owner changes (local copies).
    conn.send_request(&xfixes::SelectSelectionInput {
        window,
        selection: atoms.clipboard,
        event_mask: xfixes::SelectionEventMask::SET_SELECTION_OWNER
            | xfixes::SelectionEventMask::SELECTION_WINDOW_DESTROY
            | xfixes::SelectionEventMask::SELECTION_CLIENT_CLOSE,
    });
    conn.flush()
        .map_err(|e| format!("flush SelectSelectionInput: {e:?}"))?;

    let poll = Poll::new().map_err(|e| format!("mio Poll: {e}"))?;
    poll.registry()
        .register(
            &mut SourceFd(&conn.as_raw_fd()),
            XCB_TOKEN,
            Interest::READABLE,
        )
        .map_err(|e| format!("register X fd: {e}"))?;
    let waker =
        Arc::new(Waker::new(poll.registry(), CMD_TOKEN).map_err(|e| format!("mio Waker: {e}"))?);

    let (events_tx, backend_events) = mpsc::channel(BACKEND_EVENT_CAPACITY);

    let backend = Backend {
        conn,
        window,
        atoms,
        poll,
        cmds: cmds.clone(),
        events_tx,
        advertised: Vec::new(),
        current_files: None,
        offer_uris: Vec::new(),
        offer_uris_kde: Vec::new(),
        pending: None,
        owned: false,
        last_owner_foreign: false,
        next_generation: 0,
        next_paste_token: 0,
        cache: Cache::default(),
        pending_owner_change: None,
        shutdown: false,
        exit_code: None,
    };
    let handle = X11Backend { cmds, waker };

    Ok(crate::os::Created {
        handle,
        backend_events,
        event_loop: X11Loop { backend },
    })
}

#[cfg(test)]
mod tests {
    //! Pure-helper tests only: the atom↔format mapping and the cache
    //! generation-gate. Nothing here opens a `Connection` (the Xvfb
    //! integration suite lives outside this module). The mapping helpers only
    //! compare atom fields, so a hand-built `Atoms` from distinct predefined
    //! atoms exercises them without a server.
    use super::*;

    fn test_atoms() -> Atoms {
        Atoms {
            clipboard: x::ATOM_PRIMARY,
            targets: x::ATOM_SECONDARY,
            timestamp: x::ATOM_INTEGER,
            utf8_string: x::ATOM_STRING,
            string: x::ATOM_CARDINAL,
            text: x::ATOM_WINDOW,
            image_png: x::ATOM_BITMAP,
            prop: x::ATOM_DRAWABLE,
            kde_password_hint: x::ATOM_FONT,
            uri_list: x::ATOM_POINT,
            gnome_copied: x::ATOM_RECTANGLE,
            kde_copied: x::ATOM_COLORMAP,
        }
    }

    #[test]
    fn advertised_targets_include_the_text_aliases() {
        let a = test_atoms();
        assert_eq!(
            a.targets_for_format(FORMAT_TEXT),
            vec![a.utf8_string, a.string, a.text]
        );
        assert_eq!(a.targets_for_format(FORMAT_PNG), vec![a.image_png]);
        assert_eq!(
            a.targets_for_format(FORMAT_FILES),
            vec![a.uri_list, a.gnome_copied, a.kde_copied]
        );
        assert!(a.targets_for_format("bogus").is_empty());
    }

    #[test]
    fn detect_maps_the_file_target_atoms() {
        let a = test_atoms();
        assert_eq!(a.format_for_target(a.uri_list), Some(FORMAT_FILES));
        assert_eq!(a.format_for_target(a.gnome_copied), Some(FORMAT_FILES));
        assert_eq!(a.format_for_target(a.kde_copied), Some(FORMAT_FILES));
        for target in a.targets_for_format(FORMAT_FILES) {
            assert_eq!(a.format_for_target(target), Some(FORMAT_FILES));
        }
    }

    #[test]
    fn type_atom_is_utf8_for_text_and_png_for_image() {
        let a = test_atoms();
        assert_eq!(a.type_atom_for_format(FORMAT_TEXT), a.utf8_string);
        assert_eq!(a.type_atom_for_format(FORMAT_PNG), a.image_png);
        // Unknown formats fall back to UTF8_STRING (never mojibake-prone STRING).
        assert_eq!(a.type_atom_for_format("bogus"), a.utf8_string);
    }

    #[test]
    fn detect_maps_the_text_atoms_and_png() {
        let a = test_atoms();
        assert_eq!(a.format_for_target(a.utf8_string), Some(FORMAT_TEXT));
        assert_eq!(a.format_for_target(a.string), Some(FORMAT_TEXT));
        assert_eq!(a.format_for_target(a.text), Some(FORMAT_TEXT));
        assert_eq!(a.format_for_target(a.image_png), Some(FORMAT_PNG));
        assert_eq!(a.format_for_target(a.targets), None);
        assert_eq!(a.format_for_target(a.kde_password_hint), None);
    }

    #[test]
    fn advertised_target_atoms_round_trip_back_to_their_format() {
        let a = test_atoms();
        for target in a.targets_for_format(FORMAT_TEXT) {
            assert_eq!(a.format_for_target(target), Some(FORMAT_TEXT));
        }
        for target in a.targets_for_format(FORMAT_PNG) {
            assert_eq!(a.format_for_target(target), Some(FORMAT_PNG));
        }
    }

    #[test]
    fn cache_gate_is_exact_on_generation_and_format() {
        let mut cache = Cache::default();
        let mut map = HashMap::new();
        map.insert(FORMAT_TEXT.to_string(), b"hello".to_vec());
        cache.store(7, map);
        assert_eq!(cache.get(7, FORMAT_TEXT), Some(b"hello".to_vec()));
        assert_eq!(cache.get(8, FORMAT_TEXT), None); // wrong generation
        assert_eq!(cache.get(7, FORMAT_PNG), None); // absent format
        cache.invalidate();
        assert_eq!(cache.get(7, FORMAT_TEXT), None); // superseded by an offer
    }

    #[test]
    fn size_hint_is_absent_when_sensitive() {
        assert_eq!(size_hint(false, 42), Some(42));
        assert_eq!(size_hint(true, 42), None);
    }
}
