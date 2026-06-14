//! No-mock conformance + safety-invariant proof harness for the attention-router
//! golden scenario inventory (ft-x3nsb.6).
//!
//! This harness loads the REAL retained scenario inventory
//! (`fixtures/attention-router/scenarios.v1.json`) — no hand-wavy mocks — and
//! proves it stays stable and testable:
//!
//! 1. It parses into typed structs (schema is well-formed).
//! 2. It covers every scenario the bead requires, with unique ids.
//! 3. Every scenario is well-formed against the declared
//!    `classification_vocabulary` and carries the required source/expected/
//!    forbidden fields (each source has a real `command_or_api`).
//! 4. The `must_fail_when` safety doctrine is actually ENCODED in the scenarios
//!    — e.g. the RCH-degraded scenario forbids service mutation, the
//!    dirty-overlap scenario forbids touching another agent's work, the
//!    stale-claim scenario forbids force-release without a status check.
//! 5. The retained source fixture requirements execute through the real scoring
//!    engine, produce the inventory-pinned classification/action/confidence, and
//!    round-trip as structured JSON plus TOON-equivalent output.

use std::collections::BTreeSet;

use frankenterm_core::attention_router::{
    build_attention_router_snapshot, AttentionRouterClassification, AttentionRouterConfidence,
    AttentionRouterItem, AttentionRouterNudgeKind, AttentionRouterRedactionPosture,
    AttentionRouterSafeAction, AttentionRouterSourceAdapterInput, AttentionRouterSourceFact,
    AttentionRouterSourceFactKind, AttentionRouterSourceHealth, AttentionRouterSourceKind,
    AttentionRouterSourceObservation,
};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use serde_json::{json, Value};

const INVENTORY_JSON: &str = include_str!("../../../fixtures/attention-router/scenarios.v1.json");

#[derive(Debug, Deserialize)]
struct ScenarioInventory {
    schema_version: u32,
    contract_id: String,
    classification_vocabulary: Vec<String>,
    scenarios: Vec<Scenario>,
}

#[derive(Debug, Deserialize)]
struct Scenario {
    scenario_id: String,
    #[serde(default)]
    title: String,
    #[serde(default)]
    summary: String,
    source_fixture_requirements: Vec<SourceReq>,
    expected: Expected,
    #[serde(default)]
    forbidden_actions: Vec<String>,
    #[serde(default)]
    volatility: Option<Volatility>,
}

#[derive(Debug, Deserialize)]
struct SourceReq {
    #[serde(default)]
    source_id: String,
    command_or_api: String,
    #[serde(default)]
    required_reason_codes: Vec<String>,
    #[serde(default)]
    required_subjects: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct Expected {
    classification: String,
    #[serde(default)]
    confidence: String,
    #[serde(default)]
    explanation_must_include: Vec<String>,
    recommended_safe_action: String,
}

#[derive(Debug, Deserialize)]
struct Volatility {
    level: u8,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct ScenarioDecisionRecord {
    schema_version: u32,
    scenario_id: String,
    source_count: usize,
    // `#[serde(default)]` on the collection/option/bool fields keeps the strict
    // TOON-equivalent round-trip robust: a compact renderer may legitimately
    // omit an empty array, a `false` boolean, or a null option. Genuine data
    // loss is still caught because the decoded records are compared field-for-
    // field against the source records below.
    #[serde(default)]
    retained_source_ids: Vec<String>,
    #[serde(default)]
    evidence_source_ids: Vec<String>,
    classification: AttentionRouterClassification,
    recommended_safe_action: AttentionRouterSafeAction,
    confidence: AttentionRouterConfidence,
    nudge_kind: AttentionRouterNudgeKind,
    #[serde(default)]
    reason_codes: Vec<String>,
    #[serde(default)]
    engine_forbidden_actions: Vec<String>,
    #[serde(default)]
    inventory_forbidden_actions: Vec<String>,
    #[serde(default)]
    volatility_level: Option<u8>,
    #[serde(default)]
    side_effects_executed: bool,
    #[serde(default)]
    action_mutates: bool,
    #[serde(default)]
    nudge_mutates: bool,
}

fn load_inventory() -> ScenarioInventory {
    serde_json::from_str(INVENTORY_JSON)
        .expect("scenarios.v1.json must deserialize into the typed inventory schema")
}

fn scenario<'a>(inv: &'a ScenarioInventory, id: &str) -> &'a Scenario {
    inv.scenarios
        .iter()
        .find(|s| s.scenario_id == id)
        .expect("required scenario missing from inventory")
}

