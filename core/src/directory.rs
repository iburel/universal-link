// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

//! The account's directory as persisted: the device records (attestations
//! included), so a Core that starts with no reachable server still recognizes
//! its siblings. Without it, the LAN route (`dataplane::peer_reachable`) is
//! useless precisely when it matters most — the server unreachable and two
//! machines in the same room.
//!
//! It can never MINT trust: every use of a record re-verifies its attestation
//! against the account key from the local keyring (C7), exactly as for a live
//! snapshot — a tampered or injected file grants nothing, just like a lying
//! server. What it CAN do is PROLONG trust: a device revoked while this one was
//! offline keeps its stored attestation until the next server contact.
//! `CACHE_TTL` bounds that window — a snapshot past it no longer vouches and is
//! ignored at load, so a machine that boots after weeks in a drawer starts
//! fail-closed, exactly as before this file existed. (Load-time only,
//! deliberately: a Core that stays RUNNING keeps its in-memory map across server
//! outages, as it always has.)
//!
//! **That bound assumes an authority to be stale with respect to.** A Core with
//! no server configured has none: nothing will ever refresh what it holds, so
//! expiring it would not fail closed, it would erase the account — the file IS
//! the directory there, not a cache of one. So `expires` is the caller's answer
//! to "is there a server that could refresh this?", and a store that outlives
//! its window without one keeps vouching.
//!
//! What takes the TTL's place there is a **tombstone**: the account key's
//! signature striking a `node_id` off ([`account_key::revoke`]), kept in
//! `revoked.json`. Permanent, so it needs no freshness; verifiable by every
//! device, so it needs no authority to carry it. And a file of its own, not the
//! store's: a session ends and takes the directory cache with it, an account's
//! revocation outlives both (the struck-off device keeps a valid attestation
//! forever — the tombstone is the only thing that keeps it out).
//!
//! **A record signs itself.** A device is the authority on its own description:
//! it signs `{node_id, name, platform, seq, addrs, relay_hint}` with the very
//! key its `node_id` IS ([`record_message`]), so no one who relays that record
//! can rewrite the name (or plant a route), and `seq`, which only its owner can
//! raise, is what makes one description supersede another. Deliberately NOT
//! signed: `relay_url`, `online`, `last_seen`, `status`. Those are said in the
//! present tense, and a peer states them first-hand over an authenticated stream
//! (or a server states them, and there the server is the authority anyway); a
//! signature over them would be a stale fact wearing a proof. The reach hints
//! ([`Reach`]) are the line between the two: they claim no liveness, only "these
//! are MY hints, as of this description", a claim only a signature can carry
//! across a relayer, and whose staleness costs one failed dial, never a wrong
//! peer.
//!
//! A record the SERVER minted carries no such signature, and that is not a
//! defect: with a server in the picture the server owns the name
//! (`devices.rename` can come from another device). The self-signature is what
//! the serverless half has instead.
//!
//! **What travels between devices.** A [`Roster`] is a directory as offered to a
//! peer: the records it could verify on its own, plus the tombstones it could
//! verify on its own — and nothing else. Both halves are filtered by the SAME
//! predicate on the way out and on the way in ([`shareable`]), which is what
//! makes relaying safe: whoever hands us a roster is never trusted with its
//! contents, only with the trouble of carrying it. So a device learns of a
//! sibling it has never met, from a third one that has — and that is
//! [`crate::dirsync`], whose module header holds the protocol.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};

/// How long a persisted snapshot vouches. This is the revocation-staleness
/// bound: the longest a struck-off device can keep being served by a sibling
/// that restarts without ever reaching the server.
const CACHE_TTL: Duration = Duration::from_secs(7 * 24 * 60 * 60);

const FILE: &str = "directory.json";

/// The account's tombstones: `node_id` → the account key's signature striking it
/// off. Its own file, and one no logout removes — see the module header.
const REVOKED_FILE: &str = "revoked.json";

/// Domain separation (and version) for a device's signature over its own
/// description. Distinct from the account key's domains: this one is a DEVICE
/// saying what it is, not the ACCOUNT saying who belongs.
///
/// `v2`: the description grew the device's reachability hints (`addrs`,
/// `relay_hint`). Bumped while no released build had ever signed a `v1`
/// record (v0.6.0 predates the directory record entirely), so nothing in the
/// field stops verifying: the version moved so that the two framings can
/// never be mistaken for each other, not to migrate anyone.
const RECORD_DOMAIN: &[u8] = b"1device-dir-record-v2:";

/// Address hints a record may carry, at most. Every hint is one dial attempt
/// a sibling may waste on it, and one line every device of the account
/// persists. Real machines DO exceed it (iroh has seen macOS hosts with more
/// than 25 interfaces: VPN TUNs, Docker bridges), which is why the device's
/// own claim is clamped to this bound before signing
/// (`dataplane::claimed_reach`) rather than trusted to fit.
pub(crate) const MAX_ADDR_HINTS: usize = 16;

/// One address hint, at most, in bytes. A socket address in text form,
/// `[IPv6%zone]:port` included, fits well under this.
pub(crate) const MAX_ADDR_LEN: usize = 64;

/// A relay hint, at most, in bytes. Mirrors the server's own cap on a
/// published `relay_url` (`RELAY_URL_MAX`).
pub(crate) const MAX_RELAY_HINT_LEN: usize = 2048;

/// Where a device says it can be dialed, first-hand: the socket addresses its
/// endpoint is bound on (LAN, VPN, public IPv6; as text, `ip:port`), and the
/// relay somebody chose for it, if any (a configured URL, or the home relay
/// an explicit n0 opt-in elected, #104). Part of the SIGNED
/// description since v2: in a serverless account there is no authority to
/// state these facts first-hand before the first dial, so the device's own
/// signature, ordered by `seq`, is what keeps a relayer from rewriting them.
/// A stale or poisoned hint costs a failed dial attempt, never a
/// man-in-the-middle: connections are authenticated by the node key.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Reach {
    pub addrs: Vec<String>,
    pub relay_hint: Option<String>,
}

impl Reach {
    pub fn is_empty(&self) -> bool {
        self.addrs.is_empty() && self.relay_hint.is_none()
    }
}

/// The reach a record carries, read fail-closed: absent fields are an empty
/// claim (a record from before the fields existed, or a device with nothing
/// to say), but a field that IS there and does not have the published shape
/// (`addrs` not an array of strings, an entry too long, too many of them, a
/// `relay_hint` that is not a string) is `None`, and a caller treating that
/// as "no reach" would verify a signature over bytes the record does not
/// carry. Callers refuse the record instead.
pub(crate) fn record_reach(record: &Value) -> Option<Reach> {
    let addrs = match record.get("addrs") {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Array(entries)) if entries.len() <= MAX_ADDR_HINTS => entries
            .iter()
            .map(|entry| match entry.as_str() {
                Some(addr) if addr.len() <= MAX_ADDR_LEN => Some(addr.to_string()),
                _ => None,
            })
            .collect::<Option<Vec<String>>>()?,
        Some(_) => return None,
    };
    let relay_hint = match record.get("relay_hint") {
        None | Some(Value::Null) => None,
        Some(Value::String(url)) if url.len() <= MAX_RELAY_HINT_LEN && !url.is_empty() => {
            Some(url.clone())
        }
        Some(_) => return None,
    };
    Some(Reach { addrs, relay_hint })
}

/// Writes `reach` onto the record, in the shape [`record_reach`] reads back:
/// `addrs` always present (an empty array is an empty claim said out loud),
/// `relay_hint` present-and-null when there is none, like `relay_url`. Said
/// explicitly so that a superseding description can never leave a stale hint
/// behind by merely omitting the field.
pub(crate) fn set_reach(record: &mut Value, reach: &Reach) {
    record["addrs"] = json!(reach.addrs);
    record["relay_hint"] = match &reach.relay_hint {
        Some(url) => json!(url),
        None => Value::Null,
    };
}

