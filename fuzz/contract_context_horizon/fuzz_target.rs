#![no_main]

#[path = "../contract_fuzz_common.rs"]
mod contract_fuzz_common;

use arbitrary::Arbitrary;
use contract_fuzz_common::{
    MAX_ITEMS, assert_no_raw_content_flags, assert_schema_valid, bounded_ms, compile_schema,
    limited_text, roundtrip_value, stable_fragment,
};
use frankenterm_core::context_horizon::{
    ContextHorizonEvidenceState, ContextHorizonFailureClass, ContextHorizonInput,
    ContextHorizonPaneEvidence, ContextHorizonUnavailableDomain, advise_context_horizon,
    predict_context_horizon,
};
use jsonschema::Validator;
use libfuzzer_sys::fuzz_target;
use std::sync::OnceLock;

static SCHEMA: OnceLock<Validator> = OnceLock::new();

fn schema() -> &'static Validator {
    SCHEMA.get_or_init(|| {
        compile_schema(include_bytes!(
            "../../docs/json-schema/ft-context-horizon.json"
        ))
    })
}

#[derive(Arbitrary, Debug)]
struct FuzzInput {
    generated_at_ms: u64,
    horizon_window_ms: u64,
    panes: Vec<RawPaneEvidence>,
    unavailable_domains: Vec<RawUnavailableDomain>,
    artifact_paths: Vec<String>,
}

#[derive(Arbitrary, Debug)]
struct RawPaneEvidence {
    pane_id: u64,
    active_context_present: bool,
    token_budget: Option<u32>,
    tokens_consumed: Option<u32>,
    pressure_tier: RawPressureTier,
    compaction_count: Option<u16>,
    last_rotated_ago_ms: Option<u32>,
    last_activity_ago_ms: Option<u32>,
    previous_utilization_percent: Option<u8>,
    recent_rate_limit_events: u16,
    recent_compaction_events: u16,
    evidence_state: RawEvidenceState,
}

#[derive(Arbitrary, Debug)]
struct RawUnavailableDomain {
    domain: String,
    reason_code: String,
    evidence_state: RawEvidenceState,
    failure_class: RawFailureClass,
}

#[derive(Arbitrary, Debug)]
enum RawPressureTier {
    Known(u8),
    Custom(String),
    Missing,
}

#[derive(Arbitrary, Debug)]
enum RawEvidenceState {
    Measured,
    Inferred,
    Simulated,
    Stale,
    Unavailable,
    Mixed,
}

#[derive(Arbitrary, Debug)]
enum RawFailureClass {
    SourceRegression,
    PrivacyViolation,
    EnvironmentBlocked,
    UnavailableEvidence,
    TargetHardwareSkipped,
}

impl RawEvidenceState {
    fn into_contract(self) -> ContextHorizonEvidenceState {
        match self {
            Self::Measured => ContextHorizonEvidenceState::Measured,
            Self::Inferred => ContextHorizonEvidenceState::Inferred,
            Self::Simulated => ContextHorizonEvidenceState::Simulated,
            Self::Stale => ContextHorizonEvidenceState::Stale,
            Self::Unavailable => ContextHorizonEvidenceState::Unavailable,
            Self::Mixed => ContextHorizonEvidenceState::Mixed,
        }
    }
}

impl RawFailureClass {
    fn into_contract(self) -> ContextHorizonFailureClass {
        match self {
            Self::SourceRegression => ContextHorizonFailureClass::SourceRegression,
            Self::PrivacyViolation => ContextHorizonFailureClass::PrivacyViolation,
            Self::EnvironmentBlocked => ContextHorizonFailureClass::EnvironmentBlocked,
            Self::UnavailableEvidence => ContextHorizonFailureClass::UnavailableEvidence,
            Self::TargetHardwareSkipped => ContextHorizonFailureClass::TargetHardwareSkipped,
        }
    }
}

impl RawPressureTier {
    fn into_option(self) -> Option<String> {
        match self {
            Self::Known(index) => Some(
                ["unknown", "green", "yellow", "red", "black"][usize::from(index) % 5].to_string(),
            ),
            Self::Custom(value) => Some(stable_fragment(value, "unknown")),
            Self::Missing => None,
        }
    }
}

impl RawPaneEvidence {
    fn into_contract(self, generated_at_ms: u64) -> ContextHorizonPaneEvidence {
        ContextHorizonPaneEvidence {
            pane_id: self.pane_id % 1_000_000,
            active_context_present: self.active_context_present,
            token_budget: self
                .token_budget
                .map(|value| i64::from(value % 200_000) + 1),
            tokens_consumed: self.tokens_consumed.map(|value| i64::from(value % 250_000)),
            pressure_tier: self.pressure_tier.into_option(),
            compaction_count: self.compaction_count.map(i64::from),
            last_rotated_at_ms: self
                .last_rotated_ago_ms
                .map(|age| generated_at_ms.saturating_sub(u64::from(age)) as i64),
            last_activity_at_ms: self
                .last_activity_ago_ms
                .map(|age| generated_at_ms.saturating_sub(u64::from(age)) as i64),
            previous_utilization: self
                .previous_utilization_percent
                .map(|percent| f64::from(percent.min(200)) / 100.0),
            recent_rate_limit_events: u32::from(self.recent_rate_limit_events % 512),
            recent_compaction_events: u32::from(self.recent_compaction_events % 512),
            evidence_state: self.evidence_state.into_contract(),
        }
    }
}

impl RawUnavailableDomain {
    fn into_contract(self) -> ContextHorizonUnavailableDomain {
        ContextHorizonUnavailableDomain {
            domain: stable_fragment(self.domain, "fuzz.domain"),
            evidence_state: self.evidence_state.into_contract(),
            reason_codes: vec![stable_fragment(self.reason_code, "evidence.fuzz")],
            failure_class: self.failure_class.into_contract(),
        }
    }
}

impl FuzzInput {
    fn into_contract(self) -> ContextHorizonInput {
        let generated_at_ms = bounded_ms(self.generated_at_ms);
        let mut input = ContextHorizonInput::new(generated_at_ms);
        input.horizon_window_ms = (self.horizon_window_ms % 3_600_000).max(1);
        input.panes = self
            .panes
            .into_iter()
            .take(MAX_ITEMS)
            .map(|pane| pane.into_contract(generated_at_ms))
            .collect();
        input.unavailable_domains = self
            .unavailable_domains
            .into_iter()
            .take(MAX_ITEMS)
            .map(RawUnavailableDomain::into_contract)
            .collect();
        input.artifact_paths = self
            .artifact_paths
            .into_iter()
            .take(MAX_ITEMS)
            .map(|path| limited_text(path, "fuzz/context-horizon.json"))
            .collect();
        input
    }
}

fuzz_target!(|input: FuzzInput| {
    let contract_input = input.into_contract();
    let report = predict_context_horizon(&contract_input);
    let advisors = advise_context_horizon(&report);
    assert!(
        advisors.iter().all(|advisor| !advisor.mutation_allowed),
        "context horizon advisor must remain non-mutating"
    );

    let value = roundtrip_value(&report);
    assert_schema_valid(schema(), &value);
    assert_no_raw_content_flags(&value);
});
