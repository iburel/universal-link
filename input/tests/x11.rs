// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

//! Live X11 integration tests for the keyboard and mouse backend, against a real X
//! server: XTEST injection observed through XInput2 raw capture, the device grab
//! actually SWALLOWING (proved against a second X client that is watching the same
//! root window), the pointer pinned and released, the hand written usage table checked
//! against the server's own keymap, and the crash guard's whole claim on this platform
//! (that the server releases a dead client's grab) proved by closing a connection that
//! holds one.
//!
//! They are NOT `#[ignore]`d and they run in CI, which is the difference between this
//! platform and the other two: an X server is something a runner can have. CI starts
//! `Xvfb :99` and exports `DISPLAY`, and this machine's XWayland answers the same
//! requests.
//!
//! **One at a time, and that is enforced by `.config/nextest.toml` rather than by the
//! `X_LOCK` below.** nextest runs every test in its own process, so a process-global
//! `Mutex` serialises nothing; the `serial-os-clipboard` group is what does, and it is
//! declared for both profiles precisely because it was on `ci` alone at first and a suite
//! run by hand failed nine times out of fifteen with two tests fighting over the display's
//! one keyboard grab. The lock stays as the guard for a `cargo test` run, which does share
//! a process.
//!
//! # What an XWayland or an Xvfb can and cannot witness
//!
//! Both are real X servers with XTEST, XInput2, XKB, RandR and XFixes, so everything
//! this file asserts is the real protocol being exercised. Neither is a full desktop:
//!
//! - Under XWayland, an X11 client can neither observe nor inject to NATIVE Wayland
//!   clients, which on a modern desktop is most of the windows on screen. That is why
//!   `os::create` refuses to build this backend for a Wayland session at all, and why
//!   these tests call [`onedevice_input::x11::create`] directly rather than going
//!   through it.
//! - Under Xvfb there is no window manager, no keyboard focus to steal and no other
//!   client competing for a grab, so a grab here always succeeds where a real desktop
//!   can refuse one (`AlreadyGrabbed` when a menu is open, `Frozen` when another client
//!   holds a synchronous grab). The REFUSAL path is therefore compiled and unit tested
//!   and not witnessed here.
//! - Neither has real input devices, so every event in this file is one XTEST put
//!   there. A device's own acceleration profile, a real mouse's report rate and the
//!   feel of an unaccelerated delta are things only a hand can judge.
//!
//! Those three are on the deferred list for the live validation ticket, named as they
//! are here.

#![cfg(target_os = "linux")]

use std::sync::mpsc as std_mpsc;
use std::thread;
use std::time::Duration;

use onedevice_input::backend::{
    Action, BackendEvent, CaptureMode, InputBackend, PlatformKey, Point, Rect, Want,
};
use onedevice_input::{keys, os, x11};
use tokio::sync::Mutex;
use xcb::{x, xinput};

/// The X server's input state is global to it: one grab, one pointer, one keymap. So
/// the whole file is one test at a time.
static X_LOCK: Mutex<()> = Mutex::const_new(());

/// Every wait is bounded, so a failure is a clean failure and never a hung runner
/// holding the display's keyboard.
const DEADLINE: Duration = Duration::from_secs(5);

/// The keys these tests are willing to press, in the order they are tried.
///
/// Chosen at RUNTIME and not fixed, because a keymap need not have every key a HID
/// keyboard can report: an `xkeyboard-config` `pc105` leaves F13 to F24 unbound, so
/// this backend correctly resolves them to nothing and a test that insisted on F24
/// would be testing the keymap rather than the code. Every candidate is a key nothing
/// binds an action to, and none of them is a half duplex lock (a lock reports only its
/// press, which the swallow test needs both halves of).
const CANDIDATES: &[&str] = &["F24", "F23", "F13", "Pause", "F12"];

/// The first candidate this server's keymap actually has, with its usage and keycode.
async fn a_key_nothing_binds(handle: &os::Backend) -> Option<(&'static str, u32, u8)> {
    for name in CANDIDATES {
        let usage = keys::usage_of(name)?;
        if let Some(keycode) = keycode_for(handle, usage).await {
            return Some((name, usage, keycode));
        }
    }
    None
}

/// Spawns the real backend on a dedicated thread (the X connection and its pump are
/// pinned there) and hands back the `Clone` handle, the upcall receiver and the
/// loop's join handle. `None` when there is no reachable X server, and the test skips.
///
/// A macro rather than a function so the handle's type is inferred, and it builds the
/// X11 backend DIRECTLY rather than through `os::create`: the module header says why.
macro_rules! spawn_backend {
    () => {{
        let (ready_tx, ready_rx) = std_mpsc::channel();
        let loop_thread = thread::spawn(move || match x11::create() {
            Ok(created) => {
                let event_loop = created.event_loop;
                let _ = ready_tx.send(Some((created.handle, created.backend_events)));
                let _code = event_loop.run();
            }
            Err(_) => {
                let _ = ready_tx.send(None);
            }
        });
        match ready_rx.recv() {
            Ok(Some((handle, events))) => Some((Session::new(handle, loop_thread), events)),
            _ => {
                let _ = loop_thread.join();
                None
            }
        }
    }};
}

