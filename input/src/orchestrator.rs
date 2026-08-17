// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

//! The event loop, and the whole supervised-component contract.
//!
//! One `select!`, one [`crate::session::Engine`], no locks. The loop bridges four
//! sources onto it: the Core's control plane (the routed `input.*` facade, the
//! `devices` topic, `peer.message`, `peer.channel`, `peer.channel_closed`), the
//! platform backend's upcalls, the live channels' inbound frames, and one
//! deadline. Everything the engine needs from the Core comes back as an
//! [`crate::session::Effect`] executed here on a task, so a Core that stopped
//! draining can never hold the loop, the stop signal, or a keyboard.
//!
//! # One task per warm channel
//!
//! A [`PeerChannel`] is owned by its own task, which `select!`s between the pipe
//! and an `mpsc` outbox the loop writes into. It forwards every inbound frame,
//! and finally its own death, to the loop over one shared channel: one channel,
//! so a task's `Attached` can never be processed after the frames it announced.
//! That is also why nothing here needs to split a `PeerChannel`: `recv` is
//! cancel-safe and lives in the task's `select!`, while `send` is a write that
//! must not be cancelled and is awaited inside its own arm.
//!
//! # Why the request timeout is 25 s
//!
//! `peers.channel` does not answer until the far end has ATTACHED, and its
//! budgets for getting there are up to 15 s of dialling plus 10 s more
//! ([`PEER_CHANNEL_REQUEST_TIMEOUT`]). A tighter budget on this side would turn
//! the one refusal the primitive owes a person (`NO_DIRECT_PATH`, whose remedy is
//! the pair's network) into a bare timeout, which is a lie about whose fault it
//! is.

use std::collections::BTreeMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use onedevice_ipc_client::{
    Client, ClientConfig, Event, PEER_CHANNEL_REQUEST_TIMEOUT, PeerChannel, RequestError,
    RequestId, TokenSource,
};
use serde_json::{Value, json};
use tokio::sync::mpsc;

use crate::backend::{BackendEvent, InputBackend};
use crate::session::{Directory, Effect, Engine, OUTBOX};
use crate::store::Store;

/// The facade methods this engine answers (doc/input-sharing.md, section 12),
/// whole: anything else in the namespace is refused with `-32601` by the IPC
/// client itself, and the Core relays that refusal verbatim to the caller.
pub const SERVED_METHODS: [&str; 10] = [
    "input.status",
    "input.place",
    "input.allow",
    "input.drive",
    "input.take",
    "input.release",
    "input.guards",
    "input.lock",
    "input.hotkey",
    "input.remap",
];

/// Set by the supervisor: the Core's listening endpoint.
const IPC_PATH_ENV: &str = "ONEDEVICE_IPC_PATH";

/// The role and rights the engine asks for: exactly the documented profile
/// (doc/core-api.md, "Scopes").
const ROLE: &str = "input-backend";
const SCOPES: [&str; 5] = [
    "input.serve",
    "peers.channel",
    "peers.message",
    "devices.read",
    "session.read",
];
/// Never `input` here: that topic requires `input.read`, which this engine does
/// not hold (it PUBLISHES the topic through `input.emit`), and a refused topic
/// fails the whole subscription.
const TOPICS: [&str; 2] = ["devices", "session"];

/// Barely matters: a spawn token is single-use, so the loop exits on the first
/// loss rather than letting the client retry.
const RECONNECT_BASE_DELAY: Duration = Duration::from_millis(500);

/// Frames drained from the inbound queue in one batch, which is what the target's
/// read-side coalescing works on (D6). Bounded so one flooding peer cannot starve
/// the backend's upcalls or the return hotkey.
const MAX_BATCH: usize = 128;

/// Emissions buffered towards the one task that publishes them in order.
const EMIT_QUEUE: usize = 64;

/// Why the loop ended, mapped by the binary to a process exit code.
#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    /// Standard input closed: the supervisor asked us to stop. Exit success.
    StdinClosed,
    /// The IPC connection dropped after being established. The spawn token is
    /// single-use, so exiting lets the supervisor restart us with a fresh one.
    ConnectionLost,
    /// The Core announced an incompatible API version: retrying will not heal it.
    Incompatible,
    /// The client task ended on its own (no `Client` left).
    ClientEnded,
    /// The platform backend's upcall stream ended: its OS loop stopped, so there
    /// is nothing left to capture with and nothing left to type with.
    BackendLost,
}

