// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 Iwan Burel <iwan.burel@gmail.com>

//! The engine's own keypair: the trust root of membership
//! (doc/sync-engine.md, section 2). The engine speaks only the public Core
//! API and cannot ask the Core to sign anything, so membership records are
//! signed with this key; its public half is what peers pin, bound to the
//! node_id by the channel that delivered it, never by a certificate.
//!
//! Minted once at first start, persisted as a seed, and NEVER regenerated
//! over a corrupt file: a silently fresh key would unpin this device from
//! every set it belongs to.

use std::io;
use std::path::Path;

use ed25519_dalek::{Signer, SigningKey};

const IDENTITY_FILE: &str = "identity.json";

pub struct Identity {
    key: SigningKey,
}

impl Identity {
    /// Loads the persisted keypair, or mints one if the file is ABSENT.
    /// Any other state (unreadable, unparsable, a seed of the wrong length)
    /// is an error the component reports and dies on - deliberately not
    /// self-healing (module header).
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

    /// The public half, hex: what travels as `sync_pub` and what peers pin.
    pub fn public_hex(&self) -> String {
        hex::encode(self.key.verifying_key().to_bytes())
    }

    /// Detached signature, hex. The message ALWAYS carries its own domain
    /// (a record's canonical encoding includes `set_id` and `status`):
    /// core/src/identity.rs's separation doctrine, unchanged.
    pub fn sign(&self, msg: &[u8]) -> String {
        hex::encode(self.key.sign(msg).to_bytes())
    }
}

#[cfg(test)]
mod tests {
    use ed25519_dalek::{Signature, Verifier, VerifyingKey};

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

    #[test]
    fn signatures_verify_under_the_published_key() {
        let dir = tempfile::tempdir().expect("tempdir");
        let identity = Identity::load_or_generate(dir.path()).expect("mint");
        let msg = b"domain:v1:payload";
        let sig_bytes: [u8; 64] = hex::decode(identity.sign(msg))
            .expect("hex")
            .try_into()
            .expect("length");
        let key_bytes: [u8; 32] = hex::decode(identity.public_hex())
            .expect("hex")
            .try_into()
            .expect("length");
        let key = VerifyingKey::from_bytes(&key_bytes).expect("key");
        assert!(
            key.verify(msg, &Signature::from_bytes(&sig_bytes)).is_ok(),
            "the detached signature must verify under the published key"
        );
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
