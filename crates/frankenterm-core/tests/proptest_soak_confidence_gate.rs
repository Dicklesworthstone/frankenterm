//! Property tests for soak_confidence_gate module (ft-e34d9.10.8.5).
//!
//! Covers serde roundtrips, SoakMatrix plan generation, execution result
//! counter arithmetic, confidence gate evaluation logic, and standard
//! factory invariants.

use std::path::PathBuf;

use frankenterm_core::soak_confidence_gate::*;
use proptest::prelude::*;

// =============================================================================
// Strategies
// =============================================================================

fn arb_journey_category() -> impl Strategy<Value = JourneyCategory> {
    (0..JourneyCategory::ALL.len()).prop_map(|i| JourneyCategory::ALL[i])
}

fn arb_workload_profile() -> impl Strategy<Value = WorkloadProfile> {
    (0..WorkloadProfile::ALL.len()).prop_map(|i| WorkloadProfile::ALL[i])
}

fn arb_failure_injection() -> impl Strategy<Value = FailureInjectionProfile> {
    (0..FailureInjectionProfile::ALL.len()).prop_map(|i| FailureInjectionProfile::ALL[i])
}

fn arb_confidence_decision() -> impl Strategy<Value = ConfidenceDecision> {
    prop_oneof![
        Just(ConfidenceDecision::Confident),
        Just(ConfidenceDecision::ConditionallyConfident),
        Just(ConfidenceDecision::NotConfident),
    ]
}

fn _arb_cell_result(passed: bool) -> impl Strategy<Value = CellResult> {
    (
        "[a-z-]{3,15}",
        arb_journey_category(),
        arb_workload_profile(),
        arb_failure_injection(),
        any::<bool>(),
        0..10000u64,
        0.0..0.5f64,
        0.0..500.0f64,
    )
        .prop_map(
            move |(id, cat, wl, inj, blocking, dur, err_rate, p95)| CellResult {
                cell_id: id,
                category: cat,
                workload: wl,
                injection: inj,
                passed,
                blocking,
                duration_ms: dur,
                failure_reason: if passed {
                    None
                } else {
                    Some("test fail".into())
                },
                error_rate: err_rate,
                p95_latency_ms: p95,
                seed: None,
                telemetry: CellTelemetry::default(),
            },
        )
}

// =============================================================================
// Serde roundtrips
// =============================================================================

proptest! {
    #[test]
    fn serde_roundtrip_journey_category(cat in arb_journey_category()) {
        let json = serde_json::to_string(&cat).unwrap();
        let back: JourneyCategory = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(cat, back);
    }

    #[test]
    fn serde_roundtrip_workload_profile(wl in arb_workload_profile()) {
        let json = serde_json::to_string(&wl).unwrap();
        let back: WorkloadProfile = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(wl, back);
    }

    #[test]
    fn serde_roundtrip_failure_injection(inj in arb_failure_injection()) {
        let json = serde_json::to_string(&inj).unwrap();
        let back: FailureInjectionProfile = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(inj, back);
    }

    #[test]
    fn serde_roundtrip_confidence_decision(dec in arb_confidence_decision()) {
        let json = serde_json::to_string(&dec).unwrap();
        let back: ConfidenceDecision = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(dec, back);
    }
}

#[test]
fn custom_matrix_expansion_is_checked_and_bounded() {
    let scenario = UserJourneyScenario {
        scenario_id: "duplicate".into(),
        category: JourneyCategory::Watch,
        description: "duplicate scenario validation".into(),
        expected_duration_ms: 1,
        blocking: true,
        seed: Some(1),
    };
    let duplicate = SoakMatrix::custom(
        vec![scenario.clone(), scenario],
        vec![WorkloadProfile::Steady],
        vec![FailureInjectionProfile::None],
    );
    assert!(matches!(
        duplicate.to_plan(),
        Err(SoakMatrixPlanError::DuplicateScenarioId { .. })
    ));

    let scenarios = (0_u64..4_097)
        .map(|index| UserJourneyScenario {
            scenario_id: format!("bounded-{index}"),
            category: JourneyCategory::Watch,
            description: "bounded matrix scenario".into(),
            expected_duration_ms: 1,
            blocking: true,
            seed: Some(index),
        })
        .collect();
    let oversized = SoakMatrix::custom(
        scenarios,
        vec![WorkloadProfile::Steady],
        vec![FailureInjectionProfile::None],
    );
    assert!(matches!(
        oversized.to_plan(),
        Err(SoakMatrixPlanError::CellCountLimit {
            count: 4_097,
            maximum: 4_096
        })
    ));
}

// =============================================================================
// Deterministic long-haul workload corpus
// =============================================================================

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn long_haul_corpus() -> SoakWorkloadCorpus {
    let path = repo_root().join(SOAK_WORKLOAD_CORPUS_FIXTURE);
    let json = std::fs::read_to_string(path).expect("read tracked soak workload corpus");
    parse_soak_workload_corpus(&json).expect("tracked soak workload corpus validates")
}