/// Persists the map — called under the session lock at every mutation (the
/// same state-then-disk discipline as `session.json` at login), so the file
/// is never newer than the memory it mirrors. Failure is not fatal: the Core
/// just loses its head start at the next offline startup — said out loud,
/// because a silently missing cache would look like a bug exactly when the
/// user has no network to debug with.
pub(crate) fn save(config_dir: &Path, devices: &BTreeMap<String, Value>) {
    write_store(config_dir, devices, now_secs());
}

/// Persists the records WITHOUT moving `saved_at`: they changed, but nothing
/// re-checked them against the authority the freshness bound is about.
///
/// A roster from a sibling ([`crate::dirsync`]) is exactly that case, and the
/// distinction is not academic. It carries the account's tombstones, which is a
/// revocation channel — but not a server-side `devices.revoke`, which mints no
/// tombstone at all. Treating the exchange as a refresh would therefore extend,
/// quietly and indefinitely, the window in which a device the SERVER revoked keeps
/// being vouched for by a Core that has not reached that server in weeks. Two
/// devices left talking to each other in a room must not be able to hold the bound
/// open between them.
///
/// Where there is no server the timestamp means nothing either way (`load` does
/// not expire the store), so this costs that case nothing.
pub(crate) fn save_unrefreshed(config_dir: &Path, devices: &BTreeMap<String, Value>) {
    let kept = std::fs::read_to_string(config_dir.join(FILE))
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .and_then(|value| value.get("saved_at")?.as_u64());
    write_store(config_dir, devices, kept.unwrap_or_else(now_secs));
}

fn write_store(config_dir: &Path, devices: &BTreeMap<String, Value>, saved_at: u64) {
    let payload = json!({ "saved_at": saved_at, "devices": devices });
    if let Err(e) = crate::write_private_file(&config_dir.join(FILE), &payload.to_string()) {
        tracing::warn!(error = %e, "failed to persist the directory cache");
    }
}

/// The stored records, if the file exists, parses, and — when `expires` — is
/// fresh. Anything else — absent, corrupt, stale — is `None`: the Core starts
/// fail-closed, exactly as before this file existed. (A `saved_at` in the future
/// reads as age zero — the file cannot mint trust either way, and a machine
/// whose clock walks backward defeats any bound we could write here.)
///
/// `expires`: is there a server that could refresh this? See the module header —
/// without one there is nothing for the store to be stale with respect to.
pub(crate) fn load(config_dir: &Path, expires: bool) -> Option<BTreeMap<String, Value>> {
    let path = config_dir.join(FILE);
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
        Err(e) => {
            tracing::warn!(error = %e, "unreadable directory cache: ignored");
            return None;
        }
    };
    let value: Value = match serde_json::from_str(&text) {
        Ok(value) => value,
        Err(e) => {
            tracing::warn!(error = %e, "corrupt directory cache: ignored");
            return None;
        }
    };
    let Some(saved_at) = value.get("saved_at").and_then(Value::as_u64) else {
        tracing::warn!("directory cache without a timestamp: ignored");
        return None;
    };
    let age = now_secs().saturating_sub(saved_at);
    if expires && age > CACHE_TTL.as_secs() {
        tracing::info!(
            days = age / 86_400,
            "directory cache expired: ignored, the next server contact rebuilds it"
        );
        return None;
    }
    let devices = value.get("devices")?.as_object()?;
    Some(
        devices
            .iter()
            .map(|(id, record)| (id.clone(), record.clone()))
            .collect(),
    )
}

/// Removes the cache — logout, revocation: the Core no longer acts for the
/// account, so it forgets whom the account trusted. Same fallback as
/// `session.json`: if the deletion fails, emptying has the same effect (an
/// empty file does not parse, so `load` ignores it).
pub(crate) fn remove(config_dir: &Path) {
    let path = config_dir.join(FILE);
    match std::fs::remove_file(&path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => {
            tracing::error!(error = %e, "failed to remove the directory cache: contents emptied instead");
            if let Err(e) = crate::write_private_file(&path, "") {
                tracing::error!(error = %e, "failed to erase the directory cache");
            }
        }
    }
}

/// Removes the tombstones — the one caller is a device LEAVING the account
/// (`account_key::leave`): the tombstones were the account's to keep, and a
/// device out of the account keeps nothing of its. Same fallback as the cache;
/// an empty file loads as no tombstones, which is what a device with no trust
/// root effectively holds anyway (nothing verifies without the account key).
pub(crate) fn remove_revoked(config_dir: &Path) {
    let path = config_dir.join(REVOKED_FILE);
    match std::fs::remove_file(&path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => {
            tracing::error!(error = %e, "failed to remove the revocations: contents emptied instead");
            if let Err(e) = crate::write_private_file(&path, "") {
                tracing::error!(error = %e, "failed to erase the revocations");
            }
        }
    }
}

/// The stored tombstones. An absent, corrupt or unreadable file
/// reads as none: a tombstone bars a device only once its signature has been
/// checked, so a missing file costs nothing here — it is the callers' checks that
/// keep the account safe, and a garbled one cannot be silently trusted.
pub(crate) fn load_revoked(config_dir: &Path) -> BTreeMap<String, String> {
    let path = config_dir.join(REVOKED_FILE);
    let Ok(text) = std::fs::read_to_string(&path) else {
        return BTreeMap::new();
    };
    let parsed = serde_json::from_str::<Value>(&text)
        .ok()
        .and_then(|v| v.get("revoked")?.as_object().cloned());
    let Some(revoked) = parsed else {
        tracing::warn!("unreadable revoked.json: no device is barred by it");
        return BTreeMap::new();
    };
    revoked
        .iter()
        .filter_map(|(node_id, sig)| Some((node_id.clone(), sig.as_str()?.to_string())))
        .collect()
}

/// Persists the tombstones. A failure is loud: this file is what keeps a
/// struck-off device out at the next startup, and losing it silently would take
/// that device back into the account.
pub(crate) fn save_revoked(config_dir: &Path, revoked: &BTreeMap<String, String>) {
    let payload = json!({ "revoked": revoked });
    if let Err(e) = crate::write_private_file(&config_dir.join(REVOKED_FILE), &payload.to_string())
    {
        tracing::error!(error = %e, "failed to persist the account's revocations");
    }
}

/// The bytes a device signs to stand behind its own description. Every field is
/// length-prefixed: `name` is arbitrary text the user chose, and a plain
/// concatenation would let two different descriptions produce one message
/// (`"Ada" + "|Bo"` against `"Ada|Bo"`). The address list is additionally
/// count-prefixed, so its boundary with the relay hint cannot be forged
/// either; an absent relay hint signs as the empty string, which no real
/// relay URL can be.
///
/// Public because the same encoding has to be reproducible outside the Core — a
/// test harness standing in for a peer signs exactly these bytes.
pub fn record_message(
    node_id: &str,
    name: &str,
    platform: &str,
    seq: u64,
    reach: &Reach,
) -> Vec<u8> {
    fn push(msg: &mut Vec<u8>, field: &str) {
        msg.extend_from_slice(&(field.len() as u64).to_be_bytes());
        msg.extend_from_slice(field.as_bytes());
    }
    let mut msg = RECORD_DOMAIN.to_vec();
    msg.extend_from_slice(&seq.to_be_bytes());
    for field in [node_id, name, platform] {
        push(&mut msg, field);
    }
    msg.extend_from_slice(&(reach.addrs.len() as u64).to_be_bytes());
    for addr in &reach.addrs {
        push(&mut msg, addr);
    }
    push(&mut msg, reach.relay_hint.as_deref().unwrap_or(""));
    msg
}

