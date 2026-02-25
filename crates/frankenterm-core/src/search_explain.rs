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
    let (total_segments, pane_scope) = if let Some(pane_id) = ctx.pane_filter {
        if !ctx.panes.iter().any(|p| p.pane_id == pane_id) {
            // Let PANE_NOT_FOUND own this case.
            return;
        }
        (
            ctx.indexing_stats
                .iter()
                .filter(|s| s.pane_id == pane_id)
                .map(|s| s.segment_count)
                .sum::<u64>(),
            Some(pane_id),
        )
    } else {
        (
            ctx.indexing_stats
                .iter()
                .map(|s| s.segment_count)
                .sum::<u64>(),
            None,
        )
    };

    if total_segments == 0 {
        let summary = if let Some(pane_id) = pane_scope {
            format!("Pane {pane_id} has no indexed terminal output yet.")
        } else {
            "No terminal output has been captured yet.".to_string()
        };
        let mut evidence = vec![SearchExplainEvidence {
            key: "total_segments".to_string(),
            value: "0".to_string(),
        }];
        if let Some(pane_id) = pane_scope {
            evidence.push(SearchExplainEvidence {
                key: "pane_id".to_string(),
                value: pane_id.to_string(),
            });
        }
        reasons.push(SearchExplainReason {
            code: "NO_INDEXED_DATA",
            summary,
            evidence,
            suggestions: vec![
                "Start the watcher: ft watch".to_string(),
                "Check that panes are active and not excluded.".to_string(),
            ],
            confidence: if pane_scope.is_some() { 0.8 } else { 1.0 },
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
                        "Remove the exclusion rule from ft.toml pane filters.".to_string(),
                        format!("Check config: ft config show --effective | grep pane"),
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
                "Review exclusion rules: ft config show --effective".to_string(),
                "Check pane status: ft list".to_string(),
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
                    "Verify the pane ID: ft list".to_string(),
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
        .filter(|s| {
            if let Some(pane_id) = ctx.pane_filter {
                s.pane_id == pane_id && !s.fts_consistent && s.segment_count > 0
            } else {
                !s.fts_consistent && s.segment_count > 0
            }
        })
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
                "Run diagnostics: ft doctor".to_string(),
                "The FTS index may need rebuilding.".to_string(),
            ],
            confidence: 0.85,
        });
    }
}

