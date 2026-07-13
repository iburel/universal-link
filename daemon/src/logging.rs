// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

//! The daemon's log. A Core launched at login has no terminal to shout at: the
//! file is authoritative, the error output is only added if someone is
//! watching (`stderr` attached to a terminal).
//!
//! The returned `WorkerGuard` must live until the end of `main`: it is its
//! `drop` that flushes the non-blocking writer's buffer. A shutdown that does
//! not go back through the return of `main` (SIGKILL) loses the last lines —
//! that is the price of non-blocking, and it is why graceful shutdown always
//! comes back into `main`.

use std::io::IsTerminal;

use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::filter::{EnvFilter, LevelFilter};
use tracing_subscriber::prelude::*;

/// Default level, and its override. `RUST_LOG` is too widely shared: a
/// developer who exports it for another tool must not make our daemon chatty.
const LOG_ENV: &str = "UNIVERSALLINK_LOG";

/// Installs the collector. Keep the guard, do not throw it away.
#[must_use]
pub fn init() -> Option<WorkerGuard> {
    let filter = EnvFilter::builder()
        .with_default_directive(LevelFilter::INFO.into())
        .with_env_var(LOG_ENV)
        .from_env_lossy();
    let Some((writer, guard)) = file_writer() else {
        // No usable log directory: we do not give up on logging, we make do
        // with what we have.
        tracing_subscriber::registry()
            .with(filter)
            .with(stderr_layer())
            .init();
        return None;
    };
    tracing_subscriber::registry()
        .with(filter)
        .with(
            tracing_subscriber::fmt::layer()
                .with_ansi(false)
                .with_writer(writer),
        )
        .with(stderr_layer())
        .init();
    Some(guard)
}

/// The error output, only if someone is reading it. Generic over the layer
/// stack: its type depends on what it stacks onto, so it cannot be built once
/// for both branches of `init`.
fn stderr_layer<S>() -> Option<impl tracing_subscriber::Layer<S>>
where
    S: tracing::Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>,
{
    std::io::stderr().is_terminal().then(|| {
        tracing_subscriber::fmt::layer()
            .with_writer(std::io::stderr)
            .with_target(false)
    })
}

fn file_writer() -> Option<(tracing_appender::non_blocking::NonBlocking, WorkerGuard)> {
    let dir = universallink_paths::log_dir()?;
    std::fs::create_dir_all(&dir).ok()?;
    // Daily rotation, seven files kept: enough to understand yesterday's
    // incident, not enough to fill a disk.
    let appender = tracing_appender::rolling::Builder::new()
        .rotation(tracing_appender::rolling::Rotation::DAILY)
        .filename_prefix("universallink")
        .filename_suffix("log")
        .max_log_files(7)
        .build(&dir)
        .ok()?;
    Some(tracing_appender::non_blocking(appender))
}
