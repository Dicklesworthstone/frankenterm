//! Terminal content indexing pipeline for FrankenSearch.
//!
//! Provides an incremental, background-safe pipeline that:
//! - Tracks per-pane indexing watermarks (high-water-mark of captured_at_ms)
//! - Extracts indexable documents from terminal scrollback, command output, and agent artifacts
//! - Deduplicates via content-hash (with optional cass hash set)
//! - Feeds extracted documents into [`SearchIndex`] in batches
//! - Pauses during resize storms to avoid thrashing
//! - Supports pause/resume control for background indexing
//!
//! # Architecture
//!
//! ```text
//! Terminal Pane Scrollback
//!          ↓
//!   ContentIndexingPipeline::ingest_pane_content()
//!          ↓
//!   [Watermark check: skip already-indexed content]
//!          ↓
//!   chunk_scrollback_lines() + extract_command_output_blocks() + extract_agent_artifacts()
//!          ↓
//!   SearchIndex::ingest_documents()
//!          ↓
//!   Persisted IndexedDocument + updated watermark
//! ```

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use super::indexing::{
    CommandBlockExtractionConfig, IndexableDocument, IndexingIngestReport, ScrollbackLine,
    SearchIndex, SearchIndexStats, chunk_scrollback_lines, extract_agent_artifacts,
    extract_command_output_blocks,
};

/// Per-pane watermark tracking what content has already been indexed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneWatermark {
    /// Pane identifier.
    pub pane_id: u64,
    /// Highest captured_at_ms value that has been indexed for this pane.
    pub last_indexed_at_ms: i64,
    /// Total documents indexed from this pane.
    pub total_docs_indexed: u64,
    /// Session ID associated with this pane (for metadata).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
}

/// Pipeline run mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PipelineState {
    /// Actively processing new content.
    Running,
    /// Temporarily paused (e.g., resize storm or manual pause).
    Paused,
    /// Pipeline has been stopped and requires explicit restart.
    Stopped,
}

/// Configuration for the indexing pipeline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PipelineConfig {
    /// Minimum gap in ms between scrollback lines to force a chunk boundary.
    pub scrollback_gap_ms: i64,
    /// Command block extraction configuration.
    pub command_block_config: CommandBlockExtractionConfig,
    /// Whether to extract agent artifacts (errors, code blocks, tool traces).
    pub extract_artifacts: bool,
    /// Maximum number of scrollback lines to process per pane per tick.
    pub max_lines_per_pane_tick: usize,
    /// Maximum number of panes to process per tick.
    pub max_panes_per_tick: usize,
    /// Whether to auto-pause during resize storms.
    pub pause_on_resize_storm: bool,
}

impl Default for PipelineConfig {
    fn default() -> Self {
        Self {
            scrollback_gap_ms: 10_000,
            command_block_config: CommandBlockExtractionConfig::default(),
            extract_artifacts: true,
            max_lines_per_pane_tick: 500,
            max_panes_per_tick: 20,
            pause_on_resize_storm: true,
        }
    }
}

/// Report from a single pipeline tick.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PipelineTickReport {
    /// Number of panes processed in this tick.
    pub panes_processed: usize,
    /// Number of panes skipped (no new content above watermark).
    pub panes_skipped: usize,
    /// Number of panes that hit the per-pane line limit.
    pub panes_truncated: usize,
    /// Total scrollback lines consumed across all panes.
    pub total_lines_consumed: usize,
    /// Aggregate ingest report from SearchIndex.
    pub ingest_report: IndexingIngestReport,
    /// Whether the tick was skipped due to pause or resize storm.
    pub skipped_reason: Option<PipelineSkipReason>,
}

/// Reason a pipeline tick was skipped.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PipelineSkipReason {
    Paused,
    ResizeStorm,
    Stopped,
    NoPanes,
}

/// Pipeline status snapshot for diagnostics.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PipelineStatus {
    pub state: PipelineState,
    pub watermarks: Vec<PaneWatermark>,
    pub total_ticks: u64,
    pub total_docs_indexed: u64,
    pub total_lines_consumed: u64,
    pub index_stats: SearchIndexStats,
}

