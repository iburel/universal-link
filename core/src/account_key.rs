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
//! high-entropy code generated on the first device; every device re-derives AK
//! by entering the same code, signs ITS OWN node_id, persists `ak_pub` + its
//! attestation (not secrets) and DISCARDS AK_priv — after enrollment no device
//! holds the account's private key at rest; only the code (in the user's hands)
//! reconstitutes it.
//!
//! The attestation binds the `node_id` alone (a stable crypto identity), not
//! the `device_id` (an ephemeral label re-minted by the server at each
//! enrollment): it survives a re-login and says what matters — "this
//! cryptographic device is one of ours". Revoking a specific device remains a
//! server-side directory removal; the "compromised device that keeps the
//! secret" case is handled by an AK rotation (follow-up building block). The
//! signed payload is versioned (`ATTEST_DOMAIN`) to make that rotation possible
//! later.

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
/// AK_priv is NOT here: discarded after setup.
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
/// with the caller, who discards it afterwards.
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

    #[test]
    fn a_typo_is_caught_by_the_checksum() {
        let code = generate_recovery_code();
        // Change a content character (the first one), staying within the
        // alphabet, to target the checksum and not the format.
        let mut chars: Vec<char> = code.chars().collect();
        let first = chars.iter().position(|c| *c != '-').unwrap();
        chars[first] = if chars[first] == '0' { '1' } else { '0' };
        let typo: String = chars.into_iter().collect();
        match account_key_from_code(&typo) {
            Err(RecoveryCodeError::Checksum) => {}
            other => panic!("a typo must be caught: {other:?}"),
        }
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
