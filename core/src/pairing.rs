// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

//! Pairing, Core side: how a device hands the account to another one, and how
//! that other one receives it — the two ends of `server/src/pairing.rs`, which
//! only relays between them.
//!
//! **What crosses is the account key's seed**, so the channel carries it sealed:
//! the server sees ciphertext and an unguessable id, and nothing else. The key
//! that seals it is derived from two halves:
//!
//! - an **X25519 exchange** between two keypairs generated for this one pairing:
//!   the displaying side puts its public key in the QR code, the scanning side
//!   sends its own back as the session's `channel`;
//! - the **pre-shared secret** of the QR code, 128 bits that travel by a screen
//!   and a camera — a channel the server is not on.
//!
//! Either half alone is not enough. A server that records everything lacks the
//! optical secret; someone who photographs the screen lacks a private key.
//! Photographing the screen AND being faster than the legitimate scanner does
//! work — the server hands the session to whoever claims first — and that is
//! precisely what the confirmation screen is for: what the human is shown is the
//! *device that scanned*, and an intruder cannot make that look like the phone in
//! the user's hand. The residual risk is stated in `doc/architecture.md`.
//!
//! **The seed is never held here.** The sponsor reads it out of the keyring at
//! the instant it seals the bundle (`account_key::recall`) and lets it go; a
//! pairing in flight holds a channel key and nothing else. The joiner, for its
//! part, checks what it receives before trusting it: a seed that derives a key
//! other than the one this device is already attested under is refused
//! (`account_key::install`), which is what stops a pairing from quietly moving a
//! device to somebody else's account.
//!
//! **Two wires, one conversation.** A pairing is a conversation between two live
//! connections, and the server pins each side to the connection it spoke on. A
//! device that is already logged in therefore pairs over its session connection
//! (`session::proxy`) — which is also what tells the server its account, so a
//! sponsor from another one is turned away. A device that has never enrolled has
//! no session at all: it opens a connection of its own for the pairing, serves
//! its own `auth.enroll` on it, and closes it when the session task takes over.
//!
//! Spec: `doc/core-api.md` (`pairing.*`) and `doc/server-api.md` ("Pairing").

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use base64::Engine;
use chacha20poly1305::aead::{Aead, KeyInit};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use ed25519_dalek::SigningKey;
use serde_json::{Value, json};
use sha2::Sha256;
use tokio::sync::{mpsc, oneshot};
use x25519_dalek::{EphemeralSecret, PublicKey, SharedSecret};

use crate::rpc::RpcErr;
use crate::state::{AppState, ServerCmd};

/// Version tag of the payload a QR code (or a pasted text code) carries. A
/// version, not decoration: it is what lets a later scheme be told apart from
/// this one instead of being half-parsed.
const PAYLOAD_TAG: &str = "UL1";

/// Bytes of pre-shared secret in the payload. 128 bits: unguessable, and short
/// enough to keep the QR code small.
const PSK_LEN: usize = 16;

/// Domain separation for the channel key. Versioned like the rest of the
/// project's derivations (`account_key`).
const CHANNEL_DOMAIN: &[u8] = b"ul-pairing-channel-v1";

/// Depth of the request queue toward a pairing's own connection. Tiny: the
/// exchange is a handful of sequential calls.
const DIRECT_QUEUE_DEPTH: usize = 4;

/// Beyond this, a request on a pairing's own connection counts as a lost server
/// — the same budget as `session::PROXY_TIMEOUT`, for the same reason: no caller
/// waits indefinitely on a server that has stopped answering.
const DIRECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Which end of the exchange this device is on, as the server settles it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Role {
    /// Receives the account: it will hold the key and, if it is not in the
    /// directory yet, enroll on the grant.
    Joiner,
    /// Gives the account away: it must hold the key AND be authenticated in the
    /// account at the rendezvous.
    Sponsor,
}

impl Role {
    fn parse(raw: &str) -> Option<Role> {
        match raw {
            "joiner" => Some(Role::Joiner),
            "sponsor" => Some(Role::Sponsor),
            _ => None,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Role::Joiner => "joiner",
            Role::Sponsor => "sponsor",
        }
    }
}

/// Distinguishes one pairing from the next, so a task that outlives its own
/// pairing (an expiry timer, a connection closing) cannot settle the pairing
/// that replaced it. A counter, not the `pairing_id`: the id is minted by the
/// server, and a pairing exists locally before the server has answered.
static EPOCH: AtomicU64 = AtomicU64::new(0);

/// How this pairing reaches the server.
#[derive(Clone)]
enum Wire {
    /// Over the session task's connection — the only one the server knows as a
    /// device of the account.
    Session,
    /// Over a connection opened for this pairing alone: what a device with no
    /// session has to use, and what serves its `auth.enroll`.
    Direct {
        /// The server this connection was opened to — the joiner writes it into
        /// `session.json` once enrolled.
        url: String,
        tx: mpsc::Sender<ServerCmd>,
        abort: tokio::task::AbortHandle,
    },
}

/// The channel's halves, and what becomes of them: the ephemeral secret is
/// consumed the moment the other side's public key lands, leaving a single
/// symmetric key behind.
struct Channel {
    psk: [u8; PSK_LEN],
    secret: Option<EphemeralSecret>,
    ours: PublicKey,
    /// Did we display the code? It fixes the order of the two public keys in the
    /// derivation, which both sides must agree on.
    offered: bool,
    key: Option<[u8; 32]>,
}

impl Channel {
    /// Our side of a code we are about to display: the optical secret is ours to
    /// mint, and the screen is how it travels.
    fn displaying() -> Channel {
        let mut psk = [0u8; PSK_LEN];
        rand::Rng::fill_bytes(&mut rand::rng(), &mut psk);
        Channel::with_psk(psk, true)
    }

