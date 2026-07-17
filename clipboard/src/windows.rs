// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

//! The Windows clipboard backend: an owner-driven, delayed-render state machine
//! that plugs into the frozen [`crate::backend`] seam. Built on the classic
//! Win32 clipboard API (`user32`), NOT on OLE / `IDataObject` (files land in a
//! later brick). Scope of this brick: `text` and `image/png` only.
//!
//! Two threads meet here, mirroring the X11 backend. A message-only window
//! (`HWND_MESSAGE`) and its `GetMessage`-class pump are pinned to the MAIN
//! thread inside [`Backend`], driven by [`WindowsLoop::run`]. The SIDE thread
//! runs the async orchestrator; it drives the OS through the cheap, `Clone`
//! [`WindowsBackend`] handle (which carries the `hwnd` as a `Send` `isize`) and
//! observes local activity through the `BackendEvent` channel the loop pushes on
//! (`try_send`, never blocking the pump).
//!
//! # The critical divergence from X11: `WM_RENDERFORMAT` is SYNCHRONOUS
//!
//! Under X11 a `SelectionRequest` is answered *later* (record it, emit `Paste`,
//! reply with a `SendEvent` once the bytes arrive — true async deferral). Under
//! Windows the pasting application blocks inside `GetClipboardData` until the
//! owner calls `SetClipboardData(fmt, handle)`. So the `WM_RENDERFORMAT` handler
//! must BLOCK on the pump thread until the orchestrator delivers the bytes it
//! pulled over the network. Those bytes therefore CANNOT travel through the
//! command queue: while the pump is blocked in the handler it does not drain the
//! queue, so a queued `deliver` would deadlock. They travel through a separate
//! direct rendezvous instead — an [`PasteSync`] (`Mutex<Option<PasteOutcome>>` +
//! `Condvar`) the handle also holds. A hard [`PASTE_TIMEOUT`] refuses cleanly if
//! nothing ever comes, so the pasting app never freezes indefinitely.
//!
//! # Anti-echo & re-entrancy
//!
//! Our own `EmptyClipboard` (taking ownership for a remote offer) posts a
//! `WM_CLIPBOARDUPDATE` we filter statelessly via `GetClipboardOwner() == hwnd`
//! (no mutable "owned" flag — always query live). But `EmptyClipboard` also
//! SENDS a synchronous, re-entrant `WM_DESTROYCLIPBOARD` back to our `wndproc`,
//! and `DestroyWindow` sends `WM_RENDERALLFORMATS`/`WM_DESTROY`: those must NOT
//! dereference `&mut Backend` (we already hold that borrow) — they are handled as
//! no-ops without touching the backend pointer.
//!
//! # Images
//!
//! On the wire images are `image/png`. On READ we prefer a registered `"PNG"`
//! clipboard format (raw, lossless); failing that we decode `CF_DIBV5`/`CF_DIB`
//! into RGBA and re-encode to PNG (so Snipping Tool / Paint, which are DIB-only,
//! work). On WRITE (an offer) we promise BOTH the `"PNG"` format AND `CF_DIBV5`,
//! advertising `"PNG"` first (many apps take the first compatible format); at
//! `WM_RENDERFORMAT` we render whichever exact format was requested.

use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use tokio::sync::{mpsc, oneshot};
use windows_sys::Win32::Foundation::{GlobalFree, HGLOBAL, HWND, LPARAM, LRESULT, WPARAM};
use windows_sys::Win32::System::DataExchange::{
    AddClipboardFormatListener, CloseClipboard, EmptyClipboard, GetClipboardData,
    GetClipboardOwner, IsClipboardFormatAvailable, OpenClipboard, RegisterClipboardFormatW,
    RemoveClipboardFormatListener, SetClipboardData,
};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::Memory::{
    GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GWLP_USERDATA,
    GetWindowLongPtrW, MSG, MWMO_INPUTAVAILABLE, MsgWaitForMultipleObjectsEx, PM_REMOVE,
    PeekMessageW, PostMessageW, QS_ALLINPUT, RegisterClassW, SetWindowLongPtrW, TranslateMessage,
    WM_APP, WM_CLIPBOARDUPDATE, WM_DESTROY, WM_DESTROYCLIPBOARD, WM_RENDERALLFORMATS,
    WM_RENDERFORMAT, WNDCLASSW,
};

use crate::backend::{BackendEvent, ClipboardBackend, Format, LocalClip, RemoteClip};

/// Deadline of a pending paste: past it, refuse cleanly (the app never freezes).
/// A safety net independent of `release()` when a disconnection is silent.
const PASTE_TIMEOUT: Duration = Duration::from_secs(30);

/// Cap of the pump's idle wait so `shutdown` (and a coalesced/lost wake) are
/// always observed even without an incoming message.
const SHUTDOWN_POLL_CAP: Duration = Duration::from_millis(250);

/// Bounded retry when opening the clipboard: another process (clipboard history,
/// an RDP monitor, an antivirus…) may hold it open for a moment. A single lost
/// race would otherwise drop a copy or an offer entirely — this fixed a real
/// flaky-CI race in the prior POC.
const OPEN_CLIPBOARD_RETRIES: u32 = 10;
const OPEN_CLIPBOARD_RETRY_DELAY: Duration = Duration::from_millis(5);

/// Bounded capacity of the upcall channel. Generous; the orchestrator drains
/// promptly, and a full queue only ever means it has stalled or gone.
const BACKEND_EVENT_CAPACITY: usize = 256;

/// Core v1 format strings.
const FORMAT_TEXT: &str = "text";
const FORMAT_PNG: &str = "image/png";

// --- Stable Win32 clipboard-format ids, hard-coded (documented by the API).
// The typed `CLIPBOARD_FORMAT` constants live behind the `Win32_System_Ole`
// feature; hard-coding the three we use as `u32` avoids pulling that whole
// module in just for their values. ---

/// Unicode text (UTF-16LE, `NUL`-terminated).
const CF_UNICODETEXT: u32 = 13;
/// Device-independent bitmap, `BITMAPINFOHEADER` family.
const CF_DIB: u32 = 8;
/// Device-independent bitmap, `BITMAPV5HEADER` (carries an explicit alpha mask).
const CF_DIBV5: u32 = 17;

