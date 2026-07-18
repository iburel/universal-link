// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

//! The macOS clipboard backend: an owner-driven, delayed-render state machine
//! that plugs into the frozen [`crate::backend`] seam. Built on AppKit's
//! `NSPasteboard` through the typed `objc2` bindings. Scope: `text`, `image/png`
//! (brick 4) and `files` (brick 7 — see the files section below).
//!
//! One thread does everything here, and it MUST be the main thread. Our
//! `main.rs` already pins [`MacLoop::run`] to the main thread and runs the async
//! orchestrator on a side thread; the SIDE thread drives the OS through the
//! cheap, `Clone` [`MacBackend`] handle and observes local activity through the
//! `BackendEvent` channel the loop pushes on (`try_send`, never blocking).
//!
//! # The critical divergence from X11: `provideDataForType:` is SYNCHRONOUS
//!
//! `declareTypes:owner:` posts no bytes — it is a promise. When an app pastes,
//! AppKit calls `pasteboard:provideDataForType:` on our owner object, and the
//! pasting app blocks inside `dataForType:` until we post the data (or return
//! `nil` = a clean refusal). So the provide callback must BLOCK on the main
//! thread until the orchestrator delivers the bytes it pulled over the network.
//! Those bytes therefore CANNOT travel through the command queue: while the pump
//! is blocked in the callback it does not drain the queue, so a queued `deliver`
//! would deadlock. They travel through a separate direct rendezvous instead — a
//! [`PasteSync`] (`Mutex<Option<PasteOutcome>>` + `Condvar`) the handle also
//! holds. A hard [`PASTE_TIMEOUT`] refuses cleanly if nothing ever comes, so the
//! pasting app never freezes indefinitely. This is the exact transposition of the
//! Windows `WM_RENDERFORMAT` model.
//!
//! # The main-run-loop requirement (~60 s freeze otherwise)
//!
//! While WE own the pasteboard, the pboard server calls back into our process ON
//! ITS MAIN RUN LOOP to serve cross-process lazy reads. If the main run loop is
//! not pumping, any other app reading the clipboard freezes for the ~60 s
//! watchdog before it aborts. So the whole loop lives on the main thread and
//! pumps the run loop every turn (this is simpler than clipnet's two-run-loop
//! split, and matches windows.rs). No `NSApplication` is needed — a bare main run
//! loop suffices.
//!
//! # No change event → poll `changeCount`; anti-echo without owner identity
//!
//! macOS offers no clipboard-change notification, so we poll `changeCount` every
//! [`POLL_INTERVAL`] and re-read only when it moves. macOS also exposes no "who
//! owns the pasteboard" query, so anti-echo is done by value: our own mutations
//! (`declareTypes`/`clearContents`) return the new `changeCount`; we record it in
//! `last_change` and ignore any poll whose counter equals it. Ownership (needed to
//! relinquish cleanly) is tracked SEPARATELY by an explicit `is_owner` flag —
//! `changeCount` cannot double as ownership proof, because a foreign copy
//! overwrites `last_change` with its own value. But `is_owner` can itself lag:
//! with no change event, a foreign copy landing while the pump is blocked is not
//! seen until the next poll, and commands are drained before it — so `on_release`
//! additionally re-reads `changeCount` live and skips the clear when a foreign
//! change has intervened, never wiping another app's clipboard (see there).
//! Because our own `clearContents` is absorbed by `last_change`, a self-release
//! never resurfaces as a foreign clear — this subsumes X11's separate
//! `last_owner_foreign` gate (there is no explicit clear event to disambiguate
//! here).
//!
//! # Re-entrancy
//!
//! Our own `declareTypes`/`clearContents` can synchronously re-invoke
//! `provideDataForType:` for the PREVIOUS promise, on this same thread. A
//! `suppress` flag on the owner is raised around every self-mutation; under it the
//! provide callback is an immediate no-op (returns `nil`). This is the analogue of
//! windows.rs no-op'ing `WM_DESTROYCLIPBOARD`.
//!
//! # Images
//!
//! On the wire images are `image/png`. On READ we prefer `public.png` verbatim
//! (lossless); failing that we take `public.tiff` and convert TIFF → PNG. On WRITE
//! (an offer) we promise BOTH `public.png` AND `public.tiff` (PNG advertised
//! first); at paste time we render whichever exact type was requested, converting
//! the delivered PNG to TIFF when TIFF is asked for. The conversions use AppKit's
//! `NSBitmapImageRep` (native, in-memory, headless-safe via ImageIO — no image
//! codec crate).
//!
//! # Files: fill-on-paste through `NSFilePresenter` (both directions)
//!
//! A files clip does NOT ride the delayed-render `provideDataForType:` path above
//! (macOS file promises — `NSFilePromiseProvider` — are dead at Cmd-V: the paste
//! yields 0 bytes). Instead, on an `offer_files` we materialize a **skeleton** on
//! disk in a uid-private (0700) temp dir: real subdirectories plus one EMPTY file
//! per manifest leaf. We register **one `NSFilePresenter` per leaf file** (never a
//! single presenter on a directory — that pastes an empty folder) and publish only
//! the top-level `file://` URLs (`writeObjects:` of `NSURL`s, which are concrete —
//! not a lazy promise). When an app pastes (Cmd-V), the Finder performs a
//! coordinated read of each leaf; the system then calls
//! `relinquishPresentedItemToReader:` on that leaf's presenter, on the presenter's
//! serial `NSOperationQueue` — a BACKGROUND thread, NOT the main run loop, so it
//! does not block the run-loop pump and needs no `PasteSync`. On the FIRST such
//! relinquish we run ONE whole-clip [`FileFetcher::fill`] (every non-`dir` leaf →
//! its skeleton path); the Core writes those files itself (push). Concurrent and
//! later leaf relinquishes block on the SAME shared result through a
//! [`FillCoordinator`] (`Mutex<FillProgress>` + `Condvar`) until it is `Done`, then
//! release immediately. The reader is ALWAYS released — even on fill failure: the
//! Finder has no refusal channel, so a failed leaf simply stays empty (an
//! "incomplete paste", logged), never a partially-written file passed off as whole.
//!
//! Tear-down obeys the FreeRDP #12355 lesson: a `relinquish` may still be running
//! on a presenter's queue when its offer is superseded, and destroying the
//! presenter under its own callback is a use-after-free. So a superseded offer is
//! RETIRED (`removeFilePresenter:`, stopping new callbacks) into a kept-alive list
//! rather than dropped, and only freed at the next full release.
//!
//! Source side (this Mac copied files): `NSFilenamesPboardType` (a plist array of
//! POSIX paths) is read FIRST, before text/image — a files copy usually also
//! carries the paths as text, which must not be mistaken for content. The
//! top-level paths are announced as a `files` clip (`paths` set, no inline bytes);
//! the Core enumerates them. A `{files: []}` sentinel keeps the capture "live" so a
//! later foreign copy still supersedes it — exactly the X11 / Windows pattern.

use std::collections::{HashMap, VecDeque};
use std::fs::File;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use block2::{DynBlock, RcBlock};
use objc2::rc::{Retained, autoreleasepool};
use objc2::runtime::{AnyObject, NSObject, NSObjectProtocol, ProtocolObject};
use objc2::{AnyThread, DefinedClass, define_class, msg_send};
// `NSFilenamesPboardType` is deprecated (10.14) but is still the only files type
// that is agnostic to the file COUNT — the modern `public.file-url` is one item
// per file, which a pure metadata offer cannot size up front. The Finder still
// translates it, so it stays the source-detection type.
#[allow(deprecated)]
use objc2_app_kit::NSFilenamesPboardType;
use objc2_app_kit::{
    NSBitmapImageFileType, NSBitmapImageRep, NSBitmapImageRepPropertyKey, NSPasteboard,
    NSPasteboardType, NSPasteboardTypeOwner, NSPasteboardTypePNG, NSPasteboardTypeString,
    NSPasteboardTypeTIFF, NSPasteboardWriting,
};
use objc2_foundation::{
    NSArray, NSData, NSDate, NSDefaultRunLoopMode, NSDictionary, NSFileCoordinator,
    NSFilePresenter, NSInteger, NSOperationQueue, NSRunLoop, NSString, NSURL,
};
use tokio::sync::{mpsc, oneshot};

use crate::backend::{
    BackendEvent, ClipboardBackend, FileFetcher, Format, LocalClip, RemoteClip, RemoteFile,
};

