//! Agent pane state detection and visualization.
//!
//! Provides [`AgentPaneState`] — the visual state of an agent-controlled pane —
//! and [`AgentDetectionConfig`] — the thresholds used to classify each pane.
//!
//! Detection logic is time-based:
//! - **Active** (green): output received within `active_output_threshold_ms`
//! - **Thinking** (yellow): input sent but no output for `thinking_silence_ms`..`stuck_silence_ms`
//! - **Stuck** (red): no output for > `stuck_silence_ms`, or flagged by watchdog/circuit-breaker
//! - **Idle** (gray): no input AND no output for > `idle_silence_ms`
//!
//! The GUI reads these states to color pane borders and drive mass operations
//! like "kill all stuck" or "focus on errors".

use serde::{Deserialize, Serialize};

/// Visual state of an agent-controlled pane.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentPaneState {
    /// Agent is actively producing output (green border).
    Active,
    /// Agent received input but has not produced output yet (yellow border).
    Thinking,
    /// Agent appears stuck — no output beyond threshold or flagged by watchdog (red border).
    Stuck,
    /// Pane is idle — no input or output for an extended period (gray border).
    Idle,
    /// Pane is not agent-controlled (no special border).
    #[default]
    Human,
}

impl AgentPaneState {
    /// Returns the RGBA border color for this state.
    ///
    /// Colors follow the bead spec:
    /// - Active  → green  (0, 200, 83)
    /// - Thinking → yellow (255, 193, 7)
    /// - Stuck   → red    (244, 67, 54)
    /// - Idle    → gray   (158, 158, 158)
    /// - Human   → None (use default border)
    pub fn border_color_rgba(&self) -> Option<(u8, u8, u8, u8)> {
        match self {
            Self::Active => Some((0, 200, 83, 255)),
            Self::Thinking => Some((255, 193, 7, 255)),
            Self::Stuck => Some((244, 67, 54, 255)),
            Self::Idle => Some((158, 158, 158, 255)),
            Self::Human => None,
        }
    }

    /// Short label for display in pane chrome overlay.
    pub fn label(&self) -> &'static str {
        match self {
            Self::Active => "ACTIVE",
            Self::Thinking => "THINKING",
            Self::Stuck => "STUCK",
            Self::Idle => "IDLE",
            Self::Human => "",
        }
    }

    /// Whether this state should trigger alert-level visual indicators.
    pub fn is_alert(&self) -> bool {
        matches!(self, Self::Stuck)
    }
}

/// Configuration for agent pane state detection thresholds.
///
/// All durations are in milliseconds. Maps to the `[agent_detection]` section
/// in `frankenterm.toml`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentDetectionConfig {
    /// Enable agent pane state detection (default: true).
    pub enabled: bool,

    /// Pane produced output within this window → Active.
    /// Default: 5000ms (5 seconds).
    pub active_output_threshold_ms: u64,

    /// Input sent but no output for this long → Thinking.
    /// Default: 5000ms (5 seconds).
    pub thinking_silence_ms: u64,

    /// No output for this long after input → Stuck.
    /// Default: 30000ms (30 seconds).
    pub stuck_silence_ms: u64,

    /// No input AND no output for this long → Idle.
    /// Default: 60000ms (60 seconds).
    pub idle_silence_ms: u64,

    /// Show agent name overlay in pane title bar.
    pub show_agent_name_overlay: bool,

    /// Show backpressure tier indicator in pane chrome.
    pub show_backpressure_indicator: bool,

    /// Show queue depth sparkline (requires show_backpressure_indicator).
    pub show_queue_sparkline: bool,

    /// Border width in pixels for agent state indicator.
    pub agent_border_width_px: u32,
}

impl Default for AgentDetectionConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            active_output_threshold_ms: 5000,
            thinking_silence_ms: 5000,
            stuck_silence_ms: 30_000,
            idle_silence_ms: 60_000,
            show_agent_name_overlay: true,
            show_backpressure_indicator: true,
            show_queue_sparkline: false,
            agent_border_width_px: 2,
        }
    }
}

/// Per-pane timing state used to classify [`AgentPaneState`].
#[derive(Debug, Clone)]
pub struct PaneActivityTimestamps {
    /// Millisecond timestamp of last output received from pane.
    pub last_output_ms: u64,
    /// Millisecond timestamp of last input sent to pane.
    pub last_input_ms: u64,
    /// Whether this pane is agent-controlled.
    pub is_agent: bool,
    /// Whether the watchdog or circuit breaker has flagged this pane.
    pub flagged_stuck: bool,
}

impl PaneActivityTimestamps {
    /// Classify the pane state given the current time and detection config.
    pub fn classify(&self, now_ms: u64, config: &AgentDetectionConfig) -> AgentPaneState {
        if !self.is_agent {
            return AgentPaneState::Human;
        }

        // Watchdog/circuit-breaker override
        if self.flagged_stuck {
            return AgentPaneState::Stuck;
        }

        let since_output = now_ms.saturating_sub(self.last_output_ms);
        let since_input = now_ms.saturating_sub(self.last_input_ms);

        // Recent output → Active
        if since_output < config.active_output_threshold_ms {
            return AgentPaneState::Active;
        }

        // No input AND no output for a long time → Idle
        if since_output >= config.idle_silence_ms && since_input >= config.idle_silence_ms {
            return AgentPaneState::Idle;
        }

        // Input sent but output silent beyond stuck threshold → Stuck
        if self.last_input_ms > self.last_output_ms && since_output >= config.stuck_silence_ms {
            return AgentPaneState::Stuck;
        }

        // Input sent but not yet stuck → Thinking
        if self.last_input_ms > self.last_output_ms && since_output >= config.thinking_silence_ms {
            return AgentPaneState::Thinking;
        }

        // Fallback: activity is outside the immediate "active output" window,
        // but it also has not been quiet long enough to be idle and any
        // post-input silence has not yet crossed the thinking/stuck thresholds.
        AgentPaneState::Active
    }
}

