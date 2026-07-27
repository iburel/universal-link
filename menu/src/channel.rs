// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

//! The manager's private local channel: how a click reaches us.
//!
//! # Why a channel at all
//!
//! A menu entry is a command line the OS keeps on disk, so a click starts a
//! brand-new process whose argv the shell composed. That process must not hold
//! Core credentials: the file token is the GUI's total-trust root, and handing
//! it to something the shell launches with attacker-influenceable arguments
//! would turn every writable registry key or `.desktop` file into a Core
//! capability. So the helper is a courier — it carries `(target, paths[])` to
//! the manager, which owns the one scoped IPC connection and calls `files.send`
//! itself. This is doc/architecture.md's rule for the family-B shims ("they talk
//! only to their manager, never directly to the Core"), applied to the family-A
//! helpers too.
//!
//! # Security
//!
//! Same level 1 as the Core's own IPC (doc/architecture.md): the surface is open
//! to the current user only — a private folder plus a **peer credential check**
//! on unix (the folder alone is not enough: macOS ignores a socket file's mode),
//! an owner-only DACL on Windows. Binding also takes the user's exclusivity, so
//! two managers cannot fight over the same artifacts.
//!
//! This module is a deliberate second copy of `core/src/transport.rs`'s platform
//! glue. Not shared, because a component linking the Core library would break
//! the layering the whole IPC exists to enforce; the project's rule is to
//! extract at the THIRD copy.
//!
//! # Protocol
//!
//! One line of JSON in, one line of JSON out, then the connection closes. No
//! framing header and no multiplexing: a courier makes exactly one request and
//! dies. Bounded (`MAX_REQUEST_BYTES`) before anything is allocated.
//!
//! ```text
//! → {"v":1,"kind":"send","device_id":"d_7f3a…","paths":["/home/u/a.txt"]}
//! ← {"ok":true,"transfer_id":"t_1a2b3c4d"}
//! ← {"ok":false,"error":"NO_SUCH_TARGET"}
//!
//! → {"v":1,"kind":"targets"}
//! ← {"ok":true,"targets":[{"device_id":"d_7f3a…","name":"PC-B","platform":"linux"}]}
//! ```
//!
//! `targets` has no consumer in v1 — it is the pull the family-B shims (a
//! Windows 11 `IExplorerCommand` DLL, a FinderSync appex) will use when the menu
//! opens, and it is implemented and tested now so that adding them does not
//! reopen this protocol. `--targets` also makes the manager's view inspectable
//! by hand on a real desktop.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

use crate::surface::Target;

/// Version of this channel's protocol, carried by every request. A shim built
/// against a future version must be refused, not misread.
pub const PROTOCOL_VERSION: u64 = 1;

/// Ceiling on a request line. A legitimate one can be large — a multi-select of
/// thousands of files is a list of thousands of absolute paths — so this is
/// generous, but it is a ceiling: nothing is allocated on a length the peer
/// merely claims. Matches the Core's own IPC frame ceiling.
pub const MAX_REQUEST_BYTES: u64 = 16 * 1024 * 1024;

/// How long a courier is given to state its business before we hang up. It is a
/// local process that has nothing to compute: this only stops a stuck one from
/// holding a slot.
pub const REQUEST_READ_TIMEOUT: Duration = Duration::from_secs(10);

/// Concurrent couriers served at once. A burst is real — the Windows classic
/// menu starts one process per selected file — but it is bounded, and beyond
/// this the extra connections wait rather than being refused.
pub const MAX_CONCURRENT_CLIENTS: usize = 32;

/// How long a courier waits for a free listening instance before giving up.
///
/// Windows only, and not an edge case. A named pipe has a fixed number of server
/// instances and the manager keeps exactly ONE waiting, so a courier that arrives
/// between the hand-over of the previous one and the creation of its replacement
/// is refused outright with `ERROR_PIPE_BUSY` — and a courier has no second
/// chance: the click would simply be lost. On unix there is nothing to wait for
/// (a listening socket queues the connection in its backlog).
pub const CONNECT_RETRY_BUDGET: Duration = Duration::from_secs(5);
#[cfg(windows)]
const CONNECT_RETRY_PAUSE: Duration = Duration::from_millis(20);

