//! Automatic restart scheduling for mux server restarts.
//!
//! Bead: wa-lr93
//!
//! Finds optimal restart windows by scoring candidate times based on:
//! - Hazard urgency: how urgently a restart is needed (from survival model)
//! - Activity minimum: prefer low-activity periods (from 24h EWMA profile)
//! - Recency penalty: avoid too-frequent restarts (cooldown enforcement)
//!
//! Score formula: `score(t) = hazard_urgency(t) × activity_minimum(t) × recency_penalty(t)`

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// Restart scheduling mode.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RestartMode {
    /// Fully automatic — restart when score exceeds threshold.
    Automatic { min_score: f64 },
    /// Suggest only — alert user with recommended window.
    Advisory,
    /// Disabled — manual restarts only.
    Manual,
}

impl Default for RestartMode {
    fn default() -> Self {
        Self::Advisory
    }
}

/// Configuration for the restart scheduler.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RestartSchedulerConfig {
    /// Scheduling mode.
    #[serde(default)]
    pub mode: RestartMode,
    /// Minimum score threshold for automatic restart (overridden by mode if Automatic).
    #[serde(default = "default_min_score")]
    pub min_score_threshold: f64,
    /// Minimum hours between restarts.
    #[serde(default = "default_cooldown_hours")]
    pub cooldown_hours: f64,
    /// Minutes of advance warning before restart.
    #[serde(default = "default_advance_warning")]
    pub advance_warning_minutes: u32,
    /// Whether to take a full snapshot before restart.
    #[serde(default = "default_true")]
    pub pre_restart_snapshot: bool,
    /// Hazard rate threshold above which urgency ramps up.
    #[serde(default = "default_hazard_threshold")]
    pub hazard_threshold: f64,
    /// Window size in minutes for scoring granularity.
    #[serde(default = "default_window_minutes")]
    pub window_minutes: u32,
    /// How many hours ahead to scan for optimal windows.
    #[serde(default = "default_lookahead_hours")]
    pub lookahead_hours: u32,
}

fn default_min_score() -> f64 {
    0.7
}
fn default_cooldown_hours() -> f64 {
    12.0
}
fn default_advance_warning() -> u32 {
    30
}
fn default_true() -> bool {
    true
}
fn default_hazard_threshold() -> f64 {
    0.5
}
fn default_window_minutes() -> u32 {
    1
}
fn default_lookahead_hours() -> u32 {
    24
}

impl Default for RestartSchedulerConfig {
    fn default() -> Self {
        Self {
            mode: RestartMode::default(),
            min_score_threshold: default_min_score(),
            cooldown_hours: default_cooldown_hours(),
            advance_warning_minutes: default_advance_warning(),
            pre_restart_snapshot: true,
            hazard_threshold: default_hazard_threshold(),
            window_minutes: default_window_minutes(),
            lookahead_hours: default_lookahead_hours(),
        }
    }
}

// ---------------------------------------------------------------------------
// Activity profile (24-hour EWMA)
// ---------------------------------------------------------------------------

/// 24-hour activity profile using EWMA per hour-of-day.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActivityProfile {
    /// EWMA activity level per hour (0..24).
    hourly_activity: [f64; 24],
    /// EWMA smoothing factor (0..1). Higher = more weight on recent.
    alpha: f64,
    /// Total observations incorporated.
    observations: u64,
}

impl ActivityProfile {
    /// Create a new activity profile with uniform activity.
    #[must_use]
    pub fn new(alpha: f64) -> Self {
        Self {
            hourly_activity: [0.5; 24],
            alpha: alpha.clamp(0.0, 1.0),
            observations: 0,
        }
    }

    /// Update the profile with a new activity observation.
    ///
    /// `hour` is 0..24, `activity` is 0.0..1.0 normalized.
    pub fn update(&mut self, hour: u32, activity: f64) {
        let idx = (hour % 24) as usize;
        let a = self.alpha;
        self.hourly_activity[idx] = (1.0 - a).mul_add(self.hourly_activity[idx], a * activity);
        self.observations += 1;
    }

    /// Predict activity at a given hour-of-day.
    #[must_use]
    pub fn predict(&self, hour: u32) -> f64 {
        self.hourly_activity[(hour % 24) as usize]
    }

