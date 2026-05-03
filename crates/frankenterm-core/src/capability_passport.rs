//! Capability passport schema (ft-1650n.1, substrate slice).
//!
//! A machine-checkable record of what each pane / agent / session is
//! permitted to do — model family, tool availability, auth surfaces,
//! filesystem scope, network policy, expected runtime wrapper, cargo
//! target dir policy, and known safety constraints.
//!
//! Each capability carries a `verification` state (Declared / Observed /
//! Verified / Unknown) so operators can distinguish "the agent claimed
//! it" from "we observed it in pane output" from "a bounded probe
//! confirmed it." Sensitive proofs are stored as `RedactedProof`
//! (a stable hash of the underlying value) so the passport itself can
//! be persisted, transmitted, and logged without leaking secrets.
//!
//! # Scope of this slice
//!
//! Substrate-pass: this file ships the passport schema + serde + a
//! redaction primitive. Preflight probes (bead acceptance §2), robot
//! /doctor output (§3), and the E2E dispatch fixture (§5) are tracked
//! as follow-up beads — see ft-1650n.1's disposition notes.
//!
//! # Forward-compat
//!
//! All structs use `#[serde(default)]` on every optional field so older
//! serialized passports remain deserializable as the schema evolves.
//! Capability classes are defined as a closed enum with a catch-all
//! `Other(String)` variant so packs can carry vendor-specific
//! capabilities without a schema bump.
//!
//! # Trust boundary
//!
//! Passport structs intentionally keep their fields public because they
//! are wire-format and fixture types. Public construction does not make
//! a passport trusted: dispatch decisions must use methods such as
//! [`CapabilityPassport::has_verified`], and stores or durable importers
//! that accept fresh passports must run
//! [`crate::capability_passport_store::PassportValidator`] before
//! replacing an existing trusted value. Tests in this module and in
//! `capability_passport_store` pin that fail-closed boundary for forged
//! direct construction.

use serde::{Deserialize, Serialize};

/// Length (in characters) of the redacted-proof hex digest we surface
/// in operator-facing output. The full 64-char hex digest of the stable
/// hash is preserved in the underlying `RedactedProof` value; this
/// constant only governs the truncated display form.
pub const REDACTED_PROOF_DISPLAY_LEN: usize = 12;

/// Closed enum of capability classes with a vendor-extensible escape
/// hatch. The named variants cover the eight constraint families called
/// out in the bead; `Other` lets pattern packs / pack vendors carry
/// capabilities not yet in the canonical set without a schema bump.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", content = "name", rename_all = "snake_case")]
pub enum CapabilityClass {
    /// Model family (e.g. "claude-opus-4-7", "gpt-5", "gemini-2.5").
    ModelFamily(String),
    /// Tool availability (e.g. "bash", "edit", "write", "agent").
    ToolAvailability(String),
    /// Auth surface (e.g. "anthropic_api", "openai_api", "github_pat").
    AuthSurface(String),
    /// Filesystem scope (e.g. "/Users/jemanuel/projects/frankenterm").
    FilesystemScope(String),
    /// Network policy (e.g. "internet_egress_allowed",
    /// "loopback_only", "deny_all").
    NetworkPolicy(String),
    /// Expected runtime wrapper (e.g. "claude_code_cli", "codex_cli",
    /// "gemini_cli").
    RuntimeWrapper(String),
    /// Cargo target dir policy (e.g. "/tmp/ft-cc1-target").
    CargoTargetDirPolicy(String),
    /// Known safety constraint (e.g. "forbid_unsafe_code",
    /// "no_destructive_git", "no_force_push_main").
    SafetyConstraint(String),
    /// Vendor-specific or experimental capability.
    Other(String),
}

impl CapabilityClass {
    /// Stable name for telemetry / display: returns the class kind plus
    /// the inner value, e.g. `model_family:claude-opus-4-7`.
    #[must_use]
    pub fn label(&self) -> String {
        match self {
            Self::ModelFamily(v) => format!("model_family:{v}"),
            Self::ToolAvailability(v) => format!("tool:{v}"),
            Self::AuthSurface(v) => format!("auth:{v}"),
            Self::FilesystemScope(v) => format!("fs:{v}"),
            Self::NetworkPolicy(v) => format!("net:{v}"),
            Self::RuntimeWrapper(v) => format!("runtime:{v}"),
            Self::CargoTargetDirPolicy(v) => format!("cargo_target:{v}"),
            Self::SafetyConstraint(v) => format!("safety:{v}"),
            Self::Other(v) => format!("other:{v}"),
        }
    }
}

