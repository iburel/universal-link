// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

//! The official sync engine: the exclusive `sync-backend` component behind
//! the Core's routed `sync.*` facade. Its design is doc/sync-engine.md - the
//! letter this crate follows, rule by rule - on top of the generic Core
//! primitives (published transactions, `peers.send`, the facade).
//!
//! Supervised-component contract: see `daemon/src/supervisor.rs` and
//! `src/main.rs` (which is wiring only). The lib holds everything testable:
//! the orchestrator (the event loop behind the facade), the engine identity
//! (the sync keypair that signs membership records) and the persistent store.

pub mod canonical;
pub mod clock;
pub mod engine;
pub mod identity;
pub mod index;
pub mod membership;
pub mod orchestrator;
pub mod protocol;
pub mod records;
pub mod scan;
pub mod store;
pub mod vv;
pub mod wirepath;

pub use orchestrator::{Outcome, SERVED_METHODS, run};
pub use store::Store;
