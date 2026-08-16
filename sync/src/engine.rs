// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

//! The engine core, sans I/O of its own beyond the store: gestures come in
//! as method calls, dialect messages come in through [`Engine::on_message`],
//! and everything the wire should carry comes back as [`Outgoing`] values
//! the orchestrator hands to `peers.send`. Deliberately synchronous and
//! deterministic: two engines exchanging return values IS the protocol, and
//! the tests drive exactly that.
//!
//! The rounds (doc/sync-engine.md, section 4): a round-opening head
//! declares where we stand; the answer declares and carries pages (records
//! always - the answer to ANY head includes the records the answerer holds
//! about the sender - and the entries delta when the answerer is an active
//! member serving an active member); absorbing an answer to our opener
//! sends the symmetric leg; an answer to an answer closes the round.
//! Watermarks advance per peer only when a COMPLETE leg has been absorbed
//! and persisted, and records-only legs advance nothing.

use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use crate::clock::SetClock;
use crate::index::{Entry, SetIndex};
use crate::membership::{Absorb, Effective, SetMembership};
use crate::protocol::{DIALECT, HeadPosition, Message, descriptor_hash, paginate};
use crate::records::{
    ConflictRecord, Endorsement, InvitedRecord, MemberRecord, MemberStatus, Record, SetDescriptor,
    SetKind, valid_node_id, valid_set_id,
};
use crate::scan::ScanReport;
use crate::store::Store;
use crate::vv::Vv;

/// One message to hand to `peers.send`, targeted by node_id (the
/// orchestrator translates to the Core's device_id).
#[derive(Clone, Debug)]
pub struct Outgoing {
    pub to: String,
    pub payload: Value,
}

/// What the engine wants done. The engine core never touches the network
/// or the Core API itself: it returns effects, the orchestrator executes
/// them (and the tests simulate them), feeding results back through the
/// `on_*` callbacks.
#[derive(Clone, Debug)]
pub enum Effect {
    /// Hand to `peers.send`.
    Send(Outgoing),
    /// Call `transactions.publish` on these absolute paths, then
    /// [`Engine::on_published`] (or [`Engine::on_publish_failed`]).
    Publish {
        to: String,
        set_id: String,
        need_id: u64,
        paths: Vec<PathBuf>,
    },
    /// Call `transactions.revoke`: the peer said done, or the slot was
    /// superseded.
    Revoke { tx_id: String },
    /// Call `transactions.adopt` then `transactions.fill` into `staging`
    /// (each file under its published basename), then
    /// [`Engine::on_pull_started`] with the transfer id (or
    /// [`Engine::on_pull_failed`]).
    AdoptFill {
        from: String,
        set_id: String,
        need_id: u64,
        tx_id: String,
        /// published wire name (basename) -> set path.
        files: BTreeMap<String, String>,
        staging: PathBuf,
    },
}

impl Effect {
    fn send(to: &str, message: &Message) -> Effect {
        Effect::Send(Outgoing {
            to: to.to_string(),
            payload: message.to_value(),
        })
    }
}

/// Concurrent published transactions per peer (the letter, section 5): a
/// third need from the same peer supersedes its oldest.
const PUBLISHED_PER_PEER: usize = 2;

/// Outstanding needs per peer on the needer side, mirroring the source cap.
const NEEDS_PER_PEER: usize = 2;

/// Files per need chunk, besides the unique-basename rule and the manifest
/// cap far above it.
const NEED_CHUNK_FILES: usize = 256;

/// Bytes per need chunk: a sane bound on one pull.
const NEED_CHUNK_BYTES: u64 = 4 * 1024 * 1024 * 1024;

/// Retries granted to a one-shot before giving up: invitations, stubs and
/// introductions all use it. Generous - each attempt costs one message on a
/// reachability change or a safety tick.
const RETRY_BUDGET: u32 = 64;

/// How long a partial round's buffered pages wait for their stragglers
/// before the next round has to resend, in seconds.
const ROUND_TTL_SECS: u64 = 60;

/// Concurrent remote versions parked per path: two is the honest maximum
/// (ours is in the index); the rest is hostile or pathological.
const PENDING_PER_PATH: usize = 4;

/// Minimum seconds between round openers toward one peer for one set, when
/// nothing changed: reachability flaps must not turn into message storms.
const OPEN_INTERVAL_SECS: u64 = 5;

/// A received invitation's claim, kept for the consent card.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InviteClaim {
    pub inviter: String,
    pub entries: u64,
    pub total_size: u64,
}

/// One buffered records page: membership records, endorsements, conflict
/// records.
type RecordsPage = (Vec<Record>, Vec<Endorsement>, Vec<ConflictRecord>);

struct RoundBuffer {
    head: Option<(HeadPosition, Option<u64>)>,
    records: BTreeMap<u64, RecordsPage>,
    entries: BTreeMap<u64, Vec<Entry>>,
    born_at: u64,
}

pub struct SetState {
    pub membership: SetMembership,
    pub index: SetIndex,
    pub clock: SetClock,
    /// The local root; `None` until the accept gesture picked one (an
    /// invitation creates the state, consent gives it a place on disk).
    pub root: Option<PathBuf>,
    /// Entries absorbed but not yet reconciled with the disk: what drives
    /// the needs (brick 5), survives crashes, and is invisible to the
    /// rescan's tombstoning.
    pub pending: BTreeMap<String, Vec<Entry>>,
    /// Per peer: the head vv of the last COMPLETE leg absorbed from it.
    pub watermarks: BTreeMap<String, Vv>,
    /// The inviter's claim, while our own status is `Invited`.
    pub invite_claim: Option<InviteClaim>,
    inflight: BTreeMap<(String, u64), RoundBuffer>,
    /// Outgoing invitations awaiting their ack: invitee, retries left.
    open_invites: BTreeMap<String, u32>,
    /// Introduction attempts left, per unpinned target.
    intro_attempts: BTreeMap<String, u32>,
    /// After our decline or leave: the members still owed the proof.
    stub: Option<Stub>,
    /// Members owed a records-only opener because a local gesture changed
    /// our records (pause, resume, accept, a new invitation): drained by
    /// the pump, best-effort - the safety tick and the answers-echo rule
    /// cover a lost push. Ephemeral on purpose.
    announce: BTreeSet<String>,
    /// Throttle and change detection for round openers.
    last_opened: BTreeMap<String, (u64, Vv)>,
    /// Needer side: the needs in flight, by need_id. Ephemeral - a restart
    /// re-needs from the pending set; only the counter persists (the
    /// source keys its published slots by (peer, need_id)).
    needs: BTreeMap<u64, NeedState>,
    /// Persisted: (peer, need_id) must never repeat across restarts.
    need_counter: u64,
    /// Source side: the published transactions, keyed (peer, need_id).
    /// Ephemeral by nature - a transaction dies with the engine.
    published: BTreeMap<(String, u64), String>,
    /// Source side: a publish the orchestrator is running, with the
    /// basename map the offer will carry.
    publishing: BTreeMap<(String, u64), BTreeMap<String, String>>,
    /// Verified staged bytes whose final rename keeps failing (the Office
    /// case): kept, persisted, retried - never re-transferred.
    parked: BTreeMap<String, ParkedEntry>,
    /// The conflict records, keyed (path, conflict_id): first-class
    /// gossiped state, `resolved` a kept tombstone.
    conflicts: BTreeMap<(String, String), ConflictRecord>,
    /// Bytes landed while the invitation's claim still stands: the consent
    /// guard measures against it. Persisted.
    landed_bytes: u64,
    /// The last rescan's exclusions (the card's ignored list). In-memory:
    /// the next rescan recomputes it.
    ignored: Vec<crate::scan::Ignored>,
    /// The watcher could not be installed: degraded to periodic scanning,
    /// said on the card. Pushed by the orchestrator.
    watch_degraded: bool,
}

#[derive(Clone, Debug)]
struct ParkedEntry {
    staged: PathBuf,
    entry: Entry,
    reason: String,
}

/// What one staged file's apply did.
enum Apply {
    Landed,
    /// Concurrent with a local LIVE version: the keep-both machinery's
    /// case.
    Conflict,
    Failed,
}

struct NeedState {
    peer: String,
    /// Set paths still expected under this need.
    paths: BTreeSet<String>,
    pull: Option<ActivePull>,
}

struct ActivePull {
    tx_id: String,
    staging: PathBuf,
    /// published wire name (basename) -> set path.
    files: BTreeMap<String, String>,
    transfer: Option<String>,
}

#[derive(Clone, Debug)]
struct Stub {
    remaining: BTreeSet<String>,
    attempts: u32,
}

pub struct Engine {
    store: Store,
    self_node: String,
    sets: BTreeMap<String, SetState>,
    round_counter: u64,
    /// The rounds WE sent, by id: `true` when the round was itself an
    /// answer (its own answer closes the exchange). Bounded.
    sent_rounds: BTreeMap<u64, bool>,
    /// The directory as the orchestrator last saw it: translations, names,
    /// reachability, platforms. The engine core never asks the Core - the
    /// view is pushed in.
    directory: DirectoryView,
}

/// What the engine knows of the device directory, pushed by the
/// orchestrator from every `devices.list` snapshot.
#[derive(Clone, Debug, Default)]
pub struct DirectoryView {
    /// node_id -> the Core's device_id (what the facade and `peers.send`
    /// speak).
    pub device_of: BTreeMap<String, String>,
    /// node_id -> display name.
    pub names: BTreeMap<String, String>,
    /// Nodes with a route right now.
    pub reachable: BTreeSet<String>,
    /// node_id -> platform (the computers-only rule needs it).
    pub platforms: BTreeMap<String, String>,
    /// node_id -> last_seen, relayed verbatim into the cards.
    pub last_seen: BTreeMap<String, Value>,
}

impl Engine {
    /// Opens the engine over its persisted state. `self_node` is this
    /// device's node_id, resolved by the orchestrator from the directory.
    pub fn open(store: Store, self_node: String, now: u64) -> io::Result<Engine> {
        let mut sets = BTreeMap::new();
        for (set_id, meta) in store.load_metas()? {
            let state = SetState::from_meta(&store, &set_id, &meta, &self_node, now)?;
            state.sweep_staging();
            sets.insert(set_id, state);
        }
        Ok(Engine {
            store,
            self_node,
            sets,
            round_counter: 0,
            sent_rounds: BTreeMap::new(),
            directory: DirectoryView::default(),
        })
    }

    /// The orchestrator's directory push.
    pub fn set_directory(&mut self, directory: DirectoryView) {
        self.directory = directory;
    }

    /// The orchestrator's word on whether this set's watcher could be
    /// installed: a set reduced to periodic scanning says so on its card.
    pub fn set_watch_degraded(&mut self, set_id: &str, degraded: bool) {
        if let Some(state) = self.sets.get_mut(set_id) {
            state.set_watch_degraded(degraded);
        }
    }

    pub fn self_node(&self) -> &str {
        &self.self_node
    }

    pub fn set(&self, set_id: &str) -> Option<&SetState> {
        self.sets.get(set_id)
    }

    #[cfg(test)]
    pub(crate) fn set_mut(&mut self, set_id: &str) -> Option<&mut SetState> {
        self.sets.get_mut(set_id)
    }

    #[cfg(test)]
    pub(crate) fn sync_pub(&self) -> String {
        self.store.identity().public_hex()
    }

    pub fn set_ids(&self) -> Vec<String> {
        self.sets.keys().cloned().collect()
    }

    /// The `sync.status` snapshot (doc/sync-engine.md, section 10): the
    /// AUTHORITATIVE state the notifications merely echo. A GUI starting
    /// after the fact must be able to render everything - conflicts,
    /// ignored, blocked - from this alone.
    pub fn status(&self) -> Value {
        let mut sets = Vec::new();
        let mut invitations = Vec::new();
        for (set_id, state) in &self.sets {
            match state.membership.effective(&self.self_node) {
                Effective::Invited => {
                    let claim = state.invite_claim.clone().unwrap_or(InviteClaim {
                        inviter: String::new(),
                        entries: 0,
                        total_size: 0,
                    });
                    invitations.push(json!({
                        "set_id": set_id,
                        "name": state.membership.descriptor.name,
                        "kind": kind_str(state.membership.descriptor.kind),
                        "device_id": self.device_id_of(&claim.inviter),
                        "entries": claim.entries,
                        "total_size": claim.total_size,
                        "default_path": format!("~/1Device/{}", state.membership.descriptor.name),
                    }));
                }
                Effective::Active | Effective::Paused => {
                    sets.push(self.card(set_id, state));
                }
                // Declined, left, or nothing verifiable: no card - the set
                // left this device.
                _ => {}
            }
        }
        json!({ "sets": sets, "invitations": invitations })
    }

    fn device_id_of(&self, node: &str) -> String {
        self.directory
            .device_of
            .get(node)
            .cloned()
            .unwrap_or_else(|| node.to_string())
    }

    fn card(&self, set_id: &str, state: &SetState) -> Value {
        let effective_self = state.membership.effective(&self.self_node);
        let open_conflicts: Vec<&ConflictRecord> =
            state.conflicts.values().filter(|r| !r.resolved).collect();

        let mut devices = Vec::new();
        let mut any_other_active = false;
        for node in state.membership.device_ids() {
            let membership = match state.membership.effective(&node) {
                Effective::Active => "active",
                Effective::Paused => "paused",
                Effective::Invited => "invited",
                Effective::Declined => "declined",
                Effective::Left => "left",
                Effective::Unknown => continue,
            };
            if node != self.self_node && membership == "active" {
                any_other_active = true;
            }
            let behind = match state.watermarks.get(&node) {
                _ if node == self.self_node => 0,
                Some(watermark) => state
                    .index
                    .iter()
                    .filter(|e| !watermark.covers(&e.vv))
                    .count(),
                None => state.index.len(),
            };
            let sync = if node == self.self_node {
                "up_to_date"
            } else if !self.directory.reachable.contains(&node) {
                "offline"
            } else if behind > 0 {
                "behind"
            } else {
                "up_to_date"
            };
            devices.push(json!({
                "device_id": self.device_id_of(&node),
                "membership": membership,
                "sync": sync,
                "behind": behind,
                "last_seen": self.directory.last_seen.get(&node).cloned().unwrap_or(Value::Null),
            }));
        }

        let conflicts: Vec<Value> = open_conflicts
            .iter()
            .map(|record| {
                let mut versions = Vec::new();
                for path in [record.path.as_str(), record.path_on_disk.as_str()] {
                    if let Some(entry) = state.index.get(path).filter(|e| !e.deleted) {
                        versions.push(json!({
                            "version_id": version_id(entry),
                            "device_id": self.device_id_of(
                                &author_of(&entry.vv).unwrap_or_default(),
                            ),
                            "mtime": { "secs": entry.mtime.secs, "nanos": entry.mtime.nanos },
                            "size": entry.size,
                            "path_on_disk": path,
                        }));
                    }
                }
                json!({
                    "path": record.path,
                    "conflict_id": record.conflict_id,
                    "versions": versions,
                })
            })
            .collect();

        let state_str = if !open_conflicts.is_empty() {
            "conflicts"
        } else if effective_self == Effective::Paused {
            "paused"
        } else if !state.pending.is_empty() || !state.needs.is_empty() || !state.parked.is_empty() {
            "catching_up"
        } else if !any_other_active {
            "waiting"
        } else {
            "in_order"
        };
        let problem = if state.size_guard_tripped() {
            json!("size_exceeds_invitation")
        } else if state.watch_degraded {
            json!("watch_degraded")
        } else {
            Value::Null
        };

        json!({
            "set_id": set_id,
            "kind": kind_str(state.membership.descriptor.kind),
            "name": state.membership.descriptor.name,
            "path": state.root.as_ref().map(|p| p.to_string_lossy().into_owned()),
            "state": state_str,
            "problem": problem,
            "behind": state.pending.len(),
            "devices": devices,
            "conflicts": conflicts,
            "ignored": state
                .ignored
                .iter()
                .map(|i| json!({ "path": i.name, "reason": i.reason }))
                .collect::<Vec<_>>(),
            "blocked": state
                .parked
                .values()
                .map(|p| json!({ "path": p.entry.path, "reason": p.reason }))
                .collect::<Vec<_>>(),
        })
    }

    /// Does `path` equal, contain, or live inside any set root? One file,
    /// one set (SYNC_ROOT_OVERLAP).
    pub fn overlaps(&self, path: &Path) -> bool {
        self.sets.values().any(|state| {
            state.root.as_deref().is_some_and(|root| {
                root == path || root.starts_with(path) || path.starts_with(root)
            })
        })
    }

