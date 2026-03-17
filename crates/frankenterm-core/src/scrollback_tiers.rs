//! Tiered scrollback storage for memory-efficient multi-pane agent swarms (ft-1memj.19).
//!
//! Prevents OOM when running 200+ agent panes with large output volumes by
//! organizing scrollback into three tiers:
//!
//! 1. **Hot** (RAM): Last N lines in a VecDeque. Instant access for rendering.
//! 2. **Warm** (compressed): Older pages zstd-compressed in memory. ~5:1 ratio.
//! 3. **Cold** (evicted): Oldest pages evicted to free memory. Can be re-fetched
//!    from the capture pipeline's SQLite storage.
//!
//! # Memory Budget
//!
//! With default config (1000 hot lines, 50 MB warm cap per pane), 200 panes:
//! - Hot: 200 * 1000 * ~200 bytes = ~40 MB
//! - Warm: 200 * (warm cap / compression ratio) = varies, but bounded
//! - Total: < 1 GB vs ~4+ GB in stock WezTerm
//!
//! # Page Model
//!
//! Lines are grouped into fixed-size pages (default 256 lines). Pages transition
//! through tiers as scrollback grows:
//!
//! ```text
//! New lines → [Hot VecDeque] → overflow → [Warm compressed pages] → cap → [Cold evicted]
//! ```

use serde::{Deserialize, Serialize};
use std::collections::VecDeque;

use crate::byte_compression::{ByteCompressor, CompressionLevel};

// =============================================================================
// Configuration
// =============================================================================

/// Configuration for tiered scrollback storage.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScrollbackConfig {
    /// Maximum lines kept in the hot (RAM) tier per pane. Default: 1000.
    pub hot_lines: usize,
    /// Lines per page when compressing to warm tier. Default: 256.
    pub page_size: usize,
    /// Maximum compressed bytes in the warm tier per pane. Default: 50 MB.
    pub warm_max_bytes: usize,
    /// Compression level for warm tier. Default: Fast.
    pub compression: CompressionLevel,
    /// Whether cold tier eviction is enabled. Default: true.
    pub cold_eviction_enabled: bool,
}

impl Default for ScrollbackConfig {
    fn default() -> Self {
        Self {
            hot_lines: 1000,
            page_size: 256,
            warm_max_bytes: 50 * 1024 * 1024, // 50 MB
            compression: CompressionLevel::Fast,
            cold_eviction_enabled: true,
        }
    }
}

// =============================================================================
// Scrollback Page
// =============================================================================

/// A compressed page of scrollback lines.
#[derive(Debug, Clone)]
struct CompressedPage {
    /// Zero-based page index (0 = oldest page). Used for cold-tier retrieval.
    #[allow(dead_code)]
    page_index: u64,
    /// Number of lines in this page.
    line_count: usize,
    /// Compressed bytes (zstd).
    data: Vec<u8>,
    /// Uncompressed size for memory accounting.
    uncompressed_size: usize,
}

impl CompressedPage {
    /// Compressed size in bytes.
    fn compressed_size(&self) -> usize {
        self.data.len()
    }
}

/// Tier in which a line resides.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScrollbackTier {
    /// In-memory, uncompressed. Instant access.
    Hot,
    /// In-memory, zstd-compressed. Decompress on demand.
    Warm,
    /// Evicted from memory. Must re-fetch from capture pipeline.
    Cold,
}

/// Page-level metadata retained after a page is evicted to the cold tier.
#[derive(Debug, Clone, Copy)]
struct ColdPageMeta {
    page_index: u64,
    line_count: usize,
}

