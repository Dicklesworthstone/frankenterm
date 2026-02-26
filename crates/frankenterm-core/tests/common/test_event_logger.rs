//! Structured test event logger for unified evidence collection (ft-e34d9.10.6.5).
//!
//! Implements the ADR-0012 structured logging contract at the Rust level,
//! enabling unit and integration tests to emit the same evidence format as
//! e2e shell scripts.  All ten required fields are present:
//!
//! | Field            | Source                                          |
//! |------------------|-------------------------------------------------|
//! | timestamp        | Auto-generated (ISO-8601)                       |
//! | component        | Caller-supplied                                 |
//! | scenario_id      | Caller-supplied or auto from test name + bead   |
//! | correlation_id   | Auto-generated per `TestEventLogger` instance   |
//! | decision_path    | Caller-supplied per event                       |
//! | input_summary    | Caller-supplied per event                       |
//! | outcome          | Enum (`Outcome`)                                |
//! | reason_code      | Enum (`ReasonCode`)                             |
//! | error_code       | Enum (`ErrorCode`)                              |
//! | artifact_path    | Optional caller-supplied path                   |

use std::fs::{self, OpenOptions};
use std::io::Write as IoWrite;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use chrono::Utc;
use serde::{Deserialize, Serialize};

use super::reason_codes::{ErrorCode, Outcome, ReasonCode};

// -------------------------------------------------------------------------
// Static counter for unique correlation IDs within a process
// -------------------------------------------------------------------------

static LOGGER_COUNTER: AtomicU64 = AtomicU64::new(0);

// -------------------------------------------------------------------------
// TestEvent — a single structured log entry
// -------------------------------------------------------------------------

/// A single structured test event matching the ADR-0012 contract.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TestEvent {
    /// ISO-8601 timestamp.
    pub timestamp: String,
    /// Component identifier (e.g., "search_integration.unit").
    pub component: String,
    /// Scenario identifier (e.g., "ft_e34d9_10_6_5:rrf_fusion").
    pub scenario_id: String,
    /// Run-level correlation ID.
    pub correlation_id: String,
    /// Decision/execution path (e.g., "setup", "execute", "verify").
    pub decision_path: String,
    /// Summary of inputs for this event.
    pub input_summary: String,
    /// Outcome classification.
    pub outcome: Outcome,
    /// Reason code.
    pub reason_code: ReasonCode,
    /// Error code.
    pub error_code: ErrorCode,
    /// Path to an evidence artifact (empty if none).
    pub artifact_path: String,
}

impl TestEvent {
    /// Validate that all required fields are populated (non-empty for strings).
    pub fn validate(&self) -> Result<(), String> {
        if self.timestamp.is_empty() {
            return Err("timestamp is empty".into());
        }
        if self.component.is_empty() {
            return Err("component is empty".into());
        }
        if self.scenario_id.is_empty() {
            return Err("scenario_id is empty".into());
        }
        if self.correlation_id.is_empty() {
            return Err("correlation_id is empty".into());
        }
        // decision_path, input_summary, and artifact_path may be empty.
        Ok(())
    }
}

// -------------------------------------------------------------------------
// TestEventLogger — structured evidence emitter
// -------------------------------------------------------------------------

/// Structured test event logger.
///
/// Each instance has a unique `correlation_id` and emits events to an
/// in-memory buffer.  Optionally, events can be flushed to a `.jsonl`
/// file in an artifact directory.
///
/// # Example
///
/// ```ignore
/// use crate::common::test_event_logger::TestEventLogger;
/// use crate::common::reason_codes::{Outcome, ReasonCode, ErrorCode};
///
/// let mut logger = TestEventLogger::new("search.unit", "ft-e34d9.10.6.5", "rrf_fusion");
/// logger.emit(Outcome::Started, ReasonCode::None, ErrorCode::None)
///     .decision_path("setup")
///     .input_summary("100 ranked items")
///     .log();
/// logger.emit(Outcome::Passed, ReasonCode::Completed, ErrorCode::None)
///     .decision_path("verify")
///     .log();
///
/// assert!(logger.all_passed());
/// ```
pub struct TestEventLogger {
    component: String,
    bead_id: String,
    scenario_name: String,
    correlation_id: String,
    events: Vec<TestEvent>,
    artifact_dir: Option<PathBuf>,
}

