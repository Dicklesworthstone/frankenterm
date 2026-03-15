//! Asupersync runtime observability, operability, and SLO gates (ft-e34d9.10.7).
//!
//! Operationalizes the asupersync runtime migration with runtime-aware telemetry
//! schemas, health/doctor surfaces, SLO conformance gates, and automated alerting.
//!
//! # Architecture
//!
//! ```text
//! AsupersyncObservabilityMonitor
//!   ├── config:  AsupersyncObservabilityConfig   (thresholds, intervals, enabled flags)
//!   ├── telemetry: AsupersyncTelemetry           (AtomicU64 counters, lock-free)
//!   ├── snapshot() → AsupersyncTelemetrySnapshot (serializable export)
//!   ├── evaluate_health() → Vec<RuntimeHealthCheck>
//!   ├── evaluate_slos()   → GateReport
//!   └── incident_context() → AsupersyncIncidentContext
//! ```
//!
//! # Design principles
//!
//! 1. **Config + Monitor pattern**: Immutable config → lock-free telemetry reads via
//!    `AtomicU64` → serializable snapshot for export.
//! 2. **4-tier health**: Maps to project-wide Green/Yellow/Red/Black via `HealthTier`.
//! 3. **SLO integration**: Feeds samples into `GateReport::evaluate` from
//!    `runtime_slo_gates` for automated pass/fail verdicts.
//! 4. **Doctor-ready**: Health checks emit `RuntimeHealthCheck` with actionable
//!    `RemediationHint` entries for `ft doctor`.

use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};

use crate::runtime_health::{
    CheckStatus, HealthCheckRegistry, RemediationEffort, RemediationHint, RuntimeHealthCheck,
};
use crate::runtime_slo_gates::{
    AlertPolicy, GateReport, GateVerdict, RuntimeAlertTier, RuntimeSlo, RuntimeSloId,
    RuntimeSloSample, standard_alert_policy, standard_runtime_slos,
};
use crate::runtime_telemetry::{FailureClass, HealthTier, RuntimePhase};

// =============================================================================
// Configuration
// =============================================================================

/// Configuration for asupersync runtime observability.
///
/// Controls thresholds, sampling intervals, and feature flags for the
/// observability monitor. All fields have sensible defaults.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AsupersyncObservabilityConfig {
    /// Whether observability is enabled.
    #[serde(default = "default_true")]
    pub enabled: bool,

    /// Sampling interval for telemetry collection (milliseconds).
    #[serde(default = "default_sample_interval_ms")]
    pub sample_interval_ms: u64,

    // ── Scope tree thresholds ────────────────────────────────────────────
    /// Maximum expected active scope count before Yellow tier.
    #[serde(default = "default_scope_yellow")]
    pub scope_count_yellow: u64,

    /// Maximum expected active scope count before Red tier.
    #[serde(default = "default_scope_red")]
    pub scope_count_red: u64,

    /// Maximum scope tree depth before warning.
    #[serde(default = "default_scope_depth_warn")]
    pub scope_depth_warn: u32,

    // ── Task queue thresholds ────────────────────────────────────────────
    /// Queue backlog depth threshold for warning.
    #[serde(default = "default_queue_warn")]
    pub queue_backlog_warn: u64,

    /// Queue backlog depth threshold for critical.
    #[serde(default = "default_queue_critical")]
    pub queue_backlog_critical: u64,

    // ── Cancellation thresholds ──────────────────────────────────────────
    /// Cancellation latency p99 threshold (ms) for warning.
    #[serde(default = "default_cancel_warn_ms")]
    pub cancel_latency_warn_ms: u64,

    /// Cancellation latency p99 threshold (ms) for critical.
    #[serde(default = "default_cancel_critical_ms")]
    pub cancel_latency_critical_ms: u64,

    // ── Channel thresholds ───────────────────────────────────────────────
    /// Maximum channel depth before backpressure warning.
    #[serde(default = "default_channel_depth_warn")]
    pub channel_depth_warn: u64,

    // ── Task leak thresholds ─────────────────────────────────────────────
    /// Maximum task leak ratio before warning.
    #[serde(default = "default_leak_ratio_warn")]
    pub task_leak_ratio_warn: f64,

    /// Maximum task leak ratio before critical.
    #[serde(default = "default_leak_ratio_critical")]
    pub task_leak_ratio_critical: f64,

    // ── Lock contention ──────────────────────────────────────────────────
    /// Lock contention ratio above which to warn (0.0-1.0).
    #[serde(default = "default_lock_contention_warn")]
    pub lock_contention_warn: f64,

    // ── Recovery ─────────────────────────────────────────────────────────
    /// Maximum recovery time (ms) before critical alert.
    #[serde(default = "default_recovery_time_critical_ms")]
    pub recovery_time_critical_ms: u64,

    // ── Gate policy ──────────────────────────────────────────────────────
    /// Whether automated gate enforcement is active.
    #[serde(default = "default_true")]
    pub gate_enforcement_enabled: bool,

    /// Minimum number of samples required before gate evaluation.
    #[serde(default = "default_min_gate_samples")]
    pub min_gate_samples: u64,
}

fn default_true() -> bool {
    true
}
fn default_sample_interval_ms() -> u64 {
    1000
}
fn default_scope_yellow() -> u64 {
    500
}
fn default_scope_red() -> u64 {
    2000
}
fn default_scope_depth_warn() -> u32 {
    32
}
fn default_queue_warn() -> u64 {
    500
}
fn default_queue_critical() -> u64 {
    1000
}
fn default_cancel_warn_ms() -> u64 {
    25
}
fn default_cancel_critical_ms() -> u64 {
    50
}
fn default_channel_depth_warn() -> u64 {
    256
}
fn default_leak_ratio_warn() -> f64 {
    0.0005
}
fn default_leak_ratio_critical() -> f64 {
    0.001
}
fn default_lock_contention_warn() -> f64 {
    0.10
}
fn default_recovery_time_critical_ms() -> u64 {
    5000
}
fn default_min_gate_samples() -> u64 {
    10
}

impl Default for AsupersyncObservabilityConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            sample_interval_ms: default_sample_interval_ms(),
            scope_count_yellow: default_scope_yellow(),
            scope_count_red: default_scope_red(),
            scope_depth_warn: default_scope_depth_warn(),
            queue_backlog_warn: default_queue_warn(),
            queue_backlog_critical: default_queue_critical(),
            cancel_latency_warn_ms: default_cancel_warn_ms(),
            cancel_latency_critical_ms: default_cancel_critical_ms(),
            channel_depth_warn: default_channel_depth_warn(),
            task_leak_ratio_warn: default_leak_ratio_warn(),
            task_leak_ratio_critical: default_leak_ratio_critical(),
            lock_contention_warn: default_lock_contention_warn(),
            recovery_time_critical_ms: default_recovery_time_critical_ms(),
            gate_enforcement_enabled: true,
            min_gate_samples: default_min_gate_samples(),
        }
    }
}

// =============================================================================
// Telemetry counters (lock-free via AtomicU64)
// =============================================================================

/// Lock-free telemetry counters for the asupersync runtime.
///
/// Uses `AtomicU64` for `&self` access without synchronization overhead.
/// Call `snapshot()` to produce a serializable copy.
#[derive(Debug)]
pub struct AsupersyncTelemetry {
    // ── Scope tree metrics ───────────────────────────────────────────────
    pub scopes_created: AtomicU64,
    pub scopes_destroyed: AtomicU64,
    pub scope_max_depth: AtomicU64,
    pub scope_max_active: AtomicU64,

    // ── Task metrics ─────────────────────────────────────────────────────
    pub tasks_spawned: AtomicU64,
    pub tasks_completed: AtomicU64,
    pub tasks_cancelled: AtomicU64,
    pub tasks_leaked: AtomicU64,
    pub tasks_panicked: AtomicU64,

    // ── Cancellation metrics ─────────────────────────────────────────────
    pub cancel_requests: AtomicU64,
    pub cancel_completions: AtomicU64,
    pub cancel_latency_sum_us: AtomicU64,
    pub cancel_latency_max_us: AtomicU64,
    pub cancel_grace_expirations: AtomicU64,

