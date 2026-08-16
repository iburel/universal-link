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

/// Sets this engine will hold at once. A real account has a handful; the
/// cap is what keeps an active-but-compromised sibling from minting
/// persisted state without end.
const SETS_MAX: usize = 64;

/// Rounds one peer may have in flight here at once. A legitimate exchange
/// uses one; the cap is what keeps a compromised sibling from parking
/// memory in our buffers.
const ROUNDS_IN_FLIGHT_PER_PEER: usize = 4;

/// Conflict records held per set: far above any real number of open
/// conflicts, and a bound a member cannot push past.
const CONFLICTS_MAX: usize = 10_000;

/// Rename attempts a parked entry spends on the ordinary drains before it
/// waits for a walk instead. Its bytes are kept either way.
const PARKED_ATTEMPTS_MAX: u32 = 16;

/// (peer, path) refusals remembered, and for how long: long enough that a
/// pump does not loop on the same peer, short enough that a relay which has
/// since pulled the bytes is asked again.
const REFUSALS_MAX: usize = 4096;
const REFUSAL_TTL_SECS: u64 = 300;

/// Terminal outcomes remembered while their transfer id is still in
/// flight: a handful is plenty (at most two pulls per peer).
const EARLY_OUTCOMES_MAX: usize = 16;

/// How long a need waits for its offer and its bytes before it is
/// reclaimed. Generous: a real pull of a large batch takes minutes, and
/// reclaiming early would only duplicate work.
const NEED_TTL_SECS: u64 = 600;

/// Paths the pending set may hold per set: bounds what a hostile active
/// member can make us remember (and write to meta.json). Far above any
/// real delta - a bigger initial sync simply arrives in several rounds.
const PENDING_PATHS_MAX: usize = 100_000;

/// Minimum seconds between full answers to one peer: an inbound head costs
/// us a delta computation, so a flood of tiny heads must not reflect our
/// whole index back at line rate.
const ANSWER_INTERVAL_SECS: u64 = 1;

/// Minimum seconds between needs served to one peer: a publish walks a tree
/// in the Core, so serving must not be free to a caller.
const SERVE_INTERVAL_SECS: u64 = 1;

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
    /// When we last answered each peer with a full leg (the amplification
    /// throttle). Ephemeral: a restart owes everyone an answer.
    last_answered: BTreeMap<String, u64>,
    /// When we last served each peer a need (the publish rate limit).
    last_served: BTreeMap<String, u64>,
    /// (peer, path) pairs a peer has told us it cannot serve, and when.
    /// Ephemeral and bounded: it steers the next need at another member.
    refused: BTreeMap<(String, String), u64>,
    /// When a complete leg from each peer was last absorbed: the card's
    /// "offline since T" (section 4), which is OUR fact rather than the
    /// directory's. Ephemeral: after a restart the directory's word does.
    last_round: BTreeMap<String, u64>,
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
    /// The root itself could not be read (unmounted, renamed, deleted):
    /// said on the card, and nothing is tombstoned on that word alone.
    root_unreadable: bool,
    /// The disk said no: pulling stops and the card says why, until a
    /// resume gesture. Persisted, because a full disk survives a restart.
    disk_full: bool,
    /// Terminal transfer outcomes that arrived before the message naming
    /// their transfer id: applied the moment it registers. Bounded.
    early_outcomes: BTreeMap<String, (bool, String)>,
    /// Paths whose bytes keep failing to arrive, with their strike count:
    /// beyond one strike a path is needed ALONE, so one hot file cannot
    /// starve a batch. In memory - a restart may as well try afresh.
    hot: BTreeMap<String, u32>,
}

#[derive(Clone, Debug)]
struct ParkedEntry {
    staged: PathBuf,
    entry: Entry,
    reason: String,
    /// Rename attempts spent. Past the cap the entry stops retrying on
    /// every drain and waits for the safety tick's walk, so a permanently
    /// locked file costs a rename per walk rather than per round.
    attempts: u32,
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
    /// When the need was minted (or its pull last progressed). A source that
    /// silently drops a need must not wedge these paths for ever: past the
    /// TTL the need is reclaimed and the paths are needed again, from
    /// whoever can serve them.
    born_at: u64,
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
            // Seeded per boot, in the canonical integer's safe range: two
            // incarnations must never mint the same round id, or a peer's
            // stale buffer could complete under a fresh head and advance a
            // watermark over entries never absorbed.
            round_counter: boot_round_base(),
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

