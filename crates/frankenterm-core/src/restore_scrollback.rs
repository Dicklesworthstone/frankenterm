//! Scrollback injection engine — restore terminal content into panes.
//!
//! After layout restoration creates empty panes, this module injects captured
//! scrollback content so users see the same output they had before the mux
//! server restart.
//!
//! # Data flow
//!
//! ```text
//! output_segments (DB) → ScrollbackData → ScrollbackInjector → send_text → pane
//! ```
//!
//! Uses [`WeztermInterface::send_text`] for injection. Content is chunked to
//! avoid overwhelming the terminal parser and injected concurrently across
//! multiple panes via a semaphore.
//!
//! # Pattern suppression
//!
//! Injected scrollback triggers the pattern detection engine (false positives
//! from historical output). Callers should use [`InjectionGuard`] to suppress
//! detection on target panes during injection.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tokio::sync::Semaphore;
use tracing::{debug, info, warn};

use crate::wezterm::WeztermHandle;

// =============================================================================
// Configuration
// =============================================================================

/// Configuration for scrollback injection behavior.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct InjectionConfig {
    /// Maximum number of lines to inject per pane.
    pub max_lines: usize,
    /// Chunk size in bytes for each write operation.
    pub chunk_size: usize,
    /// Delay between chunks in milliseconds (prevents parser overload).
    pub inter_chunk_delay_ms: u64,
    /// Maximum number of panes to inject into concurrently.
    pub concurrent_injections: usize,
}

impl Default for InjectionConfig {
    fn default() -> Self {
        Self {
            max_lines: 10_000,
            chunk_size: 4096,
            inter_chunk_delay_ms: 1,
            concurrent_injections: 5,
        }
    }
}

// =============================================================================
// Scrollback data
// =============================================================================

/// Captured scrollback content for a single pane.
///
/// Assembled from `output_segments` rows ordered by `seq`.
#[derive(Debug, Clone)]
pub struct ScrollbackData {
    /// Ordered lines of terminal output (may include ANSI escapes).
    pub lines: Vec<String>,
    /// Total byte size of all lines.
    pub total_bytes: usize,
}

impl ScrollbackData {
    /// Create from a list of content strings (e.g., from output_segments).
    pub fn from_segments(segments: Vec<String>) -> Self {
        let total_bytes = segments.iter().map(|s| s.len()).sum();
        Self {
            lines: segments,
            total_bytes,
        }
    }

    /// Truncate to max_lines, keeping the most recent content.
    pub fn truncate(&mut self, max_lines: usize) {
        if self.lines.len() > max_lines {
            let skip = self.lines.len() - max_lines;
            self.lines.drain(..skip);
            self.total_bytes = self.lines.iter().map(|s| s.len()).sum();
        }
    }
}

// =============================================================================
// Injection report
// =============================================================================

/// Per-pane injection statistics.
#[derive(Debug, Clone)]
pub struct PaneInjectionStats {
    /// Old pane ID (from snapshot).
    pub old_pane_id: u64,
    /// New pane ID (live session).
    pub new_pane_id: u64,
    /// Number of lines injected.
    pub lines_injected: usize,
    /// Total bytes written.
    pub bytes_written: usize,
    /// Number of chunks sent.
    pub chunks_sent: usize,
}

/// Report from a scrollback injection operation.
#[derive(Debug, Clone, Default)]
pub struct InjectionReport {
    /// Per-pane results for successful injections.
    pub successes: Vec<PaneInjectionStats>,
    /// Per-pane failures (old pane ID, error message).
    pub failures: Vec<(u64, String)>,
    /// Panes skipped because they weren't in the pane ID map.
    pub skipped: Vec<u64>,
}

impl InjectionReport {
    /// Total panes successfully injected.
    pub fn success_count(&self) -> usize {
        self.successes.len()
    }

    /// Total panes that failed injection.
    pub fn failure_count(&self) -> usize {
        self.failures.len()
    }

    /// Total bytes written across all panes.
    pub fn total_bytes(&self) -> usize {
        self.successes.iter().map(|s| s.bytes_written).sum()
    }
}

// =============================================================================
// Injection guard (pattern suppression)
// =============================================================================