/// Does this record carry the signature of the very device it describes? The key
/// that verifies it is the `node_id` itself, so this needs no directory, no
/// server and no account key to answer — which is what lets a description travel
/// from device to device without the relaying one being trusted with it.
///
/// It says nothing about MEMBERSHIP: that is the attestation's job
/// (`account_key::verify`), and both are required of a record from a peer. Any
/// defect — a missing field, an unparseable signature, a `seq` that is not a
/// number — answers `false`, so a caller cannot mistake "not signed" for
/// "signed by someone".
pub fn verify_record(record: &Value) -> bool {
    let field = |key: &str| record.get(key).and_then(Value::as_str);
    let (Some(node_id), Some(name), Some(platform), Some(sig)) = (
        field("node_id"),
        field("name"),
        field("platform"),
        field("self_sig"),
    ) else {
        return false;
    };
    let Some(seq) = record.get("seq").and_then(Value::as_u64) else {
        return false;
    };
    // A reach that does not have the published shape is not "no reach": the
    // signature would then be checked over bytes the record does not carry.
    let Some(reach) = record_reach(record) else {
        return false;
    };
    crate::account_key::verify_detached(
        node_id,
        &record_message(node_id, name, platform, seq, &reach),
        sig,
    )
}

/// Entries a peer may hand us in one roster, records and tombstones together.
/// The frame is already bounded (`dataplane::MAX_FRAME`); this bounds the WORK
/// instead — every entry costs a signature verification, and two per record. Far
/// above any real account, so it only ever bites a peer that is not describing an
/// account.
const MAX_ROSTER: usize = 512;

/// The fields a peer's roster is allowed to carry onto a record we ALREADY hold:
/// the signed description (reach hints included, since v2) and the
/// attestation that came verified alongside it.
///
/// What is left out is left out on purpose. `device_id` is our own key for the
/// record and is not covered by any signature — taking a peer's would let
/// whoever relays a record re-label another device's entry. `relay_url`,
/// `online`, `last_seen` and `status` are said in the present tense about a
/// device the relayer is not: the record would arrive claiming a route and a
/// liveness nobody could check, and a route that fails is worse than no route
/// (it is one the user is told exists). What a peer says about ITSELF, first-hand,
/// still reaches us the same way it always has — over its own authenticated
/// stream, or from a server that owns those facts.
///
/// `addrs` and `relay_hint` are the deliberate exception to that rule, and what
/// makes them safe is exactly what the present-tense fields lack: they are
/// covered by the device's own signature, ordered by its `seq`. They claim no
/// liveness, only "these are MY hints, as of this description", and the worst
/// a stale one costs is a failed dial attempt, never a connection to the wrong
/// key.
const CARRIED: [&str; 7] = [
    "name",
    "platform",
    "seq",
    "self_sig",
    "attestation",
    "addrs",
    "relay_hint",
];

/// A directory as handed to a peer: the records and the tombstones, filtered down
/// to what that peer can verify with the account key alone. Deliberately not a
/// map — the receiver keys records itself, by `node_id`, since that is the only
/// label a relayer cannot rewrite.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Roster {
    pub records: Vec<Value>,
    pub revoked: BTreeMap<String, String>,
}

impl Roster {
    /// What this Core has to offer: every record it holds that a peer could
    /// verify, and every tombstone it holds that a peer could verify. Filtered on
    /// the way OUT as well as on the way in, because sending what the other side
    /// will refuse is not generosity — it is a frame that says nothing and a log
    /// line on the far end blaming us for it.
    pub fn of(
        devices: Option<&BTreeMap<String, Value>>,
        revoked: &BTreeMap<String, String>,
        ak_pub: &str,
    ) -> Roster {
        let records: Vec<Value> = devices
            .into_iter()
            .flat_map(|devices| devices.values())
            .filter(|record| shareable(record, ak_pub))
            .cloned()
            .collect();
        let revoked: BTreeMap<String, String> = revoked
            .iter()
            .filter(|(node_id, revocation)| {
                crate::account_key::verify_revocation(ak_pub, node_id, revocation)
            })
            .map(|(node_id, revocation)| (node_id.clone(), revocation.clone()))
            .collect();
        if records.len() + revoked.len() > MAX_ROSTER {
            tracing::warn!(
                records = records.len(),
                revoked = revoked.len(),
                "our directory is larger than a roster may carry: peers will refuse it whole"
            );
        }
        Roster { records, revoked }
    }

    /// Nothing to say. A Core with no trust root produces one of these, and never
    /// opens a stream for it.
    pub fn is_empty(&self) -> bool {
        self.records.is_empty() && self.revoked.is_empty()
    }

    /// The roster as a frame body (the caller adds the `type`).
    pub fn payload(&self) -> Value {
        json!({ "records": self.records, "revoked": self.revoked })
    }

    /// Reads a roster off the wire. `revoked` may be absent (a peer with nothing
    /// struck off) — but present and NOT an object refuses the whole frame rather
    /// than reading as none: quietly ignoring a garbled tombstone set is exactly
    /// how a struck-off device would walk back into the account.
    ///
    /// The individual entries are not checked here: they are checked where they
    /// are absorbed, against the account key, which this module does not hold.
    pub fn parse(value: &Value) -> Option<Roster> {
        let records = value.get("records")?.as_array()?;
        let revoked = match value.get("revoked") {
            None | Some(Value::Null) => serde_json::Map::new(),
            Some(Value::Object(map)) => map.clone(),
            Some(_) => {
                tracing::warn!("roster whose revocations are not an object: refused");
                return None;
            }
        };
        if records.len() + revoked.len() > MAX_ROSTER {
            tracing::warn!(
                records = records.len(),
                revoked = revoked.len(),
                "roster larger than {MAX_ROSTER} entries: refused"
            );
            return None;
        }
        Some(Roster {
            records: records.clone(),
            revoked: revoked
                .iter()
                .filter_map(|(node_id, revocation)| {
                    Some((node_id.clone(), revocation.as_str()?.to_string()))
                })
                .collect(),
        })
    }
}

/// Can this record travel between devices? It has to say two things on its own,
/// because the device that relays it vouches for neither: WHO it describes (its
/// own signature over its description, verified against the `node_id` itself) and
/// that the account admitted that `node_id` (an attestation under the account
/// key). A record missing either is not a lesser record, it is one that proves
/// nothing — a server's record among them, which is why a directory learned from
/// a server does not fan out over the data plane.
pub(crate) fn shareable(record: &Value, ak_pub: &str) -> bool {
    let Some(node_id) = record.get("node_id").and_then(Value::as_str) else {
        return false;
    };
    verify_record(record)
        && record
            .get("attestation")
            .and_then(Value::as_str)
            .is_some_and(|attestation| crate::account_key::verify(ak_pub, node_id, attestation))
}

/// Copies onto `held` what an incoming record is allowed to carry (see
/// [`CARRIED`]) — the caller has already established that the incoming record is
/// shareable and supersedes the one held.
///
/// A carried field the incoming record does NOT have is removed rather than
/// left: the superseding description is taken whole. Leaving a hold-over would
/// compose a record half of one description and half of another: one whose
/// signature covers neither, so it would verify nowhere and quietly stop
/// traveling.
pub(crate) fn carry_over(held: &mut Value, incoming: &Value) {
    for field in CARRIED {
        match incoming.get(field) {
            Some(value) => held[field] = value.clone(),
            None => {
                if let Some(held) = held.as_object_mut() {
                    held.remove(field);
                }
            }
        }
    }
}

