//! Proptest pinning the validate-at-boundary gate contract for
//! [`CapabilityPassport`] and [`HandoffCapsule`] (br-ft-n9btw,
//! Option C from the bead's recommended remediation).
//!
//! ## Why this exists
//!
//! ft-n9btw flagged 28 pub fields across `capability_passport.rs` and
//! `handoff_capsule.rs` as a maintainability/security risk: the
//! "validate at the boundary" convention works only as long as every
//! production entry point routes through `PassportValidator::validate`
//! (for passports) or `HandoffCapsule::verify_integrity` /
//! `validate_for_destination` (for capsules). A future refactor that
//! splits the validator from the type — or a new entry point that
//! constructs the type directly — could silently bypass the gate.
//!
//! These proptests assert the gate contract holds: regardless of the
//! pub-field values an attacker / buggy caller dialed in, the
//! validator catches every violation listed in
//! [`crate::capability_passport_store::ValidationError`] and the
//! capsule integrity / version gates catch every tamper case.
//!
//! ## Scope
//!
//! - PassportValidator: 5 violations × random pub-field shapes.
//! - HandoffCapsule: integrity-mismatch via section mutation; version
//!   mismatch via direct field assignment; validate_for_destination
//!   short-circuits on integrity error.
//!
//! Future refactors that change the validator's API or rebrand the
//! gate must re-pin these contracts.

use frankenterm_core::capability_passport::{
    CapabilityClass, CapabilityEntry, CapabilityPassport, CapabilityVerification, RedactedProof,
};
use frankenterm_core::capability_passport_store::{
    DEFAULT_MAX_CAPABILITIES_PER_PASSPORT, DEFAULT_MAX_CLOCK_SKEW_MS, PassportValidator,
    ValidationError,
};
use frankenterm_core::handoff_capsule::{
    CapsuleEndpoint, CapsuleSection, CapsuleValidationError, HANDOFF_CAPSULE_VERSION,
    HandoffCapsule,
};
use proptest::prelude::*;

// ── Strategies ──────────────────────────────────────────────────────────────

fn arb_capability_class() -> impl Strategy<Value = CapabilityClass> {
    prop_oneof![
        "[a-z][a-z0-9_-]{0,15}".prop_map(CapabilityClass::ModelFamily),
        "[a-z][a-z0-9_-]{0,15}".prop_map(CapabilityClass::ToolAvailability),
        "[a-z][a-z0-9_-]{0,15}".prop_map(CapabilityClass::AuthSurface),
        "[a-z][a-z0-9_-]{0,15}".prop_map(CapabilityClass::SafetyConstraint),
        "[a-z][a-z0-9_-]{0,15}".prop_map(CapabilityClass::Other),
    ]
}

fn arb_capability_entry() -> impl Strategy<Value = CapabilityEntry> {
    (
        arb_capability_class(),
        prop_oneof![
            Just(CapabilityVerification::Declared),
            Just(CapabilityVerification::Observed),
            Just(CapabilityVerification::Verified),
            Just(CapabilityVerification::Unknown),
        ],
        proptest::option::of(0u64..=10_000_000_000_u64),
    )
        .prop_map(
            |(class, verification, last_observed_at_ms)| CapabilityEntry {
                class,
                verification,
                last_observed_at_ms,
                proof: RedactedProof::empty(),
            },
        )
}

fn arb_passport_with_caps(
    cap_range: std::ops::RangeInclusive<usize>,
) -> impl Strategy<Value = CapabilityPassport> {
    (
        "[a-zA-Z0-9_-]{1,32}",
        proptest::option::of(1u64..=10_000),
        proptest::collection::vec(arb_capability_entry(), cap_range),
        0u64..1_000,
        0u64..=10_000_000_000_u64,
    )
        .prop_map(
            |(agent_id, pane_id, capabilities, generation, signed_at_ms)| CapabilityPassport {
                agent_id,
                pane_id,
                capabilities,
                generation,
                signed_at_ms,
            },
        )
}

fn arb_capsule_section() -> impl Strategy<Value = CapsuleSection> {
    prop_oneof![
        "[ -~]{0,128}".prop_map(|text| CapsuleSection::ContextSummary { text }),
        proptest::collection::vec("[ -~]{0,32}".prop_map(String::from), 0..=4)
            .prop_map(|items| CapsuleSection::VerificationChecklist { items }),
        proptest::collection::vec("[a-z0-9_-]{1,16}".prop_map(String::from), 0..=4)
            .prop_map(|claim_ids| CapsuleSection::CausalSummary { claim_ids }),
    ]
}

fn arb_endpoint(id: u64) -> CapsuleEndpoint {
    CapsuleEndpoint {
        agent_id: format!("agent-{id}"),
        pane_id: Some(id),
        label: None,
    }
}

// ── PassportValidator gate-contract tests ───────────────────────────────────