/// Deadline of a pending paste: past it, refuse cleanly (the app never freezes).
/// A safety net independent of `release()` when a disconnection is silent.
const PASTE_TIMEOUT: Duration = Duration::from_secs(30);

/// Poll period of `changeCount` (macOS has no change notification; clipboard
/// managers conventionally poll at 200–500 ms). Also bounds command latency
/// (`Offer`/`Release`) and shutdown reactivity — all drained at the top of the
/// loop, so ≤ this after `request_exit` (which also Aborts any in-flight paste).
const POLL_INTERVAL: Duration = Duration::from_millis(200);

/// Floor for one run-loop turn before we own anything: `runMode:beforeDate:`
/// returns immediately when no input source is attached, so without a floor the
/// loop would busy-spin until the first ownership. Once we own the pasteboard the
/// run loop blocks on its sources and returns promptly on a message.
const RUN_LOOP_IDLE_FLOOR: Duration = Duration::from_millis(20);

/// Bounded capacity of the upcall channel. Generous; the orchestrator drains
/// promptly, and a full queue only ever means it has stalled or gone.
const BACKEND_EVENT_CAPACITY: usize = 256;

/// Core v1 format strings.
const FORMAT_TEXT: &str = "text";
const FORMAT_PNG: &str = "image/png";
const FORMAT_FILES: &str = "files";

// --- The modern pasteboard UTIs, as their stable string values. Kept as plain
// `&str` so the format↔type mapping is pure and unit-tested off a Mac; the thin
// `ns_type_for_uti` bridge maps these strings to the AppKit `extern` statics
// (`NSPasteboardTypeString` == `public.utf8-plain-text`, `NSPasteboardTypePNG` ==
// `public.png`, `NSPasteboardTypeTIFF` == `public.tiff`). Matching by string
// value (not pointer) is also what the provide callback needs: the system may
// hand back an equal-but-distinct `NSString`. ---
const UTI_TEXT: &str = "public.utf8-plain-text";
const UTI_PNG: &str = "public.png";
const UTI_TIFF: &str = "public.tiff";

/// A downcall from the orchestrator, queued for the main-thread loop. Note the
/// two paste replies — `deliver`/`paste_failed` — are NOT here: they go through
/// [`PasteSync`] because the pump is blocked in the synchronous provide callback
/// and could not drain this queue (see the module docs).
enum Cmd {
    /// Answer `provide(generation, format)` from the eager-read cache.
    Provide {
        generation: u64,
        format: String,
        reply: oneshot::Sender<Option<Vec<u8>>>,
    },
    /// Take ownership of the pasteboard, promising a remote clip (bytes later).
    Offer(RemoteClip),
    /// Take ownership for a remote FILES clip: build the skeleton, register a
    /// presenter per leaf, and publish the top-level `file://` URLs. The leaves
    /// are filled from `fetcher` on the first coordinated read (fill-on-paste).
    OfferFiles {
        clip: RemoteClip,
        fetcher: Arc<dyn FileFetcher>,
    },
    /// Drop OS ownership (promise withdrawn / superseded / shutting down).
    Release,
    /// Stop the loop with this process exit code (dropping ownership first).
    Exit(i32),
}

/// The verdict the orchestrator hands the blocked provide callback, through
/// [`PasteSync`]. `Bytes`/`Refuse` are gated on the paste `token`; `Abort`
/// unconditionally unblocks any in-flight render (session ending).
enum PasteOutcome {
    /// Fetched bytes for the pending paste `token`.
    Bytes(u64, Vec<u8>),
    /// The pending paste `token` could not be satisfied: refuse cleanly.
    Refuse(u64),
    /// Session ending: unblock immediately with a clean refusal.
    Abort,
}

/// The synchronous rendezvous between the orchestrator (which delivers) and the
/// blocked provide callback (which waits). Shared by `Arc` between the
/// [`MacBackend`] handle and the pump-side owner. Copied almost verbatim from the
/// Windows backend.
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

    /// Store a verdict and wake the blocked callback.
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
/// resource — just the shared command queue and the paste rendezvous. A downcall
/// either pushes a [`Cmd`] onto the queue (drained within one [`POLL_INTERVAL`];
/// no wake is needed, and shutdown Aborts the paste immediately) or resolves the
/// [`PasteSync`] rendezvous directly.
#[derive(Clone)]
pub struct MacBackend {
    cmds: Arc<Mutex<VecDeque<Cmd>>>,
    paste: Arc<PasteSync>,
}

