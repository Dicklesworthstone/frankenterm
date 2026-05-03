//! Capability preflight gate (ft-1650n.1 slice 3 — probe site wiring).
//!
//! Sits atop [`crate::capability_passport_store::PassportStore`] and
//! produces a [`PreflightOutcome`] for a `(key, required_classes)`
//! pair: callers query whether a planned mux operation is permitted
//! BEFORE dispatching it. The decision is fail-closed — anything
//! short of "every required class has a fresh, Verified capability"
//! returns a non-allowed outcome with operator-readable diagnostics.
//!
//! # Wiring
//!
//! Slice 3 wires this primitive into the headless mux server as a
//! new `RemoteRequest::PassportPreflight` endpoint so callers can
//! gate operations remotely. Direct in-process gating of existing
//! `RemoteRequest::Command` dispatch is deferred to a follow-up
//! bead — that requires a per-command capability schema (which
//! commands need which classes) that's its own design exercise.
//!
//! # Freshness
//!
//! By default the preflight checker requires capabilities observed /
//! verified within [`DEFAULT_PREFLIGHT_FRESHNESS_MS`]. Override via
//! [`PreflightChecker::with_max_age_ms`].

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tracing::info;

use crate::capability_passport::{CapabilityClass, CapabilityVerification};
use crate::capability_passport_store::{PassportKey, PassportStore};

/// Default freshness window (5 minutes) for capabilities to count
/// toward an Allowed preflight outcome. Anything observed longer ago
/// downgrades the outcome to StalePassport so the operator can decide
/// whether to refresh probes before dispatch.
pub const DEFAULT_PREFLIGHT_FRESHNESS_MS: u64 = 300_000;

/// Outcome of a preflight capability check.
///
/// Allowed is the only state that authorizes dispatch. All other
/// variants carry diagnostic data so the operator can see exactly
/// why the check failed without inspecting passport internals.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PreflightOutcome {
    /// Every required capability is Verified and fresh — dispatch ok.
    Allowed,
    /// No passport is registered for the requested key.
    MissingPassport,
    /// A passport exists but at least one required capability is
    /// either absent or not at the Verified verification level.
    /// `unmet` lists the classes that failed; `present_at` lists the
    /// verification level (if any) for each unmet class so the
    /// operator can distinguish "never declared" from "declared but
    /// not verified".
    MissingCapabilities {
        unmet: Vec<CapabilityClass>,
        present_at: Vec<(CapabilityClass, Option<CapabilityVerification>)>,
    },
    /// All required capabilities are Verified but at least one was
    /// last observed before the freshness window cutoff. Operator
    /// should refresh probes before dispatch.
    StalePassport {
        oldest_observed_at_ms: u64,
        max_age_ms: u64,
    },
    /// Internal error reading from the passport store
    /// (poisoned lock, etc.). Treated as fail-closed — callers MUST
    /// NOT assume the capability is present.
    StoreUnavailable,
}

impl PreflightOutcome {
    /// True iff the outcome authorizes dispatch.
    #[must_use]
    pub fn is_allowed(&self) -> bool {
        matches!(self, Self::Allowed)
    }

    /// Stable label for telemetry / display.
    #[must_use]
    pub fn label(&self) -> &'static str {
        match self {
            Self::Allowed => "allowed",
            Self::MissingPassport => "missing_passport",
            Self::MissingCapabilities { .. } => "missing_capabilities",
            Self::StalePassport { .. } => "stale_passport",
            Self::StoreUnavailable => "store_unavailable",
        }
    }
}

/// Capability-aware preflight checker. Wraps a [`PassportStore`]
/// and a freshness window.
#[derive(Debug, Clone)]
pub struct PreflightChecker {
    store: Arc<PassportStore>,
    max_age_ms: u64,
}

impl PreflightChecker {
    /// Construct with the default freshness window
    /// ([`DEFAULT_PREFLIGHT_FRESHNESS_MS`]).
    #[must_use]
    pub fn new(store: Arc<PassportStore>) -> Self {
        Self {
            store,
            max_age_ms: DEFAULT_PREFLIGHT_FRESHNESS_MS,
        }
    }

    /// Override the freshness window.
    #[must_use]
    pub fn with_max_age_ms(mut self, max_age_ms: u64) -> Self {
        self.max_age_ms = max_age_ms;
        self
    }

    #[must_use]
    pub fn max_age_ms(&self) -> u64 {
        self.max_age_ms
    }

    #[must_use]
    pub fn store(&self) -> &Arc<PassportStore> {
        &self.store
    }

