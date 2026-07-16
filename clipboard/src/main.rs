// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

//! The `universallink-clipboard` binary.
//!
//! Brick 1 ships the OS-agnostic orchestrator (`universallink_clipboard::run`,
//! covered by the integration suite against a real Core) and the backend seam,
//! but no platform backend yet. Wiring the supervised-component contract (the
//! spawn token on the first line of standard input, the `UNIVERSALLINK_IPC_PATH`
//! endpoint, the main-thread OS event loop bridged to a tokio worker running the
//! orchestrator — à la `universallink-tray`) arrives with the first backend, so
//! this binary reports there is nothing to run and exits cleanly. The supervisor
//! does not register the component until then.

use std::process::ExitCode;

use universallink_clipboard::os;

fn main() -> ExitCode {
    match os::create() {
        // `Ok` is uninhabited (`Infallible`) in brick 1: no backend exists on
        // any platform yet.
        Err(os::Unsupported) => {
            eprintln!(
                "universallink-clipboard: no clipboard backend for this platform yet \
                 (backends land in later bricks); nothing to run."
            );
            ExitCode::SUCCESS
        }
    }
}