/// A backend and its loop, torn down by `Drop`.
///
/// The whole reason it exists is a FAILING assertion. Every wait in this file is bounded,
/// so a failure is a panic, and a panic unwinds past whatever teardown was written after
/// it: the loop would keep the keyboard grabbed, the pointer pinned, the cursor hidden and
/// (in the Unicode test) a keycode remapped for every client on the display, until the
/// test PROCESS exited. Under one process per file that is a cascade in which every
/// failure after the first is meaningless, and on a developer's own display it is exactly
/// the dead keyboard this component exists to avoid, caused by the test that went looking
/// for it.
struct Session {
    handle: Option<os::Backend>,
    loop_thread: Option<thread::JoinHandle<()>>,
}

impl Session {
    fn new(handle: os::Backend, loop_thread: thread::JoinHandle<()>) -> Session {
        Session {
            handle: Some(handle),
            loop_thread: Some(loop_thread),
        }
    }

    fn handle(&self) -> &os::Backend {
        self.handle.as_ref().expect("the session is still open")
    }

    /// Stops the loop and waits for it, so a test can assert about what the teardown did.
    /// `Drop` does the same for every path that does not reach this.
    fn close(&mut self) {
        if let Some(handle) = &self.handle {
            handle.capture(CaptureMode::Off);
            handle.confine(None);
            handle.request_exit(0);
        }
        if let Some(thread) = self.loop_thread.take() {
            // `join` rather than `join().unwrap()`: a panicked loop thread would otherwise
            // panic HERE and hide the assertion that was the real failure.
            if thread.join().is_err() {
                eprintln!("note: the backend's loop thread panicked");
            }
        }
        self.handle = None;
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.close();
    }
}

macro_rules! skip_if_no_x {
    ($bound:expr) => {
        match $bound {
            Some(parts) => parts,
            None => {
                eprintln!("skipping: no reachable X server (set DISPLAY, or run under Xvfb)");
                return;
            }
        }
    };
}

/// A second X client, which is what makes the swallow provable: it watches the same
/// root window for the same raw events, so "the grab consumed it" and "the injection
/// never happened" are told apart by whether IT saw anything.
struct Peer {
    conn: xcb::Connection,
    root: x::Window,
    window: Option<x::Window>,
}

impl Peer {
    fn new() -> Option<Peer> {
        let (conn, screen) = xcb::Connection::connect_with_extensions(
            None,
            &[xcb::Extension::Input, xcb::Extension::Test],
            &[],
        )
        .ok()?;
        let root = conn.get_setup().roots().nth(screen.max(0) as usize)?.root();
        let cookie = conn.send_request(&xinput::XiQueryVersion {
            major_version: 2,
            minor_version: 2,
        });
        conn.wait_for_reply(cookie).ok()?;
        Some(Peer {
            conn,
            root,
            window: None,
        })
    }

    /// Watches the raw key events, exactly as the backend does.
    fn watch(&self) {
        let masks = [xinput::EventMaskBuf::new(
            xinput::Device::AllMaster,
            &[xinput::XiEventMask::RAW_KEY_PRESS | xinput::XiEventMask::RAW_KEY_RELEASE],
        )];
        self.conn.send_request(&xinput::XiSelectEvents {
            window: self.root,
            masks: &masks,
        });
        let _ = self.conn.flush();
        self.drain();
    }

    /// Fakes a key, as another application would.
    fn fake_key(&self, keycode: u8, down: bool) {
        self.conn.send_request(&xcb::xtest::FakeInput {
            r#type: if down { 2 } else { 3 },
            detail: keycode,
            time: 0,
            root: self.root,
            root_x: 0,
            root_y: 0,
            deviceid: 0,
        });
        let _ = self.conn.flush();
    }

    fn fake_move(&self, dx: i16, dy: i16) {
        self.conn.send_request(&xcb::xtest::FakeInput {
            r#type: 6,
            detail: 1,
            time: 0,
            root: self.root,
            root_x: dx,
            root_y: dy,
            deviceid: 0,
        });
        let _ = self.conn.flush();
    }

    fn pointer(&self) -> Option<Point> {
        let cookie = self
            .conn
            .send_request(&x::QueryPointer { window: self.root });
        let reply = self.conn.wait_for_reply(cookie).ok()?;
        Some(Point {
            x: i32::from(reply.root_x()),
            y: i32::from(reply.root_y()),
        })
    }

    fn keysym(&self, keycode: u8) -> Option<u32> {
        let cookie = self.conn.send_request(&x::GetKeyboardMapping {
            first_keycode: keycode,
            count: 1,
        });
        let reply = self.conn.wait_for_reply(cookie).ok()?;
        reply.keysyms().first().copied().filter(|k| *k != 0)
    }