    // ── Channel metrics ──────────────────────────────────────────────────
    pub channel_sends: AtomicU64,
    pub channel_recvs: AtomicU64,
    pub channel_send_failures: AtomicU64,
    pub channel_max_depth: AtomicU64,

    // ── Lock contention metrics ──────────────────────────────────────────
    pub lock_acquisitions: AtomicU64,
    pub lock_contentions: AtomicU64,
    pub lock_timeout_failures: AtomicU64,

    // ── Permit/semaphore metrics ─────────────────────────────────────────
    pub permit_acquisitions: AtomicU64,
    pub permit_timeouts: AtomicU64,
    pub permit_max_wait_us: AtomicU64,

    // ── Recovery metrics ─────────────────────────────────────────────────
    pub recovery_attempts: AtomicU64,
    pub recovery_successes: AtomicU64,
    pub recovery_failures: AtomicU64,
    pub recovery_latency_max_ms: AtomicU64,

    // ── Health samples ───────────────────────────────────────────────────
    pub health_samples: AtomicU64,
    pub health_green_samples: AtomicU64,
    pub health_yellow_samples: AtomicU64,
    pub health_red_samples: AtomicU64,
    pub health_black_samples: AtomicU64,

    // ── SLO gate evaluations ─────────────────────────────────────────────
    pub gate_evaluations: AtomicU64,
    pub gate_passes: AtomicU64,
    pub gate_conditional_passes: AtomicU64,
    pub gate_failures: AtomicU64,
}

impl AsupersyncTelemetry {
    /// Create a new zeroed telemetry instance.
    #[must_use]
    pub fn new() -> Self {
        Self {
            scopes_created: AtomicU64::new(0),
            scopes_destroyed: AtomicU64::new(0),
            scope_max_depth: AtomicU64::new(0),
            scope_max_active: AtomicU64::new(0),

            tasks_spawned: AtomicU64::new(0),
            tasks_completed: AtomicU64::new(0),
            tasks_cancelled: AtomicU64::new(0),
            tasks_leaked: AtomicU64::new(0),
            tasks_panicked: AtomicU64::new(0),

            cancel_requests: AtomicU64::new(0),
            cancel_completions: AtomicU64::new(0),
            cancel_latency_sum_us: AtomicU64::new(0),
            cancel_latency_max_us: AtomicU64::new(0),
            cancel_grace_expirations: AtomicU64::new(0),

            channel_sends: AtomicU64::new(0),
            channel_recvs: AtomicU64::new(0),
            channel_send_failures: AtomicU64::new(0),
            channel_max_depth: AtomicU64::new(0),

            lock_acquisitions: AtomicU64::new(0),
            lock_contentions: AtomicU64::new(0),
            lock_timeout_failures: AtomicU64::new(0),

            permit_acquisitions: AtomicU64::new(0),
            permit_timeouts: AtomicU64::new(0),
            permit_max_wait_us: AtomicU64::new(0),

            recovery_attempts: AtomicU64::new(0),
            recovery_successes: AtomicU64::new(0),
            recovery_failures: AtomicU64::new(0),
            recovery_latency_max_ms: AtomicU64::new(0),

            health_samples: AtomicU64::new(0),
            health_green_samples: AtomicU64::new(0),
            health_yellow_samples: AtomicU64::new(0),
            health_red_samples: AtomicU64::new(0),
            health_black_samples: AtomicU64::new(0),

            gate_evaluations: AtomicU64::new(0),
            gate_passes: AtomicU64::new(0),
            gate_conditional_passes: AtomicU64::new(0),
            gate_failures: AtomicU64::new(0),
        }
    }

    /// Produce a serializable snapshot of all counters.
    #[must_use]
    pub fn snapshot(&self) -> AsupersyncTelemetrySnapshot {
        AsupersyncTelemetrySnapshot {
            scopes_created: self.scopes_created.load(Ordering::Relaxed),
            scopes_destroyed: self.scopes_destroyed.load(Ordering::Relaxed),
            scope_max_depth: self.scope_max_depth.load(Ordering::Relaxed),
            scope_max_active: self.scope_max_active.load(Ordering::Relaxed),

            tasks_spawned: self.tasks_spawned.load(Ordering::Relaxed),
            tasks_completed: self.tasks_completed.load(Ordering::Relaxed),
            tasks_cancelled: self.tasks_cancelled.load(Ordering::Relaxed),
            tasks_leaked: self.tasks_leaked.load(Ordering::Relaxed),
            tasks_panicked: self.tasks_panicked.load(Ordering::Relaxed),

            cancel_requests: self.cancel_requests.load(Ordering::Relaxed),
            cancel_completions: self.cancel_completions.load(Ordering::Relaxed),
            cancel_latency_sum_us: self.cancel_latency_sum_us.load(Ordering::Relaxed),
            cancel_latency_max_us: self.cancel_latency_max_us.load(Ordering::Relaxed),
            cancel_grace_expirations: self.cancel_grace_expirations.load(Ordering::Relaxed),

            channel_sends: self.channel_sends.load(Ordering::Relaxed),
            channel_recvs: self.channel_recvs.load(Ordering::Relaxed),
            channel_send_failures: self.channel_send_failures.load(Ordering::Relaxed),
            channel_max_depth: self.channel_max_depth.load(Ordering::Relaxed),

            lock_acquisitions: self.lock_acquisitions.load(Ordering::Relaxed),
            lock_contentions: self.lock_contentions.load(Ordering::Relaxed),
            lock_timeout_failures: self.lock_timeout_failures.load(Ordering::Relaxed),

            permit_acquisitions: self.permit_acquisitions.load(Ordering::Relaxed),
            permit_timeouts: self.permit_timeouts.load(Ordering::Relaxed),
            permit_max_wait_us: self.permit_max_wait_us.load(Ordering::Relaxed),

            recovery_attempts: self.recovery_attempts.load(Ordering::Relaxed),
            recovery_successes: self.recovery_successes.load(Ordering::Relaxed),
            recovery_failures: self.recovery_failures.load(Ordering::Relaxed),
            recovery_latency_max_ms: self.recovery_latency_max_ms.load(Ordering::Relaxed),

            health_samples: self.health_samples.load(Ordering::Relaxed),
            health_green_samples: self.health_green_samples.load(Ordering::Relaxed),
            health_yellow_samples: self.health_yellow_samples.load(Ordering::Relaxed),
            health_red_samples: self.health_red_samples.load(Ordering::Relaxed),
            health_black_samples: self.health_black_samples.load(Ordering::Relaxed),

            gate_evaluations: self.gate_evaluations.load(Ordering::Relaxed),
            gate_passes: self.gate_passes.load(Ordering::Relaxed),
            gate_conditional_passes: self.gate_conditional_passes.load(Ordering::Relaxed),
            gate_failures: self.gate_failures.load(Ordering::Relaxed),
        }
    }

    /// Record a health tier sample.
    pub fn record_health_sample(&self, tier: HealthTier) {
        self.health_samples.fetch_add(1, Ordering::Relaxed);
        match tier {
            HealthTier::Green => self.health_green_samples.fetch_add(1, Ordering::Relaxed),
            HealthTier::Yellow => self.health_yellow_samples.fetch_add(1, Ordering::Relaxed),
            HealthTier::Red => self.health_red_samples.fetch_add(1, Ordering::Relaxed),
            HealthTier::Black => self.health_black_samples.fetch_add(1, Ordering::Relaxed),
        };
    }

    /// Record a gate evaluation result.
    pub fn record_gate_verdict(&self, verdict: GateVerdict) {
        self.gate_evaluations.fetch_add(1, Ordering::Relaxed);
        match verdict {
            GateVerdict::Pass => self.gate_passes.fetch_add(1, Ordering::Relaxed),
            GateVerdict::ConditionalPass => {
                self.gate_conditional_passes.fetch_add(1, Ordering::Relaxed)
            }
            GateVerdict::Fail => self.gate_failures.fetch_add(1, Ordering::Relaxed),
        };
    }

    /// Update a max-value counter (e.g., max depth, max latency).
    fn update_max(counter: &AtomicU64, new_value: u64) {
        let mut current = counter.load(Ordering::Relaxed);
        while new_value > current {
            match counter.compare_exchange_weak(
                current,
                new_value,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(actual) => current = actual,
            }
        }
    }

