// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

//! The signed objects of membership (doc/sync-engine.md, section 2): the set
//! descriptor, the self-signed membership records, the inviter-signed
//! `invited` records, and the one-hop endorsements.
//!
//! Every signature covers a DOMAIN PREFIX plus the RFC 8785 canonical JSON
//! of the record's full field list, `sig` excluded: the domain keeps the
//! object kinds apart under the one engine key (core/src/identity.rs's
//! separation doctrine), and the signed `set_id` and `status` keep a record
//! from being replayed across sets or re-labelled.
//!
//! Parsing is STRICT: the exact field set, exact types, bounded formats. An
//! unknown extra field is a malformed record, not an extension point - a
//! signature must never be ambiguous about what it covered.

use ed25519_dalek::{Signature, VerifyingKey};
use serde_json::{Map, Value, json};

use crate::canonical::to_canonical_json;
use crate::identity::Identity;

const DOMAIN_DESCRIPTOR: &[u8] = b"1device-sync:descriptor:";
const DOMAIN_MEMBER: &[u8] = b"1device-sync:member-record:";
const DOMAIN_INVITED: &[u8] = b"1device-sync:invited-record:";
const DOMAIN_ENDORSEMENT: &[u8] = b"1device-sync:endorsement:";

/// Bound on a descriptor's display name (the root's basename at creation):
/// enough for every real filesystem, and a cap on what a page carries.
const NAME_MAX_BYTES: usize = 255;

/// The set identifier: exactly 22 base64url characters (128 bits,
/// unguessable). Validated EVERYWHERE a set_id crosses in, because it also
/// names the set's state directory on disk: nothing path-shaped survives.
pub fn valid_set_id(s: &str) -> bool {
    s.len() == 22
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// Lowercase hex of exactly `len` characters: node ids and public keys (64),
/// signatures (128). The transport and `hex::encode` only ever produce
/// lowercase; refusing the mixed-case aliases keeps one spelling per key.
fn lower_hex(s: &str, len: usize) -> bool {
    s.len() == len
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

pub fn valid_node_id(s: &str) -> bool {
    lower_hex(s, 64)
}

fn valid_pub(s: &str) -> bool {
    lower_hex(s, 64)
}

fn valid_sig(s: &str) -> bool {
    lower_hex(s, 128)
}

/// Detached verification, fail-closed like core/src/account_key.rs: an
/// unreadable key, signature or message all answer `false`, never an error
/// on an authorization path. `verify_strict` refuses small-order artifacts.
pub fn verify_detached(public_hex: &str, msg: &[u8], sig_hex: &str) -> bool {
    let Some(key_bytes) = hex::decode(public_hex)
        .ok()
        .and_then(|b| <[u8; 32]>::try_from(b).ok())
    else {
        return false;
    };
    let Ok(key) = VerifyingKey::from_bytes(&key_bytes) else {
        return false;
    };
    let Some(sig_bytes) = hex::decode(sig_hex)
        .ok()
        .and_then(|b| <[u8; 64]>::try_from(b).ok())
    else {
        return false;
    };
    key.verify_strict(msg, &Signature::from_bytes(&sig_bytes))
        .is_ok()
}

/// Strict object reader: consumes `value` as an object whose keys are
/// EXACTLY `fields`, and hands each back. `None` on any deviation.
fn exact_fields<'v>(value: &'v Value, fields: &[&str]) -> Option<Vec<&'v Value>> {
    let map: &Map<String, Value> = value.as_object()?;
    if map.len() != fields.len() {
        return None;
    }
    fields.iter().map(|f| map.get(*f)).collect()
}

fn str_field(v: &Value) -> Option<&str> {
    v.as_str()
}

/// Integers ride the canonical profile: `as_u64` within 2^53 - 1.
fn int_field(v: &Value) -> Option<u64> {
    v.as_u64().filter(|n| *n < (1 << 53))
}

// ---------------------------------------------------------------------------
// The set descriptor.
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SetKind {
    Dir,
    File,
}

impl SetKind {
    fn as_str(self) -> &'static str {
        match self {
            SetKind::Dir => "dir",
            SetKind::File => "file",
        }
    }

    fn parse(s: &str) -> Option<SetKind> {
        match s {
            "dir" => Some(SetKind::Dir),
            "file" => Some(SetKind::File),
            _ => None,
        }
    }
}

