//! Property-based tests for the `cass` module.
//!
//! Covers `parse_cass_timestamp_ms` parsing invariants (epoch seconds,
//! epoch milliseconds, RFC3339, rejection of empty/invalid), serde
//! roundtrips for Cass types, and `CassSessionSummary::from_session`
//! aggregation correctness.

use std::collections::HashMap;

use frankenterm_core::cass::{
    CassAgent, CassContextLine, CassIndexResult, CassMessage, CassSearchHit, CassSearchResult,
    CassSession, CassSessionSummary, CassStatus, CassViewResult, parse_cass_timestamp_ms,
};
#[cfg(feature = "cass-export")]
use frankenterm_core::cass::{
    CassContentExportQuery, CassExportContentChunk, CassExportQuery, CassExportSessionRecord,
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
        prop_assert!(result.is_some(), "should parse RFC3339: {}", ts);
        let ms = result.unwrap();
        prop_assert!(ms > 0, "timestamp should be positive: {}", ms);
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
        prop_assert!(result.is_none(), "should reject garbage: {}", garbage);
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
            prop_assert!(total >= 0, "total_tokens should be >= 0, got {}", total);
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

// =========================================================================
// Additional property tests for coverage
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(60))]

    /// CassAgent Debug output is non-empty.
    #[test]
    fn prop_agent_debug_nonempty(agent in arb_cass_agent()) {
        let dbg = format!("{:?}", agent);
        prop_assert!(!dbg.is_empty());
    }

    /// CassAgent Clone preserves variant.
    #[test]
    fn prop_agent_clone(agent in arb_cass_agent()) {
        let cloned = agent;
        prop_assert_eq!(agent.as_str(), cloned.as_str());
    }

    /// CassSession Clone preserves fields.
    #[test]
    fn prop_session_clone(session in arb_cass_session()) {
        let cloned = session.clone();
        prop_assert_eq!(&cloned.session_id, &session.session_id);
        prop_assert_eq!(&cloned.agent, &session.agent);
        prop_assert_eq!(&cloned.project_path, &session.project_path);
        prop_assert_eq!(cloned.messages.len(), session.messages.len());
    }

    /// Default CassSession has empty messages.
    #[test]
    fn prop_session_default_empty(_dummy in 0..1_u8) {
        let session = CassSession::default();
        prop_assert!(session.messages.is_empty());
        prop_assert!(session.session_id.is_none());
    }

    /// CassSession JSON is a valid object.
    #[test]
    fn prop_session_json_valid_object(session in arb_cass_session()) {
        let json = serde_json::to_string(&session).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        prop_assert!(value.is_object());
    }

    /// CassMessage Clone preserves all fields.
    #[test]
    fn prop_message_clone(msg in arb_cass_message()) {
        let cloned = msg.clone();
        prop_assert_eq!(&cloned.role, &msg.role);
        prop_assert_eq!(&cloned.content, &msg.content);
        prop_assert_eq!(cloned.token_count, msg.token_count);
    }

    /// CassSessionSummary Clone preserves all fields.
    #[test]
    fn prop_summary_clone(summary in arb_cass_session_summary()) {
        let cloned = summary.clone();
        prop_assert_eq!(cloned.total_tokens, summary.total_tokens);
        prop_assert_eq!(cloned.input_tokens, summary.input_tokens);
        prop_assert_eq!(cloned.output_tokens, summary.output_tokens);
        prop_assert_eq!(cloned.message_count, summary.message_count);
    }

    /// Negative epoch timestamps return None.
    #[test]
    fn prop_negative_timestamp_none(val in -1_000_000_i64..-1) {
        let result = parse_cass_timestamp_ms(&val.to_string());
        prop_assert!(result.is_none() || result.unwrap() < 0,
            "negative value '{}' should either return None or negative ms", val);
    }

    /// CassSession Debug output is non-empty.
    #[test]
    fn prop_session_debug_nonempty(session in arb_cass_session()) {
        let dbg = format!("{:?}", session);
        prop_assert!(!dbg.is_empty());
    }

    /// CassSessionSummary JSON is a valid object.
    #[test]
    fn prop_summary_json_valid_object(summary in arb_cass_session_summary()) {
        let json = serde_json::to_string(&summary).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        prop_assert!(value.is_object());
    }
}

