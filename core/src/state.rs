// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

//! The Core's shared state: connection registry, bootstrap tokens, enrolled
//! components, pending requests, and server session state. Lock ordering:
//! `session` then `registry` (taking the registry while holding the session is
//! allowed — this is how `session.changed` goes out atomically with its
//! transition — the reverse never); `login` is never held with another;
//! `account_root`, `transfers`, `server_config` and `clipboard` are LEAVES
//! (never held with another lock: always cloned/released beforehand). No lock
//! across an await.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::Mutex;

use serde_json::{Value, json};
use tokio::sync::mpsc;

use crate::rpc::{self, RpcErr};
use crate::session::SessionInfo;

pub type ConnId = u64;

/// An outbound message to a connection, via its bounded queue.
pub enum OutMsg {
    Frame(String),
    Close,
}

pub struct AppState {
    pub registry: Mutex<Registry>,
    pub session: Mutex<SessionState>,
    /// The account's trust root (C7): `ak_pub` to verify peers' attestations,
    /// plus our own to republish on every connection (the server, in memory,
    /// forgets it). `None` until the device has joined the account —
    /// fail-closed. Leaf lock: never held at the same time as
    /// `session`/`registry` (always cloned then released beforehand).
    pub account_root: Mutex<Option<crate::account_key::AccountRoot>>,
    /// Pending OIDC flow (login or revoke re-auth) — one at a time, the next
    /// one replaces it.
    pub login: Mutex<Option<LoginSlot>>,
    /// The pairing in flight (`pairing.rs`): one at a time, the next one replaces
    /// it — a device displays one code and confirms one device. LEAF lock,
    /// deliberately: the paths that touch it read the session, the account root
    /// and the keyring, and they release each before taking this one.
    pub pairing: Mutex<Option<crate::pairing::Pairing>>,
    /// To remove `session.json` at logout / revocation.
    pub config_dir: PathBuf,
    /// The device's Ed25519 identity (`device.key`), cloned by the session and
    /// login tasks.
    pub identity: crate::identity::DeviceIdentity,
    /// The deployment's server + OIDC — without it, no login is possible.
    /// Behind a lock because `session.reload` swaps it live: the GUI writes a
    /// fresh `config.json`, the Core re-reads it — no process restart. LEAF
    /// lock (cloned/released before any other, never across an await).
    pub server_config: Mutex<Option<crate::ServerConfig>>,
    /// The human reason the persisted config is faulty, `None` when it is
    /// sound. Seeded at spawn, refreshed by every `session.reload` (a reload
    /// that fails leaves `server_config` alone but the file's fresh fault is
    /// still the truth to show). Served by `session.status` (`problem`): the
    /// interface is where a cure sentence reaches the user, a log line does
    /// not. LEAF lock, same rule as `server_config`.
    pub config_problem: Mutex<Option<String>>,
    /// Re-reads the persisted config and returns the server it describes (or a
    /// human reason it is unusable). Injected by the daemon, which owns
    /// `config.json` parsing; `session.reload` calls it to apply what the GUI
    /// just wrote. `Ok(None)` = nothing configured.
    pub reload_server:
        std::sync::Arc<dyn Fn() -> Result<Option<crate::ServerConfig>, String> + Send + Sync>,
    /// The device's name in the directory, chosen at enrollment.
    pub device_name: String,
    /// Keyring for the durable secrets (OIDC refresh token).
    pub secrets: std::sync::Arc<dyn crate::SecretStore>,
    /// Opens the outbound streams — in the clear, or over TLS if the binary
    /// wired it in.
    pub connector: std::sync::Arc<dyn crate::Connector>,
    /// P2P data plane: iroh (daemon) or in-memory transport (tests).
    pub transport: std::sync::Arc<dyn crate::PeerTransport>,
    /// Where received files land (`files.send` from a peer). Created at the
    /// first incoming transfer. The binary points it at the user's downloads;
    /// the tests at a temporary folder.
    pub receive_dir: PathBuf,
    /// Transfers in progress (T2), cancelable in both directions. LEAF lock.
    pub transfers: Mutex<Transfers>,
    /// The clipboard's transactions (the pull-at-paste capability table). LEAF
    /// lock: taken alone, never across an await.
    pub clipboard: Mutex<crate::clipboard::ClipboardState>,
    /// Fired when every transaction is dropped (Core stop, logout): the open
    /// consumer channels wake, push `TX_STALE`, and close — a revocation that
    /// takes effect at once rather than at the next request. `notify_waiters`
    /// (not `notify_one`): all sessions must be cut.
    pub clipboard_reset: tokio::sync::Notify,
    /// The live peer channels (#124), one per (role, peer) pair. LEAF lock:
    /// taken alone, never across an await.
    pub peer_channels: Mutex<crate::peerchannel::PeerChannels>,
    /// Nudges the directory sync task (`dirsync`) into a round right away: a
    /// local rename or revocation is something the account's other devices should
    /// hear now, not at the next tick. `notify_one` memorizes a single permit, so
    /// a nudge landing between two rounds is not lost (same reasoning as a
    /// transfer's cancel token).
    pub dirsync_wake: tokio::sync::Notify,
    /// Nudges the reach watcher (`dataplane::watch_own_reach`) to re-apply the
    /// transport's claim: joining an account is the one moment a reach becomes
    /// signable without the transport observing anything new, and the watcher
    /// only hears the transport. Same `notify_one` reasoning as `dirsync_wake`.
    pub reach_wake: tokio::sync::Notify,
    pub reconnect_base_delay: std::time::Duration,
    /// Fired by `system.shutdown` (the tray's Quit): a component asked the Core
    /// to stop. The library only signals — the binary awaits it next to the OS
    /// signals and runs the orderly teardown. `notify_one` memorizes a single
    /// permit, so a request that lands before the binary waits is not lost (same
    /// reasoning as a transfer's cancel token).
    pub shutdown_request: tokio::sync::Notify,
}

/// The pending OIDC flow: its `state` (anti-CSRF — also the flow's identity)
/// and the means to stop its task when a new flow replaces it.
pub struct LoginSlot {
    pub state_param: String,
    pub abort: tokio::task::AbortHandle,
}

/// A request proxied to the server, handled by the session task.
pub struct ServerCmd {
    pub method: &'static str,
    pub params: Value,
    pub reply: tokio::sync::oneshot::Sender<Result<Value, RpcErr>>,
}

/// What a device knows about itself with no server involved: its crypto identity,
/// the name it carries, and its attestation under the account key (C7). Enough to
/// hold its own directory record — through a startup with no session, and through
/// the end of one.
///
/// The identity itself rather than just the `node_id`, because the record is
/// SIGNED with it: what this device says about itself is only worth anything to a
/// peer if it comes with the signature of the key that `node_id` is.
#[derive(Clone, Copy)]
pub struct OwnDevice<'a> {
    pub identity: &'a crate::identity::DeviceIdentity,
    pub name: &'a str,
    pub attestation: &'a str,
}

impl OwnDevice<'_> {
    pub fn node_id(&self) -> String {
        self.identity.node_id()
    }
}

/// What taking in a peer's roster changed ([`SessionState::absorb`]) — the four
/// things worth telling anyone about, and the only reasons to touch the disk.
/// Empty is the common case: two devices that already agree exchange no state.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Absorbed {
    /// Devices this Core had never heard of, as they now stand in the directory.
    pub added: Vec<Value>,
    /// Devices whose description this Core replaced with a fresher one.
    pub updated: Vec<Value>,
    /// `device_id`s that left the directory, a tombstone having caught up with
    /// them.
    pub removed: Vec<String>,
    /// `node_id`s newly struck off — which is not the same list: the account may
    /// strike off a device this Core never held, and that tombstone still has to
    /// reach the disk so it bars the record when it eventually shows up.
    pub barred: Vec<String>,
    /// The roster carried the account's signature striking THIS device off, and
    /// nothing else was taken in. The caller's next move is to leave the account
    /// (`account_key::leave`) — everything else in that roster describes an
    /// account this device is out of.
    pub struck_ourselves: bool,
}

impl Absorbed {
    pub fn is_empty(&self) -> bool {
        self.added.is_empty()
            && self.updated.is_empty()
            && self.removed.is_empty()
            && self.barred.is_empty()
            && !self.struck_ourselves
    }
}

/// State of the server session, published on the IPC (`session.status`,
/// `session.changed`, `devices.*`).
pub struct SessionState {
    /// `session.json` present and readable at startup (or not yet abandoned).
    /// Says nothing about the connection.
    pub logged_in: bool,
    /// The `account` field of session.json, replayed as-is.
    pub account: Option<Value>,
    /// Authenticated AND directory snapshotted: connected ⇒ cache primed.
    pub server_connected: bool,
    /// The Core's device_id in the directory.
    pub own_device_id: Option<String>,
    /// The account's directory, keyed by `device_id`: the server's last snapshot
    /// while there is a session, maintained by the `device.*` events — plus, for
    /// a device that has joined the account, its OWN record, which owes nothing
    /// to a server (`adopt_own`). `None` while this Core knows of no device at
    /// all, its own included: there is then nothing honest to serve.
    pub devices: Option<BTreeMap<String, Value>>,
    /// Whom the account has struck off, for good: `node_id` → the account key's
    /// signature over that revocation (`account_key::revoke`). Signatures rather
    /// than a set of ids, because a tombstone is checked before it bars anything
    /// ([`SessionState::barred`]) and passed on to peers as what it is — a
    /// statement anyone can verify.
    ///
    /// Held next to `devices` because the two are always consulted together, and
    /// it survives everything the directory does not: a logout, a revocation of
    /// THIS device, a stale store. See `directory` (`revoked.json`).
    pub revoked: BTreeMap<String, String>,
    /// Sender toward the session task — present when the connection is
    /// established (the `devices.rename`… proxies go through it).
    pub server_tx: Option<mpsc::Sender<ServerCmd>>,
    /// To stop the session task at logout.
    pub session_abort: Option<tokio::task::AbortHandle>,
    /// This device obeyed a tombstone naming it (`account_key::leave`): it is
    /// out, and nothing is absorbed anymore. The flag exists for one race the
    /// trust root alone cannot close: an exchange already past the root read
    /// when the leave ran would otherwise write its records into the directory
    /// the leave just wiped — under this very lock, which is why the flag lives
    /// here and is checked here ([`Self::absorb`]). Never cleared: the way back
    /// is a restart under a fresh identity.
    pub left_the_account: bool,
}

