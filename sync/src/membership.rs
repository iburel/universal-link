// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

//! One set's membership state (doc/sync-engine.md, section 2): the records
//! absorbed verify-then-keep-newest, the pin table that decides trust, and
//! the pure derivation of each device's effective status - where the door
//! rule lives.
//!
//! The shape of the rules, deliberately: records are FACTS, kept per class
//! (newest live, newest terminal, newest invitation), and the door is
//! evaluated at READ time. An `active` whose admitting invitation has not
//! arrived yet is thereby "held" without a holding pen: it is simply not
//! counted until the invitation lands, and ordering never loses a
//! legitimate member. Only signature failures drop records.

use std::collections::BTreeMap;

use serde_json::{Map, Value, json};

use crate::records::{
    Endorsement, InvitedRecord, MemberRecord, MemberStatus, Record, SetDescriptor, valid_node_id,
};

/// Cap on records stashed for a device whose key we cannot check yet: enough
/// for the real classes (a live, a terminal, an invitation, strays), a bound
/// on what a hostile page can park here.
const UNVERIFIED_CAP: usize = 8;

/// Cap on devices tracked per set. Verified members are real, bounded
/// devices; this bounds the STRANGERS gossip can name (a device we track
/// only as unverified blobs). Far above any real account.
const DEVICE_CAP: usize = 64;

/// How a binding between a node_id and a sync key was established.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PinSource {
    /// A direct, channel-authenticated message from the device itself: the
    /// only source that may CREATE or REPLACE a pin.
    Direct,
    /// A one-hop endorsement, accepted from a directly-pinned active member.
    /// Fills absence only; upgraded in place on first direct contact.
    Endorsed { by: String },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pin {
    pub key: String,
    pub source: PinSource,
}

/// What absorbing one record did.
#[derive(Debug, PartialEq, Eq)]
pub enum Absorb {
    /// Verified and kept (or superseded by what we already held: idempotent
    /// either way).
    Absorbed,
    /// Signature-valid shape but no usable binding for its signer yet: kept
    /// aside, re-checked whenever a binding changes. These devices are the
    /// introduction rounds' targets.
    Unverified,
    /// Refused: a broken signature, a foreign set, or a bound exceeded. The
    /// reason is for the log.
    Dropped(&'static str),
}

/// A device's effective status in the set, derived - never stored.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Effective {
    Active,
    Paused,
    /// Invited (or re-invited after a terminal record), acceptance pending.
    Invited,
    Declined,
    Left,
    /// Named by gossip, nothing verifiable yet.
    Unknown,
}

#[derive(Clone, Debug, Default)]
struct DeviceState {
    /// Newest verified `active`/`paused` record.
    live: Option<MemberRecord>,
    /// Newest verified `left`/`declined` record. Kept even when a later live
    /// record exists: the door is measured against it.
    terminal: Option<MemberRecord>,
    /// Newest verified invitation.
    invited: Option<InvitedRecord>,
    /// The highest seq-like value seen about this device (self-signed seqs
    /// and invitations' supersedes_seq): what one's own next record must
    /// exceed (seq seeding, and the invite payload counts).
    max_seq_seen: u64,
    /// Records awaiting a binding, capped, oldest out.
    unverified: Vec<Value>,
}

impl DeviceState {
    fn is_empty(&self) -> bool {
        self.live.is_none()
            && self.terminal.is_none()
            && self.invited.is_none()
            && self.unverified.is_empty()
    }
}

pub struct SetMembership {
    pub descriptor: SetDescriptor,
    devices: BTreeMap<String, DeviceState>,
    pins: BTreeMap<String, Pin>,
    /// Accepted endorsements, keyed by the endorsed device: re-gossiped so
    /// the witness travels with the records.
    endorsements: BTreeMap<String, Endorsement>,
}

impl SetMembership {
    pub fn new(descriptor: SetDescriptor) -> SetMembership {
        SetMembership {
            descriptor,
            devices: BTreeMap::new(),
            pins: BTreeMap::new(),
            endorsements: BTreeMap::new(),
        }
    }