    /// The orchestrator's word on whether the root could be read at all: an
    /// unmounted or deleted folder is the loudest problem a set can have,
    /// and going quiet about it would look like "in order".
    pub fn set_root_unreadable(&mut self, set_id: &str, unreadable: bool) {
        if let Some(state) = self.sets.get_mut(set_id) {
            state.root_unreadable = unreadable;
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
            // A device the ACCOUNT revoked is gone from the directory, and
            // gone from here too: its rows go, rather than lingering under a
            // label no interface could resolve.
            let Some(device_id) = self.directory.device_of.get(&node).cloned() else {
                continue;
            };
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
            // "N files behind": live FILES, never tombstones or directories -
            // a count a human can compare with what they see in the folder.
            let ours = || {
                state
                    .index
                    .live()
                    .filter(|e| e.kind == crate::index::EntryKind::File)
            };
            let behind = match state.watermarks.get(&node) {
                _ if node == self.self_node => 0,
                Some(watermark) => ours().filter(|e| !watermark.covers(&e.vv)).count(),
                None => ours().count(),
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
                "device_id": device_id,
                "membership": membership,
                "sync": sync,
                "behind": behind,
                // Our own last successful round with it, which is what the
                // card means by "offline since"; the directory's word is the
                // fallback after a restart.
                "last_seen": state
                    .last_round
                    .get(&node)
                    .map(|at| Value::from(*at))
                    .or_else(|| self.directory.last_seen.get(&node).cloned())
                    .unwrap_or(Value::Null),
            }));
        }

        let conflicts: Vec<Value> = open_conflicts
            .iter()
            // Announced AFTER materialization, per the contract: a record
            // that reached us by gossip before its second version did says
            // nothing an interface could act on, so it waits.
            .filter(|record| {
                [record.path.as_str(), record.path_on_disk.as_str()]
                    .iter()
                    .all(|path| state.index.get(path).is_some_and(|e| !e.deleted))
            })
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

        // Never claim to be in order before a single round completed: a
        // freshly accepted set holds nothing, and "in_order" would be a lie
        // the user could act on. And "conflicts" counts the MATERIALIZED
        // ones: a record that arrived before its second version says nothing
        // an interface could act on yet.
        let ever_synced = !state.last_round.is_empty() || !state.watermarks.is_empty();
        let state_str = if !conflicts.is_empty() {
            "conflicts"
        } else if effective_self == Effective::Paused {
            "paused"
        } else if !state.pending.is_empty()
            || !state.needs.is_empty()
            || !state.parked.is_empty()
            || (!ever_synced && any_other_active)
        {
            "catching_up"
        } else if !any_other_active {
            "waiting"
        } else {
            "in_order"
        };
        let problem = if state.root_unreadable {
            json!("root_unreadable")
        } else if state.disk_full {
            json!("disk_full")
        } else if state.size_guard_tripped() {
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
            // What is still on the way here: pending paths plus what a
            // blocked rename is holding.
            "behind": state.pending.len() + state.parked.len(),
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
    pub fn sign_status(
        &mut self,
        set_id: &str,
        status: MemberStatus,
        now: u64,
    ) -> io::Result<Vec<Effect>> {
        let self_node = self.self_node.clone();
        let mut state = self
            .sets
            .remove(set_id)
            .ok_or_else(|| io::Error::other("unknown set"))?;
        // Only a member pauses, resumes or leaves. An INVITED device has one
        // pair of gestures (accept, decline) and nothing else: pausing what
        // one has not accepted would sign a live status with no root, and
        // resuming it would disarm the consent guard before consent.
        let standing = state.membership.effective(&self_node);
        let allowed = match status {
            MemberStatus::Declined => standing == Effective::Invited,
            MemberStatus::Left | MemberStatus::Paused | MemberStatus::Active => {
                matches!(standing, Effective::Active | Effective::Paused)
            }
        };
        if !allowed {
            self.sets.insert(set_id.to_string(), state);
            return Err(io::Error::other("gesture not available in this state"));
        }
        let mut out = Vec::new();
        if status == MemberStatus::Active {
            // Resuming is the user gesture that lifts both guards: the
            // consent claim and the disk-full stop.
            state.invite_claim = None;
            state.disk_full = false;
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
            // Nothing we published may outlive the gesture: a transaction
            // already handed out would keep serving the bytes of a folder
            // this device has left.
            for tx_id in std::mem::take(&mut state.published).into_values() {
                out.push(Effect::Revoke { tx_id });
            }
            state.publishing.clear();
            // The set left this device: the local files stay exactly where
            // they are (the letter, and the interface says so), and we stop
            // touching them - no watcher, no walk, no write. The membership
            // state stays, for the stub's proof.
            state.root = None;
            state.pending.clear();
            state.needs.clear();
            state.parked.clear();
        }
        state.announce_to_members(&self_node);
        state.persist(&self.store, set_id)?;
        self.sets.insert(set_id.to_string(), state);
        Ok(out)
    }

    /// `sync.resolve { set_id, path, keep }`: `keep` is a version_id (that
    /// content takes the plain path, the other copy is deleted) or "all"
    /// (both files stay). The record flips to resolved either way and the
    /// gesture's effects propagate as ordinary synced operations - nothing
    /// else ever deletes.
    pub fn resolve(&mut self, set_id: &str, path: &str, keep: &str) -> Result<(), &'static str> {
        let self_node = self.self_node.clone();
        let state = self.sets.get_mut(set_id).ok_or("SYNC_UNKNOWN_SET")?;
        let record = state
            .conflicts
            .values()
            .find(|r| !r.resolved && r.path == path)
            .cloned()
            .ok_or("SYNC_NO_CONFLICT")?;
        let key = state.clock.self_component(&self_node);
        if keep != "all" {
            if state.root.is_none() {
                return Err("SYNC_INTERNAL");
            }
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
                    if let Some(fs) = state.fs_path(&copy.path) {
                        let _ = std::fs::remove_file(fs);
                    }
                    tombstone(state, copy, &key);
                }
            } else if copy.as_ref().is_none_or(|c| c.deleted) {
                // The losing copy has not landed here yet: resolving now
                // would mark the conflict resolved fleet-wide while this
                // device still holds one of the two versions.
                return Err("SYNC_NOT_READY");
            } else if copy_vid.as_deref() == Some(keep) {
                // The copy's content takes the plain path.
                let (Some(plain), Some(copy)) = (&plain, &copy) else {
                    return Err("SYNC_NO_VERSION");
                };
                let (Some(plain_fs), Some(copy_fs)) =
                    (state.fs_path(path), state.fs_path(&copy.path))
                else {
                    return Err("SYNC_INTERNAL");
                };
                let _ = std::fs::remove_file(&plain_fs);
                if std::fs::rename(&copy_fs, &plain_fs).is_err() {
                    return Err("SYNC_INTERNAL");
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
                return Err("SYNC_NO_VERSION");
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
        state
            .persist(&self.store, set_id)
            .map_err(|_| "SYNC_INTERNAL")?;
        Ok(())
    }

    /// Whether `node` is already part of `set_id` in any live or pending
    /// sense: inviting it again is the caller's mistake, not a gesture.
    pub fn already_in(&self, set_id: &str, node: &str) -> bool {
        self.sets.get(set_id).is_some_and(|state| {
            matches!(
                state.membership.effective(node),
                Effective::Active | Effective::Paused | Effective::Invited
            )
        })
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

    /// What a walk needs, as a snapshot: the expensive half runs off the
    /// event loop (it reads and hashes files), so it must not borrow the
    /// engine. `None` = nothing to walk (no root yet).
    pub fn scan_inputs(
        &self,
        set_id: &str,
    ) -> Option<(PathBuf, SetKind, SetIndex, BTreeSet<String>)> {
        let state = self.sets.get(set_id)?;
        let root = state.root.clone()?;
        Some((
            root,
            state.membership.descriptor.kind,
            state.index.clone(),
            state.pending.keys().cloned().collect(),
        ))
    }

    /// Folds a walk's observation into the set: clock values, tombstones,
    /// the drain, the ignored list, one persist. Cheap - no file is read.
    pub fn fold_scan(
        &mut self,
        set_id: &str,
        observation: crate::scan::Observation,
    ) -> io::Result<ScanReport> {
        let self_node = self.self_node.clone();
        let state = self
            .sets
            .get_mut(set_id)
            .ok_or_else(|| io::Error::other("unknown set"))?;
        let key = state.clock.self_component(&self_node);
        // A walk is the slow beat: it re-arms the parked retries, so a
        // destination unlocked in the meantime is tried again.
        for parked in state.parked.values_mut() {
            parked.attempts = 0;
        }
        let report = crate::scan::fold(observation, &mut state.index, &mut state.clock, &key)?;
        let _ = state.drain_pending(&key);
        state.ignored = report.ignored.clone();
        state.persist(&self.store, set_id)?;
        Ok(report)
    }

    /// Rescans inline: the two halves back to back. For callers that may
    /// block (the tests, and a set whose tree is known small).
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
                if let Some(state) = self.sets.get_mut(&set_id)
                    && state.open_invites.remove(from).is_some()
                    && let Err(e) = state.persist(&self.store, &set_id)
                {
                    // The retry was our durable promise: if the write
                    // failed, keep retrying rather than forget silently.
                    eprintln!("[1device-sync] cannot persist the invite ack: {e}");
                    state.open_invites.insert(from.to_string(), RETRY_BUDGET);
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
            } => self.on_need(from, &set_id, need_id, paths, now),
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
            Message::Cannot { set_id, need_id } => {
                self.on_cannot(from, &set_id, need_id, now);
                Vec::new()
            }
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
        now: u64,
    ) -> Vec<Effect> {
        let self_node = self.self_node.clone();
        let Some(state) = self.sets.get_mut(set_id) else {
            return Vec::new();
        };
        if state.membership.effective(from) != Effective::Active {
            return Vec::new();
        }
        // And OUR own standing: a device that paused or left keeps its files
        // but stops serving them.
        if state.membership.effective(&self_node) != Effective::Active {
            return Vec::new();
        }
        if state.root.is_none() {
            return Vec::new();
        }
        // Rate limit per peer: a publish walks a tree in the Core, so
        // serving must not be free to a caller.
        match state.last_served.get(from) {
            Some(at) if now.saturating_sub(*at) < SERVE_INTERVAL_SECS => return Vec::new(),
            _ => {
                state.last_served.insert(from.to_string(), now);
            }
        }
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
                // One path we cannot vouch for drops the whole need, and the
                // needer is TOLD: the ordinary case is a relay that carried
                // the entry without the bytes, and silence would make the
                // needer wait out its TTL and then ask us again for ever.
                eprintln!("[1device-sync] need for a path outside the live index, set {set_id}");
                return vec![Effect::send(
                    from,
                    &Message::Cannot {
                        set_id: set_id.to_string(),
                        need_id,
                    },
                )];
            }
            let Some(fs_path) = state.fs_path(path) else {
                eprintln!("[1device-sync] need for a path we may not read, set {set_id}");
                return vec![Effect::send(
                    from,
                    &Message::Cannot {
                        set_id: set_id.to_string(),
                        need_id,
                    },
                )];
            };
            fs_paths.push(fs_path);
            map.insert(crate::protocol::basename(path).to_string(), path.clone());
        }
        if map.is_empty() {
            return Vec::new();
        }
        // The per-peer slots: a peer beyond its budget supersedes its own
        // oldest published transaction, never another member's. The count
        // includes the publishes IN FLIGHT - otherwise a burst of needs
        // would open as many as it liked before any of them completed.
        let mut out = Vec::new();
        let in_flight = state
            .publishing
            .keys()
            .filter(|(peer, _)| peer == from)
            .count();
        let mut mine: Vec<(String, u64)> = state
            .published
            .keys()
            .filter(|(peer, _)| peer == from)
            .cloned()
            .collect();
        if mine.len() + in_flight >= PUBLISHED_PER_PEER {
            if mine.is_empty() {
                // Nothing of ours to retire: the peer is asking faster than
                // we can publish. Its next round asks again.
                return out;
            }
            mine.sort_by_key(|(_, id)| *id);
            let oldest = mine.remove(0);
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

    pub fn on_publish_failed(&mut self, to: &str, set_id: &str, need_id: u64) -> Vec<Effect> {
        if let Some(state) = self.sets.get_mut(set_id) {
            state.publishing.remove(&(to.to_string(), need_id));
        }
        // The needer is told, rather than left waiting: a file deleted
        // between our last walk and its need is the ordinary cause.
        vec![Effect::send(
            to,
            &Message::Cannot {
                set_id: set_id.to_string(),
                need_id,
            },
        )]
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
    pub fn on_pull_started(
        &mut self,
        set_id: &str,
        need_id: u64,
        transfer_id: &str,
        now: u64,
    ) -> Vec<Effect> {
        let mut early = None;
        if let Some(state) = self.sets.get_mut(set_id)
            && let Some(need) = state.needs.get_mut(&need_id)
        {
            // Progress: the reclaim clock restarts, so a long transfer is
            // never taken away from itself.
            need.born_at = now;
            if let Some(pull) = &mut need.pull {
                pull.transfer = Some(transfer_id.to_string());
            }
            early = state.early_outcomes.remove(transfer_id);
        }
        // The outcome that arrived before this id was known: applied now.
        match early {
            Some((ok, error)) => self.finish_pull(set_id, need_id, ok, &error),
            None => Vec::new(),
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
    pub fn on_transfer_outcome(&mut self, transfer_id: &str, ok: bool, error: &str) -> Vec<Effect> {
        let set_ids = self.set_ids();
        for set_id in set_ids {
            let state = self.sets.get_mut(&set_id).expect("listed");
            let found = state
                .needs
                .iter()
                .find(|(_, n)| {
                    n.pull
                        .as_ref()
                        .is_some_and(|p| p.transfer.as_deref() == Some(transfer_id))
                })
                .map(|(id, _)| *id);
            match found {
                Some(need_id) => return self.finish_pull(&set_id, need_id, ok, error),
                None => continue,
            }
        }
        // The outcome beat the message that names the transfer (both ride the
        // loop's own channels): remembered briefly, applied the moment the id
        // is registered. Without this the need would wait out its whole TTL.
        for state in self.sets.values_mut() {
            if state.needs.values().any(|n| n.pull.is_some()) {
                while state.early_outcomes.len() >= EARLY_OUTCOMES_MAX {
                    let oldest = state
                        .early_outcomes
                        .keys()
                        .next()
                        .cloned()
                        .expect("non-empty");
                    state.early_outcomes.remove(&oldest);
                }
                state
                    .early_outcomes
                    .insert(transfer_id.to_string(), (ok, error.to_string()));
            }
        }
        Vec::new()
    }

    /// Applies one finished (or failed) pull: verify what landed, apply or
    /// keep-both, salvage the rest.
    fn finish_pull(&mut self, set_id: &str, need_id: u64, ok: bool, error: &str) -> Vec<Effect> {
        let self_node = self.self_node.clone();
        let mut out = Vec::new();
        {
            let Some(state) = self.sets.get_mut(set_id) else {
                return out;
            };
            let Some(need) = state.needs.remove(&need_id) else {
                return out;
            };
            let Some(pull) = need.pull else {
                return out;
            };
            let key = state.clock.self_component(&self_node);
            // The letter's disk-full rule, from the failure rather than a
            // prediction: no portable way to ask a filesystem how much room
            // is left without a new dependency, and a retry loop against a
            // full disk is exactly what the rule exists to prevent. The set
            // stops pulling and says why; resuming is a user gesture.
            if !ok && out_of_space(error) {
                state.disk_full = true;
            }
            let mut landed = 0usize;
            for (wire, set_path) in &pull.files {
                let staged = pull.staging.join(wire);
                // A path whose bytes keep failing is isolated into a need of
                // its own with backoff, so one hot file (a log, a database)
                // never starves the rest of a batch.
                if !staged.exists() {
                    let strikes = state.hot.entry(set_path.clone()).or_insert(0);
                    *strikes = strikes.saturating_add(1);
                    continue;
                }
                state.hot.remove(set_path);
                match state.apply_staged(&staged, set_path, &key) {
                    Apply::Landed => landed += 1,
                    Apply::Failed => {
                        let strikes = state.hot.entry(set_path.clone()).or_insert(0);
                        *strikes = strikes.saturating_add(1);
                    }
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
                        } else {
                            let strikes = state.hot.entry(set_path.clone()).or_insert(0);
                            *strikes = strikes.saturating_add(1);
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
            let _ = state.persist(&self.store, set_id);
            if landed == pull.files.len() {
                out.push(Effect::send(
                    &need.peer,
                    &Message::Done {
                        set_id: set_id.to_string(),
                        need_id,
                        tx_id: pull.tx_id.clone(),
                    },
                ));
            }
        }
        out
    }

    /// The needer's side of a refusal: release the need at once and remember
    /// that this peer cannot serve those paths, so the next pump asks
    /// somebody else instead of the same one for ever.
    fn on_cannot(&mut self, from: &str, set_id: &str, need_id: u64, now: u64) {
        let Some(state) = self.sets.get_mut(set_id) else {
            return;
        };
        let Some(need) = state.needs.remove(&need_id) else {
            return;
        };
        if need.peer != from {
            // Not the peer we asked: put it back, ignore the claim.
            state.needs.insert(need_id, need);
            return;
        }
        for path in need.paths {
            while state.refused.len() >= REFUSALS_MAX {
                let oldest = state.refused.keys().next().cloned().expect("non-empty");
                state.refused.remove(&oldest);
            }
            state.refused.insert((from.to_string(), path), now);
        }
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
            if self.sets.len() >= SETS_MAX {
                eprintln!("[1device-sync] too many sets: invitation dropped");
                return Vec::new();
            }
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
            // The descriptor is the creator's signed word: check it against
            // whatever binding the carried records established for the
            // creator. Absent a binding, an introduction will confirm or
            // refute it later; a binding that REFUTES it stops here.
            let creator = membership.descriptor.created_by.clone();
            if let Some(key) = membership.key_for(&creator).map(str::to_string)
                && !membership.descriptor.verify(&key)
            {
                eprintln!("[1device-sync] invitation carries an unsigned descriptor: dropped");
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
        state.prune_buffers(now);
        if !state.admit_buffer(from, round, now) {
            eprintln!("[1device-sync] too many rounds in flight from one peer: head dropped");
            return Vec::new();
        }
        let buffer = state
            .inflight
            .get_mut(&(from.to_string(), round))
            .expect("admitted");
        buffer.head = Some((position, answers));

        let mut out = self.try_complete(from, set_id, round, now);

        // An opener, or an answer to OUR opener, gets our answering leg; an
        // answer to our own ANSWER closes the exchange (termination).
        let answer_due = match answers {
            None => true,
            Some(theirs) => matches!(self.sent_rounds.get(&theirs), Some(false)),
        };
        if answer_due {
            // The records ALWAYS travel (the answer to any head carries what
            // we hold about its sender: that is what proves a leaver's
            // terminal record and keeps a paused member visible). Only the
            // entries DELTA is throttled, because only the delta is
            // expensive - and a suppressed delta is declared incomplete, so
            // it advances no watermark.
            let with_delta = self.answer_due_now(set_id, from, now);
            out.extend(self.answer_head(from, set_id, round, &their_vv, with_delta));
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
        with_delta: bool,
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
        let record_pages = paginate_gossip(&records, &endorsements, &conflict_values);

        let serving = with_delta
            && state.root.is_some()
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
        for (page, (records, endorsements, conflicts)) in record_pages.into_iter().enumerate() {
            out.push(Outgoing {
                to: from.to_string(),
                payload: json!({
                    "dialect": DIALECT,
                    "type": "records",
                    "set_id": set_id,
                    "round": round,
                    "page": page as u64,
                    "records": records,
                    "endorsements": endorsements,
                    "conflicts": conflicts,
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
        state.prune_buffers(now);
        if !state.admit_buffer(from, round, now) {
            return;
        }
        let buffer = state
            .inflight
            .get_mut(&(from.to_string(), round))
            .expect("admitted");
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
        // Prune FIRST, then take: a buffer the TTL retires between the check
        // and the take would otherwise be a peer-triggerable panic.
        state.prune_buffers(now);
        let complete = state.inflight.get(&key).is_some_and(|b| {
            b.head.as_ref().is_some_and(|(p, _)| {
                // Exactly the declared pages, numbered 0..n: a peer that
                // numbered them otherwise did not send the batch its head
                // promised, and the next round resends.
                (0..p.records_pages).all(|i| b.records.contains_key(&i))
                    && (0..p.entries_pages).all(|i| b.entries.contains_key(&i))
            })
        });
        if !complete {
            return Vec::new();
        }
        let Some(buffer) = state.inflight.remove(&key) else {
            return Vec::new();
        };
        let Some((position, _)) = buffer.head else {
            return Vec::new();
        };

        // The stub's proof is our OWN terminal seq echoed back (or a record
        // superseding it): a stale terminal from an earlier era proves
        // nothing about the news we are pushing now.
        let our_terminal_seq = state.membership.terminal_seq_of(&self_node);
        let mut echoed_terminal_about_self = false;
        for (records, endorsements, conflicts) in buffer.records.values() {
            for record in records {
                if let Record::Member(m) = record
                    && m.node_id == self_node
                    && m.status.is_terminal()
                    && m.seq >= our_terminal_seq
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
        let mut all_absorbed = true;
        if sender_active {
            for entries in buffer.entries.values() {
                for entry in entries {
                    if !state.absorb_entry(entry) {
                        all_absorbed = false;
                    }
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

        // A complete leg landed: this is the "offline since" a card means -
        // our own fact, not the directory's.
        state.last_round.insert(from.to_string(), now);

        // What can settle without bytes settles now.
        let key = state.clock.self_component(&self_node);
        let _ = state.drain_pending(&key);

        // The watermark and the state it covers ride ONE atomic meta.json
        // write - and the advertisement follows the DISK, not the memory: a
        // failed persist rolls the in-memory watermark back, so nothing is
        // ever advertised that a crash could lose.
        if position.entries_complete && sender_active && all_absorbed {
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

    /// Whether we owe `peer` a full answer now: an inbound head costs a
    /// delta computation, so a flood of tiny heads must not reflect our
    /// whole index back at line rate. The peer loses nothing - its next
    /// round (or our next pump) carries what this one skipped.
    fn answer_due_now(&mut self, set_id: &str, peer: &str, now: u64) -> bool {
        let Some(state) = self.sets.get_mut(set_id) else {
            return false;
        };
        match state.last_answered.get(peer) {
            Some(at) if now.saturating_sub(*at) < ANSWER_INTERVAL_SECS => false,
            _ => {
                state.last_answered.insert(peer.to_string(), now);
                true
            }
        }
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
        let set_ids = self.set_ids();
        for set_id in set_ids {
            // Invitations first: they are the door for everything else. The
            // letter's stop condition, whole: an ack OR any invitee-signed
            // record for the set - a device that accepted (or declined)
            // through some other path needs no more invitations. And the
            // budget re-arms when a target becomes reachable again: a new
            // route is new information, not another wasted attempt.
            let invites: Vec<String> = {
                let state = self.sets.get_mut(&set_id).expect("listed");
                let heard: Vec<String> = state
                    .open_invites
                    .keys()
                    .filter(|n| {
                        !matches!(
                            state.membership.effective(n),
                            Effective::Invited | Effective::Unknown
                        )
                    })
                    .cloned()
                    .collect();
                for node in heard {
                    state.open_invites.remove(&node);
                }
                for node in reachable {
                    if let Some(budget) = state.open_invites.get_mut(node)
                        && *budget == 0
                    {
                        *budget = RETRY_BUDGET;
                    }
                }
                state
                    .open_invites
                    .iter()
                    .filter(|(n, budget)| reachable.contains(n) && **budget > 0)
                    .map(|(n, _)| n.clone())
                    .collect()
            };
            for invitee in invites {
                if let Some(message) = self.build_invite(&set_id, &invitee) {
                    out.push(Effect::Send(Outgoing {
                        to: invitee.clone(),
                        payload: message,
                    }));
                    let state = self.sets.get_mut(&set_id).expect("listed");
                    // The budget bounds one reachable stretch, never the
                    // invitation: it re-arms when the target reappears.
                    if let Some(budget) = state.open_invites.get_mut(&invitee) {
                        *budget = budget.saturating_sub(1);
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
                out.extend(self.opener(&set_id, &target));
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
                    out.extend(self.opener(&set_id, &peer));
                }
            }

            // Needs: pending FILE bytes pulled from peers whose absorbed
            // watermark says they hold them.
            let need_effects = self.pump_needs(&set_id, reachable, now);
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
                out.extend(self.opener(&set_id, &target));
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
                out.extend(self.opener(&set_id, &target));
            }
        }
        out
    }

    /// Builds the needs one pump owes: pending FILE versions grouped per
    /// serving peer, chunked so no two paths share a basename (the
    /// published record names files by basename: the bijection), within
    /// the per-peer slots and the chunk bounds.
    fn pump_needs(&mut self, set_id: &str, reachable: &[String], now: u64) -> Vec<Effect> {
        let self_node = self.self_node.clone();
        let mut out = Vec::new();
        let Some(state) = self.sets.get_mut(set_id) else {
            return out;
        };
        // Reclaim what never came back: a need whose offer never arrived, or
        // whose pull never reported, releases its paths so the next pump can
        // ask again (of another member, if this one has gone quiet).
        state
            .needs
            .retain(|_, need| now.saturating_sub(need.born_at) <= NEED_TTL_SECS);
        if state.root.is_none()
            || state.membership.effective(&self_node) != Effective::Active
            || state.size_guard_tripped()
            || state.disk_full
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
            // A struck path goes alone, and only once the rest of the batch
            // has had its turn: the backoff is "after the others".
            let hot: BTreeSet<String> = state
                .hot
                .iter()
                .filter(|(_, strikes)| **strikes > 1)
                .map(|(path, _)| path.clone())
                .collect();
            let others_pending = state
                .pending
                .keys()
                .any(|p| !hot.contains(p) && !claimed.contains(p));
            for (path, versions) in &state.pending {
                if claimed.contains(path) {
                    continue;
                }
                // This peer already said it cannot serve this path: ask
                // someone else, and only try it again much later (its own
                // pull may have landed by then).
                if state
                    .refused
                    .get(&(peer.clone(), path.clone()))
                    .is_some_and(|at| now.saturating_sub(*at) < REFUSAL_TTL_SECS)
                {
                    continue;
                }
                // A struck path waits for the rest of the batch, then goes
                // alone: nothing else can starve behind it.
                if hot.contains(path) && (others_pending || !chunk.is_empty()) {
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
                let alone = hot.contains(path);
                chunk.push(path.clone());
                bytes = bytes.saturating_add(entry.size);
                if alone || chunk.len() >= NEED_CHUNK_FILES || bytes >= NEED_CHUNK_BYTES {
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
                    born_at: now,
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

    /// A round opener toward `peer`: the declaring head plus our records
    /// pages. A "records-only head" that carried no records proved nothing:
    /// the stub's terminal news, a pause, a resume all ride these pages, so
    /// one exchange suffices for the answer to echo them back.
    fn opener(&mut self, set_id: &str, peer: &str) -> Vec<Effect> {
        let self_node = self.self_node.clone();
        let sync_pub = self.store.identity().public_hex();
        let round = self.next_round(false);
        let Some(state) = self.sets.get(set_id) else {
            return Vec::new();
        };
        let records: Vec<Value> = state.membership.all_records();
        let endorsements: Vec<Value> = state
            .membership
            .endorsements()
            .into_iter()
            .map(Endorsement::to_value)
            .collect();
        let conflicts: Vec<Value> = state
            .conflicts
            .values()
            .map(ConflictRecord::to_value)
            .collect();
        let pages = paginate_gossip(&records, &endorsements, &conflicts);
        let head = Message::Head {
            set_id: set_id.to_string(),
            round,
            answers: None,
            position: Some(HeadPosition {
                descriptor_hash: descriptor_hash(&state.membership.descriptor),
                set_vv: state.advertised(&state.clock.self_component(&self_node)),
                records_pages: pages.len() as u64,
                entries_pages: 0,
                // An opener computes no delta: only the answering leg can
                // advance a watermark.
                entries_complete: false,
            }),
            sync_pub,
        };
        let mut out = vec![Effect::send(peer, &head)];
        for (page, (records, endorsements, conflicts)) in pages.into_iter().enumerate() {
            out.push(Effect::Send(Outgoing {
                to: peer.to_string(),
                payload: json!({
                    "dialect": DIALECT,
                    "type": "records",
                    "set_id": set_id,
                    "round": round,
                    "page": page as u64,
                    "records": records,
                    "endorsements": endorsements,
                    "conflicts": conflicts,
                }),
            }));
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
            last_answered: BTreeMap::new(),
            last_served: BTreeMap::new(),
            last_round: BTreeMap::new(),
            refused: BTreeMap::new(),
            needs: BTreeMap::new(),
            need_counter: 0,
            published: BTreeMap::new(),
            publishing: BTreeMap::new(),
            parked: BTreeMap::new(),
            conflicts: BTreeMap::new(),
            landed_bytes: 0,
            ignored: Vec::new(),
            watch_degraded: false,
            root_unreadable: false,
            disk_full: false,
            early_outcomes: BTreeMap::new(),
            hot: BTreeMap::new(),
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
    /// versions already parked for its path, bounded per path and per set.
    /// Beyond the bounds the entry is REFUSED rather than displacing a
    /// version already parked - a watermark covers what we absorbed, so a
    /// dropped version would never be offered again.
    fn absorb_entry(&mut self, entry: &Entry) -> bool {
        if let Some(ours) = self.index.get(&entry.path)
            && ours.vv.covers(&entry.vv)
        {
            return true;
        }
        if !self.pending.contains_key(&entry.path) && self.pending.len() >= PENDING_PATHS_MAX {
            eprintln!("[1device-sync] pending set full: entry refused, the round stays open");
            return false;
        }
        let parked = self.pending.entry(entry.path.clone()).or_default();
        parked.retain(|held| !entry.vv.covers(&held.vv));
        if parked.iter().any(|held| held.vv.covers(&entry.vv)) {
            return true;
        }
        if parked.len() >= PENDING_PER_PATH {
            eprintln!("[1device-sync] too many concurrent versions parked: entry refused");
            return false;
        }
        parked.push(entry.clone());
        true
    }

    /// Where a wire path lives on this disk. Two rules in one place:
    ///
    /// - a set of ONE FILE has the file itself as its root, so its single
    ///   wire name resolves to the root rather than under it;
    /// - no component of the resolved path may be a SYMLINK. Symlinks are
    ///   never indexed (section 8), so one standing where a directory of the
    ///   set should be can only be a local trap - and following it would
    ///   write or unlink outside the root, which the letter forbids
    ///   absolutely.
    ///
    /// `None` means "not somewhere we may touch": the caller refuses.
    fn fs_path(&self, wire: &str) -> Option<PathBuf> {
        let root = self.root.as_ref()?;
        if self.membership.descriptor.kind == SetKind::File {
            // The one name of a single-file set: the root IS the file.
            return (crate::protocol::basename(wire) == wire)
                .then(|| crate::wirepath::long_path(root));
        }
        let mut path = root.clone();
        let mut components = wire.split('/').peekable();
        while let Some(component) = components.next() {
            path.push(component);
            // The last component may be anything (we are about to create or
            // replace it); every ANCESTOR must be a real directory.
            if components.peek().is_some() {
                match std::fs::symlink_metadata(&path) {
                    Ok(meta) if meta.is_dir() => {}
                    // Absent is fine: we create it ourselves.
                    Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                    _ => {
                        eprintln!(
                            "[1device-sync] refusing to work through a non-directory: {wire}"
                        );
                        return None;
                    }
                }
            }
        }
        // Windows: the extended-length form, so a deep tree under a long
        // root cannot wedge on MAX_PATH. Identity everywhere else.
        Some(crate::wirepath::long_path(&path))
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

        if self.root.is_none() {
            return Apply::Failed;
        }
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
        // The "catching up" window, honestly: the disk may have moved under
        // the pull. Replacing what the index does not recognise would destroy
        // a local edit silently, so an unrecognised occupant is treated as
        // what it is - a concurrent change - and takes the conflict path once
        // the next walk has given it a version of its own.
        if let Some(ours) = self.index.get(set_path).filter(|e| !e.deleted) {
            let disk = self
                .fs_path(set_path)
                .and_then(|p| std::fs::symlink_metadata(&p).ok().map(|m| (p, m)));
            if let Some((path, meta)) = disk {
                let moved = meta.len() != ours.size
                    || meta
                        .modified()
                        .map(crate::index::Mtime::of)
                        .is_ok_and(|m| m != ours.mtime);
                if moved
                    && crate::scan::hash_file(&path)
                        .map(|h| h != ours.hash)
                        .unwrap_or(true)
                {
                    eprintln!(
                        "[1device-sync] {set_path} changed locally under the pull: kept, not replaced"
                    );
                    return Apply::Failed;
                }
            }
        }
        // Filesystem first, index after (load-bearing, the letter).
        let Some(dest) = self.fs_path(set_path) else {
            return Apply::Failed;
        };
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
        clear_readonly(&dest);
        // A mount point inside the root makes the staging rename cross a
        // filesystem: copy beside the target and rename from there, which is
        // still atomic where it matters (the final replace).
        let staged = match std::fs::rename(staged, &dest) {
            Ok(()) => {
                return self.record_landed(
                    set_path,
                    &dest,
                    &entry,
                    hash,
                    resurrects,
                    self_component,
                );
            }
            Err(e) if is_cross_device(&e) => {
                let beside =
                    dest.with_file_name(format!(".{}.1dtmp", crate::protocol::basename(set_path)));
                if std::fs::copy(staged, &beside).is_err() {
                    let _ = std::fs::remove_file(&beside);
                    return Apply::Failed;
                }
                let _ = std::fs::remove_file(staged);
                beside
            }
            Err(_) => staged.to_path_buf(),
        };
        if let Err(e) = std::fs::rename(&staged, &dest) {
            // A destination held open by an application (the Office case):
            // keep the VERIFIED staged copy and retry the rename alone,
            // never the transfer. The staged file moves to a parked home
            // that survives the pull's staging-dir cleanup.
            let Some(root) = self.root.clone() else {
                return Apply::Failed;
            };
            let parked_dir = root.join(crate::wirepath::STAGING_DIR).join("parked");
            if std::fs::create_dir_all(&parked_dir).is_ok() {
                let home = parked_dir.join(format!(
                    "{}-{}",
                    &entry.hash[..16.min(entry.hash.len())],
                    crate::protocol::basename(set_path)
                ));
                if std::fs::rename(&staged, &home).is_ok() {
                    self.parked.insert(
                        set_path.to_string(),
                        ParkedEntry {
                            staged: home,
                            entry: entry.clone(),
                            reason: format!("blocked: {e}"),
                            attempts: 0,
                        },
                    );
                }
            }
            return Apply::Failed;
        }
        self.record_landed(set_path, &dest, &entry, hash, resurrects, self_component)
    }

    /// The index half of a landed file: permissions, the mtime read BACK from
    /// the disk, the consent-guard accounting, the vv.
    fn record_landed(
        &mut self,
        set_path: &str,
        dest: &Path,
        entry: &Entry,
        hash: String,
        resurrects: bool,
        self_component: &str,
    ) -> Apply {
        #[cfg(unix)]
        if entry.exec {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(dest, std::fs::Permissions::from_mode(0o755));
        }
        // The source's mtime, then the filesystem's own rounding read BACK:
        // the index records what the disk will report, not our wish.
        let mtime = (|| {
            let file = std::fs::File::options().write(true).open(dest).ok()?;
            let secs = u64::try_from(entry.mtime.secs).ok()?;
            let stamp = std::time::SystemTime::UNIX_EPOCH
                + std::time::Duration::new(secs, entry.mtime.nanos);
            file.set_times(std::fs::FileTimes::new().set_modified(stamp))
                .ok()?;
            Some(crate::index::Mtime::of(
                std::fs::metadata(dest).ok()?.modified().ok()?,
            ))
        })()
        .unwrap_or(entry.mtime);
        let size = std::fs::metadata(dest)
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
        if self.root.is_none() {
            return;
        }
        let fold = tomb.path.to_lowercase();
        let folded_live = self
            .index
            .live()
            .any(|e| e.path != tomb.path && e.path.to_lowercase() == fold);
        let Some(dest) = self.fs_path(&tomb.path) else {
            return;
        };
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

        if self.root.is_none() {
            return;
        }
        let prefix = format!("{}/", tomb.path);
        let mut live_descendants: Vec<String> = self
            .index
            .live()
            .filter(|e| e.path.starts_with(&prefix))
            .map(|e| e.path.clone())
            .collect();
        // A descendant whose bytes are still on their way counts as live:
        // the tombstone must not win a race against the child that will
        // recreate this very directory.
        let pending_descendant = self.pending.iter().any(|(path, versions)| {
            path.starts_with(&prefix) && versions.iter().any(|e| !e.deleted)
        });
        let dominates_dir = match self.index.get(&tomb.path) {
            None => true,
            Some(ours) => matches!(
                tomb.vv.relation(&ours.vv),
                Relation::Descends | Relation::Equal
            ),
        };
        if !live_descendants.is_empty() || pending_descendant || !dominates_dir {
            // The edit wins, up the tree: the directory and every live
            // descendant dominate the tombstone from now on.
            live_descendants.push(tomb.path.clone());
            let survivors = live_descendants;
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
        let Some(dest) = self.fs_path(&tomb.path) else {
            return;
        };
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
        // Signed by a device the set ADMITS: a conflict record moves files
        // around (the fold) and shows up in every interface, so a mere
        // account device's word is not enough.
        if !matches!(
            self.membership.effective(&record.signed_by),
            Effective::Active | Effective::Paused
        ) {
            return;
        }
        let Some(signer_key) = self.membership.key_for(&record.signed_by) else {
            return;
        };
        if !record.verify(signer_key) {
            return;
        }
        let key = (record.path.clone(), record.conflict_id.clone());
        if !self.conflicts.contains_key(&key) && self.conflicts.len() >= CONFLICTS_MAX {
            eprintln!("[1device-sync] too many conflict records: one dropped");
            return;
        }
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
        use crate::vv::Relation;

        let paths: Vec<String> = self.parked.keys().cloned().collect();
        for path in paths {
            let Some(parked) = self.parked.get(&path).cloned() else {
                continue;
            };
            // Superseded: the index already holds this version or better, so
            // the staged bytes have no work left. Release them.
            if self.index.get(&path).is_some_and(|ours| {
                matches!(
                    ours.vv.relation(&parked.entry.vv),
                    Relation::Descends | Relation::Equal
                )
            }) {
                let _ = std::fs::remove_file(&parked.staged);
                self.parked.remove(&path);
                continue;
            }
            if !parked.staged.exists() {
                // The bytes are gone (a sweep, a human): the path goes back
                // to the pending set so the next pump needs it again, rather
                // than being forgotten.
                self.pending
                    .entry(path.clone())
                    .or_default()
                    .push(parked.entry.clone());
                self.parked.remove(&path);
                continue;
            }
            // Back through the ordinary gate: pending must still name it.
            self.pending
                .entry(path.clone())
                .or_default()
                .push(parked.entry.clone());
            // Past the cap, a locked destination is only retried when a walk
            // comes round (the safety tick), not on every drain.
            if parked.attempts >= PARKED_ATTEMPTS_MAX {
                self.settle_pending(&path, &parked.entry.vv);
                continue;
            }
            if let Some(held) = self.parked.get_mut(&path) {
                held.attempts = held.attempts.saturating_add(1);
            }
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

        if self.root.is_none() {
            return;
        }
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
            let Some(dest) = self.fs_path(&path) else {
                continue;
            };
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

    /// Makes room for a round buffer, bounded per peer: a compromised
    /// sibling must not be able to park arbitrarily many. Beyond the cap
    /// its OWN oldest round is retired (never another member's), and
    /// `false` means even that failed.
    fn admit_buffer(&mut self, from: &str, round: u64, now: u64) -> bool {
        let key = (from.to_string(), round);
        if self.inflight.contains_key(&key) {
            return true;
        }
        let mine: Vec<(String, u64)> = self
            .inflight
            .keys()
            .filter(|(peer, _)| peer == from)
            .cloned()
            .collect();
        if mine.len() >= ROUNDS_IN_FLIGHT_PER_PEER {
            let oldest = mine
                .into_iter()
                .min_by_key(|k| self.inflight.get(k).map(|b| b.born_at).unwrap_or(0))
                .expect("non-empty");
            self.inflight.remove(&oldest);
        }
        self.inflight.insert(
            key,
            RoundBuffer {
                head: None,
                records: BTreeMap::new(),
                entries: BTreeMap::new(),
                born_at: now,
            },
        );
        true
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
            "disk_full": self.disk_full,
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
                                "attempts": p.attempts,
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
            last_answered: BTreeMap::new(),
            last_served: BTreeMap::new(),
            last_round: BTreeMap::new(),
            refused: BTreeMap::new(),
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
            root_unreadable: false,
            disk_full: meta
                .get("disk_full")
                .and_then(Value::as_bool)
                .ok_or_else(corrupt)?,
            early_outcomes: BTreeMap::new(),
            hot: BTreeMap::new(),
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
                            attempts: p
                                .get("attempts")
                                .and_then(Value::as_u64)
                                .unwrap_or_default() as u32,
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

/// One page of gossip: membership records, endorsements, conflict records.
type GossipPage = (Vec<Value>, Vec<Value>, Vec<Value>);

/// Pages the three gossip lists TOGETHER under the byte budget. Putting
/// endorsements and conflict records on page 0 would have let a set with
/// enough of them exceed the transport's hard 64 KiB cap, and a frame that
/// cannot be sent stalls the exchange silently.
fn paginate_gossip(
    records: &[Value],
    endorsements: &[Value],
    conflicts: &[Value],
) -> Vec<GossipPage> {
    // Tagged so one budget covers all three, then split back per page.
    let tagged: Vec<Value> = records
        .iter()
        .map(|v| json!({ "k": "r", "v": v }))
        .chain(endorsements.iter().map(|v| json!({ "k": "e", "v": v })))
        .chain(conflicts.iter().map(|v| json!({ "k": "c", "v": v })))
        .collect();
    let mut pages: Vec<GossipPage> = paginate(&tagged)
        .unwrap_or_default()
        .into_iter()
        .map(|chunk| {
            let mut page: GossipPage = (Vec::new(), Vec::new(), Vec::new());
            for item in chunk {
                let value = item["v"].clone();
                match item["k"].as_str() {
                    Some("r") => page.0.push(value),
                    Some("e") => page.1.push(value),
                    _ => page.2.push(value),
                }
            }
            page
        })
        .collect();
    // An empty page is still a page when there is nothing to say and a head
    // must declare something (never for a set with no gossip at all).
    if pages.is_empty() && !tagged.is_empty() {
        pages.push((Vec::new(), Vec::new(), Vec::new()));
    }
    pages
}

fn wrap(out: Vec<Outgoing>) -> Vec<Effect> {
    out.into_iter().map(Effect::Send).collect()
}

/// A per-boot base for round ids: 40 random bits, leaving room for 2^12
/// rounds per incarnation under the canonical integer bound.
fn boot_round_base() -> u64 {
    let mut bytes = [0u8; 8];
    rand::Rng::fill_bytes(&mut rand::rng(), &mut bytes);
    (u64::from_be_bytes(bytes) % (1 << 40)) << 12
}

/// Whether the Core's words on a failed fill name a full disk. The Core
/// relays the failure's own words, so this reads them rather than guessing:
/// a wrong guess would stop a set for nothing (a resume gesture undoes it).
/// Windows only: clears FILE_ATTRIBUTE_READONLY so a replace can proceed.
/// The clippy lint on `set_readonly(false)` warns about a UNIX hazard (it
/// means world-writable there), which cannot apply in a windows-only
/// function: here it clears exactly that one attribute and nothing else.
#[cfg(windows)]
#[allow(clippy::permissions_set_readonly_false)]
fn clear_readonly(dest: &Path) {
    if let Ok(meta) = std::fs::metadata(dest) {
        let mut perms = meta.permissions();
        if perms.readonly() {
            perms.set_readonly(false);
            let _ = std::fs::set_permissions(dest, perms);
        }
    }
}

/// Whether an I/O error is the cross-device one (`EXDEV`, and Windows's
/// own refusal to rename across volumes). `raw_os_error` rather than a
/// string: the message is localized, the number is not.
fn is_cross_device(e: &io::Error) -> bool {
    #[cfg(unix)]
    let codes = [18];
    #[cfg(windows)]
    // ERROR_NOT_SAME_DEVICE.
    let codes = [17];
    #[cfg(not(any(unix, windows)))]
    let codes: [i32; 0] = [];
    e.raw_os_error().is_some_and(|c| codes.contains(&c))
}

fn out_of_space(error: &str) -> bool {
    let error = error.to_ascii_lowercase();
    error.contains("no space left")
        || error.contains("not enough space")
        || error.contains("disk full")
        || error.contains("insufficient")
        || error.contains("enospc")
}

fn kind_str(kind: SetKind) -> &'static str {
    match kind {
        SetKind::Dir => "dir",
        SetKind::File => "file",
    }
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
    if state.root.is_none() {
        return false;
    }
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

    let author = author_of(&loser.vv).unwrap_or_else(|| self_node.to_string());
    let mut device = names
        .get(&author)
        .map(|n| sanitize_device_name(n))
        .unwrap_or_default();
    if device.is_empty() {
        device = author.chars().take(8).collect();
    }
    let base = crate::protocol::basename(set_path);
    let dir_prefix = &set_path[..set_path.len() - base.len()];
    // The discriminator is the LOSING version's own id, not a local
    // counter: two devices materializing the same pair must land on the
    // same name, and a counter would have read state that differs between
    // them. It only appears when the plain candidate is taken by different
    // content.
    let plain_name = conflict_copy_name(base, &device, loser.mtime, None);
    let plain_candidate = format!("{dir_prefix}{plain_name}");
    let taken = state
        .index
        .get(&plain_candidate)
        .is_some_and(|e| !e.deleted && e.hash != loser.hash);
    let loser_vid = if local_wins { &rvid } else { &lvid };
    let gate = |candidate: String| -> Option<String> {
        crate::wirepath::check_wire_path(&candidate)
            .ok()
            .map(|()| candidate)
    };
    let loser_path = if taken {
        format!(
            "{dir_prefix}{}",
            conflict_copy_name(base, &device, loser.mtime, Some(&loser_vid[..8]))
        )
    } else {
        plain_candidate
    };
    // A path that would not survive the wire gate must never become an index
    // entry: it would drop the page carrying it at every peer, for ever.
    let Some(loser_path) = gate(loser_path) else {
        eprintln!("[1device-sync] cannot name a conflict copy for {set_path}: kept pending");
        return false;
    };
    let occupied_same = state
        .index
        .get(&loser_path)
        .is_some_and(|e| !e.deleted && e.hash == loser.hash);

    let (Some(plain_fs), Some(loser_fs)) = (state.fs_path(set_path), state.fs_path(&loser_path))
    else {
        return false;
    };

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

/// The version's author, for a name and for a card: the component that
/// touched it last, ties broken by key. ONE rule, so the copy's filename
/// and the card's row never disagree - and it reads only wire data, so
/// every device computes the same answer.
fn author_of(vv: &Vv) -> Option<String> {
    vv.components()
        .max_by(|a, b| (a.1, a.0).cmp(&(b.1, b.0)))
        .map(|(key, _)| key.split('@').next().unwrap_or(key).to_string())
}

/// "name (DeviceName, 2026-08-16 14h02 UTC).ext", sanitized against the
/// full gate and deterministically truncated to the component budget. The
/// `discriminator` (the losing version's own id, never a local counter)
/// only appears when the plain name is taken by different content.
fn conflict_copy_name(
    base: &str,
    device: &str,
    mtime: crate::index::Mtime,
    discriminator: Option<&str>,
) -> String {
    let (stem, ext) = match base.rfind('.') {
        Some(i) if i > 0 => base.split_at(i),
        _ => (base, ""),
    };
    let tag = match discriminator {
        None => format!(" ({device}, {})", utc_stamp(mtime.secs)),
        Some(d) => format!(" ({device}, {}) {d}", utc_stamp(mtime.secs)),
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
                        let started = needer.on_pull_started(&set_id, need_id, &transfer, now);
                        queue.extend(started.into_iter().map(|e| (producer.clone(), e)));
                        let more = needer.on_transfer_outcome(&transfer, true, "");
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

    /// The relay's corollary: an entry can reach A through B before its
    /// bytes do, so B must REFUSE the need rather than drop it silently -
    /// and A must then ask the device that actually holds the bytes.
    #[test]
    fn a_refused_need_sends_the_needer_to_another_member() {
        let mut rig = Rig::new(&['a', 'b', 'c']);
        let (set_id, _root) = set_with_files(&mut rig, 'a');
        for letter in ['b', 'c'] {
            rig.of('a')
                .invite(&set_id, &node(letter), NOW)
                .expect("invite");
        }
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

        // C writes and tells B only; B absorbs the entry but not the bytes.
        let c_root = root_of(&mut rig, 'c', &set_id);
        std::fs::write(c_root.join("from-c.txt"), "made by c").expect("write");
        rig.of('c').rescan_set(&set_id).expect("rescan");
        let out = rig.of('c').pump(&[node('b')], NOW + 100, true);
        rig.deliver(&node('c'), out, NOW + 100);
        assert!(
            rig.of('b')
                .set(&set_id)
                .expect("state")
                .pending
                .contains_key("from-c.txt"),
            "B holds the entry, not the bytes"
        );

        // A talks to B ALONE: it learns the entry, needs it, and B refuses.
        for i in 0..3 {
            let out = rig.of('a').pump(&[node('b')], NOW + 110 + i * 10, true);
            rig.deliver(&node('a'), out, NOW + 110 + i * 10);
        }
        let a_root = root_of(&mut rig, 'a', &set_id);
        assert!(
            !a_root.join("from-c.txt").exists(),
            "B cannot serve what it has not pulled"
        );

        // C comes back within reach: A asks IT, and the bytes land. Without
        // the refusal, A's need would still be parked against B.
        for i in 0..4 {
            rig.settle(NOW + 200 + i * 10);
        }
        assert_eq!(
            std::fs::read_to_string(a_root.join("from-c.txt")).expect("landed at a"),
            "made by c"
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

    /// A set one was only INVITED to offers exactly two gestures. Pausing it
    /// would sign a live status with no root; resuming it would disarm the
    /// consent guard before consent.
    #[test]
    fn an_invited_set_offers_accept_and_decline_and_nothing_else() {
        let mut rig = Rig::new(&['a', 'b']);
        let (set_id, _root) = set_with_files(&mut rig, 'a');
        rig.of('a')
            .invite(&set_id, &node('b'), NOW)
            .expect("invite");
        let peers = rig.peers_of('a');
        let out = rig.of('a').pump(&peers, NOW + 1, false);
        rig.deliver(&node('a'), out, NOW + 1);

        for refused in [
            MemberStatus::Paused,
            MemberStatus::Active,
            MemberStatus::Left,
        ] {
            assert!(
                rig.of('b').sign_status(&set_id, refused, NOW + 2).is_err(),
                "{refused:?} must not be available to an invitee"
            );
        }
        assert_eq!(
            rig.of('b')
                .set(&set_id)
                .expect("state")
                .membership
                .effective(&node('b')),
            Effective::Invited,
            "and nothing was signed"
        );
        // Declining IS available.
        rig.of('b')
            .sign_status(&set_id, MemberStatus::Declined, NOW + 3)
            .expect("decline");
    }

    /// Leaving stops touching the folder: the local files stay, and the
    /// engine holds no root to walk, watch or write.
    #[test]
    fn leaving_lets_go_of_the_folder() {
        let mut rig = Rig::new(&['a', 'b']);
        let (set_id, _a_root) = synced_pair(&mut rig);
        let b_root = root_of(&mut rig, 'b', &set_id);

        rig.of('b')
            .sign_status(&set_id, MemberStatus::Left, NOW + 100)
            .expect("leave");
        let state = rig.of('b').set(&set_id).expect("state");
        assert!(state.root.is_none(), "no root to touch any more");
        assert!(state.pending.is_empty());
        // The files stay exactly where they were.
        assert!(b_root.join("a.txt").exists());
        // And a later scan is simply not possible: nothing to scan.
        assert!(rig.of('b').rescan_set(&set_id).is_err());
    }

    /// A device that paused (or left) keeps its files and stops serving
    /// them: a need from a member gets nothing.
    #[test]
    fn a_paused_device_stops_serving_its_bytes() {
        let mut rig = Rig::new(&['a', 'b']);
        let (set_id, _a_root) = synced_pair(&mut rig);
        rig.of('a')
            .sign_status(&set_id, MemberStatus::Paused, NOW + 100)
            .expect("pause");

        let need = Message::Need {
            set_id: set_id.clone(),
            need_id: 7,
            paths: vec!["a.txt".into()],
        };
        let out = rig
            .of('a')
            .on_message(&node('b'), &need.to_value(), NOW + 101);
        assert!(out.is_empty(), "a paused source publishes nothing: {out:?}");
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