/// Private wake message (in the `WM_APP` range): the orchestrator posted a
/// [`Cmd`] onto the queue.
const WM_APP_CMD: u32 = WM_APP + 1;

/// Parent handle of a message-only window: `HWND_MESSAGE == (HWND)-3`.
const HWND_MESSAGE: isize = -3;

/// Unique window-class suffix per backend instance: avoids
/// `ERROR_CLASS_ALREADY_EXISTS` when several backends coexist in one process
/// (the integration tests spawn more than one).
static CLASS_SEQ: AtomicU64 = AtomicU64::new(0);

/// A downcall from the orchestrator, queued for the main-thread loop. Note the
/// two paste replies — `deliver`/`paste_failed` — are NOT here: they go through
/// [`PasteSync`] because the pump is blocked in the synchronous render handler
/// and could not drain this queue (see the module docs).
enum Cmd {
    /// Answer `provide(generation, format)` from the eager-read cache.
    Provide {
        generation: u64,
        format: String,
        reply: oneshot::Sender<Option<Vec<u8>>>,
    },
    /// Take ownership of the clipboard, promising a remote clip (bytes later).
    Offer(RemoteClip),
    /// Drop OS ownership (promise withdrawn / superseded / shutting down).
    Release,
    /// Stop the loop with this process exit code (dropping ownership first).
    Exit(i32),
}

/// The verdict the orchestrator hands the blocked `WM_RENDERFORMAT` handler,
/// through [`PasteSync`]. `Bytes`/`Refuse` are gated on the paste `token`;
/// `Abort` unconditionally unblocks any in-flight render (session ending).
enum PasteOutcome {
    /// Fetched bytes for the pending paste `token`.
    Bytes(u64, Vec<u8>),
    /// The pending paste `token` could not be satisfied: refuse cleanly.
    Refuse(u64),
    /// Session ending: unblock immediately with a clean refusal.
    Abort,
}

/// The synchronous rendezvous between the orchestrator (which delivers) and the
/// blocked `WM_RENDERFORMAT` handler (which waits). Shared by `Arc` between the
/// [`WindowsBackend`] handle and the pump-side [`Backend`].
struct PasteSync {
    state: Mutex<Option<PasteOutcome>>,
    cv: Condvar,
}

impl PasteSync {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(None),
            cv: Condvar::new(),
        })
    }

    /// Lock the state, recovering the guard even if the mutex is poisoned (a
    /// thread panicked holding it). The content is a plain `Option`, so it stays
    /// coherent, and swallowing the poison keeps a blocked render always
    /// wake-able — never a lost notify.
    fn lock(&self) -> std::sync::MutexGuard<'_, Option<PasteOutcome>> {
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Clear any residual verdict (from a previous paste) before awaiting a new.
    fn reset(&self) {
        *self.lock() = None;
    }

    /// Store a verdict and wake the blocked handler.
    fn resolve(&self, outcome: PasteOutcome) {
        *self.lock() = Some(outcome);
        self.cv.notify_all();
    }

    /// Block until the verdict for paste `token` (or `Abort`, or the deadline).
    /// Returns the bytes to render, or `None` for a clean refusal. A stale
    /// verdict (a different `token`) is dropped and the wait continues.
    fn wait(&self, token: u64, deadline: Instant) -> Option<Vec<u8>> {
        let mut guard = self.lock();
        loop {
            match guard.take() {
                Some(PasteOutcome::Bytes(t, bytes)) if t == token => return Some(bytes),
                Some(PasteOutcome::Refuse(t)) if t == token => return None,
                Some(PasteOutcome::Abort) => return None,
                // Nothing yet, or a stale verdict: keep waiting.
                _ => {}
            }
            let now = Instant::now();
            if now >= deadline {
                return None;
            }
            guard = match self.cv.wait_timeout(guard, deadline - now) {
                Ok((g, _)) => g,
                Err(p) => p.into_inner().0,
            };
        }
    }
}

/// The cheap, `Clone` handle the orchestrator holds. Carries no non-`Send` OS
/// resource: the `hwnd` is stored as an `isize` and reconstituted for
/// `PostMessageW` (safe from any thread). A downcall either pushes a [`Cmd`]
/// then wakes the loop (push BEFORE wake, or a coalesced wake could drop the
/// command) or resolves the [`PasteSync`] rendezvous directly.
#[derive(Clone)]
pub struct WindowsBackend {
    hwnd: isize,
    cmds: Arc<Mutex<VecDeque<Cmd>>>,
    paste: Arc<PasteSync>,
}

impl WindowsBackend {
    /// Enqueue a command, then wake the pump. Recovers a poisoned mutex so
    /// `Release`/`Exit` are never lost.
    fn push(&self, cmd: Cmd) {
        match self.cmds.lock() {
            Ok(mut q) => q.push_back(cmd),
            Err(p) => p.into_inner().push_back(cmd),
        }
        // `PostMessageW` is safe from any thread. A lost/coalesced wake leaves a
        // queued command, but the loop caps its idle wait at SHUTDOWN_POLL_CAP.
        unsafe {
            PostMessageW(self.hwnd as HWND, WM_APP_CMD, 0, 0);
        }
    }

    /// Queue a request to stop the loop with `code` (from another thread). Also
    /// unblocks any in-flight synchronous render (a clean refusal) so shutdown
    /// never waits out a paste.
    pub fn request_exit(&self, code: i32) {
        self.paste.resolve(PasteOutcome::Abort);
        self.push(Cmd::Exit(code));
    }
}

impl ClipboardBackend for WindowsBackend {
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

    fn deliver(&self, token: u64, _format: &str, bytes: Vec<u8>) {
        // Straight to the blocked render handler (never the command queue). The
        // handler already knows the exact clipboard format requested, so the
        // Core format is not needed here — only the token, which gates staleness.
        self.paste.resolve(PasteOutcome::Bytes(token, bytes));
    }

    fn paste_failed(&self, token: u64, _format: &str) {
        self.paste.resolve(PasteOutcome::Refuse(token));
    }

    fn release(&self) {
        // Unblock an in-flight render first (clean refusal), then relinquish.
        self.paste.resolve(PasteOutcome::Abort);
        self.push(Cmd::Release);
    }
}