proptest! {
    /// br-ft-n9btw: `agent_id == ""` MUST be rejected as
    /// `ValidationError::EmptyAgentId` regardless of all other pub
    /// fields. Pin: a refactor that drops the empty-string guard
    /// breaks this test.
    #[test]
    fn passport_validator_rejects_empty_agent_id_ft_n9btw(
        mut passport in arb_passport_with_caps(1..=4),
    ) {
        passport.agent_id = String::new();
        let validator = PassportValidator::default();
        let now_ms = 1_000_000_000_u64;
        let check = matches!(
            validator.validate(&passport, None, now_ms),
            Err(ValidationError::EmptyAgentId)
        );
        prop_assert!(check);
    }

    /// br-ft-n9btw: empty `capabilities` list MUST be rejected.
    /// Vacuous passports carry no assertions and are useless to a
    /// dispatcher — fail-closed at the boundary.
    #[test]
    fn passport_validator_rejects_no_capabilities_ft_n9btw(
        mut passport in arb_passport_with_caps(1..=4),
    ) {
        passport.capabilities.clear();
        let validator = PassportValidator::default();
        let now_ms = 1_000_000_000_u64;
        let check = matches!(
            validator.validate(&passport, None, now_ms),
            Err(ValidationError::NoCapabilities)
        );
        prop_assert!(check);
    }

    /// br-ft-n9btw: oversized capability lists MUST be rejected as
    /// DoS defense. Operator can set max_capabilities to a small
    /// value; pin that the validator catches any list bigger than
    /// that bound.
    #[test]
    fn passport_validator_rejects_too_many_capabilities_ft_n9btw(
        mut passport in arb_passport_with_caps(5..=10),
        cap in 1usize..=4,
    ) {
        let validator = PassportValidator {
            max_capabilities: cap,
            max_clock_skew_ms: DEFAULT_MAX_CLOCK_SKEW_MS,
        };
        // Ensure non-empty agent_id and signed_at within skew so we
        // hit the TooManyCapabilities arm specifically.
        if passport.agent_id.is_empty() {
            passport.agent_id = "a".to_string();
        }
        passport.signed_at_ms = 0;
        let now_ms = 1_000_000_000_u64;
        let outcome = validator.validate(&passport, None, now_ms);
        let count_expected = passport.capabilities.len();
        let check = matches!(
            outcome,
            Err(ValidationError::TooManyCapabilities { count, cap: max })
                if count == count_expected && max == cap
        );
        prop_assert!(check);
    }

    /// br-ft-n9btw: `signed_at_ms` more than `max_clock_skew_ms`
    /// past `now_ms` MUST be rejected. Pin the clock-skew bound
    /// against direct pub-field assignment.
    #[test]
    fn passport_validator_rejects_future_signed_at_ft_n9btw(
        mut passport in arb_passport_with_caps(1..=4),
        skew_excess in 1u64..=1_000_000_u64,
    ) {
        if passport.agent_id.is_empty() {
            passport.agent_id = "a".to_string();
        }
        let now_ms = 1_000_000_000_u64;
        passport.signed_at_ms = now_ms + DEFAULT_MAX_CLOCK_SKEW_MS + skew_excess;
        let validator = PassportValidator::default();
        let outcome = validator.validate(&passport, None, now_ms);
        let check = matches!(
            outcome,
            Err(ValidationError::SignedAtTooFarInFuture { .. })
        );
        prop_assert!(check);
    }

    /// br-ft-n9btw: `generation` MUST be strictly > prior at the
    /// same key. Stale or replayed passports must not overwrite.
    /// Pin against direct pub-field assignment.
    #[test]
    fn passport_validator_rejects_non_monotonic_generation_ft_n9btw(
        mut incoming in arb_passport_with_caps(1..=4),
        existing_gen in 1u64..=1_000,
        delta in 0u64..=10,
    ) {
        if incoming.agent_id.is_empty() {
            incoming.agent_id = "a".to_string();
        }
        // Force incoming.generation <= existing_gen (the rejection
        // condition).
        incoming.generation = existing_gen.saturating_sub(delta);
        incoming.signed_at_ms = 0;
        let mut existing = incoming.clone();
        existing.generation = existing_gen;
        let validator = PassportValidator::default();
        let now_ms = 1_000_000_000_u64;
        let outcome = validator.validate(&incoming, Some(&existing), now_ms);
        let check = matches!(
            outcome,
            Err(ValidationError::GenerationNonMonotonic { .. })
        );
        prop_assert!(check);
    }

    /// br-ft-n9btw negative: a structurally-valid passport
    /// constructed via direct pub-field assignment MUST pass the
    /// validator. Pin the OK path so we catch over-strict
    /// regressions in addition to too-permissive ones.
    #[test]
    fn passport_validator_accepts_well_formed_direct_construction_ft_n9btw(
        agent_seed in "[a-z]{1,16}",
        pane_id in 1u64..=1000,
        cap_count in 1usize..=8,
        signed_offset in 0u64..=DEFAULT_MAX_CLOCK_SKEW_MS,
        generation in 1u64..=1_000,
    ) {
        let now_ms = 1_000_000_000_u64;
        let capabilities = (0..cap_count)
            .map(|i| CapabilityEntry {
                class: CapabilityClass::ToolAvailability(format!("t{i}")),
                verification: CapabilityVerification::Verified,
                last_observed_at_ms: Some(now_ms),
                proof: RedactedProof::empty(),
            })
            .collect();
        let passport = CapabilityPassport {
            agent_id: agent_seed,
            pane_id: Some(pane_id),
            capabilities,
            generation,
            signed_at_ms: now_ms.saturating_sub(signed_offset),
        };
        let validator = PassportValidator {
            max_capabilities: DEFAULT_MAX_CAPABILITIES_PER_PASSPORT,
            max_clock_skew_ms: DEFAULT_MAX_CLOCK_SKEW_MS,
        };
        prop_assert!(validator.validate(&passport, None, now_ms).is_ok());
    }
}