impl SessionState {
    pub fn new(info: Option<&SessionInfo>) -> SessionState {
        SessionState {
            logged_in: info.is_some(),
            account: info.and_then(|i| i.account.clone()),
            server_connected: false,
            own_device_id: info.map(|i| i.device_id.clone()),
            devices: None,
            revoked: BTreeMap::new(),
            server_tx: None,
            session_abort: None,
            left_the_account: false,
        }
    }

    /// The result of `session.status` — also the payload of `session.changed`.
    pub fn status_record(&self) -> Value {
        let mut v = json!({
            "logged_in": self.logged_in,
            "server_connected": self.server_connected,
        });
        if let Some(account) = &self.account {
            v["account"] = account.clone();
        }
        v
    }

    /// Ensures the directory holds this device's OWN record, carrying its
    /// attestation — the half of the directory that owes nothing to a server:
    /// this Core knows its `node_id`, its name and its attestation first-hand.
    ///
    /// Called wherever that becomes true: at startup for a device already in the
    /// account, when it joins one with no server in sight, after a logout (the
    /// session leaves, the account stays) — and on the server path, where it
    /// carries a freshly published attestation onto our own record, which the
    /// server excludes us from hearing back.
    ///
    /// The record is keyed by the server's `device_id` when there is one, and by
    /// our `node_id` otherwise — a self-minted label for a device no server has
    /// named. It has to be the same value `own_device_id` holds, since that is
    /// what `enrich_device` derives `is_self` from.
    ///
    /// The name is NOT refreshed, and neither is the signature over it: a record
    /// already there is kept verbatim, `seq` included. While a session exists the
    /// server owns the name (`devices.rename` can come from another device), and
    /// with no server the description this device signed is the one its peers
    /// have — re-minting it at every startup would hand them a "new" record each
    /// boot, saying nothing new. Changing it is a rename ([`Self::rename_own`]).
    ///
    /// With one exception: a record that CLAIMS our signature without carrying it
    /// is dropped and minted again. That is a record we could not stand behind —
    /// `device.key` changed under us, or the store was edited — and peers verify a
    /// self-signature against the `node_id` itself, so republishing it would mean
    /// offering the account a description nobody accepts and nothing local
    /// notices. A record with NO signature at all is a server's — the server keeps
    /// the name — and this device COUNTERSIGNS it rather than leave it alone: the
    /// name stays the server's word, the signature becomes ours, and the record
    /// can now travel to siblings the server never met (the continuum;
    /// [`crate::directory::countersign_own`]).
    pub fn adopt_own(&mut self, own: OwnDevice<'_>) {
        let node_id = own.node_id();
        let id = self
            .own_device_id
            .get_or_insert_with(|| node_id.clone())
            .clone();
        let devices = self.devices.get_or_insert_with(BTreeMap::new);
        let unsignable = devices.get(&id).is_some_and(|record| {
            record.get("self_sig").is_some() && !crate::directory::verify_record(record)
        });
        if unsignable {
            tracing::warn!("our own directory record does not carry our signature: minted again");
            devices.remove(&id);
        }
        let record = devices.entry(id.clone()).or_insert_with(|| {
            // A first minting claims no reach: the transport has observed
            // nothing yet, and the reach watcher re-signs when it does.
            crate::directory::own_record(&id, own, None, &crate::directory::Reach::default())
        });
        if record.get("self_sig").is_none() {
            crate::directory::countersign_own(record, own.identity);
        }
        record["attestation"] = json!(own.attestation);
        // Our own liveness is the one presence fact we know first-hand, and it is
        // true by construction: this record is being written by the running Core.
        // A stored record that said otherwise would have us report ourselves
        // offline until something else corrected it.
        record["online"] = json!(true);
    }

    /// This device renames ITSELF, with no server to ask: a fresh description
    /// under a strictly higher `seq`, signed again — which is what makes a peer
    /// that already knows us prefer the new one. Returns the record as it now
    /// stands, or `None` if this Core holds no record of its own (nothing to
    /// rename: it has neither joined an account nor been named by a server).
    ///
    /// Only our own: another device's description is not ours to sign, and the
    /// caller is the one that refuses that (`conn::devices_rename`).
    pub fn rename_own(&mut self, own: OwnDevice<'_>, name: &str) -> Option<Value> {
        let id = self.own_device_id.clone()?;
        let devices = self.devices.as_mut()?;
        let held = devices.get(&id)?;
        let previous = held.get("seq").and_then(Value::as_u64);
        // The reach is carried forward: a rename changes the name, not where
        // this device can be dialed. (A held reach that lost the published
        // shape, an edited store, re-mints empty; the watcher restores it.)
        let reach = crate::directory::record_reach(held).unwrap_or_default();
        let renamed =
            crate::directory::own_record(&id, OwnDevice { name, ..own }, previous, &reach);
        devices.insert(id, renamed.clone());
        Some(renamed)
    }

    /// This device's word on how it can be dialed moved: its addresses, or its
    /// chosen relay. The same gesture as a rename: a fresh description under
    /// a strictly higher `seq`, signed again, so peers prefer it. Returns the
    /// record as it now stands when something actually changed; `None` when the
    /// claim is already the one signed (the common case: the watcher re-checks
    /// far more often than networks move), or when this Core has no record of
    /// its own to refresh.
    pub fn refresh_own_reach(
        &mut self,
        identity: &crate::identity::DeviceIdentity,
        reach: &crate::directory::Reach,
    ) -> Option<Value> {
        let id = self.own_device_id.clone()?;
        let record = self.devices.as_mut()?.get_mut(&id)?;
        if crate::directory::record_reach(record).as_ref() == Some(reach) {
            return None;
        }
        // On a candidate, so a description that cannot be countersigned (a
        // store edited into an unsignable shape) is left untouched rather than
        // half-mutated: an unsigned reach would be exactly the unproven claim
        // the whole design refuses. Said out loud: a claim refused HERE means
        // the record's hints stop following the machine, which no user-visible
        // failure would otherwise name.
        let mut candidate = record.clone();
        crate::directory::set_reach(&mut candidate, reach);
        if !crate::directory::countersign_own(&mut candidate, identity) {
            tracing::warn!("our own record cannot be re-signed: its reach hints will go stale");
            return None;
        }
        *record = candidate;
        Some(record.clone())
    }

    /// Has the account struck this `node_id` off? A tombstone counts only once
    /// its signature verifies under `ak_pub` — the account's public key, which
    /// this device derived itself. So a `revoked.json` someone else wrote bars
    /// nobody, and a device with no trust root bars nobody either (it recognizes
    /// nobody at all: fail-closed both ways).
    pub fn barred(&self, ak_pub: &str, node_id: &str) -> bool {
        self.revoked.get(node_id).is_some_and(|revocation| {
            crate::account_key::verify_revocation(ak_pub, node_id, revocation)
        })
    }

    /// Strikes a `node_id` off: keeps the tombstone, and evicts every record that
    /// described it. Returns the `device_id`s that left, for the notifications.
    ///
    /// Both halves matter, and they answer different questions. The eviction is
    /// what a component sees NOW; the tombstone is what refuses the record when it
    /// comes back — from a peer's roster, from a server that was never told, or
    /// from this Core's own store at the next startup.
    pub fn strike_off(&mut self, node_id: &str, revocation: String) -> Vec<String> {
        self.revoked.insert(node_id.to_string(), revocation);
        let Some(devices) = self.devices.as_mut() else {
            return Vec::new();
        };
        let struck: Vec<String> = devices
            .iter()
            .filter(|(_, record)| record.get("node_id").and_then(Value::as_str) == Some(node_id))
            .map(|(id, _)| id.clone())
            .collect();
        for id in &struck {
            devices.remove(id);
        }
        struck
    }

    /// Takes in what a peer knows of the account: the descriptions it holds and
    /// the tombstones it holds. Returns what actually changed — nothing else is
    /// broadcast, and nothing else is written to disk, so a sync round between two
    /// devices that already agree is silent.
    ///
    /// The rules, and each one is load-bearing:
    ///
    /// - **Tombstones first.** A record for a device the same roster strikes off
    ///   must not slip in on the way past.
    /// - **The account key decides, never the relayer.** Every record must be
    ///   [`shareable`](crate::directory::shareable) — signed by the device it
    ///   describes AND attested under our account key — and every tombstone must
    ///   verify under that same key. A peer that hands us anything else has handed
    ///   us nothing.
    /// - **We are the authority on ourselves.** A record describing this device is
    ///   skipped, whatever its `seq`: what a peer holds about us is at best what we
    ///   told it, and adopting a name we never chose is not a merge, it is an
    ///   identity we would then re-sign and hand on.
    /// - **The highest `seq` wins, and only among signed descriptions.** A record
    ///   we hold WITHOUT a `seq` was minted by a server, and there the server owns
    ///   the name (`devices.rename` can come from another device): we leave it
    ///   alone rather than let the data plane overrule it. Equal `seq` changes
    ///   nothing — two devices that agree exchange no state.
    /// - **A device we did not know arrives keyed by its `node_id`**, and knowing
    ///   itself only: see [`as_learned`](crate::directory::as_learned).
    ///
    /// `own_node_id` rather than `own_device_id`: the latter is the server's label
    /// while a session lives, and what a roster names is the crypto identity.
    pub fn absorb(
        &mut self,
        ak_pub: &str,
        own_node_id: &str,
        roster: &crate::directory::Roster,
    ) -> Absorbed {
        let mut out = Absorbed::default();
        // Out of the account: absorbed NOTHING, whatever the roster says. The
        // caller read the trust root before taking this lock, so an exchange
        // that was in flight when the leave ran can reach this point holding a
        // key the device no longer has — and would re-populate the directory
        // the leave just wiped.
        if self.left_the_account {
            return out;
        }
        // The account striking THIS device off. Verified FIRST, under the account
        // key — nothing a peer merely asserts may end an account — and then
        // obeyed: the caller leaves (the trust root, the account key, the
        // session, the directory, `device.key` itself — `account_key::leave`).
        // A short-circuit, deliberately: the rest of the roster describes an
        // account this device is out of, and absorbing it would be writing a
        // directory the leave is about to erase — and announcing arrivals to
        // components about to be told the account is gone. An unverifiable
        // signature naming us falls through and dies at the same check as any
        // other tombstone.
        if let Some(revocation) = roster.revoked.get(own_node_id)
            && crate::account_key::verify_revocation(ak_pub, own_node_id, revocation)
        {
            out.struck_ourselves = true;
            return out;
        }
        for (node_id, revocation) in &roster.revoked {
            if self.revoked.contains_key(node_id) {
                continue;
            }
            // Reached only by a signature that does NOT verify (the valid case
            // returned above): skipped by the check below, like any tombstone the
            // account never signed. Named anyway so `strike_off` can never grow a
            // path that evicts our own record.
            if node_id == own_node_id {
                continue;
            }
            if !crate::account_key::verify_revocation(ak_pub, node_id, revocation) {
                continue;
            }
            out.removed
                .extend(self.strike_off(node_id, revocation.clone()));
            out.barred.push(node_id.clone());
        }

        // Filtered before the map is borrowed mutably, and filtered against the
        // tombstones just applied.
        let acceptable: Vec<&Value> = roster
            .records
            .iter()
            .filter(|record| {
                let Some(node_id) = record.get("node_id").and_then(Value::as_str) else {
                    return false;
                };
                node_id != own_node_id
                    && !self.barred(ak_pub, node_id)
                    && crate::directory::shareable(record, ak_pub)
            })
            .collect();
        if acceptable.is_empty() {
            return out;
        }
        let devices = self.devices.get_or_insert_with(BTreeMap::new);
        for incoming in acceptable {
            let node_id = incoming["node_id"].as_str().expect("filtered above");
            let held = devices.iter().find_map(|(id, record)| {
                (record.get("node_id").and_then(Value::as_str) == Some(node_id)).then(|| id.clone())
            });
            let Some(key) = held else {
                let learned = crate::directory::as_learned(incoming, node_id);
                devices.insert(node_id.to_string(), learned.clone());
                out.added.push(learned);
                continue;
            };
            // A description we hold and did not sign ourselves: only a strictly
            // higher `seq` supersedes it, and only if ours is signed at all.
            let ours = devices[&key].get("seq").and_then(Value::as_u64);
            let theirs = incoming["seq"].as_u64().expect("shareable: a seq");
            if !ours.is_some_and(|ours| theirs > ours) {
                continue;
            }
            let record = devices.get_mut(&key).expect("just found");
            crate::directory::carry_over(record, incoming);
            out.updated.push(record.clone());
        }
        out
    }