// ---------------------------------------------------------------------------
// The line protocol (pure).
// ---------------------------------------------------------------------------

/// What a courier asks for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Request {
    /// Send `paths` to the device the clicked entry names.
    Send {
        device_id: String,
        paths: Vec<PathBuf>,
    },
    /// The manager's current target list (the family-B pull).
    Targets,
}

/// Error codes this channel answers with, on top of the application codes the
/// Core's own reply carries through verbatim (`DEVICE_UNKNOWN`,
/// `DEVICE_OFFLINE`, `MANIFEST_TOO_LARGE`…).
pub mod error {
    /// Unparseable, unknown kind, or missing/ill-typed field.
    pub const BAD_REQUEST: &str = "BAD_REQUEST";
    /// A protocol version this manager does not speak.
    pub const UNSUPPORTED_VERSION: &str = "UNSUPPORTED_VERSION";
    /// The request line exceeded [`super::MAX_REQUEST_BYTES`].
    pub const REQUEST_TOO_LARGE: &str = "REQUEST_TOO_LARGE";
    /// `paths` was empty: there is nothing to send.
    pub const NO_PATHS: &str = "NO_PATHS";
    /// A path was not absolute. The Core would resolve it against ITS working
    /// directory, which is not the file manager's — refused rather than sent
    /// somewhere the user did not point at.
    pub const RELATIVE_PATH: &str = "RELATIVE_PATH";
    /// A path cannot be expressed on the Core's JSON control plane. Refused
    /// rather than lossily transcoded into a name that would designate a
    /// different file (or none).
    pub const NON_UTF8_PATH: &str = "NON_UTF8_PATH";
    /// The device is not a current target. Fail-closed and purely local: a stale
    /// artifact (written before the peer went offline, or left behind by a
    /// crashed manager) never reaches the Core.
    pub const NO_SUCH_TARGET: &str = "NO_SUCH_TARGET";
    /// The manager has no usable connection to the Core.
    pub const CORE_UNREACHABLE: &str = "CORE_UNREACHABLE";
}

impl Request {
    pub fn to_line(&self) -> String {
        let v = match self {
            Request::Send { device_id, paths } => json!({
                "v": PROTOCOL_VERSION,
                "kind": "send",
                "device_id": device_id,
                "paths": paths.iter().map(|p| p.to_string_lossy()).collect::<Vec<_>>(),
            }),
            Request::Targets => json!({ "v": PROTOCOL_VERSION, "kind": "targets" }),
        };
        format!("{v}\n")
    }

    /// Parses a request line, or the error code to answer with. Strict on
    /// purpose: a malformed courier is a bug or an intruder, and guessing what
    /// it meant would mean sending files somewhere on a guess.
    pub fn parse(line: &str) -> Result<Request, &'static str> {
        let v: Value = serde_json::from_str(line.trim()).map_err(|_| error::BAD_REQUEST)?;
        match v["v"].as_u64() {
            Some(PROTOCOL_VERSION) => {}
            Some(_) => return Err(error::UNSUPPORTED_VERSION),
            None => return Err(error::BAD_REQUEST),
        }
        match v["kind"].as_str() {
            Some("targets") => Ok(Request::Targets),
            Some("send") => {
                let device_id = v["device_id"]
                    .as_str()
                    .map(str::trim)
                    .filter(|id| !id.is_empty())
                    .ok_or(error::BAD_REQUEST)?
                    .to_string();
                let raw = v["paths"].as_array().ok_or(error::BAD_REQUEST)?;
                if raw.is_empty() {
                    return Err(error::NO_PATHS);
                }
                let mut paths = Vec::with_capacity(raw.len());
                for p in raw {
                    let p = p
                        .as_str()
                        .filter(|p| !p.is_empty())
                        .ok_or(error::BAD_REQUEST)?;
                    let p = PathBuf::from(p);
                    if !p.is_absolute() {
                        return Err(error::RELATIVE_PATH);
                    }
                    paths.push(p);
                }
                Ok(Request::Send { device_id, paths })
            }
            _ => Err(error::BAD_REQUEST),
        }
    }
}