/// Incremental terminal content indexing pipeline.
///
/// Maintains per-pane watermarks so only new content is indexed on each tick.
/// Integrates with [`SearchIndex`] for persistence, dedup, and lifecycle.
pub struct ContentIndexingPipeline {
    config: PipelineConfig,
    state: PipelineState,
    watermarks: HashMap<u64, PaneWatermark>,
    total_ticks: u64,
    total_docs_indexed: u64,
    total_lines_consumed: u64,
    index: SearchIndex,
}

impl ContentIndexingPipeline {
    /// Create a new pipeline wrapping the given search index.
    #[must_use]
    pub fn new(config: PipelineConfig, index: SearchIndex) -> Self {
        Self {
            config,
            state: PipelineState::Running,
            watermarks: HashMap::new(),
            total_ticks: 0,
            total_docs_indexed: 0,
            total_lines_consumed: 0,
            index,
        }
    }

    /// Get current pipeline state.
    #[must_use]
    pub fn state(&self) -> PipelineState {
        self.state
    }

    /// Pause the pipeline. No-op if already paused or stopped.
    pub fn pause(&mut self) {
        if self.state == PipelineState::Running {
            self.state = PipelineState::Paused;
        }
    }

    /// Resume the pipeline from paused state. No-op if running or stopped.
    pub fn resume(&mut self) {
        if self.state == PipelineState::Paused {
            self.state = PipelineState::Running;
        }
    }

    /// Stop the pipeline. Requires explicit restart via [`Self::restart`].
    pub fn stop(&mut self) {
        self.state = PipelineState::Stopped;
    }

    /// Restart a stopped pipeline.
    pub fn restart(&mut self) {
        if self.state == PipelineState::Stopped {
            self.state = PipelineState::Running;
        }
    }

    /// Get the watermark for a specific pane.
    #[must_use]
    pub fn watermark(&self, pane_id: u64) -> Option<&PaneWatermark> {
        self.watermarks.get(&pane_id)
    }

    /// Get all watermarks.
    #[must_use]
    pub fn watermarks(&self) -> &HashMap<u64, PaneWatermark> {
        &self.watermarks
    }

    /// Get a status snapshot for diagnostics.
    #[must_use]
    pub fn status(&self, now_ms: i64) -> PipelineStatus {
        PipelineStatus {
            state: self.state,
            watermarks: self.watermarks.values().cloned().collect(),
            total_ticks: self.total_ticks,
            total_docs_indexed: self.total_docs_indexed,
            total_lines_consumed: self.total_lines_consumed,
            index_stats: self.index.stats(now_ms),
        }
    }

    /// Access the underlying search index for queries.
    #[must_use]
    pub fn index(&self) -> &SearchIndex {
        &self.index
    }

    /// Mutable access to the underlying search index.
    pub fn index_mut(&mut self) -> &mut SearchIndex {
        &mut self.index
    }