/// One step derived from an IPC event. Pure, so the exit conditions (the
/// supervised-component contract) are unit-tested without a Core.
enum Step {
    Facade(RequestId, String, Value),
    /// Connection established: the directory must be (re)resolved.
    Resync,
    /// A layout document from a sibling engine.
    PeerMessage(Value),
    /// A peer opened a channel to us: the other direction of the warm channel.
    PeerChannel(Value),
    /// A channel ended, with the Core's reason.
    PeerChannelClosed(Value),
    /// The directory changed: refresh it, then re-evaluate the warm set.
    DirectoryStale,
    Idle,
    Exit(Outcome),
}

fn classify(event: Option<Event>) -> Step {
    match event {
        // The client only delivers requests whose method is in SERVED_METHODS.
        Some(Event::Request { id, method, params }) => Step::Facade(id, method, params),
        Some(Event::Connected { .. }) => Step::Resync,
        Some(Event::Notification { method, params }) => match method.as_str() {
            "peer.message" => Step::PeerMessage(params),
            "peer.channel" => Step::PeerChannel(params),
            "peer.channel_closed" => Step::PeerChannelClosed(params),
            "session.changed" => Step::DirectoryStale,
            other if other.starts_with("device.") => Step::DirectoryStale,
            _ => Step::Idle,
        },
        Some(Event::Disconnected) => Step::Exit(Outcome::ConnectionLost),
        Some(Event::Incompatible { .. }) => Step::Exit(Outcome::Incompatible),
        None => Step::Exit(Outcome::ClientEnded),
    }
}

/// What the side tasks report back into the loop. ONE channel for all of it, so a
/// channel task's `Attached` is always seen before the frames it then forwards.
enum Inbound {
    /// A fresh `devices.list` (`None`: the Core knows of no device at all, so the
    /// engine waits and every gesture answers `INPUT_NOT_READY`).
    Directory(Option<Value>),
    /// A channel is live: the outbox is how the loop writes to it, and dropping
    /// the outbox is how the task is asked to end.
    Attached {
        node: String,
        out: mpsc::Sender<Vec<u8>>,
        /// Did we open it, or did the peer? Only ours are dropped when a peer
        /// leaves the warm set.
        mine: bool,
    },
    Frame {
        node: String,
        bytes: Vec<u8>,
    },
    /// The pipe ended. Either this or `peer.channel_closed` can come first.
    Ended {
        node: String,
    },
    OpenFailed {
        node: String,
        code: String,
    },
}

struct Loop<B> {
    engine: Engine<B>,
    client: Client,
    ipc_path: PathBuf,
    inbound: mpsc::Sender<Inbound>,
    emissions: mpsc::Sender<(String, Value)>,
    /// Channels being opened right now, so a second trigger does not dial twice.
    opening: BTreeMap<String, ()>,
}