    /// Our side of a code we have just read: the optical secret is the one the
    /// OTHER side minted, and using anything else would derive a key that opens
    /// nothing. Taking it as a parameter is what keeps that from being a mistake
    /// one can make.
    fn scanning(psk: [u8; PSK_LEN]) -> Channel {
        Channel::with_psk(psk, false)
    }

    fn with_psk(psk: [u8; PSK_LEN], offered: bool) -> Channel {
        let secret = EphemeralSecret::random_from_rng(&mut rand::rng());
        Channel {
            psk,
            ours: PublicKey::from(&secret),
            secret: Some(secret),
            offered,
            key: None,
        }
    }

    /// Derives the channel key from the other side's public half. `None` when
    /// there is nothing sound to derive — an unusable public key, a
    /// non-contributory exchange, or a second attempt (the secret is gone).
    /// Fail-closed: the caller fails the pairing rather than carrying on with a
    /// key an attacker could have dictated.
    fn establish(&mut self, theirs: &PublicKey, pairing_id: &str) -> Option<[u8; 32]> {
        let shared = self.secret.take()?.diffie_hellman(theirs);
        // A peer that sends a low-order point forces a shared secret that is
        // publicly known: everyone who watched would hold the channel key. The
        // check costs nothing and is constant-time.
        if !shared.was_contributory() {
            return None;
        }
        let (offerer, claimer) = if self.offered {
            (&self.ours, theirs)
        } else {
            (theirs, &self.ours)
        };
        let key = derive_key(&shared, &self.psk, offerer, claimer, pairing_id);
        self.key = Some(key);
        Some(key)
    }
}

/// The pairing in flight. One at a time per Core: a device displays one code and
/// confirms one device, and the server holds one session per connection.
pub(crate) struct Pairing {
    epoch: u64,
    pairing_id: String,
    role: Role,
    channel: Channel,
    wire: Wire,
    /// The expiry timer, cancelled by whatever settles the pairing first.
    expiry: Option<tokio::task::AbortHandle>,
}

impl Pairing {
    /// Stops the countdown: this pairing has an outcome now, whatever it is, and
    /// an expiry firing after the fact would speak for a session that is over.
    fn disarm(&self) {
        if let Some(expiry) = &self.expiry {
            expiry.abort();
        }
    }

    /// Lets go of everything, the connection it opened for itself included.
    ///
    /// Not the same thing as being settled: a joiner whose bundle has just
    /// arrived still needs that connection — it is what its `auth.enroll` goes
    /// over. Only the paths where nothing more will be said come through here.
    fn abandon(&self) {
        self.disarm();
        if let Wire::Direct { abort, .. } = &self.wire {
            abort.abort();
        }
    }
}

// ---------------------------------------------------------------------------
// The payload of the QR code (and of the text code that stands in for it).
// ---------------------------------------------------------------------------

/// What the displaying side puts on screen: the session to claim, the optical
/// secret, and the public half of its channel.
struct Payload {
    pairing_id: String,
    psk: [u8; PSK_LEN],
    epk: PublicKey,
}

impl Payload {
    /// `UL1:<psk>:<public key>:<pairing_id>` — the two fixed-length fields
    /// first, base64url, and the id LAST so that whatever the server puts in it
    /// (a separator included) survives the split untouched.
    fn encode(&self) -> String {
        format!(
            "{PAYLOAD_TAG}:{}:{}:{}",
            b64(&self.psk),
            b64(self.epk.as_bytes()),
            self.pairing_id,
        )
    }

    /// Parses what a camera read or a human pasted. Surrounding whitespace is
    /// forgiven (a pasted line brings its newline); nothing else is — the fields
    /// are machine-generated.
    fn parse(text: &str) -> Option<Payload> {
        let mut fields = text.trim().splitn(4, ':');
        if fields.next()? != PAYLOAD_TAG {
            return None;
        }
        let psk: [u8; PSK_LEN] = unb64(fields.next()?)?.try_into().ok()?;
        let epk: [u8; 32] = unb64(fields.next()?)?.try_into().ok()?;
        let pairing_id = fields.next()?;
        if pairing_id.is_empty() {
            return None;
        }
        Some(Payload {
            pairing_id: pairing_id.to_string(),
            psk,
            epk: PublicKey::from(epk),
        })
    }
}

fn b64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

fn unb64(text: &str) -> Option<Vec<u8>> {
    base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(text)
        .ok()
}

// ---------------------------------------------------------------------------
// Channel crypto.
// ---------------------------------------------------------------------------

/// The channel key: HKDF over the X25519 secret, keyed by the optical secret,
/// bound to the exchange it belongs to.
///
/// The PSK goes in as the **salt** and the exchange as the keying material: the
/// extraction is then a PRF keyed by the secret the server never saw, so
/// recovering the key demands both halves. The `info` pins the transcript —
/// both public keys in the order display-then-scan, and the session id — so a
/// key derived for one pairing cannot be mistaken for another's, whatever the
/// server hands out.
fn derive_key(
    shared: &SharedSecret,
    psk: &[u8; PSK_LEN],
    offerer: &PublicKey,
    claimer: &PublicKey,
    pairing_id: &str,
) -> [u8; 32] {
    let mut info = Vec::with_capacity(CHANNEL_DOMAIN.len() + 64 + pairing_id.len());
    info.extend_from_slice(CHANNEL_DOMAIN);
    info.extend_from_slice(offerer.as_bytes());
    info.extend_from_slice(claimer.as_bytes());
    info.extend_from_slice(pairing_id.as_bytes());
    let mut key = [0u8; 32];
    hkdf::Hkdf::<Sha256>::new(Some(psk), shared.as_bytes())
        .expand(&info, &mut key)
        .expect("32 bytes is a valid HKDF-SHA256 output length");
    key
}