    /// The binding for `node_id`, whatever its source.
    pub fn key_for(&self, node_id: &str) -> Option<&str> {
        self.pins.get(node_id).map(|p| p.key.as_str())
    }

    /// Records a DIRECT, channel-authenticated binding: the only operation
    /// that creates or replaces a pin. A different key from the same device
    /// (the engine was reinstalled) replaces the pin and drops the records
    /// verified under the old key - untrusted pending re-receipt, at which
    /// point they park as unverified against the new key.
    pub fn pin_direct(&mut self, node_id: &str, key: &str) {
        match self.pins.get(node_id) {
            Some(pin) if pin.key == key => {
                // Same key: at most an upgrade from endorsed to direct.
                self.pins.insert(
                    node_id.to_string(),
                    Pin {
                        key: key.to_string(),
                        source: PinSource::Direct,
                    },
                );
                return;
            }
            Some(_) => {
                // Only what the OLD key signed is retired: the device's own
                // records. Its invitation is the INVITER's signature and
                // survives the reinstall - which is what lets the reinstalled
                // device show as invited rather than vanish.
                if let Some(dev) = self.devices.get_mut(node_id) {
                    dev.live = None;
                    dev.terminal = None;
                }
            }
            None => {}
        }
        self.pins.insert(
            node_id.to_string(),
            Pin {
                key: key.to_string(),
                source: PinSource::Direct,
            },
        );
        self.reverify();
    }

    /// The devices gossip placed in the set without a binding we could
    /// check: the introduction rounds' target list.
    pub fn unpinned_devices(&self) -> Vec<String> {
        self.devices
            .iter()
            .filter(|(node, dev)| !dev.unverified.is_empty() && !self.pins.contains_key(*node))
            .map(|(node, _)| node.clone())
            .collect()
    }

    /// Absorbs one record: verify, then keep-newest. See [`Absorb`].
    pub fn absorb(&mut self, record: &Record) -> Absorb {
        if record.set_id() != self.descriptor.set_id {
            return Absorb::Dropped("foreign set");
        }
        match record {
            Record::Member(member) => {
                if !member.verify() {
                    return Absorb::Dropped("bad signature");
                }
                match self.pins.get(&member.node_id) {
                    Some(pin) if pin.key == member.sync_pub => {
                        self.absorb_member_verified(member.clone());
                        Absorb::Absorbed
                    }
                    // No binding, or a record signed under another key than
                    // the one we pinned: parked, re-checked on pin changes.
                    _ => self.stash_unverified(record),
                }
            }
            Record::Invited(invited) => {
                let Some(inviter_key) = self.key_for(&invited.invited_by).map(str::to_string)
                else {
                    return self.stash_unverified(record);
                };
                if !invited.verify(&inviter_key) {
                    return Absorb::Dropped("bad signature");
                }
                self.absorb_invited_verified(invited.clone());
                Absorb::Absorbed
            }
        }
    }

    fn absorb_member_verified(&mut self, member: MemberRecord) {
        let dev = self.devices.entry(member.node_id.clone()).or_default();
        dev.max_seq_seen = dev.max_seq_seen.max(member.seq);
        let slot = if member.status.is_terminal() {
            &mut dev.terminal
        } else {
            &mut dev.live
        };
        let newer = slot.as_ref().is_none_or(|held| member.beats(held));
        if newer {
            *slot = Some(member);
        }
    }

    fn absorb_invited_verified(&mut self, invited: InvitedRecord) {
        let dev = self.devices.entry(invited.node_id.clone()).or_default();
        dev.max_seq_seen = dev.max_seq_seen.max(invited.supersedes_seq);
        let newer = dev.invited.as_ref().is_none_or(|held| invited.beats(held));
        if newer {
            dev.invited = Some(invited);
        }
    }

    fn stash_unverified(&mut self, record: &Record) -> Absorb {
        let node = record.node_id();
        if !self.devices.contains_key(node) && self.devices.len() >= DEVICE_CAP {
            return Absorb::Dropped("device cap");
        }
        let dev = self.devices.entry(node.to_string()).or_default();
        let value = record.to_value();
        if dev.unverified.contains(&value) {
            return Absorb::Unverified;
        }
        if dev.unverified.len() >= UNVERIFIED_CAP {
            dev.unverified.remove(0);
        }
        dev.unverified.push(value);
        Absorb::Unverified
    }

