// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

//! The wire dialect (doc/sync-engine.md, section 4): every control message
//! is a `peers.send` payload, self-describing and idempotent - a lost
//! message costs a round, never correctness. Every outgoing frame is
//! BYTE-budgeted at serialization time; records and entries page.
//!
//! A third-party engine may hold the same role on a sibling, so every
//! message opens with the dialect marker; an unknown dialect is ignored,
//! which is the replaceability contract working as intended.

use serde_json::{Value, json};

use crate::index::Entry;
use crate::records::{ConflictRecord, Endorsement, Record, SetDescriptor, valid_set_id};
use crate::vv::Vv;

pub const DIALECT: &str = "1device-sync/1";

/// Serialization target per frame: comfortably under the transport's hard
/// 64 KiB cap, with room for the envelope.
pub const PAGE_BUDGET_BYTES: usize = 48 * 1024;

/// Bound on pages a single round may declare: a round is a delta, not a
/// bulk export, and a peer declaring more is trying to park memory here.
pub const ROUND_PAGES_MAX: u64 = 256;

/// One incoming dialect message, strictly parsed. Anything that does not
/// parse is dropped with a log line; the Core's ack is not ours to shape.
#[derive(Clone, Debug, PartialEq)]
pub enum Message {
    /// "Here is where I stand", and the declaration of the pages that
    /// follow it under `round`.
    Head {
        set_id: String,
        round: u64,
        /// The round this head ANSWERS, if any: an answering head is
        /// absorbed and triggers no further answer (termination).
        answers: Option<u64>,
        /// The sender's word about the whole set, or `None` for the
        /// no-membership marker (its channel-authenticated word that it
        /// holds NO state for the set).
        position: Option<HeadPosition>,
        /// The sender's engine public key: with the channel authentication
        /// `peers.send` provides, receiving this IS direct contact - the
        /// pin.
        sync_pub: String,
    },
    /// A page of membership and conflict records (the gossip).
    Records {
        set_id: String,
        round: u64,
        page: u64,
        records: Vec<Record>,
        endorsements: Vec<Endorsement>,
        conflicts: Vec<ConflictRecord>,
    },
    /// A page of index entries: the delta the peer's head showed it lacks.
    Entries {
        set_id: String,
        round: u64,
        page: u64,
        entries: Vec<Entry>,
    },
    /// The confirmed one-shot: descriptor, roster records, the inviter's
    /// claim about size.
    Invite {
        set_id: String,
        descriptor: SetDescriptor,
        records: Vec<Record>,
        endorsements: Vec<Endorsement>,
        stats_entries: u64,
        stats_total_size: u64,
        sync_pub: String,
    },
    /// The invitee's receipt: the invitation is persisted, stop retrying.
    InviteAck { set_id: String },
    /// "Publish these for me": wire paths of FILE entries the needer wants
    /// the bytes of. Chunked so no two paths share a basename (the
    /// published record names files by basename: the bijection).
    Need {
        set_id: String,
        need_id: u64,
        paths: Vec<String>,
    },
    /// "Adopt this and pull": the published transaction and the explicit
    /// basename-to-set-path map.
    Offer {
        set_id: String,
        need_id: u64,
        tx_id: String,
        files: Vec<(String, String)>,
    },
    /// "I have landed everything; you may revoke."
    Done {
        set_id: String,
        need_id: u64,
        tx_id: String,
    },
}

/// A head's declared position: descriptor hash, the advertised set_vv, and
/// the page counts that follow.
#[derive(Clone, Debug, PartialEq)]
pub struct HeadPosition {
    pub descriptor_hash: String,
    pub set_vv: Vv,
    pub records_pages: u64,
    pub entries_pages: u64,
    /// Whether the declared entries pages CONTAIN the full delta against
    /// the receiver's advertised vv. A paused device's answer and an
    /// introduction say `false` (no delta computed), and so does a
    /// round-opening head: only a complete leg may advance a watermark -
    /// "records-only advances nothing" needs the difference between an
    /// empty delta and an uncomputed one to be on the wire.
    pub entries_complete: bool,
}

impl Message {
    pub fn set_id(&self) -> &str {
        match self {
            Message::Head { set_id, .. }
            | Message::Records { set_id, .. }
            | Message::Entries { set_id, .. }
            | Message::Invite { set_id, .. }
            | Message::InviteAck { set_id }
            | Message::Need { set_id, .. }
            | Message::Offer { set_id, .. }
            | Message::Done { set_id, .. } => set_id,
        }
    }