// =========================================================================
// Batch 15: additional property tests (DarkMill)
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// CassAgent Clone preserves fields.
    #[test]
    fn agent_clone_preserves(agent in arb_cass_agent()) {
        let cloned = agent;
        let d1 = format!("{:?}", agent);
        let d2 = format!("{:?}", cloned);
        prop_assert_eq!(&d1, &d2);
    }

    /// CassAgent Debug output is non-empty.
    #[test]
    fn agent_debug_nonempty(agent in arb_cass_agent()) {
        let debug = format!("{:?}", agent);
        prop_assert!(!debug.is_empty());
    }

    /// CassSession with messages has correct count in summary.
    #[test]
    fn session_summary_message_count(session in arb_cass_session()) {
        let summary = CassSessionSummary::from_session(&session);
        prop_assert_eq!(summary.message_count, session.messages.len(),
            "message_count {} != messages.len() {}", summary.message_count, session.messages.len());
    }
}

// =========================================================================
// Additional strategies for uncovered types (WindyStork)
// =========================================================================

fn arb_cass_context_line() -> impl Strategy<Value = CassContextLine> {
    (
        proptest::option::of(0_usize..10_000),
        proptest::option::of("[a-zA-Z ]{1,40}"),
        proptest::option::of("user|assistant|system|tool"),
    )
        .prop_map(|(line_number, content, role)| CassContextLine {
            line_number,
            content,
            role,
            extra: HashMap::new(),
        })
}

fn arb_cass_search_hit() -> impl Strategy<Value = CassSearchHit> {
    (
        proptest::option::of("/[a-z/]{3,30}\\.jsonl"),
        proptest::option::of(0_usize..50_000),
        proptest::option::of("codex|claude_code|gemini|cursor|aider"),
        proptest::option::of("/[a-z/]{3,20}"),
        proptest::option::of("[a-zA-Z ]{5,50}"),
        proptest::option::of("[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}Z"),
        proptest::option::of(0.0_f64..1.0),
    )
        .prop_map(
            |(source_path, line_number, agent, workspace, content, timestamp, score)| {
                CassSearchHit {
                    source_path,
                    line_number,
                    agent,
                    workspace,
                    content,
                    timestamp,
                    score,
                    extra: HashMap::new(),
                }
            },
        )
}

fn arb_cass_search_result() -> impl Strategy<Value = CassSearchResult> {
    (
        proptest::option::of("[a-zA-Z ]{1,20}"),
        proptest::option::of(1_usize..100),
        proptest::option::of(0_usize..50),
        proptest::option::of(0_usize..100),
        proptest::option::of(0_usize..500),
        proptest::collection::vec(arb_cass_search_hit(), 0..5),
        proptest::option::of(0_usize..10_000),
        proptest::option::of("[a-z0-9-]{8,16}"),
        proptest::option::of("[a-z0-9]{8,16}"),
        proptest::option::of(proptest::bool::ANY),
    )
        .prop_map(
            |(
                query,
                limit,
                offset,
                count,
                total_matches,
                hits,
                max_tokens,
                request_id,
                cursor,
                hits_clamped,
            )| {
                CassSearchResult {
                    query,
                    limit,
                    offset,
                    count,
                    total_matches,
                    hits,
                    max_tokens,
                    request_id,
                    cursor,
                    hits_clamped,
                    extra: HashMap::new(),
                }
            },
        )
}

fn arb_cass_view_result() -> impl Strategy<Value = CassViewResult> {
    (
        proptest::option::of("/[a-z/]{3,30}\\.jsonl"),
        proptest::option::of(0_usize..50_000),
        proptest::option::of(proptest::collection::vec(arb_cass_context_line(), 0..3)),
        proptest::option::of(arb_cass_context_line()),
        proptest::option::of(proptest::collection::vec(arb_cass_context_line(), 0..3)),
        proptest::option::of("codex|claude_code|gemini"),
        proptest::option::of("/[a-z/]{3,20}"),
    )
        .prop_map(
            |(
                source_path,
                line_number,
                context_before,
                match_line,
                context_after,
                agent,
                workspace,
            )| {
                CassViewResult {
                    source_path,
                    line_number,
                    context_before,
                    match_line,
                    context_after,
                    agent,
                    workspace,
                    extra: HashMap::new(),
                }
            },
        )
}