    /// Run one indexing tick across the given pane content.
    ///
    /// Each entry in `pane_content` is `(pane_id, session_id, scrollback_lines)`.
    /// Lines with `captured_at_ms <= watermark` for that pane are skipped.
    ///
    /// Returns a report of what was processed.
    pub fn tick(
        &mut self,
        pane_content: &[(u64, Option<String>, Vec<ScrollbackLine>)],
        now_ms: i64,
        resize_storm_active: bool,
        cass_hashes: Option<&dyn super::indexing::CassContentHashProvider>,
    ) -> PipelineTickReport {
        self.total_ticks += 1;

        // Check skip conditions.
        if self.state == PipelineState::Stopped {
            return PipelineTickReport {
                skipped_reason: Some(PipelineSkipReason::Stopped),
                ..Default::default()
            };
        }
        if self.state == PipelineState::Paused {
            return PipelineTickReport {
                skipped_reason: Some(PipelineSkipReason::Paused),
                ..Default::default()
            };
        }
        if self.config.pause_on_resize_storm && resize_storm_active {
            return PipelineTickReport {
                skipped_reason: Some(PipelineSkipReason::ResizeStorm),
                ..Default::default()
            };
        }
        if pane_content.is_empty() {
            return PipelineTickReport {
                skipped_reason: Some(PipelineSkipReason::NoPanes),
                ..Default::default()
            };
        }

        let mut report = PipelineTickReport::default();
        let mut all_docs: Vec<IndexableDocument> = Vec::new();

        // Process each pane up to the per-tick pane limit.
        for (pane_id, session_id, lines) in
            pane_content.iter().take(self.config.max_panes_per_tick)
        {
            let watermark_ms = self
                .watermarks
                .get(pane_id)
                .map_or(i64::MIN, |w| w.last_indexed_at_ms);

            // Filter to only new lines above the watermark.
            let new_lines: Vec<ScrollbackLine> = lines
                .iter()
                .filter(|l| l.captured_at_ms > watermark_ms)
                .take(self.config.max_lines_per_pane_tick)
                .cloned()
                .collect();

            if new_lines.is_empty() {
                report.panes_skipped += 1;
                continue;
            }

            let was_truncated = lines.iter().filter(|l| l.captured_at_ms > watermark_ms).count()
                > self.config.max_lines_per_pane_tick;
            if was_truncated {
                report.panes_truncated += 1;
            }

            report.panes_processed += 1;
            report.total_lines_consumed += new_lines.len();

            // Annotate lines with pane/session info for extraction.
            let annotated: Vec<ScrollbackLine> = new_lines
                .into_iter()
                .map(|mut l| {
                    l.pane_id = Some(*pane_id);
                    l.session_id.clone_from(session_id);
                    l
                })
                .collect();

            // Extract documents using multiple strategies.
            let scrollback_docs =
                chunk_scrollback_lines(&annotated, self.config.scrollback_gap_ms);
            let command_docs =
                extract_command_output_blocks(&annotated, &self.config.command_block_config);

            all_docs.extend(scrollback_docs);
            all_docs.extend(command_docs);

            // Extract agent artifacts from the combined text.
            if self.config.extract_artifacts {
                let combined_text: String =
                    annotated.iter().map(|l| l.text.as_str()).collect::<Vec<_>>().join("\n");
                let max_ts = annotated
                    .iter()
                    .map(|l| l.captured_at_ms)
                    .max()
                    .unwrap_or(now_ms);
                let artifact_docs = extract_agent_artifacts(
                    &combined_text,
                    max_ts,
                    Some(*pane_id),
                    session_id.clone(),
                );
                all_docs.extend(artifact_docs);
            }

            // Update watermark to the highest timestamp we processed.
            let max_processed_ts = annotated
                .iter()
                .map(|l| l.captured_at_ms)
                .max()
                .unwrap_or(watermark_ms);
            let entry = self.watermarks.entry(*pane_id).or_insert_with(|| PaneWatermark {
                pane_id: *pane_id,
                last_indexed_at_ms: i64::MIN,
                total_docs_indexed: 0,
                session_id: session_id.clone(),
            });
            if max_processed_ts > entry.last_indexed_at_ms {
                entry.last_indexed_at_ms = max_processed_ts;
            }
            entry.session_id.clone_from(session_id);
        }

        // Ingest all extracted documents into the search index.
        if !all_docs.is_empty() {
            let ingest_report = match self
                .index
                .ingest_documents(&all_docs, now_ms, resize_storm_active, cass_hashes)
            {
                Ok(r) => r,
                Err(_) => {
                    // On ingest error, return partial report with what we have so far.
                    return report;
                }
            };

            // Update per-pane doc counts (approximate: distribute accepted docs
            // proportionally to pane content contribution).
            let accepted = ingest_report.accepted_docs as u64;
            self.total_docs_indexed += accepted;
            self.total_lines_consumed += report.total_lines_consumed as u64;

            // Distribute accepted docs to watermarks by counting per-pane contributions.
            let mut pane_doc_counts: HashMap<u64, u64> = HashMap::new();
            for doc in &all_docs {
                if let Some(pid) = doc.pane_id {
                    *pane_doc_counts.entry(pid).or_insert(0) += 1;
                }
            }
            let total_submitted = all_docs.len().max(1) as f64;
            for (pid, count) in &pane_doc_counts {
                if let Some(wm) = self.watermarks.get_mut(pid) {
                    // Scale accepted docs by this pane's share of submitted docs.
                    let share = (*count as f64 / total_submitted * accepted as f64).round() as u64;
                    wm.total_docs_indexed += share;
                }
            }

            report.ingest_report = ingest_report;
        }

        report
    }

