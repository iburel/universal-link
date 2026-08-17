// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

//! The `1device-input` binary: wiring for the official keyboard and mouse
//! engine.
//!
//! Supervised-component contract (see `daemon/src/supervisor.rs`): find the Core
//! at `ONEDEVICE_IPC_PATH`, read the single-use spawn token from the first line of
//! standard input, keep that standard input open (its EOF means "stop"), and exit
//! if the IPC connection drops (the token is single-use, so a reconnection would
//! fail; exiting lets the supervisor restart us fresh). That whole contract lives
//! in the lib (`orchestrator::supervised`), where the integration suite can drive
//! it; this file is the two threads and nothing else.
//!
//! The shape is the clipboard component's, for the clipboard component's reason:
//! the OS event loop is pinned to the main thread (a message pump on Windows, a
//! run loop on macOS, a non-`Send` X connection on Linux) while the async engine
//! runs on a side thread with its own runtime, and the two are bridged by the
//! `Clone` backend handle for downcalls and a channel of events for upcalls.
//!
//! Unlike the clipboard's, this binary does NOT exit when there is no platform
//! backend. `os::create` always succeeds and hands back a backend that reports
//! what it cannot do, because a machine with no OS half still has to answer
//! `input.status` so the interface can say "nothing here can type", still has to
//! put its screens on the shared plane, and still has to hold the permissions a
//! person granted. The reasoning is in `os.rs`'s header, including the practical
//! half: the supervisor has no notion of "unsupported" and would relaunch an
//! exiting component every minute for ever.

// The trait, for `request_exit` on the handle: the one downcall this file makes.
use onedevice_input::backend::InputBackend;
use onedevice_input::os;

fn main() {
    let created = os::create();
    let handle = created.handle;
    let backend_events = created.backend_events;
    let event_loop = created.event_loop;

    // The async brain on its own thread with its own runtime. It owns a clone of
    // the handle (the backend the engine drives) and, when it returns, asks the
    // main-thread loop to exit with the mapped code.
    let brain_backend = handle.clone();
    std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("tokio runtime");
        let code = runtime.block_on(onedevice_input::supervised(brain_backend, backend_events));
        // NOT a plain drop of the runtime: it waits for every blocking task, and
        // the engine leaves one behind for ever, the read on standard input that
        // is its stop signal, which only ends when the supervisor closes it.
        runtime.shutdown_background();
        handle.request_exit(code);
    });

    // The OS event loop pumps on THIS (main) thread until the brain asks it to
    // stop; its return value is the process exit code.
    let code = event_loop.run();
    std::process::exit(code);
}
