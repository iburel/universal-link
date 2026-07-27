// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

//! Which devices the menu offers, derived from the Core's directory cache and
//! the session state. Pure: no I/O, no Core, so the whole fail-closed rule set
//! is unit-tested here and only its *wiring* needs a real Core.

use std::collections::BTreeMap;

use serde_json::Value;

use crate::surface::Target;

/// The device directory as the manager mirrors it, plus the session facts that
/// decide whether ANY target is offerable.
///
/// State comes only from a snapshot (`devices.list`, `session.status`) or from a
/// notification — never from the reply to something we asked for. Same rule as
/// the GUI's store and the clipboard orchestrator.
#[derive(Debug, Default)]
pub struct Directory {
    devices: BTreeMap<String, Value>,
    logged_in: bool,
    server_connected: bool,
    holds_account_key: bool,
}

impl Directory {
    pub fn new() -> Directory {
        Directory::default()
    }

    /// Replaces the whole directory from a `devices.list` snapshot. Total, never
    /// merged: a resync must not leave a device the server has since forgotten.
    pub fn replace_all(&mut self, list: &[Value]) {
        self.devices = list
            .iter()
            .filter_map(|d| device_id(d).map(|id| (id.to_string(), d.clone())))
            .collect();
    }

    /// Applies a `session.status` result or a `session.changed` payload.
    /// `session.changed` may omit `configured`; it never omits the two fields
    /// read here.
    pub fn apply_session(&mut self, payload: &Value) {
        self.logged_in = payload["logged_in"].as_bool().unwrap_or(false);
        self.server_connected = payload["server_connected"].as_bool().unwrap_or(false);
    }

    /// Applies an `account.status` result: whether THIS device holds the account
    /// key. Read at every snapshot — there is no `account.*` notification, so a
    /// resnapshot is the only way it can ever change.
    pub fn apply_account(&mut self, payload: &Value) {
        self.holds_account_key = payload["attested"].as_bool().unwrap_or(false);
    }

    /// Applies a `device.*` notification. Returns whether it was understood — an
    /// unknown one is ignored (a newer Core may notify more than we model).
    pub fn apply_device_event(&mut self, method: &str, params: &Value) -> bool {
        match method {
            // Carry the whole record: added / online / updated all ship one.
            "device.added" | "device.online" | "device.updated" => {
                let record = &params["device"];
                match device_id(record) {
                    Some(id) => {
                        self.devices.insert(id.to_string(), record.clone());
                        true
                    }
                    None => false,
                }
            }
            // `device.offline { device_id, last_seen }` — the id only, so the
            // record we hold is patched rather than replaced. A device we have
            // never seen is NOT invented from an offline event: an entry with no
            // name or platform would be a target we cannot even label, and the
            // next snapshot is authoritative anyway.
            //
            // Only `online` is patched. The relay dies with the connection
            // server-side too, but clearing it here would be unobservable: an
            // offline device is already excluded, and any later event that could
            // bring it back carries a WHOLE record which replaces this one.
            "device.offline" => match params["device_id"].as_str() {
                Some(id) => {
                    if let Some(record) = self.devices.get_mut(id) {
                        record["online"] = Value::Bool(false);
                    }
                    true
                }
                None => false,
            },
            "device.removed" => match params["device_id"].as_str() {
                Some(id) => {
                    self.devices.remove(id);
                    true
                }
                None => false,
            },
            _ => false,
        }
    }

    /// The menu's targets, in the order they must be shown.
    ///
    /// Fail-closed, and every exclusion is a decision, not an accident. The rule
    /// behind all of them: offer only what a click could actually reach, because
    /// a menu entry that always fails is worse than no entry.
    /// - **no session, or no server connection** → nothing at all. The directory
    ///   cache outlives the connection (the Core serves it offline), so its
    ///   `online` flags go stale the moment the link drops: offering them would
    ///   be offering sends that cannot start.
    /// - **we do not hold the account key** → nothing at all. The Core resolves
    ///   every peer against our own account root, so without it `files.send`
    ///   answers `DEVICE_UNKNOWN` for EVERY device — a full menu of dead entries
    ///   on a device that is logged in but has not joined the account yet.
    /// - **`is_self`** → we are not a destination for ourselves.
    /// - **not `online`** → `files.send` answers `DEVICE_OFFLINE`.
    /// - **no `relay_url`** → also `DEVICE_OFFLINE`. Coming online and publishing
    ///   a relay are two separate steps (`auth.authenticate` carries none, and the
    ///   server clears it when a connection closes), so there is a real window in
    ///   which a peer is `online` and unreachable.
    /// - **`android`** → the phone receives into app-private storage that nothing
    ///   yet opens, so a file sent there is a file lost (decision, 2026-07-27).
    /// - **no `attestation`** → `files.send` answers `DEVICE_UNKNOWN`. We check
    ///   *presence*, not validity: verifying the signature would mean holding the
    ///   account key, which a component has no business holding. A device
    ///   attested under a foreign key therefore still shows, and its click fails
    ///   cleanly at the Core — the divergent-account case the GUI's onboarding
    ///   already guards against.
    pub fn targets(&self) -> Vec<Target> {
        if !self.logged_in || !self.server_connected || !self.holds_account_key {
            return Vec::new();
        }
        let mut targets: Vec<Target> = self
            .devices
            .values()
            .filter(|d| is_offerable(d))
            .map(|d| Target {
                device_id: d["device_id"].as_str().unwrap_or_default().to_string(),
                name: display_name(d),
                platform: d["platform"].as_str().unwrap_or_default().to_string(),
            })
            .collect();
        // A menu the user reads: alphabetical, case-insensitively, and the id
        // breaks ties so two identically-named PCs keep a stable order rather
        // than swapping places between two rewrites.
        targets.sort_by(|a, b| {
            a.name
                .to_lowercase()
                .cmp(&b.name.to_lowercase())
                .then_with(|| a.device_id.cmp(&b.device_id))
        });
        targets
    }
}

