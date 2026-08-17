// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

//! Everything this machine decides for itself, and never replicates
//! (doc/input-sharing.md, section 9): who may drive it, who it is willing to
//! drive, the per neighbour crossing guards, the incoming modifier remappings,
//! the return hotkey, and whether the pointer is pinned to this screen.
//!
//! # The grants are the security boundary of the whole feature
//!
//! Driving a computer means typing on it, which is remote code execution with a
//! friendly interface, so it is never implied by account membership. `allow` is
//! stored HERE, on the machine that will be driven, and it is **never
//! replicated**: a replicated grant would be a door openable from somewhere else.
//! The driving side learns by trying and gets a clean refusal; nothing in the
//! handshake hints at it, because a grant can be withdrawn between a hint and its
//! use, and because the interface must not imply that the far side's list is
//! knowable from here.
//!
//! `drive` is the other list, and it is a CONVENIENCE: which peers this machine
//! is willing to hand its keyboard to, which is what gates the warm channels and
//! carries the per peer session mode. It grants nothing anywhere. Consenting to
//! be driven says nothing about driving: the invitation is not a door that opens
//! both ways here, unlike a sync set.
//!
//! # Keyed by node_id, and that is not cosmetic
//!
//! The Core's `device_id` is not stable across the account (a server-enrolled
//! sibling is keyed by the server's id there and by node_id on a serverless
//! sibling, and a logout re-keys it). A grant keyed by `device_id` would
//! therefore be a grant that quietly moved, or quietly vanished, on a logout.
//! node_id is the one label the whole account shares.

use std::collections::BTreeMap;

use serde_json::{Value, json};

use crate::graph::{Guards, Side};
use crate::keys::{Hotkey, mods};
use crate::wire::KeyMode;

/// At most this many peers are remembered.
///
/// **Derived from the plane's [`crate::plane::MAX_DEVICES`] and not a second
/// number**, because "how big is an account" must have one answer in one
/// component: this file said 64 while the plane said 16, which is two files of
/// one component disagreeing about the same fleet. The plane's is the real bound
/// (it is forced by the signed document having to fit one `peers.send`), so it is
/// the account's size and this is derived from it.
///
/// Doubled, deliberately, and the factor is the reason rather than a fudge: a row
/// here can outlive its device (a device revoked and re-enrolled has two
/// node_ids, and only one of them is ever forgotten), while a table that refuses
/// is a door that cannot be opened. This is a garbage bound above the account's
/// size, not a second opinion about it.
const MAX_PEERS: usize = crate::plane::MAX_DEVICES * 2;
/// At most this many per neighbour guard entries. One per crossing segment, and a
/// segment needs both ends' monitors, so this is generous for any real desk (it is
/// `MAX_DEVICES * MAX_MONITORS`, the plane's own count of screens).
const MAX_GUARDS: usize = 256;
/// At most this many remapping entries per peer. There are five canonical
/// holdable modifiers, so anything beyond is not a remapping.
const MAX_REMAP: usize = 8;

/// What a local gesture did to one of these tables.
///
/// It used to be a `bool`, and the two answers it conflated are not close: "you
/// asked for what was already true" and "this table is full and your grant was
/// NOT stored" both came back as `false`, so the facade answered success while
/// granting nothing and the interface showed a door as open. Section 12 has
/// `INPUT_INTERNAL` for exactly this, and a caller can only send it if it can
/// tell the two apart.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Applied {
    /// Stored: persist and tell the interface.
    Changed,
    /// The state already said this: nothing to persist, and nothing wrong.
    Unchanged,
    /// NOT stored: the table is full, or the key is not one this table can hold.
    /// The caller owes the interface a refusal rather than a success.
    Refused,
}

impl Applied {
    /// Did anything change? What a caller tests to decide whether to persist.
    pub fn changed(self) -> bool {
        self == Applied::Changed
    }

    /// Did the gesture fail to take effect? What a caller tests to decide whether
    /// to answer `INPUT_INTERNAL` instead of success.
    pub fn refused(self) -> bool {
        self == Applied::Refused
    }
}

