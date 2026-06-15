//! Centralized test-logging convention for FrankenTerm reality-check tests.
//!
//! G52 (`ft-tf6g3.40`) substrate. Every test under the reality-check epic
//! emits structured JSONL rows via this crate so cross-test queries
//! (G31 reality-check discipline) can reason over a uniform corpus.
//!
//! Canonical row shape:
//!
//! ```text
//! { ts, area, test, kind, payload, run_id }
//! ```
//!
//! - `ts` is an RFC3339 UTC timestamp string
//! - `area` is a stable category (e.g. `"sprt"`, `"renderer-slo"`, `"taxonomy"`)
//! - `test` is the test function name (or doctest line)
//! - `kind` is one of the 7 canonical kinds: `assertion`, `stage_enter`,
//!   `stage_exit`, `measurement`, `error`, `decision`, `evidence_emit`
//! - `payload` is an arbitrary JSON value supplied by the test
//! - `run_id` is a UUIDv7 (time-ordered) generated once per `TestLogger`
//!   so rows from the same logger sort naturally and rows from different
//!   loggers don't collide
//!
//! Files land under `target/test-logs/<area>/<test>/<run_id>.jsonl`.
//!
//! ```
//! use ft_test_log::{TestLogger, ROW_SCHEMA_VERSION};
//!
//! let logger = TestLogger::new("doctest", "example");
//! logger.assertion("invariant holds", serde_json::json!({"ok": true})).unwrap();
//! logger.measurement("latency_ms", serde_json::json!({"value": 4.2, "unit": "ms"})).unwrap();
//! assert_eq!(ROW_SCHEMA_VERSION, "ft.test-log.row.v1");
//! ```

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fs::{OpenOptions, create_dir_all};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};
use thiserror::Error;
use uuid::Uuid;

/// Stable schema version for the canonical JSONL row shape.
pub const ROW_SCHEMA_VERSION: &str = "ft.test-log.row.v1";

/// The seven canonical row kinds. Test writers should prefer the named
/// helpers on [`TestLogger`] over passing strings manually; the enum is
/// the source of truth.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RowKind {
    /// A pass/fail assertion was evaluated.
    Assertion,
    /// A logical stage began (paired with `StageExit`).
    StageEnter,
    /// A logical stage ended.
    StageExit,
    /// A numeric measurement was recorded (latency, byte count, ratio).
    Measurement,
    /// A non-fatal error was observed.
    Error,
    /// A gate decision was rendered (accept / reject / continue / etc).
    Decision,
    /// An evidence-stream sample (G47) was emitted to a sibling file.
    EvidenceEmit,
}

impl RowKind {
    /// Canonical lower-case wire form (`"assertion"`, `"stage_enter"`, ...).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Assertion => "assertion",
            Self::StageEnter => "stage_enter",
            Self::StageExit => "stage_exit",
            Self::Measurement => "measurement",
            Self::Error => "error",
            Self::Decision => "decision",
            Self::EvidenceEmit => "evidence_emit",
        }
    }
}

/// One canonical JSONL row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TestLogRow {
    /// Schema version constant. Always `ft.test-log.row.v1`.
    pub schema_version: String,
    /// RFC3339 UTC timestamp.
    pub ts: String,
    /// Test area / category (e.g. `"sprt"`, `"renderer-slo"`).
    pub area: String,
    /// Test function name or doctest origin.
    pub test: String,
    /// Canonical kind.
    pub kind: RowKind,
    /// Caller-supplied JSON payload.
    pub payload: Value,
    /// UUIDv7 run identifier (time-ordered).
    pub run_id: String,
}

