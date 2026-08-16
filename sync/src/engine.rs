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
use std::path::PathBuf;

use serde_json::{Value, json};

use crate::clock::SetClock;
use crate::index::{Entry, SetIndex};
use crate::membership::{Absorb, Effective, SetMembership};
use crate::protocol::{DIALECT, HeadPosition, Message, descriptor_hash, paginate};
use crate::records::{
    Endorsement, InvitedRecord, MemberRecord, MemberStatus, Record, SetDescriptor, SetKind,
    valid_node_id, valid_set_id,
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

struct RoundBuffer {
    head: Option<(HeadPosition, Option<u64>)>,
    records: BTreeMap<u64, (Vec<Record>, Vec<Endorsement>)>,
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
}

impl Engine {
    /// Opens the engine over its persisted state. `self_node` is this
    /// device's node_id, resolved by the orchestrator from the directory.
    pub fn open(store: Store, self_node: String, now: u64) -> io::Result<Engine> {
        let mut sets = BTreeMap::new();
        for (set_id, meta) in store.load_metas()? {
            let state = SetState::from_meta(&store, &set_id, &meta, now)?;
            sets.insert(set_id, state);
        }
        Ok(Engine {
            store,
            self_node,
            sets,
            round_counter: 0,
            sent_rounds: BTreeMap::new(),
        })
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

    pub fn set_ids(&self) -> Vec<String> {
        self.sets.keys().cloned().collect()
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
        let mut state = SetState::fresh(&self.store, &set_id, membership, Some(root), now)?;
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
        state.persist(&self.store, set_id)?;
        Ok(report)
    }

    // -----------------------------------------------------------------------
    // The wire, inbound.
    // -----------------------------------------------------------------------

    /// One `peer.message` payload from the device `from` (node_id, already
    /// translated and transport-authenticated by the Core). Returns what to
    /// send back.
    pub fn on_message(&mut self, from: &str, payload: &Value, now: u64) -> Vec<Outgoing> {
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
            } => self.on_invite(
                from,
                set_id,
                descriptor,
                records,
                endorsements,
                (stats_entries, stats_total_size),
                sync_pub,
                now,
            ),
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
            } => self.on_head(from, &set_id, round, answers, position, &sync_pub, now),
            Message::Records {
                set_id,
                round,
                page,
                records,
                endorsements,
            } => {
                self.buffer_page(from, &set_id, round, now, |buffer| {
                    buffer.records.insert(page, (records, endorsements));
                });
                self.try_complete(from, &set_id, round, now)
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
                self.try_complete(from, &set_id, round, now)
            }
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
        if !self.sets.contains_key(&set_id) {
            // A new invitation: the shadow state is born here - membership
            // without a root, until the consent gesture. The inviter's key
            // is pinned by this very channel.
            let membership = SetMembership::new(descriptor);
            let Ok(state) = SetState::fresh(&self.store, &set_id, membership, None, now) else {
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
        // The claim is the INVITER's word, kept for the consent card, and
        // only while the invitation stands open.
        if state.membership.effective(&self.self_node) == Effective::Invited {
            state.invite_claim = Some(InviteClaim {
                inviter: from.to_string(),
                entries: stats.0,
                total_size: stats.1,
            });
        }
        let _ = state.persist(&self.store, &set_id);
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
        let record_pages = paginate(&records).unwrap_or_default();

        let serving = state.root.is_some()
            && state.membership.effective(&self_node) == Effective::Active
            && state.membership.effective(from) == Effective::Active;
        let (entry_pages, complete) = if serving {
            let delta: Vec<Value> = state
                .index
                .not_covered_by(their_vv)
                .map(Entry::to_value)
                .collect();
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
        for (records, endorsements) in buffer.records.values() {
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

        // Persist the absorbed state BEFORE the watermark moves: the two
        // live in one meta.json write here, which makes the letter's
        // ordering rule an atomicity property of the rename.
        if position.entries_complete && sender_active {
            let watermark = state.watermarks.entry(from.to_string()).or_default();
            watermark.merge_max(&position.set_vv);
        }
        let _ = state.persist(&self.store, set_id);
        Vec::new()
    }

    // -----------------------------------------------------------------------
    // The wire, outbound: rounds, invitations, introductions, stubs.
    // -----------------------------------------------------------------------

    /// The periodic and on-change driver: opens rounds toward reachable
    /// active members with news, retries invitations, introduces unpinned
    /// devices, keeps stub proofs going. `force` is the safety tick
    /// (opens regardless of the change heuristic).
    pub fn pump(&mut self, reachable: &[String], now: u64, force: bool) -> Vec<Outgoing> {
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
                    out.push(Outgoing {
                        to: invitee.clone(),
                        payload: message,
                    });
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
                out.push(Outgoing {
                    to: target,
                    payload: head.to_value(),
                });
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
                    out.push(Outgoing {
                        to: peer,
                        payload: head.to_value(),
                    });
                }
            }

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
                out.push(Outgoing {
                    to: target,
                    payload: head.to_value(),
                });
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
                out.push(Outgoing {
                    to: target,
                    payload: head.to_value(),
                });
            }
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
        now: u64,
    ) -> io::Result<SetState> {
        let dir = store.set_dir(set_id)?;
        let index = SetIndex::new();
        let clock = SetClock::open(&dir, &index, "", now)?;
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

    #[cfg(test)]
    pub(crate) fn stub_active(&self) -> bool {
        self.stub.is_some()
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
        });
        store.save_meta(set_id, &meta)?;
        store.save_index(set_id, &self.index)
    }

    fn from_meta(store: &Store, set_id: &str, meta: &Value, now: u64) -> io::Result<SetState> {
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
        let clock = SetClock::open(&dir, &index, "", now)?;
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
        })
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

        /// Delivers every queued message to its engine, feeding the answers
        /// back until quiescence. The budget assertion IS the termination
        /// proof: no exchange may ping-pong forever.
        fn deliver(&mut self, from: &str, out: Vec<Outgoing>, now: u64) -> u64 {
            let mut queue: Vec<(String, Outgoing)> =
                out.into_iter().map(|o| (from.to_string(), o)).collect();
            let mut count = 0;
            while let Some((sender, message)) = queue.pop() {
                count += 1;
                assert!(count < 500, "message storm: the protocol must terminate");
                let target_node = message.to.clone();
                let target = self.engines.get_mut(&target_node).expect("target engine");
                let more = target.on_message(&sender, &message.payload, now);
                queue.extend(more.into_iter().map(|o| (target_node.clone(), o)));
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
        // A's three entries are parked in B's pending set, and B's
        // watermark for A covers A's advertised position.
        let b = rig.of('b');
        let state = b.set(&set_id).expect("state");
        assert_eq!(state.pending.len(), 3, "{:?}", state.pending.keys());
        assert!(state.pending.contains_key("a.txt"));
        assert!(state.pending.contains_key("sub/b.txt"));
        let a_watermark = state.watermarks.get(&node('a')).expect("watermark");
        assert!(!a_watermark.is_empty());

        // Quiescence: a further forced settle exchanges bounded chatter and
        // changes nothing.
        let before = rig.of('b').set(&set_id).expect("state").pending.len();
        rig.settle(NOW + 40);
        assert_eq!(
            rig.of('b').set(&set_id).expect("state").pending.len(),
            before
        );
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
            assert_eq!(
                rig.of(follower).set(&set_id).expect("state").pending.len(),
                3,
                "A's entries parked at {follower}"
            );
        }
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
            out.iter().any(|o| o.to == node('d')),
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
            for back in b.on_message(&node('a'), &out.payload, NOW + 1) {
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
