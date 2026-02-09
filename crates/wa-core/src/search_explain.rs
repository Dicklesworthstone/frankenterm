//! Search explain engine: diagnoses why FTS search results may be missing or incomplete.
//!
//! Given a query, optional filters, and storage state, produces a ranked list of
//! likely reasons with evidence and remediation suggestions.

use serde::Serialize;

/// A single reason why search results may be missing or incomplete.
#[derive(Debug, Clone, Serialize)]
pub struct SearchExplainReason {
    /// Stable reason code for machine consumption.
    pub code: &'static str,
    /// Human-readable summary.
    pub summary: String,
    /// Structured evidence supporting this reason.
    pub evidence: Vec<SearchExplainEvidence>,
    /// Suggested remediation commands or actions.
    pub suggestions: Vec<String>,
    /// Severity: how likely this reason explains missing results (0.0–1.0).
    pub confidence: f64,
}

/// A piece of evidence supporting a search explain reason.
#[derive(Debug, Clone, Serialize)]
pub struct SearchExplainEvidence {
    /// Evidence label (e.g., "pane_id", "gap_count").
    pub key: String,
    /// Evidence value.
    pub value: String,
}

/// Full search explanation result.
#[derive(Debug, Clone, Serialize)]
pub struct SearchExplainResult {
    /// The original query.
    pub query: String,
    /// Pane filter applied (if any).
    pub pane_filter: Option<u64>,
    /// Total panes in workspace.
    pub total_panes: usize,
    /// Observed (indexed) panes.
    pub observed_panes: usize,
    /// Ignored (excluded) panes.
    pub ignored_panes: usize,
    /// Total indexed segments across all observed panes.
    pub total_segments: u64,
    /// Ranked reasons for missing results (highest confidence first).
    pub reasons: Vec<SearchExplainReason>,
}

/// Input context for the explain engine, gathered from storage state.
#[derive(Debug, Clone)]
pub struct SearchExplainContext {
    /// The search query.
    pub query: String,
    /// Optional pane filter.
    pub pane_filter: Option<u64>,
    /// All known panes with their observation state.
    pub panes: Vec<PaneExplainInfo>,
    /// Per-pane indexing statistics.
    pub indexing_stats: Vec<PaneIndexingInfo>,
    /// Known gaps in output capture.
    pub gaps: Vec<GapInfo>,
    /// Whether any retention cleanup has occurred.
    pub retention_cleanup_count: u64,
    /// Earliest segment timestamp across all panes (epoch ms).
    pub earliest_segment_at: Option<i64>,
    /// Latest segment timestamp across all panes (epoch ms).
    pub latest_segment_at: Option<i64>,
    /// Current time (epoch ms) for staleness calculations.
    pub now_ms: i64,
}

/// Pane observation info for explain context.
#[derive(Debug, Clone)]
pub struct PaneExplainInfo {
    pub pane_id: u64,
    pub observed: bool,
    pub ignore_reason: Option<String>,
    pub domain: String,
    pub last_seen_at: i64,
}

/// Per-pane indexing info for explain context.
#[derive(Debug, Clone)]
pub struct PaneIndexingInfo {
    pub pane_id: u64,
    pub segment_count: u64,
    pub total_bytes: u64,
    pub last_segment_at: Option<i64>,
    pub fts_row_count: u64,
    pub fts_consistent: bool,
}

/// Gap info for explain context.
#[derive(Debug, Clone)]
pub struct GapInfo {
    pub pane_id: u64,
    pub seq_before: u64,
    pub seq_after: u64,
    pub reason: String,
    pub detected_at: i64,
}

