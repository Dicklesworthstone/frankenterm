//! Unified timeout, backoff, and error taxonomy for network I/O paths.
//!
//! Provides:
//! - [`NetworkErrorKind`]: transient/permanent/degraded classification
//! - [`RetryPolicy`]: configurable exponential backoff with jitter
//! - [`TimeoutPolicy`]: per-subsystem timeout defaults with override
//! - [`BackoffCalculator`]: stateful retry delay computation
//! - [`IoOutcome`]: classifies raw I/O results into the taxonomy

use std::time::Duration;

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Network error taxonomy
// ---------------------------------------------------------------------------

/// Classification of a network error for retry/circuit-breaker decisions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NetworkErrorKind {
    /// Transient — retryable after a delay.
    /// Examples: timeout, connection refused (server restarting), temporary
    /// DNS failure, TCP reset.
    Transient,

    /// Permanent — retrying will not help.
    /// Examples: authentication failure, resource not found, invalid request
    /// payload, certificate validation error.
    Permanent,

    /// Degraded — the subsystem is in a reduced-capacity state.
    /// Examples: circuit open, rate-limited, resource exhausted, upstream
    /// overloaded.  Retry may succeed but with longer backoff.
    Degraded,
}

impl NetworkErrorKind {
    /// Whether a retry attempt is reasonable for this error class.
    #[must_use]
    pub fn is_retryable(self) -> bool {
        matches!(self, Self::Transient | Self::Degraded)
    }

    /// Backoff multiplier hint — degraded errors warrant longer waits.
    #[must_use]
    pub fn backoff_hint(self) -> f64 {
        match self {
            Self::Transient => 1.0,
            Self::Degraded => 2.0,
            Self::Permanent => 0.0,
        }
    }
}

/// Classify an `std::io::Error` into the network taxonomy.
#[must_use]
pub fn classify_io_error(err: &std::io::Error) -> NetworkErrorKind {
    use std::io::ErrorKind;
    match err.kind() {
        // Transient
        ErrorKind::TimedOut
        | ErrorKind::ConnectionRefused
        | ErrorKind::ConnectionReset
        | ErrorKind::ConnectionAborted
        | ErrorKind::Interrupted
        | ErrorKind::BrokenPipe
        | ErrorKind::WouldBlock => NetworkErrorKind::Transient,

        // Permanent
        ErrorKind::NotFound
        | ErrorKind::PermissionDenied
        | ErrorKind::InvalidInput
        | ErrorKind::InvalidData
        | ErrorKind::AddrNotAvailable
        | ErrorKind::Unsupported => NetworkErrorKind::Permanent,

        // Degraded (resource pressure)
        ErrorKind::AddrInUse | ErrorKind::OutOfMemory => NetworkErrorKind::Degraded,

        // Default unknown → transient (safe for retry)
        _ => NetworkErrorKind::Transient,
    }
}

// ---------------------------------------------------------------------------
// Timeout policy
// ---------------------------------------------------------------------------

/// Named subsystem for timeout configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Subsystem {
    /// WezTerm CLI commands.
    WeztermCli,
    /// WezTerm mux socket operations.
    WeztermMux,
    /// IPC (Unix socket) listener/handler.
    Ipc,
    /// Outbound HTTP / web server operations.
    Web,
    /// Search / indexing backend operations.
    Search,
    /// Storage (SQLite, file I/O).
    Storage,
    /// Distributed cluster communication.
    Distributed,
    /// Pane content capture (tailer polling).
    PaneCapture,
}

/// Configurable timeouts keyed by subsystem.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TimeoutPolicy {
    /// Connect timeout (TCP handshake, socket bind).
    pub connect: Duration,
    /// Read/write timeout for individual I/O operations.
    pub io_operation: Duration,
    /// Overall request deadline (end-to-end including retries).
    pub request_deadline: Duration,
}

