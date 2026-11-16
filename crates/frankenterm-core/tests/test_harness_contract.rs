//! Unified test harness contract validation (ft-e34d9.10.6.5).
//!
//! Validates that the structured logging harness itself conforms to the
//! ADR-0012 structured log contract.  These tests serve double duty:
//!
//! 1. Contract verification — every emitted event has all 10 required fields.
//! 2. Usage demonstration — shows how other tests should use the harness.

mod common;

use common::reason_codes::{ErrorCode, Outcome, ReasonCode};
use common::test_event_logger::{ScenarioRunner, TestEvent, TestEventLogger};

// -------------------------------------------------------------------------
// ADR-0012 Contract Conformance
// -------------------------------------------------------------------------

const REQUIRED_FIELDS: &[&str] = &[
    "timestamp",
    "component",
    "scenario_id",
    "correlation_id",
    "decision_path",
    "input_summary",
    "outcome",
    "reason_code",
    "error_code",
    "artifact_path",
];

fn assert_contract_conformance(event: &TestEvent) {
    let json: serde_json::Value = serde_json::to_value(event).unwrap();
    for field in REQUIRED_FIELDS {
        assert!(
            json.get(field).is_some(),
            "ADR-0012 violation: missing required field '{field}' in event"
        );
    }
    // Timestamps must be ISO-8601.
    assert!(
        !event.timestamp.is_empty(),
        "ADR-0012 violation: empty timestamp"
    );
    // Component must be non-empty.
    assert!(
        !event.component.is_empty(),
        "ADR-0012 violation: empty component"
    );
    // Scenario ID must be non-empty.
    assert!(
        !event.scenario_id.is_empty(),
        "ADR-0012 violation: empty scenario_id"
    );
    // Correlation ID must be non-empty.
    assert!(
        !event.correlation_id.is_empty(),
        "ADR-0012 violation: empty correlation_id"
    );
}

#[test]
fn contract_all_events_have_required_fields() {
    let logger = ScenarioRunner::new(
        "harness.contract",
        "ft-e34d9.10.6.5",
        "required_fields_check",
    )
    .run(|logger| {
        logger.checkpoint("step_a");
        logger
            .emit(
                Outcome::Checkpoint,
                ReasonCode::ChaosInjected,
                ErrorCode::None,
            )
            .decision_path("fault_point")
            .input_summary("delay=100ms")
            .artifact_path("/tmp/artifact.jsonl")
            .log();
    });

    for event in logger.events() {
        assert_contract_conformance(event);
    }
}

#[test]
fn contract_scenario_id_contains_bead_and_name() {
    let logger = TestEventLogger::new("harness.contract", "ft-e34d9.10.6.5", "scenario_id_format");
    let sid = logger.scenario_id();
    assert!(
        sid.contains("ft_e34d9_10_6_5"),
        "scenario_id must contain sanitized bead ID, got: {sid}"
    );
    assert!(
        sid.contains("scenario_id_format"),
        "scenario_id must contain scenario name, got: {sid}"
    );
    assert!(
        sid.contains(':'),
        "scenario_id must separate bead and name with ':', got: {sid}"
    );
}

#[test]
fn contract_correlation_id_contains_bead() {
    let logger = TestEventLogger::new(
        "harness.contract",
        "ft-e34d9.10.6.5",
        "correlation_id_format",
    );
    assert!(
        logger.correlation_id().contains("ft-e34d9.10.6.5"),
        "correlation_id must contain bead ID, got: {}",
        logger.correlation_id()
    );
}

#[test]
fn contract_correlation_ids_unique_across_loggers() {
    let ids: Vec<String> = (0..10)
        .map(|i| {
            TestEventLogger::new(
                "harness.contract",
                "ft-e34d9.10.6.5",
                &format!("uniqueness_{i}"),
            )
            .correlation_id()
            .to_string()
        })
        .collect();

    let unique: std::collections::HashSet<&String> = ids.iter().collect();
    assert_eq!(
        unique.len(),
        ids.len(),
        "correlation_ids must be unique across loggers"
    );
}

