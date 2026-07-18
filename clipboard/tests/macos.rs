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

use std::path::{Path, PathBuf};
use std::ptr::NonNull;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use block2::RcBlock;
use objc2::rc::Retained;
use objc2_app_kit::{
    NSPasteboard, NSPasteboardType, NSPasteboardTypeFileURL, NSPasteboardTypePNG,
    NSPasteboardTypeString, NSPasteboardTypeTIFF,
};
use objc2_foundation::{
    NSArray, NSData, NSError, NSFileCoordinator, NSFileCoordinatorReadingOptions, NSString, NSURL,
};
use tokio::sync::Mutex;
use tokio::sync::mpsc;
use universallink_clipboard::os;
use universallink_clipboard::{
    BackendEvent, ClipboardBackend, FileFetcher, Format, RemoteClip, RemoteFile,
};

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

/// Plays a foreign app copying FILES: writes the POSIX paths under
/// `NSFilenamesPboardType` (the legacy array-of-strings a Finder copy exposes),
/// declaring the type first. macOS canonicalizes these paths (security-scoped
/// bookmarks) at WRITE time, so they must be real, existing paths — which is also
/// why a malformed (non-string) `NSFilenamesPboardType` cannot be stored at all
/// (the writer aborts), and the reader is safe from that shape by construction.
fn set_pasteboard_filenames(paths: &[&Path]) {
    let pb = general();
    pb.clearContents();
    let strings: Vec<Retained<NSString>> = paths
        .iter()
        .map(|p| NSString::from_str(&p.to_string_lossy()))
        .collect();
    let refs: Vec<&NSString> = strings.iter().map(|s| &**s).collect();
    let array = NSArray::from_slice(&refs);
    // SAFETY: AppKit extern static (the deprecated legacy filenames type).
    #[allow(deprecated)]
    let ty = unsafe { objc2_app_kit::NSFilenamesPboardType };
    let types = NSArray::from_slice(&[ty]);
    let _ = unsafe { pb.declareTypes_owner(&types, None) };
    // SAFETY: `NSFilenamesPboardType`'s property list is an array of path strings.
    let ok = unsafe { pb.setPropertyList_forType(&array, ty) };
    assert!(ok, "setPropertyList:forType: (files copy)");
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

/// Source side, files: a foreign app copies real files (their POSIX paths under
/// `NSFilenamesPboardType`, as a Finder copy exposes them). The backend detects a
/// FILES copy and announces `files` (the Core enumerates the paths — we do not
/// walk directories). This is also the coverage for the checked-downcast
/// `read_files`: a broken downcast would return `None`, and the copy would be
/// announced as text (the paths are also legible as a string) or not at all,
/// failing the `files` assertion.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "real macOS pasteboard; run manually with --ignored --test-threads=1"]
async fn a_foreign_files_copy_is_detected() {
    let _guard = PASTEBOARD_LOCK.lock().await;
    let (handle, mut events, loop_thread) = skip_if_unsupported!(spawn_backend!());

    // Real temp files: macOS canonicalizes the paths at write time.
    let dir = std::env::temp_dir().join(format!("ul-clip-src-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let a = dir.join("a.txt");
    let b = dir.join("b.txt");
    std::fs::write(&a, b"a").expect("write a");
    std::fs::write(&b, b"b").expect("write b");

    set_pasteboard_filenames(&[&a, &b]);

    let (_generation, formats, sensitive) = recv_copied(&mut events)
        .await
        .expect("a foreign files copy must be detected");
    assert!(
        formats.iter().any(|f| f.id == "files"),
        "the copy must be announced as files (read_files parsed the array), got {formats:?}"
    );
    assert!(!sensitive);

    handle.request_exit(0);
    loop_thread.join().unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

// ----- Files: fill-on-paste (destination side) -----

/// A `FileFetcher` double standing in for the Core's push: on `fill` it writes the
/// payload into each destination skeleton path (as the real Core would) and counts
/// its calls. `read` is never used on macOS (fill-only).
struct FillFetcher {
    payload: Vec<u8>,
    calls: AtomicUsize,
}

impl FillFetcher {
    fn new(payload: Vec<u8>) -> Arc<Self> {
        Arc::new(Self {
            payload,
            calls: AtomicUsize::new(0),
        })
    }
}

impl FileFetcher for FillFetcher {
    fn read(&self, _file_id: &str, _offset: u64, _len: u64) -> std::io::Result<Vec<u8>> {
        Err(std::io::Error::other("macOS uses fill, not read"))
    }

    fn fill(&self, entries: &[(String, PathBuf)]) -> std::io::Result<Vec<PathBuf>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let mut written = Vec::new();
        for (_file_id, dest) in entries {
            std::fs::write(dest, &self.payload)?;
            written.push(dest.clone());
        }
        Ok(written)
    }
}

fn files_offer(files: Vec<RemoteFile>) -> RemoteClip {
    RemoteClip {
        tx_id: "tx-files".into(),
        formats: vec![Format {
            id: "files".into(),
            size: None,
        }],
        files,
        sensitive: false,
    }
}

/// The POSIX path of the first published `file://` URL, waited for (bounded). Our
/// skeleton names have no special characters, so a plain `file://<authority>/path`
/// strip yields the path (no percent-decoding needed here).
fn wait_for_published_path() -> Option<PathBuf> {
    // SAFETY: AppKit extern static.
    let ty = unsafe { NSPasteboardTypeFileURL };
    let deadline = Instant::now() + DEADLINE;
    while Instant::now() < deadline {
        if let Some(s) = general().stringForType(ty) {
            let url = s.to_string();
            if let Some(rest) = url.strip_prefix("file://") {
                let idx = rest.find('/').unwrap_or(0);
                return Some(PathBuf::from(&rest[idx..]));
            }
        }
        thread::sleep(Duration::from_millis(20));
    }
    None
}

/// Source wiring: offering a files clip builds the skeleton, registers a presenter
/// per leaf, and publishes the top-level `file://` URLs. We do NOT paste here (a
/// real Finder is needed for that) — this exercises the whole objc2 SOURCE path
/// (skeleton on disk + `NSFilePresenter`/`NSFileCoordinator` + `writeObjects:` +
/// the `removeFilePresenter:` + skeleton removal on teardown) that no pure test
/// covers.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "real macOS pasteboard; run manually with --ignored --test-threads=1"]
async fn a_files_offer_publishes_file_urls() {
    let _guard = PASTEBOARD_LOCK.lock().await;
    let (handle, _events, loop_thread) = skip_if_unsupported!(spawn_backend!());

    let fetcher = FillFetcher::new(b"unused".to_vec());
    handle.offer_files(
        files_offer(vec![RemoteFile {
            file_id: "f0".into(),
            path: "folder/leaf.bin".into(),
            size: 6,
            dir: false,
        }]),
        fetcher,
    );

    let path = wait_for_published_path().expect("a file URL must be published");
    assert!(
        path.to_string_lossy().contains("universallink-clip-files"),
        "the published URL must point into the skeleton, got {path:?}"
    );
    // The published top-level element is the directory `folder`.
    assert!(
        path.ends_with("folder"),
        "top-level URL is the root dir, got {path:?}"
    );

    // Teardown: request_exit → release → removeFilePresenter + skeleton removal,
    // with no panic / use-after-free.
    handle.request_exit(0);
    loop_thread.join().unwrap();
}

/// Fill-on-paste: a coordinated read of a leaf triggers
/// `relinquishPresentedItemToReader:`, which runs the whole-clip `fill` once; the
/// empty skeleton leaf ends up holding exactly the pushed bytes. Uses a single
/// top-level file so the published root URL IS the leaf.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "real macOS pasteboard; run manually with --ignored --test-threads=1"]
async fn a_coordinated_read_fills_the_leaf() {
    let _guard = PASTEBOARD_LOCK.lock().await;
    let (handle, _events, loop_thread) = skip_if_unsupported!(spawn_backend!());

    let payload = b"filled by the core push".to_vec();
    let fetcher = FillFetcher::new(payload.clone());
    let probe = fetcher.clone();
    handle.offer_files(
        files_offer(vec![RemoteFile {
            file_id: "f0".into(),
            path: "solo.bin".into(),
            size: payload.len() as u64,
            dir: false,
        }]),
        fetcher,
    );

    let path = wait_for_published_path().expect("a file URL must be published");
    assert_eq!(
        std::fs::read(&path).unwrap().len(),
        0,
        "the skeleton leaf starts empty"
    );

    // A coordinated read blocks until the presenter's relinquish (and thus the
    // fill) completes. Run it on a worker thread to keep the parity with a real
    // reader; the accessor block itself is a no-op (we assert on disk after).
    let read_path = path.clone();
    let reader = thread::spawn(move || {
        let coordinator = NSFileCoordinator::new();
        let url = NSURL::fileURLWithPath(&NSString::from_str(&read_path.to_string_lossy()));
        let accessor = RcBlock::new(|_new_url: NonNull<NSURL>| {});
        let mut error: Option<Retained<NSError>> = None;
        coordinator.coordinateReadingItemAtURL_options_error_byAccessor(
            &url,
            NSFileCoordinatorReadingOptions(0),
            Some(&mut error),
            &accessor,
        );
    });
    reader.join().unwrap();

    assert_eq!(
        std::fs::read(&path).unwrap(),
        payload,
        "the coordinated read must have filled the leaf via fill()"
    );
    assert_eq!(
        probe.calls.load(Ordering::SeqCst),
        1,
        "exactly one whole-clip fill"
    );

    handle.request_exit(0);
    loop_thread.join().unwrap();
}