/// Backpressure visualization data for a single pane.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PaneBackpressureOverlay {
    /// Current backpressure tier (mirrors BackpressureTier).
    pub tier: String,
    /// Queue depth as a fraction 0.0..1.0 for sparkline rendering.
    pub queue_fill_ratio: f64,
    /// Whether the pane is currently rate-limited.
    pub rate_limited: bool,
}

/// Policy for smart auto-layout of agent panes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AutoLayoutPolicy {
    /// Group panes by project/domain.
    ByDomain,
    /// Sort by status: errors first, active next, idle last.
    #[default]
    ByStatus,
    /// Sort by most recent activity.
    ByActivity,
    /// No auto-layout; manual arrangement only.
    Manual,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn human_pane_always_returns_human() {
        let ts = PaneActivityTimestamps {
            last_output_ms: 0,
            last_input_ms: 0,
            is_agent: false,
            flagged_stuck: false,
        };
        let config = AgentDetectionConfig::default();
        assert_eq!(ts.classify(100_000, &config), AgentPaneState::Human);
    }

    #[test]
    fn recent_output_is_active() {
        let ts = PaneActivityTimestamps {
            last_output_ms: 98_000,
            last_input_ms: 95_000,
            is_agent: true,
            flagged_stuck: false,
        };
        let config = AgentDetectionConfig::default();
        assert_eq!(ts.classify(100_000, &config), AgentPaneState::Active);
    }

    #[test]
    fn input_without_output_is_thinking() {
        let ts = PaneActivityTimestamps {
            last_output_ms: 80_000,
            last_input_ms: 92_000,
            is_agent: true,
            flagged_stuck: false,
        };
        let config = AgentDetectionConfig::default();
        // 20s since output, input was at 92s (more recent than output)
        // since_output=20000 > thinking_silence_ms=5000 but < stuck_silence_ms=30000
        assert_eq!(ts.classify(100_000, &config), AgentPaneState::Thinking);
    }

    #[test]
    fn recent_input_before_thinking_threshold_stays_active() {
        let ts = PaneActivityTimestamps {
            last_output_ms: 96_000,
            last_input_ms: 99_000,
            is_agent: true,
            flagged_stuck: false,
        };
        let config = AgentDetectionConfig::default();
        // since_output=4000 < thinking_silence_ms=5000, so the pane is still
        // within the grace window before it should be considered thinking.
        assert_eq!(ts.classify(100_000, &config), AgentPaneState::Active);
    }

    #[test]
    fn long_silence_after_input_is_stuck() {
        let ts = PaneActivityTimestamps {
            last_output_ms: 60_000,
            last_input_ms: 65_000,
            is_agent: true,
            flagged_stuck: false,
        };
        let config = AgentDetectionConfig::default();
        // 40s since output, input was more recent than output → Stuck
        assert_eq!(ts.classify(100_000, &config), AgentPaneState::Stuck);
    }

    #[test]
    fn flagged_stuck_overrides_all() {
        let ts = PaneActivityTimestamps {
            last_output_ms: 99_999,
            last_input_ms: 99_999,
            is_agent: true,
            flagged_stuck: true,
        };
        let config = AgentDetectionConfig::default();
        assert_eq!(ts.classify(100_000, &config), AgentPaneState::Stuck);
    }

    #[test]
    fn no_activity_for_long_is_idle() {
        let ts = PaneActivityTimestamps {
            last_output_ms: 10_000,
            last_input_ms: 10_000,
            is_agent: true,
            flagged_stuck: false,
        };
        let config = AgentDetectionConfig::default();
        // 90s since both → Idle
        assert_eq!(ts.classify(100_000, &config), AgentPaneState::Idle);
    }

    #[test]
    fn border_color_mapping() {
        assert_eq!(
            AgentPaneState::Active.border_color_rgba(),
            Some((0, 200, 83, 255))
        );
        assert_eq!(
            AgentPaneState::Thinking.border_color_rgba(),
            Some((255, 193, 7, 255))
        );
        assert_eq!(
            AgentPaneState::Stuck.border_color_rgba(),
            Some((244, 67, 54, 255))
        );
        assert_eq!(
            AgentPaneState::Idle.border_color_rgba(),
            Some((158, 158, 158, 255))
        );
        assert_eq!(AgentPaneState::Human.border_color_rgba(), None);
    }

    #[test]
    fn default_config_has_expected_thresholds() {
        let config = AgentDetectionConfig::default();
        assert_eq!(config.active_output_threshold_ms, 5000);
        assert_eq!(config.thinking_silence_ms, 5000);
        assert_eq!(config.stuck_silence_ms, 30_000);
        assert_eq!(config.idle_silence_ms, 60_000);
        assert!(config.enabled);
    }
}
