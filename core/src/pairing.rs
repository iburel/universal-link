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
//! precisely what the confirmation screen is for. What the human is shown there
//! is the device that scanned AND a **number derived from the channel key**
//! ([`verification`]): only the two ends of one exchange can compute it, so an
//! intruder's number cannot match the one on the device in the user's hand. The
//! name is recognition; the number is the check.
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
use tokio::io::AsyncWriteExt;
use tokio::sync::{mpsc, oneshot};
use x25519_dalek::{EphemeralSecret, PublicKey, SharedSecret};

use crate::connector::IoStream;
use crate::dataplane::{self, PeerAddr};
use crate::directory::Roster;
use crate::rpc::RpcErr;
use crate::state::{AppState, ServerCmd};

/// Version tag of the payload a QR code (or a pasted text code) carries. A
/// version, not decoration: it is what lets a later scheme be told apart from
/// this one instead of being half-parsed.
///
/// Re-exported from the crate root (`PAIRING_CODE_TAG`) for the one caller that
/// needs it outside the Core: a camera scanner has to know which QR code in its
/// view is a pairing code, and that question has exactly one right answer per
/// version of this format.
pub const PAYLOAD_TAG: &str = "UL1";

/// Bytes of pre-shared secret in the payload. 128 bits: unguessable, and short
/// enough to keep the QR code small.
const PSK_LEN: usize = 16;

/// Domain separation for the channel key. Versioned like the rest of the
/// project's derivations (`account_key`).
const CHANNEL_DOMAIN: &[u8] = b"ul-pairing-channel-v1";

/// Domain separation for the confirmation number (see [`verification`]).
const SAS_DOMAIN: &[u8] = b"ul-pairing-sas-v1";

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
    /// It does not: there is no server in this device's life, and the other
    /// device is dialled on the local network instead (see "Pairing on the local
    /// network" below). Nothing here is ever asked of a server.
    Lan,
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
        self.settle(theirs, CHANNEL_DOMAIN, pairing_id)
    }

    /// The same exchange for a pairing with no server in it: bound to the
    /// DISPLAYING device's `node_id` rather than to a session id (there is no
    /// server to mint one, and the `node_id` is the one field of the code the
    /// transport itself authenticates), under a domain of its own so that neither
    /// scheme's key can ever pass for the other's.
    fn establish_lan(&mut self, theirs: &PublicKey, node_id: &str) -> Option<[u8; 32]> {
        self.settle(theirs, LAN_CHANNEL_DOMAIN, node_id)
    }

    fn settle(&mut self, theirs: &PublicKey, domain: &[u8], binding: &str) -> Option<[u8; 32]> {
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
        let key = derive_key(domain, &shared, &self.psk, offerer, claimer, binding);
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
    /// LAN pairing only: how the human's yes reaches the task that holds the
    /// stream. Its absence is how that task hears everything else — a
    /// cancellation, an expiry, another pairing taking this slot all DROP this
    /// sender, and the task then owes the other device a refusal rather than
    /// silence. On the server path there is nothing to hand over: the
    /// confirmation is a request of its own, and the server carries it.
    confirm: Option<oneshot::Sender<()>>,
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
/// both public keys in the order display-then-scan, and what identifies this one
/// exchange — so a key derived for one pairing cannot be mistaken for another's,
/// whatever the server hands out. `domain` is the scheme: a code relayed by a
/// server and one dialled on the local network derive under labels of their own,
/// so a channel of one can never be a channel of the other.
fn derive_key(
    domain: &[u8],
    shared: &SharedSecret,
    psk: &[u8; PSK_LEN],
    offerer: &PublicKey,
    claimer: &PublicKey,
    binding: &str,
) -> [u8; 32] {
    let mut info = Vec::with_capacity(domain.len() + 64 + binding.len());
    info.extend_from_slice(domain);
    info.extend_from_slice(offerer.as_bytes());
    info.extend_from_slice(claimer.as_bytes());
    info.extend_from_slice(binding.as_bytes());
    let mut key = [0u8; 32];
    hkdf::Hkdf::<Sha256>::new(Some(psk), shared.as_bytes())
        .expand(&info, &mut key)
        .expect("32 bytes is a valid HKDF-SHA256 output length");
    key
}

/// The number both ends put in front of their human, so that the confirmation
/// screen asks something an intruder cannot answer.
///
/// It comes out of the **channel key**, which only the two ends of this one
/// exchange hold: whoever photographed the code and claimed the session first has
/// a channel of its own, hence different digits from the ones showing on the
/// device the user is actually holding. A name is not that check — the joining
/// side declares its own, and an intruder picks a convincing one. Six digits, two
/// groups, like the account fingerprint: the point is a human reading them aloud.
///
/// Another output of the same KDF, with its own label: the key is already a PRK,
/// and a digest of it says nothing about it.
fn verification(key: &[u8; 32]) -> String {
    let mut out = [0u8; 4];
    hkdf::Hkdf::<Sha256>::from_prk(key)
        .expect("a 32-byte channel key is a valid HKDF-SHA256 PRK")
        .expand(SAS_DOMAIN, &mut out)
        .expect("4 bytes is a valid HKDF-SHA256 output length");
    let n = u32::from_be_bytes(out) % 1_000_000;
    format!("{:03} {:03}", n / 1000, n % 1000)
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
    // No server in this device's life: there is no rendezvous to go to, and none
    // is needed — the code says whom to dial and the other device dials it (see
    // "Pairing on the local network" below).
    if crate::state::serverless(state) {
        return offer_lan(state).await;
    }
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
            // The server path confirms through a request of its own.
            confirm: None,
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
    // The code's own version tag decides which scheme this is — that is what a
    // version tag is for, and it is why a LAN code is `UL2` and not a `UL1` with a
    // field missing.
    if let Some(payload) = LanPayload::parse(code) {
        return accept_lan(state, payload).await;
    }
    let payload = Payload::parse(code).ok_or_else(|| RpcErr::invalid_params("code"))?;
    let epoch = EPOCH.fetch_add(1, Ordering::Relaxed) + 1;
    let mut channel = Channel::scanning(payload.psk);
    // Derived before claiming: the code carries everything needed, and a code
    // whose public key is unusable must cost the server nothing.
    let Some(key) = channel.establish(&payload.epk, &payload.pairing_id) else {
        return Err(RpcErr::invalid_params("code"));
    };
    let number = verification(&key);
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
            // The server path confirms through a request of its own.
            confirm: None,
        },
        expires_in,
    );
    let mut result = json!({
        "pairing_id": payload.pairing_id,
        "role": role.as_str(),
        "verification": number,
    });
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
        let mut slot = state.pairing.lock().expect("lock pairing");
        let p = slot
            .as_mut()
            .filter(|p| p.pairing_id == pairing_id)
            .ok_or_else(|| RpcErr::app("PAIRING_UNKNOWN"))?;
        if p.role != Role::Sponsor {
            return Err(RpcErr::app("PAIRING_STATE"));
        }
        // No channel key means nobody has scanned yet: there is no one to seal
        // for, and the server would refuse the confirmation anyway.
        let key = p.channel.key.ok_or_else(|| RpcErr::app("PAIRING_STATE"))?;
        if matches!(p.wire, Wire::Lan) {
            // On the local network the bundle is sealed and sent by the task that
            // holds the stream, not here: with no server to relay anything, the
            // conversation is already open and this call is the yes it was waiting
            // for. `PAIRING_STATE` if that task is gone — a pairing whose stream
            // died has nothing left to confirm. And no `reauth_required` on this
            // path ever: there is no OIDC to be fresh with.
            return p
                .confirm
                .take()
                .and_then(|confirm| confirm.send(()).ok())
                .map(|()| json!({ "status": "done" }))
                .ok_or_else(|| RpcErr::app("PAIRING_STATE"));
        }
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
    //
    // Nothing to tell a server that is not there: on the local network, dropping
    // the pairing drops the confirmation channel, and that is what tells the task
    // holding the stream to refuse for us (`lan_sponsor`).
    if !matches!(p.wire, Wire::Lan) {
        let _ = request(
            state,
            &p.wire,
            "pairing.cancel",
            json!({ "pairing_id": pairing_id }),
        )
        .await;
    }
    p.abandon();
    Ok(json!({}))
}

// ---------------------------------------------------------------------------
// What the server says.
// ---------------------------------------------------------------------------