/// How a capability was established. Operators MUST be able to
/// distinguish these — a Declared capability is no stronger than the
/// agent's word; an Observed one is inferred from pane output (could
/// be coincidence); a Verified one passed a bounded probe; Unknown is
/// the fail-closed default when a probe times out or errors.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapabilityVerification {
    /// Agent / operator declared the capability (lowest confidence).
    Declared,
    /// Inferred from pane output, title, cwd, or other side-channel.
    Observed,
    /// A bounded read-only probe confirmed the capability.
    Verified,
    /// Probe failed, timed out, or produced an indeterminate result.
    /// Fail-closed default — a downstream dispatcher MUST treat this
    /// as "do not assume this capability is present."
    Unknown,
}

impl CapabilityVerification {
    /// Operator-facing confidence label (`high` / `medium` / `low` /
    /// `none`). Used by robot/doctor output rendering.
    #[must_use]
    pub fn confidence_label(self) -> &'static str {
        match self {
            Self::Verified => "high",
            Self::Observed => "medium",
            Self::Declared => "low",
            Self::Unknown => "none",
        }
    }

    /// True when downstream dispatchers may rely on the capability.
    /// Verified is the only state that returns true — all others are
    /// fail-closed-conservative.
    #[must_use]
    pub fn is_dispatchable(self) -> bool {
        matches!(self, Self::Verified)
    }
}

impl Default for CapabilityVerification {
    fn default() -> Self {
        Self::Unknown
    }
}

/// A redacted proof carrying a stable hash of an underlying sensitive
/// value (e.g. a credential, a probe response containing a secret).
/// The hash is deterministic so identical proofs across passports can
/// be correlated, but the underlying value is never serialized.
///
/// Use [`RedactedProof::from_value`] to construct from a raw secret;
/// the secret bytes are immediately hashed and discarded. The
/// resulting struct can be safely serialized, logged, and persisted.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
pub struct RedactedProof {
    /// Hex digest of the FNV-1a + CRC32 dual hash of the underlying
    /// proof bytes. Reuses the 96-bit collision domain established in
    /// `cell_consistency_crc` (FNV-1a 2^-64 + CRC32 2^-32).
    pub digest_hex: String,
    /// Optional length of the underlying value in bytes — exposed so
    /// operators can distinguish "empty proof" from "redacted proof of
    /// some specific length" without leaking the value itself.
    #[serde(default)]
    pub byte_len: Option<u64>,
}

impl RedactedProof {
    /// Construct from raw proof bytes. The bytes are immediately
    /// hashed and dropped on return — only the digest survives.
    #[must_use]
    pub fn from_value(value: &[u8]) -> Self {
        let fnv = crate::cell_consistency_crc::fnv1a_64(value);
        let crc = crate::cell_consistency_crc::crc32_ieee(value);
        Self {
            digest_hex: format!("{fnv:016x}{crc:08x}"),
            byte_len: Some(value.len() as u64),
        }
    }

    /// Construct an empty / unknown proof. Used when a capability is
    /// declared without underlying evidence yet.
    #[must_use]
    pub fn empty() -> Self {
        Self {
            digest_hex: String::new(),
            byte_len: None,
        }
    }

    /// Operator-facing truncated digest (first
    /// [`REDACTED_PROOF_DISPLAY_LEN`] hex chars). Sufficient for
    /// human pattern-matching across recent log lines without
    /// exposing the full hash that an attacker could brute-force
    /// against a known short input.
    #[must_use]
    pub fn display_truncated(&self) -> String {
        if self.digest_hex.len() <= REDACTED_PROOF_DISPLAY_LEN {
            self.digest_hex.clone()
        } else {
            format!("{}…", &self.digest_hex[..REDACTED_PROOF_DISPLAY_LEN])
        }
    }
}

