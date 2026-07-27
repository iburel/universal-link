// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

//! The manager's brain: it mirrors the Core's directory, decides what the menu
//! may offer, renders that onto the OS surfaces, and turns a courier's click
//! into a `files.send`.
//!
//! Three rules it shares with the GUI's store and the Core's session task, and
//! for the same reasons:
//! 1. **state comes only from a snapshot or a notification**, never from the
//!    reply to a command we issued;
//! 2. **a resynchronization is total and generational** — a reply from a
//!    superseded generation is dropped, and notifications that land while one is
//!    in flight are buffered and replayed on top of it;
//! 3. **connected implies eventually primed** — a `Timeout` leaves the IPC
//!    connection alive, so nothing else would ever retry a snapshot that failed;
//!    this loop retries it itself.
//!
//! Nothing here awaits the Core inside the event loop: a snapshot runs in its own
//! task and reports back, and a click is served by a per-courier task. A stop
//! request is therefore never queued behind a 10 s request timeout.

use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};
use tokio::sync::{mpsc, watch};
use universallink_ipc_client::{Client, Event, RequestError};

use crate::channel::{self, Listener, Request, Response, Stream, error};
use crate::clicks::Clicks;
use crate::surface::{MenuSurface, Target};
use crate::targets::Directory;

/// Quiet window before a change is rendered. Device events arrive in bursts (a
/// PC waking up publishes its relay right after coming online, and the Core
/// notifies both), and each render rewrites registry keys or files the desktop
/// watches — so we wait for the burst to end. Short enough that hiding a menu
/// that just became unusable still feels immediate.
const RENDER_DEBOUNCE: Duration = Duration::from_millis(250);
/// Longest a render may be postponed by a stream of further changes. The quiet
/// window is extended on every change, so this is what keeps a flapping device
/// from holding the menu stale forever.
const MAX_RENDER_DELAY: Duration = Duration::from_secs(2);
/// Wait before retrying a snapshot that failed on a live connection.
const SNAPSHOT_RETRY: Duration = Duration::from_secs(2);
/// Time allowed to remove every menu entry when stopping. Bounded: a wedged
/// surface must not hold the supervisor's 3 s grace hostage.
const CLEANUP_GRACE: Duration = Duration::from_secs(2);
/// Notifications held while a snapshot is in flight. Beyond this, the buffer is
/// dropped and a fresh snapshot asked for — replaying a truncated history would
/// be worse than resynchronizing.
const BUFFERED_EVENTS_CAP: usize = 256;
/// Pause after a failed `accept` so a persistent error (a descriptor ceiling)
/// cannot spin the loop hot.
const ACCEPT_ERROR_PAUSE: Duration = Duration::from_millis(100);
/// Initial deadline of the two on-demand timers. Never reached: arming always
/// resets the deadline first, and a disabled `select!` branch is not polled.
const DORMANT: Duration = Duration::from_secs(86_400);

/// Why the manager stopped — mapped by `main` to an exit code.
#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    /// Standard input closed: the supervisor asked us to stop. The only
    /// graceful-stop channel that exists on all three OSes. Exit success.
    StdinClosed,
    /// The IPC connection dropped after having been established. The spawn token
    /// is single-use — we exit and the supervisor restarts us with a fresh one.
    ConnectionLost,
    /// The Core announced an incompatible API version: retrying will not heal
    /// it.
    Incompatible,
    /// The client task ended on its own (no `Client` left).
    ClientEnded,
}

/// Runs the manager until it must stop. Always leaves the OS surfaces empty.
pub async fn run(
    client: Client,
    events: mpsc::Receiver<Event>,
    stdin_closed: impl Future<Output = ()>,
    listener: Listener,
    surfaces: Vec<Box<dyn MenuSurface>>,
) -> Outcome {
    let (targets_tx, targets_rx) = watch::channel::<Arc<[Target]>>(Arc::from([]));
    let applier = tokio::spawn(crate::applier::run(surfaces, targets_rx));
    let couriers = tokio::spawn(serve(listener, client.clone(), targets_tx.subscribe()));

    let outcome = drive(&client, events, stdin_closed, &targets_tx).await;

    // Dropping the sender is the single teardown signal: the applier renders an
    // empty list and exits, and the courier loop stops accepting. Then the
    // listener drops, taking its socket and its exclusivity lock with it.
    drop(targets_tx);
    if tokio::time::timeout(CLEANUP_GRACE, applier).await.is_err() {
        eprintln!("[universallink-menu] a surface did not clear in time: entries may remain");
    }
    couriers.abort();
    outcome
}