#[test]
fn long_haul_corpus_assets_are_content_addressed() {
    let corpus = long_haul_corpus();
    verify_soak_workload_assets(&repo_root(), &corpus)
        .expect("all deterministic actor assets match their sha256 pins");
    let plan = materialize_soak_workload_plan(&corpus, 20).expect("20-pane plan");
    verify_soak_workload_plan_assets(&repo_root(), &plan)
        .expect("materialized plan preserves verifiable asset pins and modes");
    assert_eq!(corpus.version, SOAK_WORKLOAD_CORPUS_VERSION);
    assert!(corpus.dogfood.excluded_from_deterministic_verdict);
}

#[test]
fn corpus_actor_activation_and_burst_envelope_are_explicit() {
    let corpus = long_haul_corpus();
    let persistent = corpus
        .actors
        .iter()
        .filter(|actor| actor.activation == SoakActorActivation::Persistent)
        .map(|actor| actor.actor_id.as_str())
        .collect::<std::collections::BTreeSet<_>>();
    let expected_persistent = ["agent-like-stream", "editor-tui"]
        .into_iter()
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(persistent, expected_persistent);
    for actor in corpus
        .actors
        .iter()
        .filter(|actor| actor.activation == SoakActorActivation::Persistent)
    {
        let timeout_seconds = actor
            .command
            .args
            .last()
            .expect("persistent actor timeout argument")
            .parse::<u64>()
            .expect("numeric persistent actor timeout");
        assert!(timeout_seconds * 1_000 >= 630_000);
    }

    let burst = corpus
        .actors
        .iter()
        .find(|actor| actor.actor_id == "output-burst")
        .expect("output burst actor");
    assert_eq!(burst.activation, SoakActorActivation::Scheduled);
    let line_count = burst
        .command
        .args
        .first()
        .expect("burst line count argument")
        .parse::<u64>()
        .expect("numeric burst line count");
    let marker = burst.command.args.get(1).expect("burst marker argument");
    let bytes_per_action = (1..=line_count)
        .map(|line| {
            u64::try_from(format!("Line {line}: {marker}\n").len())
                .expect("line byte count fits u64")
        })
        .sum::<u64>();
    let burst_phase = corpus
        .phases
        .iter()
        .find(|phase| phase.phase_id == "adversarial-burst")
        .expect("adversarial burst phase");
    let actions_per_second = 1_000 / burst_phase.action_cadence_ms;
    assert_eq!(
        bytes_per_action * actions_per_second,
        burst.output_bytes_per_second
    );
}

#[test]
fn long_haul_plans_are_deterministic_at_every_required_scale() {
    let corpus = long_haul_corpus();
    for pane_count in SOAK_WORKLOAD_REQUIRED_PANE_COUNTS {
        let first =
            materialize_soak_workload_plan(&corpus, *pane_count).expect("materialize first plan");
        let second =
            materialize_soak_workload_plan(&corpus, *pane_count).expect("materialize second plan");
        assert_eq!(first, second);
        assert_eq!(first.pane_count, *pane_count);
        assert_eq!(
            u32::try_from(first.identities.len()).expect("identity count fits u32"),
            *pane_count
        );
        assert_eq!(first.plan_sha256.len(), 64);
        assert_eq!(first.base_seed, corpus.base_seed);
        assert_eq!(first.phases.len(), corpus.phases.len());
        assert_ne!(
            first.actions,
            [] as [frankenterm_core::soak_confidence_gate::SoakScheduledAction; 0]
        );
        assert_eq!(
            u32::try_from(
                first
                    .identities
                    .iter()
                    .filter(|identity| identity.interactive)
                    .count()
            )
            .expect("interactive identity count fits u32"),
            first.interactive_pane_count
        );
        assert_eq!(first.assets.len(), corpus.assets.len());
        let interactive_dimensions = first
            .identities
            .iter()
            .filter(|identity| identity.interactive)
            .map(|identity| identity.dimension)
            .collect::<std::collections::BTreeSet<_>>();
        let priority_prefix_len = usize::try_from(first.interactive_pane_count)
            .expect("interactive count fits usize")
            .min(first.interactive_dimension_priority.len());
        assert!(
            first.interactive_dimension_priority[..priority_prefix_len]
                .iter()
                .all(|dimension| interactive_dimensions.contains(dimension))
        );
        assert!(interactive_dimensions.contains(&SoakWorkloadDimension::EditorTui));
        assert!(
            first
                .identities
                .iter()
                .all(|identity| !identity.expected_final_marker.is_empty())
        );

        let identity_ids = first
            .identities
            .iter()
            .map(|identity| identity.identity_id.as_str())
            .collect::<std::collections::BTreeSet<_>>();
        let actor_seeds = first
            .identities
            .iter()
            .map(|identity| identity.actor_seed)
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(identity_ids.len(), first.identities.len());
        assert_eq!(actor_seeds.len(), first.identities.len());

        let json = serde_json::to_string(&first).expect("serialize plan");
        let roundtrip: SoakWorkloadPlan = serde_json::from_str(&json).expect("parse plan");
        assert_eq!(roundtrip, first);

        let summary = replay_soak_workload_plan(&first, None).expect("logical replay");
        let repeated_summary =
            replay_soak_workload_plan(&first, None).expect("repeated logical replay");
        assert_eq!(repeated_summary, summary);
        assert_eq!(summary.summary_sha256.len(), 64);
        assert!(summary.teardown_complete);
        assert!(summary.all_dimensions_observed);
        assert_eq!(summary.actions_skipped, 0);
        assert_eq!(summary.remaining_workspaces, 0);
        assert_eq!(summary.remaining_windows, 0);
        assert_eq!(summary.remaining_tabs, 0);
        assert_eq!(summary.remaining_panes, 0);
        assert_eq!(summary.remaining_actors, 0);
        assert!(
            summary
                .logical_oracle_results
                .values()
                .all(|passed| *passed)
        );
        assert_eq!(summary.production_runner_oracles.len(), 6);
    }
}