/// Errors that can arise emitting a row.
#[derive(Debug, Error)]
pub enum TestLogError {
    /// The underlying file write failed.
    #[error("test-log io: {0}")]
    Io(#[from] io::Error),
    /// Serializing the payload to JSON failed.
    #[error("test-log serde: {0}")]
    Serde(#[from] serde_json::Error),
}

/// Test-side logger. One instance per test; rows from the same logger
/// share a `run_id` and sort naturally by `ts`.
pub struct TestLogger {
    area: String,
    test: String,
    run_id: String,
    output_path: PathBuf,
    sink: Mutex<Box<dyn Write + Send>>,
}

impl std::fmt::Debug for TestLogger {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TestLogger")
            .field("area", &self.area)
            .field("test", &self.test)
            .field("run_id", &self.run_id)
            .field("output_path", &self.output_path)
            .field("sink", &"<dyn Write + Send>")
            .finish()
    }
}

impl TestLogger {
    /// Build a logger writing to `target/test-logs/<area>/<test>/<run_id>.jsonl`.
    ///
    /// Falls back to in-memory buffering when the filesystem target cannot be
    /// created (e.g. read-only test runner); see [`TestLogger::in_memory`]
    /// for the explicit form.
    pub fn new(area: impl Into<String>, test: impl Into<String>) -> Self {
        let area = area.into();
        let test = test.into();
        let run_id = Uuid::now_v7().to_string();
        let base = Self::output_root().join(&area).join(&test);
        let path = base.join(format!("{run_id}.jsonl"));
        // Two-step fallback: try to mkdir, then try to open. Both steps may
        // fail on read-only / sandboxed runners; in that case we fall back
        // to an in-memory sink AND mark output_path with the in-memory
        // sentinel so operator inspection of `output_path()` doesn't
        // promise a file that does not exist.
        let (sink, resolved_path): (Box<dyn Write + Send>, PathBuf) = match create_dir_all(&base) {
            Ok(()) => match OpenOptions::new().create(true).append(true).open(&path) {
                Ok(f) => (Box::new(f), path),
                Err(_) => (Box::new(Vec::new()), PathBuf::from("<in-memory>")),
            },
            Err(_) => (Box::new(Vec::new()), PathBuf::from("<in-memory>")),
        };
        Self {
            area,
            test,
            run_id,
            output_path: resolved_path,
            sink: Mutex::new(sink),
        }
    }

    /// Build a logger backed by an in-memory sink, useful when the test
    /// host's filesystem is unavailable or when the test wants to assert
    /// on the emitted rows directly.
    pub fn in_memory(area: impl Into<String>, test: impl Into<String>) -> Self {
        let area = area.into();
        let test = test.into();
        let run_id = Uuid::now_v7().to_string();
        Self {
            area,
            test,
            run_id,
            output_path: PathBuf::from("<in-memory>"),
            sink: Mutex::new(Box::new(Vec::<u8>::new())),
        }
    }

    /// Test area.
    pub fn area(&self) -> &str {
        &self.area
    }

    /// Test name.
    pub fn test_name(&self) -> &str {
        &self.test
    }

    /// UUIDv7 run identifier.
    pub fn run_id(&self) -> &str {
        &self.run_id
    }

    /// Resolved output path (may be `<in-memory>`).
    pub fn output_path(&self) -> &Path {
        &self.output_path
    }

    /// Resolve the root directory for log files. Honors `FT_TEST_LOG_DIR`
    /// when set, otherwise defaults to `target/test-logs`.
    fn output_root() -> PathBuf {
        std::env::var_os("FT_TEST_LOG_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("target/test-logs"))
    }

    fn sink_lock(&self) -> MutexGuard<'_, Box<dyn Write + Send>> {
        match self.sink.lock() {
            Ok(sink) => sink,
            Err(poisoned) => {
                self.sink.clear_poison();
                poisoned.into_inner()
            }
        }
    }

    /// Emit a row of arbitrary kind. Prefer the named helpers below.
    pub fn row(&self, kind: RowKind, payload: Value) -> Result<TestLogRow, TestLogError> {
        let row = TestLogRow {
            schema_version: ROW_SCHEMA_VERSION.to_string(),
            ts: chrono::Utc::now().to_rfc3339(),
            area: self.area.clone(),
            test: self.test.clone(),
            kind,
            payload,
            run_id: self.run_id.clone(),
        };
        let line = serde_json::to_string(&row)?;
        let mut sink = self.sink_lock();
        sink.write_all(line.as_bytes())?;
        sink.write_all(b"\n")?;
        Ok(row)
    }

    /// Emit an `assertion` row.
    pub fn assertion(&self, label: &str, detail: Value) -> Result<TestLogRow, TestLogError> {
        let mut obj = serde_json::Map::new();
        obj.insert("label".to_string(), Value::String(label.to_string()));
        obj.insert("detail".to_string(), detail);
        self.row(RowKind::Assertion, Value::Object(obj))
    }