impl TimeoutPolicy {
    /// Sensible defaults for a given subsystem.
    #[must_use]
    pub fn for_subsystem(subsystem: Subsystem) -> Self {
        match subsystem {
            Subsystem::WeztermCli => Self {
                connect: Duration::from_secs(2),
                io_operation: Duration::from_secs(5),
                request_deadline: Duration::from_secs(10),
            },
            Subsystem::WeztermMux => Self {
                connect: Duration::from_secs(1),
                io_operation: Duration::from_secs(3),
                request_deadline: Duration::from_secs(8),
            },
            Subsystem::Ipc => Self {
                connect: Duration::from_millis(500),
                io_operation: Duration::from_secs(2),
                request_deadline: Duration::from_secs(5),
            },
            Subsystem::Web => Self {
                connect: Duration::from_secs(5),
                io_operation: Duration::from_secs(30),
                request_deadline: Duration::from_secs(60),
            },
            Subsystem::Search => Self {
                connect: Duration::from_millis(200),
                io_operation: Duration::from_secs(2),
                request_deadline: Duration::from_secs(5),
            },
            Subsystem::Storage => Self {
                connect: Duration::from_millis(100),
                io_operation: Duration::from_secs(5),
                request_deadline: Duration::from_secs(15),
            },
            Subsystem::Distributed => Self {
                connect: Duration::from_secs(5),
                io_operation: Duration::from_secs(10),
                request_deadline: Duration::from_secs(30),
            },
            Subsystem::PaneCapture => Self {
                connect: Duration::from_millis(100),
                io_operation: Duration::from_millis(500),
                request_deadline: Duration::from_secs(2),
            },
        }
    }
}

// ---------------------------------------------------------------------------
// Retry / backoff policy
// ---------------------------------------------------------------------------

/// Configuration for exponential backoff with jitter.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RetryPolicy {
    /// Maximum number of retry attempts (0 = no retries).
    pub max_attempts: u32,
    /// Initial delay before the first retry.
    pub initial_backoff: Duration,
    /// Maximum delay between retries.
    pub max_backoff: Duration,
    /// Multiplicative factor applied per attempt (e.g. 2.0 for doubling).
    pub backoff_multiplier: f64,
    /// Fraction of delay randomized: 0.0 = no jitter, 1.0 = full jitter.
    /// Full jitter: delay = random(0, calculated_delay).
    pub jitter_factor: f64,
}

impl RetryPolicy {
    /// Aggressive retry — low latency, few attempts, small jitter.
    #[must_use]
    pub fn aggressive() -> Self {
        Self {
            max_attempts: 2,
            initial_backoff: Duration::from_millis(50),
            max_backoff: Duration::from_millis(200),
            backoff_multiplier: 2.0,
            jitter_factor: 0.25,
        }
    }

    /// Standard retry — balanced for most network operations.
    #[must_use]
    pub fn standard() -> Self {
        Self {
            max_attempts: 3,
            initial_backoff: Duration::from_millis(100),
            max_backoff: Duration::from_secs(5),
            backoff_multiplier: 2.0,
            jitter_factor: 0.5,
        }
    }

    /// Patient retry — longer waits, more attempts, full jitter.
    #[must_use]
    pub fn patient() -> Self {
        Self {
            max_attempts: 5,
            initial_backoff: Duration::from_millis(500),
            max_backoff: Duration::from_secs(30),
            backoff_multiplier: 2.0,
            jitter_factor: 1.0,
        }
    }

    /// No retries — fail immediately.
    #[must_use]
    pub fn no_retry() -> Self {
        Self {
            max_attempts: 0,
            initial_backoff: Duration::ZERO,
            max_backoff: Duration::ZERO,
            backoff_multiplier: 1.0,
            jitter_factor: 0.0,
        }
    }
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self::standard()
    }
}

// ---------------------------------------------------------------------------
// Backoff calculator (stateful)
// ---------------------------------------------------------------------------

/// Tracks retry state and computes next delay.
#[derive(Debug, Clone)]
pub struct BackoffCalculator {
    policy: RetryPolicy,
    attempt: u32,
    /// Deterministic seed for jitter (incremented per call so tests stay
    /// reproducible without pulling in a random crate).
    jitter_seed: u64,
}