// -------------------------------------------------------------------------
// Outcome Tracking
// -------------------------------------------------------------------------

#[test]
fn outcome_tracking_started_passed() {
    let logger =
        ScenarioRunner::new("harness.tracking", "ft-e34d9.10.6.5", "happy_path").run(|_| {});

    assert!(logger.all_passed());
    assert_eq!(logger.count_outcome(Outcome::Started), 1);
    assert_eq!(logger.count_outcome(Outcome::Passed), 1);
    assert_eq!(logger.count_outcome(Outcome::Failed), 0);
}

#[test]
fn outcome_tracking_failure_recorded() {
    let mut logger =
        TestEventLogger::new("harness.tracking", "ft-e34d9.10.6.5", "explicit_failure");
    logger.started();
    logger.failed(ReasonCode::TimeoutExpired, ErrorCode::Timeout);

    assert!(!logger.all_passed());
    assert_eq!(logger.count_outcome(Outcome::Failed), 1);

    let failed = logger
        .events()
        .iter()
        .find(|e| e.outcome == Outcome::Failed)
        .unwrap();
    assert_eq!(failed.reason_code, ReasonCode::TimeoutExpired);
    assert_eq!(failed.error_code, ErrorCode::Timeout);
}

#[test]
fn outcome_tracking_panic_capture() {
    let logger =
        ScenarioRunner::new("harness.tracking", "ft-e34d9.10.6.5", "panic_capture").run(|_| {
            panic!("deliberate panic for testing");
        });

    assert!(!logger.all_passed());
    assert_eq!(logger.count_outcome(Outcome::Failed), 1);

    let failed = logger
        .events()
        .iter()
        .find(|e| e.outcome == Outcome::Failed)
        .unwrap();
    assert_eq!(failed.reason_code, ReasonCode::PanicPropagated);
    assert_eq!(failed.error_code, ErrorCode::Panic);
    assert!(failed.input_summary.contains("deliberate panic"));
}

// -------------------------------------------------------------------------
// JSON Serialization Contract
// -------------------------------------------------------------------------

#[test]
fn json_roundtrip_preserves_all_fields() {
    let mut logger = TestEventLogger::new("harness.serde", "ft-e34d9.10.6.5", "json_roundtrip");
    logger
        .emit(
            Outcome::Checkpoint,
            ReasonCode::CancellationRequested,
            ErrorCode::DataLoss,
        )
        .decision_path("verify_cancellation")
        .input_summary("channel_size=100")
        .artifact_path("/evidence/cancel.jsonl")
        .log();

    let event = &logger.events()[0];
    let json_str = serde_json::to_string(event).unwrap();
    let roundtripped: TestEvent = serde_json::from_str(&json_str).unwrap();

    assert_eq!(roundtripped.component, event.component);
    assert_eq!(roundtripped.scenario_id, event.scenario_id);
    assert_eq!(roundtripped.correlation_id, event.correlation_id);
    assert_eq!(roundtripped.decision_path, event.decision_path);
    assert_eq!(roundtripped.input_summary, event.input_summary);
    assert_eq!(roundtripped.outcome, event.outcome);
    assert_eq!(roundtripped.reason_code, event.reason_code);
    assert_eq!(roundtripped.error_code, event.error_code);
    assert_eq!(roundtripped.artifact_path, event.artifact_path);
}

#[test]
fn json_uses_snake_case_enums() {
    let mut logger = TestEventLogger::new("harness.serde", "ft-e34d9.10.6.5", "snake_case_check");
    logger
        .emit(
            Outcome::Checkpoint,
            ReasonCode::CancellationLoss,
            ErrorCode::SafetyViolation,
        )
        .decision_path("check")
        .log();

    let json_str = serde_json::to_string(&logger.events()[0]).unwrap();

    // Verify snake_case serialization (not PascalCase).
    assert!(
        json_str.contains("\"cancellation_loss\""),
        "reason_code should be snake_case, got: {json_str}"
    );
    assert!(
        json_str.contains("\"safety_violation\""),
        "error_code should be snake_case, got: {json_str}"
    );
    assert!(
        json_str.contains("\"checkpoint\""),
        "outcome should be snake_case, got: {json_str}"
    );
}

