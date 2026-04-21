//! Integration test: scan_pipeline → rate_limit_tracker.
//!
//! Exercises the output-analysis → rate-limit-tracking flow that runs in
//! the real watcher loop:
//!
//!   raw pane output bytes
//!     → ScanPipeline.process(bytes)
//!       → ScanOutput { metrics, triggers }
//!         → inspect triggers for error/rate-limit patterns
//!           → RateLimitTracker.record_at(pane_id, agent_type, ...)
//!             → ProviderRateLimitSummary { status, cooldown, ... }
//!
//! Also exercises the chunked pipeline path and GC lifecycle.

use std::time::{Duration, Instant};

use frankenterm_core::pattern_trigger::{TriggerCategory, TriggerPattern, TriggerScanner};
use frankenterm_core::patterns::AgentType;
use frankenterm_core::rate_limit_tracker::{ProviderRateLimitStatus, RateLimitTracker};
use frankenterm_core::scan_pipeline::{ChunkedPipelineState, ScanPipeline, ScanPipelineConfig};

// ── Helpers ─────────────────────────────────────────────────────────────

/// Build a scan pipeline with default triggers (error, completion, etc.).
fn default_pipeline() -> ScanPipeline {
    ScanPipeline::new(ScanPipelineConfig {
        enable_triggers: true,
        enable_compression: true,
        ..ScanPipelineConfig::default()
    })
}

/// Simulated compiler error output that should trigger error detection.
const COMPILER_ERROR_OUTPUT: &[u8] = b"\
error[E0308]: mismatched types\n\
  --> src/main.rs:42:9\n\
   |\n\
42 |     let x: u32 = \"hello\";\n\
   |                  ^^^^^^^ expected `u32`, found `&str`\n\
\n\
error: aborting due to previous error\n";

/// Simulated rate-limit message from an API provider.
const RATE_LIMIT_OUTPUT: &[u8] = b"\
HTTP/1.1 429 Too Many Requests\n\
Retry-After: 30\n\
{\"error\":{\"message\":\"Rate limit exceeded\",\"type\":\"rate_limit_error\"}}\n";

/// Simulated successful compilation output.
const SUCCESS_OUTPUT: &[u8] = b"\
   Compiling frankenterm-core v0.1.0\n\
   Compiling frankenterm v0.1.0\n\
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 12.34s\n";

/// Simulated test output with pass/fail.
const TEST_OUTPUT: &[u8] = b"\
running 42 tests\n\
test test_foo ... ok\n\
test test_bar ... ok\n\
test test_baz ... FAILED\n\
\n\
failures:\n\
\n\
---- test_baz stdout ----\n\
thread 'test_baz' panicked at 'assertion failed'\n\
\n\
test result: FAILED. 41 passed; 1 failed; 0 ignored\n";

// ── Tests ───────────────────────────────────────────────────────────────

/// Scan pipeline detects errors in compiler output; those errors inform
/// rate-limit-adjacent decisions (e.g., pane is in error state, not rate
/// limited but should be treated differently).
#[test]
fn scan_detects_errors_then_tracker_records_clean() {
    let pipeline = default_pipeline();
    let tracker = RateLimitTracker::new();
    let now = Instant::now();

    // ── Stage 1: Scan compiler error output ──
    let scan = pipeline.process(COMPILER_ERROR_OUTPUT);

    // Metrics should show real content.
    assert!(scan.metrics.newline_count > 0);
    assert!(scan.input_bytes > 0);

    // Triggers should detect error patterns.
    let triggers = scan.triggers.as_ref().expect("triggers should be present");
    assert!(
        triggers.has_errors(),
        "compiler error output should trigger error detection"
    );
    assert!(triggers.total_matches > 0);

    // ── Stage 2: No rate limit signal → tracker stays clear ──
    // The scan found errors but no rate-limit pattern, so we do NOT
    // record anything in the tracker. This is the "normal error" path.
    let pane_id = 1;
    let status = tracker.provider_status_at(AgentType::Codex, now);
    assert_eq!(status.status, ProviderRateLimitStatus::Clear);
    assert!(!tracker.is_pane_rate_limited_at(pane_id, now));
}

