//! Scrollback eviction — tier-based scrollback trimming under memory pressure.
//!
//! Reduces memory and SQLite storage by trimming captured scrollback data based
//! on pane activity tiers and system memory pressure.  Active panes keep full
//! scrollback; idle/dormant panes are trimmed progressively; under memory
//! pressure, all panes are trimmed aggressively.
//!
//! # Architecture
//!
//! ```text
//! MemoryPressureTier ──┐
//!                      ├──► EvictionPolicy ──► EvictionPlan ──► SegmentStore
//! PaneTier per pane ───┘
//! ```
//!
//! The module computes per-pane segment limits from pane tier + memory pressure,
//! then delegates actual deletion to a [`SegmentStore`] trait implementor.

use std::collections::VecDeque;

use serde::{Deserialize, Serialize};

use crate::memory_pressure::MemoryPressureTier;
use crate::pane_tiers::PaneTier;
use crate::patterns::{PatternEngine, Severity};

// =============================================================================
// Configuration
// =============================================================================

/// Configuration for scrollback eviction policies.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvictionConfig {
    /// Max segments for active panes under no memory pressure.
    pub active_max_segments: usize,
    /// Max segments for thinking panes.
    pub thinking_max_segments: usize,
    /// Max segments for idle panes.
    pub idle_max_segments: usize,
    /// Max segments for background panes.
    pub background_max_segments: usize,
    /// Max segments for dormant panes.
    pub dormant_max_segments: usize,
    /// Under memory pressure, override all limits to this value.
    pub pressure_max_segments: usize,
    /// Minimum segments to always keep (floor for any pane).
    pub min_segments: usize,
}

impl Default for EvictionConfig {
    fn default() -> Self {
        Self {
            active_max_segments: 10_000,
            thinking_max_segments: 5_000,
            idle_max_segments: 1_000,
            background_max_segments: 500,
            dormant_max_segments: 100,
            pressure_max_segments: 200,
            min_segments: 10,
        }
    }
}

impl EvictionConfig {
    /// Compute the max segments for a pane given its tier and current pressure.
    #[must_use]
    pub fn max_segments_for(&self, tier: PaneTier, pressure: MemoryPressureTier) -> usize {
        let base = match tier {
            PaneTier::Active => self.active_max_segments,
            PaneTier::Thinking => self.thinking_max_segments,
            PaneTier::Idle => self.idle_max_segments,
            PaneTier::Background => self.background_max_segments,
            PaneTier::Dormant => self.dormant_max_segments,
        };

        let effective = match pressure {
            MemoryPressureTier::Green => base,
            MemoryPressureTier::Yellow => base / 2,
            MemoryPressureTier::Orange => base / 4,
            // Red: emergency cap, but never more generous than Orange
            MemoryPressureTier::Red => (base / 4).min(self.pressure_max_segments),
        };

        effective.max(self.min_segments)
    }
}

// =============================================================================
// Importance-Weighted Line Retention
// =============================================================================

/// Configuration for line importance scoring.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImportanceScoringConfig {
    /// Baseline score before bonuses/penalties.
    pub baseline: f64,
    /// Bonus for critical/error-like content.
    pub critical_bonus: f64,
    /// Bonus for warning-like content.
    pub warning_bonus: f64,
    /// Bonus for tool boundary markers.
    pub tool_boundary_bonus: f64,
    /// Bonus for compilation/build output.
    pub compilation_bonus: f64,
    /// Bonus for test output/results.
    pub test_result_bonus: f64,
    /// Penalty for blank lines.
    pub blank_line_penalty: f64,
    /// Penalty for progress-bar/status update lines.
    pub progress_line_penalty: f64,
    /// Penalty for ANSI-only lines.
    pub ansi_only_penalty: f64,
    /// Penalty for exact repeated lines.
    pub repeated_line_penalty: f64,
}

impl Default for ImportanceScoringConfig {
    fn default() -> Self {
        Self {
            baseline: 0.3,
            critical_bonus: 0.35,
            warning_bonus: 0.2,
            tool_boundary_bonus: 0.25,
            compilation_bonus: 0.15,
            test_result_bonus: 0.25,
            blank_line_penalty: 0.2,
            progress_line_penalty: 0.25,
            ansi_only_penalty: 0.3,
            repeated_line_penalty: 0.1,
        }
    }
}

/// Budget + policy for importance-weighted line eviction.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImportanceRetentionConfig {
    /// Maximum bytes retained per pane.
    pub byte_budget_per_pane: usize,
    /// Always keep at least this many lines.
    pub min_lines: usize,
    /// Never keep more than this many lines.
    pub max_lines: usize,
    /// Prefer to never evict lines at/above this threshold while lower-value lines exist.
    pub importance_threshold: f64,
    /// Fraction (0.0-1.0] of oldest lines scanned to pick a victim.
    pub oldest_window_fraction: f64,
}

impl Default for ImportanceRetentionConfig {
    fn default() -> Self {
        Self {
            byte_budget_per_pane: 2 * 1024 * 1024, // 2 MB
            min_lines: 500,
            max_lines: 10_000,
            importance_threshold: 0.8,
            oldest_window_fraction: 0.25,
        }
    }
}

