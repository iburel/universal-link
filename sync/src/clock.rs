// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

//! Clock durability (doc/sync-engine.md, section 3): vv monotonicity is
//! what correctness rests on, so `clock[self]` is never trusted to a file
//! written "often enough". A tiny lease persists the generation and
//! `reserved = clock + 1000`, fsynced BEFORE any clock in that block is
//! issued: a crash can waste up to 1000 values, never reuse one.
//!
//! The lease survives `sync.leave` and decline (leaving keeps local files;
//! it keeps the clock too). A FRESH lease - accept on a wiped device, the
//! reinstall case - mints a NEW generation (`gen` = unix seconds at
//! seeding), and a new generation is a new vv component by construction:
//! no seeding from heads is needed for safety.

use std::io;
use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use crate::index::SetIndex;
use crate::store::write_private_atomic;
use crate::vv::component;

const LEASE_FILE: &str = "clock.lease";

/// Values wasted at worst per crash; the price of not fsyncing every tick.
const RESERVE_BLOCK: u64 = 1000;

/// The canonical integer ceiling, minus a full block of slack: a clock that
/// climbs here is broken beyond apology, and erroring beats wrapping.
const CLOCK_CEILING: u64 = (1 << 53) - 1 - RESERVE_BLOCK;

pub struct SetClock {
    lease_path: PathBuf,
    generation: u64,
    current: u64,
    reserved: u64,
}

impl SetClock {
    /// Opens the set's clock. A present lease resumes its generation, at
    /// the reservation or the index's own high-water mark, whichever is
    /// higher; an ABSENT lease (first accept, or the reinstall that lost
    /// everything) mints a new generation stamped with the current unix
    /// second and starts at zero.
    pub fn open(
        set_dir: &Path,
        index: &SetIndex,
        self_node: &str,
        unix_now: u64,
    ) -> io::Result<SetClock> {
        let lease_path = set_dir.join(LEASE_FILE);
        let mut clock = match std::fs::read_to_string(&lease_path) {
            Ok(text) => {
                let corrupt = || io::Error::other("corrupt clock.lease");
                let doc: Value = serde_json::from_str(&text).map_err(|_| corrupt())?;
                let generation = doc.get("gen").and_then(Value::as_u64).ok_or_else(corrupt)?;
                let reserved = doc
                    .get("reserved")
                    .and_then(Value::as_u64)
                    .ok_or_else(corrupt)?;
                let floor = index.max_component(&component(self_node, generation));
                SetClock {
                    lease_path,
                    generation,
                    current: reserved.max(floor),
                    reserved,
                }
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => SetClock {
                lease_path,
                generation: unix_now,
                current: 0,
                reserved: 0,
            },
            Err(e) => return Err(e),
        };
        // The invariant, on every path out of here: `reserved` on DISK
        // covers every value this incarnation may issue before its next
        // persist.
        if clock.current >= clock.reserved {
            clock.reserve()?;
        }
        Ok(clock)
    }

    fn reserve(&mut self) -> io::Result<()> {
        if self.current > CLOCK_CEILING {
            return Err(io::Error::other("set clock exhausted"));
        }
        self.reserved = self.current + RESERVE_BLOCK;
        let doc = json!({ "gen": self.generation, "reserved": self.reserved });
        write_private_atomic(&self.lease_path, doc.to_string().as_bytes())
    }

    /// Issues the next clock value, extending the persisted reservation
    /// FIRST whenever the block runs out.
    pub fn tick(&mut self) -> io::Result<u64> {
        if self.current + 1 > self.reserved {
            self.reserve()?;
        }
        self.current += 1;
        Ok(self.current)
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// The vv component key this clock feeds.
    pub fn self_component(&self, self_node: &str) -> String {
        component(self_node, self.generation)
    }

    /// The last issued value (0 before any tick of this incarnation).
    pub fn current(&self) -> u64 {
        self.current
    }
}

#[cfg(test)]
mod tests {
    use crate::index::SetIndex;

    use super::*;

    const A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    #[test]
    fn a_fresh_lease_mints_a_generation_and_reserves_before_issuing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let index = SetIndex::new();
        let mut clock = SetClock::open(dir.path(), &index, A, 1_700_000_000).expect("open");
        assert_eq!(clock.generation(), 1_700_000_000);
        assert_eq!(clock.tick().expect("tick"), 1);
        assert_eq!(clock.tick().expect("tick"), 2);
        // The reservation hit the disk before any issue.
        let lease: Value =
            serde_json::from_str(&std::fs::read_to_string(dir.path().join(LEASE_FILE)).unwrap())
                .expect("lease json");
        assert_eq!(lease["reserved"], json!(RESERVE_BLOCK));
    }

    #[test]
    fn a_crash_never_reuses_a_value() {
        let dir = tempfile::tempdir().expect("tempdir");
        let index = SetIndex::new();
        let mut clock = SetClock::open(dir.path(), &index, A, 1_700_000_000).expect("open");
        for _ in 0..5 {
            clock.tick().expect("tick");
        }
        let last = clock.current();
        drop(clock); // The crash: nothing persisted since the reservation.
        let mut reopened = SetClock::open(dir.path(), &index, A, 1_700_009_999).expect("reopen");
        assert_eq!(reopened.generation(), 1_700_000_000, "the lease survives");
        assert!(
            reopened.tick().expect("tick") > last,
            "resumed clocks must exceed every issued value"
        );
    }

    #[test]
    fn the_index_high_water_mark_counts_too() {
        // An index newer than the lease (a restored lease file, an older
        // backup): the clock resumes above BOTH.
        let dir = tempfile::tempdir().expect("tempdir");
        let index = SetIndex::new();
        let mut clock = SetClock::open(dir.path(), &index, A, 1_700_000_000).expect("open");
        clock.tick().expect("tick");
        let key = clock.self_component(A);
        drop(clock);
        let mut index = SetIndex::new();
        let mut vv = crate::vv::Vv::new();
        vv.set(&key, 5_000);
        index.upsert(crate::index::Entry {
            path: "a".into(),
            kind: crate::index::EntryKind::File,
            size: 0,
            mtime: crate::index::Mtime::default(),
            exec: false,
            hash: String::new(),
            vv,
            deleted: true,
        });
        let mut reopened = SetClock::open(dir.path(), &index, A, 1_700_000_001).expect("reopen");
        assert!(reopened.tick().expect("tick") > 5_000);
    }

    #[test]
    fn losing_the_lease_is_a_new_generation() {
        let dir = tempfile::tempdir().expect("tempdir");
        let index = SetIndex::new();
        let clock = SetClock::open(dir.path(), &index, A, 1_700_000_000).expect("open");
        let old_gen = clock.generation();
        drop(clock);
        std::fs::remove_file(dir.path().join(LEASE_FILE)).expect("wipe");
        let reopened = SetClock::open(dir.path(), &index, A, 1_700_000_777).expect("reopen");
        assert_ne!(reopened.generation(), old_gen);
        assert_eq!(reopened.generation(), 1_700_000_777);
    }

    #[test]
    fn a_corrupt_lease_is_an_error_not_a_fresh_generation() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join(LEASE_FILE), "garbage").expect("write");
        assert!(SetClock::open(dir.path(), &SetIndex::new(), A, 1).is_err());
    }
}