fn device_id(record: &Value) -> Option<&str> {
    record["device_id"].as_str().filter(|id| !id.is_empty())
}

fn is_offerable(d: &Value) -> bool {
    device_id(d).is_some()
        && d["online"].as_bool().unwrap_or(false)
        && !d["is_self"].as_bool().unwrap_or(false)
        && d["platform"].as_str() != Some("android")
        && non_empty(&d["attestation"])
        && non_empty(&d["relay_url"])
}

fn non_empty(v: &Value) -> bool {
    v.as_str().is_some_and(|s| !s.trim().is_empty())
}

/// A device with no usable name still has to be clickable: the id is a poor
/// label but an honest one, and dropping the device instead would hide a
/// reachable PC.
fn display_name(d: &Value) -> String {
    match d["name"].as_str().map(str::trim) {
        Some(name) if !name.is_empty() => name.to_string(),
        _ => d["device_id"].as_str().unwrap_or_default().to_string(),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    /// An online, attested peer — the shape `devices.list` really serves.
    fn peer(id: &str, name: &str, platform: &str) -> Value {
        json!({
            "device_id": id,
            "name": name,
            "platform": platform,
            "node_id": "aa",
            "relay_url": "https://relay.example/",
            "attestation": "beef",
            "online": true,
            "status": null,
            "last_seen": "2026-07-27T00:00:00Z",
            "is_self": false,
        })
    }

    fn live(list: &[Value]) -> Directory {
        let mut dir = Directory::new();
        dir.apply_session(&json!({ "logged_in": true, "server_connected": true }));
        dir.apply_account(&json!({ "attested": true }));
        dir.replace_all(list);
        dir
    }

    fn ids(dir: &Directory) -> Vec<String> {
        dir.targets().into_iter().map(|t| t.device_id).collect()
    }

    #[test]
    fn an_online_attested_peer_is_a_target() {
        let dir = live(&[peer("d_1", "PC-B", "linux")]);
        assert_eq!(
            dir.targets(),
            [Target {
                device_id: "d_1".into(),
                name: "PC-B".into(),
                platform: "linux".into(),
            }]
        );
    }

    #[test]
    fn every_exclusion_rule_removes_its_device() {
        let mut offline = peer("d_off", "Offline", "linux");
        offline["online"] = json!(false);
        let mut myself = peer("d_self", "Me", "linux");
        myself["is_self"] = json!(true);
        let phone = peer("d_phone", "Phone", "android");
        let mut bare = peer("d_bare", "Never attested", "linux");
        bare["attestation"] = Value::Null;
        let mut blank = peer("d_blank", "Blank attestation", "linux");
        blank["attestation"] = json!("   ");
        // Online but with no relay published yet: `files.send` would answer
        // DEVICE_OFFLINE. A real window — authenticating and publishing a relay
        // are two separate steps.
        let mut relayless = peer("d_norelay", "No relay yet", "linux");
        relayless["relay_url"] = Value::Null;

        let dir = live(&[
            offline,
            myself,
            phone,
            bare,
            blank,
            relayless,
            peer("d_ok", "Keeper", "windows"),
        ]);
        assert_eq!(ids(&dir), ["d_ok"]);
    }

    /// The Core resolves every peer against OUR account root: without it every
    /// single click answers `DEVICE_UNKNOWN`, so a full menu would be a menu of
    /// dead entries.
    #[test]
    fn nothing_is_offered_while_this_device_has_no_account_key() {
        let devices = [peer("d_1", "PC-B", "linux")];

        let mut dir = Directory::new();
        dir.apply_session(&json!({ "logged_in": true, "server_connected": true }));
        dir.replace_all(&devices);
        // Never snapshotted an account status: fail-closed.
        assert!(dir.targets().is_empty());

        dir.apply_account(&json!({ "attested": false }));
        assert!(dir.targets().is_empty());

        dir.apply_account(&json!({ "attested": true, "fingerprint": "12345 67890" }));
        assert_eq!(ids(&dir), ["d_1"]);
    }

    #[test]
    fn nothing_is_offered_without_a_session_or_a_server() {
        let devices = [peer("d_1", "PC-B", "linux")];

        let mut dir = Directory::new();
        dir.apply_account(&json!({ "attested": true }));
        dir.replace_all(&devices);
        // Never snapshotted a session: fail-closed.
        assert!(dir.targets().is_empty());

        // Logged in but the server link is down: the `online` flags we hold are
        // last-known, not current.
        dir.apply_session(&json!({ "logged_in": true, "server_connected": false }));
        assert!(dir.targets().is_empty());

        dir.apply_session(&json!({ "logged_in": true, "server_connected": true }));
        assert_eq!(ids(&dir), ["d_1"]);

        // A logout empties the menu even though the cache still holds the peer.
        dir.apply_session(&json!({ "logged_in": false, "server_connected": false }));
        assert!(dir.targets().is_empty());
    }

    #[test]
    fn device_events_move_a_peer_in_and_out() {
        let mut dir = live(&[]);
        assert!(dir.targets().is_empty());

        assert!(dir.apply_device_event(
            "device.added",
            &json!({ "device": peer("d_1", "A", "linux") })
        ));
        assert_eq!(ids(&dir), ["d_1"]);

        // Offline carries the id alone: the held record is patched.
        assert!(dir.apply_device_event(
            "device.offline",
            &json!({ "device_id": "d_1", "last_seen": "2026-07-27T00:00:00Z" })
        ));
        assert!(dir.targets().is_empty());

        assert!(dir.apply_device_event(
            "device.online",
            &json!({ "device": peer("d_1", "A", "linux") })
        ));
        assert_eq!(ids(&dir), ["d_1"]);

        // A rename arrives as a whole record.
        let mut renamed = peer("d_1", "Renamed", "linux");
        renamed["name"] = json!("Renamed");
        assert!(dir.apply_device_event("device.updated", &json!({ "device": renamed })));
        assert_eq!(dir.targets()[0].name, "Renamed");

        assert!(dir.apply_device_event("device.removed", &json!({ "device_id": "d_1" })));
        assert!(dir.targets().is_empty());
    }

    #[test]
    fn an_offline_event_never_invents_a_device() {
        // A device we have never snapshotted must not become a nameless target.
        let mut dir = live(&[]);
        assert!(dir.apply_device_event("device.offline", &json!({ "device_id": "d_ghost" })));
        assert!(dir.targets().is_empty());
        // And it must not resurrect as a target either.
        assert!(dir.apply_device_event(
            "device.online",
            &json!({ "device": peer("d_ghost", "Ghost", "linux") })
        ));
        assert_eq!(ids(&dir), ["d_ghost"]);
    }

    /// A device that comes back online BEFORE publishing a relay is still not
    /// offerable — the record replaces ours wholesale, relay included, so there is
    /// nothing stale to inherit and nothing to offer yet.
    #[test]
    fn coming_back_online_without_a_relay_is_not_yet_a_target() {
        let mut dir = live(&[peer("d_1", "A", "linux")]);
        assert_eq!(ids(&dir), ["d_1"]);

        dir.apply_device_event("device.offline", &json!({ "device_id": "d_1" }));
        assert!(dir.targets().is_empty());

        let mut back = peer("d_1", "A", "linux");
        back["relay_url"] = Value::Null;
        dir.apply_device_event("device.online", &json!({ "device": back }));
        assert!(dir.targets().is_empty(), "online but unreachable");

        // And offerable again once the relay is published (a `device.updated`).
        dir.apply_device_event(
            "device.updated",
            &json!({ "device": peer("d_1", "A", "linux") }),
        );
        assert_eq!(ids(&dir), ["d_1"]);
    }

    #[test]
    fn unknown_and_malformed_events_are_reported_as_not_understood() {
        let mut dir = live(&[]);
        assert!(!dir.apply_device_event("device.teleported", &json!({})));
        assert!(!dir.apply_device_event("device.added", &json!({ "device": { "name": "no id" } })));
        assert!(!dir.apply_device_event("device.removed", &json!({})));
        assert!(dir.targets().is_empty());
    }

    #[test]
    fn targets_are_sorted_for_a_human_and_stable_between_rewrites() {
        let dir = live(&[
            peer("d_3", "zeta", "linux"),
            peer("d_1", "Alpha", "windows"),
            peer("d_2", "beta", "macos"),
        ]);
        assert_eq!(ids(&dir), ["d_1", "d_2", "d_3"]);

        // Same name on two PCs: the id decides, so the order never flickers.
        let dir = live(&[peer("d_9", "Twin", "linux"), peer("d_4", "Twin", "linux")]);
        assert_eq!(ids(&dir), ["d_4", "d_9"]);
    }

    #[test]
    fn a_nameless_device_falls_back_to_its_id() {
        let mut nameless = peer("d_1", "", "linux");
        nameless["name"] = json!("  ");
        let dir = live(&[nameless]);
        assert_eq!(dir.targets()[0].name, "d_1");
    }

    #[test]
    fn a_snapshot_replaces_rather_than_merges() {
        let mut dir = live(&[peer("d_1", "A", "linux"), peer("d_2", "B", "linux")]);
        assert_eq!(ids(&dir), ["d_1", "d_2"]);
        dir.replace_all(&[peer("d_2", "B", "linux")]);
        assert_eq!(ids(&dir), ["d_2"]);
    }
}