impl MacBackend {
    /// Enqueue a command. Recovers a poisoned mutex so `Release`/`Exit` are never
    /// lost. No wake: the main loop drains the queue every poll turn (≤ 200 ms).
    fn push(&self, cmd: Cmd) {
        match self.cmds.lock() {
            Ok(mut q) => q.push_back(cmd),
            Err(p) => p.into_inner().push_back(cmd),
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

impl ClipboardBackend for MacBackend {
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

    fn deliver(&self, token: u64, _format: &str, bytes: Vec<u8>) {
        // Straight to the blocked provide callback (never the command queue). The
        // callback already knows the exact pasteboard type requested, so the Core
        // format is not needed here — only the token, which gates staleness.
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
pub struct MacLoop {
    backend: Backend,
}

impl MacLoop {
    /// Pumps the run loop on the calling (main) thread until a [`Cmd::Exit`] (or a
    /// vanished orchestrator) stops it, dropping pasteboard ownership on the way
    /// out. Returns the requested exit code.
    pub fn run(mut self) -> i32 {
        self.backend.run()
    }
}

/// Core format → the pasteboard UTIs to promise for an offer, in preference
/// order. Pure (plain `&str`), so it is unit-tested without AppKit. For an image,
/// `public.png` is advertised BEFORE `public.tiff`: PNG is lossless and passes
/// through verbatim. Unknown formats contribute nothing (their paste is refused).
fn utis_for_format(format: &str) -> &'static [&'static str] {
    match format {
        FORMAT_TEXT => &[UTI_TEXT],
        FORMAT_PNG => &[UTI_PNG, UTI_TIFF],
        _ => &[],
    }
}

/// A requested pasteboard UTI → the Core format we would render, if any. Pure, so
/// it is unit-tested without AppKit. Both `public.png` and `public.tiff` map to
/// `image/png` (the wire format is always PNG; TIFF is converted at render time).
fn format_for_uti(uti: &str) -> Option<&'static str> {
    if uti == UTI_TEXT {
        Some(FORMAT_TEXT)
    } else if uti == UTI_PNG || uti == UTI_TIFF {
        Some(FORMAT_PNG)
    } else {
        None
    }
}

/// A pasteboard UTI string → the matching AppKit `extern` static. The thin bridge
/// from the pure string layer to the runtime type objects.
fn ns_type_for_uti(uti: &str) -> Option<&'static NSPasteboardType> {
    // SAFETY: AppKit `extern` statics, valid while the framework is loaded.
    if uti == UTI_TEXT {
        Some(unsafe { NSPasteboardTypeString })
    } else if uti == UTI_PNG {
        Some(unsafe { NSPasteboardTypePNG })
    } else if uti == UTI_TIFF {
        Some(unsafe { NSPasteboardTypeTIFF })
    } else {
        None
    }
}

// --- TIFF ↔ PNG codec via AppKit's NSBitmapImageRep (native, in-memory,
// headless-safe through ImageIO — no image crate). A `None` result means the
// input did not decode: on read we skip that format, on write we refuse cleanly;
// never a panic. ---

/// Decode `input` (any format `NSBitmapImageRep` understands) and re-encode it to
/// `storage_type`. `None` if the input is undecodable or the encode returns nil.
fn image_convert(input: &[u8], storage_type: NSBitmapImageFileType) -> Option<Vec<u8>> {
    let data = NSData::with_bytes(input);
    let rep = NSBitmapImageRep::imageRepWithData(&data)?;
    let props: Retained<NSDictionary<NSBitmapImageRepPropertyKey, AnyObject>> = NSDictionary::new();
    // SAFETY: `props` has the type the method's generic expects (an empty options
    // dictionary — we take the codec defaults).
    let out = unsafe { rep.representationUsingType_properties(storage_type, &props) }?;
    Some(out.to_vec())
}

/// TIFF bytes → PNG bytes.
fn tiff_to_png(tiff: &[u8]) -> Option<Vec<u8>> {
    image_convert(tiff, NSBitmapImageFileType::PNG)
}

/// PNG bytes → TIFF bytes.
fn png_to_tiff(png: &[u8]) -> Option<Vec<u8>> {
    image_convert(png, NSBitmapImageFileType::TIFF)
}

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

/// Instance variables of the owner object. The provide callback is an ObjC method
/// with only `&self`, so everything it needs lives here (mutated through interior
/// mutability). All fields are `Send`+`Sync` (atomics + `Arc`/channel), so the
/// class need not be `MainThreadOnly`.
struct OwnerIvars {
    /// Bounded upcall channel to the orchestrator (a clone of the loop's sender).
    events_tx: mpsc::Sender<BackendEvent>,
    paste: Arc<PasteSync>,
    /// Raised around our own `declareTypes`/`clearContents`: under it, the provide
    /// callback is a no-op (anti re-entrancy — see the module docs).
    suppress: AtomicBool,
    /// Monotonic id per deferred paste (correlation token). Lives ENTIRELY here:
    /// only the callback mints paste tokens. SEPARATE from the source-side
    /// `next_generation` — conflating them breaks the seam.
    next_paste_token: AtomicU64,
}

define_class!(
    // SAFETY:
    // - The `NSObject` superclass imposes no subclassing requirement.
    // - This class overrides no `dealloc`/`Drop` in an unsound way, and every
    //   ivar is `Send`+`Sync`, so the class is not `MainThreadOnly`.
    #[unsafe(super(NSObject))]
    #[name = "UniversalLinkClipboardPasteboardOwner"]
    #[ivars = OwnerIvars]
    struct Owner;

    unsafe impl NSObjectProtocol for Owner {}

    unsafe impl NSPasteboardTypeOwner for Owner {
        // Delayed render: AppKit asks us for the bytes of a promised type.
        // Synchronous from the pasting app's point of view.
        #[unsafe(method(pasteboard:provideDataForType:))]
        fn provide_data(&self, sender: &NSPasteboard, ty: &NSPasteboardType) {
            self.on_provide(sender, ty);
        }
    }
);

impl Owner {
    fn new(events_tx: mpsc::Sender<BackendEvent>, paste: Arc<PasteSync>) -> Retained<Self> {
        let this = Self::alloc().set_ivars(OwnerIvars {
            events_tx,
            paste,
            suppress: AtomicBool::new(false),
            next_paste_token: AtomicU64::new(0),
        });
        // SAFETY: `NSObject`'s `init` has this signature.
        unsafe { msg_send![super(this), init] }
    }

    /// Raise/lower the anti-re-entrancy flag around our own mutations.
    fn set_suppress(&self, on: bool) {
        self.ivars().suppress.store(on, Ordering::SeqCst);
    }

    /// The heart of the delayed render: emit `Paste`, block until the pulled bytes
    /// arrive, then post them — rendering as the EXACT type requested (converting
    /// the delivered PNG to TIFF if TIFF was asked for). On any failure, post
    /// nothing (`dataForType:` returns nil = a clean refusal; the app never
    /// freezes). A failed post for one paste is recoverable — logged, never fatal.
    fn on_provide(&self, sender: &NSPasteboard, ty: &NSPasteboardType) {
        if self.ivars().suppress.load(Ordering::SeqCst) {
            // Forced resolution during our own declareTypes/clearContents: post
            // nothing (the previous promise is being replaced or dropped).
            return;
        }
        let uti = ty.to_string();
        let Some(format) = format_for_uti(&uti) else {
            return; // a type we never promised — clean refusal
        };
        self.ivars().paste.reset();
        let token = self
            .ivars()
            .next_paste_token
            .fetch_add(1, Ordering::Relaxed);
        use mpsc::error::TrySendError;
        match self.ivars().events_tx.try_send(BackendEvent::Paste {
            format: format.to_string(),
            token,
        }) {
            Ok(()) => {}
            // The orchestrator is gone or wedged: refuse now rather than block for
            // PASTE_TIMEOUT with no one left to deliver.
            Err(TrySendError::Closed(_)) => return,
            Err(TrySendError::Full(_)) => {
                warn("backend event queue full; refusing a paste");
                return;
            }
        }
        let deadline = Instant::now() + PASTE_TIMEOUT;
        let Some(bytes) = self.ivars().paste.wait(token, deadline) else {
            return; // refused / superseded / timed out → clean refusal
        };
        render_into(sender, &uti, ty, bytes);
    }
}

/// Post the delivered bytes for the EXACT type requested (never a substitute):
/// UTF-8 text as an `NSString`, PNG verbatim, or the delivered PNG converted to
/// TIFF when TIFF is asked for. The pasteboard is already ours (via
/// `declareTypes:owner:`), so we post straight onto `sender`.
fn render_into(sender: &NSPasteboard, uti: &str, ty: &NSPasteboardType, bytes: Vec<u8>) {
    if uti == UTI_TEXT {
        // The seam invariant is `text == UTF-8`; `from_utf8_lossy` is a defensive
        // no-op on well-formed bytes and never posts a broken NSString.
        let s = NSString::from_str(&String::from_utf8_lossy(&bytes));
        sender.setString_forType(&s, ty);
    } else if uti == UTI_PNG {
        let d = NSData::with_bytes(&bytes);
        sender.setData_forType(Some(&d), ty);
    } else if uti == UTI_TIFF {
        match png_to_tiff(&bytes) {
            Some(tiff) => {
                let d = NSData::with_bytes(&tiff);
                sender.setData_forType(Some(&d), ty);
            }
            // Recoverable: post nothing (clean refusal), never a panic.
            None => warn("cannot encode PNG -> TIFF — paste refused"),
        }
    }
    // Any other UTI is unreachable (only promised types reach here) — no-op.
}

// === Files: fill-on-paste skeleton + NSFilePresenter (destination side) =======

/// The deprecated `NSFilenamesPboardType` (a plist array of POSIX paths) — the
/// only files type agnostic to the file count (see the module docs). Still
/// translated by the Finder, so it is our source-detection type.
fn filenames_type() -> &'static NSPasteboardType {
    // SAFETY: AppKit `extern` static, valid while the framework is loaded.
    #[allow(deprecated)]
    unsafe {
        NSFilenamesPboardType
    }
}

/// An existing on-disk `Path` → its `file://` `NSURL`. The skeleton already
/// exists when this runs, so `fileURLWithPath:`'s `stat` classifies it correctly.
fn file_url(path: &Path) -> Retained<NSURL> {
    let s = NSString::from_str(&path.to_string_lossy());
    NSURL::fileURLWithPath(&s)
}

/// A self-deleting, uid-private (0700) temp dir holding one files-clip skeleton.
/// Its `Drop` does a best-effort `remove_dir_all` (a fill in flight writes into an
/// already-unlinked inode — harmless). We avoid the dev-only `tempfile` crate,
/// mirroring the FUSE tier's uid-private mount discipline.
struct ScopedTempDir {
    path: PathBuf,
}

impl ScopedTempDir {
    fn new() -> std::io::Result<Self> {
        use std::os::unix::fs::DirBuilderExt;
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "universallink-clip-files-{}-{n}",
            std::process::id()
        ));
        std::fs::DirBuilder::new().mode(0o700).create(&dir)?;
        Ok(Self { path: dir })
    }
}

impl Drop for ScopedTempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// One skeleton leaf to fill: the manifest `file_id` and the absolute skeleton
/// path the Core will write DIRECTLY (no temp+rename — that is impossible on an
/// OS-watched skeleton path). These pairs are the [`FileFetcher::fill`] entries.
#[derive(Clone, Debug, PartialEq, Eq)]
struct LeafEntry {
    file_id: String,
    dest: PathBuf,
}

/// Whether a manifest path is unsafe to place in the skeleton. The receiving Core
/// has re-validated it, but a files backend must still never join an absolute
/// path, a `..`/`.` component, or a `\0` (never a filename). Unlike the X11/FUSE
/// tree we do NOT reject `:` — it is a legal byte in a macOS (APFS/HFS+) filename.
fn path_is_malformed(path: &str, comps: &[&str]) -> bool {
    path.starts_with('/')
        || comps
            .iter()
            .any(|c| *c == ".." || *c == "." || c.contains('\0'))
}

