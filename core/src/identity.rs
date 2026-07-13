// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

//! Device identity: an Ed25519 key pair — the same as the iroh identity
//! (`node_id` = public key) and as the server authentication key
//! (doc/server-api.md, "Identities"). Generated at first startup, before any
//! login: it precedes the session.

use std::path::Path;

use ed25519_dalek::{Signer, SigningKey};

/// Filename in the config folder: seed in hex (64 chars), 0600.
const KEY_FILE: &str = "device.key";

#[derive(Clone)]
pub struct DeviceIdentity {
    key: SigningKey,
}

/// Re-reads the device's Ed25519 seed (`device.key`, hex 0600) or generates it
/// at first startup. A corrupt file is an error: regenerating it silently would
/// destroy the device's identity (and its enrollment).
///
/// Exposed for the binary: the daemon's iroh connector must seed its endpoint
/// with THIS key, otherwise its iroh `node_id` would not be the one the Core
/// publishes in the directory, and peers would not reach it.
pub fn load_or_generate_device_seed(config_dir: &Path) -> anyhow::Result<[u8; 32]> {
    let path = config_dir.join(KEY_FILE);
    if path.exists() {
        let text = std::fs::read_to_string(&path)?;
        let bytes: [u8; 32] = hex::decode(text.trim())
            .ok()
            .and_then(|b| b.try_into().ok())
            .ok_or_else(|| anyhow::anyhow!("corrupt device.key: {}", path.display()))?;
        return Ok(bytes);
    }
    let key = SigningKey::generate(&mut rand::rng());
    let seed = key.to_bytes();
    crate::write_private_file(&path, &hex::encode(seed))?;
    Ok(seed)
}

impl DeviceIdentity {
    /// Re-reads the key from disk, or generates one at first startup.
    pub fn load_or_generate(config_dir: &Path) -> anyhow::Result<DeviceIdentity> {
        let seed = load_or_generate_device_seed(config_dir)?;
        Ok(DeviceIdentity {
            key: SigningKey::from_bytes(&seed),
        })
    }

    /// Public key in hex (64 chars) — the `node_id` of the directory and of iroh.
    pub fn node_id(&self) -> String {
        hex::encode(self.key.verifying_key().to_bytes())
    }

    /// Proof of possession: signature of the nonce (UTF-8 bytes), in hex.
    pub fn proof(&self, nonce: &str) -> String {
        hex::encode(self.key.sign(nonce.as_bytes()).to_bytes())
    }
}