    /// Check whether dispatching an operation requiring every class
    /// in `required_classes` against the passport at `key` is
    /// permitted at `now_ms`.
    pub fn check(
        &self,
        key: &PassportKey,
        required_classes: &[CapabilityClass],
        now_ms: u64,
    ) -> PreflightOutcome {
        // ft-11wl6: every preflight decision lands a structured
        // tracing event so operators can query gate-decision streams
        // (e.g. "how many MissingPassport in the last hour").
        let outcome = self.check_inner(key, required_classes, now_ms);
        info!(
            target: "ft.preflight",
            agent_id = key.agent_id.as_str(),
            pane_id = key.pane_id,
            required = required_classes.len(),
            outcome = outcome.label(),
            "passport preflight outcome",
        );
        outcome
    }

    fn check_inner(
        &self,
        key: &PassportKey,
        required_classes: &[CapabilityClass],
        now_ms: u64,
    ) -> PreflightOutcome {
        let Some(passport) = self.store.get(key) else {
            return PreflightOutcome::MissingPassport;
        };

        let mut unmet = Vec::new();
        let mut present_at = Vec::new();
        let mut verified_observation_times = Vec::new();

        for class in required_classes {
            // Find the strongest verification level for this class.
            let mut best: Option<CapabilityVerification> = None;
            let mut best_observed_at: Option<u64> = None;
            for entry in &passport.capabilities {
                if &entry.class != class {
                    continue;
                }
                let promote = match best {
                    None => true,
                    Some(b) => verification_rank(entry.verification) > verification_rank(b),
                };
                if promote {
                    best = Some(entry.verification);
                    best_observed_at = entry.last_observed_at_ms;
                }
            }

            match best {
                Some(CapabilityVerification::Verified) => {
                    if let Some(t) = best_observed_at {
                        verified_observation_times.push(t);
                    } else {
                        // Verified without an observation timestamp —
                        // treat as stale (fail-closed).
                        verified_observation_times.push(0);
                    }
                }
                other => {
                    unmet.push(class.clone());
                    present_at.push((class.clone(), other));
                }
            }
        }

        if !unmet.is_empty() {
            return PreflightOutcome::MissingCapabilities { unmet, present_at };
        }

        // All required capabilities are Verified — apply the
        // freshness gate against the OLDEST observation among them.
        if let Some(&oldest) = verified_observation_times.iter().min()
            && now_ms.saturating_sub(oldest) > self.max_age_ms
        {
            return PreflightOutcome::StalePassport {
                oldest_observed_at_ms: oldest,
                max_age_ms: self.max_age_ms,
            };
        }

        PreflightOutcome::Allowed
    }
}

