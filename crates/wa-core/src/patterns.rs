//! Pattern detection engine
//!
//! Provides fast, reliable detection of agent state transitions.

use serde::{Deserialize, Serialize};

/// Agent types we support
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentType {
    /// Codex CLI (OpenAI)
    Codex,
    /// Claude Code (Anthropic)
    ClaudeCode,
    /// Gemini CLI (Google)
    Gemini,
    /// Unknown agent
    Unknown,
}

/// Detection severity levels
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    /// Informational
    Info,
    /// Warning - attention needed
    Warning,
    /// Critical - immediate action needed
    Critical,
}

/// A detected pattern match
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Detection {
    /// Stable rule identifier (e.g., "core.codex:usage_reached")
    pub rule_id: String,
    /// Agent type this detection applies to
    pub agent_type: AgentType,
    /// Type of event detected
    pub event_type: String,
    /// Severity level
    pub severity: Severity,
    /// Confidence score 0.0-1.0
    pub confidence: f64,
    /// Extracted structured data
    pub extracted: serde_json::Value,
    /// Original matched text
    pub matched_text: String,
}

/// Pattern engine for detecting agent state transitions
pub struct PatternEngine {
    /// Whether the engine is initialized
    initialized: bool,
}

impl Default for PatternEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl PatternEngine {
    /// Create a new pattern engine with default packs
    #[must_use]
    pub fn new() -> Self {
        // TODO: Initialize patterns
        Self { initialized: true }
    }

    /// Check if the engine is initialized
    #[must_use]
    pub fn is_initialized(&self) -> bool {
        self.initialized
    }

    /// Detect patterns in text
    #[must_use]
    pub fn detect(&self, _text: &str) -> Vec<Detection> {
        // TODO: Implement pattern matching
        Vec::new()
    }

    /// Quick reject check - returns false if text definitely has no matches
    #[must_use]
    pub fn quick_reject(&self, _text: &str) -> bool {
        // TODO: Implement memchr-based quick reject
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn engine_can_be_created() {
        let engine = PatternEngine::new();
        assert!(engine.is_initialized());
    }

    #[test]
    fn detect_returns_empty_for_now() {
        let engine = PatternEngine::new();
        let detections = engine.detect("some text");
        assert!(detections.is_empty());
    }
}
