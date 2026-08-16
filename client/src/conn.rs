// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

//! The managed connection cycle: establishment (token, hello, subscriptions),
//! service (multiplexed requests, notifications, incoming requests),
//! reconnection with backoff.
//!
//! A single "manager" task owns the connection and the pending map; a
//! dedicated reader task feeds the manager with parsed messages (read_frame
//! cannot be cancelled cleanly inside a select). The manager's writes are
//! bounded by WRITE_TIMEOUT: a Core that has stopped reading is a dead
//! connection, not a client hang.

use std::collections::HashMap;
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncWrite, AsyncWriteExt, BufReader, ReadHalf, WriteHalf};
use tokio::sync::{mpsc, oneshot};
use tokio::time::timeout;

use crate::transport::{self, Stream};
use crate::{ClientConfig, Event, RequestError, RequestId, RpcError, TokenSource, framing};

/// A full establishment attempt (connection + hello + subscribe)
/// beyond this: failure. Generous — the Core replies in milliseconds.
const ESTABLISH_TIMEOUT: Duration = Duration::from_secs(10);
/// Writing a frame beyond this: connection considered dead.
const WRITE_TIMEOUT: Duration = Duration::from_secs(10);
/// Ceiling for the reconnection backoff.
const BACKOFF_CAP: Duration = Duration::from_secs(60);
/// Events pending on the consumer side; full = backpressure all the way to
/// the socket (the Core will eventually close a component that stops reading).
const EVENT_CAPACITY: usize = 256;
/// Commands pending on the manager side.
const CMD_CAPACITY: usize = 64;
/// Parsed messages between the reader task and the manager.
const READ_CAPACITY: usize = 64;

enum Cmd {
    Request {
        method: String,
        params: Value,
        reply: oneshot::Sender<Result<Value, RequestError>>,
    },
    Respond {
        /// The connection generation the request was delivered on — a response
        /// is only written if it still matches the live connection.
        generation: u64,
        id: Value,
        payload: RespondPayload,
        reply: oneshot::Sender<Result<(), RequestError>>,
    },
}

enum RespondPayload {
    /// A successful `result`.
    Ok(Value),
    /// An application error code (`error.data.code`, e.g. `CLIP_STALE`).
    Err(String),
    /// A malformed request: the JSON-RPC `-32602`, worded exactly as the
    /// Core words its own (`invalid params: <what>`). A component serving
    /// a routed facade needs it: a shape refusal is not an application
    /// state, and dressing one as an app code would make every interface
    /// special-case it.
    InvalidParams(String),
}

/// Request handle to the Core — clonable, shareable across tasks.
#[derive(Clone)]
pub struct Client {
    cmd: mpsc::Sender<Cmd>,
    request_timeout: Duration,
}

impl Client {
    /// Sends a JSON-RPC request and awaits its response. Offline:
    /// immediate `NotConnected`. The result is the Core's raw `result`.
    pub async fn request(&self, method: &str, params: Value) -> Result<Value, RequestError> {
        let (tx, rx) = oneshot::channel();
        // The timeout ALSO covers enqueuing: a suspended manager (for
        // example under backpressure from an event consumer that has stopped
        // reading) must never block a caller without bound.
        match timeout(self.request_timeout, async {
            self.cmd
                .send(Cmd::Request {
                    method: method.to_string(),
                    params,
                    reply: tx,
                })
                .await
                .map_err(|_| RequestError::NotConnected)?;
            // Manager gone (incompatibility, shutdown) without replying.
            rx.await.map_err(|_| RequestError::NotConnected)?
        })
        .await
        {
            // The response may still arrive: it will be dropped
            // (the pending entry dies with the connection, at the latest).
            Err(_) => Err(RequestError::Timeout),
            Ok(r) => r,
        }
    }

    /// Answers an [`Event::Request`] with a successful result. `id` is the one
    /// carried by the event. `Disconnected` if the connection that delivered
    /// the request is gone (a stale id is never sent onto a fresh connection).
    pub async fn respond(&self, id: RequestId, result: Value) -> Result<(), RequestError> {
        self.send_response(id, RespondPayload::Ok(result)).await
    }

    /// Answers an [`Event::Request`] with an application error code (surfaced to
    /// the Core as `error.data.code`, e.g. `CLIP_STALE`).
    pub async fn respond_error(&self, id: RequestId, code: &str) -> Result<(), RequestError> {
        self.send_response(id, RespondPayload::Err(code.to_string()))
            .await
    }

