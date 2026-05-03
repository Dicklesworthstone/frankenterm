#![allow(clippy::float_cmp)]
#![allow(clippy::similar_names)]
#![allow(clippy::overly_complex_bool_expr)]
#![allow(unused_parens)]
//! Context budget and compaction observability for AI agent sessions.
//!
//! Tracks context-window pressure, compaction/rotation events, and recovery
//! state for long-running AI agent sessions so operators can proactively
//! manage degradation before it silently impacts quality.
//!
//! # Design
//!
//! Each monitored agent pane has a [`ContextBudgetTracker`] that accumulates:
//! - Token consumption estimates (input + output)
//! - Compaction events (when the context window is compressed/rotated)
//! - Pressure tier classification (Green/Yellow/Red/Black)
//! - Recovery guidance when pressure is high
//!
//! The [`ContextBudgetRegistry`] aggregates trackers across all panes and
//! produces fleet-wide [`ContextBudgetSnapshot`] for dashboards and alerting.
//!
//! # Integration
//!
//! - Pattern engine detects compaction markers in agent output
//! - Unified telemetry envelopes carry context budget snapshots
//! - Fleet dashboard surfaces pressure as an operator-visible panel
//!
//! # Bead
//!
//! Implements ft-3681t.9.4 — context budget and compaction observability.

use std::collections::{HashMap, VecDeque};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

const MAX_REMOVED_PANE_SNAPSHOTS: usize = 128;

// ---------------------------------------------------------------------------
// Pressure tier
// ---------------------------------------------------------------------------

/// Context-window pressure tier for an agent session.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum ContextPressureTier {
    /// < 50% of context window used.
    #[default]
    Green,
    /// 50-75% — operator should be aware.
    Yellow,
    /// 75-90% — compaction likely imminent.
    Red,
    /// > 90% — compaction active or context nearly exhausted.
    Black,
}

impl ContextPressureTier {
    /// Classify pressure from a utilization ratio (0.0..1.0).
    pub fn from_utilization(ratio: f64) -> Self {
        if ratio >= 0.90 {
            Self::Black
        } else if ratio >= 0.75 {
            Self::Red
        } else if ratio >= 0.50 {
            Self::Yellow
        } else {
            Self::Green
        }
    }

    /// Whether this tier warrants operator attention.
    pub fn needs_attention(self) -> bool {
        matches!(self, Self::Red | Self::Black)
    }
}

// ---------------------------------------------------------------------------
// Compaction event
// ---------------------------------------------------------------------------

/// A recorded compaction/rotation event in an agent's context window.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompactionEvent {
    /// When the compaction was detected (epoch ms).
    pub detected_at_ms: u64,
    /// Pane ID where the compaction occurred.
    pub pane_id: u64,
    /// Estimated tokens before compaction.
    pub tokens_before: u64,
    /// Estimated tokens after compaction.
    pub tokens_after: u64,
    /// What triggered the compaction.
    pub trigger: CompactionTrigger,
    /// Agent program that experienced the compaction.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_program: Option<String>,
    /// Anomaly marker when the before/after estimate pair is suspicious.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub estimate_anomaly: Option<CompactionEstimateAnomaly>,
}

/// Suspicious accounting detected while recording a compaction event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompactionEstimateAnomaly {
    /// The post-compaction estimate is larger than the pre-compaction estimate.
    TokensIncreased,
}

/// What triggered a context compaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompactionTrigger {
    /// Automatic compaction by the AI agent runtime.
    Automatic,
    /// Operator-initiated compaction (e.g. `/compact` command).
    OperatorInitiated,
    /// Session rotation (new conversation started).
    SessionRotation,
    /// Unknown/detected from output patterns.
    PatternDetected,
}

// ---------------------------------------------------------------------------
// Recovery guidance
// ---------------------------------------------------------------------------

/// Operator-facing recovery guidance when context pressure is high.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextRecoveryGuidance {
    /// Recommended action.
    pub action: RecoveryAction,
    /// Human-readable explanation.
    pub reason: String,
    /// Estimated tokens that would be freed by this action.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub estimated_freed_tokens: Option<u64>,
}

/// Recovery actions an operator can take.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecoveryAction {
    /// Send `/compact` to the agent.
    SendCompact,
    /// Rotate to a new session.
    RotateSession,
    /// Reduce output verbosity.
    ReduceVerbosity,
    /// No action needed yet.
    Monitor,
}

// ---------------------------------------------------------------------------
// Per-pane tracker
// ---------------------------------------------------------------------------