#[test]
fn corpus_input_order_does_not_change_fleet_identity_or_plan_hash() {
    let corpus = long_haul_corpus();
    let expected = materialize_soak_workload_plan(&corpus, 200).expect("canonical plan");

    let mut reordered = corpus;
    reordered.assets.reverse();
    reordered.actors.reverse();
    for actor in &mut reordered.actors {
        actor.asset_ids.reverse();
    }
    reordered.scales.reverse();
    for scale in &mut reordered.scales {
        scale.allocations.reverse();
    }
    reordered.phases.reverse();
    for phase in &mut reordered.phases {
        phase.dimensions.reverse();
    }
    reordered.final_oracles.reverse();
    reordered.dogfood.required_identity_fields.reverse();

    validate_soak_workload_corpus(&reordered).expect("input collections are order-independent");
    let actual = materialize_soak_workload_plan(&reordered, 200).expect("reordered plan");
    assert_eq!(actual, expected);
    assert_eq!(actual.plan_sha256, expected.plan_sha256);
}

#[test]
fn failed_actor_still_reaches_exact_teardown_state() {
    let corpus = long_haul_corpus();
    let plan = materialize_soak_workload_plan(&corpus, 20).expect("20-pane plan");
    let failed = plan
        .identities
        .iter()
        .find(|identity| identity.actor_id == "images")
        .expect("images actor identity");
    let summary = replay_soak_workload_plan(&plan, Some(&failed.identity_id))
        .expect("logical failure replay");

    assert!(summary.actions_skipped > 0);
    assert!(summary.teardown_complete);
    assert!(!summary.all_dimensions_observed);
    assert_eq!(summary.remaining_actors, 0);
    assert_eq!(summary.remaining_panes, 0);
    assert_eq!(
        summary.failed_identity_id.as_deref(),
        Some(failed.identity_id.as_str())
    );
}

#[test]
fn replay_rejects_unknown_failure_and_tampered_plan() {
    let corpus = long_haul_corpus();
    let mut plan = materialize_soak_workload_plan(&corpus, 20).expect("20-pane plan");
    assert!(replay_soak_workload_plan(&plan, Some("unknown-identity")).is_err());

    plan.actions[0].payload_profile.push_str("-tampered");
    let error = replay_soak_workload_plan(&plan, None).expect_err("action mismatch must fail");
    assert!(error.to_string().contains("phase contract"));

    let mut digest_tamper = materialize_soak_workload_plan(&corpus, 20).expect("fresh plan");
    let replacement = if digest_tamper.plan_sha256.starts_with('0') {
        "1"
    } else {
        "0"
    };
    digest_tamper.plan_sha256.replace_range(..1, replacement);
    let error = validate_soak_workload_plan(&digest_tamper).expect_err("digest mismatch must fail");
    assert!(error.to_string().contains("plan sha256 mismatch"));
}

#[test]
fn materialized_plan_rejects_rehashed_structural_tampering_before_digest_authority() {
    let corpus = long_haul_corpus();
    let plan = materialize_soak_workload_plan(&corpus, 20).expect("20-pane plan");

    let mut wrong_parent = plan.clone();
    wrong_parent.setup[1].parent_id = None;
    assert!(
        validate_soak_workload_plan(&wrong_parent)
            .expect_err("wrong lifecycle parent must fail")
            .to_string()
            .contains("lifecycle resources")
    );

    let mut wrong_seed = plan.clone();
    wrong_seed.identities[0].actor_seed ^= 1;
    assert!(
        validate_soak_workload_plan(&wrong_seed)
            .expect_err("wrong actor seed must fail")
            .to_string()
            .contains("actor seed")
    );

    let mut missing_action = plan.clone();
    missing_action.actions.pop();
    assert!(
        validate_soak_workload_plan(&missing_action)
            .expect_err("missing scheduled action must fail")
            .to_string()
            .contains("phase contract")
    );

    let mut phase_gap = plan.clone();
    phase_gap.phases[1].start_offset_ms += 1;
    assert!(
        validate_soak_workload_plan(&phase_gap)
            .expect_err("phase gap must fail")
            .to_string()
            .contains("preceding phase boundary")
    );

    let mut inconsistent_actor = plan.clone();
    let second_quiet = inconsistent_actor
        .identities
        .iter_mut()
        .filter(|identity| identity.actor_id == "quiet-shell")
        .nth(1)
        .expect("second quiet-shell identity");
    second_quiet.output_bytes_per_second = 1;
    assert!(
        validate_soak_workload_plan(&inconsistent_actor)
            .expect_err("inconsistent actor templates must fail")
            .to_string()
            .contains("inconsistent materialized templates")
    );

    let mut noncanonical_assets = plan.clone();
    noncanonical_assets.assets.swap(0, 1);
    assert!(
        validate_soak_workload_plan(&noncanonical_assets)
            .expect_err("non-canonical assets must fail")
            .to_string()
            .contains("assets are not canonical")
    );

    let mut noncanonical_oracles = plan;
    noncanonical_oracles.final_oracles.swap(0, 1);
    assert!(
        validate_soak_workload_plan(&noncanonical_oracles)
            .expect_err("non-canonical oracles must fail")
            .to_string()
            .contains("oracles are not canonical")
    );
}

