// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

//! Test harness: test IPC component (LSP framing + JSON-RPC over
//! UDS / named pipe) and a Core started in a temporary directory.
//!
//! Protocol decisions **frozen by this suite** (they complement doc/core-api.md):
//! - Transport: `Config.ipc_path` — unix: path of the UDS socket; windows:
//!   full name of the pipe (`\\.\pipe\…`).
//! - LSP framing: `Name: value` lines terminated by `\r\n` (a lone `\n` is
//!   tolerated), an empty line, then exactly `Content-Length` bytes of UTF-8
//!   JSON. Only `Content-Length` is required (case-insensitive), any other
//!   header is ignored. Framing violation (line without a `:`, unreadable
//!   length or > 16 MiB, header section > 8 KiB) → the Core closes the
//!   connection. No heartbeat on the local IPC: EOF is authoritative.
//! - JSON-RPC 2.0: same grammar as the server API (numeric id, notifications
//!   without an id, application codes in `error.data.code`, `-32700` with
//!   `id: null`). Unknown client→Core notification: ignored.
//! - `api_version` = 1 — versioning specific to the IPC, independent of the
//!   server.
//! - `hello`: `name`, `version`, `role`, `scopes` required, `token` optional.
//!   `role` and `scopes` are closed enumerations (`-32602` otherwise).
//!   A refused hello leaves the connection pristine (retrying is allowed);
//!   after an accepted hello, any re-hello → `-32600`.
//! - Spawn token (`CoreHandle::mint_spawn_token`): consumed by an ACCEPTED
//!   hello only; the hello's role must be that of the mint (`INVALID_TOKEN`
//!   otherwise); requested scopes ⊆ minted scopes (`SCOPE_DENIED` otherwise);
//!   granted = requested, in the order of the request.
//! - File token: `ipc-token` (0600), regenerated at each startup, reusable;
//!   grants all requested scopes, `components.approve` included.
//! - Third-party enrollment: hello without a token → `{ status: "pending" }`
//!   (without a request_id); `component.pending` is pushed WITHOUT a
//!   subscription to every active connection that holds `components.approve`;
//!   the granted token is persistent (in memory in v1: a Core restart forgets
//!   it); deny → `enrollment.decided { approved: false }` then closure by the
//!   Core; `components.approve` is never grantable via approve (`-32602`).
//! - `ROLE_CONFLICT` (`clipboard-backend` exclusive): checked at ACTIVATION,
//!   either at the hello with a token, or at the approve — where the request
//!   then stays pending. A hello without a token can therefore queue a request
//!   for an occupied role (replacing the backend in place is a matter for a
//!   deliberate request).
//! - Identifier prefixes: `r_` (request), `c_` (component). Unknown reference
//!   (request_id, component_id) → `-32602`.
//! - `peer_info` = `{ pid?, exe? }`, best-effort per platform: linux and
//!   windows provide both, macOS none in v1.
//! - The phase is checked BEFORE the params: a non-enrolled connection
//!   receives `NOT_ENROLLED`/`PENDING_APPROVAL`, never `-32602` — it probes
//!   neither the methods nor the shape of their parameters.
//! - `events.subscribe` replaces the current subscription; unknown topic →
//!   `-32602`; topic without the required scope → `SCOPE_DENIED` (nothing is
//!   applied). Result: `{}`.
//! - `components.list`: all active connections (bootstrap and GUI included) +
//!   the enrolled third parties even when disconnected →
//!   `{ component_id, name, role, scopes, connected, enrolled }`.
//!   `enrolled` distinguishes the third party bearing a persistent (revocable)
//!   token from the bootstrap connection (file token or spawn token), which
//!   `components.revoke` would merely disconnect. The role is not enough: an
//!   approved third party can bear any role.
//! - Closures: no farewell message — the Core closes the socket (EOF).
//!
//! Server session (building block 2 — OIDC enrollment is building block 3, the
//! harness seeds the identity as the login will write it):
//! - Identity: `device.key` (config directory, 0600) = Ed25519 seed in hex
//!   (64 chars), generated at first startup if absent — it is the same identity
//!   as iroh. `session.json` = `{ server_url, device_id, account? }`; its
//!   presence counts as `logged_in`, `account` (opaque JSON, written by the
//!   login) is replayed as-is in `session.status` and `session.changed`.
//! - `server_connected` only turns `true` once the directory is snapshotted
//!   (authenticate then server-side devices.list): connected ⇒ primed cache.
//! - `session.changed { logged_in, server_connected, account? }` on each
//!   transition (topic `session`); the logout emits only one notification.
//! - Reconnection: exponential backoff (base `Config.reconnect_base_delay`,
//!   doubled, capped). Server closure `REPLACED` → we reconnect (replacing the
//!   possible intruder); `DEVICE_REVOKED` on closure or on an authenticate
//!   error, and `DEVICE_UNKNOWN` → session abandoned (`session.json` deleted,
//!   `session.changed`), the enrollment is to be redone.
//! - `session.logout`: idempotent (`{}` even outside a session), cuts the
//!   connection (the server broadcasts `device.offline`), deletes
//!   `session.json` (or empties it if the deletion fails — an empty file counts
//!   as "no session" at startup), does not reconnect.
//! - `devices.list` IPC: serves the cache (last known snapshot), even when
//!   disconnected — freshness is read from `session.changed`;
//!   `SERVER_UNREACHABLE` if no snapshot since startup (or no session). Each
//!   device record served over the IPC is enriched with `is_self`.
//! - `devices.rename` IPC: proxy to the server; the response (`{ device }`) is
//!   relayed enriched; a server error is relayed as-is (JSON-RPC code +
//!   `data.code`); `SERVER_UNREACHABLE` if disconnected. The Core synthesizes
//!   `device.updated` for its IPC subscribers (the exclusion of the requester
//!   is a Core↔server detail, not an IPC one).
//! - Relaying of `device.*` (topic `devices`, subscription required): same
//!   payloads as on the server side, records enriched with `is_self`. On
//!   (re)connection, no synthetic diff: `session.changed` invites a re-snapshot.
//!
//! OIDC login + enrollment (building block 3):
//! - `session.login` (scope `session.manage`): OIDC discovery on the configured
//!   issuer (`Config.server`), PKCE S256, loopback listener
//!   `http://127.0.0.1:{port}/callback` → `{ auth_url }` — the caller opens the
//!   browser, the completion is read from `session.changed`. Already logged in →
//!   `ALREADY_LOGGED_IN`; Core without a server config, or unreachable IdP →
//!   `SERVER_UNREACHABLE`. A pending flow is REPLACED by the next one (its
//!   listener dies); a logout outside a session does not cancel a login flow —
//!   but the end of a session (logout, revocation) CANCELS the pending re-auth
//!   flow, and a re-auth that would untangle after the fact fails without
//!   stashing anything in the keyring (guards `logged_in`).
//! - Loopback callback: unknown `state` → 400 and the flow SURVIVES (a forged
//!   request does not kill a login in progress); another path → 404, survives
//!   too; `error` from the IdP (user refusal) → 403, flow consumed; `code` →
//!   the page (200 success / 502 failure) only responds once the enrollment —
//!   or the revocation — is untangled.
//! - Login completion: PKCE exchange, enrollment over a dedicated WS connection
//!   (challenge → `auth.enroll` with the `device.key` identity), writing of
//!   `session.json` (`account` = `{ email }` from the ID token claim, absent
//!   otherwise), refresh token → keyring, start of the session task:
//!   `session.changed { logged_in: true, server_connected: false }` then the
//!   connection converges. A re-login re-enrolls a NEW device (v1: the old entry
//!   stays in the directory, offline).
//! - Keyring: injected `SecretStore` (`Config.secret_store`); `FileSecretStore`
//!   = `secrets.json` 0600 (file fallback, the binary will wire up the OS).
//!   Secret "oidc-refresh-token", deleted on logout and on session abandonment
//!   (revocation).
//! - `devices.revoke` IPC (scope `devices.manage`): refresh token → fresh ID
//!   token → server proxy → `{ status: "done" }` (the Core synthesizes
//!   `device.removed` for its subscribers, cache updated in the session task,
//!   like the rename). Refresh absent or dead (`invalid_grant` — the secret is
//!   then thrown out), or `OIDC_INVALID` from the server →
//!   `{ status: "reauth_required", auth_url }`: the browser completion carries
//!   out the revocation and stashes the fresh refresh token. Other server
//!   errors relayed as-is; disconnected → `SERVER_UNREACHABLE` (before any OIDC
//!   spend). Self-revocation follows the existing `DEVICE_REVOKED` path: session
//!   abandoned.
//!
//! Startup and outbound transport (daemon building block):
//! - `spawn` takes over listening BEFORE writing `ipc-token`: a second Core
//!   must give up without having revoked the first's secret. It returns
//!   `SpawnError::AlreadyRunning` — not a failure, a Core already in place; the
//!   lib NEVER exits the process (it runs in-process in the tests), it is up to
//!   the binary to conclude.
//! - Single instance: on unix, a non-blocking `flock` on `<socket>.lock`, held
//!   by the `CoreHandle` (and not by the `Listener`, which the accept task owns
//!   and which we only `abort()`) — an immediate restart on the same socket
//!   thus reclaims the lock right away. Windows: `first_pipe_instance`, already
//!   there.
//! - `CoreHandle::revoke_spawn_token`: reclaims a still-pristine spawn grant.
//!   The supervisor calls it when each child dies — otherwise a child that
//!   crashed before its `hello` would leave behind a live activation token, one
//!   more at each relaunch.
//! - `Config.connector` (`Arc<dyn Connector>`, `Debug` mandatory like
//!   `SecretStore`) opens ALL outbound streams: server WS, enrollment WS, IdP
//!   HTTP. The URL scheme decides the encryption, and the lib settles it once
//!   and for all; `PlainConnector` REFUSES a `wss`/`https` target rather than
//!   serving it in the clear. TLS lives in the binary: no TLS stack
//!   cross-compiles without a C compiler for the target, and the Core is
//!   cross-checked lib-only.
//! - The Core's HTTP client: `Connection: close`, body delimited by
//!   `Content-Length` OR `Transfer-Encoding: chunked` — Google's token endpoint
//!   is always chunked, discovery is not. The harness's fake OIDC mimics
//!   exactly that.
//!
//! File transfers (building block T2 — homegrown protocol over the data-plane
//! streams):
//! - `files.send { device_id, paths[] }` (scope `files.send`) → `{ transfer_id }`,
//!   fire-and-forget: tracking goes through the `transfers` topic. The target is
//!   resolved by the directory, C7 attestation verified BEFORE any opening —
//!   absent/foreign → `DEVICE_UNKNOWN` (fail-closed, indistinguishable); known
//!   but without a published relay → `DEVICE_OFFLINE`. v1 "flat files": each
//!   path must be a regular file — a directory or an absent path → `-32602`.
//! - The receiver AUTO-ACCEPTS (these are the user's devices, C7 gate): the
//!   bytes land in `Config.receive_dir`, each file via a `.part` temporary
//!   renamed atomically at the END of the transfer (nothing partial is ever
//!   exposed); name collision → "(n)" suffix, never an overwrite; the received
//!   name must be a simple basename, refused (failed transfer) if it carries a
//!   separator (`/` or `\`), `..`, `:` or a control character — no traversal,
//!   identical refusal on all OSes.
//! - Topic `transfers` (scope `transfers.read`): `transfer.started`
//!   (outbound) / `transfer.incoming` (inbound) `{ transfer_id, device_id,
//!   files:[{name,size}], total? }`, `transfer.progress { transfer_id, done,
//!   total }` (throttled ~2/s, first and last point always emitted),
//!   `transfer.finished { transfer_id, paths? }` / `transfer.failed
//!   { transfer_id, error }`. Each side has ITS OWN `transfer_id` (local mint,
//!   no cross-device correlation in v1).
//! - `files.cancel { transfer_id }` (scope `files.send`, both directions) → `{}`,
//!   or `TRANSFER_UNKNOWN` (already finished / never existed). The terminal
//!   outcome (`transfer.failed { error: "cancelled" }`) is emitted by the task,
//!   ONCE, AFTER deregistration — a `files.cancel` replayed immediately after
//!   seeing it therefore finds `TRANSFER_UNKNOWN`.

