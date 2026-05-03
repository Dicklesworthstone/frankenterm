use serde::{Deserialize, Serialize};

// ── C4: Adaptive Transport Policy ──────────────────────────────────

/// Transport mode for data transfer between pipeline stages.
///
/// # Invariants
/// - Local mode: zero-copy or memcpy, no serialization overhead.
/// - Compressed mode: zstd/lz4-style framing, higher latency, lower bandwidth.
/// - Bypass mode: skip compression when data is already compact or small.
/// - Mode selection is deterministic given the same cost model inputs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum TransportMode {
    /// In-process zero-copy or memcpy (fastest).
    Local,
    /// Compressed transfer for large or remote payloads.
    Compressed,
    /// Skip compression — data is small or already compact.
    Bypass,
}

impl std::fmt::Display for TransportMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TransportMode::Local => write!(f, "LOCAL"),
            TransportMode::Compressed => write!(f, "COMPRESSED"),
            TransportMode::Bypass => write!(f, "BYPASS"),
        }
    }
}

/// Cost model inputs for transport mode selection.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TransportCostModel {
    /// Compression CPU cost per byte (microseconds).
    pub compress_cost_per_byte_us: f64,
    /// Decompression CPU cost per byte (microseconds).
    pub decompress_cost_per_byte_us: f64,
    /// Network transfer cost per byte (microseconds) — 0 for local.
    pub network_cost_per_byte_us: f64,
    /// Expected compression ratio (0.0–1.0, lower = better compression).
    pub expected_compression_ratio: f64,
    /// Threshold below which bypass is cheaper than compress.
    pub bypass_threshold_bytes: u64,
    /// Threshold above which compression is always used.
    pub compress_threshold_bytes: u64,
}

impl Default for TransportCostModel {
    fn default() -> Self {
        Self {
            compress_cost_per_byte_us: 0.01,
            decompress_cost_per_byte_us: 0.005,
            network_cost_per_byte_us: 0.0,
            expected_compression_ratio: 0.4,
            bypass_threshold_bytes: 4096,
            compress_threshold_bytes: 65536,
        }
    }
}

/// Transport policy configuration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TransportPolicyConfig {
    /// Cost model for mode selection.
    pub cost_model: TransportCostModel,
    /// Enable adaptive mode switching (vs. fixed mode).
    pub adaptive: bool,
    /// Fixed mode when adaptive is disabled.
    pub fixed_mode: TransportMode,
    /// EWMA alpha for cost tracking (0.0–1.0).
    pub ewma_alpha: f64,
    /// Maximum history entries for cost tracking.
    pub max_history: usize,
}

impl Default for TransportPolicyConfig {
    fn default() -> Self {
        Self {
            cost_model: TransportCostModel::default(),
            adaptive: true,
            fixed_mode: TransportMode::Local,
            ewma_alpha: 0.1,
            max_history: 256,
        }
    }
}

/// A single transport decision record.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TransportDecision {
    pub payload_bytes: u64,
    pub selected_mode: TransportMode,
    pub estimated_cost_us: f64,
    pub actual_cost_us: f64,
    pub savings_us: f64,
    pub timestamp_us: u64,
}

/// Snapshot of the adaptive transport policy state.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TransportPolicySnapshot {
    pub total_decisions: u64,
    pub local_count: u64,
    pub compressed_count: u64,
    pub bypass_count: u64,
    pub total_bytes_transferred: u64,
    pub total_savings_us: f64,
    pub ewma_cost_us: f64,
}

/// Adaptive transport policy engine.
///
/// # Invariants
/// - `local_count + compressed_count + bypass_count == total_decisions`.
/// - Mode selection is pure function of (payload_bytes, cost_model, ewma state).
/// - EWMA cost tracks running average of actual transfer costs.
pub struct TransportPolicy {
    config: TransportPolicyConfig,
    total_decisions: u64,
    local_count: u64,
    compressed_count: u64,
    bypass_count: u64,
    total_bytes: u64,
    total_savings_us: f64,
    ewma_cost_us: f64,
    decisions: Vec<TransportDecision>,
}

impl TransportPolicy {
    /// Create with explicit config.
    pub fn new(config: TransportPolicyConfig) -> Self {
        Self {
            config,
            total_decisions: 0,
            local_count: 0,
            compressed_count: 0,
            bypass_count: 0,
            total_bytes: 0,
            total_savings_us: 0.0,
            ewma_cost_us: 0.0,
            decisions: Vec::new(),
        }
    }