    /// Whether `node` may be invited at all: a computer of the account,
    /// not us. The mobile exclusion is the v1 rule, not a judgement.
    pub fn invite_eligible(&self, node: &str) -> Result<(), &'static str> {
        if node == self.self_node {
            return Err("oneself");
        }
        if !self.directory.device_of.contains_key(node) {
            return Err("unknown device");
        }
        if self.directory.platforms.get(node).map(String::as_str) == Some("android") {
            return Err("mobile");
        }
        Ok(())
    }

    /// The invitation state, for the facade's refusals.
    pub fn is_invited(&self, set_id: &str) -> bool {
        self.sets
            .get(set_id)
            .is_some_and(|state| state.membership.effective(&self.self_node) == Effective::Invited)
    }

    fn next_round(&mut self, answer: bool) -> u64 {
        self.round_counter += 1;
        self.sent_rounds.insert(self.round_counter, answer);
        while self.sent_rounds.len() > 512 {
            let oldest = *self.sent_rounds.keys().next().expect("non-empty");
            self.sent_rounds.remove(&oldest);
        }
        self.round_counter
    }

    // -----------------------------------------------------------------------
    // Gestures (called by the facade bricks and the tests).
    // -----------------------------------------------------------------------

    /// Creates a set over `root` and becomes its first active member. The
    /// caller has validated the root (exists, not overlapping); scanning is
    /// the caller's next step ([`Engine::rescan_set`]).
    pub fn create_set(
        &mut self,
        root: PathBuf,
        kind: SetKind,
        name: String,
        now: u64,
    ) -> io::Result<String> {
        let set_id = mint_set_id();
        let descriptor = SetDescriptor::create(
            set_id.clone(),
            kind,
            name,
            self.self_node.clone(),
            now,
            self.store.identity(),
        );
        let mut membership = SetMembership::new(descriptor);
        membership.pin_direct(&self.self_node, &self.store.identity().public_hex());
        let self_node = self.self_node.clone();
        let mut state = SetState::fresh(
            &self.store,
            &set_id,
            membership,
            Some(root),
            &self_node,
            now,
        )?;
        let record = self.sign_own(&mut state, MemberStatus::Active, now);
        debug_assert!(matches!(record, Absorb::Absorbed));
        state.persist(&self.store, &set_id)?;
        self.sets.insert(set_id.clone(), state);
        Ok(set_id)
    }

    /// Registers an invitation to send: the messages flow from [`Engine::pump`]
    /// until the invitee acks (or the budget runs out). Only an active
    /// member invites - the facade will phrase the refusal.
    pub fn invite(&mut self, set_id: &str, invitee: &str, now: u64) -> io::Result<()> {
        if !valid_node_id(invitee) || invitee == self.self_node {
            return Err(io::Error::other("not an invitable device"));
        }
        let self_node = self.self_node.clone();
        let state = self
            .sets
            .get_mut(set_id)
            .ok_or_else(|| io::Error::other("unknown set"))?;
        if state.membership.effective(&self_node) != Effective::Active {
            return Err(io::Error::other("only an active member invites"));
        }
        let supersedes = state.membership.next_seq(invitee).saturating_sub(1);
        let invited = InvitedRecord::sign_invite(
            set_id.to_string(),
            invitee.to_string(),
            self_node.clone(),
            supersedes,
            now,
            self.store.identity(),
        );
        // Our own signed word absorbs at home first: the invitation exists
        // even if the invite message takes days to land.
        let absorbed = state.membership.absorb(&Record::Invited(invited));
        debug_assert!(matches!(absorbed, Absorb::Absorbed));
        state.open_invites.insert(invitee.to_string(), RETRY_BUDGET);
        state.announce_to_members(&self_node);
        state.persist(&self.store, set_id)?;
        Ok(())
    }

    /// The local consent: sign `active` behind the absorbed invitation and
    /// take a root. The first ordinary round pulls everything after that.
    pub fn accept(&mut self, set_id: &str, root: PathBuf, now: u64) -> io::Result<()> {
        let self_node = self.self_node.clone();
        let state = self
            .sets
            .get_mut(set_id)
            .ok_or_else(|| io::Error::other("unknown set"))?;
        if state.membership.effective(&self_node) != Effective::Invited {
            return Err(io::Error::other("not invited"));
        }
        state.root = Some(root);
        let mut state = self.sets.remove(set_id).expect("present");
        let absorbed = self.sign_own(&mut state, MemberStatus::Active, now);
        debug_assert!(matches!(absorbed, Absorb::Absorbed));
        state.announce_to_members(&self.self_node.clone());
        state.persist(&self.store, set_id)?;
        self.sets.insert(set_id.to_string(), state);
        Ok(())
    }

    /// Declines the invitation (or leaves, or pauses, or resumes: the four
    /// self-signed gestures share this path). Terminal gestures grow a stub
    /// that keeps proving itself to each member until echoed.
    pub fn sign_status(&mut self, set_id: &str, status: MemberStatus, now: u64) -> io::Result<()> {
        let self_node = self.self_node.clone();
        let mut state = self
            .sets
            .remove(set_id)
            .ok_or_else(|| io::Error::other("unknown set"))?;
        if status == MemberStatus::Active {
            // Resuming is the user gesture that lifts the consent guard.
            state.invite_claim = None;
        }
        let absorbed = self.sign_own(&mut state, status, now);
        debug_assert!(matches!(absorbed, Absorb::Absorbed));
        if status.is_terminal() {
            let remaining: BTreeSet<String> = state
                .membership
                .device_ids()
                .into_iter()
                .filter(|n| *n != self_node)
                .filter(|n| {
                    matches!(
                        state.membership.effective(n),
                        Effective::Active | Effective::Paused | Effective::Invited
                    )
                })
                .collect();
            if !remaining.is_empty() {
                state.stub = Some(Stub {
                    remaining,
                    attempts: RETRY_BUDGET,
                });
            }
        }
        state.announce_to_members(&self_node);
        state.persist(&self.store, set_id)?;
        self.sets.insert(set_id.to_string(), state);
        Ok(())
    }

    /// `sync.resolve { set_id, path, keep }`: `keep` is a version_id (that
    /// content takes the plain path, the other copy is deleted) or "all"
    /// (both files stay). The record flips to resolved either way and the
    /// gesture's effects propagate as ordinary synced operations - nothing
    /// else ever deletes.
    pub fn resolve(&mut self, set_id: &str, path: &str, keep: &str) -> io::Result<()> {
        let self_node = self.self_node.clone();
        let state = self
            .sets
            .get_mut(set_id)
            .ok_or_else(|| io::Error::other("unknown set"))?;
        let record = state
            .conflicts
            .values()
            .find(|r| !r.resolved && r.path == path)
            .cloned()
            .ok_or_else(|| io::Error::other("no open conflict at this path"))?;
        let key = state.clock.self_component(&self_node);
        if keep != "all" {
            let root = state
                .root
                .clone()
                .ok_or_else(|| io::Error::other("no root"))?;
            let join = |wire: &str| {
                let mut p = root.clone();
                for c in wire.split('/') {
                    p.push(c);
                }
                p
            };
            let plain = state.index.get(path).cloned();
            let copy = state.index.get(&record.path_on_disk).cloned();
            let plain_vid = plain.as_ref().filter(|e| !e.deleted).map(version_id);
            let copy_vid = copy.as_ref().filter(|e| !e.deleted).map(version_id);
            let tombstone = |state: &mut SetState, entry: &Entry, key: &str| {
                let mut tomb = entry.clone();
                tomb.deleted = true;
                tomb.hash = String::new();
                tomb.size = 0;
                if let Ok(tick) = state.clock.tick() {
                    tomb.vv.set(key, tick);
                }
                state.index.upsert(tomb);
            };
            if plain_vid.as_deref() == Some(keep) {
                // Keep the plain path's occupant: the copy goes.
                if let Some(copy) = &copy {
                    let _ = std::fs::remove_file(join(&copy.path));
                    tombstone(state, copy, &key);
                }
            } else if copy_vid.as_deref() == Some(keep) {
                // The copy's content takes the plain path.
                let (Some(plain), Some(copy)) = (&plain, &copy) else {
                    return Err(io::Error::other("no such version"));
                };
                let _ = std::fs::remove_file(join(path));
                if std::fs::rename(join(&copy.path), join(path)).is_err() {
                    return Err(io::Error::other("cannot move the kept version"));
                }
                let mut kept = copy.clone();
                kept.path = path.to_string();
                kept.vv = plain.vv.clone();
                kept.vv.merge_max(&copy.vv);
                if let Ok(tick) = state.clock.tick() {
                    kept.vv.set(&key, tick);
                }
                state.index.upsert(kept);
                tombstone(state, copy, &key);
            } else {
                return Err(io::Error::other("no such version"));
            }
        }
        let resolved = ConflictRecord::sign(
            record.set_id.clone(),
            record.path.clone(),
            record.conflict_id.clone(),
            record.path_on_disk.clone(),
            Some(self_node.clone()),
            self_node,
            self.store.identity(),
        );
        state.absorb_conflict(&resolved);
        state.announce_to_members(&self.self_node.clone());
        state.persist(&self.store, set_id)?;
        Ok(())
    }

    fn sign_own(&mut self, state: &mut SetState, status: MemberStatus, now: u64) -> Absorb {
        // One's own key is one's own direct contact: a shadow state born
        // from an invitation has pinned only the inviter so far.
        state
            .membership
            .pin_direct(&self.self_node, &self.store.identity().public_hex());
        let record = MemberRecord::sign_own(
            state.membership.descriptor.set_id.clone(),
            self.self_node.clone(),
            status,
            state.membership.next_seq(&self.self_node),
            state.clock.generation(),
            now,
            self.store.identity(),
        );
        state.membership.absorb(&Record::Member(record))
    }

    /// Rescans the set's root against its index (brick 3's machinery),
    /// persisting the outcome.
    pub fn rescan_set(&mut self, set_id: &str) -> io::Result<ScanReport> {
        let self_node = self.self_node.clone();
        let state = self
            .sets
            .get_mut(set_id)
            .ok_or_else(|| io::Error::other("unknown set"))?;
        let root = state
            .root
            .clone()
            .ok_or_else(|| io::Error::other("no root accepted"))?;
        let kind = state.membership.descriptor.kind;
        let pending: BTreeSet<String> = state.pending.keys().cloned().collect();
        let key = state.clock.self_component(&self_node);
        let report = crate::scan::rescan(
            &root,
            kind,
            &mut state.index,
            &pending,
            &mut state.clock,
            &key,
        )?;
        let _ = state.drain_pending(&key);
        state.ignored = report.ignored.clone();
        state.persist(&self.store, set_id)?;
        Ok(report)
    }

    // -----------------------------------------------------------------------
    // The wire, inbound.
    // -----------------------------------------------------------------------

    /// One `peer.message` payload from the device `from` (node_id, already
    /// translated and transport-authenticated by the Core). Returns what to
    /// send back.
    pub fn on_message(&mut self, from: &str, payload: &Value, now: u64) -> Vec<Effect> {
        if from == self.self_node || !valid_node_id(from) {
            return Vec::new();
        }
        let Some(message) = Message::from_value(payload) else {
            // Foreign dialect or malformed: ignored by design.
            return Vec::new();
        };
        match message {
            Message::Invite {
                set_id,
                descriptor,
                records,
                endorsements,
                stats_entries,
                stats_total_size,
                sync_pub,
            } => wrap(self.on_invite(
                from,
                set_id,
                descriptor,
                records,
                endorsements,
                (stats_entries, stats_total_size),
                sync_pub,
                now,
            )),
            Message::InviteAck { set_id } => {
                if let Some(state) = self.sets.get_mut(&set_id) {
                    state.open_invites.remove(from);
                    let _ = state.persist(&self.store, &set_id);
                }
                Vec::new()
            }
            Message::Head {
                set_id,
                round,
                answers,
                position,
                sync_pub,
            } => wrap(self.on_head(from, &set_id, round, answers, position, &sync_pub, now)),
            Message::Records {
                set_id,
                round,
                page,
                records,
                endorsements,
                conflicts,
            } => {
                self.buffer_page(from, &set_id, round, now, |buffer| {
                    buffer
                        .records
                        .insert(page, (records, endorsements, conflicts));
                });
                wrap(self.try_complete(from, &set_id, round, now))
            }
            Message::Entries {
                set_id,
                round,
                page,
                entries,
            } => {
                self.buffer_page(from, &set_id, round, now, |buffer| {
                    buffer.entries.insert(page, entries);
                });
                wrap(self.try_complete(from, &set_id, round, now))
            }
            Message::Need {
                set_id,
                need_id,
                paths,
            } => self.on_need(from, &set_id, need_id, paths),
            Message::Offer {
                set_id,
                need_id,
                tx_id,
                files,
            } => self.on_offer(from, &set_id, need_id, tx_id, files),
            Message::Done {
                set_id,
                need_id,
                tx_id,
            } => self.on_done(from, &set_id, need_id, &tx_id),
        }
    }

    /// Source side of a need: serve ONLY paths that match live, non-deleted
    /// FILE entries of our own index (watcher-derived, inside the root by
    /// construction), for a verified-active member, within the per-peer
    /// slots.
    fn on_need(
        &mut self,
        from: &str,
        set_id: &str,
        need_id: u64,
        paths: Vec<String>,
    ) -> Vec<Effect> {
        let Some(state) = self.sets.get_mut(set_id) else {
            return Vec::new();
        };
        if state.membership.effective(from) != Effective::Active {
            return Vec::new();
        }
        let Some(root) = state.root.clone() else {
            return Vec::new();
        };
        if state.published.contains_key(&(from.to_string(), need_id))
            || state.publishing.contains_key(&(from.to_string(), need_id))
        {
            // A retransmitted need: the offer either flew or will.
            return Vec::new();
        }
        let mut fs_paths = Vec::new();
        let mut map = BTreeMap::new();
        for path in &paths {
            let live_file = state
                .index
                .get(path)
                .is_some_and(|e| !e.deleted && e.kind == crate::index::EntryKind::File);
            if !live_file {
                // One path we cannot vouch for drops the whole need,
                // loudly: a hostile needer learns nothing piecemeal.
                eprintln!("[1device-sync] need for a path outside the live index, set {set_id}");
                return Vec::new();
            }
            let mut fs_path = root.clone();
            for component in path.split('/') {
                fs_path.push(component);
            }
            fs_paths.push(fs_path);
            map.insert(crate::protocol::basename(path).to_string(), path.clone());
        }
        if map.is_empty() {
            return Vec::new();
        }
        // The per-peer slots: a peer beyond its budget supersedes its own
        // oldest published transaction, never another member's.
        let mut out = Vec::new();
        let mine: Vec<(String, u64)> = state
            .published
            .keys()
            .filter(|(peer, _)| peer == from)
            .cloned()
            .collect();
        if mine.len() >= PUBLISHED_PER_PEER {
            let oldest = mine
                .into_iter()
                .min_by_key(|(_, id)| *id)
                .expect("non-empty");
            if let Some(tx_id) = state.published.remove(&oldest) {
                out.push(Effect::Revoke { tx_id });
            }
        }
        state.publishing.insert((from.to_string(), need_id), map);
        out.push(Effect::Publish {
            to: from.to_string(),
            set_id: set_id.to_string(),
            need_id,
            paths: fs_paths,
        });
        out
    }

    /// The publish came back: register the slot and send the offer with the
    /// explicit basename-to-set-path map.
    pub fn on_published(
        &mut self,
        to: &str,
        set_id: &str,
        need_id: u64,
        tx_id: &str,
    ) -> Vec<Effect> {
        let Some(state) = self.sets.get_mut(set_id) else {
            return vec![Effect::Revoke {
                tx_id: tx_id.to_string(),
            }];
        };
        let Some(map) = state.publishing.remove(&(to.to_string(), need_id)) else {
            return vec![Effect::Revoke {
                tx_id: tx_id.to_string(),
            }];
        };
        state
            .published
            .insert((to.to_string(), need_id), tx_id.to_string());
        vec![Effect::send(
            to,
            &Message::Offer {
                set_id: set_id.to_string(),
                need_id,
                tx_id: tx_id.to_string(),
                files: map.into_iter().collect(),
            },
        )]
    }

    pub fn on_publish_failed(&mut self, to: &str, set_id: &str, need_id: u64) {
        if let Some(state) = self.sets.get_mut(set_id) {
            state.publishing.remove(&(to.to_string(), need_id));
        }
    }

    /// Needer side of an offer: only for our own outstanding need, from the
    /// device we sent it to, with a map that stays inside what we asked.
    fn on_offer(
        &mut self,
        from: &str,
        set_id: &str,
        need_id: u64,
        tx_id: String,
        files: Vec<(String, String)>,
    ) -> Vec<Effect> {
        let Some(state) = self.sets.get_mut(set_id) else {
            return Vec::new();
        };
        let Some(root) = state.root.clone() else {
            return Vec::new();
        };
        let Some(need) = state.needs.get_mut(&need_id) else {
            return Vec::new();
        };
        if need.peer != from || need.pull.is_some() {
            return Vec::new();
        }
        let mut map = BTreeMap::new();
        for (wire, set_path) in files {
            // The published name is the source basename, which the chunking
            // rule made a bijection; anything outside the need is refused
            // whole.
            if crate::protocol::basename(&set_path) != wire
                || !need.paths.contains(&set_path)
                || map.insert(wire, set_path).is_some()
            {
                eprintln!("[1device-sync] offer outside its need, set {set_id}");
                return Vec::new();
            }
        }
        if map.is_empty() {
            return Vec::new();
        }
        let staging = root
            .join(crate::wirepath::STAGING_DIR)
            .join(format!("pull-{need_id}"));
        need.pull = Some(ActivePull {
            tx_id: tx_id.clone(),
            staging: staging.clone(),
            files: map.clone(),
            transfer: None,
        });
        vec![Effect::AdoptFill {
            from: from.to_string(),
            set_id: set_id.to_string(),
            need_id,
            tx_id,
            files: map,
            staging,
        }]
    }

    /// The fill is running: remember the transfer id the outcome will name.
    pub fn on_pull_started(&mut self, set_id: &str, need_id: u64, transfer_id: &str) {
        if let Some(state) = self.sets.get_mut(set_id)
            && let Some(need) = state.needs.get_mut(&need_id)
            && let Some(pull) = &mut need.pull
        {
            pull.transfer = Some(transfer_id.to_string());
        }
    }

    /// Adopt or fill refused outright (TX_STALE, offline): drop the pull;
    /// the pending set re-needs on the next pump.
    pub fn on_pull_failed(&mut self, set_id: &str, need_id: u64) {
        if let Some(state) = self.sets.get_mut(set_id) {
            state.needs.remove(&need_id);
        }
    }

    /// A `transfer.finished` / `transfer.failed` for one of our fills. On
    /// either outcome every staged file is verified by hash and the
    /// complete ones are applied (the salvage rule): only the remainder is
    /// re-needed, by the next pump.
    pub fn on_transfer_outcome(&mut self, transfer_id: &str, _ok: bool) -> Vec<Effect> {
        let self_node = self.self_node.clone();
        let mut out = Vec::new();
        let set_ids = self.set_ids();
        for set_id in set_ids {
            let state = self.sets.get_mut(&set_id).expect("listed");
            let Some((&need_id, _)) = state.needs.iter().find(|(_, n)| {
                n.pull
                    .as_ref()
                    .is_some_and(|p| p.transfer.as_deref() == Some(transfer_id))
            }) else {
                continue;
            };
            let need = state.needs.remove(&need_id).expect("found");
            let pull = need.pull.expect("matched");
            let key = state.clock.self_component(&self_node);
            let mut landed = 0usize;
            for (wire, set_path) in &pull.files {
                let staged = pull.staging.join(wire);
                match state.apply_staged(&staged, set_path, &key) {
                    Apply::Landed => landed += 1,
                    Apply::Failed => {}
                    Apply::Conflict => {
                        // Both contents are local now: keep both,
                        // deterministically, and say so with a signed
                        // record.
                        if materialize_conflict(
                            state,
                            &self.directory.names,
                            self.store.identity(),
                            &self_node,
                            &staged,
                            set_path,
                            &key,
                        ) {
                            landed += 1;
                        }
                    }
                }
            }
            let _ = state.drain_pending(&key);
            if state.pending.is_empty() && state.needs.is_empty() {
                // The initial sync delivered everything: the claim served
                // its purpose.
                state.invite_claim = None;
            }
            // The staging dir has served: verified files were moved out,
            // failures are re-pulled whole.
            let _ = std::fs::remove_dir_all(&pull.staging);
            let _ = state.persist(&self.store, &set_id);
            if landed == pull.files.len() {
                out.push(Effect::send(
                    &need.peer,
                    &Message::Done {
                        set_id: set_id.clone(),
                        need_id,
                        tx_id: pull.tx_id.clone(),
                    },
                ));
            }
            break;
        }
        out
    }

    /// Source side of a done: only the peer the slot was published for may
    /// end it.
    fn on_done(&mut self, from: &str, set_id: &str, need_id: u64, tx_id: &str) -> Vec<Effect> {
        let Some(state) = self.sets.get_mut(set_id) else {
            return Vec::new();
        };
        match state.published.get(&(from.to_string(), need_id)) {
            Some(held) if held == tx_id => {
                state.published.remove(&(from.to_string(), need_id));
                vec![Effect::Revoke {
                    tx_id: tx_id.to_string(),
                }]
            }
            _ => Vec::new(),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn on_invite(
        &mut self,
        from: &str,
        set_id: String,
        descriptor: SetDescriptor,
        records: Vec<Record>,
        endorsements: Vec<Endorsement>,
        stats: (u64, u64),
        sync_pub: String,
        now: u64,
    ) -> Vec<Outgoing> {
        // The authorization row, before any other processing: an invite
        // counts only from a device whose CARRIED records make it active in
        // the set. A never-admitted sibling plants no state and gets no
        // ack.
        if !self.sets.contains_key(&set_id) {
            let mut membership = SetMembership::new(descriptor);
            membership.pin_direct(from, &sync_pub);
            for record in &records {
                let _ = membership.absorb(record);
            }
            for endorsement in &endorsements {
                let _ = membership.absorb_endorsement(endorsement);
            }
            if membership.effective(from) != Effective::Active {
                eprintln!("[1device-sync] invite from a non-admitted device dropped");
                return Vec::new();
            }
            let self_node = self.self_node.clone();
            let Ok(state) =
                SetState::fresh(&self.store, &set_id, membership, None, &self_node, now)
            else {
                return Vec::new();
            };
            self.sets.insert(set_id.clone(), state);
        }
        let state = self.sets.get_mut(&set_id).expect("just ensured");
        state.membership.pin_direct(from, &sync_pub);
        for record in &records {
            let _ = state.membership.absorb(record);
        }
        for endorsement in &endorsements {
            let _ = state.membership.absorb_endorsement(endorsement);
        }
        if state.membership.effective(from) != Effective::Active {
            eprintln!("[1device-sync] invite from a non-admitted device dropped");
            return Vec::new();
        }
        // The claim is the INVITER's word, kept for the consent card, and
        // only while the invitation stands open.
        if state.membership.effective(&self.self_node) == Effective::Invited {
            state.invite_claim = Some(InviteClaim {
                inviter: from.to_string(),
                entries: stats.0,
                total_size: stats.1,
            });
        }
        // The ack means "persisted": a failed write keeps the inviter
        // retrying instead of believing a hold that does not exist.
        if let Err(e) = state.persist(&self.store, &set_id) {
            eprintln!("[1device-sync] cannot persist the invitation: {e}");
            return Vec::new();
        }
        vec![Outgoing {
            to: from.to_string(),
            payload: Message::InviteAck { set_id }.to_value(),
        }]
    }

    #[allow(clippy::too_many_arguments)]
    fn on_head(
        &mut self,
        from: &str,
        set_id: &str,
        round: u64,
        answers: Option<u64>,
        position: Option<HeadPosition>,
        sync_pub: &str,
        now: u64,
    ) -> Vec<Outgoing> {
        let Some(state) = self.sets.get_mut(set_id) else {
            // Our channel-authenticated word that we hold nothing: the
            // sender drops its pending row and stops introducing.
            let marker = Message::Head {
                set_id: set_id.to_string(),
                round: self.next_round(true),
                answers: Some(round),
                position: None,
                sync_pub: self.store.identity().public_hex(),
            };
            return vec![Outgoing {
                to: from.to_string(),
                payload: marker.to_value(),
            }];
        };

        // Receiving any head IS direct contact: the pin.
        state.membership.pin_direct(from, sync_pub);

        let Some(position) = position else {
            // The no-membership marker: drop the stranger's rows, stop
            // introducing. Records outrank the marker (drop_stranger keeps
            // verified state).
            state.membership.drop_stranger(from);
            state.intro_attempts.remove(from);
            let _ = state.persist(&self.store, set_id);
            return Vec::new();
        };

        if position.descriptor_hash != descriptor_hash(&state.membership.descriptor) {
            // Two different descriptors under one set id: nothing here is
            // absorbable, loudly.
            eprintln!("[1device-sync] descriptor mismatch from a peer, set {set_id}");
            return Vec::new();
        }

        let their_vv = position.set_vv.clone();
        let buffer = state
            .inflight
            .entry((from.to_string(), round))
            .or_insert(RoundBuffer {
                head: None,
                records: BTreeMap::new(),
                entries: BTreeMap::new(),
                born_at: now,
            });
        buffer.head = Some((position, answers));

        let mut out = self.try_complete(from, set_id, round, now);

        // An opener, or an answer to OUR opener, gets our answering leg; an
        // answer to our own ANSWER closes the exchange (termination).
        let answer_due = match answers {
            None => true,
            Some(theirs) => matches!(self.sent_rounds.get(&theirs), Some(false)),
        };
        if answer_due {
            out.extend(self.answer_head(from, set_id, round, &their_vv));
        }
        out
    }

    /// Builds the answering leg toward `from`: our declaring head, the
    /// records pages (ALWAYS - they carry what we hold about the sender,
    /// the stub and pause proofs), and the entries delta when both ends
    /// stand active.
    fn answer_head(
        &mut self,
        from: &str,
        set_id: &str,
        answered: u64,
        their_vv: &Vv,
    ) -> Vec<Outgoing> {
        let self_node = self.self_node.clone();
        let sync_pub = self.store.identity().public_hex();
        let round = self.next_round(true);
        let Some(state) = self.sets.get_mut(set_id) else {
            return Vec::new();
        };

        let records: Vec<Value> = state.membership.all_records();
        let endorsements: Vec<Value> = state
            .membership
            .endorsements()
            .into_iter()
            .map(Endorsement::to_value)
            .collect();
        let conflict_values: Vec<Value> = state
            .conflicts
            .values()
            .map(ConflictRecord::to_value)
            .collect();
        let mut record_pages = paginate(&records).unwrap_or_default();
        if record_pages.is_empty() && (!endorsements.is_empty() || !conflict_values.is_empty()) {
            record_pages.push(Vec::new());
        }

        let serving = state.root.is_some()
            && state.membership.effective(&self_node) == Effective::Active
            && state.membership.effective(from) == Effective::Active;
        let (entry_pages, complete) = if serving {
            // The delta rule, whole: every entry not covered by the peer's
            // advertised vv, WHEREVER it lives here - the live index, and
            // the pending set (our advertised vv covers absorbed-but-
            // unpulled entries, so a relay must serve them too or it
            // punches the exact hole the watermark rule exists to prevent).
            let mut delta: Vec<Value> = state
                .index
                .not_covered_by(their_vv)
                .map(Entry::to_value)
                .collect();
            for versions in state.pending.values() {
                for entry in versions {
                    if !their_vv.covers(&entry.vv) {
                        delta.push(entry.to_value());
                    }
                }
            }
            (paginate(&delta).unwrap_or_default(), true)
        } else {
            (Vec::new(), false)
        };

        let head = Message::Head {
            set_id: set_id.to_string(),
            round,
            answers: Some(answered),
            position: Some(HeadPosition {
                descriptor_hash: descriptor_hash(&state.membership.descriptor),
                set_vv: state.advertised(&state.clock.self_component(&self_node)),
                records_pages: record_pages.len() as u64,
                entries_pages: entry_pages.len() as u64,
                entries_complete: complete,
            }),
            sync_pub,
        };
        let mut out = vec![Outgoing {
            to: from.to_string(),
            payload: head.to_value(),
        }];
        for (page, chunk) in record_pages.into_iter().enumerate() {
            out.push(Outgoing {
                to: from.to_string(),
                payload: json!({
                    "dialect": DIALECT,
                    "type": "records",
                    "set_id": set_id,
                    "round": round,
                    "page": page as u64,
                    "records": chunk,
                    "endorsements": if page == 0 { endorsements.clone() } else { Vec::new() },
                    "conflicts": if page == 0 { conflict_values.clone() } else { Vec::new() },
                }),
            });
        }
        for (page, chunk) in entry_pages.into_iter().enumerate() {
            out.push(Outgoing {
                to: from.to_string(),
                payload: json!({
                    "dialect": DIALECT,
                    "type": "entries",
                    "set_id": set_id,
                    "round": round,
                    "page": page as u64,
                    "entries": chunk,
                }),
            });
        }
        out
    }

    fn buffer_page(
        &mut self,
        from: &str,
        set_id: &str,
        round: u64,
        now: u64,
        fill: impl FnOnce(&mut RoundBuffer),
    ) {
        let Some(state) = self.sets.get_mut(set_id) else {
            return;
        };
        let buffer = state
            .inflight
            .entry((from.to_string(), round))
            .or_insert(RoundBuffer {
                head: None,
                records: BTreeMap::new(),
                entries: BTreeMap::new(),
                born_at: now,
            });
        if buffer.records.len() as u64 > crate::protocol::ROUND_PAGES_MAX
            || buffer.entries.len() as u64 > crate::protocol::ROUND_PAGES_MAX
        {
            return;
        }
        fill(buffer);
    }

    /// Absorbs the round once every declared page arrived: records first,
    /// endorsements, then the entries into the pending set; the watermark
    /// advances only on a COMPLETE leg, after the absorbed state persisted.
    fn try_complete(&mut self, from: &str, set_id: &str, round: u64, now: u64) -> Vec<Outgoing> {
        let self_node = self.self_node.clone();
        let Some(state) = self.sets.get_mut(set_id) else {
            return Vec::new();
        };
        let key = (from.to_string(), round);
        let complete = state.inflight.get(&key).is_some_and(|b| {
            b.head.as_ref().is_some_and(|(p, _)| {
                (b.records.len() as u64) >= p.records_pages
                    && (b.entries.len() as u64) >= p.entries_pages
            })
        });
        state.prune_buffers(now);
        if !complete {
            return Vec::new();
        }
        let buffer = state.inflight.remove(&key).expect("checked");
        let (position, _) = buffer.head.expect("checked");

        let mut echoed_terminal_about_self = false;
        for (records, endorsements, conflicts) in buffer.records.values() {
            for record in records {
                if let Record::Member(m) = record
                    && m.node_id == self_node
                    && m.status.is_terminal()
                {
                    echoed_terminal_about_self = true;
                }
                let _ = state.membership.absorb(record);
            }
            for endorsement in endorsements {
                let _ = state.membership.absorb_endorsement(endorsement);
            }
            for conflict in conflicts {
                state.absorb_conflict(conflict);
            }
        }

        // The self-component guard: a head or entry claiming OUR component
        // above our own clock is an impossible claim about our own
        // history. The clock jumps above it, loudly, and life goes on - a
        // hostile member inflating our component delays nothing for long
        // and suppresses nothing silently.
        let self_key = state.clock.self_component(&self_node);
        let mut claim = position.set_vv.get(&self_key);
        for entries in buffer.entries.values() {
            for entry in entries {
                claim = claim.max(entry.vv.get(&self_key));
            }
        }
        if claim > state.clock.current() {
            eprintln!(
                "[1device-sync] impossible claim about our own clock ({claim}): jumping past it"
            );
            let _ = state.clock.jump_above(claim);
        }

        // Entries flow only from a verified-active member (the
        // authorization table); records-only traffic flows from any account
        // device, which is what lets introductions and terminal news
        // travel.
        let sender_active = state.membership.effective(from) == Effective::Active;
        if sender_active {
            for entries in buffer.entries.values() {
                for entry in entries {
                    state.absorb_entry(entry);
                }
            }
        }

        // The stub's proof: the answerer echoed our terminal record.
        if echoed_terminal_about_self && let Some(stub) = &mut state.stub {
            stub.remaining.remove(from);
            if stub.remaining.is_empty() {
                state.stub = None;
            }
        }

        // What can settle without bytes settles now.
        let key = state.clock.self_component(&self_node);
        let _ = state.drain_pending(&key);

        // The watermark and the state it covers ride ONE atomic meta.json
        // write - and the advertisement follows the DISK, not the memory: a
        // failed persist rolls the in-memory watermark back, so nothing is
        // ever advertised that a crash could lose.
        if position.entries_complete && sender_active {
            let previous = state.watermarks.get(from).cloned();
            let watermark = state.watermarks.entry(from.to_string()).or_default();
            watermark.merge_max(&position.set_vv);
            if let Err(e) = state.persist(&self.store, set_id) {
                eprintln!("[1device-sync] cannot persist the round, watermark held back: {e}");
                match previous {
                    Some(vv) => {
                        state.watermarks.insert(from.to_string(), vv);
                    }
                    None => {
                        state.watermarks.remove(from);
                    }
                }
            }
        } else if let Err(e) = state.persist(&self.store, set_id) {
            eprintln!("[1device-sync] cannot persist the round: {e}");
        }
        Vec::new()
    }

    // -----------------------------------------------------------------------
    // The wire, outbound: rounds, invitations, introductions, stubs.
    // -----------------------------------------------------------------------

    /// The periodic and on-change driver: opens rounds toward reachable
    /// active members with news, retries invitations, introduces unpinned
    /// devices, keeps stub proofs going. `force` is the safety tick
    /// (opens regardless of the change heuristic).
    pub fn pump(&mut self, reachable: &[String], now: u64, force: bool) -> Vec<Effect> {
        let mut out = Vec::new();
        let self_node = self.self_node.clone();
        let sync_pub = self.store.identity().public_hex();
        let set_ids = self.set_ids();
        for set_id in set_ids {
            // Invitations first: they are the door for everything else.
            let invites: Vec<String> = {
                let state = self.sets.get(&set_id).expect("listed");
                state
                    .open_invites
                    .keys()
                    .filter(|n| reachable.contains(n))
                    .cloned()
                    .collect()
            };
            for invitee in invites {
                if let Some(message) = self.build_invite(&set_id, &invitee) {
                    out.push(Effect::Send(Outgoing {
                        to: invitee.clone(),
                        payload: message,
                    }));
                    let state = self.sets.get_mut(&set_id).expect("listed");
                    let budget = state.open_invites.get_mut(&invitee).expect("listed");
                    *budget = budget.saturating_sub(1);
                    if *budget == 0 {
                        state.open_invites.remove(&invitee);
                    }
                }
            }

            // Gesture announcements: records-only openers, one shot each;
            // the answers-echo rule and the safety tick cover a loss.
            let announce_targets: Vec<String> = {
                let state = self.sets.get_mut(&set_id).expect("listed");
                let targets: Vec<String> = state
                    .announce
                    .iter()
                    .filter(|n| reachable.contains(*n))
                    .cloned()
                    .collect();
                for t in &targets {
                    state.announce.remove(t);
                }
                targets
            };
            for target in announce_targets {
                let round = self.next_round(false);
                let state = self.sets.get(&set_id).expect("listed");
                let head = Message::Head {
                    set_id: set_id.clone(),
                    round,
                    answers: None,
                    position: Some(HeadPosition {
                        descriptor_hash: descriptor_hash(&state.membership.descriptor),
                        set_vv: state.advertised(&state.clock.self_component(&self_node)),
                        records_pages: 0,
                        entries_pages: 0,
                        entries_complete: false,
                    }),
                    sync_pub: sync_pub.clone(),
                };
                out.push(Effect::Send(Outgoing {
                    to: target,
                    payload: head.to_value(),
                }));
            }

            let state = self.sets.get(&set_id).expect("listed");
            let effective_self = state.membership.effective(&self_node);

            // Full rounds: an active member with a root, toward active
            // members with news (or on the safety tick).
            if effective_self == Effective::Active && state.root.is_some() {
                let advertised = state.advertised(&state.clock.self_component(&self_node));
                let peers: Vec<String> = state
                    .membership
                    .device_ids()
                    .into_iter()
                    .filter(|n| *n != self_node && reachable.contains(n))
                    .filter(|n| state.membership.effective(n) == Effective::Active)
                    .collect();
                for peer in peers {
                    let state = self.sets.get_mut(&set_id).expect("listed");
                    let due = force
                        || match state.last_opened.get(&peer) {
                            None => true,
                            Some((at, vv)) => {
                                *vv != advertised && now.saturating_sub(*at) >= OPEN_INTERVAL_SECS
                            }
                        };
                    if !due {
                        continue;
                    }
                    state
                        .last_opened
                        .insert(peer.clone(), (now, advertised.clone()));
                    let round = self.next_round(false);
                    let state = self.sets.get(&set_id).expect("listed");
                    let head = Message::Head {
                        set_id: set_id.clone(),
                        round,
                        answers: None,
                        position: Some(HeadPosition {
                            descriptor_hash: descriptor_hash(&state.membership.descriptor),
                            set_vv: advertised.clone(),
                            records_pages: 0,
                            entries_pages: 0,
                            entries_complete: false,
                        }),
                        sync_pub: sync_pub.clone(),
                    };
                    out.push(Effect::Send(Outgoing {
                        to: peer,
                        payload: head.to_value(),
                    }));
                }
            }

            // Needs: pending FILE bytes pulled from peers whose absorbed
            // watermark says they hold them.
            let need_effects = self.pump_needs(&set_id, reachable);
            out.extend(need_effects);

            // Introductions: records-only openers toward unpinned devices
            // the gossip placed in the set.
            let intro_targets: Vec<String> = {
                let state = self.sets.get(&set_id).expect("listed");
                state
                    .membership
                    .unpinned_devices()
                    .into_iter()
                    .filter(|n| reachable.contains(n))
                    .collect()
            };
            for target in intro_targets {
                let state = self.sets.get_mut(&set_id).expect("listed");
                let budget = state
                    .intro_attempts
                    .entry(target.clone())
                    .or_insert(RETRY_BUDGET);
                if *budget == 0 {
                    continue;
                }
                *budget -= 1;
                let round = self.next_round(false);
                let state = self.sets.get(&set_id).expect("listed");
                let head = Message::Head {
                    set_id: set_id.clone(),
                    round,
                    answers: None,
                    position: Some(HeadPosition {
                        descriptor_hash: descriptor_hash(&state.membership.descriptor),
                        set_vv: state.advertised(&state.clock.self_component(&self_node)),
                        records_pages: 0,
                        entries_pages: 0,
                        entries_complete: false,
                    }),
                    sync_pub: sync_pub.clone(),
                };
                out.push(Effect::Send(Outgoing {
                    to: target,
                    payload: head.to_value(),
                }));
            }

            // Stub proofs: records-only openers until each member echoed our
            // terminal record.
            let stub_targets: Vec<String> = {
                let state = self.sets.get_mut(&set_id).expect("listed");
                match &mut state.stub {
                    Some(stub) if stub.attempts > 0 => {
                        stub.attempts -= 1;
                        stub.remaining
                            .iter()
                            .filter(|n| reachable.contains(*n))
                            .cloned()
                            .collect()
                    }
                    Some(_) => {
                        state.stub = None;
                        Vec::new()
                    }
                    None => Vec::new(),
                }
            };
            for target in stub_targets {
                let round = self.next_round(false);
                let state = self.sets.get(&set_id).expect("listed");
                let head = Message::Head {
                    set_id: set_id.clone(),
                    round,
                    answers: None,
                    position: Some(HeadPosition {
                        descriptor_hash: descriptor_hash(&state.membership.descriptor),
                        set_vv: state.advertised(&state.clock.self_component(&self_node)),
                        records_pages: 0,
                        entries_pages: 0,
                        entries_complete: false,
                    }),
                    sync_pub: sync_pub.clone(),
                };
                out.push(Effect::Send(Outgoing {
                    to: target,
                    payload: head.to_value(),
                }));
            }
        }
        out
    }

    /// Builds the needs one pump owes: pending FILE versions grouped per
    /// serving peer, chunked so no two paths share a basename (the
    /// published record names files by basename: the bijection), within
    /// the per-peer slots and the chunk bounds.
    fn pump_needs(&mut self, set_id: &str, reachable: &[String]) -> Vec<Effect> {
        let self_node = self.self_node.clone();
        let mut out = Vec::new();
        let Some(state) = self.sets.get_mut(set_id) else {
            return out;
        };
        if state.root.is_none()
            || state.membership.effective(&self_node) != Effective::Active
            || state.size_guard_tripped()
        {
            return out;
        }
        let mut claimed: BTreeSet<String> = state
            .needs
            .values()
            .flat_map(|n| n.paths.iter().cloned())
            .collect();
        let peers: Vec<String> = reachable
            .iter()
            .filter(|n| **n != self_node)
            .filter(|n| state.membership.effective(n) == Effective::Active)
            .cloned()
            .collect();
        let mut minted = false;
        for peer in peers {
            if state.needs.values().filter(|n| n.peer == peer).count() >= NEEDS_PER_PEER {
                continue;
            }
            let Some(watermark) = state.watermarks.get(&peer).cloned() else {
                continue;
            };
            let mut chunk: Vec<String> = Vec::new();
            let mut basenames: BTreeSet<String> = BTreeSet::new();
            let mut bytes = 0u64;
            for (path, versions) in &state.pending {
                if claimed.contains(path) {
                    continue;
                }
                let Some(entry) = versions
                    .iter()
                    .rfind(|e| !e.deleted && e.kind == crate::index::EntryKind::File)
                else {
                    continue;
                };
                // The peer can only serve what its advertised position
                // covers.
                if !watermark.covers(&entry.vv) {
                    continue;
                }
                let base = crate::protocol::basename(path).to_string();
                if !basenames.insert(base) {
                    continue;
                }
                chunk.push(path.clone());
                bytes = bytes.saturating_add(entry.size);
                if chunk.len() >= NEED_CHUNK_FILES || bytes >= NEED_CHUNK_BYTES {
                    break;
                }
            }
            if chunk.is_empty() {
                continue;
            }
            state.need_counter += 1;
            let need_id = state.need_counter;
            claimed.extend(chunk.iter().cloned());
            state.needs.insert(
                need_id,
                NeedState {
                    peer: peer.clone(),
                    paths: chunk.iter().cloned().collect(),
                    pull: None,
                },
            );
            minted = true;
            out.push(Effect::send(
                &peer,
                &Message::Need {
                    set_id: set_id.to_string(),
                    need_id,
                    paths: chunk,
                },
            ));
        }
        if minted {
            // The counter must never repeat across restarts: it rides the
            // meta write.
            let _ = state.persist(&self.store, set_id);
        }
        out
    }

    fn build_invite(&self, set_id: &str, _invitee: &str) -> Option<Value> {
        let state = self.sets.get(set_id)?;
        let entries = state.index.live().count() as u64;
        let total_size: u64 = state.index.live().map(|e| e.size).sum();
        Some(
            Message::Invite {
                set_id: set_id.to_string(),
                descriptor: state.membership.descriptor.clone(),
                records: state
                    .membership
                    .all_records()
                    .iter()
                    .filter_map(Record::from_value)
                    .collect(),
                endorsements: state
                    .membership
                    .endorsements()
                    .into_iter()
                    .cloned()
                    .collect(),
                stats_entries: entries,
                stats_total_size: total_size,
                sync_pub: self.store.identity().public_hex(),
            }
            .to_value(),
        )
    }
}

impl SetState {
    fn fresh(
        store: &Store,
        set_id: &str,
        membership: SetMembership,
        root: Option<PathBuf>,
        self_node: &str,
        now: u64,
    ) -> io::Result<SetState> {
        let dir = store.set_dir(set_id)?;
        let index = SetIndex::new();
        let clock = SetClock::open(&dir, &index, self_node, now)?;
        Ok(SetState {
            membership,
            index,
            clock,
            root,
            pending: BTreeMap::new(),
            watermarks: BTreeMap::new(),
            invite_claim: None,
            inflight: BTreeMap::new(),
            open_invites: BTreeMap::new(),
            intro_attempts: BTreeMap::new(),
            stub: None,
            announce: BTreeSet::new(),
            last_opened: BTreeMap::new(),
            needs: BTreeMap::new(),
            need_counter: 0,
            published: BTreeMap::new(),
            publishing: BTreeMap::new(),
            parked: BTreeMap::new(),
            conflicts: BTreeMap::new(),
            landed_bytes: 0,
            ignored: Vec::new(),
            watch_degraded: false,
        })
    }

    /// Our advertised set_vv: own clock joined with the componentwise max
    /// of the fully-absorbed watermarks - never the index's max components
    /// (an entry learned via a relay must not advance a watermark for
    /// entries never received).
    pub fn advertised(&self, self_component: &str) -> Vv {
        let mut vv = Vv::new();
        if self.clock.current() > 0 {
            vv.set(self_component, self.clock.current());
        }
        for watermark in self.watermarks.values() {
            vv.merge_max(watermark);
        }
        vv
    }

    /// Parks one absorbed entry: dominance-pruned against the index and the
    /// versions already parked for its path.
    fn absorb_entry(&mut self, entry: &Entry) {
        if let Some(ours) = self.index.get(&entry.path)
            && ours.vv.covers(&entry.vv)
        {
            return;
        }
        let parked = self.pending.entry(entry.path.clone()).or_default();
        parked.retain(|held| !entry.vv.covers(&held.vv));
        if parked.iter().any(|held| held.vv.covers(&entry.vv)) {
            return;
        }
        if parked.len() >= PENDING_PER_PATH {
            parked.remove(0);
        }
        parked.push(entry.clone());
    }

    /// The consent guard (the letter, section 2): the inviter's size claim,
    /// honestly enforced with a stated margin. Tripped = the set stops
    /// pulling and the card says why; resuming is a user gesture.
    fn size_guard_tripped(&self) -> bool {
        self.invite_claim.as_ref().is_some_and(|claim| {
            self.landed_bytes
                > claim
                    .total_size
                    .saturating_mul(2)
                    .saturating_add(100 * 1024 * 1024)
        })
    }

    fn set_watch_degraded(&mut self, degraded: bool) {
        self.watch_degraded = degraded;
    }

    #[cfg(test)]
    pub(crate) fn stub_active(&self) -> bool {
        self.stub.is_some()
    }

    /// The fast apply path (the letter, section 6): a staged file whose
    /// pending entry cleanly DOMINATES the local one (or has none, or
    /// beats a tombstone) is verified by hash, moved into place
    /// filesystem-first, and only then indexed. A concurrent LIVE pair
    /// reports [`Apply::Conflict`] for the keep-both machinery.
    fn apply_staged_file(&mut self, staged: &Path, set_path: &str, self_component: &str) -> bool {
        match self.apply_staged(staged, set_path, self_component) {
            Apply::Landed => true,
            Apply::Conflict | Apply::Failed => false,
        }
    }

    fn apply_staged(&mut self, staged: &Path, set_path: &str, self_component: &str) -> Apply {
        use crate::vv::Relation;

        let Some(root) = self.root.clone() else {
            return Apply::Failed;
        };
        let Some(versions) = self.pending.get(set_path) else {
            // Nothing pending anymore (superseded mid-pull): nothing owed.
            return Apply::Landed;
        };
        // The version this pull was FOR: the newest pending file version.
        let Some(entry) = versions
            .iter()
            .rfind(|e| !e.deleted && e.kind == crate::index::EntryKind::File)
            .cloned()
        else {
            return Apply::Landed;
        };
        let (dominates, resurrects) = match self.index.get(set_path) {
            None => (true, false),
            Some(ours) => match entry.vv.relation(&ours.vv) {
                Relation::Descends | Relation::Equal => (true, false),
                // A live version concurrent with our TOMBSTONE: the edit
                // wins (the lossless doctrine), and the survivor will
                // dominate the tombstone.
                Relation::Concurrent if ours.deleted => (true, true),
                // Concurrent with a local LIVE version: keep both.
                Relation::Concurrent => return Apply::Conflict,
                Relation::Precedes => (false, false),
            },
        };
        if !dominates {
            return Apply::Failed;
        }
        let Ok(hash) = crate::scan::hash_file(staged) else {
            return Apply::Failed;
        };
        if hash != entry.hash {
            eprintln!("[1device-sync] staged bytes do not match their entry: re-needed");
            return Apply::Failed;
        }
        // Filesystem first, index after (load-bearing, the letter).
        let mut dest = root.clone();
        for component in set_path.split('/') {
            dest.push(component);
        }
        if let Some(parent) = dest.parent()
            && std::fs::create_dir_all(parent).is_err()
        {
            return Apply::Failed;
        }
        // A kind change applies as remove-old-kind-then-create: a directory
        // in the way goes only if empty (its children's tombstones come in
        // the same batch and land first).
        if dest.is_dir() && std::fs::remove_dir(&dest).is_err() {
            return Apply::Failed;
        }
        // Windows: a read-only destination refuses the replace; clearing
        // the attribute is deliberate (the incoming version supersedes it).
        #[cfg(windows)]
        if let Ok(meta) = std::fs::metadata(&dest) {
            let mut perms = meta.permissions();
            if perms.readonly() {
                perms.set_readonly(false);
                let _ = std::fs::set_permissions(&dest, perms);
            }
        }
        if let Err(e) = std::fs::rename(staged, &dest) {
            // A destination held open by an application (the Office case):
            // keep the VERIFIED staged copy and retry the rename alone,
            // never the transfer. The staged file moves to a parked home
            // that survives the pull's staging-dir cleanup.
            let parked_dir = root.join(crate::wirepath::STAGING_DIR).join("parked");
            if std::fs::create_dir_all(&parked_dir).is_ok() {
                let home = parked_dir.join(format!(
                    "{}-{}",
                    &entry.hash[..16.min(entry.hash.len())],
                    crate::protocol::basename(set_path)
                ));
                if std::fs::rename(staged, &home).is_ok() {
                    self.parked.insert(
                        set_path.to_string(),
                        ParkedEntry {
                            staged: home,
                            entry: entry.clone(),
                            reason: format!("blocked: {e}"),
                        },
                    );
                }
            }
            return Apply::Failed;
        }
        #[cfg(unix)]
        if entry.exec {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&dest, std::fs::Permissions::from_mode(0o755));
        }
        // The source's mtime, then the filesystem's own rounding read BACK:
        // the index records what the disk will report, not our wish.
        let mtime = (|| {
            let file = std::fs::File::options().write(true).open(&dest).ok()?;
            let secs = u64::try_from(entry.mtime.secs).ok()?;
            let stamp = std::time::SystemTime::UNIX_EPOCH
                + std::time::Duration::new(secs, entry.mtime.nanos);
            file.set_times(std::fs::FileTimes::new().set_modified(stamp))
                .ok()?;
            Some(crate::index::Mtime::of(
                std::fs::metadata(&dest).ok()?.modified().ok()?,
            ))
        })()
        .unwrap_or(entry.mtime);
        let size = std::fs::metadata(&dest)
            .map(|m| m.len())
            .unwrap_or(entry.size);
        if self.invite_claim.is_some() {
            // The consent guard measures what the initial sync lands.
            self.landed_bytes = self.landed_bytes.saturating_add(size);
        }
        let mut vv = entry.vv.clone();
        if resurrects {
            // The survivor dominates the tombstone it beat: merged, then
            // bumped past it.
            if let Some(ours) = self.index.get(set_path) {
                vv.merge_max(&ours.vv);
            }
            if let Ok(tick) = self.clock.tick() {
                vv.set(self_component, tick);
            }
        }
        self.index.upsert(Entry {
            path: set_path.to_string(),
            kind: crate::index::EntryKind::File,
            size,
            mtime,
            exec: entry.exec,
            hash,
            vv,
            deleted: false,
        });
        // Everything this landed version covers leaves the pending set.
        if let Some(versions) = self.pending.get_mut(set_path) {
            versions.retain(|held| !entry.vv.covers(&held.vv));
            if versions.is_empty() {
                self.pending.remove(set_path);
            }
        }
        Apply::Landed
    }

    /// Drains everything the pending set can settle WITHOUT pulling bytes
    /// (the letter, sections 6 and 7): covered versions drop; same-hash
    /// concurrents merge silently (vv = max); a tombstone concurrent with
    /// a local live edit loses (the edit wins, vv = max + bump); FILE
    /// tombstones that dominate apply under the case-fold guard, deletes
    /// before creates; directories materialize; DIRECTORY tombstones are
    /// evaluated at the END, remove only a then-empty directory, and lose
    /// to any concurrent live descendant - which, with its ancestors,
    /// takes vv = max + bump so the tombstone retires everywhere instead
    /// of oscillating back. Parked renames retry here too.
    fn drain_pending(&mut self, self_component: &str) -> io::Result<()> {
        use crate::vv::Relation;

        self.retry_parked(self_component);

        // Quiet passes first: drops and merges that touch no filesystem.
        let paths: Vec<String> = self.pending.keys().cloned().collect();
        for path in &paths {
            let Some(versions) = self.pending.get_mut(path) else {
                continue;
            };
            let local = self.index.get(path).cloned();
            versions.retain(|incoming| {
                let Some(ours) = &local else { return true };
                match incoming.vv.relation(&ours.vv) {
                    // Old news.
                    Relation::Precedes | Relation::Equal => false,
                    Relation::Descends | Relation::Concurrent => true,
                }
            });
            if let Some(ours) = &local {
                let mut merged = ours.clone();
                let mut changed = false;
                let mut beat_a_tombstone = false;
                versions.retain(|incoming| {
                    if incoming.vv.relation(&merged.vv) != Relation::Concurrent {
                        return true;
                    }
                    let same_content = incoming.deleted == merged.deleted
                        && incoming.kind == merged.kind
                        && incoming.hash == merged.hash;
                    if same_content {
                        // Both sides did the identical thing (tombstones
                        // included): merge silently, NO bump.
                        merged.vv.merge_max(&incoming.vv);
                        changed = true;
                        return false;
                    }
                    if incoming.deleted && !merged.deleted {
                        // Delete vs edit: the edit wins, and the survivor
                        // must DOMINATE the tombstone so it retires
                        // everywhere instead of oscillating back.
                        merged.vv.merge_max(&incoming.vv);
                        changed = true;
                        beat_a_tombstone = true;
                        return false;
                    }
                    true
                });
                if changed {
                    if beat_a_tombstone && let Ok(tick) = self.clock.tick() {
                        merged.vv.set(self_component, tick);
                    }
                    self.index.upsert(merged);
                }
            }
            if self
                .pending
                .get(path)
                .is_some_and(|versions| versions.is_empty())
            {
                self.pending.remove(path);
            }
        }

        // File tombstones that dominate: deletes before creates.
        let paths: Vec<String> = self.pending.keys().cloned().collect();
        for path in &paths {
            let Some(tomb) = self.pending.get(path).and_then(|versions| {
                versions
                    .iter()
                    .rfind(|e| e.deleted && e.kind == crate::index::EntryKind::File)
                    .cloned()
            }) else {
                continue;
            };
            let dominates = match self.index.get(path) {
                None => true,
                Some(ours) => matches!(
                    tomb.vv.relation(&ours.vv),
                    Relation::Descends | Relation::Equal
                ),
            };
            if !dominates {
                continue;
            }
            self.apply_file_tombstone(&tomb);
        }

        // Directories materialize (creates after deletes).
        self.apply_pending_dirs();

        // Directory tombstones, at the end, only-if-empty.
        let paths: Vec<String> = self.pending.keys().cloned().collect();
        for path in &paths {
            let Some(tomb) = self.pending.get(path).and_then(|versions| {
                versions
                    .iter()
                    .rfind(|e| e.deleted && e.kind == crate::index::EntryKind::Dir)
                    .cloned()
            }) else {
                continue;
            };
            self.apply_dir_tombstone(&tomb, self_component);
        }
        Ok(())
    }

    /// One dominating FILE tombstone. The guard: no LIVE index entry may
    /// case- or normalization-fold to the same path (on a case-insensitive
    /// filesystem the on-disk file belongs to the live entry: a case-only
    /// rename arriving as create-then-tombstone across two rounds must not
    /// delete the renamed file).
    fn apply_file_tombstone(&mut self, tomb: &Entry) {
        let Some(root) = self.root.clone() else {
            return;
        };
        let fold = tomb.path.to_lowercase();
        let folded_live = self
            .index
            .live()
            .any(|e| e.path != tomb.path && e.path.to_lowercase() == fold);
        let mut dest = root;
        for component in tomb.path.split('/') {
            dest.push(component);
        }
        if !folded_live {
            match std::fs::remove_file(&dest) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => {
                    eprintln!("[1device-sync] tombstone blocked on {}: {e}", tomb.path);
                    return;
                }
            }
        }
        // Folded twin on disk: the file belongs to the live entry; the
        // tombstone still lands in the index (the version is gone).
        self.index.upsert(tomb.clone());
        self.settle_pending(&tomb.path, &tomb.vv);
    }

    /// One DIRECTORY tombstone, end-of-batch: remove only a directory that
    /// is then empty; a concurrent live descendant resurrects the chain
    /// (every survivor takes vv = max + bump).
    fn apply_dir_tombstone(&mut self, tomb: &Entry, self_component: &str) {
        use crate::vv::Relation;

        let Some(root) = self.root.clone() else {
            return;
        };
        let prefix = format!("{}/", tomb.path);
        let live_descendants: Vec<String> = self
            .index
            .live()
            .filter(|e| e.path.starts_with(&prefix))
            .map(|e| e.path.clone())
            .collect();
        let dominates_dir = match self.index.get(&tomb.path) {
            None => true,
            Some(ours) => matches!(
                tomb.vv.relation(&ours.vv),
                Relation::Descends | Relation::Equal
            ),
        };
        if !live_descendants.is_empty() || !dominates_dir {
            // The edit wins, up the tree: the directory and every live
            // descendant dominate the tombstone from now on.
            let mut survivors = live_descendants;
            survivors.push(tomb.path.clone());
            for path in survivors {
                let Some(mut entry) = self.index.get(&path).cloned() else {
                    continue;
                };
                if entry.deleted {
                    continue;
                }
                entry.vv.merge_max(&tomb.vv);
                if let Ok(tick) = self.clock.tick() {
                    entry.vv.set(self_component, tick);
                }
                self.index.upsert(entry);
            }
            self.settle_pending(&tomb.path, &tomb.vv);
            return;
        }
        let mut dest = root;
        for component in tomb.path.split('/') {
            dest.push(component);
        }
        match std::fs::remove_dir(&dest) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(_) => {
                // Not empty on disk (unindexed junk, an ignored name): the
                // only-if-empty rule leaves it pending for a later drain.
                return;
            }
        }
        self.index.upsert(tomb.clone());
        self.settle_pending(&tomb.path, &tomb.vv);
    }

    /// Absorbs one gossiped conflict record: verified under its signer's
    /// binding, keep-best per key (`resolved` beats `open`, then the
    /// deterministic signer/sig order), and the winning record's
    /// path_on_disk is authoritative - a live same-content copy of ours
    /// under another name FOLDS onto it, never overwriting.
    fn absorb_conflict(&mut self, record: &ConflictRecord) {
        if record.set_id != self.membership.descriptor.set_id {
            return;
        }
        let Some(signer_key) = self.membership.key_for(&record.signed_by) else {
            return;
        };
        if !record.verify(signer_key) {
            return;
        }
        let key = (record.path.clone(), record.conflict_id.clone());
        let superseded = match self.conflicts.get(&key) {
            Some(held) => record.beats(held),
            None => true,
        };
        if !superseded {
            return;
        }
        self.conflicts.insert(key, record.clone());
        // The physical FOLD of a transiently divergent copy name onto the
        // winning record's path is deferred (a v1 note in the letter): the
        // divergence needs two detectors holding different directory names
        // at the same instant, and the duplicate it leaves is same-content,
        // synced, and harmless.
    }

    /// Removes the pending versions `vv` covers, and the path when none
    /// remain.
    fn settle_pending(&mut self, path: &str, vv: &Vv) {
        if let Some(versions) = self.pending.get_mut(path) {
            versions.retain(|held| !vv.covers(&held.vv));
            if versions.is_empty() {
                self.pending.remove(path);
            }
        }
    }

    /// Retries the parked renames (a destination unlocked since).
    fn retry_parked(&mut self, self_component: &str) {
        let paths: Vec<String> = self.parked.keys().cloned().collect();
        for path in paths {
            let Some(parked) = self.parked.get(&path).cloned() else {
                continue;
            };
            if !parked.staged.exists() {
                self.parked.remove(&path);
                continue;
            }
            // Back through the ordinary gate: pending must still name it.
            self.pending
                .entry(path.clone())
                .or_default()
                .push(parked.entry.clone());
            if self.apply_staged_file(&parked.staged, &path, self_component) {
                self.parked.remove(&path);
            } else {
                self.settle_pending(&path, &parked.entry.vv);
            }
        }
    }

    /// Directories carry no bytes: a pending DIR entry that dominates
    /// applies immediately (mkdir, index, done). Runs after every absorb
    /// and rescan.
    fn apply_pending_dirs(&mut self) {
        use crate::vv::Relation;

        let Some(root) = self.root.clone() else {
            return;
        };
        let paths: Vec<String> = self.pending.keys().cloned().collect();
        for path in paths {
            let Some(entry) = self.pending.get(&path).and_then(|versions| {
                versions
                    .iter()
                    .rfind(|e| !e.deleted && e.kind == crate::index::EntryKind::Dir)
                    .cloned()
            }) else {
                continue;
            };
            let dominates = match self.index.get(&path) {
                None => true,
                Some(ours) => matches!(
                    entry.vv.relation(&ours.vv),
                    Relation::Descends | Relation::Equal
                ),
            };
            if !dominates {
                continue;
            }
            let mut dest = root.clone();
            for component in path.split('/') {
                dest.push(component);
            }
            // Remove-old-kind-then-create: a FILE standing where the
            // directory goes is the superseded kind.
            if dest.is_file() && std::fs::remove_file(&dest).is_err() {
                continue;
            }
            if std::fs::create_dir_all(&dest).is_err() {
                continue;
            }
            self.index.upsert(entry.clone());
            if let Some(versions) = self.pending.get_mut(&path) {
                versions.retain(|held| !entry.vv.covers(&held.vv));
                if versions.is_empty() {
                    self.pending.remove(&path);
                }
            }
        }
    }

    fn prune_buffers(&mut self, now: u64) {
        self.inflight
            .retain(|_, b| now.saturating_sub(b.born_at) <= ROUND_TTL_SECS);
    }

    fn persist(&self, store: &Store, set_id: &str) -> io::Result<()> {
        let pending: Value = Value::Object(
            self.pending
                .iter()
                .map(|(path, entries)| {
                    (
                        path.clone(),
                        Value::Array(entries.iter().map(Entry::to_value).collect()),
                    )
                })
                .collect(),
        );
        let watermarks: Value = Value::Object(
            self.watermarks
                .iter()
                .map(|(node, vv)| (node.clone(), vv.to_value()))
                .collect(),
        );
        let meta = json!({
            "membership": self.membership.to_value(),
            "root": self.root.as_ref().map(|p| p.to_string_lossy().into_owned()),
            "pending": pending,
            "watermarks": watermarks,
            "invite_claim": self.invite_claim.as_ref().map(|c| json!({
                "inviter": c.inviter,
                "entries": c.entries,
                "total_size": c.total_size,
            })),
            "stub": self.stub.as_ref().map(|s| json!({
                "remaining": s.remaining.iter().collect::<Vec<_>>(),
                "attempts": s.attempts,
            })),
            "open_invites": Value::Object(
                self.open_invites
                    .iter()
                    .map(|(node, budget)| (node.clone(), Value::from(*budget)))
                    .collect(),
            ),
            "need_counter": self.need_counter,
            "landed_bytes": self.landed_bytes,
            "conflicts": Value::Array(
                self.conflicts.values().map(ConflictRecord::to_value).collect(),
            ),
            "parked": Value::Object(
                self.parked
                    .iter()
                    .map(|(path, p)| {
                        (
                            path.clone(),
                            json!({
                                "staged": p.staged.to_string_lossy(),
                                "entry": p.entry.to_value(),
                                "reason": p.reason,
                            }),
                        )
                    })
                    .collect(),
            ),
        });
        store.save_meta(set_id, &meta)?;
        store.save_index(set_id, &self.index)
    }

    fn from_meta(
        store: &Store,
        set_id: &str,
        meta: &Value,
        self_node: &str,
        now: u64,
    ) -> io::Result<SetState> {
        let corrupt = || io::Error::other(format!("corrupt meta for set {set_id}"));
        let membership = meta
            .get("membership")
            .and_then(SetMembership::from_value)
            .ok_or_else(corrupt)?;
        if membership.descriptor.set_id != set_id {
            return Err(corrupt());
        }
        let index = store.load_index(set_id)?.unwrap_or_default();
        let dir = store.set_dir(set_id)?;
        let clock = SetClock::open(&dir, &index, self_node, now)?;
        let root = match meta.get("root").ok_or_else(corrupt)? {
            Value::Null => None,
            v => Some(PathBuf::from(v.as_str().ok_or_else(corrupt)?)),
        };
        let mut pending = BTreeMap::new();
        for (path, entries) in meta
            .get("pending")
            .and_then(Value::as_object)
            .ok_or_else(corrupt)?
        {
            let entries: Option<Vec<Entry>> = entries
                .as_array()
                .ok_or_else(corrupt)?
                .iter()
                .map(Entry::from_value)
                .collect();
            pending.insert(path.clone(), entries.ok_or_else(corrupt)?);
        }
        let mut watermarks = BTreeMap::new();
        for (node, vv) in meta
            .get("watermarks")
            .and_then(Value::as_object)
            .ok_or_else(corrupt)?
        {
            watermarks.insert(node.clone(), Vv::from_value(vv).ok_or_else(corrupt)?);
        }
        let invite_claim = match meta.get("invite_claim").ok_or_else(corrupt)? {
            Value::Null => None,
            v => Some(InviteClaim {
                inviter: v
                    .get("inviter")
                    .and_then(Value::as_str)
                    .ok_or_else(corrupt)?
                    .to_string(),
                entries: v
                    .get("entries")
                    .and_then(Value::as_u64)
                    .ok_or_else(corrupt)?,
                total_size: v
                    .get("total_size")
                    .and_then(Value::as_u64)
                    .ok_or_else(corrupt)?,
            }),
        };
        let stub = match meta.get("stub").ok_or_else(corrupt)? {
            Value::Null => None,
            v => Some(Stub {
                remaining: v
                    .get("remaining")
                    .and_then(Value::as_array)
                    .ok_or_else(corrupt)?
                    .iter()
                    .map(|n| n.as_str().map(str::to_string))
                    .collect::<Option<BTreeSet<String>>>()
                    .ok_or_else(corrupt)?,
                attempts: v
                    .get("attempts")
                    .and_then(Value::as_u64)
                    .ok_or_else(corrupt)? as u32,
            }),
        };
        let mut open_invites = BTreeMap::new();
        for (node, budget) in meta
            .get("open_invites")
            .and_then(Value::as_object)
            .ok_or_else(corrupt)?
        {
            open_invites.insert(node.clone(), budget.as_u64().ok_or_else(corrupt)? as u32);
        }
        Ok(SetState {
            membership,
            index,
            clock,
            root,
            pending,
            watermarks,
            invite_claim,
            inflight: BTreeMap::new(),
            open_invites,
            intro_attempts: BTreeMap::new(),
            stub,
            announce: BTreeSet::new(),
            last_opened: BTreeMap::new(),
            needs: BTreeMap::new(),
            need_counter: meta
                .get("need_counter")
                .and_then(Value::as_u64)
                .ok_or_else(corrupt)?,
            published: BTreeMap::new(),
            publishing: BTreeMap::new(),
            landed_bytes: meta
                .get("landed_bytes")
                .and_then(Value::as_u64)
                .ok_or_else(corrupt)?,
            ignored: Vec::new(),
            watch_degraded: false,
            conflicts: {
                let mut conflicts = BTreeMap::new();
                for record in meta
                    .get("conflicts")
                    .and_then(Value::as_array)
                    .ok_or_else(corrupt)?
                {
                    let record = ConflictRecord::from_value(record).ok_or_else(corrupt)?;
                    conflicts.insert((record.path.clone(), record.conflict_id.clone()), record);
                }
                conflicts
            },
            parked: {
                let mut parked = BTreeMap::new();
                for (path, p) in meta
                    .get("parked")
                    .and_then(Value::as_object)
                    .ok_or_else(corrupt)?
                {
                    parked.insert(
                        path.clone(),
                        ParkedEntry {
                            staged: PathBuf::from(
                                p.get("staged")
                                    .and_then(Value::as_str)
                                    .ok_or_else(corrupt)?,
                            ),
                            entry: p
                                .get("entry")
                                .and_then(Entry::from_value)
                                .ok_or_else(corrupt)?,
                            reason: p
                                .get("reason")
                                .and_then(Value::as_str)
                                .ok_or_else(corrupt)?
                                .to_string(),
                        },
                    );
                }
                parked
            },
        })
    }

    /// Removes the staging leftovers a crash abandoned: only our own
    /// `pull-*` directories, never the parked home (verified bytes a
    /// locked file is waiting on) and never anything we did not name.
    fn sweep_staging(&self) {
        let Some(root) = &self.root else {
            return;
        };
        let staging = root.join(crate::wirepath::STAGING_DIR);
        let Ok(entries) = std::fs::read_dir(&staging) else {
            return;
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            if name.starts_with("pull-") {
                let _ = std::fs::remove_dir_all(entry.path());
            }
        }
    }

    /// Marks every eligible member as owed a records-only opener: the
    /// gesture announcements.
    fn announce_to_members(&mut self, self_node: &str) {
        let members: Vec<String> = self
            .membership
            .device_ids()
            .into_iter()
            .filter(|n| n != self_node)
            .filter(|n| {
                matches!(
                    self.membership.effective(n),
                    Effective::Active | Effective::Paused
                )
            })
            .collect();
        self.announce.extend(members);
    }
}