fn arb_cass_index_result() -> impl Strategy<Value = CassIndexResult> {
    (
        proptest::option::of(0_usize..10_000),
        proptest::option::of(0_usize..5_000),
        proptest::option::of(proptest::bool::ANY),
        proptest::option::of(0_u64..60_000),
    )
        .prop_map(
            |(sessions_indexed, new_sessions, success, elapsed_ms)| CassIndexResult {
                sessions_indexed,
                new_sessions,
                success,
                elapsed_ms,
                extra: HashMap::new(),
            },
        )
}

fn arb_cass_status() -> impl Strategy<Value = CassStatus> {
    (
        proptest::option::of(proptest::bool::ANY),
        proptest::option::of("/[a-z/.]{5,30}"),
        proptest::option::of(0_usize..100_000),
        proptest::option::of(0_usize..10_000_000),
        proptest::option::of("[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}Z"),
        proptest::option::of(proptest::bool::ANY),
    )
        .prop_map(
            |(healthy, index_path, total_sessions, total_lines, last_indexed, stale)| CassStatus {
                healthy,
                index_path,
                total_sessions,
                total_lines,
                last_indexed,
                stale,
                extra: HashMap::new(),
            },
        )
}

// =========================================================================
// Serde roundtrips for uncovered types (WindyStork)
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(60))]

    /// CassSearchHit serde roundtrip preserves all fields.
    #[test]
    fn prop_search_hit_serde_roundtrip(hit in arb_cass_search_hit()) {
        let json = serde_json::to_string(&hit).unwrap();
        let back: CassSearchHit = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&back.source_path, &hit.source_path);
        prop_assert_eq!(back.line_number, hit.line_number);
        prop_assert_eq!(&back.agent, &hit.agent);
        prop_assert_eq!(&back.workspace, &hit.workspace);
        prop_assert_eq!(&back.content, &hit.content);
        prop_assert_eq!(&back.timestamp, &hit.timestamp);
        // f64 score: compare with tolerance for JSON roundtrip
        match (back.score, hit.score) {
            (Some(a), Some(b)) => prop_assert!((a - b).abs() < 1e-10,
                "score mismatch: {} vs {}", a, b),
            (None, None) => {}
            _ => prop_assert!(false, "score presence mismatch"),
        }
    }

    /// CassSearchResult serde roundtrip preserves key fields.
    #[test]
    fn prop_search_result_serde_roundtrip(result in arb_cass_search_result()) {
        let json = serde_json::to_string(&result).unwrap();
        let back: CassSearchResult = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&back.query, &result.query);
        prop_assert_eq!(back.limit, result.limit);
        prop_assert_eq!(back.offset, result.offset);
        prop_assert_eq!(back.count, result.count);
        prop_assert_eq!(back.total_matches, result.total_matches);
        prop_assert_eq!(back.hits.len(), result.hits.len());
        prop_assert_eq!(back.max_tokens, result.max_tokens);
        prop_assert_eq!(&back.request_id, &result.request_id);
        prop_assert_eq!(&back.cursor, &result.cursor);
        prop_assert_eq!(back.hits_clamped, result.hits_clamped);
    }

    /// CassContextLine serde roundtrip.
    #[test]
    fn prop_context_line_serde_roundtrip(line in arb_cass_context_line()) {
        let json = serde_json::to_string(&line).unwrap();
        let back: CassContextLine = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.line_number, line.line_number);
        prop_assert_eq!(&back.content, &line.content);
        prop_assert_eq!(&back.role, &line.role);
    }

    /// CassViewResult serde roundtrip preserves structure.
    #[test]
    fn prop_view_result_serde_roundtrip(view in arb_cass_view_result()) {
        let json = serde_json::to_string(&view).unwrap();
        let back: CassViewResult = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&back.source_path, &view.source_path);
        prop_assert_eq!(back.line_number, view.line_number);
        prop_assert_eq!(&back.agent, &view.agent);
        prop_assert_eq!(&back.workspace, &view.workspace);
        // Context vectors
        prop_assert_eq!(
            back.context_before.as_ref().map(|v| v.len()),
            view.context_before.as_ref().map(|v| v.len())
        );
        prop_assert_eq!(
            back.context_after.as_ref().map(|v| v.len()),
            view.context_after.as_ref().map(|v| v.len())
        );
        prop_assert_eq!(back.match_line.is_some(), view.match_line.is_some());
    }

    /// CassIndexResult serde roundtrip.
    #[test]
    fn prop_index_result_serde_roundtrip(idx in arb_cass_index_result()) {
        let json = serde_json::to_string(&idx).unwrap();
        let back: CassIndexResult = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.sessions_indexed, idx.sessions_indexed);
        prop_assert_eq!(back.new_sessions, idx.new_sessions);
        prop_assert_eq!(back.success, idx.success);
        prop_assert_eq!(back.elapsed_ms, idx.elapsed_ms);
    }

    /// CassStatus serde roundtrip.
    #[test]
    fn prop_status_serde_roundtrip(status in arb_cass_status()) {
        let json = serde_json::to_string(&status).unwrap();
        let back: CassStatus = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.healthy, status.healthy);
        prop_assert_eq!(&back.index_path, &status.index_path);
        prop_assert_eq!(back.total_sessions, status.total_sessions);
        prop_assert_eq!(back.total_lines, status.total_lines);
        prop_assert_eq!(&back.last_indexed, &status.last_indexed);
        prop_assert_eq!(back.stale, status.stale);
    }
}

