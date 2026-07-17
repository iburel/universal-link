// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

//! Live Windows-clipboard integration tests for the Windows backend, exercising
//! the real Win32 delayed-render protocol by playing the copying / pasting
//! application against the backend. They need an interactive window station with
//! a usable clipboard; where there is none (a headless/session-0 CI runner),
//! `os::create` reports `Unsupported` and each test SKIPS rather than fails.
//!
//! All are `#[ignore]`d: the workspace CI runs `cargo test` without `--ignored`,
//! so they are NOT part of the automated suite — they are validated MANUALLY on
//! a real Windows desktop with:
//!
//!     cargo test -p universallink-clipboard --test windows -- --ignored --test-threads=1
//!
//! `OpenClipboard` is process-global, so the tests serialize on
//! [`CLIPBOARD_LOCK`] and must run single-threaded.

#![cfg(target_os = "windows")]

use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use tokio::sync::Mutex;
use tokio::sync::mpsc;
use universallink_clipboard::os;
use universallink_clipboard::{
    BackendEvent, ClipboardBackend, FileFetcher, Format, LocalClip, RemoteClip, RemoteFile,
};
use windows_sys::Win32::Foundation::HWND;
use windows_sys::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, GetClipboardData, IsClipboardFormatAvailable, OpenClipboard,
    RegisterClipboardFormatW, SetClipboardData,
};
use windows_sys::Win32::System::Memory::{
    GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock,
};

/// UTF-16LE text.
const CF_UNICODETEXT: u32 = 13;
/// A dropped-files list (a `DROPFILES` blob + the paths).
const CF_HDROP: u32 = 15;

/// Serializes the process-global clipboard (`OpenClipboard` is per-process).
static CLIPBOARD_LOCK: Mutex<()> = Mutex::const_new(());

/// Every upcall wait is bounded so a failure is a clean test failure, never a
/// hang on a runner.
const DEADLINE: Duration = Duration::from_secs(5);

