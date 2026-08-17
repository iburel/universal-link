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
//! - Wayland ([`crate::wayland`]): the `InputCapture` portal for capture, the
//!   `RemoteDesktop` portal for injection. Its transport has never been run
//!   against a real compositor, so it is off unless [`WAYLAND_ENV`] switches it
//!   on, and a Wayland session otherwise says precisely which piece is missing
//!   rather than staying silent.
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
        Problem::Wayland => "this is a Wayland session with no native path in this build",
        Problem::XWayland => {
            "serving this Wayland session through its XWayland: X11 windows only, \
             native Wayland windows are out of reach"
        }
        Problem::WaylandNoBus => {
            "this is a Wayland session with no D-Bus session bus, so the input portals \
             cannot be asked for"
        }
        Problem::WaylandNoPortal => {
            "this desktop's portal does not offer the input portals \
             (org.freedesktop.portal.InputCapture, org.freedesktop.portal.RemoteDesktop)"
        }
        Problem::WaylandPortalOld => {
            "this desktop's input portals are older than this build can speak to"
        }
        Problem::WaylandPortalRefused => "the input portal refused this session",
        Problem::WaylandUntested => {
            "this desktop has everything the Wayland path needs, and that path has never \
             been run against a real compositor, so it stays off"
        }
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
    #[cfg(target_os = "linux")]
    Wayland(crate::wayland::WaylandBackend),
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
    #[cfg(target_os = "linux")]
    Wayland(Box<crate::wayland::WaylandLoop>),
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
            #[cfg(target_os = "linux")]
            EventLoop::Wayland(l) => (*l).run(),
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
///
/// # The Linux decision, in one place
///
/// Three sessions and one escape hatch, and the order below is the whole policy:
///
/// - a real X server, so [`crate::x11`] gets it;
/// - a Wayland session (with or without an XWayland alongside), so
///   [`crate::wayland`] gets it, and answers with either the portal backend or the
///   precise reason it cannot be built;
/// - nothing graphical, so [`crate::x11`] is asked anyway and fails for its own
///   reasons: the probe is advice and the backend is the authority;
/// - and [`FORCE_X11_ENV`], which hands a Wayland session to [`crate::x11`]
///   deliberately. That backend then reports [`Problem::XWayland`] for as long as
///   it serves one, so a forced session is honest about what it cannot reach
///   rather than silently claiming everything.
#[cfg(target_os = "linux")]
fn build() -> Result<Created, Unsupported> {
    // FIRST, before anything is probed. The escape hatch is what a person reaches for
    // when the detection is wrong about their machine, so it must not be hostage to
    // the detection: an earlier version asked `session_kind()` first, and since that
    // opens an X connection, a forced session with a black-holed `DISPLAY` hung in the
    // probe whose answer it was about to throw away.
    if forced_x11() {
        eprintln!("[1device-input] forced to X11");
        return crate::x11::create();
    }
    let kind = session_kind();
    eprintln!("[1device-input] session: {kind:?}");
    match kind {
        SessionKind::X11 => crate::x11::create(),
        SessionKind::XWayland | SessionKind::Wayland => crate::wayland::create(kind),
        // No compositor and no X server. Asked anyway rather than refused here,
        // because a probe that could not reach a server is not proof that a
        // connection cannot be made (a `DISPLAY` that came up a moment later, an
        // authority file that arrived): the backend decides, and its failure is
        // the honest `NoBackend`.
        SessionKind::None => crate::x11::create(),
    }
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
/// module would otherwise hand to [`crate::wayland`].
///
/// It exists for two named readers and no others: the live X suite, which runs
/// against an Xvfb or an XWayland on a machine whose own session is Wayland, and a
/// person who knows their session is really X-only and whose environment says
/// otherwise. It is not a fix for a Wayland desktop, and a session forced this way
/// half works by construction, which is exactly why it is not the default and why
/// the backend it builds reports [`Problem::XWayland`] for as long as it serves one.
#[cfg(target_os = "linux")]
pub const FORCE_X11_ENV: &str = "ONEDEVICE_INPUT_FORCE_X11";

/// The environment variable that switches the Wayland portal path ON.
///
/// **Off by default because nothing on that path has ever been executed against a
/// real compositor.** The D-Bus calls, the barrier round trip and the EI stream are
/// written, compiled and unit tested against a scripted portal; no machine
/// available while they were written implemented either portal
/// (`xdg-desktop-portal` 1.18.4 with only the GTK backend exports neither), so the
/// first real run is the live validation ticket's, not a user's.
///
/// A desktop that has everything therefore reports [`Problem::WaylandUntested`] and
/// says so on screen, which is the honest state: a backend claiming `capture` and
/// `inject_keys` on the strength of unexecuted code would be the exact lie this
/// component exists not to tell. Delete this gate in the commit that records a real
/// run, and not before.
#[cfg(target_os = "linux")]
pub const WAYLAND_ENV: &str = "ONEDEVICE_INPUT_WAYLAND";

/// Is a boolean-ish environment variable switched on? Absent, empty and `0` are
/// off, so `FOO=` in a service file does not turn something on by accident.
#[cfg(target_os = "linux")]
pub(crate) fn env_switch(name: &str) -> bool {
    std::env::var_os(name).is_some_and(|v| !v.is_empty() && v != "0")
}

#[cfg(target_os = "linux")]
fn forced_x11() -> bool {
    env_switch(FORCE_X11_ENV)
}

/// Which Linux graphical session this process is in.
///
/// Four states rather than the two the first version had, because the two it had
/// could not express the case this repository is developed on: a Wayland session
/// with an XWayland beside it, where an X11 backend works for some windows and not
/// for most.
#[cfg(target_os = "linux")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionKind {
    /// Nothing graphical: no compositor, and no X server answering.
    None,
    /// A real X server that is nobody's XWayland.
    X11,
    /// A Wayland session, and the X server reachable on `DISPLAY` is its XWayland.
    /// The ordinary shape of GNOME, KDE and WSLg.
    XWayland,
    /// A Wayland session with no X server reachable at all.
    Wayland,
}