/// What a snapshot task reports back.
enum Snapshot {
    Taken {
        generation: u64,
        session: Value,
        account: Value,
        devices: Vec<Value>,
    },
    Failed {
        generation: u64,
    },
}

/// When the debounced render must happen: at the end of the quiet window, but
/// never later than [`MAX_RENDER_DELAY`] after the first change of this burst.
///
/// Without the cap, a device flapping faster than the quiet window would postpone
/// the render for as long as it kept flapping, and the menu would stay stale
/// indefinitely — a debounce must not become a starvation.
fn render_deadline(cap: &mut Option<tokio::time::Instant>) -> tokio::time::Instant {
    let now = tokio::time::Instant::now();
    let hard = *cap.get_or_insert(now + MAX_RENDER_DELAY);
    std::cmp::min(now + RENDER_DEBOUNCE, hard)
}

/// One step of the loop, derived from an IPC event. Pure, so the exit conditions
/// — the supervised-component contract — are unit-tested without a Core.
#[derive(Debug, PartialEq, Eq)]
enum Step {
    /// Connection established: everything we knew is void, resynchronize.
    Resync,
    /// A notification to ingest.
    Ingest { method: String, params: Value },
    /// Connected but uninteresting.
    Idle,
    /// The loop must end.
    Exit(Outcome),
}

fn classify(event: Option<Event>) -> Step {
    match event {
        Some(Event::Connected { .. }) => Step::Resync,
        Some(Event::Notification { method, params }) => Step::Ingest { method, params },
        // The manager serves no Core→component method (empty served_methods).
        Some(Event::Request { .. }) => Step::Idle,
        Some(Event::Disconnected) => Step::Exit(Outcome::ConnectionLost),
        Some(Event::Incompatible { .. }) => Step::Exit(Outcome::Incompatible),
        None => Step::Exit(Outcome::ClientEnded),
    }
}

/// Applies one notification to the directory. Returns what it implies:
/// whether the target list may have moved, and whether the whole directory must
/// be re-snapshotted.
fn ingest(dir: &mut Directory, method: &str, params: &Value) -> (bool, bool) {
    match method {
        // Applied at once — a logout or a lost server link must empty the menu
        // now, not after a round trip — AND followed by a resnapshot, because a
        // session change can replace the directory wholesale.
        "session.changed" => {
            dir.apply_session(params);
            (true, true)
        }
        _ if method.starts_with("device.") => (dir.apply_device_event(method, params), false),
        _ => (false, false),
    }
}

/// Rules 1 and 2 of the module header, with the timers left out so they can be
/// verified without any: a resynchronization is TOTAL and GENERATIONAL, and the
/// notifications that land while one is in flight are held and replayed on top of
/// it.
///
/// Held, not applied: a snapshot REPLACES the directory, so an event applied
/// before it landed would be erased — and the Core never resends it. Replaying in
/// order after the snapshot is safe because each held event is either idempotent
/// (it carries a whole record) or newer than the snapshot.
#[derive(Debug, Default)]
struct Resync {
    generation: u64,
    primed: bool,
    held: Vec<(String, Value)>,
}

impl Resync {
    /// Starts a new resynchronization: everything we knew is void. Returns the
    /// generation to stamp the request with.
    fn begin(&mut self) -> u64 {
        self.generation += 1;
        self.primed = false;
        self.held.clear();
        self.generation
    }

    /// Whether a reply stamped `generation` is still the one we are waiting for.
    fn accepts(&self, generation: u64) -> bool {
        generation == self.generation
    }

    fn primed(&self) -> bool {
        self.primed
    }

    /// Holds a notification until the snapshot lands. Returns `false` if the
    /// buffer is full: the caller must then resynchronize from scratch rather
    /// than replay a history it has truncated.
    fn hold(&mut self, method: &str, params: &Value) -> bool {
        if self.held.len() >= BUFFERED_EVENTS_CAP {
            return false;
        }
        self.held.push((method.to_string(), params.clone()));
        true
    }

