// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

//! The rescan (doc/sync-engine.md, sections 3 and 8): walk the root,
//! compare against the index, bump the clock for every real difference,
//! tombstone what disappeared - except the paths the PENDING set expects
//! absent. Startup, the safety tick, `sync.resume` and watcher overflow
//! all funnel here; crash recovery falls out of it (the filesystem-first
//! apply order makes every crash window converge onto the truth).
//!
//! Exclusions are FAIL-VISIBLE: symlinks, names failing the wire gate,
//! and names colliding under case- or normalization-folding are reported
//! as ignored-with-reason, frozen in the index if they were ever indexed,
//! synced nothing.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{self, Read};
use std::path::Path;

use crate::clock::SetClock;
use crate::index::{Entry, EntryKind, Mtime, SetIndex};
use crate::records::SetKind;
use crate::wirepath::{check_component, wire_name};

/// Within this many seconds of recorded-vs-disk mtime, the size fast path
/// is not trusted and the content is hashed: FAT counts in 2 s steps, and
/// "doubt" is defined, not felt.
const MTIME_DOUBT_SECS: u64 = 2;

/// A name the scan left out, and why - the card's ignored list.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Ignored {
    /// Root-relative path as found on disk (display; it may not be a valid
    /// wire path, that being the point).
    pub name: String,
    pub reason: &'static str,
}

#[derive(Debug, Default)]
pub struct ScanReport {
    /// Entries created or updated (a real difference, clock bumped).
    pub changed: u64,
    /// Live entries tombstoned (disk says gone).
    pub tombstoned: u64,
    pub ignored: Vec<Ignored>,
}

/// Walks `root` and reconciles `index` with the tree. `pending` names the
/// wire paths absorbed but not yet applied: expected-absent, never
/// tombstoned. `self_component` is the clock's own vv key.
pub fn rescan(
    root: &Path,
    kind: SetKind,
    index: &mut SetIndex,
    pending: &BTreeSet<String>,
    clock: &mut SetClock,
    self_component: &str,
) -> io::Result<ScanReport> {
    let mut report = ScanReport::default();
    let mut seen: BTreeSet<String> = BTreeSet::new();

    match kind {
        SetKind::File => {
            scan_single_file(root, index, clock, self_component, &mut report, &mut seen)?
        }
        SetKind::Dir => {
            walk_dir(
                root,
                "",
                index,
                clock,
                self_component,
                &mut report,
                &mut seen,
            )?;
        }
    }

    // The tombstone pass: live entries the walk did not meet, minus what
    // the pending set expects absent and what the walk IGNORED (an ignored
    // name is frozen, not deleted: ignoring is not a delete gesture).
    let missing: Vec<String> = index
        .live()
        .map(|e| e.path.clone())
        .filter(|path| !seen.contains(path) && !pending.contains(path))
        .collect();
    for path in missing {
        let entry = index.get(&path).expect("listed from the index").clone();
        let mut vv = entry.vv;
        vv.set(self_component, clock.tick()?);
        index.upsert(Entry {
            path,
            kind: entry.kind,
            size: 0,
            mtime: Mtime::default(),
            exec: false,
            hash: String::new(),
            vv,
            deleted: true,
        });
        report.tombstoned += 1;
    }
    Ok(report)
}