#[test]
fn malformed_workload_contracts_fail_closed() {
    let corpus = long_haul_corpus();

    let mut wrong_version = corpus.clone();
    wrong_version.version = "ft.soak_workload_corpus.v2".into();
    assert!(validate_soak_workload_corpus(&wrong_version).is_err());

    let mut wrong_allocation = corpus.clone();
    wrong_allocation.scales[0].allocations[0].pane_count += 1;
    assert!(validate_soak_workload_corpus(&wrong_allocation).is_err());

    let mut duplicate_phase_dimension = corpus.clone();
    let duplicate = duplicate_phase_dimension.phases[0].dimensions[0];
    duplicate_phase_dimension.phases[0]
        .dimensions
        .push(duplicate);
    assert!(validate_soak_workload_corpus(&duplicate_phase_dimension).is_err());

    let mut unsafe_output = corpus.clone();
    unsafe_output.actors[0].output_bytes_per_second = u64::MAX;
    assert!(validate_soak_workload_corpus(&unsafe_output).is_err());

    let mut incomplete_interactive_priority = corpus.clone();
    incomplete_interactive_priority
        .interactive_dimension_priority
        .pop();
    assert!(validate_soak_workload_corpus(&incomplete_interactive_priority).is_err());

    let mut non_executable_program = corpus.clone();
    non_executable_program
        .assets
        .iter_mut()
        .find(|asset| asset.asset_id == "dummy-agent-v1")
        .expect("dummy agent asset")
        .executable = false;
    assert!(validate_soak_workload_corpus(&non_executable_program).is_err());

    let mut invalid_persistent_builtin = corpus.clone();
    let quiet = invalid_persistent_builtin
        .actors
        .iter_mut()
        .find(|actor| actor.actor_id == "quiet-shell")
        .expect("quiet-shell actor");
    quiet.activation = SoakActorActivation::Persistent;
    assert!(validate_soak_workload_corpus(&invalid_persistent_builtin).is_err());

    let mut unknown_adapter = corpus.clone();
    unknown_adapter
        .actors
        .iter_mut()
        .find(|actor| actor.dimension == SoakWorkloadDimension::QuietShell)
        .expect("quiet shell actor")
        .command
        .program = "ft.soak.unknown.v1".into();
    assert!(validate_soak_workload_corpus(&unknown_adapter).is_err());

    let mut oversized_burst = corpus.clone();
    oversized_burst
        .actors
        .iter_mut()
        .find(|actor| actor.dimension == SoakWorkloadDimension::OutputBurst)
        .expect("output burst actor")
        .command
        .args[0] = "1000001".into();
    assert!(validate_soak_workload_corpus(&oversized_burst).is_err());

    let mut underdeclared_output = corpus.clone();
    underdeclared_output
        .actors
        .iter_mut()
        .find(|actor| actor.dimension == SoakWorkloadDimension::OutputBurst)
        .expect("output burst actor")
        .output_bytes_per_second -= 1;
    assert!(validate_soak_workload_corpus(&underdeclared_output).is_err());

    let mut wrong_shutdown = corpus.clone();
    wrong_shutdown
        .actors
        .iter_mut()
        .find(|actor| actor.dimension == SoakWorkloadDimension::AgentLikeStream)
        .expect("agent actor")
        .shutdown = Some(SoakPersistentShutdown::Interrupt);
    assert!(validate_soak_workload_corpus(&wrong_shutdown).is_err());

    let mut dogfood_promoted = corpus.clone();
    dogfood_promoted.dogfood.excluded_from_deterministic_verdict = false;
    assert!(validate_soak_workload_corpus(&dogfood_promoted).is_err());

    let mut wrong_oracle_authority = corpus.clone();
    wrong_oracle_authority
        .final_oracles
        .iter_mut()
        .find(|oracle| oracle.oracle == SoakFinalOracleKind::FinalTerminalStateHash)
        .expect("terminal hash oracle")
        .authority = SoakOracleAuthority::LogicalReplay;
    assert!(validate_soak_workload_corpus(&wrong_oracle_authority).is_err());

    let mut traversal = corpus;
    traversal.assets[0].path = "../outside".into();
    assert!(validate_soak_workload_corpus(&traversal).is_err());

    let mut nonportable_path = long_haul_corpus();
    nonportable_path.assets[0].path = "fixtures\\outside".into();
    assert!(validate_soak_workload_corpus(&nonportable_path).is_err());

    let mut unused_asset = long_haul_corpus();
    let mut extra = unused_asset.assets[0].clone();
    extra.asset_id = "unused-asset".into();
    extra.path = "fixtures/perf/unused-asset.txt".into();
    unused_asset.assets.push(extra);
    assert!(
        validate_soak_workload_corpus(&unused_asset)
            .expect_err("unreferenced asset must fail")
            .to_string()
            .contains("referenced")
    );
}