/// Signs, with this device's own key, the description `record` ALREADY carries —
/// the name included, which is the point: where a server names this device, the
/// server keeps the name, and this device countersigns it rather than contest it.
/// The reach hints the record carries are signed the same way: they are this
/// device's own word wherever they came from (its watcher put them there, or a
/// server carried them back). That one signature is what lets its record travel
/// to siblings the server never met ([`shareable`]). The `seq` is minted strictly
/// above whatever the record carried, so the countersigned description supersedes
/// the one it replaces on every device that held it.
///
/// `false` — and the record untouched — when there is nothing here this device
/// could stand behind: a description missing a field, a reach without the
/// published shape, or a `node_id` that is not this device's own (we never sign
/// another device's description; that rule belongs to
/// [`crate::state::SessionState::absorb`] and holds here too).
pub(crate) fn countersign_own(
    record: &mut Value,
    identity: &crate::identity::DeviceIdentity,
) -> bool {
    let field = |key: &str| record.get(key).and_then(Value::as_str).map(str::to_string);
    let (Some(node_id), Some(name), Some(platform)) =
        (field("node_id"), field("name"), field("platform"))
    else {
        return false;
    };
    if node_id != identity.node_id() {
        return false;
    }
    let Some(reach) = record_reach(record) else {
        return false;
    };
    // Re-written in the published shape before signing, so the bytes signed are
    // exactly the bytes a verifier will read back (`addrs` absent and `addrs`
    // empty sign the same, and must therefore be carried the same).
    set_reach(record, &reach);
    let seq = next_seq(record.get("seq").and_then(Value::as_u64));
    record["seq"] = json!(seq);
    record["self_sig"] =
        json!(identity.sign(&record_message(&node_id, &name, &platform, seq, &reach)));
    true
}

/// Carries a signed description this Core already HELD onto the fresh record a
/// server just handed it, or keeps the fresh one's own, if it verifies. When
/// BOTH prove themselves, the newest `seq` wins, exactly as it does between
/// peers: the server's copy of a description can lag the device's latest word
/// (a reach re-signed mid-session), and lag is not authority. What it
/// returns is the only question that matters downstream: does the record now
/// stand behind its description? `false` strips `self_sig` from the fresh
/// record rather than leave it wearing a signature that proves nothing — the
/// honest state of a device renamed while it was away, until it countersigns.
///
/// The proof may not carry, but the ORDER does: a `seq` the held record had
/// PROVEN stays on the fresh one as the floor the next countersignature must
/// clear — without it, a re-signature in the same second would mint an equal
/// `seq`, which no peer treats as fresher, and the new name would sit unheard
/// until the clock moved. Only a proven `seq`, deliberately: the fresh record's
/// own claim is exactly that, and a floor taken on a stranger's word could pin
/// a device's real descriptions under an absurd number forever.
pub(crate) fn graft_signature(fresh: &mut Value, held: &Value) -> bool {
    // A held description that proves itself STRICTLY NEWER than the fresh
    // record's claim is tried first: a reach refresh mints local `seq`s while
    // a session lives, and a server event carrying yesterday's description
    // must not roll them back. The composite check keeps the server's word on
    // everything it owns: a name the server changed makes the graft fail,
    // and the server's description stands.
    let fresh_seq = fresh.get("seq").and_then(Value::as_u64);
    let held_newer = verify_record(held)
        && held
            .get("seq")
            .and_then(Value::as_u64)
            .is_some_and(|held_seq| fresh_seq.is_none_or(|fresh_seq| held_seq > fresh_seq));
    if !held_newer && verify_record(fresh) {
        return true;
    }
    if let (Some(seq), Some(sig)) = (held.get("seq"), held.get("self_sig")) {
        // The proof travels with everything it was minted over: the reach
        // hints ride along with `seq` and `self_sig`, or the composite could
        // never verify (an old server hands back records without them). Tried
        // on a candidate, so a graft that fails leaves the fresh record
        // exactly as the server said it, never wearing hints it cannot
        // prove.
        let mut candidate = fresh.clone();
        candidate["seq"] = seq.clone();
        candidate["self_sig"] = sig.clone();
        for field in ["addrs", "relay_hint"] {
            match held.get(field) {
                Some(value) => candidate[field] = value.clone(),
                None => {
                    if let Some(candidate) = candidate.as_object_mut() {
                        candidate.remove(field);
                    }
                }
            }
        }
        if verify_record(&candidate) {
            *fresh = candidate;
            return true;
        }
    }
    // The newer held description did not carry over (the server renamed the
    // device: the composite covers a name the record no longer makes). The
    // fresh record's OWN proof, older but real, still beats going bare.
    if held_newer && verify_record(fresh) {
        return true;
    }
    let floor = verify_record(held)
        .then(|| held.get("seq").and_then(Value::as_u64))
        .flatten();
    if let Some(record) = fresh.as_object_mut() {
        record.remove("self_sig");
        match floor {
            Some(floor) => {
                record.insert("seq".to_string(), json!(floor));
            }
            None => {
                record.remove("seq");
            }
        }
    }
    false
}

/// A record for a device we had never heard of, as it enters OUR directory:
/// keyed and labelled by its `node_id` (the one label a relayer cannot rewrite),
/// and stripped of everything the relayer could not know first-hand. What stays
/// is what the device SIGNED, its reach hints included: a device can arrive
/// known and worth dialing, off any LAN, on its own word alone. What still waits
/// to be filled in first-hand is the present tense (a route somebody vouches
/// for now, a liveness), from the transport hearing it, or a server that owns
/// those facts.
pub(crate) fn as_learned(record: &Value, node_id: &str) -> Value {
    let mut learned = record.clone();
    learned["device_id"] = json!(node_id);
    learned["relay_url"] = Value::Null;
    learned["online"] = json!(false);
    learned["last_seen"] = Value::Null;
    learned["status"] = Value::Null;
    learned
}

/// The `seq` of a description we are minting now. Wall-clock seconds, so it
/// survives a restart without a counter to persist — and strictly above
/// `previous` whatever the clock says, so two renames in the same second still
/// order, and a clock that walked backwards cannot leave us re-publishing a
/// description peers will consider older than the one they hold.
fn next_seq(previous: Option<u64>) -> u64 {
    let now = now_secs();
    match previous {
        Some(previous) if previous >= now => previous + 1,
        _ => now,
    }
}

/// This device's own directory record, built from what it knows first-hand: its
/// `node_id` (`device.key`), the name it carries, its attestation under the
/// account key — and its own signature over that description. Same shape as the
/// server's record (doc/server-api.md, "The device record") because every
/// consumer reads them side by side, plus the two fields the serverless half
/// needs (`seq`, `self_sig`); the fields only a server can fill are `null`, which
/// is a value that shape already allows for a device it has not seen yet.
///
/// It is what makes the directory of a Core that never logged in non-empty: a
/// device in the account knows at least itself, and `devices.list` no longer has
/// to answer "unreachable" to a component asking who is around.
///
/// `online` is `true`: this record is written by the running Core, and its own
/// liveness is the one presence fact it does not need a server for. It says
/// nothing about being *reachable* — `enrich_device` derives that, and with no
/// relay published and no LAN sighting it stays `false`.
///
/// `previous_seq` is the `seq` of the description this one replaces, when there
/// is one (a rename): the new record must supersede it everywhere it has already
/// been seen.
///
/// `reach` is what this device currently knows first-hand about how it can be
/// dialed; empty at a first minting (the transport has observed nothing yet;
/// the reach watcher re-signs when it does).
pub(crate) fn own_record(
    device_id: &str,
    own: crate::state::OwnDevice<'_>,
    previous_seq: Option<u64>,
    reach: &Reach,
) -> Value {
    let node_id = own.node_id();
    let platform = std::env::consts::OS;
    let seq = next_seq(previous_seq);
    let mut record = json!({
        "device_id": device_id,
        "name": own.name,
        "platform": platform,
        "node_id": node_id,
        "seq": seq,
        "self_sig": own.identity.sign(&record_message(&node_id, own.name, platform, seq, reach)),
        "relay_url": Value::Null,
        "attestation": own.attestation,
        "online": true,
        "status": Value::Null,
        "last_seen": Value::Null,
    });
    set_reach(&mut record, reach);
    record
}