#![allow(dead_code)]

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader, ReadHalf, WriteHalf};
use tokio::time::timeout;

use universallink_core::{Config, CoreHandle};
// Harness server building blocks: fake OIDC (+ browser flow), device keys,
// WS JSON-RPC client, mini HTTP browser. `TEST_SUB`/`TEST_EMAIL`: the test
// account — all the harness's devices and the Core live there; it is also the
// user that the fake's browser flow authenticates.
// (Selective: `RpcError` and the timeouts also exist on the IPC side, below.)
use universallink_test_support::memory_transport::MemorySwitchboard;
pub use universallink_test_support::{
    Device, DeviceKey, FakeOidc, TEST_CLIENT_ID, TEST_EMAIL, TEST_SUB, TestConn, assert_rfc3339,
    authenticate, browse, enroll_device_at, enroll_key, find_device, http_get, url_params,
};

/// Name of the Core's device in the directory (chosen at enrollment).
pub const CORE_DEVICE_NAME: &str = "PC-Core";

pub const RESPONSE_TIMEOUT: Duration = Duration::from_secs(5);
/// Observation window to assert that no notification arrives.
pub const SILENCE_WINDOW: Duration = Duration::from_millis(300);
/// Maximum delay for an asynchronous state to converge (propagated
/// disconnection...).
pub const CONVERGENCE_TIMEOUT: Duration = Duration::from_secs(5);