/// What the manager answers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Response {
    /// The Core accepted the send and minted this transfer.
    Accepted { transfer_id: String },
    /// The manager's current targets.
    Targets(Vec<Target>),
    /// Refused, with a code from [`error`] or an application code relayed from
    /// the Core.
    Failed { error: String },
}

impl Response {
    pub fn failed(code: &str) -> Response {
        Response::Failed {
            error: code.to_string(),
        }
    }

    pub fn to_line(&self) -> String {
        let v = match self {
            Response::Accepted { transfer_id } => json!({ "ok": true, "transfer_id": transfer_id }),
            Response::Targets(targets) => json!({
                "ok": true,
                "targets": targets.iter().map(|t| json!({
                    "device_id": t.device_id,
                    "name": t.name,
                    "platform": t.platform,
                })).collect::<Vec<_>>(),
            }),
            Response::Failed { error } => json!({ "ok": false, "error": error }),
        };
        format!("{v}\n")
    }

    pub fn parse(line: &str) -> Result<Response, String> {
        let v: Value =
            serde_json::from_str(line.trim()).map_err(|e| format!("malformed reply: {e}"))?;
        if v["ok"].as_bool() != Some(true) {
            return Ok(Response::Failed {
                error: v["error"].as_str().unwrap_or("UNKNOWN").to_string(),
            });
        }
        if let Some(id) = v["transfer_id"].as_str() {
            return Ok(Response::Accepted {
                transfer_id: id.to_string(),
            });
        }
        let targets = v["targets"].as_array().ok_or("reply without a payload")?;
        Ok(Response::Targets(
            targets
                .iter()
                .map(|t| Target {
                    device_id: t["device_id"].as_str().unwrap_or_default().to_string(),
                    name: t["name"].as_str().unwrap_or_default().to_string(),
                    platform: t["platform"].as_str().unwrap_or_default().to_string(),
                })
                .collect(),
        ))
    }
}

// ---------------------------------------------------------------------------
// Reading a request / writing a reply on an accepted connection.
// ---------------------------------------------------------------------------

/// Reads one bounded request line. `Err` carries the code to answer with, so a
/// refusal is still a well-formed reply rather than a silent hang-up.
pub async fn read_request(stream: &mut Stream) -> Result<Request, &'static str> {
    // `take` bounds the read itself: a courier that never sends a newline
    // cannot make us grow a buffer past the ceiling.
    let mut reader = BufReader::new(stream).take(MAX_REQUEST_BYTES + 1);
    let mut line = String::new();
    match tokio::time::timeout(REQUEST_READ_TIMEOUT, reader.read_line(&mut line)).await {
        // A line longer than the ceiling arrives truncated at the limit, with no
        // newline: distinguishable, and reported as what it is.
        Ok(Ok(n)) if n as u64 > MAX_REQUEST_BYTES => Err(error::REQUEST_TOO_LARGE),
        Ok(Ok(0)) => Err(error::BAD_REQUEST),
        Ok(Ok(_)) => Request::parse(&line),
        Ok(Err(_)) | Err(_) => Err(error::BAD_REQUEST),
    }
}

/// Writes the reply and closes. Best-effort: a courier that hung up mid-flight
/// gets no reply and there is nothing to do about it.
pub async fn write_response(stream: &mut Stream, response: &Response) {
    let _ = stream.write_all(response.to_line().as_bytes()).await;
    let _ = stream.flush().await;
    let _ = stream.shutdown().await;
}

// ---------------------------------------------------------------------------
// The courier side: one shot, no retry.
// ---------------------------------------------------------------------------

/// How long the helper waits for the whole exchange. Short: a click that does
/// nothing visible is better than a file manager entry that hangs.
pub const CLIENT_TIMEOUT: Duration = Duration::from_secs(20);

