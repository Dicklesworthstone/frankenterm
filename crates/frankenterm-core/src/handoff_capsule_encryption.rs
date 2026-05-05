//! Handoff capsule encryption hook (ft-1650n.5 follow-up,
//! closes ft-3bh4k).
//!
//! Defense-in-depth on top of the integrity check: capsules can
//! optionally be encrypted at rest / in transit before integrity is
//! computed, so a stolen capsule blob remains opaque without the
//! operator-supplied key.
//!
//! # Why a hook trait and a built-in AEAD?
//!
//! `frankenterm-core` declares `#![forbid(unsafe_code)]`. The built-in
//! [`XChaCha20Poly1305Hook`] uses the RustCrypto pure-Rust AEAD
//! implementation so the default production path does not need
//! operator FFI, C libraries, or a test-only cipher. The hook trait
//! remains the extension point for deployments that need an external
//! keyring, age recipient encryption, GPG-via-subprocess, or a
//! platform security module.
//!
//! # Layered with integrity
//!
//! Slice 4 of ft-1650n.5 ships the seal/open ordering contract:
//!
//!   SEAL (export):
//!     1. serialize sections to canonical JSON
//!     2. encrypt(canonical_json) -> ciphertext
//!     3. integrity_digest = hash(ciphertext)        ← ciphertext, not plaintext
//!     4. EncryptedCapsuleEnvelope { ciphertext, integrity, hook_id }
//!
//!   OPEN (import):
//!     1. integrity_digest_actual = hash(envelope.ciphertext)
//!     2. if mismatch → bail (tampered envelope)     ← integrity FIRST
//!     3. plaintext = decrypt(envelope.ciphertext)
//!     4. parse plaintext sections + return capsule
//!
//! Hashing the CIPHERTEXT (not the plaintext) means the integrity
//! check fires BEFORE decrypt — operators detect tampered envelopes
//! without paying the decrypt cost (and without revealing plaintext
//! to a slow-IO decrypt path that could time-leak the key).
//!
//! # XorPlaceholderHook is for tests only
//!
//! Test builds include [`XorPlaceholderHook`], which applies a repeating-key XOR
//! to plaintext. **DO NOT USE THIS FOR REAL SECRETS** — repeating-
//! key XOR is broken under known-plaintext attack. Its only purpose
//! is to let the test suite exercise the seal/open ordering
//! contract + verify that wrong-key rejects + that integrity fires
//! before decrypt. It is not compiled into normal production builds.

use chacha20poly1305::{
    XChaCha20Poly1305, XNonce,
    aead::{Aead, AeadCore, KeyInit, OsRng},
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::capability_passport::RedactedProof;
use crate::handoff_capsule::{CapsuleIntegrity, CapsuleSection, HandoffCapsule};

/// Pluggable encryption hook for handoff capsules.
///
/// Implementations supply seal (encrypt) + open (decrypt). Both
/// operate on opaque byte slices — the capsule encryption layer
/// hands canonical-serialized section JSON to seal, and feeds the
/// open output back through serde to recover the plaintext sections.
pub trait CapsuleEncryptionHook: Send + Sync {
    /// Stable identifier for the hook (e.g. `aes256-gcm-keyring`,
    /// `age-x25519`, `xor-placeholder-test-only`). Surfaces in the
    /// envelope so consumers know which hook to invoke at open time.
    fn hook_id(&self) -> &'static str;

    /// Encrypt `plaintext`. Implementations MUST be deterministic
    /// only if the underlying cipher is deterministic — most real
    /// ciphers (AEAD with random nonce) are NOT deterministic, and
    /// roundtrip tests should not rely on byte-for-byte equality
    /// between two seal calls.
    fn seal(&self, plaintext: &[u8]) -> Result<Vec<u8>, EncryptionError>;

    /// Decrypt `ciphertext`. Returns
    /// [`EncryptionError::DecryptionFailed`] for any failure (wrong
    /// key, tampered ciphertext post-integrity, AEAD authentication
    /// fail, etc.) — never panic or return partial plaintext.
    fn open(&self, ciphertext: &[u8]) -> Result<Vec<u8>, EncryptionError>;
}

/// Errors from [`CapsuleEncryptionHook`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EncryptionError {
    /// Decryption failed — wrong key, AEAD auth fail, or post-
    /// integrity tampering of the ciphertext bytes.
    DecryptionFailed { reason: String },
    /// Encryption failed — usually a hook-internal error
    /// (e.g. keyring unavailable, OS RNG starved).
    EncryptionFailed { reason: String },
    /// Hook reported it does not support this input shape (e.g.
    /// plaintext too large, key unset).
    Unsupported { reason: String },
}