// -------------------------------------------------------------------------
// File Artifact Contract
// -------------------------------------------------------------------------

#[test]
fn file_artifact_written_as_jsonl() {
    let tmp = tempfile::tempdir().unwrap();
    let logger = ScenarioRunner::new("harness.artifact", "ft-e34d9.10.6.5", "jsonl_write")
        .artifact_dir(tmp.path())
        .run(|logger| {
            logger.checkpoint("step_1");
            logger.checkpoint("step_2");
        });

    let path = logger.flush().unwrap();
    assert!(path.exists(), "Artifact file should exist");
    assert!(
        path.extension().is_some_and(|ext| ext == "jsonl"),
        "Artifact should have .jsonl extension"
    );

    let content = std::fs::read_to_string(&path).unwrap();
    let lines: Vec<&str> = content.lines().collect();

    // started + 2 checkpoints + passed = 4 events
    assert_eq!(
        lines.len(),
        4,
        "Expected 4 JSONL lines, got {}",
        lines.len()
    );

    // Each line is valid JSON conforming to the contract.
    for (i, line) in lines.iter().enumerate() {
        let event: TestEvent = serde_json::from_str(line)
            .unwrap_or_else(|e| panic!("Line {i} is not valid JSON: {e}"));
        assert_contract_conformance(&event);
    }
}

#[test]
fn file_artifact_contains_correct_events() {
    let tmp = tempfile::tempdir().unwrap();
    let logger = ScenarioRunner::new("harness.artifact", "ft-e34d9.10.6.5", "event_content")
        .artifact_dir(tmp.path())
        .run(|logger| {
            logger.checkpoint("verification_step");
        });

    let path = logger.flush().unwrap();
    let content = std::fs::read_to_string(&path).unwrap();
    let events: Vec<TestEvent> = content
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();

    // First event should be Started.
    assert_eq!(events[0].outcome, Outcome::Started);
    // Middle event should be our checkpoint.
    assert_eq!(events[1].outcome, Outcome::Checkpoint);
    assert_eq!(events[1].decision_path, "verification_step");
    // Last event should be Passed.
    assert_eq!(events[2].outcome, Outcome::Passed);

    // All events share the same correlation_id.
    let cid = &events[0].correlation_id;
    for event in &events {
        assert_eq!(&event.correlation_id, cid);
    }
}

// -------------------------------------------------------------------------
// Reason/Error Code Coverage
// -------------------------------------------------------------------------

#[test]
fn reason_codes_cover_runtime_taxonomy() {
    // Verify key reason codes exist and serialize correctly.
    let codes = vec![
        (ReasonCode::None, "none"),
        (ReasonCode::TimeoutExpired, "timeout_expired"),
        (ReasonCode::ChannelClosed, "channel_closed"),
        (ReasonCode::NoPermits, "no_permits"),
        (ReasonCode::CancellationRequested, "cancellation_requested"),
        (ReasonCode::CancellationLoss, "cancellation_loss"),
        (ReasonCode::ScopeShutdown, "scope_shutdown"),
        (ReasonCode::PanicPropagated, "panic_propagated"),
        (ReasonCode::IoError, "io_error"),
        (ReasonCode::ChaosInjected, "chaos_injected"),
        (ReasonCode::InvariantViolation, "invariant_violation"),
        (ReasonCode::OracleFailure, "oracle_failure"),
        (ReasonCode::ScheduleDivergence, "schedule_divergence"),
    ];
    for (code, expected_str) in codes {
        let json = serde_json::to_value(code).unwrap();
        assert_eq!(
            json.as_str().unwrap(),
            expected_str,
            "ReasonCode::{code:?} should serialize to \"{expected_str}\""
        );
    }
}