    /// Refuses a served request as malformed: the JSON-RPC `-32602`, worded
    /// like the Core's own. `what` names the offending field.
    pub async fn respond_invalid_params(
        &self,
        id: RequestId,
        what: &str,
    ) -> Result<(), RequestError> {
        self.send_response(id, RespondPayload::InvalidParams(what.to_string()))
            .await
    }

    async fn send_response(
        &self,
        id: RequestId,
        payload: RespondPayload,
    ) -> Result<(), RequestError> {
        let (tx, rx) = oneshot::channel();
        match timeout(self.request_timeout, async {
            self.cmd
                .send(Cmd::Respond {
                    generation: id.generation,
                    id: id.id,
                    payload,
                    reply: tx,
                })
                .await
                .map_err(|_| RequestError::NotConnected)?;
            rx.await.map_err(|_| RequestError::NotConnected)?
        })
        .await
        {
            Err(_) => Err(RequestError::Timeout),
            Ok(r) => r,
        }
    }
}

pub(crate) fn spawn(config: ClientConfig) -> (Client, mpsc::Receiver<Event>) {
    let (cmd_tx, cmd_rx) = mpsc::channel(CMD_CAPACITY);
    let (event_tx, event_rx) = mpsc::channel(EVENT_CAPACITY);
    let client = Client {
        cmd: cmd_tx,
        request_timeout: config.request_timeout,
    };
    tokio::spawn(run(config, cmd_rx, event_tx));
    (client, event_rx)
}

// ---------------------------------------------------------------------------
// The manager task.
// ---------------------------------------------------------------------------

async fn run(config: ClientConfig, mut cmd_rx: mpsc::Receiver<Cmd>, event_tx: mpsc::Sender<Event>) {
    let mut delay = config.reconnect_base_delay;
    // Request ids: monotonically increasing over the client's whole lifetime,
    // never reused (establishment consumes some too).
    let mut next_id: u64 = 0;
    // Connection generation: bumped on every established connection so a
    // response to an incoming request cannot leak onto a later connection.
    let mut generation: u64 = 0;
    loop {
        // Establishment is NOT the connection yet: requests issued during
        // the attempt fail with immediate NotConnected, just as during
        // backoff — never an offline queue that would replay on the fresh
        // connection (review G1, confirmed defect).
        let outcome = {
            let attempt = timeout(ESTABLISH_TIMEOUT, establish(&config, &mut next_id));
            tokio::pin!(attempt);
            loop {
                tokio::select! {
                    r = &mut attempt => break Some(r),
                    cmd = cmd_rx.recv() => match cmd {
                        None => break None,
                        Some(cmd) => fail_offline(cmd),
                    },
                }
            }
        };
        let Some(outcome) = outcome else {
            return; // no Client left
        };
        match outcome {
            Ok(Ok(link)) => {
                delay = config.reconnect_base_delay;
                generation += 1;
                let _ = event_tx
                    .send(Event::Connected {
                        granted_scopes: link.granted_scopes.clone(),
                        api_version: link.api_version,
                    })
                    .await;
                // Notifications that arrived during establishment: after
                // Connected, in order.
                for (method, params) in &link.pending_notifications {
                    let _ = event_tx
                        .send(Event::Notification {
                            method: method.clone(),
                            params: params.clone(),
                        })
                        .await;
                }
                // Served requests that arrived in the same window: the
                // caller answers them like any other, on this generation.
                for (id, method, params) in &link.pending_requests {
                    let _ = event_tx
                        .send(Event::Request {
                            id: RequestId {
                                generation,
                                id: id.clone(),
                            },
                            method: method.clone(),
                            params: params.clone(),
                        })
                        .await;
                }
                let served = serve(
                    link,
                    &mut cmd_rx,
                    &event_tx,
                    &mut next_id,
                    generation,
                    &config.served_methods,
                )
                .await;
                let _ = event_tx.send(Event::Disconnected).await;
                if matches!(served, Served::ClientDropped) {
                    return;
                }
            }
            Ok(Err(EstablishError::Incompatible(api_version))) => {
                // An incompatibility does not heal by retrying: permanent
                // shutdown. We keep replying NotConnected so that
                // in-flight requests do not hang.
                let _ = event_tx.send(Event::Incompatible { api_version }).await;
                while let Some(cmd) = cmd_rx.recv().await {
                    fail_offline(cmd);
                }
                return;
            }
            // Failure or attempt too slow: backoff then a new cycle.
            Ok(Err(EstablishError::Failed)) | Err(_) => {}
        }
        if !wait_backoff(&mut cmd_rx, delay).await {
            return;
        }
        delay = (delay * 2).min(BACKOFF_CAP);
    }
}