    /// Find the hour with minimum predicted activity.
    #[must_use]
    pub fn min_activity_hour(&self) -> u32 {
        self.hourly_activity
            .iter()
            .enumerate()
            .min_by(|(_, a), (_, b)| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal))
            .map(|(i, _)| i as u32)
            .unwrap_or(3) // default: 3am
    }

    /// Total observations incorporated.
    #[must_use]
    pub fn observations(&self) -> u64 {
        self.observations
    }

    /// Get the full 24-hour profile.
    #[must_use]
    pub fn hourly_profile(&self) -> &[f64; 24] {
        &self.hourly_activity
    }
}

impl Default for ActivityProfile {
    fn default() -> Self {
        Self::new(0.3)
    }
}

// ---------------------------------------------------------------------------
// Scoring
// ---------------------------------------------------------------------------

/// A candidate restart window with its computed score.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScoredWindow {
    /// Minutes from now.
    pub offset_minutes: u32,
    /// Hour of day this window falls in.
    pub hour_of_day: u32,
    /// Hazard urgency component (0..1).
    pub hazard_urgency: f64,
    /// Activity minimum component (0..1).
    pub activity_minimum: f64,
    /// Recency penalty component (0..1).
    pub recency_penalty: f64,
    /// Combined score.
    pub score: f64,
}

/// Compute hazard urgency using sigmoid function.
///
/// Returns a value in (0, 1) that ramps up as hazard_rate exceeds threshold.
#[must_use]
pub fn hazard_urgency(hazard_rate: f64, threshold: f64) -> f64 {
    let x = 10.0 * (hazard_rate - threshold);
    1.0 / (1.0 + (-x).exp())
}

/// Compute activity minimum score: 1 - activity.
///
/// Low activity → high score (good time to restart).
#[must_use]
pub fn activity_minimum(activity: f64) -> f64 {
    (1.0 - activity).clamp(0.0, 1.0)
}

/// Compute recency penalty: penalize if too soon after last restart.
///
/// Returns a value in (0, 1) that increases as more time passes since last restart.
#[must_use]
pub fn recency_penalty(hours_since_last: f64, cooldown_hours: f64) -> f64 {
    if cooldown_hours <= 0.0 {
        return 1.0;
    }
    1.0 - (-hours_since_last / cooldown_hours).exp()
}

/// Score a single candidate window.
#[must_use]
pub fn score_window(
    hazard_rate: f64,
    activity: f64,
    hours_since_last: f64,
    config: &RestartSchedulerConfig,
) -> f64 {
    let hu = hazard_urgency(hazard_rate, config.hazard_threshold);
    let am = activity_minimum(activity);
    let rp = recency_penalty(hours_since_last, config.cooldown_hours);
    hu * am * rp
}

// ---------------------------------------------------------------------------
// Scheduler
// ---------------------------------------------------------------------------

/// The restart scheduler that finds optimal restart windows.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RestartScheduler {
    config: RestartSchedulerConfig,
    activity_profile: ActivityProfile,
    /// Timestamp (ms) of last restart, or None if never restarted.
    last_restart_ms: Option<i64>,
    /// Currently scheduled restart, if any.
    scheduled: Option<ScheduledRestart>,
}

/// A scheduled restart with timing and metadata.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScheduledRestart {
    /// When the restart is scheduled (ms since epoch).
    pub scheduled_at_ms: i64,
    /// Score at time of scheduling.
    pub score: f64,
    /// Whether user was notified.
    pub notified: bool,
    /// Whether pre-restart snapshot was taken.
    pub snapshot_taken: bool,
}

/// Result of evaluating restart candidates.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SchedulingDecision {
    /// All scored windows, sorted by score descending.
    pub windows: Vec<ScoredWindow>,
    /// The recommended window (highest score), if any meet threshold.
    pub recommendation: Option<ScoredWindow>,
    /// Whether automatic restart would be triggered.
    pub would_trigger: bool,
}

impl RestartScheduler {
    /// Create a new scheduler with the given config.
    #[must_use]
    pub fn new(config: RestartSchedulerConfig) -> Self {
        Self {
            config,
            activity_profile: ActivityProfile::default(),
            last_restart_ms: None,
            scheduled: None,
        }
    }

