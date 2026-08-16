// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

//! The per-set file index (doc/sync-engine.md, section 3): one entry per
//! wire path, carrying the version vector; tombstones keep their vv and
//! stay (compaction is a later, additive concern). The index never claims
//! what the disk does not hold - the APPLY order guarantees it (section
//! 6), and the rescan re-derives the truth from the tree.

use std::collections::BTreeMap;
use std::time::SystemTime;

use serde_json::{Value, json};

use crate::vv::Vv;
use crate::wirepath::check_wire_path;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EntryKind {
    File,
    Dir,
}

impl EntryKind {
    fn as_str(self) -> &'static str {
        match self {
            EntryKind::File => "file",
            EntryKind::Dir => "dir",
        }
    }

    fn parse(s: &str) -> Option<EntryKind> {
        match s {
            "file" => Some(EntryKind::File),
            "dir" => Some(EntryKind::Dir),
            _ => None,
        }
    }
}

/// A modification time as the filesystem reported it back: seconds and
/// nanoseconds since the epoch (negative seconds for pre-1970 stamps,
/// which FAT and archives do produce).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Mtime {
    pub secs: i64,
    pub nanos: u32,
}

impl Mtime {
    pub fn of(time: SystemTime) -> Mtime {
        match time.duration_since(SystemTime::UNIX_EPOCH) {
            Ok(d) => Mtime {
                secs: i64::try_from(d.as_secs()).unwrap_or(i64::MAX),
                nanos: d.subsec_nanos(),
            },
            Err(e) => {
                let d = e.duration();
                // Round toward the epoch: exactness below the second does
                // not matter on the far side of 1970.
                Mtime {
                    secs: -i64::try_from(d.as_secs()).unwrap_or(i64::MAX),
                    nanos: 0,
                }
            }
        }
    }

    /// Whole seconds of distance, for the granularity doubt (FAT counts in
    /// 2 s steps; "doubt" is defined, not felt).
    pub fn seconds_apart(self, other: Mtime) -> u64 {
        self.secs.abs_diff(other.secs)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    /// Wire path: `/`-separated, NFC, gate-checked.
    pub path: String,
    pub kind: EntryKind,
    pub size: u64,
    pub mtime: Mtime,
    pub exec: bool,
    /// BLAKE3 of the content, lowercase hex; empty for directories and
    /// tombstones.
    pub hash: String,
    pub vv: Vv,
    pub deleted: bool,
}

impl Entry {
    pub fn to_value(&self) -> Value {
        json!({
            "path": self.path,
            "kind": self.kind.as_str(),
            "size": self.size,
            "mtime": { "secs": self.mtime.secs, "nanos": self.mtime.nanos },
            "exec": self.exec,
            "hash": self.hash,
            "vv": self.vv.to_value(),
            "deleted": self.deleted,
        })
    }

    /// Strict parse, shared by the wire (entries pages) and the index file:
    /// the path passes the full gate, the vv its own. `None` = malformed.
    pub fn from_value(value: &Value) -> Option<Entry> {
        let map = value.as_object()?;
        if map.len() != 8 {
            return None;
        }
        let path = map.get("path")?.as_str()?;
        if check_wire_path(path).is_err() {
            return None;
        }
        let hash = map.get("hash")?.as_str()?;
        if !hash.is_empty()
            && !(hash.len() == 64
                && hash
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)))
        {
            return None;
        }
        let mtime = map.get("mtime")?.as_object()?;
        if mtime.len() != 2 {
            return None;
        }
        Some(Entry {
            path: path.to_string(),
            kind: EntryKind::parse(map.get("kind")?.as_str()?)?,
            size: map.get("size")?.as_u64()?,
            mtime: Mtime {
                secs: mtime.get("secs")?.as_i64()?,
                nanos: mtime
                    .get("nanos")?
                    .as_u64()
                    .filter(|n| *n < 1_000_000_000)? as u32,
            },
            exec: map.get("exec")?.as_bool()?,
            hash: hash.to_string(),
            vv: Vv::from_value(map.get("vv")?)?,
            deleted: map.get("deleted")?.as_bool()?,
        })
    }
}

/// The set's index, keyed by wire path.
#[derive(Clone, Debug, Default)]
pub struct SetIndex {
    entries: BTreeMap<String, Entry>,
}

impl SetIndex {
    pub fn new() -> SetIndex {
        SetIndex::default()
    }

    pub fn get(&self, path: &str) -> Option<&Entry> {
        self.entries.get(path)
    }