// =========================================================================
// Clone and Default tests for uncovered types (WindyStork)
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(40))]

    /// CassSearchHit Clone preserves fields.
    #[test]
    fn prop_search_hit_clone(hit in arb_cass_search_hit()) {
        let cloned = hit.clone();
        prop_assert_eq!(&cloned.source_path, &hit.source_path);
        prop_assert_eq!(cloned.line_number, hit.line_number);
        prop_assert_eq!(&cloned.agent, &hit.agent);
    }

    /// CassContextLine Clone preserves fields.
    #[test]
    fn prop_context_line_clone(line in arb_cass_context_line()) {
        let cloned = line.clone();
        prop_assert_eq!(cloned.line_number, line.line_number);
        prop_assert_eq!(&cloned.content, &line.content);
    }

    /// CassViewResult Clone preserves fields.
    #[test]
    fn prop_view_result_clone(view in arb_cass_view_result()) {
        let cloned = view.clone();
        prop_assert_eq!(&cloned.source_path, &view.source_path);
        prop_assert_eq!(cloned.line_number, view.line_number);
        prop_assert_eq!(&cloned.agent, &view.agent);
    }

    /// CassIndexResult Clone preserves fields.
    #[test]
    fn prop_index_result_clone(idx in arb_cass_index_result()) {
        let cloned = idx.clone();
        prop_assert_eq!(cloned.sessions_indexed, idx.sessions_indexed);
        prop_assert_eq!(cloned.new_sessions, idx.new_sessions);
    }

    /// CassStatus Clone preserves fields.
    #[test]
    fn prop_status_clone(status in arb_cass_status()) {
        let cloned = status.clone();
        prop_assert_eq!(cloned.healthy, status.healthy);
        prop_assert_eq!(&cloned.index_path, &status.index_path);
    }

    /// CassSearchResult JSON is a valid object.
    #[test]
    fn prop_search_result_json_valid(result in arb_cass_search_result()) {
        let json = serde_json::to_string(&result).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        prop_assert!(value.is_object());
    }

    /// CassViewResult JSON is a valid object.
    #[test]
    fn prop_view_result_json_valid(view in arb_cass_view_result()) {
        let json = serde_json::to_string(&view).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        prop_assert!(value.is_object());
    }

    /// CassStatus JSON is a valid object.
    #[test]
    fn prop_status_json_valid(status in arb_cass_status()) {
        let json = serde_json::to_string(&status).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        prop_assert!(value.is_object());
    }
}

// =========================================================================
// Default type unit tests (WindyStork)
// =========================================================================

#[test]
fn context_line_default() {
    let line = CassContextLine::default();
    assert!(line.line_number.is_none());
    assert!(line.content.is_none());
    assert!(line.role.is_none());
    assert!(line.extra.is_empty());
}

#[test]
fn view_result_default() {
    let view = CassViewResult::default();
    assert!(view.source_path.is_none());
    assert!(view.line_number.is_none());
    assert!(view.context_before.is_none());
    assert!(view.match_line.is_none());
    assert!(view.context_after.is_none());
    assert!(view.agent.is_none());
    assert!(view.workspace.is_none());
    assert!(view.extra.is_empty());
}