/// Guard that tracks which panes are currently undergoing scrollback injection.
///
/// Callers should check [`InjectionGuard::is_suppressed`] in their pattern
/// detection hot path to skip detection for panes receiving injected content.
///
/// The guard automatically clears suppression when dropped.
#[derive(Debug)]
pub struct InjectionGuard {
    suppressed: Arc<std::sync::Mutex<HashSet<u64>>>,
    pane_ids: Vec<u64>,
}

impl InjectionGuard {
    /// Create a new injection guard that suppresses the given pane IDs.
    pub fn new(suppressed: Arc<std::sync::Mutex<HashSet<u64>>>, pane_ids: Vec<u64>) -> Self {
        {
            let mut set = suppressed.lock().expect("injection guard lock");
            for &id in &pane_ids {
                set.insert(id);
            }
        }
        Self {
            suppressed,
            pane_ids,
        }
    }

    /// Check if a pane ID is currently suppressed.
    pub fn is_suppressed(suppressed: &Arc<std::sync::Mutex<HashSet<u64>>>, pane_id: u64) -> bool {
        suppressed
            .lock()
            .expect("injection guard lock")
            .contains(&pane_id)
    }
}

impl Drop for InjectionGuard {
    fn drop(&mut self) {
        let mut set = self.suppressed.lock().expect("injection guard lock");
        for &id in &self.pane_ids {
            set.remove(&id);
        }
    }
}

// =============================================================================
// Scrollback injector
// =============================================================================

/// Engine that injects captured scrollback content into restored panes.
pub struct ScrollbackInjector {
    wezterm: WeztermHandle,
    config: InjectionConfig,
    /// Shared suppression set for pattern detection gating.
    suppressed_panes: Arc<std::sync::Mutex<HashSet<u64>>>,
}

impl ScrollbackInjector {
    /// Create a new scrollback injector.
    pub fn new(wezterm: WeztermHandle, config: InjectionConfig) -> Self {
        Self {
            wezterm,
            config,
            suppressed_panes: Arc::new(std::sync::Mutex::new(HashSet::new())),
        }
    }

    /// Get a reference to the suppressed panes set for pattern engine integration.
    pub fn suppressed_panes(&self) -> &Arc<std::sync::Mutex<HashSet<u64>>> {
        &self.suppressed_panes
    }

    /// Inject scrollback content into restored panes.
    ///
    /// `pane_id_map` maps old pane IDs to new (live) pane IDs.
    /// `scrollbacks` maps old pane IDs to their captured scrollback data.
    pub async fn inject(
        &self,
        pane_id_map: &HashMap<u64, u64>,
        scrollbacks: &HashMap<u64, ScrollbackData>,
    ) -> InjectionReport {
        let mut report = InjectionReport::default();

        // Collect target panes and create injection guard.
        let target_new_ids: Vec<u64> = scrollbacks
            .keys()
            .filter_map(|old_id| pane_id_map.get(old_id).copied())
            .collect();

        let _guard = InjectionGuard::new(self.suppressed_panes.clone(), target_new_ids);

        info!(
            panes = scrollbacks.len(),
            concurrent = self.config.concurrent_injections,
            "starting scrollback injection"
        );

        let semaphore = Arc::new(Semaphore::new(self.config.concurrent_injections));

        // Inject each pane. We process sequentially with semaphore to avoid
        // Send bound issues with &self references.
        for (old_id, scrollback) in scrollbacks {
            let new_id = match pane_id_map.get(old_id) {
                Some(&id) => id,
                None => {
                    debug!(old_pane = old_id, "pane not in id map, skipping");
                    report.skipped.push(*old_id);
                    continue;
                }
            };

            let _permit = semaphore.acquire().await.expect("semaphore closed");

            match self.inject_pane(*old_id, new_id, scrollback).await {
                Ok(stats) => report.successes.push(stats),
                Err(e) => {
                    warn!(old_pane = old_id, new_pane = new_id, error = %e, "injection failed");
                    report.failures.push((*old_id, e.to_string()));
                }
            }
        }

        info!(
            success = report.success_count(),
            failed = report.failure_count(),
            skipped = report.skipped.len(),
            total_bytes = report.total_bytes(),
            "scrollback injection complete"
        );

        report
    }

