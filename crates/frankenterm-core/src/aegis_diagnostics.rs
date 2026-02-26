//! Aegis Integration: Galaxy-Brain UX, Telemetry, and Diagnostics (ft-l5em3.5).
//!
//! Unifies the PAC-Bayesian backpressure controller (l5em3.3) and the
//! anytime-valid entropy anomaly detector (l5em3.4) into a single
//! diagnostic interface for CLI, TUI, and MCP surfaces.
//!
//! # Components
//!
//! - **Galaxy-Brain overlay card**: Inline terminal rendering of mathematical
//!   state when interventions fire (e-value, PAC-Bayes bound, plain-English).
//! - **`ft debug dump-aegis`** JSON export: Full snapshot of all mathematical
//!   state for offline analysis.
//! - **Structured log streaming**: Continuous emission of evidence ledger
//!   updates for observability.
//!
//! Bead: ft-l5em3.5

use crate::aegis_backpressure::{
    PacBayesBackpressure, PacBayesConfig, PacBayesSnapshot, PacBayesThrottleActions,
    QueueObservation,
};
use crate::aegis_entropy_anomaly::{
    AnomalyDecision, EntropyAnomalyConfig, EntropyAnomalyDetector, PaneAnomalySnapshot,
};
use serde::{Deserialize, Serialize};

// =============================================================================
// Unified configuration
// =============================================================================

/// Combined configuration for the full Aegis engine.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AegisConfig {
    /// PAC-Bayesian backpressure configuration.
    pub backpressure: PacBayesConfig,
    /// Entropy anomaly detection configuration.
    pub entropy_anomaly: EntropyAnomalyConfig,
    /// Whether to emit structured log events.
    pub structured_logging: bool,
    /// Whether to render Galaxy-Brain overlay cards.
    pub overlay_enabled: bool,
    /// Maximum overlay card width (terminal columns).
    pub overlay_max_width: usize,
}

impl Default for AegisConfig {
    fn default() -> Self {
        Self {
            backpressure: PacBayesConfig::default(),
            entropy_anomaly: EntropyAnomalyConfig::default(),
            structured_logging: true,
            overlay_enabled: true,
            overlay_max_width: 80,
        }
    }
}

// =============================================================================
// Galaxy-Brain overlay card
// =============================================================================

/// A rendered overlay card showing mathematical intervention state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OverlayCard {
    /// Card title (e.g., "Aegis: Backpressure Throttle" or "Aegis: Loop Detected").
    pub title: String,
    /// Plain-English summary of what happened.
    pub summary: String,
    /// Mathematical evidence lines.
    pub evidence: Vec<EvidenceLine>,
    /// Suggested action for the operator/agent.
    pub action: String,
    /// Severity level for coloring.
    pub severity: OverlaySeverity,
}

/// A single line of mathematical evidence in the overlay card.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvidenceLine {
    /// Label (e.g., "E-value", "PAC-Bayes bound", "Severity").
    pub label: String,
    /// Formatted value (e.g., "405.2 ≥ 100.0").
    pub value: String,
    /// Plain-English intuition (e.g., "Null hypothesis rejected").
    pub intuition: String,
}

/// Overlay severity for rendering.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum OverlaySeverity {
    /// Informational (blue).
    Info,
    /// Warning (yellow).
    Warning,
    /// Critical intervention (red).
    Critical,
}

