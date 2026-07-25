// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

//! Keeping the process alive for exactly as long as there is work.
//!
//! Android promises a backgrounded app nothing, and this OEM (OnePlus) cuts a
//! backgrounded app's outbound network within ~2s — see `KeepAlive.kt`. So the
//! app runs a foreground service, and the question this file answers is *when*.
//!
//! The service follows the WORK, not the window. Two things fall out of that:
//! work outlives the app leaving the foreground — a 300 MiB send finished with the
//! app behind HOME and an `am kill` aimed at it — and the shade stays clean while
//! the app is merely open (in the foreground the process is already privileged;
//! there is nothing to protect). What it does not buy is surviving a swipe out of
//! the Recents list: this OEM answers that gesture by killing the process, service
//! or no service (`KeepAlive.kt` has the measurement).
//!
//! Which means the work has to be tracked where it is KNOWN, and that is here
//! rather than in the frontend: the webview dies with the window, and a transfer
//! outliving the window is the entire point. So this listens to the same
//! `core:notification` stream the bridge relays to the webview (gui/src/bridge.rs)
//! and follows `transfer.*` from the Rust side. Two holds cannot be read off that
//! stream and are declared instead: a text share being pushed ([`Hold::share`],
//! taken by the share worker), and the two the frontend declares through
//! [`keep_alive`] because no Core event marks their beginning — a round-trip
//! through the browser (a login, a revocation's re-authorization) and a
//! `files.send` still waiting to be accepted.
//!
//! Every hold is bounded. `transfer.*` ends on `finished`/`failed`, which the Core
//! guarantees; a lost connection drops the holds whose ending we can no longer
//! hear; and each declared hold, whose ending nothing announces if the user
//! simply closes the browser tab, expires on its own TTL. A hold that leaked would be a
//! notification the user cannot get rid of, and a phone that never sleeps.

use std::collections::HashSet;
use std::sync::{LazyLock, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use serde_json::Value;
use tauri::{AppHandle, Listener};

/// Kinds of work, mirrored by `KeepAlive.kt`. Ordered by priority: several can
/// overlap and the notification can only name one.
const NONE: i32 = 0;
const AUTH: i32 = 1;
const SHARE: i32 = 2;
const TRANSFER: i32 = 3;

/// How long a browser round-trip may hold the process up on its own.
///
/// The Core's own flow gives up at `FLOW_TIMEOUT` (300 s, core/src/login.rs) and
/// then has an enrollment to do (30 s), so a login still worth protecting cannot
/// outlast this. Past it the hold is dropped: a user who closed the tab produces
/// no event at all, and nothing else would ever clear it.
const AUTH_TTL: Duration = Duration::from_secs(360);

/// How long a send may hold the process up before `transfer.started` takes over.
/// The Core's own budget for getting a transfer off the ground (peer connect +
/// attestation) is bounded well inside this; past it, the send failed and said so.
const SEND_TTL: Duration = Duration::from_secs(30);

#[derive(Default)]
struct Work {
    /// Transfers seen starting and not yet seen ending — incoming ones included:
    /// receiving files is work this process must stay up for too.
    transfers: HashSet<String>,
    /// Shares being pushed. A count rather than a flag: nothing in the API says
    /// they cannot overlap, and a flag would be released by the first one to end.
    shares: u32,
    /// When the browser round-trip's hold expires, if one is up.
    auth: Option<Instant>,
    /// When the hold on a send being ACCEPTED expires, if one is up. It covers
    /// the gap between the user's tap and `transfer.started`, where the transfer
    /// does not exist yet and so cannot hold anything up itself.
    send: Option<Instant>,
    /// The last kind handed to Kotlin. `None` until the seam has been told
    /// anything, so the first answer is always published — including `NONE`,
    /// which makes our state authoritative over whatever the app was doing before.
    published: Option<i32>,
}

static WORK: LazyLock<Mutex<Work>> = LazyLock::new(|| Mutex::new(Work::default()));

fn work() -> MutexGuard<'static, Work> {
    WORK.lock().expect("lock the keep-alive state")
}

/// Recomputes what the process is being held up for and tells Kotlin — but only
/// on a change, since each call crosses into the JVM and starts or stops a
/// service.
///
/// The JNI call happens UNDER the lock: it is what serializes it against a
/// concurrent hold, without which two threads could publish in one order and
/// call in the other, leaving the service in the state neither of them wanted.
/// Kotlin never calls back into us here, so there is nothing to deadlock on.
fn publish(work: &mut Work) {
    let now = Instant::now();
    let live = |deadline: Option<Instant>| deadline.is_some_and(|d| now < d);
    let kind = if !work.transfers.is_empty() || live(work.send) {
        TRANSFER
    } else if work.shares > 0 {
        SHARE
    } else if live(work.auth) {
        AUTH
    } else {
        NONE
    };
    if work.published == Some(kind) {
        return;
    }
    work.published = Some(kind);
    tracing::info!(
        kind,
        transfers = work.transfers.len(),
        shares = work.shares,
        "keep-alive"
    );
    bridge::set_work(kind);
}