    /// The server's snapshot meets the directory this Core already holds — and
    /// since the continuum, holding is not the same as owing: part of what is
    /// here may be the ACCOUNT's, learned from siblings a server never met, and a
    /// snapshot that replaced the map wholesale would erase them at every
    /// reconnection.
    ///
    /// So the snapshot is absorbed as what it is — the server's word about the
    /// server's half:
    ///
    /// - **A device the server lists is the server's word entirely** — name,
    ///   route, presence. What the fresh record may lack is the signed
    ///   description ([`crate::directory::graft_signature`]): an old server
    ///   carries none, and the one this Core held still proves itself if it
    ///   matches, so it is grafted on rather than lost. A held description that
    ///   no longer matches (the device was renamed while away) is NOT kept —
    ///   the record goes unsigned until its own device countersigns.
    /// - **A device the server does not list stays, if it can prove itself**:
    ///   signed by the device it describes and attested under the account key —
    ///   the serverless-only siblings, which are the continuum's point. It is
    ///   re-keyed by its `node_id` and stripped of every present-tense fact
    ///   ([`crate::directory::as_learned`]): nothing vouches for its route or
    ///   liveness anymore. A record that cannot prove itself leaves with the
    ///   snapshot, exactly as before the continuum.
    /// - **Without a trust root the snapshot replaces the map**, as it always
    ///   has: nothing can prove itself to a device that can verify nothing.
    ///
    /// The tombstones are not consulted here: the caller filters the snapshot
    /// against them first (nothing barred comes in), and a barred device's held
    /// records were evicted the moment the tombstone was absorbed.
    pub fn merge_snapshot(&mut self, mut fresh: BTreeMap<String, Value>, ak_pub: Option<&str>) {
        let held = self.devices.take();
        if let (Some(held), Some(ak_pub)) = (held, ak_pub) {
            for record in held.into_values() {
                let Some(node_id) = record.get("node_id").and_then(Value::as_str) else {
                    continue;
                };
                let twin = fresh
                    .values_mut()
                    .find(|f| f.get("node_id").and_then(Value::as_str) == Some(node_id));
                if let Some(twin) = twin {
                    crate::directory::graft_signature(twin, &record);
                } else if crate::directory::shareable(&record, ak_pub) {
                    fresh.insert(
                        node_id.to_string(),
                        crate::directory::as_learned(&record, node_id),
                    );
                }
            }
        }
        self.devices = Some(fresh);
    }

    /// The server has just named this device (a login completed): our own record
    /// moves to the label it gave. Whatever the old one was — the `node_id` a
    /// serverless startup minted, or a previous session's — it described US, and
    /// left where it was it would show this device to itself as a STRANGER
    /// (`enrich_device` compares `device_id`s) for as long as it takes the first
    /// snapshot to replace the map.
    ///
    /// The record is carried over rather than dropped: what we know first-hand
    /// (our attestation, our name) is true under either label, and the snapshot
    /// will overwrite it in a moment anyway.
    pub fn rekey_own(&mut self, device_id: String) {
        let previous = self.own_device_id.replace(device_id.clone());
        let (Some(previous), Some(devices)) = (previous, self.devices.as_mut()) else {
            return;
        };
        if previous == device_id {
            return;
        }
        if let Some(mut record) = devices.remove(&previous) {
            record["device_id"] = json!(device_id);
            devices.insert(device_id, record);
        }
    }

    /// Forgets the session (logout, revocation); returns the notification
    /// payload and the task's stop handle, if there is one.
    ///
    /// `own` is what this device knows about itself with no server involved —
    /// `Some` whenever it holds the account's trust root. The session goes, the
    /// ACCOUNT does not: it keeps knowing itself, under a self-minted
    /// `device_id`, the server's label having left with the session. Taken as a
    /// parameter rather than read here, because `account_root` is a leaf lock and
    /// this runs under `session`.
    ///
    /// `ak_pub` widens that survival to the whole of the account's half (the
    /// continuum): every record that proves itself under it — signed by the
    /// device it describes, attested by the account — outlives the session too,
    /// re-keyed by its `node_id` and stripped of the present-tense facts only the
    /// server could vouch for. What the server alone asserted leaves with it,
    /// exactly as everything did before the continuum. `None` keeps nothing but
    /// `own` — the caller that ends the MEMBERSHIP, not just the session
    /// ([`crate::account_key::leave`]), passes neither.
    ///
    /// The tombstones stay untouched, for the same reason: a session ending says
    /// nothing about whom the account struck off, and forgetting a revocation is
    /// the one mistake that cannot be corrected later — the struck-off device
    /// still holds a perfectly valid attestation.
    pub fn forget(
        &mut self,
        own: Option<OwnDevice<'_>>,
        ak_pub: Option<&str>,
    ) -> (Value, Option<tokio::task::AbortHandle>) {
        self.logged_in = false;
        self.account = None;
        self.server_connected = false;
        self.own_device_id = None;
        self.server_tx = None;
        let held = self.devices.take();
        self.devices = match (held, ak_pub) {
            (Some(held), Some(ak_pub)) => {
                let kept: BTreeMap<String, Value> = held
                    .into_values()
                    .filter(|record| crate::directory::shareable(record, ak_pub))
                    .filter_map(|record| {
                        let node_id = record.get("node_id")?.as_str()?.to_string();
                        let learned = crate::directory::as_learned(&record, &node_id);
                        Some((node_id, learned))
                    })
                    .collect();
                (!kept.is_empty()).then_some(kept)
            }
            _ => None,
        };
        if let Some(own) = own {
            self.adopt_own(own);
        }
        let abort = self.session_abort.take();
        (self.status_record(), abort)
    }
}

/// A transfer in progress, cancelable. No `AbortHandle`: we signal the task
/// rather than kill it, so it cleans up properly (stream reset, `.part`
/// temporaries erased, terminal notification emitted ONCE — by the task itself,
/// never by `files.cancel`).
pub struct TransferEntry {
    /// Signaled by `files.cancel`. `notify_one`: a signal posted before the
    /// task waits is not lost (a memorized permit).
    pub cancel: std::sync::Arc<tokio::sync::Notify>,
}

/// Registry of transfers (T2), outgoing and incoming. LEAF lock (see the module
/// header): never held with `session`/`registry`/`account_root`.
pub struct Transfers {
    pub entries: HashMap<String, TransferEntry>,
}

impl Transfers {
    pub fn new() -> Transfers {
        Transfers {
            entries: HashMap::new(),
        }
    }

    /// Mints a `transfer_id` and registers the transfer; returns its
    /// cancellation token (which the task `select!`s on, and `files.cancel`
    /// triggers). The id is RANDOM, not sequential: a component cannot guess and
    /// cancel another's transfer by enumerating `t_1`, `t_2`…
    pub fn register(&mut self) -> (String, std::sync::Arc<tokio::sync::Notify>) {
        let id = format!("t_{}", random_hex(8));
        let cancel = std::sync::Arc::new(tokio::sync::Notify::new());
        self.entries.insert(
            id.clone(),
            TransferEntry {
                cancel: cancel.clone(),
            },
        );
        (id, cancel)
    }
}

/// Is there **no server in this device's life at all**? Nothing configured, and
/// no session either — a session carries its own server URL (`session.json`), so a
/// Core whose `config.json` went missing under its feet still has a server it
/// answers to, and every other device of the account reads that server.
///
/// This is what every local-only path turns on: creating an account, renaming
/// oneself, striking a device off, and pairing on the local network. Getting it
/// wrong the lenient way would be the expensive mistake — this device would sign a
/// name, strike a device off, or hand the account key to a new device in a way the
/// server, and therefore the rest of the account, would never hear about.
pub fn serverless(state: &AppState) -> bool {
    let configured = state
        .server_config
        .lock()
        .expect("lock server_config")
        .is_some();
    if configured {
        return false;
    }
    !state.session.lock().expect("lock session").logged_in
}