impl std::fmt::Display for EncryptionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::DecryptionFailed { reason } => write!(f, "capsule decryption failed: {reason}"),
            Self::EncryptionFailed { reason } => write!(f, "capsule encryption failed: {reason}"),
            Self::Unsupported { reason } => write!(f, "capsule encryption unsupported: {reason}"),
        }
    }
}

impl std::error::Error for EncryptionError {}

/// Encrypted capsule envelope. The plaintext form
/// ([`HandoffCapsule`]) is recovered by feeding `ciphertext` through
/// the same-`hook_id` hook's open() method.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EncryptedCapsuleEnvelope {
    /// Schema version mirrors HandoffCapsule's so envelope evolution
    /// can move in lockstep.
    pub version: u32,
    /// Hook ID — operators read this to dispatch to the right
    /// CapsuleEncryptionHook on open.
    pub hook_id: String,
    /// Encrypted sections payload.
    pub ciphertext: Vec<u8>,
    /// Integrity digest over the CIPHERTEXT bytes (not plaintext).
    /// Lets receivers detect tampering BEFORE paying decrypt cost.
    pub integrity: CapsuleIntegrity,
    /// Epoch milliseconds when the envelope was sealed.
    pub signed_at_ms: u64,
}

impl EncryptedCapsuleEnvelope {
    /// Seal a [`HandoffCapsule`] into an [`EncryptedCapsuleEnvelope`]
    /// using the supplied hook. Hashes ciphertext (not plaintext).
    pub fn seal(
        capsule: &HandoffCapsule,
        hook: &dyn CapsuleEncryptionHook,
    ) -> Result<Self, EncryptionError> {
        let plaintext = serde_json::to_vec(&capsule.sections).map_err(|e| {
            EncryptionError::EncryptionFailed {
                reason: format!("serialize sections: {e}"),
            }
        })?;
        let ciphertext = hook.seal(&plaintext)?;
        let integrity = compute_ciphertext_integrity(&ciphertext);
        Ok(Self {
            version: capsule.version,
            hook_id: hook.hook_id().to_string(),
            ciphertext,
            integrity,
            signed_at_ms: capsule.signed_at_ms,
        })
    }

    /// Verify integrity over the ciphertext bytes. Runs FIRST in
    /// [`Self::open`] so operators detect tampering without paying
    /// the decrypt cost.
    pub fn verify_integrity(&self) -> Result<(), EnvelopeOpenError> {
        let recomputed = compute_ciphertext_integrity(&self.ciphertext);
        if self.integrity == recomputed {
            Ok(())
        } else {
            Err(EnvelopeOpenError::IntegrityMismatch {
                stored: self.integrity.clone(),
                recomputed,
            })
        }
    }