impl ImportanceRetentionConfig {
    /// Validate basic invariant constraints.
    pub fn validate(&self) -> Result<(), String> {
        if self.min_lines == 0 {
            return Err("min_lines must be > 0".to_string());
        }
        if self.max_lines < self.min_lines {
            return Err("max_lines must be >= min_lines".to_string());
        }
        if self.importance_threshold.is_nan() {
            return Err("importance_threshold must be a number".to_string());
        }
        if !(0.0..=1.0).contains(&self.importance_threshold) {
            return Err("importance_threshold must be within [0.0, 1.0]".to_string());
        }
        if self.oldest_window_fraction.is_nan() || self.oldest_window_fraction <= 0.0 {
            return Err("oldest_window_fraction must be > 0".to_string());
        }
        if self.oldest_window_fraction > 1.0 {
            return Err("oldest_window_fraction must be <= 1".to_string());
        }
        Ok(())
    }
}

/// A scored scrollback line used by weighted eviction.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScrollbackLine {
    /// Line content.
    pub text: String,
    /// UTF-8 byte length of `text`.
    pub bytes: usize,
    /// Importance score in [0.0, 1.0].
    pub importance: f64,
    /// Capture timestamp in epoch-millis.
    pub timestamp_ms: u64,
}

impl ScrollbackLine {
    /// Construct a scored line.
    #[must_use]
    pub fn new(text: impl Into<String>, importance: f64, timestamp_ms: u64) -> Self {
        let text = text.into();
        Self {
            bytes: text.len(),
            text,
            importance: importance.clamp(0.0, 1.0),
            timestamp_ms,
        }
    }
}

/// Summary of an importance-budget enforcement pass.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ImportanceBudgetReport {
    /// Number of evicted lines.
    pub lines_removed: usize,
    /// Bytes removed by eviction.
    pub bytes_removed: usize,
    /// Remaining line count.
    pub remaining_lines: usize,
    /// Remaining total bytes.
    pub remaining_bytes: usize,
}

/// Importance scorer that layers pattern-detection signals over cheap heuristics.
pub struct LineImportanceScorer {
    config: ImportanceScoringConfig,
    pattern_engine: PatternEngine,
}

impl Default for LineImportanceScorer {
    fn default() -> Self {
        Self::new(ImportanceScoringConfig::default())
    }
}

impl LineImportanceScorer {
    /// Create a scorer with custom settings.
    #[must_use]
    pub fn new(config: ImportanceScoringConfig) -> Self {
        Self {
            config,
            pattern_engine: PatternEngine::new(),
        }
    }

    /// Access scoring config.
    #[must_use]
    pub fn config(&self) -> &ImportanceScoringConfig {
        &self.config
    }

    /// Compute [0,1] importance for one line.
    #[must_use]
    pub fn score_line(&self, line: &str, previous_line: Option<&str>) -> f64 {
        let mut score = self.config.baseline;
        let lower = line.to_ascii_lowercase();

        if line.trim().is_empty() {
            score -= self.config.blank_line_penalty;
        }
        if is_ansi_only_line(line) {
            score -= self.config.ansi_only_penalty;
        }
        if is_progress_line(&lower) {
            score -= self.config.progress_line_penalty;
        }
        if previous_line.is_some_and(|prev| prev == line) {
            score -= self.config.repeated_line_penalty;
        }

        if line_contains_error_signal(&lower) {
            score += self.config.critical_bonus;
        }
        if line_contains_warning_signal(&lower) {
            score += self.config.warning_bonus;
        }
        if line_contains_tool_boundary_signal(&lower) {
            score += self.config.tool_boundary_bonus;
        }
        if line_contains_compilation_signal(&lower) {
            score += self.config.compilation_bonus;
        }
        if line_contains_test_signal(&lower) {
            score += self.config.test_result_bonus;
        }

        // Reuse existing pattern rules as an extra signal layer.
        for detection in self.pattern_engine.detect(line) {
            match detection.severity {
                Severity::Critical => score += self.config.critical_bonus,
                Severity::Warning => score += self.config.warning_bonus,
                Severity::Info => score += 0.05,
            }
            let event_lower = detection.event_type.to_ascii_lowercase();
            let rule_lower = detection.rule_id.to_ascii_lowercase();
            if event_lower.contains("tool") || rule_lower.contains("tool") {
                score += self.config.tool_boundary_bonus;
            }
        }

        score.clamp(0.0, 1.0)
    }
}

/// Insert a line with computed importance, then enforce budget constraints.
///
/// Returns `(importance, budget_report)`.
pub fn push_scrollback_line(
    lines: &mut VecDeque<ScrollbackLine>,
    line_text: impl Into<String>,
    timestamp_ms: u64,
    scorer: &LineImportanceScorer,
    config: &ImportanceRetentionConfig,
) -> (f64, ImportanceBudgetReport) {
    let line_text = line_text.into();
    let previous = lines.back().map(|line| line.text.as_str());
    let importance = scorer.score_line(&line_text, previous);
    lines.push_back(ScrollbackLine::new(line_text, importance, timestamp_ms));
    let report = enforce_importance_budget(lines, config);
    (importance, report)
}