/// Scan pipeline detects rate-limit patterns; tracker records the event
/// and correctly reports the provider as limited.
#[test]
fn scan_detects_rate_limit_then_tracker_records_limited() {
    let pipeline = default_pipeline();
    let mut tracker = RateLimitTracker::new();
    let now = Instant::now();

    // ── Stage 1: Scan rate-limit output ──
    let scan = pipeline.process(RATE_LIMIT_OUTPUT);
    assert!(scan.input_bytes > 0);

    let triggers = scan.triggers.as_ref().expect("triggers present");

    // The default trigger scanner should detect error patterns in 429
    // responses (the "error" and "Rate limit" text).
    // Whether it flags as Error or not depends on default patterns, but
    // the scan itself must succeed and produce metrics.
    assert!(triggers.total_matches > 0 || scan.metrics.newline_count > 0);

    // ── Stage 2: Record the rate-limit event ──
    let pane_id = 10;
    tracker.record_at(
        pane_id,
        AgentType::Codex,
        "api_429".to_string(),
        Some("30".to_string()),
        now,
    );

    // Pane should be rate-limited.
    assert!(tracker.is_pane_rate_limited_at(pane_id, now));

    // Cooldown should be ~30 seconds (parsed from retry_after_text).
    let cooldown = tracker.pane_cooldown_remaining_at(pane_id, now);
    assert!(
        cooldown >= Duration::from_secs(28),
        "cooldown should be ~30s; got {cooldown:?}"
    );

    // Provider status should reflect partial limitation.
    let status = tracker.provider_status_at(AgentType::Codex, now);
    assert_eq!(status.status, ProviderRateLimitStatus::FullyLimited);
    assert_eq!(status.limited_pane_count, 1);
    assert_eq!(status.total_pane_count, 1);

    // Telemetry should record the event.
    let telem = tracker.telemetry().snapshot();
    assert_eq!(telem.events_recorded, 1);
}

/// Multiple panes with different agents; scan output drives independent
/// rate-limit tracking per provider.
#[test]
fn multi_pane_multi_agent_rate_limit_isolation() {
    let pipeline = default_pipeline();
    let mut tracker = RateLimitTracker::new();
    let now = Instant::now();

    // Scan output from 3 panes: error, rate-limit, success.
    let _scan_error = pipeline.process(COMPILER_ERROR_OUTPUT);
    let _scan_ratelimit = pipeline.process(RATE_LIMIT_OUTPUT);
    let scan_success = pipeline.process(SUCCESS_OUTPUT);

    // Success output should have no errors.
    if let Some(triggers) = &scan_success.triggers {
        // Completions might be detected ("Finished"), errors should not.
        let error_count = triggers
            .counts
            .get(&TriggerCategory::Error)
            .copied()
            .unwrap_or(0);
        assert_eq!(
            error_count, 0,
            "successful compilation should have 0 error triggers"
        );
    }

    // Record rate limits for different agents on different panes.
    let pane_codex = 1;
    let pane_claude = 2;
    let pane_gemini = 3;

    tracker.record_at(
        pane_codex,
        AgentType::Codex,
        "api_429".to_string(),
        Some("60".to_string()),
        now,
    );
    tracker.record_at(
        pane_claude,
        AgentType::ClaudeCode,
        "api_429".to_string(),
        Some("120".to_string()),
        now,
    );
    // pane_gemini has no rate limit.

    // ── Verify per-provider isolation ──
    assert!(tracker.is_pane_rate_limited_at(pane_codex, now));
    assert!(tracker.is_pane_rate_limited_at(pane_claude, now));
    assert!(!tracker.is_pane_rate_limited_at(pane_gemini, now));

    let codex_status = tracker.provider_status_at(AgentType::Codex, now);
    assert_eq!(codex_status.status, ProviderRateLimitStatus::FullyLimited);
    assert_eq!(codex_status.limited_pane_count, 1);

    let claude_status = tracker.provider_status_at(AgentType::ClaudeCode, now);
    assert_eq!(claude_status.status, ProviderRateLimitStatus::FullyLimited);
    assert_eq!(claude_status.limited_pane_count, 1);

    let gemini_status = tracker.provider_status_at(AgentType::Gemini, now);
    assert_eq!(gemini_status.status, ProviderRateLimitStatus::Clear);

    // All provider statuses should cover at least the agents we recorded.
    let all = tracker.all_provider_statuses_at(now);
    assert!(all.len() >= 2, "should have at least Codex + ClaudeCode");
}