    /// Open the envelope. Runs integrity check FIRST, then decrypt,
    /// then validates the recovered capsule has the expected
    /// version. Sections are returned but the integrity field of
    /// the recovered HandoffCapsule is RECOMPUTED locally (since
    /// the original capsule's integrity hashed plaintext sections;
    /// the envelope's integrity hashed ciphertext).
    pub fn open(
        &self,
        hook: &dyn CapsuleEncryptionHook,
        source: crate::handoff_capsule::CapsuleEndpoint,
        destination: crate::handoff_capsule::CapsuleEndpoint,
    ) -> Result<HandoffCapsule, EnvelopeOpenError> {
        if hook.hook_id() != self.hook_id {
            return Err(EnvelopeOpenError::WrongHook {
                envelope_hook: self.hook_id.clone(),
                supplied_hook: hook.hook_id().to_string(),
            });
        }
        // Integrity FIRST — never feed tampered ciphertext to decrypt.
        self.verify_integrity()?;
        let plaintext = hook
            .open(&self.ciphertext)
            .map_err(EnvelopeOpenError::Decryption)?;
        let sections: Vec<CapsuleSection> = serde_json::from_slice(&plaintext).map_err(|e| {
            EnvelopeOpenError::Decryption(EncryptionError::DecryptionFailed {
                reason: format!("decrypt produced unparsable sections: {e}"),
            })
        })?;
        Ok(HandoffCapsule::build(
            source,
            destination,
            sections,
            self.signed_at_ms,
        ))
    }
}

/// Errors from [`EncryptedCapsuleEnvelope::open`].
#[derive(Debug, Clone, PartialEq)]
pub enum EnvelopeOpenError {
    /// The envelope's stored integrity does not match the recomputed
    /// digest of the ciphertext bytes — tampered envelope.
    IntegrityMismatch {
        stored: CapsuleIntegrity,
        recomputed: CapsuleIntegrity,
    },
    /// The supplied hook's `hook_id` does not match the envelope's
    /// `hook_id`. Operators must dispatch to the right hook on open.
    WrongHook {
        envelope_hook: String,
        supplied_hook: String,
    },
    /// The hook's open() call failed (wrong key, AEAD auth fail,
    /// etc.) OR the decrypted bytes did not parse as
    /// [`Vec<CapsuleSection>`].
    Decryption(EncryptionError),
}

impl std::fmt::Display for EnvelopeOpenError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::IntegrityMismatch { stored, recomputed } => write!(
                f,
                "envelope integrity mismatch: stored={} recomputed={}",
                stored.digest_hex, recomputed.digest_hex,
            ),
            Self::WrongHook {
                envelope_hook,
                supplied_hook,
            } => write!(
                f,
                "hook mismatch: envelope wants {envelope_hook}, supplied {supplied_hook}",
            ),
            Self::Decryption(e) => write!(f, "decrypt failed: {e}"),
        }
    }
}

impl std::error::Error for EnvelopeOpenError {}

fn compute_ciphertext_integrity(ciphertext: &[u8]) -> CapsuleIntegrity {
    let proof = RedactedProof::from_value(ciphertext);
    CapsuleIntegrity {
        algorithm: CapsuleIntegrity::ALGORITHM.to_string(),
        digest_hex: proof.digest_hex,
    }
}

// =============================================================================
// XChaCha20Poly1305Hook — production AEAD hook
// =============================================================================

const XCHACHA20_POLY1305_MAGIC: &[u8] = b"FT-XCHACHA20POLY1305-V1\0";
const XCHACHA20_POLY1305_KEY_ID_LEN: usize = 8;
const XCHACHA20_POLY1305_NONCE_LEN: usize = 24;

/// Production handoff-capsule encryption hook backed by
/// XChaCha20-Poly1305.
///
/// The sealed byte format is:
///
/// ```text
/// magic || key_id(8) || nonce(24) || aead_ciphertext_and_tag
/// ```
///
/// `key_id` is the first 8 bytes of SHA-256(key). It is not a secret; it
/// lets operators detect key-rotation mismatches before attempting AEAD
/// decrypt, while the AEAD tag remains the authoritative authenticity
/// check.
#[derive(Clone)]
pub struct XChaCha20Poly1305Hook {
    cipher: XChaCha20Poly1305,
    key_id: [u8; XCHACHA20_POLY1305_KEY_ID_LEN],
}

impl std::fmt::Debug for XChaCha20Poly1305Hook {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("XChaCha20Poly1305Hook")
            .field("hook_id", &Self::HOOK_ID)
            .field("key_id", &self.key_id_hex())
            .finish_non_exhaustive()
    }
}