/// What this machine allows and prefers, per peer.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Peer {
    /// May this peer drive THIS machine? The authority, never replicated.
    pub allow: bool,
    /// Is this machine willing to drive that peer? A local convenience.
    pub drive: bool,
    /// Which level of a key frame that peer's sessions resolve first, declared by
    /// this machine when it is the source.
    pub mode: KeyMode,
    /// Canonical modifier remapping applied to what THAT peer sends here, before
    /// resolution. The first thing anyone needs the moment a Mac and a PC share a
    /// desk.
    pub remap: BTreeMap<String, String>,
}

/// The engine's own settings, entirely local.
#[derive(Clone, Debug)]
pub struct Settings {
    peers: BTreeMap<String, Peer>,
    guards: BTreeMap<String, Guards>,
    hotkey: Hotkey,
    /// Pin the pointer to this screen, for games and virtual machines. Nothing
    /// crosses while it is on.
    pub locked: bool,
}

impl Default for Settings {
    fn default() -> Settings {
        Settings {
            peers: BTreeMap::new(),
            guards: BTreeMap::new(),
            hotkey: Hotkey::default_hotkey(),
            locked: false,
        }
    }
}

impl Settings {
    /// Everything this machine knows about one peer. Absent means the default,
    /// which allows nothing and drives nothing: a peer nobody has said anything
    /// about cannot type here and is not offered our keyboard.
    pub fn peer(&self, node_id: &str) -> Peer {
        self.peers.get(node_id).cloned().unwrap_or_default()
    }

    /// May this peer drive this machine? The one question the target answers when
    /// a `start` frame arrives.
    pub fn allows(&self, node_id: &str) -> bool {
        self.peers.get(node_id).is_some_and(|p| p.allow)
    }

    /// Is this machine willing to drive that peer? What gates a warm channel.
    pub fn drives(&self, node_id: &str) -> bool {
        self.peers.get(node_id).is_some_and(|p| p.drive)
    }

    /// Grants or withdraws the right to drive this machine.
    ///
    /// Returns [`Applied`], so a caller persists when it changed and refuses when
    /// it was not stored at all.
    pub fn set_allow(&mut self, node_id: &str, allowed: bool) -> Applied {
        self.update(node_id, |p| {
            let changed = p.allow != allowed;
            p.allow = allowed;
            changed
        })
    }

    /// Sets whether this machine is willing to drive that peer, and how.
    pub fn set_drive(&mut self, node_id: &str, allowed: bool, mode: Option<KeyMode>) -> Applied {
        self.update(node_id, |p| {
            let mut changed = p.drive != allowed;
            p.drive = allowed;
            if let Some(mode) = mode
                && p.mode != mode
            {
                p.mode = mode;
                changed = true;
            }
            changed
        })
    }

    /// Sets the incoming modifier remapping for one peer. An entry naming a
    /// modifier this build does not know is dropped rather than stored: a stored
    /// name nothing can apply is a setting that lies to the interface.
    pub fn set_remap(&mut self, node_id: &str, map: BTreeMap<String, String>) -> Applied {
        let map: BTreeMap<String, String> = map
            .into_iter()
            .filter(|(from, to)| {
                mods::bit(from).is_some_and(|b| b & mods::HOLDABLE != 0)
                    && mods::bit(to).is_some_and(|b| b & mods::HOLDABLE != 0)
            })
            .take(MAX_REMAP)
            .collect();
        self.update(node_id, |p| {
            let changed = p.remap != map;
            p.remap = map;
            changed
        })
    }

    fn update(&mut self, node_id: &str, f: impl FnOnce(&mut Peer) -> bool) -> Applied {
        if !crate::plane::is_hex64(node_id) {
            return Applied::Refused;
        }
        // The bound is checked only when a NEW peer would be added, so an
        // existing one can always be changed (and in particular, a grant can
        // always be withdrawn: a full table must never be able to trap an open
        // door).
        if !self.peers.contains_key(node_id) && self.peers.len() >= MAX_PEERS {
            return Applied::Refused;
        }
        let entry = self.peers.entry(node_id.to_string()).or_default();
        let changed = f(entry);
        // A peer that allows nothing, drives nothing and remaps nothing is the
        // default: forget it, so the table does not grow with entries that say
        // nothing.
        if *entry == Peer::default() {
            self.peers.remove(node_id);
        }
        if changed {
            Applied::Changed
        } else {
            Applied::Unchanged
        }
    }