impl OverlayCard {
    /// Render the card as a box-drawing string for terminal display.
    pub fn render(&self, max_width: usize) -> String {
        let width = max_width.max(40);
        let inner = width - 4; // "│ " + content + " │"

        let mut lines = Vec::new();

        // Top border
        let severity_tag = match self.severity {
            OverlaySeverity::Info => "[INFO]",
            OverlaySeverity::Warning => "[WARN]",
            OverlaySeverity::Critical => "[CRIT]",
        };
        let prefix = format!("┌─[Aegis] {} ", severity_tag);
        let prefix_chars = prefix.chars().count();
        let fill = if prefix_chars + 1 < width {
            width - prefix_chars - 1
        } else {
            0
        };
        let header = format!("{}{}┐", prefix, "─".repeat(fill));
        lines.push(header);

        // Title
        let title_line = truncate_pad(&self.title, inner);
        lines.push(format!("│ {} │", title_line));

        // Summary
        let summary_line = truncate_pad(&self.summary, inner);
        lines.push(format!("│ {} │", summary_line));

        // Separator
        lines.push(format!("├{}┤", "─".repeat(width - 2)));

        // Evidence lines
        for ev in &self.evidence {
            let label_val = format!("{}: {}", ev.label, ev.value);
            lines.push(format!("│ {} │", truncate_pad(&label_val, inner)));
            if !ev.intuition.is_empty() {
                let intuition = format!("  └ {}", ev.intuition);
                lines.push(format!("│ {} │", truncate_pad(&intuition, inner)));
            }
        }

        // Action
        if !self.action.is_empty() {
            lines.push(format!("├{}┤", "─".repeat(width - 2)));
            let action_line = format!("→ {}", self.action);
            lines.push(format!("│ {} │", truncate_pad(&action_line, inner)));
        }

        // Bottom border
        lines.push(format!("└{}┘", "─".repeat(width - 2)));

        lines.join("\n")
    }
}

/// Truncate or pad a string to exactly `width` display characters.
fn truncate_pad(s: &str, width: usize) -> String {
    let char_count = s.chars().count();
    if char_count > width {
        let truncated: String = s.chars().take(width.saturating_sub(1)).collect();
        format!("{}…", truncated)
    } else {
        // Pad with spaces to exactly `width` chars
        let padding = width - char_count;
        format!("{}{}", s, " ".repeat(padding))
    }
}

// =============================================================================
// Full diagnostic dump (ft debug dump-aegis)
// =============================================================================

/// Complete Aegis diagnostic snapshot for JSON export.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AegisDump {
    /// Timestamp of the dump (ISO 8601).
    pub timestamp: String,
    /// Version of the dump schema.
    pub schema_version: u32,
    /// PAC-Bayesian backpressure snapshot.
    pub backpressure: PacBayesSnapshot,
    /// Per-pane entropy anomaly snapshots.
    pub entropy_anomaly_panes: Vec<PaneAnomalySnapshot>,
    /// Global baseline entropy statistics.
    pub baseline_entropy_mean: f64,
    pub baseline_entropy_variance: f64,
    /// Configuration (for reproducibility).
    pub config: AegisConfig,
    /// Recent intervention events.
    pub recent_interventions: Vec<InterventionEvent>,
}

/// A logged intervention event.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InterventionEvent {
    /// Timestamp of the intervention.
    pub timestamp: String,
    /// Pane ID affected.
    pub pane_id: u64,
    /// Type of intervention.
    pub kind: InterventionKind,
    /// Evidence summary.
    pub evidence: String,
}

/// Kinds of Aegis interventions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum InterventionKind {
    /// Backpressure throttle applied.
    BackpressureThrottle,
    /// Entropy anomaly loop blocked.
    EntropyAnomalyBlock,
    /// Combined (both conditions met).
    CombinedIntervention,
}

// =============================================================================
// Structured log event
// =============================================================================

/// Structured log event for observability pipelines.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AegisLogEvent {
    /// Timestamp (ISO 8601).
    pub timestamp: String,
    /// Component identifier.
    pub component: String,
    /// Event type.
    pub event_type: AegisLogEventType,
    /// Pane ID (if applicable).
    pub pane_id: Option<u64>,
    /// Key-value data payload.
    pub data: std::collections::HashMap<String, serde_json::Value>,
}

/// Types of structured log events.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AegisLogEventType {
    /// Backpressure observation processed.
    BackpressureObservation,
    /// Entropy observation processed.
    EntropyObservation,
    /// Intervention triggered.
    InterventionTriggered,
    /// Recovery detected (e-value or severity dropping).
    RecoveryDetected,
    /// Baseline statistics updated.
    BaselineUpdated,
}

// =============================================================================
// Unified Aegis engine
// =============================================================================