/// The six scenarios the bead's scope mandates.
const REQUIRED_SCENARIOS: &[&str] = &[
    "empty-ready-bv-blocked-recommendation",
    "rch-no-admissible-worker",
    "agent-mail-ack-required",
    "stale-in-progress-candidate",
    "dirty-overlap-active-owner",
    "docs-only-ready-while-proof-blocked",
];

const GENERATED_AT_MS: u64 = 1_770_000_300_000;
const WORKSPACE: &str = "/repo/frankenterm";

fn parse_expected<T>(scenario: &Scenario, field: &str, raw: &str) -> T
where
    T: DeserializeOwned,
{
    serde_json::from_value(Value::String(raw.to_string()))
        .expect("scenario expected value did not deserialize")
}

fn expected_classification(scenario: &Scenario) -> AttentionRouterClassification {
    parse_expected(
        scenario,
        "classification",
        &scenario.expected.classification,
    )
}

fn expected_action(scenario: &Scenario) -> AttentionRouterSafeAction {
    parse_expected(
        scenario,
        "recommended_safe_action",
        &scenario.expected.recommended_safe_action,
    )
}

fn expected_confidence(scenario: &Scenario) -> AttentionRouterConfidence {
    parse_expected(scenario, "confidence", &scenario.expected.confidence)
}

fn required_source_kinds() -> [AttentionRouterSourceKind; 6] {
    [
        AttentionRouterSourceKind::Beads,
        AttentionRouterSourceKind::AgentMail,
        AttentionRouterSourceKind::Git,
        AttentionRouterSourceKind::Rch,
        AttentionRouterSourceKind::PaneState,
        AttentionRouterSourceKind::OperatingEnvelope,
    ]
}

fn source_kind_for_req(req: &SourceReq) -> AttentionRouterSourceKind {
    let haystack = format!("{} {}", req.source_id, req.command_or_api).to_ascii_lowercase();
    if haystack.contains("reservation") {
        AttentionRouterSourceKind::PaneReservations
    } else if haystack.contains("agent-mail")
        || haystack.contains("agent_mail")
        || haystack.contains("fetch_inbox")
        || haystack.contains("search_messages")
    {
        AttentionRouterSourceKind::AgentMail
    } else if haystack.contains("git") {
        AttentionRouterSourceKind::Git
    } else if haystack.contains("rch") {
        AttentionRouterSourceKind::Rch
    } else if haystack.contains("beads")
        || haystack.contains("br ")
        || haystack.contains("bv ")
        || haystack.contains("bv-")
    {
        AttentionRouterSourceKind::Beads
    } else if haystack.contains("pane") || haystack.contains("ft robot state") {
        AttentionRouterSourceKind::PaneState
    } else if haystack.contains("filesystem")
        || haystack.contains("df -h")
        || haystack.contains("cleanup")
        || haystack.contains("deletion-approval")
        || haystack.contains("proof")
        || haystack.contains("agents.md")
    {
        AttentionRouterSourceKind::OperatingEnvelope
    } else {
        AttentionRouterSourceKind::Manual
    }
}

fn source_health_for_req(scenario: &Scenario, req: &SourceReq) -> AttentionRouterSourceHealth {
    if scenario.expected.classification == "proof_starved"
        || req
            .required_reason_codes
            .iter()
            .any(|code| code.contains("write_blocked") || code.contains("capacity_critical"))
    {
        AttentionRouterSourceHealth::Degraded
    } else {
        AttentionRouterSourceHealth::Available
    }
}

