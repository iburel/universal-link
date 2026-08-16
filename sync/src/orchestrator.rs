// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

//! The engine's event loop: bridges the IPC client to the sans-wire engine
//! core. Facade calls are answered from the engine's snapshot; the
//! directory (`devices.list`) names this device and translates node ids to
//! device ids; `peer.message` payloads go through [`crate::engine::Engine::on_message`]
//! and whatever comes back rides `peers.send`; reachability changes and
//! the safety tick drive [`crate::engine::Engine::pump`].
//!
//! Blocking discipline: replies and sends ride their own tasks (a Core
//! that stops draining must not hold the stop signal past the
//! supervisor's grace); directory refreshes come back through an internal
//! channel rather than being awaited inline.

use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::time::Duration;

use onedevice_ipc_client::{Client, Event, RequestError, RequestId};
use serde_json::{Value, json};
use tokio::sync::mpsc;

use crate::engine::{Effect, Engine};
use crate::store::Store;
use crate::watcher::{DEBOUNCE, WatchHandle};

/// The frozen vocabulary (doc/sync-engine.md, section 10), whole: anything
/// else in the namespace is refused with `-32601` by the IPC client itself,
/// and the Core relays that refusal verbatim to the caller.
pub const SERVED_METHODS: [&str; 9] = [
    "sync.status",
    "sync.create",
    "sync.invite",
    "sync.accept",
    "sync.decline",
    "sync.pause",
    "sync.resume",
    "sync.leave",
    "sync.resolve",
];

/// The slow safety net (doc/sync-engine.md, section 4): rounds also run on
/// reachability changes and message receipt; this tick catches what those
/// miss, and drives the periodic rescan until the watcher brick lands.
pub const SAFETY_TICK: Duration = Duration::from_secs(15 * 60);

/// Why the loop ended - mapped by `main` to a process exit code.
#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    /// Standard input closed: the supervisor asked us to stop. Exit success.
    StdinClosed,
    /// The IPC connection dropped after being established. The spawn token is
    /// single-use - exit so the supervisor restarts us with a fresh one.
    ConnectionLost,
    /// The Core announced an incompatible API version: retrying will not heal
    /// it.
    Incompatible,
    /// The client task ended on its own (no `Client` left).
    ClientEnded,
}

/// One step derived from an IPC event. Pure, so the exit conditions - the
/// supervised-component contract - are unit-tested without a Core.
enum Action {
    /// A forwarded facade call.
    Facade(RequestId, String, Value),
    /// Connection established: the directory must be (re)resolved.
    Resync,
    /// A `peer.message` notification: a dialect payload from a sibling.
    PeerMessage(Value),
    /// A `device.*` event: the directory changed; refresh and pump.
    DirectoryStale,
    /// A terminal `transfer.*` notification: one of our fills ended.
    Transfer(Value, bool),
    /// A connected-but-uninteresting event: nothing to do.
    Idle,
    /// The loop must end.
    Exit(Outcome),
}

fn classify(event: Option<Event>) -> Action {
    match event {
        // The client only delivers requests whose method is in
        // [`SERVED_METHODS`]; the dispatch itself matches on the name.
        Some(Event::Request { id, method, params }) => Action::Facade(id, method, params),
        Some(Event::Connected { .. }) => Action::Resync,
        Some(Event::Notification { method, params }) if method == "peer.message" => {
            Action::PeerMessage(params)
        }
        Some(Event::Notification { method, .. })
            if method.starts_with("device.") || method == "session.changed" =>
        {
            Action::DirectoryStale
        }
        Some(Event::Notification { method, params })
            if method == "transfer.finished" || method == "transfer.failed" =>
        {
            let ok = method == "transfer.finished";
            Action::Transfer(params, ok)
        }
        Some(Event::Notification { .. }) => Action::Idle,
        Some(Event::Disconnected) => Action::Exit(Outcome::ConnectionLost),
        Some(Event::Incompatible { .. }) => Action::Exit(Outcome::Incompatible),
        None => Action::Exit(Outcome::ClientEnded),
    }
}

