//! Signing identity for the verifiable ledger (M2).
//!
//! An ed25519 keypair signs every journal entry's hash, so the journal is not
//! just tamper-*evident* (hash chain, M1) but **non-repudiable**: a valid run can
//! only have been produced by the holder of this private key. The public key is
//! recorded on the session; `verify` checks every signature against it.
//!
//! Trust model: storing the public key in the DB lets anyone *without* the
//! private key be detected if they alter the log. To also defend against the key
//! holder, pin the expected public key out-of-band (`--expect-key`) or anchor the
//! head hash externally (M4). For single-operator use, the embedded key is the
//! pragmatic default.

use anyhow::{anyhow, Result};
use ed25519_dalek::{Signature, Signer as _, SigningKey, Verifier as _, VerifyingKey};

/// Holds the private signing key.
pub struct Signer {
    key: SigningKey,
}

impl Signer {
    /// Generate a fresh random identity.
    pub fn generate() -> Self {
        let mut seed = [0u8; 32];
        getrandom::getrandom(&mut seed).expect("OS RNG unavailable");
        Signer {
            key: SigningKey::from_bytes(&seed),
        }
    }

    /// Load a 32-byte seed from `path`, or generate + persist one (0600).
    pub fn load_or_create(path: &str) -> Result<Self> {
        match std::fs::read(path) {
            Ok(bytes) if bytes.len() == 32 => {
                let mut seed = [0u8; 32];
                seed.copy_from_slice(&bytes);
                Ok(Signer {
                    key: SigningKey::from_bytes(&seed),
                })
            }
            Ok(_) => Err(anyhow!(
                "identity key at {path} is malformed (expected 32 bytes)"
            )),
            Err(_) => {
                let signer = Signer::generate();
                std::fs::write(path, signer.key.to_bytes())?;
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
                }
                Ok(signer)
            }
        }
    }

    /// Lowercase-hex of the 32-byte public key — used as `pubkey_id`.
    pub fn public_hex(&self) -> String {
        hex::encode(self.key.verifying_key().to_bytes())
    }

    /// Sign a message (an entry hash), returning the 64-byte signature.
    pub fn sign(&self, msg: &[u8]) -> Vec<u8> {
        self.key.sign(msg).to_bytes().to_vec()
    }
}

/// Verify a signature against a hex-encoded public key.
pub fn verify_sig(pubkey_hex: &str, msg: &[u8], sig: &[u8]) -> bool {
    let pk = match hex::decode(pubkey_hex) {
        Ok(b) if b.len() == 32 => b,
        _ => return false,
    };
    let mut pkb = [0u8; 32];
    pkb.copy_from_slice(&pk);
    let vk = match VerifyingKey::from_bytes(&pkb) {
        Ok(v) => v,
        Err(_) => return false,
    };
    if sig.len() != 64 {
        return false;
    }
    let mut sb = [0u8; 64];
    sb.copy_from_slice(sig);
    let signature = Signature::from_bytes(&sb);
    vk.verify(msg, &signature).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sign_and_verify_roundtrip() {
        let s = Signer::generate();
        let msg = b"hello ledger";
        let sig = s.sign(msg);
        assert!(verify_sig(&s.public_hex(), msg, &sig));
        assert!(!verify_sig(&s.public_hex(), b"tampered", &sig));
    }
}