// ── HandoffCapsule integrity / version gate-contract tests ──────────────────

proptest! {
    /// br-ft-n9btw: a freshly-built capsule MUST verify integrity
    /// cleanly. Pin the OK path against accidental refactors.
    #[test]
    fn capsule_verify_integrity_ok_for_pristine_capsule_ft_n9btw(
        sections in proptest::collection::vec(arb_capsule_section(), 0..=6),
        signed_at_ms in 0u64..=10_000_000,
    ) {
        let capsule = HandoffCapsule::build(
            arb_endpoint(1),
            arb_endpoint(2),
            sections,
            signed_at_ms,
        );
        prop_assert!(capsule.verify_integrity().is_ok());
    }

    /// br-ft-n9btw: in-place section mutation after build MUST be
    /// caught by verify_integrity. Pin that an attacker / buggy
    /// consumer who mutates `capsule.sections` directly cannot
    /// produce a capsule whose integrity tag still validates.
    #[test]
    fn capsule_verify_integrity_catches_section_tampering_ft_n9btw(
        sections in proptest::collection::vec(arb_capsule_section(), 1..=4),
        new_text in "[a-z]{8,32}",
        signed_at_ms in 0u64..=10_000_000,
    ) {
        let mut capsule = HandoffCapsule::build(
            arb_endpoint(1),
            arb_endpoint(2),
            sections,
            signed_at_ms,
        );
        // Mutate the first section's content via the pub field. The
        // integrity tag was computed against the pre-mutation
        // sections, so verify_integrity must now fail.
        if let Some(first) = capsule.sections.get_mut(0) {
            *first = CapsuleSection::ContextSummary { text: new_text };
        }
        let check = matches!(
            capsule.verify_integrity(),
            Err(CapsuleValidationError::IntegrityMismatch { .. })
        );
        prop_assert!(check);
    }

    /// br-ft-n9btw: validate_for_destination MUST short-circuit
    /// with UnsupportedVersion when capsule.version != current.
    /// Pin against direct pub-field assignment.
    #[test]
    fn capsule_validate_for_destination_catches_version_mismatch_ft_n9btw(
        sections in proptest::collection::vec(arb_capsule_section(), 0..=4),
        signed_at_ms in 0u64..=10_000_000,
        bad_version in (HANDOFF_CAPSULE_VERSION + 1)..=u32::MAX,
    ) {
        let mut capsule = HandoffCapsule::build(
            arb_endpoint(1),
            arb_endpoint(2),
            sections,
            signed_at_ms,
        );
        capsule.version = bad_version;
        let outcome = capsule.validate_for_destination(None);
        let check = matches!(
            outcome,
            Err(CapsuleValidationError::UnsupportedVersion { got, expected })
                if got == bad_version && expected == HANDOFF_CAPSULE_VERSION
        );
        prop_assert!(check);
    }

    /// br-ft-n9btw: validate_for_destination MUST also catch
    /// integrity-mismatch (it runs verify_integrity internally),
    /// even when the version is correct. Pin the inner-gate
    /// contract.
    #[test]
    fn capsule_validate_for_destination_catches_integrity_mismatch_ft_n9btw(
        sections in proptest::collection::vec(arb_capsule_section(), 1..=4),
        new_text in "[a-z]{8,32}",
        signed_at_ms in 0u64..=10_000_000,
    ) {
        let mut capsule = HandoffCapsule::build(
            arb_endpoint(1),
            arb_endpoint(2),
            sections,
            signed_at_ms,
        );
        if let Some(first) = capsule.sections.get_mut(0) {
            *first = CapsuleSection::ContextSummary { text: new_text };
        }
        let outcome = capsule.validate_for_destination(None);
        let check = matches!(
            outcome,
            Err(CapsuleValidationError::IntegrityMismatch { .. })
        );
        prop_assert!(check);
    }
}