impl TestEventLogger {
    /// Create a new logger for a specific component, bead, and scenario.
    pub fn new(component: &str, bead_id: &str, scenario_name: &str) -> Self {
        let seq = LOGGER_COUNTER.fetch_add(1, Ordering::Relaxed);
        let ts = Utc::now().format("%Y%m%dT%H%M%S").to_string();
        let correlation_id = format!("{bead_id}-{ts}-{seq}");

        Self {
            component: component.to_string(),
            bead_id: bead_id.to_string(),
            scenario_name: scenario_name.to_string(),
            correlation_id,
            events: Vec::new(),
            artifact_dir: None,
        }
    }

    /// Set the artifact output directory.  Events will be flushed here on
    /// `flush()` or `Drop`.
    pub fn with_artifact_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.artifact_dir = Some(dir.into());
        self
    }

    /// The auto-generated scenario_id: `{bead_id_sanitized}:{scenario_name}`.
    pub fn scenario_id(&self) -> String {
        let sanitized = self.bead_id.replace(['.', '-'], "_");
        format!("{sanitized}:{}", self.scenario_name)
    }

    /// The run-level correlation ID.
    pub fn correlation_id(&self) -> &str {
        &self.correlation_id
    }

    /// Begin building a new event.
    pub fn emit(
        &mut self,
        outcome: Outcome,
        reason_code: ReasonCode,
        error_code: ErrorCode,
    ) -> EventBuilder<'_> {
        EventBuilder {
            logger: self,
            outcome,
            reason_code,
            error_code,
            decision_path: String::new(),
            input_summary: String::new(),
            artifact_path: String::new(),
        }
    }

    /// Convenience: emit a "started" event with no codes.
    pub fn started(&mut self) {
        self.emit(Outcome::Started, ReasonCode::None, ErrorCode::None)
            .decision_path("test_start")
            .log();
    }

    /// Convenience: emit a "passed" event.
    pub fn passed(&mut self) {
        self.emit(Outcome::Passed, ReasonCode::Completed, ErrorCode::None)
            .decision_path("test_end")
            .log();
    }

    /// Convenience: emit a "failed" event.
    pub fn failed(&mut self, reason: ReasonCode, error: ErrorCode) {
        self.emit(Outcome::Failed, reason, error)
            .decision_path("test_end")
            .log();
    }

    /// Convenience: emit a checkpoint event.
    pub fn checkpoint(&mut self, name: &str) {
        self.emit(Outcome::Checkpoint, ReasonCode::None, ErrorCode::None)
            .decision_path(name)
            .log();
    }

    /// Access the collected events.
    pub fn events(&self) -> &[TestEvent] {
        &self.events
    }

    /// Returns true if no Failed events were recorded.
    pub fn all_passed(&self) -> bool {
        !self.events.iter().any(|e| e.outcome == Outcome::Failed)
    }

    /// Count events matching the given outcome.
    pub fn count_outcome(&self, outcome: Outcome) -> usize {
        self.events.iter().filter(|e| e.outcome == outcome).count()
    }

    /// Flush events to a `.jsonl` file in the artifact directory.
    ///
    /// Returns the path to the written file, or `None` if no artifact_dir
    /// was configured.
    pub fn flush(&self) -> Option<PathBuf> {
        let dir = self.artifact_dir.as_ref()?;
        fs::create_dir_all(dir).ok()?;

        let filename = format!(
            "{}_{}.jsonl",
            self.scenario_name,
            self.correlation_id.replace([':', '-'], "_")
        );
        let path = dir.join(filename);

        let mut file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&path)
            .ok()?;

        for event in &self.events {
            if let Ok(json) = serde_json::to_string(event) {
                let _ = writeln!(file, "{json}");
            }
        }

        Some(path)
    }

    /// Record a raw event (used by EventBuilder).
    fn record(&mut self, event: TestEvent) {
        self.events.push(event);
    }
}

impl Drop for TestEventLogger {
    fn drop(&mut self) {
        // Best-effort flush on drop.
        let _ = self.flush();
    }
}

// -------------------------------------------------------------------------
// EventBuilder — fluent API for constructing events
// -------------------------------------------------------------------------

/// Builder for a single test event.
pub struct EventBuilder<'a> {
    logger: &'a mut TestEventLogger,
    outcome: Outcome,
    reason_code: ReasonCode,
    error_code: ErrorCode,
    decision_path: String,
    input_summary: String,
    artifact_path: String,
}

