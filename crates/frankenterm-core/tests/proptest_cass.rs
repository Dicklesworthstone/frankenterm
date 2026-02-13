//! Property-based tests for the `cass` module.
//!
//! Covers `parse_cass_timestamp_ms` parsing invariants (epoch seconds,
//! epoch milliseconds, RFC3339, rejection of empty/invalid), serde
//! roundtrips for Cass types, and `CassSessionSummary::from_session`
//! aggregation correctness.

use std::collections::HashMap;

use frankenterm_core::cass::{
    CassAgent, CassMessage, CassSearchHit, CassSearchResult, CassSession, CassSessionSummary,
    parse_cass_timestamp_ms,
};
use proptest::prelude::*;

// =========================================================================
// Strategies
// =========================================================================

fn arb_cass_agent() -> impl Strategy<Value = CassAgent> {
    prop_oneof![
        Just(CassAgent::Codex),
        Just(CassAgent::ClaudeCode),
        Just(CassAgent::Gemini),
        Just(CassAgent::Cursor),
        Just(CassAgent::Aider),
        Just(CassAgent::ChatGpt),
    ]
}

fn arb_cass_message() -> impl Strategy<Value = CassMessage> {
    (
        proptest::option::of("user|assistant|system"),
        proptest::option::of("[a-zA-Z ]{5,30}"),
        proptest::option::of(0_u64..100_000),
    )
        .prop_map(|(role, content, token_count)| CassMessage {
            role,
            content,
            timestamp: None,
            token_count,
            extra: HashMap::new(),
        })
}

fn arb_cass_session() -> impl Strategy<Value = CassSession> {
    (
        proptest::option::of("[a-z0-9-]{10,30}"),
        proptest::option::of("codex|claude_code|gemini"),
        proptest::option::of("/[a-z/]{3,20}"),
        proptest::collection::vec(arb_cass_message(), 0..5),
    )
        .prop_map(|(session_id, agent, project_path, messages)| CassSession {
            session_id,
            agent,
            project_path,
            started_at: None,
            ended_at: None,
            messages,
            extra: HashMap::new(),
        })
}

fn arb_cass_session_summary() -> impl Strategy<Value = CassSessionSummary> {
    (
        proptest::option::of(0_i64..1_000_000),
        proptest::option::of(0_i64..500_000),
        proptest::option::of(0_i64..500_000),
        0_usize..100,
    )
        .prop_map(|(total, input, output, message_count)| CassSessionSummary {
            total_tokens: total,
            input_tokens: input,
            output_tokens: output,
            message_count,
            session_started_at_ms: None,
            session_ended_at_ms: None,
            first_message_at_ms: None,
            last_message_at_ms: None,
        })
}

// =========================================================================
// parse_cass_timestamp_ms — epoch integer parsing
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// Large integer values (> 10B) are treated as milliseconds.
    #[test]
    fn prop_large_int_is_millis(ms in 10_000_000_001_i64..100_000_000_000) {
        let result = parse_cass_timestamp_ms(&ms.to_string());
        prop_assert_eq!(result, Some(ms));
    }

    /// Small integer values (<= 10B) are treated as seconds and multiplied by 1000.
    #[test]
    fn prop_small_int_is_seconds(secs in 1_i64..10_000_000_000) {
        let result = parse_cass_timestamp_ms(&secs.to_string());
        prop_assert_eq!(result, Some(secs * 1000));
    }

    /// Zero is treated as seconds → 0 ms.
    #[test]
    fn prop_zero_is_zero_ms(_dummy in 0..1_u8) {
        let result = parse_cass_timestamp_ms("0");
        prop_assert_eq!(result, Some(0));
    }
}