/// A `pairing.*` notification arriving on a server connection — the session
/// task's, or a pairing's own.
///
/// A server does not get to speak for a pairing it is not part of. Today that is
/// unreachable — a LAN pairing exists only where there is no server at all
/// (`crate::state::serverless`), so there is no connection for such a
/// notification to arrive on — and it is written down anyway, because the two
/// schemes share one slot and the failure it would cause (a server re-keying a
/// channel keyed by a screen) is the kind nobody would think to look for.
pub(crate) fn on_server_event(state: &Arc<AppState>, method: &str, params: &Value) {
    if state
        .pairing
        .lock()
        .expect("lock pairing")
        .as_ref()
        .is_some_and(|p| matches!(p.wire, Wire::Lan))
    {
        tracing::warn!(
            method,
            "a server spoke for a pairing on the local network: ignored"
        );
        return;
    }
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
/// to show — the confirmation number both ends now share, and the device record
/// when we are the one who has to decide.
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
        match their_key
            .and_then(|theirs| p.channel.establish(&PublicKey::from(theirs), &p.pairing_id))
        {
            Some(key) => {
                let mut announced = json!({
                    "pairing_id": p.pairing_id,
                    // Both ends are told it here, and only here: this is the
                    // first moment either of them has a channel to derive it
                    // from, and the moment a human is asked to compare.
                    "verification": verification(&key),
                });
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
    let Some((ak, account)) = open_bundle(&key, &bundle) else {
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
        // Already in the directory — a device that has the account but not its
        // key, or one that has just been re-enrolled: the attestation is all that
        // is missing.
        crate::session::publish_attestation(&state, &root).await;
    } else if let Err(reason) = enroll_on_grant(&state, &p, account.as_ref()).await {
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

/// Ends the current pairing, whatever it is, and says why. `pub(crate)` for one
/// outside caller: a device leaving the account (`account_key::leave`), whose
/// pairing — either end of it — has just lost its standing.
pub(crate) fn fail_current(state: &Arc<AppState>, reason: &str) {
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
    let ak_pub = account_pub(state)?;
    crate::account_key::recall(&*state.secrets, &ak_pub)
}

/// The account's PUBLIC key, if this device is in an account at all — cloned out
/// of the leaf lock, never held (see [`account_key`]).
fn account_pub(state: &AppState) -> Option<String> {
    state
        .account_root
        .lock()
        .expect("lock account_root")
        .as_ref()
        .map(|r| r.ak_pub.clone())
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

/// Opens a bundle and reads what a joiner is owed: the account key, and the
/// account's identity as the sponsor knew it (a label for the interface — the
/// account this device really lands in is settled by what the key derives).
///
/// `None` on anything at all: a bundle that does not open, does not parse, or
/// carries no usable seed. One place, shared by both pairing schemes — what a
/// bundle IS should not be a thing two paths each have their own idea of.
fn open_bundle(key: &[u8; 32], sealed: &str) -> Option<(SigningKey, Option<Value>)> {
    let plain: Value = serde_json::from_slice(&open(key, sealed)?).ok()?;
    let ak = plain["seed"]
        .as_str()
        .and_then(|s| hex::decode(s).ok())
        .and_then(|b| <[u8; 32]>::try_from(b).ok())
        .map(|seed| SigningKey::from_bytes(&seed))?;
    let account = plain.get("account").filter(|a| !a.is_null()).cloned();
    Some((ak, account))
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
        // A LAN pairing asks a server nothing, and every caller of this is a
        // server path. Fail-closed rather than a panic: a mistake here has to cost
        // a pairing, not the Core.
        Wire::Lan => Err(RpcErr::app("PAIRING_STATE")),
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

// ---------------------------------------------------------------------------
// Pairing on the local network, with no server at all.
// ---------------------------------------------------------------------------
//
// An account with no server has, until now, had exactly one way in: the recovery
// code, typed into every machine. Which works, and leaves each machine knowing
// only itself — the exchange that carries the directory between devices
// (`dirsync`) needs each of them to already hold the other's record. This is the
// first introduction, and it is what makes that exchange run at all.
//
// # The protocol (over `dataplane::ALPN`, one bidirectional stream)
//
// ```text
// dialer   → displayer  { "type": "lan_pair",    epk, proof, record, holds_key, account? }
// displayer → dialer    { "type": "lan_hello",   record, holds_key, account? }
//                     | { "type": "lan_refused", reason }
//   ... the roles are settled, both ends show the same number, a human confirms
// sponsor  → joiner     { "type": "pair_grant",  bundle, records, revoked }
//                     | { "type": "lan_refused", reason }
// joiner   → sponsor    { "type": "pair_roster", records, revoked }
// ```
//
// The device that DISPLAYS the code is the one that gets dialled — the code says
// whom to dial, which is the field a server used to be. No new ALPN: the data
// plane dispatches on the first frame's type, and `lan_pair` is the one frame it
// serves to a device that is not in the directory yet (`dataplane::serve`), and
// only while a window is open here.
//
// # What holds it up, with no server anywhere
//
// **Both `node_id`s are authenticated by the transport**, which is more than the
// server path can say: the dialer reached exactly the key the code names (iroh
// authenticates the remote end), and the displayer is handed the dialer's key by
// the transport rather than told it in a frame. So neither side can be POSED as,
// and a description that does not match the key that sent it is refused
// (`standing_of`). What is left for an attacker is a race, and that is what the
// confirmation number catches — the same asymmetry the server path lives with.
//
// **The dialer proves it read the code off a screen** before the displaying side
// spends anything: a MAC over both public halves and the `node_id`, keyed by the
// code's 128-bit secret (`dialer_proof`). Checked BEFORE the ephemeral secret is
// consumed, deliberately — otherwise anything on the network could burn a pairing
// window for the device the human is actually holding.
//
// **The account key still crosses sealed**, under a channel keyed by that same
// optical secret, exactly as it does through a server. The transport is already
// encrypted, so this is belt on braces; it is worth it because there is then ONE
// rule about the seed rather than two, and the weaker one would be the one that
// mattered.
//
// **Two devices do not tell the network which account they are in**: whether they
// belong to the same one is compared as a MAC over the account's public key under
// the channel key (`account_mark`), which only the two ends of this one exchange
// can compute.
//
// # Who ends up sponsoring
//
// The device that holds the account's private key vouches; the other joins. When
// BOTH hold it — two devices of one account that have never met, which is exactly
// what an account looks like when the recovery code was typed into each machine —
// the one that displayed the code sponsors, and the exchange is the same one: the
// key that crosses is the key the other side already has, and `account_key::install`
// takes it as the no-op it is. What the two of them are really doing then is
// swapping rosters, which is the whole point.

/// Version tag of the payload a code carries when there is no server to be the
/// rendezvous. A version of its own rather than a `UL1` with a field standing for
/// something else: the two schemes derive different keys, and a code that
/// half-parses is worse than one that does not parse at all.
pub const LAN_PAYLOAD_TAG: &str = "UL2";

/// Domain separation for the channel key of a pairing with no server in it.
const LAN_CHANNEL_DOMAIN: &[u8] = b"ul-lanpair-channel-v1";

/// Domain separation for the dialer's proof that it read the code off a screen.
const LAN_PROOF_DOMAIN: &[u8] = b"ul-lanpair-proof-v1";

/// Domain separation for the mark that tells two devices whether they are in the
/// same account without either of them naming it.
const LAN_ACCOUNT_DOMAIN: &[u8] = b"ul-lanpair-account-v1";

/// Frame types. `lan_pair` is the one a device outside the directory may send.
const LAN_OFFER: &str = "lan_pair";
const LAN_HELLO: &str = "lan_hello";
const LAN_REFUSED: &str = "lan_refused";
const LAN_GRANT: &str = "pair_grant";
const LAN_ROSTER: &str = "pair_roster";

/// How long a LAN code is good for. Long enough to carry a phone into the next
/// room, short enough that a window nobody used does not stay open — and it is
/// this window, and only it, that lets a stranger's frame be read at all.
const LAN_TTL: Duration = Duration::from_secs(180);

/// What we allow the transport to come up in before a code goes on screen. Past
/// it the code is handed out anyway: the endpoint may well finish binding while
/// the human is walking to the other machine, and the dialer has a budget of its
/// own.
const LAN_LISTEN_BUDGET: Duration = Duration::from_secs(10);

/// Budget for reaching the device whose code was read. On a local network this is
/// a handful of milliseconds; the budget is for the case where it is not there.
const LAN_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Budget for a frame the other side sends with no human in between.
const LAN_FRAME_TIMEOUT: Duration = Duration::from_secs(10);

/// What the last writer allows itself to see the stream close (the QUIC lifecycle
/// the transfer protocol learned the hard way).
const LAN_LINGER: Duration = Duration::from_secs(5);

/// What a device with no server puts on screen: the optical secret, the public
/// half of its channel, and the device to dial.
struct LanPayload {
    psk: [u8; PSK_LEN],
    epk: PublicKey,
    node_id: String,
}

impl LanPayload {
    /// `UL2:<psk>:<public key>:<node_id>` — the shape of a `UL1` code with the
    /// device to dial where the session id was, and the `node_id` LAST for the
    /// same reason the id was: whatever is in it survives the split untouched.
    fn encode(&self) -> String {
        format!(
            "{LAN_PAYLOAD_TAG}:{}:{}:{}",
            b64(&self.psk),
            b64(self.epk.as_bytes()),
            self.node_id,
        )
    }

    fn parse(text: &str) -> Option<LanPayload> {
        let mut fields = text.trim().splitn(4, ':');
        if fields.next()? != LAN_PAYLOAD_TAG {
            return None;
        }
        let psk: [u8; PSK_LEN] = unb64(fields.next()?)?.try_into().ok()?;
        let epk: [u8; 32] = unb64(fields.next()?)?.try_into().ok()?;
        let node_id = fields.next()?;
        // Checked here rather than left to the dial: a mistyped code must be
        // refused as a code, not reported as a device that would not answer.
        if node_id.len() != 64 || !node_id.bytes().all(|b| b.is_ascii_hexdigit()) {
            return None;
        }
        Some(LanPayload {
            psk,
            epk: PublicKey::from(epk),
            node_id: node_id.to_string(),
        })
    }
}

/// The dialer's proof that it read the code off a screen: a MAC over the
/// exchange's two public halves and the device being dialled, keyed by the
/// optical secret.
///
/// Deliberately NOT derived from the channel key, which is the obvious way to
/// write this and the wrong one: computing that key spends the displaying side's
/// ephemeral secret, so anything on the network could then end a pairing window
/// for the device the human is actually holding. Keyed by the PSK alone, the check
/// costs one HKDF and turns a stranger away with the window intact. It is bound to
/// both public keys, so it cannot be replayed into another window either.
fn dialer_proof(
    psk: &[u8; PSK_LEN],
    offerer: &PublicKey,
    claimer: &PublicKey,
    node_id: &str,
) -> String {
    let mut info = Vec::with_capacity(LAN_PROOF_DOMAIN.len() + 64 + node_id.len());
    info.extend_from_slice(LAN_PROOF_DOMAIN);
    info.extend_from_slice(offerer.as_bytes());
    info.extend_from_slice(claimer.as_bytes());
    info.extend_from_slice(node_id.as_bytes());
    let mut out = [0u8; 32];
    hkdf::Hkdf::<Sha256>::new(None, psk)
        .expand(&info, &mut out)
        .expect("32 bytes is a valid HKDF-SHA256 output length");
    hex::encode(out)
}

/// Which account this device is in, as a mark only the two ends of this exchange
/// can read: another output of the channel KDF, over the account's public key.
///
/// The public key itself would do the job and would tell the local network which
/// account this device belongs to — a thing it has no business learning from a
/// pairing window left open on a screen.
fn account_mark(key: &[u8; 32], ak_pub: &str) -> String {
    let mut info = LAN_ACCOUNT_DOMAIN.to_vec();
    info.extend_from_slice(ak_pub.as_bytes());
    let mut out = [0u8; 32];
    hkdf::Hkdf::<Sha256>::from_prk(key)
        .expect("a 32-byte channel key is a valid HKDF-SHA256 PRK")
        .expand(&info, &mut out)
        .expect("32 bytes is a valid HKDF-SHA256 output length");
    hex::encode(out)
}

/// Compares two derived secrets without leaking where they stop matching. One
/// wrong answer ends the exchange, so the timing channel here is thin — but a MAC
/// comparison that returns early is not a habit worth having.
fn same_secret(ours: &str, theirs: &str) -> bool {
    ours.len() == theirs.len()
        && ours
            .bytes()
            .zip(theirs.bytes())
            .fold(0u8, |acc, (a, b)| acc | (a ^ b))
            == 0
}

/// What one side brings to a LAN pairing: whether it can vouch for a device, which
/// account it is in, and the description it stands behind.
struct Standing {
    /// Does this device hold the account's PRIVATE key — read back from the
    /// keyring, never remembered from a write, exactly as `account.status` answers
    /// it. This is what decides who sponsors.
    holds_key: bool,
    /// `Some` when the device is in an account, with or without its key — as a
    /// mark, see [`account_mark`].
    ///
    /// Both sides send it, so neither has to take the other's word for it that they
    /// belong to the same account. Between two honest Cores the DIALLED side would
    /// have refused a mismatch first, and this is what keeps the answer from
    /// depending on that.
    account: Option<String>,
    /// The device's own signed description, see [`lan_declaration`].
    record: Value,
}

/// What this device declares about itself on the local network: its own SIGNED
/// description (`directory::own_record`'s shape), not the three loose fields the
/// server path sends.
///
/// Two reasons, and both are about what happens next. The other side has to be
/// able to put this record in its directory once it has attested it, and a
/// description nobody signed enters as hearsay and travels no further
/// (`directory::shareable`). And the signature is checked against the `node_id`
/// the TRANSPORT authenticated, so a device cannot declare itself to be another
/// one.
///
/// A device in no account has no record to hold: it mints one, unattested — which
/// is precisely what it is asking to change.
fn lan_declaration(state: &AppState) -> Value {
    let held = {
        let s = state.session.lock().expect("lock session");
        s.own_device_id
            .as_deref()
            .and_then(|id| s.devices.as_ref().and_then(|devices| devices.get(id)))
            .filter(|record| crate::directory::verify_record(record))
            .cloned()
    };
    held.unwrap_or_else(|| {
        crate::directory::own_record(
            &state.identity.node_id(),
            crate::state::OwnDevice {
                identity: &state.identity,
                name: &state.device_name,
                attestation: "",
            },
            None,
        )
    })
}

/// What this device brings, with the channel key it will be compared over.
fn standing(state: &AppState, key: &[u8; 32]) -> Standing {
    Standing {
        // The keyring read, and it is the only answer worth anything here: on the
        // desktop a write is queued, so only a read tells the truth about what
        // this device can actually sign with.
        holds_key: holds_key(state),
        account: account_pub(state).map(|ak_pub| account_mark(key, &ak_pub)),
        record: lan_declaration(state),
    }
}

/// What the other side declared, as far as it can be checked without a human: a
/// description signed by the very device the transport authenticated. `None`
/// otherwise — a device does not get to describe another one, and a record nobody
/// signed could never enter a directory anyway.
fn standing_of(frame: &Value, node_id: &str) -> Option<Standing> {
    let record = frame.get("record")?.clone();
    if record.get("node_id").and_then(Value::as_str) != Some(node_id) {
        return None;
    }
    if !crate::directory::verify_record(&record) {
        return None;
    }
    Some(Standing {
        // Absent reads as false: a device that does not say it can vouch cannot.
        holds_key: frame
            .get("holds_key")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        account: frame
            .get("account")
            .and_then(Value::as_str)
            .map(str::to_string),
        record,
    })
}

/// The three fields the interface puts in front of the human, out of a declared
/// record — the same shape the server path's `device` has.
fn declared(record: &Value) -> Value {
    json!({
        "name": record.get("name").cloned().unwrap_or(Value::Null),
        "platform": record.get("platform").cloned().unwrap_or(Value::Null),
        "node_id": record.get("node_id").cloned().unwrap_or(Value::Null),
    })
}

/// Which end of this pairing each device is on. Both sides run it over the same
/// two standings and get mirror answers, `we_displayed` being the only thing that
/// differs between them.
fn lan_roles(ours: &Standing, theirs: &Standing, we_displayed: bool) -> Result<Role, &'static str> {
    if !ours.holds_key && !theirs.holds_key {
        // Neither can vouch. Two devices with no account do not make one by
        // meeting, and a device that has the account WITHOUT its key cannot hand
        // over what it does not hold — it is the one that needs a sponsor.
        return Err("no_account");
    }
    if let (Some(ours), Some(theirs)) = (&ours.account, &theirs.account)
        && ours != theirs
    {
        // Two accounts do not become one by pairing. Refused here rather than at
        // the install, so the seed does not cross at all.
        return Err("other_account");
    }
    Ok(match (ours.holds_key, theirs.holds_key) {
        (true, false) => Role::Sponsor,
        (false, true) => Role::Joiner,
        // Both hold it: see the section header. The device that displayed the code
        // answers — the human who scanned is the one asking.
        _ => {
            if we_displayed {
                Role::Sponsor
            } else {
                Role::Joiner
            }
        }
    })
}

/// The other device's refusal, in this API's vocabulary. `pairing.accept` answers
/// synchronously, so a refusal that arrives during it is an error and not an event.
fn refusal(reason: Option<&str>) -> RpcErr {
    match reason {
        // Nobody in this pair can vouch — the same wall `pairing.confirm` and
        // `devices.revoke` hit, said the same way.
        Some("no_account") => RpcErr::app("NO_ACCOUNT_KEY"),
        // One of the two is already attested under another account key.
        Some("other_account") => RpcErr::app("ACCOUNT_KEY_SET"),
        _ => RpcErr::app("PAIRING_STATE"),
    }
}

/// Is a LAN pairing window open, and still looking for its dialer? What the data
/// plane asks before reading a frame from a device it cannot place
/// (`dataplane::serve`): the one moment a stranger's stream is worth a look, and a
/// moment a human opened deliberately. It closes again the instant a dialer is
/// taken, so a window serves exactly one.
pub(crate) fn lan_window_open(state: &AppState) -> bool {
    state
        .pairing
        .lock()
        .expect("lock pairing")
        .as_ref()
        .is_some_and(|p| matches!(p.wire, Wire::Lan) && p.channel.key.is_none())
}

/// `pairing.offer` with no server: a code another device can dial.
async fn offer_lan(state: &Arc<AppState>) -> Result<Value, RpcErr> {
    let epoch = EPOCH.fetch_add(1, Ordering::Relaxed) + 1;
    let channel = Channel::displaying();
    // Minted here: with no server there is no session id to be handed one, and
    // none is needed — the two sides never name the pairing to each other (the
    // channel is bound to the `node_id` instead). It is a handle for this device's
    // own interface, and that is all.
    let pairing_id = format!("p_{}", crate::state::random_hex(8));
    let code = LanPayload {
        psk: channel.psk,
        epk: channel.ours,
        node_id: state.identity.node_id(),
    }
    .encode();
    // What this device is ABLE to be. Which end it ends up on is settled when the
    // other one dials (`lan_roles`) — with no server there is nobody to settle it
    // in advance, and a device that turns out to hold the key too changes the
    // answer.
    let role = if holds_key(state) {
        Role::Sponsor
    } else {
        Role::Joiner
    };
    // The one moment a device has to be reachable by a device it does not know:
    // the daemon's transport binds nothing until it has someone to talk to.
    if tokio::time::timeout(LAN_LISTEN_BUDGET, state.transport.listen())
        .await
        .is_err()
    {
        tracing::warn!("the data plane is slow to come up: this code may not be dialable yet");
    }
    install(
        state,
        Pairing {
            epoch,
            pairing_id: pairing_id.clone(),
            role,
            channel,
            wire: Wire::Lan,
            expiry: None,
            // Created when a dialer arrives: until then there is nothing to
            // confirm, and `pairing.confirm` says so.
            confirm: None,
        },
        LAN_TTL.as_secs(),
    );
    Ok(json!({
        "pairing_id": pairing_id,
        "code": code,
        "role": role.as_str(),
        "expires_in": LAN_TTL.as_secs(),
    }))
}

/// `pairing.accept` of a `UL2` code: dial the device that displayed it, prove we
/// read its screen, and settle who is joining. Returns as soon as that much is
/// done — what follows waits on a human, and waits in a task of its own.
async fn accept_lan(state: &Arc<AppState>, payload: LanPayload) -> Result<Value, RpcErr> {
    // A device that answers to a server pairs THROUGH it. Handing the account key
    // over here would put the other device in an account half of which the server
    // has never heard, and nothing yet makes those two halves one — that is the
    // continuum building block, and until it exists this is a refusal and not a
    // gap. A code of its own rather than `PAIRING_STATE`: nothing is out of step
    // here, and telling the user their code was answered by somebody else would be
    // a sentence about the wrong problem.
    if !crate::state::serverless(state) {
        return Err(RpcErr::app("PAIRING_VIA_SERVER"));
    }
    // Our own code, read on the machine that is displaying it.
    if payload.node_id == state.identity.node_id() {
        return Err(RpcErr::invalid_params("code"));
    }
    // A code shown by a device the account has struck off. A tombstone is
    // permanent — the `node_id` it names never returns, and a device that reset
    // itself would show a fresh one — so there is no pairing here to attempt:
    // this device would refuse everything the other declared at the absorb
    // anyway, after the humans had confirmed a number for nothing. Said as what
    // it is (the account's own decision) rather than left to end as a pairing
    // failure. The mirror case needs no check of ours: a struck-off DIALER is
    // answered its tombstone by the displayer's data plane before the offer is
    // even read (`dataplane::serve`).
    if crate::dataplane::struck_off(state, &payload.node_id).is_some() {
        return Err(RpcErr::app("DEVICE_REVOKED"));
    }
    let epoch = EPOCH.fetch_add(1, Ordering::Relaxed) + 1;
    let mut channel = Channel::scanning(payload.psk);
    // Derived before anything is dialled: the code carries everything needed, and
    // a code whose public key is unusable must cost the other device nothing.
    let Some(key) = channel.establish_lan(&payload.epk, &payload.node_id) else {
        return Err(RpcErr::invalid_params("code"));
    };
    let number = verification(&key);
    // Read before the stream exists and before any lock is taken: the keyring can
    // block, and nothing here may wait on it while holding state.
    let ours = standing(state, &key);

    let peer = PeerAddr {
        node_id: payload.node_id.clone(),
        relay_url: None,
    };
    let offline = |what: &'static str| {
        move |e: std::io::Error| {
            tracing::debug!(error = %e, "{what}");
            RpcErr::app("DEVICE_OFFLINE")
        }
    };
    // No relay, deliberately: a code carries the secret a screen and a camera
    // share, and the window it opens should be as narrow as the room. Which makes
    // the local network the route, and mDNS the way this device is found.
    let mut stream = within(
        LAN_CONNECT_TIMEOUT,
        state.transport.open(&peer),
        "dialling the device",
    )
    .await
    .map_err(offline("the device whose code was read did not answer"))?;

    let mut hello = json!({
        "type": LAN_OFFER,
        "epk": b64(channel.ours.as_bytes()),
        "proof": dialer_proof(&payload.psk, &payload.epk, &channel.ours, &payload.node_id),
        "record": ours.record.clone(),
        "holds_key": ours.holds_key,
    });
    if let Some(mark) = &ours.account {
        hello["account"] = json!(mark);
    }
    within(
        LAN_FRAME_TIMEOUT,
        dataplane::write_control(&mut stream, &hello),
        "our offer",
    )
    .await
    .map_err(offline("our pairing offer did not reach the device"))?;

    let answer = within(
        LAN_FRAME_TIMEOUT,
        dataplane::read_control(&mut stream),
        "their answer",
    )
    .await
    .map_err(offline("the device did not answer our pairing offer"))?;
    match answer.get("type").and_then(Value::as_str) {
        Some(LAN_HELLO) => {}
        // It has a window open and we are not what it is waiting for.
        Some(LAN_REFUSED) => {
            return Err(refusal(answer.get("reason").and_then(Value::as_str)));
        }
        _ => return Err(RpcErr::app("PAIRING_STATE")),
    }
    let Some(theirs) = standing_of(&answer, &payload.node_id) else {
        lan_refuse(&mut stream, "record").await;
        return Err(RpcErr::app("PAIRING_STATE"));
    };
    let role = match lan_roles(&ours, &theirs, false) {
        Ok(role) => role,
        Err(reason) => {
            // Both ends work it out for themselves, so the other one is refusing
            // too — telling it is what keeps its human from watching a screen
            // that has already given up.
            lan_refuse(&mut stream, reason).await;
            return Err(refusal(Some(reason)));
        }
    };

    let pairing_id = format!("p_{}", crate::state::random_hex(8));
    let (confirm_tx, confirm_rx) = oneshot::channel();
    let sponsoring = role == Role::Sponsor;
    install(
        state,
        Pairing {
            epoch,
            pairing_id: pairing_id.clone(),
            role,
            channel,
            wire: Wire::Lan,
            expiry: None,
            confirm: sponsoring.then_some(confirm_tx),
        },
        LAN_TTL.as_secs(),
    );
    let device = declared(&theirs.record);
    tokio::spawn(lan_tail(
        state.clone(),
        stream,
        LanTail {
            pairing_id: pairing_id.clone(),
            role,
            key,
            theirs: theirs.record,
            confirm: sponsoring.then_some(confirm_rx),
        },
    ));
    let mut result = json!({
        "pairing_id": pairing_id,
        "role": role.as_str(),
        "verification": number,
    });
    if sponsoring {
        // What the human has to be shown before confirming — and only then: a
        // joiner has nothing to decide.
        result["device"] = device;
    }
    Ok(result)
}

/// What the dialled side settled, under the pairing lock and in one go.
enum Armed {
    /// The window took this dialer.
    Taken {
        pairing_id: String,
        role: Role,
        key: [u8; 32],
        /// Our own half of the conversation, ready to send.
        hello: Value,
        confirm: Option<oneshot::Receiver<()>>,
    },
    /// No window this dialer can be the answer to. `ours` names our pairing when
    /// it ends with this refusal — a stranger that cannot prove it saw the screen
    /// must NOT be able to end a pairing, while a channel we cannot key does.
    Refused {
        reason: &'static str,
        ours: Option<String>,
    },
}

/// The dialled half, reached from the data plane's frame dispatch: a device asking
/// to be introduced. The one frame served to a device outside the directory
/// (`dataplane::serve_incoming`), and only while a window is open here.
pub(crate) async fn recv_lan_pair(
    state: Arc<AppState>,
    peer: String,
    first: Value,
    mut stream: Box<dyn IoStream>,
) {
    // Unreadable: not one of our codes at all, and not a conversation. Nothing is
    // answered — an answer would only tell whoever is probing that something here
    // is listening.
    let Some((their_epk, proof)) = channel_material(&first) else {
        tracing::warn!(peer = %peer, "unreadable pairing offer on the local network: ignored");
        return;
    };
    let Some(theirs) = standing_of(&first, &peer) else {
        tracing::warn!(peer = %peer, "a pairing offer whose description is not that device's own: refused");
        lan_refuse(&mut stream, "record").await;
        return;
    };
    // Both read before the pairing lock (which is a leaf: the keyring can block,
    // and `lan_declaration` takes the session).
    let ours_holds_key = holds_key(&state);
    let ours_ak_pub = account_pub(&state);
    let ours_record = lan_declaration(&state);
    let own_node_id = state.identity.node_id();

    let armed = {
        let mut slot = state.pairing.lock().expect("lock pairing");
        arm(
            slot.as_mut(),
            &their_epk,
            &proof,
            &own_node_id,
            &theirs,
            Standing {
                holds_key: ours_holds_key,
                // Filled in below: the mark needs the channel key, which does not
                // exist until the proof has been checked.
                account: None,
                record: ours_record,
            },
            ours_ak_pub,
        )
    };

    let (pairing_id, role, key, hello, confirm) = match armed {
        Armed::Taken {
            pairing_id,
            role,
            key,
            hello,
            confirm,
        } => (pairing_id, role, key, hello, confirm),
        Armed::Refused { reason, ours } => {
            tracing::debug!(peer = %peer, reason, "a pairing offer on the local network was refused");
            lan_refuse(&mut stream, reason).await;
            if let Some(pairing_id) = ours {
                settle(
                    &state,
                    &pairing_id,
                    "pairing.failed",
                    json!({ "reason": reason }),
                );
            }
            return;
        }
    };

    if let Err(e) = within(
        LAN_FRAME_TIMEOUT,
        dataplane::write_control(&mut stream, &hello),
        "our answer",
    )
    .await
    {
        tracing::debug!(peer = %peer, error = %e, "answering a pairing offer failed");
        settle(
            &state,
            &pairing_id,
            "pairing.failed",
            json!({ "reason": "abandoned" }),
        );
        return;
    }
    // Both ends are told the number here and only here: this is the first moment
    // either of them has a channel to derive it from, and the moment a human is
    // asked to compare.
    let mut announced = json!({ "pairing_id": pairing_id, "verification": verification(&key) });
    if role == Role::Sponsor {
        announced["device"] = declared(&theirs.record);
    }
    notify(&state, "pairing.claimed", announced);
    lan_tail(
        state.clone(),
        stream,
        LanTail {
            pairing_id,
            role,
            key,
            theirs: theirs.record,
            confirm,
        },
    )
    .await;
}

/// Settles the dialled side's half of the pairing: the window has to be open and
/// unclaimed, the dialer has to prove it saw the screen, and only then is the
/// ephemeral secret spent. Pure and under the lock — no await, no second lock.
fn arm(
    slot: Option<&mut Pairing>,
    their_epk: &PublicKey,
    proof: &str,
    own_node_id: &str,
    theirs: &Standing,
    mut ours: Standing,
    ours_ak_pub: Option<String>,
) -> Armed {
    // No window, or a window that belongs to a server pairing, or one that has
    // already taken its dialer. None of them is OUR pairing failing: the dialer is
    // simply not what is being waited for here.
    //
    // Both clauses are belt on braces, and worth having: the data plane already
    // refuses a device it cannot place unless a LAN window is open AND unclaimed
    // (`lan_window_open`), and a device it CAN place — a sibling — would be turned
    // away by the proof. What is left for them to catch is a device that is both in
    // the directory and read the code off the screen: without this, its second dial
    // would find the ephemeral secret spent and end the pairing under way.
    let busy = || Armed::Refused {
        reason: "busy",
        ours: None,
    };
    let Some(p) = slot else { return busy() };
    if !matches!(p.wire, Wire::Lan) || p.channel.key.is_some() {
        return busy();
    }
    // BEFORE the ephemeral secret is spent — see `dialer_proof`.
    let expected = dialer_proof(&p.channel.psk, &p.channel.ours, their_epk, own_node_id);
    if !same_secret(&expected, proof) {
        return Armed::Refused {
            reason: "proof",
            ours: None,
        };
    }
    let Some(key) = p.channel.establish_lan(their_epk, own_node_id) else {
        // A public key that is unusable, or an exchange somebody could have
        // dictated. The code is spent either way, so this one IS our pairing
        // failing.
        return Armed::Refused {
            reason: "channel",
            ours: Some(p.pairing_id.clone()),
        };
    };
    ours.account = ours_ak_pub.map(|ak_pub| account_mark(&key, &ak_pub));
    let role = match lan_roles(&ours, theirs, true) {
        Ok(role) => role,
        Err(reason) => {
            return Armed::Refused {
                reason,
                ours: Some(p.pairing_id.clone()),
            };
        }
    };
    // The role this device is really on, which the offer could only guess at.
    p.role = role;
    let mut hello = json!({
        "type": LAN_HELLO,
        "record": ours.record,
        "holds_key": ours.holds_key,
    });
    if let Some(mark) = &ours.account {
        hello["account"] = json!(mark);
    }
    let confirm = (role == Role::Sponsor).then(|| {
        let (tx, rx) = oneshot::channel();
        p.confirm = Some(tx);
        rx
    });
    Armed::Taken {
        pairing_id: p.pairing_id.clone(),
        role,
        key,
        hello,
        confirm,
    }
}

/// The dialer's channel material, out of its offer.
fn channel_material(frame: &Value) -> Option<(PublicKey, String)> {
    let epk: [u8; 32] = unb64(frame.get("epk")?.as_str()?)?.try_into().ok()?;
    let proof = frame.get("proof")?.as_str()?.to_string();
    Some((PublicKey::from(epk), proof))
}

/// Everything the rest of a LAN pairing needs. It owns the stream, because between
/// the roles being settled and the account crossing there is a human.
struct LanTail {
    pairing_id: String,
    role: Role,
    key: [u8; 32],
    /// What the other device declared about itself: the record a sponsor attests
    /// and takes in once it has vouched for it.
    theirs: Value,
    /// The human's yes, for a sponsor. `None` for a joiner — it has nothing to
    /// confirm.
    confirm: Option<oneshot::Receiver<()>>,
}

async fn lan_tail(state: Arc<AppState>, mut stream: Box<dyn IoStream>, mut tail: LanTail) {
    let confirm = tail.confirm.take();
    match tail.role {
        Role::Sponsor => lan_sponsor(&state, &mut stream, &tail, confirm).await,
        Role::Joiner => lan_joiner(&state, &mut stream, &tail).await,
    }
    let _ = stream.shutdown().await;
    let _ = tokio::time::timeout(LAN_LINGER, dataplane::drain(&mut stream)).await;
}

/// The giving side: wait for the human, hand over the account, take the device in.
async fn lan_sponsor(
    state: &Arc<AppState>,
    stream: &mut Box<dyn IoStream>,
    tail: &LanTail,
    confirm: Option<oneshot::Receiver<()>>,
) {
    let Some(confirm) = confirm else {
        tracing::error!("a sponsor with no confirmation to wait for: pairing abandoned");
        return;
    };
    // No budget of our own: the pairing's expiry is what bounds this wait, and it
    // bounds it by DROPPING the other end of this channel — which is also what a
    // cancellation and a replacement do. So every way this can end without a yes
    // arrives here as the same error, and the other device is told to stop waiting
    // instead of being left to time out.
    if confirm.await.is_err() {
        lan_refuse(stream, "declined").await;
        return;
    }
    let failed = |reason: &str| {
        settle(
            state,
            &tail.pairing_id,
            "pairing.failed",
            json!({ "reason": reason }),
        )
    };
    // Read at this instant and let go of again: a pairing waiting on a human holds
    // no key material beyond the channel's.
    let Some(bundle) = seal_account(state, &tail.key) else {
        lan_refuse(stream, "no_account").await;
        failed("no_account");
        return;
    };
    // The account sealed under the channel the screen keyed, and in the same frame
    // everything this device knows about the account: a device that learned only
    // the key would be in the account and know nobody in it — including us, which
    // would leave it with nobody to ask.
    let mut grant = roster_frame(state, LAN_GRANT);
    grant["bundle"] = json!(bundle);
    if let Err(e) = within(
        LAN_FRAME_TIMEOUT,
        dataplane::write_control(stream, &grant),
        "the grant",
    )
    .await
    {
        tracing::warn!(error = %e, "handing the account over failed");
        failed("abandoned");
        return;
    }
    // Only now, the write having gone through: the device really does have the key,
    // so it really is one of ours. Its own description, attested under our account
    // key, enters the directory — through `absorb`, like every record that comes
    // from another device, because that is where the rules live.
    adopt_peer(state, &tail.theirs);
    settle(state, &tail.pairing_id, "pairing.completed", json!({}));
    // What it knows and we do not: a device coming back to an account it was
    // already in brings its own directory with it. Best effort — the pairing is
    // done either way, and the sync task would catch up on its own now that the two
    // of us know each other.
    match read_roster(stream, LAN_ROSTER).await {
        Ok(roster) => crate::dirsync::absorb(state, &roster),
        Err(e) => tracing::debug!(error = %e, "the joining device sent no roster back"),
    }
}

/// The receiving side: wait for the grant, install the account, tell the sponsor
/// what we know.
async fn lan_joiner(state: &Arc<AppState>, stream: &mut Box<dyn IoStream>, tail: &LanTail) {
    let failed = |reason: &str| {
        settle(
            state,
            &tail.pairing_id,
            "pairing.failed",
            json!({ "reason": reason }),
        )
    };
    // A human is deciding on the other machine. What bounds this is the pairing's
    // own deadline, counted here as it is on the other side.
    let frame = match within(LAN_TTL, dataplane::read_control(stream), "the grant").await {
        Ok(frame) => frame,
        Err(e) => {
            tracing::debug!(error = %e, "no answer from the sponsoring device");
            failed("abandoned");
            return;
        }
    };
    match frame.get("type").and_then(Value::as_str) {
        Some(LAN_GRANT) => {}
        Some(LAN_REFUSED) => {
            failed(
                frame
                    .get("reason")
                    .and_then(Value::as_str)
                    .unwrap_or("declined"),
            );
            return;
        }
        _ => {
            failed("state");
            return;
        }
    }
    let opened = frame
        .get("bundle")
        .and_then(Value::as_str)
        .and_then(|sealed| open_bundle(&tail.key, sealed));
    // The account's identity, when the sponsor had one to name, is a label a
    // session carries — and there is no session here. Ignored rather than stored:
    // this device answers to no server, and a name for an account it cannot reach
    // would be the one thing on screen that nothing ever corrects.
    let Some((ak, _account)) = opened else {
        failed("bundle");
        return;
    };
    // The step that can refuse: a key other than the one this device is already
    // attested under is another account's, whatever the human confirmed over
    // there. (The same key is not a refusal — it is the way back in for a device
    // that has the account without its key.)
    let root = match crate::account_key::install(state, &ak) {
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
    // In the account: this device can now sign its own description AND attest it.
    join_locally(state, &root);
    // Whom the account knows, as the sponsor knows it — every record checked
    // against the key we have just installed, never taken on the sponsor's word.
    if let Some(roster) = Roster::parse(&frame) {
        crate::dirsync::absorb(state, &roster);
    }
    // And whom we know. Sent after the install, because before it this device had
    // nothing it could prove to anybody.
    if let Err(e) = within(
        LAN_FRAME_TIMEOUT,
        dataplane::write_control(stream, &roster_frame(state, LAN_ROSTER)),
        "our roster",
    )
    .await
    {
        tracing::debug!(error = %e, "our roster did not reach the sponsoring device");
    }
    settle(state, &tail.pairing_id, "pairing.completed", json!({}));
}

/// Takes the other device's own description into the directory, attested under our
/// account key — the sponsor's half of the introduction, and what makes the two of
/// them able to talk afterwards.
///
/// Through `absorb`, like every record that comes from another device: the
/// description is signed by the device it describes (checked on arrival against
/// the `node_id` the transport authenticated) and the attestation is the one minted
/// here. So nothing is being believed — this is the account admitting a device,
/// which is exactly what a human just confirmed.
fn adopt_peer(state: &Arc<AppState>, record: &Value) {
    let Some(ak) = account_key(state) else { return };
    let Some(node_id) = record.get("node_id").and_then(Value::as_str) else {
        return;
    };
    let mut record = record.clone();
    record["attestation"] = json!(crate::account_key::attest(&ak, node_id));
    crate::dirsync::absorb(
        state,
        &Roster {
            records: vec![record],
            ..Default::default()
        },
    );
}

/// Enters the account locally, the key installed: this device's own record — which
/// it can now both sign and attest — on disk, and announced if it is new.
///
/// `save_unrefreshed` rather than `save`: nothing here was checked against a
/// server, and the store's stamp is a bound against one (`directory`). With no
/// server the stamp means nothing either way; what matters is that this path does
/// not become a way to move it.
fn join_locally(state: &Arc<AppState>, root: &crate::account_key::AccountRoot) {
    // Transport snapshot before the session lock (lock ordering).
    let lan: std::collections::BTreeSet<String> = state.transport.lan_peers().into_iter().collect();
    let mut s = state.session.lock().expect("lock session");
    let own_record = |s: &crate::state::SessionState| {
        s.own_device_id
            .as_deref()
            .and_then(|id| s.devices.as_ref().and_then(|devices| devices.get(id)))
            .cloned()
    };
    let had = own_record(&s).is_some();
    s.adopt_own(crate::state::OwnDevice {
        identity: &state.identity,
        name: &state.device_name,
        attestation: &root.attestation,
    });
    if let Some(devices) = s.devices.as_ref() {
        crate::directory::save_unrefreshed(&state.config_dir, devices);
    }
    // A device that already held a record of its own has just got its account key
    // back, which changes nothing anyone is watching the directory for.
    if had {
        return;
    }
    let own = s.own_device_id.clone();
    let Some(record) = own_record(&s) else { return };
    let device = crate::state::enrich_device(&record, own.as_deref(), s.server_connected, &lan);
    // Under the session lock (order: session then registry), like every directory
    // broadcast: a Core that was answering "I know nobody at all" now answers with
    // itself, and that is worth one event.
    state.registry.lock().expect("lock registry").notify_topic(
        "devices",
        "device.added",
        &json!({ "device": device }),
    );
}

/// This device's roster as a frame of `kind`. Empty when it has no trust root —
/// which for a sponsor cannot happen, and for a joiner means it has not installed
/// the key yet.
fn roster_frame(state: &AppState, kind: &str) -> Value {
    let roster = crate::dirsync::our_roster(state).unwrap_or_default();
    let mut frame = roster.payload();
    frame["type"] = json!(kind);
    frame
}

/// Reads a roster frame, refusing anything that is not one of `kind`.
async fn read_roster(stream: &mut Box<dyn IoStream>, kind: &str) -> std::io::Result<Roster> {
    let frame = within(LAN_FRAME_TIMEOUT, dataplane::read_control(stream), kind).await?;
    if frame.get("type").and_then(Value::as_str) != Some(kind) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("expected a {kind}"),
        ));
    }
    Roster::parse(&frame)
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidData, "unreadable roster"))
}

