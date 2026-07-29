// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

//! Reading a pairing code off another device's screen, with the camera.
//!
//! This is the gesture the whole pairing flow is shaped around: a desktop shows
//! a QR code, the phone reads it. Which END of the exchange the phone turns out
//! to be is not decided here and not decided by the gesture — the Core derives
//! it from what each device can do (doc/core-api.md, "Pairing"), so *reading a
//! code* covers both directions: a phone joining an account, and a phone that
//! already holds the account key vouching for a new PC. One button, both jobs.
//!
//! Two seams meet here, in the opposite order to `share`'s.
//!
//! **Rust → Kotlin.** [`scan_code`] is a command the frontend calls, and only
//! Kotlin can put a camera on screen: `ScanBridge.startScan` opens
//! `ScanActivity`, which does the decoding (CameraX + zxing — see `ScanBridge.kt`
//! for why nothing proprietary is used for it, and what that buys).
//!
//! **Kotlin → Rust.** `ScanBridge.onScanResult` hands back one JSON object per
//! scan, and [`normalize`] decides what the frontend gets to see of it.
//!
//! # Two things that are deliberately absent
//!
//! **No keep-alive hold** (`keepalive`). The scanner is an activity of THIS app,
//! in this task and this process, so the app never stops being the one in front —
//! unlike a browser round-trip or a share, which are what holds exist for. Had
//! the scanner been Google's (`play-services-code-scanner`, the obvious
//! alternative), its UI would run in the Play Services process, our app would be
//! backgrounded, and on this OEM its network would be cut mid-pairing.
//!
//! **No camera work of our own.** Nothing in this file touches a frame or a
//! permission: an activity is opened, a string comes back. That is what keeps the
//! JNI surface here to three symbols.

use std::sync::Mutex;
use std::time::Duration;

use serde_json::{Value, json};
use tokio::sync::oneshot;

/// How long a scan may stay unanswered before the command gives up on it.
///
/// A backstop, not a policy: `ScanActivity` reports an outcome on every path it
/// can be destroyed through, so this can only fire if that failed — or if a
/// human has held the camera up for five minutes without finding a code. Either
/// way the command has to answer, because the frontend disarms the button until
/// it does.
const BACKSTOP: Duration = Duration::from_secs(300);

/// The scan waiting for its result. At most one: a second scanner on top of the
/// first would be two windows fighting over one camera.
static PENDING: Mutex<Option<oneshot::Sender<Value>>> = Mutex::new(None);

/// What a code must start with to be one of ours — from the Core's own constant,
/// so the format lives in exactly one place. The scanner is handed this and keeps
/// looking until it sees it: a camera pointed at a screen may well have somebody
/// else's QR code in view (a page, a sticker, a wifi code), and stopping on the
/// first one it decodes would take the user to a dead end.
fn tag() -> String {
    format!("{}:", universallink_core::PAIRING_CODE_TAG)
}

/// Whether this device can read a code at all: the mobile shell, and a camera.
///
/// The desktop shell does not register this command — a rejection there IS the
/// answer "no scanner here" (gui/ui/src/lib/core.ts), the same contract as the
/// share commands. On Android it answers for the device rather than the platform:
/// a tablet or a TV with no camera says no, and the button is never offered.
#[tauri::command]
pub fn scan_supported() -> bool {
    bridge::has_camera()
}

/// Reads one code, or says why not.
///
/// Never fails as a command: every outcome is a word the frontend has a sentence
/// for — `{"code": …}`, or a `reason` of `cancelled` (the user backed out, which
/// deserves no message at all), `denied`, `no_camera`, `busy` or `failed`.
#[tauri::command]
pub async fn scan_code() -> Value {
    let (tx, rx) = oneshot::channel();
    {
        let mut pending = PENDING.lock().expect("lock the pending scan");
        // `is_closed` is what makes a scan the backstop gave up on collectable: a
        // sender whose receiver is gone holds the slot for nobody.
        if pending.as_ref().is_some_and(|tx| !tx.is_closed()) {
            return json!({ "reason": "busy" });
        }
        *pending = Some(tx);
    }
    if !bridge::start_scan(&tag()) {
        // No window to open the scanner from, or the JVM refused the call.
        clear();
        return json!({ "reason": "failed" });
    }
    match tokio::time::timeout(BACKSTOP, rx).await {
        Ok(Ok(outcome)) => outcome,
        // The backstop fired, or the sender was dropped without a result.
        _ => {
            tracing::warn!("a scan ended without an outcome");
            clear();
            json!({ "reason": "failed" })
        }
    }
}