// =========================================================================
// parse_cass_timestamp_ms — string format parsing
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(60))]

    /// RFC3339 timestamps parse successfully.
    #[test]
    fn prop_rfc3339_parses(
        year in 2020_u32..2030,
        month in 1_u32..13,
        day in 1_u32..29,
        hour in 0_u32..24,
        minute in 0_u32..60,
        second in 0_u32..60,
    ) {
        let ts = format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z");
        let result = parse_cass_timestamp_ms(&ts);
        prop_assert!(result.is_some(), "should parse RFC3339: {ts}");
        let ms = result.unwrap();
        prop_assert!(ms > 0, "timestamp should be positive: {ms}");
    }

    /// Whitespace-padded inputs are handled (trimmed).
    #[test]
    fn prop_whitespace_trimmed(ms in 10_000_000_001_i64..100_000_000_000) {
        let padded = format!("  {ms}  ");
        let result = parse_cass_timestamp_ms(&padded);
        prop_assert_eq!(result, Some(ms));
    }
}

// =========================================================================
// parse_cass_timestamp_ms — rejection
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    /// Empty and whitespace-only strings return None.
    #[test]
    fn prop_empty_returns_none(spaces in " {0,5}") {
        let result = parse_cass_timestamp_ms(&spaces);
        if spaces.trim().is_empty() {
            prop_assert!(result.is_none());
        }
    }

    /// Non-numeric, non-timestamp strings return None.
    #[test]
    fn prop_garbage_returns_none(garbage in "[a-z]{3,10}") {
        let result = parse_cass_timestamp_ms(&garbage);
        prop_assert!(result.is_none(), "should reject garbage: {garbage}");
    }
}

// =========================================================================
// parse_cass_timestamp_ms — determinism
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    /// Same input always produces the same output.
    #[test]
    fn prop_parse_deterministic(input in "[0-9a-zA-Z:. -]{1,30}") {
        let r1 = parse_cass_timestamp_ms(&input);
        let r2 = parse_cass_timestamp_ms(&input);
        prop_assert_eq!(r1, r2);
    }
}

// =========================================================================
// CassSessionSummary::from_session
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(60))]

    /// Message count equals the number of messages in the session.
    #[test]
    fn prop_summary_message_count(session in arb_cass_session()) {
        let summary = CassSessionSummary::from_session(&session);
        prop_assert_eq!(summary.message_count, session.messages.len());
    }

    /// Total tokens is >= 0 when computed from non-negative message tokens.
    #[test]
    fn prop_summary_tokens_nonnegative(session in arb_cass_session()) {
        let summary = CassSessionSummary::from_session(&session);
        if let Some(total) = summary.total_tokens {
            prop_assert!(total >= 0, "total_tokens should be >= 0, got {total}");
        }
    }

    /// from_session is deterministic.
    #[test]
    fn prop_summary_deterministic(session in arb_cass_session()) {
        let s1 = CassSessionSummary::from_session(&session);
        let s2 = CassSessionSummary::from_session(&session);
        prop_assert_eq!(s1.message_count, s2.message_count);
        prop_assert_eq!(s1.total_tokens, s2.total_tokens);
        prop_assert_eq!(s1.input_tokens, s2.input_tokens);
        prop_assert_eq!(s1.output_tokens, s2.output_tokens);
    }
}

// =========================================================================
// CassAgent
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    /// CassAgent::as_str() returns non-empty lowercase strings.
    #[test]
    fn prop_agent_as_str_nonempty(agent in arb_cass_agent()) {
        let s = agent.as_str();
        prop_assert!(!s.is_empty());
        prop_assert_eq!(s, s.to_lowercase());
    }

    /// CassAgent Display matches as_str.
    #[test]
    fn prop_agent_display_matches_as_str(agent in arb_cass_agent()) {
        let display = format!("{agent}");
        prop_assert_eq!(display.as_str(), agent.as_str());
    }
}

