// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

//! The Core process: what the `universallink-core` lib cannot carry.
//!
//! The lib is cross-checked from Linux to windows-msvc and aarch64-darwin,
//! which forbids it any dependency whose `build.rs` compiles C — so rustls, so
//! keyring. This crate, by contrast, is compiled only natively, by the three
//! CI jobs. It wires in:
//!
//! - the **TLS** (`tls`), through the `Connector` that the Core's config
//!   receives;
//! - the **OS keyring** (`secrets`), through its `SecretStore`;
//! - the **logging** (`logging`): a daemon launched at login has no terminal;
//! - the **configuration** (`config`): where the server is, which the IdP is;
//! - the **supervisor** (`supervisor`): launch, restart, and take down the
//!   official components.
//!
//! The daemon lib exists so that its test suite can plug into it; the binary
//! (`main.rs`) is nothing but wiring.

mod child;
pub mod config;
pub mod dataplane;
pub mod logging;
pub mod secrets;
pub mod supervisor;
pub mod tls;
