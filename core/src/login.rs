// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

//! OIDC login (authorization code + PKCE, system browser) and device
//! enrollment. `session.login` prepares everything and returns the
//! authorization URL — it is the caller that opens the browser, the Core does
//! not touch the UI. The IdP redirects to our loopback listener; the Core
//! exchanges the code, enrolls the device (doc/server-api.md, "Enrollment"),
//! writes session.json and the refresh token, starts the session task. The same
//! flow serves as re-auth for `devices.revoke` (a fresh ID token demanded by
//! the server).
//!
//! A single flow pending at a time: the next one replaces it. The OAuth `state`
//! (anti-CSRF) is also the flow's identity — the callback verifies it, and the
//! task re-verifies it still holds the slot before publishing anything (same
//! lesson as the session: abort() only bites at the next await).
//!
//! The outbound calls (discovery, token endpoint, WS enrollment) go through the
//! config's `Connector`: in the clear in tests, over TLS under the binary. The
//! loopback listener, by contrast, is always in the clear — it is the user's
//! browser that connects to it, on 127.0.0.1.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use base64::Engine;
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use sha2::Digest;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::tungstenite::Message;

use crate::rpc::RpcErr;
use crate::session::{ServerWs, SessionInfo};
use crate::state::{AppState, LoginSlot, random_hex};

/// The browser has this long to bring back a decisive callback; beyond it, the
/// flow and its listener disappear.
const FLOW_TIMEOUT: Duration = Duration::from_secs(300);
/// The loopback has the same budget as the outbound calls (see `http.rs`).
const HTTP_TIMEOUT: Duration = Duration::from_secs(10);
/// Enrollment (WS connection + challenge + enroll) is bounded.
const ENROLL_TIMEOUT: Duration = Duration::from_secs(30);
/// Maximum size of a loopback request (method + headers).
const REQUEST_MAX: usize = 8 * 1024;

/// What the flow accomplishes once the ID token is obtained.
pub(crate) enum Goal {
    /// Enroll the device and open the session.
    Login,
    /// Revoke `device_id` — the re-auth of a `devices.revoke` whose refresh
    /// token was not enough.
    Revoke { device_id: String },
}

/// Starts an OIDC flow: discovery, loopback listener, waiting task. Returns the
/// authorization URL. Replaces the pending flow if there is one.
pub(crate) async fn start_flow(state: &Arc<AppState>, goal: Goal) -> Result<String, RpcErr> {
    let unreachable = || RpcErr::app("SERVER_UNREACHABLE");
    let Some(server) = state
        .server_config
        .lock()
        .expect("lock server_config")
        .clone()
    else {
        // Core never configured: there is nowhere to log in.
        return Err(unreachable());
    };
    let discovery = fetch_discovery(state, &server.oidc_issuer)
        .await
        .map_err(|e| {
            tracing::warn!(error = %e, "OIDC discovery failed");
            unreachable()
        })?;

    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .map_err(|_| unreachable())?;
    let port = listener.local_addr().map_err(|_| unreachable())?.port();
    let redirect_uri = format!("http://127.0.0.1:{port}/callback");

    // PKCE (RFC 7636): random verifier, S256 challenge in base64url.
    let verifier = random_hex(32);
    let challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(sha2::Sha256::digest(verifier.as_bytes()));
    let state_param = random_hex(16);

    let auth_url = format!(
        "{}?{}",
        discovery.authorization_endpoint,
        form_urlencoded::Serializer::new(String::new())
            .append_pair("response_type", "code")
            .append_pair("client_id", &server.oidc_client_id)
            .append_pair("redirect_uri", &redirect_uri)
            .append_pair("scope", "openid email")
            .append_pair("state", &state_param)
            .append_pair("code_challenge", &challenge)
            .append_pair("code_challenge_method", "S256")
            // Google only returns a refresh token with these two; other IdPs
            // ignore them.
            .append_pair("access_type", "offline")
            .append_pair("prompt", "consent")
            .finish()
    );

    let flow = Flow {
        state: state.clone(),
        server,
        token_endpoint: discovery.token_endpoint,
        redirect_uri,
        verifier,
        state_param: state_param.clone(),
        goal,
    };
    let task = tokio::spawn(flow.run(listener));
    let previous = state.login.lock().expect("lock login").replace(LoginSlot {
        state_param,
        abort: task.abort_handle(),
    });
    if let Some(previous) = previous {
        previous.abort.abort();
    }
    Ok(auth_url)
}

