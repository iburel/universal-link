// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

//! Live macOS-pasteboard integration tests for the macOS backend, exercising the
//! real delayed-render protocol by playing the copying / pasting application
//! against the backend on the general pasteboard. They need a logged-in GUI
//! session (where `pbs` runs); with none, the pasteboard calls hang, so these are
//! run only manually.
//!
//! All are `#[ignore]`d: the workspace CI runs `cargo test` without `--ignored`,
//! so they are NOT part of the automated suite — they are validated MANUALLY on a
//! real Mac desktop with:
//!
//!     cargo test -p universallink-clipboard --test macos -- --ignored --test-threads=1
//!
//! The general pasteboard is process-global, so the tests serialize on
//! [`PASTEBOARD_LOCK`] and must run single-threaded.
//!
//! # Honest caveat (unverified in-process delivery)
//!
//! Cross-process reads (a real Finder / Cmd-V) are served on the MAIN run loop
//! (see the backend's module docs). These tests, being in-process, drive the
//! backend on a spawned thread (mirroring `tests/windows.rs`) whose run loop the
//! `MacLoop` pumps. Whether AppKit delivers an in-process
//! `pasteboard:provideDataForType:` to that thread's run loop or elsewhere is
//! unverified from CI; if the happy-path paste tests hang on a real Mac, the loop
//! may need to run on the main test thread instead. The `Copied`/`provide`
//! (source-side) path does not depend on this — it is a plain `changeCount` poll.

#![cfg(target_os = "macos")]

use std::thread;
use std::time::{Duration, Instant};

use objc2_app_kit::{
    NSPasteboard, NSPasteboardType, NSPasteboardTypePNG, NSPasteboardTypeString,
    NSPasteboardTypeTIFF,
};
use objc2_foundation::{NSArray, NSData, NSString};
use tokio::sync::Mutex;
use tokio::sync::mpsc;
use universallink_clipboard::os;
use universallink_clipboard::{BackendEvent, ClipboardBackend, Format, RemoteClip};

/// Serializes the process-global general pasteboard.
static PASTEBOARD_LOCK: Mutex<()> = Mutex::const_new(());

/// Every upcall wait is bounded so a failure is a clean test failure, never a
/// hang on a runner.
const DEADLINE: Duration = Duration::from_secs(5);

/// A tiny valid PNG (2×2 RGBA) delivered as the `image/png` wire payload.
const PNG_2X2: &[u8] = &[
    0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0x00, 0x02, 0x00, 0x00, 0x00, 0x02, 0x08, 0x06, 0x00, 0x00, 0x00, 0x72, 0xB6, 0x0D,
    0x24, 0x00, 0x00, 0x00, 0x15, 0x49, 0x44, 0x41, 0x54, 0x78, 0xDA, 0x63, 0xF8, 0xCF, 0xC0, 0xF0,
    0x1F, 0x0C, 0x81, 0xF4, 0x7F, 0x2E, 0x11, 0xB9, 0x06, 0x00, 0x40, 0xF2, 0x06, 0xB7, 0xAA, 0x5F,
    0x28, 0x80, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
];

/// Spawns the real backend on a dedicated thread (its `NSPasteboard` and run-loop
/// pump are pinned there), and hands back the `Clone` handle, the upcall
/// receiver, and the loop's join handle. Evaluates to `None` when the platform
/// has no usable pasteboard (the test then skips). Kept as a macro so the
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
                eprintln!("skipping: no usable macOS pasteboard (no GUI session)");
                return;
            }
        }
    };
}

