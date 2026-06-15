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

    /// Lowercase, parse-stable token for `ft robot await state:<pane>:<token>`
    /// condition specs (ft-7h5da.4.3). Counterpart to the uppercase display
    /// [`label`](Self::label); round-trips with [`from_token`](Self::from_token).
    #[must_use]
    pub fn as_token(&self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Thinking => "thinking",
            Self::Stuck => "stuck",
            Self::Idle => "idle",
            Self::Human => "human",
        }
    }

    /// Parse a `state:` condition token (case-insensitive, surrounding
    /// whitespace ignored) into a classification. Returns `None` for an
    /// unrecognized token so callers can emit a precise parse error
    /// (ft-7h5da.4.3).
    #[must_use]
    pub fn from_token(token: &str) -> Option<Self> {
        match token.trim().to_ascii_lowercase().as_str() {
            "active" => Some(Self::Active),
            "thinking" => Some(Self::Thinking),
            "stuck" => Some(Self::Stuck),
            "idle" => Some(Self::Idle),
            "human" => Some(Self::Human),
            _ => None,
        }
    }
}

/// A parsed `ft robot await` pane-state/quiescence condition (ft-7h5da.4.3).
///
/// These generalize `wait-for` over the watcher's pane-state classification and
/// quiescence, alongside the event-`rule:` source the CLI already supports. The
/// parsing + matching logic lives here (pure, unit-tested via
/// `cargo test -p frankenterm-core --lib`) so the CLI and any MCP mirror
/// (ft-7h5da.4.4) share one implementation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AwaitPaneCondition {
    /// `state:<pane>:<class>` — pane's classification equals `class`.
    State { pane_id: u64, class: AgentPaneState },
    /// `quiescence:<pane>[:<idle_ms>]` — no output for at least `idle_ms`
    /// (defaults to the agent-detection `idle_silence_ms` when omitted).
    Quiescence { pane_id: u64, idle_ms: Option<u64> },
}

impl AwaitPaneCondition {
    /// Parse a `state:` / `quiescence:` condition spec. Returns `Ok(None)` when
    /// the spec is neither (the caller handles `rule:` and unknown sources);
    /// `Err` for a recognized-but-malformed spec so the CLI fails fast with a
    /// precise message instead of blocking forever on an uninterpretable
    /// condition.
    ///
    /// # Errors
    /// Returns a human-readable message for a malformed pane id, class token,
    /// or idle threshold.
    pub fn parse(spec: &str) -> Result<Option<Self>, String> {
        let spec = spec.trim();
        if let Some(rest) = spec.strip_prefix("state:") {
            // state:<pane>:<class>
            let (pane_part, class_part) = rest
                .split_once(':')
                .ok_or_else(|| format!("condition `{spec}`: expected `state:<pane>:<class>`"))?;
            let pane_id = pane_part.trim().parse::<u64>().map_err(|_| {
                format!("condition `{spec}`: invalid pane id `{}`", pane_part.trim())
            })?;
            let class = AgentPaneState::from_token(class_part).ok_or_else(|| {
                format!(
                    "condition `{spec}`: unknown pane-state `{}` \
                     (expected active|thinking|stuck|idle|human)",
                    class_part.trim()
                )
            })?;
            Ok(Some(Self::State { pane_id, class }))
        } else if let Some(rest) = spec.strip_prefix("quiescence:") {
            // quiescence:<pane>[:<idle_ms>]
            let mut parts = rest.splitn(2, ':');
            let pane_part = parts.next().unwrap_or("");
            let pane_id = pane_part.trim().parse::<u64>().map_err(|_| {
                format!("condition `{spec}`: invalid pane id `{}`", pane_part.trim())
            })?;
            let idle_ms = match parts.next() {
                Some(ms) if !ms.trim().is_empty() => {
                    Some(ms.trim().parse::<u64>().map_err(|_| {
                        format!("condition `{spec}`: invalid idle_ms `{}`", ms.trim())
                    })?)
                }
                _ => None,
            };
            Ok(Some(Self::Quiescence { pane_id, idle_ms }))
        } else {
            Ok(None)
        }
    }