    /// Record a cancellation latency sample (microseconds).
    pub fn record_cancel_latency_us(&self, latency_us: u64) {
        self.cancel_latency_sum_us
            .fetch_add(latency_us, Ordering::Relaxed);
        Self::update_max(&self.cancel_latency_max_us, latency_us);
    }

    /// Record scope tree depth.
    pub fn record_scope_depth(&self, depth: u64) {
        Self::update_max(&self.scope_max_depth, depth);
    }

    /// Record active scope count.
    pub fn record_active_scopes(&self, count: u64) {
        Self::update_max(&self.scope_max_active, count);
    }

    /// Record channel depth.
    pub fn record_channel_depth(&self, depth: u64) {
        Self::update_max(&self.channel_max_depth, depth);
    }

    /// Record permit wait time (microseconds).
    pub fn record_permit_wait_us(&self, wait_us: u64) {
        Self::update_max(&self.permit_max_wait_us, wait_us);
    }

    /// Record recovery latency (milliseconds).
    pub fn record_recovery_latency_ms(&self, latency_ms: u64) {
        Self::update_max(&self.recovery_latency_max_ms, latency_ms);
    }
}

impl Default for AsupersyncTelemetry {
    fn default() -> Self {
        Self::new()
    }
}

// =============================================================================
// Telemetry snapshot (serializable)
// =============================================================================

/// Serializable snapshot of asupersync runtime telemetry.
///
/// Produced by `AsupersyncTelemetry::snapshot()` for export, storage, or
/// dashboard rendering.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AsupersyncTelemetrySnapshot {
    // ── Scope tree ───────────────────────────────────────────────────────
    pub scopes_created: u64,
    pub scopes_destroyed: u64,
    pub scope_max_depth: u64,
    pub scope_max_active: u64,

    // ── Tasks ────────────────────────────────────────────────────────────
    pub tasks_spawned: u64,
    pub tasks_completed: u64,
    pub tasks_cancelled: u64,
    pub tasks_leaked: u64,
    pub tasks_panicked: u64,

    // ── Cancellation ─────────────────────────────────────────────────────
    pub cancel_requests: u64,
    pub cancel_completions: u64,
    pub cancel_latency_sum_us: u64,
    pub cancel_latency_max_us: u64,
    pub cancel_grace_expirations: u64,

    // ── Channels ─────────────────────────────────────────────────────────
    pub channel_sends: u64,
    pub channel_recvs: u64,
    pub channel_send_failures: u64,
    pub channel_max_depth: u64,

    // ── Lock contention ──────────────────────────────────────────────────
    pub lock_acquisitions: u64,
    pub lock_contentions: u64,
    pub lock_timeout_failures: u64,

    // ── Permits/semaphores ───────────────────────────────────────────────
    pub permit_acquisitions: u64,
    pub permit_timeouts: u64,
    pub permit_max_wait_us: u64,

    // ── Recovery ─────────────────────────────────────────────────────────
    pub recovery_attempts: u64,
    pub recovery_successes: u64,
    pub recovery_failures: u64,
    pub recovery_latency_max_ms: u64,

    // ── Health ────────────────────────────────────────────────────────────
    pub health_samples: u64,
    pub health_green_samples: u64,
    pub health_yellow_samples: u64,
    pub health_red_samples: u64,
    pub health_black_samples: u64,

    // ── Gate ──────────────────────────────────────────────────────────────
    pub gate_evaluations: u64,
    pub gate_passes: u64,
    pub gate_conditional_passes: u64,
    pub gate_failures: u64,
}

impl AsupersyncTelemetrySnapshot {
    /// Current active scope count (created - destroyed).
    #[must_use]
    pub fn active_scopes(&self) -> u64 {
        self.scopes_created.saturating_sub(self.scopes_destroyed)
    }

    /// Task leak ratio (leaked / spawned). Returns 0.0 if no tasks spawned.
    #[must_use]
    pub fn task_leak_ratio(&self) -> f64 {
        if self.tasks_spawned == 0 {
            return 0.0;
        }
        self.tasks_leaked as f64 / self.tasks_spawned as f64
    }

    /// Average cancellation latency in microseconds. Returns 0 if no completions.
    #[must_use]
    pub fn cancel_latency_avg_us(&self) -> u64 {
        if self.cancel_completions == 0 {
            return 0;
        }
        self.cancel_latency_sum_us / self.cancel_completions
    }

    /// Lock contention ratio (contentions / acquisitions). Returns 0.0 if no acquisitions.
    #[must_use]
    pub fn lock_contention_ratio(&self) -> f64 {
        if self.lock_acquisitions == 0 {
            return 0.0;
        }
        self.lock_contentions as f64 / self.lock_acquisitions as f64
    }

    /// Channel send failure ratio. Returns 0.0 if no sends.
    #[must_use]
    pub fn channel_failure_ratio(&self) -> f64 {
        if self.channel_sends == 0 {
            return 0.0;
        }
        self.channel_send_failures as f64 / self.channel_sends as f64
    }

    /// Recovery success ratio. Returns 1.0 if no attempts.
    #[must_use]
    pub fn recovery_success_ratio(&self) -> f64 {
        if self.recovery_attempts == 0 {
            return 1.0;
        }
        self.recovery_successes as f64 / self.recovery_attempts as f64
    }

    /// Health tier distribution as fractions. Returns `[green, yellow, red, black]`.
    #[must_use]
    pub fn health_distribution(&self) -> [f64; 4] {
        if self.health_samples == 0 {
            return [1.0, 0.0, 0.0, 0.0];
        }
        let total = self.health_samples as f64;
        [
            self.health_green_samples as f64 / total,
            self.health_yellow_samples as f64 / total,
            self.health_red_samples as f64 / total,
            self.health_black_samples as f64 / total,
        ]
    }

    /// Gate pass ratio. Returns 1.0 if no evaluations.
    #[must_use]
    pub fn gate_pass_ratio(&self) -> f64 {
        if self.gate_evaluations == 0 {
            return 1.0;
        }
        self.gate_passes as f64 / self.gate_evaluations as f64
    }

    /// Pending task count (spawned - completed - cancelled - leaked - panicked).
    #[must_use]
    pub fn tasks_pending(&self) -> u64 {
        self.tasks_spawned
            .saturating_sub(self.tasks_completed)
            .saturating_sub(self.tasks_cancelled)
            .saturating_sub(self.tasks_leaked)
            .saturating_sub(self.tasks_panicked)
    }

    /// Derive overall health tier from the snapshot.
    #[must_use]
    pub fn overall_health_tier(&self, config: &AsupersyncObservabilityConfig) -> HealthTier {
        let mut worst = HealthTier::Green;

        // Scope pressure
        let active = self.active_scopes();
        if active >= config.scope_count_red {
            worst = worst.max(HealthTier::Red);
        } else if active >= config.scope_count_yellow {
            worst = worst.max(HealthTier::Yellow);
        }

        // Task leak
        let leak = self.task_leak_ratio();
        if leak >= config.task_leak_ratio_critical {
            worst = worst.max(HealthTier::Red);
        } else if leak >= config.task_leak_ratio_warn {
            worst = worst.max(HealthTier::Yellow);
        }

        // Cancellation latency (max, in us → convert threshold from ms)
        let cancel_max_ms = self.cancel_latency_max_us / 1000;
        if cancel_max_ms >= config.cancel_latency_critical_ms {
            worst = worst.max(HealthTier::Red);
        } else if cancel_max_ms >= config.cancel_latency_warn_ms {
            worst = worst.max(HealthTier::Yellow);
        }

        // Lock contention
        let contention = self.lock_contention_ratio();
        if contention >= config.lock_contention_warn * 2.0 {
            worst = worst.max(HealthTier::Red);
        } else if contention >= config.lock_contention_warn {
            worst = worst.max(HealthTier::Yellow);
        }

        // Recovery failures → Black if any exist
        if self.recovery_failures > 0 && self.recovery_success_ratio() < 0.5 {
            worst = worst.max(HealthTier::Black);
        } else if self.recovery_failures > 0 {
            worst = worst.max(HealthTier::Red);
        }

        // Task panics → at least Red
        if self.tasks_panicked > 0 {
            worst = worst.max(HealthTier::Red);
        }

        worst
    }
}

// =============================================================================
// Incident enrichment context
// =============================================================================

