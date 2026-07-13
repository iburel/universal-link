// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

//! The OS keyring, wired in behind the Core's `SecretStore`.
//!
//! One detail decides the entire shape of this module: the Core calls `set`
//! and `delete` **while holding the session lock** (that is what makes
//! `session.changed` atomic with its transition). But a keyring call blocks:
//! D-Bus on Linux, and on macOS the Keychain can outright open a window and
//! wait for the user. Such a call would freeze all of the Core's IPC commands,
//! without bound.
//!
//! Hence: a dedicated thread serializes the keyring accesses. Writes are
//! queued and return right away; reads wait, but with a cap — beyond it,
//! "secret absent", which the trait contract allows ("Read errors count as
//! absence"). The FIFO order guarantees that a read sees the writes that
//! precede it.
//!
//! `flush()` drains the queue and joins the thread: `main` calls it before
//! exiting, otherwise an in-flight write would be lost. Losing one is not
//! dramatic (a refresh token is re-obtained through re-auth), but losing one
//! *silently* is.

use std::path::Path;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::mpsc::{Receiver, Sender, SyncSender, channel, sync_channel};
use std::time::Duration;

use universallink_core::{FileSecretStore, SecretStore};

/// Name of the service in the OS keyring.
const SERVICE: &str = "universallink-core";
/// An entry that does not exist: reading it tells whether the keyring answers.
const PROBE: &str = "__probe__";
/// Beyond this, the keyring is considered mute for this read.
const READ_TIMEOUT: Duration = Duration::from_secs(3);

/// The keyring as seen from the dedicated thread. A trait so the mechanics can
/// be tested without a keyring — the CI has none, and neither does an SSH
/// session.
pub trait Backend: Send + 'static {
    fn get(&self, name: &str) -> Result<Option<String>, String>;
    fn set(&self, name: &str, value: &str) -> Result<(), String>;
    fn delete(&self, name: &str) -> Result<(), String>;
}

/// The keyring chosen at startup. `File` is the fallback: same permissions as
/// `device.key` (0600), but the secret is in the clear at rest — which the
/// keyring avoids.
pub enum Secrets {
    Keyring(Arc<BackgroundStore>),
    File(Arc<FileSecretStore>),
}

impl Secrets {
    pub fn store(&self) -> Arc<dyn SecretStore> {
        match self {
            Secrets::Keyring(store) => store.clone(),
            Secrets::File(store) => store.clone(),
        }
    }

    /// To be called before exiting: the queued writes actually go out.
    pub fn flush(&self) {
        if let Secrets::Keyring(store) = self {
            store.flush();
        }
    }

    pub fn description(&self) -> &'static str {
        match self {
            Secrets::Keyring(_) => "OS keyring",
            Secrets::File(_) => "secrets.json (fallback: no keyring reachable)",
        }
    }
}

/// Chooses the OS keyring, or the file fallback if it does not answer.
pub fn build(config_dir: &Path) -> Secrets {
    let store = BackgroundStore::new(OsKeyring);
    if store.available() {
        Secrets::Keyring(Arc::new(store))
    } else {
        store.flush();
        Secrets::File(Arc::new(FileSecretStore::new(config_dir)))
    }
}

enum Op {
    Probe(SyncSender<bool>),
    Get {
        name: String,
        reply: SyncSender<Option<String>>,
    },
    Set {
        name: String,
        value: String,
    },
    Delete {
        name: String,
    },
    Stop,
}

pub struct BackgroundStore {
    tx: Sender<Op>,
    /// The thread keeps the paired sender; it drops when `serve` returns.
    /// `flush` waits for that hang-up — bounded, so that a backend stuck on a
    /// prompt does not pin the daemon's shutdown. Under a `Mutex` because
    /// `Receiver` is not `Sync`, and the store must be (`Arc<dyn SecretStore>`).
    done: Mutex<Receiver<()>>,
    read_timeout: Duration,
}

impl std::fmt::Debug for BackgroundStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("BackgroundStore(OS keyring, dedicated thread)")
    }
}

impl BackgroundStore {
    pub fn new(backend: impl Backend) -> BackgroundStore {
        BackgroundStore::with_timeout(backend, READ_TIMEOUT)
    }