/// Starts watching the Core's stream. Called before the bridge connects, so no
/// transfer can start unseen.
pub fn watch(app: &AppHandle) {
    app.listen("core:notification", |event| note(event.payload()));
    app.listen("core:connection", |event| connection(event.payload()));
}

/// One relayed Core notification (`{"method":…,"params":…}`).
fn note(payload: &str) {
    let Ok(event) = serde_json::from_str::<Value>(payload) else {
        return;
    };
    let params = &event["params"];
    let mut work = work();
    match event["method"].as_str().unwrap_or_default() {
        "transfer.started" | "transfer.incoming" => match params["transfer_id"].as_str() {
            Some(id) => {
                work.transfers.insert(id.to_owned());
            }
            None => return,
        },
        "transfer.finished" | "transfer.failed" => match params["transfer_id"].as_str() {
            Some(id) => {
                work.transfers.remove(id);
            }
            None => return,
        },
        // A session transition. The auth hold is done when there is nothing left
        // to protect (no session) or when a login has fully LANDED: the server
        // session up, which is the step the user is waiting for and the one that
        // comes after enrollment (core: login.rs `finish_login` then
        // `start_session_task`). The states in between are steps along the way —
        // letting go there would leave the Core opening its WebSocket against
        // the OEM's power manager, with the app still behind the browser.
        "session.changed" => {
            let landed = !params["logged_in"].as_bool().unwrap_or(false)
                || params["server_connected"].as_bool().unwrap_or(false);
            if !landed {
                return;
            }
            work.auth = None;
        }
        // The end of the OTHER browser round-trip: a revocation the server made
        // us re-authorize has gone through. Any device's removal clears the hold,
        // which at worst ends it early on an unrelated one — a hold that outstays
        // its flow is the failure that matters, and [`AUTH_TTL`] is its backstop.
        "device.removed" => work.auth = None,
        _ => return,
    }
    publish(&mut work);
}

/// One connection status from the bridge (`core:connection`).
fn connection(payload: &str) {
    let Ok(status) = serde_json::from_str::<Value>(payload) else {
        return;
    };
    if status["status"].as_str() == Some("connected") {
        return;
    }
    let mut work = work();
    if work.transfers.is_empty() {
        return;
    }
    // The Core is out of reach: the `finished`/`failed` we are waiting for will
    // not be replayed when it comes back, so waiting for one would hold the
    // process up for good. The transfer itself is the Core's business, not ours.
    tracing::warn!(
        transfers = work.transfers.len(),
        "the Core went away mid-transfer; dropping its keep-alive holds"
    );
    work.transfers.clear();
    publish(&mut work);
}

/// A hold on the process for as long as this value lives.
pub struct Hold;

impl Hold {
    /// Held while a shared text is being pushed: the share sheet has let go of
    /// the app by then (the user is back in whatever they shared FROM), so the
    /// announce and its delivery report run in the background.
    pub fn share() -> Self {
        let mut work = work();
        work.shares += 1;
        publish(&mut work);
        Hold
    }
}

impl Drop for Hold {
    fn drop(&mut self) {
        let mut work = work();
        work.shares = work.shares.saturating_sub(1);
        publish(&mut work);
    }
}

/// The two holds the frontend has to declare, because no Core event marks their
/// beginning — see [`keep_alive`].
#[derive(Clone, Copy)]
enum Declared {
    /// A round-trip through the browser: signing in (`session.login`) or
    /// re-authorizing a revocation (`devices.revoke` → `reauth_required`). Both
    /// come back INTO this process, to the Core's loopback listener, while
    /// Android has the app backgrounded behind the browser.
    Auth,
    /// A `files.send` waiting to be accepted. Until `transfer.started` there is
    /// no transfer to hold anything up, and the user has every reason to pocket
    /// the phone the moment they have tapped.
    Send,
}

impl Declared {
    fn parse(kind: &str) -> Option<Self> {
        match kind {
            "auth" => Some(Self::Auth),
            "send" => Some(Self::Send),
            _ => None,
        }
    }

    fn ttl(self) -> Duration {
        match self {
            Self::Auth => AUTH_TTL,
            Self::Send => SEND_TTL,
        }
    }

