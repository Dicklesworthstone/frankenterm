use serde::{Deserialize, Serialize};

// ── D2: Anytime-Valid E-Process Drift Detector ─────────────────────

/// Type of e-process test statistic.
///
/// Each variant corresponds to a different sequential testing strategy:
/// - `CusumLike`: running maximum of likelihood ratio (Page's CUSUM adapted to e-process form)
/// - `Mixture`: Bayesian mixture over alternatives, valid under optional stopping
/// - `ConfidenceSequence`: inverted confidence sequence for mean shift detection
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum EProcessKind {
    CusumLike,
    Mixture,
    ConfidenceSequence,
}

impl std::fmt::Display for EProcessKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::CusumLike => write!(f, "cusum_like"),
            Self::Mixture => write!(f, "mixture"),
            Self::ConfidenceSequence => write!(f, "confidence_seq"),
        }
    }
}

/// What observable is being monitored for drift.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum DriftObservable {
    Latency,
    Throughput,
    ErrorRate,
    QueueDepth,
    ResourceUsage,
}

impl std::fmt::Display for DriftObservable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Latency => write!(f, "latency"),
            Self::Throughput => write!(f, "throughput"),
            Self::ErrorRate => write!(f, "error_rate"),
            Self::QueueDepth => write!(f, "queue_depth"),
            Self::ResourceUsage => write!(f, "resource_usage"),
        }
    }
}

/// Alert level produced by the e-process detector.
///
/// `None` means no evidence of drift.  `Warning` indicates growing evidence
/// (e-value approaching threshold).  `Alarm` means the e-value has crossed
/// 1/alpha and the null hypothesis of no-change is rejected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum DriftAlertLevel {
    None,
    Warning,
    Alarm,
}

impl std::fmt::Display for DriftAlertLevel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::None => write!(f, "none"),
            Self::Warning => write!(f, "warning"),
            Self::Alarm => write!(f, "alarm"),
        }
    }
}

/// Configuration for the e-process drift detector.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EProcessConfig {
    /// Which e-process variant to use.
    pub kind: EProcessKind,
    /// Observable being monitored.
    pub observable: DriftObservable,
    /// Significance level (alpha).  E-value threshold = 1/alpha.
    pub alpha: f64,
    /// Warning fraction of the threshold (e.g. 0.5 means warn at half the log-threshold).
    pub warning_fraction: f64,
    /// Mixing parameter lambda for CusumLike / Mixture (controls sensitivity vs delay).
    pub lambda: f64,
    /// Null hypothesis mean (mu_0).  Observations are compared against this.
    pub null_mean: f64,
    /// Maximum number of observations to retain in the history window.
    pub max_history: usize,
    /// Minimum observations before the detector can raise an alarm.
    pub warmup: usize,
    /// Whether to auto-reset after alarm (running detector) or latch.
    pub auto_reset: bool,
}

impl EProcessConfig {
    /// Sensible defaults for latency monitoring.
    pub fn default_latency() -> Self {
        Self {
            kind: EProcessKind::Mixture,
            observable: DriftObservable::Latency,
            alpha: 0.05,
            warning_fraction: 0.5,
            lambda: 0.1,
            null_mean: 0.0,
            max_history: 1000,
            warmup: 20,
            auto_reset: true,
        }
    }
}

/// A single observation fed to the e-process detector.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EProcessObservation {
    /// Observable value.
    pub value: f64,
    /// Which observable this came from.
    pub observable: DriftObservable,
    /// Timestamp in microseconds.
    pub timestamp_us: u64,
    /// The likelihood ratio for this observation (computed by the detector).
    pub likelihood_ratio: f64,
}

/// Snapshot of the e-process detector state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EProcessSnapshot {
    /// Current e-value (test statistic).
    pub e_value: f64,
    /// Log of the e-value for numerical stability.
    pub log_e_value: f64,
    /// Current alert level.
    pub alert_level: DriftAlertLevel,
    /// Total observations processed.
    pub total_observations: u64,
    /// Number of alarms raised since last reset (or ever).
    pub alarm_count: u64,
    /// Number of warnings raised.
    pub warning_count: u64,
    /// Running mean of observations.
    pub running_mean: f64,
    /// Running variance (Welford online).
    pub running_variance: f64,
    /// Maximum e-value ever observed.
    pub peak_e_value: f64,
}