    pub fn with_timeout(backend: impl Backend, read_timeout: Duration) -> BackgroundStore {
        let (tx, rx) = channel();
        let (done_tx, done) = sync_channel::<()>(0);
        std::thread::Builder::new()
            .name("universallink-keyring".into())
            .spawn(move || {
                // Parked here: its drop, on the return of `serve`, signals `flush`.
                let _done = done_tx;
                serve(backend, rx);
            })
            .expect("keyring thread");
        BackgroundStore {
            tx,
            done: Mutex::new(done),
            read_timeout,
        }
    }

    /// Does the keyring answer? Only once, at startup: if the secrets agent
    /// comes up AFTER the daemon, we will stay on the file fallback until the
    /// next launch. Accepted.
    pub fn available(&self) -> bool {
        let (reply, answer) = sync_channel(1);
        if self.tx.send(Op::Probe(reply)).is_err() {
            return false;
        }
        answer.recv_timeout(self.read_timeout).unwrap_or(false)
    }

    pub fn flush(&self) {
        let _ = self.tx.send(Op::Stop);
        // `serve` processes `Op::Stop` in FIFO order then returns, dropping its
        // sender: `recv` then returns `Err(Disconnected)`. If the thread is
        // stuck in a blocking keyring call (macOS prompt…), we wait at most
        // `read_timeout` then detach it — the OS reclaims it. The daemon's
        // shutdown never depends on the keyring's goodwill.
        let _ = self
            .done
            .lock()
            .expect("lock done")
            .recv_timeout(self.read_timeout);
    }
}

impl SecretStore for BackgroundStore {
    fn get(&self, name: &str) -> Option<String> {
        let (reply, answer) = sync_channel(1);
        self.tx
            .send(Op::Get {
                name: name.to_string(),
                reply,
            })
            .ok()?;
        // A mute keyring counts as secret absent: the caller will redo the
        // flow that produced it. Staying suspended here would freeze the Core.
        match answer.recv_timeout(self.read_timeout) {
            Ok(value) => value,
            Err(_) => {
                tracing::warn!(secret = name, "mute keyring: secret assumed absent");
                None
            }
        }
    }

    fn set(&self, name: &str, value: &str) -> std::io::Result<()> {
        // Queue it: never block, we may be holding a Core lock. The actual
        // failure is logged by the thread.
        self.tx
            .send(Op::Set {
                name: name.to_string(),
                value: value.to_string(),
            })
            .map_err(|_| std::io::Error::other("keyring stopped"))
    }

    fn delete(&self, name: &str) {
        let _ = self.tx.send(Op::Delete {
            name: name.to_string(),
        });
    }
}

fn serve(backend: impl Backend, rx: Receiver<Op>) {
    while let Ok(op) = rx.recv() {
        match op {
            Op::Probe(reply) => {
                let _ = reply.send(backend.get(PROBE).is_ok());
            }
            Op::Get { name, reply } => {
                let value = backend.get(&name).unwrap_or_else(|e| {
                    tracing::warn!(secret = %name, error = %e, "could not read from the keyring");
                    None
                });
                let _ = reply.send(value);
            }
            Op::Set { name, value } => {
                if let Err(e) = backend.set(&name, &value) {
                    tracing::error!(secret = %name, error = %e, "could not write to the keyring");
                }
            }
            Op::Delete { name } => {
                if let Err(e) = backend.delete(&name) {
                    tracing::error!(secret = %name, error = %e, "could not delete from the keyring");
                }
            }
            Op::Stop => return,
        }
    }
}

/// The real keyring: Secret Service (Linux, via zbus, without libdbus),
/// Keychain (macOS), Credential Manager (Windows).
struct OsKeyring;

