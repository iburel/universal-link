// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

//! The Android **share sheet**, wired to the account's clipboard.
//!
//! Selecting text anywhere on the phone → Share → 1Device copies it to
//! every other device of the account, ready to paste. There is no OS clipboard
//! to watch here (v1 shares on an explicit gesture, and Android grants no
//! background clipboard read anyway), so this component is a *source only*: it
//! announces, it never serves a paste.
//!
//! Two seams meet in this file.
//!
//! **Kotlin → Rust.** The share intent lands in `MainActivity` (Kotlin), which
//! hands the string to [`Java_org_onedevice_mobile_ShareBridge_onShareText`].
//! tao *also* parses `ACTION_SEND` `text/plain` into a `RunEvent::Opened`, which
//! would need no JNI at all — but it wraps the text in a `data:` URL through the
//! `url` crate, and anything that parses as a URL comes back normalized
//! (`https://example.com` → `https://example.com/`). Sharing a link is the most
//! common Android text share, so that path is deliberately ignored: this seam
//! carries the bytes the user actually shared.
//!
//! **Rust → Core.** A SECOND IPC connection, in role `clipboard-backend` (the
//! exclusive role announcing requires — no clipboard component runs on Android,
//! so it is free), announces the text as a **materialized** transaction
//! (`materialize: true`): the bytes travel with the announce and each device
//! caches them, because this source is expected to be dead by the time anyone
//! pastes (doc/core-api.md — "Materialized transactions"). The Core answers
//! `pushed_to` and then reports the fan-out with `clipboard.pushed`, which is
//! what lets the UI tell the truth about a share instead of assuming it worked.
//!
//! # Files
//!
//! A shared FILE takes a different route, and not by preference: a `content://`
//! URI is a permission grant to this process, not a readable path, and it dies
//! with the activity that received it — while the Core must read the bytes
//! LATER, once the user has chosen a destination. So `ShareFiles.kt` copies them
//! into the app's cache first, and this file publishes the result to the UI,
//! which asks for a destination and calls `files.send` over the GUI bridge (a
//! connection that already carries `files.send` and `transfers.read`, so the
//! transfer shows up in the same cards the desktop uses for a drag-and-drop).
//!
//! Nothing about a file share touches the Core from here. What lives here is the
//! cache's lifecycle: [`register`] adopts a copy and sweeps the ones no one is
//! coming back for, [`discard_share`] drops one the moment the frontend is done
//! with it (destination cancelled, or transfer over).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex, OnceLock};
use std::time::Duration;

use base64::Engine as _;
use serde_json::{Value, json};
use tauri::{AppHandle, Emitter};
use tokio::sync::mpsc;
use onedevice_ipc_client::{Client, ClientConfig, Event, RequestError};

/// The frontend event carrying a share's progress — mirrored by `ShareStatus`
/// in `gui/ui/src/lib/core.ts`. Mobile-only: the desktop shell never emits it.
const SHARE_EVENT: &str = "core:share";

/// Refused before it reaches the Core, which caps a materialized payload at the
/// same 8 MiB (`MATERIALIZE_MAX`): a local check turns "invalid params" into a
/// sentence the user can act on.
const MAX_TEXT: usize = 8 * 1024 * 1024;

/// How long to wait for `clipboard.pushed` after an announce that promised one.
/// The Core's own budget per peer is bounded (connect 15 s + stalls + ack 10 s)
/// and peers are pushed in parallel, so this only has to outlast the slowest
/// one; beyond it the share is reported as unconfirmed rather than hanging.
const REPORT_TIMEOUT: Duration = Duration::from_secs(90);

/// How long a share waits for the Core to become able to push, before giving up.
///
/// This is not defensive padding: a share is normally what STARTED the app, so
/// the queued text is dequeued while the embedded Core is still booting — and
/// announcing too early loses it, twice over. Both were seen on-device:
/// ```text
/// 07:39:36.464  shared text received bytes=11
/// 07:39:36.638  embedded Core listening
/// 07:39:36.639  the share was refused error=no connection to the Core   <- IPC not up
/// 07:39:36.654  share connection ready
///
/// 07:49:xx      share announced targets=0                               <- directory not up
/// ```
/// The first is `Client::request` being fail-closed (nothing is queued on our
/// behalf). The second is worse because it looks like success: until the Core's
/// server session has fetched the device directory, `account_peers` is empty, so
/// the announce reaches nobody and reports `pushed_to: 0` — indistinguishable
/// from "this account has no other device". Hence [`wait_ready`], which waits for
/// both. The budget has to cover a cold radio doing DNS + TLS + WebSocket + token
/// refresh + enrollment challenge + `devices.list`.
const READY_WAIT: Duration = Duration::from_secs(30);

