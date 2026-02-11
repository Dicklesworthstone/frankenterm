//! Tiered pane update rates — adaptive polling for idle and background panes.
//!
//! Classifies panes into activity tiers and provides appropriate polling
//! intervals for each tier. Active panes are polled frequently (200ms) while
//! dormant panes back off to 30s intervals, reducing mux protocol requests by
//! 80–90% for typical agent swarm workloads.
//!
//! # Tier system
//!
//! | Tier       | Interval | Trigger                              |
//! |------------|----------|--------------------------------------|
//! | Active     | 200ms    | Producing output                     |
//! | Thinking   | 2s       | Agent processing, no output yet      |
//! | Idle       | 5s       | No output for > idle_threshold       |
//! | Background | 10s      | Minimized/hidden tab                 |
//! | Dormant    | 30s      | Rate-limited or paused > 5 min       |
//!
//! Tiers interact with [`BackpressureTier`](crate::backpressure::BackpressureTier)
//! via multipliers: under system pressure, all intervals are scaled up.

use std::collections::HashMap;
use std::sync::RwLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::backpressure::BackpressureTier;

// =============================================================================
// Pane tier
// =============================================================================

/// Activity-based polling tier for a pane.
///
/// Ordered from most active (shortest interval) to least active (longest).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PaneTier {
    /// Producing output — poll every 200ms.
    Active,
    /// Agent is processing — poll every 2s.
    Thinking,
    /// No output for >30s — poll every 5s.
    Idle,
    /// Minimized or hidden tab — poll every 10s.
    Background,
    /// Rate-limited or paused >5min — poll every 30s.
    Dormant,
}

impl PaneTier {
    /// Default polling interval for this tier.
    #[must_use]
    pub fn default_interval(&self) -> Duration {
        match self {
            Self::Active => Duration::from_millis(200),
            Self::Thinking => Duration::from_secs(2),
            Self::Idle => Duration::from_secs(5),
            Self::Background => Duration::from_secs(10),
            Self::Dormant => Duration::from_secs(30),
        }
    }

    /// Backpressure multiplier for effective interval calculation.
    ///
    /// Under system load, all pane intervals are scaled up to reduce
    /// mux protocol requests.
    #[must_use]
    pub fn backpressure_multiplier(&self, bp: BackpressureTier) -> f64 {
        match bp {
            BackpressureTier::Green => 1.0,
            BackpressureTier::Yellow => 1.5,
            BackpressureTier::Red => 3.0,
            BackpressureTier::Black => 10.0,
        }
    }

    /// Effective polling interval under the given backpressure tier.
    #[must_use]
    pub fn effective_interval(&self, bp: BackpressureTier) -> Duration {
        let base = self.default_interval();
        let mult = self.backpressure_multiplier(bp);
        base.mul_f64(mult)
    }

    /// Numeric index for metrics (0=Active, 4=Dormant).
    #[must_use]
    pub fn as_u8(&self) -> u8 {
        match self {
            Self::Active => 0,
            Self::Thinking => 1,
            Self::Idle => 2,
            Self::Background => 3,
            Self::Dormant => 4,
        }
    }

    /// All tier variants in order.
    #[must_use]
    pub fn all() -> &'static [PaneTier] {
        &[
            Self::Active,
            Self::Thinking,
            Self::Idle,
            Self::Background,
            Self::Dormant,
        ]
    }
}

impl std::fmt::Display for PaneTier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Active => write!(f, "active"),
            Self::Thinking => write!(f, "thinking"),
            Self::Idle => write!(f, "idle"),
            Self::Background => write!(f, "background"),
            Self::Dormant => write!(f, "dormant"),
        }
    }
}

// =============================================================================
// Configuration
// =============================================================================