    /// The pane id this condition targets.
    #[must_use]
    pub fn pane_id(&self) -> u64 {
        match self {
            Self::State { pane_id, .. } | Self::Quiescence { pane_id, .. } => *pane_id,
        }
    }

    /// Evaluate the condition against an observed pane snapshot. `observed_state`
    /// is the pane's current classification (or `None` if unknown), and
    /// `last_output_ms`/`now_ms` drive the quiescence window. Returns whether
    /// the condition is currently satisfied.
    #[must_use]
    pub fn matches(
        &self,
        observed_state: Option<AgentPaneState>,
        last_output_ms: u64,
        now_ms: u64,
        config: &AgentDetectionConfig,
    ) -> bool {
        match self {
            Self::State { class, .. } => observed_state == Some(*class),
            Self::Quiescence { idle_ms, .. } => {
                let threshold = idle_ms.unwrap_or(config.idle_silence_ms);
                pane_is_quiescent(last_output_ms, now_ms, threshold)
            }
        }
    }
}

/// Pure quiescence predicate for `ft robot await quiescence:<pane>`
/// (ft-7h5da.4.3): a pane is quiescent when no output has been observed for at
/// least `quiescent_after_ms`. Derivable from the latest captured segment's
/// timestamp, so it needs no in-process watcher gauge. `now_ms < last_output_ms`
/// (clock skew) saturates to "not quiescent".
#[must_use]
pub fn pane_is_quiescent(last_output_ms: u64, now_ms: u64, quiescent_after_ms: u64) -> bool {
    now_ms.saturating_sub(last_output_ms) >= quiescent_after_ms
}

/// Storage-derived quiescence for `ft robot await quiescence:<pane>` (ft-r0977).
///
/// `last_output_ms` is the latest captured output-segment timestamp for the
/// pane (e.g. `StorageHandle::pane_last_output_at`), or `None` when the pane has
/// no captured output at all. A pane with no captured output is treated as
/// quiescent: it has, by definition, produced no output within the last
/// `quiescent_after_ms` (the limit case of [`pane_is_quiescent`]). When a
/// timestamp is present this delegates to [`pane_is_quiescent`], so clock skew
/// (`now_ms < last_output_ms`) still saturates to "not quiescent".
#[must_use]
pub fn pane_is_quiescent_from_last_output(
    last_output_ms: Option<u64>,
    now_ms: u64,
    quiescent_after_ms: u64,
) -> bool {
    match last_output_ms {
        None => true,
        Some(last_output_ms) => pane_is_quiescent(last_output_ms, now_ms, quiescent_after_ms),
    }
}