/// Configuration for context budget tracking.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextBudgetConfig {
    /// Maximum context window size in tokens for this agent type.
    pub max_tokens: u64,
    /// Maximum compaction events to retain per pane.
    pub max_compaction_history: usize,
}

impl Default for ContextBudgetConfig {
    fn default() -> Self {
        Self {
            max_tokens: 200_000,
            max_compaction_history: 100,
        }
    }
}

/// Tracks context budget for a single agent pane.
#[derive(Debug, Clone)]
pub struct ContextBudgetTracker {
    pane_id: u64,
    config: ContextBudgetConfig,
    /// Current estimated token consumption.
    estimated_tokens: u64,
    /// Peak token consumption observed.
    peak_tokens: u64,
    /// Compaction event history.
    compactions: VecDeque<CompactionEvent>,
    /// Total compactions observed.
    total_compactions: u64,
    /// Last update timestamp (epoch ms).
    last_updated_ms: u64,
    /// Agent program name (if known).
    agent_program: Option<String>,
}

impl ContextBudgetTracker {
    /// Create a new tracker for the given pane.
    pub fn new(pane_id: u64, config: ContextBudgetConfig) -> Self {
        Self {
            pane_id,
            config,
            estimated_tokens: 0,
            peak_tokens: 0,
            compactions: VecDeque::new(),
            total_compactions: 0,
            last_updated_ms: epoch_ms(),
            agent_program: None,
        }
    }

    /// Set the agent program name.
    pub fn set_agent_program(&mut self, program: impl Into<String>) {
        self.agent_program = Some(program.into());
    }

    /// Update the estimated token count.
    pub fn update_tokens(&mut self, estimated_tokens: u64) {
        self.estimated_tokens = estimated_tokens;
        if estimated_tokens > self.peak_tokens {
            self.peak_tokens = estimated_tokens;
        }
        self.last_updated_ms = epoch_ms();
    }

    /// Record a compaction event.
    ///
    /// br-ft-ps09a: `peak_tokens` is updated from the maximum of
    /// `tokens_before`, `tokens_after`, and the existing peak.
    /// Pre-fix, `record_compaction` only set `estimated_tokens =
    /// tokens_after` and never touched `peak_tokens`, so a fresh
    /// tracker receiving its first observation as a compaction
    /// (e.g. pattern engine detected the compaction marker before
    /// any `update_tokens` call) would report
    /// `total_compactions > 0 AND peak_tokens == 0` in the
    /// snapshot — operators lost the high-water context evidence
    /// needed to judge compaction severity. The before/after pair
    /// IS the high-water evidence the snapshot needs.
    ///
    /// br-ft-q84b9: a compaction whose `tokens_after` exceeds
    /// `tokens_before` is retained but explicitly marked anomalous.
    /// That keeps the evidence available to operators without
    /// presenting suspicious accounting as an ordinary successful
    /// compaction.
    pub fn record_compaction(
        &mut self,
        tokens_before: u64,
        tokens_after: u64,
        trigger: CompactionTrigger,
    ) {
        let estimate_anomaly =
            (tokens_after > tokens_before).then_some(CompactionEstimateAnomaly::TokensIncreased);
        let event = CompactionEvent {
            detected_at_ms: epoch_ms(),
            pane_id: self.pane_id,
            tokens_before,
            tokens_after,
            trigger,
            agent_program: self.agent_program.clone(),
            estimate_anomaly,
        };
        self.compactions.push_back(event);
        self.total_compactions += 1;
        self.estimated_tokens = tokens_after;
        self.peak_tokens = self
            .peak_tokens
            .max(tokens_before)
            .max(tokens_after);
        self.last_updated_ms = epoch_ms();

        // Evict old history
        while self.compactions.len() > self.config.max_compaction_history {
            self.compactions.pop_front();
        }
    }

    /// Current utilization ratio (0.0..1.0).
    ///
    /// A zero `max_tokens` means the agent capacity is unknown or malformed.
    /// Once any tokens are observed, fail closed to full pressure instead of
    /// reporting Green and disabling budget detection.
    pub fn utilization(&self) -> f64 {
        if self.config.max_tokens == 0 {
            return if self.estimated_tokens == 0 { 0.0 } else { 1.0 };
        }
        ((self.estimated_tokens as f64) / (self.config.max_tokens as f64)).min(1.0)
    }

    /// Current pressure tier.
    pub fn pressure_tier(&self) -> ContextPressureTier {
        ContextPressureTier::from_utilization(self.utilization())
    }