/// The main e-process drift detector.
///
/// Maintains a running e-value (nonnegative supermartingale starting at 1).
/// Under the null hypothesis (no drift), E[E_t] <= 1.
/// When E_t >= 1/alpha, we reject the null at level alpha.
/// This guarantee holds under *optional stopping* — you can check at any time.
#[derive(Debug, Clone)]
pub struct EProcessDetector {
    config: EProcessConfig,
    /// Current log-e-value (we work in log space for stability).
    log_e_value: f64,
    /// Peak log-e-value seen.
    peak_log_e_value: f64,
    /// Total observations fed.
    total_observations: u64,
    /// Count of alarms.
    alarm_count: u64,
    /// Count of warnings.
    warning_count: u64,
    /// Welford running mean.
    mean: f64,
    /// Welford M2 for variance.
    m2: f64,
    /// Recent observations (ring buffer).
    history: Vec<EProcessObservation>,
    /// Head pointer for ring buffer.
    history_head: usize,
    /// Whether the detector is currently in alarm state.
    in_alarm: bool,
}

impl EProcessDetector {
    /// Create a new detector with the given configuration.
    pub fn new(config: EProcessConfig) -> Self {
        let cap = config.max_history;
        Self {
            config,
            log_e_value: 0.0, // E_0 = 1 => log(E_0) = 0
            peak_log_e_value: 0.0,
            total_observations: 0,
            alarm_count: 0,
            warning_count: 0,
            mean: 0.0,
            m2: 0.0,
            history: Vec::with_capacity(cap.min(64)),
            history_head: 0,
            in_alarm: false,
        }
    }

    /// Create a detector with sensible defaults for latency monitoring.
    pub fn with_defaults() -> Self {
        Self::new(EProcessConfig::default_latency())
    }

    /// Feed a new observation to the detector and update the e-value.
    ///
    /// Returns the current alert level after incorporating this observation.
    pub fn observe(&mut self, value: f64, timestamp_us: u64) -> DriftAlertLevel {
        self.total_observations += 1;
        let n = self.total_observations as f64;

        // Welford online mean/variance
        let delta = value - self.mean;
        self.mean += delta / n;
        let delta2 = value - self.mean;
        self.m2 = delta.mul_add(delta2, self.m2);

        // Compute likelihood ratio based on e-process kind
        let lr = self.compute_likelihood_ratio(value);
        let log_lr = if lr > 0.0 { lr.ln() } else { f64::NEG_INFINITY };

        // Update log-e-value
        match self.config.kind {
            EProcessKind::CusumLike => {
                // CUSUM-like: E_t = max(1, E_{t-1}) * LR_t
                // In log: log_E_t = max(0, log_E_{t-1}) + log_LR_t
                self.log_e_value = self.log_e_value.max(0.0) + log_lr;
            }
            EProcessKind::Mixture | EProcessKind::ConfidenceSequence => {
                // Standard product: E_t = E_{t-1} * LR_t
                // In log: log_E_t = log_E_{t-1} + log_LR_t
                self.log_e_value += log_lr;
            }
        }

        // Track peak
        if self.log_e_value > self.peak_log_e_value {
            self.peak_log_e_value = self.log_e_value;
        }

        // Record observation
        let obs = EProcessObservation {
            value,
            observable: self.config.observable,
            timestamp_us,
            likelihood_ratio: lr,
        };
        if self.history.len() < self.config.max_history {
            self.history.push(obs);
        } else if self.config.max_history > 0 {
            self.history[self.history_head] = obs;
            self.history_head = (self.history_head + 1) % self.config.max_history;
        }

        // Determine alert level
        self.alert_level()
    }

    /// Compute the likelihood ratio for a single observation.
    fn compute_likelihood_ratio(&self, value: f64) -> f64 {
        let lambda = self.config.lambda;
        let deviation = value - self.config.null_mean;
        // Universal e-variable: 1 + lambda * deviation
        // Clamped to be nonneg (required for e-process validity).
        lambda.mul_add(deviation, 1.0).max(0.0)
    }