    /// Creates, maps and focuses a window that selects key events, which is the
    /// witness a grab actually silences.
    ///
    /// A raw watcher is NOT that witness: a grab does not stop raw XInput2 events (see
    /// truth 8 of the backend, measured here). What a grab does is deliver the key to
    /// the grab window instead of to the focused one, so a focused window that stops
    /// receiving keys is the property under test.
    ///
    /// Needs a server where this client can take the focus, which means no window
    /// manager competing for it: an Xvfb, or an X session with nothing else running.
    fn focus_a_window(&mut self) -> bool {
        let setup = self.conn.get_setup();
        let Some(screen) = setup.roots().find(|s| s.root() == self.root) else {
            return false;
        };
        let visual = screen.root_visual();
        let window: x::Window = self.conn.generate_id();
        self.conn.send_request(&x::CreateWindow {
            depth: x::COPY_FROM_PARENT as u8,
            wid: window,
            parent: self.root,
            x: 0,
            y: 0,
            width: 64,
            height: 64,
            border_width: 0,
            class: x::WindowClass::InputOutput,
            visual,
            value_list: &[x::Cw::EventMask(
                x::EventMask::KEY_PRESS | x::EventMask::KEY_RELEASE,
            )],
        });
        self.conn.send_request(&x::MapWindow { window });
        let _ = self.conn.flush();
        // Mapped before the focus is asked for: `SetInputFocus` on a window that is not
        // viewable is refused, and the refusal would arrive later as a protocol error
        // rather than as a failure here.
        let cookie = self.conn.send_request(&x::GetInputFocus {});
        let _ = self.conn.wait_for_reply(cookie);
        self.conn.send_request(&x::SetInputFocus {
            revert_to: x::InputFocus::PointerRoot,
            focus: window,
            time: x::CURRENT_TIME,
        });
        let _ = self.conn.flush();
        let cookie = self.conn.send_request(&x::GetInputFocus {});
        let held = self
            .conn
            .wait_for_reply(cookie)
            .map(|r| r.focus() == window)
            .unwrap_or(false);
        self.window = Some(window);
        self.drain();
        held
    }

    /// Does the focused window receive a key within `within`?
    fn focused_saw_a_key(&self, within: Duration) -> bool {
        let deadline = std::time::Instant::now() + within;
        loop {
            let cookie = self.conn.send_request(&x::GetInputFocus {});
            let _ = self.conn.wait_for_reply(cookie);
            while let Ok(Some(event)) = self.conn.poll_for_event() {
                if matches!(
                    event,
                    xcb::Event::X(x::Event::KeyPress(_)) | xcb::Event::X(x::Event::KeyRelease(_))
                ) {
                    return true;
                }
            }
            if std::time::Instant::now() >= deadline {
                return false;
            }
            thread::sleep(Duration::from_millis(10));
        }
    }

    /// Does this client see a raw key event within `within`?
    fn saw_a_key(&self, within: Duration) -> bool {
        let deadline = std::time::Instant::now() + within;
        loop {
            // A round trip, so anything the server has already sent is in libxcb's
            // queue before it is polled: without it this would race the server rather
            // than the grab.
            let cookie = self.conn.send_request(&x::GetInputFocus {});
            let _ = self.conn.wait_for_reply(cookie);
            while let Ok(Some(event)) = self.conn.poll_for_event() {
                if matches!(
                    event,
                    xcb::Event::Input(xinput::Event::RawKeyPress(_))
                        | xcb::Event::Input(xinput::Event::RawKeyRelease(_))
                ) {
                    return true;
                }
            }
            if std::time::Instant::now() >= deadline {
                return false;
            }
            thread::sleep(Duration::from_millis(10));
        }
    }

    fn drain(&self) {
        let cookie = self.conn.send_request(&x::GetInputFocus {});
        let _ = self.conn.wait_for_reply(cookie);
        while let Ok(Some(_)) = self.conn.poll_for_event() {}
    }
}

/// Waits for the first upcall the predicate accepts, discarding the rest.
async fn wait_for<F>(
    events: &mut tokio::sync::mpsc::Receiver<BackendEvent>,
    mut accept: F,
) -> Option<BackendEvent>
where
    F: FnMut(&BackendEvent) -> bool,
{
    wait_within(events, DEADLINE, &mut accept).await
}

async fn wait_within<F>(
    events: &mut tokio::sync::mpsc::Receiver<BackendEvent>,
    within: Duration,
    accept: &mut F,
) -> Option<BackendEvent>
where
    F: FnMut(&BackendEvent) -> bool,
{
    let deadline = tokio::time::Instant::now() + within;
    loop {
        match tokio::time::timeout_at(deadline, events.recv()).await {
            Ok(Some(event)) if accept(&event) => return Some(event),
            Ok(Some(_)) => continue,
            Ok(None) | Err(_) => return None,
        }
    }
}

/// The keycode of a usage, as the backend resolves it. Every test that presses a key
/// asks the backend rather than computing it, so the table under test is the one the
/// backend uses.
async fn keycode_for(handle: &os::Backend, usage: u32) -> Option<u8> {
    handle
        .resolve(Want::Usage(usage))
        .await
        .map(|r| (r.code.code & 0xFF) as u8)
}

// ------------------------------------------------------------------ the tests