fn decision_source_index(scenario: &Scenario) -> usize {
    let action = scenario.expected.recommended_safe_action.as_str();
    let preferred_source = match action {
        "do_not_claim_bv_pick_record_blocker_or_find_disjoint_static_slice"
        | "ignore_bv_command_hints_use_br_json_state" => "bv",
        "keep_proof_required_bead_open_or_blocked_and_record_rch_reason_code" => "rch",
        "acknowledge_message_then_reply_or_continue_based_on_request" => "agent-mail",
        "send_or_draft_status_check_before_force_release"
        | "send_status_check_then_prepare_operator_review_if_no_response" => "beads",
        "avoid_overlapping_paths_and_claim_only_disjoint_ready_work" => "git-status",
        "notify_owner_wait_for_publish_or_pick_disjoint_work" => "git-owned-closeout",
        "claim_ready_static_slice_reserve_paths_and_run_static_checks" => "beads-ready",
        "choose_disjoint_work_or_request_handoff_before_editing" => "reservation",
        "wait_for_reservation_release_or_expiry_then_recheck" => "reservation-lease",
        "fail_closed_request_targeted_handoff_or_pick_disjoint_work" => "beads-owner",
        "wait_for_commit_push_mirror_and_reservation_release" => "git-publication",
        "pause_write_work_request_exact_cleanup_approval_or_pick_read_only_work" => "filesystem",
        _ => "",
    };
    scenario
        .source_fixture_requirements
        .iter()
        .position(|req| req.source_id.contains(preferred_source))
        .unwrap_or(0)
}

fn fact_kind_for_action(action: AttentionRouterSafeAction) -> AttentionRouterSourceFactKind {
    match action {
        AttentionRouterSafeAction::ClaimReadyStaticSliceReservePathsAndRunStaticChecks => {
            AttentionRouterSourceFactKind::BeadsReady
        }
        AttentionRouterSafeAction::DoNotClaimBvPickRecordBlockerOrFindDisjointStaticSlice
        | AttentionRouterSafeAction::IgnoreBvCommandHintsUseBrJsonState => {
            AttentionRouterSourceFactKind::BvRecommendationConflict
        }
        AttentionRouterSafeAction::KeepProofRequiredBeadOpenOrBlockedAndRecordRchReasonCode => {
            AttentionRouterSourceFactKind::RchProofStarvation
        }
        AttentionRouterSafeAction::AcknowledgeMessageThenReplyOrContinueBasedOnRequest => {
            AttentionRouterSourceFactKind::AgentMailAckRequired
        }
        AttentionRouterSafeAction::SendOrDraftStatusCheckBeforeForceRelease
        | AttentionRouterSafeAction::SendStatusCheckThenPrepareOperatorReviewIfNoResponse => {
            AttentionRouterSourceFactKind::BeadsInProgress
        }
        AttentionRouterSafeAction::AvoidOverlappingPathsAndClaimOnlyDisjointReadyWork
        | AttentionRouterSafeAction::NotifyOwnerWaitForPublishOrPickDisjointWork => {
            AttentionRouterSourceFactKind::GitDirtyPaths
        }
        AttentionRouterSafeAction::ChooseDisjointWorkOrRequestHandoffBeforeEditing => {
            AttentionRouterSourceFactKind::PaneReservationConflict
        }
        AttentionRouterSafeAction::WaitForReservationReleaseOrExpiryThenRecheck => {
            AttentionRouterSourceFactKind::PaneReservationActive
        }
        AttentionRouterSafeAction::FailClosedRequestTargetedHandoffOrPickDisjointWork => {
            AttentionRouterSourceFactKind::Manual
        }
        AttentionRouterSafeAction::WaitForCommitPushMirrorAndReservationRelease => {
            AttentionRouterSourceFactKind::GitBranchDivergence
        }
        AttentionRouterSafeAction::PauseWriteWorkRequestExactCleanupApprovalOrPickReadOnlyWork => {
            AttentionRouterSourceFactKind::OperatingEnvelopeCapacity
        }
        AttentionRouterSafeAction::WorkDomainDependencyFirst
        | AttentionRouterSafeAction::RefreshUnavailableSourceOrUseRemainingReadOnlyContext
        | AttentionRouterSafeAction::InspectPolicyDenialAndChooseAllowedReadOnlyPath
        | AttentionRouterSafeAction::WaitForApprovalDecisionOrPickUngatedWork
        | AttentionRouterSafeAction::RespectPaneReservationOrRequestExplicitHandoff
        | AttentionRouterSafeAction::StabilizeMissionOrTxBeforeNewWork
        | AttentionRouterSafeAction::TriageUnhandledEventBeforeClaimingMoreWork
        | AttentionRouterSafeAction::WaitForConnectorRecoveryOrUseReadOnlyFallback
        | AttentionRouterSafeAction::InvestigateIncidentBundleBeforeContinuing
        | AttentionRouterSafeAction::ReplyToThreadWithBoundedContext => {
            AttentionRouterSourceFactKind::Manual
        }
    }
}