    /// Accepts a one-hop endorsement: the endorser must be DIRECTLY pinned
    /// (never endorsed itself: one hop of witness, no chains) and verified
    /// active in this set, and the binding only fills absence - it replaces
    /// nothing, direct or endorsed.
    pub fn absorb_endorsement(&mut self, endorsement: &Endorsement) -> Absorb {
        let Some(endorser) = self.pins.get(&endorsement.endorsed_by) else {
            return Absorb::Dropped("unpinned endorser");
        };
        if endorser.source != PinSource::Direct {
            return Absorb::Dropped("endorsement is one hop only");
        }
        let endorser_key = endorser.key.clone();
        if self.effective(&endorsement.endorsed_by) != Effective::Active {
            return Absorb::Dropped("endorser not active");
        }
        if !endorsement.verify(&endorser_key) {
            return Absorb::Dropped("bad signature");
        }
        if self.pins.contains_key(&endorsement.node_id) {
            // Already bound (either way): the endorsement adds nothing and
            // must never replace.
            return Absorb::Absorbed;
        }
        self.pins.insert(
            endorsement.node_id.clone(),
            Pin {
                key: endorsement.sync_pub.clone(),
                source: PinSource::Endorsed {
                    by: endorsement.endorsed_by.clone(),
                },
            },
        );
        self.endorsements
            .insert(endorsement.node_id.clone(), endorsement.clone());
        self.reverify();
        Absorb::Absorbed
    }

    /// The endorsements this device may itself issue: one per key it pinned
    /// by DIRECT contact (never for endorsed bindings - not transitive).
    pub fn endorsable(&self) -> Vec<(String, String)> {
        self.pins
            .iter()
            .filter(|(_, pin)| pin.source == PinSource::Direct)
            .map(|(node, pin)| (node.clone(), pin.key.clone()))
            .collect()
    }

    /// The accepted endorsements, for the records pages.
    pub fn endorsements(&self) -> Vec<&Endorsement> {
        self.endorsements.values().collect()
    }

    /// Re-runs every parked record against the current bindings: called
    /// after any pin or endorsement change. Cheap (the pool is capped), and
    /// what verifies leaves the pool.
    fn reverify(&mut self) {
        let parked: Vec<(String, Vec<Value>)> = self
            .devices
            .iter_mut()
            .filter(|(_, dev)| !dev.unverified.is_empty())
            .map(|(node, dev)| (node.clone(), std::mem::take(&mut dev.unverified)))
            .collect();
        for (node, blobs) in parked {
            for blob in blobs {
                // A blob that still cannot verify re-parks itself; one whose
                // signature now provably FAILS drops here, at the first
                // moment failure is provable.
                let Some(record) = Record::from_value(&blob) else {
                    continue;
                };
                let _ = self.absorb(&record);
            }
            // A device whose last blob evaporated without leaving records
            // must not linger as an introduction target.
            if let Some(dev) = self.devices.get(&node)
                && dev.is_empty()
            {
                self.devices.remove(&node);
            }
        }
    }

