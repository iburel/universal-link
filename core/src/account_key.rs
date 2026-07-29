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
//! Keeping it is a deliberate inversion of this module's first design, which
//! discarded AK_priv after onboarding. Discarding it made one thing impossible:
//! a device already in the account could not vouch for a new one, so every
//! device had to have the recovery code typed into it. Holding the key is what
//! lets an enrolled device hand the account to another one (the pairing
//! building block) — the same reason 1Password can sign a new device in from an
//! old one and we could not.
//!
//! What that costs, stated plainly: the account's private key now exists at
//! rest on every device, under the keyring's protection (OS keyring, or a 0600
//! file — the same perimeter as `device.key` and the refresh token). Whoever
//! reads a device's storage reads the account key. Before, they would have
//! needed the user's recovery code. So a compromised device now means the
//! ACCOUNT key is compromised, and an AK rotation stops being a nicety: it
//! becomes the mandatory response. The recovery code, for its part, stops being
//! the only copy of AK_priv and becomes what its name says — the way back when
//! every device is gone.
//!
//! The attestation binds the `node_id` alone (a stable crypto identity), not
//! the `device_id` (an ephemeral label re-minted by the server at each
//! enrollment): it survives a re-login and says what matters — "this
//! cryptographic device is one of ours". Revoking a specific device remains a
//! server-side directory removal. The signed payload is versioned
//! (`ATTEST_DOMAIN`) to make a rotation possible later.

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
const SEED_DOMAIN: &[u8] = b"universallink-account-key-v1";

/// Domain separation (and version) for the attestation. A later AK rotation
/// will sign under a bumped domain so an old attestation is never mistaken for
/// a fresh one.
const ATTEST_DOMAIN: &[u8] = b"ul-account-attest-v1:";

/// Domain separation for the fingerprint (safety number) shown for out-of-band
/// verification.
const FP_DOMAIN: &[u8] = b"ul-account-fingerprint-v1";

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
    let Some(vk) = parse_public(ak_pub_hex) else {
        return false;
    };
    let Some(sig) = parse_signature(attestation_hex) else {
        return false;
    };
    // `verify_strict`: rejects small-order keys/signatures (like the
    // proof-of-possession on the server side).
    vk.verify_strict(&attest_message(node_id), &sig).is_ok()
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
/// `None` when it holds nothing usable: a device enrolled before the key was
/// kept at rest, or a keyring that does not answer. The caller then has to fall
/// back on asking the user for the recovery code.
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

fn attest_message(node_id: &str) -> Vec<u8> {
    let mut msg = ATTEST_DOMAIN.to_vec();
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
    /// stayed green. These values were computed under the RC and must survive
    /// every future bump; if one of them ever fails, the answer is not to update
    /// the constant.
    #[test]
    fn the_derivation_matches_its_published_vectors() {
        let code = encode_code(&[0x42u8; CODE_ENTROPY]);
        assert_eq!(code, "89144GJ-289144G-J289144-GJ28A80");

        let ak = account_key_from_code(&code).expect("valid code");
        assert_eq!(
            public_hex(&ak),
            "048907521bf4e62f9c8291e067adfef7e37be87cbd3f6c89944b91cb2f589101",
        );
        assert_eq!(
            attest(&ak, &a_node_id()),
            "bed950f5621357e89d478daf60bbbf5fbd4730068aba44c041fd2c6435a752c0\
             bfb1df9596e721ad82344ccf2f42caf7a782024078cd02c88d073beca6fb4109",
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