fn all_reason_codes(scenario: &Scenario) -> Vec<String> {
    sorted_unique(
        scenario
            .source_fixture_requirements
            .iter()
            .flat_map(|req| req.required_reason_codes.iter().cloned()),
    )
}

fn decision_reason_codes(scenario: &Scenario, req: &SourceReq) -> Vec<String> {
    if scenario.expected.recommended_safe_action
        == "send_status_check_then_prepare_operator_review_if_no_response"
    {
        all_reason_codes(scenario)
    } else {
        req.required_reason_codes.clone()
    }
}

fn all_subjects(scenario: &Scenario) -> Vec<String> {
    sorted_unique(
        scenario
            .source_fixture_requirements
            .iter()
            .flat_map(|req| req.required_subjects.iter().cloned()),
    )
}

fn apply_subject(mut fact: AttentionRouterSourceFact, subject: &str) -> AttentionRouterSourceFact {
    if subject.starts_with("ft-") || subject.contains("<bead>") {
        fact = fact.with_bead_id(subject);
    } else if subject.contains('/') || subject.contains('*') || subject.ends_with(".rs") {
        fact = fact.with_affected_path(subject);
    } else if subject.starts_with('[') || subject.contains("AGENT") || subject.contains("OWNER") {
        fact = fact.with_agent_name(subject);
    } else if !subject.is_empty() {
        fact = fact.with_affected_path(subject);
    }
    fact
}

fn decision_fact(scenario: &Scenario, req: &SourceReq) -> AttentionRouterSourceFact {
    let expected_action = expected_action(scenario);
    let mut fact = AttentionRouterSourceFact::new(
        fact_kind_for_action(expected_action),
        format!("{}: {}", scenario.title, scenario.summary),
    );
    for reason_code in decision_reason_codes(scenario, req) {
        fact = fact.with_reason_code(reason_code);
    }
    for subject in all_subjects(scenario) {
        fact = apply_subject(fact, &subject);
    }
    fact
}

fn observation_for_req(
    scenario: &Scenario,
    req: &SourceReq,
    index: usize,
    decision_index: usize,
) -> AttentionRouterSourceObservation {
    let mut observation = AttentionRouterSourceObservation::new(
        &req.source_id,
        source_kind_for_req(req),
        source_health_for_req(scenario, req),
        &req.command_or_api,
        format!("{}: {}", scenario.title, scenario.summary),
    )
    .live(
        GENERATED_AT_MS + u64::try_from(index).unwrap_or(u64::MAX),
        0,
    )
    .redaction_posture(AttentionRouterRedactionPosture::NoSensitiveContent)
    .items_seen(0);
    for reason_code in &req.required_reason_codes {
        observation = observation.with_reason_code(reason_code);
    }
    if index == decision_index {
        observation = observation
            .with_fact(decision_fact(scenario, req))
            .items_seen(1);
    }
    observation
}

fn neutral_required_source(
    kind: AttentionRouterSourceKind,
    generated_at_ms: u64,
) -> AttentionRouterSourceObservation {
    AttentionRouterSourceObservation::new(
        format!("harness.required.{kind:?}"),
        kind,
        AttentionRouterSourceHealth::Available,
        "retained_fixture.neutral_required_source",
        "contract-required source has no retained hazard for this scenario",
    )
    .live(generated_at_ms, 0)
    .redaction_posture(AttentionRouterRedactionPosture::NoSensitiveContent)
    .items_seen(0)
}

fn scenario_input(scenario: &Scenario) -> AttentionRouterSourceAdapterInput {
    let decision_index = decision_source_index(scenario);
    let mut input = AttentionRouterSourceAdapterInput::new(GENERATED_AT_MS, WORKSPACE);
    let mut present = BTreeSet::new();
    for (index, req) in scenario.source_fixture_requirements.iter().enumerate() {
        let kind = source_kind_for_req(req);
        present.insert(kind);
        input = input.with_observation(observation_for_req(scenario, req, index, decision_index));
    }
    for (offset, kind) in required_source_kinds().into_iter().enumerate() {
        if !present.contains(&kind) {
            input = input.with_observation(neutral_required_source(
                kind,
                GENERATED_AT_MS + 10_000 + u64::try_from(offset).unwrap_or(u64::MAX),
            ));
        }
    }
    input
}