/// Seals the bundle: a random nonce, then the ciphertext, base64url. The key is
/// used for exactly one message here; the extended nonce is what keeps that from
/// being a property the next message has to remember.
fn seal(key: &[u8; 32], plaintext: &[u8]) -> Option<String> {
    let cipher = XChaCha20Poly1305::new(key.into());
    let mut nonce = [0u8; 24];
    rand::Rng::fill_bytes(&mut rand::rng(), &mut nonce);
    let mut sealed = cipher.encrypt(&XNonce::from(nonce), plaintext).ok()?;
    let mut out = nonce.to_vec();
    out.append(&mut sealed);
    Some(b64(&out))
}

/// Opens a sealed bundle. `None` on anything at all — wrong key, tampering,
/// truncation: there is no useful distinction, and the caller fails the pairing.
fn open(key: &[u8; 32], sealed: &str) -> Option<Vec<u8>> {
    let raw = unb64(sealed)?;
    if raw.len() <= 24 {
        return None;
    }
    let (nonce, ciphertext) = raw.split_at(24);
    let nonce = XNonce::try_from(nonce).ok()?;
    let cipher = XChaCha20Poly1305::new(key.into());
    cipher.decrypt(&nonce, ciphertext).ok()
}

// ---------------------------------------------------------------------------
// IPC entry points.
// ---------------------------------------------------------------------------

/// `pairing.offer` — display a code for another device to scan.
///
/// The role is not the caller's to choose: this device sponsors when it can
/// actually vouch (it holds the account key AND is in the account), and joins
/// otherwise. Deriving it from the state rather than trusting a parameter is
/// what keeps a caller from asking to sponsor something it cannot sign, and it
/// covers the case a parameter would get wrong: a device that holds the key but
/// was revoked needs to *join* again, not to sponsor.
pub(crate) async fn offer(state: &Arc<AppState>) -> Result<Value, RpcErr> {
    let role = local_role(state);
    let epoch = EPOCH.fetch_add(1, Ordering::Relaxed) + 1;
    let channel = Channel::displaying();
    let wire = open_wire(state, epoch).await?;

    let mut params = json!({ "role": role.as_str(), "channel": b64(channel.ours.as_bytes()) });
    if role == Role::Joiner {
        // Whichever side the joiner is on, it declares what is to be confirmed
        // and enrolled: name, platform, key.
        params["device"] = declaration(state);
    }
    let created = match request(state, &wire, "pairing.create", params).await {
        Ok(created) => created,
        Err(err) => {
            wire.discard();
            return Err(err);
        }
    };
    let pairing_id = created["pairing_id"]
        .as_str()
        .ok_or_else(|| {
            wire.discard();
            RpcErr::app("SERVER_UNREACHABLE")
        })?
        .to_string();
    let expires_in = created["expires_in"].as_u64().unwrap_or(0);

    let code = Payload {
        pairing_id: pairing_id.clone(),
        psk: channel.psk,
        epk: channel.ours,
    }
    .encode();
    install(
        state,
        Pairing {
            epoch,
            pairing_id: pairing_id.clone(),
            role,
            channel,
            wire,
            expiry: None,
        },
        expires_in,
    );
    Ok(json!({
        "pairing_id": pairing_id,
        "code": code,
        "role": role.as_str(),
        "expires_in": expires_in,
    }))
}

/// `pairing.accept` — a code was scanned (or pasted): join the session.
///
/// The role comes back from the **server**, not from the code: the session is
/// what decides who is joining, and a payload that claimed otherwise would be
/// claiming it about someone else's session.
pub(crate) async fn accept(state: &Arc<AppState>, code: &str) -> Result<Value, RpcErr> {
    let payload = Payload::parse(code).ok_or_else(|| RpcErr::invalid_params("code"))?;
    let epoch = EPOCH.fetch_add(1, Ordering::Relaxed) + 1;
    let mut channel = Channel::scanning(payload.psk);
    // Derived before claiming: the code carries everything needed, and a code
    // whose public key is unusable must cost the server nothing.
    if channel
        .establish(&payload.epk, &payload.pairing_id)
        .is_none()
    {
        return Err(RpcErr::invalid_params("code"));
    }
    let wire = open_wire(state, epoch).await?;

    let mut params = json!({
        "pairing_id": payload.pairing_id,
        "channel": b64(channel.ours.as_bytes()),
    });
    // We do not know our role yet, so we declare ourselves unconditionally: the
    // server keeps the declaration only if we turn out to be the joiner, and
    // needs it in the same breath as the claim (it is what the sponsor is shown).
    params["device"] = declaration(state);
    let claimed = match request(state, &wire, "pairing.claim", params).await {
        Ok(claimed) => claimed,
        Err(err) => {
            wire.discard();
            return Err(err);
        }
    };
    let role = claimed["role"]
        .as_str()
        .and_then(Role::parse)
        .ok_or_else(|| {
            wire.discard();
            RpcErr::app("SERVER_UNREACHABLE")
        })?;
    let expires_in = claimed["expires_in"].as_u64().unwrap_or(0);

    // Told to sponsor with nothing to sponsor with: give the session back
    // rather than leave the other side waiting out the TTL for a bundle that
    // will never come.
    if role == Role::Sponsor && !holds_key(state) {
        let _ = request(
            state,
            &wire,
            "pairing.cancel",
            json!({ "pairing_id": payload.pairing_id }),
        )
        .await;
        wire.discard();
        return Err(RpcErr::app("NO_ACCOUNT_KEY"));
    }

    let device = claimed.get("device").cloned();
    install(
        state,
        Pairing {
            epoch,
            pairing_id: payload.pairing_id.clone(),
            role,
            channel,
            wire,
            expiry: None,
        },
        expires_in,
    );
    let mut result = json!({ "pairing_id": payload.pairing_id, "role": role.as_str() });
    if let Some(device) = device {
        result["device"] = device;
    }
    Ok(result)
}

