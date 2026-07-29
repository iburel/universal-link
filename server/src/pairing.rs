// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

//! Pairing sessions: how a device joins the account by being confirmed on a
//! device that is already in it.
//!
//! **The server is a rendezvous and a relay of opaque strings.** It never sees
//! the secret the QR code carries — that one travels by a screen and a camera,
//! out of its reach — and the `bundle` it hands over is ciphertext keyed by that
//! secret. So what stops a reader of this file from stealing an account is not
//! this state machine: it is the optical channel. What this state machine IS
//! answerable for:
//!
//! - bringing the two parties together under an unguessable id, one-shot,
//!   short-lived, and never written to disk (a restart cancels what is in
//!   flight);
//! - making sure the **sponsor** — the side that gives the account away — is an
//!   authenticated device of that account (`pairing.approve` additionally
//!   demands a fresh OIDC token, in `conn.rs`, like `devices.revoke`);
//! - **pinning the joining device's identity** between the moment a human
//!   confirms it and the moment it is enrolled, so that what enrolls is what was
//!   shown;
//! - refusing to bridge two different accounts.
//!
//! The two directions the UI offers are ONE state machine: whoever displays the
//! QR code creates the session (the *offerer*), whoever scans it claims the
//! session (the *claimer*), and [`Role`] says which of the two is joining. A
//! brand-new PC displays and joins while a phone already in the account scans
//! and sponsors; for a brand-new phone it is the other way round.
//!
//! Spec: `doc/server-api.md`.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tokio::sync::mpsc::Sender;

use crate::rpc::RpcErr;
use crate::state::{ConnId, OutMsg, notify, random_hex};

/// Ceiling on the sessions held at once. The real bound is one per connection
/// (a new offer replaces that connection's previous one); this is the backstop
/// that keeps a flood of connections from growing the map without limit. A real
/// account pairs one device at a time.
const MAX_SESSIONS: usize = 1024;

/// Which side of the exchange a party is on. The `Joiner` receives the
/// account's key material; the `Sponsor` hands it over and must already be in
/// the account.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Role {
    Joiner,
    Sponsor,
}

impl Role {
    pub fn parse(raw: &str) -> Option<Role> {
        match raw {
            "joiner" => Some(Role::Joiner),
            "sponsor" => Some(Role::Sponsor),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Role::Joiner => "joiner",
            Role::Sponsor => "sponsor",
        }
    }

    /// The role of the other party: a pairing has exactly one of each.
    fn other(self) -> Role {
        match self {
            Role::Joiner => Role::Sponsor,
            Role::Sponsor => Role::Joiner,
        }
    }
}

/// The device that is joining, as declared by whichever side it sits on and
/// **pinned** for the session's lifetime: the sponsor confirms these very
/// values, and enrollment reads them back from here rather than from the
/// enrolling request — nothing can drift between the confirmation and the
/// directory entry.
///
/// Any `node_id` may be declared: nothing here proves it, and nothing needs to.
/// Enrollment demands a signature by that key over the connection's nonce, so a
/// declaration that does not belong to its declarer enrolls nothing.
#[derive(Clone)]
pub struct JoiningDevice {
    pub name: String,
    pub platform: String,
    pub node_id: String,
}

impl JoiningDevice {
    /// What the sponsor is shown before confirming.
    pub fn record(&self) -> Value {
        json!({
            "name": self.name,
            "platform": self.platform,
            "node_id": self.node_id,
        })
    }
}

/// One side of a pairing, and the connection to reach it on. Pairing cannot use
/// the account broadcast: a joiner is not (yet) a device of the account, so it
/// is in no account's device list.
pub struct Party {
    conn_id: ConnId,
    tx: Sender<OutMsg>,
    /// Opaque to us: the public half of this side's channel material, relayed
    /// verbatim to the other side so the two can key their own encryption.
    channel: String,
}

impl Party {
    pub fn new(conn_id: ConnId, tx: Sender<OutMsg>, channel: String) -> Party {
        Party {
            conn_id,
            tx,
            channel,
        }
    }
}

enum Stage {
    /// Displayed, waiting for the other side to scan.
    Offered,
    /// Both sides are present; waiting for the human to confirm on the sponsor.
    Claimed,
    /// Confirmed. The session is now the joiner's enrollment grant, until it
    /// uses it or the session expires.
    Approved,
}

struct Session {
    /// The party that created the session and displays the QR code.
    offerer: Party,
    /// Its role — which fixes the claimer's, the two being opposites.
    offerer_role: Role,
    claimer: Option<Party>,
    /// The account at stake. Known from the start when the party that created
    /// the session has one, otherwise learned when the sponsor claims.
    account: Option<String>,
    /// Absent exactly while the joiner has not spoken yet (it declares its
    /// device when it creates the session, or when it claims one).
    joining: Option<JoiningDevice>,
    stage: Stage,
    expires_at: Instant,
}