    /// Generate recovery guidance based on current pressure.
    pub fn recovery_guidance(&self) -> ContextRecoveryGuidance {
        let tier = self.pressure_tier();
        match tier {
            ContextPressureTier::Black => ContextRecoveryGuidance {
                action: RecoveryAction::RotateSession,
                reason: format!(
                    "Context {:.0}% full ({}/{} tokens). Session rotation recommended.",
                    self.utilization() * 100.0,
                    self.estimated_tokens,
                    self.config.max_tokens
                ),
                estimated_freed_tokens: Some(self.estimated_tokens.saturating_sub(1000)),
            },
            ContextPressureTier::Red => ContextRecoveryGuidance {
                action: RecoveryAction::SendCompact,
                reason: format!(
                    "Context {:.0}% full. Compaction recommended to free space.",
                    self.utilization() * 100.0
                ),
                estimated_freed_tokens: Some(self.estimated_tokens / 3),
            },
            ContextPressureTier::Yellow => ContextRecoveryGuidance {
                action: RecoveryAction::Monitor,
                reason: format!(
                    "Context {:.0}% full. Monitoring — no action needed yet.",
                    self.utilization() * 100.0
                ),
                estimated_freed_tokens: None,
            },
            ContextPressureTier::Green => ContextRecoveryGuidance {
                action: RecoveryAction::Monitor,
                reason: "Context usage healthy.".into(),
                estimated_freed_tokens: None,
            },
        }
    }

    /// Produce a serializable snapshot.
    pub fn snapshot(&self) -> PaneContextSnapshot {
        PaneContextSnapshot {
            pane_id: self.pane_id,
            agent_program: self.agent_program.clone(),
            max_tokens: self.config.max_tokens,
            estimated_tokens: self.estimated_tokens,
            peak_tokens: self.peak_tokens,
            utilization: self.utilization(),
            pressure_tier: self.pressure_tier(),
            total_compactions: self.total_compactions,
            recent_compactions: self.compactions.iter().rev().take(5).cloned().collect(),
            guidance: self.recovery_guidance(),
            last_updated_ms: self.last_updated_ms,
        }
    }
}

/// Serializable snapshot of a single pane's context budget.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PaneContextSnapshot {
    pub pane_id: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_program: Option<String>,
    pub max_tokens: u64,
    pub estimated_tokens: u64,
    pub peak_tokens: u64,
    pub utilization: f64,
    pub pressure_tier: ContextPressureTier,
    pub total_compactions: u64,
    pub recent_compactions: Vec<CompactionEvent>,
    pub guidance: ContextRecoveryGuidance,
    pub last_updated_ms: u64,
}

/// Final context-budget evidence retained after a pane is removed.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemovedPaneContextSnapshot {
    pub removed_at_ms: u64,
    pub final_snapshot: PaneContextSnapshot,
}

// ---------------------------------------------------------------------------
// Fleet-wide registry
// ---------------------------------------------------------------------------

/// Aggregates context budget trackers across all monitored panes.
#[derive(Debug)]
pub struct ContextBudgetRegistry {
    trackers: HashMap<u64, ContextBudgetTracker>,
    removed_panes: VecDeque<RemovedPaneContextSnapshot>,
    default_config: ContextBudgetConfig,
}

impl ContextBudgetRegistry {
    /// Create a new registry with the given default config.
    pub fn new(default_config: ContextBudgetConfig) -> Self {
        Self {
            trackers: HashMap::new(),
            removed_panes: VecDeque::new(),
            default_config,
        }
    }

    /// Get or create a tracker for the given pane.
    pub fn tracker_mut(&mut self, pane_id: u64) -> &mut ContextBudgetTracker {
        self.trackers
            .entry(pane_id)
            .or_insert_with(|| ContextBudgetTracker::new(pane_id, self.default_config.clone()))
    }

    /// Get a tracker reference (if exists).
    pub fn tracker(&self, pane_id: u64) -> Option<&ContextBudgetTracker> {
        self.trackers.get(&pane_id)
    }

    /// Remove a tracker (pane closed).
    pub fn remove(&mut self, pane_id: u64) -> Option<ContextBudgetTracker> {
        let tracker = self.trackers.remove(&pane_id)?;
        let removed = RemovedPaneContextSnapshot {
            removed_at_ms: epoch_ms(),
            final_snapshot: tracker.snapshot(),
        };
        self.removed_panes.push_back(removed);
        while self.removed_panes.len() > MAX_REMOVED_PANE_SNAPSHOTS {
            self.removed_panes.pop_front();
        }
        Some(tracker)
    }

    /// Number of tracked panes.
    pub fn tracked_count(&self) -> usize {
        self.trackers.len()
    }

