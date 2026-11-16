//! Per-scenario golden corpus for `search_explain`. Pins the
//! `SearchExplainResult` JSON shape AND the
//! `render_explain_plain()` plain-text output for representative
//! `SearchExplainContext` fixtures that exercise distinct reason-code
//! branches (NO_INDEXED_DATA, PANE_NOT_FOUND, PANE_EXCLUDED,
//! FTS_INDEX_INCONSISTENT, CAPTURE_GAPS, RETENTION_CLEANUP,
//! STALE_PANES).
//!
//! Bead: ft-nvr6a (filed by cc_1's golden-artifacts audit).
//!
//! ## Why goldens for search_explain
//! `search_explain.rs` (71 KB, 2140 lines) is the user-facing search
//! debugging surface — what `ft search --explain`,
//! `ft robot search-explain`, and the MCP `wa.search.explain` tool
//! emit. The plain-text output is what an operator reads when
//! debugging "search returns no hits"; the JSON form is what AI
//! agents consume through MCP. Property tests pin shape invariants
//! but not exact wording, ordering, or formatting. A regression that
//! reorders reasons, drops an evidence line, or shifts ranking-score
//! formatting passes the property suite while breaking every
//! consumer's mental model.
//!
//! Pre-existing test inventory: `tests/proptest_search_explain.rs`
//! covers shape; `grep -lE "search_explain.*golden"` returned zero
//! matches before this commit.
//!
//! ## Workflow
//! - Run `cargo test -p frankenterm-core --test conformance_search_explain_corpus`
//!   to verify against goldens.
//! - Run `UPDATE_GOLDEN=1 cargo test -p frankenterm-core --test conformance_search_explain_corpus`
//!   to regenerate after intentional `explain_search` /
//!   `render_explain_plain` changes. The plain-text golden in
//!   particular needs human review for tone + formatting +
//!   truncation logic before commit.
//! - On drift, `<scenario>.actual.{json,plain}` is written next to
//!   the golden for diff inspection (gitignored via
//!   tests/fixtures/.gitignore).
//!
//! ## Coverage meta-test
//! `every_scenario_has_both_goldens` enumerates `SCENARIOS` and
//! confirms each has both `<scenario>.json` and `<scenario>.plain`
//! fixtures. New scenarios without goldens flip this test red.

use frankenterm_core::search_explain::{
    GapInfo, PaneExplainInfo, PaneIndexingInfo, SearchExplainContext, explain_search,
    render_explain_plain,
};
use serde_json::Value;
use std::path::{Path, PathBuf};

const FIXED_NOW_MS: i64 = 1_700_000_000_000;

const SCENARIOS: &[&str] = &[
    "no_indexed_data",
    "pane_not_found",
    "pane_excluded",
    "fts_inconsistent",
    "capture_gaps",
    "retention_cleanup",
    "stale_panes",
];

// ── helpers ──────────────────────────────────────────────────────────────────

fn corpus_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("search_explain")
}

fn json_path(scenario: &str) -> PathBuf {
    corpus_dir().join(format!("{scenario}.json"))
}

fn plain_path(scenario: &str) -> PathBuf {
    corpus_dir().join(format!("{scenario}.plain"))
}

fn canonicalize(value: &Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            let mut out = serde_json::Map::new();
            for k in keys {
                out.insert(k.clone(), canonicalize(&map[k]));
            }
            Value::Object(out)
        }
        Value::Array(items) => Value::Array(items.iter().map(canonicalize).collect()),
        other => other.clone(),
    }
}

fn render_canonical_json(ctx: &SearchExplainContext) -> String {
    let result = explain_search(ctx);
    let value = serde_json::to_value(&result).expect("SearchExplainResult must serialize");
    serde_json::to_string_pretty(&canonicalize(&value)).expect("canonical pretty serialize")
}

fn render_plain(ctx: &SearchExplainContext) -> String {
    let result = explain_search(ctx);
    render_explain_plain(&result)
}

fn assert_matches_golden(scenario: &str, ctx: &SearchExplainContext) {
    let json_actual = render_canonical_json(ctx);
    let plain_actual = render_plain(ctx);
    let json_p = json_path(scenario);
    let plain_p = plain_path(scenario);

    if std::env::var("UPDATE_GOLDEN").is_ok() {
        if let Some(parent) = json_p.parent() {
            std::fs::create_dir_all(parent).expect("create corpus dir");
        }
        std::fs::write(&json_p, format!("{json_actual}\n")).expect("write json golden");
        std::fs::write(&plain_p, &plain_actual).expect("write plain golden");
        return;
    }

    compare(&json_p, &json_actual, scenario, "json");
    compare(&plain_p, &plain_actual, scenario, "plain");
}

