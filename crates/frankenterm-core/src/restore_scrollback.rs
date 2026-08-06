//! Scrollback injection engine — restore terminal content into panes.
//!
//! Historical scrollback replay is intentionally unavailable until the mux
//! exposes a terminal-output or render-state restoration channel.
//!
//! # Data flow
//!
//! ```text
//! terminal-state snapshot → safe mux output channel → restored pane
//! ```
//!
//! PTY input APIs such as `send_text` are categorically unsafe for historical
//! output: captured bytes may contain shell commands, control characters, or
//! partial stream fragments. The public injector therefore fails closed before
//! touching a pane. Its data/report types remain as the contract surface for a
//! future safe mux-owned implementation.
//!
//! # Pattern suppression contract
//!
//! A future mux-owned output channel will need to suppress pattern detection
//! while it restores historical display state. [`InjectionGuard`] retains that
//! nesting-safe contract, but the current fail-closed injector never marks a
//! pane suppressed because it never mutates terminal state.

use std::collections::HashMap;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::error::RuntimeOperationSource;
use crate::wezterm::WeztermHandle;

fn restore_scrollback_cancelled_error(operation: &'static str) -> crate::Error {
    crate::Error::RuntimeOperation {
        operation,
        // Cancellation causes may contain caller-controlled or capability
        // internals. Keep this boundary finite and content-free.
        source: RuntimeOperationSource::Cancelled("caller capability stopped".to_string()),
    }
}

fn restore_scrollback_unsupported_error() -> crate::Error {
    crate::Error::RuntimeOperation {
        operation: "restore_scrollback.inject.no_safe_output_channel",
        source: RuntimeOperationSource::Backend(
            "historical terminal output cannot be restored through PTY input".to_string(),
        ),
    }
}

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

/// Logical terminal lines for a future safe render-state restoration channel.
///
/// Raw `output_segments` must not be converted with this type: each database
/// row is an arbitrary stream fragment and may contain zero, one, or many
/// lines, or end in the middle of a line.
#[derive(Debug, Clone)]
pub struct ScrollbackData {
    /// Ordered lines of terminal output (may include ANSI escapes).
    pub lines: Vec<String>,
}

impl ScrollbackData {
    /// Create from already reconstructed logical terminal lines.
    pub fn from_terminal_lines(lines: Vec<String>) -> Self {
        Self { lines }
    }

    /// Total UTF-8 byte size of the current logical lines.
    pub fn total_bytes(&self) -> usize {
        self.lines.iter().map(String::len).sum()
    }

    /// Truncate to max_lines, keeping the most recent content.
    pub fn truncate(&mut self, max_lines: usize) {
        if self.lines.len() > max_lines {
            let skip = self.lines.len() - max_lines;
            self.lines.drain(..skip);
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
    suppressed: Arc<std::sync::Mutex<HashMap<u64, usize>>>,
    pane_ids: Vec<u64>,
}

impl InjectionGuard {
    /// Create a new injection guard that suppresses the given pane IDs.
    pub fn new(
        suppressed: Arc<std::sync::Mutex<HashMap<u64, usize>>>,
        pane_ids: Vec<u64>,
    ) -> Self {
        {
            let mut set = suppressed
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            for &id in &pane_ids {
                *set.entry(id).or_insert(0) += 1;
            }
        }
        Self {
            suppressed,
            pane_ids,
        }
    }

    /// Check if a pane ID is currently suppressed.
    pub fn is_suppressed(
        suppressed: &Arc<std::sync::Mutex<HashMap<u64, usize>>>,
        pane_id: u64,
    ) -> bool {
        suppressed
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&pane_id)
            .is_some_and(|count| *count > 0)
    }
}

impl Drop for InjectionGuard {
    fn drop(&mut self) {
        let mut set = self
            .suppressed
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for &id in &self.pane_ids {
            let remove = if let Some(count) = set.get_mut(&id) {
                *count = count.saturating_sub(1);
                *count == 0
            } else {
                false
            };
            if remove {
                set.remove(&id);
            }
        }
    }
}

// =============================================================================
// Scrollback injector
// =============================================================================

/// Capability gate for future mux-owned scrollback restoration.
pub struct ScrollbackInjector {
    /// Shared suppression set for pattern detection gating.
    suppressed_panes: Arc<std::sync::Mutex<HashMap<u64, usize>>>,
}

impl ScrollbackInjector {
    /// Create a capability gate.
    ///
    /// The mux handle and tuning parameters are accepted as the future
    /// restoration-channel boundary but are deliberately not retained while
    /// that channel is unavailable. This avoids extending mux resource
    /// lifetimes for an operation that must fail before touching the mux.
    pub fn new(_wezterm: WeztermHandle, _config: InjectionConfig) -> Self {
        Self {
            suppressed_panes: Arc::new(std::sync::Mutex::new(HashMap::new())),
        }
    }