/// What the spawned side tasks report back into the loop.
enum Internal {
    /// A fresh `devices.list` snapshot (or `None`: the Core knows of no
    /// device at all - not joined yet; retried on the next event).
    Directory(Option<Value>),
    /// `transactions.publish` came back for a need (`None` = refused).
    Published {
        to: String,
        set_id: String,
        need_id: u64,
        tx_id: Option<String>,
    },
    /// The adopt-and-fill choreography reached a running transfer.
    PullStarted {
        set_id: String,
        need_id: u64,
        transfer_id: String,
    },
    /// The adopt or the fill refused outright.
    PullFailed { set_id: String, need_id: u64 },
    /// Scan a freshly created or accepted set: the gesture returned long
    /// before this (the facade's 10 s budget), progress shows up on the
    /// `sync` topic.
    Scan { set_id: String },
}

/// The directory's translation tables, rebuilt from every snapshot.
#[derive(Default)]
struct Directory {
    self_node: Option<String>,
    device_of: BTreeMap<String, String>,
    reachable: Vec<String>,
    view: crate::engine::DirectoryView,
}

impl Directory {
    fn parse(snapshot: &Value) -> Directory {
        let mut dir = Directory::default();
        let Some(rows) = snapshot.as_array() else {
            return dir;
        };
        for row in rows {
            let (Some(node), Some(device)) = (
                row.get("node_id").and_then(Value::as_str),
                row.get("device_id").and_then(Value::as_str),
            ) else {
                continue;
            };
            dir.device_of.insert(node.to_string(), device.to_string());
            dir.view
                .device_of
                .insert(node.to_string(), device.to_string());
            if let Some(name) = row.get("name").and_then(Value::as_str) {
                dir.view.names.insert(node.to_string(), name.to_string());
            }
            if let Some(platform) = row.get("platform").and_then(Value::as_str) {
                dir.view
                    .platforms
                    .insert(node.to_string(), platform.to_string());
            }
            if let Some(last_seen) = row.get("last_seen") {
                dir.view
                    .last_seen
                    .insert(node.to_string(), last_seen.clone());
            }
            if row.get("is_self").and_then(Value::as_bool) == Some(true) {
                dir.self_node = Some(node.to_string());
            } else if row.get("reachable").and_then(Value::as_bool) == Some(true) {
                dir.reachable.push(node.to_string());
                dir.view.reachable.insert(node.to_string());
            }
        }
        dir
    }

    fn node_of(&self, device_id: &str) -> Option<String> {
        self.device_of
            .iter()
            .find(|(_, d)| d.as_str() == device_id)
            .map(|(n, _)| n.clone())
    }
}

struct Loop {
    client: Client,
    store: Option<Store>,
    engine: Option<Engine>,
    directory: Directory,
    internal_tx: mpsc::Sender<Internal>,
    /// One living watch per rooted set; kept in step by `sync_watchers`.
    watchers: BTreeMap<String, WatchHandle>,
    quiesced_tx: mpsc::Sender<String>,
    /// The snapshot the notifications are diffed against: they echo what
    /// `sync.status` will say, never lead it.
    last_snapshot: Value,
}

