// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

//! Per-platform IPC listening point: UDS (unix) or named pipe (windows), with
//! security level 1 (doc/architecture.md): the surface is open only to the
//! current user — file permissions + peer credential verification on the unix
//! side, a DACL restricted to their SID on the windows side.

use std::path::Path;

/// What the peer credentials tell us about the peer — "binary, pid"
/// (doc/core-api.md), best-effort depending on the platform. Broadcast in
/// `component.pending` to inform the approval: `name` and `role` are
/// self-declared, the binary is the only datum that is not.
#[derive(Clone, Debug)]
pub struct PeerInfo {
    pub pid: Option<u32>,
    /// Path to the peer's binary, resolved from its pid.
    pub exe: Option<String>,
}

impl PeerInfo {
    pub fn record(&self) -> serde_json::Value {
        let mut v = serde_json::json!({});
        if let Some(pid) = self.pid {
            v["pid"] = pid.into();
        }
        if let Some(exe) = &self.exe {
            v["exe"] = exe.clone().into();
        }
        v
    }
}

#[cfg(unix)]
pub use unix::{Listener, Stream};
#[cfg(windows)]
pub use windows::{Listener, Stream};

/// Proof that we are the only Core of this user, to be held as long as we
/// listen. It lives in the `CoreHandle` and not in the `Listener`: the latter
/// is owned by the accept task, which we only `abort()` — but `abort()` is
/// cooperative, so the `Listener` can outlive the Core's shutdown by a few
/// moments. An immediate restart on the same socket must nonetheless reclaim
/// the lock right away.
#[cfg(unix)]
#[derive(Debug)]
pub struct InstanceGuard {
    /// Never re-read: it is its closure, at `drop`, that releases the `flock`.
    _lock: std::fs::File,
}

/// Windows has no lock to hold: uniqueness is carried by the first instance of
/// the named pipe, in the `Listener` itself.
#[cfg(windows)]
#[derive(Debug)]
pub struct InstanceGuard;

/// Why listening failed. `AlreadyRunning` is the only case the caller must
/// distinguish: it is not a failure, it is a Core already in place.
#[derive(Debug)]
pub enum BindError {
    /// A Core already holds this user's listening point.
    AlreadyRunning,
    Io(std::io::Error),
}

impl std::fmt::Display for BindError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BindError::AlreadyRunning => write!(f, "a Core is already listening for this user"),
            BindError::Io(e) => write!(f, "IPC listening failed: {e}"),
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
mod unix {
    use std::fs::{File, OpenOptions};
    use std::os::unix::fs::OpenOptionsExt;
    use std::os::unix::io::AsRawFd;
    use std::path::{Path, PathBuf};

    use super::{BindError, InstanceGuard, PeerInfo};

    pub type Stream = tokio::net::UnixStream;

    pub struct Listener {
        inner: tokio::net::UnixListener,
    }

    pub fn bind(path: &Path) -> Result<(Listener, InstanceGuard), BindError> {
        // The socket's folder (runtime dir) is not necessarily the config's:
        // no one else has created it.
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        // Mutual exclusion BEFORE touching the socket. Without it, a second
        // Core would unlink the first's socket — which stayed alive, listening
        // on an inode no one could reach anymore.
        let lock = acquire_lock(&lock_path(path))?;
        // The lock is ours: a residual socket can only be that of a dead Core.
        // Removing it is now safe.
        match std::fs::remove_file(path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
        let inner = tokio::net::UnixListener::bind(path)?;
        // A belt on top of the private folder. (macOS ignores the socket file's
        // permissions: there, it is the folder that protects.)
        let perms = std::os::unix::fs::PermissionsExt::from_mode(0o600);
        std::fs::set_permissions(path, perms)?;
        Ok((Listener { inner }, InstanceGuard { _lock: lock }))
    }

    /// `core.sock` → `core.sock.lock`. A suffix rather than an extension
    /// replacement: the socket's name does not necessarily have one.
    fn lock_path(socket: &Path) -> PathBuf {
        let name = socket.file_name().unwrap_or_default().to_string_lossy();
        socket.with_file_name(format!("{name}.lock"))
    }

    /// An advisory, non-blocking `flock`. Two `open()` of the same file give
    /// two open file descriptions: the lock therefore bites even between two
    /// Cores of the SAME process (unlike `fcntl`, which is per-process) — this
    /// is what makes it testable. The kernel releases it when the process dies,
    /// even by `kill -9`: nothing to clean up at startup. `File` is opened
    /// O_CLOEXEC by the std: a component exec'd by the supervisor does not
    /// inherit the descriptor, so it does not keep the lock after our death.
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
        pub async fn accept(&mut self) -> std::io::Result<(Stream, PeerInfo)> {
            loop {
                let (stream, _addr) = self.inner.accept().await?;
                let cred = stream.peer_cred()?;
                // Level 1: another account on the machine has no business here,
                // whatever the path's permissions.
                if cred.uid() != unsafe { libc::getuid() } {
                    continue;
                }
                let pid = peer_pid(&cred);
                let exe = pid.and_then(peer_exe);
                return Ok((stream, PeerInfo { pid, exe }));
            }
        }
    }

    #[cfg(target_os = "linux")]
    fn peer_pid(cred: &tokio::net::unix::UCred) -> Option<u32> {
        cred.pid().and_then(|p| u32::try_from(p).ok())
    }

    #[cfg(not(target_os = "linux"))]
    fn peer_pid(_cred: &tokio::net::unix::UCred) -> Option<u32> {
        None
    }