/// Build the skeleton for `files` under `base`: create every declared and implied
/// directory, and one EMPTY file per non-`dir` leaf (filled at paste time).
/// Returns the leaves (the `fill` entries) and the ordered, de-duplicated
/// top-level component names (the `file://` roots to publish). Malformed entries
/// are skipped, never joined. Pure of AppKit (std `fs` only), so it is unit-tested
/// off a Mac.
fn build_skeleton(
    base: &Path,
    files: &[RemoteFile],
) -> std::io::Result<(Vec<LeafEntry>, Vec<String>)> {
    let mut leaves: Vec<LeafEntry> = Vec::new();
    let mut roots: Vec<String> = Vec::new();
    for f in files {
        let comps: Vec<&str> = f.path.split('/').filter(|c| !c.is_empty()).collect();
        if comps.is_empty() || path_is_malformed(&f.path, &comps) {
            continue;
        }
        let root = comps[0].to_string();
        if !roots.iter().any(|r| r == &root) {
            roots.push(root);
        }
        // Rebuild from the sanitized components so a leading/duplicate `/` cannot
        // escape `base`.
        let dest = base.join(comps.iter().collect::<PathBuf>());
        if f.dir {
            std::fs::create_dir_all(&dest)?;
        } else {
            if let Some(parent) = dest.parent() {
                std::fs::create_dir_all(parent)?;
            }
            File::create(&dest)?; // empty leaf — filled on the first coordinated read
            leaves.push(LeafEntry {
                file_id: f.file_id.clone(),
                dest,
            });
        }
    }
    Ok((leaves, roots))
}

/// State of the one whole-clip fill shared by every leaf presenter of an offer.
enum FillProgress {
    /// No leaf has been read yet.
    Idle,
    /// One leaf's relinquish is running the whole-clip fill; others wait on it.
    InFlight,
    /// The fill finished; `success` is `false` if it failed (leaves may be empty).
    Done { success: bool },
}

/// Coordinates the single whole-clip fill across an offer's leaf presenters.
/// Shared by `Arc` between all presenters of one offer and the [`FileOffer`]. The
/// FIRST relinquish to arrive runs [`FileFetcher::fill`] ONCE with every leaf
/// entry; concurrent and later relinquishes block on the same result and then
/// release immediately. Pure Rust (no AppKit), so the coordination is unit-tested.
struct FillCoordinator {
    fetcher: Arc<dyn FileFetcher>,
    /// Every non-`dir` leaf as `(file_id, dest_path)`: the exact `fill` argument.
    entries: Vec<(String, PathBuf)>,
    progress: Mutex<FillProgress>,
    cv: Condvar,
}

impl FillCoordinator {
    fn new(fetcher: Arc<dyn FileFetcher>, entries: Vec<(String, PathBuf)>) -> Arc<Self> {
        Arc::new(Self {
            fetcher,
            entries,
            progress: Mutex::new(FillProgress::Idle),
            cv: Condvar::new(),
        })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, FillProgress> {
        self.progress.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Ensure the whole clip is filled, blocking until it is (called from a
    /// presenter's serial operation queue — a plain OS thread, so the blocking
    /// `fill` is legal here). Returns only once it is safe to release the reader:
    /// the first caller runs the fill; the rest wait on the `Condvar`, and every
    /// caller returns whether the fill succeeded or failed (the reader is released
    /// either way — the Finder has no refusal channel).
    fn ensure_filled(&self) {
        let mut guard = self.lock();
        loop {
            match &*guard {
                FillProgress::Idle => {
                    // We are first: claim the fill, then run it WITHOUT holding the
                    // lock so waiters can queue on the Condvar meanwhile.
                    *guard = FillProgress::InFlight;
                    drop(guard);
                    let success = self.run_fill();
                    let mut g = self.lock();
                    *g = FillProgress::Done { success };
                    self.cv.notify_all();
                    return;
                }
                // Another leaf is filling the whole clip: wait for it to finish.
                FillProgress::InFlight => {
                    guard = self.cv.wait(guard).unwrap_or_else(|p| p.into_inner());
                }
                FillProgress::Done { .. } => return,
            }
        }
    }

    /// Issue the one blocking whole-clip `fill`. `Ok` → the Core wrote every leaf;
    /// `Err` → left incomplete (logged, counted through `Done{success:false}`).
    /// A panic in `fill` is caught: it must not unwind across the Objective-C
    /// relinquish callback (undefined behavior — the brick-6 COM-boundary lesson),
    /// and it must not skip the `Done`/notify that unblocks the other leaves'
    /// waiters. A caught panic is treated as a failed (incomplete) fill.
    fn run_fill(&self) -> bool {
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.fetcher.fill(&self.entries)
        }));
        match outcome {
            Ok(Ok(_)) => true,
            Ok(Err(e)) => {
                warn(&format!("files fill failed — paste left incomplete: {e}"));
                false
            }
            Err(_) => {
                warn("files fill panicked — paste left incomplete");
                false
            }
        }
    }

    /// Whether the fill finished but failed (at least one empty leaf was pasted).
    fn incomplete(&self) -> bool {
        matches!(&*self.lock(), FillProgress::Done { success: false })
    }
}

/// Instance variables of a leaf file presenter. All fields are `Send`+`Sync`
/// (`Retained<NSURL>`/`Retained<NSOperationQueue>` are immutable/thread-safe;
/// `Arc<FillCoordinator>` is `Send`+`Sync`), so the class is NOT `MainThreadOnly`
/// — its callbacks are delivered on the instance's own serial operation queue.
struct PresenterIvars {
    /// `file://` URL of this leaf (returned by `presentedItemURL`).
    url: Retained<NSURL>,
    /// The SERIAL queue (maxConcurrency 1) the system schedules our callbacks on.
    queue: Retained<NSOperationQueue>,
    /// Shared with every leaf of this offer: runs the one whole-clip fill.
    coordinator: Arc<FillCoordinator>,
}

define_class!(
    // SAFETY:
    // - The `NSObject` superclass imposes no subclassing requirement.
    // - No unsound `dealloc`/`Drop`; every ivar is `Send`+`Sync`, so the class is
    //   not `MainThreadOnly` (callbacks arrive on the instance's serial queue).
    #[unsafe(super(NSObject))]
    #[name = "UniversalLinkClipboardFilePresenter"]
    #[ivars = PresenterIvars]
    struct FilePresenter;

    unsafe impl NSObjectProtocol for FilePresenter {}

    unsafe impl NSFilePresenter for FilePresenter {
        // Required: the URL of the presented item (the leaf). Returns a `Retained`,
        // so it is a `method_id` (retain semantics), like the generated protocol.
        #[unsafe(method_id(presentedItemURL))]
        fn presented_item_url(&self) -> Option<Retained<NSURL>> {
            Some(self.ivars().url.clone())
        }

        // Required: the (serial) queue our callbacks are scheduled on — NEVER nil.
        #[unsafe(method_id(presentedItemOperationQueue))]
        fn presented_item_operation_queue(&self) -> Retained<NSOperationQueue> {
            self.ivars().queue.clone()
        }

        // Files fill-on-paste: before a coordinated reader (Finder at Cmd-V) reads
        // this leaf, the system asks us to relinquish access. We fill the WHOLE
        // clip once (shared coordinator), then invoke the `reader` block, handing
        // it a `reacquirer` (called back when the reader is done). Runs on this
        // instance's serial operation queue (a plain OS thread, off the tokio
        // runtime) — so the blocking `fill` is legal here.
        //
        // SAFETY: protocol-generated signature (`unsafe fn`, block-of-block). The
        // `reader` block is called exactly once; the `reacquirer` outlives that
        // call (the `reader` copies/retains it if it defers).
        #[unsafe(method(relinquishPresentedItemToReader:))]
        unsafe fn relinquish_to_reader(&self, reader: &DynBlock<dyn Fn(*mut DynBlock<dyn Fn()>)>) {
            // Clone the coordinator `Arc` FIRST, so NO borrow of `self` is held
            // across the blocking whole-clip fill. If this offer is released
            // (dropped) mid-fill — a withdraw / shutdown during an active Cmd-V —
            // the fill keeps running on the independently-owned `Arc` and this
            // method never dereferences the freed presenter again; `reader` is the
            // caller's own block, independent of `self`. Without the clone, the
            // `self.ivars()` borrow would span the whole (possibly long, networked)
            // transfer and a mid-fill drop would be a use-after-free.
            let coordinator = self.ivars().coordinator.clone();
            coordinator.ensure_filled();
            // Always release the reader, even if the fill failed (otherwise the
            // Finder would hang); the leaf then reads as the empty skeleton file.
            let reacquirer = RcBlock::new(|| {});
            reader.call((RcBlock::as_ptr(&reacquirer),));
        }
    }
);