/// Total bytes represented by all lines in the deque.
#[must_use]
pub fn total_line_bytes(lines: &VecDeque<ScrollbackLine>) -> usize {
    lines.iter().map(|line| line.bytes).sum()
}

/// Pick an eviction candidate index from the oldest window.
///
/// Lines below `importance_threshold` are always preferred when available.
#[must_use]
pub fn select_importance_eviction_index(
    lines: &VecDeque<ScrollbackLine>,
    config: &ImportanceRetentionConfig,
) -> Option<usize> {
    if lines.len() <= config.min_lines {
        return None;
    }
    if lines.is_empty() {
        return None;
    }

    let len = lines.len();
    let window_len = ((len as f64) * config.oldest_window_fraction)
        .ceil()
        .max(1.0) as usize;
    let window_len = window_len.min(len);

    let mut best_below_threshold: Option<(usize, f64)> = None;
    let mut best_any: Option<(usize, f64)> = None;

    for (idx, line) in lines.iter().take(window_len).enumerate() {
        if best_any.is_none_or(|(_, best)| line.importance < best) {
            best_any = Some((idx, line.importance));
        }
        if line.importance < config.importance_threshold
            && best_below_threshold.is_none_or(|(_, best)| line.importance < best)
        {
            best_below_threshold = Some((idx, line.importance));
        }
    }

    best_below_threshold.or(best_any).map(|(idx, _)| idx)
}

/// Enforce byte and line limits using low-importance-first eviction.
pub fn enforce_importance_budget(
    lines: &mut VecDeque<ScrollbackLine>,
    config: &ImportanceRetentionConfig,
) -> ImportanceBudgetReport {
    let mut total_bytes = total_line_bytes(lines);
    let mut report = ImportanceBudgetReport::default();

    while (total_bytes > config.byte_budget_per_pane || lines.len() > config.max_lines)
        && lines.len() > config.min_lines
    {
        let Some(idx) = select_importance_eviction_index(lines, config) else {
            break;
        };

        let Some(evicted) = lines.remove(idx) else {
            break;
        };

        report.lines_removed += 1;
        report.bytes_removed += evicted.bytes;
        total_bytes = total_bytes.saturating_sub(evicted.bytes);
    }

    report.remaining_lines = lines.len();
    report.remaining_bytes = total_bytes;
    report
}

fn line_contains_error_signal(line: &str) -> bool {
    line.contains("error:")
        || line.contains(" panic")
        || line.starts_with("panic")
        || line.contains("exception")
        || line.contains("fatal")
        || line.contains("failed")
        || line.contains("traceback")
}

fn line_contains_warning_signal(line: &str) -> bool {
    line.contains("warning:") || line.contains("[warn]") || line.contains("deprecation")
}

fn line_contains_tool_boundary_signal(line: &str) -> bool {
    line.contains("using tool")
        || line.contains("tool call")
        || line.contains("executing tool")
        || line.contains("tool_use")
}

fn line_contains_compilation_signal(line: &str) -> bool {
    line.starts_with("compiling ")
        || line.contains(" finished ")
        || line.contains(" linking ")
        || line.contains("building ")
        || line.contains(" cargo ")
}

fn line_contains_test_signal(line: &str) -> bool {
    line.contains("test result:")
        || line.contains("running ")
        || line.contains("assertion failed")
        || line.contains(" tests passed")
        || line.contains(" tests failed")
}

fn is_progress_line(line: &str) -> bool {
    (line.contains('%') && (line.contains('[') && line.contains(']')))
        || line.contains(" eta ")
        || line.contains(" it/s")
        || line.contains(" bytes/s")
        || line.contains("⠋")
        || line.contains("⠙")
        || line.contains("⠹")
        || line.contains("⠸")
}

fn is_ansi_only_line(line: &str) -> bool {
    if !line.contains('\u{1b}') {
        return false;
    }
    !line.trim().is_empty() && strip_ansi_sequences(line).trim().is_empty()
}

fn strip_ansi_sequences(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();

    while let Some(ch) = chars.next() {
        if ch != '\u{1b}' {
            out.push(ch);
            continue;
        }

        match chars.peek().copied() {
            Some('[') => {
                chars.next();
                while let Some(next) = chars.next() {
                    if ('@'..='~').contains(&next) {
                        break;
                    }
                }
            }
            Some(']') => {
                chars.next();
                while let Some(next) = chars.next() {
                    if next == '\u{7}' {
                        break;
                    }
                    if next == '\u{1b}' && chars.peek().copied() == Some('\\') {
                        chars.next();
                        break;
                    }
                }
            }
            _ => {}
        }
    }

    out
}

// =============================================================================
// Eviction Plan
// =============================================================================

/// Per-pane eviction target.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvictionTarget {
    pub pane_id: u64,
    pub tier: PaneTier,
    pub current_segments: usize,
    pub max_segments: usize,
    pub segments_to_remove: usize,
}

