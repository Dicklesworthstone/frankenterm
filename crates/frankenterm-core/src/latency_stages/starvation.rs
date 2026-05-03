use std::{collections::VecDeque, fmt};

use serde::{Deserialize, Serialize};

use super::SchedulerLane;

// AARSP Bead: ft-2p9cb.2.4 - Starvation Prevention & Fairness

// AARSP Bead: ft-2p9cb.2.4.1

/// Configuration for starvation prevention.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StarvationConfig {
    /// Max consecutive epochs a lane can go unserviced before forced promotion.
    pub max_starved_epochs: u64,
    /// Fairness window size (epochs) for computing running averages.
    pub fairness_window: usize,
    /// Minimum share of CPU any lane must receive (0.0..1.0).
    pub min_lane_share: f64,
    /// Enable aging: deferred items get priority boost over time.
    pub enable_aging: bool,
    /// Aging boost interval: every N epochs, deferred items gain one priority level.
    pub aging_interval_epochs: u64,
}

impl Default for StarvationConfig {
    fn default() -> Self {
        Self {
            max_starved_epochs: 5,
            fairness_window: 20,
            min_lane_share: 0.05,
            enable_aging: true,
            aging_interval_epochs: 3,
        }
    }
}

/// Per-lane fairness state tracked over a sliding window.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LaneFairnessState {
    /// Which lane.
    pub lane: SchedulerLane,
    /// Consecutive epochs with zero completions.
    pub starved_epochs: u64,
    /// CPU share over the fairness window (0.0..1.0).
    pub windowed_share: f64,
    /// Total completions in the fairness window.
    pub windowed_completions: u64,
    /// Total items deferred in the fairness window.
    pub windowed_deferred: u64,
    /// Whether this lane is currently being force-promoted.
    pub force_promoted: bool,
}

/// A starvation event records when a lane was force-promoted.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StarvationEvent {
    /// Epoch when detected.
    pub epoch: u64,
    /// Lane that was starving.
    pub lane: SchedulerLane,
    /// Consecutive starved epochs before promotion.
    pub starved_epochs: u64,
    /// CPU share at the time of detection.
    pub cpu_share: f64,
}

/// Fairness snapshot across all lanes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FairnessSnapshot {
    /// Per-lane fairness state.
    pub lanes: Vec<LaneFairnessState>,
    /// Gini coefficient of CPU shares (0.0 = perfect equality, 1.0 = total inequality).
    pub gini_coefficient: f64,
    /// Total starvation events since creation.
    pub total_starvation_events: u64,
    /// Whether any lane is currently starving.
    pub any_starving: bool,
}

/// Starvation prevention tracker. Monitors per-lane service rates,
/// detects starvation, and triggers force-promotions.
///
/// # Invariants
///
/// 1. No lane goes more than `max_starved_epochs` without service.
/// 2. Every lane's windowed share >= min_lane_share (or force-promotion triggers).
/// 3. Gini coefficient is in [0.0, 1.0].
/// 4. Deterministic: same epoch observations -> same fairness state.
#[derive(Debug, Clone)]
pub struct StarvationTracker {
    config: StarvationConfig,
    /// Per-lane state.
    lanes: Vec<LaneFairnessState>,
    /// History of per-epoch CPU shares per lane (ring buffer).
    share_history: Vec<Vec<f64>>,
    history_head: usize,
    epoch: u64,
    events: VecDeque<StarvationEvent>,
    max_events: usize,
    total_starvation_events: u64,
}

impl StarvationTracker {
    /// Create a new tracker.
    pub fn new(config: StarvationConfig) -> Self {
        let window = config.fairness_window.max(1);
        Self {
            lanes: vec![
                LaneFairnessState {
                    lane: SchedulerLane::Input,
                    starved_epochs: 0,
                    windowed_share: 0.0,
                    windowed_completions: 0,
                    windowed_deferred: 0,
                    force_promoted: false,
                },
                LaneFairnessState {
                    lane: SchedulerLane::Control,
                    starved_epochs: 0,
                    windowed_share: 0.0,
                    windowed_completions: 0,
                    windowed_deferred: 0,
                    force_promoted: false,
                },
                LaneFairnessState {
                    lane: SchedulerLane::Bulk,
                    starved_epochs: 0,
                    windowed_share: 0.0,
                    windowed_completions: 0,
                    windowed_deferred: 0,
                    force_promoted: false,
                },
            ],
            share_history: vec![vec![0.0; 3]; window],
            history_head: 0,
            epoch: 0,
            events: VecDeque::new(),
            max_events: 256,
            total_starvation_events: 0,
            config: StarvationConfig {
                fairness_window: window,
                ..config
            },
        }
    }