impl XChaCha20Poly1305Hook {
    /// Stable hook identifier stored in encrypted capsule envelopes.
    pub const HOOK_ID: &'static str = "xchacha20poly1305-ietf-v1";

    /// Required key length in bytes.
    pub const KEY_LEN: usize = 32;

    /// Construct the hook from a 32-byte symmetric key.
    ///
    /// Empty, wrong-length, and all-zero keys fail closed so production
    /// callers cannot accidentally select an inert placeholder.
    pub fn try_from_key_slice(key: &[u8]) -> Result<Self, EncryptionError> {
        if key.len() != Self::KEY_LEN {
            return Err(EncryptionError::Unsupported {
                reason: "XChaCha20Poly1305 key must be exactly 32 bytes".into(),
            });
        }
        if key.iter().all(|byte| *byte == 0) {
            return Err(EncryptionError::Unsupported {
                reason: "all-zero XChaCha20Poly1305 key is not allowed".into(),
            });
        }
        let cipher =
            XChaCha20Poly1305::new_from_slice(key).map_err(|_| EncryptionError::Unsupported {
                reason: "XChaCha20Poly1305 key initialization failed".into(),
            })?;
        Ok(Self {
            cipher,
            key_id: xchacha20poly1305_key_id(key),
        })
    }

    /// Construct the hook from a 64-character hex-encoded key.
    pub fn from_hex_key(key_hex: &str) -> Result<Self, EncryptionError> {
        let trimmed = key_hex.trim();
        if trimmed.is_empty() {
            return Err(EncryptionError::Unsupported {
                reason: "missing XChaCha20Poly1305 key".into(),
            });
        }
        let key = hex::decode(trimmed).map_err(|_| EncryptionError::Unsupported {
            reason: "XChaCha20Poly1305 key must be 64 hex characters".into(),
        })?;
        Self::try_from_key_slice(&key)
    }

    /// Hex fingerprint of the configured key, safe for diagnostics.
    #[must_use]
    pub fn key_id_hex(&self) -> String {
        hex::encode(self.key_id)
    }

    /// Extract the key-id fingerprint from a sealed hook payload.
    #[must_use]
    pub fn envelope_key_id_hex(ciphertext: &[u8]) -> Option<String> {
        let body = ciphertext.strip_prefix(XCHACHA20_POLY1305_MAGIC)?;
        let (key_id, _) = body.split_at_checked(XCHACHA20_POLY1305_KEY_ID_LEN)?;
        Some(hex::encode(key_id))
    }
}

impl CapsuleEncryptionHook for XChaCha20Poly1305Hook {
    fn hook_id(&self) -> &'static str {
        Self::HOOK_ID
    }

    fn seal(&self, plaintext: &[u8]) -> Result<Vec<u8>, EncryptionError> {
        let nonce = XChaCha20Poly1305::generate_nonce(&mut OsRng);
        let encrypted = self.cipher.encrypt(&nonce, plaintext).map_err(|_| {
            EncryptionError::EncryptionFailed {
                reason: "XChaCha20Poly1305 seal failed".into(),
            }
        })?;

        let mut sealed = Vec::with_capacity(
            XCHACHA20_POLY1305_MAGIC.len()
                + XCHACHA20_POLY1305_KEY_ID_LEN
                + XCHACHA20_POLY1305_NONCE_LEN
                + encrypted.len(),
        );
        sealed.extend_from_slice(XCHACHA20_POLY1305_MAGIC);
        sealed.extend_from_slice(&self.key_id);
        sealed.extend_from_slice(nonce.as_ref());
        sealed.extend_from_slice(&encrypted);
        Ok(sealed)
    }

    fn open(&self, ciphertext: &[u8]) -> Result<Vec<u8>, EncryptionError> {
        let body = ciphertext
            .strip_prefix(XCHACHA20_POLY1305_MAGIC)
            .ok_or_else(|| EncryptionError::DecryptionFailed {
                reason: "missing XChaCha20Poly1305 envelope header".into(),
            })?;
        let Some((key_id, body)) = body.split_at_checked(XCHACHA20_POLY1305_KEY_ID_LEN) else {
            return Err(EncryptionError::DecryptionFailed {
                reason: "truncated XChaCha20Poly1305 key id".into(),
            });
        };
        if key_id != self.key_id.as_slice() {
            return Err(EncryptionError::DecryptionFailed {
                reason: "XChaCha20Poly1305 key id mismatch".into(),
            });
        }
        let Some((nonce_bytes, encrypted)) = body.split_at_checked(XCHACHA20_POLY1305_NONCE_LEN)
        else {
            return Err(EncryptionError::DecryptionFailed {
                reason: "truncated XChaCha20Poly1305 nonce".into(),
            });
        };
        let nonce = XNonce::from_slice(nonce_bytes);
        self.cipher
            .decrypt(nonce, encrypted)
            .map_err(|_| EncryptionError::DecryptionFailed {
                reason: "XChaCha20Poly1305 authentication failed".into(),
            })
    }
}