    /// Number of removed-pane final snapshots retained for audit evidence.
    pub fn removed_pane_count(&self) -> usize {
        self.removed_panes.len()
    }

    /// Fleet-wide context budget snapshot.
    pub fn fleet_snapshot(&self) -> ContextBudgetSnapshot {
        let now_ms = epoch_ms();
        let pane_snapshots: Vec<PaneContextSnapshot> =
            self.trackers.values().map(|t| t.snapshot()).collect();
        let removed_panes: Vec<RemovedPaneContextSnapshot> =
            self.removed_panes.iter().cloned().collect();

        let worst_active_tier = pane_snapshots
            .iter()
            .map(|s| s.pressure_tier)
            .max()
            .unwrap_or_default();
        let worst_removed_tier = removed_panes
            .iter()
            .map(|removed| removed.final_snapshot.pressure_tier)
            .max()
            .unwrap_or_default();
        let worst_tier = worst_active_tier.max(worst_removed_tier);

        let panes_needing_attention = pane_snapshots
            .iter()
            .filter(|s| s.pressure_tier.needs_attention())
            .count()
            + removed_panes
                .iter()
                .filter(|removed| removed.final_snapshot.pressure_tier.needs_attention())
                .count();

        let total_compactions: u64 = pane_snapshots
            .iter()
            .map(|s| s.total_compactions)
            .sum::<u64>()
            + removed_panes
                .iter()
                .map(|removed| removed.final_snapshot.total_compactions)
                .sum::<u64>();

        let utilization_count = pane_snapshots.len() + removed_panes.len();
        let utilization_sum = pane_snapshots.iter().map(|s| s.utilization).sum::<f64>()
            + removed_panes
                .iter()
                .map(|removed| removed.final_snapshot.utilization)
                .sum::<f64>();
        let avg_utilization = if utilization_count == 0 {
            0.0
        } else {
            utilization_sum / utilization_count as f64
        };

        ContextBudgetSnapshot {
            captured_at_ms: now_ms,
            tracked_panes: pane_snapshots.len(),
            removed_panes: removed_panes.len(),
            worst_pressure_tier: worst_tier,
            panes_needing_attention,
            total_compactions,
            average_utilization: avg_utilization,
            panes: pane_snapshots,
            removed_pane_snapshots: removed_panes,
        }
    }
}

/// Fleet-wide context budget snapshot.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextBudgetSnapshot {
    pub captured_at_ms: u64,
    pub tracked_panes: usize,
    #[serde(default)]
    pub removed_panes: usize,
    pub worst_pressure_tier: ContextPressureTier,
    pub panes_needing_attention: usize,
    pub total_compactions: u64,
    pub average_utilization: f64,
    pub panes: Vec<PaneContextSnapshot>,
    #[serde(default)]
    pub removed_pane_snapshots: Vec<RemovedPaneContextSnapshot>,
}