    /// Emit a `stage_enter` row.
    pub fn stage_enter(&self, stage: &str, detail: Value) -> Result<TestLogRow, TestLogError> {
        let mut obj = serde_json::Map::new();
        obj.insert("stage".to_string(), Value::String(stage.to_string()));
        obj.insert("detail".to_string(), detail);
        self.row(RowKind::StageEnter, Value::Object(obj))
    }

    /// Emit a `stage_exit` row.
    pub fn stage_exit(&self, stage: &str, detail: Value) -> Result<TestLogRow, TestLogError> {
        let mut obj = serde_json::Map::new();
        obj.insert("stage".to_string(), Value::String(stage.to_string()));
        obj.insert("detail".to_string(), detail);
        self.row(RowKind::StageExit, Value::Object(obj))
    }

    /// Emit a `measurement` row.
    pub fn measurement(&self, metric: &str, detail: Value) -> Result<TestLogRow, TestLogError> {
        let mut obj = serde_json::Map::new();
        obj.insert("metric".to_string(), Value::String(metric.to_string()));
        obj.insert("detail".to_string(), detail);
        self.row(RowKind::Measurement, Value::Object(obj))
    }

    /// Emit a non-fatal `error` row.
    pub fn error(&self, reason: &str, detail: Value) -> Result<TestLogRow, TestLogError> {
        let mut obj = serde_json::Map::new();
        obj.insert("reason".to_string(), Value::String(reason.to_string()));
        obj.insert("detail".to_string(), detail);
        self.row(RowKind::Error, Value::Object(obj))
    }

    /// Emit a `decision` row (gate verdict, branch chosen, etc).
    pub fn decision(&self, verdict: &str, detail: Value) -> Result<TestLogRow, TestLogError> {
        let mut obj = serde_json::Map::new();
        obj.insert("verdict".to_string(), Value::String(verdict.to_string()));
        obj.insert("detail".to_string(), detail);
        self.row(RowKind::Decision, Value::Object(obj))
    }

    /// Emit an `evidence_emit` row (cross-link to a G47 evidence-stream
    /// sample written elsewhere).
    pub fn evidence_emit(&self, claim_id: &str, detail: Value) -> Result<TestLogRow, TestLogError> {
        let mut obj = serde_json::Map::new();
        obj.insert("claim_id".to_string(), Value::String(claim_id.to_string()));
        obj.insert("detail".to_string(), detail);
        self.row(RowKind::EvidenceEmit, Value::Object(obj))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    const W12_CROSS_CUTTING_HARNESS: &str =
        include_str!("../../../tests/e2e/fixtures/duel-program/w12_cross_cutting_harness.v1.json");

    type FixtureResult<T = ()> = Result<T, String>;

    fn json_fixture() -> FixtureResult<Value> {
        serde_json::from_str(W12_CROSS_CUTTING_HARNESS)
            .map_err(|err| format!("W12.2 cross-cutting harness fixture must be valid JSON: {err}"))
    }

    fn string_field<'a>(value: &'a Value, key: &str) -> FixtureResult<&'a str> {
        value
            .get(key)
            .and_then(Value::as_str)
            .ok_or_else(|| format!("expected string field {key}"))
    }