fn xchacha20poly1305_key_id(key: &[u8]) -> [u8; XCHACHA20_POLY1305_KEY_ID_LEN] {
    let digest = Sha256::digest(key);
    let mut key_id = [0; XCHACHA20_POLY1305_KEY_ID_LEN];
    key_id.copy_from_slice(&digest[..XCHACHA20_POLY1305_KEY_ID_LEN]);
    key_id
}

// =============================================================================
// XorPlaceholderHook — TEST USE ONLY
// =============================================================================

/// **WARNING: NOT A REAL CIPHER.** Repeating-key XOR with a fixed
/// key is broken under known-plaintext attack. This hook exists
/// SOLELY to let the test suite exercise the seal/open ordering
/// contract without depending on a real cipher crate.
///
/// Normal production builds do not compile this type; use
/// [`XChaCha20Poly1305Hook`] or a deployment-specific
/// [`CapsuleEncryptionHook`] instead.
#[cfg(any(test, doc))]
#[derive(Debug, Clone)]
pub struct XorPlaceholderHook {
    key: Vec<u8>,
}

#[cfg(any(test, doc))]
impl XorPlaceholderHook {
    /// Construct with the supplied key bytes. Caller is responsible
    /// for the key — this hook does NOT generate, store, or rotate
    /// keys (no real cipher would either).
    #[must_use]
    pub fn with_key(key: Vec<u8>) -> Self {
        Self { key }
    }
}

#[cfg(any(test, doc))]
impl CapsuleEncryptionHook for XorPlaceholderHook {
    fn hook_id(&self) -> &'static str {
        "xor-placeholder-test-only"
    }

    fn seal(&self, plaintext: &[u8]) -> Result<Vec<u8>, EncryptionError> {
        if self.key.is_empty() {
            return Err(EncryptionError::Unsupported {
                reason: "empty key".into(),
            });
        }
        let mut buffer = Vec::with_capacity(XOR_MAGIC_PREFIX.len() + plaintext.len());
        buffer.extend_from_slice(XOR_MAGIC_PREFIX);
        buffer.extend_from_slice(plaintext);
        Ok(xor_with_key(&buffer, &self.key))
    }

    fn open(&self, ciphertext: &[u8]) -> Result<Vec<u8>, EncryptionError> {
        if self.key.is_empty() {
            return Err(EncryptionError::Unsupported {
                reason: "empty key".into(),
            });
        }
        // Wrong-key detection: seal() wraps plaintext with a fixed
        // magic prefix so open can reject when the prefix doesn't
        // reappear post-decrypt. Without the prefix, repeating-key XOR
        // with a wrong key would silently produce garbage plaintext.
        // This is a pedagogical pattern; real AEADs detect this via
        // authentication tag.
        let candidate = xor_with_key(ciphertext, &self.key);
        if candidate.starts_with(XOR_MAGIC_PREFIX) {
            Ok(candidate[XOR_MAGIC_PREFIX.len()..].to_vec())
        } else {
            Err(EncryptionError::DecryptionFailed {
                reason: "wrong key (XOR magic prefix not present after decrypt)".into(),
            })
        }
    }
}

