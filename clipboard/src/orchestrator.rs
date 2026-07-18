// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

//! The OS-agnostic orchestrator: the announce / promise / session state machine
//! that bridges the Core (the IPC client) and a platform backend (the
//! [`ClipboardBackend`] seam).
//!
//! Source side (this device copied): a `Copied` upcall → `clipboard.updated`
//! (an announce that supersedes the previous transaction); a `clipboard.get_data`
//! request → open a provider channel, ask the backend for the current bytes,
//! stream `DATA`/`EOF`, then reply (the reply is the completion signal), or
//! `CLIP_STALE` if the backend can no longer vouch for the generation.
//!
//! Destination side (a remote device copied): a `clipboard.remote_updated`
//! notification → the backend takes ownership of the OS clipboard with a promise.
//! An inline clip (`text`, `image/png`) then renders lazily: a `Paste` upcall →
//! `transactions.open` → a consumer channel → `FETCH` → the bytes go to the
//! backend; any error refuses the paste cleanly. A files clip takes a different
//! path — there is no paste event to fetch on. It is promised through
//! `offer_files` with a [`CoreFetcher`]: the files backend serves byte ranges on
//! demand (`transactions.open` → a consumer channel → `READ`), one pull per OS
//! `read`, so bytes are still fetched at paste time, not at offer. A backend
//! whose paste surface is on-disk destinations rather than a demand-read
//! filesystem (macOS) instead calls [`FileFetcher::fill`] on that same handle:
//! the Core writes the files itself (`transactions.fill`, fire-and-forget) and
//! the orchestrator routes the out-of-band `transfer.finished`/`transfer.failed`
//! back to the blocked paste so it can complete or refuse cleanly.
//!
//! Lifecycle: born at the announce, superseded by the next announce (own or a
//! newer remote — the Core converges globally and only notifies the winner),
//! deleted once superseded with zero sessions. The component never blindly
//! re-announces at startup: it resynchronizes with `clipboard.current` and only
//! announces on the next observed change (the anti-echo contract, extended).

use std::future::Future;
use std::ops::ControlFlow;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};
use tokio::sync::{broadcast, mpsc};
use universallink_ipc_client::{
    ChannelError, Client, ConsumerChannel, Event, ProviderChannel, RequestError, RequestId,
};

use crate::backend::{BackendEvent, ClipboardBackend, FileFetcher, Format, RemoteClip, RemoteFile};

/// Core-normalized id of the files format.
const FORMAT_FILES: &str = "files";

/// Whether a remote clip is a FILES clip (its formats include `files`), which
/// the destination side promises through the on-demand [`CoreFetcher`] path
/// rather than the inline `Paste`/`deliver` path.
fn is_files_clip(clip: &RemoteClip) -> bool {
    clip.formats.iter().any(|f| f.id == FORMAT_FILES)
}

/// Why the orchestrator loop ended — mapped by `main` to a process exit code.
#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    /// Standard input closed: the supervisor asked us to stop. Exit success.
    StdinClosed,
    /// The IPC connection dropped after being established. The spawn token is
    /// single-use — exit so the supervisor restarts us with a fresh one.
    ConnectionLost,
    /// The Core announced an incompatible API version: retrying will not heal
    /// it.
    Incompatible,
    /// The client task ended on its own (no `Client` left).
    ClientEnded,
    /// The OS backend's event stream ended (its loop stopped): nothing left to
    /// drive.
    BackendEnded,
}

/// One step derived from an IPC event. Pure, so the exit conditions — the
/// supervised-component contract — are unit-tested without a Core. `GetData`
/// carries a [`RequestId`], which only the client crate can mint; the unit
/// tests therefore cover every variant but that one, which the integration
/// suite drives.
enum Action {
    /// Connection established: resynchronize the live promise.
    Resync,
    /// A `clipboard.remote_updated` payload: a remote device copied.
    RemoteUpdated(Value),
    /// A `clipboard.get_data` request to serve over a provider channel.
    GetData(RequestId, Value),
    /// A terminal `transfer.*` notification (fill completion) to route to a
    /// blocked [`CoreFetcher::fill`].
    Transfer(TransferOutcome),
    /// A connected-but-uninteresting event: nothing to do.
    Idle,
    /// The loop must end.
    Exit(Outcome),
}