/// Asupersync runtime context for incident bundle enrichment.
///
/// Provides structured runtime state for `ft doctor --export-bundle` and
/// diagnostic pipelines.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AsupersyncIncidentContext {
    /// Current runtime phase.
    pub phase: RuntimePhase,
    /// Overall health tier at incident time.
    pub health_tier: HealthTier,
    /// Telemetry snapshot at incident time.
    pub telemetry: AsupersyncTelemetrySnapshot,
    /// Most recent gate verdict (if any).
    pub last_gate_verdict: Option<GateVerdict>,
    /// Active SLO breaches at incident time.
    pub active_breaches: Vec<SloBreachSummary>,
    /// Runtime uptime at incident time (milliseconds).
    pub uptime_ms: u64,
}

/// Summary of an active SLO breach for incident context.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SloBreachSummary {
    /// SLO identifier.
    pub slo_id: String,
    /// Current measured value.
    pub measured: f64,
    /// Target value.
    pub target: f64,
    /// How long the breach has been active (milliseconds).
    pub breach_duration_ms: u64,
    /// Alert tier.
    pub alert_tier: RuntimeAlertTier,
    /// Whether this is a critical (gate-blocking) SLO.
    pub critical: bool,
}

// =============================================================================
// Health check functions
// =============================================================================

/// Generate asupersync-specific health checks from a telemetry snapshot.
///
/// Returns a vector of `RuntimeHealthCheck` entries suitable for registration
/// with `HealthCheckRegistry` and display via `ft doctor`.
#[must_use]
pub fn evaluate_asupersync_health(
    snapshot: &AsupersyncTelemetrySnapshot,
    config: &AsupersyncObservabilityConfig,
) -> Vec<RuntimeHealthCheck> {
    let checks = vec![
        // ── Scope tree health ────────────────────────────────────────────────
        check_scope_tree(snapshot, config),
        // ── Task lifecycle health ────────────────────────────────────────────
        check_task_lifecycle(snapshot, config),
        // ── Cancellation health ──────────────────────────────────────────────
        check_cancellation(snapshot, config),
        // ── Channel health ───────────────────────────────────────────────────
        check_channel_health(snapshot, config),
        // ── Lock contention health ───────────────────────────────────────────
        check_lock_contention(snapshot, config),
        // ── Recovery health ──────────────────────────────────────────────────
        check_recovery_health(snapshot),
    ];

    checks
}

fn check_scope_tree(
    snapshot: &AsupersyncTelemetrySnapshot,
    config: &AsupersyncObservabilityConfig,
) -> RuntimeHealthCheck {
    let active = snapshot.active_scopes();

    if active >= config.scope_count_red {
        RuntimeHealthCheck {
            check_id: "asupersync.scope_tree".into(),
            display_name: "Asupersync Scope Tree".into(),
            status: CheckStatus::Fail,
            tier: HealthTier::Red,
            summary: format!(
                "Active scope count ({active}) exceeds red threshold ({})",
                config.scope_count_red
            ),
            evidence: vec![
                format!("scopes_created: {}", snapshot.scopes_created),
                format!("scopes_destroyed: {}", snapshot.scopes_destroyed),
                format!("max_depth: {}", snapshot.scope_max_depth),
            ],
            remediation: vec![
                RemediationHint::with_command(
                    "Check for scope leaks in long-running tasks",
                    "ft doctor --check runtime",
                )
                .effort(RemediationEffort::Medium),
                RemediationHint::text("Review task cancellation paths for unclosed scopes")
                    .effort(RemediationEffort::High),
            ],
            failure_class: Some(FailureClass::Overload),
            duration_us: 0,
        }
    } else if active >= config.scope_count_yellow
        || snapshot.scope_max_depth >= u64::from(config.scope_depth_warn)
    {
        RuntimeHealthCheck {
            check_id: "asupersync.scope_tree".into(),
            display_name: "Asupersync Scope Tree".into(),
            status: CheckStatus::Warn,
            tier: HealthTier::Yellow,
            summary: format!(
                "Scope tree showing elevated usage: {active} active, depth {}",
                snapshot.scope_max_depth
            ),
            evidence: vec![
                format!("active_scopes: {active}"),
                format!("max_depth: {}", snapshot.scope_max_depth),
            ],
            remediation: vec![RemediationHint::text(
                "Monitor scope growth trend; may indicate slow scope cleanup",
            )],
            failure_class: None,
            duration_us: 0,
        }
    } else {
        RuntimeHealthCheck::pass(
            "asupersync.scope_tree",
            "Asupersync Scope Tree",
            &format!(
                "Scope tree healthy: {active} active, depth {}",
                snapshot.scope_max_depth
            ),
        )
    }
}

fn check_task_lifecycle(
    snapshot: &AsupersyncTelemetrySnapshot,
    config: &AsupersyncObservabilityConfig,
) -> RuntimeHealthCheck {
    let leak_ratio = snapshot.task_leak_ratio();

    if leak_ratio >= config.task_leak_ratio_critical {
        RuntimeHealthCheck {
            check_id: "asupersync.task_lifecycle".into(),
            display_name: "Asupersync Task Lifecycle".into(),
            status: CheckStatus::Fail,
            tier: HealthTier::Red,
            summary: format!(
                "Task leak ratio {leak_ratio:.6} exceeds critical threshold ({})",
                config.task_leak_ratio_critical
            ),
            evidence: vec![
                format!("spawned: {}", snapshot.tasks_spawned),
                format!("completed: {}", snapshot.tasks_completed),
                format!("leaked: {}", snapshot.tasks_leaked),
                format!("panicked: {}", snapshot.tasks_panicked),
            ],
            remediation: vec![
                RemediationHint::with_command(
                    "Inspect leaked tasks for missing cancellation handlers",
                    "ft doctor --check runtime",
                )
                .effort(RemediationEffort::High),
            ],
            failure_class: Some(FailureClass::Deadlock),
            duration_us: 0,
        }
    } else if leak_ratio >= config.task_leak_ratio_warn || snapshot.tasks_panicked > 0 {
        let mut evidence = vec![
            format!("leak_ratio: {leak_ratio:.6}"),
            format!("pending: {}", snapshot.tasks_pending()),
        ];
        if snapshot.tasks_panicked > 0 {
            evidence.push(format!("panicked: {}", snapshot.tasks_panicked));
        }

        RuntimeHealthCheck {
            check_id: "asupersync.task_lifecycle".into(),
            display_name: "Asupersync Task Lifecycle".into(),
            status: CheckStatus::Warn,
            tier: HealthTier::Yellow,
            summary: "Task lifecycle showing early warning signs".into(),
            evidence,
            remediation: vec![RemediationHint::text(
                "Review task completion paths and panic handlers",
            )],
            failure_class: if snapshot.tasks_panicked > 0 {
                Some(FailureClass::Panic)
            } else {
                None
            },
            duration_us: 0,
        }
    } else {
        RuntimeHealthCheck::pass(
            "asupersync.task_lifecycle",
            "Asupersync Task Lifecycle",
            &format!(
                "Task lifecycle healthy: {} spawned, {} pending, {:.6} leak ratio",
                snapshot.tasks_spawned,
                snapshot.tasks_pending(),
                leak_ratio,
            ),
        )
    }
}

