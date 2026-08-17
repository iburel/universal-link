// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

//! Per-OS backend construction: which platform half this build selected, and the
//! honest absence of one when the session cannot provide it.
//!
//! - X11 ([`crate::x11`]): XInput2 raw events plus a device grab for swallowing,
//!   XTEST for injection, RandR for the monitors.
//! - Windows ([`crate::windows`]): `WH_KEYBOARD_LL` and `WH_MOUSE_LL` on a message
//!   pump, `SendInput` for injection, `ClipCursor` for the pin.
//! - macOS ([`crate::macos`]): an active `CGEventTap` on a run loop source under
//!   the Input Monitoring grant, `CGEventPost` under the Accessibility grant.
//! - Wayland: a ticket of its own (#128). It says what it cannot do rather than
//!   staying silent, which is the standing decision for Linux.
//!
//! # Why this does not fail, unlike the clipboard's equivalent
//!
//! `clipboard/src/os.rs` reports `Unsupported` and its `main` exits cleanly,
//! because a clipboard component with no backend has nothing at all to do. This
//! component is not in that position, and the difference is worth being explicit
//! about:
//!
//! - The interface has to be able to say "this computer cannot be driven: nothing
//!   here can type", which it can only do if something is answering
//!   `input.status`.
//! - The plane still has to converge, so a person can arrange their screens
//!   before the platform half exists, and so the machine's own screens are on the
//!   plane for its siblings to cross towards.
//! - The grants still have to be storable and honoured, and they are the security
//!   boundary of the whole feature.
//! - And the practical reason: the supervisor restarts a component that dies,
//!   with a backoff capped at a minute and no notion of "unsupported". A
//!   component that exited here would be relaunched every minute for ever on
//!   every install.
//!
//! So [`create`] always succeeds. What varies is what the backend it returns says
//! it can DO: a real platform backend when one could be built, and [`Absent`] with
//! a [`Problem`] naming why when it could not. The engine reads the capabilities
//! and behaves accordingly, which is the same path it takes on a real backend whose
//! OS grant has been refused.
//!
//! # Why an enum rather than a `#[cfg]` type alias
//!
//! `clipboard/src/os.rs` aliases `Created`'s fields to the one platform type per
//! target, because there a construction failure is the end of the process. Here it
//! is not: a machine that claims X11 and has no reachable X server has to fall back
//! to [`Absent`] and keep running, so `create` needs a return type that can hold
//! EITHER the real backend or the absent one. [`InputBackend`] returns
//! `impl Future` from three methods and is therefore not object safe, so a boxed
//! trait object is not available: the enum is what is left, and it costs one match
//! per downcall on a path that is already crossing a thread boundary.
//!
//! # One rule for whoever adds the fourth backend
//!
//! **`request_exit` must go through the OS loop, not through
//! `std::process::exit`.** [`Absent`] exits directly and that is honest for it: it
//! holds no key, has no loop to ask, and its "main-thread loop" is a park. A real
//! backend is in the opposite position on both counts, and it is called from the
//! engine's thread: exiting there would skip every teardown path on a machine that
//! may be holding somebody's modifiers down, which is the one failure this whole
//! component is built to avoid. The method exists on the seam precisely so a real
//! backend can post to its own message pump or run loop and let `main` return
//! normally, releasing what it holds on the way out.

use tokio::sync::mpsc;

use crate::backend::{
    Action, BackendEvent, Capabilities, CaptureMode, InputBackend, Monitor, PlatformKey, Point,
    Problem, Rect, Resolved, Want,
};

/// A platform backend could not be built, and why.
///
/// Not a failure of [`create`] (see the module header): the reason it carries
/// becomes the [`Capabilities::problem`] of an [`Absent`] backend, so it reaches
/// the interface as a sentence instead of ending the process.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Unsupported(pub Problem);

impl std::fmt::Display for Unsupported {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", describe(self.0))
    }
}

/// The prose for a problem, for the log. The interface gets the code and words it
/// itself (doc/input-sharing.md, section 13).
pub fn describe(problem: Problem) -> &'static str {
    match problem {
        Problem::NoBackend => "no keyboard and mouse backend for this platform yet",
        Problem::NoPermission => "the OS has not granted permission to read the input devices",
        Problem::MonitorsUnstable => "the monitors of this session cannot be identified",
        Problem::Wayland => "this is a Wayland session, whose input portals are a later ticket",
    }
}

/// The pieces a built backend hands back: the `Clone` handle the engine drives,
/// the upcall stream it consumes, and the main-thread event loop `main` pumps.
pub struct Created {
    pub handle: Backend,
    pub backend_events: mpsc::Receiver<BackendEvent>,
    pub event_loop: EventLoop,
}

