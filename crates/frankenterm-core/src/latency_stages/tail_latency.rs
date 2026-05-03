use serde::{Deserialize, Serialize};

// ── C5: Kernel/Hardware Tail-Latency ───────────────────────────────

/// Syscall batching strategy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum SyscallStrategy {
    /// Issue syscalls one at a time.
    Immediate,
    /// Batch multiple syscalls before issuing.
    Batched,
    /// Adaptive: batch under load, immediate under low latency.
    Adaptive,
}

impl std::fmt::Display for SyscallStrategy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SyscallStrategy::Immediate => write!(f, "IMMEDIATE"),
            SyscallStrategy::Batched => write!(f, "BATCHED"),
            SyscallStrategy::Adaptive => write!(f, "ADAPTIVE"),
        }
    }
}

/// Wakeup source attribution.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum WakeupSource {
    /// Timer-based wakeup (epoll_wait timeout, select, etc.).
    Timer,
    /// I/O event wakeup (read/write ready, socket, pty).
    IoEvent,
    /// Signal-based wakeup (SIGCHLD, SIGWINCH, etc.).
    Signal,
    /// Explicit nudge from another thread/task.
    Nudge,
}

impl std::fmt::Display for WakeupSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WakeupSource::Timer => write!(f, "TIMER"),
            WakeupSource::IoEvent => write!(f, "IO_EVENT"),
            WakeupSource::Signal => write!(f, "SIGNAL"),
            WakeupSource::Nudge => write!(f, "NUDGE"),
        }
    }
}

/// CPU affinity placement hint.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum AffinityHint {
    /// No preference — OS scheduler decides.
    Any,
    /// Prefer performance cores (P-cores on hybrid CPUs).
    PerformanceCore,
    /// Prefer efficiency cores (E-cores on hybrid CPUs).
    EfficiencyCore,
    /// Pin to a specific core ID.
    Pinned(u32),
}

impl std::fmt::Display for AffinityHint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AffinityHint::Any => write!(f, "ANY"),
            AffinityHint::PerformanceCore => write!(f, "P_CORE"),
            AffinityHint::EfficiencyCore => write!(f, "E_CORE"),
            AffinityHint::Pinned(id) => write!(f, "PINNED({})", id),
        }
    }
}

/// Configuration for the tail-latency controller.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TailLatencyConfig {
    /// Syscall batching strategy.
    pub syscall_strategy: SyscallStrategy,
    /// Maximum batch size before forced flush.
    pub max_batch_size: usize,
    /// Timer precision target in microseconds.
    pub timer_precision_us: u64,
    /// Affinity hint for the hot path thread.
    pub affinity: AffinityHint,
    /// p99 latency budget in microseconds.
    pub p99_budget_us: u64,
    /// p999 latency budget in microseconds.
    pub p999_budget_us: u64,
}

impl Default for TailLatencyConfig {
    fn default() -> Self {
        Self {
            syscall_strategy: SyscallStrategy::Adaptive,
            max_batch_size: 64,
            timer_precision_us: 1000, // 1ms
            affinity: AffinityHint::Any,
            p99_budget_us: 10_000,  // 10ms
            p999_budget_us: 50_000, // 50ms
        }
    }
}

/// A single wakeup event observation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WakeupEvent {
    pub source: WakeupSource,
    pub latency_us: u64,
    pub timestamp_us: u64,
    pub batch_depth: usize,
}

/// Tail-latency snapshot.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TailLatencySnapshot {
    pub total_wakeups: u64,
    pub timer_wakeups: u64,
    pub io_wakeups: u64,
    pub signal_wakeups: u64,
    pub nudge_wakeups: u64,
    pub total_syscalls: u64,
    pub total_batches: u64,
    pub avg_batch_depth: f64,
    pub p99_latency_us: u64,
    pub max_latency_us: u64,
    pub budget_violations: u64,
}

/// Tail-latency controller: tracks wakeup latencies, syscall batching, and budget compliance.
///
/// # Invariants
/// - `timer + io + signal + nudge == total_wakeups`.
/// - Latency samples are stored in a bounded ring for percentile estimation.
/// - Budget violations count only p99 breaches (not p50).
pub struct TailLatencyController {
    config: TailLatencyConfig,
    total_wakeups: u64,
    timer_wakeups: u64,
    io_wakeups: u64,
    signal_wakeups: u64,
    nudge_wakeups: u64,
    total_syscalls: u64,
    total_batches: u64,
    batch_depth_sum: u64,
    latency_samples: Vec<u64>,
    max_samples: usize,
    sample_head: usize,
    max_latency_us: u64,
    budget_violations: u64,
}