impl FilePresenter {
    fn new(url: Retained<NSURL>, coordinator: Arc<FillCoordinator>) -> Retained<Self> {
        // A dedicated serial queue (one relinquish at a time for this leaf).
        let queue = NSOperationQueue::new();
        queue.setMaxConcurrentOperationCount(1);
        let this = Self::alloc().set_ivars(PresenterIvars {
            url,
            queue,
            coordinator,
        });
        // SAFETY: `NSObject`'s `init` has this signature.
        unsafe { msg_send![super(this), init] }
    }
}

/// A live files offer: the skeleton (self-deleting), its leaf presenters (kept
/// ALIVE — `addFilePresenter:` does not retain them), and the shared fill
/// coordinator. [`unregister`](Self::unregister) does the `removeFilePresenter:`
/// before the offer is retired/dropped.
struct FileOffer {
    _tempdir: ScopedTempDir,
    presenters: Vec<Retained<FilePresenter>>,
    coordinator: Arc<FillCoordinator>,
}

impl FileOffer {
    /// Deregister every presenter from the shared `NSFileCoordinator` (no new
    /// callbacks). An in-flight relinquish on a presenter's queue keeps running —
    /// which is why the offer is kept alive (retired), not dropped, until release.
    fn unregister(&self) {
        for p in &self.presenters {
            let proto = ProtocolObject::from_ref(&**p);
            NSFileCoordinator::removeFilePresenter(proto);
        }
    }
}

impl Drop for FileOffer {
    fn drop(&mut self) {
        // Diagnostic: at least one leaf was pasted empty (the fill failed / was
        // cut short). No native Finder refusal channel exists (mirrors clipnet);
        // for now we log it — a GUI notification lands with the desktop app. The
        // skeleton is removed immediately after (ScopedTempDir's Drop).
        if self.coordinator.incomplete() {
            warn("files paste left incomplete (a fill failed) — some leaves stayed empty");
        }
    }
}

/// Backend state, living on the pump (main) thread. Owns the non-`Send`
/// `Retained<NSPasteboard>` and `Retained<Owner>`, so it is not `Send` (created
/// and pumped here only).
struct Backend {
    pb: Retained<NSPasteboard>,
    /// The owner object (delayed render). MUST stay alive as long as we own the
    /// pasteboard: `declareTypes:owner:` does NOT retain it (a weak ref).
    owner: Retained<Owner>,
    cmds: Arc<Mutex<VecDeque<Cmd>>>,
    paste: Arc<PasteSync>,
    /// Bounded upcall channel to the orchestrator (never blocks the loop).
    events_tx: mpsc::Sender<BackendEvent>,
    cache: Cache,
    /// Monotonic id per local copy (announce generation).
    next_generation: u64,
    /// Last `changeCount` we observed or caused. Anti-echo ONLY (ignore a poll
    /// whose counter equals it); NOT proof of ownership — see `is_owner`.
    last_change: NSInteger,
    /// Whether we (still) own the pasteboard. Explicit flag (macOS has no owner
    /// identity): true after `declareTypes`, forced false the instant a foreign
    /// change is detected (AppKit has dropped our promise, like an X11
    /// `SelectionClear`). Guards `on_release` from wiping another app's clipboard.
    is_owner: bool,
    /// The live files offer's skeleton + presenters (fill-on-paste), if any.
    /// `None` outside a files clip.
    files_offer: Option<FileOffer>,
    /// Files offers RETIRED (presenters deregistered) but kept alive until the
    /// next full release: a `relinquish` may still be running on a presenter's
    /// queue when its offer is superseded, and dropping the presenter under its
    /// own callback is a use-after-free (FreeRDP #12355). Bounded by session.
    retired_files: Vec<FileOffer>,
    shutdown: bool,
    exit_code: Option<i32>,
}

impl Drop for Backend {
    /// Safety net at loop exit: relinquish ownership (dropping the delayed-render
    /// promise) and unblock any in-flight render, so an orphaned offer never
    /// wedges other apps' pastes.
    fn drop(&mut self) {
        self.on_release();
        self.paste.resolve(PasteOutcome::Abort);
    }
}

impl Backend {
    fn new(
        cmds: Arc<Mutex<VecDeque<Cmd>>>,
        paste: Arc<PasteSync>,
        events_tx: mpsc::Sender<BackendEvent>,
    ) -> Result<Self, String> {
        let pb = NSPasteboard::generalPasteboard();
        let owner = Owner::new(events_tx.clone(), paste.clone());
        let last_change = pb.changeCount();
        Ok(Self {
            pb,
            owner,
            cmds,
            paste,
            events_tx,
            cache: Cache::default(),
            next_generation: 0,
            last_change,
            is_owner: false,
            files_offer: None,
            retired_files: Vec::new(),
            shutdown: false,
            exit_code: None,
        })
    }