/// Full eviction plan across all panes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvictionPlan {
    pub pressure: MemoryPressureTier,
    pub targets: Vec<EvictionTarget>,
    pub total_segments_to_remove: usize,
    pub panes_affected: usize,
}

impl EvictionPlan {
    /// Whether this plan requires any eviction work.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.total_segments_to_remove == 0
    }
}

// =============================================================================
// Eviction Report
// =============================================================================

/// Result of executing an eviction plan.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct EvictionReport {
    pub panes_trimmed: usize,
    pub segments_removed: usize,
    pub errors: Vec<String>,
}

// =============================================================================
// Segment Store Trait
// =============================================================================

/// Trait for segment storage operations needed by the evictor.
///
/// Implementations provide actual database access; the trait enables testing
/// with mocks.
pub trait SegmentStore: Send + Sync {
    /// Count segments for a given pane.
    fn count_segments(&self, pane_id: u64) -> Result<usize, String>;

    /// Delete the oldest `count` segments for a pane, preserving the newest.
    ///
    /// Returns the number of segments actually deleted.
    fn delete_oldest_segments(&self, pane_id: u64, count: usize) -> Result<usize, String>;

    /// List all known pane IDs.
    fn list_pane_ids(&self) -> Result<Vec<u64>, String>;
}

// =============================================================================
// Pane Info Source Trait
// =============================================================================

/// Provides per-pane tier classification.
pub trait PaneTierSource: Send + Sync {
    /// Get the current tier for a pane. Returns `None` if the pane is unknown.
    fn tier_for(&self, pane_id: u64) -> Option<PaneTier>;
}

// =============================================================================
// Scrollback Evictor
// =============================================================================

/// Computes and executes tier-based scrollback eviction.
pub struct ScrollbackEvictor<S: SegmentStore, T: PaneTierSource> {
    config: EvictionConfig,
    store: S,
    tier_source: T,
}

impl<S: SegmentStore, T: PaneTierSource> ScrollbackEvictor<S, T> {
    /// Create a new evictor.
    pub fn new(config: EvictionConfig, store: S, tier_source: T) -> Self {
        Self {
            config,
            store,
            tier_source,
        }
    }

    /// Compute an eviction plan without executing it.
    pub fn plan(&self, pressure: MemoryPressureTier) -> Result<EvictionPlan, String> {
        let pane_ids = self.store.list_pane_ids()?;
        let mut targets = Vec::new();
        let mut total_to_remove = 0usize;

        for pane_id in pane_ids {
            let tier = self
                .tier_source
                .tier_for(pane_id)
                .unwrap_or(PaneTier::Dormant); // Unknown panes treated as dormant

            let current = self.store.count_segments(pane_id)?;
            let max = self.config.max_segments_for(tier, pressure);

            if current > max {
                let to_remove = current - max;
                total_to_remove += to_remove;
                targets.push(EvictionTarget {
                    pane_id,
                    tier,
                    current_segments: current,
                    max_segments: max,
                    segments_to_remove: to_remove,
                });
            }
        }

        let panes_affected = targets.len();

        Ok(EvictionPlan {
            pressure,
            targets,
            total_segments_to_remove: total_to_remove,
            panes_affected,
        })
    }

    /// Execute an eviction plan, deleting excess segments.
    pub fn execute(&self, plan: &EvictionPlan) -> EvictionReport {
        let mut report = EvictionReport::default();

        for target in &plan.targets {
            match self
                .store
                .delete_oldest_segments(target.pane_id, target.segments_to_remove)
            {
                Ok(deleted) => {
                    report.segments_removed += deleted;
                    if deleted > 0 {
                        report.panes_trimmed += 1;
                    }
                }
                Err(e) => {
                    report.errors.push(format!(
                        "pane {}: failed to delete {} segments: {}",
                        target.pane_id, target.segments_to_remove, e
                    ));
                }
            }
        }

        report
    }

    /// Plan and execute in one step.
    pub fn evict(&self, pressure: MemoryPressureTier) -> Result<EvictionReport, String> {
        let plan = self.plan(pressure)?;
        Ok(self.execute(&plan))
    }

    /// Get the current config.
    #[must_use]
    pub fn config(&self) -> &EvictionConfig {
        &self.config
    }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{HashMap, VecDeque};

    // ── Mock implementations ──────────────────────────────────────────

    /// Simple in-memory segment store for testing.
    #[derive(Debug, Default)]
    struct MockStore {
        segments: HashMap<u64, usize>,
    }

    impl MockStore {
        fn with_panes(panes: &[(u64, usize)]) -> Self {
            Self {
                segments: panes.iter().copied().collect(),
            }
        }
    }

    impl SegmentStore for MockStore {
        fn count_segments(&self, pane_id: u64) -> Result<usize, String> {
            Ok(*self.segments.get(&pane_id).unwrap_or(&0))
        }

        fn delete_oldest_segments(&self, _pane_id: u64, count: usize) -> Result<usize, String> {
            Ok(count) // Pretend we deleted them
        }

