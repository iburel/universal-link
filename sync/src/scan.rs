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

/// What one walk SAW, without deciding anything about clocks. The walk is
/// the expensive half (it reads and hashes every doubtful file), so it runs
/// off the event loop against a snapshot of the index; folding the
/// observation back in is pure bookkeeping and stays on the loop, where the
/// clock and the index actually live.
#[derive(Debug, Default)]
pub struct Observation {
    /// A real difference: this is what the disk holds now.
    pub changed: Vec<Observed>,
    /// Same content, moved stamp: the record follows the disk, no bump.
    pub touched: Vec<Observed>,
    /// Live index paths the walk did not meet, deepest first.
    pub tombstoned: Vec<String>,
    pub ignored: Vec<Ignored>,
}

#[derive(Clone, Debug)]
pub struct Observed {
    pub path: String,
    pub kind: EntryKind,
    pub size: u64,
    pub mtime: Mtime,
    pub exec: bool,
    pub hash: String,
}

/// Walks `root` and reports what it saw against `index` (read-only, so it
/// is safe to run off the event loop). `pending` names the wire paths
/// absorbed but not yet applied: expected-absent, never tombstoned.
pub fn observe(
    root: &Path,
    kind: SetKind,
    index: &SetIndex,
    pending: &BTreeSet<String>,
) -> io::Result<Observation> {
    let mut report = Observation::default();
    let mut seen: BTreeSet<String> = BTreeSet::new();

    match kind {
        SetKind::File => scan_single_file(root, index, &mut report, &mut seen)?,
        SetKind::Dir => {
            walk_dir(root, "", index, &mut report, &mut seen)?;
        }
    }

    // The tombstone pass: live entries the walk did not meet, minus what
    // the pending set expects absent and what the walk IGNORED (an ignored
    // name is frozen, not deleted: ignoring is not a delete gesture).
    let mut missing: Vec<String> = index
        .live()
        .map(|e| e.path.clone())
        .filter(|path| !seen.contains(path) && !pending.contains(path))
        .collect();
    // Deepest first: tombstoning a folder is ordered AFTER its contents, so
    // the children carry the lower clock and a peer's end-of-batch
    // directory guard sees an empty directory rather than a live child.
    missing.sort_by(|a, b| {
        let depth = |p: &String| p.matches('/').count();
        depth(b).cmp(&depth(a)).then_with(|| b.cmp(a))
    });
    report.tombstoned = missing;
    Ok(report)
}