    /// The device's effective status: the pure derivation where the door
    /// rule lives. The creator's exception covers its FIRST join only; a
    /// creator that left its own set is re-invited like anyone.
    pub fn effective(&self, node_id: &str) -> Effective {
        let Some(dev) = self.devices.get(node_id) else {
            return Effective::Unknown;
        };
        let terminal_seq = dev.terminal.as_ref().map_or(0, |r| r.seq);
        let door_open = (node_id == self.descriptor.created_by && terminal_seq == 0)
            || dev
                .invited
                .as_ref()
                .is_some_and(|i| i.supersedes_seq >= terminal_seq);

        let live_current = match (&dev.live, &dev.terminal) {
            (Some(live), Some(terminal)) => live.beats(terminal),
            (Some(_), None) => true,
            _ => false,
        };
        if live_current && door_open {
            return match dev.live.as_ref().expect("live_current").status {
                MemberStatus::Active => Effective::Active,
                MemberStatus::Paused => Effective::Paused,
                _ => unreachable!("live slot holds live statuses only"),
            };
        }
        // Either no current live record, or one held at a closed door: what
        // the rest of the records say.
        if let Some(terminal) = &dev.terminal {
            let reopened = dev
                .invited
                .as_ref()
                .is_some_and(|i| i.supersedes_seq >= terminal.seq);
            if reopened {
                return Effective::Invited;
            }
            return match terminal.status {
                MemberStatus::Declined => Effective::Declined,
                MemberStatus::Left => Effective::Left,
                _ => unreachable!("terminal slot holds terminal statuses only"),
            };
        }
        if dev.invited.is_some() {
            return Effective::Invited;
        }
        Effective::Unknown
    }

    /// The seq one's own NEXT record must carry: `1 + max(seq over every
    /// record about self ever seen)`, invitations' supersedes_seq included -
    /// never a local counter restarted.
    pub fn next_seq(&self, node_id: &str) -> u64 {
        1 + self.devices.get(node_id).map_or(0, |dev| dev.max_seq_seen)
    }

    /// The verified records held ABOUT `node_id` - what every answer to a
    /// head echoes about its sender, and what proves absorption to stubs.
    pub fn records_about(&self, node_id: &str) -> Vec<Value> {
        let Some(dev) = self.devices.get(node_id) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        if let Some(r) = &dev.live {
            out.push(r.to_value());
        }
        if let Some(r) = &dev.terminal {
            out.push(r.to_value());
        }
        if let Some(r) = &dev.invited {
            out.push(r.to_value());
        }
        out
    }

    /// Every device with any state, for the gossip pages and (later) the
    /// cards.
    pub fn device_ids(&self) -> Vec<String> {
        self.devices.keys().cloned().collect()
    }

    // -----------------------------------------------------------------------
    // Persistence (nested under "membership" in the set's meta.json; the
    // later bricks add sibling keys).
    // -----------------------------------------------------------------------

    pub fn to_value(&self) -> Value {
        let devices: Map<String, Value> = self
            .devices
            .iter()
            .map(|(node, dev)| {
                (
                    node.clone(),
                    json!({
                        "live": dev.live.as_ref().map(MemberRecord::to_value),
                        "terminal": dev.terminal.as_ref().map(MemberRecord::to_value),
                        "invited": dev.invited.as_ref().map(InvitedRecord::to_value),
                        "max_seq_seen": dev.max_seq_seen,
                        "unverified": dev.unverified,
                    }),
                )
            })
            .collect();
        let pins: Map<String, Value> = self
            .pins
            .iter()
            .map(|(node, pin)| {
                let source = match &pin.source {
                    PinSource::Direct => json!("direct"),
                    PinSource::Endorsed { by } => json!({ "endorsed_by": by }),
                };
                (node.clone(), json!({ "key": pin.key, "source": source }))
            })
            .collect();
        json!({
            "descriptor": self.descriptor.to_value(),
            "devices": devices,
            "pins": pins,
            "endorsements": self.endorsements.values()
                .map(Endorsement::to_value).collect::<Vec<_>>(),
        })
    }