fn classify(event: Option<Event>) -> Action {
    match event {
        Some(Event::Connected { .. }) => Action::Resync,
        Some(Event::Notification { method, params }) if method == "clipboard.remote_updated" => {
            Action::RemoteUpdated(params)
        }
        Some(Event::Notification { method, params })
            if method == "transfer.finished" || method == "transfer.failed" =>
        {
            Action::Transfer(parse_transfer_outcome(&method, &params))
        }
        Some(Event::Notification { .. }) => Action::Idle,
        Some(Event::Request { id, method, params }) if method == "clipboard.get_data" => {
            Action::GetData(id, params)
        }
        // The client only delivers requests whose method is in `served_methods`
        // (just `clipboard.get_data`); anything else is auto-refused there.
        Some(Event::Request { .. }) => Action::Idle,
        Some(Event::Disconnected) => Action::Exit(Outcome::ConnectionLost),
        Some(Event::Incompatible { .. }) => Action::Exit(Outcome::Incompatible),
        None => Action::Exit(Outcome::ClientEnded),
    }
}

/// The current local announce (source side): the `tx_id` the Core returned and
/// the backend `generation` it maps to.
struct Announced {
    tx_id: String,
    generation: u64,
}

/// The remote promise currently on the OS clipboard (destination side).
struct Promise {
    tx_id: String,
    formats: Vec<String>,
}

/// A terminal `transfer.*` outcome, broadcast to whichever [`CoreFetcher::fill`]
/// is awaiting that `transfer_id`. Only `transfer.finished`/`transfer.failed`
/// are published (started/progress are ignored), so the channel carries at most
/// one item per fill and can never lag it out.
#[derive(Clone, Debug)]
struct TransferOutcome {
    transfer_id: String,
    /// The written paths on success, or the Core's error string on failure.
    result: Result<Vec<PathBuf>, String>,
}

struct State<B: ClipboardBackend> {
    client: Client,
    ipc_path: PathBuf,
    backend: B,
    /// This device's id, resolved once (via `devices.list` `is_self`) for the
    /// resync self-check. `None` while unresolved.
    self_device_id: Option<String>,
    announced: Option<Announced>,
    promise: Option<Promise>,
    /// Publishes terminal `transfer.*` outcomes; each [`CoreFetcher`] subscribes
    /// to await its own fill's completion (macOS files paste).
    transfers: broadcast::Sender<TransferOutcome>,
}

/// Runs the orchestrator until a terminal condition. Consumes the Core `events`
/// stream and the backend's `backend_events` stream; `stdin_closed` resolves on
/// the supervisor's graceful-stop signal. `ipc_path` is kept to open the data
/// channels (the client does not re-expose it).
pub async fn run<B: ClipboardBackend>(
    client: Client,
    mut events: mpsc::Receiver<Event>,
    backend: B,
    ipc_path: PathBuf,
    mut backend_events: mpsc::Receiver<BackendEvent>,
    stdin_closed: impl Future<Output = ()>,
) -> Outcome {
    let mut state = State {
        client,
        ipc_path,
        backend,
        self_device_id: None,
        announced: None,
        promise: None,
        // Only terminal fill outcomes flow here (one per fill), so a small buffer
        // never lags; kept alive in `State` so `CoreFetcher`s can subscribe.
        transfers: broadcast::channel(64).0,
    };
    tokio::pin!(stdin_closed);

    let outcome = loop {
        tokio::select! {
            biased;
            () = &mut stdin_closed => break Outcome::StdinClosed,
            event = events.recv() => {
                if let ControlFlow::Break(outcome) = state.on_ipc_event(event).await {
                    break outcome;
                }
            }
            backend_event = backend_events.recv() => match backend_event {
                Some(event) => state.on_backend_event(event).await,
                None => break Outcome::BackendEnded,
            },
        }
    };

    // Release the OS clipboard promise on any exit: a dangling promise would
    // leave the desktop offering bytes we can no longer fetch.
    state.backend.release();
    outcome
}

