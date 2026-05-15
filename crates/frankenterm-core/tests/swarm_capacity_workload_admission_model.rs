use frankenterm_core::runtime_telemetry::{
    HealthTier, SwarmCapacityAdmissionAction, SwarmCapacityAgentWorkloadClass,
    SwarmCapacityTelemetryGapState, SwarmCapacityWorkClass, SwarmCapacityWorkloadAdmissionInput,
    SwarmCapacityWorkloadAdmissionSignal, SwarmCapacityWorkloadAdmissionSignals,
    SwarmCapacityWorkloadEvidenceState, SwarmCapacityWorkloadSignalKind,
    plan_swarm_capacity_workload_admission, swarm_capacity_workload_admission_dry_run_examples,
    swarm_capacity_workload_admission_table,
};

fn signal_mut(
    signals: &mut SwarmCapacityWorkloadAdmissionSignals,
    kind: SwarmCapacityWorkloadSignalKind,
) -> &mut SwarmCapacityWorkloadAdmissionSignal {
    match kind {
        SwarmCapacityWorkloadSignalKind::ContextHorizon => &mut signals.context_horizon,
        SwarmCapacityWorkloadSignalKind::BlockerRadar => &mut signals.blocker_radar,
        SwarmCapacityWorkloadSignalKind::HerdWave => &mut signals.herd_wave,
        SwarmCapacityWorkloadSignalKind::ResourcePressure => &mut signals.resource_pressure,
    }
}

fn plan_one(input: SwarmCapacityWorkloadAdmissionInput) -> SwarmCapacityAdmissionAction {
    let plan = plan_swarm_capacity_workload_admission(1_700_000_000_000, "test", &[input]);
    plan.decisions.first().expect("one decision").action
}

fn normalize_integral_toon_numbers(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Array(items) => {
            for item in items {
                normalize_integral_toon_numbers(item);
            }
        }
        serde_json::Value::Object(map) => {
            for nested in map.values_mut() {
                normalize_integral_toon_numbers(nested);
            }
        }
        serde_json::Value::Number(number) => {
            if let Some(float) = number.as_f64() {
                #[allow(clippy::cast_precision_loss)]
                let max_u64 = u64::MAX as f64;
                if float.fract() == 0.0 && float >= 0.0 && float <= max_u64 {
                    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                    let as_u64 = float as u64;
                    *value = serde_json::Value::Number(serde_json::Number::from(as_u64));
                }
            }
        }
        serde_json::Value::Null | serde_json::Value::Bool(_) | serde_json::Value::String(_) => {}
    }
}

fn toon_roundtrip_json(value: &serde_json::Value) -> serde_json::Value {
    let toon_text = toon_rust::encode(value.clone(), None);
    let decoded = toon_rust::try_decode(&toon_text, None).expect("decode TOON");
    let decoded_json_text =
        toon_rust::cli::json_stringify::json_stringify_lines(&decoded, 0).join("\n");
    let mut roundtripped: serde_json::Value =
        serde_json::from_str(&decoded_json_text).expect("roundtrip JSON parse");
    normalize_integral_toon_numbers(&mut roundtripped);
    roundtripped
}

#[test]
fn workload_admission_table_covers_every_class() {
    let table = swarm_capacity_workload_admission_table();
    let classes = table
        .iter()
        .map(|row| row.workload_class)
        .collect::<Vec<_>>();
    assert_eq!(classes, SwarmCapacityAgentWorkloadClass::ALL);

    for row in table {
        assert_eq!(
            row.capacity_workload_class,
            row.workload_class.capacity_workload_class()
        );
        assert_eq!(row.work_class, row.workload_class.capacity_work_class());
        assert_eq!(
            row.measured_green_action,
            row.workload_class.baseline_admission_action()
        );
        assert_eq!(
            row.requested_units,
            row.workload_class.default_requested_units()
        );
        assert!(
            row.reason_codes
                .iter()
                .any(|reason| reason.ends_with(row.workload_class.as_str())),
            "{row:?}"
        );
    }
}

#[test]
fn green_evidence_uses_class_baseline_transitions() {
    for workload_class in SwarmCapacityAgentWorkloadClass::ALL {
        let input = SwarmCapacityWorkloadAdmissionInput::new(
            format!("green.{workload_class}"),
            100,
            workload_class,
        );
        let plan = plan_swarm_capacity_workload_admission(1_700_000_000_000, "test", &[input]);
        let decision = plan.decisions.first().expect("one decision");

        assert_eq!(decision.workload_class, workload_class);
        assert_eq!(decision.work_class, workload_class.capacity_work_class());
        assert_eq!(decision.action, workload_class.baseline_admission_action());
        assert!(!decision.side_effects_executed);
        if decision.action == SwarmCapacityAdmissionAction::Admit {
            assert_eq!(decision.admitted_units, decision.requested_units);
        } else {
            assert_eq!(decision.admitted_units, 0);
        }
    }
}