/// Rank of a verification level — higher means stronger.
/// Used to pick the strongest matching capability when a passport
/// declares the same class at multiple verification levels.
fn verification_rank(v: CapabilityVerification) -> u8 {
    match v {
        CapabilityVerification::Unknown => 0,
        CapabilityVerification::Declared => 1,
        CapabilityVerification::Observed => 2,
        CapabilityVerification::Verified => 3,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capability_passport::{
        CapabilityClass, CapabilityEntry, CapabilityPassport, CapabilityVerification, RedactedProof,
    };

    fn store_with_passport(p: CapabilityPassport) -> Arc<PassportStore> {
        let store = Arc::new(PassportStore::new());
        store.insert(p);
        store
    }

    fn entry(class: CapabilityClass, v: CapabilityVerification, observed_ms: Option<u64>) -> CapabilityEntry {
        CapabilityEntry {
            class,
            verification: v,
            last_observed_at_ms: observed_ms,
            proof: RedactedProof::empty(),
        }
    }

    fn passport_with(agent: &str, pane: Option<u64>, capabilities: Vec<CapabilityEntry>) -> CapabilityPassport {
        CapabilityPassport {
            agent_id: agent.into(),
            pane_id: pane,
            capabilities,
            generation: 1,
            signed_at_ms: 1_000,
        }
    }

    #[test]
    fn missing_passport_returns_missing_passport_outcome() {
        let store = Arc::new(PassportStore::new());
        let checker = PreflightChecker::new(store);
        let outcome = checker.check(
            &PassportKey::pane("nobody", 1),
            &[CapabilityClass::ToolAvailability("bash".into())],
            10_000,
        );
        assert_eq!(outcome, PreflightOutcome::MissingPassport);
        assert!(!outcome.is_allowed());
        assert_eq!(outcome.label(), "missing_passport");
    }

    #[test]
    fn all_required_verified_and_fresh_returns_allowed() {
        let store = store_with_passport(passport_with(
            "cc1",
            Some(1),
            vec![
                entry(
                    CapabilityClass::ToolAvailability("bash".into()),
                    CapabilityVerification::Verified,
                    Some(9_000),
                ),
                entry(
                    CapabilityClass::ToolAvailability("write".into()),
                    CapabilityVerification::Verified,
                    Some(9_500),
                ),
            ],
        ));
        let checker = PreflightChecker::new(store).with_max_age_ms(5_000);
        let outcome = checker.check(
            &PassportKey::pane("cc1", 1),
            &[
                CapabilityClass::ToolAvailability("bash".into()),
                CapabilityClass::ToolAvailability("write".into()),
            ],
            10_000,
        );
        assert_eq!(outcome, PreflightOutcome::Allowed);
        assert!(outcome.is_allowed());
    }

    #[test]
    fn missing_capability_lists_unmet_class() {
        let store = store_with_passport(passport_with(
            "cc1",
            Some(1),
            vec![entry(
                CapabilityClass::ToolAvailability("bash".into()),
                CapabilityVerification::Verified,
                Some(9_000),
            )],
        ));
        let checker = PreflightChecker::new(store);
        let outcome = checker.check(
            &PassportKey::pane("cc1", 1),
            &[
                CapabilityClass::ToolAvailability("bash".into()),
                CapabilityClass::ToolAvailability("write".into()),
            ],
            10_000,
        );
        let PreflightOutcome::MissingCapabilities { unmet, present_at } = outcome else {
            panic!("expected MissingCapabilities");
        };
        assert_eq!(unmet, vec![CapabilityClass::ToolAvailability("write".into())]);
        // present_at carries None for the missing class — distinguishes
        // "never declared" from "declared but not verified".
        assert_eq!(present_at.len(), 1);
        assert_eq!(present_at[0].0, CapabilityClass::ToolAvailability("write".into()));
        assert_eq!(present_at[0].1, None);
    }

    #[test]
    fn declared_but_not_verified_capability_listed_in_present_at() {
        let store = store_with_passport(passport_with(
            "cc1",
            Some(1),
            vec![entry(
                CapabilityClass::ToolAvailability("bash".into()),
                CapabilityVerification::Declared,
                None,
            )],
        ));
        let checker = PreflightChecker::new(store);
        let outcome = checker.check(
            &PassportKey::pane("cc1", 1),
            &[CapabilityClass::ToolAvailability("bash".into())],
            10_000,
        );
        let PreflightOutcome::MissingCapabilities { unmet, present_at } = outcome else {
            panic!("expected MissingCapabilities for declared-only class");
        };
        assert_eq!(unmet.len(), 1);
        assert_eq!(present_at[0].1, Some(CapabilityVerification::Declared));
    }

    #[test]
    fn stale_passport_after_freshness_window() {
        let store = store_with_passport(passport_with(
            "cc1",
            Some(1),
            vec![entry(
                CapabilityClass::ToolAvailability("bash".into()),
                CapabilityVerification::Verified,
                Some(1_000),
            )],
        ));
        let checker = PreflightChecker::new(store).with_max_age_ms(1_000);
        let outcome = checker.check(
            &PassportKey::pane("cc1", 1),
            &[CapabilityClass::ToolAvailability("bash".into())],
            10_000,
        );
        let PreflightOutcome::StalePassport { oldest_observed_at_ms, max_age_ms } = outcome else {
            panic!("expected StalePassport (got {outcome:?})");
        };
        assert_eq!(oldest_observed_at_ms, 1_000);
        assert_eq!(max_age_ms, 1_000);
    }

    #[test]
    fn freshness_uses_oldest_observation_across_required_classes() {
        // Two required Verified caps observed at different times; the
        // OLDEST should drive the freshness check (most-restrictive).
        let store = store_with_passport(passport_with(
            "cc1",
            Some(1),
            vec![
                entry(
                    CapabilityClass::ToolAvailability("bash".into()),
                    CapabilityVerification::Verified,
                    Some(8_000), // fresh
                ),
                entry(
                    CapabilityClass::ToolAvailability("write".into()),
                    CapabilityVerification::Verified,
                    Some(2_000), // stale
                ),
            ],
        ));
        let checker = PreflightChecker::new(store).with_max_age_ms(3_000);
        let outcome = checker.check(
            &PassportKey::pane("cc1", 1),
            &[
                CapabilityClass::ToolAvailability("bash".into()),
                CapabilityClass::ToolAvailability("write".into()),
            ],
            10_000,
        );
        match outcome {
            PreflightOutcome::StalePassport { oldest_observed_at_ms, .. } => {
                assert_eq!(oldest_observed_at_ms, 2_000);
            }
            other => panic!("expected StalePassport carrying oldest=2000 (got {other:?})"),
        }
    }

    #[test]
    fn checker_picks_strongest_verification_level_for_class() {
        // Same class declared at multiple levels — Verified wins
        // over Observed/Declared/Unknown.
        let store = store_with_passport(passport_with(
            "cc1",
            Some(1),
            vec![
                entry(
                    CapabilityClass::ToolAvailability("bash".into()),
                    CapabilityVerification::Declared,
                    None,
                ),
                entry(
                    CapabilityClass::ToolAvailability("bash".into()),
                    CapabilityVerification::Verified,
                    Some(9_000),
                ),
                entry(
                    CapabilityClass::ToolAvailability("bash".into()),
                    CapabilityVerification::Observed,
                    Some(9_500),
                ),
            ],
        ));
        let checker = PreflightChecker::new(store).with_max_age_ms(5_000);
        let outcome = checker.check(
            &PassportKey::pane("cc1", 1),
            &[CapabilityClass::ToolAvailability("bash".into())],
            10_000,
        );
        assert_eq!(outcome, PreflightOutcome::Allowed);
    }

    #[test]
    fn empty_required_classes_returns_allowed() {
        // Vacuous truth: no required capabilities → Allowed.
        // Useful when a caller wants to check passport-existence
        // separately from capability requirements.
        let store = store_with_passport(passport_with(
            "cc1",
            Some(1),
            vec![entry(
                CapabilityClass::ToolAvailability("bash".into()),
                CapabilityVerification::Verified,
                Some(9_000),
            )],
        ));
        let checker = PreflightChecker::new(store);
        let outcome = checker.check(&PassportKey::pane("cc1", 1), &[], 10_000);
        assert_eq!(outcome, PreflightOutcome::Allowed);
    }

    #[test]
    fn agent_scoped_key_distinguishes_from_pane_scoped() {
        let agent_passport = passport_with(
            "cc1",
            None,
            vec![entry(
                CapabilityClass::ToolAvailability("bash".into()),
                CapabilityVerification::Verified,
                Some(9_000),
            )],
        );
        let store = store_with_passport(agent_passport);
        let checker = PreflightChecker::new(store).with_max_age_ms(5_000);

        // Lookup by agent-only key → Allowed.
        assert_eq!(
            checker.check(
                &PassportKey::agent("cc1"),
                &[CapabilityClass::ToolAvailability("bash".into())],
                10_000
            ),
            PreflightOutcome::Allowed
        );
        // Lookup by pane-scoped key → MissingPassport (different key).
        assert_eq!(
            checker.check(
                &PassportKey::pane("cc1", 1),
                &[CapabilityClass::ToolAvailability("bash".into())],
                10_000
            ),
            PreflightOutcome::MissingPassport
        );
    }

    #[test]
    fn outcome_serde_roundtrip() {
        let cases = vec![
            PreflightOutcome::Allowed,
            PreflightOutcome::MissingPassport,
            PreflightOutcome::StalePassport {
                oldest_observed_at_ms: 1_000,
                max_age_ms: 5_000,
            },
            PreflightOutcome::MissingCapabilities {
                unmet: vec![CapabilityClass::ToolAvailability("write".into())],
                present_at: vec![(
                    CapabilityClass::ToolAvailability("write".into()),
                    Some(CapabilityVerification::Declared),
                )],
            },
            PreflightOutcome::StoreUnavailable,
        ];
        for outcome in cases {
            let json = serde_json::to_string(&outcome).expect("serialize");
            let back: PreflightOutcome = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(back, outcome);
        }
    }

    #[test]
    fn outcome_labels_are_stable_strings() {
        assert_eq!(PreflightOutcome::Allowed.label(), "allowed");
        assert_eq!(PreflightOutcome::MissingPassport.label(), "missing_passport");
        assert_eq!(
            PreflightOutcome::StalePassport {
                oldest_observed_at_ms: 0,
                max_age_ms: 0
            }
            .label(),
            "stale_passport"
        );
        assert_eq!(
            PreflightOutcome::MissingCapabilities {
                unmet: vec![],
                present_at: vec![]
            }
            .label(),
            "missing_capabilities"
        );
        assert_eq!(PreflightOutcome::StoreUnavailable.label(), "store_unavailable");
    }
}