    fn slot(self, work: &mut Work) -> &mut Option<Instant> {
        match self {
            Self::Auth => &mut work.auth,
            Self::Send => &mut work.send,
        }
    }
}

/// A declared piece of work is starting (`on`) or over (`off`) — the frontend's
/// side of the keep-alive, for the two things this file cannot read off the Core's
/// event stream (see [`Declared`]).
///
/// Each hold is bounded by its own TTL whatever the frontend does next, including
/// dying with its window. `on: false` is therefore an optimization of the truth,
/// never the only thing standing between us and a stuck notification.
///
/// Mobile-only, like the share commands: the desktop shell does not register the
/// command, and a rejection there IS the answer.
#[tauri::command]
pub fn keep_alive(kind: String, on: bool) {
    let Some(declared) = Declared::parse(&kind) else {
        tracing::error!(kind, "unknown keep-alive kind");
        return;
    };
    {
        let mut work = work();
        *declared.slot(&mut work) = on.then(|| Instant::now() + declared.ttl());
        publish(&mut work);
    }
    if on {
        tauri::async_runtime::spawn(async move {
            tokio::time::sleep(declared.ttl()).await;
            expire(declared);
        });
    }
}

/// Drops a declared hold if it is the expired one — a newer hold has its own timer.
fn expire(declared: Declared) {
    let mut work = work();
    let slot = declared.slot(&mut work);
    if !slot.is_some_and(|deadline| Instant::now() >= deadline) {
        return;
    }
    *slot = None;
    tracing::warn!("a declared hold reached its limit; letting go");
    publish(&mut work);
}

/// Publishes the current state again, whatever it is. Used when the seam comes up
/// after the state has already moved — a share can be under way before Kotlin has
/// wired us (see `KeepAlive.attach`).
#[cfg_attr(not(target_os = "android"), allow(dead_code))]
fn republish() {
    let mut work = work();
    work.published = None;
    publish(&mut work);
}

/// The Rust → Kotlin end of the seam (`KeepAlive.setWork`).
#[cfg(target_os = "android")]
mod bridge {
    use std::sync::OnceLock;

    use tauri::tao::platform::android::prelude::{GlobalRef, JClass, JNIEnv, jni};

    /// The JVM and the `KeepAlive` class, captured from the one call Kotlin makes
    /// into us. Both are needed, and the class especially: `FindClass` on a thread
    /// we attached ourselves resolves against the BOOTSTRAP class loader, which
    /// cannot see the app's classes. The reference therefore has to be taken on a
    /// Java thread, which is exactly what `nativeInit` is.
    static SEAM: OnceLock<(jni::JavaVM, GlobalRef)> = OnceLock::new();

    /// `org.universallink.mobile.KeepAlive.nativeInit` — see `KeepAlive.kt`.
    #[unsafe(no_mangle)]
    pub extern "system" fn Java_org_universallink_mobile_KeepAlive_nativeInit(
        env: JNIEnv<'_>,
        class: JClass<'_>,
    ) {
        // A panic unwinding back into the JVM is undefined behaviour.
        let wired = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let (Ok(vm), Ok(class)) = (env.get_java_vm(), env.new_global_ref(&class)) else {
                tracing::error!("the keep-alive seam could not be captured");
                return;
            };
            if SEAM.set((vm, class)).is_err() {
                // A second activity: the seam is already up and still valid (the
                // class outlives any activity).
                return;
            }
            // The state may already have moved: a share can start the app.
            super::republish();
        }));
        if wired.is_err() {
            tracing::error!("panic while wiring the keep-alive seam");
        }
    }

    /// Tells Kotlin what the process is being held up for. Any thread.
    pub fn set_work(kind: i32) {
        let Some((vm, class)) = SEAM.get() else {
            // Before `KeepAlive.attach`. Nothing is lost: `nativeInit` republishes.
            tracing::debug!(kind, "no keep-alive seam yet");
            return;
        };
        let mut env = match vm.attach_current_thread() {
            Ok(env) => env,
            Err(e) => {
                tracing::error!(error = %e, "could not attach to the JVM");
                return;
            }
        };
        let call =
            env.call_static_method(class, "setWork", "(I)V", &[jni::objects::JValue::Int(kind)]);
        if let Err(e) = call {
            tracing::error!(error = %e, kind, "KeepAlive.setWork failed");
            // A pending exception poisons every later JNI call on this thread.
            let _ = env.exception_clear();
        }
    }
}

/// A host build (`cargo check` during development) has no JVM to talk to.
#[cfg(not(target_os = "android"))]
mod bridge {
    pub fn set_work(kind: i32) {
        let _ = kind;
    }
}