/// A fresh ID token obtained via the keyring's refresh token — the
/// browser-free path for sensitive operations.
pub(crate) enum FreshToken {
    Token(String),
    /// No refresh token, or the IdP no longer accepts it: re-auth needed.
    NeedsReauth,
    /// The IdP is unreachable (or the Core is not configured).
    Unreachable,
}

pub(crate) async fn fresh_id_token(state: &AppState) -> FreshToken {
    let Some(server) = state
        .server_config
        .lock()
        .expect("lock server_config")
        .clone()
    else {
        return FreshToken::Unreachable;
    };
    let Some(refresh) = state.secrets.get(crate::secrets::REFRESH_TOKEN) else {
        return FreshToken::NeedsReauth;
    };
    let discovery = match fetch_discovery(state, &server.oidc_issuer).await {
        Ok(d) => d,
        Err(e) => {
            tracing::warn!(error = %e, "OIDC discovery failed");
            return FreshToken::Unreachable;
        }
    };
    let mut fields = vec![
        ("grant_type", "refresh_token"),
        ("refresh_token", refresh.as_str()),
        ("client_id", server.oidc_client_id.as_str()),
    ];
    // Same Google requirement as at the code exchange (see `ServerConfig`).
    if let Some(secret) = &server.oidc_client_secret {
        fields.push(("client_secret", secret.as_str()));
    }
    let response = post_form(state, &discovery.token_endpoint, &fields).await;
    match response {
        Ok(v) => match v.get("id_token").and_then(Value::as_str) {
            Some(t) => FreshToken::Token(t.to_string()),
            None => FreshToken::NeedsReauth,
        },
        // Dead refresh token (expired, revoked on the IdP side): it will never
        // open anything again, so no point keeping it.
        Err(e) if e == "invalid_grant" => {
            state.secrets.delete(crate::secrets::REFRESH_TOKEN);
            FreshToken::NeedsReauth
        }
        Err(_) => FreshToken::Unreachable,
    }
}

// ---------------------------------------------------------------------------
// The pending flow: loopback listener, completion.
// ---------------------------------------------------------------------------

struct Flow {
    state: Arc<AppState>,
    server: crate::ServerConfig,
    token_endpoint: String,
    redirect_uri: String,
    verifier: String,
    state_param: String,
    goal: Goal,
}

impl Flow {
    async fn run(self, listener: TcpListener) {
        match tokio::time::timeout(FLOW_TIMEOUT, self.wait_callback(listener)).await {
            // No one came: give the slot back if we still hold it.
            Err(_) => {
                self.claim_slot();
            }
            // Flow consumed (user refusal) or dead listener: already settled.
            Ok(None) => {}
            // Decisive callback: the rest (exchange, enrollment/revocation) is
            // bounded by its own timeouts, and the browser page tells the
            // outcome.
            Ok(Some((mut conn, code))) => match self.complete(&code).await {
                Ok(message) => respond(&mut conn, 200, &message).await,
                Err(message) => respond(&mut conn, 502, &message).await,
            },
        }
    }

    /// Serves the loopback until the decisive callback: a `code` with the right
    /// `state` (returned with its connection, the reply will await the
    /// outcome), or a user refusal (`None`, flow consumed). Everything else is
    /// answered without consuming the flow — a forged request must not be able
    /// to kill a login in progress.
    async fn wait_callback(&self, listener: TcpListener) -> Option<(TcpStream, String)> {
        loop {
            let Ok((mut conn, _)) = listener.accept().await else {
                return None;
            };
            let Some(target) = read_request_target(&mut conn).await else {
                respond(&mut conn, 400, "Unreadable request.").await;
                continue;
            };
            let (path, query) = target.split_once('?').unwrap_or((target.as_str(), ""));
            if path != "/callback" {
                respond(&mut conn, 404, "Nothing here.").await;
                continue;
            }
            let params: HashMap<String, String> = form_urlencoded::parse(query.as_bytes())
                .into_owned()
                .collect();
            if params.get("state") != Some(&self.state_param) {
                respond(&mut conn, 400, "This link matches no login in progress.").await;
                continue;
            }
            if let Some(error) = params.get("error") {
                // The user refused (or the IdP gave up): the flow is over,
                // starting a new login is the only recourse.
                self.claim_slot();
                respond(
                    &mut conn,
                    403,
                    &format!("Login refused ({error}). You can close this tab."),
                )
                .await;
                return None;
            }
            let Some(code) = params.get("code") else {
                respond(&mut conn, 400, "Callback with neither code nor error.").await;
                continue;
            };
            return Some((conn, code.clone()));
        }
    }

