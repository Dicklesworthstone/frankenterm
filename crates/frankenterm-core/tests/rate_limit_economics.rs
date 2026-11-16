//! W7.T (ft-7h5da.8.6) — zero-RCH unit slice for the rate-limit ledger + capacity
//! forecast. Standalone integration test, public API only, no mocks.
//!
//! Covers the parts of the W7.T acceptance whose implementation has LANDED
//! (W7.1 limit_windows ledger record + W7.2 deterministic forecast helper):
//!   * reset-timestamp parsing across the agent-CLI fixture formats, and
//!     graceful degradation when the reset text is unparseable (never a silent
//!     mis-schedule — the caller applies a conservative TTL instead);
//!   * `LimitWindowRecord` effective-deadline math: a missing/unparseable reset
//!     degrades to `last_seen_at + conservative_ttl_ms` and is flagged
//!     `reset_known() == false`, and the window stays active under that fallback;
//!   * `build_limits_forecast` determinism + the `usable + limited == total`
//!     capacity invariant, including conservative-TTL windows counting as
//!     limited and a pane recovering only when its LAST active window clears.
//!
//! GATED remainder (blocked impl / heavier seams) is tracked in the bead comment:
//! limit.window.reset events + `ft robot limits` surface (W7.2, ft-7h5da.8.2),
//! economic circuit breaker HardStop (W7.4, ft-7h5da.8.4 — also needs a full
//! MissionTxContract construction seam), cost-attribution labeling (W7.5,
//! ft-7h5da.8.5), ledger pane+account idempotency (storage integration), and the
//! shell e2e (virtual clock + watch-events).

use std::time::Duration;

use frankenterm_core::limit_forecast::build_limits_forecast;
use frankenterm_core::rate_limit_tracker::parse_retry_after_duration;
use frankenterm_core::storage::LimitWindowRecord;

/// Build a `LimitWindowRecord` with the fields the forecast/deadline logic reads;
/// the rest are filled with representative, irrelevant-to-the-assertion values.
fn limit_window(
    pane_id: u64,
    account_id: &str,
    reset_at: Option<i64>,
    reset_source: &str,
    last_seen_at: i64,
    conservative_ttl_ms: i64,
) -> LimitWindowRecord {
    LimitWindowRecord {
        id: 0,
        pane_id,
        service: "anthropic".to_string(),
        account_id: account_id.to_string(),
        account_db_id: None,
        account_known: false,
        agent_type: Some("claude_code".to_string()),
        rule_id: "claude_code.usage.reached".to_string(),
        event_type: "usage_limit".to_string(),
        limited_at: last_seen_at,
        reset_at,
        reset_source: reset_source.to_string(),
        reset_text: None,
        conservative_ttl_ms,
        last_seen_at,
        seen_count: 1,
        metadata: None,
        created_at: last_seen_at,
        updated_at: last_seen_at,
    }
}

// ── reset-timestamp parsing across fixture formats ───────────────────────────

#[test]
fn reset_text_parses_across_agent_cli_fixture_formats() {
    // Plain seconds, compact (s/m/h), and "N unit" forms all parse.
    assert_eq!(
        parse_retry_after_duration("30"),
        Some(Duration::from_secs(30))
    );
    assert_eq!(
        parse_retry_after_duration("30s"),
        Some(Duration::from_secs(30))
    );
    assert_eq!(
        parse_retry_after_duration("5m"),
        Some(Duration::from_secs(300))
    );
    assert_eq!(
        parse_retry_after_duration("2h"),
        Some(Duration::from_secs(7200))
    );
    assert_eq!(
        parse_retry_after_duration("30 seconds"),
        Some(Duration::from_secs(30))
    );
    assert_eq!(
        parse_retry_after_duration("5 minutes"),
        Some(Duration::from_secs(300))
    );
    assert_eq!(
        parse_retry_after_duration("2 hours"),
        Some(Duration::from_secs(7200))
    );
    // Surrounding whitespace is tolerated.
    assert_eq!(
        parse_retry_after_duration("  45  "),
        Some(Duration::from_secs(45))
    );
}

#[test]
fn unparseable_reset_text_returns_none_so_caller_uses_conservative_ttl() {
    // Unparseable reset must NOT yield a (wrong) duration — that would silently
    // mis-schedule recovery. It returns None so the ledger applies its
    // conservative TTL fallback instead.
    for junk in [
        "",
        "soon",
        "later today",
        "30 lightyears",
        "abc",
        "tomorrow",
    ] {
        assert_eq!(
            parse_retry_after_duration(junk),
            None,
            "unparseable reset {junk:?} must degrade to None (conservative TTL), not a guessed duration"
        );
    }
}