/// Build a `SearchExplainContext` from storage state.
///
/// Queries the database for panes, indexing stats, gaps, retention cleanup events,
/// and segment time range, then assembles the context for `explain_search`.
pub async fn build_explain_context(
    storage: &crate::storage::StorageHandle,
    query: &str,
    pane_filter: Option<u64>,
) -> crate::Result<SearchExplainContext> {
    let pane_records = storage.get_panes().await?;
    let indexing_stats_raw = storage.get_pane_indexing_stats().await?;
    let gaps_raw = storage.get_gaps().await?;
    let retention_cleanup_count = storage.get_retention_cleanup_count().await?;
    let (earliest_segment_at, latest_segment_at) = storage.get_segment_time_range().await?;

    let panes = pane_records
        .iter()
        .map(|p| PaneExplainInfo {
            pane_id: p.pane_id,
            observed: p.observed,
            ignore_reason: p.ignore_reason.clone(),
            domain: p.domain.clone(),
            last_seen_at: p.last_seen_at,
        })
        .collect();

    let indexing_stats = indexing_stats_raw
        .iter()
        .map(|s| PaneIndexingInfo {
            pane_id: s.pane_id,
            segment_count: s.segment_count,
            total_bytes: s.total_bytes,
            last_segment_at: s.last_segment_at,
            fts_row_count: s.fts_row_count,
            fts_consistent: s.fts_consistent,
        })
        .collect();

    let gaps = gaps_raw
        .iter()
        .map(|g| GapInfo {
            pane_id: g.pane_id,
            seq_before: g.seq_before,
            seq_after: g.seq_after,
            reason: g.reason.clone(),
            detected_at: g.detected_at,
        })
        .collect();

    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64;

    Ok(SearchExplainContext {
        query: query.to_string(),
        pane_filter,
        panes,
        indexing_stats,
        gaps,
        retention_cleanup_count,
        earliest_segment_at,
        latest_segment_at,
        now_ms,
    })
}