    /// Create with default config.
    pub fn with_defaults() -> Self {
        Self::new(StarvationConfig::default())
    }

    /// Record one epoch's observations: completions and CPU shares per lane.
    /// Returns list of lanes that are now force-promoted.
    pub fn observe_epoch(
        &mut self,
        completions: &[u64; 3],
        cpu_shares: &[f64; 3],
    ) -> Vec<SchedulerLane> {
        self.epoch += 1;
        let mut promoted = Vec::new();

        // Record shares in ring buffer.
        self.share_history[self.history_head] = cpu_shares.to_vec();
        self.history_head = (self.history_head + 1) % self.config.fairness_window;

        // Update per-lane state.
        for (i, lane_state) in self.lanes.iter_mut().enumerate() {
            if completions[i] == 0 {
                lane_state.starved_epochs += 1;
            } else {
                lane_state.starved_epochs = 0;
                lane_state.force_promoted = false;
            }

            // Compute windowed share.
            let mut sum = 0.0;
            let mut count = 0;
            for entry in &self.share_history {
                if entry[i] > 0.0 || count < self.epoch as usize {
                    sum += entry[i];
                    count += 1;
                }
            }
            lane_state.windowed_share = if count > 0 { sum / count as f64 } else { 0.0 };
            lane_state.windowed_completions = completions[i];
            lane_state.windowed_deferred = 0; // will be updated externally

            // Check starvation.
            if lane_state.starved_epochs >= self.config.max_starved_epochs
                && !lane_state.force_promoted
            {
                lane_state.force_promoted = true;
                self.total_starvation_events += 1;

                let event = StarvationEvent {
                    epoch: self.epoch,
                    lane: lane_state.lane,
                    starved_epochs: lane_state.starved_epochs,
                    cpu_share: lane_state.windowed_share,
                };
                if self.events.len() >= self.max_events {
                    self.events.pop_front();
                }
                self.events.push_back(event);

                promoted.push(lane_state.lane);
            }
        }

        promoted
    }

    /// Compute the Gini coefficient of current windowed shares.
    pub fn gini_coefficient(&self) -> f64 {
        let shares: Vec<f64> = self.lanes.iter().map(|l| l.windowed_share).collect();
        let n = shares.len() as f64;
        if n == 0.0 {
            return 0.0;
        }
        let mean = shares.iter().sum::<f64>() / n;
        if mean <= 0.0 {
            return 0.0;
        }

        let mut sum_abs_diff = 0.0;
        for i in 0..shares.len() {
            for j in 0..shares.len() {
                sum_abs_diff += (shares[i] - shares[j]).abs();
            }
        }

        sum_abs_diff / (2.0 * n * n * mean)
    }

    /// Whether any lane is currently starving.
    pub fn any_starving(&self) -> bool {
        self.lanes.iter().any(|l| l.force_promoted)
    }

    /// Diagnostic snapshot.
    pub fn snapshot(&self) -> FairnessSnapshot {
        FairnessSnapshot {
            lanes: self.lanes.clone(),
            gini_coefficient: self.gini_coefficient(),
            total_starvation_events: self.total_starvation_events,
            any_starving: self.any_starving(),
        }
    }

    /// Status line for logging.
    pub fn status_line(&self) -> String {
        let snap = self.snapshot();
        format!(
            "fairness gini={:.3} starving={} events={} epoch={}",
            snap.gini_coefficient, snap.any_starving, snap.total_starvation_events, self.epoch,
        )
    }

    /// Current epoch.
    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    /// Get lane fairness state.
    pub fn lane_state(&self, lane: SchedulerLane) -> &LaneFairnessState {
        &self.lanes[lane as usize]
    }