    /// Every peer this machine has said anything about.
    pub fn peers(&self) -> impl Iterator<Item = (&String, &Peer)> {
        self.peers.iter()
    }

    /// The device left the account: everything about it goes, the grant first.
    /// "A grant dies the instant the device is revoked from the account" is the
    /// epic's rule, and this is where it is kept.
    pub fn forget(&mut self, node_id: &str) -> bool {
        let had_peer = self.peers.remove(node_id).is_some();
        let before = self.guards.len();
        // Its guards go too: a stretch of edge facing a device that is gone is
        // not a neighbour whose settings mean anything.
        self.guards.retain(|key, _| {
            // The second field of a guard key is the FAR monitor, `<node>/<id>`:
            // its node half is the device this guard faces.
            !split_guard_key(key)
                .and_then(|(_, to, _)| crate::plane::split_spot_key(to))
                .is_some_and(|(node, _)| node == node_id)
        });
        had_peer || self.guards.len() != before
    }

    /// THIS device left the account: every grant goes, fail closed.
    ///
    /// Deliberately not "keep them in case we come back": re-joining would then
    /// silently restore a door somebody opened under a membership that has since
    /// ended, and nobody would be told. The guards and the hotkey stay, being
    /// preferences rather than permissions.
    pub fn forget_all_grants(&mut self) -> bool {
        // Every row, not only the ones holding a grant: the clear below drops the
        // modes and the remappings too, and reporting "nothing changed" while
        // dropping them meant the caller did not persist. The state was then gone
        // from memory and still on disk, so a restart brought it back after the
        // account had been left, which is the opposite of what this function is
        // for.
        let changed = !self.peers.is_empty();
        self.peers.clear();
        changed
    }

    /// The guards on one crossing, and the defaults for one that has none.
    pub fn guards(&self, from_monitor: &str, to_key: &str, side: Side) -> Guards {
        self.guards
            .get(&guard_key(from_monitor, to_key, side))
            .copied()
            .unwrap_or_default()
    }

    pub fn set_guards(
        &mut self,
        from_monitor: &str,
        to_key: &str,
        side: Side,
        guards: Guards,
    ) -> Applied {
        let key = guard_key(from_monitor, to_key, side);
        // The key is validated where it is BUILT, not only where it is read back.
        // [`guard_key`]'s comment claims nothing in either half can be a `|`; that
        // was an assumption and not a rule until [`crate::plane::is_sane`] made it
        // one, and while it was only an assumption a monitor id containing a `|`
        // built a key that [`split_guard_key`] then rejected on load. A wall a human
        // asked for held until the next restart and then silently stopped applying.
        // Validating here means the invariant is enforced where the comment claims
        // it, whatever a caller passes.
        if split_guard_key(&key).is_none() {
            return Applied::Refused;
        }
        if !self.guards.contains_key(&key) && self.guards.len() >= MAX_GUARDS {
            return Applied::Refused;
        }
        // The default is stored as an ABSENCE, so the table never fills with
        // entries that say what the code already says.
        if guards == Guards::default() {
            return match self.guards.remove(&key) {
                Some(_) => Applied::Changed,
                None => Applied::Unchanged,
            };
        }
        if self.guards.insert(key, guards) == Some(guards) {
            Applied::Unchanged
        } else {
            Applied::Changed
        }
    }

    pub fn hotkey(&self) -> Hotkey {
        self.hotkey
    }

    pub fn set_hotkey(&mut self, hotkey: Hotkey) -> bool {
        let changed = self.hotkey != hotkey;
        self.hotkey = hotkey;
        changed
    }

    pub fn to_value(&self) -> Value {
        let mut peers = serde_json::Map::new();
        for (node_id, p) in &self.peers {
            peers.insert(
                node_id.clone(),
                json!({
                    "allow": p.allow,
                    "drive": p.drive,
                    "mode": p.mode.as_str(),
                    "remap": p.remap,
                }),
            );
        }
        let mut guards = serde_json::Map::new();
        for (key, g) in &self.guards {
            guards.insert(key.clone(), g.to_value());
        }
        json!({
            "peers": Value::Object(peers),
            "guards": Value::Object(guards),
            "hotkey": self.hotkey.to_names(),
            "locked": self.locked,
        })
    }