/// What the X server on `DISPLAY` turned out to be, asked of the server itself.
#[cfg(target_os = "linux")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum XServer {
    /// No `DISPLAY`, or nothing answered on it.
    Unreachable,
    /// An X server that does not announce the `XWAYLAND` extension.
    Plain,
    /// An X server that does. Only an XWayland does
    /// ([`crate::x11::XWAYLAND_EXTENSION`]).
    XWayland,
}

/// What kind of session this is, from what is really there.
#[cfg(target_os = "linux")]
pub fn session_kind() -> SessionKind {
    session_kind_for(
        wayland_socket_present(),
        std::env::var_os("XDG_SESSION_TYPE").as_deref(),
        local_x_server(),
    )
}

/// The X server on `DISPLAY`, asked about itself, but **only when `DISPLAY` names a
/// local one**.
///
/// # Why the remote case is refused rather than probed
///
/// Two reasons, and either alone would be enough.
///
/// It is correct: a display on another host is not the desk this person is sitting at.
/// A `DISPLAY` left behind by an `ssh -X`, or handed to a container, names somebody
/// else's screen, and capturing a keyboard there or typing into it is not what anybody
/// asked for.
///
/// And it is the only way to bound this call. `xcb::Connection::connect` blocks through
/// the whole handshake with no timeout of its own, so a host that is up and then
/// black-holed costs the kernel's full TCP connect budget, about two minutes, at
/// component start-up: `os::create` would not return, nothing would answer
/// `input.status`, and the supervisor would restart into the same wait for ever. The
/// version of this function that probed unconditionally introduced that on Wayland
/// desktops, which had never connected to an X server at all before.
///
/// The residual exposure is a LOCAL server that accepts and then goes quiet (a
/// suspended virtual machine, a half-dead `Xvfb`). That one is not new: the X11 backend
/// has always connected the same way on every X11 session, and bounding it means a
/// timeout inside `crate::x11`. It is on the deferred list under its own name.
#[cfg(target_os = "linux")]
fn local_x_server() -> XServer {
    let Some(display) = std::env::var_os("DISPLAY") else {
        return XServer::Unreachable;
    };
    // Lossy on purpose: a display name that is not UTF-8 is not one of the three local
    // shapes either way, and the comparison is about the first character.
    let display = display.to_string_lossy();
    let local =
        display.starts_with(':') || display.starts_with("unix:") || display.starts_with('/');
    if !local {
        eprintln!("[1device-input] DISPLAY={display} is not local, so it is not this session's");
        return XServer::Unreachable;
    }
    crate::x11::server_kind()
}