    pub fn to_value(&self) -> Value {
        match self {
            Message::Head {
                set_id,
                round,
                answers,
                position,
                sync_pub,
            } => {
                let mut v = json!({
                    "dialect": DIALECT,
                    "type": "head",
                    "set_id": set_id,
                    "round": round,
                    "sync_pub": sync_pub,
                });
                if let Some(answers) = answers {
                    v["answers"] = json!(answers);
                }
                match position {
                    Some(p) => {
                        v["descriptor_hash"] = json!(p.descriptor_hash);
                        v["set_vv"] = p.set_vv.to_value();
                        v["records_pages"] = json!(p.records_pages);
                        v["entries_pages"] = json!(p.entries_pages);
                        v["entries_complete"] = json!(p.entries_complete);
                    }
                    None => {
                        v["membership"] = json!("none");
                    }
                }
                v
            }
            Message::Records {
                set_id,
                round,
                page,
                records,
                endorsements,
                conflicts,
            } => json!({
                "dialect": DIALECT,
                "type": "records",
                "set_id": set_id,
                "round": round,
                "page": page,
                "records": records.iter().map(Record::to_value).collect::<Vec<_>>(),
                "endorsements": endorsements.iter().map(Endorsement::to_value).collect::<Vec<_>>(),
                "conflicts": conflicts.iter().map(ConflictRecord::to_value).collect::<Vec<_>>(),
            }),
            Message::Entries {
                set_id,
                round,
                page,
                entries,
            } => json!({
                "dialect": DIALECT,
                "type": "entries",
                "set_id": set_id,
                "round": round,
                "page": page,
                "entries": entries.iter().map(Entry::to_value).collect::<Vec<_>>(),
            }),
            Message::Invite {
                set_id,
                descriptor,
                records,
                endorsements,
                stats_entries,
                stats_total_size,
                sync_pub,
            } => json!({
                "dialect": DIALECT,
                "type": "invite",
                "set_id": set_id,
                "descriptor": descriptor.to_value(),
                "records": records.iter().map(Record::to_value).collect::<Vec<_>>(),
                "endorsements": endorsements.iter().map(Endorsement::to_value).collect::<Vec<_>>(),
                "stats": { "entries": stats_entries, "total_size": stats_total_size },
                "sync_pub": sync_pub,
            }),
            Message::InviteAck { set_id } => json!({
                "dialect": DIALECT,
                "type": "invite_ack",
                "set_id": set_id,
            }),
            Message::Need {
                set_id,
                need_id,
                paths,
            } => json!({
                "dialect": DIALECT,
                "type": "need",
                "set_id": set_id,
                "need_id": need_id,
                "paths": paths,
            }),
            Message::Offer {
                set_id,
                need_id,
                tx_id,
                files,
            } => json!({
                "dialect": DIALECT,
                "type": "offer",
                "set_id": set_id,
                "need_id": need_id,
                "tx_id": tx_id,
                "files": files
                    .iter()
                    .map(|(wire, set_path)| json!({ "wire_path": wire, "set_path": set_path }))
                    .collect::<Vec<_>>(),
            }),
            Message::Done {
                set_id,
                need_id,
                tx_id,
            } => json!({
                "dialect": DIALECT,
                "type": "done",
                "set_id": set_id,
                "need_id": need_id,
                "tx_id": tx_id,
            }),
        }
    }

