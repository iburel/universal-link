// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

//! The bridge: a task that lives with the app, consumes events from the IPC
//! client and pushes them to the webview; two commands, one snapshot.

use std::sync::{Mutex, OnceLock};

use serde_json::{Value, json};
use tauri::{AppHandle, Builder, Emitter, Manager, Runtime, State};
use universallink_ipc_client::{Client, ClientConfig, Event, RequestError};

/// Managed state: the client only exists once the bridge task is started —
/// before that, every request is `not_connected` (same fail-closed as before
/// the first connection is established).
struct CoreState {
    client: OnceLock<Client>,
    /// Latest connection snapshot, updated BEFORE the event is emitted:
    /// subscribing then reading the snapshot never misses a state.
    connection: Mutex<Value>,
}

/// Attaches the Core bridge to a Tauri builder (real or MockRuntime): the
/// state, the commands, and an internal plugin whose setup starts the bridge
/// loop. A plugin, because its setup runs at `build()` (MockRuntime included)
/// — whereas the `Builder`'s own setup only runs at `run()`.
pub fn shell<R: Runtime>(builder: Builder<R>, config: ClientConfig) -> Builder<R> {
    builder
        .manage(CoreState {
            client: OnceLock::new(),
            connection: Mutex::new(json!({ "status": "connecting" })),
        })
        .invoke_handler(tauri::generate_handler![core_request, connection_status])
        .plugin(
            tauri::plugin::Builder::<R, ()>::new("universallink-bridge")
                .setup(move |app, _api| {
                    tauri::async_runtime::spawn(bridge_loop(app.clone(), config));
                    Ok(())
                })
                .build(),
        )
}

/// Lives as long as the client publishes (forever, except `Incompatible` —
/// the terminal state then stays shown by the snapshot).
async fn bridge_loop<R: Runtime>(app: AppHandle<R>, config: ClientConfig) {
    let (client, mut events) = universallink_ipc_client::spawn(config);
    let _ = app.state::<CoreState>().client.set(client);
    while let Some(event) = events.recv().await {
        if let Event::Notification { method, params } = event {
            let _ = app.emit(
                "core:notification",
                json!({ "method": method, "params": params }),
            );
        } else if let Some(snapshot) = connection_snapshot(&event) {
            publish_connection(&app, snapshot);
        }
    }
}

/// Publishes a state change: snapshot updated THEN event emitted. This order
/// is the invariant that lets the frontend subscribe then read the snapshot
/// without ever missing a state — it is pinned by a test.
fn publish_connection<R: Runtime>(app: &AppHandle<R>, snapshot: Value) {
    let state = app.state::<CoreState>();
    *state.connection.lock().expect("snapshot lock") = snapshot.clone();
    let _ = app.emit("core:connection", snapshot);
}

/// State snapshot for a connection event; `None` for a notification.
/// `Disconnected` reverts to "connecting": reconnection is automatic, there
/// is no "stable disconnected" state.
fn connection_snapshot(event: &Event) -> Option<Value> {
    match event {
        Event::Connected {
            granted_scopes,
            api_version,
        } => Some(json!({
            "status": "connected",
            "granted_scopes": granted_scopes,
            "api_version": api_version,
        })),
        Event::Disconnected => Some(json!({ "status": "connecting" })),
        Event::Incompatible { api_version } => Some(json!({
            "status": "incompatible",
            "api_version": api_version,
        })),
        Event::Notification { .. } => None,
    }
}

/// A command's error: faithful relay of `RequestError`, serialized for the
/// frontend.
#[derive(Clone, serde::Serialize)]
struct CommandError {
    kind: &'static str,
    message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    code: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    data_code: Option<String>,
}

impl From<RequestError> for CommandError {
    fn from(err: RequestError) -> CommandError {
        let kind = match &err {
            RequestError::NotConnected => "not_connected",
            RequestError::Timeout => "timeout",
            RequestError::Disconnected => "disconnected",
            RequestError::Rpc(_) => "rpc",
        };
        match err {
            RequestError::Rpc(e) => CommandError {
                kind,
                message: e.message,
                code: Some(e.code),
                data_code: e.data_code,
            },
            other => CommandError {
                kind,
                message: other.to_string(),
                code: None,
                data_code: None,
            },
        }
    }
}

#[tauri::command]
async fn core_request(
    state: State<'_, CoreState>,
    method: String,
    params: Option<Value>,
) -> Result<Value, CommandError> {
    let Some(client) = state.client.get() else {
        return Err(CommandError::from(RequestError::NotConnected));
    };
    client
        .request(&method, params.unwrap_or_else(|| json!({})))
        .await
        .map_err(CommandError::from)
}