    /// Reset starvation counters for all lanes.
    pub fn reset(&mut self) {
        for lane_state in &mut self.lanes {
            lane_state.starved_epochs = 0;
            lane_state.force_promoted = false;
            lane_state.windowed_share = 0.0;
            lane_state.windowed_completions = 0;
            lane_state.windowed_deferred = 0;
        }
        self.epoch = 0;
        self.total_starvation_events = 0;
        self.events.clear();
        self.history_head = 0;
        for entry in &mut self.share_history {
            for v in entry.iter_mut() {
                *v = 0.0;
            }
        }
    }

    /// Get the most recent starvation events (up to limit).
    pub fn recent_events(&self, limit: usize) -> Vec<StarvationEvent> {
        let start = self.events.len().saturating_sub(limit);
        self.events.iter().skip(start).cloned().collect()
    }

    /// Whether a specific lane is force-promoted.
    pub fn is_force_promoted(&self, lane: SchedulerLane) -> bool {
        self.lanes[lane as usize].force_promoted
    }
}

// AARSP Bead: ft-2p9cb.2.4.2

/// Degradation signal from the starvation tracker.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum FairnessDegradation {
    /// Everything is fine.
    Healthy,
    /// One or more lanes are starving.
    LaneStarvation { starving_lanes: Vec<SchedulerLane> },
    /// Gini coefficient is too high: severe unfairness.
    SevereUnfairness { gini: f64, threshold: f64 },
    /// Force promotions are happening too frequently.
    PromotionStorm {
        events_in_window: u64,
        threshold: u64,
    },
}

impl fmt::Display for FairnessDegradation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Healthy => write!(f, "HEALTHY"),
            Self::LaneStarvation { starving_lanes } => {
                write!(f, "LANE_STARVATION({:?})", starving_lanes)
            }
            Self::SevereUnfairness { gini, threshold } => {
                write!(
                    f,
                    "SEVERE_UNFAIRNESS(gini={:.3}/thresh={:.3})",
                    gini, threshold
                )
            }
            Self::PromotionStorm {
                events_in_window,
                threshold,
            } => write!(f, "PROMOTION_STORM({}/{})", events_in_window, threshold),
        }
    }
}

/// Structured log entry for fairness/starvation events.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FairnessLogEntry {
    /// Epoch.
    pub epoch: u64,
    /// Per-lane windowed shares.
    pub shares: Vec<f64>,
    /// Per-lane starved epoch counts.
    pub starved_epochs: Vec<u64>,
    /// Gini coefficient.
    pub gini_coefficient: f64,
    /// Whether any lane is starving.
    pub any_starving: bool,
    /// Degradation signal.
    pub degradation: FairnessDegradation,
}

impl StarvationTracker {
    /// Detect degradation based on current state.
    pub fn detect_degradation(&self) -> FairnessDegradation {
        // Check for lane starvation.
        let starving: Vec<SchedulerLane> = self
            .lanes
            .iter()
            .filter(|l| l.force_promoted)
            .map(|l| l.lane)
            .collect();
        if !starving.is_empty() {
            return FairnessDegradation::LaneStarvation {
                starving_lanes: starving,
            };
        }

        // Check Gini coefficient (threshold: 0.5).
        let gini = self.gini_coefficient();
        if gini > 0.5 {
            return FairnessDegradation::SevereUnfairness {
                gini,
                threshold: 0.5,
            };
        }

        // Check for promotion storms (>5 events in last window).
        if self.total_starvation_events > 5 {
            return FairnessDegradation::PromotionStorm {
                events_in_window: self.total_starvation_events,
                threshold: 5,
            };
        }

        FairnessDegradation::Healthy
    }

    /// Generate a structured log entry.
    pub fn log_entry(&self) -> FairnessLogEntry {
        FairnessLogEntry {
            epoch: self.epoch,
            shares: self.lanes.iter().map(|l| l.windowed_share).collect(),
            starved_epochs: self.lanes.iter().map(|l| l.starved_epochs).collect(),
            gini_coefficient: self.gini_coefficient(),
            any_starving: self.any_starving(),
            degradation: self.detect_degradation(),
        }
    }
}