/// Owns the pinned [`Backend`]; [`run`](Self::run) is the blocking main-thread
/// pump. Returns the process exit code once the loop stops.
pub struct WindowsLoop {
    backend: Backend,
}

impl WindowsLoop {
    /// Pumps the Windows message loop on the calling (main) thread until a
    /// [`Cmd::Exit`] (or a vanished orchestrator) stops it, dropping clipboard
    /// ownership on the way out. Returns the requested exit code.
    pub fn run(mut self) -> i32 {
        self.backend.run()
    }
}

/// The format↔clipboard-format mapping. `png` is the runtime-registered `"PNG"`
/// clipboard format (`RegisterClipboardFormatW`); the rest are stable `CF_*`
/// ids. Pure helpers so they are unit-tested against a hand-built `Formats`.
struct Formats {
    png: u32,
}

impl Formats {
    /// A `WM_RENDERFORMAT` request → the Core format we would render, if any.
    /// `CF_DIBV5` renders as `image/png` too (we promised it alongside `"PNG"`).
    /// `CF_DIB` is never promised (we advertise `CF_DIBV5`), so it maps to none.
    fn format_for_clipformat(&self, fmt: u32) -> Option<&'static str> {
        if fmt == CF_UNICODETEXT {
            Some(FORMAT_TEXT)
        } else if fmt == self.png || fmt == CF_DIBV5 {
            Some(FORMAT_PNG)
        } else {
            None
        }
    }

    /// The clipboard formats to promise (in order) for a remote offer. For an
    /// image, `"PNG"` is advertised BEFORE `CF_DIBV5`: many apps take the first
    /// compatible format in enumeration order, and the raw PNG is lossless.
    /// Unknown Core formats contribute nothing (their paste is refused cleanly).
    fn offer_clipformats(&self, formats: &[Format]) -> Vec<u32> {
        let mut out = Vec::new();
        for f in formats {
            match f.id.as_str() {
                FORMAT_TEXT => out.push(CF_UNICODETEXT),
                FORMAT_PNG => {
                    out.push(self.png);
                    out.push(CF_DIBV5);
                }
                _ => {}
            }
        }
        out
    }
}

/// One eager read of the OS clipboard: the bytes of every supported format
/// present, plus the announce metadata derived from them.
type EagerRead = (HashMap<String, Vec<u8>>, Vec<Format>);

/// The per-generation eager-read cache: the bytes of the local copy announced as
/// `generation`, keyed by Core format. A pure struct (no OS handles) so its
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

/// Backend state, living on the pump (main) thread. Owns the raw `hwnd`, so it
/// is not `Send` (never shared across threads — created and pumped here only).
struct Backend {
    hwnd: HWND,
    formats: Formats,
    cmds: Arc<Mutex<VecDeque<Cmd>>>,
    paste: Arc<PasteSync>,
    /// Bounded upcall channel to the orchestrator (never blocks the loop).
    events_tx: mpsc::Sender<BackendEvent>,
    cache: Cache,
    /// Monotonic id per local copy (announce generation).
    next_generation: u64,
    /// Monotonic id per deferred paste (correlation token). SEPARATE from
    /// `next_generation`: conflating them breaks the seam.
    next_paste_token: u64,
    shutdown: bool,
    exit_code: Option<i32>,
}

impl Drop for Backend {
    /// Safety net at loop exit: relinquish ownership (dropping delayed-render
    /// promises), stop listening, and destroy the window, so an orphaned offer
    /// never wedges other apps' pastes.
    fn drop(&mut self) {
        self.relinquish();
        unsafe {
            RemoveClipboardFormatListener(self.hwnd);
            SetWindowLongPtrW(self.hwnd, GWLP_USERDATA, 0);
            DestroyWindow(self.hwnd);
        }
    }
}

impl Backend {
    fn new(
        cmds: Arc<Mutex<VecDeque<Cmd>>>,
        paste: Arc<PasteSync>,
        events_tx: mpsc::Sender<BackendEvent>,
    ) -> Result<Self, String> {
        unsafe {
            let hinstance = GetModuleHandleW(std::ptr::null());
            let class_name = wide(&format!(
                "universallink-clipboard-window-{}",
                CLASS_SEQ.fetch_add(1, Ordering::Relaxed)
            ));
            let wc = WNDCLASSW {
                lpfnWndProc: Some(wndproc),
                hInstance: hinstance,
                lpszClassName: class_name.as_ptr(),
                ..Default::default()
            };
            if RegisterClassW(&wc) == 0 {
                return Err("RegisterClassW failed".into());
            }
            let window_name = wide("universallink-clipboard");
            let hwnd = CreateWindowExW(
                0,
                class_name.as_ptr(),
                window_name.as_ptr(),
                0,
                0,
                0,
                0,
                0,
                HWND_MESSAGE as HWND,
                std::ptr::null_mut(),
                hinstance,
                std::ptr::null(),
            );
            if hwnd.is_null() {
                return Err("CreateWindowExW failed".into());
            }
            // A clipboard listener we cannot register (e.g. a session-0/headless
            // window station with no clipboard) means there is nothing to run:
            // surface it as an error → `Unsupported` → a clean exit.
            if AddClipboardFormatListener(hwnd) == 0 {
                DestroyWindow(hwnd);
                return Err("AddClipboardFormatListener failed".into());
            }
            let png = RegisterClipboardFormatW(wide("PNG").as_ptr());
            Ok(Self {
                hwnd,
                formats: Formats { png },
                cmds,
                paste,
                events_tx,
                cache: Cache::default(),
                next_generation: 0,
                next_paste_token: 0,
                shutdown: false,
                exit_code: None,
            })
        }
    }