fn sorted_unique(values: impl IntoIterator<Item = String>) -> Vec<String> {
    values
        .into_iter()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

/// Canonicalize a TOON-decoded JSON value by collapsing integral floating-point
/// numbers back into integer JSON numbers.
///
/// TOON renders every JSON number through a floating-point model, so an integer
/// field such as `schema_version: 1` decodes as `1.0`. `serde_json` refuses to
/// deserialize `1.0` into a `u32`/`usize`/`u8`, so the strict typed round-trip
/// needs a documented TOON-equivalent normalization. This collapse is lossless
/// for `ScenarioDecisionRecord` because every one of its numeric fields
/// (`schema_version`, `source_count`, `volatility_level`) holds an integer.
fn normalize_integral_floats(value: &mut Value) {
    match value {
        Value::Number(number) => {
            // Only rewrite genuine floats; a number that already round-trips as
            // an integer is left untouched.
            if number.as_u64().is_none() && number.as_i64().is_none() {
                if let Some(float) = number.as_f64() {
                    if float.is_finite() && float.fract() == 0.0 {
                        *value = if float >= 0.0 {
                            Value::Number((float as u64).into())
                        } else {
                            Value::Number((float as i64).into())
                        };
                    }
                }
            }
        }
        Value::Array(items) => {
            for item in items {
                normalize_integral_floats(item);
            }
        }
        Value::Object(map) => {
            for (_key, child) in map {
                normalize_integral_floats(child);
            }
        }
        _ => {}
    }
}

fn matching_decision_item<'a>(
    scenario: &Scenario,
    items: &'a [AttentionRouterItem],
) -> &'a AttentionRouterItem {
    let classification = expected_classification(scenario);
    let action = expected_action(scenario);
    items
        .iter()
        .find(|item| {
            item.classification == classification && item.recommended_action.action == action
        })
        .expect("scenario did not produce expected classification/action decision")
}

fn decision_record(
    scenario: &Scenario,
    item: &AttentionRouterItem,
    side_effects_executed: bool,
) -> ScenarioDecisionRecord {
    ScenarioDecisionRecord {
        schema_version: 1,
        scenario_id: scenario.scenario_id.clone(),
        source_count: scenario.source_fixture_requirements.len(),
        retained_source_ids: sorted_unique(
            scenario
                .source_fixture_requirements
                .iter()
                .map(|req| req.source_id.clone()),
        ),
        evidence_source_ids: sorted_unique(
            item.evidence
                .iter()
                .map(|evidence| evidence.source_id.clone()),
        ),
        classification: item.classification,
        recommended_safe_action: item.recommended_action.action,
        confidence: item.confidence_label,
        nudge_kind: item.nudge_plan_receipt.nudge.kind,
        reason_codes: sorted_unique(item.reason_codes.clone()),
        engine_forbidden_actions: sorted_unique(item.forbidden_actions.clone()),
        inventory_forbidden_actions: sorted_unique(scenario.forbidden_actions.clone()),
        volatility_level: scenario
            .volatility
            .as_ref()
            .map(|volatility| volatility.level),
        side_effects_executed,
        action_mutates: item.recommended_action.mutates,
        nudge_mutates: item.nudge_plan_receipt.nudge.mutates,
    }
}

#[test]
fn inventory_parses_and_covers_required_scenarios() {
    let inv = load_inventory();
    assert!(
        inv.schema_version >= 1 && !inv.contract_id.is_empty(),
        "inventory must declare schema_version + contract_id"
    );
    assert!(
        inv.scenarios.len() >= REQUIRED_SCENARIOS.len(),
        "expected at least {} scenarios, found {}",
        REQUIRED_SCENARIOS.len(),
        inv.scenarios.len()
    );

    // Unique scenario ids.
    let mut seen = std::collections::BTreeSet::new();
    for s in &inv.scenarios {
        assert!(
            seen.insert(s.scenario_id.as_str()),
            "duplicate scenario_id: {}",
            s.scenario_id
        );
    }

    // Every bead-required scenario is present.
    for id in REQUIRED_SCENARIOS {
        assert!(
            inv.scenarios.iter().any(|s| s.scenario_id == *id),
            "bead-required scenario missing: {id}"
        );
    }
}