/// Configuration for pane tier classification and polling intervals.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct TierConfig {
    /// Polling interval for active panes (ms).
    pub active_ms: u64,
    /// Polling interval for thinking panes (ms).
    pub thinking_ms: u64,
    /// Polling interval for idle panes (ms).
    pub idle_ms: u64,
    /// Polling interval for background panes (ms).
    pub background_ms: u64,
    /// Polling interval for dormant panes (ms).
    pub dormant_ms: u64,

    /// Seconds of silence before a pane is classified as idle.
    pub idle_threshold_secs: u64,
    /// Seconds of silence before a pane is classified as dormant.
    pub dormant_threshold_secs: u64,
}

impl Default for TierConfig {
    fn default() -> Self {
        Self {
            active_ms: 200,
            thinking_ms: 2000,
            idle_ms: 5000,
            background_ms: 10000,
            dormant_ms: 30000,
            idle_threshold_secs: 30,
            dormant_threshold_secs: 300,
        }
    }
}

impl TierConfig {
    /// Get the configured interval for a tier.
    #[must_use]
    pub fn interval_for(&self, tier: PaneTier) -> Duration {
        match tier {
            PaneTier::Active => Duration::from_millis(self.active_ms),
            PaneTier::Thinking => Duration::from_millis(self.thinking_ms),
            PaneTier::Idle => Duration::from_millis(self.idle_ms),
            PaneTier::Background => Duration::from_millis(self.background_ms),
            PaneTier::Dormant => Duration::from_millis(self.dormant_ms),
        }
    }
}

// =============================================================================
// Pane state for classification
// =============================================================================

/// Per-pane tracking state used for tier classification.
#[derive(Debug, Clone)]
struct PaneState {
    /// Current tier.
    tier: PaneTier,
    /// Last time output was observed from this pane.
    last_output: Instant,
    /// Whether this pane is in a background/hidden tab.
    is_background: bool,
    /// Whether the pane's agent is rate-limited.
    is_rate_limited: bool,
    /// Whether the agent is in a thinking/processing state.
    is_thinking: bool,
    /// Number of tier transitions for this pane.
    transitions: u64,
}

impl PaneState {
    fn new() -> Self {
        Self {
            tier: PaneTier::Active,
            last_output: Instant::now(),
            is_background: false,
            is_rate_limited: false,
            is_thinking: false,
            transitions: 0,
        }
    }
}

// =============================================================================
// Tier metrics
// =============================================================================

/// Aggregate metrics for the tier classification system.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TierMetrics {
    /// Number of panes in each tier.
    pub tier_counts: HashMap<String, u64>,
    /// Total tier transitions across all panes.
    pub total_transitions: u64,
    /// Total number of tracked panes.
    pub total_panes: u64,
    /// Estimated requests per second at current tier distribution.
    pub estimated_rps: f64,
}

// =============================================================================
// Tier classifier
// =============================================================================

/// Classifies panes into polling tiers based on activity.
///
/// Thread-safe: classification and state updates are protected by an internal
/// `RwLock`.
pub struct PaneTierClassifier {
    config: TierConfig,
    panes: RwLock<HashMap<u64, PaneState>>,
    total_transitions: AtomicU64,
    total_promotions: AtomicU64,
}

impl PaneTierClassifier {
    /// Create a new classifier with the given configuration.
    #[must_use]
    pub fn new(config: TierConfig) -> Self {
        Self {
            config,
            panes: RwLock::new(HashMap::new()),
            total_transitions: AtomicU64::new(0),
            total_promotions: AtomicU64::new(0),
        }
    }

    /// Register a new pane. Starts at Active tier.
    pub fn register_pane(&self, pane_id: u64) {
        let mut panes = self.panes.write().expect("pane lock poisoned");
        panes.entry(pane_id).or_insert_with(PaneState::new);
    }

    /// Remove a pane from tracking.
    pub fn unregister_pane(&self, pane_id: u64) {
        let mut panes = self.panes.write().expect("pane lock poisoned");
        panes.remove(&pane_id);
    }