/// Analyze the search context and produce a ranked explanation.
pub fn explain_search(ctx: &SearchExplainContext) -> SearchExplainResult {
    let mut reasons = Vec::new();

    check_no_indexed_data(ctx, &mut reasons);
    check_pane_excluded(ctx, &mut reasons);
    check_pane_not_found(ctx, &mut reasons);
    check_fts_inconsistency(ctx, &mut reasons);
    check_gaps(ctx, &mut reasons);
    check_retention_cleanup(ctx, &mut reasons);
    check_stale_panes(ctx, &mut reasons);
    check_narrow_time_range(ctx, &mut reasons);

    // Sort by confidence descending
    reasons.sort_by(|a, b| {
        b.confidence
            .partial_cmp(&a.confidence)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    let total_panes = ctx.panes.len();
    let observed_panes = ctx.panes.iter().filter(|p| p.observed).count();
    let ignored_panes = total_panes - observed_panes;
    let total_segments: u64 = ctx.indexing_stats.iter().map(|s| s.segment_count).sum();

    SearchExplainResult {
        query: ctx.query.clone(),
        pane_filter: ctx.pane_filter,
        total_panes,
        observed_panes,
        ignored_panes,
        total_segments,
        reasons,
    }
}

fn check_no_indexed_data(ctx: &SearchExplainContext, reasons: &mut Vec<SearchExplainReason>) {
    let total_segments: u64 = ctx.indexing_stats.iter().map(|s| s.segment_count).sum();

    if total_segments == 0 {
        reasons.push(SearchExplainReason {
            code: "NO_INDEXED_DATA",
            summary: "No terminal output has been captured yet.".to_string(),
            evidence: vec![SearchExplainEvidence {
                key: "total_segments".to_string(),
                value: "0".to_string(),
            }],
            suggestions: vec![
                "Start the watcher: wa watch".to_string(),
                "Check that panes are active and not excluded.".to_string(),
            ],
            confidence: 1.0,
        });
    }
}

fn check_pane_excluded(ctx: &SearchExplainContext, reasons: &mut Vec<SearchExplainReason>) {
    if let Some(pane_id) = ctx.pane_filter {
        if let Some(pane) = ctx.panes.iter().find(|p| p.pane_id == pane_id) {
            if !pane.observed {
                let reason_text = pane.ignore_reason.as_deref().unwrap_or("unknown");
                reasons.push(SearchExplainReason {
                    code: "PANE_EXCLUDED",
                    summary: format!(
                        "Pane {pane_id} is excluded from capture (reason: {reason_text})."
                    ),
                    evidence: vec![
                        SearchExplainEvidence {
                            key: "pane_id".to_string(),
                            value: pane_id.to_string(),
                        },
                        SearchExplainEvidence {
                            key: "ignore_reason".to_string(),
                            value: reason_text.to_string(),
                        },
                    ],
                    suggestions: vec![
                        "Remove the exclusion rule from wa.toml pane filters.".to_string(),
                        format!("Check config: wa config show --effective | grep pane"),
                    ],
                    confidence: 0.95,
                });
            }
        }
    }

    // Also report if many panes are excluded
    let ignored: Vec<_> = ctx.panes.iter().filter(|p| !p.observed).collect();
    if !ignored.is_empty() && ctx.pane_filter.is_none() {
        let ignored_ids: Vec<String> = ignored.iter().map(|p| p.pane_id.to_string()).collect();
        reasons.push(SearchExplainReason {
            code: "PANES_EXCLUDED",
            summary: format!(
                "{} pane(s) excluded from capture. Content in those panes is not searchable.",
                ignored.len()
            ),
            evidence: vec![SearchExplainEvidence {
                key: "excluded_pane_ids".to_string(),
                value: ignored_ids.join(", "),
            }],
            suggestions: vec![
                "Review exclusion rules: wa config show --effective".to_string(),
                "Check pane status: wa list".to_string(),
            ],
            confidence: 0.5,
        });
    }
}

fn check_pane_not_found(ctx: &SearchExplainContext, reasons: &mut Vec<SearchExplainReason>) {
    if let Some(pane_id) = ctx.pane_filter {
        if !ctx.panes.iter().any(|p| p.pane_id == pane_id) {
            reasons.push(SearchExplainReason {
                code: "PANE_NOT_FOUND",
                summary: format!("Pane {pane_id} is not known to the watcher."),
                evidence: vec![SearchExplainEvidence {
                    key: "pane_id".to_string(),
                    value: pane_id.to_string(),
                }],
                suggestions: vec![
                    "Verify the pane ID: wa list".to_string(),
                    "The pane may have been closed or never discovered.".to_string(),
                ],
                confidence: 0.9,
            });
        }
    }
}

fn check_fts_inconsistency(ctx: &SearchExplainContext, reasons: &mut Vec<SearchExplainReason>) {
    let inconsistent: Vec<_> = ctx
        .indexing_stats
        .iter()
        .filter(|s| !s.fts_consistent && s.segment_count > 0)
        .collect();

    if !inconsistent.is_empty() {
        let mut evidence = Vec::new();
        for stat in &inconsistent {
            evidence.push(SearchExplainEvidence {
                key: format!("pane_{}_segments", stat.pane_id),
                value: stat.segment_count.to_string(),
            });
            evidence.push(SearchExplainEvidence {
                key: format!("pane_{}_fts_rows", stat.pane_id),
                value: stat.fts_row_count.to_string(),
            });
        }

        reasons.push(SearchExplainReason {
            code: "FTS_INDEX_INCONSISTENT",
            summary: format!(
                "FTS index is inconsistent for {} pane(s). Some content may not be searchable.",
                inconsistent.len()
            ),
            evidence,
            suggestions: vec![
                "Run diagnostics: wa doctor".to_string(),
                "The FTS index may need rebuilding.".to_string(),
            ],
            confidence: 0.85,
        });
    }
}

fn check_gaps(ctx: &SearchExplainContext, reasons: &mut Vec<SearchExplainReason>) {
    if ctx.gaps.is_empty() {
        return;
    }

    // Group gaps by pane
    let mut pane_gaps: std::collections::HashMap<u64, Vec<&GapInfo>> =
        std::collections::HashMap::new();
    for gap in &ctx.gaps {
        pane_gaps.entry(gap.pane_id).or_default().push(gap);
    }

    let total_gap_segments: u64 = ctx
        .gaps
        .iter()
        .map(|g| g.seq_after.saturating_sub(g.seq_before))
        .sum();

    let mut evidence = vec![
        SearchExplainEvidence {
            key: "total_gaps".to_string(),
            value: ctx.gaps.len().to_string(),
        },
        SearchExplainEvidence {
            key: "affected_panes".to_string(),
            value: pane_gaps.len().to_string(),
        },
        SearchExplainEvidence {
            key: "estimated_missing_segments".to_string(),
            value: total_gap_segments.to_string(),
        },
    ];

    // Add gap reasons
    let mut gap_reasons: Vec<String> = ctx.gaps.iter().map(|g| g.reason.clone()).collect();
    gap_reasons.sort();
    gap_reasons.dedup();
    evidence.push(SearchExplainEvidence {
        key: "gap_reasons".to_string(),
        value: gap_reasons.join(", "),
    });

    reasons.push(SearchExplainReason {
        code: "CAPTURE_GAPS",
        summary: format!(
            "{} capture gap(s) detected across {} pane(s). ~{} segments may be missing.",
            ctx.gaps.len(),
            pane_gaps.len(),
            total_gap_segments,
        ),
        evidence,
        suggestions: vec![
            "Gaps occur during daemon restarts or high load.".to_string(),
            "Reduce poll interval: wa watch --poll-interval 2000".to_string(),
            "Check gap details: wa events --rule-id gap".to_string(),
        ],
        confidence: 0.6,
    });
}

fn check_retention_cleanup(ctx: &SearchExplainContext, reasons: &mut Vec<SearchExplainReason>) {
    if ctx.retention_cleanup_count == 0 {
        return;
    }

    reasons.push(SearchExplainReason {
        code: "RETENTION_CLEANUP",
        summary: format!(
            "Retention cleanup has run {} time(s). Older content may have been purged.",
            ctx.retention_cleanup_count
        ),
        evidence: vec![
            SearchExplainEvidence {
                key: "cleanup_count".to_string(),
                value: ctx.retention_cleanup_count.to_string(),
            },
            SearchExplainEvidence {
                key: "earliest_segment_at".to_string(),
                value: ctx
                    .earliest_segment_at
                    .map_or_else(|| "none".to_string(), |t| t.to_string()),
            },
        ],
        suggestions: vec![
            "Check retention settings: wa config show --effective".to_string(),
            "Increase retention window if needed.".to_string(),
        ],
        confidence: 0.7,
    });
}

fn check_stale_panes(ctx: &SearchExplainContext, reasons: &mut Vec<SearchExplainReason>) {
    // A pane is "stale" if its last_seen_at is more than 5 minutes old
    let stale_threshold_ms = 5 * 60 * 1000;

    let stale: Vec<_> = ctx
        .panes
        .iter()
        .filter(|p| p.observed && (ctx.now_ms - p.last_seen_at) > stale_threshold_ms)
        .collect();

    if stale.is_empty() {
        return;
    }

    let stale_ids: Vec<String> = stale.iter().map(|p| p.pane_id.to_string()).collect();

    reasons.push(SearchExplainReason {
        code: "STALE_PANES",
        summary: format!(
            "{} observed pane(s) have not been seen recently. They may be closed or disconnected.",
            stale.len()
        ),
        evidence: vec![SearchExplainEvidence {
            key: "stale_pane_ids".to_string(),
            value: stale_ids.join(", "),
        }],
        suggestions: vec![
            "Check pane status: wa list".to_string(),
            "Verify the watcher is running: wa status".to_string(),
        ],
        confidence: 0.3,
    });
}

fn check_narrow_time_range(ctx: &SearchExplainContext, reasons: &mut Vec<SearchExplainReason>) {
    // If there is data but only from a narrow window, note it
    if let (Some(earliest), Some(latest)) = (ctx.earliest_segment_at, ctx.latest_segment_at) {
        let range_ms = latest - earliest;
        let one_minute_ms = 60_000;

        if range_ms < one_minute_ms && range_ms > 0 {
            reasons.push(SearchExplainReason {
                code: "NARROW_TIME_RANGE",
                summary:
                    "Captured data spans less than 1 minute. The watcher may have just started."
                        .to_string(),
                evidence: vec![SearchExplainEvidence {
                    key: "data_range_ms".to_string(),
                    value: range_ms.to_string(),
                }],
                suggestions: vec![
                    "Wait for more data to be captured.".to_string(),
                    "The watcher needs time to accumulate output.".to_string(),
                ],
                confidence: 0.4,
            });
        }
    }
}

/// Render a search explain result as plain text.
pub fn render_explain_plain(result: &SearchExplainResult) -> String {
    let mut out = String::new();
    out.push_str(&format!("Search explain for query: \"{}\"\n", result.query));
    if let Some(pane_id) = result.pane_filter {
        out.push_str(&format!("  Pane filter: {pane_id}\n"));
    }
    out.push_str(&format!(
        "  Panes: {} total ({} observed, {} ignored)\n",
        result.total_panes, result.observed_panes, result.ignored_panes
    ));
    out.push_str(&format!("  Indexed segments: {}\n", result.total_segments));

    if result.reasons.is_empty() {
        out.push_str("\nNo issues detected. Search infrastructure looks healthy.\n");
    } else {
        out.push_str(&format!("\n{} potential issue(s):\n", result.reasons.len()));
        for (i, reason) in result.reasons.iter().enumerate() {
            out.push_str(&format!(
                "\n  {}. [{}] {}\n",
                i + 1,
                reason.code,
                reason.summary
            ));
            for ev in &reason.evidence {
                out.push_str(&format!("     {}: {}\n", ev.key, ev.value));
            }
            if !reason.suggestions.is_empty() {
                out.push_str("     Suggestions:\n");
                for sug in &reason.suggestions {
                    out.push_str(&format!("       - {sug}\n"));
                }
            }
        }
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now_ms() -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_millis() as i64)
    }

    fn empty_context() -> SearchExplainContext {
        SearchExplainContext {
            query: "test".to_string(),
            pane_filter: None,
            panes: vec![],
            indexing_stats: vec![],
            gaps: vec![],
            retention_cleanup_count: 0,
            earliest_segment_at: None,
            latest_segment_at: None,
            now_ms: now_ms(),
        }
    }

    #[test]
    fn explain_empty_database_reports_no_data() {
        let ctx = empty_context();
        let result = explain_search(&ctx);
        assert!(result.reasons.iter().any(|r| r.code == "NO_INDEXED_DATA"));
        assert_eq!(result.total_segments, 0);
    }

    #[test]
    fn explain_excluded_pane_reports_exclusion() {
        let now = now_ms();
        let ctx = SearchExplainContext {
            pane_filter: Some(3),
            panes: vec![PaneExplainInfo {
                pane_id: 3,
                observed: false,
                ignore_reason: Some("title_match".to_string()),
                domain: "local".to_string(),
                last_seen_at: now,
            }],
            ..empty_context()
        };
        let result = explain_search(&ctx);
        assert!(result.reasons.iter().any(|r| r.code == "PANE_EXCLUDED"));
        assert_eq!(result.ignored_panes, 1);
    }

    #[test]
    fn explain_unknown_pane_reports_not_found() {
        let ctx = SearchExplainContext {
            pane_filter: Some(99),
            panes: vec![PaneExplainInfo {
                pane_id: 1,
                observed: true,
                ignore_reason: None,
                domain: "local".to_string(),
                last_seen_at: now_ms(),
            }],
            ..empty_context()
        };
        let result = explain_search(&ctx);
        assert!(result.reasons.iter().any(|r| r.code == "PANE_NOT_FOUND"));
    }

    #[test]
    fn explain_fts_inconsistency_detected() {
        let ctx = SearchExplainContext {
            indexing_stats: vec![PaneIndexingInfo {
                pane_id: 1,
                segment_count: 100,
                total_bytes: 5000,
                last_segment_at: Some(now_ms()),
                fts_row_count: 80,
                fts_consistent: false,
            }],
            ..empty_context()
        };
        let result = explain_search(&ctx);
        assert!(
            result
                .reasons
                .iter()
                .any(|r| r.code == "FTS_INDEX_INCONSISTENT")
        );
    }

    #[test]
    fn explain_gaps_detected() {
        let now = now_ms();
        let ctx = SearchExplainContext {
            panes: vec![PaneExplainInfo {
                pane_id: 1,
                observed: true,
                ignore_reason: None,
                domain: "local".to_string(),
                last_seen_at: now,
            }],
            indexing_stats: vec![PaneIndexingInfo {
                pane_id: 1,
                segment_count: 50,
                total_bytes: 2500,
                last_segment_at: Some(now),
                fts_row_count: 50,
                fts_consistent: true,
            }],
            gaps: vec![GapInfo {
                pane_id: 1,
                seq_before: 10,
                seq_after: 20,
                reason: "daemon_restart".to_string(),
                detected_at: now,
            }],
            ..empty_context()
        };
        let result = explain_search(&ctx);
        assert!(result.reasons.iter().any(|r| r.code == "CAPTURE_GAPS"));
    }

    #[test]
    fn explain_retention_cleanup_reported() {
        let ctx = SearchExplainContext {
            retention_cleanup_count: 3,
            earliest_segment_at: Some(now_ms() - 3_600_000),
            indexing_stats: vec![PaneIndexingInfo {
                pane_id: 1,
                segment_count: 10,
                total_bytes: 500,
                last_segment_at: Some(now_ms()),
                fts_row_count: 10,
                fts_consistent: true,
            }],
            ..empty_context()
        };
        let result = explain_search(&ctx);
        assert!(result.reasons.iter().any(|r| r.code == "RETENTION_CLEANUP"));
    }

    #[test]
    fn explain_stale_pane_detected() {
        let now = now_ms();
        let stale_time = now - (10 * 60 * 1000); // 10 minutes ago
        let ctx = SearchExplainContext {
            panes: vec![PaneExplainInfo {
                pane_id: 1,
                observed: true,
                ignore_reason: None,
                domain: "local".to_string(),
                last_seen_at: stale_time,
            }],
            indexing_stats: vec![PaneIndexingInfo {
                pane_id: 1,
                segment_count: 50,
                total_bytes: 2500,
                last_segment_at: Some(stale_time),
                fts_row_count: 50,
                fts_consistent: true,
            }],
            now_ms: now,
            ..empty_context()
        };
        let result = explain_search(&ctx);
        assert!(result.reasons.iter().any(|r| r.code == "STALE_PANES"));
    }

    #[test]
    fn explain_narrow_time_range_reported() {
        let now = now_ms();
        let ctx = SearchExplainContext {
            earliest_segment_at: Some(now - 30_000), // 30 seconds
            latest_segment_at: Some(now),
            indexing_stats: vec![PaneIndexingInfo {
                pane_id: 1,
                segment_count: 5,
                total_bytes: 200,
                last_segment_at: Some(now),
                fts_row_count: 5,
                fts_consistent: true,
            }],
            ..empty_context()
        };
        let result = explain_search(&ctx);
        assert!(result.reasons.iter().any(|r| r.code == "NARROW_TIME_RANGE"));
    }

    #[test]
    fn explain_healthy_system_no_issues() {
        let now = now_ms();
        let ctx = SearchExplainContext {
            panes: vec![PaneExplainInfo {
                pane_id: 1,
                observed: true,
                ignore_reason: None,
                domain: "local".to_string(),
                last_seen_at: now,
            }],
            indexing_stats: vec![PaneIndexingInfo {
                pane_id: 1,
                segment_count: 1000,
                total_bytes: 50000,
                last_segment_at: Some(now),
                fts_row_count: 1000,
                fts_consistent: true,
            }],
            gaps: vec![],
            retention_cleanup_count: 0,
            earliest_segment_at: Some(now - 3_600_000),
            latest_segment_at: Some(now),
            now_ms: now,
            ..empty_context()
        };
        let result = explain_search(&ctx);
        assert!(
            result.reasons.is_empty(),
            "Healthy system should have no issues, got: {:?}",
            result.reasons.iter().map(|r| r.code).collect::<Vec<_>>()
        );
    }

    #[test]
    fn explain_reasons_sorted_by_confidence() {
        let now = now_ms();
        let ctx = SearchExplainContext {
            pane_filter: Some(5),
            panes: vec![PaneExplainInfo {
                pane_id: 5,
                observed: false,
                ignore_reason: Some("cwd_match".to_string()),
                domain: "local".to_string(),
                last_seen_at: now - (10 * 60 * 1000),
            }],
            indexing_stats: vec![],
            gaps: vec![GapInfo {
                pane_id: 5,
                seq_before: 1,
                seq_after: 10,
                reason: "timeout".to_string(),
                detected_at: now,
            }],
            retention_cleanup_count: 0,
            earliest_segment_at: None,
            latest_segment_at: None,
            now_ms: now,
            query: "test".to_string(),
        };
        let result = explain_search(&ctx);

        // Verify reasons are sorted by confidence (descending)
        for window in result.reasons.windows(2) {
            assert!(
                window[0].confidence >= window[1].confidence,
                "Reasons should be sorted by confidence: {} ({}) should be >= {} ({})",
                window[0].code,
                window[0].confidence,
                window[1].code,
                window[1].confidence,
            );
        }
    }

    #[test]
    fn render_plain_output_contains_key_sections() {
        let ctx = empty_context();
        let result = explain_search(&ctx);
        let rendered = render_explain_plain(&result);
        assert!(rendered.contains("Search explain for query:"));
        assert!(rendered.contains("Indexed segments:"));
        assert!(rendered.contains("NO_INDEXED_DATA"));
    }
}
