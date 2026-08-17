// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

//! Live Windows integration tests for the keyboard and mouse backend, exercising the
//! real thing: `SendInput` reaching the OS input stream, the two low level hooks
//! observing and SWALLOWING a keystroke that is not ours, the cursor clip going on
//! and coming off, the relative source the whole pointer design rests on, and the
//! platform half of the crash guard.
//!
//! They need an interactive window station: a headless or session-0 runner has no
//! desktop to hook, and there the backend either refuses to be built (and each test
//! skips) or installs a hook nothing will ever call (and the waits time out). So all
//! of them are `#[ignore]`d, the workspace CI runs without `--ignored`, and they are
//! validated by hand on a real Windows desktop with:
//!
//!     cargo test -p onedevice-input --test windows -- --ignored --test-threads=1
//!
//! `--test-threads=1` is not optional. The cursor clip, the cursor position, the
//! keyboard state and the low level hook chain are all PROCESS-GLOBAL or
//! DESKTOP-global, so two of these running at once would fight over them; they
//! serialize on [`DESKTOP_LOCK`] as well, for the same reason the clipboard's live
//! suite serializes on the clipboard.
//!
//! # What these tests do to the machine they run on
//!
//! They swallow the real keyboard for a few milliseconds, press and release F24
//! (which nothing binds), move the pointer by a few pixels and put it back, and clip
//! the cursor to one monitor and unclip it. Every one of them tears that down on
//! every path, including a failure: a test that left a swallow behind would leave the
//! machine with a dead keyboard, which is exactly the defect the backend exists to
//! avoid, so it would be a poor way to look for it.

#![cfg(target_os = "windows")]

use std::sync::mpsc as std_mpsc;
use std::thread;
use std::time::{Duration, Instant};

use onedevice_input::backend::{
    BackendEvent, CaptureMode, InputBackend, PlatformKey, Point, Rect, Want,
};
use onedevice_input::{keys, os};
use tokio::sync::Mutex;
use windows_sys::Win32::Foundation::{POINT, RECT};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT,
    KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP, KEYEVENTF_SCANCODE, MOUSEEVENTF_MOVE, MOUSEINPUT,
    SendInput,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    ClipCursor, GetClipCursor, GetCursorPos, GetSystemMetrics, SM_CXVIRTUALSCREEN,
    SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN, SetCursorPos,
};

/// The desktop's input state is global to it, so the whole file is one test at a
/// time.
static DESKTOP_LOCK: Mutex<()> = Mutex::const_new(());

/// Every wait is bounded so a failure is a clean failure and never a hung runner
/// holding somebody's keyboard.
const DEADLINE: Duration = Duration::from_secs(5);

/// The key these tests press. F24 is real, is on every layout, and nothing in
/// Windows or in an application binds it, so pressing it on a live desktop changes
/// nothing a person would notice.
const F24_USAGE: u32 = keys::usage(keys::PAGE_KEYBOARD, 0x73);
/// `VK_F24`.
const VK_F24: u16 = 0x87;

/// Spawns the real backend on a dedicated thread (its message-only window, its pump
/// and its hooks are pinned there) and hands back the `Clone` handle, the upcall
/// receiver and the loop's join handle. `None` when this platform could not build
/// one, and the test then skips.
///
/// A macro rather than a function so the handle's type is inferred: it is
/// `os::Backend`, but the point of going through `os::create` is that the test asks
/// for whatever this build selected rather than naming a platform type.
macro_rules! spawn_backend {
    () => {{
        let (ready_tx, ready_rx) = std_mpsc::channel();
        let loop_thread = thread::spawn(move || {
            let created = os::create();
            let event_loop = created.event_loop;
            let _ = ready_tx.send((created.handle, created.backend_events));
            let _code = event_loop.run();
        });
        match ready_rx.recv() {
            Ok((handle, events)) => Some((handle, events, loop_thread)),
            Err(_) => {
                let _ = loop_thread.join();
                None
            }
        }
    }};
}

macro_rules! skip_if_no_desktop {
    ($bound:expr) => {
        match $bound {
            Some(parts) => parts,
            None => {
                eprintln!("skipping: no interactive desktop to hook");
                return;
            }
        }
    };
}