/// The hand written usage table, checked against the SERVER's keymap rather than
/// against itself: every keyboard-page usage resolves to a keycode, and the keys whose
/// keysym is the same on every layout have the keysym they should.
///
/// This is the test that catches a transcription error in the one table a person wrote,
/// and a transcription error there is a remote keystroke that types the wrong key. It
/// is also what makes truth 7 of the backend (the X keycode is the evdev code plus 8)
/// a checked claim rather than a comment.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_usage_table_names_the_keys_the_server_names() {
    let _guard = X_LOCK.lock().await;
    let (mut session, _events) = skip_if_no_x!(spawn_backend!());
    let handle = session.handle().clone();
    let peer = Peer::new().expect("a second connection to the same server");

    // Usage, expected keysym. Only the keys a keymap cannot move: a letter's position
    // differs between QWERTY and AZERTY, so asserting one would be asserting whichever
    // layout the server happens to have.
    let expected: &[(u32, u32, &str)] = &[
        (0x28, 0xFF0D, "Return"),
        (0x29, 0xFF1B, "Escape"),
        (0x2A, 0xFF08, "BackSpace"),
        (0x2B, 0xFF09, "Tab"),
        (0x2C, 0x0020, "space"),
        (0x39, 0xFFE5, "Caps_Lock"),
        (0x3A, 0xFFBE, "F1"),
        (0x45, 0xFFC9, "F12"),
        (0x47, 0xFF14, "Scroll_Lock"),
        (0x4A, 0xFF50, "Home"),
        (0x4C, 0xFFFF, "Delete"),
        (0x4F, 0xFF53, "Right"),
        (0x50, 0xFF51, "Left"),
        (0x51, 0xFF54, "Down"),
        (0x52, 0xFF52, "Up"),
        (0x53, 0xFF7F, "Num_Lock"),
        (0x58, 0xFF8D, "KP_Enter"),
        (0xE0, 0xFFE3, "Control_L"),
        (0xE1, 0xFFE1, "Shift_L"),
        (0xE2, 0xFFE9, "Alt_L"),
        (0xE4, 0xFFE4, "Control_R"),
        (0xE5, 0xFFE2, "Shift_R"),
    ];
    let mut checked = 0usize;
    for (id, want, name) in expected {
        let usage = keys::usage(keys::PAGE_KEYBOARD, *id);
        let Some(keycode) = keycode_for(&handle, usage).await else {
            // A keymap need not have every key a HID keyboard can report, and this
            // backend answers `None` for one it does not have rather than naming a
            // keycode that produces nothing. That is the honest answer, so it is
            // skipped rather than failed; a WRONG table entry does not hide here,
            // because it would name a keycode that exists and whose keysym is another
            // key's.
            eprintln!("note: this keymap has no {name}");
            continue;
        };
        let got = peer.keysym(keycode);
        assert_eq!(
            got,
            Some(*want),
            "usage {id:#04x} resolved to keycode {keycode}, whose first keysym this \
             server calls {got:x?} and which should be {name} ({want:#x})"
        );
        checked += 1;
    }
    assert!(checked > 20, "the table was barely checked");

    session.close();
}

/// Injection and capture, proved against each other in one round trip: a key this
/// backend injects through XTEST comes back through its own XInput2 raw capture, with
/// the usage it was asked for.
///
/// It is the same path #123 measured at 81 microseconds with nothing missed out of
/// three hundred, and it proves both halves of the seam at once: a backend that could
/// inject but not observe, or observe but not inject, fails here.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_key_this_backend_injects_comes_back_through_its_own_capture() {
    let _guard = X_LOCK.lock().await;
    let (mut session, mut events) = skip_if_no_x!(spawn_backend!());
    let handle = session.handle().clone();
    let caps = handle.capabilities();
    assert!(
        caps.capture && caps.inject_keys,
        "this server has neither XTEST nor XInput2: {caps:?}"
    );

    let Some((name, usage, _)) = a_key_nothing_binds(&handle).await else {
        eprintln!("skipping: this keymap has none of the keys this test is willing to press");
        return;
    };
    let resolved = handle
        .resolve(Want::Named(name.into()))
        .await
        .expect("it resolved a moment ago");
    handle.capture(CaptureMode::Watch);

    // The mode reaches the loop through its queue, so the first key can go out before
    // the selection exists. Sent repeatedly with a short wait each time.
    let mut saw: Option<BackendEvent> = None;
    let start = std::time::Instant::now();
    while start.elapsed() < DEADLINE && saw.is_none() {
        handle.inject(vec![
            Action::Key {
                code: resolved.code,
                down: true,
            },
            Action::Key {
                code: resolved.code,
                down: false,
            },
        ]);
        saw = wait_within(
            &mut events,
            Duration::from_millis(200),
            &mut |e| matches!(e, BackendEvent::Key(k) if k.usage == usage),
        )
        .await;
    }
    let Some(BackendEvent::Key(event)) = saw else {
        panic!("nothing came back from an injected key");
    };
    assert_eq!(event.usage, usage);
    assert_eq!(
        event.key.as_deref(),
        Some(name),
        "and it is named, so a target with an incomplete usage table can still press it"
    );
    assert!(
        event.sym.is_none(),
        "{name} produces no character, and claiming one would have the far side type it"
    );

    session.close();
}

