use serde::{Deserialize, Serialize};

// ── D1: Bayesian Hitch-Risk Posterior Model ────────────────────────

/// Evidence signal types for the hitch-risk posterior.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum EvidenceSignal {
    /// p99 latency probe from a specific stage.
    LatencyProbe,
    /// Backpressure level change.
    BackpressureChange,
    /// Queue depth observation.
    QueueDepth,
    /// Budget violation event.
    BudgetViolation,
    /// GC or memory pressure event.
    MemoryPressure,
    /// CPU load observation.
    CpuLoad,
}

impl std::fmt::Display for EvidenceSignal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EvidenceSignal::LatencyProbe => write!(f, "LATENCY_PROBE"),
            EvidenceSignal::BackpressureChange => write!(f, "BACKPRESSURE"),
            EvidenceSignal::QueueDepth => write!(f, "QUEUE_DEPTH"),
            EvidenceSignal::BudgetViolation => write!(f, "BUDGET_VIOLATION"),
            EvidenceSignal::MemoryPressure => write!(f, "MEMORY_PRESSURE"),
            EvidenceSignal::CpuLoad => write!(f, "CPU_LOAD"),
        }
    }
}

/// A single evidence entry in the ledger.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EvidenceEntry {
    pub signal: EvidenceSignal,
    pub value: f64,
    pub log_likelihood_ratio: f64,
    pub timestamp_us: u64,
}

/// Hitch-risk level classification.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum HitchRiskLevel {
    /// Low risk — system is healthy.
    Low,
    /// Elevated risk — some signals above baseline.
    Elevated,
    /// High risk — multiple signals indicate impending hitch.
    High,
    /// Critical — hitch is imminent or occurring.
    Critical,
}

impl std::fmt::Display for HitchRiskLevel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HitchRiskLevel::Low => write!(f, "LOW"),
            HitchRiskLevel::Elevated => write!(f, "ELEVATED"),
            HitchRiskLevel::High => write!(f, "HIGH"),
            HitchRiskLevel::Critical => write!(f, "CRITICAL"),
        }
    }
}

/// Configuration for the hitch-risk model.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HitchRiskConfig {
    /// Prior probability of hitch (0.0–1.0).
    pub prior_hitch_prob: f64,
    /// Threshold for Elevated risk (log-odds).
    pub elevated_threshold: f64,
    /// Threshold for High risk (log-odds).
    pub high_threshold: f64,
    /// Threshold for Critical risk (log-odds).
    pub critical_threshold: f64,
    /// Maximum evidence entries to retain.
    pub max_evidence: usize,
    /// Decay factor for old evidence (0.0–1.0, 1.0 = no decay).
    pub evidence_decay: f64,
}

impl Default for HitchRiskConfig {
    fn default() -> Self {
        Self {
            prior_hitch_prob: 0.05,
            elevated_threshold: 1.0,
            high_threshold: 3.0,
            critical_threshold: 5.0,
            max_evidence: 512,
            evidence_decay: 0.95,
        }
    }
}

/// Snapshot of the hitch-risk model state.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HitchRiskSnapshot {
    pub log_odds: f64,
    pub posterior_prob: f64,
    pub risk_level: HitchRiskLevel,
    pub evidence_count: usize,
    pub total_updates: u64,
}

/// Bayesian hitch-risk posterior model.
///
/// # Invariants
/// - `posterior_prob` is always in [0, 1].
/// - `log_odds` is the log-odds form of posterior (allows stable additive updates).
/// - Evidence is decayed by `evidence_decay` each update to reduce stale signal weight.
/// - Risk level is monotonically mapped from log_odds via thresholds.
pub struct HitchRiskModel {
    config: HitchRiskConfig,
    log_odds: f64,
    evidence: Vec<EvidenceEntry>,
    total_updates: u64,
}

impl HitchRiskModel {
    /// Create with explicit config.
    pub fn new(config: HitchRiskConfig) -> Self {
        let prior = config.prior_hitch_prob.clamp(1e-10, 1.0 - 1e-10);
        let log_odds = (prior / (1.0 - prior)).ln();
        Self {
            config,
            log_odds,
            evidence: Vec::new(),
            total_updates: 0,
        }
    }

    /// Create with defaults.
    pub fn with_defaults() -> Self {
        Self::new(HitchRiskConfig::default())
    }

    /// Submit evidence and update the posterior.
    /// `log_likelihood_ratio` > 0 means evidence favors hitch, < 0 favors healthy.
    pub fn update(&mut self, signal: EvidenceSignal, value: f64, llr: f64, timestamp_us: u64) {
        // Decay existing log-odds
        self.log_odds *= self.config.evidence_decay;
        // Add new evidence
        self.log_odds += llr;
        self.total_updates += 1;

        let entry = EvidenceEntry {
            signal,
            value,
            log_likelihood_ratio: llr,
            timestamp_us,
        };
        if self.evidence.len() < self.config.max_evidence {
            self.evidence.push(entry);
        } else {
            // Circular overwrite
            let idx = (self.total_updates as usize - 1) % self.config.max_evidence;
            self.evidence[idx] = entry;
        }
    }

    /// Current posterior probability of hitch.
    pub fn posterior_prob(&self) -> f64 {
        let odds = self.log_odds.exp();
        if odds.is_infinite() {
            return 1.0;
        }
        odds / (1.0 + odds)
    }

    /// Current risk level.
    pub fn risk_level(&self) -> HitchRiskLevel {
        if self.log_odds >= self.config.critical_threshold {
            HitchRiskLevel::Critical
        } else if self.log_odds >= self.config.high_threshold {
            HitchRiskLevel::High
        } else if self.log_odds >= self.config.elevated_threshold {
            HitchRiskLevel::Elevated
        } else {
            HitchRiskLevel::Low
        }
    }