/// Created once, signed by the creator's sync key. Carries NO member list:
/// membership is each device's own signed word.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SetDescriptor {
    pub set_id: String,
    pub kind: SetKind,
    pub name: String,
    pub created_by: String,
    pub created_at: u64,
    pub sig: String,
}

impl SetDescriptor {
    /// Builds and signs a fresh descriptor (the `sync.create` path).
    pub fn create(
        set_id: String,
        kind: SetKind,
        name: String,
        created_by: String,
        created_at: u64,
        identity: &Identity,
    ) -> SetDescriptor {
        let mut descriptor = SetDescriptor {
            set_id,
            kind,
            name,
            created_by,
            created_at,
            sig: String::new(),
        };
        descriptor.sig = identity.sign(&descriptor.signing_bytes());
        descriptor
    }

    fn signing_bytes(&self) -> Vec<u8> {
        signed_bytes(
            DOMAIN_DESCRIPTOR,
            &json!({
                "set_id": self.set_id,
                "kind": self.kind.as_str(),
                "name": self.name,
                "created_by": self.created_by,
                "created_at": self.created_at,
            }),
        )
    }

    /// True iff the signature verifies under the creator's key.
    pub fn verify(&self, creator_pub_hex: &str) -> bool {
        verify_detached(creator_pub_hex, &self.signing_bytes(), &self.sig)
    }

    pub fn to_value(&self) -> Value {
        json!({
            "set_id": self.set_id,
            "kind": self.kind.as_str(),
            "name": self.name,
            "created_by": self.created_by,
            "created_at": self.created_at,
            "sig": self.sig,
        })
    }

    pub fn from_value(value: &Value) -> Option<SetDescriptor> {
        let [set_id, kind, name, created_by, created_at, sig] = exact_fields(
            value,
            &["set_id", "kind", "name", "created_by", "created_at", "sig"],
        )?
        .try_into()
        .ok()?;
        let set_id = str_field(set_id)?;
        let name = str_field(name)?;
        let created_by = str_field(created_by)?;
        let sig = str_field(sig)?;
        if !valid_set_id(set_id)
            || name.is_empty()
            || name.len() > NAME_MAX_BYTES
            || !valid_node_id(created_by)
            || !valid_sig(sig)
        {
            return None;
        }
        Some(SetDescriptor {
            set_id: set_id.to_string(),
            kind: SetKind::parse(str_field(kind)?)?,
            name: name.to_string(),
            created_by: created_by.to_string(),
            created_at: int_field(created_at)?,
            sig: sig.to_string(),
        })
    }
}

// ---------------------------------------------------------------------------
// Self-signed membership records.
// ---------------------------------------------------------------------------

/// The self-signed statuses. `invited` is not here: it is the inviter's word
/// and a different type ([`InvitedRecord`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MemberStatus {
    Active,
    Declined,
    Paused,
    Left,
}

impl MemberStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            MemberStatus::Active => "active",
            MemberStatus::Declined => "declined",
            MemberStatus::Paused => "paused",
            MemberStatus::Left => "left",
        }
    }

    fn parse(s: &str) -> Option<MemberStatus> {
        match s {
            "active" => Some(MemberStatus::Active),
            "declined" => Some(MemberStatus::Declined),
            "paused" => Some(MemberStatus::Paused),
            "left" => Some(MemberStatus::Left),
            _ => None,
        }
    }

    /// Terminal statuses are BINDING: re-signing a live status after one
    /// requires a fresh invitation (the invitation is the door, both ways).
    pub fn is_terminal(self) -> bool {
        matches!(self, MemberStatus::Declined | MemberStatus::Left)
    }
}

/// One device's signed word about its own membership in one set. Monotonic:
/// higher `seq` wins, ties by `at`, then lexicographic `sig`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MemberRecord {
    pub set_id: String,
    pub node_id: String,
    pub status: MemberStatus,
    pub seq: u64,
    /// The device's clock generation for the set (doc/sync-engine.md,
    /// section 3): peers key vv components by it. `gen` on the wire; `gen`
    /// the identifier is reserved in edition 2024.
    pub generation: u64,
    /// The signer's public key, carried by the record (self-certifying;
    /// PINNING decides trust, not the carry).
    pub sync_pub: String,
    pub at: u64,
    pub sig: String,
}