/// Runs the engine until a terminal condition. Consumes the Core `events`
/// stream; `stdin_closed` resolves on the supervisor's graceful-stop
/// signal; `tick` is the safety-net period (production: [`SAFETY_TICK`]).
pub async fn run(
    client: Client,
    mut events: mpsc::Receiver<Event>,
    store: Store,
    stdin_closed: impl Future<Output = ()>,
    tick: Duration,
) -> Outcome {
    tokio::pin!(stdin_closed);
    let (internal_tx, mut internal_rx) = mpsc::channel::<Internal>(8);
    let (quiesced_tx, mut quiesced_rx) = mpsc::channel::<String>(64);
    let mut state = Loop {
        client,
        store: Some(store),
        engine: None,
        directory: Directory::default(),
        internal_tx,
        watchers: BTreeMap::new(),
        quiesced_tx,
        last_snapshot: empty_status(),
    };
    let mut ticker = tokio::time::interval(tick);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            biased;
            () = &mut stdin_closed => break Outcome::StdinClosed,
            event = events.recv() => match classify(event) {
                Action::Facade(id, method, params) => state.on_facade(id, &method, &params),
                Action::Resync | Action::DirectoryStale => state.request_directory(),
                Action::PeerMessage(params) => state.on_peer_message(&params),
                Action::Transfer(params, ok) => {
                    let Some(transfer_id) = params
                        .get("transfer_id")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                    else {
                        continue;
                    };
                    let effects = match &mut state.engine {
                        Some(engine) => engine.on_transfer_outcome(&transfer_id, ok),
                        None => Vec::new(),
                    };
                    state.execute(effects);
                    state.emit_changes();
                }
                Action::Idle => {}
                Action::Exit(outcome) => break outcome,
            },
            Some(internal) = internal_rx.recv() => match internal {
                Internal::Directory(snapshot) => state.on_directory(snapshot),
                Internal::Published { to, set_id, need_id, tx_id } => {
                    let effects = match &mut state.engine {
                        Some(engine) => match tx_id {
                            Some(tx_id) => engine.on_published(&to, &set_id, need_id, &tx_id),
                            None => {
                                engine.on_publish_failed(&to, &set_id, need_id);
                                Vec::new()
                            }
                        },
                        None => Vec::new(),
                    };
                    state.execute(effects);
                }
                Internal::PullStarted { set_id, need_id, transfer_id } => {
                    if let Some(engine) = &mut state.engine {
                        engine.on_pull_started(&set_id, need_id, &transfer_id);
                    }
                }
                Internal::PullFailed { set_id, need_id } => {
                    if let Some(engine) = &mut state.engine {
                        engine.on_pull_failed(&set_id, need_id);
                    }
                }
                Internal::Scan { set_id } => {
                    if let Some(engine) = &mut state.engine
                        && let Err(e) = engine.rescan_set(&set_id)
                    {
                        eprintln!("[1device-sync] first scan of {set_id} failed: {e}");
                    }
                    state.sync_watchers();
                    state.pump(true);
                    state.emit_changes();
                }
            },
            Some(set_id) = quiesced_rx.recv() => state.on_quiesced(&set_id),
            _ = ticker.tick() => state.on_tick(),
        }
    }
}

impl Loop {
    /// One forwarded facade call: everything is local work, so the answer
    /// is computed inline and only the REPLY rides a task (a Core that
    /// stopped draining must not hold the loop).
    fn on_facade(&mut self, id: RequestId, method: &str, params: &Value) {
        let outcome = self.serve(method, params);
        let client = self.client.clone();
        tokio::spawn(async move {
            let sent = match outcome {
                Ok(result) => client.respond(id, result).await,
                // A shape refusal is the JSON-RPC `-32602`, not an
                // application state (the letter, section 10).
                Err(what) if what.starts_with("param:") => {
                    client
                        .respond_invalid_params(id, what.trim_start_matches("param:"))
                        .await
                }
                Err(code) => client.respond_error(id, code).await,
            };
            swallow_stale(sent);
        });
        self.emit_changes();
    }

