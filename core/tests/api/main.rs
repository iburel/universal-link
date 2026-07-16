// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

//! Integration suite for the Core's public IPC API (doc/core-api.md).
//! TDD: each building block writes its tests before its implementation.

mod support;

mod account;
mod clipboard;
mod components;
mod dataplane;
mod devices;
mod enrollment;
mod events;
mod hello;
mod login;
mod protocol;
mod session;
mod startup;
mod system;
