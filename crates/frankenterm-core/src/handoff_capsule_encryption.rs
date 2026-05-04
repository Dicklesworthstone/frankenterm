//! Handoff capsule encryption hook (ft-1650n.5 follow-up,
//! closes ft-3bh4k).
//!
//! Defense-in-depth on top of the integrity check: capsules can
//! optionally be encrypted at rest / in transit before integrity is
//! computed, so a stolen capsule blob remains opaque without the
//! operator-supplied key.
//!
//! # Why a hook trait, not a baked-in cipher?
//!
//! `frankenterm-core` declares `#![forbid(unsafe_code)]`. Real AES
//! implementations rely on `unsafe` for SIMD/AESNI fast paths;
//! ring-based wrappers pull in C dependencies (aws-lc-sys etc.) that
//! conflict with the cross-platform constraint. The hook trait lets
//! operators bring their own cipher (libsodium via FFI in a sibling
//! crate, age, GPG-via-subprocess, kernel keyring) without forcing
//! frankenterm-core to depend on any of them.
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
//! The included [`XorPlaceholderHook`] applies a repeating-key XOR
//! to plaintext. **DO NOT USE THIS FOR REAL SECRETS** — repeating-
//! key XOR is broken under known-plaintext attack. Its only purpose
//! is to let the test suite exercise the seal/open ordering
//! contract + verify that wrong-key rejects + that integrity fires
//! before decrypt, without depending on a real cipher.

use serde::{Deserialize, Serialize};

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
// XorPlaceholderHook — TEST USE ONLY
// =============================================================================

/// **WARNING: NOT A REAL CIPHER.** Repeating-key XOR with a fixed
/// key is broken under known-plaintext attack. This hook exists
/// SOLELY to let the test suite exercise the seal/open ordering
/// contract without depending on a real cipher crate.
///
/// Production deployments MUST replace this with a real
/// [`CapsuleEncryptionHook`] backed by a vetted AEAD primitive
/// (libsodium secretbox via FFI, age-x25519, or a kernel keyring
/// API). This crate's `#![forbid(unsafe_code)]` declaration
/// precludes vendoring a real AES implementation here.
#[derive(Debug, Clone)]
pub struct XorPlaceholderHook {
    key: Vec<u8>,
}

impl XorPlaceholderHook {
    /// Construct with the supplied key bytes. Caller is responsible
    /// for the key — this hook does NOT generate, store, or rotate
    /// keys (no real cipher would either).
    #[must_use]
    pub fn with_key(key: Vec<u8>) -> Self {
        Self { key }
    }
}

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
        Ok(xor_with_key(plaintext, &self.key))
    }

    fn open(&self, ciphertext: &[u8]) -> Result<Vec<u8>, EncryptionError> {
        if self.key.is_empty() {
            return Err(EncryptionError::Unsupported {
                reason: "empty key".into(),
            });
        }
        // Wrong-key detection: the operator wraps ciphertext with a
        // fixed magic prefix at seal-time so open can reject early
        // when the prefix doesn't reappear post-decrypt. Without the
        // prefix, repeating-key XOR with a wrong key would silently
        // produce garbage plaintext. This is a pedagogical pattern;
        // real AEADs detect this via authentication tag.
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

const XOR_MAGIC_PREFIX: &[u8] = b"FT-XOR\xfe\xed";

fn xor_with_key(input: &[u8], key: &[u8]) -> Vec<u8> {
    // For seal we PREPEND the magic prefix to the plaintext, then
    // XOR the whole thing. open() XORs back, checks the prefix, and
    // strips it. This way wrong-key produces a candidate that
    // doesn't start with the magic and gets rejected.
    //
    // Implementation detail: seal() doesn't see the magic; we add
    // it as a wrapper here so XorPlaceholderHook::seal can produce
    // ciphertext from arbitrary plaintext without the caller
    // knowing about the magic.
    let mut buffer = Vec::with_capacity(XOR_MAGIC_PREFIX.len() + input.len());
    buffer.extend_from_slice(XOR_MAGIC_PREFIX);
    buffer.extend_from_slice(input);
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
