// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

//! Supervisor test fixture: a component that behaves like a real one. It reads
//! its token from standard input, connects to the Core, says `hello`, and
//! records everything it does in a log that the test reads back.
//!
//! This is not a shipped artifact — it exists only so that the chain
//! "supervisor → process → token → IPC → hello" is exercised for real rather
//! than simulated. Deliberately blocking `std` and dependency-free: what it
//! proves is the contract, not the mechanics of the Rust client.
//!
//! Configured by its ARGUMENTS (not by its environment: the tests run in
//! parallel within a single process, which cannot hold two environments at
//! once). Only `UNIVERSALLINK_IPC_PATH` comes from the environment — the
//! supervisor is the one that sets it.
//!
//! - `--dir=<path>`     : where to write `journal` and the heartbeat files.
//! - `--mode=<mode>`    : `stay` (default), `exit`, `leak`, `heartbeat`.
//! - `--role=<role>`    : role requested at `hello`.
//! - `--scopes=a,b`     : requested scopes.
//! - `--exit=<code>`    : exit code for `exit` mode.
//! - `--child=1`        : in `stay` mode, spawns a `heartbeat` grandchild.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

fn main() {
    let args = parse_args();
    let get = |key: &str| args.get(key).map(String::as_str);
    let dir = PathBuf::from(get("dir").expect("--dir"));
    let mode = get("mode").unwrap_or("stay");

    if mode == "heartbeat" {
        // The grandchild: no IPC, it beats until it is killed. It is the
        // stopping of its heartbeat that proves the descendants are swept.
        heartbeat(&dir, "grandchild");
    }

    let token = read_token();
    let role = get("role").unwrap_or("tray");

    // The frozen contract (support.rs): the token arrives via stdin, NEVER via
    // argv nor the environment. We check it from the inside — nobody is better
    // placed than the child to say what it actually received.
    let in_env = std::env::vars().any(|(_, v)| v.contains(&token));
    let in_argv = std::env::args().any(|a| a.contains(&token));
    journal(&dir, &format!("leak env={in_env} argv={in_argv}"));

    if mode == "leak" {
        // Dies without ever introducing itself: the test will check that the
        // supervisor took the token back. (A real component would obviously
        // never disclose its own.)
        journal(&dir, &format!("token {token}"));
        return;
    }

    let scopes: Vec<String> = get("scopes")
        .unwrap_or("session.read")
        .split(',')
        .map(str::to_string)
        .collect();

    match hello(&token, role, &scopes) {
        Ok(granted) => journal(&dir, &format!("hello ok {}", granted.join(","))),
        Err(e) => journal(&dir, &format!("hello err {e}")),
    }

    if mode == "exit" {
        let code = get("exit").and_then(|c| c.parse().ok()).unwrap_or(1);
        std::process::exit(code);
    }

    if get("child").is_some() {
        // Deliberately never `wait()`ed: this grandchild IS the subject of the
        // test. We want it to outlive its parent and for the supervisor to be
        // the one that takes it down (process group / Job Object).
        #[allow(clippy::zombie_processes)]
        std::process::Command::new(std::env::current_exe().expect("current_exe"))
            .arg("--mode=heartbeat")
            .arg(format!("--dir={}", dir.display()))
            .stdin(std::process::Stdio::null())
            .spawn()
            .expect("grandchild");
    }

    // EOF on standard input is the shutdown request. We wait for it on a
    // thread while the main thread keeps the beat.
    let dir_for_stdin = dir.clone();
    std::thread::spawn(move || {
        let mut sink = Vec::new();
        let _ = std::io::stdin().read_to_end(&mut sink);
        journal(&dir_for_stdin, "bye");
        std::process::exit(0);
    });
    heartbeat(&dir, role);
}

/// `--key=value`, nothing more.
fn parse_args() -> HashMap<String, String> {
    std::env::args()
        .skip(1)
        .filter_map(|arg| {
            let (key, value) = arg.strip_prefix("--")?.split_once('=')?;
            Some((key.to_string(), value.to_string()))
        })
        .collect()
}

