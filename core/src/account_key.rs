// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

//! Account key (C7): the trust root that the server does NOT hold.
//!
//! Today a peer is authorized on the sole word of the server directory: "this
//! `node_id` is in the account's list" (`dataplane::peer_in_directory`). iroh
//! proves that the peer holds the private key for its `node_id`, but nothing
//! proves that this `node_id` is a device THE USER enrolled — the server merely
//! asserts it. A malicious server can therefore inject its own `node_id` into
//! the directory and pose as a device of the account.
//!
//! C7 inserts a link that is verifiable without the server: an **account key**
//! (AK, Ed25519) signs an **attestation** for each `node_id` of the account. A
//! peer verifies another's attestation against the AK_pub it DERIVED itself
//! (never learned from the server) — the server can neither forge a member, nor
//! substitute the key, nor "un-verify" a peer (a record without a valid
//! attestation is refused: *fail-closed*).
//!
//! Chosen provisioning (the "recovery code" model): AK is derived from a
//! high-entropy code generated on the first device; every device that joins the
//! account signs ITS OWN node_id, persists `ak_pub` + its attestation in
//! `account-key.json` (public data) — and **keeps AK_priv at rest**, as a seed
//! in the keyring (`remember` / `recall`).
//!
//! Keeping it is what makes one thing possible: a device already in the account
//! can vouch for a new one, so the recovery code does not have to be typed into
//! every device (the pairing building block — the same reason 1Password can sign
//! a new device in from an old one).
//!
//! What that costs, stated plainly: the account's private key exists at rest on
//! every device, under the keyring's protection (OS keyring, or a 0600 file — the
//! same perimeter as `device.key` and the refresh token). Whoever reads a
//! device's storage reads the account key, where the user's recovery code would
//! otherwise have been needed. So a compromised device means the ACCOUNT key is
//! compromised, and an AK rotation is not a nicety: it is the response. The
//! recovery code, for its part, is not a copy to be typed everywhere — it is what
//! its name says, the way back when every device is gone.
//!
//! A device that has the account but not the key (a keyring that lost it, or one
//! that never answered) still works: `ak_pub` is what verifies peers. It simply
//! cannot vouch, and [`install`] is the one way back — the recovery code, or a
//! pairing from a device that holds the key.
//!
//! The attestation binds the `node_id` alone (a stable crypto identity), not
//! the `device_id` (an ephemeral label re-minted by the server at each
//! enrollment): it survives a re-login and says what matters — "this
//! cryptographic device is one of ours". The signed payload is versioned
//! (`ATTEST_DOMAIN`) to make a rotation possible later.
//!
//! **Taking it back.** An attestation cannot be withdrawn — a signature, once
//! made, verifies forever, and the device holds a copy of it. Where there is a
//! server, striking the device from its directory is what stops it (the peers
//! only ever learn of devices the directory lists). Where there is none, the
//! account key signs the withdrawal itself: a **revocation** ([`revoke`]) over
//! the `node_id`, under a domain of its own so that neither signature can ever
//! pass for the other. It is deliberately not dated and not countersignable —
//! nothing later can contradict it, because a "cancel the revocation" statement
//! would need a total order the account has no way to establish offline. A
//! revoked device comes back only as a NEW device, with a fresh `node_id`.

use std::path::Path;

use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use sha2::{Digest, Sha256, Sha512};

/// Entropy of the recovery code, in bytes. 128 bits: an offline brute-force of
/// AK (the attacker knows AK_pub, which is published) is out of reach, and the
/// code stays transcribable by hand.
const CODE_ENTROPY: usize = 16;

/// Domain separation for the code → AK seed derivation. Versioned: a change to
/// the derivation scheme will bump the suffix, with no collision against the
/// old one.
const SEED_DOMAIN: &[u8] = b"1device-account-key-v1";

/// Domain separation (and version) for the attestation. A later AK rotation
/// will sign under a bumped domain so an old attestation is never mistaken for
/// a fresh one.
const ATTEST_DOMAIN: &[u8] = b"1device-account-attest-v1:";

/// Domain separation (and version) for a revocation. A domain of its own, not a
/// flag inside the attestation's: an attestation must never read as a revocation
/// — anyone holding a peer's record would then be able to strike it off — and a
/// revocation must never read as an attestation.
const REVOKE_DOMAIN: &[u8] = b"1device-account-revoke-v1:";

/// Domain separation for the fingerprint (safety number) shown for out-of-band
/// verification.
const FP_DOMAIN: &[u8] = b"1device-account-fingerprint-v1";

/// File for the persisted trust root: `ak_pub` (the account's public key) + our
/// own attestation. Not a secret (unlike `device.key`) — an attacker who reads
/// it can sign nothing.
const KEY_FILE: &str = "account-key.json";

/// Crockford base32 alphabet: no I, L, O, or U (transcription ambiguities).
const CROCKFORD: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

/// The account's trust root, as persisted and held at runtime. `ak_pub` serves
/// to VERIFY peers; `attestation` is our own, republished on every
/// (re)connection (the server keeps it in memory — it forgets it on restart).
/// AK_priv is NOT here: this file carries public data only, and the private key
/// lives in the keyring (`remember` / `recall`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AccountRoot {
    /// The account's public key (AK_pub), in hex (64 chars).
    pub ak_pub: String,
    /// Our attestation: AK's signature over OUR `node_id`.
    pub attestation: String,
}

/// Why an entered recovery code is refused — distinguished for the UI
/// ("invalid format" vs "typo detected").
#[derive(Debug, PartialEq, Eq)]
pub enum RecoveryCodeError {
    /// Characters outside the alphabet, or an unexpected length.
    Malformed,
    /// Well-formed but the checksum does not add up: at least one character is
    /// wrong.
    Checksum,
}