    /// Exchanges the code for the tokens, then accomplishes the flow's goal.
    /// Returns the browser page's message.
    async fn complete(&self, code: &str) -> Result<String, String> {
        let mut fields = vec![
            ("grant_type", "authorization_code"),
            ("code", code),
            ("redirect_uri", self.redirect_uri.as_str()),
            ("client_id", self.server.oidc_client_id.as_str()),
            ("code_verifier", self.verifier.as_str()),
        ];
        // Google requires the client_secret even under PKCE (see
        // `ServerConfig`); IdPs that conform to RFC 7636 have none and do not
        // receive it.
        if let Some(secret) = &self.server.oidc_client_secret {
            fields.push(("client_secret", secret.as_str()));
        }
        let tokens = post_form(&self.state, &self.token_endpoint, &fields)
            .await
            .map_err(|e| format!("Code exchange failed ({e})."))?;
        let Some(id_token) = tokens.get("id_token").and_then(Value::as_str) else {
            return Err("IdP response without an id_token.".to_string());
        };
        let refresh_token = tokens.get("refresh_token").and_then(Value::as_str);

        match &self.goal {
            Goal::Login => self.finish_login(id_token, refresh_token).await,
            Goal::Revoke { device_id } => {
                self.finish_revoke(device_id, id_token, refresh_token).await
            }
        }
    }

    async fn finish_login(
        &self,
        id_token: &str,
        refresh_token: Option<&str>,
    ) -> Result<String, String> {
        let device_id = tokio::time::timeout(
            ENROLL_TIMEOUT,
            enroll(&self.state, &self.server.url, id_token),
        )
        .await
        .map_err(|_| "Enrollment took too long.".to_string())?
        .map_err(|e| format!("Enrollment refused ({e})."))?;

        // Synchronous zone: if the slot is no longer ours (flow replaced), the
        // abort simply has not bitten yet — publish nothing.
        if !self.claim_slot() {
            return Err("Login replaced by another.".to_string());
        }
        let info = SessionInfo {
            server_url: self.server.url.clone(),
            device_id,
            account: account_from_id_token(id_token),
        };
        {
            let mut s = self.state.session.lock().expect("lock session");
            // A session opened during enrollment (a faster concurrent flow): do
            // not overwrite it.
            if s.logged_in {
                return Err("A session is already open.".to_string());
            }
            // session.json under the same lock as the state it materializes:
            // nothing can slip between the disk and memory.
            let mut session_json = json!({
                "server_url": info.server_url,
                "device_id": info.device_id,
            });
            if let Some(account) = &info.account {
                session_json["account"] = account.clone();
            }
            crate::write_private_file(
                &self.state.config_dir.join("session.json"),
                &session_json.to_string(),
            )
            .map_err(|e| format!("Writing session.json failed ({e})."))?;
            if let Some(refresh) = refresh_token {
                // Under the same lock: a logout serialized after us will delete
                // it — never the other way around. Degraded if the write fails:
                // revokes will go through re-auth.
                if let Err(e) = self
                    .state
                    .secrets
                    .set(crate::secrets::REFRESH_TOKEN, refresh)
                {
                    tracing::error!(error = %e, "refresh token not stored");
                }
            }
            s.logged_in = true;
            s.account = info.account.clone();
            s.own_device_id = Some(info.device_id.clone());
            let payload = s.status_record();
            // Broadcast under the session lock (order: session then registry):
            // the order of notifications is the order of transitions.
            self.state
                .registry
                .lock()
                .expect("lock registry")
                .notify_topic("session", "session.changed", &payload);
        }
        crate::start_session_task(&self.state, info);
        Ok("Login succeeded. You can close this tab.".to_string())
    }