/// Frees the slot. Not `take()`-and-send: the caller is the one giving up.
fn clear() {
    *PENDING.lock().expect("lock the pending scan") = None;
}

/// `org.universallink.mobile.ScanBridge.onScanResult` — see `ScanBridge.kt`.
///
/// Called from Kotlin's main thread, once per scan.
#[cfg(target_os = "android")]
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_universallink_mobile_ScanBridge_onScanResult(
    mut env: tauri::tao::platform::android::prelude::JNIEnv<'_>,
    _class: tauri::tao::platform::android::prelude::JClass<'_>,
    json: tauri::tao::platform::android::prelude::JString<'_>,
) {
    // A panic unwinding back into the JVM is undefined behaviour.
    let taken = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        match env.get_string(&json) {
            Ok(s) => report(&String::from(s)),
            Err(e) => {
                tracing::error!(error = %e, "unreadable scan result");
                // Still an answer: the caller is waiting on one.
                report(r#"{"reason":"failed"}"#);
            }
        }
    }));
    if taken.is_err() {
        tracing::error!("panic while taking a scan result; the scan was dropped");
    }
}

/// Hands one reported outcome to whoever is waiting for it.
///
/// An outcome nobody is waiting for is dropped, and that is the normal ending of
/// a scan the backstop gave up on: the command has already answered, and the
/// scanner the user eventually closes has no one to report to. Handing it to the
/// NEXT scan instead would answer a fresh gesture with a stale result.
fn report(json: &str) {
    let outcome = normalize(json, &tag());
    let taken = PENDING.lock().expect("lock the pending scan").take();
    match taken {
        Some(tx) => {
            let _ = tx.send(outcome);
        }
        None => tracing::warn!(outcome = %outcome, "a scan result arrived with nobody waiting"),
    }
}

/// What the frontend gets to see of Kotlin's report.
///
/// Fail-closed on every field: the alternative to a word we know is `failed`,
/// never a guess. The payload comes from our own Kotlin, so anything else here is
/// a bug on that side — and one that must not travel any further than this
/// function, because the next stop for a code is `pairing.accept`.
fn normalize(json: &str, tag: &str) -> Value {
    let failed = || json!({ "reason": "failed" });
    let Ok(reported) = serde_json::from_str::<Value>(json) else {
        tracing::error!(json, "a scan reported something that is not JSON");
        return failed();
    };
    if let Some(code) = reported["code"].as_str() {
        if !code.starts_with(tag) {
            // The scanner filters on this tag itself; a code that reaches here
            // got past that filter, and `pairing.accept` is not the place to
            // find that out.
            tracing::error!("a scanned code did not carry the pairing tag");
            return failed();
        }
        return json!({ "code": code });
    }
    match reported["reason"].as_str() {
        Some(reason @ ("cancelled" | "denied" | "no_camera" | "failed")) => {
            json!({ "reason": reason })
        }
        _ => {
            tracing::error!(json, "a scan reported an outcome we have no word for");
            failed()
        }
    }
}

/// The Rust ↔ Kotlin ends of the seam (`ScanBridge`).
#[cfg(target_os = "android")]
mod bridge {
    use std::sync::OnceLock;
    use std::sync::atomic::{AtomicBool, Ordering};

    use tauri::tao::platform::android::prelude::{GlobalRef, JClass, JNIEnv, jni};