/// A device record as served on the IPC: the server's, enriched with `is_self`
/// (doc/core-api.md, "devices.*").
pub fn enrich_device(
    record: &Value,
    own_device_id: Option<&str>,
    server_connected: bool,
    lan: &std::collections::BTreeSet<String>,
) -> Value {
    let mut v = record.clone();
    let is_self =
        own_device_id.is_some() && record.get("device_id").and_then(Value::as_str) == own_device_id;
    v["is_self"] = json!(is_self);
    // First-hand presence: the transport itself hears this device on the
    // local network right now (mDNS). Unlike `online`, it owes nothing to the
    // server link.
    let lan_visible = record
        .get("node_id")
        .and_then(Value::as_str)
        .is_some_and(|node_id| lan.contains(node_id));
    v["lan"] = json!(lan_visible);
    // What a send could reach RIGHT NOW, with fresh knowledge: the LAN, or —
    // only while the server link that feeds the `online` flags is up — an
    // online device with a published relay (coming online and publishing a
    // relay are two separate steps, so there is a window where a peer is
    // online and routeless). Derived HERE, once, so no consumer re-assembles
    // presence from parts that go stale at different times.
    let online = record
        .get("online")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let has_relay = record
        .get("relay_url")
        .and_then(Value::as_str)
        .is_some_and(|url| !url.trim().is_empty());
    // Third route: the device's own signed word on where it can be dialed
    // (`addrs`, `relay_hint`). No liveness rides on it (the claim is "worth
    // trying", the dial is what answers), so it counts with no server link
    // and no `online` flag: that absence of an authority is exactly the
    // situation the hints exist for.
    let has_hints = crate::directory::record_reach(record).is_some_and(|reach| !reach.is_empty());
    v["reachable"] = json!(lan_visible || (server_connected && online && has_relay) || has_hints);
    v
}

/// Rights carried by a spawn token (bootstrap path B), single-use.
pub struct SpawnGrant {
    pub role: String,
    pub scopes: Vec<String>,
}

/// What a `channel_token` opens, and what the Core needs to serve it. A sum
/// type rather than a flat record with optional fields: the kinds share the
/// token machinery and nothing else, so each carries exactly its own means.
pub enum ChannelKind {
    /// Destination side of a transaction: the component pulls
    /// (`transactions.open`).
    Consumer { tx_id: String },
    /// Source side of a transaction: the backend pushes an inline blob
    /// (`clipboard.get_data`). The sink is where the blob is forwarded to the
    /// consumer's waiting `FETCH`.
    Provider {
        sink: mpsc::Sender<crate::datachannel::ProviderMsg>,
    },
    /// The local end of a peer channel THIS device opened (`peers.channel`,
    /// #124): the peer stream is already open and parked, waiting for its
    /// component to come and take it.
    PeerOpen(crate::peerchannel::Parked),
    /// The local end of a peer channel a PEER opened to us: the handler still
    /// holds the peer stream, so the attaching component hands its own halves
    /// back to it instead.
    PeerJoin(crate::peerchannel::Joining),
}

/// Rights carried by a `channel_token`: what it opens, and the pid of the
/// component it was minted for (peer-credential binding: the data connection
/// carries no `hello`). Single-use, taken at the attach.
pub struct ChannelGrant {
    pub kind: ChannelKind,
    pub pid: Option<u32>,
    /// The connection that owns this grant (the opener for a consumer, the
    /// announcer for a provider): reclaimed when it tears down, so a token that
    /// is never attached does not linger.
    pub conn_id: ConnId,
}

/// An enrolled third-party component: the token persists beyond the connection.
/// (v1: in memory only — a Core restart forgets the enrollment.)
pub struct Enrolled {
    pub component_id: String,
    pub token: String,
    pub name: String,
    pub role: String,
    pub scopes: Vec<String>,
}

/// An enrollment request awaiting the user's decision.
pub struct PendingRequest {
    pub request_id: String,
    pub conn_id: ConnId,
    pub name: String,
    pub role: String,
    pub scopes: Vec<String>,
    /// `peer_info` as broadcast (pid… drawn from the peer credentials).
    pub peer_info: Value,
}

impl PendingRequest {
    pub fn record(&self) -> Value {
        json!({
            "request_id": self.request_id,
            "name": self.name,
            "role": self.role,
            "scopes": self.scopes,
            "peer_info": self.peer_info,
        })
    }
}

/// A connection whose `hello` has been accepted.
pub struct Active {
    pub component_id: String,
    pub name: String,
    pub role: String,
    /// Granted scopes, in the order of the request.
    pub scopes: Vec<String>,
    /// Current subscription (`events.subscribe` replaces it). Read by the topic
    /// broadcast, wired with the server session (next building block).
    pub topics: Vec<String>,
}

impl Active {
    pub fn has_scope(&self, scope: &str) -> bool {
        self.scopes.iter().any(|s| s == scope)
    }
}

pub enum Phase {
    /// Connected, no `hello` accepted.
    Fresh,
    /// `hello` without a token: request queued, referenced by its request_id.
    Pending(String),
    Active(Active),
}

pub struct ConnEntry {
    pub tx: mpsc::Sender<OutMsg>,
    pub phase: Phase,
    /// The peer's pid (best-effort per platform): a data channel this connection
    /// mints (`clipboard.get_data`'s provider token) is bound to it.
    pub pid: Option<u32>,
    /// Core→component requests awaiting their reply (`clipboard.get_data`):
    /// `id` → the waiter to resolve when the response frame comes back. Dropped
    /// at teardown, which unblocks any waiter (the caller then treats it as the
    /// component being gone).
    pub pending: HashMap<u64, tokio::sync::oneshot::Sender<Result<Value, RpcErr>>>,
}

pub struct Registry {
    next_conn_id: ConnId,
    /// Trust root A: the contents of `ipc-token`, reusable.
    pub file_token: String,
    /// Trust root B: tokens passed at spawn, consumed at the first accepted
    /// `hello`.
    pub spawn_tokens: HashMap<String, SpawnGrant>,
    /// persistent token → component_id.
    pub enrolled_tokens: HashMap<String, String>,
    /// Live data-channel tokens (`transactions.open`, `clipboard.get_data`),
    /// consumed at the attach.
    pub channel_tokens: HashMap<String, ChannelGrant>,
    /// component_id → enrolled third-party component. BTreeMap: stable
    /// inventory.
    pub enrolled: BTreeMap<String, Enrolled>,
    /// request_id → request. BTreeMap: stable `components.pending`.
    pub pending: BTreeMap<String, PendingRequest>,
    pub conns: HashMap<ConnId, ConnEntry>,
    /// Counter for the ids of Core→component requests (`issue_request`). Its own
    /// namespace: a response frame (no `method`) is matched here, a component's
    /// request (a `method`) is dispatched — no collision even on equal numbers.
    next_request_id: u64,
    /// The Core is shutting down (drop of the `CoreHandle`). Set and read under
    /// this lock: a connection that registers sees either `false` (it will
    /// receive the Close from the sweep) or `true` (it gives up on its own) —
    /// no window where it would survive the shutdown.
    pub shutdown: bool,
}

impl Registry {
    pub fn new(file_token: String) -> Registry {
        Registry {
            next_conn_id: 0,
            file_token,
            spawn_tokens: HashMap::new(),
            enrolled_tokens: HashMap::new(),
            channel_tokens: HashMap::new(),
            enrolled: BTreeMap::new(),
            pending: BTreeMap::new(),
            conns: HashMap::new(),
            next_request_id: 0,
            shutdown: false,
        }
    }

    pub fn new_conn_id(&mut self) -> ConnId {
        self.next_conn_id += 1;
        self.next_conn_id
    }

    /// Mints a data-channel token bound to `grant`. Unguessable (32 random
    /// bytes) — possession is the authorization, like `tx_id`.
    pub fn mint_channel_token(&mut self, grant: ChannelGrant) -> String {
        let token = random_hex(32);
        self.channel_tokens.insert(token.clone(), grant);
        token
    }

    /// Consumes a data-channel token (single-use).
    pub fn take_channel_token(&mut self, token: &str) -> Option<ChannelGrant> {
        self.channel_tokens.remove(token)
    }

    /// Issues a request TO an active component and returns the receiver for its
    /// reply. `None` if the connection is gone or not active (the caller then
    /// treats it as "the component cannot answer"). The waiter is dropped at the
    /// connection's teardown, which unblocks the receiver.
    pub fn issue_request(
        &mut self,
        conn_id: ConnId,
        method: &str,
        params: Value,
    ) -> Option<(u64, tokio::sync::oneshot::Receiver<Result<Value, RpcErr>>)> {
        let entry = self.conns.get_mut(&conn_id)?;
        if !matches!(entry.phase, Phase::Active(_)) {
            return None;
        }
        self.next_request_id += 1;
        let id = self.next_request_id;
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        let frame = rpc::request(method, id, &params);
        if entry.tx.try_send(OutMsg::Frame(frame)).is_err() {
            return None;
        }
        entry.pending.insert(id, reply_tx);
        Some((id, reply_rx))
    }

    /// Drops a still-outstanding request waiter — the caller no longer needs the
    /// reply (it settled on the data-channel outcome). Without it, a reply the
    /// backend never sends would leave the waiter until the connection closes.
    pub fn cancel_request(&mut self, conn_id: ConnId, id: u64) {
        if let Some(entry) = self.conns.get_mut(&conn_id) {
            entry.pending.remove(&id);
        }
    }

    /// Reclaims every data-channel token owned by a connection — called at its
    /// teardown, so a token minted but never attached (a paste abandoned before
    /// the second connection) does not linger.
    pub fn drop_channel_tokens_of(&mut self, conn_id: ConnId) {
        self.channel_tokens.retain(|_, g| g.conn_id != conn_id);
    }

    /// Every active connection holding `role` with `scope`, each with its pid:
    /// the `peer.channel` fan-out (#124). One frame cannot serve here the way
    /// `peer.message`'s broadcast does, because a channel token is bound to one
    /// component: each candidate is offered its own.
    pub fn conns_for_role_scope(&self, role: &str, scope: &str) -> Vec<(ConnId, Option<u32>)> {
        self.conns
            .iter()
            .filter_map(|(id, e)| match &e.phase {
                Phase::Active(a) if a.role == role && a.has_scope(scope) => Some((*id, e.pid)),
                _ => None,
            })
            .collect()
    }

    /// Delivers a component's reply to the waiter that issued the request (a
    /// response frame: an `id` we minted, no `method`). No-op if the id is
    /// unknown (a stray or late reply).
    pub fn deliver_response(&mut self, conn_id: ConnId, id: u64, outcome: Result<Value, RpcErr>) {
        if let Some(entry) = self.conns.get_mut(&conn_id)
            && let Some(waiter) = entry.pending.remove(&id)
        {
            let _ = waiter.send(outcome);
        }
    }

    /// Is the exclusive role already held by an active connection?
    pub fn role_taken(&self, role: &str) -> bool {
        self.conns
            .values()
            .any(|c| matches!(&c.phase, Phase::Active(a) if a.role == role))
    }

