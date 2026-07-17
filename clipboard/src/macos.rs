// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

//! The macOS clipboard backend: an owner-driven, delayed-render state machine
//! that plugs into the frozen [`crate::backend`] seam. Built on AppKit's
//! `NSPasteboard` through the typed `objc2` bindings. Scope of this brick:
//! `text` and `image/png` only (files land in a later brick).
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

use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use objc2::rc::{Retained, autoreleasepool};
use objc2::runtime::{AnyObject, NSObject, NSObjectProtocol};
use objc2::{AnyThread, DefinedClass, define_class, msg_send};
use objc2_app_kit::{
    NSBitmapImageFileType, NSBitmapImageRep, NSBitmapImageRepPropertyKey, NSPasteboard,
    NSPasteboardType, NSPasteboardTypeOwner, NSPasteboardTypePNG, NSPasteboardTypeString,
    NSPasteboardTypeTIFF,
};
use objc2_foundation::{
    NSArray, NSData, NSDate, NSDefaultRunLoopMode, NSDictionary, NSInteger, NSRunLoop, NSString,
};
use tokio::sync::{mpsc, oneshot};

use crate::backend::{BackendEvent, ClipboardBackend, Format, LocalClip, RemoteClip};

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
        let types: Vec<&NSPasteboardType> = clip
            .formats
            .iter()
            .flat_map(|f| utis_for_format(&f.id))
            .filter_map(|uti| ns_type_for_uti(uti))
            .collect();
        if types.is_empty() {
            // Nothing we can promise: relinquish rather than hold a stale offer.
            self.on_release();
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

    /// Relinquish ownership if we still hold it (drop the delayed-render promise).
    /// Shared by `Cmd::Release`, the shutdown path, and `Drop`.
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
    fn on_release(&mut self) {
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
}