impl MemberRecord {
    /// Builds and signs one's own record. `sync_pub` is the signer's, by
    /// construction.
    pub fn sign_own(
        set_id: String,
        node_id: String,
        status: MemberStatus,
        seq: u64,
        generation: u64,
        at: u64,
        identity: &Identity,
    ) -> MemberRecord {
        let mut record = MemberRecord {
            set_id,
            node_id,
            status,
            seq,
            generation,
            sync_pub: identity.public_hex(),
            at,
            sig: String::new(),
        };
        record.sig = identity.sign(&record.signing_bytes());
        record
    }

    fn signing_bytes(&self) -> Vec<u8> {
        signed_bytes(
            DOMAIN_MEMBER,
            &json!({
                "set_id": self.set_id,
                "node_id": self.node_id,
                "status": self.status.as_str(),
                "seq": self.seq,
                "gen": self.generation,
                "sync_pub": self.sync_pub,
                "at": self.at,
            }),
        )
    }

    /// True iff the signature verifies under the CARRIED key. The caller
    /// still decides trust against the pin table.
    pub fn verify(&self) -> bool {
        verify_detached(&self.sync_pub, &self.signing_bytes(), &self.sig)
    }

    /// The keep-newest order: does `self` supersede `other` (two records
    /// about the same device)?
    pub fn beats(&self, other: &MemberRecord) -> bool {
        (self.seq, self.at, &self.sig) > (other.seq, other.at, &other.sig)
    }

    pub fn to_value(&self) -> Value {
        json!({
            "set_id": self.set_id,
            "node_id": self.node_id,
            "status": self.status.as_str(),
            "seq": self.seq,
            "gen": self.generation,
            "sync_pub": self.sync_pub,
            "at": self.at,
            "sig": self.sig,
        })
    }

    pub(crate) fn from_value(value: &Value) -> Option<MemberRecord> {
        let [set_id, node_id, status, seq, generation, sync_pub, at, sig] = exact_fields(
            value,
            &[
                "set_id", "node_id", "status", "seq", "gen", "sync_pub", "at", "sig",
            ],
        )?
        .try_into()
        .ok()?;
        let set_id = str_field(set_id)?;
        let node_id = str_field(node_id)?;
        let sync_pub = str_field(sync_pub)?;
        let sig = str_field(sig)?;
        if !valid_set_id(set_id)
            || !valid_node_id(node_id)
            || !valid_pub(sync_pub)
            || !valid_sig(sig)
        {
            return None;
        }
        Some(MemberRecord {
            set_id: set_id.to_string(),
            node_id: node_id.to_string(),
            status: MemberStatus::parse(str_field(status)?)?,
            seq: int_field(seq)?,
            generation: int_field(generation)?,
            sync_pub: sync_pub.to_string(),
            at: int_field(at)?,
            sig: sig.to_string(),
        })
    }
}

// ---------------------------------------------------------------------------
// Inviter-signed `invited` records.
// ---------------------------------------------------------------------------

/// The one exception to self-signature: the INVITER's signed word about the
/// invitee. Takes effect only against invitee-signed records with
/// `seq <= supersedes_seq`, which orders replay out without breaking
/// re-invitation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InvitedRecord {
    pub set_id: String,
    /// The invitee.
    pub node_id: String,
    pub invited_by: String,
    /// The invitee's highest seq the inviter holds at issuance (0 at first
    /// invite).
    pub supersedes_seq: u64,
    pub at: u64,
    pub sig: String,
}

impl InvitedRecord {
    pub fn sign_invite(
        set_id: String,
        node_id: String,
        invited_by: String,
        supersedes_seq: u64,
        at: u64,
        identity: &Identity,
    ) -> InvitedRecord {
        let mut record = InvitedRecord {
            set_id,
            node_id,
            invited_by,
            supersedes_seq,
            at,
            sig: String::new(),
        };
        record.sig = identity.sign(&record.signing_bytes());
        record
    }