// ---------------------------------------------------------------------------
// Client stream per platform.
// ---------------------------------------------------------------------------

#[cfg(unix)]
type ClientStream = tokio::net::UnixStream;
#[cfg(windows)]
type ClientStream = tokio::net::windows::named_pipe::NamedPipeClient;

#[cfg(unix)]
async fn connect_stream(path: &Path) -> ClientStream {
    tokio::net::UnixStream::connect(path)
        .await
        .expect("UDS connection")
}

#[cfg(windows)]
async fn connect_stream(path: &Path) -> ClientStream {
    use tokio::net::windows::named_pipe::ClientOptions;
    // All instances of the pipe are busy: retry, as a real component would (the
    // next instance arrives as soon as the Core accepts).
    const ERROR_PIPE_BUSY: i32 = 231;
    let name = path.to_str().expect("UTF-8 pipe name");
    loop {
        match ClientOptions::new().open(name) {
            Ok(stream) => return stream,
            Err(e) if e.raw_os_error() == Some(ERROR_PIPE_BUSY) => {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            Err(e) => panic!("pipe connection: {e}"),
        }
    }
}

#[cfg(unix)]
fn ipc_path_for(dir: &Path) -> PathBuf {
    dir.join("core.sock")
}

#[cfg(windows)]
fn ipc_path_for(_dir: &Path) -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    PathBuf::from(format!(
        r"\\.\pipe\universallink-test-{}-{}",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    ))
}

// ---------------------------------------------------------------------------
// Test environment: the real server (universallink-server lib), behind a
// severable TCP proxy. Cutting the `ServerHandle` would not kill the already
// accepted connections (axum spawns them): the proxy, for its part, lets us
// simulate an unreachable server — connections in progress cut, new ones
// refused — then its return.
// ---------------------------------------------------------------------------

pub struct TestServer {
    pub oidc: FakeOidc,
    _server: universallink_server::ServerHandle,
    /// Direct URL, for the harness's devices (insensitive to `cut`).
    direct_url: String,
    /// URL via the proxy — the one the harness writes into session.json.
    core_url: String,
    up: Arc<AtomicBool>,
    pipes: Arc<std::sync::Mutex<tokio::task::JoinSet<()>>>,
    accept_task: tokio::task::JoinHandle<()>,
}

impl Drop for TestServer {
    fn drop(&mut self) {
        self.accept_task.abort();
    }
}

impl TestServer {
    pub async fn start() -> TestServer {
        let oidc = FakeOidc::start().await;
        let config = universallink_server::Config {
            bind_addr: "127.0.0.1:0".parse().expect("addr"),
            oidc: universallink_server::OidcConfig {
                issuer_url: oidc.issuer(),
                client_id: TEST_CLIENT_ID.into(),
                max_fresh_token_age: Duration::from_secs(300),
            },
            heartbeat_interval: Duration::from_secs(30),
            heartbeat_max_missed: 2,
            nonce_ttl: Duration::from_secs(60),
            max_requests_per_minute: None,
        };
        let server = universallink_server::spawn(config)
            .await
            .expect("server startup");
        let real_addr = server.local_addr();

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("proxy bind");
        let proxy_addr = listener.local_addr().expect("proxy addr");
        let up = Arc::new(AtomicBool::new(true));
        let pipes = Arc::new(std::sync::Mutex::new(tokio::task::JoinSet::new()));
        let accept_task = {
            let up = up.clone();
            let pipes = pipes.clone();
            tokio::spawn(async move {
                loop {
                    let Ok((mut inbound, _)) = listener.accept().await else {
                        return;
                    };
                    // "Unreachable" server: the connection drops before any
                    // WebSocket handshake.
                    if !up.load(Ordering::SeqCst) {
                        continue;
                    }
                    let Ok(mut outbound) = tokio::net::TcpStream::connect(real_addr).await else {
                        continue;
                    };
                    pipes.lock().expect("lock pipes").spawn(async move {
                        let _ = tokio::io::copy_bidirectional(&mut inbound, &mut outbound).await;
                    });
                }
            })
        };

        TestServer {
            oidc,
            _server: server,
            direct_url: format!("ws://{real_addr}/ws"),
            core_url: format!("ws://{proxy_addr}/ws"),
            up,
            pipes,
            accept_task,
        }
    }

    /// The URL the Core knows (session.json).
    pub fn core_url(&self) -> String {
        self.core_url.clone()
    }

    /// Makes the server unreachable for the Core: cuts the connections in
    /// progress and refuses new ones.
    pub fn cut(&self) {
        self.up.store(false, Ordering::SeqCst);
        self.pipes.lock().expect("lock pipes").abort_all();
    }

    /// The server becomes reachable again.
    pub fn restore(&self) {
        self.up.store(true, Ordering::SeqCst);
    }

    /// Harness device, enrolled on the test account (not authenticated: in the
    /// directory, offline).
    pub async fn enrolled_device(&self, name: &str, platform: &str) -> Device {
        enroll_device_at(&self.direct_url, &self.oidc, TEST_SUB, name, platform).await
    }

    /// Harness device, enrolled and authenticated (online) — to observe the
    /// account and act on it as if from another PC.
    pub async fn online_device(&self, name: &str, platform: &str) -> Device {
        let mut d = self.enrolled_device(name, platform).await;
        authenticate(&mut d.conn, &d.key, &d.device_id).await;
        d
    }

    /// Raw direct connection to the server (to authenticate with the Core's
    /// key, revoke, etc.).
    pub async fn connect_direct(&self) -> TestConn {
        TestConn::connect(&self.direct_url).await
    }
}

// ---------------------------------------------------------------------------
// Test environment: a Core in a temporary directory.
// ---------------------------------------------------------------------------

pub struct TestCore {
    pub handle: CoreHandle,
    dir: tempfile::TempDir,
    /// Identity seeded by `start_enrolled` (the Core's device on the server).
    enrolled: Option<(String, DeviceKey)>,
    /// Server+OIDC config passed to the Core (preserved by `restart`).
    server_cfg: Option<universallink_core::ServerConfig>,
    /// Data-plane transport injected at spawn — preserved by `restart`: a
    /// restarted Core keeps its memory switchboard, hence its peers (otherwise a
    /// "reconnection then re-ping" scenario would be inexpressible here).
    transport: Arc<dyn universallink_core::PeerTransport>,
    /// The config `session.reload` re-reads: a shared slot standing in for
    /// `config.json`, so a test can reconfigure the Core the way the GUI does
    /// (see `stage_config` / `stage_invalid_config`). `Err` models a
    /// malformed/half-filled file.
    reload_slot: Arc<std::sync::Mutex<Result<Option<universallink_core::ServerConfig>, String>>>,
}