fn check_cancellation(
    snapshot: &AsupersyncTelemetrySnapshot,
    config: &AsupersyncObservabilityConfig,
) -> RuntimeHealthCheck {
    if snapshot.cancel_requests == 0 {
        return RuntimeHealthCheck::pass(
            "asupersync.cancellation",
            "Asupersync Cancellation",
            "No cancellation requests recorded",
        );
    }

    let max_ms = snapshot.cancel_latency_max_us / 1000;
    let avg_us = snapshot.cancel_latency_avg_us();
    let pending = snapshot
        .cancel_requests
        .saturating_sub(snapshot.cancel_completions);

    if max_ms >= config.cancel_latency_critical_ms || pending > 0 {
        RuntimeHealthCheck {
            check_id: "asupersync.cancellation".into(),
            display_name: "Asupersync Cancellation".into(),
            status: CheckStatus::Fail,
            tier: HealthTier::Red,
            summary: format!(
                "Cancellation latency {max_ms}ms exceeds critical threshold ({}ms), {pending} pending",
                config.cancel_latency_critical_ms
            ),
            evidence: vec![
                format!("max_latency_ms: {max_ms}"),
                format!("avg_latency_us: {avg_us}"),
                format!("requests: {}", snapshot.cancel_requests),
                format!("completions: {}", snapshot.cancel_completions),
                format!("grace_expirations: {}", snapshot.cancel_grace_expirations),
            ],
            remediation: vec![
                RemediationHint::with_command(
                    "Check for tasks ignoring cancellation tokens",
                    "ft doctor --check runtime",
                )
                .effort(RemediationEffort::High),
                RemediationHint::text(
                    "Review long-running computations for cooperative cancellation points",
                )
                .effort(RemediationEffort::High),
            ],
            failure_class: Some(FailureClass::Timeout),
            duration_us: 0,
        }
    } else if max_ms >= config.cancel_latency_warn_ms || snapshot.cancel_grace_expirations > 0 {
        RuntimeHealthCheck {
            check_id: "asupersync.cancellation".into(),
            display_name: "Asupersync Cancellation".into(),
            status: CheckStatus::Warn,
            tier: HealthTier::Yellow,
            summary: format!(
                "Cancellation latency {max_ms}ms approaching threshold, {} grace expirations",
                snapshot.cancel_grace_expirations
            ),
            evidence: vec![
                format!("max_latency_ms: {max_ms}"),
                format!("avg_latency_us: {avg_us}"),
            ],
            remediation: vec![RemediationHint::text(
                "Monitor cancellation paths; grace expirations indicate tasks not responding promptly",
            )],
            failure_class: None,
            duration_us: 0,
        }
    } else {
        RuntimeHealthCheck::pass(
            "asupersync.cancellation",
            "Asupersync Cancellation",
            &format!(
                "Cancellation healthy: max {max_ms}ms, avg {}us, {} requests",
                avg_us, snapshot.cancel_requests,
            ),
        )
    }
}

fn check_channel_health(
    snapshot: &AsupersyncTelemetrySnapshot,
    config: &AsupersyncObservabilityConfig,
) -> RuntimeHealthCheck {
    let failure_ratio = snapshot.channel_failure_ratio();
    let max_depth = snapshot.channel_max_depth;

    if failure_ratio > 0.01 || max_depth >= config.channel_depth_warn * 2 {
        RuntimeHealthCheck {
            check_id: "asupersync.channels".into(),
            display_name: "Asupersync Channels".into(),
            status: CheckStatus::Fail,
            tier: HealthTier::Red,
            summary: format!(
                "Channel health degraded: {:.4} failure ratio, max depth {max_depth}",
                failure_ratio,
            ),
            evidence: vec![
                format!("sends: {}", snapshot.channel_sends),
                format!("failures: {}", snapshot.channel_send_failures),
                format!("max_depth: {max_depth}"),
            ],
            remediation: vec![
                RemediationHint::text("Check for disconnected receivers causing send failures")
                    .effort(RemediationEffort::Medium),
                RemediationHint::text("Increase channel capacity or add backpressure")
                    .effort(RemediationEffort::Medium),
            ],
            failure_class: Some(FailureClass::Overload),
            duration_us: 0,
        }
    } else if failure_ratio > 0.001 || max_depth >= config.channel_depth_warn {
        RuntimeHealthCheck {
            check_id: "asupersync.channels".into(),
            display_name: "Asupersync Channels".into(),
            status: CheckStatus::Warn,
            tier: HealthTier::Yellow,
            summary: format!(
                "Channel depth {max_depth} approaching warn threshold ({})",
                config.channel_depth_warn
            ),
            evidence: vec![
                format!("failure_ratio: {failure_ratio:.4}"),
                format!("max_depth: {max_depth}"),
            ],
            remediation: vec![RemediationHint::text(
                "Monitor channel growth; consider increasing capacity",
            )],
            failure_class: None,
            duration_us: 0,
        }
    } else {
        RuntimeHealthCheck::pass(
            "asupersync.channels",
            "Asupersync Channels",
            &format!(
                "Channels healthy: {} sends, {:.4} failure ratio, max depth {max_depth}",
                snapshot.channel_sends, failure_ratio,
            ),
        )
    }
}

fn check_lock_contention(
    snapshot: &AsupersyncTelemetrySnapshot,
    config: &AsupersyncObservabilityConfig,
) -> RuntimeHealthCheck {
    let contention = snapshot.lock_contention_ratio();

    if contention >= config.lock_contention_warn * 2.0 || snapshot.lock_timeout_failures > 0 {
        RuntimeHealthCheck {
            check_id: "asupersync.lock_contention".into(),
            display_name: "Asupersync Lock Contention".into(),
            status: CheckStatus::Fail,
            tier: HealthTier::Red,
            summary: format!(
                "Lock contention {contention:.4} exceeds threshold, {} timeout failures",
                snapshot.lock_timeout_failures,
            ),
            evidence: vec![
                format!("acquisitions: {}", snapshot.lock_acquisitions),
                format!("contentions: {}", snapshot.lock_contentions),
                format!("timeout_failures: {}", snapshot.lock_timeout_failures),
            ],
            remediation: vec![
                RemediationHint::text(
                    "Reduce critical section duration or use finer-grained locks",
                )
                .effort(RemediationEffort::High),
                RemediationHint::text("Consider lock-free alternatives (AtomicU64, channels)")
                    .effort(RemediationEffort::High),
            ],
            failure_class: Some(FailureClass::Deadlock),
            duration_us: 0,
        }
    } else if contention >= config.lock_contention_warn {
        RuntimeHealthCheck {
            check_id: "asupersync.lock_contention".into(),
            display_name: "Asupersync Lock Contention".into(),
            status: CheckStatus::Warn,
            tier: HealthTier::Yellow,
            summary: format!("Lock contention ratio {contention:.4} above warning threshold"),
            evidence: vec![format!("contention_ratio: {contention:.4}")],
            remediation: vec![RemediationHint::text(
                "Monitor contention trend; consider reducing lock hold times",
            )],
            failure_class: None,
            duration_us: 0,
        }
    } else {
        RuntimeHealthCheck::pass(
            "asupersync.lock_contention",
            "Asupersync Lock Contention",
            &format!(
                "Lock contention healthy: {contention:.4} ratio, {} acquisitions",
                snapshot.lock_acquisitions,
            ),
        )
    }
}

fn check_recovery_health(snapshot: &AsupersyncTelemetrySnapshot) -> RuntimeHealthCheck {
    if snapshot.recovery_attempts == 0 {
        return RuntimeHealthCheck::pass(
            "asupersync.recovery",
            "Asupersync Recovery",
            "No recovery attempts recorded",
        );
    }

    let success_ratio = snapshot.recovery_success_ratio();

    if success_ratio < 0.5 {
        RuntimeHealthCheck {
            check_id: "asupersync.recovery".into(),
            display_name: "Asupersync Recovery".into(),
            status: CheckStatus::Fail,
            tier: HealthTier::Black,
            summary: format!(
                "Recovery success ratio {success_ratio:.2} below 50%: {} failures of {} attempts",
                snapshot.recovery_failures, snapshot.recovery_attempts,
            ),
            evidence: vec![
                format!("attempts: {}", snapshot.recovery_attempts),
                format!("successes: {}", snapshot.recovery_successes),
                format!("failures: {}", snapshot.recovery_failures),
                format!("max_latency_ms: {}", snapshot.recovery_latency_max_ms),
            ],
            remediation: vec![
                RemediationHint::with_command(
                    "Export diagnostic bundle for analysis",
                    "ft doctor --export-bundle",
                )
                .effort(RemediationEffort::Medium),
                RemediationHint::text("Check recovery handlers for persistent failure conditions")
                    .effort(RemediationEffort::High),
            ],
            failure_class: Some(FailureClass::Degraded),
            duration_us: 0,
        }
    } else if snapshot.recovery_failures > 0 {
        RuntimeHealthCheck {
            check_id: "asupersync.recovery".into(),
            display_name: "Asupersync Recovery".into(),
            status: CheckStatus::Warn,
            tier: HealthTier::Yellow,
            summary: format!(
                "Recovery has {} failures ({success_ratio:.2} success ratio)",
                snapshot.recovery_failures,
            ),
            evidence: vec![
                format!("success_ratio: {success_ratio:.2}"),
                format!("max_latency_ms: {}", snapshot.recovery_latency_max_ms),
            ],
            remediation: vec![RemediationHint::text(
                "Review recovery failure logs for patterns",
            )],
            failure_class: None,
            duration_us: 0,
        }
    } else {
        RuntimeHealthCheck::pass(
            "asupersync.recovery",
            "Asupersync Recovery",
            &format!(
                "Recovery healthy: {}/{} succeeded, max {}ms",
                snapshot.recovery_successes,
                snapshot.recovery_attempts,
                snapshot.recovery_latency_max_ms,
            ),
        )
    }
}