/// Interval between `session.status` reads while waiting. Polling rather than
/// subscribing to the `session` topic on purpose: the wait happens once, lasts
/// seconds, and a snapshot read has no ordering subtleties against a concurrent
/// `session.changed` — an announce sent one notification too early is exactly
/// the bug this guards.
const READY_POLL: Duration = Duration::from_millis(400);

/// Announce attempts. Only a `NotConnected` refusal is retried: the client
/// guarantees nothing was sent, so a retry cannot announce twice. A
/// `Disconnected` one (fate unknown) is not, since a second announce would push
/// the bytes again for an outcome the user could not tell apart.
const ANNOUNCE_ATTEMPTS: u32 = 2;

/// One gesture from the share sheet.
enum Share {
    /// Text to announce to the whole account (materialized clipboard).
    Text(String),
    /// A file share whose bytes are already in the cache: only its status is
    /// left to publish, the destination being the frontend's business.
    Files(Value),
}

type Shares = mpsc::UnboundedReceiver<Share>;

/// Shares, queued between the JVM thread that receives the intent and the
/// worker that acts on them. Created on first touch from EITHER side: on a
/// cold share the intent can reach us while the Core is still booting, and
/// dropping the user's share because we were not ready yet is not an option.
static QUEUE: OnceLock<(mpsc::UnboundedSender<Share>, Mutex<Option<Shares>>)> = OnceLock::new();

fn queue() -> &'static (mpsc::UnboundedSender<Share>, Mutex<Option<Shares>>) {
    QUEUE.get_or_init(|| {
        let (tx, rx) = mpsc::unbounded_channel();
        (tx, Mutex::new(Some(rx)))
    })
}

/// Takes a shared text from the share sheet. Never blocks and never fails: the
/// queue is unbounded and drained by the worker as soon as it exists.
fn submit_text(text: String) {
    if text.is_empty() {
        return;
    }
    tracing::info!(bytes = text.len(), "shared text received");
    let _ = queue().0.send(Share::Text(text));
}

/// `org.onedevice.mobile.ShareBridge.onShareText` — see `ShareBridge.kt`.
///
/// The JVM calls this on its own thread, possibly before the Tauri setup has
/// run; nothing here touches app state, only the queue.
#[cfg(target_os = "android")]
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_onedevice_mobile_ShareBridge_onShareText(
    mut env: tauri::tao::platform::android::prelude::JNIEnv<'_>,
    _class: tauri::tao::platform::android::prelude::JClass<'_>,
    text: tauri::tao::platform::android::prelude::JString<'_>,
) {
    // A panic unwinding back into the JVM is undefined behaviour: the share is
    // dropped with a log instead of taking the app down.
    let taken = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        match env.get_string(&text) {
            Ok(s) => submit_text(String::from(s)),
            Err(e) => tracing::error!(error = %e, "unreadable shared text"),
        }
    }));
    if taken.is_err() {
        tracing::error!("panic while taking a shared text; it was dropped");
    }
}

/// `org.onedevice.mobile.ShareBridge.onShareFiles` — see `ShareFiles.kt`.
///
/// The payload is JSON rather than a flat argument list because a file share has
/// three shapes (see [`file_share_status`]), and one seam that carries its own
/// structure beats three externs that have to stay in step with each other.
///
/// Always called from Kotlin's single copy worker, never from the UI thread —
/// every shape reaches disk here (even `preparing`, which drops a superseded
/// share's copy), and that worker being single is what makes the sweep in
/// [`register`] safe.
#[cfg(target_os = "android")]
#[unsafe(no_mangle)]
pub extern "system" fn Java_org_onedevice_mobile_ShareBridge_onShareFiles(
    mut env: tauri::tao::platform::android::prelude::JNIEnv<'_>,
    _class: tauri::tao::platform::android::prelude::JClass<'_>,
    json: tauri::tao::platform::android::prelude::JString<'_>,
) {
    // A panic unwinding back into the JVM is undefined behaviour.
    let taken = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        match env.get_string(&json) {
            Ok(s) => submit_files(&String::from(s)),
            Err(e) => tracing::error!(error = %e, "unreadable file share"),
        }
    }));
    if taken.is_err() {
        tracing::error!("panic while taking a file share; it was dropped");
    }
}