impl EventBuilder<'_> {
    /// Set the decision path (e.g., "setup", "execute", "verify").
    pub fn decision_path(mut self, path: &str) -> Self {
        self.decision_path = path.to_string();
        self
    }

    /// Set the input summary.
    pub fn input_summary(mut self, summary: &str) -> Self {
        self.input_summary = summary.to_string();
        self
    }

    /// Set the artifact path.
    pub fn artifact_path(mut self, path: &str) -> Self {
        self.artifact_path = path.to_string();
        self
    }

    /// Finalize and record the event.
    pub fn log(self) {
        let event = TestEvent {
            timestamp: Utc::now().to_rfc3339(),
            component: self.logger.component.clone(),
            scenario_id: self.logger.scenario_id(),
            correlation_id: self.logger.correlation_id.clone(),
            decision_path: self.decision_path,
            input_summary: self.input_summary,
            outcome: self.outcome,
            reason_code: self.reason_code,
            error_code: self.error_code,
            artifact_path: self.artifact_path,
        };
        self.logger.record(event);
    }
}

// -------------------------------------------------------------------------
// ScenarioRunner — higher-level test harness with auto start/end logging
// -------------------------------------------------------------------------

/// Runs a test scenario with automatic start/end event logging and
/// artifact flushing.
///
/// ```ignore
/// ScenarioRunner::new("search.integration", "ft-e34d9.10.6.5", "full_pipeline")
///     .run(|logger| {
///         logger.checkpoint("index_created");
///         // ... test logic ...
///         logger.checkpoint("search_verified");
///     });
/// ```
pub struct ScenarioRunner {
    component: String,
    bead_id: String,
    scenario_name: String,
    artifact_dir: Option<PathBuf>,
}

impl ScenarioRunner {
    /// Create a new scenario runner.
    pub fn new(component: &str, bead_id: &str, scenario_name: &str) -> Self {
        Self {
            component: component.to_string(),
            bead_id: bead_id.to_string(),
            scenario_name: scenario_name.to_string(),
            artifact_dir: None,
        }
    }

    /// Set the artifact output directory.
    pub fn artifact_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.artifact_dir = Some(dir.into());
        self
    }

    /// Run the scenario.  Emits Started/Passed/Failed events automatically.
    /// Returns the logger for inspection.
    pub fn run<F>(self, test_fn: F) -> TestEventLogger
    where
        F: FnOnce(&mut TestEventLogger),
    {
        let mut logger =
            TestEventLogger::new(&self.component, &self.bead_id, &self.scenario_name);
        if let Some(dir) = self.artifact_dir {
            logger = logger.with_artifact_dir(dir);
        }

        logger.started();

        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            test_fn(&mut logger);
        }));

        match result {
            Ok(()) => {
                logger.passed();
            }
            Err(panic_payload) => {
                let msg = panic_payload
                    .downcast_ref::<String>()
                    .map(|s| s.as_str())
                    .or_else(|| panic_payload.downcast_ref::<&str>().copied())
                    .unwrap_or("unknown panic");
                logger
                    .emit(
                        Outcome::Failed,
                        ReasonCode::PanicPropagated,
                        ErrorCode::Panic,
                    )
                    .decision_path("test_end")
                    .input_summary(msg)
                    .log();
            }
        }

        logger
    }
}

