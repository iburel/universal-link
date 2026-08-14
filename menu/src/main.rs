// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

//! The `1device-menu` binary: one executable, two jobs.
//!
//! - **manager** (no mode flag) — what the Core's supervisor launches. It holds
//!   the IPC connection, keeps the target list, rewrites the OS menu surfaces,
//!   and serves the couriers.
//! - **courier** (`--send <device_id> -- <paths…>`) — what a menu entry actually
//!   invokes. It is started by the file manager, so it carries no credentials and
//!   knows nothing: it hands `(target, paths[])` to the manager and exits. See
//!   `channel`'s header for why it must not talk to the Core itself.
//! - `--targets` prints the manager's current view: a way to inspect a real
//!   desktop by hand, and the same request a future family-B shim will make.
//!
//! All the testable logic lives in the lib; this is wiring.
//!
//! # Why this is a GUI-subsystem binary on Windows
//!
//! Because a click starts it. Explorer launches a verb's command line with no
//! console of its own, so a console-subsystem process gets a brand-new console —
//! and a black window flashes on screen at every single click. The supervised
//! components do not have this problem (they inherit the Core's windowless
//! console); a process the shell starts does.
//!
//! The consequence is that this process may have NO standard handles at all, and
//! `println!` panics when it cannot write. Hence [`say`] and [`note`] below: a
//! courier's job is to deliver a click, not to report on it.

#![cfg_attr(windows, windows_subsystem = "windows")]

use std::path::PathBuf;
use std::time::Duration;

use onedevice_ipc_client::{ClientConfig, TokenSource};
use onedevice_menu::channel::{self, Request, Response};
use onedevice_menu::{HelperCommand, Outcome, os, run};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};

/// Set by the supervisor: the Core's listening endpoint.
const IPC_PATH_ENV: &str = "ONEDEVICE_IPC_PATH";
/// Barely matters: a spawn token is single-use, so we exit on the first loss
/// rather than let the client retry.
const RECONNECT_BASE_DELAY: Duration = Duration::from_millis(500);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// The role and rights this component asks for. `session.read` is what makes the
/// menu honest: without the `session` topic and `session.status` there is no way
/// to know the Core is actually connected to the server, and the directory cache
/// it serves offline would have us offer targets that cannot be reached.
const ROLE: &str = "menu-backend";
const SCOPES: [&str; 3] = ["session.read", "devices.read", "files.send"];
const TOPICS: [&str; 2] = ["session", "devices"];

/// Writes a line to standard output, or nowhere. See the module header: a process
/// the shell started may have no standard handles, and `println!` panics on a write
/// it cannot do.
macro_rules! say {
    ($($arg:tt)*) => {{
        use std::io::Write;
        let _ = writeln!(std::io::stdout(), $($arg)*);
    }};
}

/// Same, for standard error — which is where the supervisor collects a component's
/// log, and where a hand-run courier reports a refusal.
macro_rules! note {
    ($($arg:tt)*) => {{
        use std::io::Write;
        let _ = writeln!(std::io::stderr(), $($arg)*);
    }};
}

enum Mode {
    Manager,
    Send {
        device_id: String,
        paths: Vec<PathBuf>,
    },
    Targets,
}

struct Args {
    mode: Mode,
    /// Overrides the channel path resolved from the environment. Used by the
    /// live tests, and by hand during development; production artifacts carry no
    /// such argument.
    channel: Option<PathBuf>,
}

fn main() {
    // `args_os`, not `args`: the latter PANICS on an argument that is not valid
    // Unicode, and on unix a file name is an arbitrary byte string — so a click
    // on a latin-1 named file would abort here instead of being refused with a
    // message.
    let raw: Vec<std::ffi::OsString> = std::env::args_os().skip(1).collect();
    let mut args = Vec::with_capacity(raw.len());
    for arg in raw {
        match arg.into_string() {
            Ok(arg) => args.push(arg),
            Err(bad) => {
                note!(
                    "[1device-menu] argument is not valid UTF-8: {}",
                    bad.display()
                );
                std::process::exit(1);
            }
        }
    }
    let args = match parse_args(&args) {
        Ok(args) => args,
        Err(message) => {
            note!("[1device-menu] {message}");
            note!("{USAGE}");
            std::process::exit(2);
        }
    };

    // A courier has one round trip to make: a single-threaded runtime starts
    // faster, and a click should feel immediate.
    let is_manager = matches!(args.mode, Mode::Manager);
    let runtime = runtime(is_manager);
    let code = if is_manager {
        runtime.block_on(manager(args.channel))
    } else {
        runtime.block_on(courier(args))
    };
    // NOT a plain drop: `Runtime::drop` waits for every blocking task, and the
    // manager leaves one behind for ever — the read on standard input that is its
    // stop signal, which only ends when the supervisor closes it. Dropping here
    // would hang the process exactly when the contract demands it exit (a lost
    // IPC connection, an incompatible Core), leaving a live and useless child the
    // supervisor cannot detect. The sibling components dodge this by keeping
    // their runtime on a side thread; we shut it down explicitly instead.
    runtime.shutdown_background();
    std::process::exit(code);
}

