// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

//! Directory persistence: the boundary between DURABLE state (device identity,
//! account membership, C7 attestation, revocations) and EPHEMERAL state
//! (connection, relay_url, presence) that lasts only for a session and is
//! rebuilt on reconnect.
//!
//! The lib defines only the CONTRACT (`DirectoryStore`) and a memory store, for
//! tests and the ephemeral mode. The disk backend lives in the deployment
//! binary (`server-daemon`): the lib — a dev-dependency of several crates —
//! thereby stays free of any storage dependency.

use std::sync::Mutex;

use serde::{Deserialize, Serialize};

/// Persistence contract. `load` once at startup; `save` after each durable
/// mutation (the dataset is small: we rewrite the complete snapshot).
/// Synchronous: writes are rare and tiny, a backend that blocks briefly is
/// acceptable for a control plane.
pub trait DirectoryStore: Send + Sync {
    fn load(&self) -> anyhow::Result<DurableState>;
    fn save(&self, state: &DurableState) -> anyhow::Result<()>;
}

/// The directory's durable state, as it crosses a restart.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct DurableState {
    pub devices: Vec<DurableDevice>,
    /// Struck-off ids: a revoked device must stay revoked after a restart.
    pub revoked: Vec<String>,
}

/// A device reduced to what SURVIVES a disconnection and a restart: its
/// identity, its account, and the C7 attestation (bound to the node_id, stable).
/// Neither `relay_url`, nor `status`, nor `last_seen`, nor the connection — all
/// of that is session-specific and republishes itself on reconnect.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DurableDevice {
    pub account: String,
    pub device_id: String,
    pub name: String,
    pub platform: String,
    pub node_id: String,
    pub attestation: Option<String>,
}

/// Memory store: retains the last saved snapshot. This is the default EPHEMERAL
/// mode (`spawn`) — the state is lost when the process stops — and the test
/// store, where a single `MemoryStore` shared between two `spawn_with_store`
/// calls simulates a restart without touching the disk.
#[derive(Default)]
pub struct MemoryStore(Mutex<DurableState>);

impl DirectoryStore for MemoryStore {
    fn load(&self) -> anyhow::Result<DurableState> {
        Ok(self.0.lock().expect("lock MemoryStore").clone())
    }

    fn save(&self, state: &DurableState) -> anyhow::Result<()> {
        *self.0.lock().expect("lock MemoryStore") = state.clone();
        Ok(())
    }
}