/// The swallow, proved against a witness rather than asserted: a second X client whose
/// window has the keyboard focus receives the key while this backend is only WATCHING,
/// and does not receive it while the backend is SWALLOWING.
///
/// That is the whole security property of a source, and getting the witness right took
/// one wrong turn worth recording: a client that has selected the RAW XInput2 events
/// keeps receiving them through another client's grab, measured on this server, so it
/// proves nothing about swallowing. What a grab does is deliver the key to the grab
/// window rather than to the focused one, and this backend selects nothing on its grab
/// window, so the key goes nowhere. A focused window is therefore the witness, and the
/// backend still reporting the key in both modes is what rules out "the injection
/// simply did not happen".
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn watching_lets_a_focused_window_see_the_key_and_swallowing_does_not() {
    let _guard = X_LOCK.lock().await;
    let (mut session, mut events) = skip_if_no_x!(spawn_backend!());
    let handle = session.handle().clone();
    let caps = handle.capabilities();
    if !caps.swallow || !caps.inject_keys {
        eprintln!("skipping: this server cannot both grab and fake input: {caps:?}");
        return;
    }
    let mut peer = Peer::new().expect("a second connection to the same server");
    let Some((_, _, keycode)) = a_key_nothing_binds(&handle).await else {
        eprintln!("skipping: this keymap has none of the keys this test is willing to press");
        return;
    };
    if !peer.focus_a_window() {
        eprintln!(
            "skipping: this client could not take the keyboard focus, so there is no \
             witness a grab could silence (a window manager owns it here)"
        );
        return;
    }

    // Watching: the focused window sees it, and so does the backend.
    //
    // BOTH in one loop, and that is not tidiness: the peer's window can receive a key
    // before this backend's raw selection exists, because the selection is a command the
    // loop has yet to process. A version that waited for the peer first and then for the
    // backend passed nine times out of ten and failed on the tenth, on the one key
    // nobody was listening for yet.
    handle.capture(CaptureMode::Watch);
    let mut seen_by_peer = false;
    let mut seen_by_backend = false;
    let start = std::time::Instant::now();
    while start.elapsed() < DEADLINE && !(seen_by_peer && seen_by_backend) {
        peer.fake_key(keycode, true);
        peer.fake_key(keycode, false);
        seen_by_peer |= peer.focused_saw_a_key(Duration::from_millis(100));
        seen_by_backend |= wait_within(&mut events, Duration::from_millis(100), &mut |e| {
            matches!(e, BackendEvent::Key(_))
        })
        .await
        .is_some();
    }
    assert!(
        seen_by_peer,
        "a focused window must receive a key while this backend only watches"
    );
    assert!(seen_by_backend, "and so must the backend");

    // Swallowing: it does not.
    handle.capture(CaptureMode::Swallow);
    // The GRAB first, proved by the witness going quiet, and only then the observation.
    // This order is the point, and the version that had it the other way round passed for
    // a bad reason: the raw event it waited for could have been generated before the
    // server processed the grab request, so it proved nothing about the grabbed state. It
    // hid a real bug for exactly as long as it existed. A grab taken with
    // `owner_events: false` silences this backend's own capture completely, and only a
    // FRESH event, observed after the witness has stopped hearing anything, can tell that
    // apart from a working one.
    let mut grabbed = false;
    let start = std::time::Instant::now();
    while start.elapsed() < DEADLINE && !grabbed {
        peer.drain();
        peer.fake_key(keycode, true);
        peer.fake_key(keycode, false);
        grabbed = !peer.focused_saw_a_key(Duration::from_millis(200));
    }
    assert!(
        grabbed,
        "the grab must reach the server: the focused window is still receiving keys"
    );
    // Everything the channel already holds is from before the grab, and none of it counts.
    while events.try_recv().is_ok() {}
    let mut ours = None;
    let start = std::time::Instant::now();
    while start.elapsed() < DEADLINE && ours.is_none() {
        peer.drain();
        peer.fake_key(keycode, true);
        peer.fake_key(keycode, false);
        ours = wait_within(&mut events, Duration::from_millis(200), &mut |e| {
            matches!(e, BackendEvent::Key(_))
        })
        .await;
    }
    assert!(
        ours.is_some(),
        "a swallowing backend must still report what it swallowed, or the source \
         would send nothing"
    );
    peer.drain();
    peer.fake_key(keycode, true);
    peer.fake_key(keycode, false);
    let leaked = peer.focused_saw_a_key(Duration::from_millis(500));

    // And the finding that made this test what it is, kept as an assertion so it stops
    // being true loudly rather than quietly: a client watching the RAW events keeps
    // receiving keys straight through another client's grab. That is truth 8 of the
    // backend, it is why the witness above is a focused window, and it is why an X11
    // backend hears its own XTEST injections come back.
    peer.watch();
    peer.fake_key(keycode, true);
    peer.fake_key(keycode, false);
    let raw_still_arrives = peer.saw_a_key(Duration::from_millis(500));

    // Torn down before asserting: a grab left behind is a display with a dead
    // keyboard, and a failing assertion must not be the thing that causes it.
    session.close();
    assert!(
        !leaked,
        "while swallowing, the focused window must not receive the keystroke"
    );
    assert!(
        raw_still_arrives,
        "a grab does not stop the raw events; if this ever changes, the backend's \
         truth 8 and this test's choice of witness both need revisiting"
    );

    // And the teardown works: with the backend gone, the window receives keys again.
    peer.drain();
    let mut back = false;
    let start = std::time::Instant::now();
    while start.elapsed() < DEADLINE && !back {
        peer.fake_key(keycode, true);
        peer.fake_key(keycode, false);
        back = peer.focused_saw_a_key(Duration::from_millis(200));
    }
    assert!(
        back,
        "after the backend stopped, the keyboard must work for everyone again"
    );
}