    fn run(&mut self) -> i32 {
        // Drive the loop through a RAW self-pointer, never a live `&mut self`
        // held across `DispatchMessageW`. That call re-enters `wndproc`, which
        // re-derives `&mut Backend` from `GWLP_USERDATA`; keeping a `&mut self`
        // borrow alive across it would be aliasing UB (two live `&mut`, and the
        // `noalias` on the receiver could let LLVM miscompile). Every access
        // below is a transient reborrow from `this`, one at a time (the pump is
        // single-threaded, and the only re-entrant messages are no-ops).
        let this: *mut Backend = self;
        // Publish the pointer so `wndproc` can find us for the messages that do
        // borrow the backend (`WM_CLIPBOARDUPDATE`/`WM_RENDERFORMAT`/the wake).
        unsafe {
            SetWindowLongPtrW((*this).hwnd, GWLP_USERDATA, this as isize);
        }
        loop {
            // Commands first, exactly like X11: a `deliver` for the current paste
            // (which arrives out-of-band via PasteSync, not here) must be able to
            // land before a new offer replaces state. `Provide`/`Offer`/`Release`/
            // `Exit` are drained unconditionally each pass (cheap when empty).
            unsafe { (*this).process_cmds() };
            // Then OS events. `PeekMessageW` also DISPATCHES pending sent
            // (nonqueued) messages to `wndproc` before returning — including a
            // synchronous `WM_RENDERFORMAT`, which then blocks in the handler
            // until the orchestrator delivers (or the deadline refuses).
            let mut msg = MSG::default();
            while unsafe { PeekMessageW(&mut msg, std::ptr::null_mut(), 0, 0, PM_REMOVE) } != 0 {
                unsafe {
                    TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
            }
            if unsafe { (*this).shutdown } {
                unsafe { (*this).relinquish() };
                return unsafe { (*this).exit_code }.unwrap_or(1);
            }
            // Bounded idle wait: returns on a new message (QS_ALLINPUT covers
            // sent messages) or after SHUTDOWN_POLL_CAP, so a lost wake or a set
            // `shutdown` is always observed within the cap.
            unsafe {
                MsgWaitForMultipleObjectsEx(
                    0,
                    std::ptr::null(),
                    SHUTDOWN_POLL_CAP.as_millis() as u32,
                    QS_ALLINPUT,
                    MWMO_INPUTAVAILABLE,
                );
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
                Cmd::Release => self.relinquish(),
                Cmd::Exit(code) => {
                    self.exit_code = Some(code);
                    self.shutdown = true;
                }
            }
        }
    }

    /// Take ownership of the clipboard, promising the remote `clip` with
    /// delayed-render placeholders (`SetClipboardData(fmt, NULL)`). A new offer
    /// also supersedes any local capture (a remote promise wins convergence), so
    /// `provide` for the old generation must stop vouching — invalidate the cache.
    fn on_offer(&mut self, clip: RemoteClip) {
        self.cache.invalidate();
        let clipformats = self.formats.offer_clipformats(&clip.formats);
        if clipformats.is_empty() {
            // Nothing we can promise: relinquish rather than hold a stale offer.
            self.relinquish();
            return;
        }
        unsafe {
            if !self.open_clipboard() {
                warn("OpenClipboard (offer) failed — offer dropped");
                return;
            }
            EmptyClipboard(); // our window becomes the owner
            for fmt in clipformats {
                // NULL = a delayed-render promise, honored at `WM_RENDERFORMAT`.
                SetClipboardData(fmt, std::ptr::null_mut());
            }
            CloseClipboard();
        }
    }

    /// Relinquish ownership if we still hold it (drop the delayed-render
    /// promises). Live-queried via `GetClipboardOwner` — no mutable "owned" flag.
    /// Shared by `Cmd::Release`, the shutdown path, and `Drop`.
    fn relinquish(&self) {
        unsafe {
            if GetClipboardOwner() == self.hwnd && self.open_clipboard() {
                EmptyClipboard();
                CloseClipboard();
            }
        }
    }

    // ----- Source side: a foreign copy → eager read → Copied/Cleared -----

    fn on_clipboard_update(&mut self) {
        // Anti-echo: our own offers set us as the owner — never re-read them.
        // Stateless on purpose (always query live), so it cannot desynchronize.
        if unsafe { GetClipboardOwner() } == self.hwnd {
            return;
        }
        self.read_and_announce_copy();
    }

    /// Eager-read every inline format the foreign owner offers, cache the bytes
    /// under a fresh generation, and announce the metadata. If nothing usable is
    /// there, supersede the stale capture instead of continuing to vouch for it.
    fn read_and_announce_copy(&mut self) {
        let Some((bytes_by_format, formats)) = self.read_clipboard() else {
            self.supersede_local();
            return;
        };
        if formats.is_empty() {
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
                // Windows confidentiality markers (clipboard-history exclusion)
                // are a later brick: nothing is announced sensitive yet.
                sensitive: false,
            },
        });
    }

    /// A foreign owner took the clipboard but we could not capture a usable clip
    /// (unreadable, or only unsupported formats). Drop the stale local capture
    /// and announce a clear, so the orchestrator stops vouching for a generation
    /// the clipboard has already moved past — otherwise `provide` would keep
    /// serving bytes the user has replaced (a staleness + confidentiality bug).
    fn supersede_local(&mut self) {
        if !self.cache.is_empty() {
            self.cache.invalidate();
            self.emit(BackendEvent::Cleared);
        }
    }

    /// Open the clipboard and read every supported format eagerly. `None` iff the
    /// clipboard could not be opened (→ supersede); an opened-but-empty read
    /// yields empty vectors (also → supersede).
    fn read_clipboard(&self) -> Option<EagerRead> {
        unsafe {
            if !self.open_clipboard() {
                warn("OpenClipboard (read) failed");
                return None;
            }
            let mut bytes_by_format = HashMap::new();
            let mut formats = Vec::new();
            // Text: `CF_UNICODETEXT` is UTF-16LE. Windows auto-synthesizes it
            // from `CF_TEXT`, so reading it alone covers legacy ANSI sources; the
            // transcode is lossless UTF-16 → UTF-8 (never announced mojibake).
            if IsClipboardFormatAvailable(CF_UNICODETEXT) != 0
                && let Some(text) = read_text()
            {
                formats.push(Format {
                    id: FORMAT_TEXT.to_string(),
                    size: Some(text.len() as u64),
                });
                bytes_by_format.insert(FORMAT_TEXT.to_string(), text);
            }
            if let Some(png) = self.read_image() {
                formats.push(Format {
                    id: FORMAT_PNG.to_string(),
                    size: Some(png.len() as u64),
                });
                bytes_by_format.insert(FORMAT_PNG.to_string(), png);
            }
            CloseClipboard();
            Some((bytes_by_format, formats))
        }
    }

    /// Read an image as `image/png`: a registered `"PNG"` format verbatim
    /// (lossless), else a `CF_DIBV5`/`CF_DIB` decoded to RGBA and re-encoded to
    /// PNG (so DIB-only sources — Snipping Tool, Paint, PrintScreen — work).
    ///
    /// The clipboard must already be open (caller's responsibility).
    fn read_image(&self) -> Option<Vec<u8>> {
        if is_format_available(self.formats.png)
            && let Some(bytes) = read_bytes(self.formats.png)
        {
            return Some(bytes);
        }
        for cf in [CF_DIBV5, CF_DIB] {
            if is_format_available(cf)
                && let Some(dib) = read_bytes(cf)
                && let Some(png) = dib_to_png(&dib)
            {
                return Some(png);
            }
        }
        None
    }

    // ----- Destination side: synchronous delayed render -----

    /// A pasting app blocked in `GetClipboardData(fmt)`. Mint a token, emit the
    /// `Paste` upcall, then BLOCK here until the orchestrator delivers the bytes
    /// (or refuses / the deadline elapses). On success, render the exact format
    /// requested WITHOUT opening the clipboard (the requestor already holds it
    /// open). On any failure, return without `SetClipboardData` — a clean refusal
    /// (`GetClipboardData` returns NULL; the app does not freeze).
    fn on_render_format(&mut self, requested: u32) {
        let Some(format) = self.formats.format_for_clipformat(requested) else {
            return; // a format we never promised — ignore
        };
        self.paste.reset();
        let token = self.next_paste_token;
        self.next_paste_token += 1;
        self.emit(BackendEvent::Paste {
            format: format.to_string(),
            token,
        });
        if self.shutdown {
            // The orchestrator is gone (the upcall channel closed): refuse now
            // rather than block for PASTE_TIMEOUT with no one left to deliver.
            return;
        }
        let deadline = Instant::now() + PASTE_TIMEOUT;
        let Some(bytes) = self.paste.wait(token, deadline) else {
            return; // refused / superseded / timed out → clean refusal
        };
        render_into_clipboard(requested, self.formats.png, bytes);
    }

    /// Bounded-retry `OpenClipboard(self.hwnd)`. The clipboard must be closed on
    /// entry; the caller closes it. `true` if it is now open.
    fn open_clipboard(&self) -> bool {
        for _ in 0..OPEN_CLIPBOARD_RETRIES {
            if unsafe { OpenClipboard(self.hwnd) } != 0 {
                return true;
            }
            std::thread::sleep(OPEN_CLIPBOARD_RETRY_DELAY);
        }
        false
    }
}