impl std::fmt::Display for RecoveryCodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RecoveryCodeError::Malformed => write!(f, "malformed recovery code"),
            RecoveryCodeError::Checksum => {
                write!(f, "invalid recovery code (typo?)")
            }
        }
    }
}

impl std::error::Error for RecoveryCodeError {}

/// Generates a new recovery code (the account's first device). It is the ONLY
/// copy of AK_priv: show it once, hand it to the user.
pub fn generate_recovery_code() -> String {
    let mut entropy = [0u8; CODE_ENTROPY];
    rand::Rng::fill_bytes(&mut rand::rng(), &mut entropy);
    encode_code(&entropy)
}

/// Derives the account key (AK_priv) from an entered recovery code. The code is
/// case-insensitive and ignores separators/spaces; its checksum catches a typo
/// before a device would derive a silently different AK (which would leave it
/// outside the account — fail-closed).
pub fn account_key_from_code(code: &str) -> Result<SigningKey, RecoveryCodeError> {
    let entropy = decode_code(code)?;
    Ok(derive_account_key(&entropy))
}

/// AK_pub of an AK_priv, in hex (64 chars) — same encoding as a `node_id`.
pub fn public_hex(ak: &SigningKey) -> String {
    hex::encode(ak.verifying_key().to_bytes())
}

/// The trust root to persist for THIS device: AK_pub (to verify peers) + our
/// attestation over `node_id` (to republish on every connection). AK_priv stays
/// with the caller, who stows it with `remember`.
pub fn root_for(ak: &SigningKey, node_id: &str) -> AccountRoot {
    AccountRoot {
        ak_pub: public_hex(ak),
        attestation: attest(ak, node_id),
    }
}

/// Signs the attestation binding `node_id` to the account. Payload =
/// `ATTEST_DOMAIN` + `node_id` (hex, fixed length: no concatenation ambiguity).
pub fn attest(ak: &SigningKey, node_id: &str) -> String {
    hex::encode(ak.sign(&attest_message(node_id)).to_bytes())
}

/// Does the attestation `attestation_hex` prove, under `ak_pub_hex`, that
/// `node_id` is a device of the account? Any defect (unreadable key/hex/
/// signature, invalid signature) answers `false`: *fail-closed*, no exception
/// bubbles up to the authorization path.
pub fn verify(ak_pub_hex: &str, node_id: &str, attestation_hex: &str) -> bool {
    verify_detached(ak_pub_hex, &attest_message(node_id), attestation_hex)
}

/// Strikes `node_id` off the account: the account key's signature over a
/// statement no later one can contradict. This is what a device that never
/// reaches a server has instead of a directory removal — it needs no authority to
/// be checked, only the account's public key, which every device derived itself.
///
/// Permanent, by design. See the module header: un-revoking would need a total
/// order the account cannot establish offline, so the way back for a struck-off
/// device is a fresh `node_id`, attested anew.
pub fn revoke(ak: &SigningKey, node_id: &str) -> String {
    hex::encode(ak.sign(&revoke_message(node_id)).to_bytes())
}

/// Does `revocation_hex` prove, under `ak_pub_hex`, that the account struck
/// `node_id` off? Fail-closed like [`verify`]: anything unreadable or unsigned
/// answers `false` — we bar a device on a signature we checked, never on the
/// mere presence of a file.
pub fn verify_revocation(ak_pub_hex: &str, node_id: &str, revocation_hex: &str) -> bool {
    verify_detached(ak_pub_hex, &revoke_message(node_id), revocation_hex)
}

/// Detached Ed25519 verification, fail-closed: an unreadable key, an unreadable
/// signature or an invalid one all answer `false`, so no error ever bubbles up to
/// an authorization path. The single place this project checks a signature it
/// built itself, which is what makes `verify_strict` (small-order keys and
/// signatures refused, like the server's proof-of-possession) the rule rather
/// than a habit.
pub(crate) fn verify_detached(public_hex: &str, msg: &[u8], sig_hex: &str) -> bool {
    let Some(vk) = parse_public(public_hex) else {
        return false;
    };
    let Some(sig) = parse_signature(sig_hex) else {
        return false;
    };
    vk.verify_strict(msg, &sig).is_ok()
}

/// A human-readable fingerprint of AK_pub (safety number) — the anchor of
/// out-of-band verification: identical on every device of the account, it
/// diverges as soon as a device has derived a different AK (wrong code) or a
/// key has been substituted. Six groups of five digits. `None` if `ak_pub_hex`
/// is not a valid key.
pub fn fingerprint(ak_pub_hex: &str) -> Option<String> {
    let vk = parse_public(ak_pub_hex)?;
    let mut hasher = Sha512::new();
    hasher.update(FP_DOMAIN);
    hasher.update(vk.to_bytes());
    let digest = hasher.finalize();
    let groups: Vec<String> = (0..6)
        .map(|i| {
            let chunk = &digest[i * 5..i * 5 + 5];
            let n = chunk.iter().fold(0u64, |acc, &b| (acc << 8) | b as u64);
            format!("{:05}", n % 100_000)
        })
        .collect();
    Some(groups.join(" "))
}

/// Re-reads the account's trust root (`account-key.json`), or `None` if the
/// device has not joined the account yet (no attestation → fail-closed: it
/// authorizes and opens no P2P stream). A corrupt file counts as absent: setup
/// will rewrite it.
pub fn load(config_dir: &Path) -> Option<AccountRoot> {
    let text = std::fs::read_to_string(config_dir.join(KEY_FILE)).ok()?;
    let v: serde_json::Value = serde_json::from_str(&text).ok()?;
    let ak_pub = v.get("ak_pub")?.as_str()?.to_string();
    let attestation = v.get("attestation")?.as_str()?.to_string();
    Some(AccountRoot {
        ak_pub,
        attestation,
    })
}