    /// Strict parse of one incoming payload. `None` covers BOTH the foreign
    /// dialect (ignored by design) and the malformed (dropped): the caller
    /// logs the difference if it cares, nothing else distinguishes them.
    pub fn from_value(value: &Value) -> Option<Message> {
        if value.get("dialect").and_then(Value::as_str) != Some(DIALECT) {
            return None;
        }
        let set_id = value.get("set_id").and_then(Value::as_str)?;
        if !valid_set_id(set_id) {
            return None;
        }
        let set_id = set_id.to_string();
        let int = |field: &str| value.get(field).and_then(Value::as_u64);
        match value.get("type").and_then(Value::as_str)? {
            "head" => {
                let sync_pub = value.get("sync_pub").and_then(Value::as_str)?;
                if !crate::records::valid_key(sync_pub) {
                    return None;
                }
                let position = if value.get("membership").and_then(Value::as_str) == Some("none") {
                    None
                } else {
                    let hash = value.get("descriptor_hash").and_then(Value::as_str)?;
                    if hash.len() != 64 {
                        return None;
                    }
                    let records_pages = int("records_pages")?;
                    let entries_pages = int("entries_pages")?;
                    if records_pages > ROUND_PAGES_MAX || entries_pages > ROUND_PAGES_MAX {
                        return None;
                    }
                    Some(HeadPosition {
                        descriptor_hash: hash.to_string(),
                        set_vv: Vv::from_value(value.get("set_vv")?)?,
                        records_pages,
                        entries_pages,
                        entries_complete: value.get("entries_complete")?.as_bool()?,
                    })
                };
                Some(Message::Head {
                    set_id,
                    round: int("round")?,
                    answers: match value.get("answers") {
                        None => None,
                        Some(v) => Some(v.as_u64()?),
                    },
                    position,
                    sync_pub: sync_pub.to_string(),
                })
            }
            "records" => Some(Message::Records {
                set_id,
                round: int("round")?,
                page: int("page")?,
                records: parse_all(value.get("records")?, Record::from_value)?,
                endorsements: parse_all(value.get("endorsements")?, Endorsement::from_value)?,
                conflicts: parse_all(value.get("conflicts")?, ConflictRecord::from_value)?,
            }),
            "entries" => Some(Message::Entries {
                set_id,
                round: int("round")?,
                page: int("page")?,
                entries: parse_all(value.get("entries")?, Entry::from_value)?,
            }),
            "invite" => {
                let stats = value.get("stats")?;
                let sync_pub = value.get("sync_pub").and_then(Value::as_str)?;
                if !crate::records::valid_key(sync_pub) {
                    return None;
                }
                let descriptor = SetDescriptor::from_value(value.get("descriptor")?)?;
                if descriptor.set_id != set_id {
                    return None;
                }
                Some(Message::Invite {
                    set_id,
                    descriptor,
                    records: parse_all(value.get("records")?, Record::from_value)?,
                    endorsements: parse_all(value.get("endorsements")?, Endorsement::from_value)?,
                    stats_entries: stats.get("entries").and_then(Value::as_u64)?,
                    stats_total_size: stats.get("total_size").and_then(Value::as_u64)?,
                    sync_pub: sync_pub.to_string(),
                })
            }
            "invite_ack" => Some(Message::InviteAck { set_id }),
            "need" => {
                let paths = parse_all(value.get("paths")?, |v| {
                    let path = v.as_str()?;
                    crate::wirepath::check_wire_path(path).ok()?;
                    Some(path.to_string())
                })?;
                // The bijection rule, enforced at the gate: one basename,
                // one path.
                let mut basenames = std::collections::BTreeSet::new();
                for path in &paths {
                    if !basenames.insert(basename(path)) {
                        return None;
                    }
                }
                Some(Message::Need {
                    set_id,
                    need_id: int("need_id")?,
                    paths,
                })
            }
            "offer" => {
                let tx_id = value.get("tx_id").and_then(Value::as_str)?;
                if tx_id.is_empty() || tx_id.len() > 128 {
                    return None;
                }
                Some(Message::Offer {
                    set_id,
                    need_id: int("need_id")?,
                    tx_id: tx_id.to_string(),
                    files: parse_all(value.get("files")?, |v| {
                        let wire = v.get("wire_path")?.as_str()?;
                        let set_path = v.get("set_path")?.as_str()?;
                        crate::wirepath::check_wire_path(wire).ok()?;
                        crate::wirepath::check_wire_path(set_path).ok()?;
                        Some((wire.to_string(), set_path.to_string()))
                    })?,
                })
            }
            "done" => {
                let tx_id = value.get("tx_id").and_then(Value::as_str)?;
                if tx_id.is_empty() || tx_id.len() > 128 {
                    return None;
                }
                Some(Message::Done {
                    set_id,
                    need_id: int("need_id")?,
                    tx_id: tx_id.to_string(),
                })
            }
            _ => None,
        }
    }
}

