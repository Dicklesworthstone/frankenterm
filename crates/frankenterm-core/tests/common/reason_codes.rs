//! Structured reason/error code taxonomy for test evidence (ft-e34d9.10.6.5).
//!
//! Provides a unified vocabulary for outcome classification across unit,
//! integration, and e2e tests. Maps to the `reason_code` and `error_code`
//! fields in the structured logging contract (ADR-0012).

use std::fmt;

/// Reason code: explains *why* an outcome occurred.
///
/// Used in the `reason_code` field of structured log events.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReasonCode {
    /// No specific reason (happy path).
    None,
    /// Operation completed normally.
    Completed,
    /// Timeout expired before completion.
    TimeoutExpired,
    /// Channel closed by peer.
    ChannelClosed,
    /// No permits available (semaphore exhausted).
    NoPermits,
    /// Operation was cancelled via structured cancellation.
    CancellationRequested,
    /// Cancellation caused data loss (a violation).
    CancellationLoss,
    /// Scope owner shut down.
    ScopeShutdown,
    /// Panic propagated from a subtask.
    PanicPropagated,
    /// I/O error on underlying resource.
    IoError,
    /// Serialization/deserialization failure.
    SerdeError,
    /// Resource (file, socket, lock) contention.
    ResourceContention,
    /// Rate limit exceeded.
    RateLimited,
    /// Configuration invalid.
    ConfigInvalid,
    /// Precondition not met.
    PreconditionFailed,
    /// Schema migration required.
    SchemaMigration,
    /// Chaos/fault injection triggered.
    ChaosInjected,
    /// Test infrastructure setup failure.
    SetupFailed,
    /// Invariant violation detected during test.
    InvariantViolation,
    /// Oracle check failed (LabRuntime).
    OracleFailure,
    /// DPOR exploration found divergent schedule.
    ScheduleDivergence,
}

impl fmt::Display for ReasonCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = serde_json::to_value(self)
            .ok()
            .and_then(|v| v.as_str().map(String::from))
            .unwrap_or_else(|| format!("{self:?}"));
        f.write_str(&s)
    }
}

/// Error code: classifies the *type* of failure.
///
/// Used in the `error_code` field of structured log events.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    /// No error.
    None,
    /// Assertion failed in test.
    AssertionFailed,
    /// Timeout (wall-clock or virtual).
    Timeout,
    /// Runtime panic.
    Panic,
    /// I/O failure.
    Io,
    /// Serialization round-trip failure.
    Serde,
    /// Deadlock detected.
    Deadlock,
    /// Task leak (obligation not fulfilled).
    TaskLeak,
    /// Data loss detected.
    DataLoss,
    /// Invariant violation (safety property).
    SafetyViolation,
    /// Liveness violation (progress property).
    LivenessViolation,
    /// Configuration error.
    Config,
    /// Test harness internal error.
    HarnessInternal,
    /// External dependency failure (e.g., rch worker).
    ExternalDependency,
}

impl fmt::Display for ErrorCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = serde_json::to_value(self)
            .ok()
            .and_then(|v| v.as_str().map(String::from))
            .unwrap_or_else(|| format!("{self:?}"));
        f.write_str(&s)
    }
}

/// Outcome classification for a test event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    /// Test/scenario started.
    Started,
    /// Test/scenario passed.
    Passed,
    /// Test/scenario failed.
    Failed,
    /// Test/scenario skipped.
    Skipped,
    /// Checkpoint reached during test.
    Checkpoint,
    /// Setup phase completed.
    SetupComplete,
    /// Teardown phase completed.
    TeardownComplete,
}

impl fmt::Display for Outcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = serde_json::to_value(self)
            .ok()
            .and_then(|v| v.as_str().map(String::from))
            .unwrap_or_else(|| format!("{self:?}"));
        f.write_str(&s)
    }
}

// -------------------------------------------------------------------------
// Tests
// -------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reason_code_serde_roundtrip() {
        let codes = [
            ReasonCode::None,
            ReasonCode::TimeoutExpired,
            ReasonCode::CancellationLoss,
            ReasonCode::OracleFailure,
        ];
        for code in codes {
            let json = serde_json::to_string(&code).unwrap();
            let back: ReasonCode = serde_json::from_str(&json).unwrap();
            assert_eq!(code, back);
        }
    }

    #[test]
    fn error_code_serde_roundtrip() {
        let codes = [
            ErrorCode::None,
            ErrorCode::Timeout,
            ErrorCode::DataLoss,
            ErrorCode::SafetyViolation,
        ];
        for code in codes {
            let json = serde_json::to_string(&code).unwrap();
            let back: ErrorCode = serde_json::from_str(&json).unwrap();
            assert_eq!(code, back);
        }
    }

    #[test]
    fn outcome_serde_roundtrip() {
        let outcomes = [
            Outcome::Started,
            Outcome::Passed,
            Outcome::Failed,
            Outcome::Checkpoint,
        ];
        for outcome in outcomes {
            let json = serde_json::to_string(&outcome).unwrap();
            let back: Outcome = serde_json::from_str(&json).unwrap();
            assert_eq!(outcome, back);
        }
    }

    #[test]
    fn display_matches_serde() {
        assert_eq!(ReasonCode::TimeoutExpired.to_string(), "timeout_expired");
        assert_eq!(ErrorCode::DataLoss.to_string(), "data_loss");
        assert_eq!(Outcome::Passed.to_string(), "passed");
    }
}