/// Persists the trust root. Not a secret file (0644 by OS default): it carries
/// only public data.
pub fn save(config_dir: &Path, root: &AccountRoot) -> std::io::Result<()> {
    let body = serde_json::json!({
        "ak_pub": root.ak_pub,
        "attestation": root.attestation,
    });
    std::fs::write(config_dir.join(KEY_FILE), body.to_string())
}

/// Stows AK_priv in the keyring, as a hex seed — the same shape as `device.key`.
/// From then on this device can attest another `node_id` without the user
/// retyping the recovery code; that is the entire reason it is kept.
///
/// A failure is reported rather than swallowed: "joined the account but unable
/// to vouch for anyone" is a state nothing would surface until the user reached
/// a pairing screen, weeks later. Beware, though, of what an `Ok` is worth: on
/// the desktop the keyring's `set` queues the write and returns before it is
/// attempted (`daemon::secrets`), so `Ok` means *accepted*, not *written*. The
/// authoritative answer is a read-back — which is what `recall` is for.
pub fn remember(secrets: &dyn crate::SecretStore, ak: &SigningKey) -> std::io::Result<()> {
    secrets.set(crate::secrets::ACCOUNT_SEED, &hex::encode(ak.to_bytes()))
}

/// Re-reads AK_priv from the keyring, for a device that has something to attest.
/// `None` when it holds nothing usable — a keyring that does not answer, or one
/// the seed has been taken out of. The device then has the account but cannot
/// vouch for another, and the way back in is [`install`]: the recovery code, or a
/// pairing from a device that does hold the key.
///
/// The seed is checked against `ak_pub_hex` — the account's public key as
/// persisted in `account-key.json` — and any mismatch answers `None`. That is
/// not a corruption check: it is what stops the keyring from choosing which key
/// we sign with. Whoever can write the keyring but not `account-key.json` would
/// otherwise have us vouch for a device under THEIR account key, and that device
/// would join THEIR account believing it had joined ours. Fail-closed, like
/// `verify`.
pub fn recall(secrets: &dyn crate::SecretStore, ak_pub_hex: &str) -> Option<SigningKey> {
    let stored = secrets.get(crate::secrets::ACCOUNT_SEED)?;
    let seed: [u8; 32] = hex::decode(stored).ok()?.try_into().ok()?;
    let ak = SigningKey::from_bytes(&seed);
    // Compared as keys, not as strings: the encoding of `ak_pub_hex` is not the
    // subject, and an unreadable one mismatches — which is the safe answer.
    if parse_public(ak_pub_hex) != Some(ak.verifying_key()) {
        tracing::warn!("the stored account seed is not this account's key: ignored");
        return None;
    }
    Some(ak)
}

/// Why installing a trust root was refused.
#[derive(Debug, PartialEq, Eq)]
pub enum InstallError {
    /// A root is already installed for ANOTHER account key.
    OtherKey,
    /// The key or the root could not be persisted (keyring refused, folder not
    /// writable) — nothing was installed.
    SaveFailed,
}

/// Attests our `node_id` under `ak`, stows AK_priv, persists the root and
/// installs it in memory. The single way in for every path that joins the
/// account: a typed recovery code (`account.setup` / `account.join`) or a pairing
/// that handed the seed over.
///
/// A root already there for **another** account key is refused
/// ([`InstallError::OtherKey`]): replacing it is a rotation, a follow-up building
/// block, and it is also the surface an attacker would use to re-attest this
/// device under a key of their choosing — whether they hold `session.manage` or
/// sponsor a pairing. The **same** account key, however, goes through: re-deriving
/// the key this device is already attested under proves the caller has the
/// account, re-attesting is byte-for-byte the same signature (Ed25519 is
/// deterministic), and the one thing it does change is the keyring. That is what
/// makes this the way back in for a device that has the account but not its key.
///
/// **The keyring first, and read back before anything else.** Of the two halves
/// the keyring is the fragile one (a write handed to an OS service that may
/// refuse it), while the root is the *visible* one: it is what makes
/// `account.status` answer `attested: true` and what the data plane verifies
/// peers against. And a write that returned `Ok` is not a write that landed — on
/// the desktop it was merely queued (`daemon::secrets`). So the fragile half is
/// attempted first and then **verified by a read**, which is authoritative and,
/// the queue being FIFO, cannot answer before the write has been tried. A device
/// therefore never claims to have joined the account on the strength of a
/// half-install: `attested` implies the key is really there.
///
/// A seed left behind by a refused install is inert: nothing reads it but
/// `recall`, which checks it against the `ak_pub` of an installed root, and the
/// next install of any key overwrites it.
///
/// The lock is taken twice, deliberately, and never held across the keyring: that
/// round trip waits on an OS service (seconds, in the bad case) and
/// `account_root` sits on the data plane's authorization path, so holding it
/// there would stall every peer check. The first pass refuses another account's
/// key before any key material is written; the second is the one that counts,
/// since it is where the root is installed.
pub(crate) fn install(
    state: &crate::state::AppState,
    ak: &SigningKey,
) -> Result<AccountRoot, InstallError> {
    let root = root_for(ak, &state.identity.node_id());
    refuse_another_key(state, &root)?;
    stow_and_verify(&*state.secrets, ak, &root.ak_pub)?;

    let mut slot = state.account_root.lock().expect("lock account_root");
    if let Some(existing) = slot.as_ref()
        && existing.ak_pub != root.ak_pub
    {
        return Err(InstallError::OtherKey);
    }
    save(&state.config_dir, &root).map_err(|_| InstallError::SaveFailed)?;
    *slot = Some(root.clone());
    Ok(root)
}