    fn array_field<'a>(value: &'a Value, key: &str) -> FixtureResult<&'a Vec<Value>> {
        value
            .get(key)
            .and_then(Value::as_array)
            .ok_or_else(|| format!("expected array field {key}"))
    }

    fn object_array_ids(value: &Value, key: &str) -> FixtureResult<BTreeSet<String>> {
        array_field(value, key)?
            .iter()
            .map(|entry| string_field(entry, "id").map(str::to_owned))
            .collect()
    }

    fn string_array(value: &Value, key: &str) -> FixtureResult<Vec<String>> {
        array_field(value, key)?
            .iter()
            .map(|entry| {
                entry
                    .as_str()
                    .map(str::to_owned)
                    .ok_or_else(|| format!("expected string entry in {key}"))
            })
            .collect()
    }

    fn emit_test_row<T>(result: Result<T, TestLogError>) -> FixtureResult<T> {
        result.map_err(|err| format!("emit test log row: {err}"))
    }

    #[test]
    fn row_carries_schema_version() {
        let logger = TestLogger::in_memory("test-area", "test_row_schema");
        let row = logger.assertion("ok", Value::Bool(true)).expect("emit row");
        assert_eq!(row.schema_version, ROW_SCHEMA_VERSION);
        assert_eq!(row.area, "test-area");
        assert_eq!(row.test, "test_row_schema");
        assert_eq!(row.kind, RowKind::Assertion);
    }

    #[test]
    fn run_id_is_uuidv7() {
        let logger = TestLogger::in_memory("a", "b");
        // v7 has version nibble = 7 in the third hex group: xxxxxxxx-xxxx-7xxx-...
        let parts: Vec<&str> = logger.run_id().split('-').collect();
        assert_eq!(parts.len(), 5);
        assert!(
            parts[2].starts_with('7'),
            "expected v7 UUID, got {}",
            logger.run_id()
        );
    }

    #[test]
    fn all_seven_kinds_serialize() {
        let logger = TestLogger::in_memory("a", "b");
        logger.assertion("x", Value::Null).unwrap();
        logger.stage_enter("s", Value::Null).unwrap();
        logger.stage_exit("s", Value::Null).unwrap();
        logger.measurement("m", Value::Null).unwrap();
        logger.error("e", Value::Null).unwrap();
        logger.decision("d", Value::Null).unwrap();
        logger.evidence_emit("c", Value::Null).unwrap();
    }

    #[test]
    fn row_recovers_after_poisoned_sink_lock() {
        let logger = TestLogger::in_memory("a", "poisoned_sink");

        let poison = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = logger.sink.lock().unwrap();
            std::panic::panic_any("poison ft-test-log sink");
        }));
        assert!(poison.is_err());

        let row = logger
            .assertion("after poison", Value::Bool(true))
            .expect("poisoned sink should recover");
        assert_eq!(row.kind, RowKind::Assertion);

        logger
            .measurement("second row", Value::Null)
            .expect("sink poison should be cleared");
    }

    #[test]
    fn kind_wire_form_is_snake_case() {
        assert_eq!(RowKind::Assertion.as_str(), "assertion");
        assert_eq!(RowKind::StageEnter.as_str(), "stage_enter");
        assert_eq!(RowKind::StageExit.as_str(), "stage_exit");
        assert_eq!(RowKind::Measurement.as_str(), "measurement");
        assert_eq!(RowKind::Error.as_str(), "error");
        assert_eq!(RowKind::Decision.as_str(), "decision");
        assert_eq!(RowKind::EvidenceEmit.as_str(), "evidence_emit");
    }

    #[test]
    fn row_roundtrips_serde() {
        let logger = TestLogger::in_memory("a", "b");
        let row = logger
            .measurement("latency_ms", serde_json::json!({"value": 4.2}))
            .unwrap();
        let json = serde_json::to_string(&row).unwrap();
        let back: TestLogRow = serde_json::from_str(&json).unwrap();
        assert_eq!(row, back);
    }

    #[test]
    fn ts_is_rfc3339() {
        let logger = TestLogger::in_memory("a", "b");
        let row = logger.assertion("x", Value::Null).unwrap();
        // Parses cleanly as RFC3339.
        chrono::DateTime::parse_from_rfc3339(&row.ts).expect("ts is rfc3339");
    }

    #[test]
    fn duel_program_cross_cutting_harness_contract_pins_w12_requirements() -> FixtureResult {
        let fixture = json_fixture()?;
        assert_eq!(fixture["schema_version"], 1);
        assert_eq!(
            string_field(&fixture, "contract_id")?,
            "ft.duel.cross_cutting_e2e_harness.v1"
        );
        assert_eq!(string_field(&fixture, "producing_bead")?, "ft-7h5da.13.2");
        assert_eq!(
            string_field(&fixture, "status")?,
            "contract_ready_live_swarm_not_yet_proven"
        );

        let claim_boundaries = fixture
            .get("claim_boundaries")
            .ok_or_else(|| "claim boundaries are present".to_string())?;
        for key in [
            "live_ntm_swarm_proof_claimed",
            "remote_cargo_passed",
            "local_cargo_counts_as_proof",
            "side_effects_executed_by_contract_fixture",
            "eligible_for_release_attestation",
        ] {
            assert_eq!(
                claim_boundaries.get(key).and_then(Value::as_bool),
                Some(false),
                "{key} must be false until the live W12.2 proof exists"
            );
        }

        let artifact_contract = fixture
            .get("artifact_contract")
            .ok_or_else(|| "artifact contract is present".to_string())?;
        assert_eq!(
            string_array(artifact_contract, "required_files")?,
            [
                "commands.txt",
                "env.txt",
                "manifest.json",
                "structured.log",
                "stdout.txt",
                "stderr.txt",
                "summary.json"
            ]
        );
        assert_eq!(
            artifact_contract["structured_log"]["schema_version"],
            ROW_SCHEMA_VERSION
        );
        assert_eq!(
            string_array(&artifact_contract["structured_log"], "required_phases")?,
            ["SETUP", "ACT", "ASSERT", "TEARDOWN"]
        );
        assert_eq!(
            string_array(&artifact_contract["structured_log"], "required_kinds")?,
            [
                RowKind::Assertion.as_str(),
                RowKind::StageEnter.as_str(),
                RowKind::StageExit.as_str(),
                RowKind::Measurement.as_str(),
                RowKind::Error.as_str(),
                RowKind::Decision.as_str(),
                RowKind::EvidenceEmit.as_str()
            ]
        );

        let phase_names: Vec<String> = array_field(&fixture, "phases")?
            .iter()
            .map(|phase| string_field(phase, "name").map(str::to_owned))
            .collect::<FixtureResult<Vec<String>>>()?;
        assert_eq!(phase_names, ["SETUP", "ACT", "ASSERT", "TEARDOWN"]);
        for phase in array_field(&fixture, "phases")? {
            assert!(
                !array_field(phase, "must_record_commands")?.is_empty(),
                "{} phase must pin exact commands",
                string_field(phase, "name")?
            );
            assert!(
                !array_field(phase, "assertions")?.is_empty(),
                "{} phase must pin assertions",
                string_field(phase, "name")?
            );
        }

        let surface_ids = object_array_ids(&fixture, "surfaces")?;
        for required_surface in [
            "W0.redaction_canary",
            "W1.semantic_api",
            "W2.verified_submit",
            "W3.watch_events",
            "W5.steer_receipt",
            "W6.robot_next_policy_cockpit",
        ] {
            assert!(
                surface_ids.contains(required_surface),
                "missing surface {required_surface}"
            );
        }
        for surface in array_field(&fixture, "surfaces")? {
            assert!(
                ["SETUP", "ACT", "ASSERT", "TEARDOWN"].contains(&string_field(surface, "phase")?),
                "{} has an invalid phase",
                string_field(surface, "id")?
            );
            assert!(
                !array_field(surface, "commands")?.is_empty(),
                "{} must record live command shape",
                string_field(surface, "id")?
            );
            assert!(
                !array_field(surface, "required_artifacts")?.is_empty(),
                "{} must retain artifacts",
                string_field(surface, "id")?
            );
            assert!(
                !array_field(surface, "positive_assertions")?.is_empty()
                    || !array_field(surface, "negative_assertions")?.is_empty(),
                "{} must declare at least one assertion",
                string_field(surface, "id")?
            );
        }

        let negative_case_ids = object_array_ids(&fixture, "negative_cases")?;
        for required_case in [
            "typed_semantic_data_unavailable",
            "stuck_composer",
            "cursor_unavailable",
            "tampered_hash",
            "policy_denial",
            "planted_canary",
            "rch_fail_closed",
        ] {
            assert!(
                negative_case_ids.contains(required_case),
                "missing negative case {required_case}"
            );
        }
        for negative in array_field(&fixture, "negative_cases")? {
            assert!(
                negative.get("expected_error_code").is_some()
                    || negative.get("expected_state").is_some()
                    || negative.get("expected_redaction").is_some(),
                "{} must pin a typed expected outcome",
                string_field(negative, "id")?
            );
            assert!(
                string_field(negative, "must_not")?.len() > 8,
                "{} must pin the forbidden false-positive outcome",
                string_field(negative, "id")?
            );
        }

        let proof_lane = fixture
            .get("proof_lane")
            .ok_or_else(|| "proof lane is present".to_string())?;
        assert_eq!(proof_lane["remote_required"], true);
        assert_eq!(
            string_field(proof_lane, "cargo_target_dir")?,
            "/tmp/ft-7h5da132-cod5-target"
        );
        let narrow_command = string_field(proof_lane, "narrow_command")?;
        for required in [
            "RCH_REQUIRE_REMOTE=1",
            "RCH_NO_SELF_HEALING=1",
            "rch --no-self-healing exec",
            "CARGO_TARGET_DIR=/tmp/ft-7h5da132-cod5-target",
            "cargo test -p ft-test-log --lib duel_program_cross_cutting_harness_contract",
        ] {
            assert!(
                narrow_command.contains(required),
                "proof command must contain {required}"
            );
        }
        assert!(
            string_array(proof_lane, "forbidden_outputs")?
                .iter()
                .any(|output| output == "[RCH] local"),
            "fail-closed RCH local fallback marker must be forbidden"
        );
        Ok(())
    }

    #[test]
    fn duel_program_cross_cutting_harness_contract_emits_canonical_rows() -> FixtureResult {
        let fixture = json_fixture()?;
        let logger = TestLogger::in_memory("duel-program", "w12_cross_cutting_contract");
        let artifact_root = string_field(
            fixture
                .get("artifact_contract")
                .ok_or_else(|| "artifact contract is present".to_string())?,
            "root_template",
        )?;
        let structured_log_artifact = format!("{artifact_root}/structured.log");

        for phase in array_field(&fixture, "phases")? {
            let phase_name = string_field(phase, "name")?;
            let phase_commands = array_field(phase, "must_record_commands")?;
            let phase_assertions = array_field(phase, "assertions")?;
            let first_command = phase_commands
                .first()
                .ok_or("phase must record at least one command")?;
            let enter = emit_test_row(logger.stage_enter(
                phase_name,
                serde_json::json!({
                    "bead_id": "ft-7h5da.13.2",
                    "artifact_root": artifact_root,
                    "command_count": phase_commands.len()
                }),
            ))?;
            assert_eq!(enter.kind, RowKind::StageEnter);
            assert_eq!(enter.payload["stage"], phase_name);

            let assertion = emit_test_row(logger.assertion(
                "contract assertions pinned",
                serde_json::json!({
                    "status": "pass",
                    "phase": phase_name,
                    "command": first_command,
                    "artifact": structured_log_artifact.as_str(),
                    "assertion_count": phase_assertions.len()
                }),
            ))?;
            assert_eq!(assertion.kind, RowKind::Assertion);
            assert_eq!(assertion.payload["detail"]["status"], "pass");
            assert_eq!(assertion.payload["detail"]["phase"], phase_name);
            assert_eq!(
                assertion.payload["detail"]["artifact"]
                    .as_str()
                    .map(|artifact| artifact.ends_with("structured.log")),
                Some(true)
            );

            let exit = emit_test_row(logger.stage_exit(
                phase_name,
                serde_json::json!({
                    "status": "pass",
                    "artifact_root": artifact_root
                }),
            ))?;
            assert_eq!(exit.kind, RowKind::StageExit);
            assert_eq!(exit.payload["stage"], phase_name);
        }

        let decision = emit_test_row(logger.decision(
            "blocked_until_live_swarm_and_remote_rch_pass",
            serde_json::json!({
                "status": string_field(&fixture, "status")?,
                "local_cargo_counts_as_proof": false,
                "remote_required": true
            }),
        ))?;
        assert_eq!(decision.kind, RowKind::Decision);
        assert_eq!(
            decision.payload["verdict"],
            "blocked_until_live_swarm_and_remote_rch_pass"
        );

        let evidence = emit_test_row(logger.evidence_emit(
            "ft-7h5da.13.2.cross_cutting_contract",
            serde_json::json!({
                "path": "tests/e2e/fixtures/duel-program/w12_cross_cutting_harness.v1.json",
                "schema_version": 1
            }),
        ))?;
        assert_eq!(evidence.kind, RowKind::EvidenceEmit);
        assert_eq!(
            evidence.payload["claim_id"],
            "ft-7h5da.13.2.cross_cutting_contract"
        );
        Ok(())
    }
}