fn wrap(out: Vec<Outgoing>) -> Vec<Effect> {
    out.into_iter().map(Effect::Send).collect()
}

fn kind_str(kind: SetKind) -> &'static str {
    match kind {
        SetKind::Dir => "dir",
        SetKind::File => "file",
    }
}

/// The version's author for the card: the node holding the greatest
/// (value, key) component - a deterministic attribution, not a judgement.
fn author_of(vv: &Vv) -> Option<String> {
    vv.components()
        .max_by(|a, b| (a.1, a.0).cmp(&(b.1, b.0)))
        .map(|(key, _)| key.split('@').next().unwrap_or(key).to_string())
}

/// Keep both, deterministically (the letter, section 7): the winner of the
/// plain path is the lower version_id - symmetric, arbitrary, identical
/// everywhere - with vv = max of both plus the detector's bump; the loser
/// is materialized beside it under its honest name. Placement never
/// overwrites: an occupied candidate with different content bumps the
/// deterministic counter, a same-content occupant merges silently. Returns
/// whether the staged bytes were consumed.
#[allow(clippy::too_many_arguments)]
fn materialize_conflict(
    state: &mut SetState,
    names: &BTreeMap<String, String>,
    identity: &crate::identity::Identity,
    self_node: &str,
    staged: &Path,
    set_path: &str,
    self_component: &str,
) -> bool {
    let Some(root) = state.root.clone() else {
        return false;
    };
    let Some(local) = state.index.get(set_path).cloned() else {
        return false;
    };
    let Some(incoming) = state.pending.get(set_path).and_then(|v| {
        v.iter()
            .rfind(|e| !e.deleted && e.kind == crate::index::EntryKind::File)
            .cloned()
    }) else {
        return false;
    };
    let Ok(staged_hash) = crate::scan::hash_file(staged) else {
        return false;
    };
    if staged_hash != incoming.hash {
        return false;
    }
    let lvid = version_id(&local);
    let rvid = version_id(&incoming);
    let conflict_id = conflict_id_of(&lvid, &rvid);
    let local_wins = lvid < rvid;
    let (loser, winner) = if local_wins {
        (incoming.clone(), local.clone())
    } else {
        (local.clone(), incoming.clone())
    };

    let author = loser_author(&loser.vv, &winner.vv).unwrap_or_else(|| self_node.to_string());
    let mut device = names
        .get(&author)
        .map(|n| sanitize_device_name(n))
        .unwrap_or_default();
    if device.is_empty() {
        device = author.chars().take(8).collect();
    }
    let base = crate::protocol::basename(set_path);
    let dir_prefix = &set_path[..set_path.len() - base.len()];
    let mut counter = 1u32;
    let loser_path = loop {
        let name = conflict_copy_name(base, &device, loser.mtime, counter);
        let candidate = format!("{dir_prefix}{name}");
        match state.index.get(&candidate) {
            Some(e) if !e.deleted && e.hash != loser.hash => {
                counter += 1;
                if counter > 32 {
                    return false;
                }
            }
            _ => break candidate,
        }
    };
    let occupied_same = state
        .index
        .get(&loser_path)
        .is_some_and(|e| !e.deleted && e.hash == loser.hash);

    let join = |wire: &str| {
        let mut p = root.clone();
        for c in wire.split('/') {
            p.push(c);
        }
        p
    };
    let plain_fs = join(set_path);
    let loser_fs = join(&loser_path);

    if local_wins {
        // The staged remote version becomes the copy; the plain path keeps
        // our content, dominating both histories.
        if !occupied_same {
            let Some((mtime, size)) = install_file(staged, &loser_fs, incoming.mtime) else {
                return false;
            };
            let mut vv = Vv::new();
            if let Ok(tick) = state.clock.tick() {
                vv.set(self_component, tick);
            }
            state.index.upsert(Entry {
                path: loser_path.clone(),
                kind: crate::index::EntryKind::File,
                size,
                mtime,
                exec: incoming.exec,
                hash: incoming.hash.clone(),
                vv,
                deleted: false,
            });
        }
        let mut plain = local;
        plain.vv.merge_max(&incoming.vv);
        if let Ok(tick) = state.clock.tick() {
            plain.vv.set(self_component, tick);
        }
        state.index.upsert(plain);
    } else {
        // Our content moves aside under its honest name; the staged remote
        // version takes the plain path.
        if occupied_same {
            let _ = std::fs::remove_file(&plain_fs);
        } else {
            if std::fs::rename(&plain_fs, &loser_fs).is_err() {
                return false;
            }
            let mut vv = Vv::new();
            if let Ok(tick) = state.clock.tick() {
                vv.set(self_component, tick);
            }
            state.index.upsert(Entry {
                path: loser_path.clone(),
                kind: crate::index::EntryKind::File,
                size: loser.size,
                mtime: loser.mtime,
                exec: loser.exec,
                hash: loser.hash.clone(),
                vv,
                deleted: false,
            });
        }
        let Some((mtime, size)) = install_file(staged, &plain_fs, incoming.mtime) else {
            return false;
        };
        let mut vv = incoming.vv.clone();
        vv.merge_max(&winner.vv);
        vv.merge_max(&loser.vv);
        if let Ok(tick) = state.clock.tick() {
            vv.set(self_component, tick);
        }
        state.index.upsert(Entry {
            path: set_path.to_string(),
            kind: crate::index::EntryKind::File,
            size,
            mtime,
            exec: incoming.exec,
            hash: incoming.hash.clone(),
            vv,
            deleted: false,
        });
    }
    state.settle_pending(set_path, &incoming.vv);

    let record = ConflictRecord::sign(
        state.membership.descriptor.set_id.clone(),
        set_path.to_string(),
        conflict_id,
        loser_path,
        None,
        self_node.to_string(),
        identity,
    );
    state.absorb_conflict(&record);
    true
}