/// A single capability claim within a passport.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CapabilityEntry {
    pub class: CapabilityClass,
    #[serde(default)]
    pub verification: CapabilityVerification,
    /// Epoch milliseconds when this capability was last observed /
    /// verified. `None` when only declared.
    #[serde(default)]
    pub last_observed_at_ms: Option<u64>,
    /// Optional redacted proof — the digest of any underlying
    /// sensitive value (probe response, credential check echo).
    #[serde(default)]
    pub proof: RedactedProof,
}

impl CapabilityEntry {
    /// True when the capability has been observed / verified within
    /// `max_age_ms` of `now_ms`. Stale capabilities (or those never
    /// observed) return false — this is the freshness gate consumed
    /// by robot/doctor output.
    #[must_use]
    pub fn is_fresh(&self, now_ms: u64, max_age_ms: u64) -> bool {
        let Some(last) = self.last_observed_at_ms else {
            return false;
        };
        now_ms.saturating_sub(last) <= max_age_ms
    }
}

/// Capability passport for one pane / agent / session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CapabilityPassport {
    /// Stable agent identity (e.g. "PinkForge", "cc1", "AmberStream").
    pub agent_id: String,
    /// Stable pane / session identity. None when the passport is
    /// agent-scoped rather than pane-scoped.
    #[serde(default)]
    pub pane_id: Option<u64>,
    /// All capabilities asserted for this pane / agent.
    #[serde(default)]
    pub capabilities: Vec<CapabilityEntry>,
    /// Monotonic generation: incremented when the passport is
    /// re-issued (e.g. after a probe pass or runtime wrapper change).
    #[serde(default)]
    pub generation: u64,
    /// Epoch milliseconds when this passport was assembled.
    pub signed_at_ms: u64,
}

impl CapabilityPassport {
    /// Number of capabilities at the operator-supplied verification
    /// level. Used by robot/doctor summaries.
    #[must_use]
    pub fn count_at(&self, level: CapabilityVerification) -> usize {
        self.capabilities
            .iter()
            .filter(|c| c.verification == level)
            .count()
    }