/// Is there a Wayland socket we could actually connect to?
///
/// **The socket, not the variable**, and that distinction is the point of the
/// function. `WAYLAND_DISPLAY` is inherited like any other variable, so a shell
/// started inside a compositor and then used after that compositor died carries a
/// name pointing at nothing; a user service that imported an old environment does
/// the same. Believing the name there would send a live X11 session down the portal
/// path and leave it with no backend at all.
///
/// An absolute name is used as given (the protocol allows one); a relative name is
/// resolved under `XDG_RUNTIME_DIR`, which is where a compositor puts it. No
/// connection is opened: the file being there is enough to decide which family of
/// session this is, and the Wayland half opens its own connection when it needs
/// one.
#[cfg(target_os = "linux")]
pub fn wayland_socket_present() -> bool {
    // `OsStr` throughout: a socket name is not required to be UTF-8, and a session
    // whose variable happened not to be would otherwise read as no session at all.
    socket_present(
        std::env::var_os("WAYLAND_DISPLAY").as_deref(),
        std::env::var_os("XDG_RUNTIME_DIR").as_deref(),
    )
}

/// [`wayland_socket_present`] with the environment handed in, so the resolution rule
/// is testable against a directory a test owns rather than against the session the
/// test happens to run under.
#[cfg(target_os = "linux")]
fn socket_present(name: Option<&std::ffi::OsStr>, runtime_dir: Option<&std::ffi::OsStr>) -> bool {
    let Some(name) = name.filter(|n| !n.is_empty()) else {
        return false;
    };
    let path = std::path::Path::new(name);
    if path.is_absolute() {
        return path.exists();
    }
    match runtime_dir.filter(|d| !d.is_empty()) {
        Some(dir) => std::path::Path::new(dir).join(path).exists(),
        // No runtime directory to resolve a relative name against. The name is
        // still evidence of a compositor (it was set by one), and nothing here can
        // check it, so it is believed: the cost of believing it is the portal path
        // saying precisely what it cannot do, and the cost of disbelieving it is an
        // X11 backend under a Wayland desktop pretending to work.
        None => true,
    }
}