/// The server+OIDC config of a Core pointed at the test environment.
fn server_cfg(server: &TestServer) -> universallink_core::ServerConfig {
    universallink_core::ServerConfig {
        url: server.core_url(),
        oidc_issuer: server.oidc.issuer(),
        oidc_client_id: TEST_CLIENT_ID.into(),
        oidc_client_secret: None,
    }
}

/// Seeds the account trust root (C7) into `dir`: derives the account key from
/// the `code` recovery code, attests `node_id`, writes `account-key.json` —
/// exactly what `account.join` would do. Without it, the data plane is
/// fail-closed: no peer is authorized or reachable.
fn seed_account_from_code(dir: &Path, node_id: &str, code: &str) {
    let ak =
        universallink_core::account_key::account_key_from_code(code).expect("valid test code");
    let root = universallink_core::account_key::root_for(&ak, node_id);
    universallink_core::account_key::save(dir, &root).expect("seed account-key.json");
}

impl TestCore {
    pub async fn start() -> TestCore {
        let dir = tempfile::tempdir().expect("tempdir");
        Self::spawn_in(dir, None, None, None).await
    }

    /// Core configured (server + OIDC) but never logged in: the state a
    /// `session.login` starts from.
    pub async fn start_with_server(server: &TestServer) -> TestCore {
        Self::start_with_config(server_cfg(server)).await
    }

    /// Core configured with an arbitrary server config (dead IdP...).
    pub async fn start_with_config(cfg: universallink_core::ServerConfig) -> TestCore {
        let dir = tempfile::tempdir().expect("tempdir");
        Self::spawn_in(dir, None, Some(cfg), None).await
    }

    /// Enrolls an identity for the Core on the server, seeds it into the config
    /// directory (device.key + session.json, as the login writes them), then
    /// starts the Core — which must connect on its own.
    pub async fn start_enrolled(server: &TestServer) -> TestCore {
        let (dir, enrolled) = Self::seed_enrolled(server).await;
        Self::spawn_in(dir, Some(enrolled), Some(server_cfg(server)), None).await
    }

    /// Like `start_enrolled`, but the server is cut between the enrollment and
    /// the startup: session on disk, server unreachable from the outset.
    pub async fn start_enrolled_server_cut(server: &TestServer) -> TestCore {
        let (dir, enrolled) = Self::seed_enrolled(server).await;
        server.cut();
        Self::spawn_in(dir, Some(enrolled), Some(server_cfg(server)), None).await
    }

    /// Restarts the Core on the same config directory (new socket) — to test
    /// what survives a restart. The transport is reused: it is the same
    /// "endpoint" coming back, as the real daemon would rewire iroh with the
    /// same `device.key`.
    pub async fn restart(self) -> TestCore {
        let TestCore {
            handle,
            dir,
            enrolled,
            server_cfg,
            transport,
            // A fresh Core re-seeds its reload slot from `server_cfg`; no test
            // stages a config across a restart.
            reload_slot: _,
        } = self;
        drop(handle);
        Self::spawn_in(dir, enrolled, server_cfg, Some(transport)).await
    }

    /// Two Cores enrolled on the SAME account, sharing a memory switchboard:
    /// they open data-plane streams to each other like two iroh endpoints.
    /// Each is registered under its real node_id (`device.key` seeded from its
    /// key) and will publish a synthetic relay in the directory. Same account ⇒
    /// SAME account key (C7): a single recovery code, shared, that each Core
    /// attests for its own node_id — without which they would refuse each other
    /// (fail-closed).
    pub async fn start_pair(server: &TestServer) -> (TestCore, TestCore) {
        let switchboard = MemorySwitchboard::new();
        let code = universallink_core::account_key::generate_recovery_code();
        let a = Self::start_enrolled_on_with_code(server, &switchboard, Some(&code)).await;
        let b = Self::start_enrolled_on_with_code(server, &switchboard, Some(&code)).await;
        (a, b)
    }

    /// Like `start_pair`, but each Core derives a DIFFERENT account key
    /// (distinct codes): they share the server account's directory, but no
    /// common trust root. C7 must make them refuse each other — mere directory
    /// membership no longer suffices.
    pub async fn start_mismatched_pair(server: &TestServer) -> (TestCore, TestCore) {
        let switchboard = MemorySwitchboard::new();
        let code_a = universallink_core::account_key::generate_recovery_code();
        let code_b = universallink_core::account_key::generate_recovery_code();
        let a = Self::start_enrolled_on_with_code(server, &switchboard, Some(&code_a)).await;
        let b = Self::start_enrolled_on_with_code(server, &switchboard, Some(&code_b)).await;
        (a, b)
    }

    /// An enrolled Core plugged into a SHARED memory switchboard, with its own
    /// fresh account key — to compose data-plane scenarios (peers, intruders)
    /// beyond `start_pair`.
    pub async fn start_enrolled_on(
        server: &TestServer,
        switchboard: &MemorySwitchboard,
    ) -> TestCore {
        let code = universallink_core::account_key::generate_recovery_code();
        Self::start_enrolled_on_with_code(server, switchboard, Some(&code)).await
    }

    /// A Core enrolled on `switchboard`. `code = Some(..)` derives and seeds its
    /// trust root (C7) — two Cores with the same code share the account key;
    /// `None` leaves the Core WITHOUT an account key (never joined: the data
    /// plane must then be fail-closed).
    pub async fn start_enrolled_on_with_code(
        server: &TestServer,
        switchboard: &MemorySwitchboard,
        code: Option<&str>,
    ) -> TestCore {
        let (dir, enrolled) = Self::seed_enrolled(server).await;
        let node_id = enrolled.1.node_id();
        if let Some(code) = code {
            seed_account_from_code(dir.path(), &node_id, code);
        }
        let relay = Some(format!("iroh+memory://{node_id}"));
        let transport: Arc<dyn universallink_core::PeerTransport> =
            switchboard.endpoint(node_id, relay);
        Self::spawn_in(
            dir,
            Some(enrolled),
            Some(server_cfg(server)),
            Some(transport),
        )
        .await
    }

