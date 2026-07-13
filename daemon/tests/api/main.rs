// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

//! Integration suite for the Core daemon. Here the supervisor launches REAL
//! processes, which talk to a REAL Core: it is the only way to prove that the
//! trust bootstrap holds end to end.

mod support;

mod dataplane;
mod supervisor;