    /// The main-thread pump. Each turn drains commands, polls `changeCount`, then
    /// pumps the run loop for up to [`POLL_INTERVAL`] (where a cross-process
    /// provide callback is delivered and blocks). Commands run BEFORE observing OS
    /// changes, exactly like X11/Windows. Returns the process exit code.
    fn run(&mut self) -> i32 {
        // SAFETY: AppKit `extern` static (the run loop's default mode).
        let mode = unsafe { NSDefaultRunLoopMode };
        // We are the main thread (main.rs pins us here), so this is the main run
        // loop the pboard server calls back on.
        let run_loop = NSRunLoop::currentRunLoop();
        while !self.shutdown {
            autoreleasepool(|_| {
                self.process_cmds();
                if self.shutdown {
                    return;
                }
                self.poll_change_count();
                if self.shutdown {
                    return;
                }
                // Pump the run loop until the deadline (delivers provide
                // callbacks, which then block in the handler until a deliver).
                let start = Instant::now();
                let until = NSDate::dateWithTimeIntervalSinceNow(POLL_INTERVAL.as_secs_f64());
                run_loop.runMode_beforeDate(mode, &until);
                // If it returned early (no source attached yet), sleep the rest so
                // we do not busy-spin before the first ownership.
                let elapsed = start.elapsed();
                if elapsed < RUN_LOOP_IDLE_FLOOR {
                    std::thread::sleep(RUN_LOOP_IDLE_FLOOR - elapsed);
                }
            });
        }
        self.on_release();
        self.exit_code.unwrap_or(1)
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
        // Drain the WHOLE queue under one lock. Recover a poisoned mutex so
        // Exit/Release are never dropped.
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
                Cmd::Release => self.on_release(),
                Cmd::Exit(code) => {
                    self.exit_code = Some(code);
                    self.shutdown = true;
                }
            }
        }
    }

    /// Take ownership of the pasteboard, promising the remote `clip` with a
    /// delayed-render placeholder (`declareTypes:owner:`). A new offer also
    /// supersedes any local capture (a remote promise wins convergence), so
    /// `provide` for the old generation must stop vouching — invalidate the cache.
    fn on_offer(&mut self, clip: RemoteClip) {
        self.cache.invalidate();
        // A text/image offer supersedes any files offer: retire its presenters
        // (kept alive until release — a fill may be in flight on their queues).
        self.retire_files_offer();
        let types: Vec<&NSPasteboardType> = clip
            .formats
            .iter()
            .flat_map(|f| utis_for_format(&f.id))
            .filter_map(|uti| ns_type_for_uti(uti))
            .collect();
        if types.is_empty() {
            // Nothing we can promise: relinquish rather than hold a stale offer.
            self.relinquish_ownership();
            return;
        }
        // OS confidentiality markers (org.nspasteboard.ConcealedType et al.) are a
        // later brick: nothing is announced sensitive yet, exactly as brick 3
        // deferred the Windows clipboard-history exclusion.
        let array = NSArray::from_slice(&types);
        let owner_obj: &AnyObject = self.owner.as_ref();
        // Raise the anti-re-entrancy flag around our own acquisition.
        self.owner.set_suppress(true);
        // SAFETY: `owner_obj` conforms to `NSPasteboardTypeOwner`.
        let cc = unsafe { self.pb.declareTypes_owner(&array, Some(owner_obj)) };
        self.owner.set_suppress(false);
        self.last_change = cc;
        self.is_owner = true;
    }

    /// Take ownership for a remote FILES clip: build the skeleton, register one
    /// presenter per leaf, and publish the top-level `file://` URLs. A files offer
    /// supersedes any local capture (a remote promise wins convergence), so the
    /// cache is invalidated; any prior offer is retired first. Every refusal path
    /// relinquishes ownership so we never keep owning the pasteboard while
    /// promising files we cannot serve (the brick-5 phantom-ownership bug class).
    fn on_offer_files(&mut self, clip: RemoteClip, fetcher: Arc<dyn FileFetcher>) {
        self.cache.invalidate();
        self.retire_files_offer();

        let tempdir = match ScopedTempDir::new() {
            Ok(dir) => dir,
            Err(e) => {
                warn(&format!(
                    "cannot create files skeleton dir: {e} — files refused"
                ));
                self.relinquish_ownership();
                return;
            }
        };
        let (leaves, roots) = match build_skeleton(&tempdir.path, &clip.files) {
            Ok(built) => built,
            Err(e) => {
                warn(&format!("cannot build files skeleton: {e} — files refused"));
                self.relinquish_ownership();
                return;
            }
        };
        if leaves.is_empty() {
            // Nothing fillable (dir-only or all-malformed manifest): a dir-only
            // presenter would paste an empty folder, so make no promise.
            self.relinquish_ownership();
            return;
        }

        let entries: Vec<(String, PathBuf)> = leaves
            .iter()
            .map(|l| (l.file_id.clone(), l.dest.clone()))
            .collect();
        let coordinator = FillCoordinator::new(fetcher, entries);

        // One presenter per leaf (never a single one on a directory), each sharing
        // the offer's coordinator so the FIRST relinquish fills the whole clip.
        let mut presenters = Vec::with_capacity(leaves.len());
        for leaf in &leaves {
            let presenter = FilePresenter::new(file_url(&leaf.dest), coordinator.clone());
            // `addFilePresenter:` does NOT retain — we keep the presenter alive in
            // the FileOffer below.
            let proto = ProtocolObject::from_ref(&*presenter);
            NSFileCoordinator::addFilePresenter(proto);
            presenters.push(presenter);
        }

        // Publish ONLY the top-level `file://` URLs (`writeObjects:` writes them as
        // concrete data — NOT a lazy promise). For a directory root the Finder then
        // enumerates the now-non-empty tree and coordinated-reads each leaf.
        let root_urls: Vec<Retained<NSURL>> = roots
            .iter()
            .map(|r| file_url(&tempdir.path.join(r)))
            .collect();
        self.write_file_urls(&root_urls);

        self.files_offer = Some(FileOffer {
            _tempdir: tempdir,
            presenters,
            coordinator,
        });
    }

    /// Publish a list of `file://` URLs on the pasteboard (taking ownership), with
    /// the same anti-re-entrancy + anti-echo discipline as `on_offer`. NSURL is
    /// concrete `NSPasteboardWriting` data, so — unlike text/image — no delayed
    /// render is involved; the file CONTENTS come later via the leaf presenters.
    fn write_file_urls(&mut self, urls: &[Retained<NSURL>]) {
        let writers: Vec<&ProtocolObject<dyn NSPasteboardWriting>> = urls
            .iter()
            .map(|u| ProtocolObject::from_ref(&**u))
            .collect();
        let array = NSArray::from_slice(&writers);
        self.owner.set_suppress(true);
        let _ = self.pb.clearContents();
        let ok = self.pb.writeObjects(&array);
        self.owner.set_suppress(false);
        // `clearContents` + `writeObjects:` both moved the counter: record the
        // FINAL value so our own write is not re-read as a foreign copy.
        self.last_change = self.pb.changeCount();
        self.is_owner = true;
        if !ok {
            warn("writeObjects: refused some file URLs");
        }
    }

    /// Retire the current files offer: deregister its presenters (no new
    /// callbacks) and move it to the kept-alive list — a `relinquish` may still be
    /// running on a presenter's queue, and dropping it here would be a
    /// use-after-free (FreeRDP #12355). Freed at the next full release.
    fn retire_files_offer(&mut self) {
        if let Some(offer) = self.files_offer.take() {
            offer.unregister();
            self.retired_files.push(offer);
        }
    }

    /// Relinquish pasteboard ownership if we still hold it. Shared by `on_release`,
    /// the files-refusal branches, and (via `on_release`) shutdown / `Drop`.
    ///
    /// The `is_owner` flag alone is NOT enough: macOS delivers no change event, so
    /// a foreign copy that lands while the pump is blocked leaves `is_owner`
    /// stale-true until the next poll — and commands are drained BEFORE that poll
    /// each turn (and the shutdown path returns before it runs at all). So before
    /// clearing we re-read `changeCount` live: if it no longer equals the value our
    /// own last mutation produced, a foreign app has taken the pasteboard since
    /// (AppKit already dropped our promise). Clearing then would wipe that app's
    /// fresh copy, so we drop our claim instead and let the next poll read and
    /// announce it. This is the analogue of Windows' live `GetClipboardOwner() ==
    /// hwnd` guard in `relinquish`.
    fn relinquish_ownership(&mut self) {
        if !self.is_owner {
            return;
        }
        if self.pb.changeCount() != self.last_change {
            // A foreign copy raced in before the poll could observe it: we no
            // longer own the pasteboard. Do NOT clear (that would erase the other
            // app's copy). Drop our claim and leave `last_change` untouched so the
            // next poll detects the foreign change and announces it.
            self.is_owner = false;
            return;
        }
        self.owner.set_suppress(true);
        let cc = self.pb.clearContents();
        self.owner.set_suppress(false);
        // Anti-echo: our own clearContents must not be re-read as a foreign copy —
        // recording `cc` absorbs it (subsumes X11's clear gate).
        self.last_change = cc;
        self.is_owner = false;
    }

    /// Full release (promise withdrawn / superseded / shutting down): relinquish
    /// pasteboard ownership AND free every files offer. Retiring the current offer
    /// first (deregister, keep alive) then clearing the retired list frees them
    /// only once — the session is ending, so no live presenter should be filling.
    fn on_release(&mut self) {
        self.relinquish_ownership();
        self.retire_files_offer();
        self.retired_files.clear();
    }

    // ----- Source side: a foreign copy → eager read → Copied/Cleared -----

    /// Poll `changeCount`; on a genuine foreign move, drop our ownership claim and
    /// eager-read the new copy. Our own mutations are absorbed by `last_change`.
    fn poll_change_count(&mut self) {
        let cc = self.pb.changeCount();
        if cc == self.last_change {
            return; // no change, or our own mutation (anti-echo)
        }
        self.last_change = cc;
        // A foreign change: AppKit has dropped our delayed-render promise, so we
        // no longer own the pasteboard (analogue of an X11 SelectionClear). Vital
        // so `on_release` does not later wipe the clipboard of the app that copied.
        self.is_owner = false;
        self.read_and_announce_copy();
    }

    /// Eager-read every inline format the foreign owner offers, cache the bytes
    /// under a fresh generation, and announce the metadata. If nothing usable is
    /// there, supersede the stale capture instead of continuing to vouch for it.
    fn read_and_announce_copy(&mut self) {
        // Files take priority over inline formats, mirroring X11 / Windows: a files
        // copy usually ALSO offers the paths as text, which must not be mistaken
        // for content. If any file path is present, announce a files copy and stop
        // — the Core reads the paths, so no inline bytes are cached. A `{files: []}`
        // sentinel keeps the capture "live" so a later foreign non-files copy still
        // supersedes it (fires `Cleared`), exactly like the X11 / Windows sentinel.
        if let Some(paths) = self.read_files() {
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
                    // OS confidentiality markers (org.nspasteboard.ConcealedType)
                    // are brick 8: nothing is announced sensitive yet.
                    sensitive: false,
                },
            });
            return;
        }

        let mut bytes_by_format: HashMap<String, Vec<u8>> = HashMap::new();
        let mut formats: Vec<Format> = Vec::new();

        if let Some(text) = self.read_text() {
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
                sensitive: false,
            },
        });
    }

    /// A foreign owner took the pasteboard but we could not capture a usable clip
    /// (unsupported types, or an image that would not decode). Drop the stale
    /// local capture and announce a clear, so the orchestrator stops vouching for
    /// a generation the clipboard has already moved past — otherwise `provide`
    /// would keep serving bytes the user has replaced (a staleness + confidentiality
    /// bug).
    fn supersede_local(&mut self) {
        if !self.cache.is_empty() {
            self.cache.invalidate();
            self.emit(BackendEvent::Cleared);
        }
    }

    /// Read `NSFilenamesPboardType` (a plist array of POSIX paths) as a files copy:
    /// the announced TOP-LEVEL paths (the Core enumerates them itself — we do NOT
    /// walk directories, mirroring the X11 / Windows source sides). `None` if the
    /// type is absent or the array is empty (the caller then falls through to
    /// text/image).
    fn read_files(&self) -> Option<Vec<PathBuf>> {
        let plist = self.pb.propertyListForType(filenames_type())?;
        // `NSFilenamesPboardType` is, by contract, an `NSArray<NSString>` of POSIX
        // paths — but the general pasteboard is a shared surface ANY local process
        // can write, and AppKit stores/returns this legacy type's plist verbatim
        // with no schema enforcement. Sending `count`/`objectAtIndex:`/`NSString`
        // selectors to a wrong-class object (a dictionary, an array of numbers…)
        // would raise an Objective-C exception that unwinds out of the pump and
        // aborts the process. So validate the shape with CHECKED downcasts and
        // refuse the whole read (`None`) on any mismatch — never send a selector to
        // an unverified class. `NSArray<AnyObject>` is a downcast target (its
        // element type is not runtime-checkable, so we downcast each element to
        // `NSString` individually).
        let array = plist.downcast::<NSArray>().ok()?;
        let count = array.count();
        let mut paths = Vec::with_capacity(count);
        for i in 0..count {
            let element = array.objectAtIndex(i);
            let s = element.downcast::<NSString>().ok()?;
            paths.push(PathBuf::from(s.to_string()));
        }
        if paths.is_empty() { None } else { Some(paths) }
    }

    /// Read `public.utf8-plain-text` as Core `text` (UTF-8). `stringForType`
    /// yields an `NSString` (already Unicode), so `to_string()` is valid UTF-8 by
    /// construction — no separate validation needed (the seam invariant holds).
    fn read_text(&self) -> Option<Vec<u8>> {
        let ty = ns_type_for_uti(UTI_TEXT)?;
        let s = self.pb.stringForType(ty)?;
        Some(s.to_string().into_bytes())
    }

    /// Read an image as `image/png`: `public.png` verbatim (lossless), else
    /// `public.tiff` decoded and re-encoded to PNG. `None` if neither is present
    /// or the TIFF will not decode.
    fn read_image(&self) -> Option<Vec<u8>> {
        if let Some(png) = self.read_data(UTI_PNG) {
            return Some(png);
        }
        if let Some(tiff) = self.read_data(UTI_TIFF)
            && let Some(png) = tiff_to_png(&tiff)
        {
            return Some(png);
        }
        None
    }

    /// Read one binary pasteboard type by UTI as raw bytes; `None` if absent or
    /// empty. We read by EXPLICIT UTI only (never enumerate all types), which
    /// sidesteps the `com.apple.pasteboard.promised-suggested-file-name` read-hang.
    fn read_data(&self, uti: &str) -> Option<Vec<u8>> {
        let ty = ns_type_for_uti(uti)?;
        let d = self.pb.dataForType(ty)?;
        if d.is_empty() {
            return None;
        }
        Some(d.to_vec())
    }
}