/// The spawn token: first line of standard input.
fn read_token() -> String {
    let mut line = String::new();
    BufReader::new(std::io::stdin())
        .read_line(&mut line)
        .expect("reading the token");
    line.trim().to_string()
}

/// A counter that keeps advancing: a test that sees it frozen knows the
/// process is dead, without having to poll pids (nor fear their reuse).
fn heartbeat(dir: &Path, tag: &str) -> ! {
    let path = dir.join(format!("alive-{tag}"));
    let mut beat: u64 = 0;
    loop {
        beat += 1;
        let _ = std::fs::write(&path, beat.to_string());
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn journal(dir: &Path, line: &str) {
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("journal"))
        .expect("journal");
    writeln!(file, "{line}").expect("writing the log");
}

/// `hello` in JSON-RPC 2.0, LSP framing, on the Core's listening endpoint.
/// Returns the granted scopes, or the application error code.
fn hello(token: &str, role: &str, scopes: &[String]) -> Result<Vec<String>, String> {
    let path = std::env::var("UNIVERSALLINK_IPC_PATH")
        .map_err(|_| "UNIVERSALLINK_IPC_PATH not set".to_string())?;
    let mut stream = connect(&path)?;

    let scopes_json = scopes
        .iter()
        .map(|s| format!("\"{s}\""))
        .collect::<Vec<_>>()
        .join(",");
    let body = format!(
        r#"{{"jsonrpc":"2.0","id":1,"method":"hello","params":{{"name":"fixture","version":"0","role":"{role}","scopes":[{scopes_json}],"token":"{token}"}}}}"#
    );
    let frame = format!("Content-Length: {}\r\n\r\n{body}", body.len());
    stream
        .write_all(frame.as_bytes())
        .map_err(|e| format!("write: {e}"))?;

    let response = read_frame(&mut stream)?;
    // The numeric JSON-RPC `code` does not match: only `data.code` is a
    // string.
    if let Some(code) = between(&response, r#""code":""#, "\"") {
        return Err(code);
    }
    if response.contains(r#""error""#) {
        return Err("error without code".to_string());
    }
    let granted = between(&response, r#""granted_scopes":["#, "]").unwrap_or_default();
    Ok(granted
        .split(',')
        .map(|s| s.trim().trim_matches('"').to_string())
        .filter(|s| !s.is_empty())
        .collect())
}

#[cfg(unix)]
fn connect(path: &str) -> Result<std::os::unix::net::UnixStream, String> {
    std::os::unix::net::UnixStream::connect(path).map_err(|e| format!("connect: {e}"))
}

#[cfg(windows)]
fn connect(path: &str) -> Result<std::fs::File, String> {
    // A named pipe opens like a file.
    std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .map_err(|e| format!("connect: {e}"))
}

/// Reads an LSP frame: headers, blank line, `Content-Length` bytes.
fn read_frame(stream: &mut impl Read) -> Result<String, String> {
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        let n = stream.read(&mut byte).map_err(|e| format!("read: {e}"))?;
        if n == 0 {
            return Err("connection closed".to_string());
        }
        head.push(byte[0]);
        if head.len() > 8192 {
            return Err("oversized headers".to_string());
        }
    }
    let head = String::from_utf8_lossy(&head).into_owned();
    let length: usize = head
        .lines()
        .filter_map(|l| l.split_once(':'))
        .find(|(name, _)| name.trim().eq_ignore_ascii_case("content-length"))
        .and_then(|(_, value)| value.trim().parse().ok())
        .ok_or("frame without Content-Length")?;
    let mut body = vec![0u8; length];
    stream
        .read_exact(&mut body)
        .map_err(|e| format!("body: {e}"))?;
    Ok(String::from_utf8_lossy(&body).into_owned())
}

/// Hand-rolled extraction: the fixture has no dependencies, and the shape of
/// the Core's responses is frozen by the Core's tests.
fn between(text: &str, start: &str, end: &str) -> Option<String> {
    let rest = text.split_once(start)?.1;
    let value = rest.split_once(end)?.0;
    Some(value.to_string())
}