#[test]
fn stale_and_unavailable_signal_states_fail_closed_for_every_class() {
    for workload_class in SwarmCapacityAgentWorkloadClass::ALL {
        for kind in SwarmCapacityWorkloadSignalKind::ALL {
            for evidence_state in [
                SwarmCapacityWorkloadEvidenceState::Stale,
                SwarmCapacityWorkloadEvidenceState::Redacted,
                SwarmCapacityWorkloadEvidenceState::Contradictory,
                SwarmCapacityWorkloadEvidenceState::Unavailable,
            ] {
                let mut input = SwarmCapacityWorkloadAdmissionInput::new(
                    format!("{workload_class}.{kind:?}.{evidence_state:?}"),
                    200,
                    workload_class,
                );
                *signal_mut(&mut input.signals, kind) = SwarmCapacityWorkloadAdmissionSignal::new(
                    kind,
                    evidence_state,
                    HealthTier::Green,
                );

                let plan =
                    plan_swarm_capacity_workload_admission(1_700_000_000_000, "test", &[input]);
                let decision = plan.decisions.first().expect("one decision");

                assert_eq!(decision.evidence_state, evidence_state);
                assert!(
                    decision.action.conservatism_rank()
                        >= SwarmCapacityAdmissionAction::Defer.conservatism_rank(),
                    "{workload_class:?} {kind:?} {evidence_state:?} -> {:?}",
                    decision.action
                );
                assert!(
                    decision
                        .reason_codes
                        .iter()
                        .any(|reason| reason.contains(&format!(
                            "{}.fail_closed_{}",
                            kind.as_str(),
                            evidence_state.as_str()
                        ))),
                    "{:?}",
                    decision.reason_codes
                );
                assert!(decision.pause_admission, "{decision:?}");
                if matches!(
                    evidence_state,
                    SwarmCapacityWorkloadEvidenceState::Contradictory
                        | SwarmCapacityWorkloadEvidenceState::Unavailable
                ) {
                    assert_eq!(
                        decision.telemetry_gap_state,
                        SwarmCapacityTelemetryGapState::KillSwitch
                    );
                    assert!(decision.kill_switch_active, "{decision:?}");
                } else {
                    assert_eq!(
                        decision.telemetry_gap_state,
                        SwarmCapacityTelemetryGapState::PauseAdmission
                    );
                    assert!(!decision.kill_switch_active, "{decision:?}");
                }
            }
        }
    }
}