    /// Loads what [`SetMembership::to_value`] wrote. `None` = corrupt: the
    /// caller fails the set loudly rather than resyncing from a guess.
    pub fn from_value(value: &Value) -> Option<SetMembership> {
        let descriptor = SetDescriptor::from_value(value.get("descriptor")?)?;
        let mut membership = SetMembership::new(descriptor);
        for (node, pin) in value.get("pins")?.as_object()? {
            if !valid_node_id(node) {
                return None;
            }
            let key = pin.get("key")?.as_str()?;
            let source = match pin.get("source")? {
                Value::String(s) if s == "direct" => PinSource::Direct,
                other => PinSource::Endorsed {
                    by: other.get("endorsed_by")?.as_str()?.to_string(),
                },
            };
            membership.pins.insert(
                node.clone(),
                Pin {
                    key: key.to_string(),
                    source,
                },
            );
        }
        for endorsement in value.get("endorsements")?.as_array()? {
            let endorsement = Endorsement::from_value(endorsement)?;
            membership
                .endorsements
                .insert(endorsement.node_id.clone(), endorsement);
        }
        for (node, dev) in value.get("devices")?.as_object()? {
            if !valid_node_id(node) {
                return None;
            }
            let mut state = DeviceState {
                max_seq_seen: dev.get("max_seq_seen")?.as_u64()?,
                ..DeviceState::default()
            };
            for (slot, field) in [("live", &mut state.live), ("terminal", &mut state.terminal)] {
                match dev.get(slot)? {
                    Value::Null => {}
                    record => *field = Some(MemberRecord::from_value(record)?),
                }
            }
            match dev.get("invited")? {
                Value::Null => {}
                record => state.invited = Some(InvitedRecord::from_value(record)?),
            }
            state.unverified = dev.get("unverified")?.as_array()?.clone();
            membership.devices.insert(node.clone(), state);
        }
        Some(membership)
    }
}

#[cfg(test)]
mod tests {
    use crate::identity::Identity;
    use crate::records::SetKind;

    use super::*;

    const SET: &str = "AAAAAAAAAAAAAAAAAAAAAA";

    struct Device {
        _dir: tempfile::TempDir,
        identity: Identity,
        node_id: String,
    }

    fn device(node_id: &str) -> Device {
        let dir = tempfile::tempdir().expect("tempdir");
        let identity = Identity::load_or_generate(dir.path()).expect("identity");
        Device {
            _dir: dir,
            identity,
            node_id: node_id.to_string(),
        }
    }

    fn node(letter: char) -> String {
        std::iter::repeat_n(letter, 64).collect()
    }

    /// A set created by `creator`, whose key is pinned (we met it).
    fn set_of(creator: &Device) -> SetMembership {
        let descriptor = SetDescriptor::create(
            SET.into(),
            SetKind::Dir,
            "Projects".into(),
            creator.node_id.clone(),
            1000,
            &creator.identity,
        );
        let mut membership = SetMembership::new(descriptor);
        membership.pin_direct(&creator.node_id, &creator.identity.public_hex());
        membership
    }

    fn own(dev: &Device, status: MemberStatus, seq: u64, at: u64) -> Record {
        Record::Member(MemberRecord::sign_own(
            SET.into(),
            dev.node_id.clone(),
            status,
            seq,
            1,
            at,
            &dev.identity,
        ))
    }

    fn invite(inviter: &Device, invitee: &Device, supersedes: u64, at: u64) -> Record {
        Record::Invited(InvitedRecord::sign_invite(
            SET.into(),
            invitee.node_id.clone(),
            inviter.node_id.clone(),
            supersedes,
            at,
            &inviter.identity,
        ))
    }

    /// The standard fixture: creator A active, invitee B pinned.
    fn fixture() -> (SetMembership, Device, Device) {
        let a = device(&node('a'));
        let b = device(&node('b'));
        let mut m = set_of(&a);
        assert_eq!(
            m.absorb(&own(&a, MemberStatus::Active, 1, 1000)),
            Absorb::Absorbed
        );
        m.pin_direct(&b.node_id, &b.identity.public_hex());
        (m, a, b)
    }

    #[test]
    fn the_creator_needs_no_invitation_for_its_first_join() {
        let (m, a, _b) = fixture();
        assert_eq!(m.effective(&a.node_id), Effective::Active);
    }

    #[test]
    fn an_active_without_its_admitting_invitation_is_held_not_dropped() {
        let (mut m, a, b) = fixture();
        // B's active arrives BEFORE the invitation (gossip pages out of
        // order): absorbed, counted for nothing.
        assert_eq!(
            m.absorb(&own(&b, MemberStatus::Active, 1, 1100)),
            Absorb::Absorbed
        );
        assert_eq!(m.effective(&b.node_id), Effective::Unknown);
        // The invitation lands: the held record is re-evaluated, nothing was
        // lost to ordering.
        assert_eq!(m.absorb(&invite(&a, &b, 0, 1050)), Absorb::Absorbed);
        assert_eq!(m.effective(&b.node_id), Effective::Active);
    }