/// Moves a verified staged file into place, stamps the wanted mtime and
/// reads BACK what the filesystem kept.
fn install_file(
    staged: &Path,
    dest: &Path,
    want: crate::index::Mtime,
) -> Option<(crate::index::Mtime, u64)> {
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent).ok()?;
    }
    std::fs::rename(staged, dest).ok()?;
    let mtime = (|| {
        let file = std::fs::File::options().write(true).open(dest).ok()?;
        let secs = u64::try_from(want.secs).ok()?;
        let stamp = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::new(secs, want.nanos);
        file.set_times(std::fs::FileTimes::new().set_modified(stamp))
            .ok()?;
        Some(crate::index::Mtime::of(
            std::fs::metadata(dest).ok()?.modified().ok()?,
        ))
    })()
    .unwrap_or(want);
    let size = std::fs::metadata(dest).map(|m| m.len()).ok()?;
    Some((mtime, size))
}

/// The deterministic identity of one version: BLAKE3 over the canonical vv
/// and the content hash. Every device computes the same ids, so the fold
/// needs no negotiation.
pub fn version_id(entry: &Entry) -> String {
    let canonical = crate::canonical::to_canonical_json(&entry.vv.to_value())
        .expect("a vv is canonicalizable by construction");
    let mut hasher = blake3::Hasher::new();
    hasher.update(canonical.as_bytes());
    hasher.update(b"|");
    hasher.update(entry.hash.as_bytes());
    hasher.finalize().to_hex().to_string()
}