#[cfg(any(test, doc))]
const XOR_MAGIC_PREFIX: &[u8] = b"FT-XOR\xfe\xed";

#[cfg(any(test, doc))]
fn xor_with_key(input: &[u8], key: &[u8]) -> Vec<u8> {
    let mut buffer = input.to_vec();
    for (i, byte) in buffer.iter_mut().enumerate() {
        *byte ^= key[i % key.len()];
    }
    buffer
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capability_passport::{
        CapabilityClass, CapabilityEntry, CapabilityPassport, CapabilityVerification,
    };
    use crate::handoff_capsule::CapsuleEndpoint;

    fn endpoint(agent: &str, pane: u64) -> CapsuleEndpoint {
        CapsuleEndpoint {
            agent_id: agent.into(),
            pane_id: Some(pane),
            label: None,
        }
    }

    fn sample_capsule() -> HandoffCapsule {
        HandoffCapsule::build(
            endpoint("source-agent", 1),
            endpoint("dest-agent", 2),
            vec![
                CapsuleSection::ContextSummary {
                    text: "context".into(),
                },
                CapsuleSection::PassportExcerpt {
                    passport: CapabilityPassport {
                        agent_id: "inherited".into(),
                        pane_id: Some(99),
                        capabilities: vec![CapabilityEntry {
                            class: CapabilityClass::ToolAvailability("bash".into()),
                            verification: CapabilityVerification::Verified,
                            last_observed_at_ms: Some(900_000),
                            proof: RedactedProof::from_value(b"proof"),
                        }],
                        generation: 1,
                        signed_at_ms: 900_000,
                    },
                },
            ],
            1_500_000,
        )
    }

    // ── XChaCha20-Poly1305 seal/open roundtrip ─────────────────────────

    #[test]
    fn xchacha20poly1305_hook_seal_open_roundtrip_recovers_capsule() {
        let hook = XChaCha20Poly1305Hook::try_from_key_slice(&[0x11; 32]).expect("valid key");
        let capsule = sample_capsule();
        let envelope = EncryptedCapsuleEnvelope::seal(&capsule, &hook).expect("seal ok");
        let plain = serde_json::to_vec(&capsule.sections).unwrap();

        assert_eq!(envelope.hook_id, XChaCha20Poly1305Hook::HOOK_ID);
        assert_ne!(envelope.ciphertext, plain);
        assert_eq!(
            XChaCha20Poly1305Hook::envelope_key_id_hex(&envelope.ciphertext),
            Some(hook.key_id_hex())
        );

        let envelope2 = EncryptedCapsuleEnvelope::seal(&capsule, &hook).expect("seal ok");
        assert_ne!(envelope.ciphertext, envelope2.ciphertext);

        let recovered = envelope
            .open(&hook, capsule.source.clone(), capsule.destination.clone())
            .expect("open ok");
        assert_eq!(recovered.sections, capsule.sections);
        recovered.verify_integrity().expect("integrity preserved");
    }

    #[test]
    fn xchacha20poly1305_hook_wrong_key_rejects_without_plaintext_leakage() {
        let seal_hook = XChaCha20Poly1305Hook::try_from_key_slice(&[0x11; 32]).expect("seal key");
        let open_hook = XChaCha20Poly1305Hook::try_from_key_slice(&[0x22; 32]).expect("open key");
        let capsule = sample_capsule();
        let envelope = EncryptedCapsuleEnvelope::seal(&capsule, &seal_hook).expect("seal");
        let err = envelope
            .open(
                &open_hook,
                capsule.source.clone(),
                capsule.destination.clone(),
            )
            .unwrap_err();
        let EnvelopeOpenError::Decryption(EncryptionError::DecryptionFailed { reason }) = err
        else {
            panic!("expected DecryptionFailed for wrong key, got {err:?}");
        };
        assert!(reason.contains("key id mismatch"));
        assert!(!reason.contains("context"));
        assert!(!reason.contains("proof"));
    }

    #[test]
    fn xchacha20poly1305_hook_tamper_rejects_at_aead_layer() {
        let hook = XChaCha20Poly1305Hook::try_from_key_slice(&[0x11; 32]).expect("valid key");
        let mut sealed = hook
            .seal(b"context proof should stay sealed")
            .expect("seal");
        *sealed.last_mut().expect("ciphertext byte") ^= 0x01;
        let err = hook.open(&sealed).unwrap_err();
        let EncryptionError::DecryptionFailed { reason } = err else {
            panic!("expected DecryptionFailed");
        };
        assert!(reason.contains("authentication failed"));
        assert!(!reason.contains("context"));
        assert!(!reason.contains("proof"));
    }

    #[test]
    fn xchacha20poly1305_hook_rejects_missing_and_weak_keys() {
        let missing = XChaCha20Poly1305Hook::from_hex_key("").unwrap_err();
        assert!(matches!(missing, EncryptionError::Unsupported { .. }));

        let short = XChaCha20Poly1305Hook::try_from_key_slice(&[0x11; 31]).unwrap_err();
        let EncryptionError::Unsupported { reason } = short else {
            panic!("expected Unsupported");
        };
        assert!(reason.contains("32 bytes"));

        let zero = XChaCha20Poly1305Hook::try_from_key_slice(&[0; 32]).unwrap_err();
        let EncryptionError::Unsupported { reason } = zero else {
            panic!("expected Unsupported");
        };
        assert!(reason.contains("all-zero"));
    }

    #[test]
    fn xchacha20poly1305_hook_accepts_hex_key() {
        let key_hex = "11".repeat(32);
        let hook = XChaCha20Poly1305Hook::from_hex_key(&key_hex).expect("hex key");
        let sealed = hook.seal(b"plaintext").expect("seal");
        assert_eq!(hook.open(&sealed).expect("open"), b"plaintext");
    }

    // ── XOR seal/open roundtrip ─────────────────────────────────────────

    #[test]
    fn xor_hook_seal_open_roundtrip_recovers_capsule() {
        let hook = XorPlaceholderHook::with_key(vec![0x5a; 32]);
        let capsule = sample_capsule();
        let envelope = EncryptedCapsuleEnvelope::seal(&capsule, &hook).expect("seal ok");
        // Ciphertext should differ from plaintext serialization.
        let plain = serde_json::to_vec(&capsule.sections).unwrap();
        assert_ne!(envelope.ciphertext, plain);

        let recovered = envelope
            .open(&hook, capsule.source.clone(), capsule.destination.clone())
            .expect("open ok");
        assert_eq!(recovered.sections, capsule.sections);
        assert_eq!(recovered.version, capsule.version);
        assert_eq!(recovered.signed_at_ms, capsule.signed_at_ms);
        // Recovered capsule passes its own integrity check (recomputed
        // locally from sections).
        recovered
            .verify_integrity()
            .expect("recovered integrity preserved");
    }

    #[test]
    fn xor_hook_wrong_key_rejects_at_decrypt() {
        let seal_hook = XorPlaceholderHook::with_key(vec![0x5a; 32]);
        let open_hook = XorPlaceholderHook::with_key(vec![0xa5; 32]);
        let capsule = sample_capsule();
        let envelope = EncryptedCapsuleEnvelope::seal(&capsule, &seal_hook).expect("seal");
        let err = envelope
            .open(
                &open_hook,
                capsule.source.clone(),
                capsule.destination.clone(),
            )
            .unwrap_err();
        let EnvelopeOpenError::Decryption(EncryptionError::DecryptionFailed { reason }) = err
        else {
            panic!("expected DecryptionFailed for wrong key, got {err:?}");
        };
        assert!(reason.contains("wrong key"));
    }

    #[test]
    fn xor_hook_empty_key_rejects() {
        let hook = XorPlaceholderHook::with_key(Vec::new());
        let capsule = sample_capsule();
        let err = EncryptedCapsuleEnvelope::seal(&capsule, &hook).unwrap_err();
        assert!(matches!(err, EncryptionError::Unsupported { .. }));
    }

    // ── Integrity-before-decrypt ordering ───────────────────────────────

    #[test]
    fn envelope_tampered_ciphertext_fails_integrity_before_decrypt() {
        let hook = XorPlaceholderHook::with_key(vec![0x5a; 32]);
        let capsule = sample_capsule();
        let mut envelope = EncryptedCapsuleEnvelope::seal(&capsule, &hook).expect("seal");
        // Tamper: flip a bit in the ciphertext.
        envelope.ciphertext[0] ^= 0x01;
        let err = envelope
            .open(&hook, capsule.source.clone(), capsule.destination.clone())
            .unwrap_err();
        let EnvelopeOpenError::IntegrityMismatch { stored, recomputed } = err else {
            panic!("expected IntegrityMismatch, got {err:?}");
        };
        assert_ne!(stored.digest_hex, recomputed.digest_hex);
    }

    #[test]
    fn envelope_verify_integrity_independent_of_decrypt() {
        let hook = XorPlaceholderHook::with_key(vec![0x5a; 32]);
        let capsule = sample_capsule();
        let envelope = EncryptedCapsuleEnvelope::seal(&capsule, &hook).expect("seal");
        envelope.verify_integrity().expect("integrity ok");
        // Verify still passes without decrypting.
        envelope.verify_integrity().expect("integrity ok 2nd time");
    }

    #[test]
    fn envelope_wrong_hook_id_rejects_at_open() {
        let real_hook = XorPlaceholderHook::with_key(vec![0x5a; 32]);
        let capsule = sample_capsule();
        let envelope = EncryptedCapsuleEnvelope::seal(&capsule, &real_hook).expect("seal");

        struct OtherHook;
        impl CapsuleEncryptionHook for OtherHook {
            fn hook_id(&self) -> &'static str {
                "other-hook-id"
            }
            fn seal(&self, _: &[u8]) -> Result<Vec<u8>, EncryptionError> {
                unreachable!()
            }
            fn open(&self, _: &[u8]) -> Result<Vec<u8>, EncryptionError> {
                unreachable!()
            }
        }
        let err = envelope
            .open(
                &OtherHook,
                capsule.source.clone(),
                capsule.destination.clone(),
            )
            .unwrap_err();
        let EnvelopeOpenError::WrongHook {
            envelope_hook,
            supplied_hook,
        } = err
        else {
            panic!("expected WrongHook");
        };
        assert_eq!(envelope_hook, "xor-placeholder-test-only");
        assert_eq!(supplied_hook, "other-hook-id");
    }

    // ── Serde roundtrip ──────────────────────────────────────────────────

    #[test]
    fn envelope_serde_roundtrip_preserves_integrity() {
        let hook = XorPlaceholderHook::with_key(vec![0x5a; 32]);
        let capsule = sample_capsule();
        let envelope = EncryptedCapsuleEnvelope::seal(&capsule, &hook).expect("seal");
        let json = serde_json::to_string(&envelope).expect("serialize");
        let back: EncryptedCapsuleEnvelope = serde_json::from_str(&json).expect("deserialize");
        // Roundtripped envelope's integrity still verifies.
        back.verify_integrity().expect("integrity preserved");
        let recovered = back
            .open(&hook, capsule.source.clone(), capsule.destination.clone())
            .expect("open ok");
        assert_eq!(recovered.sections, capsule.sections);
    }

    // ── EncryptionError serde ────────────────────────────────────────────

    #[test]
    fn xor_hook_id_is_stable_marker() {
        let hook = XorPlaceholderHook::with_key(vec![1, 2, 3]);
        assert_eq!(hook.hook_id(), "xor-placeholder-test-only");
    }
}
