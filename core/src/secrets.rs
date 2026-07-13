// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

//! Keyring abstraction: where the Core stows its durable secrets (the OIDC
//! refresh token). The deployment binary will wire in the OS keyring (abstract
//! plumbing: it is the Core's job, not the components'); in the meantime,
//! `FileSecretStore` — a 0600 file in the config folder — offers the same trust
//! perimeter as `device.key`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// Name of the secret carrying the OIDC refresh token: obtain fresh ID tokens
/// (sensitive operations) without reopening a browser.
pub(crate) const REFRESH_TOKEN: &str = "oidc-refresh-token";

/// A keyring of named secrets. Read errors count as absence: a lost secret is
/// recovered by re-running the flow that produced it.
pub trait SecretStore: Send + Sync + std::fmt::Debug {
    fn get(&self, name: &str) -> Option<String>;
    fn set(&self, name: &str, value: &str) -> std::io::Result<()>;
    fn delete(&self, name: &str);
}

/// Secrets in `secrets.json` (0600, config folder) — the file fallback until
/// the OS keyring.
#[derive(Debug)]
pub struct FileSecretStore {
    path: PathBuf,
    /// Serializes the read-modify-write cycles on the file.
    lock: Mutex<()>,
}

impl FileSecretStore {
    pub fn new(config_dir: &Path) -> FileSecretStore {
        FileSecretStore {
            path: config_dir.join("secrets.json"),
            lock: Mutex::new(()),
        }
    }

    fn read_all(&self) -> BTreeMap<String, String> {
        let Ok(text) = std::fs::read_to_string(&self.path) else {
            return BTreeMap::new();
        };
        serde_json::from_str(&text).unwrap_or_default()
    }

    fn write_all(&self, secrets: &BTreeMap<String, String>) -> std::io::Result<()> {
        let text = serde_json::to_string(secrets).expect("secrets as JSON");
        crate::write_private_file(&self.path, &text)
    }
}

impl SecretStore for FileSecretStore {
    fn get(&self, name: &str) -> Option<String> {
        let _guard = self.lock.lock().expect("lock secrets");
        self.read_all().remove(name)
    }

    fn set(&self, name: &str, value: &str) -> std::io::Result<()> {
        let _guard = self.lock.lock().expect("lock secrets");
        let mut secrets = self.read_all();
        secrets.insert(name.to_string(), value.to_string());
        self.write_all(&secrets)
    }

    fn delete(&self, name: &str) {
        let _guard = self.lock.lock().expect("lock secrets");
        let mut secrets = self.read_all();
        if secrets.remove(name).is_some()
            && let Err(e) = self.write_all(&secrets)
        {
            tracing::error!(secret = name, error = %e, "failed to erase the secret");
        }
    }
}