    pub fn upsert(&mut self, entry: Entry) {
        self.entries.insert(entry.path.clone(), entry);
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = &Entry> {
        self.entries.values()
    }

    /// The live (non-tombstone) entries.
    pub fn live(&self) -> impl Iterator<Item = &Entry> {
        self.entries.values().filter(|e| !e.deleted)
    }

    /// The delta rule (doc/sync-engine.md, section 3): every entry not
    /// componentwise covered by the peer's ADVERTISED vv, whoever authored
    /// it - a relay forwards third-party entries.
    pub fn not_covered_by<'i>(&'i self, peer_vv: &'i Vv) -> impl Iterator<Item = &'i Entry> {
        self.entries.values().filter(|e| !peer_vv.covers(&e.vv))
    }

    /// The highest value of `component` across the index: the clock-resume
    /// floor for one's own current generation.
    pub fn max_component(&self, component: &str) -> u64 {
        self.entries
            .values()
            .map(|e| e.vv.get(component))
            .max()
            .unwrap_or(0)
    }

    pub fn to_value(&self) -> Value {
        json!({ "entries": self.entries.values().map(Entry::to_value).collect::<Vec<_>>() })
    }

    /// Loads what `to_value` wrote. `None` = corrupt; the caller treats a
    /// lost index as loud-but-safe (the lease keeps the clock monotonic and
    /// the rescan re-detects the tree), but a PRESENT-and-garbled one fails
    /// the set loudly instead of guessing.
    pub fn from_value(value: &Value) -> Option<SetIndex> {
        let mut index = SetIndex::new();
        for entry in value.get("entries")?.as_array()? {
            let entry = Entry::from_value(entry)?;
            index.entries.insert(entry.path.clone(), entry);
        }
        Some(index)
    }
}

#[cfg(test)]
mod tests {
    use crate::vv::component;

    use super::*;

    const A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    fn entry(path: &str, clock: u64) -> Entry {
        let mut vv = Vv::new();
        vv.set(&component(A, 1), clock);
        Entry {
            path: path.to_string(),
            kind: EntryKind::File,
            size: 3,
            mtime: Mtime {
                secs: 1000,
                nanos: 500,
            },
            exec: false,
            hash: "a".repeat(64),
            vv,
            deleted: false,
        }
    }

    #[test]
    fn entries_round_trip_and_parse_strictly() {
        let e = entry("dir/a.txt", 3);
        assert_eq!(Entry::from_value(&e.to_value()), Some(e.clone()));

        for (field, bad) in [
            ("path", json!("../escape")),
            ("path", json!("")),
            ("kind", json!("link")),
            ("size", json!(-1)),
            ("hash", json!("zz")),
            ("hash", json!("A".repeat(64))),
            ("exec", json!(1)),
            ("deleted", json!("no")),
            ("vv", json!({ "bad": 1 })),
        ] {
            let mut v = e.to_value();
            v[field] = bad.clone();
            assert!(
                Entry::from_value(&v).is_none(),
                "should refuse {field} = {bad}"
            );
        }
        let mut extra = e.to_value();
        extra["more"] = json!(1);
        assert!(Entry::from_value(&extra).is_none());
    }

    #[test]
    fn the_delta_is_everything_the_peer_does_not_cover() {
        let mut index = SetIndex::new();
        index.upsert(entry("a", 1));
        index.upsert(entry("b", 5));
        let mut peer = Vv::new();
        peer.set(&component(A, 1), 3);
        let delta: Vec<&str> = index
            .not_covered_by(&peer)
            .map(|e| e.path.as_str())
            .collect();
        assert_eq!(delta, ["b"]);
        assert_eq!(index.max_component(&component(A, 1)), 5);
    }

    #[test]
    fn the_index_round_trips() {
        let mut index = SetIndex::new();
        index.upsert(entry("a", 1));
        let mut tomb = entry("gone", 2);
        tomb.deleted = true;
        tomb.hash = String::new();
        index.upsert(tomb);
        let reloaded = SetIndex::from_value(&index.to_value()).expect("reload");
        assert_eq!(reloaded.len(), 2);
        assert_eq!(reloaded.live().count(), 1);
        assert!(SetIndex::from_value(&json!({ "entries": [{}] })).is_none());
    }

    /// Sub-second precision is the PLATFORM's, not ours: Windows counts in
    /// 100-nanosecond units, so a stamp is asserted at a precision every
    /// filesystem can actually hold (which is the whole reason the index
    /// records the value read BACK from the disk rather than the one we
    /// asked for).
    #[test]
    fn mtimes_capture_the_filesystem_report() {
        let m = Mtime::of(SystemTime::UNIX_EPOCH + std::time::Duration::new(10, 500_000_000));
        assert_eq!(
            m,
            Mtime {
                secs: 10,
                nanos: 500_000_000
            }
        );
        let before = Mtime::of(SystemTime::UNIX_EPOCH - std::time::Duration::new(5, 0));
        assert_eq!(before.secs, -5);
        assert_eq!(m.seconds_apart(before), 15);
    }
}
