// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

//! The on-demand FUSE filesystem that serves a remote FILES clip (Linux).
//!
//! On X11 there is no "files are being pasted" event: the file manager reads
//! `file://` URIs from the clipboard and then reads those files itself. So a
//! promised remote clip is exposed as a real POSIX tree mounted in user space;
//! the X11 backend publishes `file://` URIs pointing into that mount. Each
//! kernel `read()` is the paste-time trigger: it pulls exactly that byte range
//! from the source through the [`FileFetcher`] seam — pull-at-paste, never an
//! eager download at offer time, nothing spilled to disk.
//!
//! Invariants:
//! - The manifest is frozen while offered (a new clip = a new mount), so the
//!   tree and the returned attributes are immutable and a generous `getattr` TTL
//!   is safe.
//! - No silent truncation: a failed pull surfaces as `EIO` at the syscall, so a
//!   copy fails cleanly rather than producing a truncated file. [`FileFetcher`]
//!   returns fewer bytes than asked only at genuine EOF.
//!
//! Unprivileged, no C link: `fuser` is built without its `libfuse` feature, so
//! its build script selects the pure-Rust mount path and mounts through the
//! setuid `fusermount3`/`fusermount` helper. Unmount is lazy (`MNT_DETACH`):
//! dropping [`FuseMount`] detaches at once even with a `read()` in flight, so it
//! never stalls the X11 thread.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime};

use fuser::{
    BackgroundSession, Config, Errno, FileAttr, FileHandle, FileType, Filesystem, FopenFlags,
    Generation, INodeNo, LockOwner, MountOption, OpenFlags, ReplyAttr, ReplyData, ReplyDirectory,
    ReplyEntry, ReplyOpen, Request,
};

use crate::backend::{FileFetcher, RemoteFile};
use crate::files::{self, FileTree, NodeKind};

/// The FUSE root inode must be what the tree calls its root. `fuser` no longer
/// exports `FUSE_ROOT_ID`; the same 1 is now `INodeNo::ROOT`, whose field is
/// public and therefore still readable in a const assert.
const _: () = assert!(files::ROOT_INO == INodeNo::ROOT.0);

/// Attribute/entry cache lifetime handed to the kernel. The clip is immutable
/// while mounted, so a generous TTL avoids repeated `getattr` with no risk of
/// staleness.
const TTL: Duration = Duration::from_secs(1);

/// Probe (non-destructive): is unprivileged FUSE usable here? True iff
/// `/dev/fuse` opens read/write AND a `fusermount3`/`fusermount` helper exists on
/// some `$PATH` directory. No probe mount.
pub fn fuse_available() -> bool {
    if std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/fuse")
        .is_err()
    {
        return false;
    }
    std::env::var_os("PATH")
        .map(|paths| {
            std::env::split_paths(&paths)
                .any(|d| d.join("fusermount3").exists() || d.join("fusermount").exists())
        })
        .unwrap_or(false)
}

/// A live FUSE mount of a remote FILES clip. Its lifetime is the offer's: the
/// X11 backend holds it while the clip is promised and drops it to withdraw.
/// Dropping unmounts (lazily if busy) and removes the temporary mountpoint.
pub struct FuseMount {
    /// The background session; its `Drop` unmounts (`umount2(MNT_DETACH)`).
    session: Option<BackgroundSession>,
    mountpoint: PathBuf,
    /// Absolute paths of the top-level elements (`<mountpoint>/<root>`), to
    /// publish as `file://` URIs at paste time.
    root_paths: Vec<PathBuf>,
}

impl FuseMount {
    /// Mounts a FUSE filesystem exposing `files`, served on demand by `fetcher`.
    /// `Err` if the manifest yields no usable root (empty or all-malformed), if
    /// the mountpoint cannot be created, or if the mount itself fails.
    ///
    /// BLOCKS the calling thread for one kernel round-trip: `fuser` now performs
    /// the `FUSE_INIT` handshake inside the mount call rather than on the
    /// background thread, so a handshake failure surfaces here as `Err` instead
    /// of dying silently after we have already returned success.
    pub fn mount(
        files: &[RemoteFile],
        fetcher: Arc<dyn FileFetcher>,
    ) -> std::io::Result<FuseMount> {
        let tree = FileTree::build(files);
        if tree.roots().is_empty() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "files manifest yields no usable root",
            ));
        }
        let root_names: Vec<String> = tree.roots().to_vec();
        let mountpoint = unique_mount_dir()?;
        let fs = ClipboardFs::new(tree, fetcher);
        // RO + hardening: the tree comes from a remote peer, so no setuid/dev/exec
        // and no atime writes. The other three `Config` fields stay at their
        // defaults, and each default is load-bearing:
        // - `acl: SessionACL::Owner` — only the mounting user (the one pasting)
        //   may traverse the tree. This replaces the `AllowOther`/`AllowRoot`
        //   mount options, which no longer exist as `MountOption` variants; the
        //   default is the restrictive end, so omitting it is the safe choice.
        // - `n_threads: None` — read as 1, i.e. ONE dispatch loop, which is what
        //   makes the `&self` callbacks below serial (see `ClipboardFs`).
        // - `clone_fd: false` — a single `/dev/fuse` fd, as before.
        // `Config` is `#[non_exhaustive]`, so it cannot be built with a struct
        // literal — not even with `..Default::default()`, which is why the
        // default is mutated in place inside a block instead.
        let config = {
            let mut config = Config::default();
            config.mount_options = vec![
                MountOption::FSName("universallink".to_string()),
                MountOption::Subtype("universallink-clip".to_string()),
                MountOption::RO,
                MountOption::NoSuid,
                MountOption::NoDev,
                MountOption::NoExec,
                MountOption::NoAtime,
            ];
            config
        };
        let session = match fuser::spawn_mount(fs, &mountpoint, &config) {
            Ok(session) => session,
            Err(e) => {
                let _ = std::fs::remove_dir(&mountpoint);
                return Err(e);
            }
        };
        let root_paths = root_names.iter().map(|r| mountpoint.join(r)).collect();
        Ok(FuseMount {
            session: Some(session),
            mountpoint,
            root_paths,
        })
    }

    /// The absolute top-level paths to publish as `file://` URIs (one per root).
    pub fn root_paths(&self) -> &[PathBuf] {
        &self.root_paths
    }
}