    /// The active connection holding `role` with `scope` - the `sync.*`
    /// facade's routing lookup. For an exclusive role there is at most one;
    /// were there several (a non-exclusive role someday), any one serves.
    pub fn conn_for_role_scope(&self, role: &str, scope: &str) -> Option<ConnId> {
        self.conns.iter().find_map(|(id, e)| match &e.phase {
            Phase::Active(a) if a.role == role && a.has_scope(scope) => Some(*id),
            _ => None,
        })
    }

    /// Pushes a notification to all active connections carrying `scope` — with
    /// no subscription: it is the duty attached to the scope
    /// (`component.pending` → `components.approve`).
    pub fn notify_scope(&self, scope: &str, method: &str, params: &Value) {
        let frame = rpc::notification(method, params);
        for entry in self.conns.values() {
            if let Phase::Active(a) = &entry.phase
                && a.has_scope(scope)
            {
                // Queue full or dying connection: too bad for it, the snapshot
                // (`components.pending`) catches up.
                let _ = entry.tx.try_send(OutMsg::Frame(frame.clone()));
            }
        }
    }

    /// Pushes a notification to every active connection holding BOTH `role`
    /// and `scope` - the `peer.message` delivery (a peer message lands on the
    /// components holding the same role as its sender, doc/core-api.md
    /// "peers.*"; no subscription, like `component.pending`). Returns whether
    /// at least one target existed - the wire ack's `delivered`; the
    /// queue-full drop stays best-effort, as for every notification.
    pub fn notify_role_scope(&self, role: &str, scope: &str, method: &str, params: &Value) -> bool {
        let frame = rpc::notification(method, params);
        let mut delivered = false;
        for entry in self.conns.values() {
            if let Phase::Active(a) = &entry.phase
                && a.role == role
                && a.has_scope(scope)
            {
                delivered = true;
                let _ = entry.tx.try_send(OutMsg::Frame(frame.clone()));
            }
        }
        delivered
    }

    /// Pushes a notification to the active connections subscribed to `topic`
    /// (the scope was verified at subscription; a connection's scopes do not
    /// change).
    pub fn notify_topic(&self, topic: &str, method: &str, params: &Value) {
        let frame = rpc::notification(method, params);
        for entry in self.conns.values() {
            if let Phase::Active(a) = &entry.phase
                && a.topics.iter().any(|t| t == topic)
            {
                // Queue full or dying connection: too bad for it, the snapshots
                // (`devices.list`, `session.status`) catch up.
                let _ = entry.tx.try_send(OutMsg::Frame(frame.clone()));
            }
        }
    }

    /// Sends a notification to a specific connection.
    pub fn notify_conn(&self, conn_id: ConnId, method: &str, params: &Value) {
        if let Some(entry) = self.conns.get(&conn_id) {
            let _ = entry
                .tx
                .try_send(OutMsg::Frame(rpc::notification(method, params)));
        }
    }

    /// Requests the closure of a connection (curt: EOF on the peer side).
    pub fn close_conn(&self, conn_id: ConnId) {
        if let Some(entry) = self.conns.get(&conn_id) {
            let _ = entry.tx.try_send(OutMsg::Close);
        }
    }
}