/// The set-of-one-file case: the root IS the file, its wire path its NFC
/// basename.
fn scan_single_file(
    root: &Path,
    index: &mut SetIndex,
    clock: &mut SetClock,
    self_component: &str,
    report: &mut ScanReport,
    seen: &mut BTreeSet<String>,
) -> io::Result<()> {
    let Some(disk_name) = root.file_name().and_then(|n| n.to_str()) else {
        report.ignored.push(Ignored {
            name: root.display().to_string(),
            reason: "name is not valid UTF-8",
        });
        return Ok(());
    };
    let Some(wire) = accepted_name(disk_name, report) else {
        return Ok(());
    };
    let meta = match std::fs::symlink_metadata(root) {
        Ok(meta) => meta,
        // Absent: the tombstone pass will notice.
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    if meta.file_type().is_symlink() {
        report.ignored.push(Ignored {
            name: disk_name.to_string(),
            reason: "symlink",
        });
        return Ok(());
    }
    if !meta.is_file() {
        report.ignored.push(Ignored {
            name: disk_name.to_string(),
            reason: "not a regular file",
        });
        return Ok(());
    }
    reconcile_file(root, &wire, &meta, index, clock, self_component, report)?;
    seen.insert(wire);
    Ok(())
}

/// One directory level: detect fold collisions among the survivors of the
/// name gate, then reconcile files and recurse into directories.
fn walk_dir(
    dir: &Path,
    prefix: &str,
    index: &mut SetIndex,
    clock: &mut SetClock,
    self_component: &str,
    report: &mut ScanReport,
    seen: &mut BTreeSet<String>,
) -> io::Result<()> {
    struct Candidate {
        disk_name: String,
        wire: String,
        is_dir: bool,
        meta: std::fs::Metadata,
    }
    let mut candidates: Vec<Candidate> = Vec::new();
    let mut folds: BTreeMap<String, u32> = BTreeMap::new();

    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let display = |name: &std::ffi::OsStr| format!("{prefix}{}", name.to_string_lossy());
        let Some(disk_name) = entry.file_name().to_str().map(str::to_string) else {
            report.ignored.push(Ignored {
                name: display(&entry.file_name()),
                reason: "name is not valid UTF-8",
            });
            continue;
        };
        let file_type = entry.file_type()?;
        if file_type.is_symlink() {
            report.ignored.push(Ignored {
                name: format!("{prefix}{disk_name}"),
                reason: "symlink",
            });
            continue;
        }
        let Some(wire) = accepted_name_at(&disk_name, prefix, report) else {
            continue;
        };
        let meta = entry.metadata()?;
        let is_dir = file_type.is_dir();
        if !is_dir && !meta.is_file() {
            report.ignored.push(Ignored {
                name: format!("{prefix}{disk_name}"),
                reason: "not a regular file",
            });
            continue;
        }
        *folds.entry(fold_key(&wire)).or_insert(0) += 1;
        candidates.push(Candidate {
            disk_name,
            wire,
            is_dir,
            meta,
        });
    }

    for c in candidates {
        // Names that collide under case- or normalization-folding: BOTH are
        // ignored (a case-insensitive member could hold only one), neither
        // is tombstoned, whatever the index used to say.
        if folds.get(&fold_key(&c.wire)).copied().unwrap_or(0) > 1 {
            report.ignored.push(Ignored {
                name: format!("{prefix}{}", c.disk_name),
                reason: "collides under folding",
            });
            // Frozen, not deleted: keep any indexed entry out of the
            // tombstone pass.
            if let Some(e) = index.get(&wire_path(prefix, &c.wire)) {
                seen.insert(e.path.clone());
            }
            continue;
        }
        let path = dir.join(&c.disk_name);
        let wire = wire_path(prefix, &c.wire);
        if c.is_dir {
            reconcile_dir(&wire, index, clock, self_component, report)?;
            seen.insert(wire.clone());
            walk_dir(
                &path,
                &format!("{wire}/"),
                index,
                clock,
                self_component,
                report,
                seen,
            )?;
        } else {
            reconcile_file(&path, &wire, &c.meta, index, clock, self_component, report)?;
            seen.insert(wire);
        }
    }
    Ok(())
}

fn wire_path(prefix: &str, name: &str) -> String {
    format!("{prefix}{name}")
}

/// The fold key two names must not share: NFC + Unicode lowercase, the
/// approximation of what case-insensitive filesystems collapse.
fn fold_key(wire: &str) -> String {
    wire.to_lowercase()
}

fn accepted_name_at(disk_name: &str, prefix: &str, report: &mut ScanReport) -> Option<String> {
    let full = format!("{prefix}{disk_name}");
    let Some(wire) = wire_name(disk_name) else {
        report.ignored.push(Ignored {
            name: full,
            reason: "not NFC",
        });
        return None;
    };
    match check_component(&wire) {
        Ok(()) => Some(wire),
        Err(reason) => {
            report.ignored.push(Ignored { name: full, reason });
            None
        }
    }
}

fn accepted_name(disk_name: &str, report: &mut ScanReport) -> Option<String> {
    accepted_name_at(disk_name, "", report)
}