fn check_gaps(ctx: &SearchExplainContext, reasons: &mut Vec<SearchExplainReason>) {
    let relevant_gaps: Vec<_> = ctx
        .gaps
        .iter()
        .filter(|gap| {
            if let Some(pane_id) = ctx.pane_filter {
                gap.pane_id == pane_id
            } else {
                true
            }
        })
        .collect();

    if relevant_gaps.is_empty() {
        return;
    }

    // Group gaps by pane
    let mut pane_gaps: std::collections::HashMap<u64, Vec<&GapInfo>> =
        std::collections::HashMap::new();
    for gap in &relevant_gaps {
        pane_gaps.entry(gap.pane_id).or_default().push(gap);
    }

    let total_gap_segments: u64 = relevant_gaps
        .iter()
        .map(|g| g.seq_after.saturating_sub(g.seq_before))
        .sum();

    let mut evidence = vec![
        SearchExplainEvidence {
            key: "total_gaps".to_string(),
            value: relevant_gaps.len().to_string(),
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
    let mut gap_reasons: Vec<String> = relevant_gaps.iter().map(|g| g.reason.clone()).collect();
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
            relevant_gaps.len(),
            pane_gaps.len(),
            total_gap_segments,
        ),
        evidence,
        suggestions: vec![
            "Gaps occur during daemon restarts or high load.".to_string(),
            "Reduce poll interval: ft watch --poll-interval 2000".to_string(),
            "Check gap details: ft events --rule-id gap".to_string(),
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
            "Check retention settings: ft config show --effective".to_string(),
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
        .filter(|p| {
            let in_scope = if let Some(pane_id) = ctx.pane_filter {
                p.pane_id == pane_id
            } else {
                true
            };
            p.observed && in_scope && ctx.now_ms.saturating_sub(p.last_seen_at) > stale_threshold_ms
        })
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
            "Check pane status: ft list".to_string(),
            "Verify the watcher is running: ft status".to_string(),
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

    // ── Additional tests ──

    #[test]
    fn search_explain_reason_serde() {
        let reason = SearchExplainReason {
            code: "TEST_CODE",
            summary: "Test summary".to_string(),
            evidence: vec![SearchExplainEvidence {
                key: "k".to_string(),
                value: "v".to_string(),
            }],
            suggestions: vec!["Fix it".to_string()],
            confidence: 0.75,
        };
        let json = serde_json::to_string(&reason).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["code"], "TEST_CODE");
        assert_eq!(parsed["confidence"], 0.75);
        assert!(parsed["evidence"].is_array());
    }

    #[test]
    fn search_explain_evidence_serde() {
        let ev = SearchExplainEvidence {
            key: "gap_count".to_string(),
            value: "5".to_string(),
        };
        let json = serde_json::to_string(&ev).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["key"], "gap_count");
        assert_eq!(parsed["value"], "5");
    }

    #[test]
    fn search_explain_result_serde() {
        let result = SearchExplainResult {
            query: "error".to_string(),
            pane_filter: Some(42),
            total_panes: 5,
            observed_panes: 3,
            ignored_panes: 2,
            total_segments: 100,
            reasons: vec![],
        };
        let json = serde_json::to_string(&result).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["query"], "error");
        assert_eq!(parsed["pane_filter"], 42);
        assert_eq!(parsed["total_panes"], 5);
    }

    #[test]
    fn render_plain_no_issues() {
        let result = SearchExplainResult {
            query: "hello".to_string(),
            pane_filter: None,
            total_panes: 2,
            observed_panes: 2,
            ignored_panes: 0,
            total_segments: 500,
            reasons: vec![],
        };
        let rendered = render_explain_plain(&result);
        assert!(rendered.contains("No issues detected"));
        assert!(rendered.contains("looks healthy"));
    }

    #[test]
    fn render_plain_with_pane_filter() {
        let result = SearchExplainResult {
            query: "test".to_string(),
            pane_filter: Some(42),
            total_panes: 1,
            observed_panes: 1,
            ignored_panes: 0,
            total_segments: 10,
            reasons: vec![],
        };
        let rendered = render_explain_plain(&result);
        assert!(rendered.contains("Pane filter: 42"));
    }

    #[test]
    fn excluded_panes_without_filter_reports_panes_excluded() {
        let now = now_ms();
        let ctx = SearchExplainContext {
            pane_filter: None,
            panes: vec![
                PaneExplainInfo {
                    pane_id: 1,
                    observed: true,
                    ignore_reason: None,
                    domain: "local".to_string(),
                    last_seen_at: now,
                },
                PaneExplainInfo {
                    pane_id: 2,
                    observed: false,
                    ignore_reason: Some("excluded".to_string()),
                    domain: "local".to_string(),
                    last_seen_at: now,
                },
                PaneExplainInfo {
                    pane_id: 3,
                    observed: false,
                    ignore_reason: Some("excluded".to_string()),
                    domain: "local".to_string(),
                    last_seen_at: now,
                },
            ],
            indexing_stats: vec![PaneIndexingInfo {
                pane_id: 1,
                segment_count: 100,
                total_bytes: 5000,
                last_segment_at: Some(now),
                fts_row_count: 100,
                fts_consistent: true,
            }],
            ..empty_context()
        };
        let result = explain_search(&ctx);
        assert!(result.reasons.iter().any(|r| r.code == "PANES_EXCLUDED"));
        assert_eq!(result.ignored_panes, 2);
    }

    #[test]
    fn fts_consistent_pane_no_fts_reason() {
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
                segment_count: 100,
                total_bytes: 5000,
                last_segment_at: Some(now),
                fts_row_count: 100,
                fts_consistent: true,
            }],
            earliest_segment_at: Some(now - 3_600_000),
            latest_segment_at: Some(now),
            now_ms: now,
            ..empty_context()
        };
        let result = explain_search(&ctx);
        assert!(
            !result
                .reasons
                .iter()
                .any(|r| r.code == "FTS_INDEX_INCONSISTENT")
        );
    }

    #[test]
    fn multiple_gaps_across_panes() {
        let now = now_ms();
        let ctx = SearchExplainContext {
            panes: vec![
                PaneExplainInfo {
                    pane_id: 1,
                    observed: true,
                    ignore_reason: None,
                    domain: "local".to_string(),
                    last_seen_at: now,
                },
                PaneExplainInfo {
                    pane_id: 2,
                    observed: true,
                    ignore_reason: None,
                    domain: "local".to_string(),
                    last_seen_at: now,
                },
            ],
            indexing_stats: vec![
                PaneIndexingInfo {
                    pane_id: 1,
                    segment_count: 50,
                    total_bytes: 2500,
                    last_segment_at: Some(now),
                    fts_row_count: 50,
                    fts_consistent: true,
                },
                PaneIndexingInfo {
                    pane_id: 2,
                    segment_count: 30,
                    total_bytes: 1500,
                    last_segment_at: Some(now),
                    fts_row_count: 30,
                    fts_consistent: true,
                },
            ],
            gaps: vec![
                GapInfo {
                    pane_id: 1,
                    seq_before: 10,
                    seq_after: 20,
                    reason: "restart".to_string(),
                    detected_at: now,
                },
                GapInfo {
                    pane_id: 2,
                    seq_before: 5,
                    seq_after: 15,
                    reason: "timeout".to_string(),
                    detected_at: now,
                },
            ],
            earliest_segment_at: Some(now - 3_600_000),
            latest_segment_at: Some(now),
            now_ms: now,
            ..empty_context()
        };
        let result = explain_search(&ctx);
        let gap_reason = result
            .reasons
            .iter()
            .find(|r| r.code == "CAPTURE_GAPS")
            .unwrap();
        assert!(gap_reason.summary.contains("2 capture gap(s)"));
        assert!(gap_reason.summary.contains("2 pane(s)"));
    }

    #[test]
    fn narrow_time_range_exactly_one_minute_no_report() {
        let now = now_ms();
        let ctx = SearchExplainContext {
            earliest_segment_at: Some(now - 60_000),
            latest_segment_at: Some(now),
            panes: vec![PaneExplainInfo {
                pane_id: 1,
                observed: true,
                ignore_reason: None,
                domain: "local".to_string(),
                last_seen_at: now,
            }],
            indexing_stats: vec![PaneIndexingInfo {
                pane_id: 1,
                segment_count: 10,
                total_bytes: 500,
                last_segment_at: Some(now),
                fts_row_count: 10,
                fts_consistent: true,
            }],
            now_ms: now,
            ..empty_context()
        };
        let result = explain_search(&ctx);
        assert!(!result.reasons.iter().any(|r| r.code == "NARROW_TIME_RANGE"));
    }

    #[test]
    fn stale_unobserved_pane_not_reported() {
        let now = now_ms();
        let stale_time = now - (10 * 60 * 1000);
        let ctx = SearchExplainContext {
            panes: vec![PaneExplainInfo {
                pane_id: 1,
                observed: false, // not observed = not stale
                ignore_reason: Some("excluded".to_string()),
                domain: "local".to_string(),
                last_seen_at: stale_time,
            }],
            indexing_stats: vec![],
            now_ms: now,
            ..empty_context()
        };
        let result = explain_search(&ctx);
        assert!(!result.reasons.iter().any(|r| r.code == "STALE_PANES"));
    }

    #[test]
    fn result_counts_panes_correctly() {
        let now = now_ms();
        let ctx = SearchExplainContext {
            panes: vec![
                PaneExplainInfo {
                    pane_id: 1,
                    observed: true,
                    ignore_reason: None,
                    domain: "local".to_string(),
                    last_seen_at: now,
                },
                PaneExplainInfo {
                    pane_id: 2,
                    observed: true,
                    ignore_reason: None,
                    domain: "local".to_string(),
                    last_seen_at: now,
                },
                PaneExplainInfo {
                    pane_id: 3,
                    observed: false,
                    ignore_reason: Some("excluded".to_string()),
                    domain: "local".to_string(),
                    last_seen_at: now,
                },
            ],
            indexing_stats: vec![
                PaneIndexingInfo {
                    pane_id: 1,
                    segment_count: 50,
                    total_bytes: 2500,
                    last_segment_at: Some(now),
                    fts_row_count: 50,
                    fts_consistent: true,
                },
                PaneIndexingInfo {
                    pane_id: 2,
                    segment_count: 30,
                    total_bytes: 1500,
                    last_segment_at: Some(now),
                    fts_row_count: 30,
                    fts_consistent: true,
                },
            ],
            earliest_segment_at: Some(now - 3_600_000),
            latest_segment_at: Some(now),
            now_ms: now,
            ..empty_context()
        };
        let result = explain_search(&ctx);
        assert_eq!(result.total_panes, 3);
        assert_eq!(result.observed_panes, 2);
        assert_eq!(result.ignored_panes, 1);
        assert_eq!(result.total_segments, 80);
    }

    #[test]
    fn reason_clone_and_debug() {
        let reason = SearchExplainReason {
            code: "TEST",
            summary: "Test".to_string(),
            evidence: vec![],
            suggestions: vec![],
            confidence: 0.5,
        };
        let cloned = reason.clone();
        assert_eq!(cloned.code, "TEST");
        let debug = format!("{:?}", reason);
        assert!(debug.contains("TEST"));
    }

    #[test]
    fn evidence_clone_and_debug() {
        let ev = SearchExplainEvidence {
            key: "k".to_string(),
            value: "v".to_string(),
        };
        let cloned = ev.clone();
        assert_eq!(cloned.key, "k");
        let debug = format!("{:?}", ev);
        assert!(debug.contains("k"));
    }

    #[test]
    fn render_plain_with_evidence_and_suggestions() {
        let result = SearchExplainResult {
            query: "error".to_string(),
            pane_filter: None,
            total_panes: 1,
            observed_panes: 1,
            ignored_panes: 0,
            total_segments: 0,
            reasons: vec![SearchExplainReason {
                code: "NO_INDEXED_DATA",
                summary: "No data".to_string(),
                evidence: vec![SearchExplainEvidence {
                    key: "total_segments".to_string(),
                    value: "0".to_string(),
                }],
                suggestions: vec!["Start the watcher".to_string()],
                confidence: 1.0,
            }],
        };
        let rendered = render_explain_plain(&result);
        assert!(rendered.contains("[NO_INDEXED_DATA]"));
        assert!(rendered.contains("total_segments: 0"));
        assert!(rendered.contains("Start the watcher"));
        assert!(rendered.contains("1 potential issue(s)"));
    }

    // ── Batch 2: RubyBeaver wa-1u90p.7.1 ────────────────────────────────

    #[test]
    fn no_indexed_data_not_reported_when_segments_exist() {
        let now = now_ms();
        let ctx = SearchExplainContext {
            indexing_stats: vec![PaneIndexingInfo {
                pane_id: 1,
                segment_count: 1,
                total_bytes: 50,
                last_segment_at: Some(now),
                fts_row_count: 1,
                fts_consistent: true,
            }],
            panes: vec![PaneExplainInfo {
                pane_id: 1,
                observed: true,
                ignore_reason: None,
                domain: "local".to_string(),
                last_seen_at: now,
            }],
            earliest_segment_at: Some(now - 3_600_000),
            latest_segment_at: Some(now),
            now_ms: now,
            ..empty_context()
        };
        let result = explain_search(&ctx);
        assert!(
            !result.reasons.iter().any(|r| r.code == "NO_INDEXED_DATA"),
            "should not report NO_INDEXED_DATA when segments exist"
        );
    }

    #[test]
    fn pane_filter_reports_no_indexed_data_when_selected_pane_empty() {
        let now = now_ms();
        let ctx = SearchExplainContext {
            pane_filter: Some(1),
            panes: vec![
                PaneExplainInfo {
                    pane_id: 1,
                    observed: true,
                    ignore_reason: None,
                    domain: "local".to_string(),
                    last_seen_at: now,
                },
                PaneExplainInfo {
                    pane_id: 2,
                    observed: true,
                    ignore_reason: None,
                    domain: "local".to_string(),
                    last_seen_at: now,
                },
            ],
            indexing_stats: vec![PaneIndexingInfo {
                pane_id: 2,
                segment_count: 10,
                total_bytes: 500,
                last_segment_at: Some(now),
                fts_row_count: 10,
                fts_consistent: true,
            }],
            ..empty_context()
        };
        let result = explain_search(&ctx);
        let reason = result
            .reasons
            .iter()
            .find(|r| r.code == "NO_INDEXED_DATA")
            .expect("should report pane-scoped NO_INDEXED_DATA");
        assert!(reason.summary.contains("Pane 1"));
        assert!(
            reason
                .evidence
                .iter()
                .any(|e| e.key == "pane_id" && e.value == "1")
        );
    }

    #[test]
    fn pane_filter_not_found_does_not_emit_no_indexed_data() {
        let now = now_ms();
        let ctx = SearchExplainContext {
            pane_filter: Some(99),
            panes: vec![PaneExplainInfo {
                pane_id: 1,
                observed: true,
                ignore_reason: None,
                domain: "local".to_string(),
                last_seen_at: now,
            }],
            indexing_stats: vec![PaneIndexingInfo {
                pane_id: 1,
                segment_count: 10,
                total_bytes: 500,
                last_segment_at: Some(now),
                fts_row_count: 10,
                fts_consistent: true,
            }],
            ..empty_context()
        };
        let result = explain_search(&ctx);
        assert!(result.reasons.iter().any(|r| r.code == "PANE_NOT_FOUND"));
        assert!(
            !result.reasons.iter().any(|r| r.code == "NO_INDEXED_DATA"),
            "pane not found should not be shadowed by NO_INDEXED_DATA"
        );
    }

    #[test]
    fn pane_excluded_filter_on_observed_pane_no_exclusion_reason() {
        let now = now_ms();
        let ctx = SearchExplainContext {
            pane_filter: Some(1),
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
            earliest_segment_at: Some(now - 3_600_000),
            latest_segment_at: Some(now),
            now_ms: now,
            ..empty_context()
        };
        let result = explain_search(&ctx);
        assert!(
            !result.reasons.iter().any(|r| r.code == "PANE_EXCLUDED"),
            "observed pane should not trigger PANE_EXCLUDED"
        );
    }

    #[test]
    fn pane_excluded_no_ignore_reason_shows_unknown() {
        let now = now_ms();
        let ctx = SearchExplainContext {
            pane_filter: Some(5),
            panes: vec![PaneExplainInfo {
                pane_id: 5,
                observed: false,
                ignore_reason: None, // no reason given
                domain: "local".to_string(),
                last_seen_at: now,
            }],
            ..empty_context()
        };
        let result = explain_search(&ctx);
        let reason = result
            .reasons
            .iter()
            .find(|r| r.code == "PANE_EXCLUDED")
            .expect("should report PANE_EXCLUDED");
        assert!(reason.summary.contains("unknown"));
    }

    #[test]
    fn fts_inconsistency_zero_segments_not_reported() {
        let ctx = SearchExplainContext {
            indexing_stats: vec![PaneIndexingInfo {
                pane_id: 1,
                segment_count: 0, // no segments
                total_bytes: 0,
                last_segment_at: None,
                fts_row_count: 0,
                fts_consistent: false, // inconsistent but no data
            }],
            ..empty_context()
        };
        let result = explain_search(&ctx);
        assert!(
            !result
                .reasons
                .iter()
                .any(|r| r.code == "FTS_INDEX_INCONSISTENT"),
            "zero-segment pane should not trigger FTS inconsistency"
        );
    }

    #[test]
    fn fts_inconsistency_multiple_panes() {
        let now = now_ms();
        let ctx = SearchExplainContext {
            indexing_stats: vec![
                PaneIndexingInfo {
                    pane_id: 1,
                    segment_count: 100,
                    total_bytes: 5000,
                    last_segment_at: Some(now),
                    fts_row_count: 80,
                    fts_consistent: false,
                },
                PaneIndexingInfo {
                    pane_id: 2,
                    segment_count: 50,
                    total_bytes: 2500,
                    last_segment_at: Some(now),
                    fts_row_count: 30,
                    fts_consistent: false,
                },
            ],
            ..empty_context()
        };
        let result = explain_search(&ctx);
        let reason = result
            .reasons
            .iter()
            .find(|r| r.code == "FTS_INDEX_INCONSISTENT")
            .expect("should report FTS_INDEX_INCONSISTENT");
        assert!(reason.summary.contains("2 pane(s)"));
        // Should have evidence for both panes
        assert!(reason.evidence.iter().any(|e| e.key.contains("pane_1")));
        assert!(reason.evidence.iter().any(|e| e.key.contains("pane_2")));
    }

    #[test]
    fn pane_filter_scopes_fts_inconsistency_to_selected_pane() {
        let now = now_ms();
        let ctx = SearchExplainContext {
            pane_filter: Some(1),
            indexing_stats: vec![
                PaneIndexingInfo {
                    pane_id: 1,
                    segment_count: 50,
                    total_bytes: 2500,
                    last_segment_at: Some(now),
                    fts_row_count: 50,
                    fts_consistent: true,
                },
                PaneIndexingInfo {
                    pane_id: 2,
                    segment_count: 100,
                    total_bytes: 5000,
                    last_segment_at: Some(now),
                    fts_row_count: 10,
                    fts_consistent: false,
                },
            ],
            ..empty_context()
        };
        let result = explain_search(&ctx);
        assert!(
            !result
                .reasons
                .iter()
                .any(|r| r.code == "FTS_INDEX_INCONSISTENT"),
            "pane-filtered explain should ignore unrelated pane FTS inconsistency"
        );
    }

    #[test]
    fn gap_saturating_sub_handles_inversion() {
        let now = now_ms();
        let ctx = SearchExplainContext {
            gaps: vec![GapInfo {
                pane_id: 1,
                seq_before: 20, // before > after (shouldn't happen, but be robust)
                seq_after: 10,
                reason: "weird".to_string(),
                detected_at: now,
            }],
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
            earliest_segment_at: Some(now - 3_600_000),
            latest_segment_at: Some(now),
            now_ms: now,
            ..empty_context()
        };
        let result = explain_search(&ctx);
        let reason = result
            .reasons
            .iter()
            .find(|r| r.code == "CAPTURE_GAPS")
            .expect("should still report gap");
        // saturating_sub means missing segments = 0
        assert!(reason.summary.contains("~0 segments"));
    }

    #[test]
    fn gap_reasons_deduplicated() {
        let now = now_ms();
        let ctx = SearchExplainContext {
            gaps: vec![
                GapInfo {
                    pane_id: 1,
                    seq_before: 10,
                    seq_after: 20,
                    reason: "restart".to_string(),
                    detected_at: now,
                },
                GapInfo {
                    pane_id: 1,
                    seq_before: 30,
                    seq_after: 40,
                    reason: "restart".to_string(),
                    detected_at: now,
                },
                GapInfo {
                    pane_id: 2,
                    seq_before: 5,
                    seq_after: 15,
                    reason: "timeout".to_string(),
                    detected_at: now,
                },
            ],
            indexing_stats: vec![PaneIndexingInfo {
                pane_id: 1,
                segment_count: 50,
                total_bytes: 2500,
                last_segment_at: Some(now),
                fts_row_count: 50,
                fts_consistent: true,
            }],
            ..empty_context()
        };
        let result = explain_search(&ctx);
        let reason = result
            .reasons
            .iter()
            .find(|r| r.code == "CAPTURE_GAPS")
            .expect("should report CAPTURE_GAPS");
        let gap_reasons_ev = reason
            .evidence
            .iter()
            .find(|e| e.key == "gap_reasons")
            .expect("should have gap_reasons evidence");
        // "restart" appears once after dedup, "timeout" appears once
        assert_eq!(gap_reasons_ev.value, "restart, timeout");
    }

    #[test]
    fn pane_filter_scopes_capture_gaps_to_selected_pane() {
        let now = now_ms();
        let ctx = SearchExplainContext {
            pane_filter: Some(1),
            gaps: vec![GapInfo {
                pane_id: 2,
                seq_before: 10,
                seq_after: 20,
                reason: "restart".to_string(),
                detected_at: now,
            }],
            panes: vec![
                PaneExplainInfo {
                    pane_id: 1,
                    observed: true,
                    ignore_reason: None,
                    domain: "local".to_string(),
                    last_seen_at: now,
                },
                PaneExplainInfo {
                    pane_id: 2,
                    observed: true,
                    ignore_reason: None,
                    domain: "local".to_string(),
                    last_seen_at: now,
                },
            ],
            ..empty_context()
        };
        let result = explain_search(&ctx);
        assert!(
            !result.reasons.iter().any(|r| r.code == "CAPTURE_GAPS"),
            "pane-filtered explain should ignore unrelated pane gaps"
        );
    }

    #[test]
    fn retention_cleanup_includes_earliest_segment() {
        let now = now_ms();
        let earliest = now - 7_200_000;
        let ctx = SearchExplainContext {
            retention_cleanup_count: 1,
            earliest_segment_at: Some(earliest),
            indexing_stats: vec![PaneIndexingInfo {
                pane_id: 1,
                segment_count: 5,
                total_bytes: 250,
                last_segment_at: Some(now),
                fts_row_count: 5,
                fts_consistent: true,
            }],
            ..empty_context()
        };
        let result = explain_search(&ctx);
        let reason = result
            .reasons
            .iter()
            .find(|r| r.code == "RETENTION_CLEANUP")
            .expect("should report RETENTION_CLEANUP");
        let ev = reason
            .evidence
            .iter()
            .find(|e| e.key == "earliest_segment_at")
            .expect("should have earliest_segment_at evidence");
        assert_eq!(ev.value, earliest.to_string());
    }

    #[test]
    fn retention_cleanup_no_earliest_shows_none() {
        let ctx = SearchExplainContext {
            retention_cleanup_count: 2,
            earliest_segment_at: None,
            indexing_stats: vec![PaneIndexingInfo {
                pane_id: 1,
                segment_count: 5,
                total_bytes: 250,
                last_segment_at: Some(now_ms()),
                fts_row_count: 5,
                fts_consistent: true,
            }],
            ..empty_context()
        };
        let result = explain_search(&ctx);
        let reason = result
            .reasons
            .iter()
            .find(|r| r.code == "RETENTION_CLEANUP")
            .expect("should report RETENTION_CLEANUP");
        let ev = reason
            .evidence
            .iter()
            .find(|e| e.key == "earliest_segment_at")
            .unwrap();
        assert_eq!(ev.value, "none");
    }

    #[test]
    fn stale_pane_at_exactly_five_minutes_not_stale() {
        let now = now_ms();
        let exactly_5min = now - (5 * 60 * 1000); // exactly at threshold
        let ctx = SearchExplainContext {
            panes: vec![PaneExplainInfo {
                pane_id: 1,
                observed: true,
                ignore_reason: None,
                domain: "local".to_string(),
                last_seen_at: exactly_5min,
            }],
            indexing_stats: vec![PaneIndexingInfo {
                pane_id: 1,
                segment_count: 50,
                total_bytes: 2500,
                last_segment_at: Some(now),
                fts_row_count: 50,
                fts_consistent: true,
            }],
            earliest_segment_at: Some(now - 3_600_000),
            latest_segment_at: Some(now),
            now_ms: now,
            ..empty_context()
        };
        let result = explain_search(&ctx);
        assert!(
            !result.reasons.iter().any(|r| r.code == "STALE_PANES"),
            "pane at exactly 5min threshold should not be stale (need > threshold)"
        );
    }

    #[test]
    fn pane_filter_scopes_stale_panes_to_selected_pane() {
        let now = now_ms();
        let ctx = SearchExplainContext {
            pane_filter: Some(1),
            panes: vec![
                PaneExplainInfo {
                    pane_id: 1,
                    observed: true,
                    ignore_reason: None,
                    domain: "local".to_string(),
                    last_seen_at: now,
                },
                PaneExplainInfo {
                    pane_id: 2,
                    observed: true,
                    ignore_reason: None,
                    domain: "local".to_string(),
                    last_seen_at: now - (10 * 60 * 1000),
                },
            ],
            indexing_stats: vec![
                PaneIndexingInfo {
                    pane_id: 1,
                    segment_count: 10,
                    total_bytes: 500,
                    last_segment_at: Some(now),
                    fts_row_count: 10,
                    fts_consistent: true,
                },
                PaneIndexingInfo {
                    pane_id: 2,
                    segment_count: 10,
                    total_bytes: 500,
                    last_segment_at: Some(now),
                    fts_row_count: 10,
                    fts_consistent: true,
                },
            ],
            earliest_segment_at: Some(now - 3_600_000),
            latest_segment_at: Some(now),
            now_ms: now,
            ..empty_context()
        };
        let result = explain_search(&ctx);
        assert!(
            !result.reasons.iter().any(|r| r.code == "STALE_PANES"),
            "pane-filtered explain should ignore stale state from unrelated panes"
        );
    }

    #[test]
    fn narrow_time_range_zero_span_not_reported() {
        let now = now_ms();
        let ctx = SearchExplainContext {
            earliest_segment_at: Some(now),
            latest_segment_at: Some(now), // same timestamp = range_ms == 0
            indexing_stats: vec![PaneIndexingInfo {
                pane_id: 1,
                segment_count: 1,
                total_bytes: 50,
                last_segment_at: Some(now),
                fts_row_count: 1,
                fts_consistent: true,
            }],
            panes: vec![PaneExplainInfo {
                pane_id: 1,
                observed: true,
                ignore_reason: None,
                domain: "local".to_string(),
                last_seen_at: now,
            }],
            now_ms: now,
            ..empty_context()
        };
        let result = explain_search(&ctx);
        assert!(
            !result.reasons.iter().any(|r| r.code == "NARROW_TIME_RANGE"),
            "zero-span range should not trigger NARROW_TIME_RANGE"
        );
    }

    #[test]
    fn narrow_time_range_no_timestamps_not_reported() {
        let ctx = SearchExplainContext {
            earliest_segment_at: None,
            latest_segment_at: None,
            indexing_stats: vec![PaneIndexingInfo {
                pane_id: 1,
                segment_count: 5,
                total_bytes: 250,
                last_segment_at: Some(now_ms()),
                fts_row_count: 5,
                fts_consistent: true,
            }],
            ..empty_context()
        };
        let result = explain_search(&ctx);
        assert!(
            !result.reasons.iter().any(|r| r.code == "NARROW_TIME_RANGE"),
            "missing timestamps should not trigger NARROW_TIME_RANGE"
        );
    }

    #[test]
    fn all_confidence_values_in_valid_range() {
        let now = now_ms();
        let ctx = SearchExplainContext {
            pane_filter: Some(99),
            panes: vec![PaneExplainInfo {
                pane_id: 1,
                observed: false,
                ignore_reason: Some("test".to_string()),
                domain: "local".to_string(),
                last_seen_at: now - (10 * 60 * 1000),
            }],
            indexing_stats: vec![PaneIndexingInfo {
                pane_id: 1,
                segment_count: 10,
                total_bytes: 500,
                last_segment_at: Some(now),
                fts_row_count: 5,
                fts_consistent: false,
            }],
            gaps: vec![GapInfo {
                pane_id: 1,
                seq_before: 1,
                seq_after: 5,
                reason: "test".to_string(),
                detected_at: now,
            }],
            retention_cleanup_count: 1,
            earliest_segment_at: Some(now - 30_000),
            latest_segment_at: Some(now),
            now_ms: now,
            ..empty_context()
        };
        let result = explain_search(&ctx);
        for reason in &result.reasons {
            assert!(
                (0.0..=1.0).contains(&reason.confidence),
                "confidence {} out of range for reason {}",
                reason.confidence,
                reason.code
            );
        }
    }

    #[test]
    fn render_plain_multiple_reasons_numbered() {
        let result = SearchExplainResult {
            query: "test".to_string(),
            pane_filter: None,
            total_panes: 2,
            observed_panes: 2,
            ignored_panes: 0,
            total_segments: 0,
            reasons: vec![
                SearchExplainReason {
                    code: "NO_INDEXED_DATA",
                    summary: "No data".to_string(),
                    evidence: vec![],
                    suggestions: vec![],
                    confidence: 1.0,
                },
                SearchExplainReason {
                    code: "CAPTURE_GAPS",
                    summary: "Gaps found".to_string(),
                    evidence: vec![],
                    suggestions: vec![],
                    confidence: 0.6,
                },
            ],
        };
        let rendered = render_explain_plain(&result);
        assert!(rendered.contains("1. [NO_INDEXED_DATA]"));
        assert!(rendered.contains("2. [CAPTURE_GAPS]"));
        assert!(rendered.contains("2 potential issue(s)"));
    }

    #[test]
    fn render_plain_reason_with_empty_suggestions() {
        let result = SearchExplainResult {
            query: "q".to_string(),
            pane_filter: None,
            total_panes: 0,
            observed_panes: 0,
            ignored_panes: 0,
            total_segments: 0,
            reasons: vec![SearchExplainReason {
                code: "TEST",
                summary: "Test reason".to_string(),
                evidence: vec![],
                suggestions: vec![], // empty
                confidence: 0.5,
            }],
        };
        let rendered = render_explain_plain(&result);
        assert!(rendered.contains("[TEST]"));
        assert!(!rendered.contains("Suggestions:"));
    }

    #[test]
    fn context_clone_and_debug() {
        let ctx = empty_context();
        let cloned = ctx.clone();
        assert_eq!(cloned.query, "test");
        let debug = format!("{:?}", ctx);
        assert!(debug.contains("SearchExplainContext"));
    }

    #[test]
    fn pane_explain_info_clone_and_debug() {
        let info = PaneExplainInfo {
            pane_id: 42,
            observed: true,
            ignore_reason: None,
            domain: "remote".to_string(),
            last_seen_at: 1234,
        };
        let cloned = info.clone();
        assert_eq!(cloned.pane_id, 42);
        assert_eq!(cloned.domain, "remote");
        let debug = format!("{:?}", info);
        assert!(debug.contains("42"));
    }

    #[test]
    fn pane_indexing_info_clone_and_debug() {
        let info = PaneIndexingInfo {
            pane_id: 7,
            segment_count: 100,
            total_bytes: 5000,
            last_segment_at: Some(9999),
            fts_row_count: 95,
            fts_consistent: true,
        };
        let cloned = info.clone();
        assert_eq!(cloned.segment_count, 100);
        let debug = format!("{:?}", info);
        assert!(debug.contains("PaneIndexingInfo"));
    }

    #[test]
    fn gap_info_clone_and_debug() {
        let gap = GapInfo {
            pane_id: 3,
            seq_before: 10,
            seq_after: 20,
            reason: "crash".to_string(),
            detected_at: 5555,
        };
        let cloned = gap.clone();
        assert_eq!(cloned.reason, "crash");
        let debug = format!("{:?}", gap);
        assert!(debug.contains("GapInfo"));
    }

    #[test]
    fn query_with_special_characters_preserved() {
        let ctx = SearchExplainContext {
            query: r#"error "disk full" OR (code=500 AND path=/api/*)"#.to_string(),
            ..empty_context()
        };
        let result = explain_search(&ctx);
        assert_eq!(
            result.query,
            r#"error "disk full" OR (code=500 AND path=/api/*)"#
        );
    }

    #[test]
    fn empty_panes_with_filter_reports_not_found() {
        let ctx = SearchExplainContext {
            pane_filter: Some(42),
            panes: vec![], // no panes at all
            ..empty_context()
        };
        let result = explain_search(&ctx);
        assert!(result.reasons.iter().any(|r| r.code == "PANE_NOT_FOUND"));
        assert_eq!(result.total_panes, 0);
    }
}