/// The pointer pinned, the delta still arriving, and both undone.
///
/// This is truth 2 and truth 3 of the backend together: a grab does not stop the
/// pointer moving, so the pin is a warp back to an anchor on every raw motion, and the
/// delta comes from the raw valuators rather than from differencing a position that a
/// pin holds still. A backend that got either wrong produces a pointer that is pinned
/// and frozen, which the seam's `confine` contract is worded to prevent.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_confined_pointer_reports_a_delta_and_comes_back_to_its_anchor() {
    let _guard = X_LOCK.lock().await;
    let (mut session, mut events) = skip_if_no_x!(spawn_backend!());
    let handle = session.handle().clone();
    let caps = handle.capabilities();
    if !caps.confine || !caps.inject_pointer {
        eprintln!("skipping: this server cannot both grab and fake input: {caps:?}");
        return;
    }
    let peer = Peer::new().expect("a second connection to the same server");
    let monitors = handle.monitors().await;
    let rect = match monitors.first() {
        Some(m) => Rect {
            x: m.x,
            y: m.y,
            w: m.w,
            h: m.h,
        },
        // A server with no RandR still has a screen, and the pin is about the pointer
        // rather than about the monitor list.
        None => Rect {
            x: 0,
            y: 0,
            w: 640,
            h: 480,
        },
    };
    let anchor = Point {
        x: rect.x + rect.w / 2,
        y: rect.y + rect.h / 2,
    };

    // Through WATCH and then Swallow, which is the sequence a real session takes (a warm
    // peer puts this machine in Watch and the crossing puts it in Swallow) and which is
    // not the same thing as going straight to Swallow. It is the sequence that found the
    // grab silencing this backend's own capture, because going straight there let a raw
    // event generated before the grab satisfy the assertion below.
    handle.capture(CaptureMode::Watch);
    handle.capture(CaptureMode::Swallow);
    handle.confine(Some(rect));
    // Settled, so the grab and the pin are certainly installed, and then everything the
    // channel holds from before them is thrown away: what this test asserts is that a
    // FRESH device move is observed while grabbed and confined.
    thread::sleep(Duration::from_millis(400));
    while events.try_recv().is_ok() {}
    let mut at_anchor = false;
    let start = std::time::Instant::now();
    while start.elapsed() < DEADLINE && !at_anchor {
        at_anchor = peer.pointer() == Some(anchor);
        if !at_anchor {
            thread::sleep(Duration::from_millis(10));
        }
    }
    assert!(
        at_anchor,
        "the confine must put the pointer at the centre of its rectangle: {:?} against \
         {anchor:?}",
        peer.pointer()
    );

    // A move from another client, which is what a real mouse looks like to the server.
    let mut motion = None;
    let start = std::time::Instant::now();
    while start.elapsed() < DEADLINE && motion.is_none() {
        peer.fake_move(24, 0);
        motion = wait_within(
            &mut events,
            Duration::from_millis(200),
            &mut |e| matches!(e, BackendEvent::Motion(m) if m.dx != 0 || m.dy != 0),
        )
        .await;
    }
    let Some(BackendEvent::Motion(m)) = motion else {
        panic!("no motion upcall arrived for a device move while confined");
    };
    assert!(
        m.dx > 0 && m.dy == 0,
        "the delta must be the device's own and point the way the move did: {m:?}"
    );
    assert!(
        wait_until(DEADLINE, || peer.pointer() == Some(anchor)),
        "and the pointer must come back to the anchor: {:?}",
        peer.pointer()
    );

    // Released, and the pointer is free again.
    handle.confine(None);
    handle.capture(CaptureMode::Off);
    assert!(
        wait_until(DEADLINE, || {
            peer.fake_move(11, 0);
            peer.pointer().is_some_and(|p| p != anchor)
        }),
        "after the release the pointer must move again"
    );

    session.close();
}

/// The crash guard's whole claim on this platform, proved on the mechanism it rests
/// on: the X server releases every grab of a client whose connection closes.
///
/// So a component that dies holding this machine's keyboard cannot leave it swallowed,
/// which is why the X11 backend has no "clear what a predecessor left" step where the
/// Windows one does. The proof is a connection that takes a grab and then drops,
/// followed by a second one that takes the same grab: with the first still holding it,
/// the second is refused.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_server_releases_a_dead_clients_grab() {
    let _guard = X_LOCK.lock().await;
    let Some(first) = Peer::new() else {
        eprintln!("skipping: no reachable X server");
        return;
    };
    let Some(second) = Peer::new() else {
        return;
    };
    assert_eq!(
        grab_keyboard(&first),
        Some(x::GrabStatus::Success),
        "the first client takes the grab"
    );
    assert_eq!(
        grab_keyboard(&second),
        Some(x::GrabStatus::AlreadyGrabbed),
        "and the second is refused while the first holds it, which is what makes the \
         rest of this test mean anything"
    );

    // The connection closing is the whole simulation of a process dying: there is no
    // ungrab, no flush and no teardown.
    drop(first);

    let mut freed = None;
    let start = std::time::Instant::now();
    while start.elapsed() < DEADLINE && freed != Some(x::GrabStatus::Success) {
        freed = grab_keyboard(&second);
        if freed != Some(x::GrabStatus::Success) {
            thread::sleep(Duration::from_millis(10));
        }
    }
    assert_eq!(
        freed,
        Some(x::GrabStatus::Success),
        "a dead client's grab must be released by the server itself"
    );
    second.conn.send_request(&x::UngrabKeyboard {
        time: x::CURRENT_TIME,
    });
    let _ = second.conn.flush();
}

