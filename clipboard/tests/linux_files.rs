// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

//! Native-only integration tests for the Linux files backend: they mount a real
//! FUSE tree and read it through the kernel. Every test is `#[ignore]`d AND
//! early-returns when `fuse_available()` is false, so it never runs in CI (which
//! passes neither `--ignored` nor guarantees `/dev/fuse` + `fusermount3`) and is
//! skipped on any box without unprivileged FUSE.
//!
//! Run natively with:
//!   cargo test -p universallink-clipboard --test linux_files -- --ignored --test-threads=1
//! A FUSE mount is a per-display-independent, per-process resource, but the tests
//! still serialize on [`MOUNT_LOCK`] and want `--test-threads=1`, matching the
//! other backends' live suites.

#![cfg(target_os = "linux")]

use std::io::{Read, Seek, SeekFrom};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use universallink_clipboard::{FileFetcher, FuseMount, RemoteFile, fuse_available};

/// Serializes the mount/read tests (they run under `--test-threads=1`).
static MOUNT_LOCK: Mutex<()> = Mutex::new(());

/// Deterministic byte at absolute offset `k` (same formula on write and check).
fn byte_at(k: u64) -> u8 {
    (k % 251) as u8
}

/// An in-process fetcher serving deterministic bytes for a fixed set of files,
/// truncating at each file's declared size (fewer than `len` only at EOF).
struct FakeFetcher {
    sizes: std::collections::HashMap<String, u64>,
}

impl FileFetcher for FakeFetcher {
    fn read(&self, file_id: &str, offset: u64, len: u64) -> std::io::Result<Vec<u8>> {
        let size = *self
            .sizes
            .get(file_id)
            .ok_or_else(|| std::io::Error::other(format!("unknown file_id {file_id}")))?;
        if offset >= size {
            return Ok(Vec::new());
        }
        let end = (offset + len).min(size);
        Ok((offset..end).map(byte_at).collect())
    }
}

const TOP_SIZE: u64 = 100_003;
const INNER_SIZE: u64 = 250_000;

fn manifest() -> Vec<RemoteFile> {
    vec![
        RemoteFile {
            file_id: "f-top".into(),
            path: "top.bin".into(),
            size: TOP_SIZE,
            dir: false,
        },
        RemoteFile {
            file_id: "f-inner".into(),
            path: "dir/inner.bin".into(),
            size: INNER_SIZE,
            dir: false,
        },
    ]
}

fn fetcher() -> Arc<dyn FileFetcher> {
    let mut sizes = std::collections::HashMap::new();
    sizes.insert("f-top".to_string(), TOP_SIZE);
    sizes.insert("f-inner".to_string(), INNER_SIZE);
    Arc::new(FakeFetcher { sizes })
}

macro_rules! skip_if_no_fuse {
    () => {
        if !fuse_available() {
            eprintln!("skipping: FUSE unavailable (no /dev/fuse or fusermount3)");
            return;
        }
    };
}