/// Tells the other device why, best effort. A stream that closes without a word is
/// a human watching a screen, waiting for something to happen.
async fn lan_refuse(stream: &mut Box<dyn IoStream>, reason: &str) {
    let frame = json!({ "type": LAN_REFUSED, "reason": reason });
    if let Err(e) = within(
        LAN_FRAME_TIMEOUT,
        dataplane::write_control(stream, &frame),
        "the refusal",
    )
    .await
    {
        tracing::debug!(error = %e, "the refusal did not reach the other device");
    }
    let _ = stream.shutdown().await;
}

/// Bounds a frame exchange: past `dur` the other device is not answering, and a
/// task that holds a stream must never wait on one forever.
async fn within<T>(
    dur: Duration,
    fut: impl Future<Output = std::io::Result<T>>,
    what: &str,
) -> std::io::Result<T> {
    match tokio::time::timeout(dur, fut).await {
        Ok(result) => result,
        Err(_) => Err(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            format!("timed out: {what}"),
        )),
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

    // The confirmation number is what the human is asked to compare, so the two
    // ends have to agree on it — and an intruder who claimed the session ahead of
    // the legitimate device must not.
    #[test]
    fn both_ends_show_the_same_confirmation_number() {
        let (mut offerer, mut claimer) = two_ends();
        let theirs = claimer.ours;
        let ours = offerer.ours;
        let here = offerer.establish(&theirs, "p_1").expect("key");
        let there = claimer.establish(&ours, "p_1").expect("key");
        assert_eq!(verification(&here), verification(&there));
        // Six digits in two groups, and nothing else: a number to read aloud.
        assert!(
            regex_lite_six_digits(&verification(&here)),
            "unexpected shape: {}",
            verification(&here)
        );
    }

    // A fixed point, not a property: the number is compared BETWEEN two devices,
    // so its derivation is a compatibility contract like the payload's version
    // tag. A Core that changed it — another label, another slice of the digest —
    // would show a different number from its peer, and every confirmation would
    // read as an intruder.
    #[test]
    fn the_number_is_derived_the_way_it_always_was() {
        assert_eq!(verification(&[7u8; 32]), "150 048");
        assert_eq!(verification(&[0u8; 32]), "695 908");
    }

    #[test]
    fn another_channel_shows_another_number() {
        let (mut offerer, claimer) = two_ends();
        let legitimate = offerer.establish(&claimer.ours, "p_1").expect("key");
        // Someone who read the code off the screen and claimed the session first:
        // same optical secret, same offerer, but a keypair of their own.
        let intruder = Channel::scanning(offerer.psk);
        let mut theirs = Channel::displaying();
        theirs.psk = offerer.psk;
        let raced = theirs.establish(&intruder.ours, "p_1").expect("key");
        assert_ne!(
            verification(&legitimate),
            verification(&raced),
            "the number must not survive a different channel — it is the whole \
             point of showing it"
        );
    }

    /// `NNN NNN`, without pulling a regex crate in for one assertion.
    fn regex_lite_six_digits(text: &str) -> bool {
        let bytes = text.as_bytes();
        bytes.len() == 7
            && bytes[3] == b' '
            && bytes
                .iter()
                .enumerate()
                .all(|(i, b)| i == 3 || b.is_ascii_digit())
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
        // Shorter than the nonce it is supposed to start with. A relay we do not
        // trust chooses this string, so the length has to be checked before it is
        // split — this is the vector that says so.
        for tiny in ["", "AAAA", &b64(&[0u8; 23])] {
            assert!(open(&receiver, tiny).is_none(), "too short: {tiny:?}");
        }
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

    // -- Pairing on the local network ---------------------------------------

    /// A plausible `node_id`: 64 hex characters, which is what the code carries
    /// and what the transport authenticates.
    fn a_node_id() -> String {
        crate::identity::DeviceIdentity::from_test_seed(7).node_id()
    }

    fn a_lan_code() -> (Channel, LanPayload) {
        let channel = Channel::displaying();
        let payload = LanPayload {
            psk: channel.psk,
            epk: channel.ours,
            node_id: a_node_id(),
        };
        (channel, payload)
    }

    #[test]
    fn a_lan_code_round_trips() {
        let (_, payload) = a_lan_code();
        let text = payload.encode();
        let parsed = LanPayload::parse(&text).expect("our own code parses");
        assert_eq!(parsed.psk, payload.psk);
        assert_eq!(parsed.epk.as_bytes(), payload.epk.as_bytes());
        assert_eq!(parsed.node_id, payload.node_id);
        // A pasted code brings its whitespace along.
        assert!(LanPayload::parse(&format!("  {text}\n")).is_some());
    }

    /// The two schemes are told apart by their tag and nothing else, so each has
    /// to refuse the other's code WHOLE rather than half-read it.
    #[test]
    fn the_two_code_schemes_do_not_read_each_other() {
        let (channel, payload) = a_lan_code();
        let server_code = Payload {
            pairing_id: "p_0011223344556677".to_string(),
            psk: channel.psk,
            epk: channel.ours,
        }
        .encode();

        assert!(LanPayload::parse(&server_code).is_none(), "{server_code}");
        assert!(Payload::parse(&payload.encode()).is_none());
    }

    #[test]
    fn what_is_not_a_lan_code_is_refused() {
        let (_, payload) = a_lan_code();
        let sound = payload.encode();
        let fields: Vec<&str> = sound.split(':').collect();
        for wrong in [
            "".to_string(),
            "hello".to_string(),
            sound.replacen(LAN_PAYLOAD_TAG, "UL3", 1),
            sound.replacen(LAN_PAYLOAD_TAG, "ul2", 1),
            // A field short, and an empty device to dial.
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
            // A device to dial that cannot be one: too short, too long, not hex.
            format!(
                "{}:{}:{}:{}",
                fields[0],
                fields[1],
                fields[2],
                "ab".repeat(31)
            ),
            format!(
                "{}:{}:{}:{}",
                fields[0],
                fields[1],
                fields[2],
                "ab".repeat(33)
            ),
            format!(
                "{}:{}:{}:{}",
                fields[0],
                fields[1],
                fields[2],
                "z".repeat(64)
            ),
        ] {
            assert!(
                LanPayload::parse(&wrong).is_none(),
                "accepted as a LAN pairing code: {wrong:?}"
            );
        }
        assert!(LanPayload::parse(&sound).is_some(), "the sound one parses");
    }

    /// Both ends of a LAN pairing derive the same key, and it is NOT the key the
    /// same material would derive through a server: the two schemes carry their own
    /// domain, so a channel of one can never be a channel of the other.
    #[test]
    fn a_lan_channel_is_shared_and_its_own() {
        let offerer = Channel::displaying();
        let mut dialer = Channel::scanning(offerer.psk);
        let mut displayer = Channel { ..offerer };
        let theirs = dialer.ours;
        let ours = displayer.ours;
        let node_id = a_node_id();

        let a = displayer
            .establish_lan(&theirs, &node_id)
            .expect("the displaying side's key");
        let b = dialer
            .establish_lan(&ours, &node_id)
            .expect("the dialling side's key");
        assert_eq!(a, b, "the two ends must agree, or nothing opens");

        // The same exchange under the server scheme, with the node_id where the
        // session id goes: a different key.
        let mut again = Channel::scanning(offerer.psk);
        let through_a_server = again.establish(&ours, &node_id).expect("key");
        assert_ne!(b, through_a_server);
        // And bound to the device it names.
        let mut elsewhere = Channel::scanning(offerer.psk);
        assert_ne!(
            b,
            elsewhere
                .establish_lan(&ours, &"ab".repeat(32))
                .expect("key")
        );
    }

    /// The proof is what a device that read the code can produce and nothing else
    /// can. Keyed by the optical secret ALONE, deliberately: the displaying side
    /// checks it before spending its ephemeral secret (see `dialer_proof`).
    #[test]
    fn only_a_device_that_saw_the_screen_can_prove_it() {
        let offerer = Channel::displaying();
        let dialer = Channel::scanning(offerer.psk);
        let node_id = a_node_id();
        let proof = dialer_proof(&offerer.psk, &offerer.ours, &dialer.ours, &node_id);
        // Both ends compute it from what they each hold.
        assert_eq!(
            proof,
            dialer_proof(&offerer.psk, &offerer.ours, &dialer.ours, &node_id)
        );
        // Someone who watched the whole conversation holds both public keys and the
        // node_id. Without the secret that travelled by the screen, that is not a
        // proof.
        assert_ne!(
            proof,
            dialer_proof(&[0u8; PSK_LEN], &offerer.ours, &dialer.ours, &node_id)
        );
        // And it is bound to this exchange: another window (another epk), or
        // another device, is another proof — so it cannot be replayed.
        let other = Channel::scanning(offerer.psk);
        assert_ne!(
            proof,
            dialer_proof(&offerer.psk, &offerer.ours, &other.ours, &node_id)
        );
        assert_ne!(
            proof,
            dialer_proof(&offerer.psk, &other.ours, &dialer.ours, &node_id)
        );
        assert_ne!(
            proof,
            dialer_proof(&offerer.psk, &offerer.ours, &dialer.ours, &"ab".repeat(32))
        );
    }

    /// Two devices find out whether they are in the same account without telling
    /// the local network which account that is.
    #[test]
    fn an_account_mark_compares_without_naming() {
        let key = [3u8; 32];
        let ak_pub = "ab".repeat(32);
        let other = "cd".repeat(32);

        assert_eq!(account_mark(&key, &ak_pub), account_mark(&key, &ak_pub));
        assert_ne!(account_mark(&key, &ak_pub), account_mark(&key, &other));
        // Another channel, another mark: a device that did not take part cannot
        // recognize an account by the mark it saw somewhere else.
        assert_ne!(
            account_mark(&key, &ak_pub),
            account_mark(&[4u8; 32], &ak_pub)
        );
        assert!(
            !account_mark(&key, &ak_pub).contains(&ak_pub),
            "the mark must not carry the key it is about"
        );
    }

    #[test]
    fn a_derived_secret_is_compared_whole() {
        assert!(same_secret("abcd", "abcd"));
        assert!(!same_secret("abcd", "abce"));
        assert!(!same_secret("abcd", "zbcd"));
        assert!(!same_secret("abcd", "abcde"), "a prefix is not a match");
        assert!(!same_secret("", "a"));
        assert!(same_secret("", ""));
    }

    fn standing(holds_key: bool, account: Option<&str>) -> Standing {
        Standing {
            holds_key,
            account: account.map(str::to_string),
            record: json!({}),
        }
    }

    /// Who ends up sponsoring, in every case there is. Both sides run this over the
    /// same two standings, so the answers have to be mirror images — a table that
    /// disagreed with itself would leave two joiners waiting for each other.
    #[test]
    fn the_two_ends_settle_on_mirror_roles() {
        let ours = "same-account";
        let theirs = "another-account";
        let cases = [
            // (we hold, our account, they hold, their account, what we are)
            (true, Some(ours), false, None, Ok(Role::Sponsor)),
            (false, None, true, Some(ours), Ok(Role::Joiner)),
            // A device that has the account WITHOUT its key is the one that needs
            // a sponsor, not one.
            (true, Some(ours), false, Some(ours), Ok(Role::Sponsor)),
            (false, Some(ours), true, Some(ours), Ok(Role::Joiner)),
            // Nobody can vouch.
            (false, None, false, None, Err("no_account")),
            (false, Some(ours), false, Some(ours), Err("no_account")),
            // Two accounts do not become one.
            (true, Some(ours), true, Some(theirs), Err("other_account")),
            (true, Some(ours), false, Some(theirs), Err("other_account")),
            (false, Some(theirs), true, Some(ours), Err("other_account")),
        ];
        for (we_hold, our_account, they_hold, their_account, expected) in cases {
            let us = standing(we_hold, our_account);
            let them = standing(they_hold, their_account);
            let described =
                format!("{we_hold}/{our_account:?} against {they_hold}/{their_account:?}");
            for we_displayed in [true, false] {
                assert_eq!(
                    lan_roles(&us, &them, we_displayed),
                    expected,
                    "{described} (displayed: {we_displayed})"
                );
            }
            // The mirror: what the OTHER side works out has to be the other end of
            // the same pairing.
            let mirrored = lan_roles(&them, &us, false);
            match expected {
                Ok(Role::Sponsor) => assert_eq!(mirrored, Ok(Role::Joiner), "{described}"),
                Ok(Role::Joiner) => assert_eq!(mirrored, Ok(Role::Sponsor), "{described}"),
                Err(reason) => assert_eq!(mirrored, Err(reason), "{described}"),
            }
        }
    }

    /// Both holding the key is two devices of ONE account meeting: somebody has to
    /// lead, and it is the device that displayed the code.
    #[test]
    fn when_both_hold_the_key_the_one_that_showed_the_code_answers() {
        let both = || standing(true, Some("same-account"));
        assert_eq!(lan_roles(&both(), &both(), true), Ok(Role::Sponsor));
        assert_eq!(lan_roles(&both(), &both(), false), Ok(Role::Joiner));
    }

    /// A device does not get to describe another one: the declaration is checked
    /// against the `node_id` the transport authenticated, and against the signature
    /// of the key that `node_id` IS.
    #[test]
    fn a_declaration_stands_only_for_the_device_that_sent_it() {
        let identity = crate::identity::DeviceIdentity::from_test_seed(7);
        let node_id = identity.node_id();
        let record = crate::directory::signed_record(&identity, "Office-PC", 5, "attestation");
        let frame = |record: Value| json!({ "record": record, "holds_key": true });

        let standing = standing_of(&frame(record.clone()), &node_id).expect("its own record");
        assert!(standing.holds_key);
        assert_eq!(standing.account, None, "absent until a channel exists");
        assert_eq!(declared(&standing.record)["name"], json!("Office-PC"));
        assert_eq!(declared(&standing.record)["node_id"], json!(node_id));

        // Another device's record, whoever relayed it.
        let elsewhere = crate::identity::DeviceIdentity::from_test_seed(9);
        let borrowed = crate::directory::signed_record(&elsewhere, "Not-Mine", 5, "attestation");
        assert!(standing_of(&frame(borrowed), &node_id).is_none());
        // Its own node_id, and a description it never signed.
        let mut rewritten = record.clone();
        rewritten["name"] = json!("Renamed-In-Flight");
        assert!(standing_of(&frame(rewritten), &node_id).is_none());
        // A record a server minted: attested, unsigned — and unable to travel.
        assert!(
            standing_of(
                &frame(
                    json!({ "node_id": node_id, "name": "Named-By-A-Server", "platform": "linux" })
                ),
                &node_id,
            )
            .is_none()
        );
        assert!(standing_of(&frame(json!({})), &node_id).is_none());
        assert!(standing_of(&json!({}), &node_id).is_none());
        // A device that does not say it can vouch cannot.
        assert!(
            !standing_of(&json!({ "record": record }), &node_id)
                .expect("a record is enough to be read")
                .holds_key
        );
    }

    /// The refusal a dialer is told, in the vocabulary its caller answers in.
    #[test]
    fn a_refusal_keeps_its_meaning_across_the_wire() {
        assert_eq!(
            refusal(Some("no_account")).app.as_deref(),
            Some("NO_ACCOUNT_KEY")
        );
        assert_eq!(
            refusal(Some("other_account")).app.as_deref(),
            Some("ACCOUNT_KEY_SET")
        );
        for anything_else in [Some("proof"), Some("busy"), Some("record"), None] {
            assert_eq!(
                refusal(anything_else).app.as_deref(),
                Some("PAIRING_STATE"),
                "{anything_else:?}"
            );
        }
    }
}