/// `IsClipboardFormatAvailable` as a bool (the raw returns a `BOOL` `i32`). The
/// clipboard need not be open for this query.
fn is_format_available(fmt: u32) -> bool {
    unsafe { IsClipboardFormatAvailable(fmt) != 0 }
}

/// Read `CF_UNICODETEXT` (UTF-16LE) as Core `text` (UTF-8). The trailing `NUL`
/// is only a convention the kernel does not enforce, and the data comes from
/// another process, so the scan is bounded by `GlobalSize/2` (never a blind
/// search that could read past the allocation). The clipboard must be open.
fn read_text() -> Option<Vec<u8>> {
    unsafe {
        let h = GetClipboardData(CF_UNICODETEXT);
        if h.is_null() {
            return None;
        }
        let max_u16 = GlobalSize(h) / 2;
        if max_u16 == 0 {
            return None;
        }
        let ptr = GlobalLock(h) as *const u16;
        if ptr.is_null() {
            return None;
        }
        let mut len = 0usize;
        while len < max_u16 && *ptr.add(len) != 0 {
            len += 1;
        }
        let s = String::from_utf16_lossy(std::slice::from_raw_parts(ptr, len));
        GlobalUnlock(h);
        Some(s.into_bytes())
    }
}

/// Read a binary clipboard format (PNG, DIB) as raw bytes. The clipboard must be
/// open; the returned handle is owned by the clipboard (never freed here).
fn read_bytes(fmt: u32) -> Option<Vec<u8>> {
    unsafe {
        let h = GetClipboardData(fmt);
        if h.is_null() {
            return None;
        }
        let size = GlobalSize(h);
        if size == 0 {
            return None;
        }
        let ptr = GlobalLock(h) as *const u8;
        if ptr.is_null() {
            return None;
        }
        let bytes = std::slice::from_raw_parts(ptr, size).to_vec();
        GlobalUnlock(h);
        Some(bytes)
    }
}

/// Render the delivered bytes for the EXACT clipboard format the requestor asked
/// for (never a substitute): UTF-16LE for text, raw PNG for the registered
/// format, a `BITMAPV5HEADER` DIB for `CF_DIBV5`. Called only from
/// `WM_RENDERFORMAT`, where the clipboard is already open — so `SetClipboardData`
/// is issued WITHOUT `OpenClipboard`/`CloseClipboard`.
fn render_into_clipboard(requested: u32, png_format: u32, bytes: Vec<u8>) {
    let blob: Vec<u8> = if requested == CF_UNICODETEXT {
        wide_text_blob(&bytes)
    } else if requested == png_format {
        bytes // already PNG
    } else if requested == CF_DIBV5 {
        match png_to_dibv5(&bytes) {
            Some(dib) => dib,
            None => {
                warn("cannot encode PNG → DIBV5 — paste refused");
                return;
            }
        }
    } else {
        return; // unreachable: only promised formats reach here
    };
    set_clipboard_blob(requested, &blob);
}

/// Allocate a movable `HGLOBAL`, copy `buf` into it, and hand it to the clipboard
/// with `SetClipboardData(fmt, h)`. If `SetClipboardData` fails, ownership was
/// NOT transferred, so the block is freed. Must be called only from a
/// `WM_RENDERFORMAT` handler (the clipboard is already open by the requestor).
fn set_clipboard_blob(fmt: u32, buf: &[u8]) {
    unsafe {
        let h: HGLOBAL = GlobalAlloc(GMEM_MOVEABLE, buf.len());
        if h.is_null() {
            warn("GlobalAlloc failed — paste refused");
            return;
        }
        let dst = GlobalLock(h) as *mut u8;
        if dst.is_null() {
            GlobalFree(h);
            warn("GlobalLock failed — paste refused");
            return;
        }
        std::ptr::copy_nonoverlapping(buf.as_ptr(), dst, buf.len());
        GlobalUnlock(h);
        if SetClipboardData(fmt, h).is_null() {
            GlobalFree(h);
            warn("SetClipboardData failed — paste refused");
        }
    }
}