    /// Notify that a pane produced output — instantly promotes to Active.
    pub fn on_pane_output(&self, pane_id: u64) {
        let mut panes = self.panes.write().expect("pane lock poisoned");
        if let Some(state) = panes.get_mut(&pane_id) {
            state.last_output = Instant::now();
            if state.tier != PaneTier::Active {
                state.tier = PaneTier::Active;
                state.transitions += 1;
                self.total_transitions.fetch_add(1, Ordering::Relaxed);
                self.total_promotions.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    /// Mark a pane as in a background/hidden tab.
    pub fn set_background(&self, pane_id: u64, is_background: bool) {
        let mut panes = self.panes.write().expect("pane lock poisoned");
        if let Some(state) = panes.get_mut(&pane_id) {
            state.is_background = is_background;
        }
    }

    /// Mark a pane as rate-limited.
    pub fn set_rate_limited(&self, pane_id: u64, is_rate_limited: bool) {
        let mut panes = self.panes.write().expect("pane lock poisoned");
        if let Some(state) = panes.get_mut(&pane_id) {
            state.is_rate_limited = is_rate_limited;
        }
    }

    /// Mark a pane as in thinking/processing state.
    pub fn set_thinking(&self, pane_id: u64, is_thinking: bool) {
        let mut panes = self.panes.write().expect("pane lock poisoned");
        if let Some(state) = panes.get_mut(&pane_id) {
            state.is_thinking = is_thinking;
        }
    }

    /// Classify a single pane based on current state.
    ///
    /// Updates the pane's tier and returns it. Records tier transitions.
    #[must_use]
    pub fn classify(&self, pane_id: u64) -> PaneTier {
        let mut panes = self.panes.write().expect("pane lock poisoned");
        let state = match panes.get_mut(&pane_id) {
            Some(s) => s,
            None => return PaneTier::Active, // unknown pane treated as active
        };

        let old_tier = state.tier;
        let new_tier = self.compute_tier(state);

        if new_tier != old_tier {
            state.tier = new_tier;
            state.transitions += 1;
            self.total_transitions.fetch_add(1, Ordering::Relaxed);
        }

        new_tier
    }

    /// Reclassify all panes. Returns the distribution of tiers.
    #[must_use]
    pub fn classify_all(&self) -> HashMap<u64, PaneTier> {
        let mut panes = self.panes.write().expect("pane lock poisoned");
        let mut result = HashMap::with_capacity(panes.len());

        for (&pane_id, state) in panes.iter_mut() {
            let old_tier = state.tier;
            let new_tier = self.compute_tier(state);

            if new_tier != old_tier {
                state.tier = new_tier;
                state.transitions += 1;
                self.total_transitions.fetch_add(1, Ordering::Relaxed);
            }

            result.insert(pane_id, new_tier);
        }

        result
    }

    /// Get the current tier of a pane without reclassifying.
    #[must_use]
    pub fn current_tier(&self, pane_id: u64) -> PaneTier {
        let panes = self.panes.read().expect("pane lock poisoned");
        panes
            .get(&pane_id)
            .map(|s| s.tier)
            .unwrap_or(PaneTier::Active)
    }

    /// Get the effective polling interval for a pane under given backpressure.
    #[must_use]
    pub fn effective_interval(&self, pane_id: u64, bp: BackpressureTier) -> Duration {
        let tier = self.current_tier(pane_id);
        let base = self.config.interval_for(tier);
        let mult = tier.backpressure_multiplier(bp);
        base.mul_f64(mult)
    }

    /// Number of tracked panes.
    #[must_use]
    pub fn pane_count(&self) -> usize {
        self.panes.read().expect("pane lock poisoned").len()
    }

    /// Aggregate metrics.
    #[must_use]
    pub fn metrics(&self) -> TierMetrics {
        let panes = self.panes.read().expect("pane lock poisoned");
        let mut tier_counts = HashMap::new();
        let mut estimated_rps = 0.0;

        for state in panes.values() {
            *tier_counts.entry(state.tier.to_string()).or_insert(0u64) += 1;
            // RPS contribution: 1 / interval_seconds
            let interval = self.config.interval_for(state.tier);
            estimated_rps += 1.0 / interval.as_secs_f64();
        }

        TierMetrics {
            tier_counts,
            total_transitions: self.total_transitions.load(Ordering::Relaxed),
            total_panes: panes.len() as u64,
            estimated_rps,
        }
    }

    /// Total promotions (instant upgrades to Active on output).
    #[must_use]
    pub fn total_promotions(&self) -> u64 {
        self.total_promotions.load(Ordering::Relaxed)
    }

    /// Configuration reference.
    #[must_use]
    pub fn config(&self) -> &TierConfig {
        &self.config
    }

    // ── Internal classification logic ───────────────────────────────────

    fn compute_tier(&self, state: &PaneState) -> PaneTier {
        // Rate-limited → Dormant
        if state.is_rate_limited {
            return PaneTier::Dormant;
        }

        // Background tab → Background
        if state.is_background {
            return PaneTier::Background;
        }

        // Thinking/processing state → Thinking
        if state.is_thinking {
            return PaneTier::Thinking;
        }

        // Time-based classification
        let elapsed = state.last_output.elapsed();
        let idle_threshold = Duration::from_secs(self.config.idle_threshold_secs);
        let dormant_threshold = Duration::from_secs(self.config.dormant_threshold_secs);

        if elapsed >= dormant_threshold {
            PaneTier::Dormant
        } else if elapsed >= idle_threshold {
            PaneTier::Idle
        } else {
            PaneTier::Active
        }
    }
}

impl std::fmt::Debug for PaneTierClassifier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PaneTierClassifier")
            .field("config", &self.config)
            .field("pane_count", &self.pane_count())
            .field(
                "total_transitions",
                &self.total_transitions.load(Ordering::Relaxed),
            )
            .finish()
    }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // -- PaneTier basics -------------------------------------------------------

    #[test]
    fn tier_ordering() {
        assert!(PaneTier::Active < PaneTier::Thinking);
        assert!(PaneTier::Thinking < PaneTier::Idle);
        assert!(PaneTier::Idle < PaneTier::Background);
        assert!(PaneTier::Background < PaneTier::Dormant);
    }

    #[test]
    fn tier_intervals_increase() {
        let tiers = PaneTier::all();
        for i in 1..tiers.len() {
            assert!(
                tiers[i].default_interval() > tiers[i - 1].default_interval(),
                "{} interval should be > {} interval",
                tiers[i],
                tiers[i - 1]
            );
        }
    }

    #[test]
    fn tier_display() {
        assert_eq!(PaneTier::Active.to_string(), "active");
        assert_eq!(PaneTier::Thinking.to_string(), "thinking");
        assert_eq!(PaneTier::Idle.to_string(), "idle");
        assert_eq!(PaneTier::Background.to_string(), "background");
        assert_eq!(PaneTier::Dormant.to_string(), "dormant");
    }

    #[test]
    fn tier_as_u8() {
        assert_eq!(PaneTier::Active.as_u8(), 0);
        assert_eq!(PaneTier::Dormant.as_u8(), 4);
    }

    #[test]
    fn tier_serde_roundtrip() {
        for tier in PaneTier::all() {
            let json = serde_json::to_string(tier).unwrap();
            let back: PaneTier = serde_json::from_str(&json).unwrap();
            assert_eq!(*tier, back);
        }
    }

    // -- Backpressure integration ----------------------------------------------

    #[test]
    fn backpressure_multiplier_monotonic() {
        for tier in PaneTier::all() {
            let m_g = tier.backpressure_multiplier(BackpressureTier::Green);
            let m_y = tier.backpressure_multiplier(BackpressureTier::Yellow);
            let m_r = tier.backpressure_multiplier(BackpressureTier::Red);
            let m_b = tier.backpressure_multiplier(BackpressureTier::Black);
            assert!(m_g <= m_y, "{tier}: Green={m_g} <= Yellow={m_y}");
            assert!(m_y <= m_r, "{tier}: Yellow={m_y} <= Red={m_r}");
            assert!(m_r <= m_b, "{tier}: Red={m_r} <= Black={m_b}");
        }
    }

    #[test]
    fn effective_interval_under_pressure() {
        let base = PaneTier::Active.default_interval();
        let green = PaneTier::Active.effective_interval(BackpressureTier::Green);
        let red = PaneTier::Active.effective_interval(BackpressureTier::Red);
        let black = PaneTier::Active.effective_interval(BackpressureTier::Black);

        assert_eq!(green, base);
        assert!(red > green);
        assert!(black > red);
    }

    // -- Configuration ---------------------------------------------------------

    #[test]
    fn config_defaults() {
        let config = TierConfig::default();
        assert_eq!(config.active_ms, 200);
        assert_eq!(config.thinking_ms, 2000);
        assert_eq!(config.idle_ms, 5000);
        assert_eq!(config.background_ms, 10000);
        assert_eq!(config.dormant_ms, 30000);
        assert_eq!(config.idle_threshold_secs, 30);
        assert_eq!(config.dormant_threshold_secs, 300);
    }

    #[test]
    fn config_interval_for() {
        let config = TierConfig::default();
        assert_eq!(
            config.interval_for(PaneTier::Active),
            Duration::from_millis(200)
        );
        assert_eq!(
            config.interval_for(PaneTier::Dormant),
            Duration::from_millis(30000)
        );
    }

    #[test]
    fn config_serde_roundtrip() {
        let config = TierConfig::default();
        let json = serde_json::to_string(&config).unwrap();
        let back: TierConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(back.active_ms, 200);
        assert_eq!(back.dormant_threshold_secs, 300);
    }

    // -- Classifier lifecycle --------------------------------------------------

    #[test]
    fn register_and_unregister() {
        let clf = PaneTierClassifier::new(TierConfig::default());
        assert_eq!(clf.pane_count(), 0);

        clf.register_pane(1);
        clf.register_pane(2);
        assert_eq!(clf.pane_count(), 2);

        clf.unregister_pane(1);
        assert_eq!(clf.pane_count(), 1);

        clf.unregister_pane(2);
        assert_eq!(clf.pane_count(), 0);
    }

    #[test]
    fn new_pane_starts_active() {
        let clf = PaneTierClassifier::new(TierConfig::default());
        clf.register_pane(1);
        assert_eq!(clf.current_tier(1), PaneTier::Active);
    }

    #[test]
    fn unknown_pane_returns_active() {
        let clf = PaneTierClassifier::new(TierConfig::default());
        assert_eq!(clf.current_tier(999), PaneTier::Active);
        assert_eq!(clf.classify(999), PaneTier::Active);
    }

    // -- Classification logic --------------------------------------------------

    #[test]
    fn rate_limited_goes_dormant() {
        let clf = PaneTierClassifier::new(TierConfig::default());
        clf.register_pane(1);
        clf.set_rate_limited(1, true);
        assert_eq!(clf.classify(1), PaneTier::Dormant);
    }

    #[test]
    fn background_pane_classified() {
        let clf = PaneTierClassifier::new(TierConfig::default());
        clf.register_pane(1);
        clf.set_background(1, true);
        assert_eq!(clf.classify(1), PaneTier::Background);
    }

    #[test]
    fn thinking_pane_classified() {
        let clf = PaneTierClassifier::new(TierConfig::default());
        clf.register_pane(1);
        clf.set_thinking(1, true);
        assert_eq!(clf.classify(1), PaneTier::Thinking);
    }

    #[test]
    fn idle_after_threshold() {
        let clf = PaneTierClassifier::new(TierConfig {
            idle_threshold_secs: 0, // immediate idle
            dormant_threshold_secs: 300,
            ..Default::default()
        });
        clf.register_pane(1);

        // Manually set last_output to the past
        {
            let mut panes = clf.panes.write().unwrap();
            let state = panes.get_mut(&1).unwrap();
            state.last_output = Instant::now() - Duration::from_secs(1);
        }

        assert_eq!(clf.classify(1), PaneTier::Idle);
    }

    #[test]
    fn dormant_after_threshold() {
        let clf = PaneTierClassifier::new(TierConfig {
            idle_threshold_secs: 30,
            dormant_threshold_secs: 60,
            ..Default::default()
        });
        clf.register_pane(1);

        // Set last_output well past dormant threshold
        {
            let mut panes = clf.panes.write().unwrap();
            let state = panes.get_mut(&1).unwrap();
            state.last_output = Instant::now() - Duration::from_secs(120);
        }

        assert_eq!(clf.classify(1), PaneTier::Dormant);
    }

    // -- Instant promotion -----------------------------------------------------

    #[test]
    fn output_promotes_to_active() {
        let clf = PaneTierClassifier::new(TierConfig::default());
        clf.register_pane(1);

        // Force pane to Idle
        {
            let mut panes = clf.panes.write().unwrap();
            let state = panes.get_mut(&1).unwrap();
            state.tier = PaneTier::Idle;
            state.last_output = Instant::now() - Duration::from_secs(60);
        }
        assert_eq!(clf.current_tier(1), PaneTier::Idle);

        // Output event promotes immediately
        clf.on_pane_output(1);
        assert_eq!(clf.current_tier(1), PaneTier::Active);
        assert_eq!(clf.total_promotions(), 1);
    }

    #[test]
    fn promotion_from_dormant() {
        let clf = PaneTierClassifier::new(TierConfig::default());
        clf.register_pane(1);

        clf.set_rate_limited(1, true);
        clf.classify(1);
        assert_eq!(clf.current_tier(1), PaneTier::Dormant);

        // Clear rate limit and send output
        clf.set_rate_limited(1, false);
        clf.on_pane_output(1);
        assert_eq!(clf.current_tier(1), PaneTier::Active);
    }

    // -- classify_all ----------------------------------------------------------

    #[test]
    fn classify_all_panes() {
        let clf = PaneTierClassifier::new(TierConfig::default());
        clf.register_pane(1);
        clf.register_pane(2);
        clf.register_pane(3);

        clf.set_background(2, true);
        clf.set_rate_limited(3, true);

        let result = clf.classify_all();
        assert_eq!(result[&1], PaneTier::Active);
        assert_eq!(result[&2], PaneTier::Background);
        assert_eq!(result[&3], PaneTier::Dormant);
    }

    // -- Effective interval ----------------------------------------------------

    #[test]
    fn effective_interval_with_backpressure() {
        let clf = PaneTierClassifier::new(TierConfig::default());
        clf.register_pane(1);

        let green = clf.effective_interval(1, BackpressureTier::Green);
        let red = clf.effective_interval(1, BackpressureTier::Red);
        assert!(red > green);
    }

    // -- Metrics ---------------------------------------------------------------

    #[test]
    fn metrics_reflect_state() {
        let clf = PaneTierClassifier::new(TierConfig::default());
        clf.register_pane(1);
        clf.register_pane(2);
        clf.register_pane(3);

        clf.set_background(2, true);
        clf.classify_all();

        let m = clf.metrics();
        assert_eq!(m.total_panes, 3);
        assert!(m.estimated_rps > 0.0);
        // Should have active and background panes
        assert!(m.tier_counts.contains_key("active"));
        assert!(m.tier_counts.contains_key("background"));
    }

    #[test]
    fn metrics_transitions_counted() {
        let clf = PaneTierClassifier::new(TierConfig::default());
        clf.register_pane(1);

        // Force tier change
        clf.set_rate_limited(1, true);
        clf.classify(1); // Active → Dormant

        clf.set_rate_limited(1, false);
        clf.on_pane_output(1); // Dormant → Active

        let m = clf.metrics();
        assert!(m.total_transitions >= 2);
    }

    #[test]
    fn metrics_serde_roundtrip() {
        let m = TierMetrics {
            tier_counts: HashMap::from([("active".to_string(), 5), ("idle".to_string(), 3)]),
            total_transitions: 42,
            total_panes: 8,
            estimated_rps: 26.5,
        };
        let json = serde_json::to_string(&m).unwrap();
        let back: TierMetrics = serde_json::from_str(&json).unwrap();
        assert_eq!(back.total_panes, 8);
        assert_eq!(back.tier_counts["active"], 5);
    }

    // -- Priority ordering: rate_limited > background > thinking > time --------

    #[test]
    fn priority_order_rate_limited_wins() {
        let clf = PaneTierClassifier::new(TierConfig::default());
        clf.register_pane(1);

        clf.set_rate_limited(1, true);
        clf.set_background(1, true);
        clf.set_thinking(1, true);

        // rate_limited has highest priority → Dormant
        assert_eq!(clf.classify(1), PaneTier::Dormant);
    }

    #[test]
    fn priority_order_background_over_thinking() {
        let clf = PaneTierClassifier::new(TierConfig::default());
        clf.register_pane(1);

        clf.set_background(1, true);
        clf.set_thinking(1, true);

        // background wins over thinking
        assert_eq!(clf.classify(1), PaneTier::Background);
    }

    // -- Scale test: 200 panes -------------------------------------------------

    #[test]
    fn scale_200_panes() {
        let clf = PaneTierClassifier::new(TierConfig::default());

        for i in 0..200 {
            clf.register_pane(i);
        }
        assert_eq!(clf.pane_count(), 200);

        // Set various states
        for i in 0..50 {
            clf.set_rate_limited(i, true); // 50 dormant
        }
        for i in 50..100 {
            clf.set_background(i, true); // 50 background
        }
        for i in 100..130 {
            clf.set_thinking(i, true); // 30 thinking
        }
        // 130..200 remain active (70 active)

        let result = clf.classify_all();
        assert_eq!(result.len(), 200);

        let m = clf.metrics();
        assert_eq!(m.total_panes, 200);

        // Verify distribution
        assert_eq!(*m.tier_counts.get("dormant").unwrap_or(&0), 50);
        assert_eq!(*m.tier_counts.get("background").unwrap_or(&0), 50);
        assert_eq!(*m.tier_counts.get("thinking").unwrap_or(&0), 30);
        assert_eq!(*m.tier_counts.get("active").unwrap_or(&0), 70);

        // Estimated RPS should be much lower than 200 × 5 (uniform active)
        // 70 active × 5rps + 30 thinking × 0.5rps + 50 bg × 0.1rps + 50 dormant × 0.033rps
        // ≈ 350 + 15 + 5 + 1.67 ≈ 371.67
        assert!(m.estimated_rps < 400.0, "rps={}", m.estimated_rps);
        // But more than just the active panes
        assert!(m.estimated_rps > 300.0, "rps={}", m.estimated_rps);
    }

    // -- Thread safety ---------------------------------------------------------

    #[test]
    fn concurrent_classify() {
        let clf = std::sync::Arc::new(PaneTierClassifier::new(TierConfig::default()));

        for i in 0..50 {
            clf.register_pane(i);
        }

        let mut handles = vec![];
        for _ in 0..10 {
            let clf = std::sync::Arc::clone(&clf);
            handles.push(std::thread::spawn(move || {
                for i in 0..50 {
                    let _ = clf.classify(i);
                    if i % 5 == 0 {
                        clf.on_pane_output(i);
                    }
                }
            }));
        }

        for h in handles {
            h.join().unwrap();
        }

        // Should not have panicked and all panes still tracked
        assert_eq!(clf.pane_count(), 50);
    }
}