/// Stows the key, then **reads it back**: `Ok` only once this device really holds
/// it. A keyring `set` that returned `Ok` proves nothing on its own — on the
/// desktop the write was merely queued — and the read is authoritative
/// ([`recall`] is what checks the stored seed against `ak_pub`, so an answer at
/// all means the very key we meant). The queue being FIFO, the read cannot come
/// back before the write has been attempted.
fn stow_and_verify(
    secrets: &dyn crate::SecretStore,
    ak: &SigningKey,
    ak_pub: &str,
) -> Result<(), InstallError> {
    remember(secrets, ak).map_err(|_| InstallError::SaveFailed)?;
    if recall(secrets, ak_pub).map(|k| k.to_bytes()) != Some(ak.to_bytes()) {
        tracing::error!("the account key did not reach the keyring: nothing installed");
        return Err(InstallError::SaveFailed);
    }
    Ok(())
}

/// Leaves the account: this device has been struck off (a verified tombstone
/// naming its `node_id` — [`crate::state::SessionState::absorb`] is what heard
/// it), and everything that made it a member goes. The trust root (memory and
/// file), the account key, the session and its refresh token, the directory and
/// the tombstones, and `device.key` itself. The last one is what makes the way
/// back honest: a revocation is permanent, so the `node_id` it names never
/// returns — erasing the key is what mints the fresh identity a re-pairing
/// needs, at the next startup. Until that restart the running Core keeps its
/// old identity in memory, and every door fails closed on its own: no trust
/// root means nothing is authorized, served, offered or synced.
///
/// The human's data is not the account's: nothing but the account material is
/// touched. And obeying is not what excludes the device — every peer holding
/// the tombstone already refuses it — it is the device's own hygiene: a machine
/// the account struck off must not keep the account's private key, and must not
/// keep believing.
pub(crate) fn leave(state: &std::sync::Arc<crate::state::AppState>) {
    // The trust root's slot is the claim: `take()` hands it to exactly one
    // caller, so two streams delivering the same tombstone in the same breath
    // leave ONCE — a second `account.left` would tell the human the same thing
    // twice. A leaf lock, taken and released alone, like everywhere. (A guard
    // against a race no test can schedule: between honest Cores the second
    // delivery is already turned away upstream, `dirsync::absorb` finding no
    // trust root to verify against — this claim survives a mutation campaign
    // for that reason, and is kept for the interleaving nobody arranges.)
    if state
        .account_root
        .lock()
        .expect("lock account_root")
        .take()
        .is_none()
    {
        return;
    }
    // The keyring next, before the session lock (its calls can block; and on the
    // desktop a delete is queued — accepted, like `remember`'s write). The
    // refresh token goes too: a session is part of the membership, exactly as
    // `session.logout` treats it.
    state.secrets.delete(crate::secrets::ACCOUNT_SEED);
    state.secrets.delete(crate::secrets::REFRESH_TOKEN);
    let path = state.config_dir.join(KEY_FILE);
    match std::fs::remove_file(&path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        // Loud but not fatal: at the next startup the root attests a node_id
        // whose key is gone, and `spawn`'s filter ignores it — fail-closed.
        Err(e) => tracing::error!(error = %e, "failed to erase the trust root file"),
    }
    crate::identity::remove(&state.config_dir);

    let abort = {
        let mut s = state.session.lock().expect("lock session");
        let gone: Vec<String> = s
            .devices
            .as_ref()
            .map(|devices| devices.keys().cloned().collect())
            .unwrap_or_default();
        // `forget(None, None)`: unlike a logout, nothing survives — not this
        // device's own record, and none of the account's half either (the leave
        // ends the MEMBERSHIP, not just the session). The tombstones go with the
        // rest: they were the account's signatures, and a device with no trust
        // root can verify none of them anyway.
        let (payload, abort) = s.forget(None, None);
        s.revoked.clear();
        // Under the same lock as the wipe: an exchange that read the trust root
        // before we took it must find this the moment it gets here, or it would
        // write the account it carried into the directory we just erased. (The
        // CHECK is pinned by a unit test; this SET survives a mutation campaign,
        // because the interleaving it guards is one no test can schedule — the
        // absorb must be past its root read, and not yet at this lock, in the
        // instant the leave holds it.)
        s.left_the_account = true;
        // Disk under the lock that mutated the state (state-then-disk), like
        // every directory write.
        crate::session::remove_session_file(&state.config_dir);
        crate::directory::remove(&state.config_dir);
        crate::directory::remove_revoked(&state.config_dir);
        // A device out of the account keeps nothing of the deployment's
        // relay announcement either (#105).
        crate::relays::forget(&state.config_dir);
        // Broadcast under the session lock (order: session then registry): the
        // order of notifications is the order of states, and nothing may slip a
        // stale directory event in after these.
        let registry = state.registry.lock().expect("lock registry");
        for device_id in gone {
            registry.notify_topic(
                "devices",
                "device.removed",
                &serde_json::json!({ "device_id": device_id }),
            );
        }
        registry.notify_topic("session", "session.changed", &payload);
        // The one event that says WHY, for the sentence the interface owes the
        // human: this was the account's decision, not a logout.
        registry.notify_topic(
            "session",
            "account.left",
            &serde_json::json!({ "reason": "struck_off" }),
        );
        abort
    };
    if let Some(abort) = abort {
        abort.abort();
    }
    // A pending OIDC flow belonged to the membership that just ended.
    if let Some(slot) = state.login.lock().expect("lock login").take() {
        slot.abort.abort();
    }
    // A pairing in flight cannot end well anymore, on either end: this device
    // has no account to sponsor into and no standing to join with. `no_account`
    // is the truth of the matter, said in the vocabulary the interface knows.
    crate::pairing::fail_current(state, "no_account");
    // The account's read grants do not outlive it (same as the logout).
    state.clipboard.lock().expect("lock clipboard").clear_all();
    state.clipboard_reset.notify_waiters();
}