/// Runs the engine until a terminal condition.
///
/// `stdin_closed` is the supervisor's graceful-stop signal; `backend_events` is
/// the platform backend's upcall stream; `ipc_path` is where a peer channel's
/// second connection is made.
pub async fn run<B: InputBackend>(
    client: Client,
    mut events: mpsc::Receiver<Event>,
    backend: B,
    mut backend_events: mpsc::Receiver<BackendEvent>,
    store: Store,
    ipc_path: PathBuf,
    stdin_closed: impl Future<Output = ()> + Send,
) -> Outcome {
    let engine = match Engine::open(backend, store, Instant::now()) {
        Ok(engine) => engine,
        // A corrupt identity or a corrupt settings file is deliberately NOT
        // self-healing: a silently fresh key would unpin this device from every
        // plane, and a silently fresh grant table would open or close a door
        // nobody chose. Report and stay down, visibly.
        Err(e) => {
            eprintln!("[1device-input] cannot open the engine state: {e}");
            return Outcome::ClientEnded;
        }
    };
    let (inbound_tx, mut inbound_rx) = mpsc::channel::<Inbound>(1024);
    let (emit_tx, emit_rx) = mpsc::channel::<(String, Value)>(EMIT_QUEUE);
    tokio::spawn(emitter(client.clone(), emit_rx));
    let mut state = Loop {
        engine,
        client,
        ipc_path,
        inbound: inbound_tx,
        emissions: emit_tx,
        opening: BTreeMap::new(),
    };
    state.engine.start(Instant::now()).await;
    state.after_turn(Instant::now());
    tokio::pin!(stdin_closed);

    let outcome = loop {
        // The one timer in the loop, and the flow contributes exactly one
        // deadline to it: the trailing-edge flush of a pending position, armed
        // only while one is pending (D5).
        let now = Instant::now();
        let deadline = state
            .engine
            .next_deadline(now)
            .unwrap_or_else(|| now + Duration::from_secs(3600));
        tokio::select! {
            biased;
            () = &mut stdin_closed => break Outcome::StdinClosed,
            event = events.recv() => {
                match classify(event) {
                    Step::Facade(id, method, params) => state.on_facade(id, &method, &params),
                    Step::Resync | Step::DirectoryStale => state.request_directory(),
                    Step::PeerMessage(params) => state.on_peer_message(&params),
                    Step::PeerChannel(params) => state.on_peer_channel(&params),
                    Step::PeerChannelClosed(params) => state.on_peer_channel_closed(&params),
                    Step::Idle => {}
                    Step::Exit(outcome) => break outcome,
                }
            }
            inbound = inbound_rx.recv() => {
                let Some(first) = inbound else {
                    // Impossible while the loop holds a sender, and harmless.
                    break Outcome::ClientEnded;
                };
                let mut batch = vec![first];
                // Drain what is readable WITHOUT blocking: the batch is what the
                // target's read-side coalescing needs in order to drop a position
                // whose successor is also a position.
                while batch.len() < MAX_BATCH {
                    match inbound_rx.try_recv() {
                        Ok(next) => batch.push(next),
                        Err(_) => break,
                    }
                }
                state.on_inbound(batch).await;
            }
            backend_event = backend_events.recv() => match backend_event {
                Some(event) => state.engine.on_backend(event, Instant::now()).await,
                None => break Outcome::BackendLost,
            },
            () = tokio::time::sleep_until(deadline.into()) => {}
        }
        state.after_turn(Instant::now());
    };

    // Whatever ended the loop, the keyboard of both machines has to work: this is
    // the last chance to release what we pressed on this one and to unpin this
    // one's pointer.
    state.engine.shutdown(Instant::now());
    state.run_effects();
    outcome
}

impl<B: InputBackend> Loop<B> {
    /// Everything that is due, then everything the engine asked the Core for.
    fn after_turn(&mut self, now: Instant) {
        self.engine.pump(now);
        self.run_effects();
    }

    fn run_effects(&mut self) {
        for effect in self.engine.take_effects() {
            match effect {
                Effect::Open { node_id, device_id } => self.open_channel(node_id, device_id),
                Effect::Send { device_id, payload } => {
                    let client = self.client.clone();
                    tokio::spawn(async move {
                        // Ordinary refusals: the peer is away, or holds no engine.
                        // The next round retries.
                        let _ = client
                            .request(
                                "peers.send",
                                json!({ "device_id": device_id, "payload": payload }),
                            )
                            .await;
                    });
                }
                Effect::Emit { method, params } => {
                    // Never blocking: a full queue means the Core is not draining
                    // its own topic, and a dropped notification is repaired by the
                    // next snapshot, which is authoritative.
                    if self.emissions.try_send((method, params)).is_err() {
                        eprintln!("[1device-input] the emission queue is full, dropping a notice");
                    }
                }
            }
        }
    }

    /// One forwarded facade call. All local work, so the answer is computed
    /// inline and only the REPLY rides a task.
    fn on_facade(&mut self, id: RequestId, method: &str, params: &Value) {
        let outcome = self.engine.serve(method, params, Instant::now());
        let client = self.client.clone();
        tokio::spawn(async move {
            let sent = match outcome {
                Ok(result) => client.respond(id, result).await,
                // A malformed request is not an application state: the engine
                // emits the real JSON-RPC code rather than dressing one as an app
                // code (doc/input-sharing.md, section 12).
                Err(what) if what.starts_with("param:") => {
                    client
                        .respond_invalid_params(id, what.trim_start_matches("param:"))
                        .await
                }
                Err(code) => client.respond_error(id, &code).await,
            };
            // A stale request id (its connection dropped mid-flight) is expected
            // around a reconnect; anything else deserves a trace.
            if let Err(e) = sent
                && !matches!(e, RequestError::Disconnected)
            {
                eprintln!("[1device-input] reply failed: {e}");
            }
        });
    }