    /// Create with defaults.
    pub fn with_defaults() -> Self {
        Self::new(TransportPolicyConfig::default())
    }

    /// Select the optimal transport mode for a given payload.
    pub fn select_mode(&self, payload_bytes: u64) -> TransportMode {
        if !self.config.adaptive {
            return self.config.fixed_mode;
        }
        let cm = &self.config.cost_model;
        if cm.network_cost_per_byte_us == 0.0 {
            // Local transfer — no network cost
            return TransportMode::Local;
        }
        if payload_bytes <= cm.bypass_threshold_bytes {
            return TransportMode::Bypass;
        }
        if payload_bytes >= cm.compress_threshold_bytes {
            return TransportMode::Compressed;
        }
        // Cost comparison: bypass vs compressed
        let bypass_cost = payload_bytes as f64 * cm.network_cost_per_byte_us;
        let compress_cost = (payload_bytes as f64 * cm.expected_compression_ratio).mul_add(
            cm.decompress_cost_per_byte_us,
            (payload_bytes as f64).mul_add(
                cm.compress_cost_per_byte_us,
                payload_bytes as f64 * cm.expected_compression_ratio * cm.network_cost_per_byte_us,
            ),
        );
        if bypass_cost <= compress_cost {
            TransportMode::Bypass
        } else {
            TransportMode::Compressed
        }
    }

    /// Record a transport decision and its outcome.
    pub fn record(
        &mut self,
        payload_bytes: u64,
        mode: TransportMode,
        estimated_cost_us: f64,
        actual_cost_us: f64,
        timestamp_us: u64,
    ) {
        let savings = estimated_cost_us - actual_cost_us;
        self.total_decisions += 1;
        match mode {
            TransportMode::Local => self.local_count += 1,
            TransportMode::Compressed => self.compressed_count += 1,
            TransportMode::Bypass => self.bypass_count += 1,
        }
        self.total_bytes += payload_bytes;
        self.total_savings_us += savings;

        // EWMA update
        let alpha = self.config.ewma_alpha;
        self.ewma_cost_us = alpha.mul_add(actual_cost_us, (1.0 - alpha) * self.ewma_cost_us);

        let decision = TransportDecision {
            payload_bytes,
            selected_mode: mode,
            estimated_cost_us,
            actual_cost_us,
            savings_us: savings,
            timestamp_us,
        };
        if self.decisions.len() < self.config.max_history {
            self.decisions.push(decision);
        }
    }

    /// Snapshot of current state.
    pub fn snapshot(&self) -> TransportPolicySnapshot {
        TransportPolicySnapshot {
            total_decisions: self.total_decisions,
            local_count: self.local_count,
            compressed_count: self.compressed_count,
            bypass_count: self.bypass_count,
            total_bytes_transferred: self.total_bytes,
            total_savings_us: self.total_savings_us,
            ewma_cost_us: self.ewma_cost_us,
        }
    }

    /// One-line status.
    pub fn status_line(&self) -> String {
        format!(
            "transport decisions={} local={} compressed={} bypass={} ewma={:.1}µs",
            self.total_decisions,
            self.local_count,
            self.compressed_count,
            self.bypass_count,
            self.ewma_cost_us,
        )
    }

    /// Recent decision history.
    pub fn recent_decisions(&self) -> &[TransportDecision] {
        &self.decisions
    }

    /// Reset all state.
    pub fn reset(&mut self) {
        self.total_decisions = 0;
        self.local_count = 0;
        self.compressed_count = 0;
        self.bypass_count = 0;
        self.total_bytes = 0;
        self.total_savings_us = 0.0;
        self.ewma_cost_us = 0.0;
        self.decisions.clear();
    }
}

/// Degradation states for the transport policy.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum TransportDegradation {
    Healthy,
    HighCost {
        ewma_cost_us: f64,
        threshold_us: f64,
    },
    ModeImbalance {
        dominant_mode: String,
        share: f64,
    },
}

impl std::fmt::Display for TransportDegradation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TransportDegradation::Healthy => write!(f, "HEALTHY"),
            TransportDegradation::HighCost {
                ewma_cost_us,
                threshold_us,
            } => {
                write!(f, "HIGH_COST({:.1}µs/{:.1}µs)", ewma_cost_us, threshold_us)
            }
            TransportDegradation::ModeImbalance {
                dominant_mode,
                share,
            } => {
                write!(f, "MODE_IMBALANCE({}={:.1}%)", dominant_mode, share * 100.0)
            }
        }
    }
}