/// A server has just accepted a revocation; where this device holds the account
/// key, the ACCOUNT signs it too. A server-side strike stops at the server's
/// reach — it mints no tombstone, so the account's serverless half would keep
/// trusting the struck device until its store expired, or forever where nothing
/// expires. This closes that gap whenever it can be closed: the tombstone joins
/// `revoked.json` and the next sync round carries it (the continuum).
///
/// Best-effort, and honest about it: a device that holds the account WITHOUT its
/// key cannot sign the account's word, and says so in a log rather than pretend
/// (the server's strike stands on its own, exactly as before the continuum). And
/// never against ourselves — a self-revocation through the server stays what it
/// always was, `DEVICE_REVOKED` at the next exchange: signing a tombstone
/// against one's own `node_id` is `CANNOT_REVOKE_SELF` everywhere else, and a
/// server's yes does not change whose signature it would be.
pub(crate) fn tombstone_after_server_revoke(state: &crate::state::AppState, node_id: &str) {
    if node_id == state.identity.node_id() {
        return;
    }
    // The keyring read before any state lock (it can block; leaf-lock rules).
    let ak_pub = {
        let root = state.account_root.lock().expect("lock account_root");
        root.as_ref().map(|root| root.ak_pub.clone())
    };
    let Some(ak) = ak_pub
        .as_deref()
        .and_then(|ak_pub| recall(&*state.secrets, ak_pub))
    else {
        tracing::info!(
            %node_id,
            "revoked on the server only: no account key here, so no tombstone for the serverless half"
        );
        return;
    };
    let revocation = revoke(&ak, node_id);
    {
        let mut s = state.session.lock().expect("lock session");
        let struck = s.strike_off(node_id, revocation);
        // Disk under the lock that mutated the state (state-then-disk).
        // `save_unrefreshed`: the tombstone is the account's word, not a
        // re-check of the records against the server.
        crate::directory::save_revoked(&state.config_dir, &s.revoked);
        if let Some(devices) = s.devices.as_ref() {
            crate::directory::save_unrefreshed(&state.config_dir, devices);
        }
        // Usually empty — the server's own `device.removed` evicted the record
        // before its reply reached us — but a label the server never knew (a
        // `node_id`-keyed twin) leaves here, and someone should hear it.
        let registry = state.registry.lock().expect("lock registry");
        for device_id in struck {
            registry.notify_topic(
                "devices",
                "device.removed",
                &serde_json::json!({ "device_id": device_id }),
            );
        }
    }
    // The account's other devices should hear the tombstone now, not at the
    // next tick — the struck device least of all can be left to wait.
    state.dirsync_wake.notify_one();
}

/// `OtherKey` if a root is already installed for a different account key.
fn refuse_another_key(
    state: &crate::state::AppState,
    root: &AccountRoot,
) -> Result<(), InstallError> {
    let slot = state.account_root.lock().expect("lock account_root");
    match slot.as_ref() {
        Some(existing) if existing.ak_pub != root.ak_pub => Err(InstallError::OtherKey),
        _ => Ok(()),
    }
}

fn attest_message(node_id: &str) -> Vec<u8> {
    let mut msg = ATTEST_DOMAIN.to_vec();
    msg.extend_from_slice(node_id.as_bytes());
    msg
}

fn revoke_message(node_id: &str) -> Vec<u8> {
    let mut msg = REVOKE_DOMAIN.to_vec();
    msg.extend_from_slice(node_id.as_bytes());
    msg
}

fn derive_account_key(entropy: &[u8; CODE_ENTROPY]) -> SigningKey {
    let mut hasher = Sha512::new();
    hasher.update(SEED_DOMAIN);
    hasher.update(entropy);
    let digest = hasher.finalize();
    let mut seed = [0u8; 32];
    seed.copy_from_slice(&digest[..32]);
    SigningKey::from_bytes(&seed)
}

fn parse_public(hex_str: &str) -> Option<VerifyingKey> {
    let bytes: [u8; 32] = hex::decode(hex_str).ok()?.try_into().ok()?;
    VerifyingKey::from_bytes(&bytes).ok()
}

fn parse_signature(hex_str: &str) -> Option<Signature> {
    let bytes: [u8; 64] = hex::decode(hex_str).ok()?.try_into().ok()?;
    Some(Signature::from_bytes(&bytes))
}

/// A one-byte checksum: the first byte of SHA-256(entropy). Catches a typo
/// (≈ 1 in 256 chance of slipping through unnoticed), not a forgery — that is
/// not its job.
fn checksum(entropy: &[u8; CODE_ENTROPY]) -> u8 {
    let mut hasher = Sha256::new();
    hasher.update(entropy);
    hasher.finalize()[0]
}

fn encode_code(entropy: &[u8; CODE_ENTROPY]) -> String {
    let mut payload = entropy.to_vec();
    payload.push(checksum(entropy));
    group(&base32_encode(&payload), 7)
}