fn grab_keyboard(peer: &Peer) -> Option<x::GrabStatus> {
    let cookie = peer.conn.send_request(&x::GrabKeyboard {
        owner_events: false,
        grab_window: peer.root,
        time: x::CURRENT_TIME,
        pointer_mode: x::GrabMode::Async,
        keyboard_mode: x::GrabMode::Async,
    });
    peer.conn.wait_for_reply(cookie).ok().map(|r| r.status())
}

/// This machine's monitors as RandR reports them, and the two properties the shared
/// plane needs from them: a size, and an identity no other screen shares.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_monitors_are_real_and_their_identities_differ() {
    let _guard = X_LOCK.lock().await;
    let (mut session, _events) = skip_if_no_x!(spawn_backend!());
    let handle = session.handle().clone();
    let monitors = handle.monitors().await;
    if monitors.is_empty() {
        eprintln!("skipping the monitor assertions: this server has no RandR outputs");
        return;
    }
    assert_eq!(
        monitors.iter().filter(|m| m.primary).count(),
        1,
        "exactly one monitor anchors this machine's own arrangement: {monitors:?}"
    );
    let mut ids: Vec<&str> = monitors.iter().map(|m| m.id.as_str()).collect();
    ids.sort_unstable();
    let before = ids.len();
    ids.dedup();
    assert_eq!(
        before,
        ids.len(),
        "two monitors share an identity, so the plane would put them in one place: \
         {monitors:?}"
    );
    for m in &monitors {
        assert!(m.w > 0 && m.h > 0, "a monitor of no size: {m:?}");
        assert_eq!(m.scale, 1000, "X11 reports no per-monitor scale");
        assert!(!m.name.is_empty());
    }
    // What the capability claims and what the identities look like have to agree: the
    // interface tells a person their screens may swap places on the strength of that
    // one bit.
    assert_eq!(
        handle.capabilities().monitors_stable,
        monitors.iter().all(|m| m.id.starts_with("x11:edid:")),
        "the capability and the identities must agree: {monitors:?}"
    );

    session.close();
}

/// The layout queries against the server's real keymap: a letter, its capital, and the
/// position of a key, all resolving consistently.
///
/// The consistency is the part worth having. The engine holds a `PlatformKey` in its
/// held set and compares it WHOLE to decide whether an incoming release is a key it
/// pressed, so two routes to one physical key that disagreed would leave that key
/// down.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_symbol_its_capital_and_a_position_resolve_consistently() {
    let _guard = X_LOCK.lock().await;
    let (mut session, _events) = skip_if_no_x!(spawn_backend!());
    let handle = session.handle().clone();

    let lower = handle
        .resolve(Want::Symbol("a".into()))
        .await
        .expect("a keymap that cannot type the letter a is not a keymap");
    assert_eq!(lower.mods, 0, "a lower case letter needs no modifier held");
    assert_eq!(lower.group, None, "and it is on the active group");
    let upper = handle
        .resolve(Want::Symbol("A".into()))
        .await
        .expect("its capital is producible");
    assert_eq!(
        upper.mods,
        keys::mods::SHIFT,
        "a capital is the shifted level of its key"
    );
    assert_eq!(
        upper.code, lower.code,
        "and it is the same key: {upper:?} against {lower:?}"
    );

    // A name and the position it means are one key.
    let named = handle
        .resolve(Want::Named("Enter".into()))
        .await
        .expect("Enter is on every keyboard");
    let positional = handle
        .resolve(Want::Usage(keys::usage_of("Enter").expect("in the table")))
        .await
        .expect("and so is its position");
    assert_eq!(
        named.code, positional.code,
        "the name and the position of one key must be one key"
    );

    // A character no keymap has: the answer is None, and the engine then falls to the
    // Unicode path rather than typing something else.
    assert!(
        handle
            .resolve(Want::Symbol("\u{1F600}".into()))
            .await
            .is_none(),
        "an emoji is not on a keyboard"
    );
    assert!(
        handle
            .resolve(Want::Named("NotAKey".into()))
            .await
            .is_none(),
        "and a name from a later version resolves to nothing rather than to a wrong key"
    );

    session.close();
}

/// The five modifier keys the engine learns at every session start, each resolving to
/// its own key on the real keymap.
///
/// A modifier that resolved to nothing would make every symbol needing it
/// unproducible; two that resolved to the same key would make the engine's held set
/// ambiguous about which one to let go of.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn every_modifier_resolves_to_its_own_key_on_the_real_keymap() {
    let _guard = X_LOCK.lock().await;
    let (mut session, _events) = skip_if_no_x!(spawn_backend!());
    let handle = session.handle().clone();

    let mut seen: Vec<PlatformKey> = Vec::new();
    for bit in keys::mods::holdable_bits(keys::mods::HOLDABLE) {
        let usage = keys::mod_usage(bit).expect("a holdable bit has a key");
        let resolved = handle
            .resolve(Want::Usage(usage))
            .await
            .unwrap_or_else(|| panic!("the key for {bit:#x} resolved to nothing"));
        assert_eq!(resolved.mods, 0, "a modifier key needs no modifier itself");
        assert!(
            !seen.contains(&resolved.code),
            "two modifiers resolved to the same key: {resolved:?}"
        );
        seen.push(resolved.code);
    }

    session.close();
}