impl Drop for FuseMount {
    fn drop(&mut self) {
        // Unmount (lazily if busy — never blocks this thread even with a read in
        // flight), then remove the now-empty mountpoint. Try once immediately
        // (the common case: no read in flight → instant unmount). If it is still
        // busy, hand the removal to a detached thread so the caller (the X11
        // event loop) is never stalled while the kernel finalizes the lazy
        // unmount. Dropping the session drops both the mount and the join
        // handle; the handle DETACHES rather than joins, and the unmount is a
        // plain `umount2(MNT_DETACH)` — reached because an unprivileged
        // `umount(2)` always answers EPERM — so neither half waits on the event
        // loop or on a child process. (Only if `MNT_DETACH` itself failed would
        // `fuser` fall back to fork/exec'ing the setuid helper and waiting for
        // it, which is why this says "does not block" and not "is a syscall".)
        // Nothing HERE panics; `fuser`'s unmount does probe the device with
        // `poll(2)` and panic if that syscall fails, but that is neither new in
        // 0.18 nor reachable except on ENOMEM — process-death territory a
        // `catch_unwind` here would not meaningfully improve.
        drop(self.session.take());
        let mp = std::mem::take(&mut self.mountpoint);
        if std::fs::remove_dir(&mp).is_err() {
            std::thread::spawn(move || {
                for _ in 0..50 {
                    std::thread::sleep(Duration::from_millis(20));
                    if std::fs::remove_dir(&mp).is_ok() {
                        return;
                    }
                }
                warn(&format!("FUSE mountpoint not removed: {}", mp.display()));
            });
        }
    }
}

/// Creates a unique mountpoint directory (pid + counter), mode 0700, under
/// `$XDG_RUNTIME_DIR` if it is a directory, else [`std::env::temp_dir`].
fn unique_mount_dir() -> std::io::Result<PathBuf> {
    use std::os::unix::fs::DirBuilderExt;
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let base = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .filter(|p| p.is_dir())
        .unwrap_or_else(std::env::temp_dir);
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    let dir = base.join(format!("universallink-clip-{}-{}", std::process::id(), n));
    std::fs::DirBuilder::new().mode(0o700).create(&dir)?;
    Ok(dir)
}

/// The FUSE filesystem of one clip. Reads are stateless (keyed by `file_id`),
/// so a file handle can equal the inode.
///
/// Concurrency: the callbacks take `&self` and `Filesystem` requires
/// `Send + Sync + 'static`, so the type system NO LONGER proves exclusive
/// access — it did while they took `&mut self`. Nothing here needs it to: every
/// field is read-only for the mount's whole life. What is worth knowing is that
/// the callbacks are nevertheless still SERIAL, because the mount leaves
/// `Config::n_threads` unset (read as one dispatch loop) and each request is
/// handled inline on it. So raising `n_threads` would be safe for this struct
/// but would let N kernel reads fire N concurrent blocking pulls through the
/// [`FileFetcher`] seam — the Core-side transaction, not this type, is what
/// would need re-examining first.
///
/// A slow pull therefore blocks every other request on the same mount —
/// `getattr`, `lookup`, `readdir` for sibling files all queue behind it. That is
/// not a consequence of the move to `fuser` 0.18: the dispatch has always been
/// inline, and the `n_threads` escape hatch has existed since 0.17.
struct ClipboardFs {
    tree: FileTree,
    fetcher: Arc<dyn FileFetcher>,
    uid: u32,
    gid: u32,
    /// A fixed timestamp for every node (the manifest carries no times).
    time: SystemTime,
}