    #[test]
    fn keep_newest_orders_by_seq_then_at_then_sig() {
        let (mut m, a, b) = fixture();
        m.absorb(&invite(&a, &b, 0, 1050));
        m.absorb(&own(&b, MemberStatus::Active, 1, 1100));
        m.absorb(&own(&b, MemberStatus::Paused, 2, 1200));
        assert_eq!(m.effective(&b.node_id), Effective::Paused);
        // An older seq arriving late changes nothing.
        m.absorb(&own(&b, MemberStatus::Active, 1, 1100));
        assert_eq!(m.effective(&b.node_id), Effective::Paused);
        // Same seq, later at wins.
        m.absorb(&own(&b, MemberStatus::Active, 2, 1300));
        assert_eq!(m.effective(&b.node_id), Effective::Active);
    }

    #[test]
    fn terminal_statuses_are_binding_until_a_fresh_invitation() {
        let (mut m, a, b) = fixture();
        m.absorb(&invite(&a, &b, 0, 1050));
        m.absorb(&own(&b, MemberStatus::Active, 1, 1100));
        m.absorb(&own(&b, MemberStatus::Left, 2, 1200));
        assert_eq!(m.effective(&b.node_id), Effective::Left);

        // B re-signs itself active without a new invitation: the old
        // invitation (supersedes_seq 0 < left's seq 2) does not cover it.
        m.absorb(&own(&b, MemberStatus::Active, 3, 1300));
        assert_eq!(m.effective(&b.node_id), Effective::Left);
        // Paused is a modality of membership, not a way around the door.
        m.absorb(&own(&b, MemberStatus::Paused, 4, 1400));
        assert_eq!(m.effective(&b.node_id), Effective::Left);

        // A re-invites, covering the terminal seq: the door reopens, and the
        // held active now counts.
        m.absorb(&invite(&a, &b, 2, 1500));
        assert_eq!(m.effective(&b.node_id), Effective::Paused);
    }

    #[test]
    fn a_replayed_old_invitation_loses_to_later_records() {
        let (mut m, a, b) = fixture();
        m.absorb(&invite(&a, &b, 0, 1050));
        m.absorb(&own(&b, MemberStatus::Active, 1, 1100));
        m.absorb(&own(&b, MemberStatus::Declined, 2, 1200));
        assert_eq!(m.effective(&b.node_id), Effective::Declined);
        // The first invitation replayed: stale supersedes_seq, no reopening.
        m.absorb(&invite(&a, &b, 0, 1050));
        assert_eq!(m.effective(&b.node_id), Effective::Declined);
        // A fresh re-invite reopens: pending acceptance, not active.
        m.absorb(&invite(&a, &b, 2, 1300));
        assert_eq!(m.effective(&b.node_id), Effective::Invited);
    }

    #[test]
    fn the_creator_that_left_is_reinvited_like_anyone() {
        let (mut m, a, b) = fixture();
        m.absorb(&invite(&a, &b, 0, 1050));
        m.absorb(&own(&b, MemberStatus::Active, 1, 1100));
        m.absorb(&own(&a, MemberStatus::Left, 2, 1200));
        assert_eq!(m.effective(&a.node_id), Effective::Left);
        // The creator exception covers the FIRST join only.
        m.absorb(&own(&a, MemberStatus::Active, 3, 1300));
        assert_eq!(m.effective(&a.node_id), Effective::Left);
        m.absorb(&invite(&b, &a, 2, 1400));
        assert_eq!(m.effective(&a.node_id), Effective::Active);
    }