// =============================================================================
// SLO sample generation
// =============================================================================

/// Generate `RuntimeSloSample` entries from an asupersync telemetry snapshot.
///
/// Maps snapshot metrics to the standard `RuntimeSloId` set for gate evaluation.
#[must_use]
pub fn generate_slo_samples(snapshot: &AsupersyncTelemetrySnapshot) -> Vec<RuntimeSloSample> {
    let mut samples = Vec::new();

    // Cancellation latency (max, in ms)
    if snapshot.cancel_requests > 0 {
        let max_ms = snapshot.cancel_latency_max_us as f64 / 1000.0;
        samples.push(RuntimeSloSample {
            slo_id: RuntimeSloId::CancellationLatency,
            measured: max_ms,
            good_count: snapshot.cancel_completions,
            total_count: snapshot.cancel_requests,
        });
    }

    // Queue backlog depth (pending tasks as proxy)
    samples.push(RuntimeSloSample {
        slo_id: RuntimeSloId::QueueBacklogDepth,
        measured: snapshot.tasks_pending() as f64,
        good_count: u64::from(snapshot.tasks_pending() < 1000),
        total_count: 1,
    });

    // Task leak rate
    if snapshot.tasks_spawned > 0 {
        let leak_ratio = snapshot.task_leak_ratio();
        let good = if leak_ratio <= 0.001 {
            snapshot.tasks_spawned
        } else {
            snapshot.tasks_spawned.saturating_sub(snapshot.tasks_leaked)
        };
        samples.push(RuntimeSloSample {
            slo_id: RuntimeSloId::TaskLeakRate,
            measured: leak_ratio,
            good_count: good,
            total_count: snapshot.tasks_spawned,
        });
    }

    // Service recovery time (max, in ms)
    if snapshot.recovery_attempts > 0 {
        samples.push(RuntimeSloSample {
            slo_id: RuntimeSloId::ServiceRecoveryTime,
            measured: snapshot.recovery_latency_max_ms as f64,
            good_count: snapshot.recovery_successes,
            total_count: snapshot.recovery_attempts,
        });
    }

    // Event delivery loss (channel failure ratio as proxy)
    if snapshot.channel_sends > 0 {
        samples.push(RuntimeSloSample {
            slo_id: RuntimeSloId::EventDeliveryLoss,
            measured: snapshot.channel_failure_ratio(),
            good_count: snapshot.channel_sends - snapshot.channel_send_failures,
            total_count: snapshot.channel_sends,
        });
    }

    samples
}

// =============================================================================
// Observability monitor (orchestrator)
// =============================================================================

/// Unified asupersync observability monitor.
///
/// Orchestrates telemetry collection, health evaluation, SLO gate assessment,
/// and incident enrichment for the asupersync runtime.
pub struct AsupersyncObservabilityMonitor {
    config: AsupersyncObservabilityConfig,
    telemetry: AsupersyncTelemetry,
    slos: Vec<RuntimeSlo>,
    alert_policy: AlertPolicy,
    current_phase: RuntimePhase,
    uptime_start_ms: u64,
}

impl AsupersyncObservabilityMonitor {
    /// Create a new monitor with the given configuration.
    #[must_use]
    pub fn new(config: AsupersyncObservabilityConfig) -> Self {
        Self {
            config,
            telemetry: AsupersyncTelemetry::new(),
            slos: standard_runtime_slos(),
            alert_policy: standard_alert_policy(),
            current_phase: RuntimePhase::Init,
            uptime_start_ms: 0,
        }
    }

    /// Create a monitor with default configuration.
    #[must_use]
    pub fn with_defaults() -> Self {
        Self::new(AsupersyncObservabilityConfig::default())
    }

    /// Set the current runtime phase.
    pub fn set_phase(&mut self, phase: RuntimePhase) {
        self.current_phase = phase;
    }

    /// Set the uptime start timestamp (epoch ms).
    pub fn set_uptime_start(&mut self, start_ms: u64) {
        self.uptime_start_ms = start_ms;
    }

    /// Access the telemetry counters for recording metrics.
    #[must_use]
    pub fn telemetry(&self) -> &AsupersyncTelemetry {
        &self.telemetry
    }

    /// Access the configuration.
    #[must_use]
    pub fn config(&self) -> &AsupersyncObservabilityConfig {
        &self.config
    }

    /// Produce a telemetry snapshot.
    #[must_use]
    pub fn snapshot(&self) -> AsupersyncTelemetrySnapshot {
        self.telemetry.snapshot()
    }

    /// Evaluate health and record the resulting tier.
    #[must_use]
    pub fn evaluate_health(&self) -> Vec<RuntimeHealthCheck> {
        let snapshot = self.snapshot();
        let checks = evaluate_asupersync_health(&snapshot, &self.config);
        let tier = snapshot.overall_health_tier(&self.config);
        self.telemetry.record_health_sample(tier);
        checks
    }

    /// Register asupersync health checks with a `HealthCheckRegistry`.
    pub fn register_health_checks(&self, registry: &mut HealthCheckRegistry) {
        let checks = self.evaluate_health();
        for check in checks {
            registry.register(check);
        }
    }

    /// Evaluate SLO gates and return a gate report.
    #[must_use]
    pub fn evaluate_slos(&self) -> GateReport {
        let snapshot = self.snapshot();
        let samples = generate_slo_samples(&snapshot);
        let report = GateReport::evaluate(&self.slos, &samples, &self.alert_policy);
        self.telemetry.record_gate_verdict(report.verdict);
        report
    }

    /// Check whether the runtime passes all critical SLO gates.
    #[must_use]
    pub fn passes_gate(&self) -> bool {
        let snapshot = self.snapshot();

        // Require minimum samples before enforcing gates
        if self.config.gate_enforcement_enabled
            && snapshot.tasks_spawned >= self.config.min_gate_samples
        {
            let report = self.evaluate_slos();
            report.verdict != GateVerdict::Fail
        } else {
            true
        }
    }

    /// Generate incident enrichment context.
    #[must_use]
    pub fn incident_context(&self, now_ms: u64) -> AsupersyncIncidentContext {
        let snapshot = self.snapshot();
        let report = self.evaluate_slos();

        let active_breaches: Vec<SloBreachSummary> = report
            .results
            .iter()
            .filter(|r| !r.satisfied)
            .map(|r| SloBreachSummary {
                slo_id: r.slo_id.as_str().to_string(),
                measured: r.measured,
                target: r.target,
                breach_duration_ms: 0, // Would need historical tracking
                alert_tier: r.alert_tier.unwrap_or(RuntimeAlertTier::Info),
                critical: r.critical,
            })
            .collect();

        AsupersyncIncidentContext {
            phase: self.current_phase,
            health_tier: snapshot.overall_health_tier(&self.config),
            telemetry: snapshot,
            last_gate_verdict: Some(report.verdict),
            active_breaches,
            uptime_ms: now_ms.saturating_sub(self.uptime_start_ms),
        }
    }