impl BackoffCalculator {
    /// Create a new calculator from a policy.
    #[must_use]
    pub fn new(policy: RetryPolicy) -> Self {
        Self {
            policy,
            attempt: 0,
            jitter_seed: 0,
        }
    }

    /// Create a calculator with a fixed jitter seed for deterministic tests.
    #[must_use]
    pub fn with_seed(policy: RetryPolicy, seed: u64) -> Self {
        Self {
            policy,
            attempt: 0,
            jitter_seed: seed,
        }
    }

    /// Current attempt number (0 = haven't retried yet).
    #[must_use]
    pub fn attempt(&self) -> u32 {
        self.attempt
    }

    /// Whether another retry is allowed.
    #[must_use]
    pub fn can_retry(&self) -> bool {
        self.attempt < self.policy.max_attempts
    }

    /// Whether another retry is allowed for this error kind.
    #[must_use]
    pub fn should_retry(&self, kind: NetworkErrorKind) -> bool {
        self.can_retry() && kind.is_retryable()
    }

    /// Compute the next backoff delay and advance the attempt counter.
    ///
    /// Returns `None` if retries are exhausted.
    pub fn next_delay(&mut self) -> Option<Duration> {
        self.next_delay_for_kind(NetworkErrorKind::Transient)
    }

    /// Compute the next backoff delay accounting for error severity.
    ///
    /// Degraded errors use a longer base delay. Permanent errors return `None`.
    pub fn next_delay_for_kind(&mut self, kind: NetworkErrorKind) -> Option<Duration> {
        if !self.should_retry(kind) {
            return None;
        }

        let base_ms = self.policy.initial_backoff.as_millis() as f64
            * self.policy.backoff_multiplier.powi(self.attempt as i32)
            * kind.backoff_hint();

        let capped_ms = base_ms.min(self.policy.max_backoff.as_millis() as f64);

        let jittered_ms = if self.policy.jitter_factor > 0.0 {
            let jitter_range = capped_ms * self.policy.jitter_factor;
            let jitter_frac = self.pseudo_random_fraction();
            let jitter = jitter_range * jitter_frac;
            (capped_ms - jitter_range) + jitter
        } else {
            capped_ms
        };

        self.attempt += 1;

        let delay_ms = jittered_ms.max(0.0) as u64;
        Some(Duration::from_millis(delay_ms))
    }

    /// Reset for a fresh retry sequence.
    pub fn reset(&mut self) {
        self.attempt = 0;
    }

    /// Simple deterministic pseudo-random in [0, 1).
    /// Uses a splitmix64-style hash to avoid external deps.
    fn pseudo_random_fraction(&mut self) -> f64 {
        self.jitter_seed = self.jitter_seed.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.jitter_seed;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^= z >> 31;
        (z as f64) / (u64::MAX as f64)
    }
}

// ---------------------------------------------------------------------------
// IoOutcome — wraps a raw result with classification
// ---------------------------------------------------------------------------

/// Wraps the result of a network I/O operation with error classification.
#[derive(Debug)]
pub enum IoOutcome<T> {
    /// Operation succeeded.
    Ok(T),
    /// Operation failed with a classified error.
    Err {
        kind: NetworkErrorKind,
        message: String,
    },
}

impl<T> IoOutcome<T> {
    /// Build from a `std::io::Result`.
    pub fn from_io(result: std::io::Result<T>) -> Self {
        match result {
            Ok(val) => Self::Ok(val),
            Err(e) => Self::Err {
                kind: classify_io_error(&e),
                message: e.to_string(),
            },
        }
    }

    /// Build a transient error.
    pub fn transient(msg: impl Into<String>) -> Self {
        Self::Err {
            kind: NetworkErrorKind::Transient,
            message: msg.into(),
        }
    }

    /// Build a permanent error.
    pub fn permanent(msg: impl Into<String>) -> Self {
        Self::Err {
            kind: NetworkErrorKind::Permanent,
            message: msg.into(),
        }
    }

    /// Build a degraded error.
    pub fn degraded(msg: impl Into<String>) -> Self {
        Self::Err {
            kind: NetworkErrorKind::Degraded,
            message: msg.into(),
        }
    }