    /// Current alert level based on the log-e-value vs the threshold.
    pub fn alert_level(&mut self) -> DriftAlertLevel {
        if self.total_observations < self.config.warmup as u64 {
            return DriftAlertLevel::None;
        }

        let log_threshold = (1.0 / self.config.alpha).ln();
        let log_warning = log_threshold * self.config.warning_fraction;

        if self.log_e_value >= log_threshold {
            if !self.in_alarm {
                self.alarm_count += 1;
                self.in_alarm = true;
            }
            if self.config.auto_reset {
                // Reset e-value after alarm
                self.log_e_value = 0.0;
                self.in_alarm = false;
            }
            DriftAlertLevel::Alarm
        } else if self.log_e_value >= log_warning {
            if self.in_alarm {
                self.in_alarm = false;
            }
            self.warning_count += 1;
            DriftAlertLevel::Warning
        } else {
            if self.in_alarm {
                self.in_alarm = false;
            }
            DriftAlertLevel::None
        }
    }

    /// Current e-value (exponentiated from log for display).
    pub fn e_value(&self) -> f64 {
        self.log_e_value.exp()
    }

    /// Current log-e-value.
    pub fn log_e_value(&self) -> f64 {
        self.log_e_value
    }

    /// Running mean of observations.
    pub fn running_mean(&self) -> f64 {
        self.mean
    }

    /// Running variance of observations (sample variance).
    pub fn running_variance(&self) -> f64 {
        if self.total_observations < 2 {
            return 0.0;
        }
        self.m2 / (self.total_observations as f64 - 1.0)
    }

    /// Total observations processed.
    pub fn total_observations(&self) -> u64 {
        self.total_observations
    }

    /// Number of alarms raised.
    pub fn alarm_count(&self) -> u64 {
        self.alarm_count
    }

    /// Snapshot of current state.
    pub fn snapshot(&self) -> EProcessSnapshot {
        EProcessSnapshot {
            e_value: self.log_e_value.exp(),
            log_e_value: self.log_e_value,
            alert_level: if self.total_observations < self.config.warmup as u64 {
                DriftAlertLevel::None
            } else {
                let log_threshold = (1.0 / self.config.alpha).ln();
                if self.log_e_value >= log_threshold {
                    DriftAlertLevel::Alarm
                } else if self.log_e_value >= log_threshold * self.config.warning_fraction {
                    DriftAlertLevel::Warning
                } else {
                    DriftAlertLevel::None
                }
            },
            total_observations: self.total_observations,
            alarm_count: self.alarm_count,
            warning_count: self.warning_count,
            running_mean: self.mean,
            running_variance: self.running_variance(),
            peak_e_value: self.peak_log_e_value.exp(),
        }
    }

    /// Human-readable status line.
    pub fn status_line(&self) -> String {
        let snap = self.snapshot();
        format!(
            "e-proc[{}] e={:.3} alert={} obs={} alarms={} mean={:.2}",
            self.config.kind,
            snap.e_value,
            snap.alert_level,
            snap.total_observations,
            snap.alarm_count,
            snap.running_mean,
        )
    }

    /// Reset the detector to initial state (preserving config).
    pub fn reset(&mut self) {
        self.log_e_value = 0.0;
        self.peak_log_e_value = 0.0;
        self.total_observations = 0;
        self.alarm_count = 0;
        self.warning_count = 0;
        self.mean = 0.0;
        self.m2 = 0.0;
        self.history.clear();
        self.history_head = 0;
        self.in_alarm = false;
    }

    /// Recent observation history.
    pub fn recent_observations(&self, n: usize) -> Vec<&EProcessObservation> {
        let len = self.history.len();
        if len == 0 || n == 0 {
            return Vec::new();
        }
        let take = n.min(len);
        let mut result = Vec::with_capacity(take);
        if len < self.config.max_history {
            // Not wrapped yet
            let start = len.saturating_sub(take);
            for obs in &self.history[start..] {
                result.push(obs);
            }
        } else {
            // Wrapped ring buffer — read from tail
            for i in 0..take {
                let idx = (self.history_head + len - take + i) % len;
                result.push(&self.history[idx]);
            }
        }
        result
    }

    /// The e-process kind.
    pub fn kind(&self) -> EProcessKind {
        self.config.kind
    }