/// UTF-8 bytes → a `CF_UNICODETEXT` blob: UTF-16LE, `NUL`-terminated (required).
/// Pure (testable off a Windows runtime): `"hi"` → `[h,0,i,0,0,0]`, `""` → `[0,0]`.
fn wide_text_blob(utf8: &[u8]) -> Vec<u8> {
    let s = String::from_utf8_lossy(utf8);
    let mut out = Vec::with_capacity(utf8.len() * 2 + 2);
    for u in s.encode_utf16() {
        out.extend_from_slice(&u.to_le_bytes());
    }
    out.extend_from_slice(&0u16.to_le_bytes()); // wide NUL terminator
    out
}

/// A `&str` → a `NUL`-terminated wide string for the `…W` APIs.
fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn warn(message: &str) {
    eprintln!("[universallink-clipboard] {message}");
}

// --- DIB ↔ PNG codec (pure; round-tripped in the unit tests) ---------------

const DIB_V5_HEADER_SIZE: u32 = 124;
const BI_RGB: u32 = 0;
const BI_BITFIELDS: u32 = 3;
/// `LCS_sRGB` color space tag (`'sRGB'`).
const LCS_SRGB: u32 = 0x7352_4742;
// The 32bpp channel masks we write (and require, byte order B,G,R,A in memory).
const MASK_R: u32 = 0x00FF_0000;
const MASK_G: u32 = 0x0000_FF00;
const MASK_B: u32 = 0x0000_00FF;
const MASK_A: u32 = 0xFF00_0000;

/// PNG bytes → a `CF_DIBV5` blob (decode via `image`, then lay out the DIB).
fn png_to_dibv5(png: &[u8]) -> Option<Vec<u8>> {
    let rgba = image::load_from_memory_with_format(png, image::ImageFormat::Png)
        .ok()?
        .to_rgba8();
    Some(rgba_to_dibv5(rgba.width(), rgba.height(), rgba.as_raw()))
}

/// A `CF_DIBV5`/`CF_DIB` blob → PNG bytes (decode the DIB, then encode via
/// `image`). `None` for a layout we do not decode (see [`dib_to_rgba`]).
fn dib_to_png(dib: &[u8]) -> Option<Vec<u8>> {
    let (w, h, rgba) = dib_to_rgba(dib)?;
    let mut out = Vec::new();
    use image::ImageEncoder;
    image::codecs::png::PngEncoder::new(&mut out)
        .write_image(&rgba, w, h, image::ExtendedColorType::Rgba8)
        .ok()?;
    Some(out)
}

/// RGBA8 pixels → a bottom-up 32bpp `BITMAPV5HEADER` DIB with explicit ARGB
/// bit-field masks. The explicit alpha mask (`0xFF000000`) is what avoids the
/// well-known "32bpp DIB alpha is ignored" bug on consumers that honor it.
fn rgba_to_dibv5(width: u32, height: u32, rgba: &[u8]) -> Vec<u8> {
    let pixels = width as usize * height as usize * 4;
    let mut out = Vec::with_capacity(DIB_V5_HEADER_SIZE as usize + pixels);
    // BITMAPV5HEADER (124 bytes), little-endian.
    out.extend_from_slice(&DIB_V5_HEADER_SIZE.to_le_bytes()); // bV5Size
    out.extend_from_slice(&(width as i32).to_le_bytes()); // bV5Width
    out.extend_from_slice(&(height as i32).to_le_bytes()); // bV5Height (>0: bottom-up)
    out.extend_from_slice(&1u16.to_le_bytes()); // bV5Planes
    out.extend_from_slice(&32u16.to_le_bytes()); // bV5BitCount
    out.extend_from_slice(&BI_BITFIELDS.to_le_bytes()); // bV5Compression
    out.extend_from_slice(&(pixels as u32).to_le_bytes()); // bV5SizeImage
    out.extend_from_slice(&0i32.to_le_bytes()); // bV5XPelsPerMeter
    out.extend_from_slice(&0i32.to_le_bytes()); // bV5YPelsPerMeter
    out.extend_from_slice(&0u32.to_le_bytes()); // bV5ClrUsed
    out.extend_from_slice(&0u32.to_le_bytes()); // bV5ClrImportant
    out.extend_from_slice(&MASK_R.to_le_bytes()); // bV5RedMask
    out.extend_from_slice(&MASK_G.to_le_bytes()); // bV5GreenMask
    out.extend_from_slice(&MASK_B.to_le_bytes()); // bV5BlueMask
    out.extend_from_slice(&MASK_A.to_le_bytes()); // bV5AlphaMask
    out.extend_from_slice(&LCS_SRGB.to_le_bytes()); // bV5CSType
    out.extend_from_slice(&[0u8; 36]); // bV5Endpoints (CIEXYZTRIPLE)
    out.extend_from_slice(&0u32.to_le_bytes()); // bV5GammaRed
    out.extend_from_slice(&0u32.to_le_bytes()); // bV5GammaGreen
    out.extend_from_slice(&0u32.to_le_bytes()); // bV5GammaBlue
    out.extend_from_slice(&0u32.to_le_bytes()); // bV5Intent
    out.extend_from_slice(&0u32.to_le_bytes()); // bV5ProfileData
    out.extend_from_slice(&0u32.to_le_bytes()); // bV5ProfileSize
    out.extend_from_slice(&0u32.to_le_bytes()); // bV5Reserved
    debug_assert_eq!(out.len(), DIB_V5_HEADER_SIZE as usize);
    // Pixel array, bottom-up, each pixel B,G,R,A.
    for dib_row in 0..height {
        let src_y = height - 1 - dib_row;
        for x in 0..width {
            let i = ((src_y * width + x) * 4) as usize;
            out.push(rgba[i + 2]); // B
            out.push(rgba[i + 1]); // G
            out.push(rgba[i]); // R
            out.push(rgba[i + 3]); // A
        }
    }
    out
}