const USAGE: &str = "usage:
  1device-menu                                  run the manager (launched by the Core)
  1device-menu --send <device_id> -- <paths…>    send paths to a device (a menu click)
  1device-menu --targets                         print the manager's current targets
options:
  --channel <path>   talk to a manager on this channel instead of the default";

fn runtime(multi_thread: bool) -> tokio::runtime::Runtime {
    let mut builder = if multi_thread {
        tokio::runtime::Builder::new_multi_thread()
    } else {
        tokio::runtime::Builder::new_current_thread()
    };
    builder.enable_all().build().expect("tokio runtime")
}

/// Parses argv. Hand-rolled: the whole grammar is three flags, and a menu entry
/// is a command line we generate ourselves.
fn parse_args(args: &[String]) -> Result<Args, String> {
    let mut channel = None;
    let mut mode = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--channel" => {
                let value = args.get(i + 1).ok_or("--channel needs a path")?;
                channel = Some(PathBuf::from(value));
                i += 2;
            }
            "--targets" => {
                if mode.is_some() {
                    return Err("only one mode at a time".into());
                }
                mode = Some(Mode::Targets);
                i += 1;
            }
            "--send" => {
                if mode.is_some() {
                    return Err("only one mode at a time".into());
                }
                let device_id = args.get(i + 1).ok_or("--send needs a device id")?;
                if device_id.trim().is_empty() {
                    return Err("--send needs a device id".into());
                }
                // Everything after the separator is a path — that is what makes
                // a file named `-r` a path and not a flag. A generated entry
                // always emits it; accept its absence for a hand-typed call.
                let rest = &args[i + 2..];
                let rest = match rest.first().map(String::as_str) {
                    Some("--") => &rest[1..],
                    _ => rest,
                };
                mode = Some(Mode::Send {
                    device_id: device_id.trim().to_string(),
                    paths: rest.iter().map(PathBuf::from).collect(),
                });
                i = args.len();
            }
            "--help" | "-h" => {
                say!("{USAGE}");
                std::process::exit(0);
            }
            other => return Err(format!("unexpected argument: {other}")),
        }
    }
    Ok(Args {
        mode: mode.unwrap_or(Mode::Manager),
        channel,
    })
}

/// Resolves the channel path: the override, else the per-user convention. The
/// convention is the only option for a real click — the file manager passes us
/// nothing.
fn channel_path(override_path: Option<PathBuf>) -> Result<PathBuf, String> {
    if let Some(path) = override_path {
        return Ok(path);
    }
    onedevice_paths::production_endpoint()
        .map(|endpoint| endpoint.menu_channel)
        .ok_or_else(|| "cannot resolve the local channel path from the environment".to_string())
}

// ---------------------------------------------------------------------------
// Courier
// ---------------------------------------------------------------------------

async fn courier(args: Args) -> i32 {
    let path = match channel_path(args.channel) {
        Ok(path) => path,
        Err(message) => {
            note!("[1device-menu] {message}");
            return 1;
        }
    };
    let request = match args.mode {
        Mode::Targets => Request::Targets,
        Mode::Send { device_id, paths } => {
            let paths = match absolute_paths(paths) {
                Ok(paths) => paths,
                Err(message) => {
                    note!("[1device-menu] {message}");
                    return 1;
                }
            };
            if paths.is_empty() {
                note!("[1device-menu] nothing to send");
                return 1;
            }
            Request::Send { device_id, paths }
        }
        Mode::Manager => unreachable!("the manager does not run as a courier"),
    };
    match channel::request(&path, &request).await {
        Ok(Response::Accepted { transfer_id }) => {
            say!("{transfer_id}");
            0
        }
        Ok(Response::Targets(targets)) => {
            for target in targets {
                say!("{}\t{}\t{}", target.device_id, target.name, target.platform);
            }
            0
        }
        Ok(Response::Failed { error }) => {
            note!("[1device-menu] refused: {error}");
            1
        }
        Err(message) => {
            note!("[1device-menu] {message}");
            1
        }
    }
}

