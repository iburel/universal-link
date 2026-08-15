// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

//! The deployment's relay announcement, client half (#105).
//!
//! A server account is the natural place to restore the off-LAN fallback the
//! off default gave up (#104): the operator pays for the relays, the operator
//! says where they are. The word lives in the deployment descriptor
//! (`/.well-known/1device.json`), and this module is what a Core does with
//! it: re-read it at every session establishment, keep its own copy on disk,
//! and hand the list to the transport, which folds it into its relay
//! resolution UNDER the local choice (an explicit local `relay` wins; the
//! announcement only fills the off default).
//!
//! The disk copy is the availability half of the design: a Core that boots
//! with the server down (or unreachable) still binds with the operator's
//! relays, so the serverless half of the continuum stays dialable off the
//! LAN. The copy dies where the directory cache dies: when the Core stops
//! acting for the server session (logout, revocation) or for the account
//! (leave) — the announcement was the server's standing word, and the
//! standing ends with the relationship. Records signed while it stood keep
//! their `relay_hint` (a hint is not a promise; a stale one costs one failed
//! dial), and the reach watcher re-signs honestly after the next bind.
//!
//! Everything read here is bounded and fail-closed: entries that are not
//! strings, blank, or oversized are dropped, the list is deduplicated and
//! clamped, and a fetch that fails (network, an older server without the
//! field) is NO word — the cache stands, nothing is wiped by an error.

use std::path::Path;

use serde_json::{Value, json};

use crate::state::AppState;

/// Announced relays kept, at most. A bound like the record's hint bounds
/// (`MAX_ADDR_HINTS`): the endpoint probes every relay in its map, and a
/// misconfigured (or hostile) server must not choose how much work that is.
pub(crate) const MAX_ANNOUNCED: usize = 16;
/// One announced URL, at most, in bytes (mirrors `MAX_RELAY_HINT_LEN`).
const MAX_URL_LEN: usize = 2048;

const FILE: &str = "announced-relays.json";

/// The cached announcement, `[]` when there is none (absent file, unreadable
/// file, or a deployment that announced none). Same tolerance as every cache
/// read: garbage is no announcement, never an error.
pub(crate) fn load(config_dir: &Path) -> Vec<String> {
    let Ok(text) = std::fs::read_to_string(config_dir.join(FILE)) else {
        return Vec::new();
    };
    let Ok(value) = serde_json::from_str::<Value>(&text) else {
        return Vec::new();
    };
    match value.get("relays").and_then(Value::as_array) {
        Some(list) => sanitize(list),
        None => Vec::new(),
    }
}

/// Persists the announcement — including the empty one: "this deployment
/// announced none" is a word too, and it must survive a restart so a relay
/// the operator withdrew does not come back from an older cache.
fn store(config_dir: &Path, relays: &[String]) {
    let payload = json!({ "relays": relays });
    if let Err(e) = crate::write_private_file(&config_dir.join(FILE), &payload.to_string()) {
        tracing::warn!(error = %e, "failed to persist the announced relays");
    }
}

/// Removes the cache — logout, revocation, leaving the account: the
/// announcement was the server's standing word, and the standing ended. Same
/// fallback as the directory cache: emptying reads as no announcement.
pub(crate) fn forget(config_dir: &Path) {
    let path = config_dir.join(FILE);
    match std::fs::remove_file(&path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => {
            tracing::error!(error = %e, "failed to remove the announced relays: contents emptied instead");
            if let Err(e) = crate::write_private_file(&path, "") {
                tracing::error!(error = %e, "failed to erase the announced relays");
            }
        }
    }
}

/// Reads the deployment descriptor and returns its relay list, sanitized.
/// `None` is NO word — an unreachable server, a non-200, a body that is not
/// a descriptor, or a server too old to carry the field — and the caller
/// changes nothing on no word. `Some(vec![])` by contrast IS a word: this
/// deployment runs no relay.
pub(crate) async fn fetch(
    connector: &dyn crate::Connector,
    server_url: &str,
) -> Option<Vec<String>> {
    let (descriptor_url, _) = crate::discover::addresses(server_url)?;
    let (status, body) = crate::http::request(connector, &descriptor_url, None)
        .await
        .ok()?;
    if status != 200 {
        return None;
    }
    let value: Value = serde_json::from_str(&body).ok()?;
    let list = value.get("relays")?.as_array()?;
    Some(sanitize(list))
}

/// Applies a freshly fetched announcement: nothing if it matches the cache,
/// otherwise persist it and hand it to the transport. The transport applies
/// it at its next bind; a change that arrives after the endpoint is up is
/// its to log ("applied at the next start").
pub(crate) fn apply(state: &AppState, fresh: &[String]) {
    if load(&state.config_dir) == fresh {
        return;
    }
    tracing::info!(
        relays = fresh.len(),
        "the deployment's relay announcement changed"
    );
    store(&state.config_dir, fresh);
    state.transport.announce_relays(fresh);
}

/// Fail-closed shape check shared by the descriptor read and the disk cache:
/// string entries only, trimmed, non-empty, bounded in length, deduplicated
/// in order, and the list clamped to `MAX_ANNOUNCED`.
fn sanitize(list: &[Value]) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    let mut relays: Vec<String> = list
        .iter()
        .filter_map(Value::as_str)
        .map(str::trim)
        .filter(|url| !url.is_empty() && url.len() <= MAX_URL_LEN)
        .map(str::to_string)
        .filter(|url| seen.insert(url.clone()))
        .collect();
    if relays.len() > MAX_ANNOUNCED {
        tracing::warn!(
            announced = relays.len(),
            kept = MAX_ANNOUNCED,
            "the deployment announces more relays than a client will probe: clamped"
        );
        relays.truncate(MAX_ANNOUNCED);
    }
    relays
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_cache_round_trips_and_garbage_reads_as_none() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert!(load(dir.path()).is_empty(), "absent file: no announcement");

        let relays = vec!["https://relay-eu.example".to_string()];
        store(dir.path(), &relays);
        assert_eq!(load(dir.path()), relays);

        // The EMPTY announcement persists too: a withdrawn relay must not
        // come back from an older cache at the next boot.
        store(dir.path(), &[]);
        assert!(load(dir.path()).is_empty());

        std::fs::write(dir.path().join(FILE), "{ not json").expect("write");
        assert!(load(dir.path()).is_empty(), "garbage is no announcement");

        store(dir.path(), &relays);
        forget(dir.path());
        assert!(load(dir.path()).is_empty(), "forgotten");
    }

    #[test]
    fn sanitize_is_fail_closed_bounded_and_deduplicated() {
        let list = vec![
            json!("https://relay-eu.example"),
            json!("  https://relay-us.example  "),
            json!("https://relay-eu.example"),
            json!(""),
            json!("   "),
            json!(42),
            json!(null),
            json!("h".repeat(MAX_URL_LEN + 1)),
        ];
        assert_eq!(
            sanitize(&list),
            vec![
                "https://relay-eu.example".to_string(),
                "https://relay-us.example".to_string()
            ]
        );

        let many: Vec<Value> = (0..MAX_ANNOUNCED + 5)
            .map(|n| json!(format!("https://relay-{n}.example")))
            .collect();
        assert_eq!(sanitize(&many).len(), MAX_ANNOUNCED);
    }
}
