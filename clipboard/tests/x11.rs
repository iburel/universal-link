// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

//! Live-X integration tests for the X11 backend, exercising the real ICCCM
//! selection protocol against a second in-test X client. They need a reachable
//! X server (run under `xvfb-run` in CI and locally); with none, `os::create`
//! reports `Unsupported` and each test SKIPS rather than fails.
//!
//! The X `CLIPBOARD` selection is a per-display global, so the tests serialize
//! on [`SELECTION_LOCK`]: only one may hold the selection at a time even under
//! `--test-threads=2`.
//!
//! Two directions are covered:
//! - destination side: the backend `offer`s a remote clip (takes ownership); a
//!   requestor pastes it; the backend emits `Paste`; `deliver` completes the
//!   render and the requestor reads the bytes (and the SelectionNotify echoes
//!   the REQUESTED target, per ICCCM);
//! - source side: a foreign owner sets the selection; the backend detects it
//!   (XFixes), eager-reads it, emits `Copied`, and `provide` serves the cached
//!   bytes (only for the live generation).

#![cfg(target_os = "linux")]

use std::sync::mpsc as std_mpsc;
use std::thread;
use std::time::{Duration, Instant};

use tokio::sync::Mutex;
use tokio::sync::mpsc;
use universallink_clipboard::os;
use universallink_clipboard::{BackendEvent, ClipboardBackend, Format, RemoteClip};
use xcb::{Xid, x};

/// Serializes access to the per-display global CLIPBOARD selection.
static SELECTION_LOCK: Mutex<()> = Mutex::const_new(());

/// Every X round-trip / upcall wait is bounded so a failure is a clean test
/// failure, never a hang.
const DEADLINE: Duration = Duration::from_secs(5);

/// Spawns the real backend on a dedicated thread (its non-`Send` connection is
/// pinned there, running the blocking pump), and hands back the `Clone` handle,
/// the upcall receiver, and the loop's join handle. Evaluates to `None` when no
/// X server is reachable (the test then skips). Kept as a macro so the handle's
/// (unnameable, module-private) type is preserved by inference.
macro_rules! spawn_backend {
    () => {{
        let (ready_tx, ready_rx) = std_mpsc::channel();
        let loop_thread = thread::spawn(move || match os::create() {
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
            Ok(Some((handle, events))) => Some((handle, events, loop_thread)),
            _ => {
                let _ = loop_thread.join();
                None
            }
        }
    }};
}

/// The reply a requestor observed for one paste: the target the owner echoed in
/// its `SelectionNotify`, the property it named, and the bytes read from it
/// (`None` when the owner refused with property `NONE`).
struct PasteReply {
    echoed_target: x::Atom,
    property: x::Atom,
    bytes: Option<Vec<u8>>,
}

fn intern(conn: &xcb::Connection, name: &[u8]) -> x::Atom {
    let cookie = conn.send_request(&x::InternAtom {
        only_if_exists: false,
        name,
    });
    conn.wait_for_reply(cookie).expect("InternAtom").atom()
}

/// Creates an unmapped 1×1 window on the connection's screen (the standard
/// invisible client window for selection work).
fn create_window(conn: &xcb::Connection, screen_num: i32) -> x::Window {
    let window: x::Window = conn.generate_id();
    let (root, visual) = {
        let setup = conn.get_setup();
        let screen = setup.roots().nth(screen_num as usize).expect("screen");
        (screen.root(), screen.root_visual())
    };
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
        value_list: &[x::Cw::EventMask(x::EventMask::PROPERTY_CHANGE)],
    });
    conn.flush().expect("flush CreateWindow");
    window
}

fn send_notify(
    conn: &xcb::Connection,
    requestor: x::Window,
    selection: x::Atom,
    target: x::Atom,
    property: x::Atom,
    time: x::Timestamp,
) {
    conn.send_request(&x::SendEvent {
        propagate: false,
        destination: x::SendEventDest::Window(requestor),
        event_mask: x::EventMask::empty(),
        event: &x::SelectionNotifyEvent::new(time, requestor, selection, target, property),
    });
    let _ = conn.flush();
}