    /// Whether the outcome is successful.
    #[must_use]
    pub fn is_ok(&self) -> bool {
        matches!(self, Self::Ok(_))
    }

    /// Extract the error kind, if any.
    #[must_use]
    pub fn error_kind(&self) -> Option<NetworkErrorKind> {
        match self {
            Self::Ok(_) => None,
            Self::Err { kind, .. } => Some(*kind),
        }
    }
}

// ---------------------------------------------------------------------------
// Composite reliability config
// ---------------------------------------------------------------------------

/// Combined timeout + retry configuration for a subsystem.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReliabilityConfig {
    /// Which subsystem this config applies to.
    pub subsystem: Subsystem,
    /// Timeout policy.
    pub timeouts: TimeoutPolicy,
    /// Retry/backoff policy.
    pub retry: RetryPolicy,
}

impl ReliabilityConfig {
    /// Build a default config for the given subsystem.
    #[must_use]
    pub fn for_subsystem(subsystem: Subsystem) -> Self {
        let retry = match subsystem {
            Subsystem::WeztermCli | Subsystem::WeztermMux => RetryPolicy::aggressive(),
            Subsystem::PaneCapture => RetryPolicy::no_retry(),
            Subsystem::Distributed | Subsystem::Web => RetryPolicy::patient(),
            _ => RetryPolicy::standard(),
        };
        Self {
            subsystem,
            timeouts: TimeoutPolicy::for_subsystem(subsystem),
            retry,
        }
    }

    /// Create a backoff calculator from this config's retry policy.
    #[must_use]
    pub fn backoff(&self) -> BackoffCalculator {
        BackoffCalculator::new(self.retry.clone())
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(clippy::float_cmp)]
mod tests {
    use super::*;

    // -- NetworkErrorKind --

    #[test]
    fn transient_is_retryable() {
        assert!(NetworkErrorKind::Transient.is_retryable());
    }

    #[test]
    fn degraded_is_retryable() {
        assert!(NetworkErrorKind::Degraded.is_retryable());
    }

    #[test]
    fn permanent_is_not_retryable() {
        assert!(!NetworkErrorKind::Permanent.is_retryable());
    }

    #[test]
    fn backoff_hint_ordering() {
        assert!(
            NetworkErrorKind::Degraded.backoff_hint() > NetworkErrorKind::Transient.backoff_hint()
        );
        assert_eq!(NetworkErrorKind::Permanent.backoff_hint(), 0.0);
    }

    // -- classify_io_error --

    #[test]
    fn classify_timeout_is_transient() {
        let err = std::io::Error::new(std::io::ErrorKind::TimedOut, "timed out");
        assert_eq!(classify_io_error(&err), NetworkErrorKind::Transient);
    }

    #[test]
    fn classify_connection_refused_is_transient() {
        let err = std::io::Error::new(std::io::ErrorKind::ConnectionRefused, "refused");
        assert_eq!(classify_io_error(&err), NetworkErrorKind::Transient);
    }

    #[test]
    fn classify_permission_denied_is_permanent() {
        let err = std::io::Error::new(std::io::ErrorKind::PermissionDenied, "denied");
        assert_eq!(classify_io_error(&err), NetworkErrorKind::Permanent);
    }

    #[test]
    fn classify_not_found_is_permanent() {
        let err = std::io::Error::new(std::io::ErrorKind::NotFound, "not found");
        assert_eq!(classify_io_error(&err), NetworkErrorKind::Permanent);
    }

    #[test]
    fn classify_addr_in_use_is_degraded() {
        let err = std::io::Error::new(std::io::ErrorKind::AddrInUse, "in use");
        assert_eq!(classify_io_error(&err), NetworkErrorKind::Degraded);
    }

    #[test]
    fn classify_out_of_memory_is_degraded() {
        let err = std::io::Error::new(std::io::ErrorKind::OutOfMemory, "oom");
        assert_eq!(classify_io_error(&err), NetworkErrorKind::Degraded);
    }