/// A backend that reports it can do nothing is a backend on a machine with no
/// desktop; every test that needs one says so and stops rather than failing.
macro_rules! skip_if_no_capture {
    ($handle:expr, $thread:expr) => {
        if !$handle.capabilities().capture {
            eprintln!("skipping: this build reports it cannot capture here");
            $handle.request_exit(0);
            let _ = $thread.join();
            return;
        }
        if !desktop_reachable() {
            eprintln!(
                "skipping: this session's input desktop is not reachable (a disconnected \
                 remote desktop, or a locked machine). Nothing can be injected or \
                 observed here; see the test module's header."
            );
            $handle.request_exit(0);
            let _ = $thread.join();
            return;
        }
    };
}

/// Is there an input desktop this process can touch?
///
/// `GetCursorPos` is the whole test, and it is a sharper one than it looks. A session
/// nobody is attached to (a DISCONNECTED remote desktop) and a locked one both leave a
/// process able to enumerate every window in the session while denying it the cursor,
/// the foreground window and `SendInput`, all three with `ERROR_ACCESS_DENIED`. So a
/// test that only checked for a window station would run, inject nothing, and fail for
/// a reason that has nothing to do with the code.
fn desktop_reachable() -> bool {
    let mut pt = POINT { x: 0, y: 0 };
    unsafe { GetCursorPos(&mut pt) != 0 }
}

// ------------------------------------------------------------- the test side