/// Concrete location hint for resolving a scrollback offset.
///
/// This is the bridge between tier selection and future retrieval logic:
/// callers can use the returned page metadata to fetch/decompress the
/// appropriate page without re-scanning the entire tier layout.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "tier", rename_all = "snake_case")]
pub enum ScrollbackLocationHint {
    /// The line is still resident in the hot tier.
    Hot {
        /// Zero-based line index within the hot deque, ordered oldest→newest.
        line_index: usize,
    },
    /// The line resides in a warm compressed page that can be decompressed on demand.
    Warm {
        /// Stable page identifier, ordered from oldest to newest over the lifetime
        /// of the scrollback instance.
        page_index: u64,
        /// Zero-based page offset counting backward from the newest warm page.
        page_offset_from_newest: usize,
        /// Zero-based line index within the page, ordered oldest→newest.
        line_index_in_page: usize,
        /// Total lines in the page.
        page_line_count: usize,
    },
    /// The line has been evicted from memory and must be re-fetched from cold storage.
    Cold {
        /// Stable page identifier, ordered from oldest to newest over the lifetime
        /// of the scrollback instance.
        page_index: u64,
        /// Zero-based page offset counting backward from the newest cold page.
        page_offset_from_newest: usize,
        /// Zero-based line index within the page, ordered oldest→newest.
        line_index_in_page: usize,
        /// Total lines in the page.
        page_line_count: usize,
    },
}

// =============================================================================
// Tiered Scrollback
// =============================================================================

/// Three-tier scrollback storage for a single pane.
///
/// Manages hot (RAM), warm (compressed), and cold (evicted) scrollback pages.
/// Thread-safe access requires external synchronization (the pane already serializes
/// access through its event loop).
#[derive(Debug)]
pub struct TieredScrollback {
    config: ScrollbackConfig,
    compressor: ByteCompressor,
    /// Hot tier: most recent lines, uncompressed for fast rendering.
    hot: VecDeque<String>,
    /// Warm tier: compressed pages, ordered oldest-first.
    warm: VecDeque<CompressedPage>,
    /// Cold tier metadata, ordered oldest-first.
    cold: VecDeque<ColdPageMeta>,
    /// Total compressed bytes in the warm tier.
    warm_bytes: usize,
    /// Total lines ever added (including evicted).
    total_lines_added: u64,
    /// Total lines currently in cold tier (evicted from warm).
    cold_line_count: u64,
    /// Next page index to assign.
    next_page_index: u64,
    /// Total pages evicted to cold tier.
    cold_page_count: u64,
}

/// Snapshot of scrollback tier statistics for telemetry/diagnostics.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ScrollbackTierSnapshot {
    /// Lines in the hot (RAM) tier.
    pub hot_lines: usize,
    /// Number of compressed pages in the warm tier.
    pub warm_pages: usize,
    /// Total compressed bytes in the warm tier.
    pub warm_bytes: usize,
    /// Estimated total lines in the warm tier.
    pub warm_lines: usize,
    /// Lines evicted to cold tier.
    pub cold_lines: u64,
    /// Pages evicted to cold tier.
    pub cold_pages: u64,
    /// Total lines ever added to this scrollback.
    pub total_lines_added: u64,
}

impl TieredScrollback {
    /// Create a new tiered scrollback with default configuration.
    #[must_use]
    pub fn new(config: ScrollbackConfig) -> Self {
        let compressor = ByteCompressor::new(config.compression);
        Self {
            config,
            compressor,
            hot: VecDeque::new(),
            warm: VecDeque::new(),
            cold: VecDeque::new(),
            warm_bytes: 0,
            total_lines_added: 0,
            cold_line_count: 0,
            next_page_index: 0,
            cold_page_count: 0,
        }
    }

    /// Push a single line into the hot tier, triggering overflow as needed.
    pub fn push_line(&mut self, line: String) {
        self.hot.push_back(line);
        self.total_lines_added += 1;

        // Overflow hot → warm when hot exceeds capacity
        if self.hot.len() > self.config.hot_lines + self.config.page_size {
            self.flush_hot_page();
        }
    }

    /// Push multiple lines at once (batch append).
    pub fn push_lines(&mut self, lines: impl IntoIterator<Item = String>) {
        for line in lines {
            self.push_line(line);
        }
    }

    /// Get lines from the hot tier (most recent N lines).
    ///
    /// Returns up to `count` lines, starting from the most recent.
    #[must_use]
    pub fn tail(&self, count: usize) -> Vec<&str> {
        let start = self.hot.len().saturating_sub(count);
        self.hot.range(start..).map(String::as_str).collect()
    }