impl Backend for OsKeyring {
    fn get(&self, name: &str) -> Result<Option<String>, String> {
        let entry = keyring::Entry::new(SERVICE, name).map_err(|e| e.to_string())?;
        match entry.get_password() {
            Ok(value) => Ok(Some(value)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(e.to_string()),
        }
    }

    fn set(&self, name: &str, value: &str) -> Result<(), String> {
        keyring::Entry::new(SERVICE, name)
            .and_then(|entry| entry.set_password(value))
            .map_err(|e| e.to_string())
    }

    fn delete(&self, name: &str) -> Result<(), String> {
        let entry = keyring::Entry::new(SERVICE, name).map_err(|e| e.to_string())?;
        match entry.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(e) => Err(e.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    #[derive(Clone, Default)]
    struct FakeKeyring {
        entries: Arc<Mutex<BTreeMap<String, String>>>,
        /// Each operation sleeps this long: enough to simulate a slow keyring,
        /// or a macOS prompt waiting for the user.
        delay: Duration,
        /// The keyring is down: everything fails.
        broken: bool,
        writes: Arc<AtomicUsize>,
    }

    impl Backend for FakeKeyring {
        fn get(&self, name: &str) -> Result<Option<String>, String> {
            std::thread::sleep(self.delay);
            if self.broken {
                return Err("keyring unreachable".into());
            }
            Ok(self.entries.lock().expect("lock").get(name).cloned())
        }

        fn set(&self, name: &str, value: &str) -> Result<(), String> {
            std::thread::sleep(self.delay);
            if self.broken {
                return Err("keyring unreachable".into());
            }
            self.entries
                .lock()
                .expect("lock")
                .insert(name.to_string(), value.to_string());
            self.writes.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }

        fn delete(&self, name: &str) -> Result<(), String> {
            std::thread::sleep(self.delay);
            if self.broken {
                return Err("keyring unreachable".into());
            }
            self.entries.lock().expect("lock").remove(name);
            Ok(())
        }
    }

    #[test]
    fn secrets_survive_a_round_trip() {
        let store = BackgroundStore::new(FakeKeyring::default());
        assert!(store.available());
        assert_eq!(store.get("token"), None);
        store.set("token", "abc").expect("write");
        // FIFO: the read is served after the write that precedes it.
        assert_eq!(store.get("token").as_deref(), Some("abc"));
        store.delete("token");
        assert_eq!(store.get("token"), None);
        store.flush();
    }

    #[test]
    fn a_write_never_waits_for_the_keyring() {
        // This is the invariant that protects the Core's session lock: `set`
        // and `delete` return without touching the keyring.
        let fake = FakeKeyring {
            delay: Duration::from_millis(400),
            ..Default::default()
        };
        let store = BackgroundStore::new(fake.clone());

        let start = std::time::Instant::now();
        store.set("token", "abc").expect("write");
        store.delete("other");
        assert!(
            start.elapsed() < Duration::from_millis(200),
            "a write waited for the keyring ({:?})",
            start.elapsed()
        );

        // And `flush` actually sends them out before returning.
        store.flush();
        assert_eq!(
            fake.entries
                .lock()
                .expect("lock")
                .get("token")
                .map(String::as_str),
            Some("abc"),
            "flush() must drain the queue before exiting"
        );
    }

    #[test]
    fn a_mute_keyring_reads_as_absent_rather_than_hanging() {
        let fake = FakeKeyring {
            delay: Duration::from_secs(30),
            ..Default::default()
        };
        let store = BackgroundStore::with_timeout(fake, Duration::from_millis(50));
        let start = std::time::Instant::now();
        assert_eq!(store.get("token"), None);
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "a read waited without bound"
        );
    }

    #[test]
    fn flush_does_not_hang_on_a_stuck_backend() {
        // The invariant the module promises: the daemon's shutdown does not
        // depend on the keyring. A backend that blocks 30 s on a write must
        // not pin `flush`.
        let fake = FakeKeyring {
            delay: Duration::from_secs(30),
            ..Default::default()
        };
        let store = BackgroundStore::with_timeout(fake, Duration::from_millis(50));
        store.set("token", "abc").expect("queueing");
        let start = std::time::Instant::now();
        store.flush();
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "flush waited for a stuck backend ({:?})",
            start.elapsed()
        );
    }

    #[test]
    fn a_broken_keyring_is_not_available() {
        let store = BackgroundStore::new(FakeKeyring {
            broken: true,
            ..Default::default()
        });
        assert!(
            !store.available(),
            "a down keyring must fall back to the file"
        );
        store.flush();
    }

    #[test]
    fn a_write_after_flush_is_reported_not_swallowed() {
        let store = BackgroundStore::new(FakeKeyring::default());
        store.flush();
        assert!(
            store.set("token", "abc").is_err(),
            "a write that will never go out must say so"
        );
    }

    #[test]
    fn build_chooses_a_backend_without_touching_it() {
        // We do NOT test a round trip here: `build` may fall back to the
        // machine's real keyring (a macOS runner has one), and a test suite
        // has nothing to write to it. What we check is that it decides without
        // panicking and that it can say what it chose.
        let dir = tempfile::tempdir().expect("tempdir");
        let secrets = build(dir.path());
        assert!(!secrets.description().is_empty());
        secrets.flush();
    }
}