    /// Record that a restart occurred at the given timestamp.
    pub fn record_restart(&mut self, timestamp_ms: i64) {
        self.last_restart_ms = Some(timestamp_ms);
        self.scheduled = None;
    }

    /// Update the activity profile with a new observation.
    pub fn update_activity(&mut self, hour: u32, activity: f64) {
        self.activity_profile.update(hour, activity);
    }

    /// Get the current scheduling mode.
    #[must_use]
    pub fn mode(&self) -> &RestartMode {
        &self.config.mode
    }

    /// Get the activity profile.
    #[must_use]
    pub fn activity_profile(&self) -> &ActivityProfile {
        &self.activity_profile
    }

    /// Get the currently scheduled restart, if any.
    #[must_use]
    pub fn scheduled(&self) -> Option<&ScheduledRestart> {
        self.scheduled.as_ref()
    }

    /// Get config.
    #[must_use]
    pub fn config(&self) -> &RestartSchedulerConfig {
        &self.config
    }

    /// Evaluate all candidate windows and return a scheduling decision.
    ///
    /// `current_ms` is the current time in milliseconds since epoch.
    /// `current_hour` is the current hour of day (0..24).
    /// `hazard_rates` maps offset_minutes → hazard_rate for each candidate window.
    #[must_use]
    pub fn evaluate(
        &self,
        current_ms: i64,
        current_hour: u32,
        hazard_rates: &[f64],
    ) -> SchedulingDecision {
        let hours_since_last = match self.last_restart_ms {
            Some(last) => (current_ms - last) as f64 / 3_600_000.0,
            None => self.config.cooldown_hours + 1.0, // no penalty if never restarted
        };

        let window_minutes = self.config.window_minutes.max(1);
        let total_windows = (self.config.lookahead_hours * 60 / window_minutes) as usize;
        let num_rates = hazard_rates.len().min(total_windows);

        let mut windows: Vec<ScoredWindow> = (0..num_rates)
            .map(|i| {
                let offset = i as u32 * window_minutes;
                let hour = (current_hour + offset / 60) % 24;
                let hours_at_window = hours_since_last + (offset as f64 / 60.0);
                let hr = hazard_rates[i];
                let act = self.activity_profile.predict(hour);

                let hu = hazard_urgency(hr, self.config.hazard_threshold);
                let am = activity_minimum(act);
                let rp = recency_penalty(hours_at_window, self.config.cooldown_hours);
                let s = hu * am * rp;

                ScoredWindow {
                    offset_minutes: offset,
                    hour_of_day: hour,
                    hazard_urgency: hu,
                    activity_minimum: am,
                    recency_penalty: rp,
                    score: s,
                }
            })
            .collect();

        // Sort by score descending
        windows.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        let min_score = match &self.config.mode {
            RestartMode::Automatic { min_score } => *min_score,
            _ => self.config.min_score_threshold,
        };

        let recommendation = windows.first().filter(|w| w.score >= min_score).cloned();
        let would_trigger =
            matches!(self.config.mode, RestartMode::Automatic { .. }) && recommendation.is_some();

        SchedulingDecision {
            windows,
            recommendation,
            would_trigger,
        }
    }

    /// Schedule a restart based on evaluation.
    ///
    /// Returns the scheduled restart if one was set.
    pub fn schedule_from_decision(
        &mut self,
        decision: &SchedulingDecision,
        current_ms: i64,
    ) -> Option<&ScheduledRestart> {
        if let Some(ref rec) = decision.recommendation {
            let scheduled_ms = current_ms + (rec.offset_minutes as i64 * 60_000);
            self.scheduled = Some(ScheduledRestart {
                scheduled_at_ms: scheduled_ms,
                score: rec.score,
                notified: false,
                snapshot_taken: false,
            });
            self.scheduled.as_ref()
        } else {
            None
        }
    }

    /// Check if the scheduled restart should trigger now.
    #[must_use]
    pub fn should_trigger(&self, current_ms: i64) -> bool {
        if !matches!(self.config.mode, RestartMode::Automatic { .. }) {
            return false;
        }
        self.scheduled
            .as_ref()
            .is_some_and(|s| current_ms >= s.scheduled_at_ms)
    }