    fn request_directory(&self) {
        let client = self.client.clone();
        let tx = self.inbound.clone();
        tokio::spawn(async move {
            let snapshot = match client.request("devices.list", json!({})).await {
                Ok(snapshot) => Some(snapshot),
                // A Core that knows of no device at all (never logged in, never
                // joined): the engine waits.
                Err(RequestError::Rpc(e))
                    if e.data_code.as_deref() == Some("SERVER_UNREACHABLE") =>
                {
                    None
                }
                Err(e) => {
                    eprintln!("[1device-input] devices.list failed: {e}");
                    None
                }
            };
            let _ = tx.send(Inbound::Directory(snapshot)).await;
        });
    }

    fn on_peer_message(&mut self, params: &Value) {
        let (Some(device_id), Some(payload)) = (
            params.get("device_id").and_then(Value::as_str),
            params.get("payload"),
        ) else {
            return;
        };
        // A sender the directory cannot resolve is DROPPED, and the directory is
        // probably stale: refresh it.
        let Some(node) = self.engine.node_of(device_id) else {
            self.request_directory();
            return;
        };
        self.engine.on_peer_message(&node, payload, Instant::now());
    }

    /// A peer opened a channel to us. The other direction of the warm channel,
    /// and the token is already minted.
    fn on_peer_channel(&mut self, params: &Value) {
        let (Some(device_id), Some(token)) = (
            params.get("device_id").and_then(Value::as_str),
            params.get("channel_token").and_then(Value::as_str),
        ) else {
            return;
        };
        let Some(node) = self.engine.node_of(device_id) else {
            self.request_directory();
            return;
        };
        let (ipc_path, tx) = (self.ipc_path.clone(), self.inbound.clone());
        let token = token.to_string();
        self.opening.insert(node.clone(), ());
        tokio::spawn(async move {
            // Theirs: an offer we came to take, so the warm set never drops it.
            attach_and_run(&ipc_path, &token, node, false, tx).await;
        });
    }

    fn on_peer_channel_closed(&mut self, params: &Value) {
        let Some(device_id) = params.get("device_id").and_then(Value::as_str) else {
            return;
        };
        let reason = params
            .get("reason")
            .and_then(Value::as_str)
            .unwrap_or("CLOSED");
        let Some(node) = self.engine.node_of(device_id) else {
            return;
        };
        self.opening.remove(&node);
        self.engine.on_channel_closed(&node, reason, Instant::now());
    }

    /// `peers.channel { device_id }`, then attach, then become the channel task.
    fn open_channel(&mut self, node: String, device_id: String) {
        if self.opening.contains_key(&node) {
            return;
        }
        self.opening.insert(node.clone(), ());
        let (client, ipc_path, tx) = (
            self.client.clone(),
            self.ipc_path.clone(),
            self.inbound.clone(),
        );
        tokio::spawn(async move {
            let token = match client
                .request("peers.channel", json!({ "device_id": device_id }))
                .await
            {
                Ok(reply) => reply
                    .get("channel_token")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                Err(e) => {
                    let code = match &e {
                        RequestError::Rpc(rpc) => rpc
                            .data_code
                            .clone()
                            .unwrap_or_else(|| "OPEN_REFUSED".to_string()),
                        _ => "OPEN_FAILED".to_string(),
                    };
                    let _ = tx.send(Inbound::OpenFailed { node, code }).await;
                    return;
                }
            };
            let Some(token) = token else {
                let _ = tx
                    .send(Inbound::OpenFailed {
                        node,
                        code: "OPEN_FAILED".into(),
                    })
                    .await;
                return;
            };
            attach_and_run(&ipc_path, &token, node, true, tx).await;
        });
    }

