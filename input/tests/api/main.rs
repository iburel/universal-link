// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

//! Integration suite for the keyboard and mouse engine (doc/input-sharing.md).
//!
//! Two real Cores of one account in one process, the real engine on each, and a
//! fake platform backend standing in for the OS half. What that buys is the only
//! thing worth having here: the sessions, the layout rounds, the exclusion and
//! every one of the live channel's ten deaths are proven against the real
//! primitives, in the order and with the timing the real ones have.

mod support;

mod facade;
mod layout;
mod session;
mod teardown;