    #[test]
    fn classify_unknown_defaults_to_transient() {
        let err = std::io::Error::other("mystery");
        assert_eq!(classify_io_error(&err), NetworkErrorKind::Transient);
    }

    // -- TimeoutPolicy --

    #[test]
    fn timeout_policy_wezterm_cli_defaults() {
        let p = TimeoutPolicy::for_subsystem(Subsystem::WeztermCli);
        assert_eq!(p.connect, Duration::from_secs(2));
        assert_eq!(p.io_operation, Duration::from_secs(5));
        assert_eq!(p.request_deadline, Duration::from_secs(10));
    }

    #[test]
    fn timeout_policy_ipc_is_faster() {
        let ipc = TimeoutPolicy::for_subsystem(Subsystem::Ipc);
        let dist = TimeoutPolicy::for_subsystem(Subsystem::Distributed);
        assert!(ipc.connect < dist.connect);
        assert!(ipc.request_deadline < dist.request_deadline);
    }

    #[test]
    fn all_subsystems_have_positive_timeouts() {
        let subsystems = [
            Subsystem::WeztermCli,
            Subsystem::WeztermMux,
            Subsystem::Ipc,
            Subsystem::Web,
            Subsystem::Search,
            Subsystem::Storage,
            Subsystem::Distributed,
            Subsystem::PaneCapture,
        ];
        for s in subsystems {
            let p = TimeoutPolicy::for_subsystem(s);
            assert!(p.connect > Duration::ZERO, "{s:?} connect");
            assert!(p.io_operation > Duration::ZERO, "{s:?} io_operation");
            assert!(p.request_deadline > Duration::ZERO, "{s:?} deadline");
        }
    }

    #[test]
    fn deadline_at_least_as_large_as_io_operation() {
        let subsystems = [
            Subsystem::WeztermCli,
            Subsystem::WeztermMux,
            Subsystem::Ipc,
            Subsystem::Web,
            Subsystem::Search,
            Subsystem::Storage,
            Subsystem::Distributed,
            Subsystem::PaneCapture,
        ];
        for s in subsystems {
            let p = TimeoutPolicy::for_subsystem(s);
            assert!(
                p.request_deadline >= p.io_operation,
                "{s:?}: deadline < io_operation"
            );
        }
    }

    // -- RetryPolicy presets --

    #[test]
    fn aggressive_few_attempts() {
        let p = RetryPolicy::aggressive();
        assert_eq!(p.max_attempts, 2);
        assert!(p.initial_backoff <= Duration::from_millis(100));
    }

    #[test]
    fn standard_moderate_attempts() {
        let p = RetryPolicy::standard();
        assert_eq!(p.max_attempts, 3);
    }

    #[test]
    fn patient_many_attempts() {
        let p = RetryPolicy::patient();
        assert!(p.max_attempts >= 5);
        assert!(p.max_backoff >= Duration::from_secs(10));
    }

    #[test]
    fn no_retry_zero_attempts() {
        let p = RetryPolicy::no_retry();
        assert_eq!(p.max_attempts, 0);
    }

    // -- BackoffCalculator --

    #[test]
    fn calculator_starts_at_zero() {
        let calc = BackoffCalculator::new(RetryPolicy::standard());
        assert_eq!(calc.attempt(), 0);
    }

    #[test]
    fn calculator_can_retry_when_under_max() {
        let calc = BackoffCalculator::new(RetryPolicy::standard());
        assert!(calc.can_retry());
    }

    #[test]
    fn calculator_exhausts_after_max_attempts() {
        let policy = RetryPolicy {
            max_attempts: 2,
            ..RetryPolicy::standard()
        };
        let mut calc = BackoffCalculator::new(policy);
        assert!(calc.next_delay().is_some()); // attempt 0→1
        assert!(calc.next_delay().is_some()); // attempt 1→2
        assert!(calc.next_delay().is_none()); // exhausted
        assert_eq!(calc.attempt(), 2);
    }

    #[test]
    fn calculator_no_retry_returns_none() {
        let mut calc = BackoffCalculator::new(RetryPolicy::no_retry());
        assert!(calc.next_delay().is_none());
    }