/// Makes every path absolute against the working directory.
///
/// Necessary, not cosmetic: a Nautilus script is run with the browsed folder as
/// its working directory and is handed bare file NAMES, while the Core resolves a
/// relative path against its own working directory — a different folder
/// entirely. The manager refuses a relative path outright, so this is where the
/// intent is preserved.
fn absolute_paths(paths: Vec<PathBuf>) -> Result<Vec<PathBuf>, String> {
    let mut cwd = None;
    let mut out = Vec::with_capacity(paths.len());
    for path in paths {
        let path = if path.is_absolute() {
            path
        } else {
            if cwd.is_none() {
                cwd = Some(
                    std::env::current_dir()
                        .map_err(|e| format!("cannot resolve the working directory: {e}"))?,
                );
            }
            cwd.as_ref().expect("resolved just above").join(path)
        };
        // The control plane is JSON: a name that is not valid UTF-8 cannot be
        // expressed, and transcoding it lossily would name a different file.
        if path.to_str().is_none() {
            return Err(format!("path is not valid UTF-8: {}", path.display()));
        }
        out.push(path);
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Manager
// ---------------------------------------------------------------------------

async fn manager(channel_override: Option<PathBuf>) -> i32 {
    let Ok(ipc_path) = std::env::var(IPC_PATH_ENV) else {
        note!("[1device-menu] {IPC_PATH_ENV} is not set: the manager is launched by the Core");
        return 1;
    };
    let program = match std::env::current_exe() {
        Ok(program) => program,
        Err(e) => {
            note!("[1device-menu] cannot resolve our own path: {e}");
            return 1;
        }
    };
    let path = match channel_path(channel_override.clone()) {
        Ok(path) => path,
        Err(message) => {
            note!("[1device-menu] {message}");
            return 1;
        }
    };

    // A generated menu entry must reach THIS manager: if we were pointed at a
    // channel by hand, the artifacts have to carry the same override.
    let extra_args = match &channel_override {
        Some(path) => vec!["--channel".into(), path.to_string_lossy().into_owned()],
        None => Vec::new(),
    };
    let surfaces = match os::create(HelperCommand {
        program,
        extra_args,
    }) {
        Ok(surfaces) => surfaces,
        Err(e) => {
            // Not a failure: this platform has no surface yet (or no desktop).
            // Exiting 0 tells the supervisor we are done, not broken.
            note!("[1device-menu] {e}");
            return 0;
        }
    };

    let listener = match channel::bind(&path) {
        Ok(listener) => listener,
        Err(channel::BindError::AlreadyRunning) => {
            note!("[1device-menu] another manager already owns the local channel");
            return 0;
        }
        Err(e) => {
            note!("[1device-menu] {e}");
            return 1;
        }
    };

    // Contract: the spawn token is the FIRST LINE of standard input — never argv
    // (world-readable) nor the environment (inherited by descendants). Standard
    // input then stays open; its EOF is the graceful-stop signal, and on Windows
    // it is the ONLY one (the supervisor sends no signal there), so it is also
    // our only chance to remove the menu entries.
    let mut stdin = BufReader::new(tokio::io::stdin());
    let mut token = String::new();
    match stdin.read_line(&mut token).await {
        Ok(_) if !token.trim().is_empty() => {}
        _ => {
            note!("[1device-menu] no spawn token on standard input");
            return 1;
        }
    }
    let token = token.trim().to_string();

    let (client, events) = onedevice_ipc_client::spawn(ClientConfig {
        ipc_path: PathBuf::from(ipc_path),
        token: TokenSource::Spawn(token),
        name: "1device-menu".into(),
        version: env!("CARGO_PKG_VERSION").into(),
        role: ROLE.into(),
        scopes: SCOPES.iter().map(|s| (*s).to_string()).collect(),
        topics: TOPICS.iter().map(|t| (*t).to_string()).collect(),
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

    match run(client, events, stdin_closed, listener, surfaces).await {
        Outcome::StdinClosed => 0,
        // IPC lost / incompatible / client ended: exit non-zero so the supervisor
        // restarts us with a fresh, single-use spawn token.
        Outcome::ConnectionLost | Outcome::Incompatible | Outcome::ClientEnded => 1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args_of(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_string()).collect()
    }

    fn send_of(list: &[&str]) -> (String, Vec<PathBuf>, Option<PathBuf>) {
        let args = parse_args(&args_of(list)).expect("parse");
        match args.mode {
            Mode::Send { device_id, paths } => (device_id, paths, args.channel),
            _ => panic!("expected a send"),
        }
    }

    #[test]
    fn no_argument_runs_the_manager() {
        assert!(matches!(
            parse_args(&[]).expect("parse").mode,
            Mode::Manager
        ));
        assert!(matches!(
            parse_args(&args_of(&["--targets"])).expect("parse").mode,
            Mode::Targets
        ));
    }

    /// The grammar a generated menu entry emits. The trailing `--` is what makes
    /// a file named like a flag a path: losing it would send an extra bogus
    /// `<cwd>/--` with every single click.
    #[test]
    fn the_separator_ends_the_flags_and_never_becomes_a_path() {
        let (device, paths, _) = send_of(&["--send", "d_1", "--", "/a.txt", "/b b.txt"]);
        assert_eq!(device, "d_1");
        assert_eq!(paths, [PathBuf::from("/a.txt"), PathBuf::from("/b b.txt")]);

        // A file whose name looks like one of our own flags is still a path.
        let (_, paths, channel) = send_of(&["--send", "d_1", "--", "--channel", "--targets"]);
        assert_eq!(
            paths,
            [PathBuf::from("--channel"), PathBuf::from("--targets")]
        );
        assert_eq!(channel, None, "nothing after the separator is an option");
    }

    /// Accepted for a hand-typed call; a generated entry always emits the
    /// separator.
    #[test]
    fn the_separator_may_be_omitted() {
        let (device, paths, _) = send_of(&["--send", "d_1", "/a.txt"]);
        assert_eq!(device, "d_1");
        assert_eq!(paths, [PathBuf::from("/a.txt")]);
    }

    /// A live test points the generated artifacts at its own channel, so the
    /// override has to be parsed BEFORE the mode.
    #[test]
    fn the_channel_override_precedes_the_mode() {
        let (device, paths, channel) =
            send_of(&["--channel", "/tmp/t.sock", "--send", "d_1", "--", "/a"]);
        assert_eq!(device, "d_1");
        assert_eq!(paths, [PathBuf::from("/a")]);
        assert_eq!(channel, Some(PathBuf::from("/tmp/t.sock")));
    }

    #[test]
    fn malformed_command_lines_are_refused() {
        for bad in [
            &["--send"][..],
            &["--send", "   "][..],
            &["--channel"][..],
            &["--targets", "--send", "d_1"][..],
            &["--nope"][..],
            &["stray"][..],
        ] {
            assert!(
                parse_args(&args_of(bad)).is_err(),
                "should have been refused: {bad:?}"
            );
        }
    }

    /// A Nautilus script runs with the browsed folder as its working directory
    /// and is handed bare NAMES; the Core would resolve them against its own
    /// working directory, a different folder entirely.
    #[test]
    fn relative_paths_are_resolved_against_the_working_directory() {
        let cwd = std::env::current_dir().expect("cwd");
        let resolved = absolute_paths(vec![PathBuf::from("notes.txt")]).expect("resolve");
        assert_eq!(resolved, [cwd.join("notes.txt")]);
        assert!(resolved[0].is_absolute());
    }

    #[test]
    fn absolute_paths_are_left_alone() {
        let absolute = if cfg!(windows) {
            PathBuf::from(r"C:\tmp\a.txt")
        } else {
            PathBuf::from("/tmp/a.txt")
        };
        assert_eq!(
            absolute_paths(vec![absolute.clone()]).expect("resolve"),
            [absolute]
        );
    }

    /// The control plane is JSON: a name that is not valid UTF-8 cannot be
    /// expressed, and transcoding it lossily would designate a different file.
    #[cfg(unix)]
    #[test]
    fn a_non_utf8_path_is_refused_rather_than_mangled() {
        use std::os::unix::ffi::OsStrExt;

        let bad = PathBuf::from(std::ffi::OsStr::from_bytes(b"/tmp/caf\xe9.txt"));
        assert!(absolute_paths(vec![bad]).is_err());
    }
}