fn warn(message: &str) {
    eprintln!("[universallink-clipboard] {message}");
}

/// Connects to the general pasteboard and builds the pinned backend plus the
/// `Clone` handle and the upcall channel. Returns `Result` for parity with the
/// other backends' seam; the general pasteboard is always available (no
/// entitlement / TCC permission is needed to read or write it), so this does not
/// fail in practice — a truly unusable pasteboard surfaces later as empty reads
/// and failed writes, handled gracefully.
pub fn create() -> Result<crate::os::Created, String> {
    let cmds: Arc<Mutex<VecDeque<Cmd>>> = Arc::new(Mutex::new(VecDeque::new()));
    let paste = PasteSync::new();
    let (events_tx, backend_events) = mpsc::channel(BACKEND_EVENT_CAPACITY);
    let backend = Backend::new(cmds.clone(), paste.clone(), events_tx)?;
    let handle = MacBackend { cmds, paste };
    Ok(crate::os::Created {
        handle,
        backend_events,
        event_loop: MacLoop { backend },
    })
}

#[cfg(test)]
mod tests {
    //! Pure-logic tests: the format↔UTI mapping, the `PasteSync` token gate, and
    //! the cache generation-gate touch no pasteboard (that is the `#[ignore]`d live
    //! suite in `tests/macos.rs`). The TIFF↔PNG round-trip does exercise AppKit's
    //! `NSBitmapImageRep`, but purely in memory (ImageIO, headless-safe) — no
    //! window server or GUI session. This whole module is `cfg(target_os =
    //! "macos")`, so these run only on a macOS target.
    use super::*;

    /// A valid 2×2 RGBA PNG (built off-Mac, CRCs verified) used as the seed for
    /// the codec round-trip.
    const PNG_2X2: &[u8] = &[
        0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44,
        0x52, 0x00, 0x00, 0x00, 0x02, 0x00, 0x00, 0x00, 0x02, 0x08, 0x06, 0x00, 0x00, 0x00, 0x72,
        0xB6, 0x0D, 0x24, 0x00, 0x00, 0x00, 0x15, 0x49, 0x44, 0x41, 0x54, 0x78, 0xDA, 0x63, 0xF8,
        0xCF, 0xC0, 0xF0, 0x1F, 0x0C, 0x81, 0xF4, 0x7F, 0x2E, 0x11, 0xB9, 0x06, 0x00, 0x40, 0xF2,
        0x06, 0xB7, 0xAA, 0x5F, 0x28, 0x80, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE,
        0x42, 0x60, 0x82,
    ];

    #[test]
    fn format_maps_to_the_right_utis() {
        assert_eq!(utis_for_format(FORMAT_TEXT), &[UTI_TEXT]);
        // PNG advertised BEFORE TIFF, on purpose (lossless first).
        assert_eq!(utis_for_format(FORMAT_PNG), &[UTI_PNG, UTI_TIFF]);
        assert!(utis_for_format("files").is_empty());
        assert!(utis_for_format("bogus").is_empty());
    }

    #[test]
    fn uti_maps_back_to_the_right_format() {
        assert_eq!(format_for_uti(UTI_TEXT), Some(FORMAT_TEXT));
        assert_eq!(format_for_uti(UTI_PNG), Some(FORMAT_PNG));
        // TIFF also maps to image/png (converted at render time).
        assert_eq!(format_for_uti(UTI_TIFF), Some(FORMAT_PNG));
        assert_eq!(format_for_uti("public.html"), None);
        assert_eq!(format_for_uti(""), None);
    }

    #[test]
    fn advertised_utis_round_trip_back_to_their_format() {
        for uti in utis_for_format(FORMAT_TEXT) {
            assert_eq!(format_for_uti(uti), Some(FORMAT_TEXT));
        }
        for uti in utis_for_format(FORMAT_PNG) {
            assert_eq!(format_for_uti(uti), Some(FORMAT_PNG));
        }
    }

    #[test]
    fn paste_sync_gates_on_the_token() {
        // Matching Bytes → the bytes; a far deadline is never reached.
        let far = Instant::now() + Duration::from_secs(30);
        let ps = PasteSync::new();
        ps.resolve(PasteOutcome::Bytes(1, b"hi".to_vec()));
        assert_eq!(ps.wait(1, far), Some(b"hi".to_vec()));

        // Matching Refuse → None.
        ps.reset();
        ps.resolve(PasteOutcome::Refuse(2));
        assert_eq!(ps.wait(2, far), None);

        // Abort → None, whatever the token.
        ps.reset();
        ps.resolve(PasteOutcome::Abort);
        assert_eq!(ps.wait(7, far), None);
    }

    #[test]
    fn paste_sync_drops_a_stale_verdict_then_times_out() {
        // A verdict for a different token is discarded; with nothing else coming,
        // the short deadline elapses → None (never the wrong bytes).
        let ps = PasteSync::new();
        ps.resolve(PasteOutcome::Bytes(99, b"stale".to_vec()));
        let soon = Instant::now() + Duration::from_millis(50);
        assert_eq!(ps.wait(1, soon), None);
    }