    #[cfg(target_os = "linux")]
    fn peer_exe(pid: u32) -> Option<String> {
        std::fs::read_link(format!("/proc/{pid}/exe"))
            .ok()
            .map(|p| p.to_string_lossy().into_owned())
    }

    #[cfg(not(target_os = "linux"))]
    fn peer_exe(_pid: u32) -> Option<String> {
        None
    }
}

#[cfg(windows)]
mod windows {
    use std::ffi::c_void;
    use std::os::windows::io::AsRawHandle;
    use std::path::Path;

    use tokio::net::windows::named_pipe::{NamedPipeServer, ServerOptions};
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, LocalFree};
    use windows_sys::Win32::Security::Authorization::{
        ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
        SDDL_REVISION_1,
    };
    use windows_sys::Win32::Security::{
        GetTokenInformation, SECURITY_ATTRIBUTES, TOKEN_QUERY, TOKEN_USER, TokenUser,
    };
    use windows_sys::Win32::System::Pipes::GetNamedPipeClientProcessId;
    use windows_sys::Win32::System::Threading::{
        GetCurrentProcess, OpenProcess, OpenProcessToken, PROCESS_QUERY_LIMITED_INFORMATION,
        QueryFullProcessImageNameW,
    };

    use super::{BindError, InstanceGuard, PeerInfo};

    pub type Stream = NamedPipeServer;

    pub struct Listener {
        path: String,
        descriptor: OwnedSecurityDescriptor,
        /// Instance waiting for the next client. Always created BEFORE handing
        /// back the previous one: a client never finds a name without a
        /// listening instance.
        next: NamedPipeServer,
    }

    pub fn bind(path: &Path) -> Result<(Listener, InstanceGuard), BindError> {
        let path = path
            .to_str()
            .ok_or_else(|| std::io::Error::other("non-UTF-8 pipe name"))?
            .to_string();
        let descriptor = owner_only_descriptor()?;
        // first_pipe_instance: fails if the name already exists — no one could
        // squat the name with their own DACL, and a second Core cannot slip in
        // behind the first. This is Windows' uniqueness instance: there is no
        // lock to set.
        let next = create_instance(&path, &descriptor, true).map_err(|e| {
            // ERROR_ACCESS_DENIED (5): the name exists. The only other cause
            // would be another user having squatted the name with their own
            // DACL — outside threat-model level 1, and the remedy (refusing to
            // start) is the same.
            match e.raw_os_error() {
                Some(5) => BindError::AlreadyRunning,
                _ => BindError::Io(e),
            }
        })?;
        Ok((
            Listener {
                path,
                descriptor,
                next,
            },
            InstanceGuard,
        ))
    }

    impl Listener {
        pub async fn accept(&mut self) -> std::io::Result<(Stream, PeerInfo)> {
            self.next.connect().await?;
            let replacement = create_instance(&self.path, &self.descriptor, false)?;
            let stream = std::mem::replace(&mut self.next, replacement);
            let pid = client_pid(&stream);
            let exe = pid.and_then(process_exe);
            Ok((stream, PeerInfo { pid, exe }))
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
        // SAFETY: the pointer comes from a valid SECURITY_DESCRIPTOR, owned by
        // `descriptor`, alive during the call.
        unsafe {
            ServerOptions::new()
                .first_pipe_instance(first)
                .create_with_security_attributes_raw(
                    path,
                    &mut attrs as *mut SECURITY_ATTRIBUTES as *mut c_void,
                )
        }
    }

    fn client_pid(stream: &NamedPipeServer) -> Option<u32> {
        let mut pid: u32 = 0;
        // SAFETY: valid handle (borrowed from the stream), valid output pointer.
        let ok = unsafe { GetNamedPipeClientProcessId(stream.as_raw_handle() as HANDLE, &mut pid) };
        (ok != 0).then_some(pid)
    }

    /// Path to a process's binary, best-effort (same user: the pipe's DACL
    /// guarantees the peer is ours).
    fn process_exe(pid: u32) -> Option<String> {
        // SAFETY: handle opened then closed right here; the buffer and its size
        // are consistent.
        unsafe {
            let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
            if handle.is_null() {
                return None;
            }
            let mut buf = [0u16; 1024];
            let mut len = buf.len() as u32;
            let ok = QueryFullProcessImageNameW(handle, 0, buf.as_mut_ptr(), &mut len);
            CloseHandle(handle);
            (ok != 0).then(|| String::from_utf16_lossy(&buf[..len as usize]))
        }
    }

    /// A SECURITY_DESCRIPTOR allocated by the platform (LocalFree on drop).
    pub struct OwnedSecurityDescriptor(*mut c_void);

    // The descriptor is inert data: moving it between threads is safe.
    unsafe impl Send for OwnedSecurityDescriptor {}

    impl Drop for OwnedSecurityDescriptor {
        fn drop(&mut self) {
            // SAFETY: allocated by ConvertStringSecurityDescriptorToSecurityDescriptorW.
            unsafe { LocalFree(self.0) };
        }
    }

    /// A "current user, and no one else" DACL (SDDL, protected against
    /// inheritance). Without it, a pipe's default DACL grants read to Everyone.
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

    /// The process user's SID, as a string ("S-1-5-21-…").
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
}

/// Opens the IPC listening point and takes the user's exclusivity.
pub fn bind(path: &Path) -> Result<(Listener, InstanceGuard), BindError> {
    #[cfg(unix)]
    return unix::bind(path);
    #[cfg(windows)]
    return windows::bind(path);
}