    /// One batch from the side tasks, in arrival order. Runs of consecutive
    /// frames from one peer are handed over together, which is exactly what the
    /// read-side coalescing rule is about.
    async fn on_inbound(&mut self, batch: Vec<Inbound>) {
        let mut frames: Vec<Vec<u8>> = Vec::new();
        let mut from = String::new();
        for item in batch {
            match item {
                Inbound::Frame { node, bytes } => {
                    if node != from {
                        self.drain(&from, std::mem::take(&mut frames)).await;
                        from = node;
                    }
                    frames.push(bytes);
                }
                other => {
                    self.drain(&from, std::mem::take(&mut frames)).await;
                    from.clear();
                    let now = Instant::now();
                    match other {
                        Inbound::Directory(snapshot) => {
                            if let Some(snapshot) = snapshot {
                                let directory = Directory::parse(&snapshot);
                                if directory.own_node.is_some() {
                                    self.engine.set_directory(directory, now);
                                }
                            }
                        }
                        Inbound::Attached { node, out, mine } => {
                            self.opening.remove(&node);
                            self.engine.attach(&node, out, mine, now);
                        }
                        Inbound::Ended { node } => {
                            self.opening.remove(&node);
                            self.engine.on_channel_ended(&node, now);
                        }
                        Inbound::OpenFailed { node, code } => {
                            self.opening.remove(&node);
                            self.engine.on_open_failed(&node, &code, now);
                        }
                        Inbound::Frame { .. } => unreachable!("handled above"),
                    }
                }
            }
        }
        self.drain(&from, frames).await;
    }

    async fn drain(&mut self, node: &str, frames: Vec<Vec<u8>>) {
        if frames.is_empty() || node.is_empty() {
            return;
        }
        self.engine.on_frames(node, frames, Instant::now()).await;
    }
}

/// Publishes on the `input` topic, in order, on ONE task: two snapshots emitted
/// from independent tasks could arrive newest first, and an interface that
/// trusted the last one would show a stale state.
async fn emitter(client: Client, mut queue: mpsc::Receiver<(String, Value)>) {
    while let Some((method, params)) = queue.recv().await {
        let _ = client
            .request("input.emit", json!({ "method": method, "params": params }))
            .await;
    }
}

/// Opens the second connection, announces itself, and becomes the channel task.
async fn attach_and_run(
    ipc_path: &Path,
    token: &str,
    node: String,
    mine: bool,
    tx: mpsc::Sender<Inbound>,
) {
    let channel = match PeerChannel::open(ipc_path, token).await {
        Ok(channel) => channel,
        Err(e) => {
            let _ = tx
                .send(Inbound::OpenFailed {
                    node,
                    code: format!("ATTACH_FAILED: {e}"),
                })
                .await;
            return;
        }
    };
    let (out, outbox) = mpsc::channel::<Vec<u8>>(OUTBOX);
    if tx
        .send(Inbound::Attached {
            node: node.clone(),
            out,
            mine,
        })
        .await
        .is_err()
    {
        return;
    }
    channel_task(node, channel, outbox, tx).await;
}

/// One warm channel: forward everything it says to the loop, write everything the
/// loop hands over, and report its own death.
async fn channel_task(
    node: String,
    mut channel: PeerChannel,
    mut outbox: mpsc::Receiver<Vec<u8>>,
    tx: mpsc::Sender<Inbound>,
) {
    loop {
        tokio::select! {
            frame = channel.recv() => match frame {
                Some(Ok(bytes)) => {
                    if tx.send(Inbound::Frame { node: node.clone(), bytes }).await.is_err() {
                        break;
                    }
                }
                // A frame announced above the cap is a Core or a peer not
                // speaking this primitive: the channel is over.
                Some(Err(e)) => {
                    eprintln!("[1device-input] a peer channel failed: {e}");
                    break;
                }
                None => break,
            },
            // Dropping the outbox is how the engine asks this task to end, which
            // closes the pipe and lets the Core end the channel at once rather
            // than waiting out its idle sweep.
            out = outbox.recv() => match out {
                Some(bytes) => {
                    if let Err(e) = channel.send(&bytes).await {
                        eprintln!("[1device-input] a peer channel write failed: {e}");
                        break;
                    }
                }
                None => break,
            },
        }
    }
    let _ = tx.send(Inbound::Ended { node }).await;
}

