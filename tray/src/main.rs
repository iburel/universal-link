// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

//! The `universallink-tray` binary: the platform tray (a `tao` event loop on
//! the main thread — macOS requires it, Linux needs gtk which tao initializes)
//! plus the async IPC brain (a tokio runtime on a side thread). The two are
//! bridged by an `EventLoopProxy` (status updates, exit) and a command channel
//! (menu clicks → brain). All the testable logic lives in the lib.

use std::path::PathBuf;
use std::time::Duration;

use tao::event::{Event, StartCause};
use tao::event_loop::{ControlFlow, EventLoopBuilder, EventLoopProxy};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio::sync::mpsc;
use tray_icon::menu::{Menu, MenuEvent, MenuItem};
use tray_icon::{Icon, TrayIconBuilder};
use universallink_ipc_client::{ClientConfig, TokenSource};
use universallink_tray::{Outcome, TrayStatus, UiCommand, run};

/// Set by the supervisor: the Core's listening endpoint.
const IPC_PATH_ENV: &str = "UNIVERSALLINK_IPC_PATH";
/// Barely matters: a spawn token is single-use, so we exit on the first loss
/// rather than let the client retry.
const RECONNECT_BASE_DELAY: Duration = Duration::from_millis(500);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// Delivered to the tao loop from the other threads.
enum UserEvent {
    /// A menu item was chosen.
    Menu(MenuEvent),
    /// The brain reports a status to reflect on the icon.
    Status(TrayStatus),
    /// The brain (or a startup failure) asks the process to exit with a code.
    Exit(i32),
}

fn main() {
    let event_loop = EventLoopBuilder::<UserEvent>::with_user_event().build();

    // Menu built now that tao has initialized gtk; ids captured to match clicks.
    let open_item = MenuItem::new("Open UniversalLink", true, None);
    let quit_item = MenuItem::new("Quit", true, None);
    let open_id = open_item.id().clone();
    let quit_id = quit_item.id().clone();
    let menu = Menu::new();
    menu.append(&open_item).expect("append Open");
    menu.append(&quit_item).expect("append Quit");

    // Menu clicks → the loop, through a forwarding thread: it wakes the loop
    // (send_event) and keeps us clear of the event-handler's Send+Sync bound.
    let menu_proxy = event_loop.create_proxy();
    std::thread::spawn(move || {
        let receiver = MenuEvent::receiver();
        while let Ok(event) = receiver.recv() {
            if menu_proxy.send_event(UserEvent::Menu(event)).is_err() {
                break;
            }
        }
    });

    // The async brain on its own thread with its own tokio runtime.
    let (cmd_tx, cmd_rx) = mpsc::channel::<UiCommand>(8);
    let brain_proxy = event_loop.create_proxy();
    std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("tokio runtime");
        let code = runtime.block_on(brain(cmd_rx, brain_proxy.clone()));
        let _ = brain_proxy.send_event(UserEvent::Exit(code));
    });

    // Created on the first iteration: gtk (Linux) and the NSApplication (macOS)
    // are only ready then. Consumed once via `take`.
    let mut deferred = Some((menu, load_icon()));
    let mut tray = None;

    event_loop.run(move |event, _, control_flow| {
        *control_flow = ControlFlow::Wait;
        match event {
            Event::NewEvents(StartCause::Init) => {
                if let Some((menu, icon)) = deferred.take() {
                    tray = Some(
                        TrayIconBuilder::new()
                            .with_menu(Box::new(menu))
                            .with_icon(icon)
                            .with_tooltip(TrayStatus::Connecting.tooltip())
                            .build()
                            .expect("build tray icon"),
                    );
                }
            }
            Event::UserEvent(UserEvent::Status(status)) => {
                if let Some(tray) = &tray {
                    let _ = tray.set_tooltip(Some(status.tooltip()));
                }
            }
            Event::UserEvent(UserEvent::Menu(menu_event)) => {
                let command = if menu_event.id == open_id {
                    Some(UiCommand::Open)
                } else if menu_event.id == quit_id {
                    Some(UiCommand::Quit)
                } else {
                    None
                };
                if let Some(command) = command {
                    // Never blocks: the brain drains promptly, and a full queue
                    // only means a stop is already under way.
                    let _ = cmd_tx.try_send(command);
                }
            }
            Event::UserEvent(UserEvent::Exit(code)) => std::process::exit(code),
            _ => {}
        }
    });
}

/// Reads the token and environment, connects, and runs the lib's brain.
/// Returns the process exit code.
async fn brain(cmd_rx: mpsc::Receiver<UiCommand>, proxy: EventLoopProxy<UserEvent>) -> i32 {
    let Ok(ipc_path) = std::env::var(IPC_PATH_ENV) else {
        eprintln!("{IPC_PATH_ENV} is not set: the tray is launched by the Core");
        return 1;
    };

    // Contract: the spawn token is the FIRST LINE of standard input — never
    // argv (world-readable) nor the environment (inherited by descendants).
    // Standard input then stays open; its EOF is the graceful-stop signal.
    let mut stdin = BufReader::new(tokio::io::stdin());
    let mut token = String::new();
    match stdin.read_line(&mut token).await {
        Ok(_) if !token.trim().is_empty() => {}
        _ => {
            eprintln!("no spawn token on standard input");
            return 1;
        }
    }
    let token = token.trim().to_string();

    let (client, events) = universallink_ipc_client::spawn(ClientConfig {
        ipc_path: PathBuf::from(ipc_path),
        token: TokenSource::Spawn(token),
        name: "universallink-tray".into(),
        version: env!("CARGO_PKG_VERSION").into(),
        role: "tray".into(),
        // session.read now (status icon); the grant also carries system.shutdown
        // for the Quit, requested when the Core call is wired in.
        scopes: vec!["session.read".into()],
        topics: vec!["session".into()],
        reconnect_base_delay: RECONNECT_BASE_DELAY,
        request_timeout: REQUEST_TIMEOUT,
    });

    // The rest of standard input: reading it to EOF is the stop signal.
    let stdin_closed = async move {
        let mut sink = Vec::new();
        let _ = stdin.read_to_end(&mut sink).await;
    };

    let outcome = run(client, events, stdin_closed, cmd_rx, move |status| {
        let _ = proxy.send_event(UserEvent::Status(status));
    })
    .await;

    match outcome {
        Outcome::StdinClosed => 0,
        // IPC lost / incompatible / client ended: exit non-zero so the
        // supervisor restarts us with a fresh, single-use spawn token.
        Outcome::ConnectionLost | Outcome::Incompatible | Outcome::ClientEnded => 1,
    }
}

/// The embedded tray icon (8-bit RGBA PNG) decoded to raw RGBA.
fn load_icon() -> Icon {
    const PNG: &[u8] = include_bytes!("../icons/tray-32.png");
    let mut reader = png::Decoder::new(PNG)
        .read_info()
        .expect("tray icon header");
    let mut buf = vec![0; reader.output_buffer_size()];
    let info = reader.next_frame(&mut buf).expect("tray icon pixels");
    buf.truncate(info.buffer_size());
    Icon::from_rgba(buf, info.width, info.height).expect("tray icon rgba")
}