impl<B: ClipboardBackend> State<B> {
    async fn on_ipc_event(&mut self, event: Option<Event>) -> ControlFlow<Outcome> {
        match classify(event) {
            Action::Resync => self.resync().await,
            Action::RemoteUpdated(params) => self.on_remote_updated(params),
            Action::GetData(id, params) => self.dispatch_get_data(id, params),
            // Route to the blocked fill; `Err` means no fill is waiting (the
            // transfer belongs to another component) — nothing to do.
            Action::Transfer(outcome) => {
                let _ = self.transfers.send(outcome);
            }
            Action::Idle => {}
            Action::Exit(outcome) => return ControlFlow::Break(outcome),
        }
        ControlFlow::Continue(())
    }

    async fn on_backend_event(&mut self, event: BackendEvent) {
        match event {
            BackendEvent::Copied { generation, clip } => {
                self.announce(generation, &clip).await;
            }
            BackendEvent::Cleared => self.clear().await,
            BackendEvent::Paste { format, token } => self.dispatch_paste(format, token),
        }
    }

    // --- Source side ------------------------------------------------------

    /// Announces a local copy. It supersedes the previous transaction, so it
    /// also drops any remote promise we were holding.
    async fn announce(&mut self, generation: u64, clip: &crate::backend::LocalClip) {
        // `sensitive` omits the inline size hint (doc/core-api.md): enforced HERE,
        // once, so every backend is correct by construction — a backend need only
        // set `clip.sensitive`, never remember to strip sizes too.
        let mut params = json!({ "formats": formats_to_json(&clip.formats, clip.sensitive) });
        if !clip.paths.is_empty() {
            params["paths"] = paths_to_json(&clip.paths);
        }
        if clip.sensitive {
            params["sensitive"] = json!(true);
        }
        match self.client.request("clipboard.updated", params).await {
            Ok(result) => {
                if let Some(tx_id) = result["tx_id"].as_str() {
                    self.announced = Some(Announced {
                        tx_id: tx_id.to_string(),
                        generation,
                    });
                    self.promise = None;
                } else {
                    warn("clipboard.updated returned no tx_id");
                }
            }
            Err(e) => warn(&format!("clipboard.updated failed: {e}")),
        }
    }

    /// Announces an empty clipboard (the local clipboard was cleared): a
    /// contentless transaction that supersedes like any announce.
    async fn clear(&mut self) {
        match self
            .client
            .request("clipboard.updated", json!({ "formats": [] }))
            .await
        {
            Ok(_) => {
                self.announced = None;
                self.promise = None;
            }
            Err(e) => warn(&format!("clipboard clear failed: {e}")),
        }
    }