    /// Get a specific line from the hot tier by offset from the end.
    ///
    /// Offset 0 = most recent line, 1 = second most recent, etc.
    #[must_use]
    pub fn hot_line(&self, offset_from_end: usize) -> Option<&str> {
        if offset_from_end >= self.hot.len() {
            return None;
        }
        let index = self.hot.len() - 1 - offset_from_end;
        self.hot.get(index).map(String::as_str)
    }

    /// Decompress and return lines from a warm tier page.
    ///
    /// `page_offset` is 0-indexed from the newest warm page.
    /// Returns `None` if the page is out of range or decompression fails.
    #[must_use]
    pub fn warm_page_lines(&self, page_offset: usize) -> Option<Vec<String>> {
        let page = self
            .warm
            .get(self.warm.len().checked_sub(1 + page_offset)?)?;
        decompress_page(&self.compressor, page)
    }

    /// Number of lines currently in the hot tier.
    #[must_use]
    pub fn hot_len(&self) -> usize {
        self.hot.len()
    }

    /// Number of compressed pages in the warm tier.
    #[must_use]
    pub fn warm_page_count(&self) -> usize {
        self.warm.len()
    }

    /// Total compressed bytes in the warm tier.
    #[must_use]
    pub fn warm_total_bytes(&self) -> usize {
        self.warm_bytes
    }

    /// Total lines evicted to cold tier.
    #[must_use]
    pub fn cold_line_count(&self) -> u64 {
        self.cold_line_count
    }

    /// Total lines across all tiers (hot + warm + cold).
    #[must_use]
    pub fn total_line_count(&self) -> u64 {
        let warm_lines: u64 = self.warm.iter().map(|p| p.line_count as u64).sum();
        self.hot.len() as u64 + warm_lines + self.cold_line_count
    }

    /// Which tier a given line offset falls into.
    ///
    /// Offset 0 = most recent line.
    #[must_use]
    pub fn tier_for_offset(&self, offset_from_end: usize) -> ScrollbackTier {
        let hot_len = self.hot.len();
        if offset_from_end < hot_len {
            return ScrollbackTier::Hot;
        }

        let warm_lines: usize = self.warm.iter().map(|p| p.line_count).sum();
        if offset_from_end < hot_len + warm_lines {
            return ScrollbackTier::Warm;
        }

        ScrollbackTier::Cold
    }

    /// Resolve an offset-from-end into a concrete tier/page/line location hint.
    ///
    /// Offset 0 = most recent line. Returns `None` if the offset falls beyond
    /// the total retained+evicted line count.
    #[must_use]
    pub fn locate_offset(&self, offset_from_end: usize) -> Option<ScrollbackLocationHint> {
        let hot_len = self.hot.len();
        if offset_from_end < hot_len {
            return Some(ScrollbackLocationHint::Hot {
                line_index: hot_len - 1 - offset_from_end,
            });
        }

        let mut remaining = offset_from_end - hot_len;
        for (page_offset_from_newest, page) in self.warm.iter().rev().enumerate() {
            if remaining < page.line_count {
                return Some(ScrollbackLocationHint::Warm {
                    page_index: page.page_index,
                    page_offset_from_newest,
                    line_index_in_page: page.line_count - 1 - remaining,
                    page_line_count: page.line_count,
                });
            }
            remaining -= page.line_count;
        }

        for (page_offset_from_newest, page) in self.cold.iter().rev().enumerate() {
            if remaining < page.line_count {
                return Some(ScrollbackLocationHint::Cold {
                    page_index: page.page_index,
                    page_offset_from_newest,
                    line_index_in_page: page.line_count - 1 - remaining,
                    page_line_count: page.line_count,
                });
            }
            remaining -= page.line_count;
        }

        None
    }