    /// Marks the snapshot applied and replays what was held, in order. Returns
    /// whether a replayed event asks for yet another resynchronization.
    fn replay(&mut self, dir: &mut Directory) -> bool {
        self.primed = true;
        let mut again = false;
        for (method, params) in std::mem::take(&mut self.held) {
            let (_, resnapshot) = ingest(dir, &method, &params);
            again |= resnapshot;
        }
        again
    }
}

async fn drive(
    client: &Client,
    mut events: mpsc::Receiver<Event>,
    stdin_closed: impl Future<Output = ()>,
    targets_tx: &watch::Sender<Arc<[Target]>>,
) -> Outcome {
    tokio::pin!(stdin_closed);
    let (snapshots_tx, mut snapshots_rx) = mpsc::channel::<Snapshot>(4);

    let mut dir = Directory::new();
    let mut resync = Resync::default();

    // Two dormant timers, armed on demand. A disabled `select!` branch is never
    // polled, and arming always resets the deadline, so neither can fire stale.
    let render = tokio::time::sleep(DORMANT);
    tokio::pin!(render);
    let mut render_armed = false;
    // Hard deadline of the burst currently being debounced, if any.
    let mut render_cap: Option<tokio::time::Instant> = None;
    let retry = tokio::time::sleep(DORMANT);
    tokio::pin!(retry);
    let mut retry_armed = false;

    loop {
        tokio::select! {
            _ = &mut stdin_closed => return Outcome::StdinClosed,

            _ = &mut render, if render_armed => {
                render_armed = false;
                render_cap = None;
                publish(targets_tx, dir.targets());
            }

            _ = &mut retry, if retry_armed => {
                retry_armed = false;
                spawn_snapshot(client, resync.begin(), &snapshots_tx);
            }

            snapshot = snapshots_rx.recv() => match snapshot {
                // Unreachable: this task owns a sender. Treated as an ended
                // client rather than a panic.
                None => return Outcome::ClientEnded,
                Some(Snapshot::Failed { generation }) => {
                    if resync.accepts(generation) {
                        // The connection is still up, so nothing else will ever
                        // retry for us: rule 3.
                        retry.as_mut().reset(tokio::time::Instant::now() + SNAPSHOT_RETRY);
                        retry_armed = true;
                    }
                }
                Some(Snapshot::Taken { generation, session, account, devices }) => {
                    if !resync.accepts(generation) {
                        continue; // superseded
                    }
                    dir.apply_session(&session);
                    dir.apply_account(&account);
                    dir.replace_all(&devices);
                    retry_armed = false;
                    if resync.replay(&mut dir) {
                        // A replayed session change may postdate this snapshot,
                        // so the directory we just installed cannot be vouched
                        // for: hide everything until the next one lands.
                        render_armed = false;
                        render_cap = None;
                        publish(targets_tx, Vec::new());
                        spawn_snapshot(client, resync.begin(), &snapshots_tx);
                    } else {
                        render.as_mut().reset(render_deadline(&mut render_cap));
                        render_armed = true;
                    }
                }
            },

            event = events.recv() => match classify(event) {
                Step::Exit(outcome) => return outcome,
                Step::Idle => {}
                Step::Resync => {
                    // A fresh connection: the directory we held belongs to the
                    // previous one. Hide everything at once — fail-closed —
                    // rather than offer stale targets while the snapshot flies.
                    retry_armed = false;
                    render_armed = false;
                    render_cap = None;
                    dir = Directory::new();
                    publish(targets_tx, Vec::new());
                    spawn_snapshot(client, resync.begin(), &snapshots_tx);
                }
                Step::Ingest { method, params } => {
                    if resync.primed() {
                        let (changed, resnapshot) = ingest(&mut dir, &method, &params);
                        if resnapshot {
                            // The session moved: what we hold describes the state
                            // before it did. Hide everything now (a logout or a
                            // dropped server link must empty the menu at once)
                            // and let the snapshot repopulate it.
                            render_armed = false;
                            render_cap = None;
                            publish(targets_tx, Vec::new());
                            spawn_snapshot(client, resync.begin(), &snapshots_tx);
                        } else if changed {
                            render.as_mut().reset(render_deadline(&mut render_cap));
                            render_armed = true;
                        }
                    } else if is_stateful(&method) && !resync.hold(&method, &params) {
                        // Too much moved while we were resynchronizing: start
                        // over rather than replay a truncated history.
                        spawn_snapshot(client, resync.begin(), &snapshots_tx);
                    }
                }
            },
        }
    }
}