fn image_offer() -> RemoteClip {
    RemoteClip {
        tx_id: "tx-test".into(),
        formats: vec![Format {
            id: "image/png".into(),
            size: None,
        }],
        files: Vec::new(),
        sensitive: false,
    }
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

fn general() -> objc2::rc::Retained<NSPasteboard> {
    NSPasteboard::generalPasteboard()
}

/// Blocks (bounded) until the backend has promised `ty` on the pasteboard.
/// Checking availability does not pull the promised data.
fn wait_until_type_available(ty: &NSPasteboardType) -> bool {
    let pb = general();
    let deadline = Instant::now() + DEADLINE;
    while Instant::now() < deadline {
        let arr = NSArray::from_slice(&[ty]);
        if pb.availableTypeFromArray(&arr).is_some() {
            return true;
        }
        thread::sleep(Duration::from_millis(20));
    }
    false
}

/// Plays a foreign app copying text: writes UTF-8 text onto the general
/// pasteboard (a change the backend sees as not its own). Like a real app, it
/// DECLARES the type before writing — `setString:forType:` on an undeclared type
/// does not round-trip through `stringForType:`.
fn set_pasteboard_text(text: &str) {
    let pb = general();
    pb.clearContents();
    let s = NSString::from_str(text);
    // SAFETY: `NSPasteboardTypeString` is an AppKit extern static; a `None` owner
    // means the data is provided immediately (no delayed render).
    let ty = unsafe { NSPasteboardTypeString };
    let types = NSArray::from_slice(&[ty]);
    let _ = unsafe { pb.declareTypes_owner(&types, None) };
    let ok = pb.setString_forType(&s, ty);
    assert!(ok, "setString:forType: (copy)");
}

/// Plays a foreign app copying an image: writes raw PNG onto the pasteboard,
/// declaring the type first (as a real app does).
fn set_pasteboard_png(png: &[u8]) {
    let pb = general();
    pb.clearContents();
    let d = NSData::with_bytes(png);
    // SAFETY: `NSPasteboardTypePNG` is an AppKit extern static.
    let ty = unsafe { NSPasteboardTypePNG };
    let types = NSArray::from_slice(&[ty]);
    let _ = unsafe { pb.declareTypes_owner(&types, None) };
    let ok = pb.setData_forType(Some(&d), ty);
    assert!(ok, "setData:forType: (copy)");
}

/// Delayed render, text happy path: we promise text; the app pastes →
/// synchronous provide callback → the "orchestrator" delivers the pulled bytes →
/// the rendered string is exactly those bytes.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "real macOS pasteboard; run manually with --ignored --test-threads=1"]
async fn delayed_render_text_roundtrip() {
    let _guard = PASTEBOARD_LOCK.lock().await;
    let (handle, mut events, loop_thread) = skip_if_unsupported!(spawn_backend!());

    handle.offer(text_offer());

    // The paste blocks inside the provide callback (synchronous render), so it
    // MUST run on its own thread while this one delivers the bytes.
    let paster = thread::spawn(|| {
        // SAFETY: AppKit extern static.
        let ty = unsafe { NSPasteboardTypeString };
        assert!(wait_until_type_available(ty), "backend never promised text");
        general().stringForType(ty).map(|s| s.to_string())
    });

    let (format, token) = recv_paste(&mut events).await.expect("Paste upcall");
    assert_eq!(format, "text");
    handle.deliver(token, "text", "pasted on macOS".as_bytes().to_vec());

    let got = paster.join().unwrap();
    assert_eq!(got.as_deref(), Some("pasted on macOS"));

    handle.request_exit(0);
    loop_thread.join().unwrap();
}

/// Delayed render, image happy path (PNG asked): the delivered PNG is rendered
/// verbatim for a `public.png` request.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "real macOS pasteboard; run manually with --ignored --test-threads=1"]
async fn delayed_render_png_roundtrip() {
    let _guard = PASTEBOARD_LOCK.lock().await;
    let (handle, mut events, loop_thread) = skip_if_unsupported!(spawn_backend!());

    handle.offer(image_offer());

    let paster = thread::spawn(|| {
        // SAFETY: AppKit extern static.
        let ty = unsafe { NSPasteboardTypePNG };
        assert!(wait_until_type_available(ty), "backend never promised PNG");
        general().dataForType(ty).map(|d| d.to_vec())
    });

    let (format, token) = recv_paste(&mut events).await.expect("Paste upcall");
    assert_eq!(format, "image/png");
    handle.deliver(token, "image/png", PNG_2X2.to_vec());

    let got = paster.join().unwrap();
    assert_eq!(got.as_deref(), Some(PNG_2X2), "PNG rendered verbatim");

    handle.request_exit(0);
    loop_thread.join().unwrap();
}

/// Delayed render, image with a TIFF ask: we deliver PNG bytes but the app asked
/// for `public.tiff`, so the backend converts PNG → TIFF and renders TIFF (the
/// "echo the requested type" discipline).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "real macOS pasteboard; run manually with --ignored --test-threads=1"]
async fn delayed_render_tiff_ask_converts_from_png() {
    let _guard = PASTEBOARD_LOCK.lock().await;
    let (handle, mut events, loop_thread) = skip_if_unsupported!(spawn_backend!());

    handle.offer(image_offer());

    let paster = thread::spawn(|| {
        // SAFETY: AppKit extern static.
        let ty = unsafe { NSPasteboardTypeTIFF };
        assert!(wait_until_type_available(ty), "backend never promised TIFF");
        general().dataForType(ty).map(|d| d.to_vec())
    });

    let (format, token) = recv_paste(&mut events).await.expect("Paste upcall");
    assert_eq!(format, "image/png");
    handle.deliver(token, "image/png", PNG_2X2.to_vec());

    let got = paster.join().unwrap().expect("some TIFF bytes");
    assert!(
        got.starts_with(b"II*\0") || got.starts_with(b"MM\0*"),
        "the PNG was rendered as TIFF for a TIFF request, got {:?}",
        &got[..got.len().min(4)]
    );

    handle.request_exit(0);
    loop_thread.join().unwrap();
}

/// A refused paste renders nothing: `stringForType:` returns nil (a clean
/// refusal, never a truncated blob or a freeze).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "real macOS pasteboard; run manually with --ignored --test-threads=1"]
async fn a_refused_paste_renders_nothing() {
    let _guard = PASTEBOARD_LOCK.lock().await;
    let (handle, mut events, loop_thread) = skip_if_unsupported!(spawn_backend!());

    handle.offer(text_offer());

    let paster = thread::spawn(|| {
        // SAFETY: AppKit extern static.
        let ty = unsafe { NSPasteboardTypeString };
        assert!(wait_until_type_available(ty), "backend never promised text");
        general().stringForType(ty).map(|s| s.to_string())
    });

    let (_format, token) = recv_paste(&mut events).await.expect("Paste upcall");
    handle.paste_failed(token, "text");

    assert_eq!(paster.join().unwrap(), None, "a refused paste renders nil");

    handle.request_exit(0);
    loop_thread.join().unwrap();
}

/// Source side: a foreign app copies text; the backend detects it (polling
/// `changeCount`), eager-reads it, emits `Copied`, and `provide` serves the
/// cached bytes only for the live generation.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "real macOS pasteboard; run manually with --ignored --test-threads=1"]
async fn a_foreign_text_copy_is_detected_and_provided() {
    let _guard = PASTEBOARD_LOCK.lock().await;
    let (handle, mut events, loop_thread) = skip_if_unsupported!(spawn_backend!());

    set_pasteboard_text("foreign macos text");

    let (generation, formats, sensitive) = recv_copied(&mut events).await.expect("Copied upcall");
    assert!(
        formats.iter().any(|f| f.id == "text"),
        "text must be among the announced formats, got {formats:?}"
    );
    assert!(!sensitive);

    let bytes = handle.provide(generation, "text").await;
    assert_eq!(bytes.as_deref(), Some("foreign macos text".as_bytes()));

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

/// Source side, image: a foreign PNG copy is detected and provided as
/// `image/png` verbatim.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "real macOS pasteboard; run manually with --ignored --test-threads=1"]
async fn a_foreign_png_copy_is_detected_and_provided() {
    let _guard = PASTEBOARD_LOCK.lock().await;
    let (handle, mut events, loop_thread) = skip_if_unsupported!(spawn_backend!());

    set_pasteboard_png(PNG_2X2);

    let (generation, formats, _sensitive) = recv_copied(&mut events).await.expect("Copied upcall");
    assert!(
        formats.iter().any(|f| f.id == "image/png"),
        "image/png must be among the announced formats, got {formats:?}"
    );

    let bytes = handle.provide(generation, "image/png").await;
    assert_eq!(bytes.as_deref(), Some(PNG_2X2), "PNG served verbatim");

    handle.request_exit(0);
    loop_thread.join().unwrap();
}

/// Regression (review finding): releasing an offer must NOT wipe a foreign copy
/// that raced in first. We take ownership with an offer, a foreign app copies
/// (no change event fires, so the backend has not polled it yet), then we
/// `release()`. Because commands are drained BEFORE the poll each turn,
/// `on_release` runs while `is_owner` is still stale-true — it must re-read
/// `changeCount` live and skip `clearContents`, leaving the foreign copy intact.
/// The proof it did NOT clear: the very next poll still sees the foreign change
/// and announces it (`Copied`), and the text is still on the pasteboard. With the
/// bug (`clearContents` on stale `is_owner`), the copy is wiped, no `Copied` is
/// emitted, and `recv_copied` times out.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "real macOS pasteboard; run manually with --ignored --test-threads=1"]
async fn release_does_not_wipe_a_foreign_copy_that_raced_in() {
    let _guard = PASTEBOARD_LOCK.lock().await;
    let (handle, mut events, loop_thread) = skip_if_unsupported!(spawn_backend!());

    // Take ownership with a remote offer.
    handle.offer(text_offer());
    // SAFETY: AppKit extern static.
    let text_ty = unsafe { NSPasteboardTypeString };
    assert!(
        wait_until_type_available(text_ty),
        "backend never promised text"
    );

    // A foreign app copies while we still believe we own the pasteboard.
    set_pasteboard_text("foreign wins the race");
    // Withdraw the offer immediately — this reaches on_release before the poll can
    // clear the stale is_owner.
    handle.release();

    // With the fix, on_release skips the clear and the next poll announces the
    // surviving foreign copy; with the bug, the copy is wiped and no Copied comes.
    let (_generation, formats, _sensitive) = recv_copied(&mut events)
        .await
        .expect("a foreign copy that survived release must still be announced");
    assert!(
        formats.iter().any(|f| f.id == "text"),
        "the surviving foreign copy is text, got {formats:?}"
    );
    let survived = general().stringForType(text_ty).map(|s| s.to_string());
    assert_eq!(
        survived.as_deref(),
        Some("foreign wins the race"),
        "release must not wipe another app's clipboard"
    );

    handle.request_exit(0);
    loop_thread.join().unwrap();
}