/// The platform backend this build selected, or the honest absence of one.
///
/// One `Clone` handle the engine drives, whichever arm it is. Every arm's own
/// handle is two or three `Arc`s and an integer, so cloning this is cheap by
/// construction, and every downcall costs one match on a path that already crosses
/// a thread boundary.
#[derive(Clone, Debug)]
pub enum Backend {
    #[cfg(target_os = "linux")]
    X11(crate::x11::X11Backend),
    #[cfg(windows)]
    Windows(crate::windows::WindowsBackend),
    #[cfg(target_os = "macos")]
    Mac(crate::macos::MacBackend),
    Absent(Absent),
}

/// The main-thread loop of whichever backend was built. `main` pumps it and its
/// return value is the process exit code.
///
/// The platform arms are BOXED, and not for tidiness: a real loop owns the whole of
/// its platform state (an X connection and a keymap, a window handle and two hook
/// handles) while the absent one owns nothing at all, so an unboxed enum would be as
/// large as the largest of them everywhere it is passed. One allocation, made once per
/// process, buys a handle-sized enum.
pub enum EventLoop {
    #[cfg(target_os = "linux")]
    X11(Box<crate::x11::X11Loop>),
    #[cfg(windows)]
    Windows(Box<crate::windows::WindowsLoop>),
    #[cfg(target_os = "macos")]
    Mac(Box<crate::macos::MacLoop>),
    Absent(AbsentLoop),
}

impl EventLoop {
    pub fn run(self) -> i32 {
        match self {
            #[cfg(target_os = "linux")]
            EventLoop::X11(l) => (*l).run(),
            #[cfg(windows)]
            EventLoop::Windows(l) => (*l).run(),
            #[cfg(target_os = "macos")]
            EventLoop::Mac(l) => (*l).run(),
            EventLoop::Absent(l) => l.run(),
        }
    }
}

/// Builds the platform input backend. Always succeeds; see the module header.
pub fn create() -> Created {
    match build() {
        Ok(created) => created,
        Err(Unsupported(problem)) => {
            eprintln!("[1device-input] {}", describe(problem));
            absent(problem)
        }
    }
}

/// The one [`Absent`] constructor, so the sender that must stay alive is never
/// forgotten (see [`Absent::_events`]).
fn absent(problem: Problem) -> Created {
    let (events, backend_events) = mpsc::channel(1);
    Created {
        handle: Backend::Absent(Absent {
            problem,
            _events: events,
        }),
        backend_events,
        event_loop: EventLoop::Absent(AbsentLoop),
    }
}

/// Tries to build the real thing for this target.
#[cfg(target_os = "linux")]
fn build() -> Result<Created, Unsupported> {
    if let Some(problem) = linux_problem() {
        return Err(Unsupported(problem));
    }
    crate::x11::create()
}

#[cfg(windows)]
fn build() -> Result<Created, Unsupported> {
    crate::windows::create()
}

#[cfg(target_os = "macos")]
fn build() -> Result<Created, Unsupported> {
    crate::macos::create()
}

#[cfg(not(any(target_os = "linux", windows, target_os = "macos")))]
fn build() -> Result<Created, Unsupported> {
    Err(Unsupported(Problem::NoBackend))
}

/// The environment variable that lets the X11 backend serve a session this
/// function would otherwise refuse.
///
/// It exists for two named readers and no others: the live X suite, which runs
/// against an Xvfb or an XWayland on a machine whose own session is Wayland, and a
/// person who knows their session is really X-only and whose environment says
/// otherwise. It is not a fix for a Wayland desktop, and a session forced this way
/// half works by construction (see [`problem_for`]), which is exactly why it is not
/// the default.
#[cfg(target_os = "linux")]
pub const FORCE_X11_ENV: &str = "ONEDEVICE_INPUT_FORCE_X11";

/// Why this Linux session cannot have an X11 backend, or `None` when it can try.
#[cfg(target_os = "linux")]
fn linux_problem() -> Option<Problem> {
    if std::env::var_os(FORCE_X11_ENV).is_some_and(|v| !v.is_empty() && v != "0") {
        return None;
    }
    // Read as `OsStr`: a display name is not required to be UTF-8, and a session
    // whose variable happened not to be would otherwise read as no session at all.
    let wayland = std::env::var_os("WAYLAND_DISPLAY");
    let session_type = std::env::var_os("XDG_SESSION_TYPE");
    let display = std::env::var_os("DISPLAY");
    match problem_for(
        wayland.as_deref(),
        session_type.as_deref(),
        display.as_deref(),
    ) {
        // The one answer that is not a refusal: no evidence of Wayland, so the
        // backend gets to try and to fail for its own reasons.
        Problem::NoBackend => None,
        problem => Some(problem),
    }
}