    /// Render a human-readable observability summary.
    #[must_use]
    pub fn render_summary(&self) -> String {
        let snapshot = self.snapshot();
        let tier = snapshot.overall_health_tier(&self.config);
        let report = self.evaluate_slos();

        let mut lines = vec![
            "=== Asupersync Runtime Observability ===".to_string(),
            format!("Phase: {}", self.current_phase),
            format!("Health: {tier}"),
            String::new(),
            "── Scope Tree ──".to_string(),
            format!("  Active scopes: {}", snapshot.active_scopes()),
            format!("  Max depth: {}", snapshot.scope_max_depth),
            String::new(),
            "── Tasks ──".to_string(),
            format!("  Spawned: {}", snapshot.tasks_spawned),
            format!("  Pending: {}", snapshot.tasks_pending()),
            format!("  Leak ratio: {:.6}", snapshot.task_leak_ratio()),
            format!("  Panicked: {}", snapshot.tasks_panicked),
            String::new(),
            "── Cancellation ──".to_string(),
            format!("  Requests: {}", snapshot.cancel_requests),
            format!("  Max latency: {}us", snapshot.cancel_latency_max_us),
            format!("  Grace expirations: {}", snapshot.cancel_grace_expirations),
            String::new(),
            "── Channels ──".to_string(),
            format!("  Sends: {}", snapshot.channel_sends),
            format!("  Failure ratio: {:.6}", snapshot.channel_failure_ratio()),
            format!("  Max depth: {}", snapshot.channel_max_depth),
            String::new(),
            "── Lock Contention ──".to_string(),
            format!(
                "  Contention ratio: {:.4}",
                snapshot.lock_contention_ratio()
            ),
            format!("  Timeout failures: {}", snapshot.lock_timeout_failures),
            String::new(),
            "── Gate ──".to_string(),
            format!("  Verdict: {:?}", report.verdict),
            format!(
                "  SLOs: {}/{} satisfied",
                report.satisfied_count, report.total_slos
            ),
        ];

        if report.critical_breached > 0 {
            lines.push(format!("  CRITICAL breaches: {}", report.critical_breached));
        }

        lines.join("\n")
    }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_has_sensible_values() {
        let cfg = AsupersyncObservabilityConfig::default();
        assert!(cfg.enabled);
        assert_eq!(cfg.sample_interval_ms, 1000);
        assert_eq!(cfg.scope_count_yellow, 500);
        assert_eq!(cfg.scope_count_red, 2000);
        assert_eq!(cfg.queue_backlog_critical, 1000);
        assert!(cfg.gate_enforcement_enabled);
    }