impl Session {
    /// The party on the given side, once it has taken part.
    fn party(&self, role: Role) -> Option<&Party> {
        if self.offerer_role == role {
            Some(&self.offerer)
        } else {
            self.claimer.as_ref()
        }
    }

    fn involves(&self, conn_id: ConnId) -> bool {
        self.offerer.conn_id == conn_id
            || self.claimer.as_ref().is_some_and(|c| c.conn_id == conn_id)
    }

    /// Tells whichever party is NOT `conn_id` that the pairing is over, so it
    /// stops waiting instead of sitting until the TTL.
    fn tell_the_other(&self, conn_id: ConnId, reason: &str) {
        for party in [Some(&self.offerer), self.claimer.as_ref()]
            .into_iter()
            .flatten()
        {
            if party.conn_id != conn_id {
                notify(&party.tx, "pairing.failed", json!({ "reason": reason }));
            }
        }
    }
}

/// What a claimer is told in return: who the server says it is, how long the
/// session has left, and — when it is the sponsor — the device it must show the
/// human.
pub struct Claimed {
    pub role: Role,
    /// Seconds left before the session expires. The claimer needs it as much as
    /// the offerer (which got it from `create`): both sides time out on their own
    /// clocks, and a side that was never told the deadline would have to invent
    /// one. Truncated downwards — expiring a shade early is the safe rounding.
    pub expires_in: u64,
    pub joining: Option<JoiningDevice>,
}

/// An approved session, read as the enrollment grant it has become: the
/// identity a human confirmed.
pub struct Grant {
    pub account: String,
    pub joining: JoiningDevice,
}

/// The pairings in flight. In memory only, deliberately: a pairing is a
/// conversation between two live connections, and one that outlived a restart
/// would be a grant nobody is waiting for.
pub struct Sessions {
    by_id: HashMap<String, Session>,
    ttl: Duration,
}

/// Unknown, expired, already consumed — or none of the caller's business. The
/// cases are deliberately not distinguished: an id is 128 random bits, so a
/// caller that does not hold one learns nothing, and a caller from another
/// account is not told that it exists.
fn unknown() -> RpcErr {
    RpcErr::app("PAIRING_UNKNOWN")
}

impl Sessions {
    pub fn new(ttl: Duration) -> Sessions {
        Sessions {
            by_id: HashMap::new(),
            ttl,
        }
    }

    /// Opens a session for the side that displays the QR code.
    ///
    /// Returns its id and how long it stays open. Nothing is notified: the
    /// other side has yet to learn the id, by reading the screen.
    pub fn create(
        &mut self,
        role: Role,
        offerer: Party,
        account: Option<String>,
        joining: Option<JoiningDevice>,
    ) -> Result<(String, u64), RpcErr> {
        self.sweep();
        // One offer per connection: a device displays one QR code at a time,
        // and a dialog closed then reopened must not be blocked by the session
        // it left behind — which would otherwise hold the slot for a whole TTL.
        self.drop_for_conn(offerer.conn_id);
        if self.by_id.len() >= MAX_SESSIONS {
            return Err(RpcErr::app("PAIRING_LIMIT"));
        }

        let pairing_id = format!("p_{}", random_hex(16));
        self.by_id.insert(
            pairing_id.clone(),
            Session {
                offerer,
                offerer_role: role,
                claimer: None,
                account,
                joining,
                stage: Stage::Offered,
                expires_at: Instant::now() + self.ttl,
            },
        );
        Ok((pairing_id, self.ttl.as_secs()))
    }

    /// The side that scanned the QR code joins the session, and the offerer is
    /// told at once so it can move from "waiting" to "confirm this device".
    pub fn claim(
        &mut self,
        pairing_id: &str,
        claimer: Party,
        account: Option<String>,
        joining: Option<JoiningDevice>,
    ) -> Result<Claimed, RpcErr> {
        self.sweep();
        let session = self.by_id.get_mut(pairing_id).ok_or_else(unknown)?;
        if !matches!(session.stage, Stage::Offered) {
            // Already claimed (or already approved): a pairing joins exactly
            // two devices, and a second scanner does not get to replace the
            // one the human is about to confirm.
            return Err(RpcErr::app("PAIRING_STATE"));
        }

        // The scanner does not get to choose: the session says which side it is
        // joining on.
        let role = session.offerer_role.other();

        // Both sides know the account only when the joiner is re-joining (it is
        // enrolled but holds no account key). Then they must agree: a sponsor
        // answering an offer from ANOTHER account would be handing over key
        // material for an account the joiner is not in.
        if let (Some(theirs), Some(ours)) = (&session.account, &account)
            && theirs != ours
        {
            return Err(unknown());
        }
        if role == Role::Sponsor {
            // The side that gives the account away must be in it.
            if account.is_none() {
                return Err(RpcErr::app("NOT_AUTHENTICATED"));
            }
            session.account = account;
        }

        // The joining device travels towards the SPONSOR, whichever side that
        // is: it is the one that has to show a human what it is vouching for.
        let mut claimed = Claimed {
            role,
            expires_in: session
                .expires_at
                .saturating_duration_since(Instant::now())
                .as_secs(),
            joining: None,
        };
        let mut announced = json!({ "channel": claimer.channel });
        match role {
            // The joiner declares the device to be confirmed and enrolled; the
            // offerer, being the sponsor, is told it in the same breath.
            Role::Joiner => {
                let declared = joining.ok_or_else(|| RpcErr::invalid_params("device"))?;
                announced["device"] = declared.record();
                session.joining = Some(declared);
            }
            // The sponsor gets what the joiner pinned when it created the
            // session (hence present). The offerer, being the joiner, is told
            // only the channel: nothing about the sponsor is disclosed here —
            // proving the account is the bundle's job.
            Role::Sponsor => claimed.joining = session.joining.clone(),
        }

        notify(&session.offerer.tx, "pairing.claimed", announced);
        session.claimer = Some(claimer);
        session.stage = Stage::Claimed;
        Ok(claimed)
    }