/// A `BITMAPINFOHEADER`/`V4`/`V5` DIB → RGBA8 pixels. Supports 24bpp (BGR) and
/// 32bpp (BGRA), `BI_RGB` or `BI_BITFIELDS` with the conventional ARGB masks;
/// any other layout (palettized, exotic masks, other header sizes) returns
/// `None` rather than risk wrong colors. A 32bpp source whose alpha is uniformly
/// zero is treated as opaque (the "alpha ignored" bug mitigation).
fn dib_to_rgba(dib: &[u8]) -> Option<(u32, u32, Vec<u8>)> {
    if dib.len() < 40 {
        return None;
    }
    let header_size = read_u32(dib, 0);
    if header_size != 40 && header_size != 108 && header_size != 124 {
        return None;
    }
    if dib.len() < header_size as usize {
        return None;
    }
    let width = read_i32(dib, 4);
    let height = read_i32(dib, 8);
    let bit_count = read_u16(dib, 14);
    let compression = read_u32(dib, 16);
    if width <= 0 {
        return None;
    }
    let w = width as u32;
    let top_down = height < 0;
    let h = height.unsigned_abs();
    if h == 0 {
        return None;
    }
    // Pixel-data offset, validating masks where they apply.
    let pixel_off = if compression == BI_RGB {
        header_size as usize
    } else if compression == BI_BITFIELDS {
        // The RGB masks sit at offset 40 either way — trailing a V3 (40-byte)
        // header, or as the `bV5RedMask..` fields inside a V4/V5 header. Only the
        // pixel-data offset differs (past the 3 trailing masks for V3).
        const MASK_OFF: usize = 40;
        let pixel = if header_size == 40 {
            MASK_OFF + 12
        } else {
            header_size as usize
        };
        if dib.len() < MASK_OFF + 12 {
            return None;
        }
        if read_u32(dib, MASK_OFF) != MASK_R
            || read_u32(dib, MASK_OFF + 4) != MASK_G
            || read_u32(dib, MASK_OFF + 8) != MASK_B
        {
            return None; // exotic channel order — do not guess
        }
        pixel
    } else {
        return None;
    };
    let (stride, bytes_per_px) = match bit_count {
        32 => (w as usize * 4, 4usize),
        24 => ((w as usize * 3 + 3) & !3, 3usize),
        _ => return None,
    };
    let needed = pixel_off.checked_add(stride.checked_mul(h as usize)?)?;
    if dib.len() < needed {
        return None;
    }
    let mut rgba = vec![0u8; w as usize * h as usize * 4];
    let mut all_alpha_zero = true;
    for dib_row in 0..h as usize {
        let src_y = if top_down {
            dib_row
        } else {
            h as usize - 1 - dib_row
        };
        let row = pixel_off + dib_row * stride;
        for x in 0..w as usize {
            let px = row + x * bytes_per_px;
            let (b, g, r, a) = if bytes_per_px == 4 {
                (dib[px], dib[px + 1], dib[px + 2], dib[px + 3])
            } else {
                (dib[px], dib[px + 1], dib[px + 2], 255)
            };
            if a != 0 {
                all_alpha_zero = false;
            }
            let d = (src_y * w as usize + x) * 4;
            rgba[d] = r;
            rgba[d + 1] = g;
            rgba[d + 2] = b;
            rgba[d + 3] = a;
        }
    }
    if bytes_per_px == 4 && all_alpha_zero {
        for px in rgba.chunks_exact_mut(4) {
            px[3] = 255;
        }
    }
    Some((w, h, rgba))
}

fn read_u32(b: &[u8], off: usize) -> u32 {
    u32::from_le_bytes([b[off], b[off + 1], b[off + 2], b[off + 3]])
}

fn read_i32(b: &[u8], off: usize) -> i32 {
    i32::from_le_bytes([b[off], b[off + 1], b[off + 2], b[off + 3]])
}

fn read_u16(b: &[u8], off: usize) -> u16 {
    u16::from_le_bytes([b[off], b[off + 1]])
}

/// Message router for the backend window.
///
/// Re-entrancy: `WM_DESTROYCLIPBOARD` (sent synchronously by our own
/// `EmptyClipboard`) and `WM_RENDERALLFORMATS`/`WM_DESTROY` (on `DestroyWindow`)
/// must NEVER borrow `Backend` — we may already hold that `&mut`. They are pure
/// no-ops here. Only `WM_CLIPBOARDUPDATE`/`WM_RENDERFORMAT`/the wake load the
/// backend from `GWLP_USERDATA`, and those are never re-entrant (posted, or sent
/// from another thread and delivered only while we pump).
unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_DESTROYCLIPBOARD | WM_RENDERALLFORMATS | WM_DESTROY => 0,
        WM_CLIPBOARDUPDATE | WM_RENDERFORMAT | WM_APP_CMD => {
            let ptr = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *mut Backend;
            if ptr.is_null() {
                return unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) };
            }
            let backend = unsafe { &mut *ptr };
            match msg {
                WM_CLIPBOARDUPDATE => backend.on_clipboard_update(),
                WM_RENDERFORMAT => backend.on_render_format(wparam as u32),
                WM_APP_CMD => backend.process_cmds(),
                _ => unreachable!(),
            }
            0
        }
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

/// Connects to the OS clipboard: registers a unique window class, creates a
/// message-only window, starts listening for clipboard changes, and builds the
/// pinned backend plus the `Clone` handle and the upcall channel. A failure (no
/// usable clipboard, e.g. a headless session-0 station) surfaces as `Err` → the
/// caller treats it as `Unsupported`.
pub fn create() -> Result<crate::os::Created, String> {
    let cmds: Arc<Mutex<VecDeque<Cmd>>> = Arc::new(Mutex::new(VecDeque::new()));
    let paste = PasteSync::new();
    let (events_tx, backend_events) = mpsc::channel(BACKEND_EVENT_CAPACITY);
    let backend = Backend::new(cmds.clone(), paste.clone(), events_tx)?;
    let handle = WindowsBackend {
        hwnd: backend.hwnd as isize,
        cmds,
        paste,
    };
    Ok(crate::os::Created {
        handle,
        backend_events,
        event_loop: WindowsLoop { backend },
    })
}