    /// Take a snapshot of the scrollback tier statistics.
    #[must_use]
    pub fn snapshot(&self) -> ScrollbackTierSnapshot {
        let warm_lines: usize = self.warm.iter().map(|p| p.line_count).sum();
        ScrollbackTierSnapshot {
            hot_lines: self.hot.len(),
            warm_pages: self.warm.len(),
            warm_bytes: self.warm_bytes,
            warm_lines,
            cold_lines: self.cold_line_count,
            cold_pages: self.cold_page_count,
            total_lines_added: self.total_lines_added,
        }
    }

    /// Force eviction of all warm pages to cold tier.
    ///
    /// Used during backpressure Red/Black tier events or when pane is idle.
    pub fn evict_all_warm(&mut self) {
        while let Some(page) = self.warm.pop_front() {
            self.evict_warm_page(page);
        }
    }

    /// Evict warm pages until warm tier is under the byte cap.
    pub fn enforce_warm_cap(&mut self) {
        while self.warm_bytes > self.config.warm_max_bytes {
            if let Some(page) = self.warm.pop_front() {
                self.evict_warm_page(page);
            } else {
                break;
            }
        }
    }

    /// Clear all tiers. Resets the scrollback to empty.
    pub fn clear(&mut self) {
        self.hot.clear();
        self.warm.clear();
        self.cold.clear();
        self.warm_bytes = 0;
        self.cold_line_count = 0;
        self.cold_page_count = 0;
        self.total_lines_added = 0;
        self.next_page_index = 0;
    }

    /// Compression ratio for the warm tier (uncompressed / compressed).
    ///
    /// Returns `None` if the warm tier is empty.
    #[must_use]
    pub fn warm_compression_ratio(&self) -> Option<f64> {
        if self.warm.is_empty() {
            return None;
        }
        let uncompressed: usize = self.warm.iter().map(|p| p.uncompressed_size).sum();
        if self.warm_bytes == 0 {
            return None;
        }
        Some(uncompressed as f64 / self.warm_bytes as f64)
    }

    // ── Internal ──────────────────────────────────────────────────────

    /// Flush the oldest `page_size` lines from hot tier into a compressed warm page.
    fn flush_hot_page(&mut self) {
        let page_size = self.config.page_size;
        if self.hot.len() <= page_size {
            return;
        }

        // Drain oldest page_size lines from hot
        let page_lines: Vec<String> = self.hot.drain(..page_size).collect();
        let line_count = page_lines.len();

        // Serialize lines to bytes for compression
        let raw = lines_to_bytes(&page_lines);
        let uncompressed_size = raw.len();

        // Compress
        let compressed = self.compressor.compress(&raw);
        let compressed_size = compressed.len();

        let page = CompressedPage {
            page_index: self.next_page_index,
            line_count,
            data: compressed,
            uncompressed_size,
        };
        self.next_page_index += 1;
        self.warm_bytes += compressed_size;
        self.warm.push_back(page);

        // Enforce warm cap
        if self.config.cold_eviction_enabled {
            self.enforce_warm_cap();
        }
    }

    fn evict_warm_page(&mut self, page: CompressedPage) {
        self.warm_bytes = self.warm_bytes.saturating_sub(page.compressed_size());
        self.cold_line_count += page.line_count as u64;
        self.cold_page_count += 1;
        self.cold.push_back(ColdPageMeta {
            page_index: page.page_index,
            line_count: page.line_count,
        });
    }
}

impl Default for TieredScrollback {
    fn default() -> Self {
        Self::new(ScrollbackConfig::default())
    }
}

// =============================================================================
// Serialization helpers
// =============================================================================

/// Serialize lines to a byte buffer (newline-separated).
fn lines_to_bytes(lines: &[String]) -> Vec<u8> {
    let total_len: usize = lines.iter().map(|l| l.len() + 1).sum();
    let mut buf = Vec::with_capacity(total_len);
    for line in lines {
        buf.extend_from_slice(line.as_bytes());
        buf.push(b'\n');
    }
    buf
}