/// `pairing.confirm` — the human said yes on the sponsor: seal the account key
/// for the device that was shown, and hand it over.
///
/// The server demands a **fresh ID token** here, exactly as it does for
/// `devices.revoke`: the keyring's refresh token mints one without a browser,
/// and otherwise the same re-auth round trip settles it. What that gate proves is
/// narrower than it looks — it is written down in `doc/server-api.md` and
/// `doc/architecture.md`, and the human-presence gate is this very confirmation.
pub(crate) async fn confirm(state: &Arc<AppState>, pairing_id: &str) -> Result<Value, RpcErr> {
    let (key, wire) = {
        let slot = state.pairing.lock().expect("lock pairing");
        let p = slot
            .as_ref()
            .filter(|p| p.pairing_id == pairing_id)
            .ok_or_else(|| RpcErr::app("PAIRING_UNKNOWN"))?;
        if p.role != Role::Sponsor {
            return Err(RpcErr::app("PAIRING_STATE"));
        }
        // No channel key means nobody has scanned yet: there is no one to seal
        // for, and the server would refuse the confirmation anyway.
        let key = p.channel.key.ok_or_else(|| RpcErr::app("PAIRING_STATE"))?;
        (key, p.wire.clone())
    };

    let bundle = seal_account(state, &key).ok_or_else(|| RpcErr::app("NO_ACCOUNT_KEY"))?;
    match crate::login::fresh_id_token(state).await {
        crate::login::FreshToken::Token(id_token) => {
            let result = request(
                state,
                &wire,
                "pairing.approve",
                json!({ "pairing_id": pairing_id, "id_token": id_token, "bundle": bundle }),
            )
            .await;
            match result {
                Ok(_) => {
                    settle(state, pairing_id, "pairing.completed", json!({}));
                    Ok(json!({ "status": "done" }))
                }
                // Not fresh enough for the server's taste: the browser settles
                // it, as it does for a revocation.
                Err(err) if err.app.as_deref() == Some("OIDC_INVALID") => {
                    confirm_reauth(state, pairing_id, bundle).await
                }
                Err(err) => Err(err),
            }
        }
        crate::login::FreshToken::NeedsReauth => confirm_reauth(state, pairing_id, bundle).await,
        crate::login::FreshToken::Unreachable => Err(RpcErr::app("SERVER_UNREACHABLE")),
    }
}

async fn confirm_reauth(
    state: &Arc<AppState>,
    pairing_id: &str,
    bundle: String,
) -> Result<Value, RpcErr> {
    let goal = crate::login::Goal::Pairing {
        pairing_id: pairing_id.to_string(),
        bundle,
    };
    let auth_url = crate::login::start_flow(state, goal).await?;
    Ok(json!({ "status": "reauth_required", "auth_url": auth_url }))
}

/// The outcome of a confirmation that went through the browser
/// (`login::Goal::Pairing`). The caller's `pairing.confirm` returned
/// `reauth_required` long ago, so the events are the only way back to it.
pub(crate) fn approved(state: &Arc<AppState>, pairing_id: &str, result: &Result<Value, RpcErr>) {
    match result {
        Ok(_) => settle(state, pairing_id, "pairing.completed", json!({})),
        Err(err) => settle(
            state,
            pairing_id,
            "pairing.failed",
            // The server's own word for it: a session that timed out while the
            // browser was open (`PAIRING_UNKNOWN`) is the ordinary outcome here,
            // and the interface has to be able to say which.
            json!({ "reason": err.app.as_deref().unwrap_or("server") }),
        ),
    }
}

/// `pairing.cancel` — the human declined, or the dialog closed. Idempotent: a
/// cancellation that names a pairing we no longer have is a `{}`, not an error —
/// the dialog it came from was closing either way.
pub(crate) async fn cancel(state: &Arc<AppState>, pairing_id: &str) -> Result<Value, RpcErr> {
    let Some(p) = take(state, pairing_id) else {
        return Ok(json!({}));
    };
    // The other side is told by the server rather than left waiting out the TTL.
    // Best-effort: the pairing is already gone from here.
    let _ = request(
        state,
        &p.wire,
        "pairing.cancel",
        json!({ "pairing_id": pairing_id }),
    )
    .await;
    p.abandon();
    Ok(json!({}))
}

// ---------------------------------------------------------------------------
// What the server says.
// ---------------------------------------------------------------------------

/// A `pairing.*` notification arriving on a server connection — the session
/// task's, or a pairing's own.
pub(crate) fn on_server_event(state: &Arc<AppState>, method: &str, params: &Value) {
    match method {
        "pairing.claimed" => on_claimed(state, params),
        "pairing.completed" => on_completed(state, params),
        "pairing.failed" => {
            let reason = params["reason"].as_str().unwrap_or("declined").to_string();
            fail_current(state, &reason);
        }
        _ => {}
    }
}