/// Connects to the manager, makes one request, returns its answer.
///
/// A connection failure means "no manager": the entry is stale (the manager
/// stopped, or crashed and left its artifacts behind — the accepted residual of
/// family-A registration). The click then fails silently, which is the decided
/// behavior.
pub async fn request(path: &Path, req: &Request) -> Result<Response, String> {
    let exchange = async {
        let mut stream = connect(path)
            .await
            .map_err(|e| format!("no manager: {e}"))?;
        stream
            .write_all(req.to_line().as_bytes())
            .await
            .map_err(|e| format!("cannot send: {e}"))?;
        stream
            .flush()
            .await
            .map_err(|e| format!("cannot send: {e}"))?;
        let mut reader = BufReader::new(&mut stream).take(MAX_REQUEST_BYTES + 1);
        let mut line = String::new();
        reader
            .read_line(&mut line)
            .await
            .map_err(|e| format!("no reply: {e}"))?;
        if line.is_empty() {
            return Err("the manager closed without replying".to_string());
        }
        Response::parse(&line)
    };
    match tokio::time::timeout(CLIENT_TIMEOUT, exchange).await {
        Ok(result) => result,
        Err(_) => Err("the manager did not reply in time".to_string()),
    }
}

// ---------------------------------------------------------------------------
// Per-platform listening point. Mirrors core/src/transport.rs.
// ---------------------------------------------------------------------------

/// Why binding failed. `AlreadyRunning` is not a failure: another manager holds
/// the channel, and this process has nothing to do.
#[derive(Debug)]
pub enum BindError {
    AlreadyRunning,
    Io(std::io::Error),
}

impl std::fmt::Display for BindError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BindError::AlreadyRunning => {
                write!(f, "a menu manager is already listening for this user")
            }
            BindError::Io(e) => write!(f, "cannot open the local channel: {e}"),
        }
    }
}

impl std::error::Error for BindError {}

impl From<std::io::Error> for BindError {
    fn from(e: std::io::Error) -> BindError {
        BindError::Io(e)
    }
}

#[cfg(unix)]
pub use unix::{Listener, Stream, bind, connect};
#[cfg(windows)]
pub use windows::{Listener, Stream, bind, connect};

#[cfg(unix)]
mod unix {
    use std::fs::{File, OpenOptions};
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
    use std::os::unix::io::AsRawFd;
    use std::path::{Path, PathBuf};

    use super::BindError;

    pub type Stream = tokio::net::UnixStream;

    pub struct Listener {
        inner: tokio::net::UnixListener,
        path: PathBuf,
        /// Never re-read: its closure at `drop` releases the `flock`.
        _lock: File,
    }

    /// Binds the channel and takes this user's exclusivity.
    pub fn bind(path: &Path) -> Result<Listener, BindError> {
        // The folder is normally the Core's runtime folder, already there. When
        // it is not (a manager started before any Core), create it 0700 — but
        // never *change* the mode of a folder that already exists: it is shared
        // with the Core's own socket and not ours to re-permission.
        if let Some(parent) = path.parent()
            && !parent.exists()
        {
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(parent)?;
        }
        // Exclusion BEFORE touching the socket: without it a second manager
        // would unlink the first's socket, leaving it listening on an inode
        // nobody can reach.
        let lock = acquire_lock(&lock_path(path))?;
        match std::fs::remove_file(path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
        let inner = tokio::net::UnixListener::bind(path)?;
        // A belt on top of the private folder. (macOS ignores a socket file's
        // permissions — there the folder, and above all the peer check, is what
        // protects.)
        std::fs::set_permissions(path, PermissionsExt::from_mode(0o600))?;
        Ok(Listener {
            inner,
            path: path.to_path_buf(),
            _lock: lock,
        })
    }

    fn lock_path(socket: &Path) -> PathBuf {
        let name = socket.file_name().unwrap_or_default().to_string_lossy();
        socket.with_file_name(format!("{name}.lock"))
    }

    /// An advisory non-blocking `flock`, released by the kernel even on a
    /// `kill -9`: nothing to clean up at startup.
    fn acquire_lock(path: &Path) -> Result<File, BindError> {
        let lock = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .mode(0o600)
            .open(path)?;
        // SAFETY: `lock` owns a valid descriptor for the duration of the call.
        if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            let err = std::io::Error::last_os_error();
            return match err.kind() {
                std::io::ErrorKind::WouldBlock => Err(BindError::AlreadyRunning),
                _ => Err(BindError::Io(err)),
            };
        }
        Ok(lock)
    }

