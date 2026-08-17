// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

//! The `1device-tray` binary: the platform tray (a `tao` event loop on
//! the main thread — macOS requires it, Linux needs gtk which tao initializes)
//! plus the async IPC brain (a tokio runtime on a side thread). The two are
//! bridged by an `EventLoopProxy` (status updates, exit) and a command channel
//! (menu clicks → brain). All the testable logic lives in the lib.

use std::path::PathBuf;
use std::time::Duration;

use onedevice_ipc_client::{ClientConfig, TokenSource};
use onedevice_tray::{Outcome, TrayStatus, UiCommand, run};
use tao::event::{Event, StartCause};
use tao::event_loop::{ControlFlow, EventLoopBuilder, EventLoopProxy};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio::sync::mpsc;
use tray_icon::menu::{Menu, MenuEvent, MenuItem};
use tray_icon::{Icon, TrayIconBuilder};

/// Set by the supervisor: the Core's listening endpoint.
const IPC_PATH_ENV: &str = "ONEDEVICE_IPC_PATH";
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
    #[cfg_attr(not(target_os = "macos"), allow(unused_mut))]
    let mut event_loop = EventLoopBuilder::<UserEvent>::with_user_event().build();

    // macOS: an agent, not an application. Our helper bundle says so with
    // `LSUIElement` (see `tray/macos/Info.plist`), but tao sets
    // `NSApplicationActivationPolicyRegular` on the application object itself,
    // and that wins over the plist — measured: the tray checked in as
    // `type="Foreground"`. Said here too, it stops being an icon in the Dock and
    // an entry in the application switcher.
    //
    // And it must not activate itself when the Core starts it at login: the
    // default is to come to the front regardless of what the user is doing.
    #[cfg(target_os = "macos")]
    {
        use tao::platform::macos::{ActivationPolicy, EventLoopExtMacOS};

        event_loop.set_activation_policy(ActivationPolicy::Accessory);
        event_loop.set_activate_ignoring_other_apps(false);
    }

    // Menu built now that tao has initialized gtk; ids captured to match clicks.
    let open_item = MenuItem::new("Open 1Device", true, None);
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

    let (client, events) = onedevice_ipc_client::spawn(ClientConfig {
        ipc_path: PathBuf::from(ipc_path),
        token: TokenSource::Spawn(token),
        name: "1device-tray".into(),
        version: env!("CARGO_PKG_VERSION").into(),
        role: "tray".into(),
        // session.read for the status icon, system.shutdown for the Quit — both
        // within the supervisor's grant.
        scopes: vec!["session.read".into(), "system.shutdown".into()],
        topics: vec!["session".into()],
        optional_scopes: Vec::new(),
        optional_topics: Vec::new(),
        served_methods: vec![],
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
    // Cursor, not the slice itself: png 0.18's Decoder requires Seek, which
    // `&[u8]` does not implement. Cursor<&[u8]> is BufRead + Seek and copies
    // nothing — it just carries a position into the embedded bytes.
    let mut reader = png::Decoder::new(std::io::Cursor::new(PNG))
        .read_info()
        .expect("tray icon header");
    // Also 0.18: output_buffer_size returns None when the size would overflow
    // usize. This icon is 32x32 and baked into the binary, so that is a
    // build-time impossibility — treated like the other expects here.
    let mut buf = vec![0; reader.output_buffer_size().expect("tray icon buffer size")];
    let info = reader.next_frame(&mut buf).expect("tray icon pixels");
    buf.truncate(info.buffer_size());
    Icon::from_rgba(buf, info.width, info.height).expect("tray icon rgba")
}