    async fn seed_enrolled(server: &TestServer) -> (tempfile::TempDir, (String, DeviceKey)) {
        let dir = tempfile::tempdir().expect("tempdir");
        let key = DeviceKey::generate();
        let mut conn = server.connect_direct().await;
        let device_id = enroll_key(
            &mut conn,
            &server.oidc,
            &key,
            TEST_SUB,
            CORE_DEVICE_NAME,
            std::env::consts::OS,
        )
        .await;
        drop(conn);

        std::fs::write(dir.path().join("device.key"), key.seed_hex()).expect("seed device.key");
        let session = json!({
            "server_url": server.core_url(),
            "device_id": device_id,
            "account": { "email": TEST_EMAIL },
        });
        std::fs::write(dir.path().join("session.json"), session.to_string())
            .expect("seed session.json");
        (dir, (device_id, key))
    }

    async fn spawn_in(
        dir: tempfile::TempDir,
        enrolled: Option<(String, DeviceKey)>,
        server_cfg: Option<universallink_core::ServerConfig>,
        transport: Option<Arc<dyn universallink_core::PeerTransport>>,
    ) -> TestCore {
        // Without a supplied transport, an isolated transport (its own memory
        // switchboard): the tests that do not exercise the data plane have no
        // one on the other side. Registered under the device's node_id if it is
        // known (harmless otherwise).
        let transport = transport.unwrap_or_else(|| {
            let node_id = enrolled
                .as_ref()
                .map(|(_, k)| k.node_id())
                .unwrap_or_else(|| "standalone".into());
            let lone: Arc<dyn universallink_core::PeerTransport> =
                MemorySwitchboard::new().endpoint(node_id, None);
            lone
        });
        // The reload source is a shared slot, seeded with the initial config: a
        // test can rewrite it (as the GUI rewrites config.json) then trigger
        // `session.reload` — see `stage_config` / `stage_invalid_config`.
        // Existing tests never touch it, so a reload just re-serves the same
        // config.
        let reload_slot = Arc::new(std::sync::Mutex::new(Ok(server_cfg.clone())));
        let config = Config {
            ipc_path: ipc_path_for(dir.path()),
            config_dir: dir.path().to_path_buf(),
            server: server_cfg.clone(),
            reload_server: {
                let slot = reload_slot.clone();
                Arc::new(move || slot.lock().expect("reload slot").clone())
            },
            device_name: CORE_DEVICE_NAME.into(),
            // The file fallback — it is also what the harness inspects
            // (`secret`).
            secret_store: Arc::new(universallink_core::FileSecretStore::new(dir.path())),
            // The lib only speaks in the clear: it is the daemon that wires up
            // TLS.
            connector: Arc::new(universallink_core::PlainConnector),
            transport: transport.clone(),
            // Receive directory inspectable by the transfer tests.
            receive_dir: dir.path().join("received"),
            // Short: the reconvergence tests wait in windows of a few hundred
            // ms.
            reconnect_base_delay: Duration::from_millis(50),
        };
        let handle = universallink_core::spawn(config)
            .await
            .expect("Core startup");
        TestCore {
            handle,
            dir,
            enrolled,
            server_cfg,
            transport,
            reload_slot,
        }
    }

    /// Rewrites the config that `session.reload` will pick up — the harness
    /// equivalent of the GUI writing a fresh `config.json`.
    pub fn stage_config(&self, cfg: Option<universallink_core::ServerConfig>) {
        *self.reload_slot.lock().expect("reload slot") = Ok(cfg);
    }

    /// Models a malformed / half-filled `config.json`: `session.reload` will
    /// surface `reason` as `INVALID_CONFIG` rather than reconfigure.
    pub fn stage_invalid_config(&self, reason: &str) {
        *self.reload_slot.lock().expect("reload slot") = Err(reason.to_string());
    }

    /// device_id of the Core in the directory (panics outside `start_enrolled`).
    pub fn device_id(&self) -> &str {
        &self.enrolled.as_ref().expect("enrolled Core").0
    }

    /// Key of the Core's device (panics outside `start_enrolled`).
    pub fn key(&self) -> &DeviceKey {
        &self.enrolled.as_ref().expect("enrolled Core").1
    }

    pub fn config_dir(&self) -> &Path {
        self.dir.path()
    }

    /// A keyring secret (`secrets.json`), as the Core stashed it.
    pub fn secret(&self, name: &str) -> Option<String> {
        let text = std::fs::read_to_string(self.dir.path().join("secrets.json")).ok()?;
        serde_json::from_str::<serde_json::Map<String, Value>>(&text)
            .ok()?
            .get(name)?
            .as_str()
            .map(str::to_string)
    }

    /// The file token, as the GUI would read it.
    pub fn file_token(&self) -> String {
        std::fs::read_to_string(self.dir.path().join("ipc-token"))
            .expect("read ipc-token")
            .trim()
            .to_string()
    }

    /// Bootstrap path B: the token a supervisor would pass at spawn.
    pub fn mint(&self, role: &str, scopes: &[&str]) -> String {
        self.handle.mint_spawn_token(role, scopes)
    }

    /// The directory where the files received by this Core land, to inspect
    /// after a transfer.
    pub fn receive_dir(&self) -> PathBuf {
        self.dir.path().join("received")
    }

    /// Writes a source file (into an `outbox/`) to be sent by `files.send`, and
    /// returns its absolute path.
    pub fn write_source(&self, name: &str, contents: &[u8]) -> PathBuf {
        let dir = self.dir.path().join("outbox");
        std::fs::create_dir_all(&dir).expect("create outbox");
        let path = dir.join(name);
        std::fs::write(&path, contents).expect("write the source file");
        path
    }

    /// A second Core on the SAME listening point and the same directory: what a
    /// user who launches the daemon twice does. Returns the error — it is not
    /// supposed to start.
    pub async fn start_rival(&self) -> universallink_core::SpawnError {
        let config = Config {
            ipc_path: self.handle.ipc_path().to_path_buf(),
            config_dir: self.dir.path().to_path_buf(),
            server: self.server_cfg.clone(),
            reload_server: {
                let s = self.server_cfg.clone();
                Arc::new(move || Ok::<_, String>(s.clone()))
            },
            device_name: CORE_DEVICE_NAME.into(),
            secret_store: Arc::new(universallink_core::FileSecretStore::new(self.dir.path())),
            connector: Arc::new(universallink_core::PlainConnector),
            transport: MemorySwitchboard::new().endpoint("rival", None),
            receive_dir: self.dir.path().join("received"),
            reconnect_base_delay: Duration::from_millis(50),
        };
        universallink_core::spawn(config)
            .await
            .err()
            .expect("a second Core must not start")
    }

    pub async fn connect(&self) -> TestComponent {
        let stream = connect_stream(self.handle.ipc_path()).await;
        let (read, write) = tokio::io::split(stream);
        TestComponent {
            reader: BufReader::new(read),
            writer: write,
            next_id: 0,
            notifications: VecDeque::new(),
        }
    }

