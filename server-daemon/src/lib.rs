// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

//! Wiring of the server binary. The control-plane logic (authentication,
//! directory, presence, signaling) lives in the `universallink-server` lib;
//! this crate only deploys it.
//!
//! Reading the configuration is here (and not in `main.rs`) so that it is
//! testable without starting a process.

pub mod config;
pub mod store;