fn decode_code(code: &str) -> Result<[u8; CODE_ENTROPY], RecoveryCodeError> {
    let raw: String = code
        .chars()
        .filter(|c| !c.is_whitespace() && *c != '-')
        .collect();
    let bytes = base32_decode(&raw).ok_or(RecoveryCodeError::Malformed)?;
    if bytes.len() != CODE_ENTROPY + 1 {
        return Err(RecoveryCodeError::Malformed);
    }
    let mut entropy = [0u8; CODE_ENTROPY];
    entropy.copy_from_slice(&bytes[..CODE_ENTROPY]);
    if bytes[CODE_ENTROPY] != checksum(&entropy) {
        return Err(RecoveryCodeError::Checksum);
    }
    Ok(entropy)
}

/// Inserts a dash every `n` characters (code readability, like a product key).
fn group(s: &str, n: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    chars
        .chunks(n)
        .map(|c| c.iter().collect::<String>())
        .collect::<Vec<_>>()
        .join("-")
}

fn base32_encode(data: &[u8]) -> String {
    let mut out = String::new();
    let mut buffer: u32 = 0;
    let mut bits: u32 = 0;
    for &b in data {
        buffer = (buffer << 8) | b as u32;
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            out.push(CROCKFORD[((buffer >> bits) & 0x1f) as usize] as char);
        }
        buffer &= (1 << bits) - 1;
    }
    if bits > 0 {
        out.push(CROCKFORD[((buffer << (5 - bits)) & 0x1f) as usize] as char);
    }
    out
}

fn base32_decode(s: &str) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let mut buffer: u32 = 0;
    let mut bits: u32 = 0;
    for c in s.chars() {
        let v = decode_char(c)?;
        buffer = (buffer << 5) | v as u32;
        bits += 5;
        if bits >= 8 {
            bits -= 8;
            out.push(((buffer >> bits) & 0xff) as u8);
        }
        buffer &= (1 << bits) - 1;
    }
    // The remaining padding bits (< 8) must be zero: otherwise the code was
    // truncated or padded out.
    if buffer != 0 {
        return None;
    }
    Some(out)
}