    /// The JVM and the `ScanBridge` class, captured from the one call Kotlin
    /// makes into us. The class reference has to be taken on a JAVA thread:
    /// `FindClass` from a thread we attached ourselves resolves against the
    /// bootstrap class loader, which cannot see the app's classes — the same
    /// reason [`crate::keepalive`]'s seam is wired the same way.
    static SEAM: OnceLock<(jni::JavaVM, GlobalRef)> = OnceLock::new();

    /// Whether the device has a camera, as Kotlin found it (`PackageManager`).
    static CAMERA: AtomicBool = AtomicBool::new(false);

    /// `org.universallink.mobile.ScanBridge.nativeInit` — see `ScanBridge.kt`.
    #[unsafe(no_mangle)]
    pub extern "system" fn Java_org_universallink_mobile_ScanBridge_nativeInit(
        env: JNIEnv<'_>,
        class: JClass<'_>,
        has_camera: jni::sys::jboolean,
    ) {
        // A panic unwinding back into the JVM is undefined behaviour.
        let wired = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let (Ok(vm), Ok(class)) = (env.get_java_vm(), env.new_global_ref(&class)) else {
                tracing::error!("the scanner seam could not be captured");
                return;
            };
            CAMERA.store(has_camera != 0, Ordering::Relaxed);
            // A second activity: the seam is already up and still valid (the
            // class outlives any activity, and the camera has not moved).
            let _ = SEAM.set((vm, class));
        }));
        if wired.is_err() {
            tracing::error!("panic while wiring the scanner seam");
        }
    }

    /// Whether a scan can be offered: a camera, and a seam to reach it through.
    pub fn has_camera() -> bool {
        SEAM.get().is_some() && CAMERA.load(Ordering::Relaxed)
    }

    /// Opens the scanner. Any thread. `false` means it never started, so no
    /// result is coming.
    pub fn start_scan(tag: &str) -> bool {
        let Some((vm, class)) = SEAM.get() else {
            tracing::error!("no scanner seam yet");
            return false;
        };
        let mut env = match vm.attach_current_thread() {
            Ok(env) => env,
            Err(e) => {
                tracing::error!(error = %e, "could not attach to the JVM");
                return false;
            }
        };
        let Ok(tag) = env.new_string(tag) else {
            tracing::error!("could not pass the pairing tag to the JVM");
            return false;
        };
        let started = env.call_static_method(
            class,
            "startScan",
            "(Ljava/lang/String;)Z",
            &[jni::objects::JValue::Object(&tag)],
        );
        match started.and_then(|value| value.z()) {
            Ok(started) => started,
            Err(e) => {
                tracing::error!(error = %e, "ScanBridge.startScan failed");
                // A pending exception poisons every later JNI call on this thread.
                let _ = env.exception_clear();
                false
            }
        }
    }
}

/// A host build (`cargo check` during development) has no JVM to talk to, and no
/// camera either — which is the honest answer for it.
#[cfg(not(target_os = "android"))]
mod bridge {
    pub fn has_camera() -> bool {
        false
    }