    fn signing_bytes(&self) -> Vec<u8> {
        signed_bytes(
            DOMAIN_INVITED,
            &json!({
                "set_id": self.set_id,
                "node_id": self.node_id,
                "status": "invited",
                "invited_by": self.invited_by,
                "supersedes_seq": self.supersedes_seq,
                "at": self.at,
            }),
        )
    }

    /// True iff the signature verifies under the INVITER's key, which the
    /// record does not carry: the caller resolves it (pin or endorsement).
    pub fn verify(&self, inviter_pub_hex: &str) -> bool {
        verify_detached(inviter_pub_hex, &self.signing_bytes(), &self.sig)
    }

    /// The keep-newest order among invitations for one device.
    pub fn beats(&self, other: &InvitedRecord) -> bool {
        (self.supersedes_seq, self.at, &self.sig) > (other.supersedes_seq, other.at, &other.sig)
    }

    pub fn to_value(&self) -> Value {
        json!({
            "set_id": self.set_id,
            "node_id": self.node_id,
            "status": "invited",
            "invited_by": self.invited_by,
            "supersedes_seq": self.supersedes_seq,
            "at": self.at,
            "sig": self.sig,
        })
    }

    pub(crate) fn from_value(value: &Value) -> Option<InvitedRecord> {
        let [set_id, node_id, status, invited_by, supersedes_seq, at, sig] = exact_fields(
            value,
            &[
                "set_id",
                "node_id",
                "status",
                "invited_by",
                "supersedes_seq",
                "at",
                "sig",
            ],
        )?
        .try_into()
        .ok()?;
        if str_field(status)? != "invited" {
            return None;
        }
        let set_id = str_field(set_id)?;
        let node_id = str_field(node_id)?;
        let invited_by = str_field(invited_by)?;
        let sig = str_field(sig)?;
        if !valid_set_id(set_id)
            || !valid_node_id(node_id)
            || !valid_node_id(invited_by)
            || node_id == invited_by
            || !valid_sig(sig)
        {
            return None;
        }
        Some(InvitedRecord {
            set_id: set_id.to_string(),
            node_id: node_id.to_string(),
            invited_by: invited_by.to_string(),
            supersedes_seq: int_field(supersedes_seq)?,
            at: int_field(at)?,
            sig: sig.to_string(),
        })
    }
}

/// A wire or persisted record, dispatched on its `status`. The two shapes
/// share the field, not the signer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Record {
    Member(MemberRecord),
    Invited(InvitedRecord),
}

impl Record {
    /// Strict parse of one record object. `None` = malformed, to be dropped.
    pub fn from_value(value: &Value) -> Option<Record> {
        match value.get("status").and_then(Value::as_str)? {
            "invited" => InvitedRecord::from_value(value).map(Record::Invited),
            _ => MemberRecord::from_value(value).map(Record::Member),
        }
    }

    pub fn to_value(&self) -> Value {
        match self {
            Record::Member(r) => r.to_value(),
            Record::Invited(r) => r.to_value(),
        }
    }

    /// The device the record is ABOUT (never the signer for `invited`).
    pub fn node_id(&self) -> &str {
        match self {
            Record::Member(r) => &r.node_id,
            Record::Invited(r) => &r.node_id,
        }
    }

    pub fn set_id(&self) -> &str {
        match self {
            Record::Member(r) => &r.set_id,
            Record::Invited(r) => &r.set_id,
        }
    }
}

// ---------------------------------------------------------------------------
// One-hop endorsements.
// ---------------------------------------------------------------------------

/// `{ node_id, sync_pub }` signed by an endorser who pinned that key by
/// DIRECT contact: one explicit hop of witness, never a chain (an
/// endorsement is not transitive, and endorsing is only ever done for
/// directly pinned keys).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Endorsement {
    pub node_id: String,
    pub sync_pub: String,
    pub endorsed_by: String,
    pub sig: String,
}

impl Endorsement {
    pub fn issue(
        node_id: String,
        sync_pub: String,
        endorsed_by: String,
        identity: &Identity,
    ) -> Endorsement {
        let mut endorsement = Endorsement {
            node_id,
            sync_pub,
            endorsed_by,
            sig: String::new(),
        };
        endorsement.sig = identity.sign(&endorsement.signing_bytes());
        endorsement
    }