    #[test]
    fn config_serde_roundtrip() {
        let cfg = AsupersyncObservabilityConfig::default();
        let json = serde_json::to_string(&cfg).unwrap();
        let deserialized: AsupersyncObservabilityConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.scope_count_yellow, cfg.scope_count_yellow);
        assert_eq!(deserialized.sample_interval_ms, cfg.sample_interval_ms);
    }

    #[test]
    fn telemetry_snapshot_zeroed() {
        let telem = AsupersyncTelemetry::new();
        let snap = telem.snapshot();
        assert_eq!(snap.tasks_spawned, 0);
        assert_eq!(snap.active_scopes(), 0);
        assert_eq!(snap.task_leak_ratio(), 0.0);
    }

    #[test]
    fn telemetry_snapshot_serde_roundtrip() {
        let telem = AsupersyncTelemetry::new();
        telem.tasks_spawned.fetch_add(100, Ordering::Relaxed);
        telem.tasks_completed.fetch_add(95, Ordering::Relaxed);
        telem.tasks_leaked.fetch_add(2, Ordering::Relaxed);
        let snap = telem.snapshot();
        let json = serde_json::to_string(&snap).unwrap();
        let deserialized: AsupersyncTelemetrySnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized, snap);
    }

    #[test]
    fn active_scopes_computed_correctly() {
        let telem = AsupersyncTelemetry::new();
        telem.scopes_created.fetch_add(50, Ordering::Relaxed);
        telem.scopes_destroyed.fetch_add(30, Ordering::Relaxed);
        let snap = telem.snapshot();
        assert_eq!(snap.active_scopes(), 20);
    }

    #[test]
    fn task_leak_ratio_computed_correctly() {
        let telem = AsupersyncTelemetry::new();
        telem.tasks_spawned.fetch_add(1000, Ordering::Relaxed);
        telem.tasks_leaked.fetch_add(5, Ordering::Relaxed);
        let snap = telem.snapshot();
        let ratio = snap.task_leak_ratio();
        assert!((ratio - 0.005).abs() < 0.0001);
    }

    #[test]
    fn lock_contention_ratio_zero_when_no_acquisitions() {
        let snap = AsupersyncTelemetry::new().snapshot();
        assert_eq!(snap.lock_contention_ratio(), 0.0);
    }

    #[test]
    fn overall_health_green_when_clean() {
        let snap = AsupersyncTelemetry::new().snapshot();
        let cfg = AsupersyncObservabilityConfig::default();
        assert_eq!(snap.overall_health_tier(&cfg), HealthTier::Green);
    }

    #[test]
    fn overall_health_yellow_when_scopes_elevated() {
        let telem = AsupersyncTelemetry::new();
        telem.scopes_created.fetch_add(600, Ordering::Relaxed);
        let snap = telem.snapshot();
        let cfg = AsupersyncObservabilityConfig::default();
        assert_eq!(snap.overall_health_tier(&cfg), HealthTier::Yellow);
    }

    #[test]
    fn overall_health_red_when_scopes_critical() {
        let telem = AsupersyncTelemetry::new();
        telem.scopes_created.fetch_add(2500, Ordering::Relaxed);
        let snap = telem.snapshot();
        let cfg = AsupersyncObservabilityConfig::default();
        assert_eq!(snap.overall_health_tier(&cfg), HealthTier::Red);
    }

    #[test]
    fn overall_health_red_when_leaks_critical() {
        let telem = AsupersyncTelemetry::new();
        telem.tasks_spawned.fetch_add(10000, Ordering::Relaxed);
        telem.tasks_leaked.fetch_add(20, Ordering::Relaxed);
        let snap = telem.snapshot();
        let cfg = AsupersyncObservabilityConfig::default();
        assert_eq!(snap.overall_health_tier(&cfg), HealthTier::Red);
    }

    #[test]
    fn overall_health_black_when_recovery_failing() {
        let telem = AsupersyncTelemetry::new();
        telem.recovery_attempts.fetch_add(10, Ordering::Relaxed);
        telem.recovery_failures.fetch_add(8, Ordering::Relaxed);
        telem.recovery_successes.fetch_add(2, Ordering::Relaxed);
        let snap = telem.snapshot();
        let cfg = AsupersyncObservabilityConfig::default();
        assert_eq!(snap.overall_health_tier(&cfg), HealthTier::Black);
    }

    #[test]
    fn health_checks_all_pass_when_clean() {
        let snap = AsupersyncTelemetry::new().snapshot();
        let cfg = AsupersyncObservabilityConfig::default();
        let checks = evaluate_asupersync_health(&snap, &cfg);
        assert_eq!(checks.len(), 6);
        for check in &checks {
            assert!(
                check.status.is_healthy(),
                "check {} should be healthy, got {:?}",
                check.check_id,
                check.status
            );
        }
    }

    #[test]
    fn health_check_scope_tree_warns_on_elevated() {
        let telem = AsupersyncTelemetry::new();
        telem.scopes_created.fetch_add(600, Ordering::Relaxed);
        let snap = telem.snapshot();
        let cfg = AsupersyncObservabilityConfig::default();
        let checks = evaluate_asupersync_health(&snap, &cfg);
        let scope_check = checks
            .iter()
            .find(|c| c.check_id == "asupersync.scope_tree")
            .unwrap();
        assert_eq!(scope_check.status, CheckStatus::Warn);
    }

    #[test]
    fn health_check_scope_tree_fails_on_critical() {
        let telem = AsupersyncTelemetry::new();
        telem.scopes_created.fetch_add(2500, Ordering::Relaxed);
        let snap = telem.snapshot();
        let cfg = AsupersyncObservabilityConfig::default();
        let checks = evaluate_asupersync_health(&snap, &cfg);
        let scope_check = checks
            .iter()
            .find(|c| c.check_id == "asupersync.scope_tree")
            .unwrap();
        assert_eq!(scope_check.status, CheckStatus::Fail);
        assert_eq!(scope_check.failure_class, Some(FailureClass::Overload));
    }

    #[test]
    fn health_check_task_lifecycle_warns_on_panic() {
        let telem = AsupersyncTelemetry::new();
        telem.tasks_spawned.fetch_add(100, Ordering::Relaxed);
        telem.tasks_panicked.fetch_add(1, Ordering::Relaxed);
        let snap = telem.snapshot();
        let cfg = AsupersyncObservabilityConfig::default();
        let checks = evaluate_asupersync_health(&snap, &cfg);
        let task_check = checks
            .iter()
            .find(|c| c.check_id == "asupersync.task_lifecycle")
            .unwrap();
        assert_eq!(task_check.status, CheckStatus::Warn);
        assert_eq!(task_check.failure_class, Some(FailureClass::Panic));
    }

    #[test]
    fn health_check_cancellation_pass_when_no_requests() {
        let snap = AsupersyncTelemetry::new().snapshot();
        let cfg = AsupersyncObservabilityConfig::default();
        let checks = evaluate_asupersync_health(&snap, &cfg);
        let cancel_check = checks
            .iter()
            .find(|c| c.check_id == "asupersync.cancellation")
            .unwrap();
        assert_eq!(cancel_check.status, CheckStatus::Pass);
    }

    #[test]
    fn slo_samples_generated_from_snapshot() {
        let telem = AsupersyncTelemetry::new();
        telem.tasks_spawned.fetch_add(1000, Ordering::Relaxed);
        telem.tasks_completed.fetch_add(990, Ordering::Relaxed);
        telem.tasks_leaked.fetch_add(1, Ordering::Relaxed);
        telem.cancel_requests.fetch_add(50, Ordering::Relaxed);
        telem.cancel_completions.fetch_add(50, Ordering::Relaxed);
        telem
            .cancel_latency_sum_us
            .fetch_add(5000, Ordering::Relaxed);
        telem.record_cancel_latency_us(200);
        telem.channel_sends.fetch_add(5000, Ordering::Relaxed);
        let snap = telem.snapshot();
        let samples = generate_slo_samples(&snap);
        assert!(samples.len() >= 3, "should generate at least 3 SLO samples");
    }

    #[test]
    fn gate_report_pass_when_healthy() {
        let monitor = AsupersyncObservabilityMonitor::with_defaults();
        // With zero metrics, gate should pass (no samples)
        assert!(monitor.passes_gate());
    }

    #[test]
    fn gate_report_evaluates_slos() {
        let monitor = AsupersyncObservabilityMonitor::with_defaults();
        monitor
            .telemetry()
            .tasks_spawned
            .fetch_add(100, Ordering::Relaxed);
        monitor
            .telemetry()
            .tasks_completed
            .fetch_add(99, Ordering::Relaxed);
        let report = monitor.evaluate_slos();
        // Should have at least task leak and queue backlog SLOs
        assert!(report.total_slos >= 2);
    }

    #[test]
    fn monitor_records_health_samples() {
        let monitor = AsupersyncObservabilityMonitor::with_defaults();
        let _checks = monitor.evaluate_health();
        let snap = monitor.snapshot();
        assert_eq!(snap.health_samples, 1);
        assert_eq!(snap.health_green_samples, 1);
    }

    #[test]
    fn monitor_records_gate_verdicts() {
        let monitor = AsupersyncObservabilityMonitor::with_defaults();
        monitor
            .telemetry()
            .tasks_spawned
            .fetch_add(100, Ordering::Relaxed);
        let _report = monitor.evaluate_slos();
        let snap = monitor.snapshot();
        assert_eq!(snap.gate_evaluations, 1);
    }

    #[test]
    fn incident_context_captures_state() {
        let mut monitor = AsupersyncObservabilityMonitor::with_defaults();
        monitor.set_phase(RuntimePhase::Running);
        monitor.set_uptime_start(1000);
        let ctx = monitor.incident_context(5000);
        assert_eq!(ctx.phase, RuntimePhase::Running);
        assert_eq!(ctx.uptime_ms, 4000);
        assert_eq!(ctx.health_tier, HealthTier::Green);
    }

    #[test]
    fn render_summary_includes_key_sections() {
        let monitor = AsupersyncObservabilityMonitor::with_defaults();
        let summary = monitor.render_summary();
        assert!(summary.contains("Asupersync Runtime Observability"));
        assert!(summary.contains("Scope Tree"));
        assert!(summary.contains("Tasks"));
        assert!(summary.contains("Cancellation"));
        assert!(summary.contains("Channels"));
        assert!(summary.contains("Gate"));
    }

    #[test]
    fn health_distribution_sums_to_one() {
        let telem = AsupersyncTelemetry::new();
        telem.record_health_sample(HealthTier::Green);
        telem.record_health_sample(HealthTier::Green);
        telem.record_health_sample(HealthTier::Yellow);
        telem.record_health_sample(HealthTier::Red);
        let snap = telem.snapshot();
        let dist = snap.health_distribution();
        let sum: f64 = dist.iter().sum();
        assert!((sum - 1.0).abs() < 0.001);
    }

    #[test]
    fn tasks_pending_saturates_to_zero() {
        let telem = AsupersyncTelemetry::new();
        telem.tasks_completed.fetch_add(100, Ordering::Relaxed);
        let snap = telem.snapshot();
        assert_eq!(snap.tasks_pending(), 0);
    }

    #[test]
    fn update_max_is_monotonic() {
        let counter = AtomicU64::new(0);
        AsupersyncTelemetry::update_max(&counter, 10);
        AsupersyncTelemetry::update_max(&counter, 5);
        AsupersyncTelemetry::update_max(&counter, 15);
        assert_eq!(counter.load(Ordering::Relaxed), 15);
    }

    #[test]
    fn recovery_success_ratio_is_one_when_no_attempts() {
        let snap = AsupersyncTelemetry::new().snapshot();
        assert_eq!(snap.recovery_success_ratio(), 1.0);
    }

    #[test]
    fn gate_pass_ratio_is_one_when_no_evaluations() {
        let snap = AsupersyncTelemetry::new().snapshot();
        assert_eq!(snap.gate_pass_ratio(), 1.0);
    }

    #[test]
    fn health_check_lock_contention_passes_when_clean() {
        let snap = AsupersyncTelemetry::new().snapshot();
        let cfg = AsupersyncObservabilityConfig::default();
        let check = check_lock_contention(&snap, &cfg);
        assert_eq!(check.status, CheckStatus::Pass);
    }

    #[test]
    fn health_check_lock_contention_warns_on_elevated() {
        let telem = AsupersyncTelemetry::new();
        telem.lock_acquisitions.fetch_add(1000, Ordering::Relaxed);
        telem.lock_contentions.fetch_add(150, Ordering::Relaxed);
        let snap = telem.snapshot();
        let cfg = AsupersyncObservabilityConfig::default();
        let check = check_lock_contention(&snap, &cfg);
        assert_eq!(check.status, CheckStatus::Warn);
    }

    #[test]
    fn health_check_channel_warns_on_depth() {
        let telem = AsupersyncTelemetry::new();
        telem.channel_sends.fetch_add(1000, Ordering::Relaxed);
        telem.record_channel_depth(300);
        let snap = telem.snapshot();
        let cfg = AsupersyncObservabilityConfig::default();
        let check = check_channel_health(&snap, &cfg);
        assert_eq!(check.status, CheckStatus::Warn);
    }

    #[test]
    fn health_check_recovery_black_on_majority_failure() {
        let telem = AsupersyncTelemetry::new();
        telem.recovery_attempts.fetch_add(10, Ordering::Relaxed);
        telem.recovery_failures.fetch_add(8, Ordering::Relaxed);
        telem.recovery_successes.fetch_add(2, Ordering::Relaxed);
        let snap = telem.snapshot();
        let check = check_recovery_health(&snap);
        assert_eq!(check.status, CheckStatus::Fail);
        assert_eq!(check.tier, HealthTier::Black);
    }

    #[test]
    fn slo_breach_summary_serializable() {
        let summary = SloBreachSummary {
            slo_id: "rt.task_leak_rate".into(),
            measured: 0.005,
            target: 0.001,
            breach_duration_ms: 60_000,
            alert_tier: RuntimeAlertTier::Critical,
            critical: true,
        };
        let json = serde_json::to_string(&summary).unwrap();
        let deserialized: SloBreachSummary = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.slo_id, "rt.task_leak_rate");
    }

    #[test]
    fn incident_context_serializable() {
        let mut monitor = AsupersyncObservabilityMonitor::with_defaults();
        monitor.set_phase(RuntimePhase::Running);
        let ctx = monitor.incident_context(1000);
        let json = serde_json::to_string(&ctx).unwrap();
        let deserialized: AsupersyncIncidentContext = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.phase, RuntimePhase::Running);
    }
}