/// The Linux decision, as a pure function of the three variables, so it can be
/// tested without an environment.
///
/// # Why `DISPLAY` is not allowed to veto, which is what this function is for
///
/// The first version of this was
/// `WAYLAND_DISPLAY.is_some() && DISPLAY.is_none()`, and that condition is FALSE on
/// every Wayland desktop there is. GNOME and KDE both start XWayland and set
/// `DISPLAY=:0`; so does WSLg, which is what this repository is developed on. So
/// the one sentence a Wayland user needs ("this is a Wayland session, whose input
/// portals are a later ticket") was never said to anyone, and they were left
/// waiting for a ticket that had shipped, reading `no_backend` instead.
///
/// It matters for the X11 backend too, and that is the reason to be firm about it
/// rather than lenient: an X11 backend running under XWayland can neither OBSERVE
/// nor INJECT to native Wayland clients, which on a modern desktop is most of the
/// windows on screen. A session that half works is worse than one that says what it
/// cannot do, and [`Problem::Wayland`] is the accurate word for it.
///
/// # Why both variables, and in this order
///
/// `WAYLAND_DISPLAY` leads because it is set by the COMPOSITOR for its own clients,
/// which is the thing we actually care about, and because it is present in cases
/// where the session type is not (a compositor started by hand from a tty leaves
/// `XDG_SESSION_TYPE=tty`). `XDG_SESSION_TYPE` is kept as a second chance because
/// it comes from logind and survives an environment that lost the socket name (a
/// user service that did not import the compositor's environment). Either one alone
/// would miss a real case, so it is the OR of the two: this is the only
/// `WAYLAND_DISPLAY` in the whole repository, so the choice sets the precedent, and
/// the precedent is "believe the strongest evidence of a Wayland session, and never
/// let the presence of an X display argue it away".
#[cfg(target_os = "linux")]
fn problem_for(
    wayland: Option<&std::ffi::OsStr>,
    session_type: Option<&std::ffi::OsStr>,
    // Deliberately unused, and kept in the signature to say so: DISPLAY is
    // XWayland's, not evidence of an X11 session.
    _display: Option<&std::ffi::OsStr>,
) -> Problem {
    let named_wayland = session_type
        .and_then(std::ffi::OsStr::to_str)
        .is_some_and(|kind| kind.eq_ignore_ascii_case("wayland"));
    if wayland.is_some() || named_wayland {
        return Problem::Wayland;
    }
    Problem::NoBackend
}

/// The backend of a machine whose OS half could not be built: it reports what it
/// cannot do and does nothing else.
///
/// Every downcall is a no-op rather than a panic or an `unimplemented!()`, and
/// that is deliberate defence in depth: the engine is written to consult
/// [`Capabilities`] before asking for anything, and a bug there should show up as
/// a session that does nothing rather than as a component that dies holding
/// somebody's keyboard.
#[derive(Clone, Debug)]
pub struct Absent {
    problem: Problem,
    /// The upcall sender, held and never used.
    ///
    /// Not dead weight: the engine's event loop watches the upcall channel, and a
    /// channel whose every sender has been dropped reads as closed at once. A
    /// loop that took that for "the backend died" would exit, the supervisor
    /// would relaunch it, and the component would restart every minute for ever
    /// on exactly the machines this backend exists to serve. Keeping the sender
    /// alive means the channel is simply silent, which is the truth.
    _events: mpsc::Sender<BackendEvent>,
}

impl Absent {
    pub fn problem(&self) -> Problem {
        self.problem
    }
}

impl InputBackend for Absent {
    fn capabilities(&self) -> Capabilities {
        Capabilities::none(Some(self.problem))
    }

    async fn monitors(&self) -> Vec<Monitor> {
        // Not an empty screen: NO screens. A machine that cannot enumerate its
        // monitors publishes nothing about them rather than a plausible fiction,
        // so its siblings place nothing of it on the plane and cross nowhere near
        // it.
        Vec::new()
    }

    async fn pointer(&self) -> Option<Point> {
        None
    }

    async fn resolve(&self, _want: Want) -> Option<Resolved> {
        None
    }

    fn capture(&self, _mode: CaptureMode) {}

    fn confine(&self, _rect: Option<Rect>) {}

    fn warp(&self, _to: Point) {}

    fn inject(&self, _actions: Vec<Action>) {}

    fn release_all(&self, _keys: Vec<PlatformKey>) {}