/// Queues a file share for publication, after validating it and adopting its
/// cache copy.
fn submit_files(json: &str) {
    match file_share_status(json) {
        Some(status) => {
            tracing::info!(status = %status, "file share received");
            let _ = queue().0.send(Share::Files(status));
        }
        // The payload comes from our own Kotlin, so this is a programming error,
        // not a user-facing one — and a status the UI could not act on is worse
        // than none.
        None => tracing::error!(json, "a file share was dropped: unusable payload"),
    }
}

/// Validates a file share from `ShareFiles.kt` and returns the status to publish.
/// Fail-closed on every field: a share whose paths we cannot vouch for is not
/// handed to the UI, which would offer to send it.
///
/// The three shapes, in the order they occur:
/// ```text
/// {"phase":"preparing","files":2}
/// {"phase":"pick","dir":"…/shares/<id>","files":[{"path":…,"name":…,"size":…}]}
/// {"phase":"failed","reason":"unreadable"|"no_space"}
/// ```
fn file_share_status(json: &str) -> Option<Value> {
    let payload: Value = serde_json::from_str(json).ok()?;
    match payload["phase"].as_str()? {
        "preparing" => {
            // A new share supersedes one still waiting for a destination: its
            // copy is bytes the user has already moved on from.
            discard_pending();
            Some(json!({ "phase": "preparing", "files": payload["files"].as_u64()? }))
        }
        "failed" => {
            // Kotlin's vocabulary, narrowed to what the UI has words for.
            let reason = match payload["reason"].as_str().unwrap_or_default() {
                "no_space" => "no_space",
                _ => "unreadable",
            };
            Some(json!({ "phase": "error", "reason": reason }))
        }
        "pick" => {
            let dir = PathBuf::from(payload["dir"].as_str()?);
            // The share's id IS its directory's name: one string identifies the
            // share for the frontend and locates its bytes for us.
            let id = dir.file_name()?.to_str()?.to_owned();
            let mut files = Vec::new();
            for entry in payload["files"].as_array()? {
                let path = entry["path"].as_str()?;
                let name = entry["name"].as_str()?;
                let size = entry["size"].as_u64()?;
                // These paths are handed to the Core to read: they must be the
                // copies we just wrote, inside this share's own directory.
                if Path::new(path).parent() != Some(dir.as_path()) {
                    tracing::error!(path, "a shared file lies outside its share directory");
                    return None;
                }
                // And they must still BE there, at that size. Offering a
                // destination for bytes that are gone would fail much later, as an
                // opaque Core error, on a share the user believed was ready.
                match std::fs::metadata(path) {
                    Ok(meta) if meta.is_file() && meta.len() == size => {}
                    _ => {
                        tracing::error!(path, size, "a shared file is not the copy we expected");
                        return None;
                    }
                }
                files.push(json!({ "path": path, "name": name, "size": size }));
            }
            if files.is_empty() {
                return None;
            }
            register(&id, &dir);
            Some(json!({ "phase": "pick", "id": id, "files": files }))
        }
        _ => None,
    }
}