    #[test]
    fn paste_sync_times_out_with_no_verdict() {
        let ps = PasteSync::new();
        let soon = Instant::now() + Duration::from_millis(30);
        assert_eq!(ps.wait(1, soon), None);
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
    fn tiff_png_round_trip_via_bitmap_rep() {
        // PNG → TIFF → PNG through NSBitmapImageRep, checking each hop's magic and
        // that the final PNG re-decodes at the original 2×2 size. In-memory only.
        let tiff = png_to_tiff(PNG_2X2).expect("PNG -> TIFF");
        assert!(
            tiff.starts_with(b"II*\0") || tiff.starts_with(b"MM\0*"),
            "TIFF magic (little- or big-endian)"
        );
        let png = tiff_to_png(&tiff).expect("TIFF -> PNG");
        assert!(png.starts_with(b"\x89PNG\r\n\x1a\n"), "PNG magic");
        let data = NSData::with_bytes(&png);
        let rep = NSBitmapImageRep::imageRepWithData(&data).expect("decode round-tripped PNG");
        assert_eq!(rep.pixelsWide(), 2);
        assert_eq!(rep.pixelsHigh(), 2);
    }

    #[test]
    fn undecodable_image_bytes_convert_to_none() {
        // Garbage is not an image: both directions return None (skip on read,
        // refuse on write) — never a panic.
        assert_eq!(png_to_tiff(b"not an image"), None);
        assert_eq!(tiff_to_png(b"not an image"), None);
    }

    // ----- Files: skeleton build + fill-coordination (AppKit-free) -----

    use std::sync::atomic::AtomicUsize;

    fn rfile(file_id: &str, path: &str, size: u64) -> RemoteFile {
        RemoteFile {
            file_id: file_id.into(),
            path: path.into(),
            size,
            dir: false,
        }
    }

    fn rdir(path: &str) -> RemoteFile {
        RemoteFile {
            file_id: String::new(),
            path: path.into(),
            size: 0,
            dir: true,
        }
    }

    /// `build_skeleton` creates the (declared + implied) directory tree and one
    /// EMPTY file per leaf, and returns the leaves (file_id → dest) plus the
    /// ordered, de-duplicated top-level roots. Malformed entries are dropped.
    #[test]
    fn build_skeleton_creates_empty_tree_leaves_and_roots() {
        let base = std::env::temp_dir().join(format!(
            "universallink-clip-skel-test-{}-{:p}",
            std::process::id(),
            &0u8
        ));
        std::fs::create_dir_all(&base).unwrap();
        let files = vec![
            rfile("f0", "dir/a.bin", 10),          // implies dir/
            rfile("f1", "dir/sub/b.bin", 20),      // implies dir/sub/
            rdir("dir/empty"),                     // explicit empty dir
            rfile("f2", "solo.bin", 5),            // top-level file
            rfile("bad-abs", "/etc/passwd", 1),    // dropped (absolute)
            rfile("bad-dotdot", "a/../escape", 1), // dropped (..)
            rfile("bad-empty", "", 1),             // dropped (empty)
        ];
        let (mut leaves, roots) = build_skeleton(&base, &files).unwrap();
        leaves.sort_by(|a, b| a.file_id.cmp(&b.file_id));

        // Roots: first component of each kept entry, ordered, de-duplicated.
        assert_eq!(roots, vec!["dir".to_string(), "solo.bin".to_string()]);

        // Directories exist (declared + implied).
        assert!(base.join("dir").is_dir());
        assert!(base.join("dir/sub").is_dir(), "implied intermediate dir");
        assert!(base.join("dir/empty").is_dir(), "explicit empty dir");

        // Leaves exist, are EMPTY, and map to the right dests.
        assert_eq!(leaves.len(), 3);
        for (fid, rel) in [
            ("f0", "dir/a.bin"),
            ("f1", "dir/sub/b.bin"),
            ("f2", "solo.bin"),
        ] {
            let leaf = leaves.iter().find(|l| l.file_id == fid).unwrap();
            assert_eq!(leaf.dest, base.join(rel));
            assert!(leaf.dest.is_file(), "{rel} must exist");
            assert_eq!(
                std::fs::metadata(&leaf.dest).unwrap().len(),
                0,
                "{rel} empty"
            );
        }

        // None of the malformed entries leaked onto disk or into the roots.
        assert!(!base.join("escape").exists());
        assert!(!roots.iter().any(|r| r == "etc" || r == "a"));

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn build_skeleton_dir_only_manifest_has_no_leaves() {
        let base = std::env::temp_dir().join(format!(
            "universallink-clip-skel-dironly-{}-{:p}",
            std::process::id(),
            &1u8
        ));
        std::fs::create_dir_all(&base).unwrap();
        let (leaves, roots) = build_skeleton(&base, &[rdir("only")]).unwrap();
        assert!(leaves.is_empty(), "a dir-only manifest fills nothing");
        assert_eq!(roots, vec!["only".to_string()]);
        let _ = std::fs::remove_dir_all(&base);
    }

    /// A `FileFetcher` double: records how many times `fill` was called and with
    /// which entries, and returns a scripted result.
    struct RecordingFetcher {
        calls: AtomicUsize,
        seen_entries: Mutex<Vec<(String, PathBuf)>>,
        ok: bool,
    }

    impl RecordingFetcher {
        fn new(ok: bool) -> Arc<Self> {
            Arc::new(Self {
                calls: AtomicUsize::new(0),
                seen_entries: Mutex::new(Vec::new()),
                ok,
            })
        }
    }

    impl FileFetcher for RecordingFetcher {
        fn read(&self, _file_id: &str, _offset: u64, _len: u64) -> std::io::Result<Vec<u8>> {
            unreachable!("macOS uses fill, never read")
        }
        fn fill(&self, entries: &[(String, PathBuf)]) -> std::io::Result<Vec<PathBuf>> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            *self.seen_entries.lock().unwrap() = entries.to_vec();
            if self.ok {
                Ok(entries.iter().map(|(_, p)| p.clone()).collect())
            } else {
                Err(std::io::Error::other("scripted fill failure"))
            }
        }
    }

    fn entries() -> Vec<(String, PathBuf)> {
        vec![
            ("f0".into(), PathBuf::from("/skel/a.bin")),
            ("f1".into(), PathBuf::from("/skel/b.bin")),
        ]
    }

    /// The whole-clip fill runs exactly ONCE across many concurrent relinquishes,
    /// with EVERY leaf entry, and every caller returns (unblocks its reader).
    #[test]
    fn fill_coordinator_fills_the_whole_clip_once_across_threads() {
        let fetcher = RecordingFetcher::new(true);
        let coord = FillCoordinator::new(fetcher.clone(), entries());

        let mut handles = Vec::new();
        for _ in 0..8 {
            let c = coord.clone();
            handles.push(std::thread::spawn(move || c.ensure_filled()));
        }
        for h in handles {
            h.join().unwrap();
        }

        assert_eq!(fetcher.calls.load(Ordering::SeqCst), 1, "fill runs once");
        assert_eq!(
            *fetcher.seen_entries.lock().unwrap(),
            entries(),
            "all leaves"
        );
        assert!(!coord.incomplete(), "a successful fill is not incomplete");
    }

    /// A subsequent relinquish after the fill is Done returns immediately and does
    /// NOT re-run the fill.
    #[test]
    fn fill_coordinator_is_idempotent_after_done() {
        let fetcher = RecordingFetcher::new(true);
        let coord = FillCoordinator::new(fetcher.clone(), entries());
        coord.ensure_filled();
        coord.ensure_filled();
        coord.ensure_filled();
        assert_eq!(fetcher.calls.load(Ordering::SeqCst), 1);
    }

    /// A failed fill still completes (the reader must be released, per the Finder
    /// having no refusal channel) and flags the offer as incomplete.
    #[test]
    fn fill_coordinator_marks_incomplete_on_failure() {
        let fetcher = RecordingFetcher::new(false);
        let coord = FillCoordinator::new(fetcher.clone(), entries());
        coord.ensure_filled(); // returns despite the failure (no hang)
        assert_eq!(fetcher.calls.load(Ordering::SeqCst), 1);
        assert!(coord.incomplete(), "a failed fill is incomplete");
    }
}