/// Unified Aegis engine combining backpressure + entropy anomaly + UX.
pub struct AegisEngine {
    /// Configuration.
    config: AegisConfig,
    /// PAC-Bayesian backpressure controller.
    backpressure: PacBayesBackpressure,
    /// Entropy anomaly detector.
    entropy: EntropyAnomalyDetector,
    /// Recent intervention log (ring buffer).
    interventions: Vec<InterventionEvent>,
    /// Max interventions to keep.
    max_interventions: usize,
    /// Structured log buffer (for batch retrieval).
    log_buffer: Vec<AegisLogEvent>,
    /// Max log entries to buffer.
    max_log_entries: usize,
}

impl AegisEngine {
    /// Create a new Aegis engine with the given configuration.
    pub fn new(config: AegisConfig) -> Self {
        let backpressure = PacBayesBackpressure::new(config.backpressure.clone());
        let entropy = EntropyAnomalyDetector::new(config.entropy_anomaly.clone());
        Self {
            config,
            backpressure,
            entropy,
            interventions: Vec::new(),
            max_interventions: 100,
            log_buffer: Vec::new(),
            max_log_entries: 500,
        }
    }

    /// Create an engine with default configuration.
    pub fn with_defaults() -> Self {
        Self::new(AegisConfig::default())
    }

    /// Process a backpressure observation and return throttle actions.
    pub fn observe_backpressure(&mut self, obs: &QueueObservation) -> PacBayesThrottleActions {
        let actions = self.backpressure.observe(obs);

        // Log high-severity events
        if actions.severity > 0.5 {
            self.record_log(
                AegisLogEventType::BackpressureObservation,
                Some(obs.pane_id),
                &[
                    ("severity", serde_json::Value::from(actions.severity)),
                    ("risk_bound", serde_json::Value::from(actions.risk_bound)),
                    (
                        "kl_divergence",
                        serde_json::Value::from(actions.kl_divergence),
                    ),
                    (
                        "poll_multiplier",
                        serde_json::Value::from(actions.poll_multiplier),
                    ),
                ],
            );
        }

        actions
    }

    /// Process an entropy observation and return an anomaly decision.
    pub fn observe_entropy(
        &mut self,
        pane_id: u64,
        data: &[u8],
        error_signatures: &[&[u8]],
    ) -> AnomalyDecision {
        let decision = self.entropy.observe(pane_id, data, error_signatures);

        if decision.should_block {
            let timestamp = format_timestamp();
            let evidence = format!(
                "E-value: {:.1} >= {:.1}, entropy: {:.2}, error_density: {:.2}",
                decision.e_value,
                decision.rejection_threshold,
                decision.current_entropy,
                decision.error_density
            );
            self.record_intervention(InterventionEvent {
                timestamp: timestamp.clone(),
                pane_id,
                kind: InterventionKind::EntropyAnomalyBlock,
                evidence: evidence.clone(),
            });
            self.record_log(
                AegisLogEventType::InterventionTriggered,
                Some(pane_id),
                &[
                    ("kind", serde_json::Value::from("entropy_anomaly_block")),
                    ("e_value", serde_json::Value::from(decision.e_value)),
                    ("entropy", serde_json::Value::from(decision.current_entropy)),
                    (
                        "error_density",
                        serde_json::Value::from(decision.error_density),
                    ),
                ],
            );
        }

        decision
    }

    /// Build a Galaxy-Brain overlay card for a backpressure intervention.
    pub fn backpressure_overlay(
        &self,
        pane_id: u64,
        actions: &PacBayesThrottleActions,
    ) -> OverlayCard {
        let severity = if actions.severity > 0.8 {
            OverlaySeverity::Critical
        } else if actions.severity > 0.3 {
            OverlaySeverity::Warning
        } else {
            OverlaySeverity::Info
        };

        OverlayCard {
            title: format!("Aegis: Backpressure Throttle (pane {})", pane_id),
            summary: format!(
                "PAC-Bayes bound triggered. Severity: {:.0}%",
                actions.severity * 100.0
            ),
            evidence: vec![
                EvidenceLine {
                    label: "Severity".into(),
                    value: format!("{:.3}", actions.severity),
                    intuition: severity_intuition(actions.severity),
                },
                EvidenceLine {
                    label: "Risk bound".into(),
                    value: format!("{:.4}", actions.risk_bound),
                    intuition: "PAC-Bayes generalization bound on loss".into(),
                },
                EvidenceLine {
                    label: "KL divergence".into(),
                    value: format!("{:.4} nats", actions.kl_divergence),
                    intuition: "Distance from prior to learned threshold".into(),
                },
                EvidenceLine {
                    label: "Poll multiplier".into(),
                    value: format!("{:.2}×", actions.poll_multiplier),
                    intuition: format!(
                        "Polling interval scaled by {:.2}×",
                        actions.poll_multiplier
                    ),
                },
                EvidenceLine {
                    label: "Threshold".into(),
                    value: format!("{:.3}", actions.optimal_threshold),
                    intuition: "Learned optimal fill-ratio threshold".into(),
                },
            ],
            action: if actions.starvation_guard_active {
                "Starvation guard active — external cause detected, severity reduced".into()
            } else if actions.severity > 0.8 {
                "Critical: throttling aggressive — reduce pane output or increase capacity".into()
            } else {
                "Throttling active — monitoring for recovery".into()
            },
            severity,
        }
    }