    impl Listener {
        pub async fn accept(&mut self) -> std::io::Result<Stream> {
            loop {
                let (stream, _addr) = self.inner.accept().await?;
                // Level 1: another account on the machine has no business here,
                // whatever the path's permissions say — and on macOS the socket
                // file's mode is not even honored.
                if stream.peer_cred()?.uid() != unsafe { libc::getuid() } {
                    continue;
                }
                return Ok(stream);
            }
        }
    }

    impl Drop for Listener {
        fn drop(&mut self) {
            // Leave nothing dangling on a graceful stop. A leftover socket is
            // harmless (the next bind unlinks it under the lock), but a courier
            // then fails on connect instead of on a refused reply.
            let _ = std::fs::remove_file(&self.path);
        }
    }

    /// No retry needed here, unlike Windows: a listening socket queues an
    /// incoming connection in its backlog, so there is no "all instances busy".
    pub async fn connect(path: &Path) -> std::io::Result<Stream> {
        tokio::net::UnixStream::connect(path).await
    }
}

#[cfg(windows)]
mod windows {
    use std::ffi::c_void;
    use std::path::Path;

    use tokio::net::windows::named_pipe::{ClientOptions, NamedPipeServer, ServerOptions};
    use windows_sys::Win32::Foundation::{CloseHandle, ERROR_PIPE_BUSY, HANDLE, LocalFree};
    use windows_sys::Win32::Security::Authorization::{
        ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
        SDDL_REVISION_1,
    };
    use windows_sys::Win32::Security::{
        GetTokenInformation, SECURITY_ATTRIBUTES, TOKEN_QUERY, TOKEN_USER, TokenUser,
    };
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    use super::BindError;

    pub type Stream = NamedPipeServer;

    pub struct Listener {
        path: String,
        descriptor: OwnedSecurityDescriptor,
        /// Instance waiting for the next client, always created BEFORE the
        /// previous one is handed out: a courier never finds the name without a
        /// listening instance.
        next: NamedPipeServer,
    }

    pub fn bind(path: &Path) -> Result<Listener, BindError> {
        let path = path
            .to_str()
            .ok_or_else(|| std::io::Error::other("non-UTF-8 pipe name"))?
            .to_string();
        let descriptor = owner_only_descriptor()?;
        // first_pipe_instance: fails if the name exists — nobody can squat it
        // with their own DACL, and a second manager cannot slip in behind the
        // first. This is Windows' exclusivity; there is no lock to take.
        let next = create_instance(&path, &descriptor, true).map_err(|e| {
            match e.raw_os_error() {
                // ERROR_ACCESS_DENIED: the name exists.
                Some(5) => BindError::AlreadyRunning,
                _ => BindError::Io(e),
            }
        })?;
        Ok(Listener {
            path,
            descriptor,
            next,
        })
    }

    impl Listener {
        pub async fn accept(&mut self) -> std::io::Result<Stream> {
            self.next.connect().await?;
            let replacement = create_instance(&self.path, &self.descriptor, false)?;
            let stream = std::mem::replace(&mut self.next, replacement);
            Ok(stream)
        }
    }

