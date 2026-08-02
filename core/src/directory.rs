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
//! its window without one keeps vouching. What takes the TTL's place in that
//! case is a revocation carried inside the store itself (signed, permanent), the
//! next building block of the serverless work.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};

/// How long a persisted snapshot vouches. This is the revocation-staleness
/// bound: the longest a struck-off device can keep being served by a sibling
/// that restarts without ever reaching the server.
const CACHE_TTL: Duration = Duration::from_secs(7 * 24 * 60 * 60);

const FILE: &str = "directory.json";

/// Persists the map — called under the session lock at every mutation (the
/// same state-then-disk discipline as `session.json` at login), so the file
/// is never newer than the memory it mirrors. Failure is not fatal: the Core
/// just loses its head start at the next offline startup — said out loud,
/// because a silently missing cache would look like a bug exactly when the
/// user has no network to debug with.
pub(crate) fn save(config_dir: &Path, devices: &BTreeMap<String, Value>) {
    let payload = json!({ "saved_at": now_secs(), "devices": devices });
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

/// This device's own directory record, built from what it knows first-hand: its
/// `node_id` (`device.key`), the name it carries, and its attestation under the
/// account key. Same shape as the server's record (doc/server-api.md, "The
/// device record") because every consumer reads them side by side — the fields
/// only a server can fill are `null`, which is a value that shape already allows
/// for a device it has not seen yet.
///
/// It is what makes the directory of a Core that never logged in non-empty: a
/// device in the account knows at least itself, and `devices.list` no longer has
/// to answer "unreachable" to a component asking who is around.
///
/// `online` is `true`: this record is written by the running Core, and its own
/// liveness is the one presence fact it does not need a server for. It says
/// nothing about being *reachable* — `enrich_device` derives that, and with no
/// relay published and no LAN sighting it stays `false`.
pub(crate) fn own_record(device_id: &str, name: &str, node_id: &str, attestation: &str) -> Value {
    json!({
        "device_id": device_id,
        "name": name,
        "platform": std::env::consts::OS,
        "node_id": node_id,
        "relay_url": Value::Null,
        "attestation": attestation,
        "online": true,
        "status": Value::Null,
        "last_seen": Value::Null,
    })
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

    #[test]
    fn remove_leaves_nothing_loadable() {
        let dir = tempfile::tempdir().expect("tempdir");
        save(dir.path(), &sample());
        remove(dir.path());
        assert_eq!(load(dir.path(), true), None);
        // Idempotent.
        remove(dir.path());
    }

    #[test]
    fn an_own_record_has_the_shape_of_a_server_one() {
        let record = own_record("dev-1", "Office-PC", "ab12", "sig");

        assert_eq!(record["device_id"], json!("dev-1"));
        assert_eq!(record["name"], json!("Office-PC"));
        assert_eq!(record["node_id"], json!("ab12"));
        assert_eq!(record["attestation"], json!("sig"));
        assert_eq!(record["platform"], json!(std::env::consts::OS));
        // Present and null rather than absent: the fields only a server fills.
        assert_eq!(record["relay_url"], json!(null));
        assert_eq!(record["last_seen"], json!(null));
        assert_eq!(record["status"], json!(null));
        // Our own liveness needs no server.
        assert_eq!(record["online"], json!(true));
    }
}