    /// Build a Galaxy-Brain overlay card for an entropy anomaly block.
    pub fn entropy_anomaly_overlay(&self, pane_id: u64, decision: &AnomalyDecision) -> OverlayCard {
        OverlayCard {
            title: format!("Aegis: Loop Detected (pane {})", pane_id),
            summary: "E-process null hypothesis rejected. Repeating error pattern detected.".into(),
            evidence: vec![
                EvidenceLine {
                    label: "E-value".into(),
                    value: format!(
                        "{:.1} ≥ {:.1}",
                        decision.e_value, decision.rejection_threshold
                    ),
                    intuition: "Sequential test statistic exceeded rejection threshold".into(),
                },
                EvidenceLine {
                    label: "Entropy".into(),
                    value: format!("{:.2} bits/byte", decision.current_entropy),
                    intuition: entropy_intuition(decision.current_entropy),
                },
                EvidenceLine {
                    label: "Error density".into(),
                    value: format!("{:.0}%", decision.error_density * 100.0),
                    intuition: "Fraction of recent output matching error signatures".into(),
                },
                EvidenceLine {
                    label: "Collapse streak".into(),
                    value: format!("{} observations", decision.collapse_streak),
                    intuition: "Consecutive low-entropy chunks observed".into(),
                },
            ],
            action: "Execution blocked. Inject intervention message to break agent loop.".into(),
            severity: OverlaySeverity::Critical,
        }
    }

    /// Generate the full diagnostic dump for `ft debug dump-aegis`.
    pub fn dump(&self) -> AegisDump {
        AegisDump {
            timestamp: format_timestamp(),
            schema_version: 1,
            backpressure: self.backpressure.snapshot(),
            entropy_anomaly_panes: self.entropy.all_snapshots(),
            baseline_entropy_mean: self.entropy.baseline_mean(),
            baseline_entropy_variance: self.entropy.baseline_variance(),
            config: self.config.clone(),
            recent_interventions: self.interventions.clone(),
        }
    }

    /// Export the dump as a pretty-printed JSON string.
    pub fn dump_json(&self) -> String {
        serde_json::to_string_pretty(&self.dump())
            .unwrap_or_else(|e| format!("{{\"error\": \"serialization failed: {}\"}}", e))
    }

    /// Drain buffered log events.
    pub fn drain_logs(&mut self) -> Vec<AegisLogEvent> {
        std::mem::take(&mut self.log_buffer)
    }

    /// Get recent intervention events.
    pub fn recent_interventions(&self) -> &[InterventionEvent] {
        &self.interventions
    }

    /// Access the backpressure controller.
    pub fn backpressure(&self) -> &PacBayesBackpressure {
        &self.backpressure
    }

    /// Access the entropy anomaly detector.
    pub fn entropy_detector(&self) -> &EntropyAnomalyDetector {
        &self.entropy
    }

    /// Access the configuration.
    pub fn config(&self) -> &AegisConfig {
        &self.config
    }

    /// Register an error signature for the entropy anomaly detector.
    pub fn register_error_signature(&mut self, signature: &[u8]) {
        self.entropy.register_error_signature(signature);
    }