#[test]
fn corpus_json_rejects_unknown_fields() {
    let path = repo_root().join(SOAK_WORKLOAD_CORPUS_FIXTURE);
    let mut value: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(path).expect("read tracked soak workload corpus"),
    )
    .expect("parse fixture as JSON value");
    value
        .as_object_mut()
        .expect("corpus object")
        .insert("base_sead".into(), serde_json::json!(7));
    assert!(serde_json::from_value::<SoakWorkloadCorpus>(value).is_err());
}

#[test]
fn legacy_soak_matrix_uses_typed_drivers_not_fabricated_shell_commands() {
    let matrix = SoakMatrix::standard();
    let plan = matrix.to_plan().expect("valid standard matrix");
    let matrix_json = serde_json::to_string(&matrix).expect("serialize standard matrix");
    let plan_json = serde_json::to_string(&plan).expect("serialize standard plan");
    assert!(plan_json.contains("\"driver\""));
    assert!(!matrix_json.contains("\"command\""));
    assert!(!matrix_json.contains("cargo test --test soak_"));
    assert!(plan.cells.iter().all(|cell| {
        let scenario = matrix
            .scenarios
            .iter()
            .find(|scenario| scenario.scenario_id == cell.scenario_id)
            .expect("cell scenario");
        cell.driver == JourneyDriver::from(scenario.category)
    }));
}

// =============================================================================
// JourneyCategory invariants
// =============================================================================

proptest! {
    #[test]
    fn journey_category_label_nonempty(cat in arb_journey_category()) {
        prop_assert!(!cat.label().is_empty());
    }

    #[test]
    fn journey_category_critical_deterministic(cat in arb_journey_category()) {
        prop_assert_eq!(cat.is_critical(), cat.is_critical());
    }
}

// =============================================================================
// SoakMatrix invariants
// =============================================================================

proptest! {
    #[test]
    fn matrix_cell_count_is_product(
        n_scenarios in 1..5usize,
        n_workloads in 1..4usize,
        n_injections in 1..4usize,
    ) {
        let scenarios: Vec<UserJourneyScenario> = (0..n_scenarios)
            .map(|i| UserJourneyScenario {
                scenario_id: format!("s-{i}"),
                category: JourneyCategory::ALL[i % JourneyCategory::ALL.len()],
                description: "test".into(),
                expected_duration_ms: 1000,
                blocking: true,
                seed: None,
            })
            .collect();

        let workloads: Vec<WorkloadProfile> = WorkloadProfile::ALL[..n_workloads].to_vec();
        let injections: Vec<FailureInjectionProfile> = FailureInjectionProfile::ALL[..n_injections].to_vec();

        let matrix = SoakMatrix::custom(scenarios, workloads, injections);
        let expected = n_scenarios * n_workloads * n_injections;
        prop_assert_eq!(matrix.cell_count().expect("valid generated matrix"), expected,
            "cell_count should be scenarios * workloads * injections: {} * {} * {} = {}",
            n_scenarios, n_workloads, n_injections, expected);
    }

    #[test]
    fn matrix_plan_cell_count_matches(
        n_scenarios in 1..4usize,
    ) {
        let scenarios: Vec<UserJourneyScenario> = (0..n_scenarios)
            .map(|i| UserJourneyScenario {
                scenario_id: format!("s-{i}"),
                category: JourneyCategory::ALL[i % JourneyCategory::ALL.len()],
                description: "test".into(),
                expected_duration_ms: 1000,
                blocking: true,
                seed: None,
            })
            .collect();

        let matrix = SoakMatrix::custom(
            scenarios,
            WorkloadProfile::ALL.to_vec(),
            FailureInjectionProfile::ALL.to_vec(),
        );
        let plan = matrix.to_plan().expect("valid generated matrix");
        prop_assert_eq!(plan.total_cells(), matrix.cell_count().expect("valid generated matrix"));
    }
}

// =============================================================================
// Execution result counter arithmetic
// =============================================================================

#[test]
fn completion_before_start_is_rejected_without_mutating_result() {
    let mut result = SoakExecutionResult::new(1_000);
    let error = result
        .complete(999)
        .expect_err("backwards completion time must fail");
    assert_eq!(error.started_at_ms, 1_000);
    assert_eq!(error.completed_at_ms, 999);
    assert_eq!(result.completed_at_ms, 0);
    assert_eq!(result.total_duration_ms, 0);
}

