// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

//! The engine's own keypair: what signs this device's word about its own
//! monitors, and the arrangement a human dragged here (doc/input-sharing.md,
//! section 6). The engine speaks only the public Core API and cannot ask the
//! Core to sign anything, so the layout document is signed with this key; its
//! public half is what peers pin, bound to the node_id by the channel that
//! delivered it, never by a certificate.
//!
//! Minted once at first start, persisted as a seed, and NEVER regenerated over a
//! corrupt file: a silently fresh key would unpin this device from every plane
//! it takes part in, and the sibling that had pinned the old one would then
//! refuse this device's word about its own screens. The sync engine's rule, for
//! the sync engine's reason.

use std::io;
use std::path::Path;

use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};

const IDENTITY_FILE: &str = "identity.json";

pub struct Identity {
    key: SigningKey,
}

impl Identity {
    /// Loads the persisted keypair, or mints one if the file is ABSENT. Any
    /// other state (unreadable, unparsable, a seed of the wrong length) is an
    /// error the component reports and dies on, deliberately not self-healing
    /// (module header).
    pub fn load_or_generate(dir: &Path) -> io::Result<Identity> {
        let path = dir.join(IDENTITY_FILE);
        match std::fs::read_to_string(&path) {
            Ok(text) => Self::parse(&text),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                let key = SigningKey::generate(&mut rand::rng());
                let doc = serde_json::json!({ "seed": hex::encode(key.to_bytes()) });
                crate::store::write_private_atomic(&path, doc.to_string().as_bytes())?;
                Ok(Identity { key })
            }
            Err(e) => Err(e),
        }
    }

    fn parse(text: &str) -> io::Result<Identity> {
        let invalid = || io::Error::other("invalid identity.json");
        let doc: serde_json::Value = serde_json::from_str(text).map_err(|_| invalid())?;
        let seed_hex = doc
            .get("seed")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(invalid)?;
        let seed: [u8; 32] = hex::decode(seed_hex)
            .ok()
            .and_then(|b| b.try_into().ok())
            .ok_or_else(invalid)?;
        Ok(Identity {
            key: SigningKey::from_bytes(&seed),
        })
    }

    /// The public half, hex: what travels in the layout document and what peers
    /// pin.
    pub fn public_hex(&self) -> String {
        hex::encode(self.key.verifying_key().to_bytes())
    }

    /// Detached signature, hex. The message ALWAYS carries its own domain (every
    /// signed object of this engine starts with a constant prefix naming its
    /// kind, so the one key cannot sign one kind and have it verify as another):
    /// core/src/identity.rs's separation doctrine, unchanged.
    pub fn sign(&self, msg: &[u8]) -> String {
        hex::encode(self.key.sign(msg).to_bytes())
    }
}

/// Does `sig_hex` verify `msg` under the peer engine key `pub_hex`? Anything
/// malformed (a key or a signature of the wrong length, not hex at all) reads as
/// "no", because a signature that cannot be checked is a signature that did not
/// verify, and this is the one place where being generous would be a hole.
pub fn verify(pub_hex: &str, msg: &[u8], sig_hex: &str) -> bool {
    let Some(key) = hex::decode(pub_hex)
        .ok()
        .and_then(|b| <[u8; 32]>::try_from(b).ok())
        .and_then(|b| VerifyingKey::from_bytes(&b).ok())
    else {
        return false;
    };
    let Some(sig) = hex::decode(sig_hex)
        .ok()
        .and_then(|b| <[u8; 64]>::try_from(b).ok())
        .map(|bytes| Signature::from_bytes(&bytes))
    else {
        return false;
    };
    key.verify(msg, &sig).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_identity_is_minted_once_and_survives_a_reload() {
        let dir = tempfile::tempdir().expect("tempdir");
        let first = Identity::load_or_generate(dir.path()).expect("mint");
        let second = Identity::load_or_generate(dir.path()).expect("reload");
        assert_eq!(first.public_hex(), second.public_hex());

        let other = tempfile::tempdir().expect("tempdir");
        let third = Identity::load_or_generate(other.path()).expect("mint elsewhere");
        assert_ne!(first.public_hex(), third.public_hex());
    }

    #[test]
    fn a_corrupt_identity_is_an_error_not_a_fresh_key() {
        let dir = tempfile::tempdir().expect("tempdir");
        for bad in ["not json", "{}", r#"{ "seed": "abc" }"#, r#"{ "seed": 7 }"#] {
            std::fs::write(dir.path().join(IDENTITY_FILE), bad).expect("write");
            assert!(
                Identity::load_or_generate(dir.path()).is_err(),
                "should have refused: {bad}"
            );
        }
    }

    /// The pair of it: a signature verifies under the published key, and
    /// everything a peer could send that is not one reads as a refusal rather
    /// than as an error a caller might forget to handle.
    #[test]
    fn signatures_verify_under_the_published_key_and_nothing_else_does() {
        let dir = tempfile::tempdir().expect("tempdir");
        let identity = Identity::load_or_generate(dir.path()).expect("mint");
        let msg = b"1device-input:placement:payload";
        let sig = identity.sign(msg);
        assert!(verify(&identity.public_hex(), msg, &sig));

        assert!(
            !verify(&identity.public_hex(), b"another message", &sig),
            "a signature is about the bytes it covered"
        );
        let other = Identity::load_or_generate(tempfile::tempdir().expect("tempdir").path())
            .expect("mint elsewhere");
        assert!(
            !verify(&other.public_hex(), msg, &sig),
            "a signature is about the key that made it"
        );
        for bad_key in ["", "zz", &"0".repeat(64)] {
            assert!(
                !verify(bad_key, msg, &sig),
                "key {bad_key:?} must not verify"
            );
        }
        for bad_sig in ["", "zz", &"0".repeat(128)] {
            assert!(
                !verify(&identity.public_hex(), msg, bad_sig),
                "signature {bad_sig:?} must not verify"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn the_seed_file_is_private() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().expect("tempdir");
        Identity::load_or_generate(dir.path()).expect("mint");
        let mode = std::fs::metadata(dir.path().join(IDENTITY_FILE))
            .expect("metadata")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600, "the seed must be owner-only");
    }
}