        fn list_pane_ids(&self) -> Result<Vec<u64>, String> {
            let mut ids: Vec<_> = self.segments.keys().copied().collect();
            ids.sort();
            Ok(ids)
        }
    }

    /// Tier source that maps pane IDs to predetermined tiers.
    struct MockTierSource {
        tiers: HashMap<u64, PaneTier>,
    }

    impl MockTierSource {
        fn new(tiers: &[(u64, PaneTier)]) -> Self {
            Self {
                tiers: tiers.iter().copied().collect(),
            }
        }
    }

    impl PaneTierSource for MockTierSource {
        fn tier_for(&self, pane_id: u64) -> Option<PaneTier> {
            self.tiers.get(&pane_id).copied()
        }
    }

    fn default_evictor(
        panes: &[(u64, usize)],
        tiers: &[(u64, PaneTier)],
    ) -> ScrollbackEvictor<MockStore, MockTierSource> {
        ScrollbackEvictor::new(
            EvictionConfig::default(),
            MockStore::with_panes(panes),
            MockTierSource::new(tiers),
        )
    }

    // ── Config tests ──────────────────────────────────────────────────

    #[test]
    fn config_defaults() {
        let c = EvictionConfig::default();
        assert_eq!(c.active_max_segments, 10_000);
        assert_eq!(c.dormant_max_segments, 100);
        assert_eq!(c.pressure_max_segments, 200);
        assert_eq!(c.min_segments, 10);
    }