/// The last component of a wire path (already gate-checked: non-empty).
pub fn basename(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// Parses every element or none: ONE bad item drops the whole page, loudly
/// at the caller (fail-closed, like the Core's manifest rule).
fn parse_all<T>(value: &Value, parse: impl Fn(&Value) -> Option<T>) -> Option<Vec<T>> {
    let items = value.as_array()?;
    if items.len() > 4096 {
        return None;
    }
    items.iter().map(parse).collect()
}

/// Splits `items` into pages whose SERIALIZED size stays under the budget:
/// measured, never a count heuristic. An item that alone exceeds the budget
/// is refused (the caller decides what that means; no current record or
/// entry can, given the field bounds).
pub fn paginate(items: &[Value]) -> Option<Vec<Vec<Value>>> {
    let mut pages: Vec<Vec<Value>> = Vec::new();
    let mut current: Vec<Value> = Vec::new();
    let mut current_bytes = 0usize;
    for item in items {
        let bytes = item.to_string().len() + 1;
        if bytes > PAGE_BUDGET_BYTES {
            return None;
        }
        if current_bytes + bytes > PAGE_BUDGET_BYTES && !current.is_empty() {
            pages.push(std::mem::take(&mut current));
            current_bytes = 0;
        }
        current_bytes += bytes;
        current.push(item.clone());
    }
    if !current.is_empty() {
        pages.push(current);
    }
    Some(pages)
}

/// The descriptor hash a head declares: BLAKE3 of the descriptor's wire
/// form. Two devices talking about different descriptors under one set_id
/// must notice before absorbing anything.
pub fn descriptor_hash(descriptor: &SetDescriptor) -> String {
    blake3::hash(descriptor.to_value().to_string().as_bytes())
        .to_hex()
        .to_string()
}

#[cfg(test)]
mod tests {
    use crate::identity::Identity;
    use crate::records::{MemberRecord, MemberStatus, SetKind};

    use super::*;

    const SET: &str = "AAAAAAAAAAAAAAAAAAAAAA";
    const NODE_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    fn identity() -> (tempfile::TempDir, Identity) {
        let dir = tempfile::tempdir().expect("tempdir");
        let identity = Identity::load_or_generate(dir.path()).expect("identity");
        (dir, identity)
    }

    #[test]
    fn messages_round_trip() {
        let (_d, id) = identity();
        let descriptor = SetDescriptor::create(
            SET.into(),
            SetKind::Dir,
            "Projects".into(),
            NODE_A.into(),
            1000,
            &id,
        );
        let record = Record::Member(MemberRecord::sign_own(
            SET.into(),
            NODE_A.into(),
            MemberStatus::Active,
            1,
            1,
            1000,
            &id,
        ));
        let mut vv = Vv::new();
        vv.set(&crate::vv::component(NODE_A, 1), 3);
        let messages = [
            Message::Head {
                set_id: SET.into(),
                round: 7,
                answers: Some(3),
                position: Some(HeadPosition {
                    descriptor_hash: descriptor_hash(&descriptor),
                    set_vv: vv,
                    records_pages: 1,
                    entries_pages: 0,
                    entries_complete: true,
                }),
                sync_pub: id.public_hex(),
            },
            Message::Head {
                set_id: SET.into(),
                round: 8,
                answers: None,
                position: None,
                sync_pub: id.public_hex(),
            },
            Message::Records {
                set_id: SET.into(),
                round: 7,
                page: 0,
                records: vec![record],
                endorsements: vec![],
                conflicts: vec![],
            },
            Message::Invite {
                set_id: SET.into(),
                descriptor,
                records: vec![],
                endorsements: vec![],
                stats_entries: 12,
                stats_total_size: 34_000,
                sync_pub: id.public_hex(),
            },
            Message::InviteAck { set_id: SET.into() },
        ];
        for m in messages {
            let parsed = Message::from_value(&m.to_value());
            assert_eq!(parsed, Some(m));
        }
    }

    #[test]
    fn foreign_dialects_and_malformed_frames_are_ignored() {
        for bad in [
            json!({ "type": "head" }),
            json!({ "dialect": "other-sync/9", "type": "head", "set_id": SET }),
            json!({ "dialect": DIALECT, "type": "warp", "set_id": SET }),
            json!({ "dialect": DIALECT, "type": "head", "set_id": "../.." }),
            json!({ "dialect": DIALECT, "type": "invite_ack" }),
        ] {
            assert_eq!(Message::from_value(&bad), None, "should ignore {bad}");
        }
    }

    #[test]
    fn one_bad_item_drops_the_whole_page() {
        let (_d, id) = identity();
        let good = MemberRecord::sign_own(
            SET.into(),
            NODE_A.into(),
            MemberStatus::Active,
            1,
            1,
            1000,
            &id,
        )
        .to_value();
        let value = json!({
            "dialect": DIALECT,
            "type": "records",
            "set_id": SET,
            "round": 1,
            "page": 0,
            "records": [good, { "garbage": true }],
            "endorsements": [],
            "conflicts": [],
        });
        assert_eq!(Message::from_value(&value), None);
    }

    #[test]
    fn a_head_declaring_absurd_page_counts_is_refused() {
        let value = json!({
            "dialect": DIALECT,
            "type": "head",
            "set_id": SET,
            "round": 1,
            "sync_pub": "a".repeat(64),
            "descriptor_hash": "b".repeat(64),
            "set_vv": {},
            "records_pages": ROUND_PAGES_MAX + 1,
            "entries_pages": 0,
            "entries_complete": true,
        });
        assert_eq!(Message::from_value(&value), None);
    }

    #[test]
    fn pagination_is_measured_in_bytes() {
        let big = json!({ "x": "y".repeat(20_000) });
        let pages = paginate(&[big.clone(), big.clone(), big.clone()]).expect("pages");
        assert_eq!(pages.len(), 2, "three 20 KB items under a 48 KiB budget");
        assert_eq!(pages[0].len(), 2);
        assert_eq!(pages[1].len(), 1);
        let oversized = json!({ "x": "y".repeat(PAGE_BUDGET_BYTES) });
        assert!(paginate(&[oversized]).is_none());
        assert!(paginate(&[]).expect("pages").is_empty());
    }
}