/// Waits `delay` while replying `NotConnected` to requests (fail-closed:
/// nothing is queued while offline). `false` = no Client left.
async fn wait_backoff(cmd_rx: &mut mpsc::Receiver<Cmd>, delay: Duration) -> bool {
    let deadline = tokio::time::sleep(delay);
    tokio::pin!(deadline);
    loop {
        tokio::select! {
            _ = &mut deadline => return true,
            cmd = cmd_rx.recv() => match cmd {
                None => return false,
                Some(cmd) => fail_offline(cmd),
            },
        }
    }
}

/// Fail-closed reply to a command received while offline (establishment,
/// backoff, permanent shutdown): a request has nothing to send it on, and a
/// response's connection is gone.
fn fail_offline(cmd: Cmd) {
    match cmd {
        Cmd::Request { reply, .. } => {
            let _ = reply.send(Err(RequestError::NotConnected));
        }
        Cmd::Respond { reply, .. } => {
            let _ = reply.send(Err(RequestError::Disconnected));
        }
    }
}

// ---------------------------------------------------------------------------
// Establishment: token, connection, hello, subscriptions.
// ---------------------------------------------------------------------------

struct Link {
    reader: BufReader<ReadHalf<Stream>>,
    writer: WriteHalf<Stream>,
    granted_scopes: Vec<String>,
    api_version: u64,
    /// Notifications received before establishment completes (possible as
    /// soon as the hello is accepted — `component.pending` does not wait for
    /// a subscription).
    pending_notifications: Vec<(String, Value)>,
    /// Core→component REQUESTS received before establishment completes, for
    /// methods this component serves. The Core routes to a connection from
    /// the moment its hello is accepted, so a routed facade call (or a
    /// `clipboard.get_data`) can land during the subscribe leg: refusing it
    /// `-32601` there would make a served method intermittently missing.
    pending_requests: Vec<(Value, String, Value)>,
}

enum EstablishError {
    Failed,
    Incompatible(u64),
}

async fn establish(config: &ClientConfig, next_id: &mut u64) -> Result<Link, EstablishError> {
    let token = match &config.token {
        // Re-read on every attempt: the Core regenerates the token on each
        // startup, a token read ahead of time would be dead after a restart.
        TokenSource::File(path) => tokio::fs::read_to_string(path)
            .await
            .map_err(|_| EstablishError::Failed)?
            .trim()
            .to_string(),
        TokenSource::Spawn(token) => token.clone(),
    };

    let stream = transport::connect(&config.ipc_path)
        .await
        .map_err(|_| EstablishError::Failed)?;
    let (read, write) = tokio::io::split(stream);
    let mut link = Link {
        reader: BufReader::new(read),
        writer: write,
        granted_scopes: Vec::new(),
        api_version: 0,
        pending_notifications: Vec::new(),
        pending_requests: Vec::new(),
    };

    *next_id += 1;
    let hello_id = *next_id;
    let hello = json!({
        "jsonrpc": "2.0",
        "id": hello_id,
        "method": "hello",
        "params": {
            "name": config.name,
            "version": config.version,
            "role": config.role,
            "scopes": config.scopes,
            "token": token,
        },
    });
    write_frame(&mut link.writer, &hello.to_string())
        .await
        .map_err(|_| EstablishError::Failed)?;
    let result = wait_response(&mut link, hello_id, &config.served_methods).await?;

    // `pending` (interactive third-party enrollment): not supported in v1 —
    // for an official component it means a missing token, hence a failure.
    if result["status"] != json!("ok") {
        return Err(EstablishError::Failed);
    }
    link.api_version = result["api_version"]
        .as_u64()
        .ok_or(EstablishError::Failed)?;
    if link.api_version != crate::API_VERSION {
        return Err(EstablishError::Incompatible(link.api_version));
    }
    link.granted_scopes = result["granted_scopes"]
        .as_array()
        .ok_or(EstablishError::Failed)?
        .iter()
        .map(|s| s.as_str().map(str::to_string))
        .collect::<Option<Vec<_>>>()
        .ok_or(EstablishError::Failed)?;

    // One call for every topic: the Core subscribes all or nothing, on purpose
    // (no silent partial subscription). A topic declared OPTIONAL may be one
    // this Core has never heard of — it is older than the topic — and its
    // refusal takes the whole call down with it. That must not cost the
    // connection: we ask for everything, and fall back to the required set
    // alone. What is then lost is the events of a topic this Core would never
    // have sent anyway.
    let required: Vec<&str> = config.topics.iter().map(String::as_str).collect();
    let wanted: Vec<&str> = required
        .iter()
        .copied()
        .chain(config.optional_topics.iter().map(String::as_str))
        .collect();
    if !wanted.is_empty()
        && subscribe(&mut link, next_id, &wanted, &config.served_methods)
            .await
            .is_err()
    {
        if config.optional_topics.is_empty() {
            return Err(EstablishError::Failed);
        }
        subscribe(&mut link, next_id, &required, &config.served_methods).await?;
    }

    Ok(link)
}