    #[test]
    fn records_for_an_unpinned_device_wait_for_the_pin() {
        let a = device(&node('a'));
        let b = device(&node('b'));
        let mut m = set_of(&a);
        m.absorb(&own(&a, MemberStatus::Active, 1, 1000));
        m.absorb(&invite(&a, &b, 0, 1050));

        // B is not pinned: its record parks, B becomes an introduction
        // target, and nothing counts yet.
        assert_eq!(
            m.absorb(&own(&b, MemberStatus::Active, 1, 1100)),
            Absorb::Unverified
        );
        assert_eq!(m.effective(&b.node_id), Effective::Invited);
        assert_eq!(m.unpinned_devices(), vec![b.node_id.clone()]);

        // The pin lands (a direct head): the parked record verifies.
        m.pin_direct(&b.node_id, &b.identity.public_hex());
        assert_eq!(m.effective(&b.node_id), Effective::Active);
        assert!(m.unpinned_devices().is_empty());
    }

    #[test]
    fn a_record_under_a_foreign_key_never_verifies() {
        let (mut m, a, b) = fixture();
        m.absorb(&invite(&a, &b, 0, 1050));
        // A record about B signed by a THIRD key (a compromised sibling
        // inventing a member): parses, carries its own key, but the pin does
        // not match - parked forever, counted never.
        let mallory = device(&b.node_id);
        assert_eq!(
            m.absorb(&own(&mallory, MemberStatus::Active, 9, 1100)),
            Absorb::Unverified
        );
        assert_eq!(m.effective(&b.node_id), Effective::Invited);
    }

    #[test]
    fn a_broken_signature_is_dropped_outright() {
        let (mut m, _a, b) = fixture();
        let record = own(&b, MemberStatus::Active, 1, 1100);
        let mut value = record.to_value();
        value["seq"] = json!(2);
        let forged = Record::from_value(&value).expect("parses");
        assert_eq!(m.absorb(&forged), Absorb::Dropped("bad signature"));
    }

    #[test]
    fn a_new_direct_pin_retires_the_old_keys_records() {
        let (mut m, a, b) = fixture();
        m.absorb(&invite(&a, &b, 0, 1050));
        m.absorb(&own(&b, MemberStatus::Active, 1, 1100));
        assert_eq!(m.effective(&b.node_id), Effective::Active);

        // B was reinstalled: a direct contact pins a NEW key. The old key's
        // records are untrusted pending re-receipt; the invitation (signed
        // by A) survives, so B shows invited, not active.
        let b_new = device(&b.node_id);
        m.pin_direct(&b.node_id, &b_new.identity.public_hex());
        assert_eq!(m.effective(&b.node_id), Effective::Invited);

        // Old-key records re-received now park unverified instead of
        // counting.
        assert_eq!(
            m.absorb(&own(&b, MemberStatus::Active, 1, 1100)),
            Absorb::Unverified
        );
        assert_eq!(m.effective(&b.node_id), Effective::Invited);
        // The new key's own word counts.
        assert_eq!(
            m.absorb(&own(&b_new, MemberStatus::Active, 2, 1200)),
            Absorb::Absorbed
        );
        assert_eq!(m.effective(&b.node_id), Effective::Active);
    }

    #[test]
    fn endorsements_are_one_hop_from_a_directly_pinned_active_member() {
        let (mut m, a, b) = fixture();
        m.absorb(&invite(&a, &b, 0, 1050));
        m.absorb(&own(&b, MemberStatus::Active, 1, 1100));

        // C is invited by A; A departs before anyone pins C. B endorses C's
        // key (B pinned it directly, in this scenario's history).
        let c = device(&node('c'));
        m.absorb(&invite(&a, &c, 0, 1060));
        let record_c = own(&c, MemberStatus::Active, 1, 1150);
        assert_eq!(m.absorb(&record_c), Absorb::Unverified);

        // An endorsement from an UNPINNED endorser is refused.
        let stranger = device(&node('d'));
        let from_stranger = Endorsement::issue(
            c.node_id.clone(),
            c.identity.public_hex(),
            stranger.node_id.clone(),
            &stranger.identity,
        );
        assert_eq!(
            m.absorb_endorsement(&from_stranger),
            Absorb::Dropped("unpinned endorser")
        );

        // From B, directly pinned and active: accepted, C's record verifies.
        let from_b = Endorsement::issue(
            c.node_id.clone(),
            c.identity.public_hex(),
            b.node_id.clone(),
            &b.identity,
        );
        assert_eq!(m.absorb_endorsement(&from_b), Absorb::Absorbed);
        assert_eq!(m.effective(&c.node_id), Effective::Active);

        // One hop only: an endorsement whose endorser is himself only
        // ENDORSED is refused (C vouching for D).
        let d = device(&node('d'));
        let from_c = Endorsement::issue(
            d.node_id.clone(),
            d.identity.public_hex(),
            c.node_id.clone(),
            &c.identity,
        );
        assert_eq!(
            m.absorb_endorsement(&from_c),
            Absorb::Dropped("endorsement is one hop only")
        );
    }