/// Spawns the real backend on a dedicated thread (its message-only window and
/// pump are pinned there), and hands back the `Clone` handle, the upcall
/// receiver, and the loop's join handle. Evaluates to `None` when the platform
/// has no usable clipboard (the test then skips). Kept as a macro so the
/// handle's (unnameable, module-private) type is preserved by inference.
macro_rules! spawn_backend {
    () => {{
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
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

macro_rules! skip_if_unsupported {
    ($bound:expr) => {
        match $bound {
            Some(parts) => parts,
            None => {
                eprintln!("skipping: no usable Windows clipboard (headless / session 0)");
                return;
            }
        }
    };
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

/// Blocks (bounded) until the backend has promised `CF_UNICODETEXT`.
fn wait_until_text_promised() -> bool {
    let deadline = Instant::now() + DEADLINE;
    while Instant::now() < deadline {
        // `IsClipboardFormatAvailable` needs no `OpenClipboard`; opening here
        // would race the backend's own open in `on_offer` (one opener per
        // station) and manufacture the very contention it means to observe.
        if unsafe { IsClipboardFormatAvailable(CF_UNICODETEXT) } != 0 {
            return true;
        }
        thread::sleep(Duration::from_millis(20));
    }
    false
}

/// Plays the pasting app: `GetClipboardData(CF_UNICODETEXT)` triggers the
/// backend's synchronous delayed render. `None` on a clean refusal.
fn paste_text() -> Option<String> {
    unsafe {
        if OpenClipboard(std::ptr::null_mut()) == 0 {
            return None;
        }
        let h = GetClipboardData(CF_UNICODETEXT);
        let out = if h.is_null() {
            None
        } else {
            let max_u16 = GlobalSize(h) / 2;
            let ptr = GlobalLock(h) as *const u16;
            if ptr.is_null() {
                None
            } else {
                let mut len = 0usize;
                while len < max_u16 && *ptr.add(len) != 0 {
                    len += 1;
                }
                let s = String::from_utf16_lossy(std::slice::from_raw_parts(ptr, len));
                GlobalUnlock(h);
                Some(s)
            }
        };
        CloseClipboard();
        out
    }
}

/// Plays a foreign app copying text: takes the clipboard with a NULL owner (so
/// the backend sees an owner that is not its own window) and sets UTF-16LE text.
fn set_clipboard_text(text: &str) {
    let mut blob: Vec<u8> = Vec::with_capacity(text.len() * 2 + 2);
    for u in text.encode_utf16() {
        blob.extend_from_slice(&u.to_le_bytes());
    }
    blob.extend_from_slice(&0u16.to_le_bytes());
    unsafe {
        assert!(
            OpenClipboard(std::ptr::null_mut()) != 0,
            "OpenClipboard (copy)"
        );
        EmptyClipboard();
        let h = GlobalAlloc(GMEM_MOVEABLE, blob.len());
        assert!(!h.is_null(), "GlobalAlloc");
        let dst = GlobalLock(h) as *mut u8;
        std::ptr::copy_nonoverlapping(blob.as_ptr(), dst, blob.len());
        GlobalUnlock(h);
        assert!(
            !SetClipboardData(CF_UNICODETEXT, h as HWND).is_null(),
            "SetClipboardData"
        );
        CloseClipboard();
    }
}

/// Delayed render, happy path: we promise `CF_UNICODETEXT`; the app pastes →
/// synchronous `WM_RENDERFORMAT` → the "orchestrator" delivers the pulled bytes
/// → the rendered data is exactly those bytes.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "real Windows clipboard; run manually with --ignored --test-threads=1"]
async fn delayed_render_text_roundtrip() {
    let _guard = CLIPBOARD_LOCK.lock().await;
    let (handle, mut events, loop_thread) = skip_if_unsupported!(spawn_backend!());

    handle.offer(text_offer());

    // The paste blocks inside GetClipboardData (synchronous render), so it MUST
    // run on its own thread while this one delivers the bytes.
    let paster = thread::spawn(|| {
        assert!(wait_until_text_promised(), "backend never promised text");
        paste_text()
    });

    let (format, token) = recv_paste(&mut events).await.expect("Paste upcall");
    assert_eq!(format, "text");
    handle.deliver(token, "text", "pasted on Windows".as_bytes().to_vec());

    let got = paster.join().unwrap();
    assert_eq!(got.as_deref(), Some("pasted on Windows"));

    handle.request_exit(0);
    loop_thread.join().unwrap();
}

/// A refused paste renders nothing: `GetClipboardData` returns NULL (a clean
/// refusal, never a truncated blob or a freeze).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "real Windows clipboard; run manually with --ignored --test-threads=1"]
async fn a_refused_paste_renders_nothing() {
    let _guard = CLIPBOARD_LOCK.lock().await;
    let (handle, mut events, loop_thread) = skip_if_unsupported!(spawn_backend!());

    handle.offer(text_offer());

    let paster = thread::spawn(|| {
        assert!(wait_until_text_promised(), "backend never promised text");
        paste_text()
    });

    let (_format, token) = recv_paste(&mut events).await.expect("Paste upcall");
    handle.paste_failed(token, "text");

    assert_eq!(paster.join().unwrap(), None, "a refused paste renders NULL");

    handle.request_exit(0);
    loop_thread.join().unwrap();
}

/// Source side: a foreign app copies text; the backend detects it
/// (`WM_CLIPBOARDUPDATE`), eager-reads it, emits `Copied`, and `provide` serves
/// the cached bytes only for the live generation.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "real Windows clipboard; run manually with --ignored --test-threads=1"]
async fn a_foreign_text_copy_is_detected_and_provided() {
    let _guard = CLIPBOARD_LOCK.lock().await;
    let (handle, mut events, loop_thread) = skip_if_unsupported!(spawn_backend!());

    set_clipboard_text("foreign windows text");

    let (generation, formats, sensitive) = recv_copied(&mut events).await.expect("Copied upcall");
    assert!(
        formats.iter().any(|f| f.id == "text"),
        "text must be among the announced formats, got {formats:?}"
    );
    assert!(!sensitive);

    let bytes = handle.provide(generation, "text").await;
    assert_eq!(bytes.as_deref(), Some("foreign windows text".as_bytes()));

    // A generation the backend never announced cannot be vouched for.
    assert!(
        handle
            .provide(generation.wrapping_add(999), "text")
            .await
            .is_none(),
        "a stale generation must not be served"
    );

    handle.request_exit(0);
    loop_thread.join().unwrap();
}

// ----- Files brick (OLE destination + CF_HDROP source) -----

/// A `&str` → a `NUL`-terminated wide string for the `…W` APIs.
fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// A trivial fetcher for the destination test: it never has to serve real bytes
/// (the test only checks the offer is installed), so every read is immediate EOF.
struct ZeroFetcher;

impl FileFetcher for ZeroFetcher {
    fn read(&self, _file_id: &str, _offset: u64, _len: u64) -> std::io::Result<Vec<u8>> {
        Ok(Vec::new())
    }
}

fn files_offer() -> RemoteClip {
    RemoteClip {
        tx_id: "tx-files".into(),
        formats: vec![Format {
            id: "files".into(),
            size: None,
        }],
        files: vec![RemoteFile {
            file_id: "id-a".into(),
            path: "a.txt".into(),
            size: 3,
            dir: false,
        }],
        sensitive: false,
    }
}

/// Waits (bounded) for the next `Copied` upcall and returns the whole clip
/// (paths included), ignoring any other event.
async fn recv_copied_clip(events: &mut mpsc::Receiver<BackendEvent>) -> Option<(u64, LocalClip)> {
    let deadline = tokio::time::Instant::now() + DEADLINE;
    loop {
        match tokio::time::timeout_at(deadline, events.recv()).await {
            Ok(Some(BackendEvent::Copied { generation, clip })) => return Some((generation, clip)),
            Ok(Some(_)) => continue,
            _ => return None,
        }
    }
}

/// Plays a foreign app copying files: takes the clipboard with a NULL owner and
/// sets a WIDE `CF_HDROP` (`DROPFILES` header + the paths, double-`NUL` ended).
fn set_clipboard_hdrop(paths: &[&str]) {
    let mut blob: Vec<u8> = Vec::new();
    blob.extend_from_slice(&20u32.to_le_bytes()); // pFiles: list right after header
    blob.extend_from_slice(&[0u8; 8]); // pt (unused)
    blob.extend_from_slice(&0u32.to_le_bytes()); // fNC (unused)
    blob.extend_from_slice(&1u32.to_le_bytes()); // fWide = true
    for p in paths {
        for u in p.encode_utf16() {
            blob.extend_from_slice(&u.to_le_bytes());
        }
        blob.extend_from_slice(&0u16.to_le_bytes());
    }
    blob.extend_from_slice(&0u16.to_le_bytes()); // final empty string
    unsafe {
        assert!(
            OpenClipboard(std::ptr::null_mut()) != 0,
            "OpenClipboard (hdrop)"
        );
        EmptyClipboard();
        let h = GlobalAlloc(GMEM_MOVEABLE, blob.len());
        assert!(!h.is_null(), "GlobalAlloc");
        let dst = GlobalLock(h) as *mut u8;
        std::ptr::copy_nonoverlapping(blob.as_ptr(), dst, blob.len());
        GlobalUnlock(h);
        assert!(
            !SetClipboardData(CF_HDROP, h as HWND).is_null(),
            "SetClipboardData(CF_HDROP)"
        );
        CloseClipboard();
    }
}

/// Blocks (bounded) until clipboard format `cf` is available.
fn wait_until_format_available(cf: u32) -> bool {
    let deadline = Instant::now() + DEADLINE;
    while Instant::now() < deadline {
        if unsafe { IsClipboardFormatAvailable(cf) } != 0 {
            return true;
        }
        thread::sleep(Duration::from_millis(20));
    }
    false
}

/// Source side: a foreign app copies files (`CF_HDROP`); the backend detects it,
/// parses the `DROPFILES` paths, and announces a `files` `Copied` with them (no
/// inline formats), mirroring the X11 files-source path.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "real Windows clipboard; run manually with --ignored --test-threads=1"]
async fn a_foreign_files_copy_is_detected_and_announced() {
    let _guard = CLIPBOARD_LOCK.lock().await;
    let (handle, mut events, loop_thread) = skip_if_unsupported!(spawn_backend!());

    let paths = ["C:\\Temp\\ul-a.txt", "C:\\Temp\\ul-dir\\b.bin"];
    set_clipboard_hdrop(&paths);

    let (_generation, clip) = recv_copied_clip(&mut events).await.expect("Copied upcall");
    assert!(
        clip.formats.iter().any(|f| f.id == "files"),
        "a files copy must announce the `files` format, got {:?}",
        clip.formats
    );
    assert_eq!(
        clip.paths,
        paths
            .iter()
            .map(std::path::PathBuf::from)
            .collect::<Vec<_>>(),
        "the announced paths must be the parsed DROPFILES paths"
    );

    handle.request_exit(0);
    loop_thread.join().unwrap();
}

/// Destination side (best-effort): an `offer_files` puts an OLE `IDataObject` on
/// the clipboard, so `FileGroupDescriptorW` becomes available (the anti-echo's
/// `OleIsCurrentClipboard` would then recognize our own object). The full paste
/// marshaling to Explorer is untestable in CI — validated manually.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "real Windows clipboard + OLE; run manually with --ignored --test-threads=1"]
async fn an_offer_files_installs_the_descriptor_format() {
    let _guard = CLIPBOARD_LOCK.lock().await;
    let (handle, _events, loop_thread) = skip_if_unsupported!(spawn_backend!());

    handle.offer_files(files_offer(), Arc::new(ZeroFetcher));

    let cf = unsafe { RegisterClipboardFormatW(wide("FileGroupDescriptorW").as_ptr()) };
    assert!(
        wait_until_format_available(cf),
        "the OLE files offer must publish FileGroupDescriptorW"
    );

    handle.request_exit(0);
    loop_thread.join().unwrap();
}