async fn subscribe(
    link: &mut Link,
    next_id: &mut u64,
    topics: &[&str],
    served: &[String],
) -> Result<(), EstablishError> {
    *next_id += 1;
    let sub_id = *next_id;
    let request = json!({
        "jsonrpc": "2.0",
        "id": sub_id,
        "method": "events.subscribe",
        "params": { "topics": topics },
    });
    write_frame(&mut link.writer, &request.to_string())
        .await
        .map_err(|_| EstablishError::Failed)?;
    wait_response(link, sub_id, served).await.map(|_| ())
}

/// Awaits response `id` during establishment, buffering notifications and
/// the requests this component serves (turning away only the rest).
async fn wait_response(
    link: &mut Link,
    id: u64,
    served: &[String],
) -> Result<Value, EstablishError> {
    loop {
        let text = framing::read_frame(&mut link.reader)
            .await
            .map_err(|_| EstablishError::Failed)?
            .ok_or(EstablishError::Failed)?;
        let v: Value = serde_json::from_str(&text).map_err(|_| EstablishError::Failed)?;
        if v.get("method").is_some() {
            if v.get("id").is_none_or(Value::is_null) {
                let method = v["method"]
                    .as_str()
                    .ok_or(EstablishError::Failed)?
                    .to_string();
                let params = v.get("params").cloned().unwrap_or(Value::Null);
                link.pending_notifications.push((method, params));
            } else if served
                .iter()
                .any(|m| Some(m.as_str()) == v["method"].as_str())
            {
                // Held, not refused: delivered as an `Event::Request` right
                // after `Connected`, with the same id.
                let method = v["method"]
                    .as_str()
                    .ok_or(EstablishError::Failed)?
                    .to_string();
                let params = v.get("params").cloned().unwrap_or(Value::Null);
                link.pending_requests
                    .push((v["id"].clone(), method, params));
            } else {
                write_frame(&mut link.writer, &method_not_found(&v))
                    .await
                    .map_err(|_| EstablishError::Failed)?;
            }
        } else if v.get("id") == Some(&json!(id)) {
            if v.get("error").is_some() {
                // hello or subscribe refused: cycle failure (a config
                // error loops forever — never Connected).
                return Err(EstablishError::Failed);
            }
            return Ok(v.get("result").cloned().unwrap_or(Value::Null));
        }
        // Response for another id: impossible during establishment (fresh
        // ids) — ignored.
    }
}

// ---------------------------------------------------------------------------
// Service: the established connection, until it dies.
// ---------------------------------------------------------------------------

enum Served {
    ConnectionLost,
    ClientDropped,
}