#[test]
fn telemetry_gap_state_machine_covers_open_stagger_pause_and_kill_switch() {
    let green = SwarmCapacityWorkloadAdmissionInput::new(
        "gap.green",
        50,
        SwarmCapacityAgentWorkloadClass::Coding,
    );
    let green_plan = plan_swarm_capacity_workload_admission(1_700_000_000_000, "test", &[green]);
    assert_eq!(
        green_plan.telemetry_gap_state,
        SwarmCapacityTelemetryGapState::Open
    );
    assert!(!green_plan.pause_admission);
    assert!(!green_plan.kill_switch_active);

    let mut herd_yellow = SwarmCapacityWorkloadAdmissionInput::new(
        "gap.herd_yellow",
        100,
        SwarmCapacityAgentWorkloadClass::Reviewing,
    );
    herd_yellow.signals.herd_wave = SwarmCapacityWorkloadAdmissionSignal::new(
        SwarmCapacityWorkloadSignalKind::HerdWave,
        SwarmCapacityWorkloadEvidenceState::Measured,
        HealthTier::Yellow,
    );
    let herd_plan =
        plan_swarm_capacity_workload_admission(1_700_000_000_001, "test", &[herd_yellow]);
    let herd_decision = herd_plan.decisions.first().expect("herd decision");
    assert_eq!(
        herd_plan.telemetry_gap_state,
        SwarmCapacityTelemetryGapState::StaggerRecommended
    );
    assert_eq!(
        herd_decision.telemetry_gap_state,
        SwarmCapacityTelemetryGapState::StaggerRecommended
    );
    assert!(!herd_plan.pause_admission);
    assert!(!herd_plan.kill_switch_active);
    assert_eq!(herd_decision.recommended_stagger_ms, Some(500));

    let mut stale_context = SwarmCapacityWorkloadAdmissionInput::new(
        "gap.stale_context",
        500,
        SwarmCapacityAgentWorkloadClass::Coding,
    );
    stale_context.signals.context_horizon = SwarmCapacityWorkloadAdmissionSignal::new(
        SwarmCapacityWorkloadSignalKind::ContextHorizon,
        SwarmCapacityWorkloadEvidenceState::Stale,
        HealthTier::Green,
    );
    let stale_plan =
        plan_swarm_capacity_workload_admission(1_700_000_000_002, "test", &[stale_context]);
    let stale_decision = stale_plan.decisions.first().expect("stale decision");
    assert_eq!(
        stale_plan.telemetry_gap_state,
        SwarmCapacityTelemetryGapState::PauseAdmission
    );
    assert!(stale_plan.pause_admission);
    assert!(!stale_plan.kill_switch_active);
    assert_eq!(stale_decision.action, SwarmCapacityAdmissionAction::Defer);
    assert_eq!(stale_decision.admitted_units, 0);

    let mut unavailable_resource = SwarmCapacityWorkloadAdmissionInput::new(
        "gap.unavailable_resource",
        500,
        SwarmCapacityAgentWorkloadClass::Building,
    );
    unavailable_resource.signals.resource_pressure = SwarmCapacityWorkloadAdmissionSignal::new(
        SwarmCapacityWorkloadSignalKind::ResourcePressure,
        SwarmCapacityWorkloadEvidenceState::Unavailable,
        HealthTier::Green,
    );
    let unavailable_plan =
        plan_swarm_capacity_workload_admission(1_700_000_000_003, "test", &[unavailable_resource]);
    let unavailable_decision = unavailable_plan
        .decisions
        .first()
        .expect("unavailable decision");
    assert_eq!(
        unavailable_plan.telemetry_gap_state,
        SwarmCapacityTelemetryGapState::KillSwitch
    );
    assert!(unavailable_plan.pause_admission);
    assert!(unavailable_plan.kill_switch_active);
    assert_eq!(
        unavailable_decision.action,
        SwarmCapacityAdmissionAction::Defer
    );
    assert_eq!(unavailable_decision.admitted_units, 0);
}

#[test]
fn evidence_degradation_never_upgrades_admission() {
    for workload_class in SwarmCapacityAgentWorkloadClass::ALL {
        for kind in SwarmCapacityWorkloadSignalKind::ALL {
            for pressure_tier in [
                HealthTier::Green,
                HealthTier::Yellow,
                HealthTier::Red,
                HealthTier::Black,
            ] {
                let mut measured =
                    SwarmCapacityWorkloadAdmissionInput::new("measured", 500, workload_class);
                *signal_mut(&mut measured.signals, kind) =
                    SwarmCapacityWorkloadAdmissionSignal::new(
                        kind,
                        SwarmCapacityWorkloadEvidenceState::Measured,
                        pressure_tier,
                    );

                let mut stale =
                    SwarmCapacityWorkloadAdmissionInput::new("stale", 500, workload_class);
                *signal_mut(&mut stale.signals, kind) = SwarmCapacityWorkloadAdmissionSignal::new(
                    kind,
                    SwarmCapacityWorkloadEvidenceState::Stale,
                    pressure_tier,
                );

                let mut unavailable =
                    SwarmCapacityWorkloadAdmissionInput::new("unavailable", 500, workload_class);
                *signal_mut(&mut unavailable.signals, kind) =
                    SwarmCapacityWorkloadAdmissionSignal::new(
                        kind,
                        SwarmCapacityWorkloadEvidenceState::Unavailable,
                        pressure_tier,
                    );

                let measured_rank = plan_one(measured).conservatism_rank();
                let stale_rank = plan_one(stale).conservatism_rank();
                let unavailable_rank = plan_one(unavailable).conservatism_rank();

                assert!(
                    measured_rank <= stale_rank,
                    "{workload_class:?} {kind:?} {pressure_tier:?}: measured={measured_rank} stale={stale_rank}"
                );
                assert!(
                    stale_rank <= unavailable_rank,
                    "{workload_class:?} {kind:?} {pressure_tier:?}: stale={stale_rank} unavailable={unavailable_rank}"
                );
            }
        }
    }
}