/// The Linux decision, as a pure function of the evidence, so it can be tested
/// without an environment and without a display.
///
/// # Why the X server's own word leads
///
/// Because it is the only piece of evidence here that cannot be inherited, stale or
/// missing. The first version of this function read `WAYLAND_DISPLAY` and
/// `XDG_SESSION_TYPE` and nothing else, and both of those failed on the machine it
/// was written on: `XDG_SESSION_TYPE` is EMPTY there, under a genuine Wayland
/// session, so a detector leaning on it is wrong about the one Wayland session
/// available to test against. An X server that announces the `XWAYLAND` extension
/// is announcing a fact about itself, and there is no environment in which that is
/// a leftover.
///
/// So: the server's word first, then the socket, then the server's word again for a
/// plain one, and logind only when no server answers at all. That last ordering was got
/// wrong first time round and a review caught it: a stale `XDG_SESSION_TYPE=wayland`
/// vetoed a live, plain, fully drivable X server, which is the one case where the
/// server's evidence is strongest.
///
/// # Why `DISPLAY` is never allowed to veto a Wayland session
///
/// The version before that one was `WAYLAND_DISPLAY.is_some() && DISPLAY.is_none()`,
/// which is FALSE on every Wayland desktop there is: GNOME, KDE and WSLg all start
/// an XWayland and set `DISPLAY=:0`. The one sentence a Wayland user needed was
/// therefore never said to anybody. It matters for the X11 backend as much as for
/// the sentence, which is the reason to be firm rather than lenient: an X11 backend
/// under XWayland can neither observe nor inject to native Wayland clients, which on
/// a modern desktop is most of the windows on screen.
///
/// # The one case that looks like a contradiction and is not
///
/// A Wayland socket present AND a plain (non-XWayland) X server on `DISPLAY`. That
/// is a nested server: an `Xvfb :99` or an `Xephyr` started inside a Wayland
/// session, which is exactly how this repository's own live X suite is sometimes
/// run. The nested server is real and fully drivable, and it is not the desk the
/// person is sitting at: driving it would move a pointer inside a window. So the
/// compositor wins and the answer is [`SessionKind::Wayland`], with
/// [`FORCE_X11_ENV`] as the deliberate way to say "no, I mean that X server".
#[cfg(target_os = "linux")]
pub fn session_kind_for(
    wayland_socket: bool,
    session_type: Option<&std::ffi::OsStr>,
    x: XServer,
) -> SessionKind {
    // The server's own word about itself, which nothing can argue with. It is also
    // the answer when the environment has lost every trace of the compositor: an
    // XWayland exists only underneath one.
    if x == XServer::XWayland {
        return SessionKind::XWayland;
    }
    if wayland_socket {
        return SessionKind::Wayland;
    }
    // A plain X server that is really there outranks logind, exactly as an XWayland
    // does above, and for the same reason: `XDG_SESSION_TYPE` is inherited by every
    // child of the login session, so it survives its own session. Somebody logged into
    // Wayland who drops to a tty and runs `startx`, or whose compositor died and who
    // started an Xorg, carries `XDG_SESSION_TYPE=wayland` into a session where X11
    // capture and XTEST both work; believing it there left them with no backend at all.
    // The rule is one sentence: the display server's own word beats what the login
    // manager remembers, and the login manager only speaks when no server answers.
    if x == XServer::Plain {
        return SessionKind::X11;
    }
    let named_wayland = session_type
        .and_then(std::ffi::OsStr::to_str)
        .is_some_and(|kind| kind.eq_ignore_ascii_case("wayland"));
    if named_wayland {
        return SessionKind::Wayland;
    }
    match x {
        XServer::Unreachable => SessionKind::None,
        // Both handled above, and matched rather than left to a catch-all so that
        // adding a fifth kind of server is a compile error here.
        XServer::Plain => SessionKind::X11,
        XServer::XWayland => SessionKind::XWayland,
    }
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
            #[cfg(target_os = "linux")]
            Backend::Wayland(b) => b.capabilities(),
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
            #[cfg(target_os = "linux")]
            Backend::Wayland(b) => b.monitors().await,
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
            #[cfg(target_os = "linux")]
            Backend::Wayland(b) => b.pointer().await,
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
            #[cfg(target_os = "linux")]
            Backend::Wayland(b) => b.resolve(want).await,
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
            #[cfg(target_os = "linux")]
            Backend::Wayland(b) => b.capture(mode),
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
            #[cfg(target_os = "linux")]
            Backend::Wayland(b) => b.confine(rect),
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
            #[cfg(target_os = "linux")]
            Backend::Wayland(b) => b.warp(to),
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
            #[cfg(target_os = "linux")]
            Backend::Wayland(b) => b.inject(actions),
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
            #[cfg(target_os = "linux")]
            Backend::Wayland(b) => b.release_all(keys),
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
            #[cfg(target_os = "linux")]
            Backend::Wayland(b) => b.request_exit(code),
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

    /// The four sessions, from the evidence, and the two traps the earlier versions
    /// of this fell into.
    ///
    /// Trap one: a Wayland session is recognised WITH an X display present, because
    /// every Wayland desktop has one (GNOME, KDE and WSLg all start an XWayland and
    /// set `DISPLAY=:0`), so a condition of the form "Wayland set and DISPLAY unset"
    /// is false on all of them and nobody was ever told anything.
    ///
    /// Trap two: `XDG_SESSION_TYPE` is EMPTY on this repository's own Wayland
    /// session, so it cannot be the thing the answer turns on. The X server's own
    /// word leads, the socket is next, and logind is only a last chance.
    #[cfg(target_os = "linux")]
    #[test]
    fn the_four_linux_sessions_are_told_apart_from_what_is_really_there() {
        use std::ffi::OsStr;
        let os = OsStr::new;

        // The ordinary Wayland desktop, and the case of this development machine:
        // the socket is there, the session type says NOTHING, and the X server on
        // DISPLAY is an XWayland. Every one of the three is what a real GNOME, KDE
        // or WSLg session looks like.
        assert_eq!(
            session_kind_for(true, None, XServer::XWayland),
            SessionKind::XWayland,
            "socket, no session type, XWayland: this repository's own machine"
        );
        assert_eq!(
            session_kind_for(true, Some(os("wayland")), XServer::XWayland),
            SessionKind::XWayland
        );
        // The server's word stands even when the environment has lost every trace of
        // the compositor, and even when logind is wrong: an XWayland exists only
        // underneath a compositor.
        assert_eq!(
            session_kind_for(false, None, XServer::XWayland),
            SessionKind::XWayland,
            "a service that did not import the compositor's environment"
        );
        assert_eq!(
            session_kind_for(false, Some(os("x11")), XServer::XWayland),
            SessionKind::XWayland,
            "the server knows better than logind"
        );

        // A Wayland session with no X server at all: a compositor built without
        // XWayland, or one where it has not started yet.
        assert_eq!(
            session_kind_for(true, None, XServer::Unreachable),
            SessionKind::Wayland
        );
        assert_eq!(
            session_kind_for(false, Some(os("wayland")), XServer::Unreachable),
            SessionKind::Wayland,
            "logind's word is the last chance, and only when no server answers"
        );
        // The one a review caught: logind remembers a Wayland login, the compositor is
        // gone, and a plain X server is right there and fully drivable. Believing
        // logind left that session with no backend at all.
        assert_eq!(
            session_kind_for(false, Some(os("wayland")), XServer::Plain),
            SessionKind::X11,
            "a plain server that is really there outranks what logind remembers"
        );
        assert_eq!(
            session_kind_for(false, Some(os("Wayland")), XServer::Unreachable),
            SessionKind::Wayland,
            "and the comparison does not turn on somebody's capitalisation"
        );
        // The nested server: an Xvfb or an Xephyr started inside a Wayland session.
        // Real, drivable, and not the desk anybody is sitting at, so the compositor
        // wins and FORCE_X11_ENV is the deliberate way to say otherwise.
        assert_eq!(
            session_kind_for(true, None, XServer::Plain),
            SessionKind::Wayland,
            "a nested X server inside a Wayland session is not that session's desktop"
        );

        // A real X11 session.
        assert_eq!(
            session_kind_for(false, Some(os("x11")), XServer::Plain),
            SessionKind::X11
        );
        assert_eq!(
            session_kind_for(false, None, XServer::Plain),
            SessionKind::X11,
            "an X server and no evidence of a compositor is an X11 session, \
             whatever logind failed to say"
        );

        // Nothing graphical. A console login is not a Wayland session.
        assert_eq!(
            session_kind_for(false, None, XServer::Unreachable),
            SessionKind::None
        );
        assert_eq!(
            session_kind_for(false, Some(os("tty")), XServer::Unreachable),
            SessionKind::None
        );
    }

    /// `WAYLAND_DISPLAY` is a name, and a name is not a socket.
    ///
    /// The variable is inherited like any other, so a shell that outlived its
    /// compositor carries one pointing at nothing. Believing it there sends a live
    /// X11 session down the portal path and leaves it with no backend at all, which
    /// is why the check is for the file.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_wayland_name_pointing_at_nothing_is_not_a_wayland_session() {
        use std::ffi::OsStr;
        let os = OsStr::new;

        // The environment is handed in rather than set: this binary's other tests
        // read the real one, and a test that mutated the process environment would
        // race them.
        let dir = tempfile::tempdir().expect("a runtime directory");
        let runtime = dir.path().as_os_str();
        std::fs::write(dir.path().join("wayland-0"), b"").expect("stand in for the socket");

        assert!(
            socket_present(Some(os("wayland-0")), Some(runtime)),
            "a relative name resolves under the runtime directory"
        );
        assert!(
            !socket_present(Some(os("wayland-1")), Some(runtime)),
            "a name the compositor never created is not a session"
        );
        let absolute = dir.path().join("wayland-0");
        assert!(
            socket_present(Some(absolute.as_os_str()), None),
            "an absolute name is used as given, which the protocol allows"
        );
        assert!(!socket_present(Some(os("/nonexistent/wayland-0")), None));

        // No variable, and the empty variable a service file leaves behind.
        assert!(!socket_present(None, Some(runtime)));
        assert!(!socket_present(Some(os("")), Some(runtime)));
        // No runtime directory to resolve against: the name is believed, because the
        // cost of disbelieving it is an X11 backend under a Wayland desktop.
        assert!(socket_present(Some(os("wayland-0")), None));
        assert!(socket_present(Some(os("wayland-0")), Some(os(""))));
    }

    /// **The session this test runs in is told apart correctly, live.**
    ///
    /// The one assertion that could not be made without a real display server, and
    /// the one that would have caught the earlier detectors: `XDG_SESSION_TYPE` is
    /// empty on the machine this was written on, so a detector leaning on it reports
    /// the wrong thing about the only Wayland session available to test against.
    ///
    /// It passes in both places it runs, and says something different in each. On a
    /// bare `Xvfb` (what CI has) there is no compositor and no `XWAYLAND`
    /// extension, so the answer is [`SessionKind::X11`]. On an XWayland (what this
    /// repository's development machine has) the server announces the extension and
    /// the answer is [`SessionKind::XWayland`]. The invariant asserted is the link
    /// between the two halves: **the server's word and the socket's presence must
    /// agree**, because an XWayland exists only underneath a compositor.
    #[cfg(target_os = "linux")]
    #[test]
    fn this_machines_own_session_is_told_apart_correctly() {
        let x = crate::x11::server_kind();
        let socket = wayland_socket_present();
        let kind = session_kind();

        match (x, socket) {
            (XServer::XWayland, _) => assert_eq!(
                kind,
                SessionKind::XWayland,
                "an X server announcing XWAYLAND is an XWayland whatever else is true"
            ),
            (XServer::Plain, false) => assert_eq!(
                kind,
                SessionKind::X11,
                "a plain server that is really there outranks what logind remembers"
            ),
            (XServer::Plain, true) => assert_eq!(
                kind,
                SessionKind::Wayland,
                "a nested X server does not make a Wayland session an X11 one"
            ),
            (XServer::Unreachable, true) => assert_eq!(kind, SessionKind::Wayland),
            (XServer::Unreachable, false) => assert!(
                kind == SessionKind::None || kind == SessionKind::Wayland,
                "with no server and no socket, only logind can still name a session"
            ),
        }

        // The link that makes the XWAYLAND probe worth trusting: an XWayland exists
        // only underneath a compositor. Asserted only when there is a socket to
        // corroborate it, because the interesting third case is real and healthy: an
        // `ssh -X` into a machine whose desktop is Wayland forwards the XWayland's own
        // extension list, and the ssh session has neither the variable nor the socket.
        // That session's `DISPLAY` is not local, so `local_x_server` refuses to probe
        // it and `x` is `Unreachable` here, which is why this arm reads as it does
        // rather than demanding a compositor from every XWayland.
        if x == XServer::XWayland {
            assert!(
                local_x_server() == XServer::XWayland,
                "the probe must be stable across two calls a moment apart"
            );
        }
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