#[test]
fn error_codes_cover_failure_taxonomy() {
    let codes = vec![
        (ErrorCode::None, "none"),
        (ErrorCode::AssertionFailed, "assertion_failed"),
        (ErrorCode::Timeout, "timeout"),
        (ErrorCode::Panic, "panic"),
        (ErrorCode::Io, "io"),
        (ErrorCode::Deadlock, "deadlock"),
        (ErrorCode::TaskLeak, "task_leak"),
        (ErrorCode::DataLoss, "data_loss"),
        (ErrorCode::SafetyViolation, "safety_violation"),
        (ErrorCode::LivenessViolation, "liveness_violation"),
    ];
    for (code, expected_str) in codes {
        let json = serde_json::to_value(code).unwrap();
        assert_eq!(
            json.as_str().unwrap(),
            expected_str,
            "ErrorCode::{code:?} should serialize to \"{expected_str}\""
        );
    }
}

// -------------------------------------------------------------------------
// Multi-scenario Evidence Isolation
// -------------------------------------------------------------------------

#[test]
fn multiple_scenarios_produce_independent_evidence() {
    let tmp = tempfile::tempdir().unwrap();

    let l1 = ScenarioRunner::new("harness.isolation", "ft-e34d9.10.6.5", "scenario_alpha")
        .artifact_dir(tmp.path())
        .run(|logger| {
            logger.checkpoint("alpha_step");
        });

    let l2 = ScenarioRunner::new("harness.isolation", "ft-e34d9.10.6.5", "scenario_beta")
        .artifact_dir(tmp.path())
        .run(|logger| {
            logger.checkpoint("beta_step");
        });

    // Different correlation IDs.
    assert_ne!(l1.correlation_id(), l2.correlation_id());

    // Different artifact files.
    let p1 = l1.flush().unwrap();
    let p2 = l2.flush().unwrap();
    assert_ne!(p1, p2);
    assert!(p1.exists());
    assert!(p2.exists());

    // Events in each file reference only their own correlation_id.
    let events1: Vec<TestEvent> = std::fs::read_to_string(&p1)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    for e in &events1 {
        assert_eq!(e.correlation_id, l1.correlation_id());
    }
}

// -------------------------------------------------------------------------
// E2E-compatible Evidence Check
// -------------------------------------------------------------------------

#[test]
fn e2e_compatible_evidence_structure() {
    // Verify that Rust-emitted events are structurally compatible with
    // what e2e shell scripts produce via `jq`.  The key contract points:
    //
    // 1. All 10 fields present.
    // 2. Enums serialize as snake_case strings (not integers).
    // 3. timestamp is ISO-8601.
    // 4. scenario_id format is `{bead}:{name}`.

    let mut logger =
        TestEventLogger::new("harness.e2e_compat", "ft-e34d9.10.6.5", "e2e_compat_check");
    logger.started();
    logger
        .emit(Outcome::Passed, ReasonCode::Completed, ErrorCode::None)
        .decision_path("final_verify")
        .input_summary("N=42")
        .log();

    for event in logger.events() {
        let json: serde_json::Value = serde_json::to_value(event).unwrap();

        // Field presence.
        for field in REQUIRED_FIELDS {
            assert!(json.get(field).is_some());
        }

        // Enums are strings, not numbers.
        assert!(json["outcome"].is_string());
        assert!(json["reason_code"].is_string());
        assert!(json["error_code"].is_string());

        // Timestamp starts with a year (basic ISO-8601 check).
        let ts = json["timestamp"].as_str().unwrap();
        assert!(ts.starts_with("20"), "Timestamp should be ISO-8601: {ts}");

        // Scenario ID has colon separator.
        let sid = json["scenario_id"].as_str().unwrap();
        assert!(sid.contains(':'));
    }
}