    /// Dispatches a `clipboard.get_data` request onto its own task, so serving
    /// (a second connection + streaming) never blocks the control loop. The
    /// generation check reads the live `announced` state now; the task then
    /// serves with it captured.
    fn dispatch_get_data(&self, id: RequestId, params: Value) {
        let tx_id = params["tx_id"].as_str().unwrap_or_default().to_string();
        let format = params["format"].as_str().unwrap_or_default().to_string();
        let channel_token = params["channel_token"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        // We can only vouch for the transaction we currently have announced.
        let generation = match &self.announced {
            Some(a) if a.tx_id == tx_id => Some(a.generation),
            _ => None,
        };
        let client = self.client.clone();
        let backend = self.backend.clone();
        let ipc_path = self.ipc_path.clone();
        tokio::spawn(async move {
            serve_get_data(
                client,
                backend,
                ipc_path,
                id,
                generation,
                &format,
                &channel_token,
            )
            .await;
        });
    }

    // --- Destination side -------------------------------------------------

    fn on_remote_updated(&mut self, params: Value) {
        let clip = build_remote_clip(&params);
        if clip.formats.is_empty() {
            // The source cleared its clipboard: withdraw the promise.
            self.backend.release();
            self.promise = None;
            return;
        }
        self.promise = Some(Promise {
            tx_id: clip.tx_id.clone(),
            formats: clip.formats.iter().map(|f| f.id.clone()).collect(),
        });
        self.offer_clip(clip);
        // A remote copy that reaches us has won the global convergence: it
        // supersedes our own announce, which we can no longer serve.
        self.announced = None;
    }

    /// Hands a remote clip to the backend on the right seam. A files clip is
    /// promised through `offer_files` with a [`CoreFetcher`] bound to its
    /// transaction (on-demand `READ` at paste time); any other clip is promised
    /// inline through `offer` (bytes pulled on the eventual `Paste`).
    fn offer_clip(&self, clip: RemoteClip) {
        if is_files_clip(&clip) {
            let fetcher = CoreFetcher::new(
                clip.tx_id.clone(),
                self.ipc_path.clone(),
                self.client.clone(),
                self.transfers.clone(),
            );
            self.backend.offer_files(clip, Arc::new(fetcher));
        } else {
            self.backend.offer(clip);
        }
    }

    /// Dispatches a local paste onto its own task: `transactions.open` then a
    /// consumer-channel `FETCH`. Refuses immediately if no promise offers the
    /// requested format.
    fn dispatch_paste(&self, format: String, token: u64) {
        let tx_id = match &self.promise {
            Some(p) if p.formats.iter().any(|f| f == &format) => p.tx_id.clone(),
            _ => {
                self.backend.paste_failed(token, &format);
                return;
            }
        };
        let client = self.client.clone();
        let backend = self.backend.clone();
        let ipc_path = self.ipc_path.clone();
        tokio::spawn(async move {
            consume_paste(client, backend, ipc_path, &tx_id, &format, token).await;
        });
    }

    // --- Resync -----------------------------------------------------------

    /// On (re)connection, re-learn the live promise (`clipboard.current`) rather
    /// than blindly re-announcing. If the current clip belongs to another
    /// device, promise it to the OS; our own clip is already physically on the
    /// clipboard and must not be re-offered to ourselves.
    async fn resync(&mut self) {
        let current = match self.client.request("clipboard.current", json!({})).await {
            Ok(v) => v,
            Err(e) => {
                warn(&format!("clipboard.current failed at resync: {e}"));
                return;
            }
        };
        if current.get("tx_id").is_none() {
            return; // no live clip
        }
        let clip = build_remote_clip(&current);
        if clip.formats.is_empty() {
            return; // contentless
        }
        let device_id = current["device_id"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        if self.is_self(&device_id).await {
            return;
        }
        self.promise = Some(Promise {
            tx_id: clip.tx_id.clone(),
            formats: clip.formats.iter().map(|f| f.id.clone()).collect(),
        });
        self.offer_clip(clip);
    }

    /// Whether `device_id` is this device. Resolves our own id once via
    /// `devices.list` (`is_self`). If it cannot be resolved (no account yet, so
    /// no remote copies exist), treats the clip as our own — never offering our
    /// own clip back to ourselves.
    async fn is_self(&mut self, device_id: &str) -> bool {
        if self.self_device_id.is_none()
            && let Ok(list) = self.client.request("devices.list", json!({})).await
        {
            self.self_device_id = list
                .as_array()
                .into_iter()
                .flatten()
                .find(|d| d["is_self"].as_bool().unwrap_or(false))
                .and_then(|d| d["device_id"].as_str())
                .map(str::to_string);
        }
        match &self.self_device_id {
            Some(id) => id == device_id,
            None => true,
        }
    }
}

// --- Serving tasks (own no orchestrator state) ----------------------------

/// Serves one inline pull. Opens a provider channel, streams the blob, then
/// replies to the RPC (the reply is the completion signal). `CLIP_STALE`
/// without opening the channel if the backend can no longer vouch.
async fn serve_get_data<B: ClipboardBackend>(
    client: Client,
    backend: B,
    ipc_path: PathBuf,
    id: RequestId,
    generation: Option<u64>,
    format: &str,
    channel_token: &str,
) {
    let bytes = match generation {
        Some(generation) => backend.provide(generation, format).await,
        None => None,
    };
    let Some(bytes) = bytes else {
        respond_clip_stale(&client, id).await;
        return;
    };
    let mut provider = match ProviderChannel::open(&ipc_path, channel_token).await {
        Ok(provider) => provider,
        Err(e) => {
            warn(&format!("cannot open provider channel: {e}"));
            respond_clip_stale(&client, id).await;
            return;
        }
    };
    if let Err(e) = provider.data(0, &bytes).await {
        warn(&format!("provider write failed: {e}"));
        let _ = provider.error("CLIP_STALE").await;
        respond_clip_stale(&client, id).await;
        return;
    }
    if let Err(e) = provider.eof().await {
        warn(&format!("provider EOF failed: {e}"));
        respond_clip_stale(&client, id).await;
        return;
    }
    if let Err(e) = client.respond(id, json!({})).await {
        warn(&format!("get_data reply failed: {e}"));
    }
}

async fn respond_clip_stale(client: &Client, id: RequestId) {
    if let Err(e) = client.respond_error(id, "CLIP_STALE").await {
        // A stale request id (its connection dropped) is expected on reconnect.
        if !matches!(e, RequestError::Disconnected) {
            warn(&format!("CLIP_STALE reply failed: {e}"));
        }
    }
}

/// Consumes one local paste: opens the transaction, opens a consumer channel,
/// fetches the inline blob, and hands it to the backend. Any failure refuses
/// the paste cleanly. Dropping the channel ends the paste session.
async fn consume_paste<B: ClipboardBackend>(
    client: Client,
    backend: B,
    ipc_path: PathBuf,
    tx_id: &str,
    format: &str,
    token: u64,
) {
    let channel_token = match client
        .request("transactions.open", json!({ "tx_id": tx_id }))
        .await
    {
        Ok(result) => match result["channel_token"].as_str() {
            Some(token) => token.to_string(),
            None => {
                warn("transactions.open returned no channel_token");
                backend.paste_failed(token, format);
                return;
            }
        },
        Err(e) => {
            // TX_STALE (superseded), DEVICE_OFFLINE (source unreachable)…
            warn(&format!("transactions.open failed: {e}"));
            backend.paste_failed(token, format);
            return;
        }
    };
    let mut channel = match ConsumerChannel::open(&ipc_path, &channel_token).await {
        Ok(channel) => channel,
        Err(e) => {
            warn(&format!("cannot open consumer channel: {e}"));
            backend.paste_failed(token, format);
            return;
        }
    };
    match channel.fetch(format).await {
        Ok(bytes) => backend.deliver(token, format, bytes),
        Err(e) => {
            // CLIP_STALE, PEER_GONE, TX_STALE…
            warn(&format!("paste fetch failed: {e}"));
            backend.paste_failed(token, format);
        }
    }
}

// --- Files fetcher (destination side, on-demand READ) ---------------------

/// The [`FileFetcher`] the files backend calls to pull byte ranges of a promised
/// remote FILES clip. Bound to one transaction; opens its consumer channel
/// lazily on the first `read` and reuses it (FUSE reads are serial, and one
/// channel serves any `file_id` sequentially). A terminal channel error drops
/// the channel so a later read reopens (and fails again → clean `EIO`).
struct CoreFetcher {
    tx_id: String,
    ipc_path: PathBuf,
    client: Client,
    /// The runtime the orchestrator runs on; the fetcher's `read`/`fill` is
    /// invoked from an OS paste thread (FUSE session / macOS presenter queue, a
    /// plain OS thread), so it bridges back here with `block_on`.
    handle: tokio::runtime::Handle,
    /// The reusable consumer channel, opened on demand (`read` path only).
    channel: tokio::sync::Mutex<Option<ConsumerChannel>>,
    /// Terminal fill outcomes, subscribed to in `fill` (macOS push path).
    transfers: broadcast::Sender<TransferOutcome>,
}

/// A backstop bounding a `fill` wait so the OS paste thread can never hang
/// forever on a lost broadcast item. The Core is the real deadline authority
/// (it always emits exactly one terminal `transfer.*`), so this is deliberately
/// generous — it must never truncate a legitimately slow large transfer.
const FILL_BACKSTOP: Duration = Duration::from_secs(3600);

impl CoreFetcher {
    /// Captures the current runtime handle — the orchestrator runs inside it.
    fn new(
        tx_id: String,
        ipc_path: PathBuf,
        client: Client,
        transfers: broadcast::Sender<TransferOutcome>,
    ) -> CoreFetcher {
        CoreFetcher {
            tx_id,
            ipc_path,
            client,
            handle: tokio::runtime::Handle::current(),
            channel: tokio::sync::Mutex::new(None),
            transfers,
        }
    }

    /// Issues one `transactions.fill` for `entries` and blocks (async) until its
    /// terminal `transfer.*` arrives. Subscribes to the outcome broadcast BEFORE
    /// issuing, so a fast completion cannot be missed between the reply and the
    /// subscription.
    async fn fill_async(&self, entries: &[(String, PathBuf)]) -> std::io::Result<Vec<PathBuf>> {
        let mut rx = self.transfers.subscribe();
        let entries_json: Vec<Value> = entries
            .iter()
            .map(|(file_id, dest)| {
                json!({ "file_id": file_id, "dest_path": dest.to_string_lossy() })
            })
            .collect();
        let reply = self
            .client
            .request(
                "transactions.fill",
                json!({ "tx_id": &self.tx_id, "entries": entries_json }),
            )
            .await
            .map_err(|e| std::io::Error::other(format!("transactions.fill failed: {e}")))?;
        let Some(transfer_id) = reply["transfer_id"].as_str().map(str::to_string) else {
            return Err(std::io::Error::other(
                "transactions.fill returned no transfer_id",
            ));
        };
        let wait = async {
            loop {
                match rx.recv().await {
                    Ok(o) if o.transfer_id == transfer_id => return o.result,
                    // A different component's transfer, or a benign lag on
                    // unrelated outcomes: keep waiting for ours.
                    Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => {
                        return Err("transfers stream closed (Core gone)".to_string());
                    }
                }
            }
        };
        match tokio::time::timeout(FILL_BACKSTOP, wait).await {
            Ok(Ok(paths)) => Ok(paths),
            Ok(Err(e)) => Err(std::io::Error::other(format!("fill failed: {e}"))),
            Err(_) => Err(std::io::Error::other("fill timed out awaiting completion")),
        }
    }

    async fn read_async(&self, file_id: &str, offset: u64, len: u64) -> std::io::Result<Vec<u8>> {
        let mut guard = self.channel.lock().await;
        if guard.is_none() {
            let result = self
                .client
                .request("transactions.open", json!({ "tx_id": &self.tx_id }))
                .await
                .map_err(|e| std::io::Error::other(format!("transactions.open failed: {e}")))?;
            let Some(channel_token) = result["channel_token"].as_str() else {
                return Err(std::io::Error::other(
                    "transactions.open returned no channel_token",
                ));
            };
            let channel = ConsumerChannel::open(&self.ipc_path, channel_token)
                .await
                .map_err(|e| std::io::Error::other(format!("open consumer channel: {e}")))?;
            *guard = Some(channel);
        }
        let channel = guard.as_mut().expect("consumer channel present after open");
        match channel.read(file_id, offset, len).await {
            Ok(bytes) => Ok(bytes),
            Err(e) => {
                // A terminal error (TX_STALE / PEER_GONE) or a dead channel ends
                // the session: drop it so a later read reopens. A request-scoped
                // error (FILE_CHANGED / FILE_UNKNOWN / TIMEOUT / …) keeps the
                // channel usable for the next read.
                let session_over = match &e {
                    ChannelError::Code(code) => code.is_terminal(),
                    ChannelError::Closed | ChannelError::Io(_) => true,
                };
                if session_over {
                    *guard = None;
                }
                Err(std::io::Error::other(format!("data channel read: {e}")))
            }
        }
    }
}

impl FileFetcher for CoreFetcher {
    fn read(&self, file_id: &str, offset: u64, len: u64) -> std::io::Result<Vec<u8>> {
        // Called from the FUSE session thread (a plain OS thread, not a runtime
        // worker), so `block_on` is valid here.
        self.handle
            .block_on(async { self.read_async(file_id, offset, len).await })
    }

    fn fill(&self, entries: &[(String, PathBuf)]) -> std::io::Result<Vec<PathBuf>> {
        // Called from the macOS presenter queue (a plain OS thread), so
        // `block_on` is valid here — same discipline as `read`.
        self.handle
            .block_on(async { self.fill_async(entries).await })
    }
}

/// Parses a terminal `transfer.*` notification into a [`TransferOutcome`].
/// `transfer.finished` carries the written `paths`; `transfer.failed` carries an
/// `error` string (defaulted if absent).
fn parse_transfer_outcome(method: &str, params: &Value) -> TransferOutcome {
    let transfer_id = params["transfer_id"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    let result = if method == "transfer.finished" {
        Ok(params["paths"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|p| p.as_str())
            .map(PathBuf::from)
            .collect())
    } else {
        Err(params["error"]
            .as_str()
            .unwrap_or("transfer failed")
            .to_string())
    };
    TransferOutcome {
        transfer_id,
        result,
    }
}

// --- JSON helpers ---------------------------------------------------------

/// Serialize the announced formats. When `sensitive`, the inline `size` hint is
/// omitted for every format (the content is confidential — see doc/core-api.md);
/// this is the single OS-agnostic point that enforces that invariant.
fn formats_to_json(formats: &[Format], sensitive: bool) -> Value {
    Value::Array(
        formats
            .iter()
            .map(|f| {
                let mut object = serde_json::Map::new();
                object.insert("format".into(), json!(f.id));
                if let Some(size) = f.size
                    && !sensitive
                {
                    object.insert("size".into(), json!(size));
                }
                Value::Object(object)
            })
            .collect(),
    )
}

fn paths_to_json(paths: &[PathBuf]) -> Value {
    Value::Array(paths.iter().map(|p| json!(p.to_string_lossy())).collect())
}

fn parse_formats(formats: &Value) -> Vec<Format> {
    formats
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|f| {
            Some(Format {
                id: f["format"].as_str()?.to_string(),
                size: f["size"].as_u64(),
            })
        })
        .collect()
}

fn parse_remote_files(files: &Value) -> Vec<RemoteFile> {
    files
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|f| {
            Some(RemoteFile {
                file_id: f["file_id"].as_str()?.to_string(),
                path: f["path"].as_str()?.to_string(),
                size: f["size"].as_u64().unwrap_or(0),
                dir: f["dir"].as_bool().unwrap_or(false),
            })
        })
        .collect()
}

fn build_remote_clip(params: &Value) -> RemoteClip {
    RemoteClip {
        tx_id: params["tx_id"].as_str().unwrap_or_default().to_string(),
        formats: parse_formats(&params["formats"]),
        files: parse_remote_files(&params["files"]),
        sensitive: params["sensitive"].as_bool().unwrap_or(false),
    }
}

fn warn(message: &str) {
    eprintln!("[universallink-clipboard] {message}");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_maps_the_exit_conditions() {
        assert!(matches!(
            classify(Some(Event::Connected {
                granted_scopes: vec![],
                api_version: 1,
            })),
            Action::Resync
        ));
        assert!(matches!(
            classify(Some(Event::Notification {
                method: "clipboard.remote_updated".into(),
                params: json!({ "tx_id": "t" }),
            })),
            Action::RemoteUpdated(_)
        ));
        assert!(matches!(
            classify(Some(Event::Notification {
                method: "device.online".into(),
                params: Value::Null,
            })),
            Action::Idle
        ));
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
    fn formats_round_trip_through_json() {
        let formats = vec![
            Format {
                id: "text".into(),
                size: Some(13),
            },
            Format {
                id: "image/png".into(),
                size: None,
            },
        ];
        let json = formats_to_json(&formats, false);
        assert_eq!(
            json,
            json!([{ "format": "text", "size": 13 }, { "format": "image/png" }])
        );
        assert_eq!(parse_formats(&json), formats);
    }

    #[test]
    fn a_sensitive_clip_omits_every_size_hint() {
        // The invariant is enforced in the orchestrator, so a backend that leaves
        // sizes on a sensitive copy (Windows/macOS set `sensitive` without
        // stripping) still announces no size — confidential content leaks no
        // length. Both formats drop their `size`.
        let formats = vec![
            Format {
                id: "text".into(),
                size: Some(13),
            },
            Format {
                id: "image/png".into(),
                size: Some(4096),
            },
        ];
        let json = formats_to_json(&formats, true);
        assert_eq!(
            json,
            json!([{ "format": "text" }, { "format": "image/png" }])
        );
    }

    #[test]
    fn build_remote_clip_reads_the_notification_shape() {
        let clip = build_remote_clip(&json!({
            "device_id": "dev-1",
            "tx_id": "tx-1",
            "formats": [{ "format": "files" }],
            "files": [
                { "file_id": "f0", "path": "a/b.txt", "size": 7 },
                { "file_id": "f1", "path": "a", "size": 0, "dir": true },
            ],
            "sensitive": true,
        }));
        assert_eq!(clip.tx_id, "tx-1");
        assert_eq!(
            clip.formats,
            vec![Format {
                id: "files".into(),
                size: None
            }]
        );
        assert!(clip.sensitive);
        assert_eq!(clip.files.len(), 2);
        assert_eq!(clip.files[0].path, "a/b.txt");
        assert_eq!(clip.files[0].size, 7);
        assert!(!clip.files[0].dir);
        assert!(clip.files[1].dir);
    }

    #[test]
    fn is_files_clip_detects_the_files_format() {
        let files = RemoteClip {
            tx_id: "t".into(),
            formats: vec![Format {
                id: "files".into(),
                size: None,
            }],
            files: Vec::new(),
            sensitive: false,
        };
        assert!(is_files_clip(&files));
        let text = RemoteClip {
            tx_id: "t".into(),
            formats: vec![Format {
                id: "text".into(),
                size: Some(3),
            }],
            files: Vec::new(),
            sensitive: false,
        };
        assert!(!is_files_clip(&text));
        assert!(!is_files_clip(&RemoteClip {
            tx_id: "t".into(),
            formats: Vec::new(),
            files: Vec::new(),
            sensitive: false,
        }));
    }

    #[test]
    fn an_empty_announce_parses_to_no_formats() {
        assert!(
            build_remote_clip(&json!({ "tx_id": "t", "formats": [] }))
                .formats
                .is_empty()
        );
    }

    #[test]
    fn classify_routes_terminal_transfer_events_and_ignores_progress() {
        assert!(matches!(
            classify(Some(Event::Notification {
                method: "transfer.finished".into(),
                params: json!({ "transfer_id": "x", "paths": [] }),
            })),
            Action::Transfer(_)
        ));
        assert!(matches!(
            classify(Some(Event::Notification {
                method: "transfer.failed".into(),
                params: json!({ "transfer_id": "x", "error": "PEER_GONE" }),
            })),
            Action::Transfer(_)
        ));
        // Non-terminal transfer events carry no fill outcome: nothing to route.
        assert!(matches!(
            classify(Some(Event::Notification {
                method: "transfer.progress".into(),
                params: json!({ "transfer_id": "x" }),
            })),
            Action::Idle
        ));
    }

    #[test]
    fn parse_transfer_outcome_reads_both_terminal_shapes() {
        let ok = parse_transfer_outcome(
            "transfer.finished",
            &json!({ "transfer_id": "t1", "paths": ["/a/b.txt", "/a/c.txt"] }),
        );
        assert_eq!(ok.transfer_id, "t1");
        assert_eq!(
            ok.result.unwrap(),
            vec![PathBuf::from("/a/b.txt"), PathBuf::from("/a/c.txt")]
        );

        let failed = parse_transfer_outcome(
            "transfer.failed",
            &json!({ "transfer_id": "t2", "error": "TX_STALE" }),
        );
        assert_eq!(failed.transfer_id, "t2");
        assert_eq!(failed.result.unwrap_err(), "TX_STALE");

        // A failed transfer without an explicit error still fails (defaulted).
        let bare = parse_transfer_outcome("transfer.failed", &json!({ "transfer_id": "t3" }));
        assert!(bare.result.is_err());
    }
}