    /// Reads the settings back.
    ///
    /// Every field is optional and every bad one falls back to its default, with
    /// ONE asymmetry that matters: a malformed `allow` reads as `false`, never as
    /// `true`. Everything here fails towards nobody being able to type on this
    /// machine, which is the only direction a guess is allowed to go. (A file that
    /// cannot be parsed at all never reaches this function: [`crate::store`]
    /// reports it and the component stays down, visibly, rather than starting with
    /// a permission set nobody chose.)
    pub fn from_value(doc: &Value) -> Settings {
        let mut settings = Settings::default();
        if let Some(peers) = doc.get("peers").and_then(Value::as_object) {
            for (node_id, entry) in peers.iter().take(MAX_PEERS) {
                if !crate::plane::is_hex64(node_id) {
                    continue;
                }
                let mut remap = BTreeMap::new();
                if let Some(map) = entry.get("remap").and_then(Value::as_object) {
                    for (from, to) in map.iter().take(MAX_REMAP) {
                        if let Some(to) = to.as_str() {
                            remap.insert(from.clone(), to.to_string());
                        }
                    }
                }
                let peer = Peer {
                    allow: entry.get("allow").and_then(Value::as_bool).unwrap_or(false),
                    drive: entry.get("drive").and_then(Value::as_bool).unwrap_or(false),
                    mode: entry
                        .get("mode")
                        .and_then(Value::as_str)
                        .and_then(KeyMode::parse)
                        .unwrap_or_default(),
                    remap: BTreeMap::new(),
                };
                if peer == Peer::default() && remap.is_empty() {
                    continue;
                }
                settings.peers.insert(node_id.clone(), peer);
                // Through the setter, so a stored remapping gets the same
                // filtering a fresh one does and a name this build cannot apply
                // never survives a reload.
                settings.set_remap(node_id, remap);
            }
        }
        if let Some(guards) = doc.get("guards").and_then(Value::as_object) {
            for (key, entry) in guards.iter().take(MAX_GUARDS) {
                if split_guard_key(key).is_none() {
                    continue;
                }
                let g = Guards::from_value(entry);
                if g != Guards::default() {
                    settings.guards.insert(key.clone(), g);
                }
            }
        }
        if let Some(names) = doc.get("hotkey").and_then(Value::as_array) {
            let names: Vec<String> = names
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect();
            // A hotkey this build cannot parse falls back to the default rather
            // than to nothing: a machine with no way to bring its keyboard home
            // is the one state this feature must never be in.
            if let Some(hotkey) = Hotkey::parse(&names) {
                settings.hotkey = hotkey;
            }
        }
        settings.locked = doc.get("locked").and_then(Value::as_bool).unwrap_or(false);
        settings
    }
}

/// The key one crossing's guards are stored under. A `|` separator, because a
/// monitor key already contains a `/` and a node_id is hex, so the two halves can
/// hold no `|` and the key cannot be made ambiguous.
///
/// That last clause is now ENFORCED rather than assumed, in two places, and it was
/// assumed for a while: [`crate::plane::is_sane`] refuses a `|` in a monitor id,
/// [`crate::plane::split_spot_key`] refuses one in a spot key, and
/// [`Settings::set_guards`] refuses a key that does not parse. While it was only
/// an assumption, a monitor id with a `|` in it produced a guard nothing could
/// read back, so a wall a human asked for held until the next restart.
fn guard_key(from_monitor: &str, to_key: &str, side: Side) -> String {
    format!("{from_monitor}|{to_key}|{}", side.as_str())
}

