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
            panic!("poison ft-test-log sink");
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
}