    #[test]
    fn calculator_permanent_error_returns_none() {
        let mut calc = BackoffCalculator::new(RetryPolicy::standard());
        assert!(
            calc.next_delay_for_kind(NetworkErrorKind::Permanent)
                .is_none()
        );
    }

    #[test]
    fn calculator_delays_grow_with_attempts() {
        let policy = RetryPolicy {
            max_attempts: 4,
            initial_backoff: Duration::from_millis(100),
            max_backoff: Duration::from_secs(10),
            backoff_multiplier: 2.0,
            jitter_factor: 0.0, // no jitter for deterministic test
        };
        let mut calc = BackoffCalculator::new(policy);
        let d0 = calc.next_delay().unwrap();
        let d1 = calc.next_delay().unwrap();
        let d2 = calc.next_delay().unwrap();
        assert!(d1 >= d0, "d1={d1:?} should >= d0={d0:?}");
        assert!(d2 >= d1, "d2={d2:?} should >= d1={d1:?}");
    }

    #[test]
    fn calculator_respects_max_backoff() {
        let policy = RetryPolicy {
            max_attempts: 10,
            initial_backoff: Duration::from_secs(1),
            max_backoff: Duration::from_secs(2),
            backoff_multiplier: 10.0,
            jitter_factor: 0.0,
        };
        let mut calc = BackoffCalculator::new(policy.clone());
        for _ in 0..10 {
            if let Some(d) = calc.next_delay() {
                assert!(
                    d <= policy.max_backoff,
                    "delay {d:?} > max {0:?}",
                    policy.max_backoff
                );
            }
        }
    }

    #[test]
    fn calculator_jitter_varies_delay() {
        let policy = RetryPolicy {
            max_attempts: 10,
            initial_backoff: Duration::from_secs(1),
            max_backoff: Duration::from_secs(60),
            backoff_multiplier: 1.0,
            jitter_factor: 1.0,
        };
        let mut calc = BackoffCalculator::with_seed(policy, 42);
        let delays: Vec<_> = (0..5).filter_map(|_| calc.next_delay()).collect();
        // With full jitter, not all delays should be identical
        let all_same = delays.windows(2).all(|w| w[0] == w[1]);
        assert!(
            !all_same,
            "full jitter should produce varying delays: {delays:?}"
        );
    }

    #[test]
    fn calculator_reset_restores_attempts() {
        let mut calc = BackoffCalculator::new(RetryPolicy {
            max_attempts: 1,
            ..RetryPolicy::standard()
        });
        assert!(calc.next_delay().is_some());
        assert!(calc.next_delay().is_none());
        calc.reset();
        assert_eq!(calc.attempt(), 0);
        assert!(calc.next_delay().is_some());
    }

    #[test]
    fn degraded_errors_get_longer_delays() {
        let policy = RetryPolicy {
            max_attempts: 5,
            initial_backoff: Duration::from_millis(100),
            max_backoff: Duration::from_secs(60),
            backoff_multiplier: 2.0,
            jitter_factor: 0.0,
        };
        let mut transient_calc = BackoffCalculator::new(policy.clone());
        let mut degraded_calc = BackoffCalculator::new(policy);
        let t_delay = transient_calc
            .next_delay_for_kind(NetworkErrorKind::Transient)
            .unwrap();
        let d_delay = degraded_calc
            .next_delay_for_kind(NetworkErrorKind::Degraded)
            .unwrap();
        assert!(
            d_delay > t_delay,
            "degraded={d_delay:?} should > transient={t_delay:?}"
        );
    }

    // -- IoOutcome --

    #[test]
    fn io_outcome_ok() {
        let outcome = IoOutcome::from_io(Ok::<_, std::io::Error>(42));
        assert!(outcome.is_ok());
        assert!(outcome.error_kind().is_none());
    }

