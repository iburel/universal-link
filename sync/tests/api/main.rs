// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

//! Integration suite: the engine lib against a real Core, over real IPC,
//! through the routed `sync.*` facade.

mod facade;
mod resolve;
mod support;