fn reconcile_dir(
    wire: &str,
    index: &mut SetIndex,
    clock: &mut SetClock,
    self_component: &str,
    report: &mut ScanReport,
) -> io::Result<()> {
    let known = index.get(wire);
    let unchanged = known.is_some_and(|e| !e.deleted && e.kind == EntryKind::Dir);
    if unchanged {
        return Ok(());
    }
    let mut vv = known.map(|e| e.vv.clone()).unwrap_or_default();
    vv.set(self_component, clock.tick()?);
    index.upsert(Entry {
        path: wire.to_string(),
        kind: EntryKind::Dir,
        size: 0,
        mtime: Mtime::default(),
        exec: false,
        hash: String::new(),
        vv,
        deleted: false,
    });
    report.changed += 1;
    Ok(())
}

fn reconcile_file(
    path: &Path,
    wire: &str,
    meta: &std::fs::Metadata,
    index: &mut SetIndex,
    clock: &mut SetClock,
    self_component: &str,
    report: &mut ScanReport,
) -> io::Result<()> {
    let size = meta.len();
    let mtime = Mtime::of(meta.modified()?);
    let known = index.get(wire).cloned();
    let exec = exec_bit(meta, known.as_ref());

    if let Some(k) = &known
        && !k.deleted
        && k.kind == EntryKind::File
    {
        let same_stat = k.size == size && k.mtime == mtime;
        let doubt = k.size == size && k.mtime.seconds_apart(mtime) <= MTIME_DOUBT_SECS;
        if same_stat && k.exec == exec {
            return Ok(());
        }
        if same_stat || doubt {
            // The quick path is not trusted here: hash to decide. Identical
            // content is NOT a change (the mtime record still follows the
            // disk); an exec flip alone is one.
            let hash = hash_file(path)?;
            if hash == k.hash && k.exec == exec {
                let mut refreshed = k.clone();
                refreshed.mtime = mtime;
                index.upsert(refreshed);
                return Ok(());
            }
            let mut vv = k.vv.clone();
            vv.set(self_component, clock.tick()?);
            index.upsert(Entry {
                path: wire.to_string(),
                kind: EntryKind::File,
                size,
                mtime,
                exec,
                hash,
                vv,
                deleted: false,
            });
            report.changed += 1;
            return Ok(());
        }
    }

    // New, resurrected, kind-changed, or plainly different: hash and bump.
    let hash = hash_file(path)?;
    if let Some(k) = &known
        && !k.deleted
        && k.kind == EntryKind::File
        && k.hash == hash
        && k.exec == exec
    {
        // Same content after all (a touch): follow the disk, no bump.
        let mut refreshed = k.clone();
        refreshed.mtime = mtime;
        refreshed.size = size;
        index.upsert(refreshed);
        return Ok(());
    }
    let mut vv = known.map(|e| e.vv).unwrap_or_default();
    vv.set(self_component, clock.tick()?);
    index.upsert(Entry {
        path: wire.to_string(),
        kind: EntryKind::File,
        size,
        mtime,
        exec,
        hash,
        vv,
        deleted: false,
    });
    report.changed += 1;
    Ok(())
}

/// The exec bit, where the filesystem has one; elsewhere the last recorded
/// value carries forward (write-only passthrough: a Windows member must
/// not strip `+x` from every script it touches).
#[cfg(unix)]
fn exec_bit(meta: &std::fs::Metadata, _known: Option<&Entry>) -> bool {
    use std::os::unix::fs::PermissionsExt;
    meta.permissions().mode() & 0o100 != 0
}

#[cfg(not(unix))]
fn exec_bit(_meta: &std::fs::Metadata, known: Option<&Entry>) -> bool {
    known.is_some_and(|e| e.exec)
}

/// Streaming BLAKE3, lowercase hex.
pub fn hash_file(path: &Path) -> io::Result<String> {
    let mut file = std::fs::File::open(path)?;
    let mut hasher = blake3::Hasher::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher.finalize().to_hex().to_string())
}

#[cfg(test)]
mod tests {
    use crate::vv::{Vv, component};

    use super::*;

    const A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    struct Fixture {
        root: tempfile::TempDir,
        /// Holds the clock lease directory alive.
        _state: tempfile::TempDir,
        index: SetIndex,
        clock: SetClock,
        key: String,
    }