/// The other side scanned: establish the channel and tell the caller what it has
/// to show (nothing, when we are the one being shown).
fn on_claimed(state: &Arc<AppState>, params: &Value) {
    let outcome = {
        let mut slot = state.pairing.lock().expect("lock pairing");
        let Some(p) = slot.as_mut() else {
            return;
        };
        let their_key: Option<[u8; 32]> = params["channel"]
            .as_str()
            .and_then(unb64)
            .and_then(|b| b.try_into().ok());
        match their_key {
            Some(theirs)
                if p.channel
                    .establish(&PublicKey::from(theirs), &p.pairing_id)
                    .is_some() =>
            {
                let mut announced = json!({ "pairing_id": p.pairing_id });
                if let Some(device) = params.get("device") {
                    announced["device"] = device.clone();
                }
                Ok(announced)
            }
            // No sound channel, no pairing: carrying on would mean sealing the
            // account key under a key someone else may hold (fail-closed).
            _ => Err(slot.take().expect("just borrowed")),
        }
    };
    match outcome {
        Ok(announced) => notify(state, "pairing.claimed", announced),
        Err(p) => {
            p.abandon();
            notify(
                state,
                "pairing.failed",
                json!({ "pairing_id": p.pairing_id, "reason": "channel" }),
            );
        }
    }
}

/// The human confirmed on the other side: the bundle is here. Taken out of the
/// slot at once — from now on this pairing belongs to the completion, and
/// nothing can cancel or replay it.
fn on_completed(state: &Arc<AppState>, params: &Value) {
    let bundle = params["bundle"].as_str().unwrap_or_default().to_string();
    let Some(p) = state.pairing.lock().expect("lock pairing").take() else {
        return;
    };
    // The countdown stops here, but the pairing's own connection does NOT: the
    // completion is about to enroll on it. It is released when the completion
    // returns — the queue closes, and the loop ends by itself.
    p.disarm();
    tokio::spawn(complete_as_joiner(state.clone(), p, bundle));
}

/// Receives the account: open the bundle, keep the key, enter the directory if
/// we are not in it yet.
async fn complete_as_joiner(state: Arc<AppState>, p: Pairing, bundle: String) {
    let failed = |reason: &str| {
        notify(
            &state,
            "pairing.failed",
            json!({ "pairing_id": p.pairing_id, "reason": reason }),
        );
    };
    if p.role != Role::Joiner {
        // A sponsor has nothing to receive; only the server could have sent us
        // here, and it does not get to reverse the roles.
        failed("state");
        return;
    }
    let Some(key) = p.channel.key else {
        failed("state");
        return;
    };
    let Some(plain) = open(&key, &bundle).and_then(|b| serde_json::from_slice::<Value>(&b).ok())
    else {
        failed("bundle");
        return;
    };
    let Some(ak) = plain["seed"]
        .as_str()
        .and_then(|s| hex::decode(s).ok())
        .and_then(|b| <[u8; 32]>::try_from(b).ok())
        .map(|seed| SigningKey::from_bytes(&seed))
    else {
        failed("bundle");
        return;
    };

    // The account key goes in FIRST, before anything reaches the server: it
    // needs nobody's permission, so a failure here costs a clean retry rather
    // than a directory entry nothing will ever use. And it is the step that can
    // refuse — a key other than the one this device is already attested under is
    // another account's, whatever the human confirmed over there.
    let root = match crate::account_key::install(&state, &ak) {
        Ok(root) => root,
        Err(crate::account_key::InstallError::OtherKey) => {
            failed("other_account");
            return;
        }
        Err(crate::account_key::InstallError::SaveFailed) => {
            failed("install");
            return;
        }
    };

    if state.session.lock().expect("lock session").logged_in {
        // Already in the directory (a device that had joined the account before
        // the key was kept at rest, or one that has just been re-enrolled): the
        // attestation is all that is missing.
        crate::session::publish_attestation(&state, &root).await;
    } else if let Err(reason) = enroll_on_grant(&state, &p, plain.get("account")).await {
        tracing::warn!(error = %reason, "enrollment on the pairing failed");
        failed("enroll");
        return;
    }
    notify(
        &state,
        "pairing.completed",
        json!({ "pairing_id": p.pairing_id }),
    );
}

/// Enters the directory on the strength of the confirmation: challenge, then
/// `auth.enroll` with the pairing as the credential. No ID token — that is the
/// point of pairing — and the record comes from the session the human confirmed,
/// not from this request.
async fn enroll_on_grant(
    state: &Arc<AppState>,
    p: &Pairing,
    account: Option<&Value>,
) -> Result<(), String> {
    let Wire::Direct { url, .. } = &p.wire else {
        // A `Wire::Session` means we are logged in, which the caller has just
        // established otherwise.
        return Err("no connection of our own to enroll on".to_string());
    };
    let challenge = request(state, &p.wire, "auth.challenge", json!({}))
        .await
        .map_err(|e| format!("challenge: {}", e.message))?;
    let nonce = challenge["nonce"]
        .as_str()
        .ok_or("challenge without a nonce")?;
    let enrolled = request(
        state,
        &p.wire,
        "auth.enroll",
        json!({
            "pairing_id": p.pairing_id,
            "proof": state.identity.proof(nonce),
        }),
    )
    .await
    .map_err(|e| format!("enroll: {}", e.message))?;
    let device_id = enrolled["device_id"]
        .as_str()
        .ok_or("enroll without a device_id")?;

    crate::login::open_session(
        state,
        crate::session::SessionInfo {
            server_url: url.clone(),
            device_id: device_id.to_string(),
            // Whom the account belongs to, as the sponsor named it: a label for
            // the interface. The account this device really landed in is the one
            // the grant said, and the server is what settled that.
            account: account.filter(|a| !a.is_null()).cloned(),
        },
        // No refresh token: nothing here went through the IdP. A later sensitive
        // operation will open a browser once, as it does on a device whose token
        // has expired.
        None,
    )
}

// ---------------------------------------------------------------------------
// The pairing's own connection.
// ---------------------------------------------------------------------------