    fn request_exit(&self, code: i32) {
        // No OS loop to ask, so the process ends here. The platform backends
        // hand this to their main-thread loop instead, which is why the method
        // exists at all.
        std::process::exit(code);
    }
}

impl InputBackend for Backend {
    fn capabilities(&self) -> Capabilities {
        match self {
            #[cfg(target_os = "linux")]
            Backend::X11(b) => b.capabilities(),
            #[cfg(windows)]
            Backend::Windows(b) => b.capabilities(),
            #[cfg(target_os = "macos")]
            Backend::Mac(b) => b.capabilities(),
            Backend::Absent(b) => b.capabilities(),
        }
    }

    async fn monitors(&self) -> Vec<Monitor> {
        match self {
            #[cfg(target_os = "linux")]
            Backend::X11(b) => b.monitors().await,
            #[cfg(windows)]
            Backend::Windows(b) => b.monitors().await,
            #[cfg(target_os = "macos")]
            Backend::Mac(b) => b.monitors().await,
            Backend::Absent(b) => b.monitors().await,
        }
    }

    async fn pointer(&self) -> Option<Point> {
        match self {
            #[cfg(target_os = "linux")]
            Backend::X11(b) => b.pointer().await,
            #[cfg(windows)]
            Backend::Windows(b) => b.pointer().await,
            #[cfg(target_os = "macos")]
            Backend::Mac(b) => b.pointer().await,
            Backend::Absent(b) => b.pointer().await,
        }
    }

    async fn resolve(&self, want: Want) -> Option<Resolved> {
        match self {
            #[cfg(target_os = "linux")]
            Backend::X11(b) => b.resolve(want).await,
            #[cfg(windows)]
            Backend::Windows(b) => b.resolve(want).await,
            #[cfg(target_os = "macos")]
            Backend::Mac(b) => b.resolve(want).await,
            Backend::Absent(b) => b.resolve(want).await,
        }
    }

    fn capture(&self, mode: CaptureMode) {
        match self {
            #[cfg(target_os = "linux")]
            Backend::X11(b) => b.capture(mode),
            #[cfg(windows)]
            Backend::Windows(b) => b.capture(mode),
            #[cfg(target_os = "macos")]
            Backend::Mac(b) => b.capture(mode),
            Backend::Absent(b) => b.capture(mode),
        }
    }

    fn confine(&self, rect: Option<Rect>) {
        match self {
            #[cfg(target_os = "linux")]
            Backend::X11(b) => b.confine(rect),
            #[cfg(windows)]
            Backend::Windows(b) => b.confine(rect),
            #[cfg(target_os = "macos")]
            Backend::Mac(b) => b.confine(rect),
            Backend::Absent(b) => b.confine(rect),
        }
    }

    fn warp(&self, to: Point) {
        match self {
            #[cfg(target_os = "linux")]
            Backend::X11(b) => b.warp(to),
            #[cfg(windows)]
            Backend::Windows(b) => b.warp(to),
            #[cfg(target_os = "macos")]
            Backend::Mac(b) => b.warp(to),
            Backend::Absent(b) => b.warp(to),
        }
    }

    fn inject(&self, actions: Vec<Action>) {
        match self {
            #[cfg(target_os = "linux")]
            Backend::X11(b) => b.inject(actions),
            #[cfg(windows)]
            Backend::Windows(b) => b.inject(actions),
            #[cfg(target_os = "macos")]
            Backend::Mac(b) => b.inject(actions),
            Backend::Absent(b) => b.inject(actions),
        }
    }

    fn release_all(&self, keys: Vec<PlatformKey>) {
        match self {
            #[cfg(target_os = "linux")]
            Backend::X11(b) => b.release_all(keys),
            #[cfg(windows)]
            Backend::Windows(b) => b.release_all(keys),
            #[cfg(target_os = "macos")]
            Backend::Mac(b) => b.release_all(keys),
            Backend::Absent(b) => b.release_all(keys),
        }
    }

    fn request_exit(&self, code: i32) {
        match self {
            #[cfg(target_os = "linux")]
            Backend::X11(b) => b.request_exit(code),
            #[cfg(windows)]
            Backend::Windows(b) => b.request_exit(code),
            #[cfg(target_os = "macos")]
            Backend::Mac(b) => b.request_exit(code),
            Backend::Absent(b) => b.request_exit(code),
        }
    }
}

/// The main-thread loop of a machine with no OS half: there is nothing to pump,
/// so it parks for ever and the process ends through
/// [`InputBackend::request_exit`].
///
/// Parked rather than returned from, on purpose: `main` treats the loop's return
/// as the process's exit, and returning immediately would end the process while
/// the engine on the side thread was still serving the facade.
pub struct AbsentLoop;