    impl Fixture {
        fn new() -> Fixture {
            let root = tempfile::tempdir().expect("root");
            let state = tempfile::tempdir().expect("state");
            let index = SetIndex::new();
            let clock = SetClock::open(state.path(), &index, A, 1_700_000_000).expect("clock");
            let key = clock.self_component(A);
            Fixture {
                root,
                _state: state,
                index,
                clock,
                key,
            }
        }

        fn scan(&mut self) -> ScanReport {
            rescan(
                self.root.path(),
                SetKind::Dir,
                &mut self.index,
                &BTreeSet::new(),
                &mut self.clock,
                &self.key,
            )
            .expect("rescan")
        }

        fn scan_pending(&mut self, pending: &BTreeSet<String>) -> ScanReport {
            rescan(
                self.root.path(),
                SetKind::Dir,
                &mut self.index,
                pending,
                &mut self.clock,
                &self.key,
            )
            .expect("rescan")
        }

        fn write(&self, rel: &str, contents: &str) {
            let path = self.root.path().join(rel);
            std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
            std::fs::write(path, contents).expect("write");
        }
    }

    #[test]
    fn a_first_scan_indexes_the_tree_with_hashes_and_clocks() {
        let mut f = Fixture::new();
        f.write("a.txt", "alpha");
        f.write("sub/b.txt", "beta");
        let report = f.scan();
        assert_eq!(report.changed, 3, "two files and one directory");
        assert!(report.ignored.is_empty());

        let a = f.index.get("a.txt").expect("indexed");
        assert_eq!(a.hash, blake3::hash(b"alpha").to_hex().to_string());
        assert_eq!(a.kind, EntryKind::File);
        assert!(a.vv.get(&f.key) > 0);
        assert_eq!(f.index.get("sub").expect("dir").kind, EntryKind::Dir);
        assert!(f.index.get("sub/b.txt").is_some());

        // Idempotent: a second scan changes nothing and bumps nothing.
        let before = f.clock.current();
        let report = f.scan();
        assert_eq!((report.changed, report.tombstoned), (0, 0));
        assert_eq!(f.clock.current(), before);
    }

    #[test]
    fn edits_bump_and_disappearances_tombstone() {
        let mut f = Fixture::new();
        f.write("a.txt", "alpha");
        f.scan();
        let v1 = f.index.get("a.txt").expect("entry").vv.get(&f.key);

        f.write("a.txt", "ALPHA2");
        let report = f.scan();
        assert_eq!(report.changed, 1);
        let e = f.index.get("a.txt").expect("entry");
        assert!(e.vv.get(&f.key) > v1);

        std::fs::remove_file(f.root.path().join("a.txt")).expect("rm");
        let report = f.scan();
        assert_eq!(report.tombstoned, 1);
        let tomb = f.index.get("a.txt").expect("tombstone").clone();
        assert!(tomb.deleted);
        assert!(tomb.hash.is_empty());
        assert!(tomb.vv.get(&f.key) > v1, "the tombstone keeps a bumped vv");

        // And a resurrection is a new live entry dominating the tombstone.
        f.write("a.txt", "back");
        f.scan();
        let back = f.index.get("a.txt").expect("entry");
        assert!(!back.deleted);
        assert!(back.vv.get(&f.key) > tomb.vv.get(&f.key));
    }

    #[test]
    fn a_touch_that_changes_nothing_is_not_a_change() {
        let mut f = Fixture::new();
        f.write("a.txt", "alpha");
        f.scan();
        let before = f.index.get("a.txt").expect("entry").clone();

        // Rewrite identical content: the mtime moves, the content does not.
        f.write("a.txt", "alpha");
        let report = f.scan();
        assert_eq!(report.changed, 0);
        let after = f.index.get("a.txt").expect("entry");
        assert_eq!(after.vv, before.vv, "no real difference, no bump");
    }