    #[test]
    fn config_serde_roundtrip() {
        let c = EvictionConfig {
            active_max_segments: 5000,
            thinking_max_segments: 2000,
            idle_max_segments: 500,
            background_max_segments: 250,
            dormant_max_segments: 50,
            pressure_max_segments: 100,
            min_segments: 5,
        };
        let json = serde_json::to_string(&c).unwrap();
        let parsed: EvictionConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.active_max_segments, 5000);
        assert_eq!(parsed.min_segments, 5);
    }

    // ── max_segments_for tests ────────────────────────────────────────

    #[test]
    fn active_green_gets_full_limit() {
        let c = EvictionConfig::default();
        assert_eq!(
            c.max_segments_for(PaneTier::Active, MemoryPressureTier::Green),
            10_000
        );
    }

    #[test]
    fn dormant_green_gets_dormant_limit() {
        let c = EvictionConfig::default();
        assert_eq!(
            c.max_segments_for(PaneTier::Dormant, MemoryPressureTier::Green),
            100
        );
    }

    #[test]
    fn yellow_pressure_halves_limits() {
        let c = EvictionConfig::default();
        assert_eq!(
            c.max_segments_for(PaneTier::Active, MemoryPressureTier::Yellow),
            5_000
        );
        assert_eq!(
            c.max_segments_for(PaneTier::Idle, MemoryPressureTier::Yellow),
            500
        );
    }

    #[test]
    fn orange_pressure_quarters_limits() {
        let c = EvictionConfig::default();
        assert_eq!(
            c.max_segments_for(PaneTier::Active, MemoryPressureTier::Orange),
            2_500
        );
    }

    #[test]
    fn red_pressure_uses_emergency_limit() {
        let c = EvictionConfig::default();
        // Active: min(10000/4, 200) = 200
        assert_eq!(
            c.max_segments_for(PaneTier::Active, MemoryPressureTier::Red),
            200
        );
        // Dormant: min(100/4, 200) = 25, but min_segments floor = 25.max(10) = 25
        assert_eq!(
            c.max_segments_for(PaneTier::Dormant, MemoryPressureTier::Red),
            25
        );
    }

    #[test]
    fn min_segments_floor_respected() {
        let c = EvictionConfig {
            dormant_max_segments: 4, // Below min_segments (10)
            min_segments: 10,
            ..Default::default()
        };
        assert_eq!(
            c.max_segments_for(PaneTier::Dormant, MemoryPressureTier::Green),
            10
        );
    }

    #[test]
    fn min_segments_floor_under_pressure() {
        let c = EvictionConfig {
            pressure_max_segments: 3, // Below min_segments
            min_segments: 5,
            ..Default::default()
        };
        assert_eq!(
            c.max_segments_for(PaneTier::Active, MemoryPressureTier::Red),
            5
        );
    }

    // ── Plan tests ────────────────────────────────────────────────────

    #[test]
    fn plan_no_eviction_needed() {
        let ev = default_evictor(
            &[(1, 100), (2, 50)],
            &[(1, PaneTier::Active), (2, PaneTier::Idle)],
        );

        let plan = ev.plan(MemoryPressureTier::Green).unwrap();
        assert!(plan.is_empty());
        assert_eq!(plan.panes_affected, 0);
    }

    #[test]
    fn plan_trims_over_limit_panes() {
        let ev = default_evictor(
            &[
                (1, 15_000), // Active: limit 10000, over by 5000
                (2, 500),    // Idle: limit 1000, under
                (3, 200),    // Dormant: limit 100, over by 100
            ],
            &[
                (1, PaneTier::Active),
                (2, PaneTier::Idle),
                (3, PaneTier::Dormant),
            ],
        );

        let plan = ev.plan(MemoryPressureTier::Green).unwrap();
        assert_eq!(plan.panes_affected, 2);
        assert_eq!(plan.total_segments_to_remove, 5100);

        let t1 = plan.targets.iter().find(|t| t.pane_id == 1).unwrap();
        assert_eq!(t1.segments_to_remove, 5000);
        assert_eq!(t1.max_segments, 10_000);

        let t3 = plan.targets.iter().find(|t| t.pane_id == 3).unwrap();
        assert_eq!(t3.segments_to_remove, 100);
    }

    #[test]
    fn plan_under_pressure_trims_more() {
        let ev = default_evictor(
            &[(1, 5000), (2, 5000)],
            &[(1, PaneTier::Active), (2, PaneTier::Idle)],
        );

        let green_plan = ev.plan(MemoryPressureTier::Green).unwrap();
        let red_plan = ev.plan(MemoryPressureTier::Red).unwrap();

        // Green: active has 5000 < 10000, idle has 5000 > 1000
        assert_eq!(green_plan.total_segments_to_remove, 4000);

        // Red: both panes get 200 limit, so 4800 + 4800 = 9600
        assert_eq!(red_plan.total_segments_to_remove, 9600);
        assert!(
            red_plan.total_segments_to_remove > green_plan.total_segments_to_remove,
            "red pressure should trim more than green"
        );
    }

    #[test]
    fn plan_unknown_panes_treated_as_dormant() {
        let ev = default_evictor(
            &[(99, 500)], // Pane 99 not in tier source
            &[],          // No tier mappings
        );

        let plan = ev.plan(MemoryPressureTier::Green).unwrap();
        // Dormant limit = 100, so 500 - 100 = 400 to remove
        assert_eq!(plan.total_segments_to_remove, 400);
    }

    // ── Execute tests ─────────────────────────────────────────────────

    #[test]
    fn execute_reports_results() {
        let ev = default_evictor(
            &[(1, 15_000), (2, 500)],
            &[(1, PaneTier::Active), (2, PaneTier::Dormant)],
        );

        let plan = ev.plan(MemoryPressureTier::Green).unwrap();
        let report = ev.execute(&plan);

        assert_eq!(report.panes_trimmed, 2);
        assert_eq!(report.segments_removed, 5400); // 5000 + 400
        assert!(report.errors.is_empty());
    }

    #[test]
    fn execute_empty_plan_is_noop() {
        let ev = default_evictor(&[(1, 100)], &[(1, PaneTier::Active)]);

        let plan = ev.plan(MemoryPressureTier::Green).unwrap();
        let report = ev.execute(&plan);

        assert_eq!(report.panes_trimmed, 0);
        assert_eq!(report.segments_removed, 0);
    }

    #[test]
    fn evict_convenience_method() {
        let ev = default_evictor(&[(1, 500)], &[(1, PaneTier::Dormant)]);

        let report = ev.evict(MemoryPressureTier::Green).unwrap();
        assert_eq!(report.segments_removed, 400);
    }

    // ── Error handling ────────────────────────────────────────────────

    struct FailingStore;

    impl SegmentStore for FailingStore {
        fn count_segments(&self, _pane_id: u64) -> Result<usize, String> {
            Ok(1000)
        }

        fn delete_oldest_segments(&self, _pane_id: u64, _count: usize) -> Result<usize, String> {
            Err("disk full".to_string())
        }

        fn list_pane_ids(&self) -> Result<Vec<u64>, String> {
            Ok(vec![1])
        }
    }

    #[test]
    fn execute_records_errors() {
        let ev = ScrollbackEvictor::new(
            EvictionConfig::default(),
            FailingStore,
            MockTierSource::new(&[(1, PaneTier::Dormant)]),
        );

        let plan = ev.plan(MemoryPressureTier::Green).unwrap();
        let report = ev.execute(&plan);

        assert_eq!(report.panes_trimmed, 0);
        assert_eq!(report.errors.len(), 1);
        assert!(report.errors[0].contains("disk full"));
    }

    // ── Eviction plan serialization ───────────────────────────────────

    #[test]
    fn plan_serializes() {
        let ev = default_evictor(
            &[(1, 500), (2, 200)],
            &[(1, PaneTier::Active), (2, PaneTier::Dormant)],
        );

        let plan = ev.plan(MemoryPressureTier::Yellow).unwrap();
        let json = serde_json::to_string(&plan).unwrap();
        let parsed: EvictionPlan = serde_json::from_str(&json).unwrap();
        assert_eq!(
            parsed.total_segments_to_remove,
            plan.total_segments_to_remove
        );
    }

    #[test]
    fn report_serializes() {
        let report = EvictionReport {
            panes_trimmed: 3,
            segments_removed: 1500,
            errors: vec!["pane 5: timeout".to_string()],
        };
        let json = serde_json::to_string(&report).unwrap();
        let parsed: EvictionReport = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.panes_trimmed, 3);
        assert_eq!(parsed.errors.len(), 1);
    }

    // ── Property-based tests ──────────────────────────────────────────

    /// Dormant panes always get trimmed more aggressively than idle,
    /// which are trimmed more aggressively than active.
    #[test]
    fn tier_ordering_invariant() {
        let config = EvictionConfig::default();

        for pressure in [
            MemoryPressureTier::Green,
            MemoryPressureTier::Yellow,
            MemoryPressureTier::Orange,
            MemoryPressureTier::Red,
        ] {
            let active = config.max_segments_for(PaneTier::Active, pressure);
            let thinking = config.max_segments_for(PaneTier::Thinking, pressure);
            let idle = config.max_segments_for(PaneTier::Idle, pressure);
            let background = config.max_segments_for(PaneTier::Background, pressure);
            let dormant = config.max_segments_for(PaneTier::Dormant, pressure);

            assert!(
                active >= thinking,
                "active({active}) >= thinking({thinking}) at {pressure:?}"
            );
            assert!(
                thinking >= idle,
                "thinking({thinking}) >= idle({idle}) at {pressure:?}"
            );
            assert!(
                idle >= background,
                "idle({idle}) >= background({background}) at {pressure:?}"
            );
            assert!(
                background >= dormant,
                "background({background}) >= dormant({dormant}) at {pressure:?}"
            );
        }
    }

    /// Higher pressure => equal or lower limits for every tier.
    #[test]
    fn pressure_monotonicity() {
        let config = EvictionConfig::default();
        let pressures = [
            MemoryPressureTier::Green,
            MemoryPressureTier::Yellow,
            MemoryPressureTier::Orange,
            MemoryPressureTier::Red,
        ];

        for tier in [
            PaneTier::Active,
            PaneTier::Thinking,
            PaneTier::Idle,
            PaneTier::Background,
            PaneTier::Dormant,
        ] {
            for window in pressures.windows(2) {
                let lower_pressure = config.max_segments_for(tier, window[0]);
                let higher_pressure = config.max_segments_for(tier, window[1]);
                assert!(
                    lower_pressure >= higher_pressure,
                    "{tier:?}: {lower_pressure} >= {higher_pressure} ({:?} vs {:?})",
                    window[0],
                    window[1]
                );
            }
        }
    }

    /// Trimming never removes more segments than the pane actually has.
    #[test]
    fn no_over_eviction() {
        let panes = vec![(1, 50), (2, 100), (3, 5000), (4, 0)];
        let tiers = vec![
            (1, PaneTier::Dormant),
            (2, PaneTier::Idle),
            (3, PaneTier::Active),
            (4, PaneTier::Active),
        ];

        let ev = default_evictor(&panes, &tiers);

        for pressure in [
            MemoryPressureTier::Green,
            MemoryPressureTier::Yellow,
            MemoryPressureTier::Orange,
            MemoryPressureTier::Red,
        ] {
            let plan = ev.plan(pressure).unwrap();
            for target in &plan.targets {
                assert!(
                    target.segments_to_remove <= target.current_segments,
                    "pane {}: removing {} > current {} at {pressure:?}",
                    target.pane_id,
                    target.segments_to_remove,
                    target.current_segments,
                );
            }
        }
    }

    /// Running plan twice with unchanged state produces same result.
    #[test]
    fn plan_idempotency() {
        let ev = default_evictor(
            &[(1, 5000), (2, 300)],
            &[(1, PaneTier::Idle), (2, PaneTier::Dormant)],
        );

        let plan1 = ev.plan(MemoryPressureTier::Green).unwrap();
        let plan2 = ev.plan(MemoryPressureTier::Green).unwrap();

        assert_eq!(
            plan1.total_segments_to_remove,
            plan2.total_segments_to_remove
        );
        assert_eq!(plan1.panes_affected, plan2.panes_affected);
    }

    /// Min segments floor prevents total eviction.
    #[test]
    fn min_segments_prevents_total_eviction() {
        let config = EvictionConfig {
            min_segments: 20,
            ..Default::default()
        };

        for tier in [
            PaneTier::Active,
            PaneTier::Thinking,
            PaneTier::Idle,
            PaneTier::Background,
            PaneTier::Dormant,
        ] {
            for pressure in [
                MemoryPressureTier::Green,
                MemoryPressureTier::Yellow,
                MemoryPressureTier::Orange,
                MemoryPressureTier::Red,
            ] {
                let max = config.max_segments_for(tier, pressure);
                assert!(
                    max >= 20,
                    "{tier:?} at {pressure:?}: max={max} < min_segments=20"
                );
            }
        }
    }

    /// With many panes at various tiers, total eviction never exceeds total excess.
    #[test]
    fn total_eviction_bounded() {
        let panes: Vec<(u64, usize)> = (0..50).map(|i| (i, 1000)).collect();
        let tiers: Vec<(u64, PaneTier)> = (0..50)
            .map(|i| {
                let tier = match i % 5 {
                    0 => PaneTier::Active,
                    1 => PaneTier::Thinking,
                    2 => PaneTier::Idle,
                    3 => PaneTier::Background,
                    _ => PaneTier::Dormant,
                };
                (i, tier)
            })
            .collect();

        let ev = default_evictor(&panes, &tiers);
        let total_segments: usize = panes.iter().map(|(_, c)| c).sum();

        for pressure in [
            MemoryPressureTier::Green,
            MemoryPressureTier::Yellow,
            MemoryPressureTier::Orange,
            MemoryPressureTier::Red,
        ] {
            let plan = ev.plan(pressure).unwrap();
            assert!(
                plan.total_segments_to_remove <= total_segments,
                "can't remove more than total at {pressure:?}: {} > {}",
                plan.total_segments_to_remove,
                total_segments,
            );
        }
    }

    // ── Importance-scoring tests ─────────────────────────────────────

    #[test]
    fn line_scoring_stays_in_range() {
        let scorer = LineImportanceScorer::default();
        let cases = [
            "",
            "\u{1b}[2K\u{1b}[1A",
            "[##########] 100%",
            "error: failed to compile crate",
            "Using tool: Bash",
            "test result: FAILED. 12 passed; 1 failed",
        ];

        for case in cases {
            let score = scorer.score_line(case, None);
            assert!(
                (0.0..=1.0).contains(&score),
                "score must be in [0,1], got {score} for case: {case:?}"
            );
        }
    }

    #[test]
    fn important_lines_outlive_low_value_lines() {
        let config = ImportanceRetentionConfig {
            byte_budget_per_pane: 60,
            min_lines: 1,
            max_lines: 100,
            importance_threshold: 0.8,
            oldest_window_fraction: 1.0,
        };

        let mut lines = VecDeque::from(vec![
            ScrollbackLine::new("progress 10%", 0.10, 1),
            ScrollbackLine::new("error: build failed", 0.95, 2),
            ScrollbackLine::new("progress 20%", 0.15, 3),
            ScrollbackLine::new("warning: unstable API", 0.85, 4),
        ]);
        let initial_bytes = total_line_bytes(&lines);
        assert!(initial_bytes > config.byte_budget_per_pane);

        let report = enforce_importance_budget(&mut lines, &config);
        assert!(report.lines_removed > 0);

        let has_error_line = lines.iter().any(|line| line.text.contains("error"));
        let has_low_progress_line = lines
            .iter()
            .any(|line| line.text.contains("progress") && line.importance < 0.8);

        assert!(has_error_line, "critical line should remain under pressure");
        assert!(
            !has_low_progress_line,
            "low-value progress lines should be evicted first"
        );
    }

    #[test]
    fn threshold_floor_prefers_low_importance_victims() {
        let config = ImportanceRetentionConfig {
            byte_budget_per_pane: 32,
            min_lines: 1,
            max_lines: 100,
            importance_threshold: 0.8,
            oldest_window_fraction: 1.0,
        };

        let mut lines = VecDeque::from(vec![
            ScrollbackLine::new("critical diagnostics", 0.95, 1),
            ScrollbackLine::new("blank-ish", 0.05, 2),
            ScrollbackLine::new("important summary", 0.9, 3),
        ]);

        let report = enforce_importance_budget(&mut lines, &config);
        assert!(report.lines_removed >= 1);
        assert!(
            lines.iter().all(|line| line.importance >= 0.8),
            "remaining lines should be high-importance once low lines are available to evict"
        );
    }

    #[test]
    fn select_eviction_scans_oldest_window() {
        let config = ImportanceRetentionConfig {
            byte_budget_per_pane: usize::MAX,
            min_lines: 1,
            max_lines: 100,
            importance_threshold: 0.8,
            oldest_window_fraction: 0.5,
        };

        let lines = VecDeque::from(vec![
            ScrollbackLine::new("old/high", 0.9, 1),
            ScrollbackLine::new("old/low", 0.1, 2),
            ScrollbackLine::new("new/very-low", 0.01, 3),
            ScrollbackLine::new("new/high", 0.95, 4),
        ]);

        let idx = select_importance_eviction_index(&lines, &config).unwrap();
        assert_eq!(idx, 1, "victim should come from oldest half only");
    }

    #[test]
    fn push_scrollback_line_scores_and_enforces() {
        let scorer = LineImportanceScorer::default();
        let config = ImportanceRetentionConfig {
            byte_budget_per_pane: 24,
            min_lines: 1,
            max_lines: 2,
            importance_threshold: 0.8,
            oldest_window_fraction: 1.0,
        };

        let mut lines = VecDeque::new();
        let (_s1, _r1) = push_scrollback_line(&mut lines, "progress 10%", 1, &scorer, &config);
        let (_s2, _r2) = push_scrollback_line(&mut lines, "error: failed", 2, &scorer, &config);
        let (_s3, report) = push_scrollback_line(&mut lines, "progress 20%", 3, &scorer, &config);

        assert!(report.lines_removed >= 1);
        assert!(lines.len() <= config.max_lines);
        assert!(
            lines.iter().any(|line| line.text.contains("error")),
            "high-value line should be retained"
        );
    }
}