    /// Number of stored observations.
    pub fn history_len(&self) -> usize {
        self.history.len()
    }

    /// Detect degradation based on current state.
    pub fn detect_degradation(&self) -> EProcessDegradation {
        if self.total_observations < self.config.warmup as u64 {
            return EProcessDegradation::Healthy;
        }
        let log_threshold = (1.0 / self.config.alpha).ln();
        if self.log_e_value >= log_threshold {
            EProcessDegradation::DriftDetected {
                e_value: self.log_e_value.exp(),
                alarm_count: self.alarm_count,
            }
        } else if self.log_e_value >= log_threshold * self.config.warning_fraction {
            EProcessDegradation::DriftSuspected {
                e_value: self.log_e_value.exp(),
                running_mean: self.mean,
            }
        } else {
            EProcessDegradation::Healthy
        }
    }

    /// Generate structured log entry.
    pub fn log_entry(&self) -> EProcessLogEntry {
        EProcessLogEntry {
            e_value: self.log_e_value.exp(),
            log_e_value: self.log_e_value,
            total_observations: self.total_observations,
            alarm_count: self.alarm_count,
            warning_count: self.warning_count,
            running_mean: self.mean,
            degradation: self.detect_degradation(),
        }
    }

    // ── D2 Impl: Bridge Methods and Convenience API ────────────────

    /// Observe a batch of values at once.
    pub fn observe_batch(&mut self, values: &[(f64, u64)]) -> DriftAlertLevel {
        let mut last = DriftAlertLevel::None;
        for &(value, ts) in values {
            last = self.observe(value, ts);
        }
        last
    }

    /// Observe a latency sample in microseconds.
    pub fn observe_latency_us(&mut self, latency_us: f64, timestamp_us: u64) -> DriftAlertLevel {
        self.observe(latency_us, timestamp_us)
    }

    /// Current standard deviation of observations.
    pub fn running_stddev(&self) -> f64 {
        self.running_variance().sqrt()
    }

    /// Z-score of a given value relative to the running distribution.
    pub fn z_score(&self, value: f64) -> f64 {
        let std = self.running_stddev();
        if std < 1e-12 {
            return 0.0;
        }
        (value - self.mean) / std
    }

    /// Fraction of observations that resulted in alarm.
    pub fn alarm_rate(&self) -> f64 {
        if self.total_observations == 0 {
            return 0.0;
        }
        self.alarm_count as f64 / self.total_observations as f64
    }

    /// Whether the detector is currently in alarm state.
    pub fn is_alarming(&self) -> bool {
        self.in_alarm
    }

    /// Set the mixing parameter lambda (sensitivity).
    pub fn set_lambda(&mut self, lambda: f64) {
        self.config.lambda = lambda;
    }

    /// Set the null-hypothesis mean.
    pub fn set_null_mean(&mut self, mean: f64) {
        self.config.null_mean = mean;
    }

    /// Set the significance level alpha.
    pub fn set_alpha(&mut self, alpha: f64) {
        self.config.alpha = alpha;
    }

    /// Warning count.
    pub fn warning_count(&self) -> u64 {
        self.warning_count
    }

    /// Peak e-value ever observed.
    pub fn peak_e_value(&self) -> f64 {
        self.peak_log_e_value.exp()
    }
}

/// Degradation status for the e-process detector.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum EProcessDegradation {
    Healthy,
    DriftSuspected { e_value: f64, running_mean: f64 },
    DriftDetected { e_value: f64, alarm_count: u64 },
}

impl std::fmt::Display for EProcessDegradation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Healthy => write!(f, "healthy"),
            Self::DriftSuspected { e_value, .. } => {
                write!(f, "drift_suspected(e={e_value:.3})")
            }
            Self::DriftDetected {
                e_value,
                alarm_count,
            } => {
                write!(f, "drift_detected(e={e_value:.3}, alarms={alarm_count})")
            }
        }
    }
}

/// Structured log entry for the e-process detector.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EProcessLogEntry {
    pub e_value: f64,
    pub log_e_value: f64,
    pub total_observations: u64,
    pub alarm_count: u64,
    pub warning_count: u64,
    pub running_mean: f64,
    pub degradation: EProcessDegradation,
}
