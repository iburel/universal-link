// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

//! Harness for the supervisor's tests: a REAL in-process Core, REAL child
//! processes (the `ul-fake-component` binary), and an on-disk log that the
//! fixture fills and the test reads back.
//!
//! Contract frozen by this suite (see also the header of `supervisor.rs`):
//!
//! - The Core's path arrives via `UNIVERSALLINK_IPC_PATH`, the spawn token via
//!   the FIRST LINE of standard input. Never via `argv` nor the environment:
//!   the one is readable by all, the other is inherited by all descendants.
//! - Standard input stays open; its EOF is the graceful-shutdown request. It
//!   is the only channel that exists on all three OSes (Windows has no
//!   SIGTERM).
//! - Each launch receives a FRESH token, and the token of a dead child is
//!   reclaimed — a spawn token is used only once, a child restarted with the
//!   old one would be refused.
//! - The child's descendants die with it: process group on unix, Job Object on
//!   Windows.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use universallink_core::{Config, CoreHandle, FileSecretStore, PlainConnector};
use universallink_daemon::supervisor::{ChildSpec, Policy, Supervisor};

/// All of this suite's waits are bounded ACTIVE waits: a test never sleeps
/// "long enough", it loops until the condition holds, or gives up.
pub const DEADLINE: Duration = Duration::from_secs(10);

/// The fixture binary, built by cargo alongside the suite.
pub fn fixture() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_ul-fake-component"))
}

pub struct TestDaemon {
    pub core: Arc<CoreHandle>,
    /// Where the fixture writes its log and its liveness files.
    pub work: tempfile::TempDir,
    _config: tempfile::TempDir,
}

impl TestDaemon {
    pub async fn start() -> TestDaemon {
        let config_dir = tempfile::tempdir().expect("tempdir");
        let work = tempfile::tempdir().expect("tempdir");
        let config = Config {
            ipc_path: ipc_path_for(config_dir.path()),
            config_dir: config_dir.path().to_path_buf(),
            server: None,
            reload_server: Arc::new(|| Ok::<_, String>(None)),
            device_name: "test-daemon".into(),
            secret_store: Arc::new(FileSecretStore::new(config_dir.path())),
            connector: Arc::new(PlainConnector),
            // These tests exercise the supervisor, not the data plane: an
            // isolated in-memory transport is enough (nobody uses it).
            transport: universallink_test_support::memory_transport::MemorySwitchboard::new()
                .endpoint("test-daemon", None),
            receive_dir: config_dir.path().join("received"),
            reconnect_base_delay: Duration::from_millis(50),
        };
        let core = universallink_core::spawn(config)
            .await
            .expect("starting the Core");
        TestDaemon {
            core: Arc::new(core),
            work,
            _config: config_dir,
        }
    }

    pub fn ipc_path(&self) -> PathBuf {
        self.core.ipc_path().to_path_buf()
    }

    /// A supervisor with a single child, tuned for a test: fast restarts,
    /// short shutdown.
    pub fn supervise(&self, spec: ChildSpec) -> Supervisor {
        Supervisor::start(
            self.core.clone(),
            self.ipc_path(),
            vec![spec],
            Policy {
                restart_base_delay: Duration::from_millis(20),
                restart_max_delay: Duration::from_millis(200),
                healthy_after: Duration::from_secs(30),
                grace: Duration::from_millis(500),
            },
        )
    }

    /// The fixture's log, line by line (empty if it does not exist yet).
    pub fn journal(&self) -> Vec<String> {
        std::fs::read_to_string(self.work.path().join("journal"))
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    }