    /// Opens a data-channel connection and presents `channel_token` (the LSP
    /// attach frame). The connection then speaks the binary protocol.
    pub async fn open_channel(&self, channel_token: &str) -> DataChannel {
        let mut stream = connect_stream(self.handle.ipc_path()).await;
        let attach = frame(&json!({ "channel_token": channel_token }).to_string());
        stream.write_all(&attach).await.expect("attach frame");
        DataChannel { stream }
    }
}

// ---------------------------------------------------------------------------
// Test component: LSP framing + JSON-RPC, buffered notifications. An
// implementation independent of the Core's — it is the protocol conformance
// mirror.
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub struct RpcError {
    /// JSON-RPC code (`error.code`).
    pub code: i64,
    pub message: String,
    /// Application code (`error.data.code`).
    pub data_code: Option<String>,
}

impl RpcError {
    /// Application code, panics if it is absent.
    pub fn app_code(&self) -> &str {
        self.data_code
            .as_deref()
            .unwrap_or_else(|| panic!("no application code in the error: {self:?}"))
    }
}

pub struct TestComponent {
    reader: BufReader<ReadHalf<ClientStream>>,
    writer: WriteHalf<ClientStream>,
    next_id: u64,
    notifications: VecDeque<(String, Value)>,
}

impl TestComponent {
    /// Sends a JSON-RPC request and waits for its response. The notifications
    /// received in the meantime are buffered.
    pub async fn request(&mut self, method: &str, params: Value) -> Result<Value, RpcError> {
        self.next_id += 1;
        let id = self.next_id;
        let msg = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
        self.send_frame(&msg.to_string()).await;

        timeout(RESPONSE_TIMEOUT, async {
            loop {
                let v = self.recv_json().await;
                if v.get("method").is_some() {
                    self.buffer_notification(v);
                } else if v.get("id") == Some(&json!(id)) {
                    return parse_response(v);
                } else {
                    panic!("response for an unexpected id: {v}");
                }
            }
        })
        .await
        .unwrap_or_else(|_| panic!("timeout waiting for the response to {method}"))
    }

    /// Full `hello`; `token: None` = third-party enrollment path.
    pub async fn hello(
        &mut self,
        name: &str,
        role: &str,
        scopes: &[&str],
        token: Option<&str>,
    ) -> Result<Value, RpcError> {
        let mut params = json!({
            "name": name,
            "version": "0.0-test",
            "role": role,
            "scopes": scopes,
        });
        if let Some(token) = token {
            params["token"] = json!(token);
        }
        self.request("hello", params).await
    }

    /// Waits for an incoming REQUEST from the Core (`clipboard.get_data`):
    /// a frame with a `method` AND an `id`. Notifications in the meantime are
    /// buffered. Returns `(id, params)`.
    pub async fn expect_request(&mut self, method: &str) -> (u64, Value) {
        timeout(RESPONSE_TIMEOUT, async {
            loop {
                let v = self.recv_json().await;
                match (v.get("method").and_then(Value::as_str), v.get("id").and_then(Value::as_u64)) {
                    (Some(m), Some(id)) => {
                        assert_eq!(m, method, "unexpected incoming request");
                        return (id, v.get("params").cloned().unwrap_or(Value::Null));
                    }
                    (Some(_), None) => self.buffer_notification(v),
                    _ => panic!("unexpected frame while waiting for request {method}: {v}"),
                }
            }
        })
        .await
        .unwrap_or_else(|_| panic!("timeout waiting for the incoming request {method}"))
    }

    /// Replies to a Core-issued request with a result.
    pub async fn respond(&mut self, id: u64, result: Value) {
        self.send_frame(&json!({ "jsonrpc": "2.0", "id": id, "result": result }).to_string())
            .await;
    }

    /// Replies to a Core-issued request with an application error code.
    pub async fn respond_error(&mut self, id: u64, code: &str) {
        self.send_frame(
            &json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": { "code": -32000, "message": code, "data": { "code": code } },
            })
            .to_string(),
        )
        .await;
    }

    /// Next notification (buffered or upcoming) → `(method, params)`.
    pub async fn notification(&mut self) -> (String, Value) {
        if let Some(n) = self.notifications.pop_front() {
            return n;
        }
        timeout(RESPONSE_TIMEOUT, async {
            let v = self.recv_json().await;
            assert!(
                v.get("method").is_some(),
                "unexpected response while waiting for a notification: {v}"
            );
            split_notification(v)
        })
        .await
        .expect("timeout waiting for a notification")
    }

    /// The next notification MUST be `method`; returns its params.
    pub async fn expect_notification(&mut self, method: &str) -> Value {
        let (m, params) = self.notification().await;
        assert_eq!(m, method, "unexpected notification (params: {params})");
        params
    }

    /// Waits for a `method` notification, ignoring the others.
    pub async fn wait_notification(&mut self, method: &str) -> Value {
        loop {
            let (m, params) = self.notification().await;
            if m == method {
                return params;
            }
        }
    }

    /// Checks that no notification arrives during `SILENCE_WINDOW`.
    pub async fn assert_silent(&mut self) {
        if let Some((m, p)) = self.notifications.front() {
            panic!("unexpected notification in the buffer: {m} {p}");
        }
        match timeout(SILENCE_WINDOW, self.recv_frame_opt()).await {
            Err(_) => {}
            Ok(Some(text)) => panic!("unexpected message: {text}"),
            Ok(None) => panic!("connection closed during the silence window"),
        }
    }

    /// Empties the buffer and absorbs whatever arrives during `SILENCE_WINDOW`.
    pub async fn drain(&mut self) {
        self.notifications.clear();
        while let Ok(Some(_)) = timeout(SILENCE_WINDOW, self.recv_frame_opt()).await {}
    }

    /// Waits for the Core to close the connection (strict EOF: any frame
    /// received in the meantime is an error).
    pub async fn expect_close(&mut self) {
        if let Some((m, p)) = self.notifications.front() {
            panic!("message received before the close: {m} {p}");
        }
        match timeout(RESPONSE_TIMEOUT, self.recv_frame_opt()).await {
            Ok(None) => {}
            Ok(Some(text)) => panic!("message received before the close: {text}"),
            Err(_) => panic!("timeout waiting for the close"),
        }
    }

    /// Sends a well-formed LSP frame carrying `text` (protocol tests).
    pub async fn send_frame(&mut self, text: &str) {
        let bytes = frame(text);
        self.send_bytes(&bytes).await;
    }

    /// Sends raw bytes (framing violations, pipelining...).
    pub async fn send_bytes(&mut self, bytes: &[u8]) {
        // The Core may close DURING the send (violation detected mid-flood):
        // where the socket buffer is small (macOS: ~8 KiB on UDS), the write
        // blocks then dies with EPIPE/reset. For a violation test that is an
        // acceptable outcome, not an error.
        match self.writer.write_all(bytes).await {
            Ok(()) => {}
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::BrokenPipe | std::io::ErrorKind::ConnectionReset
                ) => {}
            Err(e) => panic!("IPC write: {e}"),
        }
    }

    /// Next raw JSON message (response OR notification).
    pub async fn recv_raw_json(&mut self) -> Value {
        timeout(RESPONSE_TIMEOUT, self.recv_json())
            .await
            .expect("timeout waiting for a message")
    }

    fn buffer_notification(&mut self, v: Value) {
        assert!(
            v.get("id").is_none_or(Value::is_null),
            "a notification must not have an id: {v}"
        );
        self.notifications.push_back(split_notification(v));
    }

    async fn recv_json(&mut self) -> Value {
        let text = self
            .recv_frame_opt()
            .await
            .expect("connection closed by the Core");
        serde_json::from_str(&text).expect("invalid JSON received from the Core")
    }

    /// Next LSP frame; `None` = EOF.
    async fn recv_frame_opt(&mut self) -> Option<String> {
        let mut content_length: Option<usize> = None;
        let mut line = String::new();
        loop {
            line.clear();
            let n = match self.reader.read_line(&mut line).await {
                Ok(n) => n,
                // Closure by the Core with unread bytes of ours pending: the OS
                // may produce a reset rather than a clean EOF.
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::ConnectionReset | std::io::ErrorKind::BrokenPipe
                    ) =>
                {
                    0
                }
                Err(e) => panic!("IPC read: {e}"),
            };
            if n == 0 {
                assert!(
                    content_length.is_none(),
                    "EOF in the middle of a header section"
                );
                return None;
            }
            let trimmed = line.trim_end_matches(['\r', '\n']);
            if trimmed.is_empty() {
                break;
            }
            let (name, value) = trimmed
                .split_once(':')
                .unwrap_or_else(|| panic!("header without a colon: {trimmed:?}"));
            if name.eq_ignore_ascii_case("content-length") {
                content_length = Some(value.trim().parse().expect("unreadable Content-Length"));
            }
        }
        let len = content_length.expect("frame without a Content-Length");
        let mut buf = vec![0u8; len];
        self.reader.read_exact(&mut buf).await.expect("IPC payload");
        Some(String::from_utf8(buf).expect("non-UTF-8 payload"))
    }
}