/// Opens the wire this device has to use: its session connection when it has
/// one, otherwise a connection of its own.
async fn open_wire(state: &Arc<AppState>, epoch: u64) -> Result<Wire, RpcErr> {
    let logged_in = state.session.lock().expect("lock session").logged_in;
    if logged_in {
        // The authenticated connection, deliberately: it is what tells the
        // server our account, and the server pins the confirmation and the
        // grant to the very connection that took part.
        if state
            .session
            .lock()
            .expect("lock session")
            .server_tx
            .is_none()
        {
            return Err(RpcErr::app("SERVER_UNREACHABLE"));
        }
        return Ok(Wire::Session);
    }
    let url = state
        .server_config
        .lock()
        .expect("lock server_config")
        .as_ref()
        .map(|c| c.url.clone())
        // Nothing configured: there is no rendezvous to go to. The interface
        // asks for the server's address before it offers to pair — the same
        // order a login follows.
        .ok_or_else(|| RpcErr::app("SERVER_UNREACHABLE"))?;
    let ws = crate::session::open_ws(state, &url).await.map_err(|e| {
        tracing::warn!(error = %e, "opening the pairing connection failed");
        RpcErr::app("SERVER_UNREACHABLE")
    })?;
    let (tx, rx) = mpsc::channel(DIRECT_QUEUE_DEPTH);
    let task = tokio::spawn(direct_loop(state.clone(), ws, rx, epoch));
    Ok(Wire::Direct {
        url,
        tx,
        abort: task.abort_handle(),
    })
}

impl Wire {
    /// Gives up a wire that never became a pairing (the server refused the
    /// create, or answered something unreadable).
    fn discard(&self) {
        if let Wire::Direct { abort, .. } = self {
            abort.abort();
        }
    }
}

/// Serves a pairing's own connection: requests out, replies and notifications
/// in. Ends when the pairing lets go of it (the queue closes), or when the
/// connection dies — in which case whoever was waiting is told, rather than left
/// until the TTL.
async fn direct_loop(
    state: Arc<AppState>,
    mut ws: crate::session::ServerWs,
    mut cmd_rx: mpsc::Receiver<ServerCmd>,
    epoch: u64,
) {
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message;

    let mut next_id = 0u64;
    let mut pending: std::collections::HashMap<u64, oneshot::Sender<Result<Value, RpcErr>>> =
        std::collections::HashMap::new();
    loop {
        tokio::select! {
            cmd = cmd_rx.recv() => {
                // Every sender gone: the pairing is settled, and so are we.
                let Some(cmd) = cmd else { return };
                next_id += 1;
                let id = next_id;
                let msg = json!({
                    "jsonrpc": "2.0", "id": id,
                    "method": cmd.method, "params": cmd.params,
                });
                if ws.send(Message::text(msg.to_string())).await.is_err() {
                    let _ = cmd.reply.send(Err(RpcErr::app("SERVER_UNREACHABLE")));
                    break;
                }
                pending.insert(id, cmd.reply);
            }
            msg = ws.next() => match msg {
                Some(Ok(Message::Text(text))) => {
                    let Ok(v) = serde_json::from_str::<Value>(&text) else { continue };
                    if let Some(method) = v.get("method").and_then(Value::as_str) {
                        let params = v.get("params").cloned().unwrap_or_else(|| json!({}));
                        on_server_event(&state, method, &params);
                        continue;
                    }
                    if let Some(id) = v.get("id").and_then(Value::as_u64)
                        && let Some(reply) = pending.remove(&id)
                    {
                        let result = match v.get("error") {
                            Some(err) => Err(RpcErr::from_value(err)),
                            None => Ok(v.get("result").cloned().unwrap_or(Value::Null)),
                        };
                        let _ = reply.send(result);
                    }
                }
                Some(Ok(_)) => {}
                Some(Err(_)) | None => break,
            },
        }
    }
    // The rendezvous is gone. Only OUR pairing is settled — a newer one has its
    // own connection.
    if state
        .pairing
        .lock()
        .expect("lock pairing")
        .as_ref()
        .is_some_and(|p| p.epoch == epoch)
    {
        fail_current(&state, "server");
    }
}

// ---------------------------------------------------------------------------
// The slot, and what settles it.
// ---------------------------------------------------------------------------

/// Installs a pairing as the current one, arms its expiry, and settles whatever
/// was there before. Replacing rather than refusing: a dialog closed and
/// reopened must work, and the server retires the previous session of a
/// connection on its own.
fn install(state: &Arc<AppState>, mut pairing: Pairing, expires_in: u64) {
    let epoch = pairing.epoch;
    let expiry = {
        let state = state.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(expires_in)).await;
            if state
                .pairing
                .lock()
                .expect("lock pairing")
                .as_ref()
                .is_some_and(|p| p.epoch == epoch)
            {
                // The server says nothing when a session times out — both sides
                // were told the deadline and count it themselves, which is exact.
                // This is where that count is kept.
                fail_current(&state, "expired");
            }
        })
    };
    pairing.expiry = Some(expiry.abort_handle());
    let previous = state.pairing.lock().expect("lock pairing").replace(pairing);
    if let Some(previous) = previous {
        previous.abandon();
    }
}

/// Takes the current pairing if it is the one named. `None` otherwise — expired,
/// already settled, or never this Core's.
fn take(state: &AppState, pairing_id: &str) -> Option<Pairing> {
    let mut slot = state.pairing.lock().expect("lock pairing");
    if slot.as_ref().is_some_and(|p| p.pairing_id == pairing_id) {
        slot.take()
    } else {
        None
    }
}

/// Settles the named pairing with a notification. Nothing when it is no longer
/// the current one: a late outcome must not speak for its successor.
fn settle(state: &Arc<AppState>, pairing_id: &str, method: &str, mut params: Value) {
    let Some(p) = take(state, pairing_id) else {
        return;
    };
    p.abandon();
    params["pairing_id"] = json!(pairing_id);
    notify(state, method, params);
}