    pub fn start_scan(tag: &str) -> bool {
        let _ = tag;
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TAG: &str = "UL1:";
    const CODE: &str = "UL1:aaaa:bbbb:p_1";

    #[test]
    fn a_code_carrying_the_tag_is_passed_on_as_it_stands() {
        assert_eq!(
            normalize(&format!(r#"{{"code":"{CODE}"}}"#), TAG),
            json!({ "code": CODE })
        );
    }

    // The one check that cannot be left to the Core: a foreign QR code read by
    // mistake must not be sent off as a pairing code.
    #[test]
    fn a_code_without_the_tag_is_not_a_code() {
        for foreign in [
            "https://example.test/",
            "WIFI:S:home;T:WPA;P:hunter2;;",
            "UL2:aaaa:bbbb:p_1",
            "ul1:aaaa:bbbb:p_1",
            "",
        ] {
            let reported = json!({ "code": foreign }).to_string();
            assert_eq!(
                normalize(&reported, TAG),
                json!({ "reason": "failed" }),
                "{foreign}"
            );
        }
    }

    #[test]
    fn each_reason_the_frontend_knows_survives_the_crossing() {
        for reason in ["cancelled", "denied", "no_camera", "failed"] {
            let reported = json!({ "reason": reason }).to_string();
            assert_eq!(normalize(&reported, TAG), json!({ "reason": reason }));
        }
    }

    // A word from a later Kotlin, a truncated payload, an empty object: the
    // frontend is told the scan failed rather than handed something it has no
    // sentence for.
    #[test]
    fn anything_else_is_a_failure_rather_than_a_guess() {
        for reported in [
            r#"{"reason":"jammed"}"#,
            r#"{"reason":42}"#,
            r#"{"code":null}"#,
            "{}",
            "not json at all",
            "",
        ] {
            assert_eq!(
                normalize(reported, TAG),
                json!({ "reason": "failed" }),
                "{reported}"
            );
        }
    }

    // The tag comes from the Core, and it is the tag of a CODE, not of a family
    // of them: `UL10:` must not pass for `UL1:`.
    #[test]
    fn the_tag_is_the_one_the_core_mints() {
        assert_eq!(tag(), "UL1:");
        assert_eq!(
            normalize(r#"{"code":"UL10:aaaa:bbbb:p_1"}"#, &tag()),
            json!({ "reason": "failed" })
        );
    }

    // The waiting slot, whose whole job is that a button comes back to life. On a
    // host build there is no seam, so `scan_code` takes the slot and gives it
    // straight back — which is exactly the path these tests need.
    fn block_on<F: std::future::Future>(future: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .expect("build a runtime")
            .block_on(future)
    }

    /// The tests share one static slot: they hold this while they use it.
    static SERIALIZE: Mutex<()> = Mutex::new(());

    fn with_slot(test: impl FnOnce()) {
        let _guard = SERIALIZE.lock().unwrap_or_else(|e| e.into_inner());
        clear();
        test();
        clear();
    }

    // A scan that could not even start must not leave the slot taken: the next
    // gesture would be answered `busy` by a scanner that never opened.
    #[test]
    fn a_scan_that_never_started_frees_its_slot() {
        with_slot(|| {
            assert_eq!(block_on(scan_code()), json!({ "reason": "failed" }));
            assert!(
                PENDING.lock().expect("lock").is_none(),
                "the slot was left taken"
            );
        });
    }

    // A scanner is already up: the second caller is told so, and — the point of
    // the test — the first one's sender is left alone. Overwriting it would leave
    // that caller waiting out the backstop with the camera in front of them.
    #[test]
    fn a_second_scan_leaves_the_first_alone() {
        with_slot(|| {
            let (tx, mut rx) = oneshot::channel();
            *PENDING.lock().expect("lock") = Some(tx);

            assert_eq!(block_on(scan_code()), json!({ "reason": "busy" }));

            assert!(rx.try_recv().is_err(), "the first scan was answered");
            report(r#"{"code":"UL1:a:b:p_1"}"#);
            assert_eq!(
                block_on(rx).expect("the first scan still had its sender"),
                json!({ "code": "UL1:a:b:p_1" })
            );
        });
    }

    // ...whereas a scan the backstop gave up on holds the slot for nobody, and
    // must not jam the button for the rest of the app's life.
    #[test]
    fn a_scan_nobody_waits_for_any_more_does_not_hold_the_slot() {
        with_slot(|| {
            let (tx, rx) = oneshot::channel::<Value>();
            drop(rx);
            *PENDING.lock().expect("lock") = Some(tx);

            // `failed` and not `busy`: this scan got the slot.
            assert_eq!(block_on(scan_code()), json!({ "reason": "failed" }));
        });
    }

    // The other half of that: the outcome such a scan eventually reports is
    // dropped rather than kept for the next gesture, which would answer a fresh
    // scan with a stale code.
    #[test]
    fn an_outcome_nobody_waits_for_is_dropped() {
        with_slot(|| {
            report(r#"{"code":"UL1:a:b:p_1"}"#);
            assert!(PENDING.lock().expect("lock").is_none());
        });
    }
}