impl AbsentLoop {
    pub fn run(self) -> i32 {
        loop {
            std::thread::park();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The backend of a machine with no OS half says so, in a code the interface
    /// can turn into a sentence and in prose a log can carry, and it claims
    /// nothing it cannot do. A component that exited instead would be relaunched
    /// by the supervisor every minute for ever, and could never explain itself.
    #[tokio::test]
    async fn a_machine_with_no_os_half_still_says_what_it_cannot_do() {
        let created = absent(Problem::NoBackend);
        let caps = created.handle.capabilities();
        assert!(!caps.can_drive() && !caps.can_be_driven());
        assert!(!caps.capture && !caps.inject_keys && !caps.inject_pointer && !caps.unicode);
        let problem = caps.problem.expect("a machine with no backend says why");
        assert!(!problem.code().is_empty());
        assert!(!describe(problem).is_empty());
        assert_eq!(problem, Problem::NoBackend);
    }

    /// It publishes NO monitors rather than a plausible one: a fiction on the
    /// plane would have its siblings crossing towards a screen that does not
    /// exist.
    #[tokio::test]
    async fn it_publishes_no_screens_and_resolves_no_keys() {
        let created = absent(Problem::NoBackend);
        assert!(created.handle.monitors().await.is_empty());
        assert!(created.handle.pointer().await.is_none());
        assert!(
            created
                .handle
                .resolve(Want::Symbol("a".into()))
                .await
                .is_none()
        );
    }

    /// A Wayland session is recognised WITH an X display present, because every
    /// Wayland desktop has one: GNOME, KDE and WSLg all start XWayland and set
    /// `DISPLAY=:0`. The condition this replaces (`WAYLAND_DISPLAY` set and
    /// `DISPLAY` unset) was false on all of them, so the one sentence a Wayland
    /// user needs was never said and they waited for a ticket that had shipped.
    ///
    /// It protects the X11 backend as much as the sentence: an X11 backend under
    /// XWayland can neither observe nor inject to native Wayland clients, so
    /// `wayland` is the accurate word rather than a pessimistic one.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_wayland_session_is_named_even_though_xwayland_sets_a_display() {
        use std::ffi::OsStr;
        let os = OsStr::new;

        assert_eq!(
            problem_for(Some(os("wayland-0")), None, Some(os(":0"))),
            Problem::Wayland,
            "GNOME, KDE and WSLg all look exactly like this"
        );
        assert_eq!(
            problem_for(Some(os("wayland-0")), Some(os("wayland")), Some(os(":0"))),
            Problem::Wayland
        );
        assert_eq!(
            problem_for(Some(os("wayland-1")), Some(os("tty")), None),
            Problem::Wayland,
            "a compositor started from a tty leaves the session type saying tty"
        );
        assert_eq!(
            problem_for(None, Some(os("wayland")), Some(os(":0"))),
            Problem::Wayland,
            "logind's word is the second chance, for an environment that lost the socket name"
        );
        assert_eq!(
            problem_for(None, Some(os("Wayland")), None),
            Problem::Wayland,
            "and the comparison does not turn on somebody's capitalisation"
        );

        // A real X11 session, and a headless machine: both are the X11 backend's
        // business rather than the portals ticket's, and it fails for its own
        // reasons when there is no server to reach.
        assert_eq!(
            problem_for(None, Some(os("x11")), Some(os(":0"))),
            Problem::NoBackend
        );
        assert_eq!(problem_for(None, None, Some(os(":0"))), Problem::NoBackend);
        assert_eq!(problem_for(None, None, None), Problem::NoBackend);
        assert_eq!(
            problem_for(None, Some(os("tty")), None),
            Problem::NoBackend,
            "a console login is not a Wayland session"
        );
    }

    /// Every downcall is a no-op rather than a panic: the engine consults the
    /// capabilities first, and a bug there must degrade to a session that does
    /// nothing, never to a component that dies holding somebody's keyboard.
    #[test]
    fn every_downcall_is_harmless() {
        let created = absent(Problem::NoBackend);
        let backend = created.handle;
        backend.capture(CaptureMode::Swallow);
        backend.confine(Some(Rect {
            x: 0,
            y: 0,
            w: 1,
            h: 1,
        }));
        backend.warp(Point { x: 5, y: 5 });
        backend.inject(vec![Action::MoveTo(Point { x: 1, y: 1 })]);
        backend.release_all(vec![PlatformKey {
            code: 16,
            detail: 0,
        }]);
    }
}