fn conflict_id_of(a: &str, b: &str) -> String {
    let (lo, hi) = if a <= b { (a, b) } else { (b, a) };
    let mut hasher = blake3::Hasher::new();
    hasher.update(lo.as_bytes());
    hasher.update(hi.as_bytes());
    hasher.finalize().to_hex().to_string()
}

/// The losing version's author, deterministically: the greatest component
/// key the loser's vv holds ABOVE the winner's - identical on every
/// detector, whatever their directories say.
fn loser_author(loser: &Vv, winner: &Vv) -> Option<String> {
    loser
        .components()
        .filter(|(key, value)| *value > winner.get(key))
        .map(|(key, _)| key.split('@').next().unwrap_or(key).to_string())
        .max()
}

/// "name (DeviceName, 2026-08-16 14h02 UTC).ext", sanitized against the
/// full gate, deterministically truncated to the component budget, with a
/// deterministic counter from the second collision on.
fn conflict_copy_name(
    base: &str,
    device: &str,
    mtime: crate::index::Mtime,
    counter: u32,
) -> String {
    let (stem, ext) = match base.rfind('.') {
        Some(i) if i > 0 => base.split_at(i),
        _ => (base, ""),
    };
    let tag = if counter <= 1 {
        format!(" ({device}, {})", utc_stamp(mtime.secs))
    } else {
        format!(" ({device}, {}) {counter}", utc_stamp(mtime.secs))
    };
    let mut stem = stem.to_string();
    loop {
        let name = format!("{stem}{tag}{ext}");
        if crate::wirepath::check_component(&name).is_ok() {
            return name;
        }
        if stem.pop().is_none() {
            // Nothing sane left: an ungainly but valid fallback.
            return format!("conflict{tag}").trim().to_string();
        }
    }
}