    /// Waits until the log contains at least `count` lines starting with
    /// `prefix`, and returns the log.
    pub async fn wait_lines(&self, prefix: &str, count: usize) -> Vec<String> {
        let deadline = Instant::now() + DEADLINE;
        loop {
            let lines = self.journal();
            if lines.iter().filter(|l| l.starts_with(prefix)).count() >= count {
                return lines;
            }
            assert!(
                Instant::now() < deadline,
                "still fewer than {count} line(s) \"{prefix}\": {lines:?}"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// The heartbeat of process `tag`: `None` if it never beat.
    pub fn beat(&self, tag: &str) -> Option<u64> {
        std::fs::read_to_string(self.work.path().join(format!("alive-{tag}")))
            .ok()?
            .trim()
            .parse()
            .ok()
    }

    pub async fn wait_beating(&self, tag: &str) {
        let deadline = Instant::now() + DEADLINE;
        while self.beat(tag).is_none() {
            assert!(Instant::now() < deadline, "\"{tag}\" never beat");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// True if `tag` is no longer beating. The fixture beats every 20 ms: two
    /// readings 300 ms apart are enough to decide.
    pub async fn assert_dead(&self, tag: &str) {
        let before = self.beat(tag);
        tokio::time::sleep(Duration::from_millis(300)).await;
        let after = self.beat(tag);
        assert_eq!(
            before, after,
            "\"{tag}\" is still beating ({before:?} → {after:?}): a process survived the shutdown"
        );
    }
}

/// The fixture, as the supervisor will launch it. Its settings go through the
/// arguments: the supervisor only sets `UNIVERSALLINK_IPC_PATH`, and two
/// parallel tests cannot have two environments.
pub fn spec(daemon: &TestDaemon, mode: &str, extra: &[(&str, &str)]) -> ChildSpec {
    let role = "tray";
    let scopes = ["session.read", "devices.read"];
    let mut args = vec![
        format!("--dir={}", daemon.work.path().display()),
        format!("--mode={mode}"),
        format!("--role={role}"),
        format!("--scopes={}", scopes.join(",")),
    ];
    for (key, value) in extra {
        args.push(format!("--{key}={value}"));
    }

    ChildSpec {
        program: fixture(),
        args,
        role: role.to_string(),
        scopes: scopes.iter().map(|s| (*s).to_string()).collect(),
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
        r"\\.\pipe\universallink-daemon-test-{}-{}",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    ))
}

/// A raw `hello` on the Core's listening endpoint, to check what a token still
/// opens. Returns the application error code, or `None` if the hello goes
/// through.
///
/// The role and scopes MUST be those of the grant: a spawn token is bound to
/// them, and a role that does not match is refused with `INVALID_TOKEN` — the
/// same code as a revoked token. Probing with another role would make the test
/// always green, revocation or not.
pub async fn hello_with(ipc_path: &Path, token: &str) -> Option<String> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let mut stream = connect(ipc_path).await;
    let body = format!(
        r#"{{"jsonrpc":"2.0","id":1,"method":"hello","params":{{"name":"probe","version":"0","role":"tray","scopes":["session.read"],"token":"{token}"}}}}"#
    );
    let frame = format!("Content-Length: {}\r\n\r\n{body}", body.len());
    stream.write_all(frame.as_bytes()).await.expect("write");

    // LSP framing: read the headers, then exactly `Content-Length` bytes.
    let mut raw = Vec::new();
    let mut byte = [0u8; 1];
    while !raw.ends_with(b"\r\n\r\n") {
        let n = stream.read(&mut byte).await.expect("read");
        assert!(n != 0, "the Core closed without answering the hello");
        raw.push(byte[0]);
    }
    let head = String::from_utf8_lossy(&raw).into_owned();
    let length: usize = head
        .lines()
        .filter_map(|l| l.split_once(':'))
        .find(|(name, _)| name.trim().eq_ignore_ascii_case("content-length"))
        .and_then(|(_, value)| value.trim().parse().ok())
        .expect("Content-Length");
    let mut payload = vec![0u8; length];
    stream.read_exact(&mut payload).await.expect("body");

    let text = String::from_utf8_lossy(&payload).into_owned();
    // Only `data.code` is a string; the JSON-RPC `code` is a number.
    text.split_once(r#""code":""#)
        .and_then(|(_, rest)| rest.split_once('"'))
        .map(|(code, _)| code.to_string())
}

#[cfg(unix)]
async fn connect(path: &Path) -> tokio::net::UnixStream {
    tokio::net::UnixStream::connect(path)
        .await
        .expect("IPC connection")
}

#[cfg(windows)]
async fn connect(path: &Path) -> tokio::net::windows::named_pipe::NamedPipeClient {
    // Every instance of the pipe may be taken the instant after an accept: the
    // Core recreates one right away.
    let deadline = Instant::now() + DEADLINE;
    loop {
        match tokio::net::windows::named_pipe::ClientOptions::new().open(path) {
            Ok(client) => return client,
            Err(e) if Instant::now() < deadline => {
                let _ = e;
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            Err(e) => panic!("IPC connection: {e}"),
        }
    }
}
