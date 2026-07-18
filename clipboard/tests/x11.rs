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

/// A whole INCR transfer (many round-trips, up to a ~20 MiB payload in the write
/// tests) gets a wider bound than a single round-trip; still finite so a stuck
/// transfer fails cleanly.
const INCR_DEADLINE: Duration = Duration::from_secs(20);

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
    /// The reply used the INCR chunked protocol rather than a single property.
    via_incr: bool,
    /// The INCR marker's declared lower-bound size, when `via_incr` (0 for a
    /// sensitive offer).
    declared_size: Option<u32>,
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
    incr: x::Atom,
}

impl Requestor {
    fn new() -> Requestor {
        let (conn, screen_num) = xcb::Connection::connect(None).expect("connect requestor");
        let window = create_window(&conn, screen_num);
        let clipboard = intern(&conn, b"CLIPBOARD");
        let utf8 = intern(&conn, b"UTF8_STRING");
        let string = intern(&conn, b"STRING");
        let prop = intern(&conn, b"UNIVERSALLINK_TEST_REPLY");
        let incr = intern(&conn, b"INCR");
        Requestor {
            conn,
            window,
            clipboard,
            utf8,
            string,
            prop,
            incr,
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
                via_incr: false,
                declared_size: None,
            };
        }
        // Peek the reply TYPE without deleting: an INCR reply needs the chunked
        // protocol, a direct reply is taken as-is.
        let cookie = self.conn.send_request(&x::GetProperty {
            delete: false,
            window: self.window,
            property,
            r#type: x::ATOM_ANY,
            long_offset: 0,
            long_length: 0x0100_0000,
        });
        let reply = self.conn.wait_for_reply(cookie).expect("GetProperty peek");
        if reply.r#type() == self.incr {
            let declared = reply.value::<u32>().first().copied();
            let bytes = self.read_incr(property);
            return PasteReply {
                echoed_target,
                property,
                bytes: Some(bytes),
                via_incr: true,
                declared_size: declared,
            };
        }
        // Direct reply: delete to clear (as a real requestor does) and take it.
        self.conn.send_request(&x::DeleteProperty {
            window: self.window,
            property,
        });
        let _ = self.conn.flush();
        PasteReply {
            echoed_target,
            property,
            bytes: Some(reply.value::<u8>().to_vec()),
            via_incr: false,
            declared_size: None,
        }
    }

    /// Consume an INCR transfer: delete the marker (the ICCCM "go"), then
    /// accumulate each chunk on `PropertyNotify(NewValue)` until a zero-length
    /// terminator.
    fn read_incr(&self, property: x::Atom) -> Vec<u8> {
        self.conn.send_request(&x::DeleteProperty {
            window: self.window,
            property,
        });
        self.conn.flush().expect("flush INCR go");
        let mut acc = Vec::new();
        let deadline = Instant::now() + INCR_DEADLINE;
        loop {
            assert!(
                self.wait_new_value(property, deadline),
                "INCR chunk not delivered within the deadline"
            );
            let cookie = self.conn.send_request(&x::GetProperty {
                delete: true,
                window: self.window,
                property,
                r#type: x::ATOM_ANY,
                long_offset: 0,
                long_length: 0x0100_0000,
            });
            let reply = self.conn.wait_for_reply(cookie).expect("GetProperty chunk");
            let chunk = reply.value::<u8>();
            if chunk.is_empty() {
                return acc; // zero-length terminator
            }
            acc.extend_from_slice(chunk);
        }
    }

    /// Wait (bounded) for the owner to append the next INCR chunk: a
    /// `PropertyNotify(NewValue)` on our window for `property`.
    fn wait_new_value(&self, property: x::Atom, deadline: Instant) -> bool {
        loop {
            while let Ok(Some(event)) = self.conn.poll_for_event() {
                if let xcb::Event::X(x::Event::PropertyNotify(e)) = event
                    && e.window() == self.window
                    && e.atom() == property
                    && e.state() == x::Property::NewValue
                {
                    return true;
                }
            }
            if Instant::now() >= deadline {
                return false;
            }
            thread::sleep(Duration::from_millis(2));
        }
    }

    /// Start an INCR paste and read exactly `n` chunks, then return WITHOUT
    /// finishing. Dropping the `Requestor` afterwards closes the connection and
    /// destroys the window, stranding the owner's send session mid-transfer.
    /// Asserts the reply really was an INCR send.
    fn start_incr_read_partial(&self, target: x::Atom, n: usize) {
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
        let Some((_t, property)) = self.wait_notify() else {
            panic!("no SelectionNotify within the deadline");
        };
        assert_ne!(property, x::ATOM_NONE);
        let cookie = self.conn.send_request(&x::GetProperty {
            delete: false,
            window: self.window,
            property,
            r#type: x::ATOM_ANY,
            long_offset: 0,
            long_length: 0x0100_0000,
        });
        let reply = self.conn.wait_for_reply(cookie).expect("peek");
        assert_eq!(reply.r#type(), self.incr, "expected an INCR send");
        self.conn.send_request(&x::DeleteProperty {
            window: self.window,
            property,
        });
        self.conn.flush().expect("flush INCR go");
        let deadline = Instant::now() + INCR_DEADLINE;
        for _ in 0..n {
            assert!(
                self.wait_new_value(property, deadline),
                "INCR chunk not delivered within the deadline"
            );
            let cookie = self.conn.send_request(&x::GetProperty {
                delete: true,
                window: self.window,
                property,
                r#type: x::ATOM_ANY,
                long_offset: 0,
                long_length: 0x0100_0000,
            });
            let _ = self.conn.wait_for_reply(cookie).expect("chunk");
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

/// Spawns a foreign owner that serves `UTF8_STRING` via the INCR protocol: it
/// advertises `TARGETS = [TARGETS, UTF8_STRING]`, answers a UTF8_STRING request
/// by planting an `INCR` marker (declaring `declared` as the lower-bound size),
/// then appends `chunks` one per property deletion, terminating with a
/// zero-length write. If `stall_after` is `Some(n)`, it appends exactly `n`
/// chunks and then stops responding (never terminates) — a stalled transfer.
fn spawn_incr_text_owner(
    chunks: Vec<Vec<u8>>,
    declared: u32,
    stall_after: Option<usize>,
) -> (std_mpsc::Sender<()>, thread::JoinHandle<()>) {
    let (stop_tx, stop_rx) = std_mpsc::channel::<()>();
    let handle = thread::spawn(move || {
        let (conn, screen_num) = xcb::Connection::connect(None).expect("connect incr owner");
        let window = create_window(&conn, screen_num);
        let clipboard = intern(&conn, b"CLIPBOARD");
        let targets = intern(&conn, b"TARGETS");
        let utf8 = intern(&conn, b"UTF8_STRING");
        let incr = intern(&conn, b"INCR");

        conn.send_request(&x::SetSelectionOwner {
            owner: window,
            selection: clipboard,
            time: x::CURRENT_TIME,
        });
        conn.flush().expect("incr owner SetSelectionOwner");

        // The single in-flight transfer's (requestor, property), and how many
        // chunks we have appended so far.
        let mut xfer: Option<(x::Window, x::Atom)> = None;
        let mut sent = 0usize;
        loop {
            while let Ok(Some(event)) = conn.poll_for_event() {
                match event {
                    xcb::Event::X(x::Event::SelectionRequest(e)) => {
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
                            // Begin INCR: watch the requestor for its deletions,
                            // plant the marker, and answer.
                            conn.send_request(&x::ChangeWindowAttributes {
                                window: e.requestor(),
                                value_list: &[x::Cw::EventMask(x::EventMask::PROPERTY_CHANGE)],
                            });
                            conn.send_request(&x::ChangeProperty {
                                mode: x::PropMode::Replace,
                                window: e.requestor(),
                                property,
                                r#type: incr,
                                data: &[declared],
                            });
                            send_notify(
                                &conn,
                                e.requestor(),
                                clipboard,
                                e.target(),
                                property,
                                e.time(),
                            );
                            let _ = conn.flush();
                            xfer = Some((e.requestor(), property));
                            sent = 0;
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
                    xcb::Event::X(x::Event::PropertyNotify(e))
                        if e.state() == x::Property::Delete =>
                    {
                        if let Some((req, prop)) = xfer
                            && e.window() == req
                            && e.atom() == prop
                        {
                            if let Some(n) = stall_after
                                && sent >= n
                            {
                                continue; // stalled: send nothing more
                            }
                            if sent < chunks.len() {
                                conn.send_request(&x::ChangeProperty {
                                    mode: x::PropMode::Replace,
                                    window: req,
                                    property: prop,
                                    r#type: utf8,
                                    data: &chunks[sent],
                                });
                                sent += 1;
                                let _ = conn.flush();
                            } else {
                                // All chunks appended → zero-length terminator.
                                conn.send_request(&x::ChangeProperty {
                                    mode: x::PropMode::Replace,
                                    window: req,
                                    property: prop,
                                    r#type: utf8,
                                    data: &[] as &[u8],
                                });
                                let _ = conn.flush();
                                xfer = None;
                            }
                        }
                    }
                    _ => {}
                }
            }
            if stop_rx.try_recv().is_ok() {
                break;
            }
            thread::sleep(Duration::from_millis(2));
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

/// Spawns a foreign owner that offers text ONLY as an over-cap INCR transfer
/// (declaring a size beyond the backend's read cap, and never appending a chunk)
/// alongside a small, direct `image/png`. The backend must skip the un-startable
/// text and still read the PNG cleanly from the SAME scratch property — the
/// direct evidence that abandoning an INCR read (without deleting the marker)
/// leaves nothing parked to corrupt the next conversion.
fn spawn_incr_overcap_plus_png_owner(
    png: Vec<u8>,
    declared: u32,
) -> (std_mpsc::Sender<()>, thread::JoinHandle<()>) {
    let (stop_tx, stop_rx) = std_mpsc::channel::<()>();
    let handle = thread::spawn(move || {
        let (conn, screen_num) = xcb::Connection::connect(None).expect("connect overcap owner");
        let window = create_window(&conn, screen_num);
        let clipboard = intern(&conn, b"CLIPBOARD");
        let targets = intern(&conn, b"TARGETS");
        let utf8 = intern(&conn, b"UTF8_STRING");
        let image_png = intern(&conn, b"image/png");
        let incr = intern(&conn, b"INCR");

        conn.send_request(&x::SetSelectionOwner {
            owner: window,
            selection: clipboard,
            time: x::CURRENT_TIME,
        });
        conn.flush().expect("overcap owner SetSelectionOwner");

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
                            data: &[targets, utf8, image_png],
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
                        // Plant an over-cap INCR marker; never append a chunk.
                        conn.send_request(&x::ChangeWindowAttributes {
                            window: e.requestor(),
                            value_list: &[x::Cw::EventMask(x::EventMask::PROPERTY_CHANGE)],
                        });
                        conn.send_request(&x::ChangeProperty {
                            mode: x::PropMode::Replace,
                            window: e.requestor(),
                            property,
                            r#type: incr,
                            data: &[declared],
                        });
                        send_notify(
                            &conn,
                            e.requestor(),
                            clipboard,
                            e.target(),
                            property,
                            e.time(),
                        );
                        let _ = conn.flush();
                    } else if e.target() == image_png {
                        conn.send_request(&x::ChangeProperty {
                            mode: x::PropMode::Replace,
                            window: e.requestor(),
                            property,
                            r#type: image_png,
                            data: &png,
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
            thread::sleep(Duration::from_millis(2));
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

/// Spawns a foreign owner that advertises `[UTF8_STRING, image/png]`, HOLDS BACK
/// its UTF8_STRING reply (so the backend times out on it), then flushes that
/// tardy text reply onto its property RIGHT BEFORE answering the subsequent
/// image/png request — so the stale reply is in flight exactly while the backend
/// reads image/png. Reproduces the "late reply for a timed-out format grabbed by
/// the next format" hazard on the shared scratch property.
fn spawn_late_reply_owner(
    text: Vec<u8>,
    png: Vec<u8>,
) -> (std_mpsc::Sender<()>, thread::JoinHandle<()>) {
    let (stop_tx, stop_rx) = std_mpsc::channel::<()>();
    let handle = thread::spawn(move || {
        let (conn, screen_num) = xcb::Connection::connect(None).expect("connect late-reply owner");
        let window = create_window(&conn, screen_num);
        let clipboard = intern(&conn, b"CLIPBOARD");
        let targets = intern(&conn, b"TARGETS");
        let utf8 = intern(&conn, b"UTF8_STRING");
        let image_png = intern(&conn, b"image/png");

        conn.send_request(&x::SetSelectionOwner {
            owner: window,
            selection: clipboard,
            time: x::CURRENT_TIME,
        });
        conn.flush().expect("late-reply owner SetSelectionOwner");

        let mut deferred_utf8: Option<(x::Window, x::Atom, x::Timestamp)> = None;
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
                            data: &[targets, utf8, image_png],
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
                        // Hold this reply back — the backend will time out on it.
                        deferred_utf8 = Some((e.requestor(), property, e.time()));
                    } else if e.target() == image_png {
                        // Flush the tardy UTF8_STRING reply FIRST (onto its own,
                        // now-abandoned property/target), THEN answer the PNG.
                        if let Some((req, prop, time)) = deferred_utf8.take() {
                            conn.send_request(&x::ChangeProperty {
                                mode: x::PropMode::Replace,
                                window: req,
                                property: prop,
                                r#type: utf8,
                                data: &text,
                            });
                            send_notify(&conn, req, clipboard, utf8, prop, time);
                        }
                        conn.send_request(&x::ChangeProperty {
                            mode: x::PropMode::Replace,
                            window: e.requestor(),
                            property,
                            r#type: image_png,
                            data: &png,
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
            thread::sleep(Duration::from_millis(2));
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

fn sensitive_text_offer() -> RemoteClip {
    RemoteClip {
        tx_id: "tx-test-sensitive".into(),
        formats: vec![Format {
            id: "text".into(),
            size: None,
        }],
        files: Vec::new(),
        sensitive: true,
    }
}

/// A deterministic, non-repeating-per-byte payload (`i % 251`, a prime stride so
/// chunk boundaries are not aligned to any power-of-two artifact): reassembly
/// errors show up as a value mismatch at a precise offset.
fn make_payload(n: usize) -> Vec<u8> {
    (0..n).map(|i| (i % 251) as u8).collect()
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

// ----- INCR: source side (consuming a foreign owner's large copy) -----

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_incr_foreign_copy_is_reassembled() {
    let _guard = SELECTION_LOCK.lock().await;
    let (handle, mut events, loop_thread) = skip_if_no_x!(spawn_backend!());

    // Three uneven chunks; declare a LOOSE lower bound (4 < the 12-byte total) so
    // the test proves reassembly runs to the terminator, not to the declared size.
    let chunks = vec![b"AAAA".to_vec(), b"BB".to_vec(), b"CCCCCC".to_vec()];
    let (stop, owner) = spawn_incr_text_owner(chunks, 4, None);

    let (generation, formats, sensitive) = recv_copied(&mut events).await.expect("Copied upcall");
    assert!(
        formats.iter().any(|f| f.id == "text"),
        "text must be announced, got {formats:?}"
    );
    assert!(!sensitive);
    assert_eq!(
        formats.iter().find(|f| f.id == "text").and_then(|f| f.size),
        Some(12),
        "the announced size is the reassembled length"
    );
    assert_eq!(
        handle.provide(generation, "text").await.as_deref(),
        Some(b"AAAABBCCCCCC".as_ref())
    );

    let _ = stop.send(());
    owner.join().unwrap();
    handle.request_exit(0);
    loop_thread.join().unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_over_cap_incr_text_is_skipped_and_the_png_still_reads() {
    let _guard = SELECTION_LOCK.lock().await;
    let (handle, mut events, loop_thread) = skip_if_no_x!(spawn_backend!());

    // The text is offered ONLY as an INCR transfer declaring 32 MiB (over the
    // 16 MiB read cap): the backend must skip it WITHOUT starting (never deleting
    // the marker), then read the direct PNG from the same scratch property with
    // no contamination.
    let png = b"\x89PNG\r\n\x1a\n-fake-image-bytes-".to_vec();
    let over_cap: u32 = 32 * 1024 * 1024;
    let (stop, owner) = spawn_incr_overcap_plus_png_owner(png.clone(), over_cap);

    let (generation, formats, _sensitive) = recv_copied(&mut events).await.expect("Copied upcall");
    assert!(
        formats.iter().any(|f| f.id == "image/png"),
        "the direct PNG must be announced, got {formats:?}"
    );
    assert!(
        !formats.iter().any(|f| f.id == "text"),
        "the over-cap INCR text must be skipped, got {formats:?}"
    );
    assert_eq!(
        handle.provide(generation, "image/png").await.as_deref(),
        Some(png.as_ref()),
        "the PNG reads cleanly — no parked INCR chunk corrupted it"
    );

    let _ = stop.send(());
    owner.join().unwrap();
    handle.request_exit(0);
    loop_thread.join().unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stalling_incr_owner_is_abandoned_then_a_later_copy_announces() {
    let _guard = SELECTION_LOCK.lock().await;
    let (handle, mut events, loop_thread) = skip_if_no_x!(spawn_backend!());

    // An owner that starts INCR, appends one chunk, then stalls forever.
    let (stop_stall, staller) = spawn_incr_text_owner(vec![b"partial".to_vec()], 64, Some(1));

    // Wait past the per-chunk INCR timeout so the backend abandons the stalled
    // read and rotates its scratch property. Sequenced deliberately (a concurrent
    // second copy would race the in-flight read); the staller keeps ownership
    // meanwhile, so no spurious conversion is issued.
    thread::sleep(Duration::from_millis(3000));
    let _ = stop_stall.send(());
    staller.join().unwrap();

    // A fresh, normal copy must still be detected and served — on the rotated
    // scratch property, unaffected by the staller's parked chunk.
    let (stop, owner) = spawn_owner(b"after-the-stall");
    let (generation, formats, _s) = recv_copied(&mut events)
        .await
        .expect("a copy after the stall must still announce");
    assert!(formats.iter().any(|f| f.id == "text"));
    assert_eq!(
        handle.provide(generation, "text").await.as_deref(),
        Some(b"after-the-stall".as_ref())
    );

    let _ = stop.send(());
    owner.join().unwrap();
    handle.request_exit(0);
    loop_thread.join().unwrap();
}

// ----- INCR: destination side (rendering a large paste to a requestor) -----

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_large_remote_offer_is_pasted_via_incr() {
    let _guard = SELECTION_LOCK.lock().await;
    let (handle, mut events, loop_thread) = skip_if_no_x!(spawn_backend!());

    handle.offer(text_offer());

    // ~20 MiB: beyond a single ChangeProperty on the (BIG-REQUESTS) test server,
    // so the backend must switch the write path to an INCR send.
    let payload = make_payload(20 * 1024 * 1024);
    let expected = payload.clone();
    let paster = thread::spawn(|| {
        let requestor = Requestor::new();
        assert!(requestor.wait_until_owned(), "backend never took ownership");
        let utf8 = requestor.utf8;
        requestor.paste(utf8)
    });

    let (_format, token) = recv_paste(&mut events).await.expect("Paste upcall");
    handle.deliver(token, "text", payload);

    let reply = paster.join().unwrap();
    assert!(
        reply.via_incr,
        "a payload beyond one request must be sent via INCR"
    );
    assert_eq!(reply.declared_size, Some(expected.len() as u32));
    assert_eq!(
        reply.bytes.as_deref(),
        Some(expected.as_ref()),
        "every chunk reassembles to the exact payload"
    );

    handle.request_exit(0);
    loop_thread.join().unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_two_megabyte_paste_stays_direct() {
    let _guard = SELECTION_LOCK.lock().await;
    let (handle, mut events, loop_thread) = skip_if_no_x!(spawn_backend!());

    handle.offer(text_offer());

    // 2 MiB fits one ChangeProperty on a BIG-REQUESTS server (~16 MiB limit), so
    // it must stay a direct reply — readable in one GetProperty by any requestor,
    // INCR-aware or not. Guards against regressing the write threshold below what
    // a direct write already supports.
    let payload = make_payload(2 * 1024 * 1024);
    let expected = payload.clone();
    let paster = thread::spawn(|| {
        let requestor = Requestor::new();
        assert!(requestor.wait_until_owned(), "backend never took ownership");
        let utf8 = requestor.utf8;
        requestor.paste(utf8)
    });

    let (_format, token) = recv_paste(&mut events).await.expect("Paste upcall");
    handle.deliver(token, "text", payload);

    let reply = paster.join().unwrap();
    assert!(
        !reply.via_incr,
        "a 2 MiB payload must stay a direct write, not INCR"
    );
    assert_eq!(reply.bytes.as_deref(), Some(expected.as_ref()));

    handle.request_exit(0);
    loop_thread.join().unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_sensitive_large_offer_declares_incr_size_zero() {
    let _guard = SELECTION_LOCK.lock().await;
    let (handle, mut events, loop_thread) = skip_if_no_x!(spawn_backend!());

    handle.offer(sensitive_text_offer());

    let payload = make_payload(20 * 1024 * 1024);
    let expected = payload.clone();
    let paster = thread::spawn(|| {
        let requestor = Requestor::new();
        assert!(requestor.wait_until_owned(), "backend never took ownership");
        let utf8 = requestor.utf8;
        requestor.paste(utf8)
    });

    let (_format, token) = recv_paste(&mut events).await.expect("Paste upcall");
    handle.deliver(token, "text", payload);

    let reply = paster.join().unwrap();
    assert!(reply.via_incr);
    assert_eq!(
        reply.declared_size,
        Some(0),
        "a sensitive INCR send declares size 0 — no length is leaked in the marker"
    );
    assert_eq!(
        reply.bytes.as_deref(),
        Some(expected.as_ref()),
        "the bytes still transfer in full"
    );

    handle.request_exit(0);
    loop_thread.join().unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_vanishing_requestor_mid_incr_does_not_wedge_the_backend() {
    let _guard = SELECTION_LOCK.lock().await;
    let (handle, mut events, loop_thread) = skip_if_no_x!(spawn_backend!());

    handle.offer(text_offer());
    let big = make_payload(20 * 1024 * 1024);

    // Requestor A starts an INCR paste, reads two chunks, then vanishes (its
    // connection closes and window is destroyed) mid-transfer.
    let a = thread::spawn(|| {
        let requestor = Requestor::new();
        assert!(requestor.wait_until_owned(), "backend never took ownership");
        let utf8 = requestor.utf8;
        requestor.start_incr_read_partial(utf8, 2);
    });
    let (_fa, token_a) = recv_paste(&mut events).await.expect("Paste A");
    handle.deliver(token_a, "text", big.clone());
    a.join().unwrap();

    // Requestor B pastes the still-live offer and must complete fully: the
    // backend is not wedged by A's abandoned (soon timed-out) session.
    let expected = big.clone();
    let b = thread::spawn(|| {
        let requestor = Requestor::new();
        assert!(requestor.wait_until_owned(), "backend never took ownership");
        let utf8 = requestor.utf8;
        requestor.paste(utf8)
    });
    let (_fb, token_b) = recv_paste(&mut events).await.expect("Paste B");
    handle.deliver(token_b, "text", big);

    let reply = b.join().unwrap();
    assert!(reply.via_incr);
    assert_eq!(
        reply.bytes.as_deref(),
        Some(expected.as_ref()),
        "a fresh requestor completes even after another vanished mid-INCR"
    );

    handle.request_exit(0);
    loop_thread.join().unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_late_reply_for_a_timed_out_format_is_not_served_as_the_next_format() {
    let _guard = SELECTION_LOCK.lock().await;
    let (handle, mut events, loop_thread) = skip_if_no_x!(spawn_backend!());

    // The owner withholds its UTF8_STRING reply until the backend has timed out
    // and moved on to image/png, then flushes the tardy text reply right as the
    // PNG is being read. The backend must NOT serve those text bytes as the PNG:
    // the reply is correlated by echoed target (and the timed-out conversion
    // rotated the scratch property away), so the stale reply is ignored.
    let text = b"this-is-text-not-an-image".to_vec();
    let png = b"\x89PNG\r\n\x1a\n-the-real-image-".to_vec();
    let (stop, owner) = spawn_late_reply_owner(text.clone(), png.clone());

    let (generation, formats, _sensitive) = recv_copied(&mut events).await.expect("Copied upcall");
    assert!(
        formats.iter().any(|f| f.id == "image/png"),
        "image/png must be announced, got {formats:?}"
    );
    assert_eq!(
        handle.provide(generation, "image/png").await.as_deref(),
        Some(png.as_ref()),
        "image/png must serve the PNG bytes, never the late UTF8_STRING reply"
    );
    // And the timed-out text must never be vouched for as text either.
    assert!(
        handle.provide(generation, "text").await != Some(text),
        "the timed-out text must not be served"
    );

    let _ = stop.send(());
    owner.join().unwrap();
    handle.request_exit(0);
    loop_thread.join().unwrap();
}