/// A directory display name fit for a filename: the separators, colon and
/// control characters go, leading dots go, 32 chars at most, no trailing
/// dot or space. Empty falls back to the node prefix at the caller.
fn sanitize_device_name(name: &str) -> String {
    use unicode_normalization::UnicodeNormalization;

    let cleaned: String = name
        .nfc()
        .filter(|c| !matches!(c, '/' | '\\' | ':') && !c.is_control())
        .collect();
    let cleaned = cleaned.trim_start_matches('.');
    let capped: String = cleaned.chars().take(32).collect();
    capped.trim_end_matches([' ', '.']).to_string()
}

/// Civil date from unix seconds (Howard Hinnant's civil_from_days),
/// "YYYY-MM-DD HHhMM UTC". No date crate: the algorithm is twelve lines.
fn utc_stamp(secs: i64) -> String {
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!(
        "{y:04}-{m:02}-{d:02} {:02}h{:02} UTC",
        rem / 3600,
        (rem % 3600) / 60
    )
}

/// A fresh unguessable set id: 22 chars of the base64url alphabet, 132
/// bits (each byte indexes the 64-symbol alphabet exactly four times: no
/// modulo bias).
fn mint_set_id() -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut bytes = [0u8; 22];
    rand::Rng::fill_bytes(&mut rand::rng(), &mut bytes);
    let id: String = bytes
        .iter()
        .map(|b| ALPHABET[(b & 0x3f) as usize] as char)
        .collect();
    debug_assert!(valid_set_id(&id));
    id
}

#[cfg(test)]
mod tests {
    use crate::identity::Identity;

    use super::*;

    const NOW: u64 = 1_700_000_000;

    fn node(letter: char) -> String {
        std::iter::repeat_n(letter, 64).collect()
    }

    struct Rig {
        engines: BTreeMap<String, Engine>,
        /// The simulated byte plane: tx_id -> published basename -> source
        /// fs path (what transactions.publish froze).
        txs: BTreeMap<String, BTreeMap<String, PathBuf>>,
        tx_counter: u64,
        _dirs: Vec<tempfile::TempDir>,
    }

    impl Rig {
        fn new(letters: &[char]) -> Rig {
            let mut engines = BTreeMap::new();
            let mut dirs = Vec::new();
            for letter in letters {
                let dir = tempfile::tempdir().expect("tempdir");
                let store = Store::open(dir.path().join("sync")).expect("store");
                let engine = Engine::open(store, node(*letter), NOW).expect("engine");
                engines.insert(node(*letter), engine);
                dirs.push(dir);
            }
            Rig {
                engines,
                txs: BTreeMap::new(),
                tx_counter: 0,
                _dirs: dirs,
            }
        }

        fn of(&mut self, letter: char) -> &mut Engine {
            self.engines.get_mut(&node(letter)).expect("engine")
        }

        fn peers_of(&self, letter: char) -> Vec<String> {
            self.engines
                .keys()
                .filter(|n| **n != node(letter))
                .cloned()
                .collect()
        }