    /// Ingest content for a single pane. Convenience wrapper around [`Self::tick`].
    pub fn ingest_pane_content(
        &mut self,
        pane_id: u64,
        session_id: Option<String>,
        lines: Vec<ScrollbackLine>,
        now_ms: i64,
        resize_storm_active: bool,
        cass_hashes: Option<&dyn super::indexing::CassContentHashProvider>,
    ) -> PipelineTickReport {
        self.tick(
            &[(pane_id, session_id, lines)],
            now_ms,
            resize_storm_active,
            cass_hashes,
        )
    }

    /// Remove watermark for a pane that has been closed/removed.
    pub fn remove_pane(&mut self, pane_id: u64) -> Option<PaneWatermark> {
        self.watermarks.remove(&pane_id)
    }

    /// Reset watermark for a pane, causing all content to be re-indexed on next tick.
    pub fn reset_pane_watermark(&mut self, pane_id: u64) {
        if let Some(wm) = self.watermarks.get_mut(&pane_id) {
            wm.last_indexed_at_ms = i64::MIN;
        }
    }

    /// Reset all watermarks, triggering full re-indexing on next tick.
    pub fn reset_all_watermarks(&mut self) {
        for wm in self.watermarks.values_mut() {
            wm.last_indexed_at_ms = i64::MIN;
        }
    }

    /// Trigger a manual flush of pending documents.
    pub fn flush(
        &mut self,
        now_ms: i64,
    ) -> std::result::Result<super::indexing::IndexingTickResult, super::indexing::SearchIndexError>
    {
        self.index
            .flush_now(now_ms, super::indexing::IndexFlushReason::Manual)
    }
}

// ============================================================================
// Robot type conversions
// ============================================================================

impl From<&PaneWatermark> for crate::robot_types::PipelineWatermarkInfo {
    fn from(wm: &PaneWatermark) -> Self {
        Self {
            pane_id: wm.pane_id,
            last_indexed_at_ms: wm.last_indexed_at_ms,
            total_docs_indexed: wm.total_docs_indexed,
            session_id: wm.session_id.clone(),
        }
    }
}