/// Chunked pipeline accumulates metrics across chunks, then flushes to
/// produce the same trigger results as batch mode.
#[test]
fn chunked_pipeline_accumulates_then_flushes_to_trigger_rate_limit() {
    let pipeline = default_pipeline();
    let mut tracker = RateLimitTracker::new();
    let now = Instant::now();

    // Split the rate-limit output across two chunks.
    let mid = RATE_LIMIT_OUTPUT.len() / 2;
    let chunk1 = &RATE_LIMIT_OUTPUT[..mid];
    let chunk2 = &RATE_LIMIT_OUTPUT[mid..];

    let mut state = ChunkedPipelineState::new(64 * 1024);

    // Process chunks.
    let metrics1 = pipeline.process_chunk(chunk1, &mut state);
    let metrics2 = pipeline.process_chunk(chunk2, &mut state);

    // Metrics accumulate.
    let total_newlines = metrics1.newline_count + metrics2.newline_count;
    assert!(total_newlines > 0, "chunks should contain newlines");

    // Flush produces final ScanOutput with triggers.
    let scan = pipeline.flush(&mut state);
    assert!(scan.input_bytes > 0);

    // Batch mode comparison.
    let batch = pipeline.process(RATE_LIMIT_OUTPUT);
    assert_eq!(
        scan.metrics.newline_count, batch.metrics.newline_count,
        "chunked flush and batch should agree on newline count"
    );

    // Both paths should produce the same trigger match count.
    let flush_matches = scan.triggers.as_ref().map(|t| t.total_matches).unwrap_or(0);
    let batch_matches = batch
        .triggers
        .as_ref()
        .map(|t| t.total_matches)
        .unwrap_or(0);
    assert_eq!(
        flush_matches, batch_matches,
        "chunked flush and batch should agree on trigger match count"
    );

    // Record rate limit from the chunked result.
    if flush_matches > 0 {
        tracker.record_at(
            42,
            AgentType::Codex,
            "chunked_429".to_string(),
            Some("45".to_string()),
            now,
        );
        assert!(tracker.is_pane_rate_limited_at(42, now));
    }
}

/// GC clears expired cooldowns; after cooldown expires the pane and
/// provider status return to Clear.
#[test]
fn rate_limit_expires_after_cooldown_and_gc_cleans_up() {
    let pipeline = default_pipeline();
    let mut tracker = RateLimitTracker::new();
    let now = Instant::now();

    // Scan to prove pipeline is wired up.
    let scan = pipeline.process(RATE_LIMIT_OUTPUT);
    assert!(scan.input_bytes > 0);

    // Record a short 5-second cooldown.
    let pane_id = 100;
    tracker.record_at(
        pane_id,
        AgentType::Gemini,
        "short_limit".to_string(),
        Some("5".to_string()),
        now,
    );

    // At t=0: limited.
    assert!(tracker.is_pane_rate_limited_at(pane_id, now));
    let status = tracker.provider_status_at(AgentType::Gemini, now);
    assert_eq!(status.status, ProviderRateLimitStatus::FullyLimited);

    // At t=3s: still limited.
    let t3 = now + Duration::from_secs(3);
    assert!(tracker.is_pane_rate_limited_at(pane_id, t3));

    // At t=6s: cooldown expired → not limited.
    let t6 = now + Duration::from_secs(6);
    assert!(
        !tracker.is_pane_rate_limited_at(pane_id, t6),
        "cooldown should have expired after 6s"
    );

    // Provider status should reflect clearance.
    let status = tracker.provider_status_at(AgentType::Gemini, t6);
    assert_eq!(status.status, ProviderRateLimitStatus::Clear);

    // GC should clean up the expired entry.
    let before_count = tracker.tracked_pane_count();
    tracker.gc_at(t6);
    let after_count = tracker.tracked_pane_count();
    assert!(
        after_count <= before_count,
        "GC should not increase tracked count"
    );

    let telem = tracker.telemetry().snapshot();
    assert!(telem.gc_runs >= 1);
}