/// Deserialize lines from a byte buffer.
fn bytes_to_lines(data: &[u8]) -> Vec<String> {
    if data.is_empty() {
        return Vec::new();
    }
    // Split on newlines, handle trailing newline
    let text = String::from_utf8_lossy(data);
    let mut lines: Vec<String> = text.split('\n').map(String::from).collect();
    // Remove trailing empty string from final newline
    if lines.last().is_some_and(|l| l.is_empty()) {
        lines.pop();
    }
    lines
}

/// Decompress a warm page into lines.
fn decompress_page(compressor: &ByteCompressor, page: &CompressedPage) -> Option<Vec<String>> {
    let raw = compressor.decompress(&page.data).ok()?;
    Some(bytes_to_lines(&raw))
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn small_config() -> ScrollbackConfig {
        ScrollbackConfig {
            hot_lines: 10,
            page_size: 5,
            warm_max_bytes: 10_000,
            compression: CompressionLevel::Fast,
            cold_eviction_enabled: true,
        }
    }

    fn line(n: usize) -> String {
        format!("line-{n:06}")
    }

    // ── Basic push/tail ──────────────────────────────────────────────

    #[test]
    fn empty_scrollback() {
        let sb = TieredScrollback::default();
        assert_eq!(sb.hot_len(), 0);
        assert_eq!(sb.warm_page_count(), 0);
        assert_eq!(sb.cold_line_count(), 0);
        assert_eq!(sb.total_line_count(), 0);
        assert!(sb.tail(10).is_empty());
    }

    #[test]
    fn push_single_line() {
        let mut sb = TieredScrollback::new(small_config());
        sb.push_line("hello".to_string());
        assert_eq!(sb.hot_len(), 1);
        assert_eq!(sb.tail(1), vec!["hello"]);
        assert_eq!(sb.total_line_count(), 1);
    }

    #[test]
    fn push_multiple_lines_stay_in_hot() {
        let mut sb = TieredScrollback::new(small_config());
        for i in 0..10 {
            sb.push_line(line(i));
        }
        assert_eq!(sb.hot_len(), 10);
        assert_eq!(sb.warm_page_count(), 0);
        assert_eq!(
            sb.tail(3),
            vec!["line-000007", "line-000008", "line-000009"]
        );
    }

    #[test]
    fn tail_returns_fewer_than_requested() {
        let mut sb = TieredScrollback::new(small_config());
        sb.push_line("only".to_string());
        assert_eq!(sb.tail(100), vec!["only"]);
    }

    // ── Hot → Warm overflow ──────────────────────────────────────────

    #[test]
    fn overflow_creates_warm_page() {
        let mut sb = TieredScrollback::new(small_config());
        // Push hot_lines + page_size + 1 to trigger overflow
        for i in 0..16 {
            sb.push_line(line(i));
        }
        // Should have flushed one page of 5 lines
        assert!(sb.warm_page_count() >= 1);
        assert!(sb.warm_total_bytes() > 0);
        // Hot should have remaining lines
        assert!(sb.hot_len() <= 11);
    }

    #[test]
    fn warm_page_decompresses_correctly() {
        let mut sb = TieredScrollback::new(small_config());
        // Push enough to create at least one warm page
        for i in 0..20 {
            sb.push_line(line(i));
        }

        assert!(sb.warm_page_count() >= 1);

        // Decompress the newest warm page
        let lines = sb.warm_page_lines(0).expect("decompress should work");
        assert_eq!(lines.len(), 5); // page_size = 5
        // Lines should be from the overflow batch
        for l in &lines {
            assert!(l.starts_with("line-"));
        }
    }

    #[test]
    fn total_line_count_across_tiers() {
        let mut sb = TieredScrollback::new(small_config());
        for i in 0..30 {
            sb.push_line(line(i));
        }
        // All 30 lines should be accounted for across tiers
        assert_eq!(sb.total_line_count(), 30);
        assert_eq!(sb.snapshot().total_lines_added, 30);
    }

    // ── Warm → Cold eviction ──────────────────────────────────────────

    #[test]
    fn warm_cap_triggers_cold_eviction() {
        let config = ScrollbackConfig {
            hot_lines: 10,
            page_size: 5,
            warm_max_bytes: 50, // Very small cap → forces eviction
            compression: CompressionLevel::Fast,
            cold_eviction_enabled: true,
        };
        let mut sb = TieredScrollback::new(config);

        // Push enough to fill warm and trigger eviction
        for i in 0..100 {
            sb.push_line(line(i));
        }

        // Should have evicted some pages to cold
        assert!(sb.cold_line_count() > 0);
        assert!(sb.snapshot().cold_pages > 0);
        // Warm should be under cap
        assert!(sb.warm_total_bytes() <= 50);
    }

    #[test]
    fn evict_all_warm() {
        let mut sb = TieredScrollback::new(small_config());
        for i in 0..30 {
            sb.push_line(line(i));
        }

        let warm_lines_before: usize = sb.warm.iter().map(|p| p.line_count).sum();
        assert!(warm_lines_before > 0);

        sb.evict_all_warm();

        assert_eq!(sb.warm_page_count(), 0);
        assert_eq!(sb.warm_total_bytes(), 0);
        assert!(sb.cold_line_count() > 0);
        // Total should still be the same
        assert_eq!(sb.total_line_count(), 30);
    }

    #[test]
    fn cold_eviction_disabled() {
        let config = ScrollbackConfig {
            hot_lines: 10,
            page_size: 5,
            warm_max_bytes: 50,
            compression: CompressionLevel::Fast,
            cold_eviction_enabled: false,
        };
        let mut sb = TieredScrollback::new(config);

        for i in 0..100 {
            sb.push_line(line(i));
        }

        // No cold eviction → warm grows unbounded
        assert_eq!(sb.cold_line_count(), 0);
        assert!(sb.warm_page_count() > 0);
    }

    // ── Tier classification ──────────────────────────────────────────

    #[test]
    fn tier_for_offset_classification() {
        let mut sb = TieredScrollback::new(small_config());
        for i in 0..30 {
            sb.push_line(line(i));
        }

        // Offset 0 = most recent → hot
        assert_eq!(sb.tier_for_offset(0), ScrollbackTier::Hot);
        // Recent lines → hot
        assert_eq!(sb.tier_for_offset(sb.hot_len() - 1), ScrollbackTier::Hot);
        // Beyond hot → warm
        assert_eq!(sb.tier_for_offset(sb.hot_len()), ScrollbackTier::Warm);
    }

    #[test]
    fn tier_for_offset_cold() {
        let config = ScrollbackConfig {
            hot_lines: 10,
            page_size: 5,
            warm_max_bytes: 50, // Small cap → forces cold eviction
            compression: CompressionLevel::Fast,
            cold_eviction_enabled: true,
        };
        let mut sb = TieredScrollback::new(config);
        for i in 0..200 {
            sb.push_line(line(i));
        }

        // Far back offset should be cold
        assert_eq!(sb.tier_for_offset(199), ScrollbackTier::Cold);
    }

    #[test]
    fn locate_offset_hot_returns_direct_line_index() {
        let mut sb = TieredScrollback::new(small_config());
        sb.push_lines((0..5).map(line));

        assert_eq!(
            sb.locate_offset(0),
            Some(ScrollbackLocationHint::Hot { line_index: 4 })
        );
        assert_eq!(
            sb.locate_offset(4),
            Some(ScrollbackLocationHint::Hot { line_index: 0 })
        );
        assert_eq!(sb.locate_offset(5), None);
    }

    #[test]
    fn locate_offset_warm_returns_page_and_line_hint() {
        let mut sb = TieredScrollback::new(small_config());
        for i in 0..20 {
            sb.push_line(line(i));
        }

        assert_eq!(
            sb.locate_offset(sb.hot_len()),
            ScrollbackLocationHint::Warm {
                page_index: 0,
                page_offset_from_newest: 0,
                line_index_in_page: 4,
                page_line_count: 5,
            }
            .into()
        );

        assert_eq!(
            sb.warm_page_lines(0)
                .as_ref()
                .and_then(|lines| lines.get(4))
                .map(String::as_str),
            Some("line-000004")
        );
    }

    #[test]
    fn locate_offset_cold_returns_stable_page_metadata() {
        let config = ScrollbackConfig {
            hot_lines: 10,
            page_size: 5,
            warm_max_bytes: 50, // Small cap → forces cold eviction
            compression: CompressionLevel::Fast,
            cold_eviction_enabled: true,
        };
        let mut sb = TieredScrollback::new(config);
        for i in 0..200 {
            sb.push_line(line(i));
        }

        assert_eq!(
            sb.locate_offset(199),
            ScrollbackLocationHint::Cold {
                page_index: 0,
                page_offset_from_newest: sb.snapshot().cold_pages as usize - 1,
                line_index_in_page: 0,
                page_line_count: 5,
            }
            .into()
        );

        let newest_cold = sb.locate_offset(sb.hot_len() + sb.snapshot().warm_lines);
        assert!(matches!(
            newest_cold,
            Some(ScrollbackLocationHint::Cold { .. })
        ));
        if let Some(ScrollbackLocationHint::Cold {
            page_index,
            page_offset_from_newest,
            line_index_in_page,
            page_line_count,
        }) = newest_cold
        {
            assert!(page_index > 0);
            assert_eq!(page_offset_from_newest, 0);
            assert_eq!(line_index_in_page + 1, page_line_count);
        }
    }

    // ── Hot line access ──────────────────────────────────────────────

    #[test]
    fn hot_line_access() {
        let mut sb = TieredScrollback::new(small_config());
        sb.push_lines((0..5).map(line));

        assert_eq!(sb.hot_line(0), Some("line-000004")); // Most recent
        assert_eq!(sb.hot_line(4), Some("line-000000")); // Oldest
        assert_eq!(sb.hot_line(5), None); // Out of range
    }

    // ── Snapshot ─────────────────────────────────────────────────────

    #[test]
    fn snapshot_reflects_state() {
        let mut sb = TieredScrollback::new(small_config());
        for i in 0..30 {
            sb.push_line(line(i));
        }

        let snap = sb.snapshot();
        assert!(snap.hot_lines > 0);
        assert!(snap.warm_pages > 0);
        assert!(snap.warm_bytes > 0);
        assert!(snap.warm_lines > 0);
        assert_eq!(snap.total_lines_added, 30);
    }

    #[test]
    fn snapshot_serde_roundtrip() {
        let snap = ScrollbackTierSnapshot {
            hot_lines: 100,
            warm_pages: 5,
            warm_bytes: 2048,
            warm_lines: 1280,
            cold_lines: 500,
            cold_pages: 2,
            total_lines_added: 1880,
        };
        let json = serde_json::to_string(&snap).unwrap();
        let back: ScrollbackTierSnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(snap, back);
    }

    // ── Compression ratio ───────────────────────────────────────────

    #[test]
    fn compression_ratio_warm() {
        let mut sb = TieredScrollback::new(small_config());
        for i in 0..30 {
            sb.push_line(line(i));
        }

        let ratio = sb.warm_compression_ratio();
        assert!(ratio.is_some());
        // Ratio should be >= 1.0 (compressed smaller than uncompressed)
        assert!(ratio.unwrap() >= 1.0);
    }

    #[test]
    fn compression_ratio_empty_warm_is_none() {
        let sb = TieredScrollback::default();
        assert!(sb.warm_compression_ratio().is_none());
    }

    // ── Clear ────────────────────────────────────────────────────────

    #[test]
    fn clear_resets_everything() {
        let mut sb = TieredScrollback::new(small_config());
        for i in 0..50 {
            sb.push_line(line(i));
        }
        assert!(sb.total_line_count() > 0);

        sb.clear();

        assert_eq!(sb.hot_len(), 0);
        assert_eq!(sb.warm_page_count(), 0);
        assert_eq!(sb.warm_total_bytes(), 0);
        assert_eq!(sb.cold_line_count(), 0);
        assert_eq!(sb.total_line_count(), 0);
    }

    // ── Batch push ──────────────────────────────────────────────────

    #[test]
    fn push_lines_batch() {
        let mut sb = TieredScrollback::new(small_config());
        sb.push_lines((0..20).map(line));
        assert_eq!(sb.total_line_count(), 20);
    }

    // ── Config serde ────────────────────────────────────────────────

    #[test]
    fn config_serde_roundtrip() {
        let config = ScrollbackConfig::default();
        let json = serde_json::to_string(&config).unwrap();
        let back: ScrollbackConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(back.hot_lines, config.hot_lines);
        assert_eq!(back.page_size, config.page_size);
        assert_eq!(back.warm_max_bytes, config.warm_max_bytes);
    }

    // ── Default scrollback ──────────────────────────────────────────

    #[test]
    fn default_scrollback_has_expected_config() {
        let sb = TieredScrollback::default();
        assert_eq!(sb.config.hot_lines, 1000);
        assert_eq!(sb.config.page_size, 256);
        assert_eq!(sb.config.warm_max_bytes, 50 * 1024 * 1024);
    }

    // ── ScrollbackTier serde ────────────────────────────────────────

    #[test]
    fn scrollback_tier_serde_roundtrip() {
        for tier in &[
            ScrollbackTier::Hot,
            ScrollbackTier::Warm,
            ScrollbackTier::Cold,
        ] {
            let json = serde_json::to_string(tier).unwrap();
            let back: ScrollbackTier = serde_json::from_str(&json).unwrap();
            assert_eq!(*tier, back);
        }
    }

    #[test]
    fn scrollback_tier_serializes_to_snake_case() {
        assert_eq!(serde_json::to_value(ScrollbackTier::Hot).unwrap(), "hot");
        assert_eq!(serde_json::to_value(ScrollbackTier::Warm).unwrap(), "warm");
        assert_eq!(serde_json::to_value(ScrollbackTier::Cold).unwrap(), "cold");
    }

    #[test]
    fn scrollback_location_hint_serde_roundtrip() {
        let hint = ScrollbackLocationHint::Warm {
            page_index: 7,
            page_offset_from_newest: 1,
            line_index_in_page: 3,
            page_line_count: 5,
        };
        let json = serde_json::to_string(&hint).unwrap();
        let back: ScrollbackLocationHint = serde_json::from_str(&json).unwrap();
        assert_eq!(hint, back);
    }

    // ── Lines <-> bytes roundtrip ───────────────────────────────────

    #[test]
    fn lines_bytes_roundtrip() {
        let original: Vec<String> = (0..10).map(line).collect();
        let bytes = lines_to_bytes(&original);
        let back = bytes_to_lines(&bytes);
        assert_eq!(original, back);
    }

    #[test]
    fn empty_lines_bytes_roundtrip() {
        let empty: Vec<String> = vec![];
        let bytes = lines_to_bytes(&empty);
        let back = bytes_to_lines(&bytes);
        assert!(back.is_empty());
    }

    // ── Large scale ─────────────────────────────────────────────────

    #[test]
    fn scale_200_panes_memory_reasonable() {
        // Simulate 200 panes with 1000 lines each using small config
        let config = ScrollbackConfig {
            hot_lines: 100,
            page_size: 50,
            warm_max_bytes: 5000,
            compression: CompressionLevel::Fast,
            cold_eviction_enabled: true,
        };

        let mut total_warm_bytes = 0usize;
        let mut total_hot_chars = 0usize;

        for _ in 0..200 {
            let mut sb = TieredScrollback::new(config.clone());
            for i in 0..1000 {
                sb.push_line(format!(
                    "pane output line {i}: some typical terminal content here"
                ));
            }
            total_warm_bytes += sb.warm_total_bytes();
            total_hot_chars += sb.hot.iter().map(String::len).sum::<usize>();
        }

        // Memory should be reasonable (< 10 MB for hot + warm across 200 panes)
        let total_bytes = total_warm_bytes + total_hot_chars;
        assert!(
            total_bytes < 10_000_000,
            "200 panes should use < 10 MB, got {} bytes",
            total_bytes
        );
    }
}