/// Publishes a target list, but ONLY if it differs from what is already showing.
///
/// Not an optimization: every render rewrites registry keys or files the desktop
/// watches, so a rewrite that shows nothing new is pure churn — and there are
/// plenty of occasions for one (a reconnection while the menu is already empty, a
/// `presence.update` that changes a relay the menu does not display, a resnapshot
/// that confirms what we had).
fn publish(tx: &watch::Sender<Arc<[Target]>>, targets: Vec<Target>) {
    tx.send_if_modified(move |current| {
        let next: Arc<[Target]> = targets.into();
        if *current == next {
            return false;
        }
        *current = next;
        true
    });
}

/// Whether a notification carries state worth holding while a snapshot is in
/// flight. Filtered before buffering so an unrelated topic cannot fill it.
fn is_stateful(method: &str) -> bool {
    method == "session.changed" || method.starts_with("device.")
}

fn spawn_snapshot(client: &Client, generation: u64, tx: &mpsc::Sender<Snapshot>) {
    let client = client.clone();
    let tx = tx.clone();
    tokio::spawn(async move {
        let _ = tx.send(take_snapshot(&client, generation).await).await;
    });
}

/// `session.status` + `account.status` + `devices.list`, off the event loop.
///
/// All three, and every time: there is no `account.*` notification, so a snapshot
/// is the only moment the account key's presence can ever be learned.
async fn take_snapshot(client: &Client, generation: u64) -> Snapshot {
    let Ok(session) = client.request("session.status", json!({})).await else {
        return Snapshot::Failed { generation };
    };
    let account = match client.request("account.status", json!({})).await {
        Ok(account) => account,
        // An application error means we cannot claim to hold the key: fail-closed
        // (an empty menu), not a retry.
        Err(RequestError::Rpc(_)) => json!({ "attested": false }),
        Err(_) => return Snapshot::Failed { generation },
    };
    let devices = match client.request("devices.list", json!({})).await {
        Ok(list) => list.as_array().cloned().unwrap_or_default(),
        // An application error is an answer, not a failure to retry: the Core
        // replies `SERVER_UNREACHABLE` while it has never snapshotted the
        // directory (signed out, or still connecting), and that means "no
        // devices" — which is exactly the empty menu we want.
        Err(RequestError::Rpc(_)) => Vec::new(),
        Err(_) => return Snapshot::Failed { generation },
    };
    Snapshot::Taken {
        generation,
        session,
        account,
        devices,
    }
}

// ---------------------------------------------------------------------------
// The courier side.
// ---------------------------------------------------------------------------

/// Accepts couriers until the orchestrator drops its target sender.
async fn serve(
    mut listener: Listener,
    client: Client,
    mut targets: watch::Receiver<Arc<[Target]>>,
) {
    // One gesture must be one transfer, whatever number of processes the shell
    // chose to start for it — see `clicks`. The batching task lives exactly as
    // long as this loop and the courier tasks it spawns: when they are gone,
    // nobody is waiting on a batch.
    let (clicks, _batching) = Clicks::spawn(Arc::new(move |device_id, paths| {
        let client = client.clone();
        Box::pin(async move { send_files(&client, device_id, paths).await })
    }));
    let mut couriers = tokio::task::JoinSet::new();
    loop {
        // Bounded concurrency: a Windows multi-select can start one process per
        // file. Beyond the cap the extra couriers wait in the socket's backlog
        // rather than being refused.
        while couriers.len() >= channel::MAX_CONCURRENT_CLIENTS {
            if couriers.join_next().await.is_none() {
                break;
            }
        }
        let stopped = async { while targets.changed().await.is_ok() {} };
        tokio::select! {
            _ = stopped => return,
            accepted = listener.accept() => match accepted {
                Ok(stream) => {
                    couriers.spawn(handle(stream, clicks.clone(), targets.clone()));
                }
                Err(e) => {
                    eprintln!("[universallink-menu] cannot accept a click: {e}");
                    tokio::time::sleep(ACCEPT_ERROR_PAUSE).await;
                }
            },
        }
    }
}

/// Serves one courier: read its request, answer it, hang up.
async fn handle(mut stream: Stream, clicks: Clicks, targets: watch::Receiver<Arc<[Target]>>) {
    let response = match channel::read_request(&mut stream).await {
        Ok(request) => act(request, &clicks, &targets).await,
        Err(code) => Response::failed(code),
    };
    channel::write_response(&mut stream, &response).await;
}

