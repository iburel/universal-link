// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

//! Integration suite for the Core's public IPC API (doc/core-api.md).
//! TDD: each building block writes its tests before its implementation.

mod support;

mod account;
mod clipboard;
mod clipboard_net;
mod components;
mod continuum;
mod dataplane;
mod devices;
mod dirsync;
mod enrollment;
mod events;
mod hello;
mod lanpair;
mod leave;
mod login;
mod pairing;
mod protocol;
mod revocation;
mod serverless;
mod session;
mod startup;
mod system;
