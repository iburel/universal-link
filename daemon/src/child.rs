// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

//! The supervisor's OS plumbing: spawn a component in a way that lets us kill
//! it ENTIRELY, it and its descendants. A contextual-menu backend spawns
//! shims; leaving them behind would be a process leak.
//!
//! - unix: the child becomes the leader of its own group (`process_group(0)`),
//!   and we signal the GROUP (`kill(-pgid, …)`). Accepted side effect: the
//!   terminal's Ctrl-C no longer reaches the children, it is the supervisor
//!   that relays the shutdown.
//! - windows: the child is assigned to a Job Object marked
//!   `KILL_ON_JOB_CLOSE`. Closing the job handle kills the tree. The job is
//!   therefore kept alive as long as the child is.
//!
//! Windows has no equivalent of SIGTERM. Graceful shutdown therefore goes
//! through a portable channel: **closing the child's standard input**. The
//! supervisor keeps it open; its EOF means "stop".

use std::process::Stdio;

use tokio::process::{Child, Command};

/// What we keep about a spawned child so we can kill it cleanly.
pub struct Handle {
    pub child: Child,
    /// Kept open: closing it is the graceful-shutdown request.
    pub stdin: Option<tokio::process::ChildStdin>,
    /// Captured at spawn: after `wait()`, `child.id()` no longer tells us
    /// anything, and that is exactly when we need to sweep the descendants.
    #[cfg(unix)]
    pgid: i32,
    #[cfg(windows)]
    _job: windows_impl::Job,
}

pub fn spawn(
    program: &std::path::Path,
    args: &[String],
    envs: &[(&str, String)],
) -> std::io::Result<Handle> {
    let mut command = Command::new(program);
    command
        .args(args)
        .stdin(Stdio::piped())
        // The child's output goes where ours goes: the supervisor does not
        // appoint itself a log collector.
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .kill_on_drop(true);
    for (key, value) in envs {
        command.env(key, value);
    }
    platform::configure(&mut command);
    let mut child = command.spawn()?;
    let stdin = child.stdin.take();
    platform::adopt(child, stdin)
}

/// Sends the graceful-shutdown request (EOF on standard input) then, if the
/// child lingers, escalates. Returns once the child is reaped and its
/// descendants swept.
pub async fn stop(handle: &mut Handle, grace: std::time::Duration) {
    handle.stdin.take();
    if wait_for(&mut handle.child, grace).await {
        sweep(handle);
        return;
    }
    platform::terminate(handle);
    if wait_for(&mut handle.child, grace).await {
        sweep(handle);
        return;
    }
    platform::kill(handle);
    let _ = handle.child.wait().await;
    sweep(handle);
}

/// The child left on its own (crash, normal exit): its descendants, however,
/// may well survive. We sweep them before restarting.
pub fn sweep(handle: &Handle) {
    platform::sweep(handle);
}

async fn wait_for(child: &mut Child, grace: std::time::Duration) -> bool {
    tokio::time::timeout(grace, child.wait()).await.is_ok()
}

#[cfg(unix)]
use unix_impl as platform;
#[cfg(windows)]
use windows_impl as platform;

#[cfg(unix)]
mod unix_impl {
    use super::Handle;

    pub fn configure(command: &mut tokio::process::Command) {
        // pgid == the child's pid: it becomes the group leader, and its
        // descendants are born into the group with it.
        command.process_group(0);
    }

    pub fn adopt(
        child: tokio::process::Child,
        stdin: Option<tokio::process::ChildStdin>,
    ) -> std::io::Result<Handle> {
        let pgid = child
            .id()
            .ok_or_else(|| std::io::Error::other("child already reaped"))?
            as i32;
        Ok(Handle { child, stdin, pgid })
    }

    pub fn terminate(handle: &mut Handle) {
        signal_group(handle.pgid, libc::SIGTERM);
    }

    pub fn kill(handle: &mut Handle) {
        signal_group(handle.pgid, libc::SIGKILL);
    }

    /// After the group leader dies, its children remain orphaned but alive.
    /// The group survives as long as it has a member, and its number cannot be
    /// reassigned during that time: the sweep aims true.
    pub fn sweep(handle: &Handle) {
        signal_group(handle.pgid, libc::SIGKILL);
    }

    fn signal_group(pgid: i32, signal: i32) {
        if pgid <= 1 {
            // `kill(-1, …)` would hit all of our processes. Never.
            return;
        }
        // SAFETY: `kill` on a nonexistent group returns ESRCH, with no effect.
        unsafe { libc::kill(-pgid, signal) };
    }
}

#[cfg(windows)]
mod windows_impl {
    use std::ffi::c_void;

    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
        SetInformationJobObject,
    };

    use super::Handle;

    /// As long as this handle lives, the job lives; once it is closed,
    /// everything inside it dies (`KILL_ON_JOB_CLOSE`).
    pub struct Job(HANDLE);

    // SAFETY: a job HANDLE is usable from any thread.
    unsafe impl Send for Job {}
    unsafe impl Sync for Job {}

    impl Drop for Job {
        fn drop(&mut self) {
            if !self.0.is_null() {
                // SAFETY: owned handle, closed exactly once.
                unsafe { CloseHandle(self.0) };
            }
        }
    }

    pub fn configure(_command: &mut tokio::process::Command) {}

    pub fn adopt(
        child: tokio::process::Child,
        stdin: Option<tokio::process::ChildStdin>,
    ) -> std::io::Result<Handle> {
        let job = create_job()?;
        let raw = child
            .raw_handle()
            .ok_or_else(|| std::io::Error::other("child already reaped"))?;
        // Window between `spawn` and the assignment: a grandchild born there
        // would escape the job. It lasts a few microseconds, and closing the
        // window would require CREATE_SUSPENDED, which tokio does not expose.
        // SAFETY: valid handles, owned respectively by `job` and `child`.
        if unsafe { AssignProcessToJobObject(job.0, raw as HANDLE) } == 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(Handle {
            child,
            stdin,
            _job: job,
        })
    }

    fn create_job() -> std::io::Result<Job> {
        // SAFETY: anonymous job, default attributes.
        let handle = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
        if handle.is_null() {
            return Err(std::io::Error::last_os_error());
        }
        let job = Job(handle);
        // SAFETY: fully initialized struct, exact size.
        let ok = unsafe {
            let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            SetInformationJobObject(
                job.0,
                JobObjectExtendedLimitInformation,
                std::ptr::addr_of!(info) as *const c_void,
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
        };
        if ok == 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(job)
    }

    /// Windows has no shutdown signal to send to a console-less process: EOF
    /// on standard input was the only polite request.
    pub fn terminate(_handle: &mut Handle) {}

    pub fn kill(handle: &mut Handle) {
        // TerminateProcess on the child alone; its descendants will die when
        // the job is closed.
        let _ = handle.child.start_kill();
    }

    /// The job dies with the `Handle`, taking the tree down with it. Nothing
    /// to do here.
    pub fn sweep(_handle: &Handle) {}
}