        /// Delivers every queued effect, simulating the Core's byte plane
        /// (publish = freeze the source paths, adopt-and-fill = copy them
        /// into staging, transfer.finished immediately), feeding every
        /// answer back until quiescence. The budget assertion IS the
        /// termination proof.
        fn deliver(&mut self, from: &str, out: Vec<Effect>, now: u64) -> u64 {
            let mut queue: Vec<(String, Effect)> =
                out.into_iter().map(|e| (from.to_string(), e)).collect();
            let mut count = 0;
            while let Some((producer, effect)) = queue.pop() {
                count += 1;
                assert!(count < 800, "effect storm: the protocol must terminate");
                match effect {
                    Effect::Send(message) => {
                        let target_node = message.to.clone();
                        let target = self.engines.get_mut(&target_node).expect("target engine");
                        let more = target.on_message(&producer, &message.payload, now);
                        queue.extend(more.into_iter().map(|e| (target_node.clone(), e)));
                    }
                    Effect::Publish {
                        to,
                        set_id,
                        need_id,
                        paths,
                    } => {
                        self.tx_counter += 1;
                        let tx_id = format!("tx-{}", self.tx_counter);
                        let frozen: BTreeMap<String, PathBuf> = paths
                            .into_iter()
                            .map(|p| {
                                (
                                    p.file_name()
                                        .expect("published paths have names")
                                        .to_string_lossy()
                                        .into_owned(),
                                    p,
                                )
                            })
                            .collect();
                        self.txs.insert(tx_id.clone(), frozen);
                        let publisher = self.engines.get_mut(&producer).expect("publisher");
                        let more = publisher.on_published(&to, &set_id, need_id, &tx_id);
                        queue.extend(more.into_iter().map(|e| (producer.clone(), e)));
                    }
                    Effect::Revoke { tx_id } => {
                        self.txs.remove(&tx_id);
                    }
                    Effect::AdoptFill {
                        set_id,
                        need_id,
                        tx_id,
                        files,
                        staging,
                        ..
                    } => {
                        let Some(frozen) = self.txs.get(&tx_id) else {
                            let needer = self.engines.get_mut(&producer).expect("needer");
                            needer.on_pull_failed(&set_id, need_id);
                            continue;
                        };
                        std::fs::create_dir_all(&staging).expect("staging dir");
                        for wire in files.keys() {
                            let source = frozen.get(wire).expect("published file");
                            std::fs::copy(source, staging.join(wire)).expect("fill copy");
                        }
                        let transfer = format!("t-{need_id}");
                        let needer = self.engines.get_mut(&producer).expect("needer");
                        needer.on_pull_started(&set_id, need_id, &transfer);
                        let more = needer.on_transfer_outcome(&transfer, true);
                        queue.extend(more.into_iter().map(|e| (producer.clone(), e)));
                    }
                }
            }
            count
        }

        /// One pump of every engine toward every other, delivered to
        /// quiescence: "everyone talks until nobody has anything to say".
        fn settle(&mut self, now: u64) -> u64 {
            let mut total = 0;
            let nodes: Vec<String> = self.engines.keys().cloned().collect();
            for n in &nodes {
                let reachable: Vec<String> = nodes.iter().filter(|m| *m != n).cloned().collect();
                let out = self
                    .engines
                    .get_mut(n)
                    .expect("engine")
                    .pump(&reachable, now, true);
                total += self.deliver(n, out, now);
            }
            total
        }
    }

    /// A root with a couple of files, scanned into the creator's index.
    fn set_with_files(rig: &mut Rig, creator: char) -> (String, tempfile::TempDir) {
        let root = tempfile::tempdir().expect("root");
        std::fs::write(root.path().join("a.txt"), "alpha").expect("write");
        std::fs::create_dir(root.path().join("sub")).expect("mkdir");
        std::fs::write(root.path().join("sub").join("b.txt"), "beta").expect("write");
        let engine = rig.of(creator);
        let set_id = engine
            .create_set(
                root.path().to_path_buf(),
                SetKind::Dir,
                "Projects".into(),
                NOW,
            )
            .expect("create");
        engine.rescan_set(&set_id).expect("rescan");
        (set_id, root)
    }

    /// A pair created, consented and fully converged (contents included).
    fn synced_pair(rig: &mut Rig) -> (String, tempfile::TempDir) {
        let (set_id, a_root) = set_with_files(rig, 'a');
        rig.of('a')
            .invite(&set_id, &node('b'), NOW)
            .expect("invite");
        let peers = rig.peers_of('a');
        let out = rig.of('a').pump(&peers, NOW + 1, false);
        rig.deliver(&node('a'), out, NOW + 1);
        let b_root = tempfile::tempdir().expect("b root");
        rig.of('b')
            .accept(&set_id, b_root.path().to_path_buf(), NOW + 2)
            .expect("accept");
        rig._dirs.push(b_root);
        for i in 0..4 {
            rig.settle(NOW + 10 + i * 10);
        }
        assert!(
            rig.of('b').set(&set_id).expect("state").pending.is_empty(),
            "the pair must converge before the scenario starts"
        );
        (set_id, a_root)
    }

    fn root_of(rig: &mut Rig, letter: char, set_id: &str) -> PathBuf {
        rig.of(letter)
            .set(set_id)
            .expect("state")
            .root
            .clone()
            .expect("root")
    }

    /// A creates and invites; B consents; the first ordinary rounds carry
    /// the records both ways and A's entries into B's pending set, and the
    /// exchange goes quiet.
    #[test]
    fn create_invite_accept_and_the_first_rounds_pull_everything() {
        let mut rig = Rig::new(&['a', 'b']);
        let (set_id, _root) = set_with_files(&mut rig, 'a');
        rig.of('a')
            .invite(&set_id, &node('b'), NOW)
            .expect("invite");

        let reachable = rig.peers_of('a');
        let out = rig.of('a').pump(&reachable, NOW + 1, false);
        assert!(!out.is_empty(), "the invitation must flow");
        rig.deliver(&node('a'), out, NOW + 1);

        // B holds the shadow state: invited, with the inviter's claim.
        assert_eq!(
            rig.of('b')
                .set(&set_id)
                .expect("shadow state")
                .membership
                .effective(&node('b')),
            Effective::Invited
        );
        let claim = rig
            .of('b')
            .set(&set_id)
            .expect("state")
            .invite_claim
            .clone()
            .expect("claim");
        assert_eq!(claim.inviter, node('a'));
        assert_eq!(claim.entries, 3, "two files and a directory");
        // The ack landed: A stopped retrying.
        let peers = rig.peers_of('a');
        assert!(rig.of('a').pump(&peers, NOW + 20, false).is_empty());

        // Consent, then the rounds.
        let b_root = tempfile::tempdir().expect("b root");
        rig.of('b')
            .accept(&set_id, b_root.path().to_path_buf(), NOW + 2)
            .expect("accept");
        rig.settle(NOW + 10);
        rig.settle(NOW + 20);

        // Both see both active.
        for letter in ['a', 'b'] {
            for member in ['a', 'b'] {
                assert_eq!(
                    rig.of(letter)
                        .set(&set_id)
                        .expect("state")
                        .membership
                        .effective(&node(member)),
                    Effective::Active,
                    "{letter} about {member}"
                );
            }
        }
        // The bytes flowed: B's tree converged on A's, the pending set
        // drained, and B's watermark for A covers A's position.
        rig.settle(NOW + 30);
        let b_state = rig.of('b').set(&set_id).expect("state");
        assert!(b_state.pending.is_empty(), "{:?}", b_state.pending.keys());
        let b_root_path = b_state.root.clone().expect("root");
        assert_eq!(
            std::fs::read_to_string(b_root_path.join("a.txt")).expect("landed"),
            "alpha"
        );
        assert_eq!(
            std::fs::read_to_string(b_root_path.join("sub").join("b.txt")).expect("landed"),
            "beta"
        );
        let a_watermark = b_state.watermarks.get(&node('a')).expect("watermark");
        assert!(!a_watermark.is_empty());

        // Every published transaction was revoked once its done landed.
        assert!(rig.txs.is_empty(), "done must revoke: {:?}", rig.txs.keys());

        // An edit at A reaches B's disk on the next exchanges.
        let a_root = rig
            .of('a')
            .set(&set_id)
            .expect("state")
            .root
            .clone()
            .expect("root");
        std::fs::write(a_root.join("a.txt"), "alpha v2").expect("edit");
        rig.of('a').rescan_set(&set_id).expect("rescan");
        rig.settle(NOW + 50);
        rig.settle(NOW + 60);
        assert_eq!(
            std::fs::read_to_string(b_root_path.join("a.txt")).expect("landed"),
            "alpha v2"
        );

        // Quiescence: another settle moves nothing.
        let before = rig.settle(NOW + 80);
        let after = rig.settle(NOW + 90);
        assert!(after <= before, "the exchange must quiet down");
    }

    /// A deletion travels as a tombstone; a CONCURRENT edit beats it
    /// everywhere and the tombstone retires instead of oscillating.
    #[test]
    fn a_deletion_travels_and_a_concurrent_edit_survives_it() {
        let mut rig = Rig::new(&['a', 'b']);
        let (set_id, _a_root) = synced_pair(&mut rig);
        let a_root = root_of(&mut rig, 'a', &set_id);
        let b_root = root_of(&mut rig, 'b', &set_id);

        // Plain deletion: A removes, B follows.
        std::fs::remove_file(a_root.join("a.txt")).expect("rm");
        rig.of('a').rescan_set(&set_id).expect("rescan");
        rig.settle(NOW + 100);
        rig.settle(NOW + 110);
        assert!(!b_root.join("a.txt").exists(), "the tombstone must land");
        assert!(
            rig.of('b')
                .set(&set_id)
                .expect("state")
                .index
                .get("a.txt")
                .expect("tombstone")
                .deleted
        );

        // Concurrent delete vs edit: A deletes sub/b.txt while B edits it.
        std::fs::remove_file(a_root.join("sub").join("b.txt")).expect("rm");
        rig.of('a').rescan_set(&set_id).expect("rescan");
        std::fs::write(b_root.join("sub").join("b.txt"), "beta edited").expect("edit");
        rig.of('b').rescan_set(&set_id).expect("rescan");
        for i in 0..4 {
            rig.settle(NOW + 120 + i * 10);
        }
        for letter in ['a', 'b'] {
            let root = root_of(&mut rig, letter, &set_id);
            assert_eq!(
                std::fs::read_to_string(root.join("sub").join("b.txt")).expect("survives"),
                "beta edited",
                "the edit wins at {letter}"
            );
        }
    }

    /// Both sides make the IDENTICAL change concurrently: silent merge, no
    /// conflict, no re-transfer ping-pong.
    #[test]
    fn identical_concurrent_writes_merge_silently() {
        let mut rig = Rig::new(&['a', 'b']);
        let (set_id, _a_root) = synced_pair(&mut rig);
        let a_root = root_of(&mut rig, 'a', &set_id);
        let b_root = root_of(&mut rig, 'b', &set_id);

        std::fs::write(a_root.join("same.txt"), "twin").expect("write");
        std::fs::write(b_root.join("same.txt"), "twin").expect("write");
        rig.of('a').rescan_set(&set_id).expect("rescan");
        rig.of('b').rescan_set(&set_id).expect("rescan");
        for i in 0..3 {
            rig.settle(NOW + 200 + i * 10);
        }
        for letter in ['a', 'b'] {
            let state = rig.of(letter).set(&set_id).expect("state");
            let entry = state.index.get("same.txt").expect("indexed");
            assert!(!entry.deleted);
            assert!(
                !state.pending.contains_key("same.txt"),
                "nothing pending at {letter}"
            );
            // The merged vv carries BOTH devices' components.
            assert!(entry.vv.components().count() >= 2, "at {letter}");
        }
    }

    /// A directory tombstone loses to a concurrent live descendant: the
    /// chain survives with a dominating vv, on both ends.
    #[test]
    fn a_directory_tombstone_loses_to_a_concurrent_descendant() {
        let mut rig = Rig::new(&['a', 'b']);
        let (set_id, _a_root) = synced_pair(&mut rig);
        let a_root = root_of(&mut rig, 'a', &set_id);
        let b_root = root_of(&mut rig, 'b', &set_id);

        // A removes the whole folder while B drops a new file into it.
        std::fs::remove_file(a_root.join("sub").join("b.txt")).expect("rm");
        std::fs::remove_dir(a_root.join("sub")).expect("rmdir");
        rig.of('a').rescan_set(&set_id).expect("rescan");
        std::fs::write(b_root.join("sub").join("new.txt"), "fresh").expect("write");
        rig.of('b').rescan_set(&set_id).expect("rescan");
        for i in 0..5 {
            rig.settle(NOW + 300 + i * 10);
        }
        for letter in ['a', 'b'] {
            let root = root_of(&mut rig, letter, &set_id);
            assert_eq!(
                std::fs::read_to_string(root.join("sub").join("new.txt")).expect("survives"),
                "fresh",
                "the descendant survives at {letter}"
            );
            assert!(
                !root.join("sub").join("b.txt").exists(),
                "the uncontested deletion still lands at {letter}"
            );
            let state = rig.of(letter).set(&set_id).expect("state");
            assert!(!state.index.get("sub").expect("dir").deleted, "at {letter}");
        }
    }

    /// Pause travels as a records-only push, the paused device keeps
    /// answering records-only, and nothing it says advances a watermark.
    #[test]
    fn pause_travels_and_a_paused_answer_advances_nothing() {
        let mut rig = Rig::new(&['a', 'b']);
        let (set_id, _root) = set_with_files(&mut rig, 'a');
        rig.of('a')
            .invite(&set_id, &node('b'), NOW)
            .expect("invite");
        let peers = rig.peers_of('a');
        let out = rig.of('a').pump(&peers, NOW + 1, false);
        rig.deliver(&node('a'), out, NOW + 1);
        let b_root = tempfile::tempdir().expect("b root");
        rig.of('b')
            .accept(&set_id, b_root.path().to_path_buf(), NOW + 2)
            .expect("accept");
        rig.settle(NOW + 10);

        rig.of('b')
            .sign_status(&set_id, MemberStatus::Paused, NOW + 20)
            .expect("pause");
        let peers = rig.peers_of('b');
        let out = rig.of('b').pump(&peers, NOW + 21, false);
        assert!(!out.is_empty(), "the pause must push a records-only head");
        rig.deliver(&node('b'), out, NOW + 21);
        assert_eq!(
            rig.of('a')
                .set(&set_id)
                .expect("state")
                .membership
                .effective(&node('b')),
            Effective::Paused,
            "A shows paused, not offline"
        );

        // A edits; rounds run; B's watermark for A must NOT advance from
        // B's records-only answers... and A must not even open entries
        // rounds toward a paused member.
        let a_watermark_before = rig
            .of('b')
            .set(&set_id)
            .expect("state")
            .watermarks
            .get(&node('a'))
            .cloned();
        rig.settle(NOW + 30);
        let a_watermark_after = rig
            .of('b')
            .set(&set_id)
            .expect("state")
            .watermarks
            .get(&node('a'))
            .cloned();
        assert_eq!(a_watermark_before, a_watermark_after);

        // Resume: the announce travels, rounds resume.
        rig.of('b')
            .sign_status(&set_id, MemberStatus::Active, NOW + 40)
            .expect("resume");
        let peers = rig.peers_of('b');
        let out = rig.of('b').pump(&peers, NOW + 41, false);
        rig.deliver(&node('b'), out, NOW + 41);
        assert_eq!(
            rig.of('a')
                .set(&set_id)
                .expect("state")
                .membership
                .effective(&node('b')),
            Effective::Active
        );
    }

    /// Three members: B and C both consented through A. The introductions
    /// pin B and C to each other without A's mediation, and everyone ends
    /// verified active everywhere with A's entries parked on both.
    #[test]
    fn introductions_pin_the_pair_that_never_met() {
        let mut rig = Rig::new(&['a', 'b', 'c']);
        let (set_id, _root) = set_with_files(&mut rig, 'a');
        rig.of('a')
            .invite(&set_id, &node('b'), NOW)
            .expect("invite b");
        rig.of('a')
            .invite(&set_id, &node('c'), NOW)
            .expect("invite c");
        let peers = rig.peers_of('a');
        let out = rig.of('a').pump(&peers, NOW + 1, false);
        rig.deliver(&node('a'), out, NOW + 1);

        let b_root = tempfile::tempdir().expect("b root");
        let c_root = tempfile::tempdir().expect("c root");
        rig.of('b')
            .accept(&set_id, b_root.path().to_path_buf(), NOW + 2)
            .expect("accept b");
        rig.of('c')
            .accept(&set_id, c_root.path().to_path_buf(), NOW + 3)
            .expect("accept c");

        for round in 0..4 {
            rig.settle(NOW + 10 + round * 10);
        }

        for viewer in ['a', 'b', 'c'] {
            for member in ['a', 'b', 'c'] {
                assert_eq!(
                    rig.of(viewer)
                        .set(&set_id)
                        .expect("state")
                        .membership
                        .effective(&node(member)),
                    Effective::Active,
                    "{viewer} about {member}"
                );
            }
        }
        for follower in ['b', 'c'] {
            let state = rig.of(follower).set(&set_id).expect("state");
            let root = state.root.clone().expect("root");
            assert!(
                state.pending.is_empty(),
                "at {follower}: {:?}",
                state.pending.keys()
            );
            assert_eq!(
                std::fs::read_to_string(root.join("a.txt")).expect("landed"),
                "alpha",
                "A's bytes landed at {follower}"
            );
        }
    }