/// Folds an [`Observation`] into the index: a clock value per real
/// difference, a stamp refresh for what only looked different, a tombstone
/// per disappearance. Cheap by construction - no file is read here.
pub fn fold(
    observation: Observation,
    index: &mut SetIndex,
    clock: &mut SetClock,
    self_component: &str,
) -> io::Result<ScanReport> {
    let mut report = ScanReport {
        changed: 0,
        tombstoned: 0,
        ignored: observation.ignored,
    };
    for seen in observation.touched {
        // Same content: keep the vv, follow the disk's stamp.
        if let Some(known) = index.get(&seen.path) {
            let mut refreshed = known.clone();
            refreshed.size = seen.size;
            refreshed.mtime = seen.mtime;
            index.upsert(refreshed);
        }
    }
    for seen in observation.changed {
        let mut vv = index
            .get(&seen.path)
            .map(|e| e.vv.clone())
            .unwrap_or_default();
        vv.set(self_component, clock.tick()?);
        index.upsert(Entry {
            path: seen.path,
            kind: seen.kind,
            size: seen.size,
            mtime: seen.mtime,
            exec: seen.exec,
            hash: seen.hash,
            vv,
            deleted: false,
        });
        report.changed += 1;
    }
    for path in observation.tombstoned {
        let Some(entry) = index.get(&path).cloned() else {
            continue;
        };
        if entry.deleted {
            continue;
        }
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

/// The inline pair, for callers that may block (the engine's own tests, and
/// any path where the tree is known small).
pub fn rescan(
    root: &Path,
    kind: SetKind,
    index: &mut SetIndex,
    pending: &BTreeSet<String>,
    clock: &mut SetClock,
    self_component: &str,
) -> io::Result<ScanReport> {
    let observation = observe(root, kind, index, pending)?;
    fold(observation, index, clock, self_component)
}

/// The set-of-one-file case: the root IS the file, its wire path its NFC
/// basename.
fn scan_single_file(
    root: &Path,
    index: &SetIndex,
    report: &mut Observation,
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
    reconcile_file(root, &wire, &meta, index, report)?;
    seen.insert(wire);
    Ok(())
}

/// One directory level: detect fold collisions among the survivors of the
/// name gate, then reconcile files and recurse into directories.
fn walk_dir(
    dir: &Path,
    prefix: &str,
    index: &SetIndex,
    report: &mut Observation,
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
            // Frozen, not deleted: keep the indexed entry AND, for a
            // directory, everything under it out of the tombstone pass. An
            // ignored name is not a delete gesture, and a collision at one
            // level must never wipe the subtree below it.
            let wire = wire_path(prefix, &c.wire);
            seen.insert(wire.clone());
            let subtree = format!("{wire}/");
            for entry in index.live() {
                if entry.path.starts_with(&subtree) {
                    seen.insert(entry.path.clone());
                }
            }
            continue;
        }
        let path = dir.join(&c.disk_name);
        let wire = wire_path(prefix, &c.wire);
        if c.is_dir {
            reconcile_dir(&wire, index, report);
            seen.insert(wire.clone());
            walk_dir(&path, &format!("{wire}/"), index, report, seen)?;
        } else {
            reconcile_file(&path, &wire, &c.meta, index, report)?;
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

fn accepted_name_at(disk_name: &str, prefix: &str, report: &mut Observation) -> Option<String> {
    let full = format!("{prefix}{disk_name}");
    // Our own staging is EXCLUDED, not refused: reporting it would put the
    // engine's own working directory in every card's ignored list, as though
    // the user had done something wrong.
    if disk_name == crate::wirepath::STAGING_DIR
        || (disk_name.starts_with('.') && disk_name.ends_with(crate::wirepath::STAGING_SUFFIX))
    {
        return None;
    }
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

fn accepted_name(disk_name: &str, report: &mut Observation) -> Option<String> {
    accepted_name_at(disk_name, "", report)
}

fn reconcile_dir(wire: &str, index: &SetIndex, report: &mut Observation) {
    let known = index.get(wire);
    if known.is_some_and(|e| !e.deleted && e.kind == EntryKind::Dir) {
        return;
    }
    report.changed.push(Observed {
        path: wire.to_string(),
        kind: EntryKind::Dir,
        size: 0,
        mtime: Mtime::default(),
        exec: false,
        hash: String::new(),
    });
}

fn reconcile_file(
    path: &Path,
    wire: &str,
    meta: &std::fs::Metadata,
    index: &SetIndex,
    report: &mut Observation,
) -> io::Result<()> {
    let size = meta.len();
    let mtime = Mtime::of(meta.modified()?);
    let known = index.get(wire).cloned();
    let exec = exec_bit(meta, known.as_ref());
    let seen = |hash: String| Observed {
        path: wire.to_string(),
        kind: EntryKind::File,
        size,
        mtime,
        exec,
        hash,
    };

    if let Some(k) = &known
        && !k.deleted
        && k.kind == EntryKind::File
    {
        let same_stat = k.size == size && k.mtime == mtime;
        // A coarse-grained filesystem (FAT counts in 2 s steps) hands back
        // whole seconds: an equal stamp there is no proof, so the size+mtime
        // fast path only settles it when the stamp carries sub-second
        // detail. "Doubt" is defined, not felt.
        let coarse = mtime.nanos == 0;
        let doubt = k.size == size && (coarse || k.mtime.seconds_apart(mtime) <= MTIME_DOUBT_SECS);
        if same_stat && !coarse && k.exec == exec {
            return Ok(());
        }
        if same_stat || doubt {
            // The quick path is not trusted here: hash to decide. Identical
            // content is NOT a change (the mtime record still follows the
            // disk); an exec flip alone is one.
            let hash = hash_file(path)?;
            if hash == k.hash && k.exec == exec {
                report.touched.push(seen(hash));
                return Ok(());
            }
            report.changed.push(seen(hash));
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
        report.touched.push(seen(hash));
        return Ok(());
    }
    report.changed.push(seen(hash));
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

    /// What the user did wrong is reported with a reason; what is OURS is
    /// simply not there. The engine's own staging directory must never show
    /// up in a card's ignored list.
    #[test]
    fn exclusions_are_reported_but_our_own_staging_is_silent() {
        let mut f = Fixture::new();
        f.write("ok.txt", "x");
        f.write(".1device.tmp/staged", "x");
        f.write(".partial.1dtmp", "x");
        f.write("bad:name.txt", "x");
        #[cfg(unix)]
        std::os::unix::fs::symlink(f.root.path().join("ok.txt"), f.root.path().join("link.txt"))
            .expect("symlink");
        let report = f.scan();
        let reasons: Vec<&str> = report.ignored.iter().map(|i| i.reason).collect();
        assert!(reasons.contains(&"separator character"), "{reasons:?}");
        #[cfg(unix)]
        assert!(reasons.contains(&"symlink"), "{reasons:?}");
        assert!(
            !reasons.contains(&"reserved staging name"),
            "our own staging must not accuse the user: {reasons:?}"
        );
        let names: Vec<&str> = report.ignored.iter().map(|i| i.name.as_str()).collect();
        assert!(
            !names.iter().any(|n| n.contains("1device.tmp")),
            "{names:?}"
        );
        assert!(!names.iter().any(|n| n.contains("1dtmp")), "{names:?}");
        assert!(f.index.get("ok.txt").is_some());
        assert!(f.index.get(".1device.tmp/staged").is_none());
        assert!(f.index.get(".partial.1dtmp").is_none());
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