    /// Get a reference to the suppressed panes set for pattern engine integration.
    pub fn suppressed_panes(&self) -> &Arc<std::sync::Mutex<HashMap<u64, usize>>> {
        &self.suppressed_panes
    }

    /// Inject scrollback content into restored panes.
    ///
    /// `pane_id_map` maps old pane IDs to new (live) pane IDs.
    /// `scrollbacks` maps old pane IDs to their captured scrollback data.
    ///
    /// Unsupported mapped panes and cancellation are returned as typed errors.
    /// [`InjectionReport::skipped`] is reserved for old pane IDs absent from
    /// `pane_id_map`.
    pub async fn inject(
        &self,
        pane_id_map: &HashMap<u64, u64>,
        scrollbacks: &HashMap<u64, ScrollbackData>,
    ) -> crate::Result<InjectionReport> {
        let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
        self.inject_cx(&cx, pane_id_map, scrollbacks).await
    }

    /// Inject under an explicit Cx with the same fail-closed semantics.
    pub async fn inject_cx(
        &self,
        cx: &crate::cx::Cx,
        pane_id_map: &HashMap<u64, u64>,
        scrollbacks: &HashMap<u64, ScrollbackData>,
    ) -> crate::Result<InjectionReport> {
        self.inject_with_cx(cx, pane_id_map, scrollbacks).await
    }