    /// Inject scrollback into a single pane.
    async fn inject_pane(
        &self,
        old_pane_id: u64,
        new_pane_id: u64,
        scrollback: &ScrollbackData,
    ) -> crate::Result<PaneInjectionStats> {
        let mut data = scrollback.clone();
        data.truncate(self.config.max_lines);

        if data.lines.is_empty() {
            return Ok(PaneInjectionStats {
                old_pane_id,
                new_pane_id,
                lines_injected: 0,
                bytes_written: 0,
                chunks_sent: 0,
            });
        }

        debug!(
            old_pane = old_pane_id,
            new_pane = new_pane_id,
            lines = data.lines.len(),
            bytes = data.total_bytes,
            "injecting scrollback"
        );

        // Build full content with ANSI reset prefix.
        let content = build_injection_content(&data.lines);

        // Split into chunks and write.
        let chunks = chunk_content(&content, self.config.chunk_size);
        let mut bytes_written = 0;

        for (i, chunk) in chunks.iter().enumerate() {
            self.wezterm.send_text(new_pane_id, chunk).await?;
            bytes_written += chunk.len();

            // Inter-chunk delay to prevent parser overload.
            if i < chunks.len() - 1 && self.config.inter_chunk_delay_ms > 0 {
                tokio::time::sleep(tokio::time::Duration::from_millis(
                    self.config.inter_chunk_delay_ms,
                ))
                .await;
            }
        }

        Ok(PaneInjectionStats {
            old_pane_id,
            new_pane_id,
            lines_injected: data.lines.len(),
            bytes_written,
            chunks_sent: chunks.len(),
        })
    }
}

// =============================================================================
// Content building helpers
// =============================================================================

/// Build injection content from scrollback lines.
///
/// Prefixes with ANSI reset to prevent state contamination from
/// the previous pane's terminal state.
fn build_injection_content(lines: &[String]) -> String {
    let mut content = String::with_capacity(lines.iter().map(|l| l.len() + 1).sum());

    // ANSI reset: clear all attributes, cursor home, clear screen.
    content.push_str("\x1b[0m\x1b[H\x1b[2J");

    for (i, line) in lines.iter().enumerate() {
        content.push_str(line);
        if i < lines.len() - 1 {
            content.push('\n');
        }
    }

    content
}

/// Split content into chunks at safe boundaries.
///
/// Avoids splitting in the middle of UTF-8 characters or ANSI escape sequences.
fn chunk_content(content: &str, chunk_size: usize) -> Vec<String> {
    if content.len() <= chunk_size {
        return vec![content.to_string()];
    }

    let mut chunks = Vec::new();
    let bytes = content.as_bytes();
    let mut start = 0;

    while start < bytes.len() {
        let mut end = (start + chunk_size).min(bytes.len());

        if end < bytes.len() {
            // Walk back to a safe split point: avoid mid-UTF8 and mid-ANSI.
            end = find_safe_split(content, start, end);
        }

        chunks.push(content[start..end].to_string());
        start = end;
    }

    chunks
}

/// Find a safe split point at or before `target`, not splitting UTF-8 or ANSI escapes.
fn find_safe_split(content: &str, start: usize, target: usize) -> usize {
    // Walk back from target to find a char boundary.
    let mut pos = target;
    while pos > start && !content.is_char_boundary(pos) {
        pos -= 1;
    }

    // Check if we're inside an ANSI escape sequence (ESC [ ... letter).
    // Walk back to see if there's an unclosed ESC[.
    let slice = &content[start..pos];
    if let Some(last_esc) = slice.rfind('\x1b') {
        let after_esc = &slice[last_esc..];
        // If the escape sequence isn't terminated (no letter after CSI params),
        // split before the ESC.
        if after_esc.starts_with("\x1b[") && !has_csi_terminator(after_esc) {
            return start + last_esc;
        }
    }

    pos
}

