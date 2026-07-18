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