#[test]
#[ignore = "mounts a real FUSE filesystem; needs /dev/fuse + fusermount3"]
fn reads_whole_files_ranges_and_directories() {
    let _guard = MOUNT_LOCK.lock().unwrap();
    skip_if_no_fuse!();

    let mount = FuseMount::mount(&manifest(), fetcher()).expect("FUSE mount");
    let roots = mount.root_paths().to_vec();
    assert_eq!(roots.len(), 2, "top.bin + dir");

    // roots[0] = <mount>/top.bin, roots[1] = <mount>/dir.
    let top = roots
        .iter()
        .find(|p| p.ends_with("top.bin"))
        .expect("top root");
    let dir = roots.iter().find(|p| p.ends_with("dir")).expect("dir root");
    let mountpoint = top.parent().expect("mountpoint").to_path_buf();

    // Whole top-level file: exact size + exact bytes.
    let whole = std::fs::read(top).expect("read top.bin");
    assert_eq!(whole.len() as u64, TOP_SIZE);
    assert!(
        whole
            .iter()
            .enumerate()
            .all(|(i, &b)| b == byte_at(i as u64)),
        "top.bin bytes"
    );

    // A mid-file range of the nested file (seek then read): pulled on demand.
    let inner_path = dir.join("inner.bin");
    let mut f = std::fs::File::open(&inner_path).expect("open dir/inner.bin");
    let off: u64 = 123_456;
    let want: usize = 40_000;
    f.seek(SeekFrom::Start(off)).expect("seek");
    let mut buf = vec![0u8; want];
    f.read_exact(&mut buf).expect("read_exact mid-range");
    assert!(
        buf.iter()
            .enumerate()
            .all(|(i, &b)| b == byte_at(off + i as u64)),
        "mid-file range bytes"
    );

    // Whole nested file too.
    let inner = std::fs::read(&inner_path).expect("read dir/inner.bin");
    assert_eq!(inner.len() as u64, INNER_SIZE);
    assert!(
        inner
            .iter()
            .enumerate()
            .all(|(i, &b)| b == byte_at(i as u64)),
        "inner.bin bytes"
    );

    // Directory listings: the mount root and the nested directory.
    let mut top_names = read_dir_names(&mountpoint);
    top_names.sort();
    assert_eq!(top_names, vec!["dir".to_string(), "top.bin".to_string()]);
    assert_eq!(read_dir_names(dir), vec!["inner.bin".to_string()]);

    // Unmount on drop: the temporary mountpoint disappears (its removal may be
    // handed to a detached retry thread, so wait briefly).
    drop(mount);
    let mut gone = false;
    for _ in 0..100 {
        if !mountpoint.exists() {
            gone = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(
        gone,
        "mountpoint must be removed after drop: {mountpoint:?}"
    );
}

/// The seven mount options are the security contract of this backend: the tree
/// is built from a manifest a REMOTE peer sent, so it must be read-only and
/// carry no setuid/dev/exec. Nothing else in the suite can see them — a mount
/// that silently lost `RO` still reads correctly — so assert them against what
/// the kernel actually recorded, and prove read-only by attempting a write.
#[test]
#[ignore = "mounts a real FUSE filesystem; needs /dev/fuse + fusermount3"]
fn the_mount_is_read_only_and_hardened() {
    let _guard = MOUNT_LOCK.lock().unwrap();
    skip_if_no_fuse!();

    let mount = FuseMount::mount(&manifest(), fetcher()).expect("FUSE mount");
    let top = mount
        .root_paths()
        .iter()
        .find(|p| p.ends_with("top.bin"))
        .expect("top root")
        .clone();
    let mountpoint = top.parent().expect("mountpoint").to_path_buf();

    // What the kernel recorded for this mount. In /proc/self/mountinfo the 5th
    // field is the mount point and the 6th is the per-mount option list. Two
    // traps in matching that path: `fuser` canonicalizes the mountpoint before
    // mounting, and the kernel escapes space/tab/newline/backslash in octal —
    // so compare the canonical path against the UNESCAPED field, or a symlinked
    // or space-bearing $XDG_RUNTIME_DIR turns this into a spurious failure.
    let mountinfo = std::fs::read_to_string("/proc/self/mountinfo").expect("read mountinfo");
    let want = mountpoint.canonicalize().expect("canonical mountpoint");
    let line = mountinfo
        .lines()
        .find(|l| l.split(' ').nth(4).map(unescape_mountinfo) == Some(want.clone()))
        .unwrap_or_else(|| panic!("no mountinfo line for {}", want.display()));
    let opts: Vec<&str> = line
        .split(' ')
        .nth(5)
        .expect("option field")
        .split(',')
        .collect();
    for opt in ["ro", "nosuid", "nodev", "noexec", "noatime"] {
        assert!(opts.contains(&opt), "missing {opt} in {line}");
    }
    // The filesystem type carries the announced subtype, and the mount belongs
    // to the pasting user alone: `user_id` is us and nothing widened it to the
    // rest of the machine. `allow_other` would land in the superblock options
    // after the `-` separator, not in the per-mount list, so match the whole
    // line for it.
    assert!(
        line.contains("fuse.universallink-clip"),
        "subtype not announced: {line}"
    );
    // SAFETY: geteuid is an always-successful, thread-safe libc call.
    let uid = unsafe { libc::geteuid() };
    assert!(
        line.contains(&format!("user_id={uid}")),
        "the mount must be owned by the pasting user: {line}"
    );
    assert!(
        !line.contains("allow_other") && !line.contains("allow_root"),
        "the mount must not be world-traversable: {line}"
    );

    // Read-only for real, not just as an advertised flag. EROFS specifically:
    // the write must be refused BY THE MOUNT, at the VFS. `is_err()` alone
    // would be vacuous here — this filesystem implements neither `create` nor
    // `mknod`, so an unhardened mount would still refuse with ENOSYS and the
    // assertion would pass while the hardening was gone.
    let err = std::fs::OpenOptions::new()
        .write(true)
        .open(&top)
        .expect_err("writing to a pasted clip must fail");
    assert_eq!(
        err.raw_os_error(),
        Some(libc::EROFS),
        "expected EROFS, got {err:?}"
    );
    let err = std::fs::write(mountpoint.join("intruder"), b"x")
        .expect_err("creating a file in the clip must fail");
    assert_eq!(
        err.raw_os_error(),
        Some(libc::EROFS),
        "expected EROFS on create, got {err:?}"
    );
}

/// Reverses the octal escaping the kernel applies to a mountinfo path field
/// (space, tab, newline and backslash, as `\040 \011 \012 \134`).
fn unescape_mountinfo(field: &str) -> PathBuf {
    let mut out = String::with_capacity(field.len());
    let mut rest = field;
    while let Some(i) = rest.find('\\') {
        out.push_str(&rest[..i]);
        let octal = rest.get(i + 1..i + 4).unwrap_or_default();
        match u8::from_str_radix(octal, 8) {
            Ok(b) => {
                out.push(b as char);
                rest = &rest[i + 4..];
            }
            Err(_) => {
                out.push('\\');
                rest = &rest[i + 1..];
            }
        }
    }
    out.push_str(rest);
    PathBuf::from(out)
}

/// A pull that fails must surface as a clean `EIO` at the syscall — never a
/// short read passed off as a whole file. Uses a fetcher that always errors.
#[test]
#[ignore = "mounts a real FUSE filesystem; needs /dev/fuse + fusermount3"]
fn a_failed_pull_surfaces_as_eio() {
    let _guard = MOUNT_LOCK.lock().unwrap();
    skip_if_no_fuse!();

    // Counts its calls: `read` has a second EIO exit (an inode with no
    // `file_id`), so without this the test could not tell which one it hit.
    #[derive(Default)]
    struct FailingFetcher {
        calls: std::sync::atomic::AtomicUsize,
    }
    impl FileFetcher for FailingFetcher {
        fn read(&self, _file_id: &str, _offset: u64, _len: u64) -> std::io::Result<Vec<u8>> {
            self.calls
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            Err(std::io::Error::other("pull refused"))
        }
    }

    let fetcher = Arc::new(FailingFetcher::default());
    let mount = FuseMount::mount(&manifest(), fetcher.clone()).expect("FUSE mount");
    let top = mount
        .root_paths()
        .iter()
        .find(|p| p.ends_with("top.bin"))
        .expect("top root")
        .clone();

    // The metadata comes from the frozen manifest, so `stat` still succeeds and
    // announces the full size — it is the READ that must fail, and with EIO
    // rather than a short buffer that a copy would accept as the whole file.
    let meta = std::fs::metadata(&top).expect("stat is served from the manifest");
    assert_eq!(meta.len(), TOP_SIZE);
    let err = std::fs::read(&top).expect_err("a failed pull must not read as data");
    assert_eq!(
        err.raw_os_error(),
        Some(libc::EIO),
        "expected EIO, got {err:?}"
    );
    assert!(
        fetcher.calls.load(std::sync::atomic::Ordering::Relaxed) > 0,
        "the EIO must come from the refused pull, not from an unresolved inode"
    );
}

#[test]
#[ignore = "mounts a real FUSE filesystem; needs /dev/fuse + fusermount3"]
fn empty_manifest_is_refused() {
    let _guard = MOUNT_LOCK.lock().unwrap();
    skip_if_no_fuse!();

    // No usable root → mount refuses cleanly (never a mount of nothing).
    assert!(FuseMount::mount(&[], fetcher()).is_err());
}

fn read_dir_names(dir: &PathBuf) -> Vec<String> {
    std::fs::read_dir(dir)
        .expect("read_dir")
        .filter_map(|e| e.ok())
        .filter_map(|e| e.file_name().into_string().ok())
        .collect()
}