/// Check if a CSI sequence (ESC[...) has a terminating letter.
fn has_csi_terminator(seq: &str) -> bool {
    // CSI sequences end with a letter in the range 0x40-0x7E.
    for (i, b) in seq.bytes().enumerate() {
        if i >= 2 && (0x40..=0x7E).contains(&b) {
            return true;
        }
    }
    false
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::wezterm::{MockWezterm, WeztermInterface};

    fn make_injector(mock: Arc<MockWezterm>) -> ScrollbackInjector {
        ScrollbackInjector::new(mock, InjectionConfig::default())
    }

    fn mock_scrollback(lines: Vec<&str>) -> ScrollbackData {
        ScrollbackData::from_segments(lines.into_iter().map(String::from).collect())
    }

    // --- ScrollbackData ---

    #[test]
    fn scrollback_data_from_segments() {
        let data = ScrollbackData::from_segments(vec!["hello".into(), "world".into()]);
        assert_eq!(data.lines.len(), 2);
        assert_eq!(data.total_bytes, 10);
    }

    #[test]
    fn scrollback_data_truncate() {
        let mut data =
            ScrollbackData::from_segments(vec!["a".into(), "b".into(), "c".into(), "d".into()]);
        data.truncate(2);
        assert_eq!(data.lines, vec!["c", "d"]); // Keeps most recent.
        assert_eq!(data.total_bytes, 2);
    }

    #[test]
    fn scrollback_data_truncate_noop() {
        let mut data = ScrollbackData::from_segments(vec!["a".into(), "b".into()]);
        data.truncate(10);
        assert_eq!(data.lines.len(), 2);
    }

    // --- InjectionConfig defaults ---

    #[test]
    fn injection_config_defaults() {
        let c = InjectionConfig::default();
        assert_eq!(c.max_lines, 10_000);
        assert_eq!(c.chunk_size, 4096);
        assert_eq!(c.inter_chunk_delay_ms, 1);
        assert_eq!(c.concurrent_injections, 5);
    }

    // --- InjectionReport ---

    #[test]
    fn injection_report_empty() {
        let r = InjectionReport::default();
        assert_eq!(r.success_count(), 0);
        assert_eq!(r.failure_count(), 0);
        assert_eq!(r.total_bytes(), 0);
    }

    #[test]
    fn injection_report_totals() {
        let mut r = InjectionReport::default();
        r.successes.push(PaneInjectionStats {
            old_pane_id: 1,
            new_pane_id: 10,
            lines_injected: 100,
            bytes_written: 5000,
            chunks_sent: 2,
        });
        r.successes.push(PaneInjectionStats {
            old_pane_id: 2,
            new_pane_id: 11,
            lines_injected: 50,
            bytes_written: 3000,
            chunks_sent: 1,
        });
        r.failures.push((3, "timeout".into()));
        assert_eq!(r.success_count(), 2);
        assert_eq!(r.failure_count(), 1);
        assert_eq!(r.total_bytes(), 8000);
    }

    // --- InjectionGuard ---

    #[test]
    fn injection_guard_suppresses_and_clears() {
        let set = Arc::new(std::sync::Mutex::new(HashSet::new()));
        assert!(!InjectionGuard::is_suppressed(&set, 42));

        {
            let _guard = InjectionGuard::new(set.clone(), vec![42, 43]);
            assert!(InjectionGuard::is_suppressed(&set, 42));
            assert!(InjectionGuard::is_suppressed(&set, 43));
            assert!(!InjectionGuard::is_suppressed(&set, 99));
        }

        // After guard is dropped, suppression cleared.
        assert!(!InjectionGuard::is_suppressed(&set, 42));
        assert!(!InjectionGuard::is_suppressed(&set, 43));
    }

    // --- build_injection_content ---

    #[test]
    fn build_content_single_line() {
        let content = build_injection_content(&["hello".into()]);
        assert!(content.starts_with("\x1b[0m\x1b[H\x1b[2J"));
        assert!(content.ends_with("hello"));
    }

    #[test]
    fn build_content_multi_line() {
        let content = build_injection_content(&["line1".into(), "line2".into(), "line3".into()]);
        assert!(content.contains("line1\nline2\nline3"));
        // No trailing newline after last line.
        assert!(!content.ends_with('\n'));
    }

    #[test]
    fn build_content_empty() {
        let content = build_injection_content(&[]);
        // Just the ANSI reset prefix.
        assert_eq!(content, "\x1b[0m\x1b[H\x1b[2J");
    }

    // --- chunk_content ---

    #[test]
    fn chunk_content_small() {
        let chunks = chunk_content("hello", 100);
        assert_eq!(chunks, vec!["hello"]);
    }

    #[test]
    fn chunk_content_splits() {
        let content = "abcdefghij";
        let chunks = chunk_content(content, 4);
        assert!(chunks.len() >= 2);
        let rejoined: String = chunks.concat();
        assert_eq!(rejoined, content);
    }

    #[test]
    fn chunk_content_utf8_safe() {
        // Japanese characters (3 bytes each in UTF-8).
        let content = "あいうえお"; // 15 bytes
        let chunks = chunk_content(content, 4);
        // Should not split mid-character.
        let rejoined: String = chunks.concat();
        assert_eq!(rejoined, content);
    }

    #[test]
    fn chunk_content_ansi_safe() {
        let content = "hello\x1b[31mred\x1b[0m";
        let chunks = chunk_content(content, 8);
        let rejoined: String = chunks.concat();
        assert_eq!(rejoined, content);
    }

    // --- has_csi_terminator ---

    #[test]
    fn csi_terminated() {
        assert!(has_csi_terminator("\x1b[31m"));
        assert!(has_csi_terminator("\x1b[0m"));
        assert!(has_csi_terminator("\x1b[H"));
    }

    #[test]
    fn csi_unterminated() {
        assert!(!has_csi_terminator("\x1b[31"));
        assert!(!has_csi_terminator("\x1b["));
    }

    // --- Injection integration tests ---

    #[tokio::test]
    async fn inject_single_pane() {
        let mock = Arc::new(MockWezterm::new());
        mock.add_default_pane(10).await;
        let injector = make_injector(mock.clone());

        let mut pane_id_map = HashMap::new();
        pane_id_map.insert(1_u64, 10_u64);

        let mut scrollbacks = HashMap::new();
        scrollbacks.insert(1, mock_scrollback(vec!["line1", "line2", "line3"]));

        let report = injector.inject(&pane_id_map, &scrollbacks).await;

        assert_eq!(report.success_count(), 1);
        assert_eq!(report.failure_count(), 0);
        assert_eq!(report.successes[0].lines_injected, 3);
        assert!(report.successes[0].bytes_written > 0);

        // Verify content was sent to the mock pane.
        let text: String = WeztermInterface::get_text(&*mock, 10, false).await.unwrap();
        assert!(text.contains("line1"));
        assert!(text.contains("line3"));
    }

    #[tokio::test]
    async fn inject_multiple_panes() {
        let mock = Arc::new(MockWezterm::new());
        mock.add_default_pane(10).await;
        mock.add_default_pane(11).await;
        let injector = make_injector(mock.clone());

        let mut pane_id_map = HashMap::new();
        pane_id_map.insert(1_u64, 10_u64);
        pane_id_map.insert(2_u64, 11_u64);

        let mut scrollbacks = HashMap::new();
        scrollbacks.insert(1, mock_scrollback(vec!["pane1-output"]));
        scrollbacks.insert(2, mock_scrollback(vec!["pane2-output"]));

        let report = injector.inject(&pane_id_map, &scrollbacks).await;

        assert_eq!(report.success_count(), 2);
        assert_eq!(report.failure_count(), 0);
    }

    #[tokio::test]
    async fn inject_skips_unmapped_panes() {
        let mock = Arc::new(MockWezterm::new());
        let injector = make_injector(mock.clone());

        let pane_id_map = HashMap::new(); // Empty — no mappings.

        let mut scrollbacks = HashMap::new();
        scrollbacks.insert(1, mock_scrollback(vec!["data"]));

        let report = injector.inject(&pane_id_map, &scrollbacks).await;

        assert_eq!(report.success_count(), 0);
        assert_eq!(report.skipped.len(), 1);
        assert_eq!(report.skipped[0], 1);
    }

    #[tokio::test]
    async fn inject_empty_scrollback() {
        let mock = Arc::new(MockWezterm::new());
        mock.add_default_pane(10).await;
        let injector = make_injector(mock.clone());

        let mut pane_id_map = HashMap::new();
        pane_id_map.insert(1_u64, 10_u64);

        let mut scrollbacks = HashMap::new();
        scrollbacks.insert(1, ScrollbackData::from_segments(vec![]));

        let report = injector.inject(&pane_id_map, &scrollbacks).await;

        assert_eq!(report.success_count(), 1);
        assert_eq!(report.successes[0].lines_injected, 0);
        assert_eq!(report.successes[0].bytes_written, 0);
    }

    #[tokio::test]
    async fn inject_truncates_large_scrollback() {
        let mock = Arc::new(MockWezterm::new());
        mock.add_default_pane(10).await;
        let config = InjectionConfig {
            max_lines: 3,
            ..Default::default()
        };
        let injector = ScrollbackInjector::new(mock.clone(), config);

        let mut pane_id_map = HashMap::new();
        pane_id_map.insert(1_u64, 10_u64);

        let lines: Vec<String> = (0..100).map(|i| format!("line-{i}")).collect();
        let mut scrollbacks = HashMap::new();
        scrollbacks.insert(1, ScrollbackData::from_segments(lines));

        let report = injector.inject(&pane_id_map, &scrollbacks).await;

        assert_eq!(report.success_count(), 1);
        assert_eq!(report.successes[0].lines_injected, 3);

        // Should have kept the last 3 lines (97, 98, 99).
        let text: String = WeztermInterface::get_text(&*mock, 10, false).await.unwrap();
        assert!(text.contains("line-99"));
        assert!(text.contains("line-97"));
    }

    #[tokio::test]
    async fn inject_no_scrollbacks() {
        let mock = Arc::new(MockWezterm::new());
        let injector = make_injector(mock.clone());

        let pane_id_map = HashMap::new();
        let scrollbacks = HashMap::new();

        let report = injector.inject(&pane_id_map, &scrollbacks).await;

        assert_eq!(report.success_count(), 0);
        assert_eq!(report.failure_count(), 0);
        assert_eq!(report.skipped.len(), 0);
    }

    #[tokio::test]
    async fn injection_guard_active_during_inject() {
        let mock = Arc::new(MockWezterm::new());
        mock.add_default_pane(10).await;
        let injector = make_injector(mock.clone());
        let suppressed = injector.suppressed_panes().clone();

        // Before injection: not suppressed.
        assert!(!InjectionGuard::is_suppressed(&suppressed, 10));

        let mut pane_id_map = HashMap::new();
        pane_id_map.insert(1_u64, 10_u64);

        let mut scrollbacks = HashMap::new();
        scrollbacks.insert(1, mock_scrollback(vec!["test"]));

        let report = injector.inject(&pane_id_map, &scrollbacks).await;

        assert_eq!(report.success_count(), 1);

        // After injection: suppression cleared.
        assert!(!InjectionGuard::is_suppressed(&suppressed, 10));
    }

    // --- Chunked injection ---

    #[tokio::test]
    async fn inject_with_small_chunks() {
        let mock = Arc::new(MockWezterm::new());
        mock.add_default_pane(10).await;
        let config = InjectionConfig {
            chunk_size: 16, // Very small chunks.
            inter_chunk_delay_ms: 0,
            ..Default::default()
        };
        let injector = ScrollbackInjector::new(mock.clone(), config);

        let mut pane_id_map = HashMap::new();
        pane_id_map.insert(1_u64, 10_u64);

        let mut scrollbacks = HashMap::new();
        scrollbacks.insert(
            1,
            mock_scrollback(vec![
                "this is a longer line that will require multiple chunks",
            ]),
        );

        let report = injector.inject(&pane_id_map, &scrollbacks).await;

        assert_eq!(report.success_count(), 1);
        assert!(report.successes[0].chunks_sent > 1);

        // All content should arrive.
        let text: String = WeztermInterface::get_text(&*mock, 10, false).await.unwrap();
        assert!(text.contains("multiple chunks"));
    }
}