impl TailLatencyController {
    /// Create with explicit config.
    pub fn new(config: TailLatencyConfig) -> Self {
        Self {
            config,
            total_wakeups: 0,
            timer_wakeups: 0,
            io_wakeups: 0,
            signal_wakeups: 0,
            nudge_wakeups: 0,
            total_syscalls: 0,
            total_batches: 0,
            batch_depth_sum: 0,
            latency_samples: Vec::new(),
            max_samples: 1024,
            sample_head: 0,
            max_latency_us: 0,
            budget_violations: 0,
        }
    }

    /// Create with defaults.
    pub fn with_defaults() -> Self {
        Self::new(TailLatencyConfig::default())
    }

    /// Record a wakeup event.
    pub fn record_wakeup(&mut self, source: WakeupSource, latency_us: u64) {
        self.total_wakeups += 1;
        match source {
            WakeupSource::Timer => self.timer_wakeups += 1,
            WakeupSource::IoEvent => self.io_wakeups += 1,
            WakeupSource::Signal => self.signal_wakeups += 1,
            WakeupSource::Nudge => self.nudge_wakeups += 1,
        }
        if latency_us > self.max_latency_us {
            self.max_latency_us = latency_us;
        }
        if latency_us > self.config.p99_budget_us {
            self.budget_violations += 1;
        }
        // Ring buffer for samples
        if self.latency_samples.len() < self.max_samples {
            self.latency_samples.push(latency_us);
        } else {
            self.latency_samples[self.sample_head] = latency_us;
            self.sample_head = (self.sample_head + 1) % self.max_samples;
        }
    }

    /// Record a syscall batch.
    pub fn record_batch(&mut self, depth: usize) {
        self.total_batches += 1;
        self.total_syscalls += depth as u64;
        self.batch_depth_sum += depth as u64;
    }

    /// Estimate p99 latency from stored samples.
    pub fn p99_latency_us(&self) -> u64 {
        if self.latency_samples.is_empty() {
            return 0;
        }
        let mut sorted = self.latency_samples.clone();
        sorted.sort_unstable();
        let idx = ((sorted.len() as f64 * 0.99) as usize).min(sorted.len() - 1);
        sorted[idx]
    }

    /// Average batch depth.
    pub fn avg_batch_depth(&self) -> f64 {
        if self.total_batches == 0 {
            return 0.0;
        }
        self.batch_depth_sum as f64 / self.total_batches as f64
    }

    /// Snapshot.
    pub fn snapshot(&self) -> TailLatencySnapshot {
        TailLatencySnapshot {
            total_wakeups: self.total_wakeups,
            timer_wakeups: self.timer_wakeups,
            io_wakeups: self.io_wakeups,
            signal_wakeups: self.signal_wakeups,
            nudge_wakeups: self.nudge_wakeups,
            total_syscalls: self.total_syscalls,
            total_batches: self.total_batches,
            avg_batch_depth: self.avg_batch_depth(),
            p99_latency_us: self.p99_latency_us(),
            max_latency_us: self.max_latency_us,
            budget_violations: self.budget_violations,
        }
    }

    /// Status line.
    pub fn status_line(&self) -> String {
        format!(
            "tail-latency wakeups={} p99={}µs max={}µs violations={} batches={}",
            self.total_wakeups,
            self.p99_latency_us(),
            self.max_latency_us,
            self.budget_violations,
            self.total_batches,
        )
    }

    /// Reset all state.
    pub fn reset(&mut self) {
        self.total_wakeups = 0;
        self.timer_wakeups = 0;
        self.io_wakeups = 0;
        self.signal_wakeups = 0;
        self.nudge_wakeups = 0;
        self.total_syscalls = 0;
        self.total_batches = 0;
        self.batch_depth_sum = 0;
        self.latency_samples.clear();
        self.sample_head = 0;
        self.max_latency_us = 0;
        self.budget_violations = 0;
    }

    /// Current syscall strategy.
    pub fn strategy(&self) -> SyscallStrategy {
        self.config.syscall_strategy
    }

    /// Current affinity hint.
    pub fn affinity(&self) -> AffinityHint {
        self.config.affinity
    }

    /// Number of stored latency samples.
    pub fn sample_count(&self) -> usize {
        self.latency_samples.len()
    }
}

/// Degradation states for tail-latency controller.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum TailLatencyDegradation {
    Healthy,
    P99Breach { observed_us: u64, budget_us: u64 },
    P999Breach { observed_us: u64, budget_us: u64 },
    HighViolationRate { violations: u64, total: u64 },
}