    /// The relay rule, whole: an entry a relay has absorbed but not yet
    /// pulled the bytes of still travels onward - its advertised vv covers
    /// it, so withholding it would punch a permanent hole at the third
    /// device.
    #[test]
    fn a_relay_serves_what_it_holds_only_as_pending() {
        let mut rig = Rig::new(&['a', 'b', 'c']);
        let (set_id, _root) = set_with_files(&mut rig, 'a');
        rig.of('a')
            .invite(&set_id, &node('b'), NOW)
            .expect("invite b");
        rig.of('a')
            .invite(&set_id, &node('c'), NOW)
            .expect("invite c");
        let peers = rig.peers_of('a');
        let out = rig.of('a').pump(&peers, NOW + 1, false);
        rig.deliver(&node('a'), out, NOW + 1);
        for (letter, at) in [('b', NOW + 2), ('c', NOW + 3)] {
            let root = tempfile::tempdir().expect("root");
            rig.of(letter)
                .accept(&set_id, root.path().to_path_buf(), at)
                .expect("accept");
            rig._dirs.push(root);
        }
        for i in 0..4 {
            rig.settle(NOW + 10 + i * 10);
        }

        // C edits, talks to B ONLY, and goes away.
        let c_root = root_of(&mut rig, 'c', &set_id);
        std::fs::write(c_root.join("from-c.txt"), "made by c").expect("write");
        rig.of('c').rescan_set(&set_id).expect("rescan");
        let out = rig.of('c').pump(&[node('b')], NOW + 100, true);
        rig.deliver(&node('c'), out, NOW + 100);

        // A and B exchange with C absent: B must forward C's entry even
        // though B itself has not pulled the bytes yet.
        let out = rig.of('a').pump(&[node('b')], NOW + 110, true);
        rig.deliver(&node('a'), out, NOW + 110);
        let a_state = rig.of('a').set(&set_id).expect("state");
        assert!(
            a_state.pending.contains_key("from-c.txt") || a_state.index.get("from-c.txt").is_some(),
            "C's entry must reach A through B: {:?}",
            a_state.pending.keys()
        );
    }

    /// Leaving grows a stub that keeps proving itself until the member
    /// echoes the terminal record back, then drops.
    #[test]
    fn a_leavers_stub_proves_itself_and_drops() {
        let mut rig = Rig::new(&['a', 'b']);
        let (set_id, _root) = set_with_files(&mut rig, 'a');
        rig.of('a')
            .invite(&set_id, &node('b'), NOW)
            .expect("invite");
        let peers = rig.peers_of('a');
        let out = rig.of('a').pump(&peers, NOW + 1, false);
        rig.deliver(&node('a'), out, NOW + 1);
        let b_root = tempfile::tempdir().expect("b root");
        rig.of('b')
            .accept(&set_id, b_root.path().to_path_buf(), NOW + 2)
            .expect("accept");
        rig.settle(NOW + 10);

        rig.of('b')
            .sign_status(&set_id, MemberStatus::Left, NOW + 20)
            .expect("leave");
        assert!(rig.of('b').set(&set_id).expect("state").stub_active());

        // First exchange carries the news; the second carries the echo that
        // proves it.
        for i in 0..3 {
            let peers = rig.peers_of('b');
            let out = rig.of('b').pump(&peers, NOW + 21 + i, false);
            rig.deliver(&node('b'), out, NOW + 21 + i);
        }
        assert_eq!(
            rig.of('a')
                .set(&set_id)
                .expect("state")
                .membership
                .effective(&node('b')),
            Effective::Left
        );
        assert!(
            !rig.of('b').set(&set_id).expect("state").stub_active(),
            "the echoed terminal record is the proof that drops the stub"
        );
    }

    /// A head about a set the receiver holds nothing of gets the
    /// no-membership marker, and the marker drops the stranger's rows at
    /// the sender.
    #[test]
    fn the_no_membership_marker_stops_the_introductions() {
        let mut rig = Rig::new(&['a', 'd']);
        let (set_id, _root) = set_with_files(&mut rig, 'a');

        // Gossip named D (an unverifiable record parked at A): D becomes an
        // introduction target.
        let d_dir = tempfile::tempdir().expect("tempdir");
        let d_identity = Identity::load_or_generate(d_dir.path()).expect("identity");
        let stray = MemberRecord::sign_own(
            set_id.clone(),
            node('d'),
            MemberStatus::Active,
            1,
            1,
            NOW,
            &d_identity,
        );
        let a = rig.of('a');
        let absorbed = a
            .set_mut(&set_id)
            .expect("state")
            .membership
            .absorb(&Record::Member(stray));
        assert_eq!(absorbed, Absorb::Unverified);

        let out = a.pump(&[node('d')], NOW + 1, false);
        assert!(
            out.iter()
                .any(|e| matches!(e, Effect::Send(o) if o.to == node('d'))),
            "an introduction must target the stranger"
        );
        rig.deliver(&node('a'), out, NOW + 1);

        // D answered the marker; A dropped the row and stopped introducing.
        assert!(
            rig.of('a')
                .set(&set_id)
                .expect("state")
                .membership
                .unpinned_devices()
                .is_empty()
        );
        assert!(rig.of('a').pump(&[node('d')], NOW + 2, false).is_empty());
    }

    /// Entries from a device that is not a verified-active member are
    /// dropped even when the pages are well-formed: the authorization
    /// table, adversarially.
    #[test]
    fn entries_from_a_non_active_sender_are_dropped() {
        let mut rig = Rig::new(&['a', 'b']);
        let (set_id, _root) = set_with_files(&mut rig, 'a');
        rig.of('a')
            .invite(&set_id, &node('b'), NOW)
            .expect("invite");
        let peers = rig.peers_of('a');
        let out = rig.of('a').pump(&peers, NOW + 1, false);
        rig.deliver(&node('a'), out, NOW + 1);
        // B is INVITED, not active - and now crafts a full entries round.
        let descriptor = rig
            .of('b')
            .set(&set_id)
            .expect("state")
            .membership
            .descriptor
            .clone();
        let b_identity_pub = {
            let dir = tempfile::tempdir().expect("tempdir");
            Identity::load_or_generate(dir.path())
                .expect("identity")
                .public_hex()
        };
        let mut vv = Vv::new();
        vv.set(&crate::vv::component(&node('b'), 1), 5);
        let entry = Entry {
            path: "planted.txt".into(),
            kind: crate::index::EntryKind::File,
            size: 4,
            mtime: crate::index::Mtime::default(),
            exec: false,
            hash: "c".repeat(64),
            vv: vv.clone(),
            deleted: false,
        };
        let head = Message::Head {
            set_id: set_id.clone(),
            round: 99,
            answers: None,
            position: Some(HeadPosition {
                descriptor_hash: descriptor_hash(&descriptor),
                set_vv: vv,
                records_pages: 0,
                entries_pages: 1,
                entries_complete: true,
            }),
            sync_pub: b_identity_pub,
        };
        let page = Message::Entries {
            set_id: set_id.clone(),
            round: 99,
            page: 0,
            entries: vec![entry],
        };
        let a = rig.of('a');
        a.on_message(&node('b'), &head.to_value(), NOW + 5);
        a.on_message(&node('b'), &page.to_value(), NOW + 5);
        let state = a.set(&set_id).expect("state");
        assert!(state.pending.is_empty(), "no entry from a non-member");
        assert!(!state.watermarks.contains_key(&node('b')));
    }

    /// Edit vs edit: both sides keep BOTH, the plain path converges on the
    /// same winner everywhere, the loser sits beside it under the same
    /// honest name, and one open conflict record is shared.
    #[test]
    fn concurrent_edits_keep_both_deterministically() {
        let mut rig = Rig::new(&['a', 'b']);
        let (set_id, _a_root) = synced_pair(&mut rig);
        for letter in ['a', 'b'] {
            let names: BTreeMap<String, String> = [
                (node('a'), "Alpha PC".to_string()),
                (node('b'), "Beta:Box".to_string()),
            ]
            .into();
            rig.of(letter).set_directory(DirectoryView {
                names,
                ..DirectoryView::default()
            });
        }
        let a_root = root_of(&mut rig, 'a', &set_id);
        let b_root = root_of(&mut rig, 'b', &set_id);

        std::fs::write(a_root.join("a.txt"), "the a version").expect("edit");
        std::fs::write(b_root.join("a.txt"), "the b version").expect("edit");
        rig.of('a').rescan_set(&set_id).expect("rescan");
        rig.of('b').rescan_set(&set_id).expect("rescan");
        for i in 0..6 {
            rig.settle(NOW + 400 + i * 10);
        }

        let plain_a = std::fs::read_to_string(a_root.join("a.txt")).expect("plain at a");
        let plain_b = std::fs::read_to_string(b_root.join("a.txt")).expect("plain at b");
        assert_eq!(plain_a, plain_b, "the plain path converges on one winner");

        let copies = |root: &PathBuf| -> Vec<String> {
            let mut out: Vec<String> = std::fs::read_dir(root)
                .expect("dir")
                .flatten()
                .filter_map(|e| e.file_name().to_str().map(str::to_string))
                .filter(|n| n.starts_with("a (") && n.ends_with(".txt"))
                .collect();
            out.sort();
            out
        };
        let copies_a = copies(&a_root);
        let copies_b = copies(&b_root);
        assert_eq!(copies_a.len(), 1, "one loser copy at a: {copies_a:?}");
        assert_eq!(copies_a, copies_b, "the copy name is deterministic");
        let copy_a = std::fs::read_to_string(a_root.join(&copies_a[0])).expect("copy");
        let copy_b = std::fs::read_to_string(b_root.join(&copies_b[0])).expect("copy");
        assert_eq!(copy_a, copy_b);
        assert_ne!(copy_a, plain_a, "the copy holds the OTHER content");
        let both: BTreeSet<String> = [plain_a, copy_a].into();
        assert_eq!(
            both,
            ["the a version".to_string(), "the b version".to_string()].into(),
            "nothing was lost"
        );

        // One shared open record; nothing left pending.
        for letter in ['a', 'b'] {
            let state = rig.of(letter).set(&set_id).expect("state");
            let open: Vec<_> = state.conflicts.values().filter(|r| !r.resolved).collect();
            assert_eq!(open.len(), 1, "one open conflict at {letter}");
            assert_eq!(open[0].path, "a.txt");
            assert!(
                state.pending.is_empty(),
                "at {letter}: {:?}",
                state.pending.keys()
            );
        }
    }

    /// Resolving anywhere resolves everywhere: the kept version takes the
    /// plain path, the other copy is deleted, the record travels resolved.
    #[test]
    fn resolving_a_conflict_travels() {
        let mut rig = Rig::new(&['a', 'b']);
        let (set_id, _a_root) = synced_pair(&mut rig);
        let a_root = root_of(&mut rig, 'a', &set_id);
        let b_root = root_of(&mut rig, 'b', &set_id);
        std::fs::write(a_root.join("a.txt"), "the a version").expect("edit");
        std::fs::write(b_root.join("a.txt"), "the b version").expect("edit");
        rig.of('a').rescan_set(&set_id).expect("rescan");
        rig.of('b').rescan_set(&set_id).expect("rescan");
        for i in 0..6 {
            rig.settle(NOW + 500 + i * 10);
        }

        // Keep the version currently sitting AT THE COPY, from A.
        let record = rig
            .of('a')
            .set(&set_id)
            .expect("state")
            .conflicts
            .values()
            .find(|r| !r.resolved)
            .cloned()
            .expect("open conflict");
        let copy_entry = rig
            .of('a')
            .set(&set_id)
            .expect("state")
            .index
            .get(&record.path_on_disk)
            .cloned()
            .expect("copy indexed");
        let keep = version_id(&copy_entry);
        let kept_content =
            std::fs::read_to_string(a_root.join(&record.path_on_disk)).expect("copy");
        rig.of('a')
            .resolve(&set_id, "a.txt", &keep)
            .expect("resolve");
        for i in 0..6 {
            rig.settle(NOW + 600 + i * 10);
        }

        for (letter, root) in [('a', &a_root), ('b', &b_root)] {
            assert_eq!(
                std::fs::read_to_string(root.join("a.txt")).expect("plain"),
                kept_content,
                "the kept version rules the plain path at {letter}"
            );
            assert!(
                !root.join(&record.path_on_disk).exists(),
                "the other copy is gone at {letter}"
            );
            let state = rig.of(letter).set(&set_id).expect("state");
            assert!(
                state.conflicts.values().all(|r| r.resolved),
                "the record travels resolved to {letter}"
            );
        }
    }

    /// The self-component guard: an active member inflating OUR component
    /// makes the clock jump past the claim, so nothing we edit next can be
    /// covered by the lie.
    #[test]
    fn an_impossible_claim_about_our_clock_is_outrun() {
        let mut rig = Rig::new(&['a', 'b']);
        let (set_id, _a_root) = synced_pair(&mut rig);
        let a_root = root_of(&mut rig, 'a', &set_id);
        let gen_a = rig.of('a').set(&set_id).expect("state").clock.generation();
        let b_pub = rig.of('b').sync_pub();
        let descriptor = rig
            .of('a')
            .set(&set_id)
            .expect("state")
            .membership
            .descriptor
            .clone();

        // B (a real, active member) declares a set_vv claiming A's own
        // component far above A's clock.
        let claim = 40_000;
        let mut lying_vv = Vv::new();
        lying_vv.set(&crate::vv::component(&node('a'), gen_a), claim);
        let head = Message::Head {
            set_id: set_id.clone(),
            round: 4242,
            answers: None,
            position: Some(HeadPosition {
                descriptor_hash: descriptor_hash(&descriptor),
                set_vv: lying_vv,
                records_pages: 0,
                entries_pages: 0,
                entries_complete: false,
            }),
            sync_pub: b_pub,
        };
        rig.of('a')
            .on_message(&node('b'), &head.to_value(), NOW + 100);

        // A's very next local edit outruns the claim.
        std::fs::write(a_root.join("next.txt"), "after the lie").expect("write");
        rig.of('a').rescan_set(&set_id).expect("rescan");
        let entry = rig
            .of('a')
            .set(&set_id)
            .expect("state")
            .index
            .get("next.txt")
            .expect("indexed")
            .clone();
        assert!(
            entry.vv.get(&crate::vv::component(&node('a'), gen_a)) > claim,
            "the clock must jump past the impossible claim"
        );
    }

    /// The engine's whole conversation state survives a restart.
    #[test]
    fn the_engine_state_survives_a_restart() {
        let dir_a = tempfile::tempdir().expect("tempdir");
        let dir_b = tempfile::tempdir().expect("tempdir");
        let root = tempfile::tempdir().expect("root");
        std::fs::write(root.path().join("a.txt"), "alpha").expect("write");

        let store_a = Store::open(dir_a.path().join("sync")).expect("store");
        let mut a = Engine::open(store_a, node('a'), NOW).expect("engine");
        let store_b = Store::open(dir_b.path().join("sync")).expect("store");
        let mut b = Engine::open(store_b, node('b'), NOW).expect("engine");

        let set_id = a
            .create_set(
                root.path().to_path_buf(),
                SetKind::Dir,
                "Projects".into(),
                NOW,
            )
            .expect("create");
        a.rescan_set(&set_id).expect("rescan");
        a.invite(&set_id, &node('b'), NOW).expect("invite");
        for out in a.pump(&[node('b')], NOW + 1, false) {
            let Effect::Send(out) = out else { continue };
            for back in b.on_message(&node('a'), &out.payload, NOW + 1) {
                let Effect::Send(back) = back else { continue };
                a.on_message(&node('b'), &back.payload, NOW + 1);
            }
        }
        let b_root = tempfile::tempdir().expect("b root");
        b.accept(&set_id, b_root.path().to_path_buf(), NOW + 2)
            .expect("accept");

        // The restart: everything reloads from disk.
        drop(b);
        let store_b = Store::open(dir_b.path().join("sync")).expect("store");
        let b = Engine::open(store_b, node('b'), NOW + 10).expect("reopen");
        let state = b.set(&set_id).expect("state");
        assert_eq!(state.membership.effective(&node('b')), Effective::Active);
        assert_eq!(state.root.as_deref(), Some(b_root.path()));
        assert_eq!(
            state.membership.effective(&node('a')),
            Effective::Active,
            "the roster survived"
        );
    }
}