    async fn finish_revoke(
        &self,
        device_id: &str,
        id_token: &str,
        refresh_token: Option<&str>,
    ) -> Result<String, String> {
        // Take the slot before acting — after which nothing can stop us midway.
        if !self.claim_slot() {
            return Err("Re-authentication replaced by another.".to_string());
        }
        {
            let s = self.state.session.lock().expect("lock session");
            // The session may have closed while the tab was waiting (logout
            // kills the flow, but not if it had already taken the slot): an
            // orphan re-auth must neither stow a credential nor revoke.
            if !s.logged_in {
                return Err("The session was closed in the meantime.".to_string());
            }
            // The refresh token that led here was dead or absent: this one will
            // serve again for the next sensitive operations. Under the lock: a
            // logout serialized after us will delete it — never the other way
            // around.
            if let Some(refresh) = refresh_token
                && let Err(e) = self
                    .state
                    .secrets
                    .set(crate::secrets::REFRESH_TOKEN, refresh)
            {
                tracing::error!(error = %e, "refresh token not stored");
            }
        }
        crate::session::proxy(
            &self.state,
            "devices.revoke",
            json!({ "device_id": device_id, "id_token": id_token }),
        )
        .await
        .map(|_| "Revocation done. You can close this tab.".to_string())
        .map_err(|err| {
            format!(
                "Revocation refused ({}).",
                err.app.as_deref().unwrap_or("server error")
            )
        })
    }

    /// Does the flow still hold the slot? Takes it (empties it) if so — no one
    /// can then replace or stop it anymore.
    fn claim_slot(&self) -> bool {
        let mut slot = self.state.login.lock().expect("lock login");
        if slot
            .as_ref()
            .is_some_and(|s| s.state_param == self.state_param)
        {
            *slot = None;
            true
        } else {
            false
        }
    }
}

/// A WS connection dedicated to enrollment: challenge → enroll → device_id,
/// closed on return. The nominal connection (authenticate) is the business of
/// the session task.
async fn enroll(state: &AppState, server_url: &str, id_token: &str) -> Result<String, String> {
    let mut ws = crate::session::open_ws(state, server_url).await?;
    let challenge = ws_request(&mut ws, 1, "auth.challenge", json!({})).await?;
    let nonce = challenge["nonce"]
        .as_str()
        .ok_or("challenge without a nonce")?;
    let result = ws_request(
        &mut ws,
        2,
        "auth.enroll",
        json!({
            "id_token": id_token,
            "node_id": state.identity.node_id(),
            "name": state.device_name,
            "platform": std::env::consts::OS,
            "proof": state.identity.proof(nonce),
        }),
    )
    .await?;
    let _ = ws.close(None).await;
    result["device_id"]
        .as_str()
        .map(str::to_string)
        .ok_or_else(|| "enroll without a device_id".to_string())
}

/// A sequential JSON-RPC request over a throwaway WS connection; crossing
/// notifications do not concern us (not yet authenticated).
async fn ws_request(
    ws: &mut ServerWs,
    id: u64,
    method: &str,
    params: Value,
) -> Result<Value, String> {
    let msg = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
    ws.send(Message::text(msg.to_string()))
        .await
        .map_err(|e| format!("sending {method}: {e}"))?;
    loop {
        match ws.next().await {
            Some(Ok(Message::Text(t))) => {
                let Ok(v) = serde_json::from_str::<Value>(&t) else {
                    return Err("unreadable response".to_string());
                };
                if v.get("method").is_some() {
                    continue;
                }
                if v.get("id") == Some(&json!(id)) {
                    if let Some(err) = v.get("error") {
                        return Err(err
                            .pointer("/data/code")
                            .and_then(Value::as_str)
                            .unwrap_or("server error")
                            .to_string());
                    }
                    return Ok(v.get("result").cloned().unwrap_or(Value::Null));
                }
            }
            Some(Ok(Message::Close(_))) | Some(Err(_)) | None => {
                return Err("connection closed".to_string());
            }
            Some(Ok(_)) => {}
        }
    }
}