    /// The human confirmed on the sponsor: the sealed bundle goes to the
    /// joiner, and the session becomes its enrollment grant.
    pub fn approve(
        &mut self,
        pairing_id: &str,
        conn_id: ConnId,
        account: &str,
        bundle: &str,
    ) -> Result<(), RpcErr> {
        self.sweep();
        let session = self.by_id.get_mut(pairing_id).ok_or_else(unknown)?;
        if session.account.as_deref() != Some(account) {
            return Err(unknown());
        }
        if !matches!(session.stage, Stage::Claimed) {
            // Nothing to confirm before the other side has scanned, and a
            // confirmation is not repeatable.
            return Err(RpcErr::app("PAIRING_STATE"));
        }
        // Only the sponsor confirms, and only on the very connection that took
        // part: the account's other devices were not asked, and someone who
        // learned the id cannot answer in their place.
        if session.party(Role::Sponsor).map(|p| p.conn_id) != Some(conn_id) {
            return Err(RpcErr::app("PAIRING_STATE"));
        }
        let joiner = session
            .party(Role::Joiner)
            .expect("claimed → both parties present")
            .tx
            .clone();

        notify(&joiner, "pairing.completed", json!({ "bundle": bundle }));
        session.stage = Stage::Approved;
        Ok(())
    }

    /// Either side gives up — the human declined, the dialog was closed. The
    /// other is told rather than left waiting for the TTL.
    pub fn cancel(&mut self, pairing_id: &str, conn_id: ConnId) -> Result<(), RpcErr> {
        self.sweep();
        if !self
            .by_id
            .get(pairing_id)
            .is_some_and(|s| s.involves(conn_id))
        {
            return Err(unknown());
        }
        let session = self.by_id.remove(pairing_id).expect("just checked");
        session.tell_the_other(conn_id, "declined");
        Ok(())
    }

    /// Reads an approved session as the grant it has become, WITHOUT consuming
    /// it: the caller has a proof of possession left to check, and a failed
    /// check must not cost the human another scan.
    pub fn peek_grant(&mut self, pairing_id: &str, conn_id: ConnId) -> Result<Grant, RpcErr> {
        self.sweep();
        let session = self.by_id.get(pairing_id).ok_or_else(unknown)?;
        // Only the joiner enrolls on it, and only on the connection that was
        // part of the pairing — which is also the one that received the bundle.
        if session.party(Role::Joiner).map(|p| p.conn_id) != Some(conn_id) {
            return Err(unknown());
        }
        if !matches!(session.stage, Stage::Approved) {
            return Err(RpcErr::app("PAIRING_STATE"));
        }
        Ok(Grant {
            account: session
                .account
                .clone()
                .expect("approved → the account is settled"),
            joining: session
                .joining
                .clone()
                .expect("approved → the joining device is pinned"),
        })
    }

    /// Spends the grant. One shot: what follows is a device in the directory,
    /// and a second one must cost a second confirmation.
    pub fn consume(&mut self, pairing_id: &str) {
        self.by_id.remove(pairing_id);
    }

    /// Forgets every session this connection took part in. A pairing that lost
    /// a party can never complete, so the survivor is told instead of waiting.
    pub fn drop_for_conn(&mut self, conn_id: ConnId) {
        let orphaned: Vec<String> = self
            .by_id
            .iter()
            .filter(|(_, session)| session.involves(conn_id))
            .map(|(id, _)| id.clone())
            .collect();
        for id in orphaned {
            let session = self.by_id.remove(&id).expect("just listed");
            session.tell_the_other(conn_id, "abandoned");
        }
    }

    /// Drops what has timed out. Lazy, at every entry point: a pairing is a
    /// human-paced exchange, so there is no background sweeper to run.
    ///
    /// Deliberately silent — both sides were told the TTL up front and time out
    /// on their own clocks, which is exact, whereas a notification from here
    /// would arrive whenever someone else happened to call.
    fn sweep(&mut self) {
        let now = Instant::now();
        self.by_id.retain(|_, session| session.expires_at > now);
    }
}