    /// Check if the advance warning should fire.
    #[must_use]
    pub fn should_warn(&self, current_ms: i64) -> bool {
        if let Some(ref s) = self.scheduled {
            if s.notified {
                return false;
            }
            let warn_ms = s.scheduled_at_ms - (self.config.advance_warning_minutes as i64 * 60_000);
            current_ms >= warn_ms
        } else {
            false
        }
    }

    /// Mark the warning as sent.
    pub fn mark_notified(&mut self) {
        if let Some(ref mut s) = self.scheduled {
            s.notified = true;
        }
    }

    /// Mark the pre-restart snapshot as taken.
    pub fn mark_snapshot_taken(&mut self) {
        if let Some(ref mut s) = self.scheduled {
            s.snapshot_taken = true;
        }
    }

    /// Cancel the scheduled restart.
    pub fn cancel(&mut self) {
        self.scheduled = None;
    }
}

impl Default for RestartScheduler {
    fn default() -> Self {
        Self::new(RestartSchedulerConfig::default())
    }
}

// ---------------------------------------------------------------------------
// Telemetry
// ---------------------------------------------------------------------------

/// Telemetry snapshot for the restart scheduler.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RestartSchedulerTelemetry {
    pub mode: String,
    pub observations: u64,
    pub last_restart_ms: Option<i64>,
    pub scheduled_at_ms: Option<i64>,
    pub scheduled_score: Option<f64>,
    pub min_activity_hour: u32,
}