pub fn random_hex(bytes: usize) -> String {
    let mut buf = vec![0u8; bytes];
    rand::Rng::fill_bytes(&mut rand::rng(), &mut buf);
    hex::encode(buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The identity every test here belongs to, and the record it signs.
    fn identity() -> crate::identity::DeviceIdentity {
        crate::identity::DeviceIdentity::from_test_seed(7)
    }

    fn own_device(identity: &crate::identity::DeviceIdentity) -> OwnDevice<'_> {
        OwnDevice {
            identity,
            name: "Office-PC",
            attestation: "sig",
        }
    }

    /// What a Core in the account holds about itself before any server has named
    /// it — the serverless startup state.
    fn in_account_without_a_session(identity: &crate::identity::DeviceIdentity) -> SessionState {
        let mut s = SessionState::new(None);
        s.adopt_own(own_device(identity));
        s
    }

    #[test]
    fn a_device_in_the_account_knows_itself_under_its_own_node_id() {
        let identity = identity();
        let s = in_account_without_a_session(&identity);

        let node_id = identity.node_id();
        assert_eq!(s.own_device_id.as_deref(), Some(node_id.as_str()));
        let devices = s.devices.expect("a directory");
        let own = devices.get(&node_id).expect("our own record");
        assert_eq!(own["node_id"], json!(node_id));
        assert_eq!(own["name"], json!("Office-PC"));
        assert_eq!(own["attestation"], json!("sig"));
        assert!(
            crate::directory::verify_record(own),
            "and stands behind that description with its own key: {own}"
        );
    }

    /// A description already signed is kept as it is — name, `seq` and signature.
    /// Re-minting it at every startup would hand peers a record to take in and
    /// store again, saying nothing they did not know; worse, it would undo a
    /// rename, since what this Core knows at startup is the name it booted with.
    #[test]
    fn a_description_already_signed_is_not_signed_again() {
        let identity = identity();
        let mut s = in_account_without_a_session(&identity);
        // A record no fresh minting would produce: renamed, hence a name that is
        // not the one `OwnDevice` carries and a `seq` past the current second.
        s.rename_own(own_device(&identity), "Atelier")
            .expect("our own record");
        let before = s.devices.clone();

        s.adopt_own(own_device(&identity));

        assert_eq!(s.devices, before, "kept verbatim, rename included");
    }

    /// A description we cannot stand behind is not republished. Its signature is
    /// checked against our own `node_id` — exactly as a peer would check it — and a
    /// record claiming one it does not carry is minted again.
    #[test]
    fn a_description_we_cannot_sign_for_is_minted_again() {
        let identity = identity();
        let node_id = identity.node_id();
        let mut s = SessionState::new(None);
        s.own_device_id = Some(node_id.clone());
        // Signed by ANOTHER device — a tampered store, or one carried over from an
        // identity this machine no longer has.
        let mut forged = crate::directory::own_record(
            &node_id,
            OwnDevice {
                identity: &crate::identity::DeviceIdentity::from_test_seed(9),
                name: "Someone-Else",
                attestation: "sig",
            },
            None,
            &crate::directory::Reach::default(),
        );
        forged["node_id"] = json!(node_id);
        assert!(!crate::directory::verify_record(&forged), "the premise");
        s.devices = Some(BTreeMap::from([(node_id.clone(), forged)]));

        s.adopt_own(own_device(&identity));

        let record = &s.devices.as_ref().expect("a directory")[&node_id];
        assert_eq!(record["name"], json!("Office-PC"), "ours again");
        assert!(crate::directory::verify_record(record));
    }

    /// A record with NO signature is a server's, and the server keeps the name —
    /// which this device COUNTERSIGNS rather than contest (the continuum): the
    /// name stays the server's word, the signature becomes ours, and the record
    /// can now travel to siblings the server never met.
    #[test]
    fn a_record_a_server_minted_keeps_its_name_and_gains_our_signature() {
        let identity = identity();
        let mut s = SessionState::new(Some(&crate::session::SessionInfo {
            server_url: "ws://server/ws".to_string(),
            device_id: "d_7f3a".to_string(),
            account: None,
        }));
        let from_server = json!({
            "device_id": "d_7f3a", "node_id": identity.node_id(),
            "name": "Named-By-The-Server", "platform": "linux", "online": true,
        });
        s.devices = Some(BTreeMap::from([("d_7f3a".to_string(), from_server)]));

        s.adopt_own(own_device(&identity));

        let record = &s.devices.as_ref().expect("a directory")["d_7f3a"];
        assert_eq!(
            record["name"],
            json!("Named-By-The-Server"),
            "the name is still the server's word"
        );
        assert_eq!(record["attestation"], json!("sig"), "carried onto it");
        assert!(
            crate::directory::verify_record(record),
            "and the description now proves itself: {record}"
        );
    }

    /// The one thing `adopt_own` does assert about an existing record: this Core
    /// is running, so it is online. A stored record that said otherwise would have
    /// us reporting ourselves offline.
    #[test]
    fn adopting_our_own_record_asserts_that_we_are_running() {
        let identity = identity();
        let mut s = SessionState::new(None);
        let node_id = identity.node_id();
        s.own_device_id = Some(node_id.clone());
        s.devices = Some(BTreeMap::from([(
            node_id.clone(),
            json!({ "device_id": node_id, "node_id": node_id, "online": false }),
        )]));

        s.adopt_own(own_device(&identity));

        assert_eq!(
            s.devices.as_ref().expect("a directory")[&node_id]["online"],
            json!(true)
        );
    }

    /// Renaming is the one thing that DOES re-sign: a fresh description, a
    /// strictly higher `seq` (two renames in the same second still order), and a
    /// signature a peer can check before preferring it.
    #[test]
    fn a_rename_supersedes_the_description_it_replaces() {
        let identity = identity();
        let mut s = in_account_without_a_session(&identity);
        let before = s.devices.as_ref().expect("a directory")[&identity.node_id()].clone();

        let first = s
            .rename_own(own_device(&identity), "Laptop")
            .expect("our own record to rename");
        let second = s
            .rename_own(own_device(&identity), "Laptop-2")
            .expect("and again");

        assert_eq!(first["name"], json!("Laptop"));
        assert_eq!(second["name"], json!("Laptop-2"));
        assert!(crate::directory::verify_record(&first));
        assert!(crate::directory::verify_record(&second));
        let seq = |record: &Value| record["seq"].as_u64().expect("a seq");
        assert!(seq(&first) > seq(&before), "{first} over {before}");
        assert!(seq(&second) > seq(&first), "{second} over {first}");
        // And what the directory holds is the last one, under the same key.
        let devices = s.devices.as_ref().expect("a directory");
        assert_eq!(devices.len(), 1);
        assert_eq!(devices[&identity.node_id()], second);
    }

    /// A device with nothing of its own has nothing to rename — no silent
    /// invention of a record here.
    #[test]
    fn a_device_with_no_record_of_its_own_renames_nothing() {
        let identity = identity();
        let mut s = SessionState::new(None);

        assert!(s.rename_own(own_device(&identity), "Laptop").is_none());
        assert!(s.devices.is_none());
    }

    /// A reach that moved is re-signed and supersedes, exactly like a rename;
    /// one that did not move re-signs nothing: the watcher re-checks far more
    /// often than networks change, and every needless re-mint is a "new" record
    /// the whole account would store and relay for nothing.
    #[test]
    fn a_reach_that_moved_supersedes_and_one_that_did_not_is_silent() {
        let identity = identity();
        let mut s = in_account_without_a_session(&identity);
        let before_seq = s.devices.as_ref().expect("a directory")[&identity.node_id()]["seq"]
            .as_u64()
            .expect("a seq");
        let reach = crate::directory::Reach {
            addrs: vec!["192.0.2.7:41641".to_string()],
            relay_hint: Some("https://relay.example".to_string()),
        };

        let refreshed = s
            .refresh_own_reach(&identity, &reach)
            .expect("a moved reach re-signs");
        assert_eq!(refreshed["addrs"], json!(["192.0.2.7:41641"]));
        assert_eq!(refreshed["relay_hint"], json!("https://relay.example"));
        assert!(refreshed["seq"].as_u64().expect("a seq") > before_seq);
        assert!(crate::directory::verify_record(&refreshed), "{refreshed}");

        // The same claim again: nothing.
        assert!(s.refresh_own_reach(&identity, &reach).is_none());
        // And the name it signs is the one the record carries: a rename that
        // follows keeps the reach, a refresh that follows keeps the name.
        let renamed = s
            .rename_own(own_device(&identity), "Atelier")
            .expect("our own record");
        assert_eq!(renamed["addrs"], json!(["192.0.2.7:41641"]));
        assert!(crate::directory::verify_record(&renamed), "{renamed}");

        // A device with no record of its own refreshes nothing.
        let mut fresh = SessionState::new(None);
        assert!(fresh.refresh_own_reach(&identity, &reach).is_none());
    }

    /// The signed hints are a route of their own: a device whose record carries
    /// them is worth dialing with no server link, no `online` flag and no LAN
    /// sighting, and one whose record carries none is exactly as unreachable
    /// as before they existed.
    #[test]
    fn signed_hints_make_a_device_reachable_on_their_own() {
        let (ak, _) = account();
        let bare = sibling(8, "Atelier", 5, &ak);
        let reaching = {
            let identity = key(9);
            let attestation = crate::account_key::attest(&ak, &identity.node_id());
            crate::directory::signed_record_reaching(
                &identity,
                "Nomad",
                5,
                &attestation,
                &crate::directory::Reach {
                    addrs: vec!["203.0.113.9:41641".to_string()],
                    relay_hint: None,
                },
            )
        };
        let lan = std::collections::BTreeSet::new();

        let bare = enrich_device(&bare, None, false, &lan);
        assert_eq!(bare["reachable"], json!(false));
        let reaching = enrich_device(&reaching, None, false, &lan);
        assert_eq!(reaching["reachable"], json!(true), "{reaching}");
        assert_eq!(reaching["lan"], json!(false), "not a LAN claim");
    }

    /// The label the server hands out replaces the self-minted one, and the record
    /// follows it: a device must never appear in its own directory as a stranger.
    #[test]
    fn a_server_naming_this_device_carries_its_record_over() {
        let identity = identity();
        let mut s = in_account_without_a_session(&identity);

        s.rekey_own("d_7f3a".to_string());

        assert_eq!(s.own_device_id.as_deref(), Some("d_7f3a"));
        let devices = s.devices.expect("a directory");
        assert_eq!(devices.len(), 1, "one device, not two: {devices:?}");
        let own = devices.get("d_7f3a").expect("our record, re-keyed");
        assert_eq!(own["device_id"], json!("d_7f3a"), "the record says so too");
        assert_eq!(own["attestation"], json!("sig"), "what we knew is kept");
    }

    /// A session that ends (logout, revocation) leaves the account behind: the
    /// device goes back to knowing itself under its own `node_id`.
    #[test]
    fn the_end_of_a_session_leaves_the_account_knowing_itself() {
        let mut s = SessionState::new(Some(&crate::session::SessionInfo {
            server_url: "ws://server/ws".to_string(),
            device_id: "d_7f3a".to_string(),
            account: None,
        }));
        let identity = identity();
        let node_id = identity.node_id();
        s.devices = Some(BTreeMap::from([(
            "d_7f3a".to_string(),
            json!({ "device_id": "d_7f3a", "node_id": node_id }),
        )]));
        // A device the account struck off while this session lived.
        s.revoked = BTreeMap::from([("cd34".to_string(), "revocation".to_string())]);

        let (payload, _) = s.forget(Some(own_device(&identity)), None);

        assert_eq!(payload["logged_in"], json!(false));
        assert_eq!(
            s.own_device_id.as_deref(),
            Some(node_id.as_str()),
            "the server's label left with the session"
        );
        let devices = s.devices.as_ref().expect("still a directory");
        assert_eq!(devices.len(), 1);
        assert!(devices.contains_key(&node_id));
        assert!(
            s.revoked.contains_key("cd34"),
            "a session ends; a revocation does not"
        );
    }

    /// Once this device has left the account, an exchange still in flight — one
    /// that read the trust root before the leave took it — absorbs NOTHING: not
    /// a record into the directory the leave wiped, and not a second order to
    /// leave (the human would be told twice).
    #[test]
    fn a_device_out_of_the_account_absorbs_nothing() {
        let (ak, ak_pub) = account();
        let identity = identity();
        let mut s = SessionState::new(None);
        s.left_the_account = true;

        let mut roster = roster(vec![sibling(8, "Laptop", 5, &ak)]);
        roster.revoked.insert(
            identity.node_id(),
            crate::account_key::revoke(&ak, &identity.node_id()),
        );
        let taken = s.absorb(&ak_pub, &identity.node_id(), &roster);

        assert!(taken.is_empty(), "{taken:?}");
        assert!(!taken.struck_ourselves, "left once, not twice");
        assert!(s.devices.is_none(), "the wiped directory stays wiped");
        assert!(s.revoked.is_empty());
    }

    /// The server's snapshot is merged, not swapped in (the continuum): what the
    /// account can prove about devices the server never met survives it, a held
    /// proof is grafted onto the fresh record of a device both halves know, and
    /// everything the server alone asserted is the server's word entirely.
    #[test]
    fn a_snapshot_merges_the_servers_half_with_the_accounts() {
        let (ak, ak_pub) = account();
        let identity = identity();
        let mut s = in_account_without_a_session(&identity);
        let devices = s.devices.as_mut().expect("a directory");
        // A sibling only the account knows: signed and attested, with
        // present-tense claims picked up first-hand since.
        let mut lan_only = sibling(8, "Atelier", 5, &ak);
        lan_only["relay_url"] = json!("https://relay.example");
        lan_only["online"] = json!(true);
        devices.insert(key(8).node_id(), lan_only);
        // A device only the SERVER ever vouched for: no signature — it leaves
        // with the snapshot that brought it.
        devices.insert(
            "d_old".to_string(),
            json!({
                "device_id": "d_old", "node_id": key(9).node_id(),
                "name": "Server-Only", "platform": "linux",
            }),
        );
        // Two devices both halves know: one whose held proof still covers the
        // fresh description, one renamed while we were away.
        devices.insert(key(10).node_id(), sibling(10, "Laptop", 7, &ak));
        devices.insert(key(11).node_id(), sibling(11, "Old-Name", 4, &ak));
        // The server named us at login.
        s.rekey_own("d_us".to_string());

        let attest = |seed: u8| crate::account_key::attest(&ak, &key(seed).node_id());
        let fresh = BTreeMap::from([
            (
                "d_us".to_string(),
                json!({
                    "device_id": "d_us", "node_id": identity.node_id(),
                    "name": "Office-PC", "platform": std::env::consts::OS,
                    "online": true,
                }),
            ),
            (
                "d_10".to_string(),
                json!({
                    "device_id": "d_10", "node_id": key(10).node_id(),
                    "name": "Laptop", "platform": "linux",
                    "attestation": attest(10), "online": true,
                }),
            ),
            (
                "d_11".to_string(),
                json!({
                    "device_id": "d_11", "node_id": key(11).node_id(),
                    "name": "Renamed-By-The-Server", "platform": "linux",
                    "attestation": attest(11), "online": true,
                }),
            ),
        ]);

        s.merge_snapshot(fresh, Some(&ak_pub));

        let devices = s.devices.as_ref().expect("a directory");
        assert_eq!(devices.len(), 4, "{devices:?}");
        for key_present in ["d_us", "d_10", "d_11", key(8).node_id().as_str()] {
            assert!(devices.contains_key(key_present), "{key_present} missing");
        }
        assert!(
            !devices.contains_key(&key(10).node_id()),
            "one device, one entry: the node_id key moved under the server's label"
        );
        let kept = &devices[&key(8).node_id()];
        assert!(
            crate::directory::verify_record(kept),
            "still proves itself: {kept}"
        );
        assert_eq!(
            kept["relay_url"],
            json!(null),
            "nothing vouches for its route anymore"
        );
        assert_eq!(kept["online"], json!(false));
        assert!(
            crate::directory::verify_record(&devices["d_10"]),
            "the held proof grafted onto the fresh record: {}",
            devices["d_10"]
        );
        let renamed = &devices["d_11"];
        assert!(
            renamed.get("self_sig").is_none(),
            "a proof for the OLD name is not carried: {renamed}"
        );
        // Our own record: the server's label, our countersignature (adopt_own,
        // as the session setup calls it right after the merge).
        s.adopt_own(own_device(&identity));
        let own = &s.devices.as_ref().expect("a directory")["d_us"];
        assert!(crate::directory::verify_record(own), "{own}");
    }

    /// Without a trust root nothing can prove itself, and the snapshot replaces
    /// the map — exactly as it did before the continuum.
    #[test]
    fn without_a_trust_root_the_snapshot_replaces_the_map() {
        let (ak, _) = account();
        let mut s = SessionState::new(None);
        s.devices = Some(BTreeMap::from([(
            key(8).node_id(),
            sibling(8, "Atelier", 5, &ak),
        )]));
        let fresh = BTreeMap::from([("d_1".to_string(), json!({ "device_id": "d_1" }))]);

        s.merge_snapshot(fresh.clone(), None);

        assert_eq!(s.devices.as_ref(), Some(&fresh));
    }

    /// A logout keeps what the account can prove (the continuum): the shareable
    /// records survive, re-keyed by `node_id` and stripped of the present-tense
    /// facts only the server could vouch for; the server's own assertions leave
    /// with its session.
    #[test]
    fn the_end_of_a_session_keeps_what_the_account_can_prove() {
        let (ak, ak_pub) = account();
        let identity = identity();
        let mut s = SessionState::new(Some(&crate::session::SessionInfo {
            server_url: "ws://server/ws".to_string(),
            device_id: "d_7f3a".to_string(),
            account: None,
        }));
        let mut sib = sibling(8, "Atelier", 5, &ak);
        sib["device_id"] = json!("d_8");
        sib["relay_url"] = json!("https://relay.example");
        sib["online"] = json!(true);
        s.devices = Some(BTreeMap::from([
            (
                "d_7f3a".to_string(),
                json!({ "device_id": "d_7f3a", "node_id": identity.node_id() }),
            ),
            ("d_8".to_string(), sib),
            (
                "d_srv".to_string(),
                json!({
                    "device_id": "d_srv", "node_id": key(9).node_id(),
                    "name": "Server-Only", "platform": "linux",
                }),
            ),
        ]));

        s.forget(Some(own_device(&identity)), Some(&ak_pub));

        let devices = s.devices.as_ref().expect("a directory");
        let node8 = key(8).node_id();
        assert_eq!(devices.len(), 2, "{devices:?}");
        assert!(
            devices.contains_key(&node8) && devices.contains_key(&identity.node_id()),
            "the sibling and ourselves, under node_ids: {devices:?}"
        );
        let kept = &devices[&node8];
        assert_eq!(kept["device_id"], json!(node8), "re-keyed and re-labelled");
        assert_eq!(kept["relay_url"], json!(null));
        assert_eq!(kept["online"], json!(false));
        assert!(crate::directory::verify_record(kept), "{kept}");
    }

    /// A tombstone bars a device only once its signature checks out under the
    /// account's own key — the one this device derived itself.
    #[test]
    fn only_a_signed_revocation_bars_a_device() {
        let ak = crate::account_key::account_key_from_code(
            &crate::account_key::generate_recovery_code(),
        )
        .expect("a valid code");
        let ak_pub = crate::account_key::public_hex(&ak);
        let struck = identity().node_id();
        let mut s = SessionState::new(None);

        assert!(!s.barred(&ak_pub, &struck), "nothing revoked yet");

        // Someone wrote a tombstone they could not sign: it bars nobody.
        s.revoked
            .insert(struck.clone(), "de".repeat(64).to_string());
        assert!(!s.barred(&ak_pub, &struck));

        // The account's own signature: barred.
        s.revoked
            .insert(struck.clone(), crate::account_key::revoke(&ak, &struck));
        assert!(s.barred(&ak_pub, &struck));
        // And it says nothing about anyone else, nor under another account's key.
        assert!(!s.barred(&ak_pub, "cd34"));
        let other = crate::account_key::account_key_from_code(
            &crate::account_key::generate_recovery_code(),
        )
        .expect("a valid code");
        assert!(!s.barred(&crate::account_key::public_hex(&other), &struck));
    }

    /// Striking a device off does two things, and both are needed: what a
    /// component sees now (the record leaves), and what refuses it later (the
    /// tombstone stays).
    #[test]
    fn striking_a_device_off_evicts_its_records_and_keeps_the_tombstone() {
        let mut s = SessionState::new(None);
        s.devices = Some(BTreeMap::from([
            (
                "d_old".to_string(),
                json!({ "device_id": "d_old", "node_id": "ab12" }),
            ),
            (
                "d_new".to_string(),
                json!({ "device_id": "d_new", "node_id": "ab12" }),
            ),
            (
                "d_other".to_string(),
                json!({ "device_id": "d_other", "node_id": "cd34" }),
            ),
        ]));

        let struck = s.strike_off("ab12", "revocation".to_string());

        // Both labels of the same crypto identity leave: the revocation is about
        // the `node_id`, and a device may well be listed under more than one
        // label (a re-enrollment, a self-minted one and a server's).
        assert_eq!(struck, vec!["d_new".to_string(), "d_old".to_string()]);
        let devices = s.devices.as_ref().expect("a directory");
        assert_eq!(devices.len(), 1);
        assert!(devices.contains_key("d_other"), "the others stay");
        assert_eq!(s.revoked.get("ab12"), Some(&"revocation".to_string()));
    }

    /// Nothing to evict is not nothing to record: a Core that struck a device off
    /// before ever holding a directory must still refuse it later.
    #[test]
    fn a_revocation_without_a_directory_is_still_recorded() {
        let mut s = SessionState::new(None);

        assert!(s.strike_off("ab12", "revocation".to_string()).is_empty());

        assert_eq!(s.revoked.get("ab12"), Some(&"revocation".to_string()));
    }

    // -- Taking in what a peer knows (`absorb`) -----------------------------

    /// The account these tests belong to, and its public half.
    fn account() -> (ed25519_dalek::SigningKey, String) {
        let ak = crate::account_key::account_key_from_code(
            &crate::account_key::generate_recovery_code(),
        )
        .expect("a valid code");
        let ak_pub = crate::account_key::public_hex(&ak);
        (ak, ak_pub)
    }

    fn key(seed: u8) -> crate::identity::DeviceIdentity {
        crate::identity::DeviceIdentity::from_test_seed(seed)
    }

    /// A sibling's record, as that sibling signed it and the account attested it —
    /// exactly what a peer would hand us in a roster.
    fn sibling(seed: u8, name: &str, seq: u64, ak: &ed25519_dalek::SigningKey) -> Value {
        let identity = key(seed);
        let attestation = crate::account_key::attest(ak, &identity.node_id());
        crate::directory::signed_record(&identity, name, seq, &attestation)
    }

    fn roster(records: Vec<Value>) -> crate::directory::Roster {
        crate::directory::Roster {
            records,
            revoked: BTreeMap::new(),
        }
    }

    /// A device arrives known and nothing more: what the relayer could not witness
    /// is dropped, and the record is keyed by the one label nobody could rewrite.
    #[test]
    fn a_device_we_had_never_heard_of_arrives_knowing_only_itself() {
        let (ak, ak_pub) = account();
        let identity = identity();
        let mut s = in_account_without_a_session(&identity);
        let mut record = sibling(8, "Laptop", 5, &ak);
        // Present-tense claims a courier is in no position to make, plus a label
        // of its choosing.
        record["relay_url"] = json!("https://relay.invalid");
        record["online"] = json!(true);
        record["last_seen"] = json!("2026-08-01T00:00:00Z");
        record["status"] = json!("busy");
        record["device_id"] = json!("d_whatever");

        let changed = s.absorb(&ak_pub, &identity.node_id(), &roster(vec![record]));

        let node_id = key(8).node_id();
        assert_eq!(changed.added.len(), 1, "{changed:?}");
        assert!(changed.updated.is_empty() && changed.removed.is_empty());
        let devices = s.devices.as_ref().expect("a directory");
        let learned = devices.get(&node_id).expect("keyed by its node_id");
        assert_eq!(learned["name"], json!("Laptop"));
        assert_eq!(learned["device_id"], json!(node_id), "labelled by it too");
        assert_eq!(
            learned["relay_url"],
            json!(null),
            "no route we can vouch for"
        );
        assert_eq!(learned["online"], json!(false), "we have never seen it");
        assert_eq!(learned["last_seen"], json!(null));
        assert_eq!(learned["status"], json!(null));
        assert!(
            crate::directory::verify_record(learned),
            "and it still stands behind its own description: {learned}"
        );
        assert_eq!(&changed.added[0], learned);
    }

    /// `seq` is what orders two descriptions of one device. A strictly higher one
    /// supersedes; anything else changes nothing, so two devices that agree
    /// exchange no state.
    #[test]
    fn a_fresher_description_supersedes_the_one_we_hold() {
        let (ak, ak_pub) = account();
        let identity = identity();
        let mut s = in_account_without_a_session(&identity);
        let own = identity.node_id();
        s.absorb(&ak_pub, &own, &roster(vec![sibling(8, "Laptop", 5, &ak)]));

        // Older, and the same second: nothing.
        let stale = s.absorb(
            &ak_pub,
            &own,
            &roster(vec![
                sibling(8, "Older-Name", 4, &ak),
                sibling(8, "Same-Second", 5, &ak),
            ]),
        );
        assert!(stale.is_empty(), "{stale:?}");
        assert_eq!(
            s.devices.as_ref().expect("a directory")[&key(8).node_id()]["name"],
            json!("Laptop")
        );

        // Higher: the description is replaced, signature included.
        let fresh = s.absorb(&ak_pub, &own, &roster(vec![sibling(8, "Atelier", 6, &ak)]));
        assert_eq!(fresh.updated.len(), 1, "{fresh:?}");
        assert!(fresh.added.is_empty(), "the same device, not a second one");
        let devices = s.devices.as_ref().expect("a directory");
        assert_eq!(devices.len(), 2, "ourselves and it: {devices:?}");
        let record = &devices[&key(8).node_id()];
        assert_eq!(record["name"], json!("Atelier"));
        assert_eq!(record["seq"], json!(6));
        assert!(crate::directory::verify_record(record));
    }

    /// What supersedes is the DESCRIPTION, and only it. The record's key, its route
    /// and its presence stay ours — and the sharpest reason is `device_id`: nothing
    /// signs it, `enrich_device` derives `is_self` from it, so a peer allowed to
    /// carry one over could hand us a fresher description for ANOTHER device
    /// wearing our own label, and this machine would see that device as itself.
    #[test]
    fn a_fresher_description_replaces_the_description_and_nothing_else() {
        let (ak, ak_pub) = account();
        let identity = identity();
        let mut s = in_account_without_a_session(&identity);
        let own = identity.node_id();
        let node_id = key(8).node_id();
        s.absorb(&ak_pub, &own, &roster(vec![sibling(8, "Laptop", 5, &ak)]));
        // What we know first-hand about it since: a route, and that it is up.
        let held = s
            .devices
            .as_mut()
            .expect("a directory")
            .get_mut(&node_id)
            .expect("its record");
        held["relay_url"] = json!("https://relay.example");
        held["online"] = json!(true);
        held["last_seen"] = json!("2026-08-01T00:00:00Z");

        // A fresher description, carrying claims its relayer is in no position to
        // make — our own label among them.
        let mut poisoned = sibling(8, "Atelier", 6, &ak);
        poisoned["device_id"] = json!(own);
        poisoned["relay_url"] = json!("https://liar.example");
        poisoned["online"] = json!(false);
        poisoned["last_seen"] = json!("1999-01-01T00:00:00Z");
        let changed = s.absorb(&ak_pub, &own, &roster(vec![poisoned]));

        assert_eq!(changed.updated.len(), 1, "{changed:?}");
        let devices = s.devices.as_ref().expect("a directory");
        let record = &devices[&node_id];
        assert_eq!(record["name"], json!("Atelier"), "the description moved");
        assert_eq!(record["seq"], json!(6));
        assert!(crate::directory::verify_record(record));
        assert_eq!(
            record["device_id"],
            json!(node_id),
            "and not our own label: {record}"
        );
        assert_eq!(record["relay_url"], json!("https://relay.example"));
        assert_eq!(record["online"], json!(true));
        assert_eq!(record["last_seen"], json!("2026-08-01T00:00:00Z"));
        assert_eq!(devices.len(), 2, "ourselves and it: {devices:?}");
    }

    /// Where a server minted the record, the server owns the name — another device
    /// may have chosen it (`devices.rename`). The data plane does not overrule that.
    #[test]
    fn a_description_a_server_minted_is_not_overruled_by_a_peer() {
        let (ak, ak_pub) = account();
        let identity = identity();
        let mut s = in_account_without_a_session(&identity);
        let node_id = key(8).node_id();
        let from_server = json!({
            "device_id": "d_7f3a", "node_id": node_id, "name": "Named-By-The-Server",
            "platform": "linux", "online": true, "relay_url": "https://relay.example",
            "attestation": crate::account_key::attest(&ak, &node_id),
        });
        s.devices
            .as_mut()
            .expect("a directory")
            .insert("d_7f3a".to_string(), from_server.clone());

        let changed = s.absorb(
            &ak_pub,
            &identity.node_id(),
            &roster(vec![sibling(8, "Renamed-Over-The-Wire", u64::MAX, &ak)]),
        );

        assert!(changed.is_empty(), "{changed:?}");
        let devices = s.devices.as_ref().expect("a directory");
        assert_eq!(devices["d_7f3a"], from_server, "left exactly as it was");
        assert!(
            !devices.contains_key(&node_id),
            "and not duplicated under its node_id either: {devices:?}"
        );
    }

    /// A courier is not a witness: a record that does not prove, on its own, who it
    /// describes and that the account admitted them, enters nothing.
    #[test]
    fn nothing_a_peer_cannot_prove_enters_the_directory() {
        let (ak, ak_pub) = account();
        let (other_ak, _) = account();
        let identity = identity();
        let mut s = in_account_without_a_session(&identity);

        let stranger = key(9);
        let mut renamed_in_transit = sibling(8, "Laptop", 5, &ak);
        renamed_in_transit["name"] = json!("Not-What-It-Signed");
        let records = vec![
            // Signed by itself, attested under an account that is not ours.
            crate::directory::signed_record(
                &stranger,
                "Intruder",
                5,
                &crate::account_key::attest(&other_ak, &stranger.node_id()),
            ),
            // Attested, but nothing signs the description (a server's record: the
            // peer relaying it could have written any name on it).
            json!({
                "device_id": "d_1", "node_id": key(10).node_id(), "name": "Hearsay",
                "platform": "linux",
                "attestation": crate::account_key::attest(&ak, &key(10).node_id()),
            }),
            // Signed and attested, then rewritten by whoever carried it.
            renamed_in_transit,
            // Not a record at all.
            json!({}),
            Value::Null,
        ];

        let changed = s.absorb(&ak_pub, &identity.node_id(), &roster(records));

        assert!(changed.is_empty(), "{changed:?}");
        assert_eq!(
            s.devices.as_ref().expect("a directory").len(),
            1,
            "still only ourselves"
        );

        // And a Core that knows of NO device at all does not end up with an empty
        // directory instead of none: `devices.list` must keep answering
        // `SERVER_UNREACHABLE` rather than "nobody, and I am sure of it".
        let mut nothing = SessionState::new(None);
        nothing.absorb(&ak_pub, &identity.node_id(), &roster(vec![json!({})]));
        assert!(nothing.devices.is_none());
    }

    /// The label a record carries is not the key we file it under: taking it would
    /// let whoever relays a record overwrite the entry of a DIFFERENT device.
    #[test]
    fn a_relayed_record_cannot_re_label_another_device() {
        let (ak, ak_pub) = account();
        let identity = identity();
        let mut s = in_account_without_a_session(&identity);
        let own = identity.node_id();
        s.absorb(&ak_pub, &own, &roster(vec![sibling(8, "Laptop", 5, &ak)]));

        // A record for another device, wearing the first one's label.
        let mut intruder = sibling(9, "Impostor", 99, &ak);
        intruder["device_id"] = json!(key(8).node_id());
        let changed = s.absorb(&ak_pub, &own, &roster(vec![intruder]));

        assert_eq!(changed.added.len(), 1);
        let devices = s.devices.as_ref().expect("a directory");
        assert_eq!(devices[&key(8).node_id()]["name"], json!("Laptop"));
        assert_eq!(devices[&key(9).node_id()]["name"], json!("Impostor"));
        assert_eq!(devices.len(), 3, "three devices, none overwritten");
    }

    /// A tombstone travels with the roster and takes effect on arrival — and the
    /// record it names does not slip in alongside it.
    #[test]
    fn a_tombstone_from_a_peer_evicts_the_device_it_names() {
        let (ak, ak_pub) = account();
        let identity = identity();
        let mut s = in_account_without_a_session(&identity);
        let own = identity.node_id();
        s.absorb(&ak_pub, &own, &roster(vec![sibling(8, "Laptop", 5, &ak)]));
        let struck = key(8).node_id();

        let changed = s.absorb(
            &ak_pub,
            &own,
            &crate::directory::Roster {
                // The same roster still describes it, under a FRESHER `seq` than
                // the one held — so if the tombstone were applied second, that
                // description would land first and show up as an update.
                records: vec![sibling(8, "Laptop", 6, &ak), sibling(9, "Kept", 5, &ak)],
                revoked: BTreeMap::from([
                    (struck.clone(), crate::account_key::revoke(&ak, &struck)),
                    // A tombstone the account never signed bars nobody.
                    ("ab12".to_string(), "de".repeat(64)),
                ]),
            },
        );

        assert_eq!(changed.removed, vec![struck.clone()]);
        assert_eq!(changed.barred, vec![struck.clone()]);
        assert_eq!(changed.added.len(), 1, "the other one still arrives");
        assert!(
            changed.updated.is_empty(),
            "the struck device's fresher description never landed: {changed:?}"
        );
        let devices = s.devices.as_ref().expect("a directory");
        assert!(!devices.contains_key(&struck), "{devices:?}");
        assert!(s.barred(&ak_pub, &struck));
        assert!(!s.revoked.contains_key("ab12"), "unsigned: not kept");

        // And it stays out, however often it is offered again — tombstone
        // included: a revocation we already hold is not news, so a roster that
        // repeats it changes nothing and writes nothing.
        let again = s.absorb(
            &ak_pub,
            &own,
            &crate::directory::Roster {
                records: vec![sibling(8, "Laptop", 7, &ak)],
                revoked: BTreeMap::from([(
                    struck.clone(),
                    crate::account_key::revoke(&ak, &struck),
                )]),
            },
        );
        assert!(again.is_empty(), "{again:?}");
    }

    /// This device is the authority on its own description. A peer's copy is at
    /// best what we once told it — adopting it would mean re-signing, and handing
    /// on, a name we never chose.
    #[test]
    fn a_device_never_takes_in_a_description_of_itself() {
        let (ak, ak_pub) = account();
        let identity = identity();
        let mut s = in_account_without_a_session(&identity);
        let before = s.devices.clone();

        // Signed by our own key, and newer than anything we hold.
        let changed = s.absorb(
            &ak_pub,
            &identity.node_id(),
            &roster(vec![crate::directory::signed_record(
                &identity,
                "Name-A-Peer-Chose",
                u64::MAX,
                &crate::account_key::attest(&ak, &identity.node_id()),
            )]),
        );

        assert!(changed.is_empty(), "{changed:?}");
        assert_eq!(s.devices, before, "our own record, untouched");
    }

    /// The account striking THIS device off is not hearsay — it is signed by the
    /// account key, and once that signature VERIFIES it is obeyed: `absorb`
    /// reports it and takes in nothing else, and the caller leaves the account
    /// (`account_key::leave` — the state here is not `absorb`'s to erase). A
    /// signature that does not verify wipes nothing, however squarely it names
    /// us: the wipe rides on the same check as every tombstone.
    #[test]
    fn a_tombstone_against_this_very_device_is_obeyed_once_it_verifies() {
        let (ak, ak_pub) = account();
        let identity = identity();
        let mut s = in_account_without_a_session(&identity);
        let own = identity.node_id();
        let sibling = crate::identity::DeviceIdentity::from_test_seed(9);

        // Not the account's signature: nothing happens, not even the report.
        let forged = s.absorb(
            &ak_pub,
            &own,
            &crate::directory::Roster {
                records: Vec::new(),
                revoked: BTreeMap::from([(own.clone(), "de".repeat(64))]),
            },
        );
        assert!(forged.is_empty(), "{forged:?}");
        assert!(!forged.struck_ourselves);
        assert!(s.revoked.is_empty(), "not stored either");

        // The account's own word: reported for the caller to obey — and it
        // eclipses the rest of the roster, tombstones included, because every
        // remaining entry describes an account this device is out of.
        let struck = s.absorb(
            &ak_pub,
            &own,
            &crate::directory::Roster {
                records: Vec::new(),
                revoked: BTreeMap::from([
                    (own.clone(), crate::account_key::revoke(&ak, &own)),
                    (
                        sibling.node_id(),
                        crate::account_key::revoke(&ak, &sibling.node_id()),
                    ),
                ]),
            },
        );
        assert!(struck.struck_ourselves);
        assert!(!struck.is_empty(), "being struck IS a change");
        assert!(struck.barred.is_empty(), "{struck:?}");
        assert!(s.revoked.is_empty(), "nothing else was taken in");
    }

    /// A device that never joined the account has nothing of its own to keep: the
    /// directory goes, and `devices.list` has nothing honest to serve.
    #[test]
    fn the_end_of_a_session_without_an_account_leaves_nothing() {
        let mut s = SessionState::new(Some(&crate::session::SessionInfo {
            server_url: "ws://server/ws".to_string(),
            device_id: "d_7f3a".to_string(),
            account: None,
        }));
        s.devices = Some(BTreeMap::new());

        s.forget(None, None);

        assert!(s.devices.is_none(), "no account, no directory");
        assert_eq!(s.own_device_id, None);
    }
}