// ── LimitWindowRecord effective-deadline / conservative-TTL degradation ───────

#[test]
fn parsed_reset_is_used_and_marked_known() {
    let rec = limit_window(1, "acct-a", Some(10_000), "absolute", 1_000, 3_600_000);
    assert_eq!(rec.effective_reset_at_ms(), 10_000);
    assert!(rec.reset_known(), "an absolute parsed reset is known");
    assert!(rec.is_active(5_000), "still limited before the reset");
    assert!(!rec.is_active(10_001), "usable after the reset");
}

#[test]
fn missing_reset_degrades_to_conservative_ttl_and_stays_limited() {
    // `unknown_ttl`: no parsed reset -> effective deadline = last_seen + ttl.
    let rec = limit_window(2, "acct-b", None, "unknown_ttl", 1_000, 60_000);
    assert_eq!(
        rec.effective_reset_at_ms(),
        61_000,
        "conservative deadline = last_seen_at + conservative_ttl_ms"
    );
    assert!(
        !rec.reset_known(),
        "an unknown_ttl fallback must NOT be reported as a known reset"
    );
    assert!(
        rec.is_active(1_000),
        "must stay limited under the conservative fallback — never silently usable"
    );
    assert!(rec.is_active(60_999));
    assert!(
        !rec.is_active(61_000),
        "usable once the conservative deadline passes"
    );
}

// ── build_limits_forecast: determinism + capacity invariant ──────────────────

#[test]
fn forecast_is_deterministic_and_preserves_capacity_invariant() {
    let now: i64 = 1_000;
    let windows = vec![
        limit_window(1, "a", Some(5_000), "absolute", now, 0),
        limit_window(2, "b", Some(8_000), "absolute", now, 0),
    ];

    let f1 = build_limits_forecast(now, 5, &windows);
    let f2 = build_limits_forecast(now, 5, &windows);
    assert_eq!(f1, f2, "forecast must be deterministic for identical input");

    assert_eq!(f1.total_panes, 5);
    assert_eq!(f1.limited_now, 2);
    assert_eq!(f1.usable_now, 3);
    assert_eq!(
        f1.usable_now + f1.limited_now,
        f1.total_panes,
        "usable + limited == total (now)"
    );
    for point in &f1.timeline {
        assert_eq!(
            point.usable_panes + point.limited_panes,
            f1.total_panes,
            "usable + limited == total at every timeline point"
        );
    }
}

#[test]
fn forecast_counts_conservative_ttl_window_as_limited() {
    let now: i64 = 1_000;
    // A conservative (unknown_ttl, no parsed reset) window whose fallback
    // deadline (last_seen + ttl = 61_000) is still in the future.
    let windows = vec![limit_window(7, "x", None, "unknown_ttl", now, 60_000)];
    let f = build_limits_forecast(now, 3, &windows);

    assert_eq!(
        f.limited_now, 1,
        "a pane under a conservative-TTL window is limited"
    );
    assert_eq!(f.usable_now, 2);
    assert_eq!(f.active.len(), 1);
    assert!(
        !f.active[0].reset_known,
        "the conservative fallback window must be flagged reset_known = false"
    );
    assert_eq!(f.active[0].effective_reset_at_ms, 61_000);
}

#[test]
fn forecast_pane_recovers_only_when_its_last_window_clears() {
    let now: i64 = 1_000;
    // One pane limited by TWO account windows; it recovers only at the later
    // reset (8_000), not the earlier one (5_000).
    let windows = vec![
        limit_window(1, "acct-a", Some(5_000), "absolute", now, 0),
        limit_window(1, "acct-b", Some(8_000), "absolute", now, 0),
    ];
    let f = build_limits_forecast(now, 2, &windows);

    assert_eq!(
        f.limited_now, 1,
        "one distinct pane is limited (across 2 accounts)"
    );
    assert_eq!(f.usable_now, 1);

    let recovery = f
        .timeline
        .iter()
        .find(|point| point.recovering_panes.contains(&1))
        .expect("pane 1 must appear on the recovery timeline");
    assert_eq!(
        recovery.at_ms, 8_000,
        "the pane recovers only when its LAST active window clears"
    );
}