impl RestartScheduler {
    /// Capture a telemetry snapshot.
    #[must_use]
    pub fn telemetry(&self) -> RestartSchedulerTelemetry {
        RestartSchedulerTelemetry {
            mode: format!("{:?}", self.config.mode),
            observations: self.activity_profile.observations(),
            last_restart_ms: self.last_restart_ms,
            scheduled_at_ms: self.scheduled.as_ref().map(|s| s.scheduled_at_ms),
            scheduled_score: self.scheduled.as_ref().map(|s| s.score),
            min_activity_hour: self.activity_profile.min_activity_hour(),
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_is_advisory() {
        let config = RestartSchedulerConfig::default();
        assert_eq!(config.mode, RestartMode::Advisory);
        assert!((config.min_score_threshold - 0.7).abs() < f64::EPSILON);
        assert!((config.cooldown_hours - 12.0).abs() < f64::EPSILON);
    }

    #[test]
    fn hazard_urgency_below_threshold_is_low() {
        let u = hazard_urgency(0.1, 0.5);
        assert!(u < 0.1, "urgency should be low below threshold: {u}");
    }

    #[test]
    fn hazard_urgency_above_threshold_is_high() {
        let u = hazard_urgency(0.9, 0.5);
        assert!(u > 0.9, "urgency should be high above threshold: {u}");
    }

    #[test]
    fn hazard_urgency_at_threshold_is_half() {
        let u = hazard_urgency(0.5, 0.5);
        assert!(
            (u - 0.5).abs() < 0.01,
            "urgency at threshold should be ~0.5: {u}"
        );
    }

    #[test]
    fn activity_minimum_inverts_activity() {
        assert!((activity_minimum(0.0) - 1.0).abs() < f64::EPSILON);
        assert!((activity_minimum(1.0) - 0.0).abs() < f64::EPSILON);
        assert!((activity_minimum(0.3) - 0.7).abs() < f64::EPSILON);
    }

    #[test]
    fn activity_minimum_clamps() {
        assert!((activity_minimum(-0.5) - 1.0).abs() < f64::EPSILON);
        assert!((activity_minimum(1.5) - 0.0).abs() < f64::EPSILON);
    }

    #[test]
    fn recency_penalty_starts_at_zero() {
        let p = recency_penalty(0.0, 12.0);
        assert!(p.abs() < 0.01, "penalty at time=0 should be ~0: {p}");
    }

    #[test]
    fn recency_penalty_approaches_one() {
        let p = recency_penalty(100.0, 12.0);
        assert!(p > 0.99, "penalty after long time should approach 1: {p}");
    }

    #[test]
    fn recency_penalty_zero_cooldown() {
        assert!((recency_penalty(5.0, 0.0) - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn activity_profile_ewma_update() {
        let mut profile = ActivityProfile::new(0.5);
        // Initial activity at hour 3 is 0.5
        assert!((profile.predict(3) - 0.5).abs() < f64::EPSILON);

        // Update with high activity
        profile.update(3, 1.0);
        // EWMA: 0.5 * 1.0 + 0.5 * 0.5 = 0.75
        assert!((profile.predict(3) - 0.75).abs() < f64::EPSILON);

        // Update with low activity
        profile.update(3, 0.0);
        // EWMA: 0.5 * 0.0 + 0.5 * 0.75 = 0.375
        assert!((profile.predict(3) - 0.375).abs() < f64::EPSILON);
    }

    #[test]
    fn activity_profile_min_hour() {
        let mut profile = ActivityProfile::new(1.0); // alpha=1 means instant update
        for h in 0..24 {
            profile.update(h, h as f64 / 24.0);
        }
        // Hour 0 has activity 0.0 — should be minimum
        assert_eq!(profile.min_activity_hour(), 0);
    }

    #[test]
    fn scheduler_evaluate_finds_optimal_window() {
        let config = RestartSchedulerConfig {
            mode: RestartMode::Automatic { min_score: 0.3 },
            hazard_threshold: 0.5,
            cooldown_hours: 12.0,
            lookahead_hours: 24,
            window_minutes: 60,
            ..Default::default()
        };
        let mut scheduler = RestartScheduler::new(config);

        // Set up activity: low at hour 3, high elsewhere
        for h in 0..24 {
            let activity = if h == 3 { 0.1 } else { 0.8 };
            scheduler.update_activity(h, activity);
        }

        // Hazard rates: rising over 24 hours
        let hazard_rates: Vec<f64> = (0..24).map(|h| h as f64 / 24.0).collect();

        let now_ms = 1_700_000_000_000_i64;
        let decision = scheduler.evaluate(now_ms, 0, &hazard_rates);

        assert!(!decision.windows.is_empty());
        assert!(decision.recommendation.is_some());
        assert!(decision.would_trigger);

        // The top window should have a positive score
        let top = &decision.windows[0];
        assert!(top.score > 0.0);
    }

    #[test]
    fn scheduler_cooldown_suppresses_restart() {
        let config = RestartSchedulerConfig {
            mode: RestartMode::Automatic { min_score: 0.1 },
            cooldown_hours: 24.0,
            lookahead_hours: 4,
            window_minutes: 60,
            ..Default::default()
        };
        let mut scheduler = RestartScheduler::new(config);

        // Just restarted 1 hour ago
        let now_ms = 1_700_000_000_000_i64;
        scheduler.record_restart(now_ms - 3_600_000);

        // High hazard everywhere
        let hazard_rates = vec![0.9; 4];
        let decision = scheduler.evaluate(now_ms, 12, &hazard_rates);

        // All windows should have low recency penalty (restarted recently)
        for w in &decision.windows {
            assert!(
                w.recency_penalty < 0.2,
                "recency penalty should be low: {}",
                w.recency_penalty
            );
        }
    }

    #[test]
    fn scheduler_manual_mode_never_triggers() {
        let config = RestartSchedulerConfig {
            mode: RestartMode::Manual,
            ..Default::default()
        };
        let scheduler = RestartScheduler::new(config);
        let hazard_rates = vec![1.0; 24];
        let decision = scheduler.evaluate(1_700_000_000_000, 0, &hazard_rates);
        assert!(!decision.would_trigger);
    }

    #[test]
    fn scheduler_advisory_mode_recommends_but_no_trigger() {
        let config = RestartSchedulerConfig {
            mode: RestartMode::Advisory,
            min_score_threshold: 0.1,
            lookahead_hours: 4,
            window_minutes: 60,
            ..Default::default()
        };
        let scheduler = RestartScheduler::new(config);
        let hazard_rates = vec![0.9; 4];
        let decision = scheduler.evaluate(1_700_000_000_000, 0, &hazard_rates);
        // Should recommend but not trigger
        assert!(decision.recommendation.is_some());
        assert!(!decision.would_trigger);
    }

    #[test]
    fn scheduler_schedule_and_trigger() {
        let config = RestartSchedulerConfig {
            mode: RestartMode::Automatic { min_score: 0.1 },
            advance_warning_minutes: 5,
            lookahead_hours: 2,
            window_minutes: 60,
            ..Default::default()
        };
        let mut scheduler = RestartScheduler::new(config);
        let now_ms = 1_700_000_000_000_i64;

        let hazard_rates = vec![0.9; 2];
        let decision = scheduler.evaluate(now_ms, 12, &hazard_rates);
        scheduler.schedule_from_decision(&decision, now_ms);

        assert!(scheduler.scheduled().is_some());
        // Should not trigger yet (scheduled for the future)
        assert!(!scheduler.should_trigger(now_ms));
        // Should warn 5 minutes before
        let sched_ms = scheduler.scheduled().unwrap().scheduled_at_ms;
        assert!(scheduler.should_warn(sched_ms - 4 * 60_000));
        // Should trigger at scheduled time
        assert!(scheduler.should_trigger(sched_ms));
    }

    #[test]
    fn scheduler_cancel() {
        let mut scheduler = RestartScheduler::default();
        scheduler.scheduled = Some(ScheduledRestart {
            scheduled_at_ms: 1_700_000_000_000,
            score: 0.9,
            notified: false,
            snapshot_taken: false,
        });
        scheduler.cancel();
        assert!(scheduler.scheduled().is_none());
    }

    #[test]
    fn score_window_combines_factors() {
        let config = RestartSchedulerConfig {
            hazard_threshold: 0.5,
            cooldown_hours: 12.0,
            ..Default::default()
        };
        // High hazard, low activity, long since restart → high score
        let high = score_window(0.9, 0.1, 24.0, &config);
        // Low hazard, high activity, recent restart → low score
        let low = score_window(0.1, 0.9, 1.0, &config);
        assert!(high > low, "high={high}, low={low}");
        assert!(high > 0.5);
        assert!(low < 0.05);
    }

    #[test]
    fn telemetry_snapshot() {
        let scheduler = RestartScheduler::default();
        let tel = scheduler.telemetry();
        assert_eq!(tel.observations, 0);
        assert!(tel.last_restart_ms.is_none());
        assert!(tel.scheduled_at_ms.is_none());
    }

    #[test]
    fn serde_roundtrip_config() {
        let config = RestartSchedulerConfig::default();
        let json = serde_json::to_string(&config).unwrap();
        let back: RestartSchedulerConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(config.mode, back.mode);
        assert!((config.cooldown_hours - back.cooldown_hours).abs() < f64::EPSILON);
    }

    #[test]
    fn serde_roundtrip_scheduler() {
        let mut scheduler = RestartScheduler::default();
        scheduler.update_activity(3, 0.2);
        scheduler.record_restart(1_700_000_000_000);
        let json = serde_json::to_string(&scheduler).unwrap();
        let back: RestartScheduler = serde_json::from_str(&json).unwrap();
        assert_eq!(scheduler.last_restart_ms, back.last_restart_ms);
        assert_eq!(
            scheduler.activity_profile.observations(),
            back.activity_profile.observations()
        );
    }

    #[test]
    fn serde_roundtrip_decision() {
        let decision = SchedulingDecision {
            windows: vec![ScoredWindow {
                offset_minutes: 180,
                hour_of_day: 3,
                hazard_urgency: 0.8,
                activity_minimum: 0.9,
                recency_penalty: 0.7,
                score: 0.504,
            }],
            recommendation: None,
            would_trigger: false,
        };
        let json = serde_json::to_string(&decision).unwrap();
        let back: SchedulingDecision = serde_json::from_str(&json).unwrap();
        assert_eq!(decision, back);
    }

    #[test]
    fn empty_hazard_rates() {
        let scheduler = RestartScheduler::default();
        let decision = scheduler.evaluate(1_700_000_000_000, 0, &[]);
        assert!(decision.windows.is_empty());
        assert!(decision.recommendation.is_none());
        assert!(!decision.would_trigger);
    }
}