// =========================================================================
// Serde roundtrips
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(60))]

    /// CassSession serde roundtrip preserves key fields.
    #[test]
    fn prop_session_serde_roundtrip(session in arb_cass_session()) {
        let json = serde_json::to_string(&session).unwrap();
        let back: CassSession = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&back.session_id, &session.session_id);
        prop_assert_eq!(&back.agent, &session.agent);
        prop_assert_eq!(&back.project_path, &session.project_path);
        prop_assert_eq!(back.messages.len(), session.messages.len());
    }

    /// CassSessionSummary serde roundtrip.
    #[test]
    fn prop_summary_serde_roundtrip(summary in arb_cass_session_summary()) {
        let json = serde_json::to_string(&summary).unwrap();
        let back: CassSessionSummary = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.total_tokens, summary.total_tokens);
        prop_assert_eq!(back.input_tokens, summary.input_tokens);
        prop_assert_eq!(back.output_tokens, summary.output_tokens);
        prop_assert_eq!(back.message_count, summary.message_count);
    }

    /// CassMessage serde roundtrip.
    #[test]
    fn prop_message_serde_roundtrip(msg in arb_cass_message()) {
        let json = serde_json::to_string(&msg).unwrap();
        let back: CassMessage = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&back.role, &msg.role);
        prop_assert_eq!(&back.content, &msg.content);
        prop_assert_eq!(back.token_count, msg.token_count);
    }

    /// CassSearchResult default serde roundtrip.
    #[test]
    fn prop_search_result_default_serde(_dummy in 0..1_u8) {
        let result = CassSearchResult::default();
        let json = serde_json::to_string(&result).unwrap();
        let back: CassSearchResult = serde_json::from_str(&json).unwrap();
        prop_assert!(back.hits.is_empty());
        prop_assert!(back.query.is_none());
    }

    /// CassSearchHit default serde roundtrip.
    #[test]
    fn prop_search_hit_default_serde(_dummy in 0..1_u8) {
        let hit = CassSearchHit::default();
        let json = serde_json::to_string(&hit).unwrap();
        let back: CassSearchHit = serde_json::from_str(&json).unwrap();
        prop_assert!(back.source_path.is_none());
        prop_assert!(back.content.is_none());
    }
}

// =========================================================================
// Unit tests
// =========================================================================

#[test]
fn parse_timestamp_empty_is_none() {
    assert!(parse_cass_timestamp_ms("").is_none());
    assert!(parse_cass_timestamp_ms("   ").is_none());
}

#[test]
fn parse_timestamp_rfc3339_known_value() {
    let ms = parse_cass_timestamp_ms("2026-01-29T17:00:00Z").unwrap();
    assert!(ms > 0);
    // 2026-01-29 is well after epoch
    assert!(ms > 1_700_000_000_000);
}

#[test]
fn parse_timestamp_epoch_seconds() {
    // 1_700_000_000 seconds = treated as seconds
    assert_eq!(
        parse_cass_timestamp_ms("1700000000"),
        Some(1_700_000_000_000)
    );
}

#[test]
fn parse_timestamp_epoch_millis() {
    // > 10B, treated as milliseconds
    assert_eq!(
        parse_cass_timestamp_ms("1700000000000"),
        Some(1_700_000_000_000)
    );
}

#[test]
fn agent_all_variants_distinct() {
    let agents = [
        CassAgent::Codex,
        CassAgent::ClaudeCode,
        CassAgent::Gemini,
        CassAgent::Cursor,
        CassAgent::Aider,
        CassAgent::ChatGpt,
    ];
    let names: Vec<&str> = agents.iter().map(|a| a.as_str()).collect();
    let unique: std::collections::HashSet<&str> = names.iter().copied().collect();
    assert_eq!(
        names.len(),
        unique.len(),
        "all agent names should be unique"
    );
}

#[test]
fn summary_from_empty_session() {
    let session = CassSession::default();
    let summary = CassSessionSummary::from_session(&session);
    assert_eq!(summary.message_count, 0);
    assert!(summary.total_tokens.is_none() || summary.total_tokens == Some(0));
}