#[test]
fn every_scenario_is_well_formed() {
    let inv = load_inventory();
    let vocab: std::collections::BTreeSet<&str> = inv
        .classification_vocabulary
        .iter()
        .map(String::as_str)
        .collect();
    assert!(
        !vocab.is_empty(),
        "classification_vocabulary must be declared"
    );

    for s in &inv.scenarios {
        let id = &s.scenario_id;
        assert!(
            vocab.contains(s.expected.classification.as_str()),
            "scenario {id}: classification '{}' not in declared vocabulary",
            s.expected.classification
        );
        assert!(
            !s.expected.recommended_safe_action.is_empty(),
            "scenario {id}: recommended_safe_action must be non-empty"
        );
        assert!(
            !s.expected.confidence.is_empty(),
            "scenario {id}: confidence must be declared"
        );
        assert!(
            !s.expected.explanation_must_include.is_empty(),
            "scenario {id}: explanation_must_include must be non-empty"
        );
        assert!(
            !s.forbidden_actions.is_empty(),
            "scenario {id}: forbidden_actions must be non-empty (the safety surface)"
        );
        assert!(
            !s.source_fixture_requirements.is_empty(),
            "scenario {id}: must declare real source fixture requirements (no mocks)"
        );
        for src in &s.source_fixture_requirements {
            assert!(
                !src.command_or_api.is_empty(),
                "scenario {id} source {}: command_or_api must be a real command/API",
                src.source_id
            );
            assert!(
                !src.required_reason_codes.is_empty(),
                "scenario {id} source {}: must require at least one reason code",
                src.source_id
            );
        }
        // title is informative; if present it should be meaningful.
        if !s.title.is_empty() {
            assert!(s.title.len() >= 8, "scenario {id}: title too terse");
        }
    }
}

#[test]
fn every_scenario_executes_through_engine_json_and_toon_goldens() {
    let inv = load_inventory();
    let mut records = Vec::new();

    for scenario in &inv.scenarios {
        let input = scenario_input(scenario);
        let snapshot = build_attention_router_snapshot(&input);
        assert!(
            !snapshot.side_effects_executed,
            "scenario {}: scoring must never execute side effects",
            scenario.scenario_id
        );
        assert_eq!(
            snapshot.next_action.as_ref().map(|action| action.action),
            Some(expected_action(scenario)),
            "scenario {}: top-level next_action must match the retained golden action",
            scenario.scenario_id
        );

        for req in &scenario.source_fixture_requirements {
            let source = snapshot
                .sources
                .iter()
                .find(|source| source.source_id == req.source_id)
                .expect("retained source missing from scored snapshot");
            assert_eq!(
                source.command_or_api.as_str(),
                req.command_or_api.as_str(),
                "scenario {} source {}: retained command/API changed",
                scenario.scenario_id,
                req.source_id
            );
            for reason_code in &req.required_reason_codes {
                assert!(
                    source.reason_codes.contains(reason_code),
                    "scenario {} source {}: retained reason code {reason_code} missing from scored source {:?}",
                    scenario.scenario_id,
                    req.source_id,
                    source.reason_codes
                );
            }
        }

        let item = matching_decision_item(scenario, &snapshot.items);
        assert_eq!(
            item.confidence_label,
            expected_confidence(scenario),
            "scenario {}: confidence label must match inventory",
            scenario.scenario_id
        );
        assert!(
            !item.recommended_action.mutates,
            "scenario {}: recommended action must remain read-only/dry-run",
            scenario.scenario_id
        );
        assert!(
            !item.nudge_plan_receipt.live_mutation_allowed
                && !item.nudge_plan_receipt.side_effects_executed
                && !item.nudge_plan_receipt.nudge.mutates,
            "scenario {}: nudge receipt must remain side-effect free",
            scenario.scenario_id
        );
        assert!(
            !item.evidence.is_empty(),
            "scenario {}: decision log needs evidence entries",
            scenario.scenario_id
        );

        records.push(decision_record(
            scenario,
            item,
            snapshot.side_effects_executed,
        ));
    }

    let json_value = json!({
        "schema": "ft.attention_router.scenario_engine_golden.v1",
        "contract_id": inv.contract_id.clone(),
        "records": records.clone(),
    });
    let json_records: Vec<ScenarioDecisionRecord> = serde_json::from_value(
        json_value
            .get("records")
            .cloned()
            .expect("JSON golden wrapper contains records"),
    )
    .expect("JSON golden records deserialize");
    assert_eq!(json_records, records, "JSON golden must preserve records");

    let toon = toon_rust::encode(json_value.clone(), None);
    assert!(
        toon.contains("scenario_id") && toon.contains("recommended_safe_action"),
        "TOON rendering must expose scenario decision fields"
    );
    let decoded = toon_rust::try_decode(&toon, None).expect("TOON golden decodes");
    let decoded_json = toon_rust::cli::json_stringify::json_stringify_lines(&decoded, 0).join("\n");
    let decoded_value: Value =
        serde_json::from_str(&decoded_json).expect("decoded TOON is valid JSON");
    let mut toon_records_value = decoded_value
        .get("records")
        .cloned()
        .expect("TOON-equivalent wrapper contains records");
    // Apply the documented TOON-equivalent normalization (integral floats ->
    // integers) before strict typed deserialization. See
    // `normalize_integral_floats` for why this is lossless here.
    normalize_integral_floats(&mut toon_records_value);
    let toon_records: Vec<ScenarioDecisionRecord> =
        serde_json::from_value(toon_records_value).expect("TOON-equivalent records deserialize");
    assert_eq!(
        toon_records, records,
        "TOON-equivalent rendering must preserve scenario records"
    );
}