    /// ft-xbnl0.2.3 Cx-first sibling of [`inject`].
    ///
    /// The caller's capability is checked first. An uncancelled request then
    /// receives a structured unsupported-channel error; no pane API is called.
    pub async fn inject_with_cx(
        &self,
        cx: &crate::cx::Cx,
        pane_id_map: &HashMap<u64, u64>,
        scrollbacks: &HashMap<u64, ScrollbackData>,
    ) -> crate::Result<InjectionReport> {
        cx.checkpoint().map_err(|_err| {
            restore_scrollback_cancelled_error("restore_scrollback.inject.pre_flight")
        })?;
        // Mapped data cannot be restored safely today. Detect it before
        // allocating or sorting the potentially huge unmapped-ID report.
        if scrollbacks
            .keys()
            .any(|old_pane_id| pane_id_map.contains_key(old_pane_id))
        {
            return Err(restore_scrollback_unsupported_error());
        }
        let mut skipped = scrollbacks
            .keys()
            .copied()
            .collect::<Vec<_>>();
        skipped.sort_unstable();
        Ok(InjectionReport {
            skipped,
            ..InjectionReport::default()
        })
    }
}

// These parser-boundary helpers are retained only as isolated test vectors for
// a future safe mux output channel. They are deliberately absent from the
// production build so no caller can turn reconstructed content into PTY input.
#[cfg(test)]
fn build_injection_content(lines: &[String]) -> String {
    let mut content = String::with_capacity(lines.iter().map(|line| line.len() + 1).sum());
    content.push_str("\x1b[0m\x1b[H\x1b[2J");
    for (index, line) in lines.iter().enumerate() {
        content.push_str(line);
        if index + 1 < lines.len() {
            content.push('\n');
        }
    }
    content
}

#[cfg(test)]
fn chunk_content(content: &str, chunk_size: usize) -> Vec<String> {
    assert!(chunk_size > 0, "chunk size must be non-zero");
    if content.len() <= chunk_size {
        return vec![content.to_string()];
    }
    let mut chunks = Vec::new();
    let mut start = 0;
    while start < content.len() {
        let target = (start + chunk_size).min(content.len());
        let end = if target < content.len() {
            find_safe_split(content, start, target)
        } else {
            target
        };
        let end = if end == start {
            content[start..]
                .char_indices()
                .nth(1)
                .map_or(content.len(), |(offset, _)| start + offset)
        } else {
            end
        };
        chunks.push(content[start..end].to_string());
        start = end;
    }
    chunks
}

#[cfg(test)]
fn find_safe_split(content: &str, start: usize, target: usize) -> usize {
    let mut pos = target;
    while pos > start && !content.is_char_boundary(pos) {
        pos -= 1;
    }
    let slice = &content[start..pos];
    if let Some(last_esc) = slice.rfind('\x1b') {
        let after_esc = &slice[last_esc..];
        if after_esc.starts_with("\x1b[") && !has_csi_terminator(after_esc) {
            return start + last_esc;
        }
    }
    pos
}

#[cfg(test)]
fn has_csi_terminator(sequence: &str) -> bool {
    sequence
        .bytes()
        .enumerate()
        .any(|(index, byte)| index >= 2 && (0x40..=0x7e).contains(&byte))
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::wezterm::{MockWezterm, WeztermInterface};

    fn run_async_test<F>(future: F)
    where
        F: std::future::Future<Output = ()>,
    {
        use crate::runtime_async::CompatRuntime;
        let runtime = crate::runtime_async::RuntimeBuilder::current_thread()
            .enable_all()
            .build()
            .expect("failed to build restore_scrollback test runtime");
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            runtime.block_on(future);
        }));
        // Absorb TLS destructor panics from asupersync during runtime drop.
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            drop(runtime);
        }));
        // Clear handle from TLS so it doesn't panic during thread exit.
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            crate::runtime_async::clear_runtime_handle();
        }));
        if let Err(payload) = result {
            std::panic::resume_unwind(payload);
        }
    }

    fn make_injector(mock: Arc<MockWezterm>) -> ScrollbackInjector {
        ScrollbackInjector::new(mock, InjectionConfig::default())
    }

    fn mock_scrollback(lines: Vec<&str>) -> ScrollbackData {
        ScrollbackData::from_terminal_lines(lines.into_iter().map(String::from).collect())
    }

    #[test]
    fn restore_scrollback_cancelled_error_uses_structured_runtime_operation() {
        let err = restore_scrollback_cancelled_error(
            "restore_scrollback.test_checkpoint",
            "caller cancelled",
        );

        match err {
            crate::Error::RuntimeOperation { operation, source } => {
                assert_eq!(operation, "restore_scrollback.test_checkpoint");
                assert_eq!(
                    source,
                    RuntimeOperationSource::Cancelled("caller capability stopped".to_string())
                );
            }
            other => panic!("expected structured runtime operation, got {other:?}"),
        }
    }

    // --- ScrollbackData ---

    #[test]
    fn scrollback_data_from_terminal_lines() {
        let data = ScrollbackData::from_terminal_lines(vec!["hello".into(), "world".into()]);
        assert_eq!(data.lines.len(), 2);
        assert_eq!(data.total_bytes(), 10);
    }

    #[test]
    fn scrollback_data_truncate() {
        let mut data =
            ScrollbackData::from_terminal_lines(vec!["a".into(), "b".into(), "c".into(), "d".into()]);
        data.truncate(2);
        assert_eq!(data.lines, vec!["c", "d"]); // Keeps most recent.
        assert_eq!(data.total_bytes(), 2);
    }

    #[test]
    fn scrollback_data_truncate_noop() {
        let mut data = ScrollbackData::from_terminal_lines(vec!["a".into(), "b".into()]);
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
        let set = Arc::new(std::sync::Mutex::new(HashMap::new()));
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

    #[test]
    fn inject_single_pane_fails_closed_without_pty_write() {
        run_async_test(async {
            let mock = Arc::new(MockWezterm::new());
            mock.add_default_pane(10).await;
            let injector = make_injector(mock.clone());

            let mut pane_id_map = HashMap::new();
            pane_id_map.insert(1_u64, 10_u64);

            let mut scrollbacks = HashMap::new();
            scrollbacks.insert(1, mock_scrollback(vec!["line1", "line2", "line3"]));

            injector
                .inject(&pane_id_map, &scrollbacks)
                .await
                .expect_err("mapped replay must report the unsupported safe-output channel");

            let text: String = WeztermInterface::get_text(&*mock, 10, false).await.unwrap();
            assert!(text.is_empty(), "historical output must not reach PTY input");
        });
    }

    #[test]
    fn inject_multiple_panes_all_fail_closed() {
        run_async_test(async {
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

            injector
                .inject(&pane_id_map, &scrollbacks)
                .await
                .expect_err("mapped replay must report the unsupported safe-output channel");
        });
    }

    /// The explicit-Cx API preserves the same typed unsupported-channel error
    /// returned by `inject`.
    #[test]
    fn inject_with_cx_reports_missing_safe_output_channel() {
        run_async_test(async {
            let mock = Arc::new(MockWezterm::new());
            mock.add_default_pane(10).await;
            mock.add_default_pane(11).await;
            let injector = make_injector(mock.clone());

            let mut pane_id_map = HashMap::new();
            pane_id_map.insert(1_u64, 10_u64);
            pane_id_map.insert(2_u64, 11_u64);

            let mut scrollbacks = HashMap::new();
            scrollbacks.insert(1, mock_scrollback(vec!["cx-pane1-output"]));
            scrollbacks.insert(2, mock_scrollback(vec!["cx-pane2-output"]));

            let cx = crate::cx::for_request();
            let error = injector
                .inject_with_cx(&cx, &pane_id_map, &scrollbacks)
                .await
                .expect_err("safe terminal-output channel is unavailable");
            match error {
                crate::Error::RuntimeOperation { operation, source } => {
                    assert_eq!(
                        operation,
                        "restore_scrollback.inject.no_safe_output_channel"
                    );
                    assert!(matches!(source, RuntimeOperationSource::Backend(_)));
                }
                other => panic!("unexpected error: {other:?}"),
            }
        });
    }

    #[test]
    fn inject_skips_unmapped_panes() {
        run_async_test(async {
            let mock = Arc::new(MockWezterm::new());
            let injector = make_injector(mock.clone());

            let pane_id_map = HashMap::new(); // Empty — no mappings.

            let mut scrollbacks = HashMap::new();
            scrollbacks.insert(1, mock_scrollback(vec!["data"]));

            let report = injector.inject(&pane_id_map, &scrollbacks).await.unwrap();

            assert_eq!(report.success_count(), 0);
            assert_eq!(report.skipped.len(), 1);
            assert_eq!(report.skipped[0], 1);
        });
    }

    #[test]
    fn inject_empty_scrollback_still_refuses_unsupported_operation() {
        run_async_test(async {
            let mock = Arc::new(MockWezterm::new());
            mock.add_default_pane(10).await;
            let injector = make_injector(mock.clone());

            let mut pane_id_map = HashMap::new();
            pane_id_map.insert(1_u64, 10_u64);

            let mut scrollbacks = HashMap::new();
            scrollbacks.insert(1, ScrollbackData::from_terminal_lines(vec![]));

            injector
                .inject(&pane_id_map, &scrollbacks)
                .await
                .expect_err("mapped empty replay still requires a safe output channel");
        });
    }

    #[test]
    fn inject_large_scrollback_does_not_allocate_or_write_replay_content() {
        run_async_test(async {
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
            scrollbacks.insert(1, ScrollbackData::from_terminal_lines(lines));

            injector
                .inject(&pane_id_map, &scrollbacks)
                .await
                .expect_err("large mapped replay must fail before allocating replay content");

            let text: String = WeztermInterface::get_text(&*mock, 10, false).await.unwrap();
            assert!(text.is_empty());
        });
    }

    #[test]
    fn inject_no_scrollbacks() {
        run_async_test(async {
            let mock = Arc::new(MockWezterm::new());
            let injector = make_injector(mock.clone());

            let pane_id_map = HashMap::new();
            let scrollbacks = HashMap::new();

            let report = injector.inject(&pane_id_map, &scrollbacks).await.unwrap();

            assert_eq!(report.success_count(), 0);
            assert_eq!(report.failure_count(), 0);
            assert_eq!(report.skipped.len(), 0);
        });
    }

    #[test]
    fn unsupported_injection_does_not_change_suppression_state() {
        run_async_test(async {
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

            injector
                .inject(&pane_id_map, &scrollbacks)
                .await
                .expect_err("mapped replay must fail closed");

            // After injection: suppression cleared.
            assert!(!InjectionGuard::is_suppressed(&suppressed, 10));
        });
    }

    // --- ScrollbackData edge cases ---

    #[test]
    fn scrollback_data_from_empty_segments() {
        let data = ScrollbackData::from_terminal_lines(vec![]);
        assert_eq!(data.lines.len(), 0);
        assert_eq!(data.total_bytes(), 0);
    }

    #[test]
    fn scrollback_data_single_large_segment() {
        let big = "x".repeat(100_000);
        let data = ScrollbackData::from_terminal_lines(vec![big.clone()]);
        assert_eq!(data.lines.len(), 1);
        assert_eq!(data.total_bytes(), 100_000);
    }

    #[test]
    fn scrollback_data_truncate_to_zero() {
        let mut data = ScrollbackData::from_terminal_lines(vec!["a".into(), "b".into()]);
        data.truncate(0);
        assert!(data.lines.is_empty());
        assert_eq!(data.total_bytes(), 0);
    }

    #[test]
    fn scrollback_data_truncate_to_exact_count() {
        let mut data = ScrollbackData::from_terminal_lines(vec!["a".into(), "b".into(), "c".into()]);
        data.truncate(3); // Exactly the count
        assert_eq!(data.lines.len(), 3);
        assert_eq!(data.total_bytes(), 3);
    }

    #[test]
    fn scrollback_data_truncate_to_one_keeps_last() {
        let mut data =
            ScrollbackData::from_terminal_lines(vec!["first".into(), "middle".into(), "last".into()]);
        data.truncate(1);
        assert_eq!(data.lines, vec!["last"]);
        assert_eq!(data.total_bytes(), 4);
    }

    #[test]
    fn scrollback_data_total_bytes_includes_all_segments() {
        let data = ScrollbackData::from_terminal_lines(vec!["abc".into(), "de".into(), "f".into()]);
        assert_eq!(data.total_bytes(), 6); // 3 + 2 + 1
    }

    // --- InjectionGuard edge cases ---

    #[test]
    fn injection_guard_empty_pane_list() {
        let set = Arc::new(std::sync::Mutex::new(HashMap::new()));
        let _guard = InjectionGuard::new(set.clone(), vec![]);
        // No panes suppressed
        assert!(!InjectionGuard::is_suppressed(&set, 1));
        assert!(!InjectionGuard::is_suppressed(&set, 0));
    }

    #[test]
    fn injection_guard_overlapping_guards() {
        let set = Arc::new(std::sync::Mutex::new(HashMap::new()));
        let guard1 = InjectionGuard::new(set.clone(), vec![1, 2]);
        let guard2 = InjectionGuard::new(set.clone(), vec![2, 3]);

        assert!(InjectionGuard::is_suppressed(&set, 1));
        assert!(InjectionGuard::is_suppressed(&set, 2));
        assert!(InjectionGuard::is_suppressed(&set, 3));

        drop(guard1);
        // Reference counts preserve pane 2 while guard2 remains active.
        assert!(!InjectionGuard::is_suppressed(&set, 1));
        assert!(InjectionGuard::is_suppressed(&set, 2));
        assert!(InjectionGuard::is_suppressed(&set, 3));

        drop(guard2);
        assert!(!InjectionGuard::is_suppressed(&set, 2));
        assert!(!InjectionGuard::is_suppressed(&set, 3));
    }

    #[test]
    fn injection_guard_duplicate_pane_ids() {
        let set = Arc::new(std::sync::Mutex::new(HashMap::new()));
        {
            let _guard = InjectionGuard::new(set.clone(), vec![42, 42, 42]);
            assert!(InjectionGuard::is_suppressed(&set, 42));
        }
        // After drop, suppression cleared even with duplicates
        assert!(!InjectionGuard::is_suppressed(&set, 42));
    }

    // --- InjectionReport edge cases ---

    #[test]
    fn injection_report_total_bytes_with_mixed() {
        let mut r = InjectionReport::default();
        r.successes.push(PaneInjectionStats {
            old_pane_id: 1,
            new_pane_id: 10,
            lines_injected: 0,
            bytes_written: 0,
            chunks_sent: 0,
        });
        r.successes.push(PaneInjectionStats {
            old_pane_id: 2,
            new_pane_id: 11,
            lines_injected: 10,
            bytes_written: 500,
            chunks_sent: 1,
        });
        assert_eq!(r.success_count(), 2);
        assert_eq!(r.total_bytes(), 500);
    }

    // --- build_injection_content edge cases ---

    #[test]
    fn build_content_with_empty_string_elements() {
        let content = build_injection_content(&[String::new(), String::new()]);
        assert!(content.starts_with("\x1b[0m\x1b[H\x1b[2J"));
        // Two empty lines with newline between them
        assert!(content.contains("\n"));
    }

    #[test]
    fn build_content_preserves_ansi_in_lines() {
        let content = build_injection_content(&["\x1b[31mred\x1b[0m".into()]);
        assert!(content.contains("\x1b[31mred\x1b[0m"));
    }

    #[test]
    fn build_content_single_empty_line() {
        let content = build_injection_content(&[String::new()]);
        // Just reset prefix + empty string
        assert_eq!(content, "\x1b[0m\x1b[H\x1b[2J");
    }

    // --- chunk_content edge cases ---

    #[test]
    fn chunk_content_chunk_size_one() {
        let content = "abc";
        let chunks = chunk_content(content, 1);
        assert_eq!(chunks.len(), 3);
        let rejoined: String = chunks.concat();
        assert_eq!(rejoined, content);
    }

    #[test]
    fn chunk_content_exact_fit() {
        let content = "hello";
        let chunks = chunk_content(content, 5);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0], "hello");
    }

    #[test]
    fn chunk_content_empty_input() {
        let chunks = chunk_content("", 10);
        assert_eq!(chunks, vec![""]);
    }

    #[test]
    fn chunk_content_multibyte_emoji() {
        // Each emoji is 4 bytes in UTF-8
        let content = "😀😁😂";
        let chunks = chunk_content(content, 5); // Forces split between emojis
        let rejoined: String = chunks.concat();
        assert_eq!(rejoined, content);
        // Ensure no chunk splits mid-character
        for chunk in &chunks {
            assert!(chunk.is_ascii() || chunk.chars().count() > 0);
        }
    }

    #[test]
    fn chunk_content_ansi_not_split_mid_sequence() {
        // 5 bytes of text + ESC [ 3 1 m (5 ANSI bytes) = 10 total
        let content = "hello\x1b[31m";
        let chunks = chunk_content(content, 7); // Would split inside the CSI
        let rejoined: String = chunks.concat();
        assert_eq!(rejoined, content);
    }

    // --- has_csi_terminator edge cases ---

    #[test]
    fn csi_terminator_empty_string() {
        assert!(!has_csi_terminator(""));
    }

    #[test]
    fn csi_terminator_just_esc() {
        assert!(!has_csi_terminator("\x1b"));
    }

    #[test]
    fn csi_terminator_esc_bracket_only() {
        assert!(!has_csi_terminator("\x1b["));
    }

    #[test]
    fn csi_various_terminators() {
        // All valid CSI terminators are 0x40-0x7E
        assert!(has_csi_terminator("\x1b[A")); // Cursor up
        assert!(has_csi_terminator("\x1b[H")); // Cursor home
        assert!(has_csi_terminator("\x1b[J")); // Erase in display
        assert!(has_csi_terminator("\x1b[K")); // Erase in line
        assert!(has_csi_terminator("\x1b[~")); // Tilde (0x7E)
    }

    #[test]
    fn csi_with_many_parameters() {
        // Long CSI with many params: ESC [ 3 8 ; 2 ; 2 5 5 ; 0 ; 0 m
        assert!(has_csi_terminator("\x1b[38;2;255;0;0m"));
    }

    // --- find_safe_split edge cases ---

    #[test]
    fn find_safe_split_at_ansi_boundary() {
        let content = "ab\x1b[31mcd";
        // Target split at position 4 (inside ESC [ sequence)
        let pos = find_safe_split(content, 0, 4);
        // Should split before the ESC
        assert!(pos <= 2 || pos >= 7); // Either before ESC or after sequence
    }

    #[test]
    fn find_safe_split_at_utf8_boundary() {
        let content = "a日b"; // 'a' (1) + '日' (3) + 'b' (1) = 5 bytes
        // Target split at byte 2, which is mid-'日'
        let pos = find_safe_split(content, 0, 2);
        assert!(content.is_char_boundary(pos));
    }

    // --- InjectionConfig serde ---

    #[test]
    fn injection_config_serde_roundtrip() {
        let config = InjectionConfig {
            max_lines: 5000,
            chunk_size: 2048,
            inter_chunk_delay_ms: 5,
            concurrent_injections: 10,
        };
        let json = serde_json::to_string(&config).unwrap();
        let parsed: InjectionConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.max_lines, 5000);
        assert_eq!(parsed.chunk_size, 2048);
        assert_eq!(parsed.inter_chunk_delay_ms, 5);
        assert_eq!(parsed.concurrent_injections, 10);
    }

    #[test]
    fn injection_config_serde_defaults_on_missing() {
        let json = "{}";
        let parsed: InjectionConfig = serde_json::from_str(json).unwrap();
        assert_eq!(parsed.max_lines, 10_000);
        assert_eq!(parsed.chunk_size, 4096);
    }

    // --- Chunked injection ---

    #[test]
    fn chunk_configuration_cannot_reenable_pty_replay() {
        run_async_test(async {
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

            injector
                .inject(&pane_id_map, &scrollbacks)
                .await
                .expect_err("chunk configuration must not re-enable PTY replay");

            let text: String = WeztermInterface::get_text(&*mock, 10, false).await.unwrap();
            assert!(text.is_empty());
        });
    }
}