/// Structured log entry for transport policy.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TransportLogEntry {
    pub total_decisions: u64,
    pub local_count: u64,
    pub compressed_count: u64,
    pub bypass_count: u64,
    pub ewma_cost_us: f64,
    pub degradation: TransportDegradation,
}

impl TransportPolicy {
    /// Detect degradation.
    pub fn detect_degradation(&self) -> TransportDegradation {
        // High cost threshold: 100µs EWMA
        if self.ewma_cost_us > 100.0 {
            return TransportDegradation::HighCost {
                ewma_cost_us: self.ewma_cost_us,
                threshold_us: 100.0,
            };
        }
        // Mode imbalance: any single mode > 95% of decisions (with 20+ decisions)
        if self.total_decisions >= 20 {
            let max_count = self
                .local_count
                .max(self.compressed_count)
                .max(self.bypass_count);
            let share = max_count as f64 / self.total_decisions as f64;
            if share > 0.95 {
                let mode_name = if max_count == self.local_count {
                    "Local"
                } else if max_count == self.compressed_count {
                    "Compressed"
                } else {
                    "Bypass"
                };
                return TransportDegradation::ModeImbalance {
                    dominant_mode: mode_name.to_string(),
                    share,
                };
            }
        }
        TransportDegradation::Healthy
    }

    /// Create a structured log entry.
    pub fn log_entry(&self) -> TransportLogEntry {
        TransportLogEntry {
            total_decisions: self.total_decisions,
            local_count: self.local_count,
            compressed_count: self.compressed_count,
            bypass_count: self.bypass_count,
            ewma_cost_us: self.ewma_cost_us,
            degradation: self.detect_degradation(),
        }
    }

    /// Select mode AND record outcome in one step (convenience).
    pub fn select_and_record(
        &mut self,
        payload_bytes: u64,
        actual_cost_us: f64,
        timestamp_us: u64,
    ) -> TransportMode {
        let mode = self.select_mode(payload_bytes);
        let estimated = self.estimate_cost(payload_bytes, mode);
        self.record(payload_bytes, mode, estimated, actual_cost_us, timestamp_us);
        mode
    }

    /// Estimate cost for a given payload + mode using the cost model.
    pub fn estimate_cost(&self, payload_bytes: u64, mode: TransportMode) -> f64 {
        let cm = &self.config.cost_model;
        match mode {
            TransportMode::Local => 0.0,
            TransportMode::Bypass => payload_bytes as f64 * cm.network_cost_per_byte_us,
            TransportMode::Compressed => {
                let compress = payload_bytes as f64 * cm.compress_cost_per_byte_us;
                let transfer = payload_bytes as f64
                    * cm.expected_compression_ratio
                    * cm.network_cost_per_byte_us;
                let decompress = payload_bytes as f64
                    * cm.expected_compression_ratio
                    * cm.decompress_cost_per_byte_us;
                compress + transfer + decompress
            }
        }
    }

    /// Mode distribution as fractions (local_share, compressed_share, bypass_share).
    pub fn mode_distribution(&self) -> (f64, f64, f64) {
        if self.total_decisions == 0 {
            return (0.0, 0.0, 0.0);
        }
        let total = self.total_decisions as f64;
        (
            self.local_count as f64 / total,
            self.compressed_count as f64 / total,
            self.bypass_count as f64 / total,
        )
    }

    /// Average cost per byte across all recorded decisions.
    pub fn avg_cost_per_byte(&self) -> f64 {
        if self.total_bytes == 0 {
            return 0.0;
        }
        self.ewma_cost_us / (self.total_bytes as f64 / self.total_decisions as f64)
    }

    /// Total bytes transferred.
    pub fn total_bytes(&self) -> u64 {
        self.total_bytes
    }

    /// Total savings (sum of estimated - actual across all decisions).
    pub fn total_savings_us(&self) -> f64 {
        self.total_savings_us
    }

    /// Current EWMA cost.
    pub fn ewma_cost_us(&self) -> f64 {
        self.ewma_cost_us
    }

    /// Update the cost model at runtime (e.g., after measuring real network costs).
    pub fn update_cost_model(&mut self, cost_model: TransportCostModel) {
        self.config.cost_model = cost_model;
    }

    /// Switch between adaptive and fixed mode.
    pub fn set_adaptive(&mut self, adaptive: bool) {
        self.config.adaptive = adaptive;
    }

    /// Set fixed mode (used when adaptive is disabled).
    pub fn set_fixed_mode(&mut self, mode: TransportMode) {
        self.config.fixed_mode = mode;
    }
}