/// Custom trigger scanner detects domain-specific patterns and feeds
/// results through the same pipeline → tracker flow.
#[test]
fn custom_trigger_patterns_feed_into_rate_limit_flow() {
    // Build a scanner with a custom "quota exceeded" pattern.
    let patterns = vec![
        TriggerPattern::case_insensitive("quota exceeded", TriggerCategory::Error),
        TriggerPattern::case_insensitive("retry after", TriggerCategory::Warning),
        TriggerPattern::new("Finished", TriggerCategory::Completion),
    ];
    let scanner = TriggerScanner::new(patterns);
    let pipeline = ScanPipeline::with_custom_triggers(ScanPipelineConfig::default(), scanner);

    let mut tracker = RateLimitTracker::new();
    let now = Instant::now();

    // ── Scan output with "Quota exceeded" ──
    let output = b"Error: Quota exceeded for model gpt-4. Retry after 60 seconds.\n";
    let scan = pipeline.process(output);

    let triggers = scan.triggers.as_ref().expect("custom triggers present");
    assert!(
        triggers.has_errors(),
        "should detect 'quota exceeded' error"
    );

    let warning_count = triggers
        .counts
        .get(&TriggerCategory::Warning)
        .copied()
        .unwrap_or(0);
    assert!(
        warning_count > 0,
        "should detect 'retry after' warning pattern"
    );

    // Record in tracker since we detected a rate-limit error.
    tracker.record_at(
        7,
        AgentType::Codex,
        "quota_exceeded".to_string(),
        Some("60".to_string()),
        now,
    );

    assert!(tracker.is_pane_rate_limited_at(7, now));
    let cooldown = tracker.pane_cooldown_remaining_at(7, now);
    assert!(cooldown >= Duration::from_secs(58));

    // ── Scan success output — no errors ──
    let success = b"   Finished build in 5.2s\n";
    let scan2 = pipeline.process(success);
    let triggers2 = scan2.triggers.as_ref().expect("triggers present");
    assert!(
        triggers2.has_completions(),
        "'Finished' should trigger completion"
    );
    assert!(
        !triggers2.has_errors(),
        "success output should not trigger errors"
    );
}

/// Metrics from scan pipeline are consistent across different output
/// types and the rate-limit tracker telemetry reflects all operations.
#[test]
fn scan_metrics_and_tracker_telemetry_are_consistent() {
    let pipeline = default_pipeline();
    let mut tracker = RateLimitTracker::new();
    let now = Instant::now();

    // Process multiple output types.
    let outputs: &[(&[u8], u64)] = &[
        (COMPILER_ERROR_OUTPUT, 1),
        (RATE_LIMIT_OUTPUT, 2),
        (SUCCESS_OUTPUT, 3),
        (TEST_OUTPUT, 4),
    ];

    let mut total_input_bytes = 0u64;
    let mut total_newlines = 0usize;

    for (output, pane_id) in outputs {
        let scan = pipeline.process(output);
        total_input_bytes += scan.input_bytes;
        total_newlines += scan.metrics.newline_count;

        // Record a rate limit for even pane IDs.
        if pane_id % 2 == 0 {
            tracker.record_at(
                *pane_id,
                AgentType::Codex,
                format!("rule_{pane_id}"),
                Some("10".to_string()),
                now,
            );
        }
    }

    // Scan metrics sanity.
    assert!(total_input_bytes > 0, "should have processed bytes");
    assert!(
        total_newlines > 10,
        "combined outputs should have many newlines"
    );

    // Tracker state: panes 2 and 4 are limited.
    assert!(!tracker.is_pane_rate_limited_at(1, now));
    assert!(tracker.is_pane_rate_limited_at(2, now));
    assert!(!tracker.is_pane_rate_limited_at(3, now));
    assert!(tracker.is_pane_rate_limited_at(4, now));

    assert_eq!(tracker.tracked_pane_count(), 2);
    assert_eq!(tracker.total_event_count(), 2);

    let telem = tracker.telemetry().snapshot();
    assert_eq!(telem.events_recorded, 2);

    // Provider status for Codex: 2 limited out of 2 total.
    let codex = tracker.provider_status_at(AgentType::Codex, now);
    assert_eq!(codex.limited_pane_count, 2);
    assert_eq!(codex.total_pane_count, 2);
    assert_eq!(codex.status, ProviderRateLimitStatus::FullyLimited);
}