    #[test]
    fn pending_paths_are_expected_absent_and_never_tombstoned() {
        let mut f = Fixture::new();
        f.write("here.txt", "x");
        f.scan();
        // An absorbed-but-unpulled entry: in the index, not on disk.
        let mut vv = Vv::new();
        vv.set(&component(A, 9), 1);
        f.index.upsert(Entry {
            path: "incoming.txt".into(),
            kind: EntryKind::File,
            size: 1,
            mtime: Mtime::default(),
            exec: false,
            hash: "b".repeat(64),
            vv,
            deleted: false,
        });
        let pending: BTreeSet<String> = ["incoming.txt".to_string()].into();
        let report = f.scan_pending(&pending);
        assert_eq!(report.tombstoned, 0);
        assert!(!f.index.get("incoming.txt").expect("kept").deleted);

        // Without the pending mark, the same entry WOULD tombstone: the
        // suppression is the pending set's doing, not luck.
        let report = f.scan();
        assert_eq!(report.tombstoned, 1);
    }

    #[test]
    fn exclusions_are_reported_not_silent() {
        let mut f = Fixture::new();
        f.write("ok.txt", "x");
        f.write(".1device.tmp/staged", "x");
        f.write("bad:name.txt", "x");
        #[cfg(unix)]
        std::os::unix::fs::symlink(f.root.path().join("ok.txt"), f.root.path().join("link.txt"))
            .expect("symlink");
        let report = f.scan();
        let reasons: Vec<&str> = report.ignored.iter().map(|i| i.reason).collect();
        assert!(reasons.contains(&"reserved staging name"), "{reasons:?}");
        assert!(reasons.contains(&"separator character"), "{reasons:?}");
        #[cfg(unix)]
        assert!(reasons.contains(&"symlink"), "{reasons:?}");
        assert!(f.index.get("ok.txt").is_some());
        assert!(f.index.get(".1device.tmp/staged").is_none());
    }

    #[cfg(unix)]
    #[test]
    fn fold_collisions_ignore_both_and_freeze_the_indexed_one() {
        let mut f = Fixture::new();
        f.write("Readme.md", "one");
        f.scan();
        assert!(f.index.get("Readme.md").is_some());

        // A second name that folds onto the first appears (possible on a
        // case-sensitive filesystem only): both are ignored, the indexed
        // one is frozen, NOT tombstoned - ignoring is not deleting.
        f.write("readme.md", "two");
        let report = f.scan();
        let collided: Vec<&str> = report
            .ignored
            .iter()
            .filter(|i| i.reason == "collides under folding")
            .map(|i| i.name.as_str())
            .collect();
        assert_eq!(collided.len(), 2, "{:?}", report.ignored);
        assert_eq!(report.tombstoned, 0);
        assert!(!f.index.get("Readme.md").expect("frozen").deleted);
    }

    #[cfg(unix)]
    #[test]
    fn the_exec_bit_is_a_real_difference() {
        use std::os::unix::fs::PermissionsExt;

        let mut f = Fixture::new();
        f.write("run.sh", "#!/bin/sh\n");
        f.scan();
        assert!(!f.index.get("run.sh").expect("entry").exec);
        let v1 = f.index.get("run.sh").expect("entry").vv.get(&f.key);

        let path = f.root.path().join("run.sh");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        let report = f.scan();
        assert_eq!(report.changed, 1);
        let e = f.index.get("run.sh").expect("entry");
        assert!(e.exec);
        assert!(e.vv.get(&f.key) > v1);
    }

    #[test]
    fn a_single_file_set_watches_exactly_its_name() {
        let dir = tempfile::tempdir().expect("root");
        let state = tempfile::tempdir().expect("state");
        let file = dir.path().join("notes.txt");
        std::fs::write(&file, "n").expect("write");
        let mut index = SetIndex::new();
        let mut clock = SetClock::open(state.path(), &index, A, 1_700_000_000).expect("clock");
        let key = clock.self_component(A);
        let report = rescan(
            &file,
            SetKind::File,
            &mut index,
            &BTreeSet::new(),
            &mut clock,
            &key,
        )
        .expect("rescan");
        assert_eq!(report.changed, 1);
        assert!(index.get("notes.txt").is_some());

        // The file vanishes: tombstoned like any other path.
        std::fs::remove_file(&file).expect("rm");
        let report = rescan(
            &file,
            SetKind::File,
            &mut index,
            &BTreeSet::new(),
            &mut clock,
            &key,
        )
        .expect("rescan");
        assert_eq!(report.tombstoned, 1);
    }
}