    #[test]
    fn io_outcome_timeout_is_transient() {
        let err = std::io::Error::new(std::io::ErrorKind::TimedOut, "t/o");
        let outcome = IoOutcome::<()>::from_io(Err(err));
        assert!(!outcome.is_ok());
        assert_eq!(outcome.error_kind(), Some(NetworkErrorKind::Transient));
    }

    #[test]
    fn io_outcome_constructors() {
        let t = IoOutcome::<()>::transient("test");
        assert_eq!(t.error_kind(), Some(NetworkErrorKind::Transient));
        let p = IoOutcome::<()>::permanent("test");
        assert_eq!(p.error_kind(), Some(NetworkErrorKind::Permanent));
        let d = IoOutcome::<()>::degraded("test");
        assert_eq!(d.error_kind(), Some(NetworkErrorKind::Degraded));
    }

    // -- ReliabilityConfig --

    #[test]
    fn reliability_config_wezterm_cli_uses_aggressive() {
        let cfg = ReliabilityConfig::for_subsystem(Subsystem::WeztermCli);
        assert_eq!(cfg.retry.max_attempts, 2);
    }

    #[test]
    fn reliability_config_pane_capture_no_retry() {
        let cfg = ReliabilityConfig::for_subsystem(Subsystem::PaneCapture);
        assert_eq!(cfg.retry.max_attempts, 0);
    }

    #[test]
    fn reliability_config_distributed_is_patient() {
        let cfg = ReliabilityConfig::for_subsystem(Subsystem::Distributed);
        assert!(cfg.retry.max_attempts >= 5);
    }

    #[test]
    fn reliability_config_backoff_creates_calculator() {
        let cfg = ReliabilityConfig::for_subsystem(Subsystem::Search);
        let calc = cfg.backoff();
        assert_eq!(calc.attempt(), 0);
        assert!(calc.can_retry());
    }

    // -- Serde roundtrip --

    #[test]
    fn error_kind_serde_roundtrip() {
        for kind in [
            NetworkErrorKind::Transient,
            NetworkErrorKind::Permanent,
            NetworkErrorKind::Degraded,
        ] {
            let json = serde_json::to_string(&kind).unwrap();
            let back: NetworkErrorKind = serde_json::from_str(&json).unwrap();
            assert_eq!(kind, back);
        }
    }

    #[test]
    fn subsystem_serde_roundtrip() {
        let subsystems = [
            Subsystem::WeztermCli,
            Subsystem::WeztermMux,
            Subsystem::Ipc,
            Subsystem::Web,
            Subsystem::Search,
            Subsystem::Storage,
            Subsystem::Distributed,
            Subsystem::PaneCapture,
        ];
        for s in subsystems {
            let json = serde_json::to_string(&s).unwrap();
            let back: Subsystem = serde_json::from_str(&json).unwrap();
            assert_eq!(s, back);
        }
    }

    #[test]
    fn timeout_policy_serde_roundtrip() {
        let p = TimeoutPolicy::for_subsystem(Subsystem::Web);
        let json = serde_json::to_string(&p).unwrap();
        let back: TimeoutPolicy = serde_json::from_str(&json).unwrap();
        assert_eq!(p.connect, back.connect);
        assert_eq!(p.io_operation, back.io_operation);
        assert_eq!(p.request_deadline, back.request_deadline);
    }

    #[test]
    fn retry_policy_serde_roundtrip() {
        let p = RetryPolicy::patient();
        let json = serde_json::to_string(&p).unwrap();
        let back: RetryPolicy = serde_json::from_str(&json).unwrap();
        assert_eq!(p.max_attempts, back.max_attempts);
        assert_eq!(p.initial_backoff, back.initial_backoff);
        assert_eq!(p.max_backoff, back.max_backoff);
    }

    #[test]
    fn reliability_config_serde_roundtrip() {
        let cfg = ReliabilityConfig::for_subsystem(Subsystem::Ipc);
        let json = serde_json::to_string(&cfg).unwrap();
        let back: ReliabilityConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(cfg.subsystem, back.subsystem);
        assert_eq!(cfg.timeouts.connect, back.timeouts.connect);
        assert_eq!(cfg.retry.max_attempts, back.retry.max_attempts);
    }
}
