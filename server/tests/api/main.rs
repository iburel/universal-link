// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

//! Integration suite for the server's public API (doc/server-api.md).
//! TDD: written before the implementation — everything is red as long as
//! `spawn` is `todo!()`.

mod support;

mod auth_authenticate;
mod auth_enroll;
mod devices;
mod persistence;
mod presence;
mod protocol;