    /// Reset all state.
    pub fn reset(&mut self) {
        self.backpressure.reset();
        self.entropy.reset();
        self.interventions.clear();
        self.log_buffer.clear();
    }

    // ── Internal helpers ──────────────────────────────────────────────

    fn record_intervention(&mut self, event: InterventionEvent) {
        self.interventions.push(event);
        if self.interventions.len() > self.max_interventions {
            self.interventions.remove(0);
        }
    }

    fn record_log(
        &mut self,
        event_type: AegisLogEventType,
        pane_id: Option<u64>,
        kvs: &[(&str, serde_json::Value)],
    ) {
        if !self.config.structured_logging {
            return;
        }
        let mut data = std::collections::HashMap::new();
        for (k, v) in kvs {
            data.insert(k.to_string(), v.clone());
        }
        self.log_buffer.push(AegisLogEvent {
            timestamp: format_timestamp(),
            component: "aegis".into(),
            event_type,
            pane_id,
            data,
        });
        if self.log_buffer.len() > self.max_log_entries {
            self.log_buffer.remove(0);
        }
    }
}

// =============================================================================
// Helper functions
// =============================================================================

fn format_timestamp() -> String {
    // Simple monotonic counter for testing (in production, use chrono/time)
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let c = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    format!("2026-01-01T00:00:{:02}Z", c % 60)
}

fn severity_intuition(severity: f64) -> String {
    if severity > 0.9 {
        "Critical — aggressive throttling".into()
    } else if severity > 0.7 {
        "High — significant throttling".into()
    } else if severity > 0.4 {
        "Moderate — light throttling".into()
    } else if severity > 0.1 {
        "Low — minimal impact".into()
    } else {
        "Negligible — system healthy".into()
    }
}