    /// Current log-odds.
    pub fn log_odds(&self) -> f64 {
        self.log_odds
    }

    /// Snapshot.
    pub fn snapshot(&self) -> HitchRiskSnapshot {
        HitchRiskSnapshot {
            log_odds: self.log_odds,
            posterior_prob: self.posterior_prob(),
            risk_level: self.risk_level(),
            evidence_count: self.evidence.len(),
            total_updates: self.total_updates,
        }
    }

    /// Status line.
    pub fn status_line(&self) -> String {
        format!(
            "hitch-risk level={} prob={:.3} log_odds={:.2} evidence={} updates={}",
            self.risk_level(),
            self.posterior_prob(),
            self.log_odds,
            self.evidence.len(),
            self.total_updates,
        )
    }

    /// Reset to prior.
    pub fn reset(&mut self) {
        let prior = self.config.prior_hitch_prob.clamp(1e-10, 1.0 - 1e-10);
        self.log_odds = (prior / (1.0 - prior)).ln();
        self.evidence.clear();
        self.total_updates = 0;
    }

    /// Recent evidence entries.
    pub fn recent_evidence(&self) -> &[EvidenceEntry] {
        &self.evidence
    }
}

/// Degradation states for the hitch-risk model.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum HitchRiskDegradation {
    Healthy,
    ElevatedRisk {
        posterior_prob: f64,
    },
    HighRisk {
        posterior_prob: f64,
        evidence_count: usize,
    },
    CriticalRisk {
        posterior_prob: f64,
        log_odds: f64,
    },
}

impl std::fmt::Display for HitchRiskDegradation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HitchRiskDegradation::Healthy => write!(f, "HEALTHY"),
            HitchRiskDegradation::ElevatedRisk { posterior_prob } => {
                write!(f, "ELEVATED({:.1}%)", posterior_prob * 100.0)
            }
            HitchRiskDegradation::HighRisk {
                posterior_prob,
                evidence_count,
            } => {
                write!(
                    f,
                    "HIGH({:.1}%, {} evidence)",
                    posterior_prob * 100.0,
                    evidence_count
                )
            }
            HitchRiskDegradation::CriticalRisk {
                posterior_prob,
                log_odds,
            } => {
                write!(
                    f,
                    "CRITICAL({:.1}%, lo={:.2})",
                    posterior_prob * 100.0,
                    log_odds
                )
            }
        }
    }
}

/// Log entry for hitch-risk model.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HitchRiskLogEntry {
    pub log_odds: f64,
    pub posterior_prob: f64,
    pub risk_level: HitchRiskLevel,
    pub evidence_count: usize,
    pub total_updates: u64,
    pub degradation: HitchRiskDegradation,
}

impl HitchRiskModel {
    /// Detect degradation.
    pub fn detect_degradation(&self) -> HitchRiskDegradation {
        match self.risk_level() {
            HitchRiskLevel::Critical => HitchRiskDegradation::CriticalRisk {
                posterior_prob: self.posterior_prob(),
                log_odds: self.log_odds,
            },
            HitchRiskLevel::High => HitchRiskDegradation::HighRisk {
                posterior_prob: self.posterior_prob(),
                evidence_count: self.evidence.len(),
            },
            HitchRiskLevel::Elevated => HitchRiskDegradation::ElevatedRisk {
                posterior_prob: self.posterior_prob(),
            },
            HitchRiskLevel::Low => HitchRiskDegradation::Healthy,
        }
    }

    /// Log entry.
    pub fn log_entry(&self) -> HitchRiskLogEntry {
        HitchRiskLogEntry {
            log_odds: self.log_odds,
            posterior_prob: self.posterior_prob(),
            risk_level: self.risk_level(),
            evidence_count: self.evidence.len(),
            total_updates: self.total_updates,
            degradation: self.detect_degradation(),
        }
    }

    /// Quick convenience: submit a budget violation signal.
    pub fn observe_violation(&mut self, severity_llr: f64, timestamp_us: u64) {
        self.update(
            EvidenceSignal::BudgetViolation,
            1.0,
            severity_llr,
            timestamp_us,
        );
    }

    /// Quick convenience: submit a latency probe signal.
    pub fn observe_latency(&mut self, latency_us: f64, llr: f64, timestamp_us: u64) {
        self.update(EvidenceSignal::LatencyProbe, latency_us, llr, timestamp_us);
    }

    /// Quick convenience: submit healthy evidence (negative LLR).
    pub fn observe_healthy(&mut self, timestamp_us: u64) {
        self.update(EvidenceSignal::LatencyProbe, 0.0, -0.5, timestamp_us);
    }

    /// Whether the model currently recommends mitigation.
    pub fn should_mitigate(&self) -> bool {
        matches!(
            self.risk_level(),
            HitchRiskLevel::High | HitchRiskLevel::Critical
        )
    }

    /// Whether the model is in critical state.
    pub fn is_critical(&self) -> bool {
        self.risk_level() == HitchRiskLevel::Critical
    }

    /// Update the evidence decay factor.
    pub fn set_evidence_decay(&mut self, decay: f64) {
        self.config.evidence_decay = decay.clamp(0.0, 1.0);
    }

    /// Update the prior (resets log_odds to match new prior).
    pub fn set_prior(&mut self, prior: f64) {
        let p = prior.clamp(1e-10, 1.0 - 1e-10);
        self.config.prior_hitch_prob = p;
    }

    /// Total updates received.
    pub fn total_updates(&self) -> u64 {
        self.total_updates
    }

    /// Evidence count.
    pub fn evidence_count(&self) -> usize {
        self.evidence.len()
    }
}