async fn act(
    request: Request,
    clicks: &Clicks,
    targets: &watch::Receiver<Arc<[Target]>>,
) -> Response {
    match request {
        Request::Targets => Response::Targets(targets.borrow().to_vec()),
        Request::Send { device_id, paths } => {
            // Fail-closed, and locally: a stale artifact — written before the
            // peer went offline, or left behind by a manager that crashed — must
            // not reach the Core at all.
            if !targets.borrow().iter().any(|t| t.device_id == device_id) {
                return Response::failed(error::NO_SUCH_TARGET);
            }
            // Checked HERE, per courier, and not once the batch is assembled: a
            // path this process cannot express must cost its own click and not
            // the whole gesture's.
            if paths.iter().any(|path| path.to_str().is_none()) {
                return Response::failed(error::NON_UTF8_PATH);
            }
            clicks.submit(device_id, paths).await
        }
    }
}

/// One `files.send`, for a whole batch of clicks.
async fn send_files(client: &Client, device_id: String, paths: Vec<PathBuf>) -> Response {
    // Every path was checked for this on the way in.
    let list: Vec<&str> = paths.iter().filter_map(|path| path.to_str()).collect();
    let params = json!({ "device_id": device_id, "paths": list });
    match client.request("files.send", params).await {
        Ok(result) => match result["transfer_id"].as_str() {
            Some(transfer_id) => Response::Accepted {
                transfer_id: transfer_id.to_string(),
            },
            None => Response::failed(error::BAD_REQUEST),
        },
        // The Core's own application code, relayed verbatim: the courier logs it,
        // and a human reading the log sees DEVICE_OFFLINE rather than a code we
        // invented.
        Err(RequestError::Rpc(e)) => Response::Failed {
            error: e.data_code.unwrap_or_else(|| format!("RPC_{}", e.code)),
        },
        Err(_) => Response::failed(error::CORE_UNREACHABLE),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_maps_events_to_steps() {
        assert_eq!(
            classify(Some(Event::Connected {
                granted_scopes: vec![],
                api_version: 1
            })),
            Step::Resync
        );
        assert_eq!(
            classify(Some(Event::Notification {
                method: "device.online".into(),
                params: json!({}),
            })),
            Step::Ingest {
                method: "device.online".into(),
                params: json!({}),
            }
        );
        // The exit conditions of the supervised-component contract.
        assert_eq!(
            classify(Some(Event::Disconnected)),
            Step::Exit(Outcome::ConnectionLost)
        );
        assert_eq!(
            classify(Some(Event::Incompatible { api_version: 2 })),
            Step::Exit(Outcome::Incompatible)
        );
        assert_eq!(classify(None), Step::Exit(Outcome::ClientEnded));
    }

    #[test]
    fn a_session_change_is_applied_at_once_and_asks_for_a_resnapshot() {
        let mut dir = Directory::new();
        let (changed, resnapshot) = ingest(
            &mut dir,
            "session.changed",
            &json!({ "logged_in": true, "server_connected": true }),
        );
        assert!(changed);
        assert!(resnapshot, "a session change can replace the directory");

        let (changed, resnapshot) = ingest(
            &mut dir,
            "device.added",
            &json!({ "device": { "device_id": "d_1" } }),
        );
        assert!(changed);
        assert!(!resnapshot, "a device event is self-sufficient");

        // An unrelated topic moves nothing.
        assert_eq!(
            ingest(&mut dir, "transfer.progress", &json!({})),
            (false, false)
        );
    }

    #[test]
    fn only_stateful_notifications_are_worth_buffering() {
        assert!(is_stateful("session.changed"));
        assert!(is_stateful("device.offline"));
        assert!(!is_stateful("transfer.progress"));
        assert!(!is_stateful("clipboard.updated"));
    }

    /// An online, attested, relayed peer — the shape `devices.list` really
    /// serves.
    fn peer(id: &str) -> Value {
        json!({
            "device_id": id,
            "name": id,
            "platform": "linux",
            "relay_url": "https://relay.example/",
            "attestation": "beef",
            "online": true,
            "is_self": false,
        })
    }

    fn live_dir(devices: &[Value]) -> Directory {
        let mut dir = Directory::new();
        dir.apply_session(&json!({ "logged_in": true, "server_connected": true }));
        dir.apply_account(&json!({ "attested": true }));
        dir.replace_all(devices);
        dir
    }

    fn ids(dir: &Directory) -> Vec<String> {
        dir.targets().into_iter().map(|t| t.device_id).collect()
    }

    /// The window is real and narrow: a peer that goes offline between our
    /// `devices.list` request and its reply. The snapshot REPLACES the directory,
    /// so applying the event early would erase it — and the Core never resends
    /// one. Dropped, the menu would offer a dead device for the rest of the
    /// process's life.
    #[test]
    fn an_event_that_races_the_snapshot_is_held_and_replayed_on_top_of_it() {
        let mut resync = Resync::default();
        let generation = resync.begin();

        // In flight: not primed, so the event is held rather than applied.
        assert!(!resync.primed());
        assert!(resync.hold("device.offline", &json!({ "device_id": "d_1" })));

        // The snapshot lands — taken BEFORE the peer went offline, so it still
        // says online.
        assert!(resync.accepts(generation));
        let mut dir = live_dir(&[peer("d_1"), peer("d_2")]);
        assert_eq!(ids(&dir), ["d_1", "d_2"]);

        assert!(!resync.replay(&mut dir), "no session change was held");
        assert!(resync.primed());
        assert_eq!(
            ids(&dir),
            ["d_2"],
            "the held offline must have been replayed"
        );
    }

    /// Rule 1: a reply from a superseded generation is dropped. Otherwise a
    /// snapshot requested before a logout could repopulate the menu after it.
    #[test]
    fn a_superseded_snapshot_is_refused() {
        let mut resync = Resync::default();
        let first = resync.begin();
        let second = resync.begin();
        assert!(!resync.accepts(first));
        assert!(resync.accepts(second));
    }

    /// A new resynchronization voids what was held for the previous one: those
    /// events described a directory nobody is going to install now.
    #[test]
    fn beginning_again_drops_what_was_held() {
        let mut resync = Resync::default();
        resync.begin();
        assert!(resync.hold("device.offline", &json!({ "device_id": "d_1" })));
        resync.begin();

        let mut dir = live_dir(&[peer("d_1")]);
        assert!(!resync.replay(&mut dir));
        assert_eq!(ids(&dir), ["d_1"], "the stale held event must not apply");
    }

    /// Replaying a truncated history would be worse than resynchronizing: the
    /// buffer refuses rather than dropping the oldest silently.
    #[test]
    fn the_hold_buffer_is_bounded_and_says_so() {
        let mut resync = Resync::default();
        resync.begin();
        for i in 0..BUFFERED_EVENTS_CAP {
            assert!(
                resync.hold("device.offline", &json!({ "device_id": i.to_string() })),
                "held {i}"
            );
        }
        assert!(
            !resync.hold("device.offline", &json!({ "device_id": "overflow" })),
            "the caller must be told to start over"
        );
    }

    /// A held session change still asks for another resynchronization after the
    /// replay: it may postdate the snapshot we just installed.
    #[test]
    fn a_held_session_change_asks_to_start_over() {
        let mut resync = Resync::default();
        resync.begin();
        assert!(resync.hold(
            "session.changed",
            &json!({ "logged_in": false, "server_connected": false })
        ));

        let mut dir = live_dir(&[peer("d_1")]);
        assert!(resync.replay(&mut dir), "a session change must trigger one");
        // And it was applied on the way: the menu is empty at once.
        assert!(dir.targets().is_empty());
    }

    /// The quiet window is extended by every change, so without a hard cap a
    /// device flapping faster than 250 ms would postpone the render for as long as
    /// it kept flapping — a debounce must not become a starvation.
    #[test]
    fn the_render_deadline_is_capped_so_a_flapping_device_cannot_starve_it() {
        let mut cap = None;
        let first = render_deadline(&mut cap);
        let hard = cap.expect("the cap is armed with the first change");
        assert!(first <= hard);

        // Every further change extends the quiet window, but never past the cap.
        for _ in 0..100 {
            let next = render_deadline(&mut cap);
            assert!(next <= hard, "the debounce must not postpone past the cap");
        }
        assert_eq!(
            cap,
            Some(hard),
            "the cap belongs to the burst, not the event"
        );
    }
}