    /// True iff the passport carries at least one Verified capability
    /// of the requested class. Fail-closed: Unknown / Observed /
    /// Declared do NOT satisfy this predicate, by design.
    #[must_use]
    pub fn has_verified(&self, class: &CapabilityClass) -> bool {
        self.capabilities
            .iter()
            .any(|c| &c.class == class && c.verification.is_dispatchable())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn sample_passport() -> CapabilityPassport {
        CapabilityPassport {
            agent_id: "cc1".into(),
            pane_id: Some(7),
            capabilities: vec![
                CapabilityEntry {
                    class: CapabilityClass::ModelFamily("claude-opus-4-7".into()),
                    verification: CapabilityVerification::Verified,
                    last_observed_at_ms: Some(1_000_000),
                    proof: RedactedProof::from_value(b"opus-handshake-token"),
                },
                CapabilityEntry {
                    class: CapabilityClass::ToolAvailability("bash".into()),
                    verification: CapabilityVerification::Observed,
                    last_observed_at_ms: Some(900_000),
                    proof: RedactedProof::empty(),
                },
                CapabilityEntry {
                    class: CapabilityClass::SafetyConstraint("forbid_unsafe_code".into()),
                    verification: CapabilityVerification::Declared,
                    last_observed_at_ms: None,
                    proof: RedactedProof::empty(),
                },
                CapabilityEntry {
                    class: CapabilityClass::NetworkPolicy("loopback_only".into()),
                    verification: CapabilityVerification::Unknown,
                    last_observed_at_ms: None,
                    proof: RedactedProof::empty(),
                },
            ],
            generation: 3,
            signed_at_ms: 1_500_000,
        }
    }

    fn arb_verification() -> impl Strategy<Value = CapabilityVerification> {
        prop_oneof![
            Just(CapabilityVerification::Declared),
            Just(CapabilityVerification::Observed),
            Just(CapabilityVerification::Verified),
            Just(CapabilityVerification::Unknown),
        ]
    }

    #[test]
    fn capability_class_label_includes_kind_prefix() {
        assert_eq!(
            CapabilityClass::ModelFamily("opus".into()).label(),
            "model_family:opus"
        );
        assert_eq!(
            CapabilityClass::ToolAvailability("bash".into()).label(),
            "tool:bash"
        );
        assert_eq!(
            CapabilityClass::Other("vendor-x".into()).label(),
            "other:vendor-x"
        );
    }

    #[test]
    fn verification_dispatchable_only_for_verified() {
        assert!(CapabilityVerification::Verified.is_dispatchable());
        assert!(!CapabilityVerification::Observed.is_dispatchable());
        assert!(!CapabilityVerification::Declared.is_dispatchable());
        assert!(!CapabilityVerification::Unknown.is_dispatchable());
    }

    #[test]
    fn verification_default_is_unknown_fail_closed() {
        assert_eq!(
            CapabilityVerification::default(),
            CapabilityVerification::Unknown
        );
    }

    #[test]
    fn verification_confidence_labels_are_stable() {
        assert_eq!(CapabilityVerification::Verified.confidence_label(), "high");
        assert_eq!(
            CapabilityVerification::Observed.confidence_label(),
            "medium"
        );
        assert_eq!(CapabilityVerification::Declared.confidence_label(), "low");
        assert_eq!(CapabilityVerification::Unknown.confidence_label(), "none");
    }

    #[test]
    fn redacted_proof_drops_secret_bytes() {
        let secret = b"ANTHROPIC_API_KEY=sk-fake-credential-shape";
        let proof = RedactedProof::from_value(secret);
        // Digest is deterministic + length is preserved; raw secret
        // never appears in the serialized form.
        assert_eq!(proof.digest_hex.len(), 16 + 8); // 64-bit FNV + 32-bit CRC, hex
        assert_eq!(proof.byte_len, Some(secret.len() as u64));
        let json = serde_json::to_string(&proof).expect("serialize");
        assert!(!json.contains("ANTHROPIC_API_KEY"));
        assert!(!json.contains("sk-fake-credential-shape"));
    }

    #[test]
    fn redacted_proof_is_deterministic_across_constructions() {
        let a = RedactedProof::from_value(b"identical-secret");
        let b = RedactedProof::from_value(b"identical-secret");
        assert_eq!(a, b);
    }

    #[test]
    fn redacted_proof_different_inputs_yield_different_digests() {
        let a = RedactedProof::from_value(b"secret-A");
        let b = RedactedProof::from_value(b"secret-B");
        assert_ne!(a.digest_hex, b.digest_hex);
    }

    #[test]
    fn redacted_proof_display_truncates_long_digest() {
        let proof = RedactedProof::from_value(b"any-input-yields-24-hex-chars");
        let truncated = proof.display_truncated();
        // 12 hex chars + ellipsis (3 bytes for the unicode horizontal ellipsis)
        assert!(truncated.starts_with(&proof.digest_hex[..REDACTED_PROOF_DISPLAY_LEN]));
        assert!(truncated.ends_with('…'));
    }

    #[test]
    fn redacted_proof_empty_serializes_without_byte_len() {
        let proof = RedactedProof::empty();
        let json = serde_json::to_string(&proof).expect("serialize empty");
        // Empty digest, byte_len=None roundtrips.
        let back: RedactedProof = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, proof);
    }

    #[test]
    fn capability_entry_freshness_gate() {
        let now_ms = 10_000;
        let max_age_ms = 1_000;
        // Observed within window — fresh.
        let entry = CapabilityEntry {
            class: CapabilityClass::ToolAvailability("bash".into()),
            verification: CapabilityVerification::Verified,
            last_observed_at_ms: Some(9_500),
            proof: RedactedProof::empty(),
        };
        assert!(entry.is_fresh(now_ms, max_age_ms));

        // Observed outside window — stale.
        let stale = CapabilityEntry {
            last_observed_at_ms: Some(8_000),
            ..entry.clone()
        };
        assert!(!stale.is_fresh(now_ms, max_age_ms));

        // Never observed — stale.
        let never = CapabilityEntry {
            last_observed_at_ms: None,
            ..entry.clone()
        };
        assert!(!never.is_fresh(now_ms, max_age_ms));
    }