fn decode_char(c: char) -> Option<u8> {
    let c = match c.to_ascii_uppercase() {
        'I' | 'L' => '1',
        'O' => '0',
        other => other,
    };
    CROCKFORD
        .iter()
        .position(|&x| x as char == c)
        .map(|p| p as u8)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A test `node_id` (64 hex): a valid Ed25519 public key.
    fn a_node_id() -> String {
        let key = SigningKey::from_bytes(&[7u8; 32]);
        hex::encode(key.verifying_key().to_bytes())
    }

    /// Fixed vectors, deliberately, where everything else here round-trips.
    ///
    /// A recovery code is the ONLY copy of the account key, and a device's seed
    /// is persisted (identity.rs). So the mapping from code to AK_pub, and the
    /// signature over an attestation, must never move: a change would strand
    /// every enrolled device and invalidate every code ever handed out. The
    /// round-trip tests below cannot notice such a change, because both ends of
    /// the round trip move together — which is exactly what happened when
    /// `ed25519-dalek` went from `3.0.0-rc.0` to `3.0.0` and the whole suite
    /// stayed green. These values must survive every future bump; if one of
    /// them ever fails, the answer is not to update the constant. They were
    /// re-pinned exactly once, deliberately: the rename to 1Device changed the
    /// domain-separation strings, an identity break in which nothing pre-rename
    /// survives, these vectors included.
    #[test]
    fn the_derivation_matches_its_published_vectors() {
        let code = encode_code(&[0x42u8; CODE_ENTROPY]);
        assert_eq!(code, "89144GJ-289144G-J289144-GJ28A80");

        let ak = account_key_from_code(&code).expect("valid code");
        assert_eq!(
            public_hex(&ak),
            "7345bccf9cb31482522957fc861088070ae604535df7b2c666f77d630fc917b3",
        );
        assert_eq!(
            attest(&ak, &a_node_id()),
            "bd38c1c1aa7c5af80b321eaaec44e4c29a476645752a6ff6e30c5e7d751719a5\
             3315e3dcf3e4832543c71be0ca24e17db177c418b87c354bd8e672fd97d2fc0e",
        );
        // A revocation outlives everything else here: it is permanent, it is
        // persisted, and it travels between devices. A version of this project
        // that stopped verifying the ones already handed out would silently take
        // struck-off devices back into the account.
        assert_eq!(
            revoke(&ak, &a_node_id()),
            "58d373a9ed990cc51fb85945b9e0a50553aa95425f5343ca01d2bd905b9cc05d\
             f6c4807da20d4c3cf05da6bbf14a4ba146e890baefa27e8e018650e5ba78a50f",
        );
    }

    /// The seed a device persists, likewise pinned: `SigningKey::to_bytes` is
    /// what identity.rs writes to disk and `from_bytes` is what reads it back, so
    /// the public key derived from a known seed is a compatibility contract with
    /// every installation in the field, not an implementation detail.
    #[test]
    fn a_persisted_seed_still_yields_its_published_public_key() {
        let key = SigningKey::from_bytes(&[7u8; 32]);
        assert_eq!(key.to_bytes(), [7u8; 32]);
        assert_eq!(
            hex::encode(key.verifying_key().to_bytes()),
            "ea4a6c63e29c520abef5507b132ec5f9954776aebebe7b92421eea691446d22c",
        );
    }

    #[test]
    fn a_generated_code_round_trips_to_a_stable_key() {
        let code = generate_recovery_code();
        let ak1 = account_key_from_code(&code).expect("valid code");
        let ak2 = account_key_from_code(&code).expect("valid code");
        // Deterministic derivation: two devices, same code, same AK.
        assert_eq!(ak1.to_bytes(), ak2.to_bytes());
        assert_eq!(public_hex(&ak1), public_hex(&ak2));
    }

    #[test]
    fn a_code_is_insensitive_to_case_and_separators() {
        let code = generate_recovery_code();
        let mangled = code.to_lowercase().replace('-', "  ");
        let a = account_key_from_code(&code).expect("valid code");
        let b = account_key_from_code(&mangled).expect("tolerated code");
        assert_eq!(a.to_bytes(), b.to_bytes());
    }

    /// A FIXED code and a fixed typo, deliberately. The checksum is one byte, so
    /// roughly one typo in 256 slips through unnoticed — which is precisely what
    /// this test used to be: a random code plus a random substitution failed
    /// about 0.4 % of runs (measured: 79 escapes in 20 000 draws). The vector
    /// below is a typo the checksum does catch, which is the property being
    /// pinned; the sweep that follows states what one byte is actually worth,
    /// instead of pretending it is worth more.
    #[test]
    fn a_typo_is_caught_by_the_checksum() {
        let code = encode_code(&[0x42u8; CODE_ENTROPY]);
        let chars: Vec<char> = code.chars().collect();
        // '8' → '0': still inside the alphabet, so this targets the checksum and
        // not the format.
        let mut typo = chars.clone();
        typo[0] = '0';
        assert_eq!(
            account_key_from_code(&typo.into_iter().collect::<String>()).unwrap_err(),
            RecoveryCodeError::Checksum
        );

        // Every single-character substitution of that code: a one-byte checksum
        // lets about 1 in 256 through, and no more than that.
        let (mut tried, mut escaped) = (0, 0);
        for (i, c) in chars.iter().enumerate().filter(|(_, c)| **c != '-') {
            for sub in CROCKFORD.iter().map(|b| *b as char).filter(|s| s != c) {
                let mut mangled = chars.clone();
                mangled[i] = sub;
                tried += 1;
                if account_key_from_code(&mangled.into_iter().collect::<String>()).is_ok() {
                    escaped += 1;
                }
            }
        }
        assert!(
            escaped * 50 < tried,
            "the checksum let {escaped}/{tried} single-character typos through"
        );
    }

    #[test]
    fn a_code_with_a_foreign_character_is_malformed() {
        assert_eq!(
            account_key_from_code("ABCDEFG-!!!!!!!").unwrap_err(),
            RecoveryCodeError::Malformed
        );
    }

    #[test]
    fn base32_round_trips_arbitrary_bytes() {
        for len in 0..40usize {
            let data: Vec<u8> = (0..len).map(|i| (i as u8).wrapping_mul(37)).collect();
            let encoded = base32_encode(&data);
            assert_eq!(base32_decode(&encoded).as_deref(), Some(&data[..]));
        }
    }

    #[test]
    fn an_attestation_verifies_only_for_its_own_node_and_key() {
        let ak = account_key_from_code(&generate_recovery_code()).unwrap();
        let node_id = a_node_id();
        let att = attest(&ak, &node_id);
        let ak_pub = public_hex(&ak);

        assert!(
            verify(&ak_pub, &node_id, &att),
            "the nominal case must pass"
        );

        // Tampered node_id → refusal (attestation for ANOTHER device).
        let other_node = SigningKey::from_bytes(&[9u8; 32]);
        let other_node_id = hex::encode(other_node.verifying_key().to_bytes());
        assert!(!verify(&ak_pub, &other_node_id, &att));

        // Tampered signature → refusal.
        let mut tampered = att.clone();
        tampered.replace_range(0..2, if att.starts_with("00") { "11" } else { "00" });
        assert!(!verify(&ak_pub, &node_id, &tampered));

        // Verified under ANOTHER account key → refusal (the heart of C7: each
        // account has its own AK; one account's attestation is worthless
        // elsewhere).
        let other_ak = account_key_from_code(&generate_recovery_code()).unwrap();
        assert!(!verify(&public_hex(&other_ak), &node_id, &att));
    }

    /// A revocation is checked like an attestation — and, above all, is NOT one.
    /// The two signatures are made by the same key over the same `node_id`, and
    /// only their domain tells them apart: without that separation, every peer's
    /// record would carry the means to strike that peer off.
    #[test]
    fn a_revocation_is_not_an_attestation() {
        let ak = account_key_from_code(&generate_recovery_code()).unwrap();
        let ak_pub = public_hex(&ak);
        let node_id = a_node_id();
        let att = attest(&ak, &node_id);
        let tombstone = revoke(&ak, &node_id);

        assert!(verify_revocation(&ak_pub, &node_id, &tombstone));
        assert_ne!(att, tombstone, "same key, same node: different statements");
        assert!(
            !verify_revocation(&ak_pub, &node_id, &att),
            "an attestation must never read as a revocation"
        );
        assert!(
            !verify(&ak_pub, &node_id, &tombstone),
            "a revocation must never read as an attestation"
        );

        // And it says nothing about another device, nor under another account's
        // key: no one strikes off a device of an account they are not in.
        let other_node = hex::encode(
            SigningKey::from_bytes(&[9u8; 32])
                .verifying_key()
                .to_bytes(),
        );
        assert!(!verify_revocation(&ak_pub, &other_node, &tombstone));
        let other_ak = account_key_from_code(&generate_recovery_code()).unwrap();
        assert!(!verify_revocation(
            &public_hex(&other_ak),
            &node_id,
            &tombstone
        ));
        // Fail-closed on garbage, like everything else on this path.
        assert!(!verify_revocation("not-hex", &node_id, "not-hex"));
        assert!(!verify_revocation(&ak_pub, &node_id, ""));
    }

    #[test]
    fn verify_is_fail_closed_on_garbage() {
        let node_id = a_node_id();
        assert!(!verify("not-hex", &node_id, "not-hex"));
        assert!(!verify("", &node_id, ""));
        assert!(!verify(&"ab".repeat(32), &node_id, &"cd".repeat(64)));
    }

    #[test]
    fn a_fingerprint_is_stable_and_key_specific() {
        let ak = account_key_from_code(&generate_recovery_code()).unwrap();
        let fp = fingerprint(&public_hex(&ak)).expect("fingerprint");
        assert_eq!(fingerprint(&public_hex(&ak)).as_deref(), Some(fp.as_str()));
        // Shape: 6 groups of 5 digits.
        let groups: Vec<&str> = fp.split(' ').collect();
        assert_eq!(groups.len(), 6);
        assert!(
            groups
                .iter()
                .all(|g| g.len() == 5 && g.bytes().all(|b| b.is_ascii_digit()))
        );

        let other = account_key_from_code(&generate_recovery_code()).unwrap();
        assert_ne!(fingerprint(&public_hex(&other)), Some(fp));
    }

    /// A keyring that ACCEPTS a write and loses it — which is what an `Ok` from a
    /// queued write can turn out to mean — must not pass for a device that holds
    /// the account key. Nothing else can catch this: `set` said yes.
    #[test]
    fn a_write_that_never_landed_is_not_an_install() {
        /// Says yes, keeps nothing.
        #[derive(Debug)]
        struct Mute;
        impl crate::SecretStore for Mute {
            fn get(&self, _: &str) -> Option<String> {
                None
            }
            fn set(&self, _: &str, _: &str) -> std::io::Result<()> {
                Ok(())
            }
            fn delete(&self, _: &str) {}
        }

        let ak = account_key_from_code(&generate_recovery_code()).unwrap();
        assert_eq!(
            stow_and_verify(&Mute, &ak, &public_hex(&ak)),
            Err(InstallError::SaveFailed)
        );

        // And a keyring that keeps what it is given passes, on the same code path.
        let dir = tempfile::tempdir().unwrap();
        let secrets = crate::FileSecretStore::new(dir.path());
        assert_eq!(stow_and_verify(&secrets, &ak, &public_hex(&ak)), Ok(()));
    }

    #[test]
    fn a_remembered_key_comes_back_for_its_own_account() {
        let dir = tempfile::tempdir().unwrap();
        let secrets = crate::FileSecretStore::new(dir.path());
        let ak = account_key_from_code(&generate_recovery_code()).unwrap();
        let ak_pub = public_hex(&ak);

        assert!(
            recall(&secrets, &ak_pub).is_none(),
            "nothing held until the device joined"
        );
        remember(&secrets, &ak).expect("stow the account key");
        assert_eq!(
            recall(&secrets, &ak_pub).map(|k| k.to_bytes()),
            Some(ak.to_bytes()),
            "the very key, ready to attest another node"
        );

        // Asked for under ANOTHER account's key: refused. A device holds the key
        // of one account, and answering here would be answering about a key it
        // does not have.
        let other = account_key_from_code(&generate_recovery_code()).unwrap();
        assert!(recall(&secrets, &public_hex(&other)).is_none());
    }

    /// The guard that matters: the keyring does not get to choose the key we sign
    /// with. An attacker able to write it — but not `account-key.json`, which
    /// pins `ak_pub` — must not have us vouch for a device under their own
    /// account key.
    #[test]
    fn a_substituted_seed_is_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let secrets = crate::FileSecretStore::new(dir.path());
        let ours = account_key_from_code(&generate_recovery_code()).unwrap();
        let theirs = account_key_from_code(&generate_recovery_code()).unwrap();

        remember(&secrets, &theirs).expect("stow");
        assert!(
            recall(&secrets, &public_hex(&ours)).is_none(),
            "a seed that does not derive our ak_pub must count as absent"
        );
    }

    #[test]
    fn an_unusable_stored_seed_counts_as_absent() {
        let dir = tempfile::tempdir().unwrap();
        let secrets = crate::FileSecretStore::new(dir.path());
        let ak = account_key_from_code(&generate_recovery_code()).unwrap();
        let ak_pub = public_hex(&ak);

        for junk in ["", "not-hex", "ab", &"ab".repeat(31), &"ab".repeat(33)] {
            crate::SecretStore::set(&secrets, crate::secrets::ACCOUNT_SEED, junk).expect("stow");
            assert!(
                recall(&secrets, &ak_pub).is_none(),
                "unusable seed accepted: {junk:?}"
            );
        }

        // The real seed with something appended to it, and cut short. Only the
        // length check catches the first: take the leading 32 bytes of it and you
        // derive the right key, so a lenient decoder would hand back the account
        // key for a value that is not the one we stored.
        let real = hex::encode(ak.to_bytes());
        for tampered in [format!("{real}ab"), real[..60].to_string()] {
            crate::SecretStore::set(&secrets, crate::secrets::ACCOUNT_SEED, &tampered)
                .expect("stow");
            assert!(
                recall(&secrets, &ak_pub).is_none(),
                "a seed of the wrong length accepted: {tampered}"
            );
        }

        // And a valid seed asked for under an unreadable ak_pub: mismatch, so
        // absent — never "well, the seed parses, good enough".
        remember(&secrets, &ak).expect("stow");
        assert!(recall(&secrets, "not-a-key").is_none());
    }

    #[test]
    fn load_save_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            load(dir.path()),
            None,
            "no root until the account is joined"
        );
        let root = AccountRoot {
            ak_pub: public_hex(&account_key_from_code(&generate_recovery_code()).unwrap()),
            attestation: "de".repeat(64),
        };
        save(dir.path(), &root).unwrap();
        assert_eq!(load(dir.path()), Some(root));
    }
}