/// A record as another device of the account would have signed it, at an EXACT
/// `seq` — the one thing [`own_record`] cannot be asked for, since it mints `seq`
/// from the clock (which is the point of it). Test-only; the integration harness
/// keeps its own copy, built over the same published framing.
#[cfg(test)]
pub(crate) fn signed_record(
    identity: &crate::identity::DeviceIdentity,
    name: &str,
    seq: u64,
    attestation: &str,
) -> Value {
    signed_record_reaching(identity, name, seq, attestation, &Reach::default())
}

/// [`signed_record`], with the reach the device stands behind, for the tests
/// exercising what a signed hint may and may not do.
#[cfg(test)]
pub(crate) fn signed_record_reaching(
    identity: &crate::identity::DeviceIdentity,
    name: &str,
    seq: u64,
    attestation: &str,
    reach: &Reach,
) -> Value {
    let node_id = identity.node_id();
    let platform = "linux";
    let mut record = json!({
        "device_id": node_id,
        "name": name,
        "platform": platform,
        "node_id": node_id,
        "seq": seq,
        "self_sig": identity.sign(&record_message(&node_id, name, platform, seq, reach)),
        "relay_url": Value::Null,
        "attestation": attestation,
        "online": false,
        "status": Value::Null,
        "last_seen": Value::Null,
    });
    set_reach(&mut record, reach);
    record
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> BTreeMap<String, Value> {
        BTreeMap::from([(
            "dev-1".to_string(),
            json!({ "device_id": "dev-1", "node_id": "ab", "attestation": "sig" }),
        )])
    }

    #[test]
    fn a_saved_directory_loads_back_verbatim() {
        let dir = tempfile::tempdir().expect("tempdir");
        save(dir.path(), &sample());
        assert_eq!(load(dir.path(), true), Some(sample()));
    }

    #[test]
    fn absence_and_corruption_load_as_nothing() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert_eq!(load(dir.path(), true), None, "no file");
        std::fs::write(dir.path().join(FILE), "{ not json").expect("write");
        assert_eq!(load(dir.path(), true), None, "corrupt file");
        std::fs::write(dir.path().join(FILE), r#"{ "devices": {} }"#).expect("write");
        assert_eq!(load(dir.path(), true), None, "no timestamp");
        // Even where nothing will ever refresh it: unreadable is unreadable.
        assert_eq!(load(dir.path(), false), None, "no timestamp, no authority");
    }

    #[test]
    fn a_stale_snapshot_no_longer_vouches() {
        let dir = tempfile::tempdir().expect("tempdir");
        // Just inside the TTL: loads.
        let fresh = now_secs() - CACHE_TTL.as_secs() + 60;
        let payload = json!({ "saved_at": fresh, "devices": { "d": {} } });
        std::fs::write(dir.path().join(FILE), payload.to_string()).expect("write");
        assert!(load(dir.path(), true).is_some(), "within the TTL");
        // Just past it: ignored.
        let stale = now_secs() - CACHE_TTL.as_secs() - 60;
        let payload = json!({ "saved_at": stale, "devices": { "d": {} } });
        std::fs::write(dir.path().join(FILE), payload.to_string()).expect("write");
        assert_eq!(load(dir.path(), true), None, "past the TTL");
    }

    /// The TTL is a staleness bound against a server. Where there is none, the
    /// file is not a cache of a directory — it IS the directory, and expiring it
    /// would erase the account rather than fail closed.
    #[test]
    fn without_an_authority_to_refresh_it_a_store_keeps_vouching() {
        let dir = tempfile::tempdir().expect("tempdir");
        let stale = now_secs() - CACHE_TTL.as_secs() * 10;
        let payload = json!({ "saved_at": stale, "devices": { "d": { "node_id": "ab" } } });
        std::fs::write(dir.path().join(FILE), payload.to_string()).expect("write");

        assert_eq!(load(dir.path(), true), None, "with a server: expired");
        assert!(
            load(dir.path(), false).is_some_and(|d| d.contains_key("d")),
            "with no server: still the directory"
        );
    }

    /// An exchange between two siblings changes the records without re-checking
    /// them against a server, so it must not move the freshness bound. Otherwise
    /// two devices in a room could hold that bound open between themselves
    /// indefinitely — and a device the SERVER revoked (no tombstone minted) would
    /// keep being vouched for.
    #[test]
    fn what_a_sibling_taught_us_does_not_refresh_the_bound() {
        let dir = tempfile::tempdir().expect("tempdir");
        let stale = now_secs() - CACHE_TTL.as_secs() - 60;
        let payload = json!({ "saved_at": stale, "devices": { "d": { "node_id": "ab" } } });
        std::fs::write(dir.path().join(FILE), payload.to_string()).expect("write");

        save_unrefreshed(dir.path(), &sample());

        assert_eq!(load(dir.path(), true), None, "still past the TTL");
        assert_eq!(
            load(dir.path(), false),
            Some(sample()),
            "and the records ARE the ones we just learned"
        );
        // A server snapshot, by contrast, IS a refresh.
        save(dir.path(), &sample());
        assert_eq!(load(dir.path(), true), Some(sample()));
        // With no store to carry a timestamp over from, now is the honest answer.
        let fresh = tempfile::tempdir().expect("tempdir");
        save_unrefreshed(fresh.path(), &sample());
        assert_eq!(load(fresh.path(), true), Some(sample()));
    }

    #[test]
    fn remove_leaves_nothing_loadable() {
        let dir = tempfile::tempdir().expect("tempdir");
        save(dir.path(), &sample());
        remove(dir.path());
        assert_eq!(load(dir.path(), true), None);
        // Idempotent.
        remove(dir.path());
    }

    fn identity() -> crate::identity::DeviceIdentity {
        crate::identity::DeviceIdentity::from_test_seed(7)
    }

    fn own(identity: &crate::identity::DeviceIdentity) -> crate::state::OwnDevice<'_> {
        crate::state::OwnDevice {
            identity,
            name: "Office-PC",
            attestation: "sig",
        }
    }

    #[test]
    fn an_own_record_has_the_shape_of_a_server_one() {
        let identity = identity();
        let record = own_record("dev-1", own(&identity), None, &Reach::default());

        assert_eq!(record["device_id"], json!("dev-1"));
        assert_eq!(record["name"], json!("Office-PC"));
        assert_eq!(record["node_id"], json!(identity.node_id()));
        assert_eq!(record["attestation"], json!("sig"));
        assert_eq!(record["platform"], json!(std::env::consts::OS));
        // Present and null rather than absent: the fields only a server fills.
        assert_eq!(record["relay_url"], json!(null));
        assert_eq!(record["last_seen"], json!(null));
        assert_eq!(record["status"], json!(null));
        // The reach, said out loud even when empty: an explicit empty claim,
        // never an omission a stale hint could hide behind.
        assert_eq!(record["addrs"], json!([]));
        assert_eq!(record["relay_hint"], json!(null));
        // Our own liveness needs no server.
        assert_eq!(record["online"], json!(true));
        // And what the serverless half adds: a description this device stands
        // behind, ordered.
        assert!(record["seq"].as_u64().is_some_and(|seq| seq > 0));
        assert!(verify_record(&record));
    }

    /// The signature covers the description and nothing else. What a peer says in
    /// the present tense — its relay, its liveness — is left out on purpose, so
    /// the record stays valid while those change.
    #[test]
    fn a_signature_covers_the_description_and_only_that() {
        let identity = identity();
        let record = own_record("dev-1", own(&identity), None, &Reach::default());

        for volatile in ["relay_url", "online", "last_seen", "status", "device_id"] {
            let mut altered = record.clone();
            altered[volatile] = json!("something else");
            assert!(
                verify_record(&altered),
                "{volatile} is not part of what is signed"
            );
        }
        for described in ["name", "platform", "node_id"] {
            let mut altered = record.clone();
            altered[described] = json!("something else");
            assert!(!verify_record(&altered), "{described} must be covered");
        }
        // The reach hints are covered: a relayer that plants a route (a
        // well-formed one, not just garbage) un-proves the description.
        let mut rerouted = record.clone();
        rerouted["addrs"] = json!(["192.0.2.1:1"]);
        assert!(!verify_record(&rerouted), "addrs must be covered");
        let mut rerelayed = record.clone();
        rerelayed["relay_hint"] = json!("https://liar.example");
        assert!(!verify_record(&rerelayed), "relay_hint must be covered");
        // The ordering token is covered too: without it, whoever relays a record
        // could bump `seq` and shadow every later description of that device.
        let mut replayed = record.clone();
        replayed["seq"] = json!(u64::MAX);
        assert!(!verify_record(&replayed));
    }

    /// "Not signed" must never read as "signed by someone".
    #[test]
    fn verify_record_is_fail_closed() {
        let identity = identity();
        let record = own_record("dev-1", own(&identity), None, &Reach::default());

        for missing in ["node_id", "name", "platform", "seq", "self_sig"] {
            let mut incomplete = record.clone();
            incomplete
                .as_object_mut()
                .expect("an object")
                .remove(missing);
            assert!(!verify_record(&incomplete), "{missing} missing");
        }
        // A reach without the published shape is not "no reach": refused, so a
        // signature is never checked over bytes the record does not carry.
        let malformed: [Value; 5] = [
            json!("not-a-list"),
            json!([7]),
            json!([[]]),
            json!(std::iter::repeat_n("a", MAX_ADDR_HINTS + 1).collect::<Vec<_>>()),
            json!(["a".repeat(MAX_ADDR_LEN + 1)]),
        ];
        for bad in malformed {
            let mut altered = record.clone();
            altered["addrs"] = bad.clone();
            assert!(!verify_record(&altered), "addrs {bad} must be refused");
        }
        for bad in [
            json!(7),
            json!(""),
            json!("a".repeat(MAX_RELAY_HINT_LEN + 1)),
        ] {
            let mut altered = record.clone();
            altered["relay_hint"] = bad.clone();
            assert!(!verify_record(&altered), "relay_hint {bad} must be refused");
        }
        // A record the SERVER minted: no signature, and none claimed.
        assert!(!verify_record(&json!({
            "device_id": "d_1", "name": "Office-PC", "platform": "linux",
            "node_id": identity.node_id(), "attestation": "sig",
        })));
        assert!(!verify_record(&json!({})));
        assert!(!verify_record(&Value::Null));
        // A signature by another device over our very description.
        let intruder = crate::identity::DeviceIdentity::from_test_seed(9);
        let mut stolen = record.clone();
        stolen["self_sig"] = json!(intruder.sign(&record_message(
            &identity.node_id(),
            "Office-PC",
            std::env::consts::OS,
            record["seq"].as_u64().expect("a seq"),
            &Reach::default(),
        )));
        assert!(!verify_record(&stolen));
    }

    /// The encoding of what a device signs is a compatibility contract: records
    /// are persisted and travel between devices, so a version of this project that
    /// framed the same description differently would stop verifying every record
    /// already in the field. Length prefixes, deliberately: the pairs below are
    /// what a plain concatenation would collapse into one message. (`v2` replaced
    /// `v1` wholesale before any release ever signed a record; see
    /// `RECORD_DOMAIN`; these vectors are computed independently of the code.)
    #[test]
    fn the_signed_message_matches_its_published_framing() {
        let reach = Reach {
            addrs: vec!["192.0.2.7:41641".to_string()],
            relay_hint: Some("https://relay.example".to_string()),
        };
        assert_eq!(
            hex::encode(record_message("ab12", "Ada", "linux", 7, &reach)),
            "316465766963652d6469722d7265636f72642d76323a00000000000000070000\
             00000000000461623132000000000000000341646100000000000000056c696e\
             75780000000000000001000000000000000f3139322e302e322e373a34313634\
             31000000000000001568747470733a2f2f72656c61792e6578616d706c65",
        );
        // An empty reach: the shape every record minted before a transport
        // observation carries.
        assert_eq!(
            hex::encode(record_message("ab12", "Ada", "linux", 7, &Reach::default())),
            "316465766963652d6469722d7265636f72642d76323a00000000000000070000\
             00000000000461623132000000000000000341646100000000000000056c696e\
             757800000000000000000000000000000000",
        );
        assert_ne!(
            record_message("ab12", "Ada", "|Bo", 7, &Reach::default()),
            record_message("ab12", "Ada|Bo", "", 7, &Reach::default()),
            "a field boundary must not be forgeable"
        );
        // The address list is count-prefixed: an address cannot slide over
        // into the relay hint, nor the reverse.
        let two_addrs = Reach {
            addrs: vec!["a".to_string(), "b".to_string()],
            relay_hint: None,
        };
        let addr_and_relay = Reach {
            addrs: vec!["a".to_string()],
            relay_hint: Some("b".to_string()),
        };
        assert_ne!(
            record_message("ab12", "Ada", "linux", 7, &two_addrs),
            record_message("ab12", "Ada", "linux", 7, &addr_and_relay),
            "the list boundary must not be forgeable either"
        );
    }

    /// Countersigning takes the description AS the record carries it — the name
    /// is the server's word, the signature becomes ours — and mints its `seq`
    /// strictly above whatever the record already carried, so the countersigned
    /// description supersedes the stale one everywhere it has been seen.
    #[test]
    fn countersigning_signs_the_name_the_record_carries() {
        let identity = identity();
        let mut record = json!({
            "device_id": "d_1", "node_id": identity.node_id(),
            "name": "Named-By-The-Server", "platform": "linux",
        });
        assert!(countersign_own(&mut record, &identity));
        assert_eq!(
            record["name"],
            json!("Named-By-The-Server"),
            "not ours to change"
        );
        assert!(verify_record(&record), "{record}");

        // A carried seq — however far ahead of the clock — is superseded, not
        // ducked under: without this, a clock that walked backwards would leave
        // us republishing descriptions peers consider older than the stale one.
        let held_seq = record["seq"].as_u64().expect("a seq");
        let mut renamed = record.clone();
        renamed["name"] = json!("Renamed-By-The-Server");
        renamed["seq"] = json!(held_seq + 3600);
        assert!(countersign_own(&mut renamed, &identity));
        assert!(
            renamed["seq"].as_u64().expect("a seq") > held_seq + 3600,
            "{renamed}"
        );
        assert!(verify_record(&renamed));

        // Another device's description is not ours to sign — and a record with
        // nothing signable in it is refused untouched, not half-signed.
        let stranger = crate::identity::DeviceIdentity::from_test_seed(9);
        let mut other = json!({
            "node_id": stranger.node_id(), "name": "Not-Ours", "platform": "linux",
        });
        assert!(!countersign_own(&mut other, &identity));
        assert!(other.get("self_sig").is_none(), "{other}");
        let mut bare = json!({ "node_id": identity.node_id() });
        assert!(!countersign_own(&mut bare, &identity));
        assert!(bare.get("self_sig").is_none());
    }

    /// Between two descriptions that both prove themselves, the newest `seq`
    /// wins, exactly as between peers: the server's copy can lag the device's
    /// latest word (a reach re-signed mid-session), and lag is not authority.
    #[test]
    fn between_two_proven_descriptions_the_graft_keeps_the_newest() {
        let identity = identity();
        let reach = Reach {
            addrs: vec!["10.8.0.2:41641".to_string()],
            relay_hint: None,
        };
        // The held record is the device's LATEST word (a reach re-signed
        // mid-session, seq 9); the server hands back yesterday's proven
        // description (seq 5). Lag is not authority: the graft keeps seq 9,
        // hints riding with the proof they are covered by.
        let held = signed_record_reaching(&identity, "Office-PC", 9, "sig", &reach);
        let mut fresh = crate::directory::signed_record(&identity, "Office-PC", 5, "sig");
        fresh["platform"] = json!("linux");
        fresh["online"] = json!(true);
        assert!(verify_record(&fresh), "the premise: both prove themselves");

        assert!(graft_signature(&mut fresh, &held));
        assert_eq!(fresh["seq"], json!(9), "{fresh}");
        assert_eq!(fresh["addrs"], json!(["10.8.0.2:41641"]));
        assert!(verify_record(&fresh), "{fresh}");
        assert_eq!(
            fresh["online"],
            json!(true),
            "the server's present tense stays"
        );

        // The other way round: a held record OLDER than the fresh one's own
        // proof displaces nothing.
        let stale_held = signed_record_reaching(&identity, "Office-PC", 3, "sig", &reach);
        let mut newest = crate::directory::signed_record(&identity, "Office-PC", 12, "sig");
        assert!(graft_signature(&mut newest, &stale_held));
        assert_eq!(newest["seq"], json!(12), "{newest}");
        assert_eq!(newest["addrs"], json!([]), "no stale hints grafted on");
    }

    /// A held proof is grafted onto a fresh record only if the composite proves
    /// itself — a record renamed since goes bare rather than wear a signature
    /// that covers a description it no longer makes.
    #[test]
    fn a_grafted_signature_must_prove_the_composite_or_leave_it_bare() {
        let identity = identity();
        let held = own_record("d_1", own(&identity), None, &Reach::default());

        // The fresh record makes the same description: the proof carries over.
        let same_description = || {
            json!({
                "device_id": "d_2", "node_id": identity.node_id(),
                "name": "Office-PC", "platform": std::env::consts::OS,
            })
        };
        let mut same = same_description();
        assert!(graft_signature(&mut same, &held));
        assert!(verify_record(&same), "{same}");

        // Renamed since: the held proof covers a description this record no
        // longer makes — the signature goes, never rebroadcast as if it proved
        // it, but the PROVEN seq stays as the floor the countersignature must
        // clear (a re-signature in the same second must still supersede).
        let mut renamed = same_description();
        renamed["name"] = json!("Renamed-By-The-Server");
        assert!(!graft_signature(&mut renamed, &held));
        assert!(renamed.get("self_sig").is_none(), "{renamed}");
        assert_eq!(renamed["seq"], held["seq"], "the proven floor stays");
        let floor = renamed["seq"].as_u64().expect("a floor");
        assert!(countersign_own(&mut renamed, &identity));
        assert!(
            renamed["seq"].as_u64().expect("a seq") > floor,
            "the countersignature clears the floor: {renamed}"
        );

        // A record that already proves itself is left exactly as it is.
        let mut already = own_record("d_3", own(&identity), Some(99), &Reach::default());
        let before = already.clone();
        assert!(graft_signature(&mut already, &json!({})));
        assert_eq!(already, before);

        // And one wearing a stale claim, with nothing held to replace it: the
        // claim is stripped rather than passed along.
        let mut stale = same;
        stale["name"] = json!("Renamed-By-The-Server");
        assert!(!graft_signature(&mut stale, &json!({})));
        assert!(stale.get("self_sig").is_none() && stale.get("seq").is_none());

        // A held record that does not PROVE itself lends no floor either: an
        // unproven `seq` kept as a floor could pin a device's real descriptions
        // under an absurd number forever.
        let mut unproven = own_record("d_4", own(&identity), None, &Reach::default());
        unproven["self_sig"] = json!("junk");
        unproven["seq"] = json!(u64::MAX);
        let mut fresh = same_description();
        fresh["name"] = json!("Renamed-By-The-Server");
        assert!(!graft_signature(&mut fresh, &unproven));
        assert!(fresh.get("seq").is_none(), "{fresh}");
    }

    /// The reach hints ride the same signature as the name: a device that
    /// countersigns a server's description signs the hints the record carries,
    /// and a proof grafted onto a fresh server record brings its hints along,
    /// or proves nothing and leaves the record as the server said it.
    #[test]
    fn reach_hints_travel_with_the_proof_and_never_without_it() {
        let identity = identity();
        let reach = Reach {
            addrs: vec!["192.0.2.7:41641".to_string()],
            relay_hint: Some("https://relay.example".to_string()),
        };
        // Countersigning covers the hints the record carries.
        let mut record = json!({
            "device_id": "d_1", "node_id": identity.node_id(),
            "name": "Office-PC", "platform": "linux",
        });
        set_reach(&mut record, &reach);
        assert!(countersign_own(&mut record, &identity));
        assert!(verify_record(&record), "{record}");
        assert_eq!(record_reach(&record), Some(reach.clone()));
        // A record whose reach does not have the published shape is not ours
        // to sign: refused untouched, like a missing field.
        let mut garbled = record.clone();
        garbled["addrs"] = json!("everywhere");
        let before = garbled.clone();
        assert!(!countersign_own(&mut garbled, &identity));
        assert_eq!(garbled, before);

        // A fresh record from a server that does not carry reach: the graft
        // brings seq, signature AND hints, and the composite proves itself.
        let fresh_description = || {
            json!({
                "device_id": "d_2", "node_id": identity.node_id(),
                "name": "Office-PC", "platform": "linux",
                "online": true,
            })
        };
        let mut fresh = fresh_description();
        assert!(graft_signature(&mut fresh, &record));
        assert!(verify_record(&fresh), "{fresh}");
        assert_eq!(record_reach(&fresh), Some(reach));

        // Renamed while away: the graft fails, and the hints it tried must
        // not linger on a record that cannot prove them.
        let mut renamed = fresh_description();
        renamed["name"] = json!("Renamed-By-The-Server");
        assert!(!graft_signature(&mut renamed, &record));
        assert!(renamed.get("self_sig").is_none(), "{renamed}");
        assert!(renamed.get("addrs").is_none(), "{renamed}");
        assert!(renamed.get("relay_hint").is_none(), "{renamed}");
    }

    /// What an incoming description does not say anymore is gone: a superseding
    /// record without hints must take the stale ones with it, or the held record
    /// would wear a signature covering neither description.
    #[test]
    fn a_superseding_description_takes_its_absent_fields_with_it() {
        let identity = identity();
        let reach = Reach {
            addrs: vec!["192.0.2.7:41641".to_string()],
            relay_hint: None,
        };
        let mut held = signed_record_reaching(&identity, "Office-PC", 5, "sig", &reach);
        // A fresher description that says nothing about its reach: fields
        // absent entirely, the shape a pre-hints build would relay.
        let mut incoming = signed_record(&identity, "Office-PC", 6, "sig");
        for field in ["addrs", "relay_hint"] {
            incoming.as_object_mut().expect("an object").remove(field);
        }
        assert!(
            verify_record(&incoming),
            "absent signs as empty: {incoming}"
        );

        carry_over(&mut held, &incoming);

        assert!(held.get("addrs").is_none(), "{held}");
        assert!(
            verify_record(&held),
            "the carried description proves itself whole: {held}"
        );
    }

    /// The reach is read fail-closed, and absence is an empty claim, not an
    /// error, and not a distinct message either (`record_message` signs both
    /// the same, so `set_reach` writes the explicit form).
    #[test]
    fn a_reach_is_read_fail_closed() {
        assert_eq!(record_reach(&json!({})), Some(Reach::default()));
        assert_eq!(
            record_reach(&json!({ "addrs": null, "relay_hint": null })),
            Some(Reach::default())
        );
        let full = json!({
            "addrs": ["192.0.2.7:41641"], "relay_hint": "https://relay.example",
        });
        assert_eq!(
            record_reach(&full),
            Some(Reach {
                addrs: vec!["192.0.2.7:41641".to_string()],
                relay_hint: Some("https://relay.example".to_string()),
            })
        );
        for (field, bad) in [
            ("addrs", json!({"a": 1})),
            ("addrs", json!([null])),
            ("relay_hint", json!([])),
            ("relay_hint", json!("")),
        ] {
            let mut record = full.clone();
            record[field] = bad.clone();
            assert_eq!(record_reach(&record), None, "{field} = {bad}");
        }
    }

    /// `seq` orders a device's own descriptions, and nothing else has to be
    /// persisted for it to: wall-clock seconds, forced upward when they would not
    /// move (two renames in one second, or a clock that walked backwards).
    #[test]
    fn a_new_description_always_orders_after_the_one_it_replaces() {
        let now = now_secs();
        assert_eq!(next_seq(None), now, "a first description: the clock");
        assert_eq!(next_seq(Some(now - 60)), now, "an older one: the clock");
        assert_eq!(next_seq(Some(now)), now + 1, "the same second");
        assert_eq!(
            next_seq(Some(now + 3600)),
            now + 3601,
            "a clock that walked backwards must not un-order us"
        );
    }

    /// The account this device belongs to, and one it does not.
    fn account() -> (ed25519_dalek::SigningKey, String) {
        let ak = crate::account_key::account_key_from_code(
            &crate::account_key::generate_recovery_code(),
        )
        .expect("a valid code");
        let ak_pub = crate::account_key::public_hex(&ak);
        (ak, ak_pub)
    }

    /// A roster carries what a peer can check for itself, and refuses to carry
    /// anything else — the same predicate that filters what arrives, applied on the
    /// way out. Sending the rest would be a frame that says nothing and a warning
    /// on the far end with our name on it.
    #[test]
    fn a_roster_carries_only_what_a_peer_could_verify() {
        let (ak, ak_pub) = account();
        let (other_ak, _) = account();
        let mine = crate::identity::DeviceIdentity::from_test_seed(7);
        let sibling = crate::identity::DeviceIdentity::from_test_seed(8);
        let stranger = crate::identity::DeviceIdentity::from_test_seed(9);
        let attest = |identity: &crate::identity::DeviceIdentity| {
            crate::account_key::attest(&ak, &identity.node_id())
        };

        let good = signed_record(&mine, "Office-PC", 5, &attest(&mine));
        // Signed by itself, but attested under an account that is not ours.
        let outsider = signed_record(
            &stranger,
            "Intruder",
            5,
            &crate::account_key::attest(&other_ak, &stranger.node_id()),
        );
        // Attested, but the description carries no signature — a server's record.
        let from_server = json!({
            "device_id": "d_7f3a", "node_id": sibling.node_id(),
            "name": "Named-By-The-Server", "platform": "linux",
            "attestation": attest(&sibling),
        });
        // Signed, attested — and then renamed by whoever relayed it.
        let mut tampered = signed_record(&sibling, "Laptop", 5, &attest(&sibling));
        tampered["name"] = json!("Not-What-It-Signed");

        let devices = BTreeMap::from([
            ("a".to_string(), good.clone()),
            ("b".to_string(), outsider),
            ("c".to_string(), from_server),
            ("d".to_string(), tampered),
        ]);
        let struck = stranger.node_id();
        let revoked = BTreeMap::from([
            (struck.clone(), crate::account_key::revoke(&ak, &struck)),
            // A tombstone the account never signed: it bars nobody here, so
            // passing it on would only ask a peer to reject it too.
            (
                "ab12".to_string(),
                crate::account_key::revoke(&other_ak, "ab12"),
            ),
        ]);

        let roster = Roster::of(Some(&devices), &revoked, &ak_pub);

        assert_eq!(roster.records, vec![good], "only the provable one");
        assert_eq!(roster.revoked.keys().collect::<Vec<_>>(), vec![&struck]);
        assert!(!roster.is_empty());
        // A Core with nothing to say says nothing at all.
        assert!(Roster::of(None, &BTreeMap::new(), &ak_pub).is_empty());
    }

    /// What comes off the wire is read fail-closed: a frame we cannot fully make
    /// sense of is refused WHOLE. Half-reading a roster is how a struck-off device
    /// walks back into the account.
    #[test]
    fn a_roster_off_the_wire_is_read_fail_closed() {
        let (ak, ak_pub) = account();
        let mine = crate::identity::DeviceIdentity::from_test_seed(7);
        let record = signed_record(
            &mine,
            "Office-PC",
            5,
            &crate::account_key::attest(&ak, &mine.node_id()),
        );
        let revoked = BTreeMap::from([("ab12".to_string(), "revocation".to_string())]);
        let full = Roster {
            records: vec![record.clone()],
            revoked: revoked.clone(),
        };

        // A roster round-trips through its own payload.
        assert_eq!(Roster::parse(&full.payload()), Some(full));
        // No records at all is not a roster.
        assert_eq!(Roster::parse(&json!({ "revoked": {} })), None);
        assert_eq!(Roster::parse(&json!({ "records": "many" })), None);
        assert_eq!(Roster::parse(&Value::Null), None);
        // Revocations may be absent — a peer with nothing struck off.
        assert_eq!(
            Roster::parse(&json!({ "records": [] })),
            Some(Roster::default())
        );
        // Present and not an object: refused, rather than read as "none".
        assert_eq!(
            Roster::parse(&json!({ "records": [], "revoked": [] })),
            None
        );
        // An entry that is not a signature at all is dropped, not stringified.
        assert_eq!(
            Roster::parse(&json!({ "records": [], "revoked": { "ab12": 7, "cd34": "sig" } })),
            Some(Roster {
                records: Vec::new(),
                revoked: BTreeMap::from([("cd34".to_string(), "sig".to_string())]),
            })
        );
        // Beyond what we will do the work for: refused whole.
        let flood: Vec<Value> = std::iter::repeat_n(record, MAX_ROSTER + 1).collect();
        assert_eq!(Roster::parse(&json!({ "records": flood })), None);
        // And the two halves are counted together, so neither is a way around it.
        let records: Vec<Value> = std::iter::repeat_n(json!({}), MAX_ROSTER).collect();
        let revoked: serde_json::Map<String, Value> =
            (0..2).map(|n| (format!("node{n}"), json!("sig"))).collect();
        assert_eq!(
            Roster::parse(&json!({ "records": records, "revoked": revoked })),
            None
        );
        // The predicate that guards it all, in one line: our own key is not the
        // account's, and a record proves nothing without both.
        assert!(!shareable(&json!({}), &ak_pub));
    }

    #[test]
    fn revocations_round_trip_and_survive_nothing_less() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert!(load_revoked(dir.path()).is_empty(), "no file");

        let revoked = BTreeMap::from([("ab12".to_string(), "revocation".to_string())]);
        save_revoked(dir.path(), &revoked);
        assert_eq!(load_revoked(dir.path()), revoked);

        // A revocation is not the directory's: removing the cache (logout) leaves
        // it standing, since a struck-off device keeps a valid attestation for
        // good.
        save(dir.path(), &sample());
        remove(dir.path());
        assert_eq!(load_revoked(dir.path()), revoked);

        // Unreadable reads as none: a tombstone bars a device on a signature that
        // verified, so half a file must not be half-trusted.
        std::fs::write(dir.path().join(REVOKED_FILE), "{ not json").expect("write");
        assert!(load_revoked(dir.path()).is_empty());
        std::fs::write(dir.path().join(REVOKED_FILE), r#"{ "revoked": [] }"#).expect("write");
        assert!(load_revoked(dir.path()).is_empty());
        // And an entry that is not a signature at all is dropped, not stringified.
        std::fs::write(
            dir.path().join(REVOKED_FILE),
            r#"{ "revoked": { "ab12": 7, "cd34": "revocation" } }"#,
        )
        .expect("write");
        assert_eq!(
            load_revoked(dir.path()),
            BTreeMap::from([("cd34".to_string(), "revocation".to_string())])
        );
    }
}