    #[test]
    fn passport_count_at_buckets_by_verification() {
        let p = sample_passport();
        assert_eq!(p.count_at(CapabilityVerification::Verified), 1);
        assert_eq!(p.count_at(CapabilityVerification::Observed), 1);
        assert_eq!(p.count_at(CapabilityVerification::Declared), 1);
        assert_eq!(p.count_at(CapabilityVerification::Unknown), 1);
    }

    #[test]
    fn passport_has_verified_fail_closed_below_verified() {
        let p = sample_passport();
        // ModelFamily(opus-4-7) is Verified — true.
        assert!(p.has_verified(&CapabilityClass::ModelFamily("claude-opus-4-7".into())));
        // ToolAvailability(bash) is only Observed — false.
        assert!(!p.has_verified(&CapabilityClass::ToolAvailability("bash".into())));
        // SafetyConstraint(forbid_unsafe) is only Declared — false.
        assert!(!p.has_verified(&CapabilityClass::SafetyConstraint(
            "forbid_unsafe_code".into()
        )));
        // NetworkPolicy(loopback_only) is Unknown — false.
        assert!(!p.has_verified(&CapabilityClass::NetworkPolicy("loopback_only".into())));
        // Capability not in passport at all — false.
        assert!(!p.has_verified(&CapabilityClass::AuthSurface("missing".into())));
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(32))]

        #[test]
        fn proptest_directly_constructed_passport_dispatches_only_verified(
            verification in arb_verification()
        ) {
            let class = CapabilityClass::SafetyConstraint("accept_passport_excerpt".into());
            let passport = CapabilityPassport {
                agent_id: "direct-builder".into(),
                pane_id: Some(42),
                capabilities: vec![CapabilityEntry {
                    class: class.clone(),
                    verification,
                    last_observed_at_ms: Some(10_000),
                    proof: RedactedProof::empty(),
                }],
                generation: 7,
                signed_at_ms: 10_000,
            };

            prop_assert_eq!(passport.has_verified(&class), verification == CapabilityVerification::Verified);
        }
    }

    #[test]
    fn passport_serde_roundtrip_preserves_all_fields() {
        let p = sample_passport();
        let json = serde_json::to_string(&p).expect("serialize");
        let back: CapabilityPassport = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, p);
    }

    #[test]
    fn passport_serde_accepts_older_json_with_missing_optional_fields() {
        // Forward-compat: a capability entry without verification /
        // last_observed_at_ms / proof must deserialize cleanly with
        // Unknown / None / empty defaults.
        let json = r#"{
            "agent_id": "cc1",
            "capabilities": [
                {
                    "class": {"kind": "tool_availability", "name": "bash"}
                }
            ],
            "signed_at_ms": 12345
        }"#;
        let p: CapabilityPassport = serde_json::from_str(json).expect("deserialize older shape");
        assert_eq!(p.agent_id, "cc1");
        assert_eq!(p.pane_id, None);
        assert_eq!(p.capabilities.len(), 1);
        assert_eq!(
            p.capabilities[0].verification,
            CapabilityVerification::Unknown
        );
        assert_eq!(p.capabilities[0].last_observed_at_ms, None);
        assert_eq!(p.capabilities[0].proof, RedactedProof::empty());
        assert_eq!(p.generation, 0);
    }

    #[test]
    fn passport_serializes_redacted_proof_without_leaking_input() {
        // End-to-end: a passport carrying a probe response with a fake
        // credential serializes to JSON that does NOT contain the
        // credential bytes.
        let secret = "ANTHROPIC_API_KEY=sk-fake-leak-canary-1234567890ABCDEFGHIJ";
        let mut p = sample_passport();
        p.capabilities.push(CapabilityEntry {
            class: CapabilityClass::AuthSurface("anthropic_api".into()),
            verification: CapabilityVerification::Verified,
            last_observed_at_ms: Some(1_400_000),
            proof: RedactedProof::from_value(secret.as_bytes()),
        });
        let json = serde_json::to_string(&p).expect("serialize");
        assert!(
            !json.contains("sk-fake-leak-canary-1234567890ABCDEFGHIJ"),
            "raw secret leaked into passport JSON: {}",
            json
        );
        assert!(
            !json.contains("ANTHROPIC_API_KEY"),
            "credential identifier leaked into passport JSON: {}",
            json
        );
    }
}