/// The `account` of session.json, taken from the ID token's claims — without
/// verifying the signature: the token comes straight from the IdP (TLS), and it
/// is the server that is authoritative (it re-validates it at enrollment).
fn account_from_id_token(id_token: &str) -> Option<Value> {
    let payload = id_token.split('.').nth(1)?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .ok()?;
    let claims: Value = serde_json::from_slice(&bytes).ok()?;
    let email = claims.get("email")?.as_str()?;
    Some(json!({ "email": email }))
}

// ---------------------------------------------------------------------------
// Minimal HTTP — client (discovery, token endpoint) and loopback responses.
// ---------------------------------------------------------------------------

struct Discovery {
    authorization_endpoint: String,
    token_endpoint: String,
}

async fn fetch_discovery(state: &AppState, issuer: &str) -> Result<Discovery, String> {
    let url = format!(
        "{}/.well-known/openid-configuration",
        issuer.trim_end_matches('/')
    );
    let (status, body) = crate::http::request(state.connector.as_ref(), &url, None).await?;
    if status != 200 {
        return Err(format!("HTTP {status} on {url}"));
    }
    let v: Value = serde_json::from_str(&body).map_err(|e| format!("unreadable discovery: {e}"))?;
    let field = |name: &str| {
        v.get(name)
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| format!("discovery without {name}"))
    };
    Ok(Discovery {
        authorization_endpoint: field("authorization_endpoint")?,
        token_endpoint: field("token_endpoint")?,
    })
}

/// POST form → JSON. An OAuth error (4xx with a `{ "error": … }` body) is
/// returned as `Err(error_name)` — `invalid_grant` can be tested for.
async fn post_form(state: &AppState, url: &str, fields: &[(&str, &str)]) -> Result<Value, String> {
    // Block: the `Serializer` is not `Send`, it must not straddle the await.
    let encoded = {
        let mut form = form_urlencoded::Serializer::new(String::new());
        for (name, value) in fields {
            form.append_pair(name, value);
        }
        form.finish()
    };
    let (status, body) = crate::http::request(state.connector.as_ref(), url, Some(encoded)).await?;
    if status != 200 {
        let parsed = serde_json::from_str::<Value>(&body).ok();
        let field = |name: &str| {
            parsed
                .as_ref()
                .and_then(|v| v.get(name).and_then(Value::as_str).map(str::to_string))
        };
        let error = field("error");
        // The IdP's `error_description` is logged — the return carries only the
        // CODE (which `fresh_id_token` tests for `invalid_grant`). Without this,
        // a Google "invalid_request" (e.g. "client_secret is missing") stays
        // undecipherable on the user side.
        tracing::warn!(
            status,
            error = error.as_deref().unwrap_or("?"),
            description = field("error_description").as_deref().unwrap_or(""),
            "OIDC token endpoint failed"
        );
        return Err(error.unwrap_or_else(|| format!("HTTP {status}")));
    }
    serde_json::from_str(&body).map_err(|e| format!("unreadable response: {e}"))
}

/// The first line of a loopback request ("GET /callback?… HTTP/1.1") — the
/// headers are read (bounded) and ignored.
async fn read_request_target(conn: &mut TcpStream) -> Option<String> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 1024];
    loop {
        let n = conn.read(&mut chunk).await.ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&chunk[..n]);
        if buf.windows(4).any(|w| w == b"\r\n\r\n") {
            break;
        }
        if buf.len() > REQUEST_MAX {
            return None;
        }
    }
    let text = String::from_utf8_lossy(&buf);
    let first = text.lines().next()?;
    let mut parts = first.split_whitespace();
    if parts.next()? != "GET" {
        return None;
    }
    parts.next().map(str::to_string)
}

/// Responds to the browser: a minimal English page, connection closed.
async fn respond(conn: &mut TcpStream, status: u16, message: &str) {
    let reason = match status {
        200 => "OK",
        403 => "Forbidden",
        404 => "Not Found",
        502 => "Bad Gateway",
        _ => "Bad Request",
    };
    let body = format!(
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\">\
         <title>UniversalLink</title></head><body><p>{}</p></body></html>",
        escape_html(message)
    );
    let response = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: text/html; charset=utf-8\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = tokio::time::timeout(HTTP_TIMEOUT, conn.write_all(response.as_bytes())).await;
    let _ = conn.shutdown().await;
}

fn escape_html(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}