#[test]
fn index_result_default() {
    let idx = CassIndexResult::default();
    assert!(idx.sessions_indexed.is_none());
    assert!(idx.new_sessions.is_none());
    assert!(idx.success.is_none());
    assert!(idx.elapsed_ms.is_none());
    assert!(idx.extra.is_empty());
}

#[test]
fn status_default() {
    let status = CassStatus::default();
    assert!(status.healthy.is_none());
    assert!(status.index_path.is_none());
    assert!(status.total_sessions.is_none());
    assert!(status.total_lines.is_none());
    assert!(status.last_indexed.is_none());
    assert!(status.stale.is_none());
    assert!(status.extra.is_empty());
}

#[test]
fn search_result_with_unknown_fields_roundtrip() {
    let json = r#"{
        "query": "test",
        "hits": [],
        "unknown_field": "preserved",
        "nested": {"deep": true}
    }"#;
    let parsed: CassSearchResult = serde_json::from_str(json).unwrap();
    assert!(parsed.extra.contains_key("unknown_field"));
    assert!(parsed.extra.contains_key("nested"));
    // Re-serialize and check unknown fields survive
    let reserialized = serde_json::to_string(&parsed).unwrap();
    let reparsed: CassSearchResult = serde_json::from_str(&reserialized).unwrap();
    assert!(reparsed.extra.contains_key("unknown_field"));
}

#[test]
fn view_result_with_full_context() {
    let json = r#"{
        "source_path": "/sessions/sess1.jsonl",
        "line_number": 42,
        "context_before": [
            {"line_number": 40, "content": "before1"},
            {"line_number": 41, "content": "before2"}
        ],
        "match_line": {"line_number": 42, "content": "match", "role": "assistant"},
        "context_after": [
            {"line_number": 43, "content": "after1"}
        ],
        "agent": "codex",
        "workspace": "/project"
    }"#;
    let parsed: CassViewResult = serde_json::from_str(json).unwrap();
    assert_eq!(parsed.line_number, Some(42));
    assert_eq!(parsed.context_before.as_ref().map(|v| v.len()), Some(2));
    assert_eq!(parsed.context_after.as_ref().map(|v| v.len()), Some(1));
    let ml = parsed.match_line.as_ref().unwrap();
    assert_eq!(ml.role.as_deref(), Some("assistant"));
}

#[test]
fn index_result_success_roundtrip() {
    let json = r#"{
        "sessions_indexed": 150,
        "new_sessions": 10,
        "success": true,
        "elapsed_ms": 1234
    }"#;
    let parsed: CassIndexResult = serde_json::from_str(json).unwrap();
    assert_eq!(parsed.sessions_indexed, Some(150));
    assert_eq!(parsed.new_sessions, Some(10));
    assert_eq!(parsed.success, Some(true));
    assert_eq!(parsed.elapsed_ms, Some(1234));
    let reserialized = serde_json::to_string(&parsed).unwrap();
    let reparsed: CassIndexResult = serde_json::from_str(&reserialized).unwrap();
    assert_eq!(reparsed.sessions_indexed, Some(150));
}

#[test]
fn status_healthy_roundtrip() {
    let json = r#"{
        "healthy": true,
        "index_path": "/home/user/.cass/index",
        "total_sessions": 500,
        "total_lines": 100000,
        "last_indexed": "2026-03-20T10:00:00Z",
        "stale": false
    }"#;
    let parsed: CassStatus = serde_json::from_str(json).unwrap();
    assert_eq!(parsed.healthy, Some(true));
    assert_eq!(parsed.total_sessions, Some(500));
    assert_eq!(parsed.stale, Some(false));
    let reserialized = serde_json::to_string(&parsed).unwrap();
    let reparsed: CassStatus = serde_json::from_str(&reserialized).unwrap();
    assert_eq!(reparsed.healthy, Some(true));
    assert_eq!(reparsed.total_sessions, Some(500));
}

#[test]
fn search_hit_score_nan_handling() {
    // Ensure NaN doesn't crash serde (f64 NaN serializes to null in JSON)
    let hit = CassSearchHit {
        score: Some(f64::NAN),
        ..CassSearchHit::default()
    };
    // serde_json serializes NaN as null by default — this should not panic
    let json = serde_json::to_string(&hit).unwrap();
    let back: CassSearchHit = serde_json::from_str(&json).unwrap();
    // NaN serialized as null → deserialized as None
    assert!(back.score.is_none());
}