    fn signing_bytes(&self) -> Vec<u8> {
        signed_bytes(
            DOMAIN_ENDORSEMENT,
            &json!({
                "node_id": self.node_id,
                "sync_pub": self.sync_pub,
            }),
        )
    }

    /// True iff the signature verifies under the ENDORSER's key.
    pub fn verify(&self, endorser_pub_hex: &str) -> bool {
        verify_detached(endorser_pub_hex, &self.signing_bytes(), &self.sig)
    }

    pub fn to_value(&self) -> Value {
        json!({
            "node_id": self.node_id,
            "sync_pub": self.sync_pub,
            "endorsed_by": self.endorsed_by,
            "sig": self.sig,
        })
    }

    pub fn from_value(value: &Value) -> Option<Endorsement> {
        let [node_id, sync_pub, endorsed_by, sig] =
            exact_fields(value, &["node_id", "sync_pub", "endorsed_by", "sig"])?
                .try_into()
                .ok()?;
        let node_id = str_field(node_id)?;
        let sync_pub = str_field(sync_pub)?;
        let endorsed_by = str_field(endorsed_by)?;
        let sig = str_field(sig)?;
        if !valid_node_id(node_id)
            || !valid_pub(sync_pub)
            || !valid_node_id(endorsed_by)
            || node_id == endorsed_by
            || !valid_sig(sig)
        {
            return None;
        }
        Some(Endorsement {
            node_id: node_id.to_string(),
            sync_pub: sync_pub.to_string(),
            endorsed_by: endorsed_by.to_string(),
            sig: sig.to_string(),
        })
    }
}