/// A key event injected as a FOREIGN application would inject one: no marker in
/// `dwExtraInfo`, so the backend's hooks treat it as a real keystroke rather than as
/// their own echo. That distinction is the whole point of these tests.
fn foreign_key(vk: u16, scancode: u16, extended: bool, down: bool) -> INPUT {
    let mut flags = KEYEVENTF_SCANCODE;
    if !down {
        flags |= KEYEVENTF_KEYUP;
    }
    if extended {
        flags |= KEYEVENTF_EXTENDEDKEY;
    }
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: vk,
                wScan: scancode,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

fn foreign_move(dx: i32, dy: i32) -> INPUT {
    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx,
                dy,
                mouseData: 0,
                dwFlags: MOUSEEVENTF_MOVE,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

fn send(inputs: &[INPUT]) -> u32 {
    unsafe {
        SendInput(
            inputs.len() as u32,
            inputs.as_ptr(),
            std::mem::size_of::<INPUT>() as i32,
        )
    }
}

fn key_is_down(vk: u16) -> bool {
    unsafe { GetAsyncKeyState(vk as i32) as u16 & 0x8000 != 0 }
}

fn cursor() -> Point {
    let mut pt = POINT { x: 0, y: 0 };
    unsafe { GetCursorPos(&mut pt) };
    Point { x: pt.x, y: pt.y }
}

fn clip() -> RECT {
    let mut r = RECT {
        left: 0,
        top: 0,
        right: 0,
        bottom: 0,
    };
    unsafe { GetClipCursor(&mut r) };
    r
}

/// A rectangle a failure message can carry: `windows-sys`'s `RECT` has no `Debug`.
fn show(r: &RECT) -> (i32, i32, i32, i32) {
    (r.left, r.top, r.right, r.bottom)
}

fn virtual_screen() -> RECT {
    unsafe {
        let x = GetSystemMetrics(SM_XVIRTUALSCREEN);
        let y = GetSystemMetrics(SM_YVIRTUALSCREEN);
        RECT {
            left: x,
            top: y,
            right: x + GetSystemMetrics(SM_CXVIRTUALSCREEN),
            bottom: y + GetSystemMetrics(SM_CYVIRTUALSCREEN),
        }
    }
}

/// Waits for the first upcall the predicate accepts, discarding the rest. A real
/// desktop produces motion nobody asked for (a hand on the mouse, an animation
/// moving the pointer), so a test that insisted on the NEXT event would be flaky by
/// construction.
async fn wait_for<F>(
    events: &mut tokio::sync::mpsc::Receiver<BackendEvent>,
    mut accept: F,
) -> Option<BackendEvent>
where
    F: FnMut(&BackendEvent) -> bool,
{
    let deadline = tokio::time::Instant::now() + DEADLINE;
    loop {
        match tokio::time::timeout_at(deadline, events.recv()).await {
            Ok(Some(event)) if accept(&event) => return Some(event),
            Ok(Some(_)) => continue,
            Ok(None) | Err(_) => return None,
        }
    }
}

/// Waits for the first accepted upcall, giving up after `within`.
async fn wait_for_within<F>(
    events: &mut tokio::sync::mpsc::Receiver<BackendEvent>,
    within: Duration,
    mut accept: F,
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

/// Waits for a condition the OS answers, bounded.
fn wait_until(mut done: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + DEADLINE;
    while Instant::now() < deadline {
        if done() {
            return true;
        }
        thread::sleep(Duration::from_millis(5));
    }
    done()
}

// ------------------------------------------------------------------ the tests

/// The whole relative pointer design, proved end to end: with the pointer confined,
/// a mouse move injected as a foreign application would inject one reaches the hook
/// and comes back as a delta, and the pointer does NOT move, because the hook
/// swallowed the move before the system applied it.
///
/// That is truth 1 and truth 2 of the backend's header at once, and if either were
/// wrong the pointer would be pinned and frozen, which is the failure the seam's
/// `confine` contract is worded to prevent.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "real Windows desktop; run manually with --ignored --test-threads=1"]
async fn a_confined_pointer_reports_a_delta_and_does_not_move() {
    let _guard = DESKTOP_LOCK.lock().await;
    let (handle, mut events, loop_thread) = skip_if_no_desktop!(spawn_backend!());
    skip_if_no_capture!(handle, loop_thread);
    let home = cursor();

    let vs = virtual_screen();
    let rect = Rect {
        x: vs.left,
        y: vs.top,
        w: vs.right - vs.left,
        h: vs.bottom - vs.top,
    };
    handle.capture(CaptureMode::Swallow);
    handle.confine(Some(rect));
    // The confine anchors the pointer at the centre of the rectangle, which is the
    // one thing about it a test can see from outside.
    let anchor = Point {
        x: rect.x + rect.w / 2,
        y: rect.y + rect.h / 2,
    };
    assert!(
        wait_until(|| cursor() == anchor),
        "the confine should have put the pointer at the centre of the rectangle"
    );

    assert_eq!(
        send(&[foreign_move(24, 0)]),
        1,
        "the test's own move went out"
    );
    let event = wait_for(&mut events, |e| matches!(e, BackendEvent::Motion(_))).await;
    let Some(BackendEvent::Motion(motion)) = event else {
        panic!("no motion upcall arrived for an injected move");
    };
    assert!(
        motion.dx > 0 && motion.dy == 0,
        "the delta must be the OS's own and point the way the move did: {motion:?}"
    );
    assert_eq!(
        cursor(),
        anchor,
        "and the pointer must not have moved: a swallowed move never reaches the cursor"
    );

    handle.confine(None);
    handle.capture(CaptureMode::Off);
    assert!(
        wait_until(|| {
            let c = clip();
            c.left == vs.left && c.right == vs.right
        }),
        "the clip must be released"
    );
    unsafe { SetCursorPos(home.x, home.y) };
    handle.request_exit(0);
    loop_thread.join().unwrap();
}

/// The swallow, proved on a real keyboard event rather than asserted: the same
/// foreign keystroke is injected twice, once while the backend is only WATCHING and
/// once while it is SWALLOWING, and the operating system's own key state says which
/// of the two reached it.
///
/// This is the test that matters most in the file. A swallow that does not swallow
/// means a source whose every keystroke acts twice; a swallow that cannot be turned
/// off means a machine with a dead keyboard.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "real Windows desktop; run manually with --ignored --test-threads=1"]
async fn watching_lets_a_key_through_and_swallowing_eats_it() {
    let _guard = DESKTOP_LOCK.lock().await;
    let (handle, mut events, loop_thread) = skip_if_no_desktop!(spawn_backend!());
    skip_if_no_capture!(handle, loop_thread);

    let scancode = 0x76; // F24, from the backend's own table.
    assert!(!key_is_down(VK_F24), "F24 must start up");

    // Watching: the hook observes and the key still acts locally.
    handle.capture(CaptureMode::Watch);
    // The hook is installed by the pump, so there is a moment between the downcall
    // and the hook existing. Waiting for the first observed event is what proves it
    // is installed, so the key is sent until one arrives.
    let mut observed = false;
    let deadline = Instant::now() + DEADLINE;
    while Instant::now() < deadline && !observed {
        send(&[foreign_key(VK_F24, scancode, false, true)]);
        send(&[foreign_key(VK_F24, scancode, false, false)]);
        // A SHORT wait per attempt: the hook is installed by the pump, so the first
        // key can go out before it exists, and a test that spent its whole deadline
        // on that first attempt would fail on the one event nobody was listening for.
        observed = wait_for_within(
            &mut events,
            Duration::from_millis(200),
            |e| matches!(e, BackendEvent::Key(k) if k.usage == F24_USAGE),
        )
        .await
        .is_some();
    }
    assert!(observed, "a watching backend must report the key it saw");
    send(&[foreign_key(VK_F24, scancode, false, true)]);
    assert!(
        wait_until(|| key_is_down(VK_F24)),
        "while only watching, the key must still reach the operating system"
    );
    send(&[foreign_key(VK_F24, scancode, false, false)]);
    assert!(wait_until(|| !key_is_down(VK_F24)));

    // Swallowing: the hook consumes it, so the OS never sees it go down.
    handle.capture(CaptureMode::Swallow);
    // Same reasoning as above: the mode reaches the hook through the pump.
    let mut swallowed = false;
    let deadline = Instant::now() + DEADLINE;
    while Instant::now() < deadline && !swallowed {
        send(&[foreign_key(VK_F24, scancode, false, true)]);
        thread::sleep(Duration::from_millis(20));
        swallowed = !key_is_down(VK_F24);
        if !swallowed {
            send(&[foreign_key(VK_F24, scancode, false, false)]);
            thread::sleep(Duration::from_millis(20));
        }
    }
    // Whatever happened, let the key go before asserting: a stuck F24 is harmless
    // and a stuck test is not.
    send(&[foreign_key(VK_F24, scancode, false, false)]);
    assert!(
        swallowed,
        "a swallowing backend must consume the key: the operating system saw it go down"
    );
    assert!(
        wait_for(&mut events, |e| {
            matches!(e, BackendEvent::Key(k) if k.usage == F24_USAGE)
        })
        .await
        .is_some(),
        "and it must still report what it swallowed, or the source would send nothing"
    );

    handle.capture(CaptureMode::Off);
    // The teardown is the other half of the claim: after Off the key gets through
    // again, which is what a machine whose session ended needs.
    assert!(
        wait_until(|| {
            send(&[foreign_key(VK_F24, scancode, false, true)]);
            let down = key_is_down(VK_F24);
            send(&[foreign_key(VK_F24, scancode, false, false)]);
            down
        }),
        "after Off the keyboard must work again"
    );
    handle.request_exit(0);
    loop_thread.join().unwrap();
}

/// Injection, proved by the operating system's own key state: a key this backend
/// sends goes down and comes back up.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "real Windows desktop; run manually with --ignored --test-threads=1"]
async fn an_injected_key_reaches_the_operating_system() {
    let _guard = DESKTOP_LOCK.lock().await;
    let (handle, _events, loop_thread) = skip_if_no_desktop!(spawn_backend!());
    skip_if_no_capture!(handle, loop_thread);

    let resolved = handle
        .resolve(Want::Named("F24".into()))
        .await
        .expect("F24 is on every layout");
    assert!(!key_is_down(VK_F24), "F24 must start up");
    handle.inject(vec![onedevice_input::backend::Action::Key {
        code: resolved.code,
        down: true,
    }]);
    let went_down = wait_until(|| key_is_down(VK_F24));
    // Released through the hygiene path, which is the one that has to work when
    // everything else has gone wrong.
    handle.release_all(vec![resolved.code]);
    assert!(went_down, "an injected key must reach the operating system");
    assert!(
        wait_until(|| !key_is_down(VK_F24)),
        "and the release path must let it go"
    );

    handle.request_exit(0);
    loop_thread.join().unwrap();
}

/// The platform half of the crash guard, proved on the mechanism itself: a cursor
/// clip is the one piece of this backend's state a dead process cannot clean up, so a
/// fresh backend clears whatever its predecessor left before it does anything else.
///
/// The clip is set here the way a dead incarnation would have left it, and building a
/// backend is what has to remove it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "real Windows desktop; run manually with --ignored --test-threads=1"]
async fn a_new_backend_releases_a_clip_its_predecessor_left() {
    let _guard = DESKTOP_LOCK.lock().await;
    let vs = virtual_screen();
    let corner = RECT {
        left: vs.left,
        top: vs.top,
        right: vs.left + 8,
        bottom: vs.top + 8,
    };
    let home = cursor();
    unsafe { ClipCursor(&corner) };
    assert!(
        wait_until(|| {
            let c = clip();
            c.right - c.left <= 8
        }),
        "the clip a dead run would have left must be in place for this test to mean anything"
    );

    let (handle, _events, loop_thread) = skip_if_no_desktop!(spawn_backend!());
    let after = clip();
    assert_eq!(
        (after.left, after.top, after.right, after.bottom),
        (vs.left, vs.top, vs.right, vs.bottom),
        "building a backend must release a clip it did not set"
    );

    unsafe { SetCursorPos(home.x, home.y) };
    handle.request_exit(0);
    loop_thread.join().unwrap();
}

/// The pin goes on and comes off, read back from the operating system rather than
/// from the backend's own bookkeeping. A confinement that outlives its session is a
/// mouse stuck in a corner.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "real Windows desktop; run manually with --ignored --test-threads=1"]
async fn the_pin_goes_on_and_comes_off_and_the_loop_ending_lifts_it() {
    let _guard = DESKTOP_LOCK.lock().await;
    let (handle, _events, loop_thread) = skip_if_no_desktop!(spawn_backend!());
    let home = cursor();
    let monitors = handle.monitors().await;
    assert!(!monitors.is_empty(), "a real desktop has a monitor");
    let first = &monitors[0];
    let rect = Rect {
        x: first.x,
        y: first.y,
        w: first.w,
        h: first.h,
    };

    handle.confine(Some(rect));
    assert!(
        wait_until(|| {
            let c = clip();
            c.left == rect.x && c.right == rect.x + rect.w
        }),
        "the clip must be the rectangle it was given: {:?} against {rect:?}",
        show(&clip())
    );

    // Not released by hand: the loop ending is what has to do it, because that is
    // the path a component being stopped takes.
    handle.request_exit(0);
    loop_thread.join().unwrap();
    let vs = virtual_screen();
    let after = clip();
    assert_eq!(
        (after.left, after.right),
        (vs.left, vs.right),
        "the loop ending must lift the pin"
    );
    unsafe { SetCursorPos(home.x, home.y) };
}

/// What the monitors look like on a real machine: at least one, with a size, with a
/// primary, and with identities that differ from each other.
///
/// The identity is what the shared plane keys a screen by, so two monitors sharing
/// one would make the pointer cross to whichever the map happened to hold.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "real Windows desktop; run manually with --ignored --test-threads=1"]
async fn the_monitors_are_real_and_their_identities_differ() {
    let _guard = DESKTOP_LOCK.lock().await;
    let (handle, _events, loop_thread) = skip_if_no_desktop!(spawn_backend!());
    let monitors = handle.monitors().await;
    assert!(!monitors.is_empty());
    assert_eq!(
        monitors.iter().filter(|m| m.primary).count(),
        1,
        "exactly one monitor is the primary one: {monitors:?}"
    );
    let mut ids: Vec<&str> = monitors.iter().map(|m| m.id.as_str()).collect();
    ids.sort_unstable();
    let before = ids.len();
    ids.dedup();
    assert_eq!(
        before,
        ids.len(),
        "two monitors share an identity: {monitors:?}"
    );
    for m in &monitors {
        assert!(m.w > 0 && m.h > 0, "a monitor of no size: {m:?}");
        assert!(
            m.scale >= 500 && m.scale <= 8000,
            "an implausible scale: {m:?}"
        );
        assert!(!m.name.is_empty());
    }
    // What `monitors_stable` claims has to match what the identities look like: the
    // interface says the screens on this machine may swap places on the strength of
    // that one bit.
    let stable = handle.capabilities().monitors_stable;
    assert_eq!(
        stable,
        monitors.iter().all(|m| m.id.starts_with("win:dev:")),
        "the capability and the identities must agree: {monitors:?}"
    );

    handle.request_exit(0);
    loop_thread.join().unwrap();
}

/// The layout query, on whatever layout the machine really has: a letter resolves,
/// and the position of that letter resolves to the same physical key.
///
/// The second half is the one worth having. The engine holds a `PlatformKey` in its
/// held set and compares it whole to decide whether an incoming release is a key it
/// pressed, so two routes to one key that disagreed would leave that key down.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "real Windows desktop; run manually with --ignored --test-threads=1"]
async fn a_symbol_and_its_position_resolve_to_the_same_key() {
    let _guard = DESKTOP_LOCK.lock().await;
    let (handle, _events, loop_thread) = skip_if_no_desktop!(spawn_backend!());

    // The letter a, and the HID position a US keyboard calls a. On a QWERTY layout
    // they are the same key; on AZERTY they are not, and the test says which it saw
    // rather than insisting.
    let by_symbol = handle.resolve(Want::Symbol("a".into())).await;
    let by_usage = handle
        .resolve(Want::Usage(keys::usage(keys::PAGE_KEYBOARD, 0x04)))
        .await;
    let symbol = by_symbol.expect("a layout that cannot type the letter a is not a layout");
    let usage = by_usage.expect("the position of the letter a exists on every keyboard");
    assert_eq!(symbol.mods, 0, "a lower case a needs no modifier held");
    if symbol.code != usage.code {
        eprintln!(
            "note: this machine's layout puts the letter a somewhere other than the \
             QWERTY position ({symbol:?} against {usage:?}), which is what a non-US \
             layout looks like"
        );
    }

    // Enter, by name, which is what a target with an incomplete usage table falls
    // back to, and it must agree with the position of Enter.
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

    // A capital A needs Shift, and the engine builds the sequence from that.
    let shifted = handle
        .resolve(Want::Symbol("A".into()))
        .await
        .expect("a capital A is producible");
    assert_eq!(
        shifted.mods,
        keys::mods::SHIFT,
        "a capital letter is the shifted level of its key"
    );
    assert_eq!(
        shifted.code, symbol.code,
        "and it is the same key as its lower case"
    );

    handle.request_exit(0);
    loop_thread.join().unwrap();
}

/// The modifier keys the engine learns at every session start, resolved on the real
/// layout, each to a different key. A modifier that resolved to nothing would make
/// every symbol needing it unproducible; two that resolved to the same key would
/// make the engine's held set ambiguous about which one to let go of.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "real Windows desktop; run manually with --ignored --test-threads=1"]
async fn every_modifier_resolves_to_its_own_key_on_the_real_layout() {
    let _guard = DESKTOP_LOCK.lock().await;
    let (handle, _events, loop_thread) = skip_if_no_desktop!(spawn_backend!());

    let mut seen: Vec<PlatformKey> = Vec::new();
    for bit in keys::mods::holdable_bits(keys::mods::HOLDABLE) {
        let usage = keys::mod_usage(bit).expect("a holdable bit has a key");
        let resolved = handle
            .resolve(Want::Usage(usage))
            .await
            .unwrap_or_else(|| panic!("the key for {bit:#x} resolved to nothing"));
        assert!(
            !seen.contains(&resolved.code),
            "two modifiers resolved to the same key: {resolved:?}"
        );
        seen.push(resolved.code);
    }

    handle.request_exit(0);
    loop_thread.join().unwrap();
}

/// The honesty guarantee, proved on a machine that cannot be typed on: when this
/// session's input desktop is out of reach, an injection is REFUSED with a code the
/// interface can turn into a sentence, and nothing is sent.
///
/// This is the mirror of the tests above and it runs in the state they skip in, which
/// is what makes the pair complete: a disconnected remote desktop or a locked machine
/// lets a process enumerate every window in the session while denying it the cursor
/// and `SendInput`, so a backend that trusted the window station would type into the
/// void and say nothing. That silence is the one behaviour this whole feature exists
/// not to have.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "real Windows desktop; run manually with --ignored --test-threads=1"]
async fn an_unreachable_desktop_refuses_the_injection_and_says_why() {
    let _guard = DESKTOP_LOCK.lock().await;
    if desktop_reachable() {
        eprintln!(
            "skipping: this session's input desktop IS reachable, which is the case the \
             other tests cover"
        );
        return;
    }
    let (handle, mut events, loop_thread) = skip_if_no_desktop!(spawn_backend!());

    let resolved = handle
        .resolve(Want::Named("F24".into()))
        .await
        .expect("a layout exists even on a desktop nobody is looking at");
    handle.inject(vec![onedevice_input::backend::Action::Key {
        code: resolved.code,
        down: true,
    }]);
    let refusal = wait_for(&mut events, |e| matches!(e, BackendEvent::Refused(_))).await;
    assert_eq!(
        refusal,
        Some(BackendEvent::Refused(
            onedevice_input::backend::Refusal::ScreenLocked
        )),
        "an injection into a desktop nobody is attached to must be refused, not silent"
    );
    assert!(
        !key_is_down(VK_F24),
        "and nothing must have been typed: the operating system saw the key go down"
    );

    handle.request_exit(0);
    loop_thread.join().unwrap();
}