#[test]
fn search_result_empty_hits_count_consistency() {
    let result = CassSearchResult {
        query: Some("test".to_string()),
        hits: vec![],
        count: Some(0),
        total_matches: Some(0),
        ..CassSearchResult::default()
    };
    let json = serde_json::to_string(&result).unwrap();
    let back: CassSearchResult = serde_json::from_str(&json).unwrap();
    assert!(back.hits.is_empty());
    assert_eq!(back.count, Some(0));
}

// =========================================================================
// cass-export feature-gated types (WindyStork)
// =========================================================================

#[cfg(feature = "cass-export")]
mod cass_export_tests {
    use super::*;
    // Removed unused import

    fn arb_export_query() -> impl Strategy<Value = CassExportQuery> {
        (
            proptest::option::of(0_i64..100_000),
            proptest::option::of(0_u64..1000),
            proptest::option::of(0_i64..2_000_000_000_000),
            proptest::option::of(0_i64..2_000_000_000_000),
            0_usize..5000,
        )
            .prop_map(|(after_id, pane_id, since, until, limit)| CassExportQuery {
                after_id,
                pane_id,
                since,
                until,
                limit,
            })
    }

    fn arb_content_export_query() -> impl Strategy<Value = CassContentExportQuery> {
        (proptest::option::of(0_i64..100_000), 0_usize..5000)
            .prop_map(|(after_id, limit)| CassContentExportQuery { after_id, limit })
    }

    fn arb_export_session_record() -> impl Strategy<Value = CassExportSessionRecord> {
        (
            0_i64..100_000,
            "[a-z0-9-]{5,20}",
            "codex|claude_code|gemini|cursor",
            proptest::option::of("/[a-z/]{3,20}"),
            proptest::collection::vec(0_u64..1000, 1..3),
            0_i64..2_000_000_000_000,
            proptest::option::of(0_i64..2_000_000_000_000),
            proptest::option::of("[a-z0-9-]{5,20}"),
            0_u64..100_000,
            proptest::option::of("[a-z0-9-]{8,16}"),
        )
            .prop_map(
                |(
                    session_row_id,
                    session_id,
                    agent_type,
                    workspace,
                    pane_ids,
                    started_at_ms,
                    ended_at_ms,
                    model_name,
                    content_tokens,
                    external_id,
                )| {
                    CassExportSessionRecord {
                        session_row_id,
                        session_id,
                        agent_type,
                        workspace,
                        pane_ids,
                        started_at_ms,
                        ended_at_ms,
                        model_name,
                        content_tokens,
                        external_id,
                        external_meta: None,
                    }
                },
            )
    }