proptest! {
    #[test]
    fn execution_pass_fail_sum(
        n_pass in 0..10usize,
        n_fail in 0..10usize,
    ) {
        let total = n_pass + n_fail;
        if total == 0 {
            return Ok(());
        }

        let mut exec = SoakExecutionResult::new(0);

        for i in 0..n_pass {
            exec.record_cell(CellResult {
                cell_id: format!("p-{i}"),
                category: JourneyCategory::Watch,
                workload: WorkloadProfile::Steady,
                injection: FailureInjectionProfile::None,
                passed: true,
                blocking: true,
                duration_ms: 100,
                failure_reason: None,
                error_rate: 0.0,
                p95_latency_ms: 10.0,
                seed: None,
                telemetry: CellTelemetry::default(),
            });
        }

        for i in 0..n_fail {
            exec.record_cell(CellResult {
                cell_id: format!("f-{i}"),
                category: JourneyCategory::Watch,
                workload: WorkloadProfile::Steady,
                injection: FailureInjectionProfile::None,
                passed: false,
                blocking: true,
                duration_ms: 100,
                failure_reason: Some("err".into()),
                error_rate: 0.1,
                p95_latency_ms: 50.0,
                seed: None,
                telemetry: CellTelemetry::default(),
            });
        }

        exec.complete(1000).expect("valid completion time");

        prop_assert_eq!(exec.cells_passed(), n_pass);
        prop_assert_eq!(exec.cells_failed(), n_fail);

        let rate = exec.pass_rate();
        if total > 0 {
            let expected = n_pass as f64 / total as f64;
            prop_assert!((rate - expected).abs() < 1e-10);
        }
    }

    #[test]
    fn blocking_failures_subset_of_failures(
        n_pass in 0..5usize,
        n_blocking_fail in 0..3usize,
        n_nonblocking_fail in 0..3usize,
    ) {
        let mut exec = SoakExecutionResult::new(0);

        for i in 0..n_pass {
            exec.record_cell(CellResult {
                cell_id: format!("p-{i}"),
                category: JourneyCategory::Watch,
                workload: WorkloadProfile::Steady,
                injection: FailureInjectionProfile::None,
                passed: true,
                blocking: true,
                duration_ms: 100,
                failure_reason: None,
                error_rate: 0.0,
                p95_latency_ms: 10.0,
                seed: None,
                telemetry: CellTelemetry::default(),
            });
        }

        for i in 0..n_blocking_fail {
            exec.record_cell(CellResult {
                cell_id: format!("bf-{i}"),
                category: JourneyCategory::Watch,
                workload: WorkloadProfile::Steady,
                injection: FailureInjectionProfile::None,
                passed: false,
                blocking: true,
                duration_ms: 100,
                failure_reason: Some("err".into()),
                error_rate: 0.1,
                p95_latency_ms: 50.0,
                seed: None,
                telemetry: CellTelemetry::default(),
            });
        }

        for i in 0..n_nonblocking_fail {
            exec.record_cell(CellResult {
                cell_id: format!("nf-{i}"),
                category: JourneyCategory::Watch,
                workload: WorkloadProfile::Steady,
                injection: FailureInjectionProfile::None,
                passed: false,
                blocking: false,
                duration_ms: 100,
                failure_reason: Some("minor".into()),
                error_rate: 0.05,
                p95_latency_ms: 30.0,
                seed: None,
                telemetry: CellTelemetry::default(),
            });
        }

        exec.complete(1000).expect("valid completion time");
        let blocking = exec.blocking_failures();
        prop_assert!(blocking <= exec.cells_failed());
        prop_assert_eq!(blocking, n_blocking_fail);
    }
}

// =============================================================================
// Confidence gate evaluation
// =============================================================================

proptest! {
    #[test]
    fn gate_evaluation_deterministic(
        n_pass in 1..5usize,
        n_fail in 0..3usize,
    ) {
        let mut exec = SoakExecutionResult::new(0);
        for i in 0..n_pass {
            exec.record_cell(CellResult {
                cell_id: format!("p-{i}"),
                category: JourneyCategory::Watch,
                workload: WorkloadProfile::Steady,
                injection: FailureInjectionProfile::None,
                passed: true,
                blocking: true,
                duration_ms: 100,
                failure_reason: None,
                error_rate: 0.0,
                p95_latency_ms: 10.0,
                seed: None,
                telemetry: CellTelemetry::default(),
            });
        }
        for i in 0..n_fail {
            exec.record_cell(CellResult {
                cell_id: format!("f-{i}"),
                category: JourneyCategory::Watch,
                workload: WorkloadProfile::Steady,
                injection: FailureInjectionProfile::None,
                passed: false,
                blocking: true,
                duration_ms: 100,
                failure_reason: Some("err".into()),
                error_rate: 0.1,
                p95_latency_ms: 50.0,
                seed: None,
                telemetry: CellTelemetry::default(),
            });
        }
        exec.complete(1000).expect("valid completion time");

        let gate = ConfidenceGate::standard();
        let v1 = gate.evaluate(&exec);
        let v2 = gate.evaluate(&exec);
        prop_assert_eq!(v1.decision, v2.decision, "evaluation should be deterministic");
    }
}

// =============================================================================
// Standard factories
// =============================================================================

#[test]
fn standard_matrix_has_cells() {
    let matrix = SoakMatrix::standard();
    assert!(matrix.cell_count().expect("valid standard matrix") > 0);
    assert!(matrix.blocking_scenario_count() > 0);
}

#[test]
fn ci_minimal_matrix_is_smaller() {
    let standard = SoakMatrix::standard();
    let ci = SoakMatrix::ci_minimal();
    assert!(
        ci.cell_count().expect("valid CI matrix")
            <= standard.cell_count().expect("valid standard matrix")
    );
}