/// Configuration for agent pane state detection thresholds.
///
/// All durations are in milliseconds. Maps to the `[agent_detection]` section
/// in `ft.toml`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
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
    fn agent_pane_state_token_round_trips() {
        for state in [
            AgentPaneState::Active,
            AgentPaneState::Thinking,
            AgentPaneState::Stuck,
            AgentPaneState::Idle,
            AgentPaneState::Human,
        ] {
            assert_eq!(AgentPaneState::from_token(state.as_token()), Some(state));
        }
        // Case-insensitive + whitespace-tolerant.
        assert_eq!(
            AgentPaneState::from_token("  STUCK "),
            Some(AgentPaneState::Stuck)
        );
        assert_eq!(
            AgentPaneState::from_token("Idle"),
            Some(AgentPaneState::Idle)
        );
        // Unknown token rejected.
        assert_eq!(AgentPaneState::from_token("bogus"), None);
        assert_eq!(AgentPaneState::from_token(""), None);
    }

    #[test]
    fn pane_is_quiescent_threshold() {
        // Exactly at the threshold counts as quiescent.
        assert!(pane_is_quiescent(0, 60_000, 60_000));
        assert!(pane_is_quiescent(1_000, 61_000, 60_000));
        // Below threshold is not quiescent.
        assert!(!pane_is_quiescent(0, 59_999, 60_000));
        // Clock skew (now < last_output) saturates to not-quiescent.
        assert!(!pane_is_quiescent(100_000, 50_000, 10_000));
    }

    #[test]
    fn pane_is_quiescent_from_last_output_semantics() {
        // No captured output → quiescent (no output within the window).
        assert!(pane_is_quiescent_from_last_output(None, 0, 60_000));
        assert!(pane_is_quiescent_from_last_output(None, 1_000_000, 60_000));
        // Present timestamp delegates to pane_is_quiescent: idle past the
        // threshold is quiescent.
        assert!(pane_is_quiescent_from_last_output(
            Some(1_000),
            61_000,
            60_000
        ));
        // Recent output (below threshold) is not quiescent.
        assert!(!pane_is_quiescent_from_last_output(
            Some(10_000),
            59_999,
            60_000
        ));
        // Clock skew still saturates to not-quiescent via the delegate.
        assert!(!pane_is_quiescent_from_last_output(
            Some(100_000),
            50_000,
            10_000
        ));
    }

    #[test]
    fn await_pane_condition_parse_state() {
        let cond = AwaitPaneCondition::parse("state:7:stuck").unwrap().unwrap();
        assert_eq!(
            cond,
            AwaitPaneCondition::State {
                pane_id: 7,
                class: AgentPaneState::Stuck
            }
        );
        assert_eq!(cond.pane_id(), 7);
        // Unknown class → precise error, not a silent block.
        assert!(AwaitPaneCondition::parse("state:7:bogus").is_err());
        // Missing class.
        assert!(AwaitPaneCondition::parse("state:7").is_err());
        // Bad pane id.
        assert!(AwaitPaneCondition::parse("state:x:idle").is_err());
    }

    #[test]
    fn await_pane_condition_parse_quiescence() {
        assert_eq!(
            AwaitPaneCondition::parse("quiescence:3").unwrap().unwrap(),
            AwaitPaneCondition::Quiescence {
                pane_id: 3,
                idle_ms: None
            }
        );
        assert_eq!(
            AwaitPaneCondition::parse("quiescence:3:2500")
                .unwrap()
                .unwrap(),
            AwaitPaneCondition::Quiescence {
                pane_id: 3,
                idle_ms: Some(2500)
            }
        );
        assert!(AwaitPaneCondition::parse("quiescence:x").is_err());
        assert!(AwaitPaneCondition::parse("quiescence:3:bad").is_err());
        // Non-state/quiescence specs return Ok(None) for the caller to handle.
        assert_eq!(AwaitPaneCondition::parse("rule:codex.*").unwrap(), None);
    }

    #[test]
    fn await_pane_condition_matches() {
        let config = AgentDetectionConfig::default();
        let state_cond = AwaitPaneCondition::State {
            pane_id: 1,
            class: AgentPaneState::Stuck,
        };
        assert!(state_cond.matches(Some(AgentPaneState::Stuck), 0, 0, &config));
        assert!(!state_cond.matches(Some(AgentPaneState::Active), 0, 0, &config));
        // Unknown observed state never matches.
        assert!(!state_cond.matches(None, 0, 0, &config));

        let quiet = AwaitPaneCondition::Quiescence {
            pane_id: 1,
            idle_ms: Some(5_000),
        };
        assert!(quiet.matches(None, 10_000, 20_000, &config)); // 10s silence ≥ 5s
        assert!(!quiet.matches(None, 18_000, 20_000, &config)); // 2s silence < 5s
        // Default threshold falls back to config.idle_silence_ms.
        let quiet_default = AwaitPaneCondition::Quiescence {
            pane_id: 1,
            idle_ms: None,
        };
        assert!(quiet_default.matches(None, 0, config.idle_silence_ms, &config));
    }

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