fn compare(path: &Path, actual: &str, scenario: &str, kind: &str) {
    let expected = std::fs::read_to_string(path).unwrap_or_else(|err| {
        panic!(
            "missing {kind} golden at {}: {err}. Regenerate with:\n  \
             UPDATE_GOLDEN=1 cargo test -p frankenterm-core --test conformance_search_explain_corpus",
            path.display()
        )
    });
    let expected_trimmed = expected.trim_end_matches('\n');
    let actual_trimmed = actual.trim_end_matches('\n');
    if expected_trimmed != actual_trimmed {
        let actual_path = path.with_extension(format!("actual.{kind}"));
        let _ = std::fs::write(&actual_path, format!("{actual}\n"));
        panic!(
            "{kind} golden drift for `{scenario}`. Diff:\n  expected: {}\n  actual:   {}\n\n\
             If intentional, regenerate with:\n  \
             UPDATE_GOLDEN=1 cargo test -p frankenterm-core --test conformance_search_explain_corpus",
            path.display(),
            actual_path.display()
        );
    }
}

// ── fixture builders (deterministic; FIXED_NOW_MS for stable timestamps) ─────

fn pane(pane_id: u64, observed: bool, ignore_reason: Option<&str>) -> PaneExplainInfo {
    PaneExplainInfo {
        pane_id,
        observed,
        ignore_reason: ignore_reason.map(String::from),
        domain: "local".to_string(),
        last_seen_at: FIXED_NOW_MS,
    }
}

fn indexed(
    pane_id: u64,
    segment_count: u64,
    total_bytes: u64,
    fts_row_count: u64,
    fts_consistent: bool,
) -> PaneIndexingInfo {
    PaneIndexingInfo {
        pane_id,
        segment_count,
        total_bytes,
        last_segment_at: Some(FIXED_NOW_MS),
        fts_row_count,
        fts_consistent,
    }
}

fn gap(pane_id: u64, seq_before: u64, seq_after: u64, reason: &str) -> GapInfo {
    GapInfo {
        pane_id,
        seq_before,
        seq_after,
        reason: reason.to_string(),
        // Pin the gap's detected_at to a fixed offset BEFORE FIXED_NOW_MS so
        // the rendered "X minutes ago" text is deterministic across runs.
        detected_at: FIXED_NOW_MS - 5 * 60_000,
    }
}

// ── per-scenario tests ───────────────────────────────────────────────────────

#[test]
fn no_indexed_data_scenario_matches_golden() {
    // Empty workspace: zero panes, zero indexing stats. Should emit
    // NO_INDEXED_DATA as the dominant reason.
    let ctx = SearchExplainContext {
        query: "deadlock".to_string(),
        pane_filter: None,
        panes: vec![],
        indexing_stats: vec![],
        gaps: vec![],
        retention_cleanup_count: 0,
        earliest_segment_at: None,
        latest_segment_at: None,
        now_ms: FIXED_NOW_MS,
    };
    assert_matches_golden("no_indexed_data", &ctx);
}

#[test]
fn pane_not_found_scenario_matches_golden() {
    // Filter for pane 999 when only pane 1 exists → PANE_NOT_FOUND.
    let ctx = SearchExplainContext {
        query: "checkpoint".to_string(),
        pane_filter: Some(999),
        panes: vec![pane(1, true, None)],
        indexing_stats: vec![indexed(1, 50, 200_000, 1234, true)],
        gaps: vec![],
        retention_cleanup_count: 0,
        earliest_segment_at: Some(FIXED_NOW_MS - 86_400_000),
        latest_segment_at: Some(FIXED_NOW_MS),
        now_ms: FIXED_NOW_MS,
    };
    assert_matches_golden("pane_not_found", &ctx);
}

#[test]
fn pane_excluded_scenario_matches_golden() {
    // Single pane with ignore_reason set → PANE_EXCLUDED.
    let ctx = SearchExplainContext {
        query: "rate limit".to_string(),
        pane_filter: None,
        panes: vec![pane(7, false, Some("user disabled observation"))],
        indexing_stats: vec![],
        gaps: vec![],
        retention_cleanup_count: 0,
        earliest_segment_at: None,
        latest_segment_at: None,
        now_ms: FIXED_NOW_MS,
    };
    assert_matches_golden("pane_excluded", &ctx);
}