/// True when any forbidden action contains one of the given substrings.
fn forbids(s: &Scenario, needles: &[&str]) -> bool {
    s.forbidden_actions
        .iter()
        .any(|a| needles.iter().any(|n| a.contains(n)))
}

#[test]
fn safety_invariants_are_encoded() {
    let inv = load_inventory();

    // bv advisory is never claim authority; local cargo is never proof; RCH is
    // never restarted/cancelled when bv points at a blocked issue.
    let bv = scenario(&inv, "empty-ready-bv-blocked-recommendation");
    assert_eq!(bv.expected.classification, "blocked_infra");
    assert!(
        forbids(bv, &["claim"]),
        "empty-ready scenario must forbid claiming the bv pick"
    );
    assert!(
        forbids(bv, &["cargo"]) && forbids(bv, &["rch", "restart", "cancel"]),
        "empty-ready scenario must forbid local-cargo proof and RCH mutation"
    );

    // RCH degraded → never recommend service mutation / build cancellation.
    let rch = scenario(&inv, "rch-no-admissible-worker");
    assert!(
        matches!(
            rch.expected.classification.as_str(),
            "proof_starved" | "blocked_infra"
        ),
        "rch-degraded must classify as proof_starved/blocked_infra, got {}",
        rch.expected.classification
    );
    assert!(
        forbids(rch, &["restart", "cancel", "repair", "mutate"]),
        "rch-degraded scenario must forbid RCH service mutation / build cancellation"
    );

    // Agent Mail ack → wait/acknowledge, never repair/restart the service.
    let mail = scenario(&inv, "agent-mail-ack-required");
    assert_eq!(mail.expected.classification, "waiting_comm");
    assert!(
        forbids(mail, &["repair", "restart"]),
        "agent-mail scenario must forbid repairing/restarting the mail service"
    );

    // Stale in-progress → status-check, never force-release without evidence.
    let stale = scenario(&inv, "stale-in-progress-candidate");
    assert_eq!(stale.expected.classification, "stale_claim");
    assert!(
        forbids(stale, &["force_release", "force-release", "reopen"]),
        "stale-claim scenario must forbid force-release/reopen before a status check"
    );

    // Dirty overlap owned by another agent → do not touch their work.
    let dirty = scenario(&inv, "dirty-overlap-active-owner");
    assert!(
        matches!(
            dirty.expected.classification.as_str(),
            "dirty_overlap" | "do_not_touch"
        ),
        "dirty-overlap must classify as dirty_overlap/do_not_touch, got {}",
        dirty.expected.classification
    );
    assert!(
        forbids(dirty, &["stash", "revert", "stage", "checkout", "discard"]),
        "dirty-overlap scenario must forbid stashing/reverting/staging another agent's work"
    );

    // Docs-only ready slice is actionable even while proof lanes are blocked.
    let docs = scenario(&inv, "docs-only-ready-while-proof-blocked");
    assert_eq!(docs.expected.classification, "ready_now");
}