impl From<&PipelineStatus> for crate::robot_types::SearchPipelineStatusData {
    fn from(status: &PipelineStatus) -> Self {
        Self {
            state: match status.state {
                PipelineState::Running => "running".to_string(),
                PipelineState::Paused => "paused".to_string(),
                PipelineState::Stopped => "stopped".to_string(),
            },
            watermarks: status.watermarks.iter().map(Into::into).collect(),
            total_ticks: status.total_ticks,
            total_docs_indexed: status.total_docs_indexed,
            total_lines_consumed: status.total_lines_consumed,
            index_stats: serde_json::to_value(&status.index_stats).ok(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::search::indexing::IndexingConfig;

    fn test_config() -> PipelineConfig {
        PipelineConfig {
            scrollback_gap_ms: 5_000,
            command_block_config: CommandBlockExtractionConfig::default(),
            extract_artifacts: true,
            max_lines_per_pane_tick: 100,
            max_panes_per_tick: 10,
            pause_on_resize_storm: true,
        }
    }

    fn test_index(dir: &std::path::Path) -> SearchIndex {
        SearchIndex::open(IndexingConfig {
            index_dir: dir.to_path_buf(),
            max_index_size_bytes: 10 * 1024 * 1024,
            ttl_days: 30,
            flush_interval_secs: 1,
            flush_docs_threshold: 5,
            max_docs_per_second: 1000,
        })
        .unwrap()
    }

    fn make_lines(texts: &[&str], base_ms: i64, step_ms: i64) -> Vec<ScrollbackLine> {
        texts
            .iter()
            .enumerate()
            .map(|(i, t)| ScrollbackLine {
                text: (*t).to_string(),
                captured_at_ms: base_ms + (i as i64) * step_ms,
                pane_id: None,
                session_id: None,
            })
            .collect()
    }

    #[test]
    fn pipeline_state_transitions() {
        let dir = tempfile::tempdir().unwrap();
        let index = test_index(dir.path());
        let mut pipeline = ContentIndexingPipeline::new(test_config(), index);

        assert_eq!(pipeline.state(), PipelineState::Running);

        pipeline.pause();
        assert_eq!(pipeline.state(), PipelineState::Paused);

        // Pause again is no-op.
        pipeline.pause();
        assert_eq!(pipeline.state(), PipelineState::Paused);

        pipeline.resume();
        assert_eq!(pipeline.state(), PipelineState::Running);

        // Resume while running is no-op.
        pipeline.resume();
        assert_eq!(pipeline.state(), PipelineState::Running);

        pipeline.stop();
        assert_eq!(pipeline.state(), PipelineState::Stopped);

        // Resume doesn't work on stopped; must restart.
        pipeline.resume();
        assert_eq!(pipeline.state(), PipelineState::Stopped);

        pipeline.restart();
        assert_eq!(pipeline.state(), PipelineState::Running);
    }

    #[test]
    fn tick_skips_when_paused() {
        let dir = tempfile::tempdir().unwrap();
        let index = test_index(dir.path());
        let mut pipeline = ContentIndexingPipeline::new(test_config(), index);

        pipeline.pause();
        let report = pipeline.tick(&[], 1000, false, None);
        assert_eq!(report.skipped_reason, Some(PipelineSkipReason::Paused));
        assert_eq!(report.panes_processed, 0);
    }

    #[test]
    fn tick_skips_when_stopped() {
        let dir = tempfile::tempdir().unwrap();
        let index = test_index(dir.path());
        let mut pipeline = ContentIndexingPipeline::new(test_config(), index);

        pipeline.stop();
        let report = pipeline.tick(&[], 1000, false, None);
        assert_eq!(report.skipped_reason, Some(PipelineSkipReason::Stopped));
    }

    #[test]
    fn tick_skips_on_resize_storm() {
        let dir = tempfile::tempdir().unwrap();
        let index = test_index(dir.path());
        let mut pipeline = ContentIndexingPipeline::new(test_config(), index);

        let lines = make_lines(&["hello world"], 100, 10);
        let panes = vec![(1u64, None, lines)];
        let report = pipeline.tick(&panes, 1000, true, None);
        assert_eq!(report.skipped_reason, Some(PipelineSkipReason::ResizeStorm));
    }

    #[test]
    fn tick_skips_no_panes() {
        let dir = tempfile::tempdir().unwrap();
        let index = test_index(dir.path());
        let mut pipeline = ContentIndexingPipeline::new(test_config(), index);

        let report = pipeline.tick(&[], 1000, false, None);
        assert_eq!(report.skipped_reason, Some(PipelineSkipReason::NoPanes));
    }

    #[test]
    fn basic_indexing_flow() {
        let dir = tempfile::tempdir().unwrap();
        let index = test_index(dir.path());
        let mut pipeline = ContentIndexingPipeline::new(test_config(), index);

        let lines = make_lines(
            &[
                "$ cargo build",
                "   Compiling frankenterm v0.1.0",
                "   Finished dev [unoptimized] target(s)",
                "",
                "$ cargo test",
                "running 42 tests",
                "test result: ok. 42 passed",
            ],
            1000,
            100,
        );

        let panes = vec![(1u64, Some("session-a".to_string()), lines)];
        let report = pipeline.tick(&panes, 2000, false, None);

        assert_eq!(report.panes_processed, 1);
        assert_eq!(report.panes_skipped, 0);
        assert!(report.total_lines_consumed > 0);
        assert!(report.ingest_report.accepted_docs > 0);
        assert_eq!(report.skipped_reason, None);

        // Watermark should be updated.
        let wm = pipeline.watermark(1).unwrap();
        assert!(wm.last_indexed_at_ms > 0);
        assert_eq!(wm.session_id, Some("session-a".to_string()));
    }

    #[test]
    fn watermark_prevents_reindexing() {
        let dir = tempfile::tempdir().unwrap();
        let index = test_index(dir.path());
        let mut pipeline = ContentIndexingPipeline::new(test_config(), index);

        let lines = make_lines(&["line one", "line two"], 1000, 100);
        let panes = vec![(1u64, None, lines.clone())];

        // First tick indexes content.
        let r1 = pipeline.tick(&panes, 2000, false, None);
        assert!(r1.ingest_report.accepted_docs > 0);

        // Second tick with same content should skip (watermark above all timestamps).
        let r2 = pipeline.tick(&panes, 3000, false, None);
        assert_eq!(r2.panes_skipped, 1);
        assert_eq!(r2.panes_processed, 0);
    }

    #[test]
    fn new_content_above_watermark_is_indexed() {
        let dir = tempfile::tempdir().unwrap();
        let index = test_index(dir.path());
        let mut pipeline = ContentIndexingPipeline::new(test_config(), index);

        let lines1 = make_lines(&["first batch"], 1000, 100);
        let panes1 = vec![(1u64, None, lines1)];
        pipeline.tick(&panes1, 2000, false, None);

        // New content with higher timestamps.
        let lines2 = make_lines(&["second batch"], 2000, 100);
        let panes2 = vec![(1u64, None, lines2)];
        let r2 = pipeline.tick(&panes2, 3000, false, None);
        assert_eq!(r2.panes_processed, 1);
        assert!(r2.ingest_report.accepted_docs > 0);
    }

    #[test]
    fn multi_pane_indexing() {
        let dir = tempfile::tempdir().unwrap();
        let index = test_index(dir.path());
        let mut pipeline = ContentIndexingPipeline::new(test_config(), index);

        let lines_a = make_lines(&["pane A output"], 1000, 100);
        let lines_b = make_lines(&["pane B output"], 1000, 100);
        let panes = vec![
            (1u64, Some("sess-a".to_string()), lines_a),
            (2u64, Some("sess-b".to_string()), lines_b),
        ];

        let report = pipeline.tick(&panes, 2000, false, None);
        assert_eq!(report.panes_processed, 2);

        assert!(pipeline.watermark(1).is_some());
        assert!(pipeline.watermark(2).is_some());
    }

    #[test]
    fn max_panes_per_tick_limit() {
        let dir = tempfile::tempdir().unwrap();
        let index = test_index(dir.path());
        let mut config = test_config();
        config.max_panes_per_tick = 2;
        let mut pipeline = ContentIndexingPipeline::new(config, index);

        let panes: Vec<(u64, Option<String>, Vec<ScrollbackLine>)> = (0..5)
            .map(|i| {
                (
                    i as u64,
                    None,
                    make_lines(&["content"], 1000 + i * 100, 10),
                )
            })
            .collect();

        let report = pipeline.tick(&panes, 2000, false, None);
        // Should only process 2 panes despite 5 being available.
        assert!(report.panes_processed <= 2);
    }

    #[test]
    fn remove_pane_clears_watermark() {
        let dir = tempfile::tempdir().unwrap();
        let index = test_index(dir.path());
        let mut pipeline = ContentIndexingPipeline::new(test_config(), index);

        let lines = make_lines(&["hello"], 1000, 100);
        let panes = vec![(42u64, None, lines)];
        pipeline.tick(&panes, 2000, false, None);

        assert!(pipeline.watermark(42).is_some());
        let removed = pipeline.remove_pane(42);
        assert!(removed.is_some());
        assert!(pipeline.watermark(42).is_none());
    }

    #[test]
    fn reset_pane_watermark_enables_reindex() {
        let dir = tempfile::tempdir().unwrap();
        let index = test_index(dir.path());
        let mut pipeline = ContentIndexingPipeline::new(test_config(), index);

        let lines = make_lines(&["important data"], 1000, 100);
        let panes = vec![(1u64, None, lines.clone())];

        // First tick indexes.
        pipeline.tick(&panes, 2000, false, None);

        // Same content is skipped.
        let r2 = pipeline.tick(&panes, 3000, false, None);
        assert_eq!(r2.panes_skipped, 1);

        // Reset watermark.
        pipeline.reset_pane_watermark(1);

        // Now same content should be processed again (dedup in SearchIndex may still filter).
        let r3 = pipeline.tick(&panes, 4000, false, None);
        assert_eq!(r3.panes_processed, 1);
    }

    #[test]
    fn status_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let index = test_index(dir.path());
        let mut pipeline = ContentIndexingPipeline::new(test_config(), index);

        let lines = make_lines(&["test line"], 1000, 100);
        let panes = vec![(1u64, None, lines)];
        pipeline.tick(&panes, 2000, false, None);

        let status = pipeline.status(3000);
        assert_eq!(status.state, PipelineState::Running);
        assert_eq!(status.watermarks.len(), 1);
        assert!(status.total_ticks >= 1);
    }

    #[test]
    fn artifact_extraction_included() {
        let dir = tempfile::tempdir().unwrap();
        let index = test_index(dir.path());
        let mut pipeline = ContentIndexingPipeline::new(test_config(), index);

        let lines = make_lines(
            &[
                "error: cannot find value `foo` in this scope",
                "  --> src/main.rs:42:5",
                "   |",
                "42 |     foo.bar()",
                "   |     ^^^ not found in this scope",
            ],
            1000,
            100,
        );

        let panes = vec![(1u64, None, lines)];
        let report = pipeline.tick(&panes, 2000, false, None);
        // Should index both scrollback chunks and agent artifacts.
        assert!(report.ingest_report.submitted_docs >= 2);
    }

    #[test]
    fn artifact_extraction_disabled() {
        let dir = tempfile::tempdir().unwrap();
        let index = test_index(dir.path());
        let mut config = test_config();
        config.extract_artifacts = false;
        let mut pipeline = ContentIndexingPipeline::new(config, index);

        let lines = make_lines(
            &["error: something went wrong", "  --> src/lib.rs:10:3"],
            1000,
            100,
        );

        let panes = vec![(1u64, None, lines)];
        let report = pipeline.tick(&panes, 2000, false, None);
        // With artifacts disabled, fewer docs should be submitted (only scrollback + command).
        // Just verify it doesn't crash and processes the pane.
        assert_eq!(report.panes_processed, 1);
    }

    #[test]
    fn resize_storm_pause_can_be_disabled() {
        let dir = tempfile::tempdir().unwrap();
        let index = test_index(dir.path());
        let mut config = test_config();
        config.pause_on_resize_storm = false;
        let mut pipeline = ContentIndexingPipeline::new(config, index);

        let lines = make_lines(&["content during storm"], 1000, 100);
        let panes = vec![(1u64, None, lines)];

        // Even with resize_storm_active=true, pipeline should process.
        let report = pipeline.tick(&panes, 2000, true, None);
        assert_eq!(report.panes_processed, 1);
        assert_eq!(report.skipped_reason, None);
    }

    #[test]
    fn flush_delegates_to_index() {
        let dir = tempfile::tempdir().unwrap();
        let index = test_index(dir.path());
        let mut pipeline = ContentIndexingPipeline::new(test_config(), index);

        let lines = make_lines(&["data to flush"], 1000, 100);
        let panes = vec![(1u64, None, lines)];
        pipeline.tick(&panes, 2000, false, None);

        let result = pipeline.flush(3000).unwrap();
        // Should not panic regardless of whether there's pending data.
        let _ = result.flushed_docs; // Should not panic regardless of pending data.
    }

    #[test]
    fn ingest_pane_content_convenience() {
        let dir = tempfile::tempdir().unwrap();
        let index = test_index(dir.path());
        let mut pipeline = ContentIndexingPipeline::new(test_config(), index);

        let lines = make_lines(&["single pane content"], 1000, 100);
        let report = pipeline.ingest_pane_content(
            5,
            Some("my-session".to_string()),
            lines,
            2000,
            false,
            None,
        );

        assert_eq!(report.panes_processed, 1);
        let wm = pipeline.watermark(5).unwrap();
        assert_eq!(wm.session_id, Some("my-session".to_string()));
    }

    #[test]
    fn reset_all_watermarks() {
        let dir = tempfile::tempdir().unwrap();
        let index = test_index(dir.path());
        let mut pipeline = ContentIndexingPipeline::new(test_config(), index);

        // Index two panes.
        let panes = vec![
            (1u64, None, make_lines(&["a"], 1000, 100)),
            (2u64, None, make_lines(&["b"], 1000, 100)),
        ];
        pipeline.tick(&panes, 2000, false, None);
        assert!(pipeline.watermark(1).unwrap().last_indexed_at_ms > i64::MIN);
        assert!(pipeline.watermark(2).unwrap().last_indexed_at_ms > i64::MIN);

        pipeline.reset_all_watermarks();
        assert_eq!(pipeline.watermark(1).unwrap().last_indexed_at_ms, i64::MIN);
        assert_eq!(pipeline.watermark(2).unwrap().last_indexed_at_ms, i64::MIN);
    }

    #[test]
    fn pipeline_serde_roundtrip_types() {
        // Verify key types can round-trip through serde.
        let wm = PaneWatermark {
            pane_id: 42,
            last_indexed_at_ms: 123_456,
            total_docs_indexed: 10,
            session_id: Some("sess-1".to_string()),
        };
        let json = serde_json::to_string(&wm).unwrap();
        let wm2: PaneWatermark = serde_json::from_str(&json).unwrap();
        assert_eq!(wm, wm2);

        let state = PipelineState::Paused;
        let json = serde_json::to_string(&state).unwrap();
        let state2: PipelineState = serde_json::from_str(&json).unwrap();
        assert_eq!(state, state2);

        let reason = PipelineSkipReason::ResizeStorm;
        let json = serde_json::to_string(&reason).unwrap();
        let reason2: PipelineSkipReason = serde_json::from_str(&json).unwrap();
        assert_eq!(reason, reason2);
    }

    #[test]
    fn watermark_to_robot_type() {
        let wm = PaneWatermark {
            pane_id: 7,
            last_indexed_at_ms: 5000,
            total_docs_indexed: 25,
            session_id: Some("sess-x".to_string()),
        };
        let info: crate::robot_types::PipelineWatermarkInfo = (&wm).into();
        assert_eq!(info.pane_id, 7);
        assert_eq!(info.last_indexed_at_ms, 5000);
        assert_eq!(info.total_docs_indexed, 25);
        assert_eq!(info.session_id.as_deref(), Some("sess-x"));
    }

    #[test]
    fn pipeline_status_to_robot_type() {
        let dir = tempfile::tempdir().unwrap();
        let index = test_index(dir.path());
        let mut pipeline = ContentIndexingPipeline::new(test_config(), index);

        let lines = make_lines(&["test data"], 1000, 100);
        let panes = vec![(1u64, Some("sess-a".to_string()), lines)];
        pipeline.tick(&panes, 2000, false, None);

        let status = pipeline.status(3000);
        let robot_status: crate::robot_types::SearchPipelineStatusData = (&status).into();

        assert_eq!(robot_status.state, "running");
        assert!(!robot_status.watermarks.is_empty());
        assert!(robot_status.total_ticks >= 1);
        assert!(robot_status.index_stats.is_some());
    }
}