/// Ends the current pairing, whatever it is, and says why.
fn fail_current(state: &Arc<AppState>, reason: &str) {
    let Some(p) = state.pairing.lock().expect("lock pairing").take() else {
        return;
    };
    p.abandon();
    notify(
        state,
        "pairing.failed",
        json!({ "pairing_id": p.pairing_id, "reason": reason }),
    );
}

fn notify(state: &AppState, method: &str, params: Value) {
    state
        .registry
        .lock()
        .expect("lock registry")
        .notify_topic("pairing", method, &params);
}

// ---------------------------------------------------------------------------
// Local state, read the way the pairing needs it.
// ---------------------------------------------------------------------------

/// Which end this device is on when it displays a code. Sponsoring takes both
/// halves of the ability: the key to vouch with, and a place in the account at
/// the rendezvous — the server demands an authenticated connection to confirm.
fn local_role(state: &AppState) -> Role {
    let logged_in = state.session.lock().expect("lock session").logged_in;
    if logged_in && holds_key(state) {
        Role::Sponsor
    } else {
        Role::Joiner
    }
}

/// Does this device hold the account's private key? Read back from the keyring,
/// like `account.status`: on the desktop a write is queued, so only a read tells
/// the truth about what can actually be signed.
fn holds_key(state: &AppState) -> bool {
    account_key(state).is_some()
}

/// The account key, if this device holds one for the account it is attested
/// under. `ak_pub` is cloned out of its lock first: the keyring read can block,
/// and `account_root` sits on the data plane's authorization path.
fn account_key(state: &AppState) -> Option<SigningKey> {
    let ak_pub = state
        .account_root
        .lock()
        .expect("lock account_root")
        .as_ref()
        .map(|r| r.ak_pub.clone())?;
    crate::account_key::recall(&*state.secrets, &ak_pub)
}

/// Seals the account for the device that was confirmed: the key's seed, and the
/// account's identity as this device knows it (a label for the other side's
/// interface — the account it really joins is the one the grant settled).
///
/// The seed is read here and nowhere else: a pairing waiting for a human to
/// confirm holds no key material beyond the channel's.
fn seal_account(state: &AppState, key: &[u8; 32]) -> Option<String> {
    let ak = account_key(state)?;
    let mut plain = json!({ "seed": hex::encode(ak.to_bytes()) });
    if let Some(account) = &state.session.lock().expect("lock session").account {
        plain["account"] = account.clone();
    }
    seal(key, plain.to_string().as_bytes())
}

/// What this device declares about itself when it is the one joining. The same
/// fields `auth.enroll` takes on the OIDC path — this is the very record that
/// ends up in the directory, and what the human confirms.
fn declaration(state: &AppState) -> Value {
    json!({
        "name": state.device_name,
        "platform": std::env::consts::OS,
        "node_id": state.identity.node_id(),
    })
}

