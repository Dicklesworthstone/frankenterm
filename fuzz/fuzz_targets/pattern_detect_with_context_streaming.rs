#![no_main]
//! Stateful PatternEngine::detect_with_context fuzz target.
//!
//! Exercises cross-segment tail buffering and dedup state on repeated and
//! overlapping streamed segments. The oracle is:
//! - no panic on valid rule packs + UTF-8 segments
//! - tail_buffer stays UTF-8-safe and bounded
//! - emitted detection dedup keys remain unique across immediate replays

use std::collections::HashSet;

use arbitrary::Arbitrary;
use frankenterm_core::patterns::{
    AgentType, Detection, DetectionContext, PatternEngine, PatternPack, RuleDef, Severity,
};
use frankenterm_core::tuning_config::PatternsTuning;
use libfuzzer_sys::fuzz_target;

const MAX_RULES: usize = 8;
const MAX_SEGMENTS: usize = 24;
const MAX_SEGMENT_CHARS: usize = 96;
const MAX_ANCHOR_CHARS: usize = 24;

#[derive(Arbitrary, Debug)]
struct FuzzCase {
    rules: Vec<RawRule>,
    segments: Vec<String>,
    context_agent: Option<RawAgentType>,
    pane_id: Option<u16>,
}

#[derive(Arbitrary, Debug, Clone)]
struct RawRule {
    agent_type: RawAgentType,
    severity: RawSeverity,
    anchor: String,
    alternate_anchor: String,
    event_type: String,
    description: String,
    regex_kind: RegexKind,
    capture_width: u8,
}

#[derive(Arbitrary, Debug, Clone, Copy)]
enum RawAgentType {
    Codex,
    ClaudeCode,
    Gemini,
    Wezterm,
}

#[derive(Arbitrary, Debug, Clone, Copy)]
enum RawSeverity {
    Info,
    Warning,
    Critical,
}

#[derive(Arbitrary, Debug, Clone, Copy)]
enum RegexKind {
    None,
    AnchorThenDigits,
    WordThenAnchor,
}

impl RawAgentType {
    fn into_agent_type(self) -> AgentType {
        match self {
            Self::Codex => AgentType::Codex,
            Self::ClaudeCode => AgentType::ClaudeCode,
            Self::Gemini => AgentType::Gemini,
            Self::Wezterm => AgentType::Wezterm,
        }
    }

    fn rule_prefix(self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::ClaudeCode => "claude_code",
            Self::Gemini => "gemini",
            Self::Wezterm => "wezterm",
        }
    }
}

impl RawSeverity {
    fn into_severity(self) -> Severity {
        match self {
            Self::Info => Severity::Info,
            Self::Warning => Severity::Warning,
            Self::Critical => Severity::Critical,
        }
    }
}

impl RawRule {
    fn build_rule(&self, idx: usize) -> RuleDef {
        let agent_type = self.agent_type.into_agent_type();
        let primary_anchor = sanitize_token(&self.anchor, "ALERT");
        let alternate_anchor = sanitize_token(&self.alternate_anchor, &primary_anchor);
        let capture_width = usize::from(self.capture_width % 4);
        let regex = match self.regex_kind {
            RegexKind::None => None,
            RegexKind::AnchorThenDigits => Some(format!(
                r"(?P<num>{primary_anchor}[0-9]{{0,{capture_width}}})"
            )),
            RegexKind::WordThenAnchor => Some(format!(
                r"(?P<prefix>[A-Za-z0-9_]{{0,{capture_width}}}){primary_anchor}"
            )),
        };

        RuleDef {
            id: format!("{}.streaming_{idx}", self.agent_type.rule_prefix()),
            agent_type,
            event_type: limit_text(&self.event_type, "usage.warning"),
            severity: self.severity.into_severity(),
            anchors: vec![primary_anchor, alternate_anchor],
            regex,
            description: limit_text(&self.description, "streaming fuzz rule"),
            remediation: None,
            workflow: None,
            manual_fix: None,
            preview_command: None,
            learn_more_url: None,
        }
    }
}

fn sanitize_token(raw: &str, fallback: &str) -> String {
    let filtered = raw
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric() || *ch == '_')
        .take(MAX_ANCHOR_CHARS)
        .collect::<String>();
    if filtered.is_empty() {
        fallback.to_string()
    } else {
        filtered
    }
}