/// The Unicode path, which on X11 is a keycode nothing is bound to, remapped for one
/// stroke and restored.
///
/// Two things are proved: the keymap is EXACTLY what it was afterwards, and the churn
/// in the middle did not look like a layout change. The second matters more than it
/// sounds: the engine releases everything it holds when the layout changes, so a
/// backend whose own Unicode path announced one would drop the modifiers a person was
/// holding in the middle of typing a word.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn typing_a_character_no_key_has_leaves_the_keymap_exactly_as_it_was() {
    let _guard = X_LOCK.lock().await;
    let (mut session, mut events) = skip_if_no_x!(spawn_backend!());
    let handle = session.handle().clone();
    if !handle.capabilities().unicode {
        eprintln!("skipping: this keyboard has no spare keycode for the Unicode path");
        return;
    }
    let peer = Peer::new().expect("a second connection to the same server");

    // The backend announces its layout as soon as it has read the keymap, which is
    // exactly right (the engine's layout identity starts empty and only a
    // `LayoutChanged` fills it, so a backend that stayed quiet would put an empty
    // layout on every key frame). It is also buffered in this channel, so it has to be
    // taken out of the way before this test can say anything about a LATER one.
    assert!(
        wait_for(&mut events, |e| matches!(
            e,
            BackendEvent::LayoutChanged { .. }
        ))
        .await
        .is_some(),
        "the backend announces the layout it read at startup"
    );

    let before = whole_keymap(&peer);
    assert!(!before.is_empty(), "the keymap could be read");

    handle.inject(vec![Action::Text("\u{1F600}\u{4e2d}".into())]);
    // The injection is a queue away, so the keymap is read back until it settles.
    let mut after = Vec::new();
    let start = std::time::Instant::now();
    while start.elapsed() < DEADLINE {
        after = whole_keymap(&peer);
        if after == before {
            break;
        }
        thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(
        after, before,
        "the Unicode path must leave the keymap exactly as it found it"
    );
    assert!(
        wait_within(&mut events, Duration::from_millis(300), &mut |e| matches!(
            e,
            BackendEvent::LayoutChanged { .. }
        ))
        .await
        .is_none(),
        "and its own churn must not look like a layout change"
    );

    session.close();
}

fn whole_keymap(peer: &Peer) -> Vec<u32> {
    let setup = peer.conn.get_setup();
    let first = setup.min_keycode();
    let count = setup.max_keycode().saturating_sub(first).saturating_add(1);
    let cookie = peer.conn.send_request(&x::GetKeyboardMapping {
        first_keycode: first,
        count,
    });
    match peer.conn.wait_for_reply(cookie) {
        Ok(reply) => reply.keysyms().to_vec(),
        Err(_) => Vec::new(),
    }
}

/// The wheel, which X11 delivers as buttons 4 to 7 and which this backend has to
/// translate in both directions.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_wheel_notch_travels_as_a_notch_and_not_as_a_button() {
    let _guard = X_LOCK.lock().await;
    let (mut session, mut events) = skip_if_no_x!(spawn_backend!());
    let handle = session.handle().clone();
    if !handle.capabilities().capture {
        eprintln!("skipping: this server has no XInput2");
        return;
    }
    let peer = Peer::new().expect("a second connection to the same server");
    handle.capture(CaptureMode::Watch);

    let mut wheel = None;
    let start = std::time::Instant::now();
    while start.elapsed() < DEADLINE && wheel.is_none() {
        // Button 4 is a notch up, and the release must NOT double it.
        peer.conn.send_request(&xcb::xtest::FakeInput {
            r#type: 4,
            detail: 4,
            time: 0,
            root: peer.root,
            root_x: 0,
            root_y: 0,
            deviceid: 0,
        });
        peer.conn.send_request(&xcb::xtest::FakeInput {
            r#type: 5,
            detail: 4,
            time: 0,
            root: peer.root,
            root_x: 0,
            root_y: 0,
            deviceid: 0,
        });
        let _ = peer.conn.flush();
        wheel = wait_within(&mut events, Duration::from_millis(200), &mut |e| {
            matches!(e, BackendEvent::Wheel { .. })
        })
        .await;
    }
    assert_eq!(
        wheel,
        Some(BackendEvent::Wheel {
            dx: 0,
            dy: 1,
            pixels: false
        }),
        "a wheel notch up is one notch up"
    );
    assert!(
        wait_within(&mut events, Duration::from_millis(200), &mut |e| matches!(
            e,
            BackendEvent::Button { .. }
        ))
        .await
        .is_none(),
        "and it is never also a button press, or every scroll would click"
    );

    session.close();
}

/// Waits for a condition the server answers, bounded.
fn wait_until(within: Duration, mut done: impl FnMut() -> bool) -> bool {
    let deadline = std::time::Instant::now() + within;
    while std::time::Instant::now() < deadline {
        if done() {
            return true;
        }
        thread::sleep(Duration::from_millis(10));
    }
    done()
}