/// A second X client acting as a paste requestor.
struct Requestor {
    conn: xcb::Connection,
    window: x::Window,
    clipboard: x::Atom,
    utf8: x::Atom,
    string: x::Atom,
    prop: x::Atom,
}

impl Requestor {
    fn new() -> Requestor {
        let (conn, screen_num) = xcb::Connection::connect(None).expect("connect requestor");
        let window = create_window(&conn, screen_num);
        let clipboard = intern(&conn, b"CLIPBOARD");
        let utf8 = intern(&conn, b"UTF8_STRING");
        let string = intern(&conn, b"STRING");
        let prop = intern(&conn, b"UNIVERSALLINK_TEST_REPLY");
        Requestor {
            conn,
            window,
            clipboard,
            utf8,
            string,
            prop,
        }
    }

    /// Blocks (up to [`DEADLINE`]) until the CLIPBOARD selection has an owner —
    /// the backend has processed our `offer`.
    fn wait_until_owned(&self) -> bool {
        let deadline = Instant::now() + DEADLINE;
        loop {
            let cookie = self.conn.send_request(&x::GetSelectionOwner {
                selection: self.clipboard,
            });
            if let Ok(reply) = self.conn.wait_for_reply(cookie)
                && reply.owner() != x::Window::none()
            {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            thread::sleep(Duration::from_millis(5));
        }
    }

    /// Requests `target` from CLIPBOARD and returns what came back once the
    /// owner answers (the deferred render completes on `deliver`/`paste_failed`).
    fn paste(&self, target: x::Atom) -> PasteReply {
        self.conn.send_request(&x::DeleteProperty {
            window: self.window,
            property: self.prop,
        });
        self.conn.send_request(&x::ConvertSelection {
            requestor: self.window,
            selection: self.clipboard,
            target,
            property: self.prop,
            time: x::CURRENT_TIME,
        });
        self.conn.flush().expect("flush ConvertSelection");

        let Some((echoed_target, property)) = self.wait_notify() else {
            panic!("no SelectionNotify within the deadline (target {target:?})");
        };
        if property == x::ATOM_NONE {
            return PasteReply {
                echoed_target,
                property,
                bytes: None,
            };
        }
        let cookie = self.conn.send_request(&x::GetProperty {
            delete: true,
            window: self.window,
            property,
            r#type: x::ATOM_ANY,
            long_offset: 0,
            long_length: 0x0100_0000,
        });
        let reply = self.conn.wait_for_reply(cookie).expect("GetProperty");
        PasteReply {
            echoed_target,
            property,
            bytes: Some(reply.value::<u8>().to_vec()),
        }
    }

    fn wait_notify(&self) -> Option<(x::Atom, x::Atom)> {
        let deadline = Instant::now() + DEADLINE;
        loop {
            while let Ok(Some(event)) = self.conn.poll_for_event() {
                if let xcb::Event::X(x::Event::SelectionNotify(e)) = event
                    && e.selection() == self.clipboard
                {
                    return Some((e.target(), e.property()));
                }
            }
            if Instant::now() >= deadline {
                return None;
            }
            thread::sleep(Duration::from_millis(5));
        }
    }
}

/// Spawns a selection owner on its own thread and connection: it takes CLIPBOARD
/// ownership and serves `TARGETS` + `UTF8_STRING` (the text `body`) until asked
/// to stop, then drops ownership. Returns a stop signal and the join handle.
fn spawn_owner(body: &'static [u8]) -> (std_mpsc::Sender<()>, thread::JoinHandle<()>) {
    let (stop_tx, stop_rx) = std_mpsc::channel::<()>();
    let handle = thread::spawn(move || {
        let (conn, screen_num) = xcb::Connection::connect(None).expect("connect owner");
        let window = create_window(&conn, screen_num);
        let clipboard = intern(&conn, b"CLIPBOARD");
        let targets = intern(&conn, b"TARGETS");
        let utf8 = intern(&conn, b"UTF8_STRING");

        conn.send_request(&x::SetSelectionOwner {
            owner: window,
            selection: clipboard,
            time: x::CURRENT_TIME,
        });
        conn.flush().expect("owner SetSelectionOwner");

        loop {
            while let Ok(Some(event)) = conn.poll_for_event() {
                if let xcb::Event::X(x::Event::SelectionRequest(e)) = event {
                    let property = if e.property() == x::ATOM_NONE {
                        e.target()
                    } else {
                        e.property()
                    };
                    if e.target() == targets {
                        conn.send_request(&x::ChangeProperty {
                            mode: x::PropMode::Replace,
                            window: e.requestor(),
                            property,
                            r#type: x::ATOM_ATOM,
                            data: &[targets, utf8],
                        });
                        send_notify(
                            &conn,
                            e.requestor(),
                            clipboard,
                            e.target(),
                            property,
                            e.time(),
                        );
                    } else if e.target() == utf8 {
                        conn.send_request(&x::ChangeProperty {
                            mode: x::PropMode::Replace,
                            window: e.requestor(),
                            property,
                            r#type: utf8,
                            data: body,
                        });
                        send_notify(
                            &conn,
                            e.requestor(),
                            clipboard,
                            e.target(),
                            property,
                            e.time(),
                        );
                    } else {
                        send_notify(
                            &conn,
                            e.requestor(),
                            clipboard,
                            e.target(),
                            x::ATOM_NONE,
                            e.time(),
                        );
                    }
                }
            }
            if stop_rx.try_recv().is_ok() {
                break;
            }
            thread::sleep(Duration::from_millis(5));
        }

        conn.send_request(&x::SetSelectionOwner {
            owner: x::Window::none(),
            selection: clipboard,
            time: x::CURRENT_TIME,
        });
        let _ = conn.flush();
    });
    (stop_tx, handle)
}

/// Waits (bounded) for the next `Paste` upcall, ignoring any other event.
async fn recv_paste(events: &mut mpsc::Receiver<BackendEvent>) -> Option<(String, u64)> {
    let deadline = tokio::time::Instant::now() + DEADLINE;
    loop {
        match tokio::time::timeout_at(deadline, events.recv()).await {
            Ok(Some(BackendEvent::Paste { format, token })) => return Some((format, token)),
            Ok(Some(_)) => continue,
            _ => return None,
        }
    }
}

/// Waits (bounded) for the next `Copied` upcall, ignoring any other event.
async fn recv_copied(
    events: &mut mpsc::Receiver<BackendEvent>,
) -> Option<(u64, Vec<Format>, bool)> {
    let deadline = tokio::time::Instant::now() + DEADLINE;
    loop {
        match tokio::time::timeout_at(deadline, events.recv()).await {
            Ok(Some(BackendEvent::Copied { generation, clip })) => {
                return Some((generation, clip.formats, clip.sensitive));
            }
            Ok(Some(_)) => continue,
            _ => return None,
        }
    }
}

/// Waits (bounded) for the next `Cleared` upcall, ignoring any other event.
async fn recv_cleared(events: &mut mpsc::Receiver<BackendEvent>) -> bool {
    let deadline = tokio::time::Instant::now() + DEADLINE;
    loop {
        match tokio::time::timeout_at(deadline, events.recv()).await {
            Ok(Some(BackendEvent::Cleared)) => return true,
            Ok(Some(_)) => continue,
            _ => return false,
        }
    }
}

/// Spawns an owner that takes CLIPBOARD but advertises only an unsupported
/// target (and refuses every content conversion) — a copy the backend cannot
/// sync (e.g. a file manager's `x-special/*`). Stealing ownership this way,
/// with no intervening owner→None, is the path that must supersede a prior
/// capture rather than keep vouching for it.
fn spawn_unsupported_owner() -> (std_mpsc::Sender<()>, thread::JoinHandle<()>) {
    let (stop_tx, stop_rx) = std_mpsc::channel::<()>();
    let handle = thread::spawn(move || {
        let (conn, screen_num) = xcb::Connection::connect(None).expect("connect owner2");
        let window = create_window(&conn, screen_num);
        let clipboard = intern(&conn, b"CLIPBOARD");
        let targets = intern(&conn, b"TARGETS");
        let unsupported = intern(&conn, b"application/x-universallink-unsupported");

        conn.send_request(&x::SetSelectionOwner {
            owner: window,
            selection: clipboard,
            time: x::CURRENT_TIME,
        });
        conn.flush().expect("owner2 SetSelectionOwner");

        loop {
            while let Ok(Some(event)) = conn.poll_for_event() {
                if let xcb::Event::X(x::Event::SelectionRequest(e)) = event {
                    let property = if e.property() == x::ATOM_NONE {
                        e.target()
                    } else {
                        e.property()
                    };
                    if e.target() == targets {
                        conn.send_request(&x::ChangeProperty {
                            mode: x::PropMode::Replace,
                            window: e.requestor(),
                            property,
                            r#type: x::ATOM_ATOM,
                            data: &[unsupported],
                        });
                        send_notify(
                            &conn,
                            e.requestor(),
                            clipboard,
                            e.target(),
                            property,
                            e.time(),
                        );
                    } else {
                        send_notify(
                            &conn,
                            e.requestor(),
                            clipboard,
                            e.target(),
                            x::ATOM_NONE,
                            e.time(),
                        );
                    }
                }
            }
            if stop_rx.try_recv().is_ok() {
                break;
            }
            thread::sleep(Duration::from_millis(5));
        }

        conn.send_request(&x::SetSelectionOwner {
            owner: x::Window::none(),
            selection: clipboard,
            time: x::CURRENT_TIME,
        });
        let _ = conn.flush();
    });
    (stop_tx, handle)
}

fn text_offer() -> RemoteClip {
    RemoteClip {
        tx_id: "tx-test".into(),
        formats: vec![Format {
            id: "text".into(),
            size: None,
        }],
        files: Vec::new(),
        sensitive: false,
    }
}

macro_rules! skip_if_no_x {
    ($bound:expr) => {
        match $bound {
            Some(parts) => parts,
            None => {
                eprintln!("skipping: no reachable X server (run under xvfb-run / set DISPLAY)");
                return;
            }
        }
    };
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_remote_offer_is_pasteable_as_utf8() {
    let _guard = SELECTION_LOCK.lock().await;
    let (handle, mut events, loop_thread) = skip_if_no_x!(spawn_backend!());

    handle.offer(text_offer());

    // paste() blocks until we deliver, so it drives its own thread.
    let paster = thread::spawn(|| {
        let requestor = Requestor::new();
        assert!(requestor.wait_until_owned(), "backend never took ownership");
        let utf8 = requestor.utf8;
        (requestor.paste(utf8), utf8)
    });

    let (format, token) = recv_paste(&mut events).await.expect("Paste upcall");
    assert_eq!(format, "text");
    handle.deliver(token, "text", b"hello over X".to_vec());

    let (reply, utf8) = paster.join().unwrap();
    assert_eq!(reply.bytes.as_deref(), Some(b"hello over X".as_ref()));
    assert_eq!(
        reply.echoed_target, utf8,
        "the SelectionNotify must echo the requested target"
    );

    handle.request_exit(0);
    loop_thread.join().unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_remote_offer_paste_echoes_the_legacy_string_target() {
    let _guard = SELECTION_LOCK.lock().await;
    let (handle, mut events, loop_thread) = skip_if_no_x!(spawn_backend!());

    handle.offer(text_offer());

    // Request the legacy STRING target: ICCCM requires the owner to echo the
    // REQUESTED target (STRING) in its SelectionNotify, even though it renders
    // the property with type UTF8_STRING.
    let paster = thread::spawn(|| {
        let requestor = Requestor::new();
        assert!(requestor.wait_until_owned(), "backend never took ownership");
        let string = requestor.string;
        (requestor.paste(string), string)
    });

    let (_format, token) = recv_paste(&mut events).await.expect("Paste upcall");
    handle.deliver(token, "text", b"legacy".to_vec());

    let (reply, string) = paster.join().unwrap();
    assert_eq!(
        reply.bytes.as_deref(),
        Some(b"legacy".as_ref()),
        "the bytes are delivered regardless of the requested target"
    );
    assert_eq!(
        reply.echoed_target, string,
        "the SelectionNotify must echo STRING (the requested target), not the UTF8_STRING type"
    );

    handle.request_exit(0);
    loop_thread.join().unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_refused_paste_answers_property_none() {
    let _guard = SELECTION_LOCK.lock().await;
    let (handle, mut events, loop_thread) = skip_if_no_x!(spawn_backend!());

    handle.offer(text_offer());

    let paster = thread::spawn(|| {
        let requestor = Requestor::new();
        assert!(requestor.wait_until_owned(), "backend never took ownership");
        let utf8 = requestor.utf8;
        requestor.paste(utf8)
    });

    let (_format, token) = recv_paste(&mut events).await.expect("Paste upcall");
    handle.paste_failed(token, "text");

    let reply = paster.join().unwrap();
    assert_eq!(
        reply.property,
        x::ATOM_NONE,
        "a refused paste answers property=NONE (clean refusal, never a truncated render)"
    );
    assert!(reply.bytes.is_none());

    handle.request_exit(0);
    loop_thread.join().unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_foreign_copy_is_detected_and_provided() {
    let _guard = SELECTION_LOCK.lock().await;
    let (handle, mut events, loop_thread) = skip_if_no_x!(spawn_backend!());

    let (stop, owner) = spawn_owner(b"foreign-hello");

    let (generation, formats, sensitive) = recv_copied(&mut events).await.expect("Copied upcall");
    assert!(
        formats.iter().any(|f| f.id == "text"),
        "text must be among the announced formats, got {formats:?}"
    );
    assert!(!sensitive);

    let bytes = handle.provide(generation, "text").await;
    assert_eq!(bytes.as_deref(), Some(b"foreign-hello".as_ref()));

    // A generation the backend never announced cannot be vouched for.
    assert!(
        handle
            .provide(generation.wrapping_add(999), "text")
            .await
            .is_none(),
        "a stale generation must not be served"
    );

    let _ = stop.send(());
    owner.join().unwrap();
    handle.request_exit(0);
    loop_thread.join().unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unsupported_foreign_copy_supersedes_and_stops_vouching() {
    let _guard = SELECTION_LOCK.lock().await;
    let (handle, mut events, loop_thread) = skip_if_no_x!(spawn_backend!());

    // A first app copies text we can sync.
    let (stop_a, owner_a) = spawn_owner(b"first-copy");
    let (generation, formats, _sensitive) = recv_copied(&mut events).await.expect("Copied upcall");
    assert!(formats.iter().any(|f| f.id == "text"));
    assert_eq!(
        handle.provide(generation, "text").await.as_deref(),
        Some(b"first-copy".as_ref())
    );

    // A second app copies content we cannot sync, stealing ownership with no
    // intervening owner->None. The backend must supersede the stale capture.
    let (stop_b, owner_b) = spawn_unsupported_owner();
    assert!(
        recv_cleared(&mut events).await,
        "an unsupported foreign copy must supersede the prior clip with a Cleared"
    );
    assert!(
        handle.provide(generation, "text").await.is_none(),
        "the backend must stop vouching for the superseded generation (no stale/sensitive bytes)"
    );

    let _ = stop_a.send(());
    owner_a.join().unwrap();
    let _ = stop_b.send(());
    owner_b.join().unwrap();
    handle.request_exit(0);
    loop_thread.join().unwrap();
}