fn limit_text(raw: &str, fallback: &str) -> String {
    let limited = raw.chars().take(MAX_SEGMENT_CHARS).collect::<String>();
    if limited.trim().is_empty() {
        fallback.to_string()
    } else {
        limited
    }
}

fn split_token(token: &str) -> (String, String) {
    let char_len = token.chars().count();
    let split_chars = (char_len / 2).max(1).min(char_len);
    let split_byte = token
        .char_indices()
        .nth(split_chars)
        .map(|(idx, _)| idx)
        .unwrap_or(token.len());
    let (left, right) = token.split_at(split_byte);
    (left.to_string(), right.to_string())
}

fn build_rules(case: &FuzzCase) -> Vec<RuleDef> {
    let mut rules = case
        .rules
        .iter()
        .take(MAX_RULES)
        .enumerate()
        .map(|(idx, raw)| raw.build_rule(idx))
        .collect::<Vec<_>>();
    if rules.is_empty() {
        rules.push(
            RawRule {
                agent_type: RawAgentType::Codex,
                severity: RawSeverity::Warning,
                anchor: "FULLALERT".to_string(),
                alternate_anchor: "ALERT".to_string(),
                event_type: "usage.warning".to_string(),
                description: "fallback".to_string(),
                regex_kind: RegexKind::AnchorThenDigits,
                capture_width: 2,
            }
            .build_rule(0),
        );
    }
    rules
}

fn build_stream(case: &FuzzCase, rules: &[RuleDef]) -> Vec<String> {
    let mut segments = case
        .segments
        .iter()
        .take(MAX_SEGMENTS)
        .map(|segment| limit_text(segment, "noise"))
        .collect::<Vec<_>>();

    if segments.is_empty() {
        segments.push("noise".to_string());
    }

    for rule in rules.iter().take(3) {
        let anchor = rule
            .anchors
            .first()
            .cloned()
            .unwrap_or_else(|| "ALERT".to_string());
        let (left, right) = split_token(&anchor);
        segments.push(format!("prefix-{left}"));
        segments.push(format!("{right}-suffix"));
        segments.push(format!("inline {anchor} 42"));
    }

    segments.truncate(MAX_SEGMENTS);
    segments
}

fn assert_tail_invariants(ctx: &DetectionContext) {
    assert!(std::str::from_utf8(ctx.tail_buffer.as_bytes()).is_ok());
    assert!(ctx.tail_buffer.len() <= PatternsTuning::DEFAULT_MAX_TAIL_SIZE_BYTES);
}

fn assert_fresh_detection_keys(detections: &[Detection], seen_keys: &mut HashSet<String>) {
    let mut local_keys = HashSet::new();
    for detection in detections {
        let key = detection.dedup_key();
        assert!(local_keys.insert(key.clone()));
        assert!(seen_keys.insert(key));
        assert!(!detection.rule_id.is_empty());
        assert!(std::str::from_utf8(detection.matched_text.as_bytes()).is_ok());
    }
}

fuzz_target!(|data: &[u8]| {
    let Ok(case) = FuzzCase::arbitrary(&mut arbitrary::Unstructured::new(data)) else {
        return;
    };

    let rules = build_rules(&case);
    let pack = PatternPack::new("fuzz.streaming", "0.1.0", rules.clone());
    let engine = PatternEngine::with_packs(vec![pack]).expect("generated pack should validate");
    let stream = build_stream(&case, &rules);

    let mut ctx = match (case.pane_id, case.context_agent) {
        (Some(pane_id), Some(agent)) => {
            DetectionContext::with_pane(u64::from(pane_id), Some(agent.into_agent_type()))
        }
        (Some(pane_id), None) => DetectionContext::with_pane(u64::from(pane_id), None),
        (None, Some(agent)) => DetectionContext::with_agent_type(agent.into_agent_type()),
        (None, None) => DetectionContext::new(),
    };

    let mut seen_keys = HashSet::new();

    for segment in &stream {
        let first_pass = engine.detect_with_context(segment, &mut ctx);
        assert_tail_invariants(&ctx);
        assert_fresh_detection_keys(&first_pass, &mut seen_keys);

        let immediate_replay = engine.detect_with_context(segment, &mut ctx);
        assert_tail_invariants(&ctx);
        assert_fresh_detection_keys(&immediate_replay, &mut seen_keys);
    }

    for segment in &stream {
        let streamed_replay = engine.detect_with_context(segment, &mut ctx);
        assert_tail_invariants(&ctx);
        assert_fresh_detection_keys(&streamed_replay, &mut seen_keys);
    }
});