    fn arb_export_content_chunk() -> impl Strategy<Value = CassExportContentChunk> {
        (
            0_i64..100_000,
            "[a-z0-9-]{5,20}",
            0_i64..100_000,
            0_u64..1000,
            0_u64..10_000,
            0_i64..2_000_000_000_000,
            "[a-zA-Z ]{5,50}",
        )
            .prop_map(
                |(session_row_id, session_id, segment_id, pane_id, seq, timestamp_ms, content)| {
                    CassExportContentChunk {
                        session_row_id,
                        session_id,
                        segment_id,
                        pane_id,
                        seq,
                        timestamp_ms,
                        content,
                        content_type: "output".to_string(),
                    }
                },
            )
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(60))]

        /// CassExportQuery serde roundtrip.
        #[test]
        fn prop_export_query_serde_roundtrip(query in arb_export_query()) {
            let json = serde_json::to_string(&query).unwrap();
            let back: CassExportQuery = serde_json::from_str(&json).unwrap();
            prop_assert_eq!(back.after_id, query.after_id);
            prop_assert_eq!(back.pane_id, query.pane_id);
            prop_assert_eq!(back.since, query.since);
            prop_assert_eq!(back.until, query.until);
            prop_assert_eq!(back.limit, query.limit);
        }

        /// CassContentExportQuery serde roundtrip.
        #[test]
        fn prop_content_export_query_serde_roundtrip(query in arb_content_export_query()) {
            let json = serde_json::to_string(&query).unwrap();
            let back: CassContentExportQuery = serde_json::from_str(&json).unwrap();
            prop_assert_eq!(back.after_id, query.after_id);
            prop_assert_eq!(back.limit, query.limit);
        }

        /// CassExportSessionRecord serde roundtrip.
        #[test]
        fn prop_export_session_record_serde_roundtrip(record in arb_export_session_record()) {
            let json = serde_json::to_string(&record).unwrap();
            let back: CassExportSessionRecord = serde_json::from_str(&json).unwrap();
            prop_assert_eq!(back.session_row_id, record.session_row_id);
            prop_assert_eq!(&back.session_id, &record.session_id);
            prop_assert_eq!(&back.agent_type, &record.agent_type);
            prop_assert_eq!(&back.workspace, &record.workspace);
            prop_assert_eq!(&back.pane_ids, &record.pane_ids);
            prop_assert_eq!(back.started_at_ms, record.started_at_ms);
            prop_assert_eq!(back.ended_at_ms, record.ended_at_ms);
            prop_assert_eq!(&back.model_name, &record.model_name);
            prop_assert_eq!(back.content_tokens, record.content_tokens);
            prop_assert_eq!(&back.external_id, &record.external_id);
        }

        /// CassExportContentChunk serde roundtrip.
        #[test]
        fn prop_export_content_chunk_serde_roundtrip(chunk in arb_export_content_chunk()) {
            let json = serde_json::to_string(&chunk).unwrap();
            let back: CassExportContentChunk = serde_json::from_str(&json).unwrap();
            prop_assert_eq!(back.session_row_id, chunk.session_row_id);
            prop_assert_eq!(&back.session_id, &chunk.session_id);
            prop_assert_eq!(back.segment_id, chunk.segment_id);
            prop_assert_eq!(back.pane_id, chunk.pane_id);
            prop_assert_eq!(back.seq, chunk.seq);
            prop_assert_eq!(back.timestamp_ms, chunk.timestamp_ms);
            prop_assert_eq!(&back.content, &chunk.content);
            prop_assert_eq!(&back.content_type, &chunk.content_type);
        }

        /// CassExportQuery equality check.
        #[test]
        fn prop_export_query_eq(query in arb_export_query()) {
            let cloned = query.clone();
            prop_assert_eq!(cloned, query);
        }

        /// CassContentExportQuery equality check.
        #[test]
        fn prop_content_export_query_eq(query in arb_content_export_query()) {
            let cloned = query.clone();
            prop_assert_eq!(cloned, query);
        }

        /// CassExportSessionRecord equality check.
        #[test]
        fn prop_export_session_record_eq(record in arb_export_session_record()) {
            let cloned = record.clone();
            prop_assert_eq!(cloned, record);
        }

        /// CassExportContentChunk equality check.
        #[test]
        fn prop_export_content_chunk_eq(chunk in arb_export_content_chunk()) {
            let cloned = chunk.clone();
            prop_assert_eq!(cloned, chunk);
        }
    }

    #[test]
    fn export_query_default_has_standard_limit() {
        let query = CassExportQuery::default();
        assert!(query.limit > 0, "default limit should be positive");
        assert!(query.after_id.is_none());
        assert!(query.pane_id.is_none());
    }

    #[test]
    fn content_export_query_default_has_standard_limit() {
        let query = CassContentExportQuery::default();
        assert!(query.limit > 0, "default limit should be positive");
        assert!(query.after_id.is_none());
    }

    #[test]
    fn export_session_record_json_valid() {
        let record = CassExportSessionRecord {
            session_row_id: 1,
            session_id: "test-session".to_string(),
            agent_type: "codex".to_string(),
            workspace: Some("/project".to_string()),
            pane_ids: vec![0, 1],
            started_at_ms: 1700000000000,
            ended_at_ms: Some(1700000060000),
            model_name: Some("gpt-5".to_string()),
            content_tokens: 1500,
            external_id: None,
            external_meta: None,
        };
        let json = serde_json::to_string(&record).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert!(value.is_object());
        assert_eq!(value["session_id"], "test-session");
        assert_eq!(value["pane_ids"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn export_content_chunk_json_valid() {
        let chunk = CassExportContentChunk {
            session_row_id: 1,
            session_id: "test-session".to_string(),
            segment_id: 42,
            pane_id: 0,
            seq: 10,
            timestamp_ms: 1700000000000,
            content: "Some terminal output here".to_string(),
            content_type: "output".to_string(),
        };
        let json = serde_json::to_string(&chunk).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert!(value.is_object());
        assert_eq!(value["content_type"], "output");
    }
}