impl std::fmt::Display for TailLatencyDegradation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TailLatencyDegradation::Healthy => write!(f, "HEALTHY"),
            TailLatencyDegradation::P99Breach {
                observed_us,
                budget_us,
            } => {
                write!(f, "P99_BREACH({}µs/{}µs)", observed_us, budget_us)
            }
            TailLatencyDegradation::P999Breach {
                observed_us,
                budget_us,
            } => {
                write!(f, "P999_BREACH({}µs/{}µs)", observed_us, budget_us)
            }
            TailLatencyDegradation::HighViolationRate { violations, total } => {
                write!(f, "HIGH_VIOLATIONS({}/{})", violations, total)
            }
        }
    }
}

/// Structured log entry for tail-latency.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TailLatencyLogEntry {
    pub total_wakeups: u64,
    pub p99_latency_us: u64,
    pub max_latency_us: u64,
    pub budget_violations: u64,
    pub avg_batch_depth: f64,
    pub degradation: TailLatencyDegradation,
}

impl TailLatencyController {
    /// Detect degradation.
    pub fn detect_degradation(&self) -> TailLatencyDegradation {
        let p99 = self.p99_latency_us();
        if self.max_latency_us > self.config.p999_budget_us {
            return TailLatencyDegradation::P999Breach {
                observed_us: self.max_latency_us,
                budget_us: self.config.p999_budget_us,
            };
        }
        if p99 > self.config.p99_budget_us {
            return TailLatencyDegradation::P99Breach {
                observed_us: p99,
                budget_us: self.config.p99_budget_us,
            };
        }
        // High violation rate: > 5% of wakeups exceed budget
        if self.total_wakeups >= 20 {
            let rate = self.budget_violations as f64 / self.total_wakeups as f64;
            if rate > 0.05 {
                return TailLatencyDegradation::HighViolationRate {
                    violations: self.budget_violations,
                    total: self.total_wakeups,
                };
            }
        }
        TailLatencyDegradation::Healthy
    }

    /// Log entry.
    pub fn log_entry(&self) -> TailLatencyLogEntry {
        TailLatencyLogEntry {
            total_wakeups: self.total_wakeups,
            p99_latency_us: self.p99_latency_us(),
            max_latency_us: self.max_latency_us,
            budget_violations: self.budget_violations,
            avg_batch_depth: self.avg_batch_depth(),
            degradation: self.detect_degradation(),
        }
    }

    /// Estimate p50 latency from stored samples.
    pub fn p50_latency_us(&self) -> u64 {
        if self.latency_samples.is_empty() {
            return 0;
        }
        let mut sorted = self.latency_samples.clone();
        sorted.sort_unstable();
        let idx = (sorted.len() / 2).min(sorted.len() - 1);
        sorted[idx]
    }

    /// Wakeup source distribution as fractions (timer, io, signal, nudge).
    pub fn wakeup_distribution(&self) -> (f64, f64, f64, f64) {
        if self.total_wakeups == 0 {
            return (0.0, 0.0, 0.0, 0.0);
        }
        let total = self.total_wakeups as f64;
        (
            self.timer_wakeups as f64 / total,
            self.io_wakeups as f64 / total,
            self.signal_wakeups as f64 / total,
            self.nudge_wakeups as f64 / total,
        )
    }

    /// Violation rate (0.0–1.0).
    pub fn violation_rate(&self) -> f64 {
        if self.total_wakeups == 0 {
            return 0.0;
        }
        self.budget_violations as f64 / self.total_wakeups as f64
    }

    /// Whether the controller is currently within p99 budget.
    pub fn within_p99_budget(&self) -> bool {
        self.p99_latency_us() <= self.config.p99_budget_us
    }

    /// Whether the controller is currently within p999 budget.
    pub fn within_p999_budget(&self) -> bool {
        self.max_latency_us <= self.config.p999_budget_us
    }

    /// Update syscall strategy at runtime.
    pub fn set_strategy(&mut self, strategy: SyscallStrategy) {
        self.config.syscall_strategy = strategy;
    }

    /// Update affinity hint at runtime.
    pub fn set_affinity(&mut self, hint: AffinityHint) {
        self.config.affinity = hint;
    }

    /// Update p99 budget.
    pub fn set_p99_budget(&mut self, budget_us: u64) {
        self.config.p99_budget_us = budget_us;
    }

    /// Total wakeups count.
    pub fn total_wakeups(&self) -> u64 {
        self.total_wakeups
    }

    /// Total budget violations.
    pub fn budget_violations(&self) -> u64 {
        self.budget_violations
    }
}