/// The whole supervised-component contract: the environment, the spawn token on
/// standard input, the client, the loop, and the exit code.
///
/// It lives in the lib rather than in `main.rs` so that the binary's only job is
/// to build a platform backend and hand it here, which is also what keeps
/// `main.rs` free of anything a test cannot reach.
pub async fn supervised<B: InputBackend>(
    backend: B,
    backend_events: mpsc::Receiver<BackendEvent>,
) -> i32 {
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};

    let Ok(ipc_path) = std::env::var(IPC_PATH_ENV) else {
        eprintln!("[1device-input] {IPC_PATH_ENV} is not set: the engine is launched by the Core");
        return 1;
    };
    let Some(data_dir) = onedevice_paths::data_dir() else {
        eprintln!("[1device-input] cannot resolve the data directory from the environment");
        return 1;
    };
    let store = match Store::open(data_dir.join("input")) {
        Ok(store) => store,
        Err(e) => {
            eprintln!("[1device-input] cannot open the engine state: {e}");
            return 1;
        }
    };

    // Contract: the spawn token is the FIRST LINE of standard input, never argv
    // (world-readable) nor the environment (inherited by descendants). Standard
    // input then stays open; its EOF is the graceful-stop signal.
    let mut stdin = BufReader::new(tokio::io::stdin());
    let mut token = String::new();
    match stdin.read_line(&mut token).await {
        Ok(_) if !token.trim().is_empty() => {}
        _ => {
            eprintln!("[1device-input] no spawn token on standard input");
            return 1;
        }
    }
    let token = token.trim().to_string();

    let (client, events) = onedevice_ipc_client::spawn(ClientConfig {
        ipc_path: PathBuf::from(&ipc_path),
        token: TokenSource::Spawn(token),
        name: "1device-input".into(),
        version: env!("CARGO_PKG_VERSION").into(),
        role: ROLE.into(),
        scopes: SCOPES.iter().map(|s| (*s).to_string()).collect(),
        topics: TOPICS.iter().map(|t| (*t).to_string()).collect(),
        optional_scopes: Vec::new(),
        optional_topics: Vec::new(),
        served_methods: SERVED_METHODS.iter().map(|m| (*m).to_string()).collect(),
        reconnect_base_delay: RECONNECT_BASE_DELAY,
        // At least the peer channel's own budget: see the module header.
        request_timeout: PEER_CHANNEL_REQUEST_TIMEOUT,
    });

    // The rest of standard input: reading it to EOF is the stop signal.
    let stdin_closed = async move {
        let mut sink = Vec::new();
        let _ = stdin.read_to_end(&mut sink).await;
    };

    match run(
        client,
        events,
        backend,
        backend_events,
        store,
        PathBuf::from(ipc_path),
        stdin_closed,
    )
    .await
    {
        Outcome::StdinClosed => 0,
        // IPC lost, incompatible, client or backend gone: exit non-zero so the
        // supervisor restarts us with a fresh, single-use spawn token.
        _ => 1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The terminal conditions, which are the supervised-component contract.
    #[test]
    fn the_terminal_conditions_map_to_their_outcomes() {
        assert!(matches!(
            classify(Some(Event::Disconnected)),
            Step::Exit(Outcome::ConnectionLost)
        ));
        assert!(matches!(
            classify(Some(Event::Incompatible { api_version: 2 })),
            Step::Exit(Outcome::Incompatible)
        ));
        assert!(matches!(classify(None), Step::Exit(Outcome::ClientEnded)));
    }

    /// Every notification this engine acts on, and the one it must ignore.
    #[test]
    fn the_events_route_to_their_steps() {
        let notify = |method: &str| {
            classify(Some(Event::Notification {
                method: method.into(),
                params: json!({}),
            }))
        };
        assert!(matches!(
            classify(Some(Event::Connected {
                granted_scopes: Vec::new(),
                api_version: 1,
            })),
            Step::Resync
        ));
        assert!(matches!(notify("peer.message"), Step::PeerMessage(_)));
        assert!(matches!(notify("peer.channel"), Step::PeerChannel(_)));
        assert!(matches!(
            notify("peer.channel_closed"),
            Step::PeerChannelClosed(_)
        ));
        assert!(matches!(notify("device.updated"), Step::DirectoryStale));
        assert!(matches!(notify("session.changed"), Step::DirectoryStale));
        assert!(matches!(notify("clipboard.remote_updated"), Step::Idle));
    }

    /// The served list and the engine's own dispatch must agree, or a method
    /// would be routed here and refused with `-32601` by the engine itself.
    #[test]
    fn every_served_method_is_an_input_gesture() {
        assert_eq!(SERVED_METHODS.len(), 10);
        for method in SERVED_METHODS {
            assert!(method.starts_with("input."), "{method}");
            assert_ne!(
                method, "input.emit",
                "the engine publishes, it does not serve"
            );
        }
    }

    /// The request budget must not be tighter than the peer channel's own, or
    /// `NO_DIRECT_PATH` would arrive as a bare timeout.
    #[test]
    fn the_request_budget_covers_a_peer_channel_open() {
        assert!(PEER_CHANNEL_REQUEST_TIMEOUT >= Duration::from_secs(25));
    }
}