    #[test]
    fn an_endorsement_never_replaces_an_existing_binding() {
        let (mut m, a, b) = fixture();
        m.absorb(&invite(&a, &b, 0, 1050));
        m.absorb(&own(&b, MemberStatus::Active, 1, 1100));
        // B endorses a DIFFERENT key for A (a confused or hostile witness):
        // absorbed as a no-op, A's direct pin stands.
        let fake = device(&node('e'));
        let endorsement = Endorsement::issue(
            a.node_id.clone(),
            fake.identity.public_hex(),
            b.node_id.clone(),
            &b.identity,
        );
        let key_before = m.key_for(&a.node_id).expect("pinned").to_string();
        assert_eq!(m.absorb_endorsement(&endorsement), Absorb::Absorbed);
        assert_eq!(m.key_for(&a.node_id), Some(key_before.as_str()));
    }

    #[test]
    fn seq_seeding_counts_every_record_about_self_including_invitations() {
        let (mut m, a, b) = fixture();
        assert_eq!(m.next_seq(&b.node_id), 1);
        m.absorb(&invite(&a, &b, 4, 1050));
        assert_eq!(m.next_seq(&b.node_id), 5);
        m.absorb(&own(&b, MemberStatus::Active, 7, 1100));
        assert_eq!(m.next_seq(&b.node_id), 8);
    }

    #[test]
    fn strangers_are_capped_but_members_never_are() {
        let (mut m, _a, b) = fixture();
        // Fill the device table with unverifiable strangers.
        for i in 0..DEVICE_CAP {
            let stranger = device(&format!("{:064x}", 0xf000 + i));
            let _ = m.absorb(&own(&stranger, MemberStatus::Active, 1, 1100));
        }
        let overflow = device(&format!("{:064x}", 0xffff_ffff_u64));
        assert_eq!(
            m.absorb(&own(&overflow, MemberStatus::Active, 1, 1100)),
            Absorb::Dropped("device cap")
        );
        // A record for a PINNED device still lands: the cap bounds
        // strangers, not members.
        assert_eq!(
            m.absorb(&own(&b, MemberStatus::Active, 1, 1100)),
            Absorb::Absorbed
        );
    }

    #[test]
    fn membership_round_trips_through_its_persisted_form() {
        let (mut m, a, b) = fixture();
        m.absorb(&invite(&a, &b, 0, 1050));
        m.absorb(&own(&b, MemberStatus::Active, 1, 1100));
        let c = device(&node('c'));
        m.absorb(&invite(&a, &c, 0, 1060));
        let _ = m.absorb(&own(&c, MemberStatus::Active, 1, 1150));

        let reloaded = SetMembership::from_value(&m.to_value()).expect("reload");
        for node in [&a.node_id, &b.node_id, &c.node_id] {
            assert_eq!(reloaded.effective(node), m.effective(node), "for {node}");
        }
        assert_eq!(reloaded.next_seq(&b.node_id), m.next_seq(&b.node_id));
        assert_eq!(reloaded.unpinned_devices(), m.unpinned_devices());
        // The parked record still verifies once the pin lands post-reload.
        let mut reloaded = reloaded;
        reloaded.pin_direct(&c.node_id, &c.identity.public_hex());
        assert_eq!(reloaded.effective(&c.node_id), Effective::Active);

        assert!(SetMembership::from_value(&json!({ "descriptor": "no" })).is_none());
    }
}