    /// The frozen vocabulary, served. `Err` carries the engine's own
    /// application code, relayed verbatim by the Core.
    fn serve(&mut self, method: &str, params: &Value) -> Result<Value, &'static str> {
        if method == "sync.status" {
            return Ok(self
                .engine
                .as_ref()
                .map(Engine::status)
                .unwrap_or_else(empty_status));
        }
        // Every gesture needs the engine, which needs the directory: a Core
        // that has joined nothing has no sets to manage either.
        if self.engine.is_none() {
            self.request_directory();
            return Err("SYNC_NOT_READY");
        }
        let now = unix_now();
        match method {
            "sync.create" => {
                let path = abs_path(params.get("path"))?;
                let device_ids = params
                    .get("device_ids")
                    .map(device_id_list)
                    .transpose()?
                    .unwrap_or_default();
                let meta = std::fs::symlink_metadata(&path).map_err(|_| "SYNC_ROOT_UNKNOWN")?;
                if meta.file_type().is_symlink() {
                    return Err("param:path");
                }
                let engine = self.engine.as_mut().expect("checked");
                if engine.overlaps(&path) {
                    return Err("SYNC_ROOT_OVERLAP");
                }
                let kind = if meta.is_dir() {
                    crate::records::SetKind::Dir
                } else {
                    crate::records::SetKind::File
                };
                if kind == crate::records::SetKind::Dir
                    && path.join(crate::wirepath::STAGING_DIR).exists()
                {
                    // The reserved name is ours: a pre-existing user entry
                    // by that name is refused up front, never shadowed.
                    return Err("SYNC_ROOT_RESERVED");
                }
                let name = path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("set")
                    .to_string();
                // Resolve every invitee BEFORE minting: a refused gesture
                // leaves no half-made set behind.
                let mut invitees = Vec::new();
                for device_id in &device_ids {
                    let node = self
                        .directory
                        .node_of(device_id)
                        .ok_or("SYNC_DEVICE_INELIGIBLE")?;
                    engine
                        .invite_eligible(&node)
                        .map_err(|_| "SYNC_DEVICE_INELIGIBLE")?;
                    invitees.push(node);
                }
                let set_id = engine
                    .create_set(path, kind, name, now)
                    .map_err(|_| "SYNC_INTERNAL")?;
                for node in invitees {
                    let _ = engine.invite(&set_id, &node, now);
                }
                self.schedule_scan(&set_id);
                Ok(json!({ "set_id": set_id }))
            }
            "sync.invite" => {
                let set_id = set_id_of(params)?;
                let device_ids =
                    device_id_list(params.get("device_ids").ok_or("param:device_ids")?)?;
                let nodes: Vec<String> = device_ids
                    .iter()
                    .map(|device_id| {
                        let node = self
                            .directory
                            .node_of(device_id)
                            .ok_or("SYNC_DEVICE_INELIGIBLE")?;
                        self.engine
                            .as_ref()
                            .expect("checked")
                            .invite_eligible(&node)
                            .map_err(|_| "SYNC_DEVICE_INELIGIBLE")?;
                        Ok(node)
                    })
                    .collect::<Result<_, &'static str>>()?;
                let engine = self.engine.as_mut().expect("checked");
                if engine.set(&set_id).is_none() {
                    return Err("SYNC_UNKNOWN_SET");
                }
                for node in nodes {
                    engine
                        .invite(&set_id, &node, now)
                        .map_err(|_| "SYNC_DEVICE_INELIGIBLE")?;
                }
                self.pump(false);
                Ok(json!({}))
            }
            "sync.accept" => {
                let set_id = set_id_of(params)?;
                let path = abs_path(params.get("path"))?;
                let engine = self.engine.as_mut().expect("checked");
                if engine.set(&set_id).is_none() {
                    return Err("SYNC_UNKNOWN_SET");
                }
                if !engine.is_invited(&set_id) {
                    return Err("SYNC_NOT_INVITED");
                }
                if engine.overlaps(&path) {
                    return Err("SYNC_ROOT_OVERLAP");
                }
                // v1: an empty (or absent) target. Merging a pre-existing
                // tree at accept time is a conflict storm by construction.
                match std::fs::read_dir(&path) {
                    Ok(mut entries) => {
                        if entries.next().is_some() {
                            return Err("SYNC_ROOT_NOT_EMPTY");
                        }
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                        std::fs::create_dir_all(&path).map_err(|_| "SYNC_ROOT_UNKNOWN")?;
                    }
                    Err(_) => return Err("SYNC_ROOT_NOT_EMPTY"),
                }
                engine
                    .accept(&set_id, path, now)
                    .map_err(|_| "SYNC_NOT_INVITED")?;
                self.schedule_scan(&set_id);
                Ok(json!({}))
            }
            "sync.decline" | "sync.pause" | "sync.resume" | "sync.leave" => {
                let set_id = set_id_of(params)?;
                let status = match method {
                    "sync.decline" => crate::records::MemberStatus::Declined,
                    "sync.pause" => crate::records::MemberStatus::Paused,
                    "sync.resume" => crate::records::MemberStatus::Active,
                    _ => crate::records::MemberStatus::Left,
                };
                let engine = self.engine.as_mut().expect("checked");
                if engine.set(&set_id).is_none() {
                    return Err("SYNC_UNKNOWN_SET");
                }
                if method == "sync.decline" && !engine.is_invited(&set_id) {
                    return Err("SYNC_NOT_INVITED");
                }
                engine
                    .sign_status(&set_id, status, now)
                    .map_err(|_| "SYNC_INTERNAL")?;
                if method == "sync.resume"
                    && let Err(e) = self.engine.as_mut().expect("checked").rescan_set(&set_id)
                {
                    eprintln!("[1device-sync] rescan at resume failed: {e}");
                }
                self.sync_watchers();
                self.pump(true);
                Ok(json!({}))
            }
            "sync.resolve" => {
                let set_id = set_id_of(params)?;
                let path = params
                    .get("path")
                    .and_then(Value::as_str)
                    .ok_or("param:path")?;
                let keep = params
                    .get("keep")
                    .and_then(Value::as_str)
                    .ok_or("param:keep")?;
                let engine = self.engine.as_mut().expect("checked");
                if engine.set(&set_id).is_none() {
                    return Err("SYNC_UNKNOWN_SET");
                }
                engine
                    .resolve(&set_id, path, keep)
                    .map_err(|_| "SYNC_NO_CONFLICT")?;
                self.pump(false);
                Ok(json!({}))
            }
            // The client's served-methods gate makes this unreachable while
            // the two lists agree; refusing honestly beats a dropped reply.
            _ => Err("-32601"),
        }
    }

    fn schedule_scan(&self, set_id: &str) {
        let tx = self.internal_tx.clone();
        let set_id = set_id.to_string();
        tokio::spawn(async move {
            let _ = tx.send(Internal::Scan { set_id }).await;
        });
    }

    /// Publishes what changed since the last snapshot, on the `sync` topic:
    /// the notifications ECHO the authoritative status, never lead it.
    fn emit_changes(&mut self) {
        let Some(engine) = &self.engine else {
            return;
        };
        let snapshot = engine.status();
        if snapshot == self.last_snapshot {
            return;
        }
        let cards = |v: &Value| -> BTreeMap<String, Value> {
            v.get("sets")
                .and_then(Value::as_array)
                .map(|sets| {
                    sets.iter()
                        .filter_map(|card| {
                            card.get("set_id")
                                .and_then(Value::as_str)
                                .map(|id| (id.to_string(), card.clone()))
                        })
                        .collect()
                })
                .unwrap_or_default()
        };
        let invitations = |v: &Value| -> BTreeMap<String, Value> {
            v.get("invitations")
                .and_then(Value::as_array)
                .map(|list| {
                    list.iter()
                        .filter_map(|inv| {
                            inv.get("set_id")
                                .and_then(Value::as_str)
                                .map(|id| (id.to_string(), inv.clone()))
                        })
                        .collect()
                })
                .unwrap_or_default()
        };
        let (was, now) = (cards(&self.last_snapshot), cards(&snapshot));
        let (was_inv, now_inv) = (invitations(&self.last_snapshot), invitations(&snapshot));
        let mut emissions: Vec<(String, Value)> = Vec::new();
        for (set_id, card) in &now {
            if was.get(set_id) == Some(card) {
                continue;
            }
            emissions.push(("sync.updated".into(), json!({ "set": card })));
            // A conflict the card did not carry before: announced AFTER
            // materialization, per the contract.
            let before: BTreeSet<String> = conflict_ids(was.get(set_id));
            for conflict in card
                .get("conflicts")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                let id = conflict
                    .get("conflict_id")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                if !before.contains(id) {
                    emissions.push((
                        "sync.conflict".into(),
                        json!({ "set_id": set_id, "conflict": conflict }),
                    ));
                }
            }
        }
        for set_id in was.keys() {
            if !now.contains_key(set_id) && !now_inv.contains_key(set_id) {
                emissions.push(("sync.removed".into(), json!({ "set_id": set_id })));
            }
        }
        for (set_id, invitation) in &now_inv {
            if was_inv.get(set_id) != Some(invitation) {
                emissions.push((
                    "sync.invitation".into(),
                    json!({ "invitation": invitation }),
                ));
            }
        }
        self.last_snapshot = snapshot;
        for (method, params) in emissions {
            let client = self.client.clone();
            tokio::spawn(async move {
                let _ = client
                    .request("sync.emit", json!({ "method": method, "params": params }))
                    .await;
            });
        }
    }

    /// Fires a `devices.list` on its own task; the snapshot comes back as
    /// [`Internal::Directory`].
    fn request_directory(&self) {
        let client = self.client.clone();
        let tx = self.internal_tx.clone();
        tokio::spawn(async move {
            let snapshot = match client.request("devices.list", json!({})).await {
                Ok(snapshot) => Some(snapshot),
                // SERVER_UNREACHABLE = a Core that knows of no device at
                // all (never logged in, never joined): the engine waits.
                Err(RequestError::Rpc(e))
                    if e.data_code.as_deref() == Some("SERVER_UNREACHABLE") =>
                {
                    None
                }
                Err(e) => {
                    eprintln!("[1device-sync] devices.list failed: {e}");
                    None
                }
            };
            let _ = tx.send(Internal::Directory(snapshot)).await;
        });
    }

    fn on_directory(&mut self, snapshot: Option<Value>) {
        let Some(snapshot) = snapshot else {
            return;
        };
        let directory = Directory::parse(&snapshot);
        let Some(self_node) = directory.self_node.clone() else {
            return;
        };
        self.directory = directory;
        if let Some(engine) = &mut self.engine {
            engine.set_directory(self.directory.view.clone());
        }
        if self.engine.is_none()
            && let Some(store) = self.store.take()
        {
            match Engine::open(store, self_node, unix_now()) {
                Ok(mut engine) => {
                    engine.set_directory(self.directory.view.clone());
                    self.engine = Some(engine);
                    self.sync_watchers();
                }
                // A corrupt state is deliberately NOT self-healing: report
                // and serve nothing (the status stays empty; the facade
                // stays honest through COMPONENT... the snapshot).
                Err(e) => {
                    eprintln!("[1device-sync] cannot open the engine state: {e}");
                    return;
                }
            }
        }
        self.pump(false);
    }

    fn on_peer_message(&mut self, params: &Value) {
        let Some(device_id) = params.get("device_id").and_then(Value::as_str) else {
            return;
        };
        let Some(payload) = params.get("payload") else {
            return;
        };
        let Some(node) = self.directory.node_of(device_id) else {
            // A sender the directory cannot name is dropped (the letter,
            // section 1) - and the directory is probably stale: refresh.
            self.request_directory();
            return;
        };
        let Some(engine) = &mut self.engine else {
            return;
        };
        let out = engine.on_message(&node, payload, unix_now());
        self.execute(out);
        self.emit_changes();
    }

    /// The watcher went quiet after a burst: look again, then talk.
    fn on_quiesced(&mut self, set_id: &str) {
        let Some(engine) = &mut self.engine else {
            return;
        };
        if let Err(e) = engine.rescan_set(set_id) {
            eprintln!("[1device-sync] rescan of {set_id} failed: {e}");
        }
        self.pump(false);
        self.emit_changes();
    }

    /// Keeps one watch alive per rooted set: install what is missing, drop
    /// what no longer is. An installation failure degrades that set to the
    /// periodic scanning of the safety tick, loudly.
    fn sync_watchers(&mut self) {
        let Some(engine) = &self.engine else {
            return;
        };
        let mut wanted: BTreeMap<String, (std::path::PathBuf, crate::records::SetKind)> =
            BTreeMap::new();
        for set_id in engine.set_ids() {
            if let Some(state) = engine.set(&set_id)
                && let Some(root) = &state.root
            {
                wanted.insert(set_id, (root.clone(), state.membership.descriptor.kind));
            }
        }
        self.watchers
            .retain(|set_id, _| wanted.contains_key(set_id));
        for (set_id, (root, kind)) in wanted {
            if self.watchers.contains_key(&set_id) {
                continue;
            }
            match crate::watcher::watch(&set_id, &root, kind, DEBOUNCE, self.quiesced_tx.clone()) {
                Ok(handle) => {
                    self.watchers.insert(set_id, handle);
                }
                Err(e) => {
                    eprintln!("[1device-sync] set {set_id} degraded to periodic scanning: {e}");
                }
            }
        }
    }

    fn on_tick(&mut self) {
        let Some(engine) = &mut self.engine else {
            // Not resolved yet (or never joined): keep asking.
            self.request_directory();
            return;
        };
        // The periodic rescan, until the watcher brick moves detection off
        // the tick. Synchronous by design for now: the loop owns the
        // engine, and the tick is rare.
        for set_id in engine.set_ids() {
            if engine.set(&set_id).is_some_and(|s| s.root.is_some())
                && let Err(e) = engine.rescan_set(&set_id)
            {
                eprintln!("[1device-sync] rescan of {set_id} failed: {e}");
            }
        }
        self.pump(true);
        self.sync_watchers();
        self.emit_changes();
        // The directory refreshes itself on `device.*` events; the tick only
        // retries when it is missing entirely (a Core that has joined
        // nothing yet), so a resolved engine never polls.
        if self.directory.self_node.is_none() {
            self.request_directory();
        }
    }

    fn pump(&mut self, force: bool) {
        let reachable = self.directory.reachable.clone();
        let Some(engine) = &mut self.engine else {
            return;
        };
        let out = engine.pump(&reachable, unix_now(), force);
        self.execute(out);
    }

    /// Executes the engine's effects: sends ride `peers.send`, the byte
    /// choreography rides the transactions API, everything on its own task
    /// and best-effort by design (the pending set re-needs what a refusal
    /// drops).
    fn execute(&mut self, effects: Vec<Effect>) {
        for effect in effects {
            match effect {
                Effect::Send(message) => {
                    let Some(device_id) = self.directory.device_of.get(&message.to).cloned() else {
                        continue;
                    };
                    let client = self.client.clone();
                    tokio::spawn(async move {
                        // Ordinary refusals: the peer is away, or holds no
                        // engine. The pump retries on the next trigger.
                        let _ = client
                            .request(
                                "peers.send",
                                json!({ "device_id": device_id, "payload": message.payload }),
                            )
                            .await;
                    });
                }
                Effect::Publish {
                    to,
                    set_id,
                    need_id,
                    paths,
                } => {
                    let client = self.client.clone();
                    let tx = self.internal_tx.clone();
                    tokio::spawn(async move {
                        let paths: Vec<String> = paths
                            .iter()
                            .map(|p| p.to_string_lossy().into_owned())
                            .collect();
                        let tx_id = client
                            .request("transactions.publish", json!({ "paths": paths }))
                            .await
                            .ok()
                            .and_then(|r| {
                                r.get("tx_id").and_then(Value::as_str).map(str::to_string)
                            });
                        let _ = tx
                            .send(Internal::Published {
                                to,
                                set_id,
                                need_id,
                                tx_id,
                            })
                            .await;
                    });
                }
                Effect::Revoke { tx_id } => {
                    let client = self.client.clone();
                    tokio::spawn(async move {
                        let _ = client
                            .request("transactions.revoke", json!({ "tx_id": tx_id }))
                            .await;
                    });
                }
                Effect::AdoptFill {
                    from,
                    set_id,
                    need_id,
                    tx_id,
                    files,
                    staging,
                } => {
                    let Some(device_id) = self.directory.device_of.get(&from).cloned() else {
                        continue;
                    };
                    let client = self.client.clone();
                    let tx = self.internal_tx.clone();
                    tokio::spawn(async move {
                        let failed = |tx: &mpsc::Sender<Internal>| {
                            let set_id = set_id.clone();
                            let tx = tx.clone();
                            async move {
                                let _ = tx.send(Internal::PullFailed { set_id, need_id }).await;
                            }
                        };
                        let Ok(record) = client
                            .request(
                                "transactions.adopt",
                                json!({ "device_id": device_id, "tx_id": tx_id }),
                            )
                            .await
                        else {
                            failed(&tx).await;
                            return;
                        };
                        if std::fs::create_dir_all(&staging).is_err() {
                            failed(&tx).await;
                            return;
                        }
                        // Fill only what the offer's map names, each under
                        // its published basename; dest paths are OURS, from
                        // the adopted (Core-validated) record, never a
                        // peer's claim.
                        let mut entries = Vec::new();
                        for row in record
                            .get("files")
                            .and_then(Value::as_array)
                            .into_iter()
                            .flatten()
                        {
                            let (Some(file_id), Some(path)) = (
                                row.get("file_id").and_then(Value::as_str),
                                row.get("path").and_then(Value::as_str),
                            ) else {
                                continue;
                            };
                            if !files.contains_key(path) {
                                continue;
                            }
                            let dest = staging.join(path);
                            entries.push(json!({
                                "file_id": file_id,
                                "dest_path": dest.to_string_lossy(),
                            }));
                        }
                        if entries.is_empty() {
                            failed(&tx).await;
                            return;
                        }
                        let Ok(reply) = client
                            .request(
                                "transactions.fill",
                                json!({ "tx_id": tx_id, "entries": entries }),
                            )
                            .await
                        else {
                            failed(&tx).await;
                            return;
                        };
                        let Some(transfer_id) = reply
                            .get("transfer_id")
                            .and_then(Value::as_str)
                            .map(str::to_string)
                        else {
                            failed(&tx).await;
                            return;
                        };
                        let _ = tx
                            .send(Internal::PullStarted {
                                set_id,
                                need_id,
                                transfer_id,
                            })
                            .await;
                    });
                }
            }
        }
    }
}