#[test]
fn workload_inputs_map_to_existing_capacity_requests() {
    let mut input = SwarmCapacityWorkloadAdmissionInput::new(
        "build-heavy",
        200,
        SwarmCapacityAgentWorkloadClass::Building,
    );
    input.queue_depth = 17;
    input.backlog_depth = 65;
    input.pane_priority = 88;
    input.workflow_priority = 144;

    let request = input.admission_request(42);

    assert_eq!(request.stable_id, "build-heavy");
    assert_eq!(
        request.workload_class,
        SwarmCapacityAgentWorkloadClass::Building.capacity_workload_class()
    );
    assert_eq!(request.work_class, SwarmCapacityWorkClass::ClaimedAgentTask);
    assert_eq!(request.arrival_sequence, 42);
    assert_eq!(request.queue_depth, 17);
    assert_eq!(request.backlog_depth, 65);
    assert_eq!(request.pane_priority, 88);
    assert_eq!(request.workflow_priority, 144);
    assert_eq!(
        request.requested_units,
        SwarmCapacityAgentWorkloadClass::Building.default_requested_units()
    );
}

#[test]
fn invalid_workload_evidence_state_json_is_rejected() {
    let invalid = serde_json::json!({
        "kind": "context_horizon",
        "evidence_state": "fresh",
        "pressure_tier": "green",
        "reason_codes": ["capacity.workload.context_horizon.fresh"]
    });

    let err = serde_json::from_value::<SwarmCapacityWorkloadAdmissionSignal>(invalid)
        .expect_err("unknown evidence state must be rejected");
    assert!(
        err.to_string().contains("unknown variant") || err.to_string().contains("expected one of"),
        "unexpected serde error: {err}"
    );
}

#[test]
fn dry_run_examples_cover_declared_scales_and_toon_parity() {
    let plan = swarm_capacity_workload_admission_dry_run_examples(1_700_000_000_000);
    let scales = plan
        .decisions
        .iter()
        .map(|decision| decision.pane_scale)
        .collect::<Vec<_>>();
    assert_eq!(scales, vec![50, 100, 200, 500]);
    assert_eq!(
        plan.decisions[0].action,
        SwarmCapacityAdmissionAction::Admit
    );
    assert_eq!(
        plan.decisions[1].recommended_stagger_ms,
        Some(500),
        "100-pane herd-wave yellow example should stagger without deferring"
    );
    assert_eq!(
        plan.decisions[2].action,
        SwarmCapacityAdmissionAction::Defer
    );
    assert_eq!(
        plan.decisions[3].evidence_state,
        SwarmCapacityWorkloadEvidenceState::Stale
    );

    let json_value = serde_json::to_value(&plan).expect("serialize plan to JSON");
    let roundtripped = toon_roundtrip_json(&json_value);
    assert_eq!(json_value, roundtripped);

    let roundtripped_decisions = roundtripped["decisions"]
        .as_array()
        .expect("roundtripped decisions");
    assert_eq!(roundtripped_decisions.len(), plan.decisions.len());
    assert_eq!(
        roundtripped_decisions[0]["stable_id"].as_str(),
        Some("example.50.coding")
    );
    assert_eq!(roundtripped_decisions[0]["pane_scale"].as_f64(), Some(50.0));
    assert_eq!(
        roundtripped_decisions[1]["recommended_stagger_ms"].as_f64(),
        Some(500.0)
    );
    assert_eq!(roundtripped_decisions[2]["action"].as_str(), Some("defer"));
    assert_eq!(
        roundtripped_decisions[3]["evidence_state"].as_str(),
        Some("stale")
    );

    let jsonl = plan
        .decisions
        .iter()
        .map(serde_json::to_string)
        .collect::<Result<Vec<_>, _>>()
        .expect("serialize JSONL")
        .join("\n");
    assert_eq!(jsonl.lines().count(), 4);
    for line in jsonl.lines() {
        let row: serde_json::Value = serde_json::from_str(line).expect("parse JSONL row");
        assert!(row.get("stable_id").is_some(), "{row}");
        assert_eq!(
            row.get("side_effects_executed").and_then(|v| v.as_bool()),
            Some(false)
        );
    }

    let serialized = serde_json::to_string(&plan).expect("serialize plan");
    for forbidden in [
        "raw_transcript",
        "prompt_body",
        "password=",
        concat!("sk-", "proj-"),
    ] {
        assert!(!serialized.contains(forbidden), "{forbidden}");
    }
    assert!(!plan.raw_pane_content_stored);
    assert!(!plan.side_effects_executed);
}