#[cfg(test)]
mod tests {
    //! Pure-helper tests: they touch no OS clipboard (that is the `#[ignore]`d
    //! live suite in `tests/windows.rs`). They compile and run on the Windows
    //! target in CI (the module is `cfg(target_os = "windows")`).
    use super::*;

    #[test]
    fn wide_text_blob_is_utf16le_nul_terminated() {
        assert_eq!(wide_text_blob(b"hi"), vec![b'h', 0, b'i', 0, 0, 0]);
        // Empty → the wide NUL terminator only.
        assert_eq!(wide_text_blob(b""), vec![0, 0]);
    }

    #[test]
    fn wide_text_blob_encodes_non_ascii() {
        // 'é' (U+00E9) is 0xE9 0x00 in UTF-16LE, then the NUL terminator.
        assert_eq!(wide_text_blob("é".as_bytes()), vec![0xE9, 0x00, 0x00, 0x00]);
    }

    #[test]
    fn clipboard_format_mapping_round_trips() {
        let f = Formats { png: 0xC001 };
        assert_eq!(f.format_for_clipformat(CF_UNICODETEXT), Some(FORMAT_TEXT));
        assert_eq!(f.format_for_clipformat(0xC001), Some(FORMAT_PNG));
        assert_eq!(f.format_for_clipformat(CF_DIBV5), Some(FORMAT_PNG));
        // CF_DIB is never promised (we advertise CF_DIBV5), so it maps to none.
        assert_eq!(f.format_for_clipformat(CF_DIB), None);
        assert_eq!(f.format_for_clipformat(999), None);

        let text = [Format {
            id: FORMAT_TEXT.into(),
            size: None,
        }];
        assert_eq!(f.offer_clipformats(&text), vec![CF_UNICODETEXT]);
        let image = [Format {
            id: FORMAT_PNG.into(),
            size: None,
        }];
        // "PNG" (0xC001) BEFORE CF_DIBV5, on purpose.
        assert_eq!(f.offer_clipformats(&image), vec![0xC001, CF_DIBV5]);
        let both = [
            Format {
                id: FORMAT_TEXT.into(),
                size: None,
            },
            Format {
                id: FORMAT_PNG.into(),
                size: None,
            },
        ];
        assert_eq!(
            f.offer_clipformats(&both),
            vec![CF_UNICODETEXT, 0xC001, CF_DIBV5]
        );
        assert!(
            f.offer_clipformats(&[Format {
                id: "files".into(),
                size: None,
            }])
            .is_empty()
        );
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
        assert!(!cache.is_empty());
        cache.invalidate();
        assert_eq!(cache.get(7, FORMAT_TEXT), None); // superseded by an offer
        assert!(cache.is_empty());
    }

    #[test]
    fn dibv5_round_trips_a_tiny_rgba_image() {
        let (w, h) = (2u32, 2u32);
        // Distinct colors and non-uniform alpha (so the opaque fixup stays off).
        let rgba = vec![
            255, 0, 0, 255, // red
            0, 255, 0, 128, // green, half alpha
            0, 0, 255, 255, // blue
            10, 20, 30, 40, // arbitrary
        ];
        let dib = rgba_to_dibv5(w, h, &rgba);
        assert_eq!(
            dib.len(),
            DIB_V5_HEADER_SIZE as usize + (w * h * 4) as usize
        );
        let (rw, rh, back) = dib_to_rgba(&dib).expect("decode our own DIBV5");
        assert_eq!((rw, rh), (w, h));
        assert_eq!(back, rgba, "pixels (and alpha) must survive the round trip");
    }

    #[test]
    fn png_dib_round_trip_preserves_pixels_and_alpha() {
        let (w, h) = (2u32, 2u32);
        let rgba = vec![
            255, 0, 0, 255, 0, 255, 0, 128, 0, 0, 255, 255, 10, 20, 30, 40,
        ];
        // Encode our reference RGBA to PNG, then PNG → DIBV5 → PNG → RGBA.
        let mut png = Vec::new();
        {
            use image::ImageEncoder;
            image::codecs::png::PngEncoder::new(&mut png)
                .write_image(&rgba, w, h, image::ExtendedColorType::Rgba8)
                .expect("encode reference PNG");
        }
        let dib = png_to_dibv5(&png).expect("PNG → DIBV5");
        let back_png = dib_to_png(&dib).expect("DIBV5 → PNG");
        let decoded = image::load_from_memory_with_format(&back_png, image::ImageFormat::Png)
            .expect("decode round-tripped PNG")
            .to_rgba8();
        assert_eq!((decoded.width(), decoded.height()), (w, h));
        assert_eq!(decoded.as_raw(), &rgba);
    }

    #[test]
    fn dib_to_rgba_rejects_unsupported_layouts() {
        // Too short, and a bogus header size.
        assert!(dib_to_rgba(&[0u8; 10]).is_none());
        let mut bad = rgba_to_dibv5(1, 1, &[1, 2, 3, 4]);
        bad[0] = 41; // header size 41 is not one we accept
        assert!(dib_to_rgba(&bad).is_none());
    }

    #[test]
    fn dib_to_rgba_treats_all_zero_alpha_as_opaque() {
        // A 32bpp BI_RGB DIB (no alpha mask) with alpha bytes all zero.
        let (w, h) = (1u32, 1u32);
        let mut dib = Vec::new();
        dib.extend_from_slice(&40u32.to_le_bytes()); // BITMAPINFOHEADER
        dib.extend_from_slice(&(w as i32).to_le_bytes());
        dib.extend_from_slice(&(h as i32).to_le_bytes());
        dib.extend_from_slice(&1u16.to_le_bytes());
        dib.extend_from_slice(&32u16.to_le_bytes());
        dib.extend_from_slice(&BI_RGB.to_le_bytes());
        dib.extend_from_slice(&[0u8; 20]); // sizeimage + ppm + clr* (ignored)
        dib.extend_from_slice(&[7, 8, 9, 0]); // one pixel B,G,R,A with A=0
        let (_, _, rgba) = dib_to_rgba(&dib).expect("decode BI_RGB 32bpp");
        assert_eq!(rgba, vec![9, 8, 7, 255], "R,G,B and forced-opaque alpha");
    }
}