fn empty_status() -> Value {
    json!({ "sets": [], "invitations": [] })
}

fn conflict_ids(card: Option<&Value>) -> BTreeSet<String> {
    card.and_then(|c| c.get("conflicts"))
        .and_then(Value::as_array)
        .map(|list| {
            list.iter()
                .filter_map(|c| {
                    c.get("conflict_id")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                })
                .collect()
        })
        .unwrap_or_default()
}

fn set_id_of(params: &Value) -> Result<String, &'static str> {
    let set_id = params
        .get("set_id")
        .and_then(Value::as_str)
        .ok_or("param:set_id")?;
    if !crate::records::valid_set_id(set_id) {
        // Shaped wrong: not a set this engine could ever hold.
        return Err("SYNC_UNKNOWN_SET");
    }
    Ok(set_id.to_string())
}

/// A root path from a gesture: absolute, and a plain string the JSON
/// control plane can express.
fn abs_path(value: Option<&Value>) -> Result<std::path::PathBuf, &'static str> {
    let path = value.and_then(Value::as_str).ok_or("param:path")?;
    let path = std::path::PathBuf::from(path);
    if !path.is_absolute() {
        return Err("param:path");
    }
    Ok(path)
}

fn device_id_list(value: &Value) -> Result<Vec<String>, &'static str> {
    let list = value.as_array().ok_or("param:device_ids")?;
    list.iter()
        .map(|v| v.as_str().map(str::to_string).ok_or("param:device_ids"))
        .collect()
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// A stale request id (its connection dropped mid-flight) is expected
/// around a reconnect; anything else deserves a trace.
fn swallow_stale(result: Result<(), RequestError>) {
    if let Err(e) = result
        && !matches!(e, RequestError::Disconnected)
    {
        eprintln!("[1device-sync] reply failed: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_terminal_conditions_map_to_their_outcomes() {
        assert!(matches!(
            classify(Some(Event::Disconnected)),
            Action::Exit(Outcome::ConnectionLost)
        ));
        assert!(matches!(
            classify(Some(Event::Incompatible { api_version: 2 })),
            Action::Exit(Outcome::Incompatible)
        ));
        assert!(matches!(classify(None), Action::Exit(Outcome::ClientEnded)));
    }

    #[test]
    fn the_events_route_to_their_actions() {
        assert!(matches!(
            classify(Some(Event::Connected {
                granted_scopes: Vec::new(),
                api_version: 1,
            })),
            Action::Resync
        ));
        assert!(matches!(
            classify(Some(Event::Notification {
                method: "peer.message".into(),
                params: serde_json::json!({}),
            })),
            Action::PeerMessage(_)
        ));
        assert!(matches!(
            classify(Some(Event::Notification {
                method: "device.updated".into(),
                params: serde_json::json!({}),
            })),
            Action::DirectoryStale
        ));
        assert!(matches!(
            classify(Some(Event::Notification {
                method: "transfer.finished".into(),
                params: serde_json::json!({}),
            })),
            Action::Transfer(_, true)
        ));
        assert!(matches!(
            classify(Some(Event::Notification {
                method: "transfer.failed".into(),
                params: serde_json::json!({}),
            })),
            Action::Transfer(_, false)
        ));
        assert!(matches!(
            classify(Some(Event::Notification {
                method: "clipboard.remote_updated".into(),
                params: serde_json::json!({}),
            })),
            Action::Idle
        ));
    }

    #[test]
    fn the_directory_parses_self_and_reachable_rows() {
        let snapshot = serde_json::json!([
            { "device_id": "d_1", "node_id": "aa", "is_self": true, "reachable": true },
            { "device_id": "d_2", "node_id": "bb", "is_self": false, "reachable": true },
            { "device_id": "d_3", "node_id": "cc", "is_self": false, "reachable": false },
            { "no_node": true },
        ]);
        let dir = Directory::parse(&snapshot);
        assert_eq!(dir.self_node.as_deref(), Some("aa"));
        assert_eq!(dir.reachable, ["bb"]);
        assert_eq!(dir.node_of("d_2").as_deref(), Some("bb"));
        assert_eq!(dir.node_of("d_9"), None);
    }
}