async fn serve(
    link: Link,
    cmd_rx: &mut mpsc::Receiver<Cmd>,
    event_tx: &mpsc::Sender<Event>,
    next_id: &mut u64,
    generation: u64,
    served_methods: &[String],
) -> Served {
    let Link {
        mut reader,
        mut writer,
        ..
    } = link;

    // Reader task: parsed frames to the manager. Anything that is not
    // a valid JSON frame terminates the connection (fail-closed).
    let (msg_tx, mut msg_rx) = mpsc::channel::<Value>(READ_CAPACITY);
    let read_task = tokio::spawn(async move {
        loop {
            match framing::read_frame(&mut reader).await {
                Ok(Some(text)) => match serde_json::from_str::<Value>(&text) {
                    Ok(v) => {
                        if msg_tx.send(v).await.is_err() {
                            return;
                        }
                    }
                    Err(_) => return,
                },
                // EOF or framing violation: end of connection.
                _ => return,
            }
        }
    });

    // In-flight requests: the response to an expired request (timeout on the
    // caller side) is dropped on arrival; the entry dies at the latest here,
    // with the connection.
    let mut pending: HashMap<u64, oneshot::Sender<Result<Value, RequestError>>> = HashMap::new();

    let outcome = loop {
        tokio::select! {
            cmd = cmd_rx.recv() => match cmd {
                None => break Served::ClientDropped,
                Some(Cmd::Request { method, params, reply }) => {
                    *next_id += 1;
                    let id = *next_id;
                    let msg = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
                    match write_frame(&mut writer, &msg.to_string()).await {
                        Ok(()) => {
                            pending.insert(id, reply);
                        }
                        Err(_) => {
                            let _ = reply.send(Err(RequestError::Disconnected));
                            break Served::ConnectionLost;
                        }
                    }
                }
                Some(Cmd::Respond { generation: g, id, payload, reply }) => {
                    // The request's connection is gone: never write its id onto
                    // this (later) connection.
                    if g != generation {
                        let _ = reply.send(Err(RequestError::Disconnected));
                        continue;
                    }
                    let msg = match payload {
                        RespondPayload::Ok(result) => {
                            json!({ "jsonrpc": "2.0", "id": id, "result": result })
                        }
                        RespondPayload::Err(code) => json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "error": { "code": -32000, "message": code, "data": { "code": code } },
                        }),
                        RespondPayload::InvalidParams(what) => json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "error": {
                                "code": -32602,
                                "message": format!("invalid params: {what}"),
                            },
                        }),
                    };
                    match write_frame(&mut writer, &msg.to_string()).await {
                        Ok(()) => {
                            let _ = reply.send(Ok(()));
                        }
                        Err(_) => {
                            let _ = reply.send(Err(RequestError::Disconnected));
                            break Served::ConnectionLost;
                        }
                    }
                }
            },
            msg = msg_rx.recv() => match msg {
                None => break Served::ConnectionLost,
                Some(v) => {
                    if v.get("method").is_some() {
                        if v.get("id").is_none_or(Value::is_null) {
                            let Some(method) = v["method"].as_str() else {
                                break Served::ConnectionLost;
                            };
                            let method = method.to_string();
                            let params = v.get("params").cloned().unwrap_or(Value::Null);
                            // Blocks if the consumer falls behind:
                            // intended backpressure. Consumer gone:
                            // events dropped, the client stays usable.
                            let _ = event_tx.send(Event::Notification { method, params }).await;
                        } else if let Some(method) = v["method"].as_str()
                            && served_methods.iter().any(|m| m == method)
                        {
                            // A served Core→component request: surface it; the
                            // consumer answers via Client::respond. Same
                            // backpressure as a notification.
                            let method = method.to_string();
                            let params = v.get("params").cloned().unwrap_or(Value::Null);
                            let id = v.get("id").cloned().unwrap_or(Value::Null);
                            let _ = event_tx
                                .send(Event::Request {
                                    id: RequestId { generation, id },
                                    method,
                                    params,
                                })
                                .await;
                        } else if write_frame(&mut writer, &method_not_found(&v)).await.is_err() {
                            break Served::ConnectionLost;
                        }
                    } else if let Some(id) = v.get("id").and_then(Value::as_u64)
                        && let Some(reply) = pending.remove(&id)
                    {
                        let _ = reply.send(parse_result(v));
                        // (Orphan response — expired request: ignored.)
                    }
                    // Message with no usable method or id: ignored
                    // (additive extensions).
                }
            },
        }
    };

    read_task.abort();
    for (_, reply) in pending {
        let _ = reply.send(Err(RequestError::Disconnected));
    }
    outcome
}

// ---------------------------------------------------------------------------
// Building blocks.
// ---------------------------------------------------------------------------

/// `-32601` response to an incoming request: the v1 client serves no
/// method (the Core will call the clipboard backends later).
fn method_not_found(v: &Value) -> String {
    json!({
        "jsonrpc": "2.0",
        "id": v["id"],
        "error": { "code": -32601, "message": "method not found" },
    })
    .to_string()
}

fn parse_result(v: Value) -> Result<Value, RequestError> {
    if let Some(err) = v.get("error") {
        return Err(RequestError::Rpc(RpcError {
            code: err["code"].as_i64().unwrap_or(-32000),
            message: err["message"].as_str().unwrap_or_default().to_string(),
            data_code: err
                .pointer("/data/code")
                .and_then(Value::as_str)
                .map(String::from),
        }));
    }
    Ok(v.get("result").cloned().unwrap_or(Value::Null))
}

async fn write_frame<W: AsyncWrite + Unpin>(writer: &mut W, text: &str) -> std::io::Result<()> {
    let bytes = framing::encode(text);
    timeout(WRITE_TIMEOUT, writer.write_all(&bytes))
        .await
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::TimedOut, "IPC write blocked"))?
}