// -------------------------------------------------------------------------
// Tests
// -------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn logger_generates_events() {
        let mut logger =
            TestEventLogger::new("test.unit", "ft-test-bead", "basic_event_generation");
        logger.started();
        logger.checkpoint("middle");
        logger.passed();

        assert_eq!(logger.events().len(), 3);
        assert!(logger.all_passed());
        assert_eq!(logger.count_outcome(Outcome::Checkpoint), 1);
    }

    #[test]
    fn logger_scenario_id_format() {
        let logger =
            TestEventLogger::new("test.unit", "ft-e34d9.10.6.5", "rrf_fusion");
        assert_eq!(logger.scenario_id(), "ft_e34d9_10_6_5:rrf_fusion");
    }

    #[test]
    fn logger_correlation_id_unique() {
        let l1 = TestEventLogger::new("test.unit", "ft-test", "a");
        let l2 = TestEventLogger::new("test.unit", "ft-test", "b");
        assert_ne!(l1.correlation_id(), l2.correlation_id());
    }

    #[test]
    fn event_validation() {
        let mut logger = TestEventLogger::new("test.unit", "ft-test", "validation");
        logger.started();
        let event = &logger.events()[0];
        assert!(event.validate().is_ok());
    }

    #[test]
    fn event_serde_roundtrip() {
        let mut logger = TestEventLogger::new("test.unit", "ft-test", "serde");
        logger
            .emit(Outcome::Passed, ReasonCode::Completed, ErrorCode::None)
            .decision_path("verify")
            .input_summary("42 items")
            .artifact_path("/tmp/test.jsonl")
            .log();

        let event = &logger.events()[0];
        let json = serde_json::to_string(event).unwrap();
        let back: TestEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(back.component, "test.unit");
        assert_eq!(back.outcome, Outcome::Passed);
        assert_eq!(back.reason_code, ReasonCode::Completed);
        assert_eq!(back.artifact_path, "/tmp/test.jsonl");
    }

    #[test]
    fn logger_all_passed_false_on_failure() {
        let mut logger = TestEventLogger::new("test.unit", "ft-test", "failure");
        logger.started();
        logger.failed(ReasonCode::TimeoutExpired, ErrorCode::Timeout);
        assert!(!logger.all_passed());
    }

    #[test]
    fn logger_flush_to_file() {
        let tmp = tempfile::tempdir().unwrap();
        let mut logger = TestEventLogger::new("test.unit", "ft-test", "flush_test")
            .with_artifact_dir(tmp.path());
        logger.started();
        logger.passed();

        let path = logger.flush().unwrap();
        assert!(path.exists());

        let content = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(lines.len(), 2);

        // Each line should be valid JSON matching our schema.
        for line in &lines {
            let event: TestEvent = serde_json::from_str(line).unwrap();
            assert!(event.validate().is_ok());
        }
    }

    #[test]
    fn logger_all_required_fields_populated() {
        let mut logger = TestEventLogger::new("comp.test", "ft-bead.1", "required_fields");
        logger
            .emit(Outcome::Started, ReasonCode::None, ErrorCode::None)
            .decision_path("init")
            .input_summary("empty")
            .log();

        let event = &logger.events()[0];
        let json: serde_json::Value = serde_json::to_value(event).unwrap();

        // Verify all ten ADR-0012 required fields are present.
        let required = [
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
        for field in required {
            assert!(
                json.get(field).is_some(),
                "Missing required field: {field}"
            );
        }
    }

    #[test]
    fn scenario_runner_success_path() {
        let logger = ScenarioRunner::new("test.unit", "ft-test", "runner_success").run(
            |logger| {
                logger.checkpoint("step_1");
                logger.checkpoint("step_2");
            },
        );

        assert!(logger.all_passed());
        // started + 2 checkpoints + passed = 4
        assert_eq!(logger.events().len(), 4);
        assert_eq!(logger.count_outcome(Outcome::Started), 1);
        assert_eq!(logger.count_outcome(Outcome::Checkpoint), 2);
        assert_eq!(logger.count_outcome(Outcome::Passed), 1);
    }

    #[test]
    fn scenario_runner_captures_panic() {
        let logger = ScenarioRunner::new("test.unit", "ft-test", "runner_panic").run(
            |_logger| {
                panic!("intentional test panic");
            },
        );

        assert!(!logger.all_passed());
        assert_eq!(logger.count_outcome(Outcome::Failed), 1);

        let failed = logger
            .events()
            .iter()
            .find(|e| e.outcome == Outcome::Failed)
            .unwrap();
        assert_eq!(failed.reason_code, ReasonCode::PanicPropagated);
        assert_eq!(failed.error_code, ErrorCode::Panic);
    }

    #[test]
    fn emit_builder_fluent_api() {
        let mut logger = TestEventLogger::new("test.unit", "ft-test", "fluent");
        logger
            .emit(
                Outcome::Checkpoint,
                ReasonCode::ChaosInjected,
                ErrorCode::None,
            )
            .decision_path("chaos_point")
            .input_summary("delay_ms=500")
            .artifact_path("/tmp/chaos.log")
            .log();

        let event = &logger.events()[0];
        assert_eq!(event.decision_path, "chaos_point");
        assert_eq!(event.input_summary, "delay_ms=500");
        assert_eq!(event.artifact_path, "/tmp/chaos.log");
        assert_eq!(event.reason_code, ReasonCode::ChaosInjected);
    }

    #[test]
    fn multiple_scenarios_independent_correlation_ids() {
        let l1 = ScenarioRunner::new("test.unit", "ft-test", "scenario_a")
            .run(|l| l.checkpoint("a"));
        let l2 = ScenarioRunner::new("test.unit", "ft-test", "scenario_b")
            .run(|l| l.checkpoint("b"));

        assert_ne!(l1.correlation_id(), l2.correlation_id());
        // Each has: started + checkpoint + passed = 3
        assert_eq!(l1.events().len(), 3);
        assert_eq!(l2.events().len(), 3);
    }
}