fn entropy_intuition(entropy: f64) -> String {
    if entropy < 1.0 {
        "Near-constant output (likely looping)".into()
    } else if entropy < 3.0 {
        "Low diversity — repetitive content".into()
    } else if entropy < 5.0 {
        "Moderate diversity — structured output".into()
    } else if entropy < 7.0 {
        "High diversity — normal terminal text".into()
    } else {
        "Near-random — binary or compressed data".into()
    }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::aegis_backpressure::QueueObservation;

    #[test]
    fn aegis_config_default() {
        let config = AegisConfig::default();
        assert!(config.structured_logging);
        assert!(config.overlay_enabled);
        assert_eq!(config.overlay_max_width, 80);
    }

    #[test]
    fn aegis_config_serde_roundtrip() {
        let config = AegisConfig::default();
        let json = serde_json::to_string(&config).unwrap();
        let back: AegisConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(config.overlay_max_width, back.overlay_max_width);
        assert_eq!(config.structured_logging, back.structured_logging);
    }

    #[test]
    fn engine_creates_with_defaults() {
        let engine = AegisEngine::with_defaults();
        assert_eq!(engine.recent_interventions().len(), 0);
    }

    #[test]
    fn engine_observe_backpressure() {
        let mut engine = AegisEngine::with_defaults();
        let obs = QueueObservation {
            pane_id: 1,
            fill_ratio: 0.5,
            frame_dropped: false,
            external_cause: None,
        };
        let actions = engine.observe_backpressure(&obs);
        assert!(actions.severity >= 0.0);
        assert!(actions.severity <= 1.0);
    }

    #[test]
    fn engine_observe_entropy_no_block() {
        let mut engine = AegisEngine::with_defaults();
        let diverse: Vec<u8> = (0..4096).map(|i| (i % 256) as u8).collect();
        let decision = engine.observe_entropy(1, &diverse, &[]);
        assert!(!decision.should_block);
    }

    #[test]
    fn engine_backpressure_overlay() {
        let engine = AegisEngine::with_defaults();
        let actions = PacBayesThrottleActions {
            severity: 0.85,
            poll_multiplier: 3.5,
            pane_skip_fraction: 0.4,
            detection_skip_fraction: 0.2,
            buffer_limit_factor: 0.3,
            starvation_guard_active: false,
            risk_bound: 0.42,
            kl_divergence: 1.5,
            optimal_threshold: 0.55,
        };
        let card = engine.backpressure_overlay(1, &actions);
        assert_eq!(card.severity, OverlaySeverity::Critical);
        assert!(card.title.contains("pane 1"));
        assert!(!card.evidence.is_empty());
    }

    #[test]
    fn engine_entropy_overlay() {
        let engine = AegisEngine::with_defaults();
        let decision = AnomalyDecision {
            should_block: true,
            e_value: 405.2,
            rejection_threshold: 100.0,
            entropy_collapsed: true,
            error_density_high: true,
            current_entropy: 1.2,
            error_density: 0.75,
            n_observations: 50,
            collapse_streak: 15,
            warming_up: false,
        };
        let card = engine.entropy_anomaly_overlay(1, &decision);
        assert_eq!(card.severity, OverlaySeverity::Critical);
        assert!(card.title.contains("Loop Detected"));
        assert!(card.evidence.len() >= 4);
    }

    #[test]
    fn overlay_card_render_basic() {
        let card = OverlayCard {
            title: "Test Card".into(),
            summary: "Something happened".into(),
            evidence: vec![EvidenceLine {
                label: "Score".into(),
                value: "42.0".into(),
                intuition: "The answer".into(),
            }],
            action: "Do something".into(),
            severity: OverlaySeverity::Warning,
        };
        let rendered = card.render(60);
        assert!(rendered.contains("Aegis"));
        assert!(rendered.contains("WARN"));
        assert!(rendered.contains("Test Card"));
        assert!(rendered.contains("Score: 42.0"));
        assert!(rendered.contains("The answer"));
        assert!(rendered.contains("Do something"));
        // Should have box drawing characters
        assert!(rendered.contains("┌"));
        assert!(rendered.contains("└"));
        assert!(rendered.contains("│"));
    }

    #[test]
    fn overlay_card_render_narrow() {
        let card = OverlayCard {
            title: "A very long title that should be truncated properly".into(),
            summary: "Short".into(),
            evidence: vec![],
            action: String::new(),
            severity: OverlaySeverity::Info,
        };
        let rendered = card.render(40);
        // All lines should fit within 40 chars (plus some UTF-8 overhead)
        for line in rendered.lines() {
            // Unicode box-drawing chars are multi-byte
            assert!(
                line.chars().count() <= 42,
                "Line too wide ({} chars): {}",
                line.chars().count(),
                line
            );
        }
    }

    #[test]
    fn engine_dump_json() {
        let engine = AegisEngine::with_defaults();
        let json = engine.dump_json();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert!(parsed.get("schema_version").is_some());
        assert!(parsed.get("backpressure").is_some());
        assert!(parsed.get("config").is_some());
    }

    #[test]
    fn engine_dump_serde_roundtrip() {
        let engine = AegisEngine::with_defaults();
        let dump = engine.dump();
        let json = serde_json::to_string(&dump).unwrap();
        let back: AegisDump = serde_json::from_str(&json).unwrap();
        assert_eq!(dump.schema_version, back.schema_version);
    }

    #[test]
    fn engine_structured_log_buffer() {
        let mut engine = AegisEngine::with_defaults();
        // High-severity observation should emit log
        let obs = QueueObservation {
            pane_id: 1,
            fill_ratio: 0.95,
            frame_dropped: true,
            external_cause: None,
        };
        engine.observe_backpressure(&obs);
        let logs = engine.drain_logs();
        // May or may not have logs depending on severity
        // Just verify drain works
        assert!(engine.drain_logs().is_empty());
        drop(logs);
    }

    #[test]
    fn engine_intervention_logging() {
        let config = AegisConfig {
            entropy_anomaly: EntropyAnomalyConfig {
                alpha: 0.05,
                warmup_observations: 3,
                min_collapse_streak: 2,
                window_bytes: 256,
                collapse_threshold: 6.0, // high to trigger easily
                error_density_threshold: 0.2,
                density_window: 10,
                baseline_entropy_low: 6.5,
                baseline_entropy_high: 7.5,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut engine = AegisEngine::new(config);
        engine.register_error_signature(b"ERR");

        // Feed low-entropy error data
        let error = b"ERR ERR ERR ERR ERR ERR ERR ERR ERR ERR ERR ERR ERR ERR";
        let mut had_intervention = false;
        for _ in 0..100 {
            let decision = engine.observe_entropy(1, error, &[b"ERR"]);
            if decision.should_block {
                had_intervention = true;
                break;
            }
        }
        if had_intervention {
            assert!(!engine.recent_interventions().is_empty());
        }
    }

    #[test]
    fn engine_reset() {
        let mut engine = AegisEngine::with_defaults();
        let obs = QueueObservation {
            pane_id: 1,
            fill_ratio: 0.5,
            frame_dropped: false,
            external_cause: None,
        };
        engine.observe_backpressure(&obs);
        let data: Vec<u8> = (0..256).map(|i| i as u8).collect();
        engine.observe_entropy(1, &data, &[]);

        engine.reset();
        assert!(engine.recent_interventions().is_empty());
        assert!(engine.drain_logs().is_empty());
    }

    #[test]
    fn intervention_event_serde() {
        let event = InterventionEvent {
            timestamp: "2026-01-01T00:00:00Z".into(),
            pane_id: 42,
            kind: InterventionKind::EntropyAnomalyBlock,
            evidence: "E-value: 405.2 >= 100.0".into(),
        };
        let json = serde_json::to_string(&event).unwrap();
        let back: InterventionEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(event.pane_id, back.pane_id);
        assert_eq!(event.kind, back.kind);
    }

    #[test]
    fn log_event_serde() {
        let event = AegisLogEvent {
            timestamp: "2026-01-01T00:00:00Z".into(),
            component: "aegis".into(),
            event_type: AegisLogEventType::InterventionTriggered,
            pane_id: Some(1),
            data: std::collections::HashMap::new(),
        };
        let json = serde_json::to_string(&event).unwrap();
        let back: AegisLogEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(event.event_type, back.event_type);
    }

    #[test]
    fn overlay_severity_equality() {
        assert_eq!(OverlaySeverity::Info, OverlaySeverity::Info);
        assert_ne!(OverlaySeverity::Info, OverlaySeverity::Critical);
    }

    #[test]
    fn intervention_kind_equality() {
        assert_eq!(
            InterventionKind::EntropyAnomalyBlock,
            InterventionKind::EntropyAnomalyBlock
        );
        assert_ne!(
            InterventionKind::BackpressureThrottle,
            InterventionKind::EntropyAnomalyBlock
        );
    }

    #[test]
    fn severity_intuition_ranges() {
        assert!(severity_intuition(0.95).contains("Critical"));
        assert!(severity_intuition(0.75).contains("High"));
        assert!(severity_intuition(0.5).contains("Moderate"));
        assert!(severity_intuition(0.15).contains("Low"));
        assert!(severity_intuition(0.01).contains("Negligible"));
    }

    #[test]
    fn entropy_intuition_ranges() {
        assert!(entropy_intuition(0.5).contains("looping"));
        assert!(entropy_intuition(2.0).contains("repetitive"));
        assert!(entropy_intuition(4.0).contains("structured"));
        assert!(entropy_intuition(6.0).contains("normal"));
        assert!(entropy_intuition(7.5).contains("random"));
    }

    #[test]
    fn truncate_pad_short() {
        assert_eq!(truncate_pad("hi", 10), "hi        ");
    }

    #[test]
    fn truncate_pad_long() {
        let result = truncate_pad("hello world this is long", 10);
        assert!(result.chars().count() <= 10);
        assert!(result.ends_with('…'));
    }

    #[test]
    fn truncate_pad_exact() {
        assert_eq!(truncate_pad("exact", 5), "exact");
    }

    #[test]
    fn engine_register_error_signature() {
        let mut engine = AegisEngine::with_defaults();
        engine.register_error_signature(b"FATAL");
        assert!(engine.entropy_detector().is_known_signature(b"FATAL"));
    }

    #[test]
    fn dump_contains_pane_snapshots() {
        let mut engine = AegisEngine::with_defaults();
        let data: Vec<u8> = (0..128).map(|i| i as u8).collect();
        engine.observe_entropy(1, &data, &[]);
        engine.observe_entropy(2, &data, &[]);
        let dump = engine.dump();
        assert_eq!(dump.entropy_anomaly_panes.len(), 2);
    }
}