impl ContextBudgetSnapshot {
    /// Compact summary line for status bars.
    pub fn summary_line(&self) -> String {
        format!(
            "Context: {:?} | {}/{} panes need attention | avg {:.0}% | {} compactions",
            self.worst_pressure_tier,
            self.panes_needing_attention,
            self.tracked_panes,
            self.average_utilization * 100.0,
            self.total_compactions,
        )
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn epoch_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn config_100k() -> ContextBudgetConfig {
        ContextBudgetConfig {
            max_tokens: 100_000,
            max_compaction_history: 10,
        }
    }

    // -- ContextPressureTier --

    #[test]
    fn pressure_tier_from_utilization() {
        assert_eq!(
            ContextPressureTier::from_utilization(0.0),
            ContextPressureTier::Green
        );
        assert_eq!(
            ContextPressureTier::from_utilization(0.49),
            ContextPressureTier::Green
        );
        assert_eq!(
            ContextPressureTier::from_utilization(0.50),
            ContextPressureTier::Yellow
        );
        assert_eq!(
            ContextPressureTier::from_utilization(0.74),
            ContextPressureTier::Yellow
        );
        assert_eq!(
            ContextPressureTier::from_utilization(0.75),
            ContextPressureTier::Red
        );
        assert_eq!(
            ContextPressureTier::from_utilization(0.89),
            ContextPressureTier::Red
        );
        assert_eq!(
            ContextPressureTier::from_utilization(0.90),
            ContextPressureTier::Black
        );
        assert_eq!(
            ContextPressureTier::from_utilization(1.0),
            ContextPressureTier::Black
        );
    }

    #[test]
    fn pressure_tier_needs_attention() {
        assert!(!ContextPressureTier::Green.needs_attention());
        assert!(!ContextPressureTier::Yellow.needs_attention());
        assert!(ContextPressureTier::Red.needs_attention());
        assert!(ContextPressureTier::Black.needs_attention());
    }

    #[test]
    fn pressure_tier_ordering() {
        assert!(ContextPressureTier::Green < ContextPressureTier::Yellow);
        assert!(ContextPressureTier::Yellow < ContextPressureTier::Red);
        assert!(ContextPressureTier::Red < ContextPressureTier::Black);
    }

    // -- ContextBudgetTracker --

    #[test]
    fn tracker_starts_green() {
        let tracker = ContextBudgetTracker::new(0, config_100k());
        assert_eq!(tracker.pressure_tier(), ContextPressureTier::Green);
        assert_eq!(tracker.estimated_tokens, 0);
        assert_eq!(tracker.peak_tokens, 0);
    }

    #[test]
    fn tracker_update_tokens() {
        let mut tracker = ContextBudgetTracker::new(0, config_100k());
        tracker.update_tokens(60_000);
        assert_eq!(tracker.pressure_tier(), ContextPressureTier::Yellow);
        assert!((tracker.utilization() - 0.6).abs() < 0.001);

        tracker.update_tokens(80_000);
        assert_eq!(tracker.pressure_tier(), ContextPressureTier::Red);
        assert_eq!(tracker.peak_tokens, 80_000);

        // Token count goes down (after compaction)
        tracker.update_tokens(30_000);
        assert_eq!(tracker.pressure_tier(), ContextPressureTier::Green);
        assert_eq!(tracker.peak_tokens, 80_000); // Peak preserved
    }

    #[test]
    fn tracker_record_compaction() {
        let mut tracker = ContextBudgetTracker::new(1, config_100k());
        tracker.update_tokens(90_000);
        assert_eq!(tracker.pressure_tier(), ContextPressureTier::Black);

        tracker.record_compaction(90_000, 40_000, CompactionTrigger::Automatic);
        assert_eq!(tracker.estimated_tokens, 40_000);
        assert_eq!(tracker.total_compactions, 1);
        assert_eq!(tracker.compactions.len(), 1);
        assert_eq!(tracker.pressure_tier(), ContextPressureTier::Green);
        // br-ft-ps09a: peak preserved (90_000 was the high-water
        // mark before compaction).
        assert_eq!(tracker.peak_tokens, 90_000);
    }

    /// br-ft-ps09a: a fresh tracker whose first observation is a
    /// compaction event (the pattern engine detected the
    /// compaction marker before any `update_tokens` call) MUST
    /// record `peak_tokens = max(tokens_before, tokens_after)`.
    /// Pre-fix this returned `peak_tokens = 0` while
    /// `total_compactions = 1`, losing high-water evidence.
    #[test]
    fn tracker_record_compaction_first_observation_sets_peak() {
        let mut tracker = ContextBudgetTracker::new(1, config_100k());
        // Fresh tracker — no prior update_tokens call.
        assert_eq!(tracker.peak_tokens, 0);
        assert_eq!(tracker.total_compactions, 0);

        tracker.record_compaction(90_000, 40_000, CompactionTrigger::Automatic);

        assert_eq!(tracker.total_compactions, 1);
        // Bug pre-fix: peak_tokens stayed at 0. Post-fix: equals
        // tokens_before (the high-water observation).
        assert_eq!(
            tracker.peak_tokens, 90_000,
            "peak_tokens must reflect tokens_before for first-observation compactions"
        );
    }

    /// br-ft-ps09a: repeated compactions preserve the maximum
    /// observed `tokens_before` across all events. Pinning that
    /// the dedup over `max(...)` works even when later
    /// compactions report SMALLER values.
    #[test]
    fn tracker_record_compaction_repeated_preserves_max() {
        let mut tracker = ContextBudgetTracker::new(1, config_100k());
        tracker.record_compaction(85_000, 30_000, CompactionTrigger::Automatic);
        assert_eq!(tracker.peak_tokens, 85_000);

        // Lower-pressure compaction must not overwrite the peak.
        tracker.record_compaction(50_000, 20_000, CompactionTrigger::Automatic);
        assert_eq!(tracker.peak_tokens, 85_000);

        // Higher-pressure compaction lifts the peak.
        tracker.record_compaction(95_000, 35_000, CompactionTrigger::Automatic);
        assert_eq!(tracker.peak_tokens, 95_000);

        // Unusually-shaped event where after > before still
        // contributes its max to the high-water mark.
        tracker.record_compaction(20_000, 99_000, CompactionTrigger::Automatic);
        assert_eq!(tracker.peak_tokens, 99_000);
    }

    #[test]
    fn tracker_record_compaction_marks_increasing_estimates_as_anomalous() {
        let mut tracker = ContextBudgetTracker::new(1, config_100k());

        tracker.record_compaction(40_000, 80_000, CompactionTrigger::PatternDetected);

        let event = tracker.compactions.back().unwrap();
        assert_eq!(event.tokens_before, 40_000);
        assert_eq!(event.tokens_after, 80_000);
        assert_eq!(
            event.estimate_anomaly,
            Some(CompactionEstimateAnomaly::TokensIncreased)
        );
        assert_eq!(
            tracker.snapshot().recent_compactions[0].estimate_anomaly,
            event.estimate_anomaly
        );
    }

    #[test]
    fn tracker_record_compaction_normal_estimates_have_no_anomaly() {
        let mut tracker = ContextBudgetTracker::new(1, config_100k());

        tracker.record_compaction(80_000, 40_000, CompactionTrigger::Automatic);
        assert_eq!(tracker.compactions.back().unwrap().estimate_anomaly, None);

        tracker.record_compaction(40_000, 40_000, CompactionTrigger::Automatic);
        assert_eq!(tracker.compactions.back().unwrap().estimate_anomaly, None);
    }

    proptest! {
        #[test]
        fn tracker_compaction_anomaly_matches_before_after_order(
            tokens_before in any::<u64>(),
            tokens_after in any::<u64>(),
        ) {
            let mut tracker = ContextBudgetTracker::new(1, config_100k());

            tracker.record_compaction(
                tokens_before,
                tokens_after,
                CompactionTrigger::PatternDetected,
            );

            let event = tracker.compactions.back().unwrap();
            let expected = (tokens_after > tokens_before)
                .then_some(CompactionEstimateAnomaly::TokensIncreased);
            prop_assert_eq!(event.estimate_anomaly, expected);
            prop_assert_eq!(
                tracker.snapshot().recent_compactions[0].estimate_anomaly,
                expected
            );
        }
    }

    #[test]
    fn tracker_compaction_history_eviction() {
        let config = ContextBudgetConfig {
            max_tokens: 100_000,
            max_compaction_history: 3,
        };
        let mut tracker = ContextBudgetTracker::new(0, config);

        for i in 0..5 {
            tracker.record_compaction(50_000 + i, 20_000, CompactionTrigger::Automatic);
            assert!(tracker.compactions.len() <= 3, "iteration {i}");
        }
        assert_eq!(tracker.total_compactions, 5);
        assert_eq!(tracker.compactions.len(), 3);
        let retained_tokens_before: Vec<u64> = tracker
            .compactions
            .iter()
            .map(|event| event.tokens_before)
            .collect();
        assert_eq!(
            retained_tokens_before,
            vec![50_002, 50_003, 50_004],
            "ft-znj5k: context-budget FIFO eviction must retain oldest-to-newest order"
        );
    }

    #[test]
    fn tracker_recovery_guidance_green() {
        let tracker = ContextBudgetTracker::new(0, config_100k());
        let guidance = tracker.recovery_guidance();
        assert_eq!(guidance.action, RecoveryAction::Monitor);
        assert!(guidance.estimated_freed_tokens.is_none());
    }

    #[test]
    fn tracker_recovery_guidance_red() {
        let mut tracker = ContextBudgetTracker::new(0, config_100k());
        tracker.update_tokens(80_000);
        let guidance = tracker.recovery_guidance();
        assert_eq!(guidance.action, RecoveryAction::SendCompact);
        assert!(guidance.estimated_freed_tokens.is_some());
    }

    #[test]
    fn tracker_recovery_guidance_black() {
        let mut tracker = ContextBudgetTracker::new(0, config_100k());
        tracker.update_tokens(95_000);
        let guidance = tracker.recovery_guidance();
        assert_eq!(guidance.action, RecoveryAction::RotateSession);
    }

    #[test]
    fn tracker_snapshot() {
        let mut tracker = ContextBudgetTracker::new(42, config_100k());
        tracker.set_agent_program("claude-code");
        tracker.update_tokens(55_000);
        tracker.record_compaction(55_000, 25_000, CompactionTrigger::OperatorInitiated);

        let snap = tracker.snapshot();
        assert_eq!(snap.pane_id, 42);
        assert_eq!(snap.agent_program.as_deref(), Some("claude-code"));
        assert_eq!(snap.estimated_tokens, 25_000);
        assert_eq!(snap.peak_tokens, 55_000);
        assert_eq!(snap.total_compactions, 1);
        assert!(!snap.recent_compactions.is_empty());
    }

    #[test]
    fn tracker_zero_max_tokens() {
        let config = ContextBudgetConfig {
            max_tokens: 0,
            max_compaction_history: 10,
        };
        let mut tracker = ContextBudgetTracker::new(0, config);
        assert_eq!(tracker.utilization(), 0.0);
        assert_eq!(tracker.pressure_tier(), ContextPressureTier::Green);

        tracker.update_tokens(1);
        assert_eq!(tracker.utilization(), 1.0);
        assert_eq!(tracker.pressure_tier(), ContextPressureTier::Black);
        assert!(tracker.pressure_tier().needs_attention());
    }

    proptest! {
        #[test]
        fn tracker_utilization_handles_zero_and_extreme_limits(
            max_tokens in any::<u64>(),
            estimated_tokens in any::<u64>(),
        ) {
            let config = ContextBudgetConfig {
                max_tokens,
                max_compaction_history: 10,
            };
            let mut tracker = ContextBudgetTracker::new(0, config);
            tracker.update_tokens(estimated_tokens);

            let utilization = tracker.utilization();
            prop_assert!(
                utilization.is_finite(),
                "utilization should stay finite for max_tokens={max_tokens} estimated_tokens={estimated_tokens}"
            );
            prop_assert!(
                (0.0..=1.0).contains(&utilization),
                "utilization should stay bounded for max_tokens={max_tokens} estimated_tokens={estimated_tokens}: {utilization}"
            );

            if max_tokens == 0 && estimated_tokens > 0 {
                prop_assert_eq!(utilization, 1.0);
                prop_assert_eq!(tracker.pressure_tier(), ContextPressureTier::Black);
            } else if max_tokens == 0 {
                prop_assert_eq!(utilization, 0.0);
                prop_assert_eq!(tracker.pressure_tier(), ContextPressureTier::Green);
            }
        }
    }

    // -- ContextBudgetRegistry --

    #[test]
    fn registry_tracks_multiple_panes() {
        let mut registry = ContextBudgetRegistry::new(config_100k());

        registry.tracker_mut(0).update_tokens(20_000);
        registry.tracker_mut(1).update_tokens(80_000);
        registry.tracker_mut(2).update_tokens(50_000);

        assert_eq!(registry.tracked_count(), 3);

        let snap = registry.fleet_snapshot();
        assert_eq!(snap.tracked_panes, 3);
        assert_eq!(snap.worst_pressure_tier, ContextPressureTier::Red);
        assert_eq!(snap.panes_needing_attention, 1); // pane 1 (80% = Red)
    }

    #[test]
    fn registry_remove_pane() {
        let mut registry = ContextBudgetRegistry::new(config_100k());
        registry.tracker_mut(0).update_tokens(50_000);
        assert_eq!(registry.tracked_count(), 1);

        let removed = registry.remove(0);
        assert!(removed.is_some());
        assert_eq!(registry.tracked_count(), 0);
        assert_eq!(registry.removed_pane_count(), 1);
    }

    #[test]
    fn registry_remove_pane_retains_final_snapshot_evidence() {
        let mut registry = ContextBudgetRegistry::new(config_100k());
        {
            let tracker = registry.tracker_mut(9);
            tracker.update_tokens(95_000);
            tracker.record_compaction(95_000, 35_000, CompactionTrigger::Automatic);
            tracker.update_tokens(90_000);
        }

        let removed = registry.remove(9).expect("pane should be removed");
        assert_eq!(removed.total_compactions, 1);

        let snap = registry.fleet_snapshot();
        assert_eq!(snap.tracked_panes, 0);
        assert_eq!(snap.removed_panes, 1);
        assert_eq!(snap.total_compactions, 1);
        assert_eq!(snap.worst_pressure_tier, ContextPressureTier::Black);
        assert_eq!(snap.panes_needing_attention, 1);
        assert_eq!(snap.removed_pane_snapshots.len(), 1);
        assert_eq!(snap.removed_pane_snapshots[0].final_snapshot.pane_id, 9);
        assert_eq!(
            snap.removed_pane_snapshots[0]
                .final_snapshot
                .total_compactions,
            1
        );
    }

    proptest! {
        #[test]
        fn registry_removed_pane_snapshot_preserves_compaction_evidence(
            pane_id in any::<u64>(),
            tokens_before in 1_u64..=200_000,
            tokens_after in 0_u64..=200_000,
        ) {
            let mut registry = ContextBudgetRegistry::new(config_100k());
            registry
                .tracker_mut(pane_id)
                .record_compaction(tokens_before, tokens_after, CompactionTrigger::OperatorInitiated);

            let removed = registry.remove(pane_id);
            prop_assert!(removed.is_some());
            prop_assert_eq!(registry.tracked_count(), 0);

            let snap = registry.fleet_snapshot();
            prop_assert_eq!(snap.removed_panes, 1);
            prop_assert_eq!(snap.removed_pane_snapshots.len(), 1);
            prop_assert_eq!(snap.total_compactions, 1);
            prop_assert_eq!(
                snap.removed_pane_snapshots[0]
                    .final_snapshot
                    .total_compactions,
                1
            );
            prop_assert_eq!(
                snap.removed_pane_snapshots[0]
                    .final_snapshot
                    .recent_compactions
                    .len(),
                1
            );
            prop_assert_eq!(
                snap.removed_pane_snapshots[0]
                    .final_snapshot
                    .recent_compactions[0]
                    .tokens_before,
                tokens_before
            );
        }
    }

    #[test]
    fn registry_fleet_snapshot_empty() {
        let registry = ContextBudgetRegistry::new(config_100k());
        let snap = registry.fleet_snapshot();
        assert_eq!(snap.tracked_panes, 0);
        assert_eq!(snap.worst_pressure_tier, ContextPressureTier::Green);
        assert_eq!(snap.average_utilization, 0.0);
    }

    #[test]
    fn registry_fleet_snapshot_average_utilization() {
        let mut registry = ContextBudgetRegistry::new(config_100k());
        registry.tracker_mut(0).update_tokens(50_000); // 50%
        registry.tracker_mut(1).update_tokens(70_000); // 70%

        let snap = registry.fleet_snapshot();
        assert!((snap.average_utilization - 0.6).abs() < 0.001);
    }

    #[test]
    fn registry_zero_default_config_fails_closed_after_observed_tokens() {
        let mut registry = ContextBudgetRegistry::new(ContextBudgetConfig {
            max_tokens: 0,
            max_compaction_history: 10,
        });
        registry.tracker_mut(7).update_tokens(42);

        let snap = registry.fleet_snapshot();
        assert_eq!(snap.worst_pressure_tier, ContextPressureTier::Black);
        assert_eq!(snap.panes_needing_attention, 1);
        assert_eq!(snap.average_utilization, 1.0);
    }

    // -- Serde roundtrips --

    #[test]
    fn pane_snapshot_serde_roundtrip() {
        let mut tracker = ContextBudgetTracker::new(0, config_100k());
        tracker.update_tokens(75_000);
        let snap = tracker.snapshot();
        let json = serde_json::to_string(&snap).unwrap();
        let back: PaneContextSnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(back.pane_id, snap.pane_id);
        assert_eq!(back.estimated_tokens, snap.estimated_tokens);
        assert_eq!(back.pressure_tier, snap.pressure_tier);
    }

    #[test]
    fn fleet_snapshot_serde_roundtrip() {
        let mut registry = ContextBudgetRegistry::new(config_100k());
        registry.tracker_mut(0).update_tokens(60_000);
        let snap = registry.fleet_snapshot();
        let json = serde_json::to_string(&snap).unwrap();
        let back: ContextBudgetSnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(back.tracked_panes, snap.tracked_panes);
        assert_eq!(back.worst_pressure_tier, snap.worst_pressure_tier);
    }

    #[test]
    fn compaction_event_serde_roundtrip() {
        let event = CompactionEvent {
            detected_at_ms: 1_710_000_000_000,
            pane_id: 5,
            tokens_before: 90_000,
            tokens_after: 40_000,
            trigger: CompactionTrigger::OperatorInitiated,
            agent_program: Some("codex".into()),
            estimate_anomaly: None,
        };
        let json = serde_json::to_string(&event).unwrap();
        let back: CompactionEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(back.pane_id, 5);
        assert_eq!(back.trigger, CompactionTrigger::OperatorInitiated);
    }

    #[test]
    fn summary_line_format() {
        let mut registry = ContextBudgetRegistry::new(config_100k());
        registry.tracker_mut(0).update_tokens(80_000);
        registry.tracker_mut(1).update_tokens(30_000);
        let snap = registry.fleet_snapshot();
        let line = snap.summary_line();
        assert!(line.contains("Red"));
        assert!(line.contains("1/2 panes need attention"));
    }

    #[test]
    fn config_default() {
        let config = ContextBudgetConfig::default();
        assert_eq!(config.max_tokens, 200_000);
        assert_eq!(config.max_compaction_history, 100);
    }
}