#[test]
fn standard_gate_thresholds_reasonable() {
    let gate = ConfidenceGate::standard();
    assert!(gate.min_pass_rate > 0.0 && gate.min_pass_rate <= 1.0);
    assert!(gate.max_error_rate >= 0.0 && gate.max_error_rate <= 1.0);
    assert!(gate.max_p95_latency_ms > 0.0);
}

#[test]
fn strict_gate_stricter_than_standard() {
    let standard = ConfidenceGate::standard();
    let strict = ConfidenceGate::strict();
    assert!(strict.min_pass_rate >= standard.min_pass_rate);
    assert!(strict.max_error_rate <= standard.max_error_rate);
}

#[test]
fn confidence_verdict_summary_renders() {
    let mut exec = SoakExecutionResult::new(0);
    exec.record_cell(CellResult {
        cell_id: "test".into(),
        category: JourneyCategory::Watch,
        workload: WorkloadProfile::Steady,
        injection: FailureInjectionProfile::None,
        passed: true,
        blocking: true,
        duration_ms: 100,
        failure_reason: None,
        error_rate: 0.0,
        p95_latency_ms: 10.0,
        seed: None,
        telemetry: CellTelemetry::default(),
    });
    exec.complete(1000).expect("valid completion time");

    let gate = ConfidenceGate::standard();
    let verdict = gate.evaluate(&exec);
    let summary = verdict.render_summary();
    assert_ne!(summary, "");
}

// =============================================================================
// Additional serde roundtrip tests for uncovered types
// =============================================================================

fn arb_scg_str() -> impl Strategy<Value = String> {
    "[a-z]{3,12}".prop_map(String::from)
}

fn arb_cell_telemetry() -> impl Strategy<Value = CellTelemetry> {
    (
        0u64..1000,
        0u64..1000,
        0u64..100,
        0u64..500,
        0u64..500,
        0u64..100,
        0u64..50,
        0u64..50,
        0u64..10,
    )
        .prop_map(
            |(
                attempted,
                succeeded,
                failed,
                spawned,
                completed,
                cancelled,
                faults,
                recoveries,
                deadlocks,
            )| {
                CellTelemetry {
                    ops_attempted: attempted,
                    ops_succeeded: succeeded,
                    ops_failed: failed,
                    tasks_spawned: spawned,
                    tasks_completed: completed,
                    tasks_cancelled: cancelled,
                    faults_injected: faults,
                    recoveries,
                    deadlock_detected_count: deadlocks,
                }
            },
        )
}