#[tauri::command]
fn connection_status(state: State<'_, CoreState>) -> Value {
    state.connection.lock().expect("snapshot lock").clone()
}

#[cfg(test)]
mod tests {
    use universallink_ipc_client::RpcError;

    use super::*;

    #[test]
    fn snapshots_cover_every_connection_event() {
        let snap = connection_snapshot(&Event::Connected {
            granted_scopes: vec!["session.read".into()],
            api_version: 1,
        })
        .expect("snapshot");
        assert_eq!(snap["status"], "connected");
        assert_eq!(snap["granted_scopes"], json!(["session.read"]));
        assert_eq!(snap["api_version"], 1);

        let snap = connection_snapshot(&Event::Disconnected).expect("snapshot");
        assert_eq!(snap, json!({ "status": "connecting" }));

        // Incompatible is terminal: the client stops, this snapshot is the
        // last one — it must carry enough to explain why.
        let snap = connection_snapshot(&Event::Incompatible { api_version: 2 }).expect("snapshot");
        assert_eq!(snap["status"], "incompatible");
        assert_eq!(snap["api_version"], 2);

        assert!(
            connection_snapshot(&Event::Notification {
                method: "session.changed".into(),
                params: json!({}),
            })
            .is_none()
        );
    }

    #[test]
    fn command_errors_relay_request_errors() {
        let e = CommandError::from(RequestError::Rpc(RpcError {
            code: -32000,
            message: "server unreachable".into(),
            data_code: Some("SERVER_UNREACHABLE".into()),
        }));
        let v = serde_json::to_value(&e).expect("serialization");
        assert_eq!(v["kind"], "rpc");
        assert_eq!(v["code"], -32000);
        assert_eq!(v["data_code"], "SERVER_UNREACHABLE");
        assert_eq!(v["message"], "server unreachable");

        let e = CommandError::from(RequestError::Rpc(RpcError {
            code: -32601,
            message: "unknown method".into(),
            data_code: None,
        }));
        let v = serde_json::to_value(&e).expect("serialization");
        assert!(v.get("data_code").is_none(), "{v}");

        let e = CommandError::from(RequestError::NotConnected);
        let v = serde_json::to_value(&e).expect("serialization");
        assert_eq!(v["kind"], "not_connected");
        assert!(v["message"].as_str().is_some_and(|m| !m.is_empty()));
        assert!(v.get("code").is_none(), "{v}");

        // timeout and disconnected carry opposite semantics for the frontend
        // (retry vs resynchronize): the kinds are pinned.
        let v =
            serde_json::to_value(CommandError::from(RequestError::Timeout)).expect("serialization");
        assert_eq!(v["kind"], "timeout");
        assert!(v["message"].as_str().is_some_and(|m| !m.is_empty()));
        let v = serde_json::to_value(CommandError::from(RequestError::Disconnected))
            .expect("serialization");
        assert_eq!(v["kind"], "disconnected");
        assert!(v["message"].as_str().is_some_and(|m| !m.is_empty()));
    }

    #[test]
    fn snapshot_is_updated_before_the_event_is_emitted() {
        use tauri::Listener;

        let app = tauri::test::mock_builder()
            .manage(CoreState {
                client: OnceLock::new(),
                connection: Mutex::new(json!({ "status": "connecting" })),
            })
            .build(tauri::test::mock_context(tauri::test::noop_assets()))
            .expect("app mock");
        let handle = app.handle().clone();

        // The listen_any handlers run SYNCHRONOUSLY during emit: the snapshot
        // read here must already be the one in the payload. A version that
        // emitted before updating would hand the old snapshot to a frontend
        // that subscribes then reads — the missed state would never be
        // corrected as long as the connection stays stable.
        let observed = std::sync::Arc::new(Mutex::new(Vec::new()));
        let seen = observed.clone();
        let reader = handle.clone();
        handle.listen_any("core:connection", move |event| {
            let snapshot = reader
                .state::<CoreState>()
                .connection
                .lock()
                .expect("snapshot lock")
                .clone();
            let payload: Value = serde_json::from_str(event.payload()).expect("JSON payload");
            seen.lock().expect("lock").push((snapshot, payload));
        });

        let target = json!({ "status": "connected", "granted_scopes": [], "api_version": 1 });
        publish_connection(&handle, target.clone());

        let observed = observed.lock().expect("lock");
        assert_eq!(observed.len(), 1, "one event emitted");
        assert_eq!(
            observed[0].0, target,
            "snapshot read during emission ≠ payload: updated AFTER the emit"
        );
        assert_eq!(observed[0].1, target, "payload emitted");
    }
}