/// Cache directories holding shared files, by share id. A share stays here from
/// the moment its copy is complete until the frontend is done with it — which
/// covers the whole transfer, since the Core reads these files as it sends.
static SHARES: LazyLock<Mutex<HashMap<String, PathBuf>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Adopts a share's copy, then sweeps the directories no one is coming back for.
///
/// The sweep is what makes the cache self-limiting: the frontend discards a share
/// as soon as its transfer ends, but an app killed mid-transfer (or a webview
/// that never loaded) leaves its copy behind with nothing left to release it.
/// Anything not in [`SHARES`] is exactly that — a leftover, including everything
/// from a previous run of the app.
///
/// That reading holds only because copies do not overlap: Kotlin runs them on ONE
/// worker (`ShareFiles.worker`), so no directory is being written while this runs.
/// Two copies at once and this would delete the other one's directory mid-write.
fn register(id: &str, dir: &Path) {
    let mut shares = SHARES.lock().expect("lock shares");
    shares.insert(id.to_owned(), dir.to_path_buf());
    let Some(root) = dir.parent() else { return };
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let live = entry
            .file_name()
            .to_str()
            .is_some_and(|name| shares.contains_key(name));
        if !live {
            // Only ever directories in here (Kotlin creates one per share); a
            // stray file is left alone rather than guessed at.
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
}

/// Drops a share's cache copy and forgets it — the frontend calls this when the
/// destination pick is cancelled and when the transfer that was reading it ends.
/// Idempotent: an unknown id is not an error.
#[tauri::command]
pub fn discard_share(id: String) {
    discard(&id);
}

/// The pick has been ANSWERED: forget the question, keep the bytes. The transfer
/// is about to read them, so this cannot delete anything — and it is what keeps
/// [`discard_pending`] honest, by making a retained `pick` mean "still waiting".
#[tauri::command]
pub fn share_taken(id: String) {
    forget(&id);
}

fn discard(id: &str) {
    let dir = SHARES.lock().expect("lock shares").remove(id);
    if let Some(dir) = dir {
        tracing::info!(id, dir = %dir.display(), "dropping a share's copy");
        if let Err(e) = std::fs::remove_dir_all(&dir) {
            tracing::warn!(error = %e, dir = %dir.display(), "could not drop a share's copy");
        }
    }
    forget(id);
}

/// Retires a pick from the retained status: a question already answered (or whose
/// bytes are gone) must not be put to a webview that loads afterwards.
fn forget(id: &str) {
    let mut last = LAST.lock().expect("lock last share status");
    let open = last
        .as_ref()
        .is_some_and(|s| s["phase"] == "pick" && s["id"].as_str() == Some(id));
    if open {
        *last = None;
    }
}

/// Drops the share still waiting for a destination, if any — a new share means
/// the user has moved on from it.
///
/// Deleting here is safe because of [`share_taken`]: the frontend retires a pick
/// the instant it answers one, so a retained `pick` is one no transfer is reading.
/// Without that, this would happily delete the files of a transfer in flight.
fn discard_pending() {
    let pending = LAST
        .lock()
        .expect("lock last share status")
        .as_ref()
        .filter(|s| s["phase"] == "pick")
        .and_then(|s| s["id"].as_str().map(str::to_owned));
    if let Some(id) = pending {
        discard(&id);
    }
}

/// Starts the share worker: one IPC connection in the announcing role, then one
/// share at a time (a human gesture — no concurrency to win here). `gui` is the
/// bridge's own config, whose socket path and token are reused verbatim so the
/// two connections can never drift apart.
pub fn spawn(app: AppHandle, gui: &ClientConfig) {
    let Some(shares) = queue().1.lock().expect("lock share queue").take() else {
        // `spawn` is called once per Core boot; a second call would race the
        // first for the queue and silently halve the shares.
        tracing::error!("the share worker is already running");
        return;
    };
    let config = ClientConfig {
        ipc_path: gui.ipc_path.clone(),
        token: gui.token.clone(),
        name: "1device-share".into(),
        version: env!("CARGO_PKG_VERSION").into(),
        // The exclusive role `clipboard.updated` demands (core: conn.rs
        // `require_clipboard_backend`), with the scope it checks — plus
        // `session.read`, without which the phone cannot know whether an announce
        // would reach anyone at all (see `wait_ready`). No topic: the delivery
        // report is addressed to this connection directly.
        role: "clipboard-backend".into(),
        scopes: vec!["clipboard.write".into(), "session.read".into()],
        topics: vec![],
        optional_topics: Vec::new(),
        // Nothing to serve: a materialized clip is answered from the receiving
        // Core's cache, so `clipboard.get_data` never comes back to us — and if
        // one ever did, the client's automatic -32601 surfaces to the paster as
        // `CLIP_STALE`, which is exactly the truth (we cannot re-read a share).
        served_methods: vec![],
        reconnect_base_delay: Duration::from_secs(1),
        request_timeout: Duration::from_secs(30),
    };
    tauri::async_runtime::spawn(async move { run(app, config, shares).await });
}

async fn run(app: AppHandle, config: ClientConfig, mut shares: Shares) {
    let (client, events) = onedevice_ipc_client::spawn(config);
    // The delivery reports arrive as notifications on the same connection,
    // interleaved with its lifecycle events: split them off so a share can wait
    // for its own report without also owning the event stream.
    let (reports_tx, mut reports) = mpsc::unbounded_channel();
    tauri::async_runtime::spawn(relay_reports(events, reports_tx));
    while let Some(share) = shares.recv().await {
        match share {
            Share::Text(text) => {
                // By the time the Core pushes, the user is back in the app they
                // shared FROM: this one is in the background, where Android would
                // cut its network mid-announce (see `keepalive`).
                let _hold = crate::keepalive::Hold::share();
                share_text(&app, &client, &mut reports, text).await;
            }
            // A file share asks the Core for nothing: publishing its status IS
            // the work, and the frontend takes it from there (it needs a
            // destination first, and `files.send` goes over the GUI bridge).
            // One queue for both kinds keeps them ordered — the cost being that
            // a file share queued behind a slow text share waits for it, which
            // needs two share gestures in the same second to even happen.
            Share::Files(status) => emit(&app, status),
        }
    }
}

/// Forwards `clipboard.pushed` payloads and logs the rest. Ends when the client
/// stops (a `TokenSource::File` client only stops on an API incompatibility).
async fn relay_reports(mut events: mpsc::Receiver<Event>, reports: mpsc::UnboundedSender<Value>) {
    while let Some(event) = events.recv().await {
        match event {
            Event::Notification { method, params } if method == "clipboard.pushed" => {
                if reports.send(params).is_err() {
                    return; // the worker is gone
                }
            }
            Event::Connected { .. } => tracing::info!("share connection ready"),
            Event::Incompatible { api_version } => {
                tracing::error!(api_version, "the Core speaks another API version");
            }
            _ => {}
        }
    }
}

/// Whether an announce can reach anything — see [`READY_WAIT`].
enum Readiness {
    /// Signed in, server session up: the device directory exists, so a
    /// materialized announce will actually be pushed.
    Ready,
    /// No session at all. Waiting cannot fix that, and it is the one failure the
    /// user can act on.
    NotSignedIn,
    /// Signed in, but the server session did not come up in time — the phone is
    /// offline or the server is unreachable.
    Offline,
}

/// Waits, bounded, until the Core can push. `server_connected` is the exact
/// signal: the Core sets it and `session.devices` in the SAME critical section
/// (core/src/session.rs), so it is true precisely when `account_peers` can be
/// non-empty. A `session.status` that fails is not a verdict — the IPC connection
/// is simply still being established — so it keeps waiting for the deadline.
async fn wait_ready(client: &Client) -> Readiness {
    let deadline = tokio::time::Instant::now() + READY_WAIT;
    loop {
        match client.request("session.status", json!({})).await {
            Ok(status) => {
                if !status["logged_in"].as_bool().unwrap_or(false) {
                    return Readiness::NotSignedIn;
                }
                if status["server_connected"].as_bool().unwrap_or(false) {
                    return Readiness::Ready;
                }
            }
            Err(e) => tracing::debug!(error = %e, "waiting for the Core to be shareable"),
        }
        if tokio::time::Instant::now() >= deadline {
            return Readiness::Offline;
        }
        tokio::time::sleep(READY_POLL).await;
    }
}

/// Announces one shared text, then follows it to its delivery report. Every exit
/// path emits a terminal `core:share` — a share that silently goes nowhere would
/// leave the UI stuck on "sharing".
async fn share_text(
    app: &AppHandle,
    client: &Client,
    reports: &mut mpsc::UnboundedReceiver<Value>,
    text: String,
) {
    if text.len() > MAX_TEXT {
        emit(app, json!({ "phase": "error", "reason": "too_large" }));
        return;
    }
    emit(app, json!({ "phase": "sending" }));

    let params = json!({
        "formats": [{ "format": "text" }],
        "materialize": true,
        "blobs": { "text": base64::engine::general_purpose::STANDARD.encode(&text) },
    });
    let mut announced = None;
    for attempt in 1..=ANNOUNCE_ATTEMPTS {
        match wait_ready(client).await {
            Readiness::Ready => {}
            Readiness::NotSignedIn => {
                tracing::warn!("a share arrived with no session");
                emit(app, json!({ "phase": "error", "reason": "not_signed_in" }));
                return;
            }
            Readiness::Offline => {
                tracing::warn!("a share found no server session in time");
                emit(app, json!({ "phase": "error", "reason": "offline" }));
                return;
            }
        }
        match client.request("clipboard.updated", params.clone()).await {
            Ok(result) => {
                announced = Some(result);
                break;
            }
            // Nothing was sent: the connection dropped between the wait and the
            // request. Wait for it to come back and try once more.
            Err(RequestError::NotConnected) if attempt < ANNOUNCE_ATTEMPTS => continue,
            Err(e) => {
                tracing::warn!(error = %e, "the share was refused");
                emit(
                    app,
                    json!({ "phase": "error", "reason": "refused", "detail": e.to_string() }),
                );
                return;
            }
        }
    }
    // Unreachable as written (every path above either announces, returns, or
    // retries), and kept so: the invariant this function owes the UI is that it
    // ALWAYS emits a terminal status, and that must survive a future edit to the
    // loop above.
    let Some(result) = announced else {
        tracing::warn!("the share never reached a live connection");
        emit(app, json!({ "phase": "error", "reason": "offline" }));
        return;
    };

    let tx_id = result["tx_id"].as_str().unwrap_or_default().to_string();
    let targets = result["pushed_to"].as_u64().unwrap_or(0);
    tracing::info!(tx_id = %tx_id, targets, "share announced");
    if targets == 0 {
        // Nothing was pushed and no report will come: the copy exists only on
        // this phone, which for a share is a failure, however well it went.
        emit(app, json!({ "phase": "error", "reason": "no_devices" }));
        return;
    }
    emit(app, json!({ "phase": "sending", "targets": targets }));

    match await_report(reports, &tx_id).await {
        Some(report) => {
            tracing::info!(tx_id = %tx_id, report = %report, "share settled");
            emit(
                app,
                json!({
                    "phase": "done",
                    "delivered": report["delivered"].as_u64().unwrap_or(0),
                    "failed": report["failed"].as_u64().unwrap_or(targets),
                }),
            );
        }
        None => {
            tracing::warn!(tx_id = %tx_id, "no delivery report for the share");
            emit(app, json!({ "phase": "error", "reason": "unconfirmed" }));
        }
    }
}

/// Waits for `tx_id`'s report, skipping any left over from an earlier share (the
/// Core promises one per announce, but a report we timed out on could still land
/// afterwards and must not be mistaken for this one's).
async fn await_report(reports: &mut mpsc::UnboundedReceiver<Value>, tx_id: &str) -> Option<Value> {
    let deadline = tokio::time::Instant::now() + REPORT_TIMEOUT;
    loop {
        let report = tokio::time::timeout_at(deadline, reports.recv())
            .await
            .ok()??;
        if report["tx_id"].as_str() == Some(tx_id) {
            return Some(report);
        }
    }
}

/// The last status emitted, retained for [`share_status`]. A cold share runs
/// before the webview exists, and a Tauri event is delivered only to listeners
/// already registered — so without this the outcome of the very share that
/// started the app would never be shown. Same subscribe-then-read-the-snapshot
/// contract as the connection status (`gui`'s `connection_status`).
///
/// For a file share it carries more than feedback: a retained `pick` IS the share
/// waiting for a destination, and it is the normal case rather than a corner one
/// — the picker exists precisely because the share is what started the app.
static LAST: Mutex<Option<Value>> = Mutex::new(None);

/// Snapshot of the last share's status, or `null`. Read by the frontend right
/// after it subscribes to `core:share`.
#[tauri::command]
pub fn share_status() -> Option<Value> {
    LAST.lock().expect("lock last share status").clone()
}

/// Publishes a status: retained FIRST, then emitted — a frontend that reads the
/// snapshot while this runs gets the newer value, never a stale one (the order
/// `gui`'s bridge uses for the connection, for the same reason).
fn emit(app: &AppHandle, status: Value) {
    *LAST.lock().expect("lock last share status") = Some(status.clone());
    let _ = app.emit(SHARE_EVENT, status);
}