    fn create_instance(
        path: &str,
        descriptor: &OwnedSecurityDescriptor,
        first: bool,
    ) -> std::io::Result<NamedPipeServer> {
        let mut attrs = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: descriptor.0,
            bInheritHandle: 0,
        };
        // SAFETY: the pointer comes from a valid SECURITY_DESCRIPTOR owned by
        // `descriptor`, alive for the duration of the call.
        unsafe {
            ServerOptions::new()
                .first_pipe_instance(first)
                .create_with_security_attributes_raw(
                    path,
                    &mut attrs as *mut SECURITY_ATTRIBUTES as *mut c_void,
                )
        }
    }

    /// A SECURITY_DESCRIPTOR allocated by the platform (LocalFree on drop).
    pub struct OwnedSecurityDescriptor(*mut c_void);

    // Inert data: moving it between threads is safe.
    unsafe impl Send for OwnedSecurityDescriptor {}
    unsafe impl Sync for OwnedSecurityDescriptor {}

    impl Drop for OwnedSecurityDescriptor {
        fn drop(&mut self) {
            // SAFETY: allocated by ConvertStringSecurityDescriptorToSecurityDescriptorW.
            unsafe { LocalFree(self.0) };
        }
    }

    /// A "current user and nobody else" DACL, protected against inheritance.
    /// Without it a pipe's default DACL grants read to Everyone.
    fn owner_only_descriptor() -> std::io::Result<OwnedSecurityDescriptor> {
        let sid = current_user_sid()?;
        let sddl = format!("D:P(A;;GA;;;{sid})");
        let sddl_utf16: Vec<u16> = sddl.encode_utf16().chain(std::iter::once(0)).collect();
        let mut psd: *mut c_void = std::ptr::null_mut();
        // SAFETY: NUL-terminated UTF-16 string, valid output pointer.
        let ok = unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                sddl_utf16.as_ptr(),
                SDDL_REVISION_1,
                &mut psd as *mut *mut c_void as _,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(OwnedSecurityDescriptor(psd))
    }

    /// The process user's SID as a string ("S-1-5-21-…").
    fn current_user_sid() -> std::io::Result<String> {
        // SAFETY: canonical Win32 sequence — the current process's token,
        // queried in two steps (size then data), closed afterwards.
        unsafe {
            let mut token: HANDLE = std::ptr::null_mut();
            if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
                return Err(std::io::Error::last_os_error());
            }
            let mut len: u32 = 0;
            GetTokenInformation(token, TokenUser, std::ptr::null_mut(), 0, &mut len);
            let mut buf = vec![0u8; len as usize];
            let ok = GetTokenInformation(
                token,
                TokenUser,
                buf.as_mut_ptr() as *mut c_void,
                len,
                &mut len,
            );
            CloseHandle(token);
            if ok == 0 {
                return Err(std::io::Error::last_os_error());
            }
            let user = &*(buf.as_ptr() as *const TOKEN_USER);
            let mut psz: *mut u16 = std::ptr::null_mut();
            if ConvertSidToStringSidW(user.User.Sid, &mut psz) == 0 {
                return Err(std::io::Error::last_os_error());
            }
            let mut end = psz;
            while *end != 0 {
                end = end.add(1);
            }
            let sid = String::from_utf16_lossy(std::slice::from_raw_parts(
                psz,
                end.offset_from(psz) as usize,
            ));
            LocalFree(psz as *mut c_void);
            Ok(sid)
        }
    }

    /// Connects, waiting out `ERROR_PIPE_BUSY` — see
    /// [`CONNECT_RETRY_BUDGET`](super::CONNECT_RETRY_BUDGET). Without this a
    /// courier that arrives while the single listening instance is being replaced
    /// reports "no manager" and the click is silently lost, which is exactly the
    /// burst the Windows classic menu produces.
    pub async fn connect(
        path: &Path,
    ) -> std::io::Result<tokio::net::windows::named_pipe::NamedPipeClient> {
        let path = path
            .to_str()
            .ok_or_else(|| std::io::Error::other("non-UTF-8 pipe name"))?;
        let deadline = tokio::time::Instant::now() + super::CONNECT_RETRY_BUDGET;
        loop {
            match ClientOptions::new().open(path) {
                Ok(client) => return Ok(client),
                Err(e)
                    if e.raw_os_error() == Some(ERROR_PIPE_BUSY as i32)
                        && tokio::time::Instant::now() < deadline =>
                {
                    tokio::time::sleep(super::CONNECT_RETRY_PAUSE).await;
                }
                Err(e) => return Err(e),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_send_request_round_trips() {
        let req = Request::Send {
            device_id: "d_7f3a".into(),
            paths: vec![absolute("a.txt"), absolute("b b.txt")],
        };
        assert_eq!(Request::parse(&req.to_line()), Ok(req));
    }

    #[test]
    fn a_targets_request_round_trips() {
        assert_eq!(
            Request::parse(&Request::Targets.to_line()),
            Ok(Request::Targets)
        );
    }

    #[test]
    fn a_foreign_protocol_version_is_refused_not_guessed() {
        let line = json!({ "v": 2, "kind": "targets" }).to_string();
        assert_eq!(Request::parse(&line), Err(error::UNSUPPORTED_VERSION));
        // No version at all is simply malformed.
        let line = json!({ "kind": "targets" }).to_string();
        assert_eq!(Request::parse(&line), Err(error::BAD_REQUEST));
    }

    #[test]
    fn malformed_send_requests_are_refused() {
        let cases = [
            (json!({ "v": 1, "kind": "nope" }), error::BAD_REQUEST),
            (json!({ "v": 1 }), error::BAD_REQUEST),
            // No device.
            (
                json!({ "v": 1, "kind": "send", "paths": ["/a"] }),
                error::BAD_REQUEST,
            ),
            (
                json!({ "v": 1, "kind": "send", "device_id": "  ", "paths": ["/a"] }),
                error::BAD_REQUEST,
            ),
            // No paths, or not a list.
            (
                json!({ "v": 1, "kind": "send", "device_id": "d_1" }),
                error::BAD_REQUEST,
            ),
            (
                json!({ "v": 1, "kind": "send", "device_id": "d_1", "paths": [] }),
                error::NO_PATHS,
            ),
            (
                json!({ "v": 1, "kind": "send", "device_id": "d_1", "paths": [7] }),
                error::BAD_REQUEST,
            ),
            (
                json!({ "v": 1, "kind": "send", "device_id": "d_1", "paths": [""] }),
                error::BAD_REQUEST,
            ),
        ];
        for (line, expected) in cases {
            assert_eq!(
                Request::parse(&line.to_string()),
                Err(expected),
                "for {line}"
            );
        }
        assert_eq!(Request::parse("not json"), Err(error::BAD_REQUEST));
    }

    #[test]
    fn a_relative_path_is_refused_rather_than_resolved() {
        // The Core's working directory is not the file manager's: a relative
        // path would silently mean a different file.
        let line = json!({ "v": 1, "kind": "send", "device_id": "d_1", "paths": ["notes.txt"] })
            .to_string();
        assert_eq!(Request::parse(&line), Err(error::RELATIVE_PATH));
        // One bad path in a good batch refuses the whole batch.
        let good = absolute("a.txt").to_string_lossy().into_owned();
        let line =
            json!({ "v": 1, "kind": "send", "device_id": "d_1", "paths": [good, "../b.txt"] })
                .to_string();
        assert_eq!(Request::parse(&line), Err(error::RELATIVE_PATH));
    }

    #[test]
    fn responses_round_trip() {
        let accepted = Response::Accepted {
            transfer_id: "t_1a2b".into(),
        };
        assert_eq!(Response::parse(&accepted.to_line()), Ok(accepted));

        let targets = Response::Targets(vec![Target {
            device_id: "d_1".into(),
            name: "PC-B".into(),
            platform: "linux".into(),
        }]);
        assert_eq!(Response::parse(&targets.to_line()), Ok(targets));

        // An empty target list is a valid answer, not an absent payload.
        let empty = Response::Targets(vec![]);
        assert_eq!(Response::parse(&empty.to_line()), Ok(empty));

        let failed = Response::failed(error::NO_SUCH_TARGET);
        assert_eq!(Response::parse(&failed.to_line()), Ok(failed));
    }

    #[test]
    fn a_reply_without_a_payload_is_an_error_not_an_empty_list() {
        let line = json!({ "ok": true }).to_string();
        assert!(Response::parse(&line).is_err());
    }

    /// An absolute path on whichever platform the test runs.
    fn absolute(name: &str) -> PathBuf {
        if cfg!(windows) {
            PathBuf::from(format!(r"C:\tmp\{name}"))
        } else {
            PathBuf::from(format!("/tmp/{name}"))
        }
    }
}