/// A request on this pairing's wire.
async fn request(
    state: &AppState,
    wire: &Wire,
    method: &'static str,
    params: Value,
) -> Result<Value, RpcErr> {
    match wire {
        Wire::Session => crate::session::proxy(state, method, params).await,
        Wire::Direct { tx, .. } => {
            let unreachable = || RpcErr::app("SERVER_UNREACHABLE");
            let (reply_tx, reply_rx) = oneshot::channel();
            tx.try_send(ServerCmd {
                method,
                params,
                reply: reply_tx,
            })
            .map_err(|_| unreachable())?;
            match tokio::time::timeout(DIRECT_TIMEOUT, reply_rx).await {
                Ok(Ok(result)) => result,
                Ok(Err(_)) | Err(_) => Err(unreachable()),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    //! The payload and the channel, which are what the two ends have to agree on
    //! without the server. Everything above them (the state machine, the two
    //! wires, enrolling on a grant) is exercised against the real server in
    //! `tests/api/pairing.rs`.

    use super::*;

    /// The two sides of one pairing, sharing the optical secret the way a screen
    /// and a camera share it.
    fn two_ends() -> (Channel, Channel) {
        let offerer = Channel::displaying();
        // The scanner takes the secret off the screen, exactly as `accept` does:
        // a channel built any other way would hide the very mistake this is
        // shaped to prevent.
        let claimer = Channel::scanning(offerer.psk);
        (offerer, claimer)
    }

    #[test]
    fn a_code_round_trips() {
        let channel = Channel::displaying();
        let payload = Payload {
            pairing_id: "p_0011223344556677".to_string(),
            psk: channel.psk,
            epk: channel.ours,
        };
        let parsed = Payload::parse(&payload.encode()).expect("our own code parses");
        assert_eq!(parsed.pairing_id, payload.pairing_id);
        assert_eq!(parsed.psk, payload.psk);
        assert_eq!(parsed.epk.as_bytes(), payload.epk.as_bytes());
        // A pasted code brings its whitespace along.
        let padded = format!("  {}\n", payload.encode());
        assert_eq!(
            Payload::parse(&padded).expect("a pasted code parses").psk,
            payload.psk
        );
    }

    #[test]
    fn the_session_id_survives_whatever_is_in_it() {
        // The id is the LAST field precisely so that a separator inside it is
        // not a parsing accident waiting to happen.
        let channel = Channel::displaying();
        let payload = Payload {
            pairing_id: "p_with:colons:inside".to_string(),
            psk: channel.psk,
            epk: channel.ours,
        };
        assert_eq!(
            Payload::parse(&payload.encode())
                .expect("parses")
                .pairing_id,
            "p_with:colons:inside"
        );
    }

    #[test]
    fn what_is_not_a_code_is_refused() {
        let sound = Payload {
            pairing_id: "p_1".to_string(),
            psk: [7u8; PSK_LEN],
            epk: Channel::displaying().ours,
        }
        .encode();
        let fields: Vec<&str> = sound.split(':').collect();
        for wrong in [
            "".to_string(),
            "hello".to_string(),
            // Another scheme's payload, or a future version of ours.
            sound.replacen(PAYLOAD_TAG, "UL2", 1),
            sound.replacen(PAYLOAD_TAG, "ul1", 1),
            // A field short, a field too many is fine (absorbed by the id), but
            // an empty id is not a session.
            format!("{}:{}:{}", fields[0], fields[1], fields[2]),
            format!("{}:{}:{}:", fields[0], fields[1], fields[2]),
            // Unreadable or wrongly sized halves.
            format!("{}:not-base64!:{}:{}", fields[0], fields[2], fields[3]),
            format!(
                "{}:{}:{}:{}",
                fields[0],
                b64(&[7u8; 15]),
                fields[2],
                fields[3]
            ),
            format!(
                "{}:{}:{}:{}",
                fields[0],
                fields[1],
                b64(&[7u8; 31]),
                fields[3]
            ),
            format!(
                "{}:{}:{}:{}",
                fields[0],
                fields[1],
                b64(&[7u8; 33]),
                fields[3]
            ),
        ] {
            assert!(
                Payload::parse(&wrong).is_none(),
                "accepted as a pairing code: {wrong:?}"
            );
        }
        assert!(
            Payload::parse(&sound).is_some(),
            "the sound one still parses"
        );
    }

    #[test]
    fn both_ends_derive_the_same_channel_key() {
        let (mut offerer, mut claimer) = two_ends();
        let theirs = claimer.ours;
        let ours = offerer.ours;
        let a = offerer.establish(&theirs, "p_same").expect("offerer's key");
        let b = claimer.establish(&ours, "p_same").expect("claimer's key");
        assert_eq!(a, b, "the two ends must agree, or nothing opens");
    }

    #[test]
    fn a_key_is_spent_once() {
        let (mut offerer, claimer) = two_ends();
        let theirs = claimer.ours;
        assert!(offerer.establish(&theirs, "p_1").is_some());
        assert!(
            offerer.establish(&theirs, "p_1").is_none(),
            "the ephemeral secret is gone with the first exchange — a second \
             `pairing.claimed` must not re-key an established channel"
        );
    }

    #[test]
    fn the_optical_secret_is_part_of_the_key() {
        // Whoever recorded the whole conversation holds both public keys and the
        // session id. Without the secret that travelled by the screen, that is
        // not a key.
        let (mut offerer, mut claimer) = two_ends();
        let theirs = claimer.ours;
        let ours = offerer.ours;
        let real = offerer.establish(&theirs, "p_1").expect("key");
        claimer.psk = [0u8; PSK_LEN];
        let without = claimer.establish(&ours, "p_1").expect("key");
        assert_ne!(
            real, without,
            "the pre-shared secret must enter the derivation"
        );
    }

    #[test]
    fn the_key_is_bound_to_its_session() {
        let (mut offerer, mut claimer) = two_ends();
        let theirs = claimer.ours;
        let ours = offerer.ours;
        let here = offerer.establish(&theirs, "p_here").expect("key");
        let elsewhere = claimer.establish(&ours, "p_elsewhere").expect("key");
        assert_ne!(
            here, elsewhere,
            "the session id is in the transcript: a server that hands the same \
             material to two sessions cannot cross them"
        );
    }

    #[test]
    fn a_dictated_shared_secret_is_refused() {
        // A low-order point forces a shared secret everybody can compute. The
        // exchange must be refused rather than keyed on a public value.
        let mut channel = Channel::displaying();
        let identity = PublicKey::from([0u8; 32]);
        assert!(
            channel.establish(&identity, "p_1").is_none(),
            "a non-contributory exchange must not produce a channel"
        );
        assert!(channel.key.is_none());
    }

    #[test]
    fn a_bundle_opens_only_under_its_own_key() {
        let (mut offerer, mut claimer) = two_ends();
        let theirs = claimer.ours;
        let ours = offerer.ours;
        let sender = offerer.establish(&theirs, "p_1").expect("key");
        let receiver = claimer.establish(&ours, "p_1").expect("key");

        let sealed = seal(&sender, b"{\"seed\":\"cafe\"}").expect("seal");
        assert!(
            !sealed.contains("seed") && !sealed.contains("cafe"),
            "the bundle must not carry its plaintext: {sealed}"
        );
        assert_eq!(
            open(&receiver, &sealed).as_deref(),
            Some(&b"{\"seed\":\"cafe\"}"[..])
        );

        // Anyone else's key, a tampered byte, a truncation: all the same answer.
        assert!(open(&[0u8; 32], &sealed).is_none(), "wrong key");
        let mut raw = unb64(&sealed).expect("base64");
        let last = raw.len() - 1;
        raw[last] ^= 0x01;
        assert!(open(&receiver, &b64(&raw)).is_none(), "tampered tag");
        assert!(
            open(&receiver, &b64(&raw[..24])).is_none(),
            "nonce and nothing else"
        );
        assert!(open(&receiver, "not base64").is_none(), "unreadable");
    }

    #[test]
    fn two_seals_of_one_message_differ() {
        // The nonce is fresh every time, so the same bundle sealed twice does not
        // reveal that it was the same bundle.
        let key = [3u8; 32];
        let a = seal(&key, b"same").expect("seal");
        let b = seal(&key, b"same").expect("seal");
        assert_ne!(a, b);
        assert_eq!(open(&key, &a).as_deref(), Some(&b"same"[..]));
        assert_eq!(open(&key, &b).as_deref(), Some(&b"same"[..]));
    }
}