fn arb_cell_result() -> impl Strategy<Value = CellResult> {
    (
        arb_scg_str(),
        arb_journey_category(),
        arb_workload_profile(),
        arb_failure_injection(),
        proptest::bool::ANY,
        proptest::bool::ANY,
        0u64..60_000,
        arb_cell_telemetry(),
    )
        .prop_map(
            |(cell_id, category, workload, injection, passed, blocking, dur, telemetry)| {
                CellResult {
                    cell_id,
                    category,
                    workload,
                    injection,
                    passed,
                    blocking,
                    duration_ms: dur,
                    failure_reason: None,
                    error_rate: 0.01,
                    p95_latency_ms: 42.5,
                    seed: Some(42),
                    telemetry,
                }
            },
        )
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    #[test]
    fn scg_s01_user_journey_scenario_serde(
        sid in arb_scg_str(), cat in arb_journey_category(), blocking in proptest::bool::ANY,
    ) {
        let scenario = UserJourneyScenario {
            scenario_id: sid.clone(), category: cat,
            description: "test scenario".to_string(), expected_duration_ms: 60_000,
            blocking, seed: Some(42),
        };
        let json = serde_json::to_string(&scenario).unwrap();
        let back: UserJourneyScenario = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&back.scenario_id, &sid);
        prop_assert_eq!(back.blocking, blocking);
    }

    #[test]
    fn scg_s02_soak_matrix_serde(cat in arb_journey_category()) {
        let matrix = SoakMatrix::custom(
            vec![UserJourneyScenario {
                scenario_id: "s1".to_string(), category: cat,
                description: "test".to_string(), expected_duration_ms: 1000,
                blocking: true, seed: None,
            }],
            vec![WorkloadProfile::Steady],
            vec![FailureInjectionProfile::None],
        );
        let json = serde_json::to_string(&matrix).unwrap();
        let back: SoakMatrix = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.scenarios.len(), 1);
        prop_assert_eq!(back.workload_profiles.len(), 1);
    }

    #[test]
    fn scg_s03_soak_execution_plan_serde(cat in arb_journey_category(), wp in arb_workload_profile()) {
        let plan = SoakExecutionPlan {
            cells: vec![SoakCell {
                cell_id: "cell-1".to_string(), scenario_id: "s1".to_string(),
                category: cat, driver: cat.into(), expected_duration_ms: 1_000,
                workload: wp, injection: FailureInjectionProfile::None,
                blocking: true, seed: Some(42),
            }],
        };
        let json = serde_json::to_string(&plan).unwrap();
        let back: SoakExecutionPlan = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.cells.len(), 1);
        prop_assert_eq!(&back.cells[0].cell_id, "cell-1");
        prop_assert_eq!(back.cells[0].driver, JourneyDriver::from(cat));
    }

    #[test]
    fn scg_s04_soak_cell_serde(
        cid in arb_scg_str(), cat in arb_journey_category(),
        wp in arb_workload_profile(), fi in arb_failure_injection(),
    ) {
        let cell = SoakCell {
            cell_id: cid.clone(), scenario_id: "s1".to_string(),
            category: cat, driver: cat.into(), expected_duration_ms: 1_000,
            workload: wp, injection: fi,
            blocking: true, seed: Some(99),
        };
        let json = serde_json::to_string(&cell).unwrap();
        let back: SoakCell = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&back.cell_id, &cid);
        prop_assert_eq!(back.seed, Some(99));
        prop_assert_eq!(back.driver, JourneyDriver::from(cat));
    }

    #[test]
    fn scg_s05_soak_execution_result_serde(dur in 1000u64..60_000) {
        let result = SoakExecutionResult {
            cell_results: vec![], invariant_checks: vec![],
            total_duration_ms: dur, started_at_ms: 1_700_000_000_000,
            completed_at_ms: 1_700_000_000_000 + dur,
        };
        let json = serde_json::to_string(&result).unwrap();
        let back: SoakExecutionResult = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.total_duration_ms, dur);
    }

    #[test]
    fn scg_s06_cell_result_serde(cr in arb_cell_result()) {
        let cid = cr.cell_id.clone();
        let passed = cr.passed;
        let json = serde_json::to_string(&cr).unwrap();
        let back: CellResult = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&back.cell_id, &cid);
        prop_assert_eq!(back.passed, passed);
    }

    #[test]
    fn scg_s07_cell_telemetry_serde(tel in arb_cell_telemetry()) {
        let attempted = tel.ops_attempted;
        let json = serde_json::to_string(&tel).unwrap();
        let back: CellTelemetry = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.ops_attempted, attempted);
    }

    #[test]
    fn scg_s08_soak_invariant_check_serde(iid in arb_scg_str(), passed in proptest::bool::ANY) {
        let check = SoakInvariantCheck {
            invariant_id: iid.clone(), description: "test invariant".to_string(),
            passed, evidence: "ok".to_string(), mandatory: true,
        };
        let json = serde_json::to_string(&check).unwrap();
        let back: SoakInvariantCheck = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&back.invariant_id, &iid);
        prop_assert_eq!(back.passed, passed);
    }

    #[test]
    fn scg_s09_aggregated_soak_telemetry_serde(ops in 0u64..10_000, faults in 0u64..500) {
        let tel = AggregatedSoakTelemetry {
            ops_attempted: ops, ops_succeeded: ops, ops_failed: 0,
            tasks_spawned: 100, tasks_completed: 100, tasks_cancelled: 0,
            faults_injected: faults, recoveries: faults,
            deadlock_detected_count: 0,
            cells_with_task_accounting_mismatch: 0,
            cells_with_operation_accounting_mismatch: 0,
            max_p95_latency_ms: 42.5,
        };
        let json = serde_json::to_string(&tel).unwrap();
        let back: AggregatedSoakTelemetry = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.ops_attempted, ops);
        prop_assert_eq!(back.faults_injected, faults);
    }

    #[test]
    fn scg_s10_confidence_gate_serde(
        pass_rate in 0.5f64..1.0, error_rate in 0.0f64..0.1,
    ) {
        let gate = ConfidenceGate {
            min_pass_rate: pass_rate, max_error_rate: error_rate,
            max_p95_latency_ms: 5000.0,
            blocking_failures_are_hard_stop: true,
            mandatory_invariants_are_hard_stop: true,
        };
        let json = serde_json::to_string(&gate).unwrap();
        let back: ConfidenceGate = serde_json::from_str(&json).unwrap();
        prop_assert!((back.min_pass_rate - pass_rate).abs() < 1e-10);
        prop_assert!((back.max_error_rate - error_rate).abs() < 1e-10);
    }

    #[test]
    fn scg_s11_confidence_verdict_serde(
        decision in arb_confidence_decision(),
        total in 1usize..100, passed_count in 0usize..100,
    ) {
        let verdict = ConfidenceVerdict {
            decision, checks: vec![],
            cells_total: total, cells_passed: passed_count.min(total),
            cells_failed: total.saturating_sub(passed_count.min(total)),
            soak_duration_ms: 60_000,
        };
        let json = serde_json::to_string(&verdict).unwrap();
        let back: ConfidenceVerdict = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.cells_total, total);
    }

    #[test]
    fn scg_s12_gate_condition_serde(cid in arb_scg_str(), passed in proptest::bool::ANY, blocking in proptest::bool::ANY) {
        let cond = GateCondition {
            condition_id: cid.clone(), description: "test check".to_string(),
            passed, measured: "42%".to_string(), blocking,
        };
        let json = serde_json::to_string(&cond).unwrap();
        let back: GateCondition = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&back.condition_id, &cid);
        prop_assert_eq!(back.passed, passed);
        prop_assert_eq!(back.blocking, blocking);
    }
}