// ---------------------------------------------------------------------------
// Data-channel client: the binary protocol mirror (u32 length + tag byte +
// payload), independent of the Core's implementation. DATA carries an 8-byte
// big-endian offset before the bytes; a request is answered by DATA* then EOF,
// or a single ERROR.
// ---------------------------------------------------------------------------

const TAG_READ: u8 = 0x01;
const TAG_FETCH: u8 = 0x02;
const TAG_ABORT: u8 = 0x03;
const TAG_DATA: u8 = 0x10;
const TAG_EOF: u8 = 0x11;
const TAG_ERROR: u8 = 0x12;

pub struct DataChannel {
    stream: ClientStream,
}

impl DataChannel {
    /// A `READ` of `[offset, offset+len)` of a manifest file → the assembled
    /// bytes (in offset order), or the `ERROR` code on failure.
    pub async fn read(&mut self, file_id: &str, offset: u64, len: u64) -> Result<Vec<u8>, String> {
        let req = json!({ "file_id": file_id, "offset": offset, "len": len });
        self.send(TAG_READ, &serde_json::to_vec(&req).unwrap()).await;
        self.collect().await
    }

    /// A `FETCH` of an inline format → the assembled blob, or the `ERROR` code.
    pub async fn fetch(&mut self, format: &str) -> Result<Vec<u8>, String> {
        let req = json!({ "format": format });
        self.send(TAG_FETCH, &serde_json::to_vec(&req).unwrap()).await;
        self.collect().await
    }

    /// Sends an `ABORT` (cancels the in-flight request; the channel survives).
    pub async fn abort(&mut self) {
        self.send(TAG_ABORT, &[]).await;
    }

    /// Reads the next response WITHOUT issuing a request — for a terminal ERROR
    /// the Core pushes on its own when a reset cuts the session (`TX_STALE`,
    /// `PEER_GONE`): `Err(code)`, or `Err("closed")` if the channel closes first.
    pub async fn next_response(&mut self) -> Result<Vec<u8>, String> {
        self.collect().await
    }

    // Provider side (the source backend pushing an inline blob).

    /// Streams a `DATA` chunk (`offset` + bytes).
    pub async fn send_data(&mut self, offset: u64, bytes: &[u8]) {
        let mut payload = offset.to_be_bytes().to_vec();
        payload.extend_from_slice(bytes);
        self.send(TAG_DATA, &payload).await;
    }

    /// Ends the blob.
    pub async fn send_eof(&mut self) {
        self.send(TAG_EOF, &[]).await;
    }

    /// Fails the blob with an error code.
    pub async fn send_error(&mut self, code: &str) {
        self.send(TAG_ERROR, &serde_json::to_vec(&json!({ "code": code })).unwrap())
            .await;
    }

    async fn send(&mut self, tag: u8, payload: &[u8]) {
        let len = (1 + payload.len()) as u32;
        let mut frame = len.to_be_bytes().to_vec();
        frame.push(tag);
        frame.extend_from_slice(payload);
        // The Core may have closed the channel already (e.g. a proactive
        // TX_STALE on a reset): a failed send is not the assertion — the pending
        // ERROR frame, read by `collect`, is.
        match self.stream.write_all(&frame).await {
            Ok(()) => {}
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::BrokenPipe | std::io::ErrorKind::ConnectionReset
                ) => {}
            Err(e) => panic!("data-channel send: {e}"),
        }
    }

    /// Collects one response: DATA* then EOF (→ the bytes), or ERROR (→ its
    /// code), or a closed channel (→ "closed").
    async fn collect(&mut self) -> Result<Vec<u8>, String> {
        let mut out = Vec::new();
        loop {
            match timeout(RESPONSE_TIMEOUT, self.recv())
                .await
                .expect("timeout on the data channel")
            {
                None => return Err("closed".to_string()),
                Some((TAG_DATA, payload)) => {
                    assert!(payload.len() >= 8, "DATA without an offset header");
                    out.extend_from_slice(&payload[8..]);
                }
                Some((TAG_EOF, _)) => return Ok(out),
                Some((TAG_ERROR, payload)) => {
                    let v: Value = serde_json::from_slice(&payload).expect("ERROR payload");
                    return Err(v["code"].as_str().expect("error code").to_string());
                }
                Some((tag, _)) => panic!("unexpected data-channel tag: {tag:#x}"),
            }
        }
    }

    /// Reads one message; `None` on a clean EOF.
    async fn recv(&mut self) -> Option<(u8, Vec<u8>)> {
        let mut len = [0u8; 4];
        match self.stream.read_exact(&mut len).await {
            Ok(_) => {}
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::UnexpectedEof
                        | std::io::ErrorKind::ConnectionReset
                        | std::io::ErrorKind::BrokenPipe
                ) =>
            {
                return None;
            }
            Err(e) => panic!("data-channel read: {e}"),
        }
        let len = u32::from_be_bytes(len) as usize;
        let mut buf = vec![0u8; len];
        self.stream.read_exact(&mut buf).await.expect("data-channel payload");
        let tag = buf.remove(0);
        Some((tag, buf))
    }
}