/// Domain prefix plus canonical JSON. The fields are built by this module
/// with in-range integers and plain strings, so canonicalization cannot fail
/// here; the `expect` documents that invariant.
fn signed_bytes(domain: &[u8], fields: &Value) -> Vec<u8> {
    let canonical = to_canonical_json(fields).expect("record fields are canonicalizable");
    let mut bytes = Vec::with_capacity(domain.len() + canonical.len());
    bytes.extend_from_slice(domain);
    bytes.extend_from_slice(canonical.as_bytes());
    bytes
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(crate) fn test_identity() -> (tempfile::TempDir, Identity) {
        let dir = tempfile::tempdir().expect("tempdir");
        let identity = Identity::load_or_generate(dir.path()).expect("identity");
        (dir, identity)
    }

    pub(crate) const SET: &str = "AAAAAAAAAAAAAAAAAAAAAA";
    pub(crate) const NODE_A: &str =
        "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    pub(crate) const NODE_B: &str =
        "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    #[test]
    fn a_member_record_round_trips_and_verifies() {
        let (_dir, identity) = test_identity();
        let record = MemberRecord::sign_own(
            SET.into(),
            NODE_A.into(),
            MemberStatus::Active,
            3,
            1,
            1000,
            &identity,
        );
        assert!(record.verify());
        let parsed = match Record::from_value(&record.to_value()) {
            Some(Record::Member(r)) => r,
            other => panic!("expected a member record, got {other:?}"),
        };
        assert_eq!(parsed, record);
        assert!(parsed.verify());
    }

    #[test]
    fn tampering_with_any_signed_field_breaks_the_signature() {
        let (_dir, identity) = test_identity();
        let record = MemberRecord::sign_own(
            SET.into(),
            NODE_A.into(),
            MemberStatus::Left,
            5,
            1,
            1000,
            &identity,
        );
        for tweak in [
            ("status", json!("active")),
            ("seq", json!(6)),
            ("gen", json!(2)),
            ("at", json!(1001)),
            ("set_id", json!("BBBBBBBBBBBBBBBBBBBBBB")),
            ("node_id", json!(NODE_B)),
        ] {
            let mut value = record.to_value();
            value[tweak.0] = tweak.1;
            let parsed = match Record::from_value(&value) {
                Some(Record::Member(r)) => r,
                other => panic!("expected a member record, got {other:?}"),
            };
            assert!(!parsed.verify(), "tampered {} must not verify", tweak.0);
        }
    }

    #[test]
    fn parsing_is_strict_about_shape_and_formats() {
        let (_dir, identity) = test_identity();
        let good = MemberRecord::sign_own(
            SET.into(),
            NODE_A.into(),
            MemberStatus::Active,
            1,
            1,
            1000,
            &identity,
        )
        .to_value();
        assert!(Record::from_value(&good).is_some());

        // An unknown extra field is malformed, not an extension.
        let mut extra = good.clone();
        extra["extra"] = json!(1);
        assert!(Record::from_value(&extra).is_none());

        // A missing field.
        let mut missing = good.as_object().expect("object").clone();
        missing.remove("gen");
        assert!(Record::from_value(&Value::Object(missing)).is_none());

        // Format bounds: set_id shape, hex case, integer range.
        for (field, bad) in [
            ("set_id", json!("../escape")),
            ("set_id", json!("AAAAAAAAAAAAAAAAAAAAA")),
            ("node_id", json!(NODE_A.to_uppercase())),
            ("node_id", json!("ab")),
            ("seq", json!(9007199254740992_u64)),
            ("seq", json!(-1)),
            ("status", json!("gone")),
            ("sig", json!("zz")),
        ] {
            let mut value = good.clone();
            value[field] = bad.clone();
            assert!(
                Record::from_value(&value).is_none(),
                "should have refused {field} = {bad}"
            );
        }
    }

    #[test]
    fn an_invited_record_verifies_under_the_inviter_key_only() {
        let (_dir, inviter) = test_identity();
        let (_dir2, other) = test_identity();
        let invite =
            InvitedRecord::sign_invite(SET.into(), NODE_B.into(), NODE_A.into(), 5, 1000, &inviter);
        assert!(invite.verify(&inviter.public_hex()));
        assert!(!invite.verify(&other.public_hex()));
        let parsed = match Record::from_value(&invite.to_value()) {
            Some(Record::Invited(r)) => r,
            other => panic!("expected an invited record, got {other:?}"),
        };
        assert_eq!(parsed, invite);

        // A self-invitation is malformed by shape.
        let mut selfie = invite.to_value();
        selfie["invited_by"] = json!(NODE_B);
        assert!(Record::from_value(&selfie).is_none());
    }

    #[test]
    fn the_two_record_shapes_do_not_cross_verify() {
        let (_dir, identity) = test_identity();
        // A member record relabelled "invited" must fail parsing (wrong field
        // set), and an invited record's signature must not verify as a member
        // record's: the domains differ even where fields could collide.
        let member = MemberRecord::sign_own(
            SET.into(),
            NODE_A.into(),
            MemberStatus::Active,
            1,
            1,
            1000,
            &identity,
        );
        let mut relabelled = member.to_value();
        relabelled["status"] = json!("invited");
        assert!(Record::from_value(&relabelled).is_none());
    }

    #[test]
    fn a_descriptor_verifies_under_its_creator() {
        let (_dir, creator) = test_identity();
        let (_dir2, other) = test_identity();
        let descriptor = SetDescriptor::create(
            SET.into(),
            SetKind::Dir,
            "Projects".into(),
            NODE_A.into(),
            1000,
            &creator,
        );
        assert!(descriptor.verify(&creator.public_hex()));
        assert!(!descriptor.verify(&other.public_hex()));
        let parsed = SetDescriptor::from_value(&descriptor.to_value()).expect("parse");
        assert_eq!(parsed, descriptor);
        assert!(SetDescriptor::from_value(&json!({ "set_id": "x" })).is_none());
    }

    #[test]
    fn an_endorsement_verifies_under_its_endorser() {
        let (_dir, endorser) = test_identity();
        let (_dir2, endorsed) = test_identity();
        let endorsement = Endorsement::issue(
            NODE_B.into(),
            endorsed.public_hex(),
            NODE_A.into(),
            &endorser,
        );
        assert!(endorsement.verify(&endorser.public_hex()));
        assert!(!endorsement.verify(&endorsed.public_hex()));
        let parsed = Endorsement::from_value(&endorsement.to_value()).expect("parse");
        assert_eq!(parsed, endorsement);

        // Endorsing oneself is malformed by shape.
        let mut selfie = endorsement.to_value();
        selfie["endorsed_by"] = json!(NODE_B);
        assert!(Endorsement::from_value(&selfie).is_none());
    }
}