#[test]
fn fts_inconsistent_scenario_matches_golden() {
    // Pane with fts_consistent=false → FTS_INDEX_INCONSISTENT.
    let ctx = SearchExplainContext {
        query: "panic".to_string(),
        pane_filter: None,
        panes: vec![pane(3, true, None)],
        indexing_stats: vec![indexed(3, 100, 500_000, 0, false)], // 0 fts rows but 100 segments
        gaps: vec![],
        retention_cleanup_count: 0,
        earliest_segment_at: Some(FIXED_NOW_MS - 3_600_000),
        latest_segment_at: Some(FIXED_NOW_MS),
        now_ms: FIXED_NOW_MS,
    };
    assert_matches_golden("fts_inconsistent", &ctx);
}

#[test]
fn capture_gaps_scenario_matches_golden() {
    // Pane has segment gaps → CAPTURE_GAPS.
    let ctx = SearchExplainContext {
        query: "error".to_string(),
        pane_filter: None,
        panes: vec![pane(2, true, None)],
        indexing_stats: vec![indexed(2, 80, 320_000, 5_000, true)],
        gaps: vec![
            gap(2, 100, 150, "connection reset"),
            gap(2, 240, 270, "shell crashed"),
        ],
        retention_cleanup_count: 0,
        earliest_segment_at: Some(FIXED_NOW_MS - 86_400_000),
        latest_segment_at: Some(FIXED_NOW_MS),
        now_ms: FIXED_NOW_MS,
    };
    assert_matches_golden("capture_gaps", &ctx);
}

#[test]
fn retention_cleanup_scenario_matches_golden() {
    // Workspace has retention cleanup events → RETENTION_CLEANUP.
    let ctx = SearchExplainContext {
        query: "old session".to_string(),
        pane_filter: None,
        panes: vec![pane(1, true, None)],
        indexing_stats: vec![indexed(1, 30, 120_000, 1_500, true)],
        gaps: vec![],
        retention_cleanup_count: 7,
        earliest_segment_at: Some(FIXED_NOW_MS - 7 * 86_400_000),
        latest_segment_at: Some(FIXED_NOW_MS),
        now_ms: FIXED_NOW_MS,
    };
    assert_matches_golden("retention_cleanup", &ctx);
}

#[test]
fn stale_panes_scenario_matches_golden() {
    // Pane last_seen_at is 30 days old → STALE_PANES.
    let stale_ts = FIXED_NOW_MS - 30 * 86_400_000;
    let ctx = SearchExplainContext {
        query: "find".to_string(),
        pane_filter: None,
        panes: vec![PaneExplainInfo {
            pane_id: 5,
            observed: true,
            ignore_reason: None,
            domain: "local".to_string(),
            last_seen_at: stale_ts,
        }],
        indexing_stats: vec![PaneIndexingInfo {
            pane_id: 5,
            segment_count: 200,
            total_bytes: 800_000,
            last_segment_at: Some(stale_ts),
            fts_row_count: 10_000,
            fts_consistent: true,
        }],
        gaps: vec![],
        retention_cleanup_count: 0,
        earliest_segment_at: Some(stale_ts - 86_400_000),
        latest_segment_at: Some(stale_ts),
        now_ms: FIXED_NOW_MS,
    };
    assert_matches_golden("stale_panes", &ctx);
}

// ── coverage meta-test ───────────────────────────────────────────────────────

#[test]
fn every_scenario_has_both_goldens() {
    let dir = corpus_dir();
    assert!(
        dir.is_dir(),
        "corpus directory missing at {}; run UPDATE_GOLDEN=1 first",
        dir.display()
    );
    let mut missing = Vec::new();
    for s in SCENARIOS {
        if !json_path(s).exists() {
            missing.push(format!("{s}.json"));
        }
        if !plain_path(s).exists() {
            missing.push(format!("{s}.plain"));
        }
    }
    assert!(
        missing.is_empty(),
        "missing search_explain goldens: {missing:?}. \
         Regenerate with: UPDATE_GOLDEN=1 cargo test -p frankenterm-core \
         --test conformance_search_explain_corpus",
    );

    // Inverse: no orphan goldens (scenario removed without cleaning up
    // the corresponding fixture files).
    let mut orphans = Vec::new();
    for entry in std::fs::read_dir(&dir).expect("read corpus dir") {
        let entry = entry.expect("dir entry");
        let path = entry.path();
        let name = match path.file_name().and_then(|s| s.to_str()) {
            Some(n) => n,
            None => continue,
        };
        if name.ends_with(".actual.json") || name.ends_with(".actual.plain") {
            continue; // drift artifacts, gitignored
        }
        let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
        if !SCENARIOS.contains(&stem) {
            orphans.push(name.to_string());
        }
    }
    assert!(
        orphans.is_empty(),
        "orphan search_explain goldens (no matching SCENARIOS entry): {orphans:?}",
    );
}