/// Encodes `text` into an LSP frame.
pub fn frame(text: &str) -> Vec<u8> {
    let mut bytes = format!("Content-Length: {}\r\n\r\n", text.len()).into_bytes();
    bytes.extend_from_slice(text.as_bytes());
    bytes
}

fn split_notification(v: Value) -> (String, Value) {
    let method = v["method"].as_str().expect("method").to_string();
    let params = v.get("params").cloned().unwrap_or(Value::Null);
    (method, params)
}

fn parse_response(v: Value) -> Result<Value, RpcError> {
    assert_eq!(v["jsonrpc"], "2.0", "non-JSON-RPC 2.0 response: {v}");
    if let Some(err) = v.get("error") {
        Err(RpcError {
            code: err["code"].as_i64().expect("error.code"),
            message: err["message"].as_str().unwrap_or_default().to_string(),
            data_code: err
                .pointer("/data/code")
                .and_then(Value::as_str)
                .map(String::from),
        })
    } else {
        Ok(v.get("result").cloned().unwrap_or(Value::Null))
    }
}

// ---------------------------------------------------------------------------
// Flows: ready-to-use components.
// ---------------------------------------------------------------------------

/// Official component: spawn token + accepted hello.
pub async fn spawn_component(
    core: &TestCore,
    name: &str,
    role: &str,
    scopes: &[&str],
) -> TestComponent {
    let token = core.mint(role, scopes);
    let mut c = core.connect().await;
    let r = c
        .hello(name, role, scopes, Some(&token))
        .await
        .expect("hello with spawn token");
    assert_eq!(r["status"], "ok");
    c
}

/// GUI: file token, approval scopes included.
pub async fn gui(core: &TestCore) -> TestComponent {
    let token = core.file_token();
    let mut c = core.connect().await;
    let r = c
        .hello(
            "gui-test",
            "gui",
            &[
                "components.approve",
                "session.read",
                "devices.read",
                "transfers.read",
            ],
            Some(&token),
        )
        .await
        .expect("GUI hello (file token)");
    assert_eq!(r["status"], "ok");
    c
}

/// Third-party component without a token: hello answered `pending`.
pub async fn pending_component(
    core: &TestCore,
    name: &str,
    role: &str,
    scopes: &[&str],
) -> TestComponent {
    let mut c = core.connect().await;
    let r = c
        .hello(name, role, scopes, None)
        .await
        .expect("hello without a token");
    assert_eq!(r["status"], "pending");
    c
}

/// Retries `attempt` (50 ms step) until `true`, within the limit of
/// `CONVERGENCE_TIMEOUT` — for the states that converge after a disconnection,
/// which the Core processes asynchronously.
pub async fn eventually(mut attempt: impl AsyncFnMut() -> bool, what: &str) {
    let deadline = tokio::time::Instant::now() + CONVERGENCE_TIMEOUT;
    loop {
        if attempt().await {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "condition never reached: {what}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Runs through a complete login as seen by `c` (scopes `session.manage` +
/// `session.read`): `session.login`, browser flow, connected session.
pub async fn complete_login(c: &mut TestComponent) {
    let r = c
        .request("session.login", json!({}))
        .await
        .expect("session.login");
    let auth_url = r["auth_url"].as_str().expect("auth_url");
    let page = browse(auth_url).await.expect("browser flow");
    assert_eq!(page.status, 200, "completion page: {}", page.body);
    wait_server_connected(c, true).await;
}

/// The Core's device_id, read from the directory (`is_self`) — for the Cores
/// enrolled by a real login, whose id the harness does not know.
pub async fn own_device_id(c: &mut TestComponent) -> String {
    let list = c
        .request("devices.list", json!({}))
        .await
        .expect("devices.list");
    list.as_array()
        .expect("list of devices")
        .iter()
        .find(|d| d["is_self"] == json!(true))
        .unwrap_or_else(|| panic!("no is_self device: {list}"))["device_id"]
        .as_str()
        .expect("device_id")
        .to_string()
}

/// Waits for `session.status` (as seen by `c`, which has `session.read`) to
/// converge on the desired server connection state.
pub async fn wait_server_connected(c: &mut TestComponent, want: bool) {
    eventually(
        async || {
            let r = c
                .request("session.status", json!({}))
                .await
                .expect("session.status");
            r["server_connected"] == json!(want)
        },
        "convergence of server_connected",
    )
    .await;
}

/// Waits for the directory as seen by `c` to carry both the attestation AND the
/// relay of `device_id` (so a data-plane stream can resolve and open it).
pub async fn wait_reachable(c: &mut TestComponent, device_id: &str) {
    wait_directory(
        c,
        device_id,
        |d| {
            d.get("attestation").and_then(Value::as_str).is_some()
                && d.get("relay_url").and_then(Value::as_str).is_some()
        },
        "reachable peer (attestation + relay)",
    )
    .await;
}

/// Waits for the directory as seen by `c` to carry the attestation of
/// `device_id`.
pub async fn wait_attested(c: &mut TestComponent, device_id: &str) {
    wait_directory(
        c,
        device_id,
        |d| d.get("attestation").and_then(Value::as_str).is_some(),
        "attestation visible",
    )
    .await;
}

/// Waits for a device in the directory as seen by `c` to satisfy `pred`. Robust
/// to the transient absence of the record and to a directory not yet
/// snapshotted.
pub async fn wait_directory(
    c: &mut TestComponent,
    device_id: &str,
    pred: impl Fn(&Value) -> bool,
    what: &str,
) {
    eventually(
        async || {
            let Ok(list) = c.request("devices.list", json!({})).await else {
                return false;
            };
            list.as_array()
                .into_iter()
                .flatten()
                .any(|d| d.get("device_id").and_then(Value::as_str) == Some(device_id) && pred(d))
        },
        what,
    )
    .await;
}

/// Finds a component's entry by name in a `components.list` result.
pub fn find_component<'a>(list: &'a Value, name: &str) -> &'a Value {
    list.as_array()
        .expect("list of components")
        .iter()
        .find(|c| c["name"] == name)
        .unwrap_or_else(|| panic!("component {name} absent from the list: {list}"))
}