impl ClipboardFs {
    fn new(tree: FileTree, fetcher: Arc<dyn FileFetcher>) -> ClipboardFs {
        // SAFETY: geteuid/getegid are always-successful, thread-safe libc calls.
        let uid = unsafe { libc::geteuid() };
        let gid = unsafe { libc::getegid() };
        ClipboardFs {
            tree,
            fetcher,
            uid,
            gid,
            time: SystemTime::now(),
        }
    }

    /// The `FileAttr` of an inode, or `None` if it does not exist. Files are
    /// read-only regular files (`0o444`), directories are traversable
    /// (`0o555`); both are owned by this process's effective uid/gid.
    fn attr(&self, ino: u64) -> Option<FileAttr> {
        let (kind, size) = self.tree.attr(ino)?;
        let (file_type, perm, nlink) = match kind {
            NodeKind::Dir => (FileType::Directory, 0o555, 2),
            NodeKind::File => (FileType::RegularFile, 0o444, 1),
        };
        Some(FileAttr {
            ino: INodeNo(ino),
            size,
            blocks: size.div_ceil(512),
            atime: self.time,
            mtime: self.time,
            ctime: self.time,
            crtime: self.time,
            kind: file_type,
            perm,
            nlink,
            uid: self.uid,
            gid: self.gid,
            rdev: 0,
            blksize: 512,
            flags: 0,
        })
    }
}

impl Filesystem for ClipboardFs {
    fn lookup(&self, _req: &Request, parent: INodeNo, name: &std::ffi::OsStr, reply: ReplyEntry) {
        match self
            .tree
            .lookup(parent.0, name)
            .and_then(|ino| self.attr(ino))
        {
            // Generation 0: inodes are never recycled here (the manifest is
            // frozen for the mount's whole life), so one generation suffices.
            Some(attr) => reply.entry(&TTL, &attr, Generation(0)),
            None => reply.error(Errno::ENOENT),
        }
    }

    fn getattr(&self, _req: &Request, ino: INodeNo, _fh: Option<FileHandle>, reply: ReplyAttr) {
        match self.attr(ino.0) {
            Some(attr) => reply.attr(&TTL, &attr),
            None => reply.error(Errno::ENOENT),
        }
    }

    fn open(&self, _req: &Request, ino: INodeNo, _flags: OpenFlags, reply: ReplyOpen) {
        // Files only. `FOPEN_DIRECT_IO`: no page cache / readahead, so the
        // kernel forwards the application's reads directly — each becomes one
        // on-demand pull. The file handle can equal the inode: reads carry the
        // `file_id`, so no per-open state is needed.
        match self.tree.attr(ino.0) {
            Some((NodeKind::File, _)) => {
                reply.opened(FileHandle(ino.0), FopenFlags::FOPEN_DIRECT_IO)
            }
            Some((NodeKind::Dir, _)) => reply.error(Errno::EISDIR),
            None => reply.error(Errno::ENOENT),
        }
    }

    fn read(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        size: u32,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        reply: ReplyData,
    ) {
        let Some(file_id) = self.tree.file_id(ino.0) else {
            reply.error(Errno::EIO);
            return;
        };
        // One pull returns the whole requested range (or the EOF-truncated part)
        // — no manual fill loop. Any pull failure is a clean `EIO`, never a
        // silent truncation. No clamping of `offset`: it arrives as a `u64`
        // because `fuser` itself validates the kernel's `off_t` and rejects a
        // negative one with an errno before ever reaching this callback.
        match self.fetcher.read(file_id, offset, u64::from(size)) {
            Ok(bytes) => reply.data(&bytes),
            Err(_) => reply.error(Errno::EIO),
        }
    }

    fn readdir(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        mut reply: ReplyDirectory,
    ) {
        let ino = ino.0;
        let Some(children) = self.tree.children(ino) else {
            reply.error(Errno::ENOTDIR);
            return;
        };
        let parent = self.tree.parent(ino).unwrap_or(ino);
        // `.`, `..`, then the children. `add` returns true when the kernel buffer
        // is full: stop (the kernel resumes from the offset in a further call).
        let mut entries: Vec<(u64, FileType, std::ffi::OsString)> =
            Vec::with_capacity(children.len() + 2);
        entries.push((ino, FileType::Directory, std::ffi::OsString::from(".")));
        entries.push((parent, FileType::Directory, std::ffi::OsString::from("..")));
        for (name, child) in children {
            let kind = match self.tree.attr(*child) {
                Some((NodeKind::Dir, _)) => FileType::Directory,
                _ => FileType::RegularFile,
            };
            entries.push((*child, kind, std::ffi::OsString::from(name)));
        }
        for (i, (e_ino, e_kind, e_name)) in entries.into_iter().enumerate().skip(offset as usize) {
            if reply.add(INodeNo(e_ino), (i + 1) as u64, e_kind, &e_name) {
                break;
            }
        }
        reply.ok();
    }
}

fn warn(message: &str) {
    eprintln!("[universallink-clipboard] {message}");
}