/// Splits a guard key, or `None` if it is not one. Used on load, so a
/// hand-edited or a later version's key is skipped instead of sitting in the
/// table forever matching nothing.
fn split_guard_key(key: &str) -> Option<(&str, &str, Side)> {
    let mut parts = key.split('|');
    let from = parts.next()?;
    let to = parts.next()?;
    let side = Side::parse(parts.next()?)?;
    if parts.next().is_some() {
        return None;
    }
    crate::plane::split_spot_key(from)?;
    crate::plane::split_spot_key(to)?;
    Some((from, to, side))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(n: u8) -> String {
        std::iter::repeat_n(format!("{n:02x}"), 32).collect()
    }

    fn monitor_key(n: u8, id: &str) -> String {
        crate::plane::spot_key(&node(n), id)
    }

    /// The two lists are independent, and that is the doctrine: being willing to
    /// be driven says nothing about being willing to drive.
    #[test]
    fn allowing_and_driving_are_two_separate_answers() {
        let mut settings = Settings::default();
        let peer = node(0xa);
        assert!(!settings.allows(&peer) && !settings.drives(&peer));

        assert!(settings.set_allow(&peer, true).changed());
        assert!(settings.allows(&peer));
        assert!(
            !settings.drives(&peer),
            "letting a computer type here says nothing about typing on it"
        );
        assert_eq!(
            settings.set_allow(&peer, true),
            Applied::Unchanged,
            "saying the same thing twice changes nothing, and is not a refusal"
        );
        assert!(settings.set_allow(&peer, false).changed());
        assert!(!settings.allows(&peer));
    }

    /// A grant dies the instant its device leaves the account, and so do the
    /// guards facing it. This is the epic's rule and the one this file exists
    /// for.
    #[test]
    fn forgetting_a_revoked_device_takes_its_grant_and_its_guards() {
        let mut settings = Settings::default();
        let gone = node(0xa);
        let other = node(0xb);
        settings.set_allow(&gone, true);
        settings.set_allow(&other, true);
        settings.set_guards(
            &monitor_key(1, "M"),
            &crate::plane::spot_key(&gone, "T"),
            Side::Right,
            Guards {
                wall: true,
                ..Guards::default()
            },
        );
        settings.set_guards(
            &monitor_key(1, "M"),
            &crate::plane::spot_key(&other, "T"),
            Side::Left,
            Guards {
                wall: true,
                ..Guards::default()
            },
        );

        assert!(settings.forget(&gone));
        assert!(!settings.allows(&gone), "the grant goes with the device");
        assert!(
            !settings
                .guards(
                    &monitor_key(1, "M"),
                    &crate::plane::spot_key(&gone, "T"),
                    Side::Right
                )
                .wall,
            "so do the guards facing it"
        );
        assert!(
            settings.allows(&other),
            "and nobody else's grant is touched"
        );
        assert!(
            settings
                .guards(
                    &monitor_key(1, "M"),
                    &crate::plane::spot_key(&other, "T"),
                    Side::Left
                )
                .wall
        );
        assert!(!settings.forget(&gone), "forgetting twice changes nothing");
    }

    /// Leaving the account drops every grant, fail closed: re-joining must not
    /// silently restore a door opened under a membership that has ended.
    #[test]
    fn leaving_the_account_drops_every_grant_and_keeps_the_preferences() {
        let mut settings = Settings::default();
        settings.set_allow(&node(0xa), true);
        settings.set_drive(&node(0xb), true, None);
        settings
            .set_hotkey(Hotkey::parse(&["ctrl".into(), "alt".into(), "F1".into()]).expect("parse"));
        settings.locked = true;

        assert!(settings.forget_all_grants());
        assert!(!settings.allows(&node(0xa)));
        assert!(!settings.drives(&node(0xb)));
        assert_eq!(
            settings.hotkey().to_names(),
            vec!["ctrl", "alt", "F1"],
            "a hotkey is a preference, not a permission"
        );
        assert!(settings.locked, "so is the lock");
        assert!(!settings.forget_all_grants(), "and it is idempotent");
    }

    /// Leaving the account reports EVERYTHING it dropped, not only the grants.
    ///
    /// `changed` was read off `allow || drive` while the call clears the whole
    /// table, so a peer holding only a remapping (or only a mode) was dropped while
    /// the answer said nothing had changed. The caller then did not persist: the
    /// state was gone from memory and still on disk, and the next start brought it
    /// back after the account had been left. Fail closed was the point of this
    /// function.
    #[test]
    fn leaving_the_account_reports_the_state_it_dropped_and_not_only_the_grants() {
        let mut settings = Settings::default();
        let peer = node(0xa);
        assert!(
            settings
                .set_remap(&peer, BTreeMap::from([("ctrl".into(), "meta".into())]),)
                .changed()
        );
        assert!(!settings.allows(&peer) && !settings.drives(&peer));
        assert!(
            settings.forget_all_grants(),
            "a row that held only a remapping was still a row, and it is gone: \
             a caller that does not persist leaves it on disk to come back"
        );
        assert!(settings.peer(&peer).remap.is_empty());
        assert!(!settings.forget_all_grants());

        // The same for a peer holding only a mode.
        let mut settings = Settings::default();
        settings.set_drive(&peer, true, Some(KeyMode::Positional));
        settings.set_drive(&peer, false, None);
        assert_eq!(settings.peer(&peer).mode, KeyMode::Positional);
        assert!(settings.forget_all_grants());
    }

    /// A guard key that could not be read back is never stored in the first place.
    ///
    /// [`guard_key`]'s comment claimed nothing in either half could be a `|`, and
    /// nothing enforced it: a monitor id containing one built a key `split_guard_key`
    /// then rejected on load, so a wall a human asked for held until the next restart
    /// and silently stopped applying. The plane refuses such an id at its own door
    /// now, and this refuses the key here, where the comment makes the claim.
    #[test]
    fn a_guard_key_that_cannot_be_read_back_is_never_stored() {
        let mut settings = Settings::default();
        let wall = Guards {
            wall: true,
            ..Guards::default()
        };
        let hostile = crate::plane::spot_key(&node(2), "DP-1|evil");
        assert_eq!(
            settings.set_guards(&monitor_key(1, "M"), &hostile, Side::Right, wall),
            Applied::Refused,
            "a key nothing can parse is refused where it is built"
        );
        assert_eq!(
            settings.set_guards("not-a-monitor-key", &monitor_key(2, "T"), Side::Right, wall),
            Applied::Refused
        );
        assert_eq!(
            settings.to_value()["guards"],
            json!({}),
            "and nothing that cannot be read back is on disk"
        );

        // A legal pair, on the other hand, survives the whole round trip, which is
        // what the human asked for.
        assert!(
            settings
                .set_guards(
                    &monitor_key(1, "M"),
                    &monitor_key(2, "T"),
                    Side::Right,
                    wall
                )
                .changed()
        );
        let reloaded = Settings::from_value(&settings.to_value());
        assert!(
            reloaded
                .guards(&monitor_key(1, "M"), &monitor_key(2, "T"), Side::Right)
                .wall,
            "a wall a human asked for still holds after a restart"
        );
    }

    /// Everything round trips, and the defaults are stored as absences so the
    /// file does not fill with entries that repeat the code.
    #[test]
    fn the_settings_round_trip_and_a_default_is_an_absence() {
        let mut settings = Settings::default();
        let peer = node(0xa);
        settings.set_allow(&peer, true);
        settings.set_drive(&peer, true, Some(KeyMode::Positional));
        settings.set_remap(
            &peer,
            BTreeMap::from([
                ("ctrl".into(), "meta".into()),
                ("meta".into(), "ctrl".into()),
            ]),
        );
        let guards = Guards {
            dwell_ms: 400,
            double_tap_ms: 300,
            dead_corner: 4,
            require_mods: mods::ALT,
            wall: false,
        };
        settings.set_guards(
            &monitor_key(1, "M"),
            &monitor_key(2, "T"),
            Side::Right,
            guards,
        );
        settings.set_hotkey(
            Hotkey::parse(&["ctrl".into(), "shift".into(), "Home".into()]).expect("parse"),
        );
        settings.locked = true;

        let stored = settings.to_value();
        let back = Settings::from_value(&stored);
        assert_eq!(back.peer(&peer), settings.peer(&peer));
        assert_eq!(
            back.guards(&monitor_key(1, "M"), &monitor_key(2, "T"), Side::Right),
            guards
        );
        assert_eq!(back.hotkey(), settings.hotkey());
        assert!(back.locked);

        // A guard set back to the default is an absence, not a stored copy.
        let mut settings = Settings::default();
        settings.set_guards(
            &monitor_key(1, "M"),
            &monitor_key(2, "T"),
            Side::Right,
            guards,
        );
        assert!(
            settings
                .set_guards(
                    &monitor_key(1, "M"),
                    &monitor_key(2, "T"),
                    Side::Right,
                    Guards::default()
                )
                .changed()
        );
        assert_eq!(
            settings.to_value()["guards"],
            json!({}),
            "the default is what the code says, not what the file says"
        );
    }

    /// A file nobody wrote by hand still has to be read safely, and it fails
    /// towards NOBODY being able to type here: that is the only direction a guess
    /// about a permission may go.
    #[test]
    fn a_garbled_settings_file_never_grants_anything() {
        let hostile = json!({
            "peers": {
                // Not a node_id: skipped entirely.
                "nope": { "allow": true },
                // A grant of the wrong type reads as no grant.
                node(0xa): { "allow": "yes", "drive": 7, "mode": "telepathy" },
                // A remapping naming something this build cannot apply.
                node(0xb): { "allow": true, "remap": { "hyper": "ctrl", "ctrl": 9 } },
            },
            "guards": {
                "not-a-key": { "wall": true },
                // A well-formed key with an absurd dwell.
                format!("{}|{}|right", monitor_key(1, "M"), monitor_key(2, "T")):
                    { "dwell_ms": 9_999_999u64 },
            },
            "hotkey": ["ctrl", "nonsense"],
            "locked": "sure",
        });
        let settings = Settings::from_value(&hostile);
        assert!(!settings.allows("nope"));
        assert!(
            !settings.allows(&node(0xa)),
            "a grant that is not a boolean is not a grant"
        );
        assert!(!settings.drives(&node(0xa)));
        assert_eq!(settings.peer(&node(0xa)).mode, KeyMode::default());
        assert!(
            settings.allows(&node(0xb)),
            "a real grant beside a bad remapping still stands"
        );
        assert!(
            settings.peer(&node(0xb)).remap.is_empty(),
            "a remapping this build cannot apply is not stored"
        );
        assert_eq!(
            settings
                .guards(&monitor_key(1, "M"), &monitor_key(2, "T"), Side::Right)
                .dwell_ms,
            10_000,
            "an absurd dwell is bounded, not obeyed"
        );
        assert_eq!(
            settings.hotkey(),
            Hotkey::default_hotkey(),
            "a hotkey this build cannot parse falls back to one that works"
        );
        assert!(!settings.locked);
    }

    /// The tables are bounded, and a full table can still be made to say NO: a
    /// bound that could trap an open door would be a bound that creates a hole.
    #[test]
    fn the_tables_are_bounded_and_a_grant_can_always_be_withdrawn() {
        let mut settings = Settings::default();
        for i in 0..MAX_PEERS {
            assert!(
                settings.set_allow(&node(i as u8), true).changed(),
                "peer {i}"
            );
        }
        let extra = node(200);
        assert_eq!(
            settings.set_allow(&extra, true),
            Applied::Refused,
            "the bound refuses a new peer, and SAYS so: a refusal that read as \
             \"nothing changed\" made the facade answer success while granting nothing"
        );
        assert!(!settings.allows(&extra));
        // And every existing grant can still be withdrawn with the table full.
        assert!(settings.set_allow(&node(0), false).changed());
        assert!(!settings.allows(&node(0)));
    }

    /// A guard key cannot be made ambiguous: both halves are monitor keys and the
    /// separator appears in neither.
    #[test]
    fn a_guard_key_is_two_monitors_and_a_side() {
        let key = guard_key(&monitor_key(1, "M"), &monitor_key(2, "T"), Side::Bottom);
        let (from, to, side) = split_guard_key(&key).expect("its own key parses");
        assert_eq!(from, monitor_key(1, "M"));
        assert_eq!(to, monitor_key(2, "T"));
        assert_eq!(side, Side::Bottom);
        for bad in [
            "",
            "a|b|right",
            &format!("{}|{}", monitor_key(1, "M"), monitor_key(2, "T")),
            &format!("{}|{}|sideways", monitor_key(1, "M"), monitor_key(2, "T")),
            &format!(
                "{}|{}|right|extra",
                monitor_key(1, "M"),
                monitor_key(2, "T")
            ),
        ] {
            assert!(split_guard_key(bad).is_none(), "{bad} must not be a key");
        }
    }
}
