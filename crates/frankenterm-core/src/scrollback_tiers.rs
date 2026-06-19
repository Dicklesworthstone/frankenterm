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
use std::collections::{HashMap, VecDeque};

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

impl ScrollbackConfig {
    /// Clamp out-of-range values to safe equivalents.
    ///
    /// `page_size = 0` would make every hot-tier overflow flush drain zero
    /// lines while still appending an empty warm page: the hot tier grows
    /// without bound and the warm tier fills with empty pages. Clamped to
    /// ≥ 1 at construction so the all-`pub` config struct cannot put the
    /// scrollback into that state.
    #[must_use]
    pub fn sanitized(mut self) -> Self {
        self.page_size = self.page_size.max(1);
        self
    }
}

// =============================================================================
// Scrollback Page
// =============================================================================

/// Backing store for a compressed warm page.
///
/// An instance is uniformly one variant: a `cdc_dedup`-off scrollback only ever
/// builds [`PageData::Plain`] pages, an on scrollback only [`PageData::Cdc`]
/// pages. The default (off) representation is byte-for-byte the legacy
/// standalone-zstd blob, so the legacy path is unchanged.
#[derive(Debug, Clone)]
enum PageData {
    /// Legacy: a standalone zstd blob of the page's raw `lines_to_bytes` buffer.
    Plain(Vec<u8>),
    /// M4: ordered chunk ids (the "recipe") resolved against the shared
    /// content-addressed [`CdcStore`]. The page owns no bytes directly; its
    /// chunks live (deduplicated, zstd-compressed) in the store.
    Cdc(Vec<u64>),
}

/// A compressed page of scrollback lines.
#[derive(Debug, Clone)]
struct CompressedPage {
    /// Zero-based page index (0 = oldest page). Used for cold-tier retrieval.
    #[allow(dead_code)]
    page_index: u64,
    /// Number of lines in this page.
    line_count: usize,
    /// Compressed backing (standalone blob, or CDC recipe into the chunk store).
    data: PageData,
    /// Uncompressed size for memory accounting.
    uncompressed_size: usize,
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
    /// Monotonic counter incremented on every push/access. Used by the fleet
    /// controller to identify idle panes for preferential eviction.
    activity_counter: u64,
    /// Estimated total uncompressed bytes evicted to cold tier (for memory reporting).
    cold_uncompressed_bytes: u64,
    /// Seqlock-style version counter for the warm/cold prefix index. Bumped on
    /// every structural mutation of the warm/cold deques (flush, evict, clear);
    /// republished into [`PrefixIndex::seq`] so a reader can detect a torn /
    /// stale snapshot and fail closed to the linear walk.
    prefix_seq: u64,
    /// Q1 (round-4 gauntlet): incrementally-maintained cumulative line-count
    /// prefix over the warm/cold pages, enabling `O(log pages)` resolution in
    /// [`Self::locate_offset`] / [`Self::tier_for_offset`] via binary search
    /// instead of the `O(pages)` re-sum + linear walk. `None` unless the
    /// `scrollback.prefix_index` gate (env `FT_MOONSHOT_SCROLLBACK_PREFIX_INDEX`,
    /// or [`Self::new_with_prefix_index`]) is enabled. Default **off**.
    prefix: Option<PrefixIndex>,
    /// M4 (round-4 gauntlet): shared content-addressed chunk store for
    /// content-defined-chunking dedup of warm pages before zstd. Identical
    /// chunks across pages (repeated prompts/redraws) are stored once,
    /// refcounted, and freed on eviction. `None` unless the
    /// `scrollback.cdc_dedup` gate (env `FT_MOONSHOT_SCROLLBACK_CDC_DEDUP`, or
    /// [`Self::new_with_options`]) is enabled. Default **off**; the off path is
    /// byte-for-byte the legacy standalone-zstd page.
    cdc: Option<CdcStore>,
}

// =============================================================================
// M4: content-defined chunking (CDC) dedup
// =============================================================================

/// A unique, deduplicated chunk held in the shared [`CdcStore`].
#[derive(Debug)]
struct CdcChunk {
    /// zstd-compressed chunk bytes (the dedup unit, compressed once).
    data: Vec<u8>,
    /// Length of the *raw* (uncompressed) chunk — an integrity guard checked on
    /// reconstruction so a damaged blob can never silently shift page bytes.
    raw_len: usize,
    /// 128-bit content key (kept so the index entry can be removed on free).
    key: u128,
    /// Reference count across the currently-resident warm pages.
    refs: u32,
}

/// Shared content-addressed store backing every [`PageData::Cdc`] page of one
/// scrollback instance.
///
/// Dedup is content-addressed by a 128-bit hash of the raw chunk bytes; the
/// probability of a collision (the only theoretical way reconstruction could
/// diverge) is ~2⁻⁶⁴ even across 10⁹ distinct chunks, far below any real
/// hardware error rate. Reconstruction is otherwise exact: every chunk is
/// stored losslessly (zstd) and `raw_len`-guarded, and a page's `recipe`
/// concatenates its chunks in order to reproduce the original `lines_to_bytes`
/// buffer byte-for-byte.
#[derive(Debug, Default)]
struct CdcStore {
    /// `chunk_id` → stored chunk.
    chunks: HashMap<u64, CdcChunk>,
    /// content key → `chunk_id` (the dedup index).
    index: HashMap<u128, u64>,
    /// Next chunk id to assign (monotonic; ids are never reused).
    next_id: u64,
    /// Total compressed bytes of all live chunks (this instance's
    /// `warm_bytes` contribution when CDC is enabled).
    total_compressed: usize,
}

impl CdcStore {
    /// Chunk `raw` (content-defined boundaries), intern each chunk, and return
    /// `(recipe, newly_added_compressed_bytes)`. Reused chunks add 0 bytes;
    /// only chunks new to the store contribute to `warm_bytes`.
    fn intern_page(&mut self, raw: &[u8], compressor: &ByteCompressor) -> (Vec<u64>, usize) {
        let mut recipe = Vec::new();
        let mut added = 0usize;
        for (start, end) in cdc_chunk_bounds(raw) {
            let chunk = &raw[start..end];
            let key = content_key_128(chunk);
            let id = if let Some(&existing) = self.index.get(&key) {
                if let Some(c) = self.chunks.get_mut(&existing) {
                    c.refs += 1;
                }
                existing
            } else {
                let compressed = compressor.compress(chunk);
                let clen = compressed.len();
                let id = self.next_id;
                self.next_id += 1;
                self.chunks.insert(
                    id,
                    CdcChunk {
                        data: compressed,
                        raw_len: chunk.len(),
                        key,
                        refs: 1,
                    },
                );
                self.index.insert(key, id);
                self.total_compressed += clen;
                added += clen;
                id
            };
            recipe.push(id);
        }
        (recipe, added)
    }

    /// Release every chunk referenced by `recipe`; free chunks whose refcount
    /// reaches zero. Returns the number of compressed bytes freed (subtracted
    /// from `warm_bytes`).
    fn release_recipe(&mut self, recipe: &[u64]) -> usize {
        let mut freed = 0usize;
        for &id in recipe {
            let drop_it = match self.chunks.get_mut(&id) {
                Some(c) => {
                    c.refs = c.refs.saturating_sub(1);
                    c.refs == 0
                }
                None => false,
            };
            if drop_it {
                if let Some(c) = self.chunks.remove(&id) {
                    self.index.remove(&c.key);
                    self.total_compressed = self.total_compressed.saturating_sub(c.data.len());
                    freed += c.data.len();
                }
            }
        }
        freed
    }

    /// Reconstruct a page's raw `lines_to_bytes` buffer from its recipe, exactly.
    /// Returns `None` if a chunk is missing or fails its `raw_len` integrity
    /// guard (fail-closed: the caller then treats the page as undecodable).
    fn reconstruct(&self, recipe: &[u64], compressor: &ByteCompressor) -> Option<Vec<u8>> {
        let mut raw = Vec::new();
        for &id in recipe {
            let chunk = self.chunks.get(&id)?;
            let part = compressor.decompress(&chunk.data).ok()?;
            if part.len() != chunk.raw_len {
                return None;
            }
            raw.extend_from_slice(&part);
        }
        Some(raw)
    }

    /// Clear all chunks (on scrollback `clear`).
    fn clear(&mut self) {
        self.chunks.clear();
        self.index.clear();
        self.next_id = 0;
        self.total_compressed = 0;
    }
}

/// Deterministic 256-entry Gear table for content-defined chunking.
///
/// Built once from a fixed xorshift seed so chunk boundaries — and therefore
/// the dedup outcome — are reproducible across runs and processes.
fn gear_table() -> &'static [u64; 256] {
    static TABLE: std::sync::OnceLock<[u64; 256]> = std::sync::OnceLock::new();
    TABLE.get_or_init(|| {
        let mut t = [0u64; 256];
        let mut s = 0x2545_F491_4F6C_DD1D_u64;
        for slot in t.iter_mut() {
            s ^= s << 13;
            s ^= s >> 7;
            s ^= s << 17;
            *slot = s;
        }
        t
    })
}

/// Minimum chunk size (bytes) — avoids pathological tiny chunks.
const CDC_MIN: usize = 64;
/// Average chunk-size mask: a boundary is allowed when the low `1` bits of the
/// Gear hash are clear, targeting ~512-byte average chunks.
const CDC_MASK: u64 = (1 << 9) - 1;
/// Maximum chunk size (bytes) — forces a boundary so chunks stay bounded.
const CDC_MAX: usize = 4096;

/// Split `raw` into contiguous, non-overlapping content-defined chunks via a
/// Gear rolling hash, returning `(start, end)` byte ranges that exactly tile
/// `raw` in order. Identical byte regions (bounded by the same content) chunk
/// identically regardless of surrounding shifts — the property that lets
/// repeated prompts/redraws across pages deduplicate.
fn cdc_chunk_bounds(raw: &[u8]) -> Vec<(usize, usize)> {
    let table = gear_table();
    let n = raw.len();
    let mut bounds = Vec::new();
    let mut start = 0usize;
    let mut h = 0u64;
    let mut i = 0usize;
    while i < n {
        h = (h << 1).wrapping_add(table[raw[i] as usize]);
        let len = i - start + 1;
        if (len >= CDC_MIN && (h & CDC_MASK) == 0) || len >= CDC_MAX {
            bounds.push((start, i + 1));
            start = i + 1;
            h = 0;
        }
        i += 1;
    }
    if start < n {
        bounds.push((start, n));
    }
    bounds
}

/// 128-bit content key for a chunk: two independently-salted `DefaultHasher`
/// (SipHash) passes concatenated. Deterministic (fixed keys) and effectively
/// collision-free for content addressing.
fn content_key_128(chunk: &[u8]) -> u128 {
    use std::hash::Hasher;
    let mut a = std::collections::hash_map::DefaultHasher::new();
    a.write(chunk);
    let mut b = std::collections::hash_map::DefaultHasher::new();
    b.write_u64(0x9E37_79B9_7F4A_7C15);
    b.write(chunk);
    ((a.finish() as u128) << 64) | b.finish() as u128
}

/// Incrementally-maintained cumulative line-count prefix over the warm and cold
/// page deques, published behind a seqlock-style version counter
/// ([`TieredScrollback::prefix_seq`]).
///
/// Both `warm_cum` and `cold_cum` store **absolute** global end positions — the
/// cumulative line count from the very first line ever flushed out of the hot
/// tier (global line 0) up to and including each page. Because the coordinates
/// are absolute they survive `pop_front` (warm→cold eviction) without any
/// rebase: the surviving entries keep their values and only the front entry is
/// dropped. That is what keeps both push-back (flush) and pop-front (evict)
/// `O(1)` while leaving the arrays monotone and binary-searchable.
///
/// # Seqlock discipline
///
/// `TieredScrollback` is externally serialized (the pane event loop owns it), so
/// this is the single-threaded analog of a seqlock: the owner bumps
/// `prefix_seq` and republishes it into [`Self::seq`] after each structural
/// mutation; a reader that observes `seq != prefix_seq` (or a length mismatch
/// against the live deques) treats the snapshot as torn and falls back to the
/// deterministic linear walk. With correct maintenance the check always passes,
/// so the binary-search path is taken — the guard is fail-closed insurance, not
/// the steady state.
#[derive(Debug, Default)]
struct PrefixIndex {
    /// Absolute global end position of each warm page, oldest→newest. Parallel
    /// to [`TieredScrollback::warm`]; strictly increasing (pages are non-empty).
    warm_cum: VecDeque<u64>,
    /// Absolute global end position of each cold page, oldest→newest. Parallel
    /// to [`TieredScrollback::cold`]. Cold only ever grows, so `cold_cum.back()`
    /// is also the running total of lines evicted to cold.
    cold_cum: VecDeque<u64>,
    /// Seqlock version this snapshot was last published at.
    seq: u64,
}

impl PrefixIndex {
    /// Absolute global coordinate of the warm/cold boundary (== total cold lines).
    fn cold_total(&self) -> u64 {
        self.cold_cum.back().copied().unwrap_or(0)
    }

    /// Total lines ever flushed out of hot (== global coordinate ceiling).
    fn flushed_total(&self) -> u64 {
        self.warm_cum
            .back()
            .copied()
            .unwrap_or_else(|| self.cold_total())
    }

    /// Lines currently resident in the warm tier.
    fn warm_lines(&self) -> u64 {
        self.flushed_total() - self.cold_total()
    }

    /// Seqlock + structural consistency check against the live deques.
    fn is_consistent(&self, warm_len: usize, cold_len: usize, expected_seq: u64) -> bool {
        self.seq == expected_seq
            && self.warm_cum.len() == warm_len
            && self.cold_cum.len() == cold_len
    }
}

/// Outcome of the gated indexed resolution path.
enum IndexedLocate {
    /// The prefix index was active and consistent; this is the authoritative
    /// answer (`None` = offset beyond all flushed lines).
    Resolved(Option<ScrollbackLocationHint>),
    /// The index is disabled or failed its seqlock check — use the linear walk.
    Fallback,
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
    /// Activity counter (monotonic, for idle detection).
    pub activity_counter: u64,
    /// Estimated uncompressed bytes evicted to cold tier.
    pub cold_uncompressed_bytes: u64,
}

impl TieredScrollback {
    /// Create a new tiered scrollback with the given configuration.
    ///
    /// The configuration is passed through [`ScrollbackConfig::sanitized`]
    /// first, so a zero `page_size` is clamped instead of degrading the
    /// tier-migration loop.
    ///
    /// The Q1 prefix-index and M4 CDC-dedup fast paths default **off**; each is
    /// enabled only when its `FT_MOONSHOT_SCROLLBACK_*` env gate is set (see
    /// [`Self::new_with_options`] for a deterministic, env-free opt-in).
    #[must_use]
    pub fn new(config: ScrollbackConfig) -> Self {
        Self::new_with_options(
            config,
            prefix_index_enabled_from_env(),
            cdc_dedup_enabled_from_env(),
        )
    }

    /// Create a tiered scrollback, explicitly choosing the Q1 prefix-index gate.
    ///
    /// `prefix_index = false` is the default behavior (legacy linear walk).
    /// `true` enables the incrementally-maintained cumulative line-count prefix
    /// + binary-search resolution in [`Self::locate_offset`] /
    /// [`Self::tier_for_offset`]. Observable behavior is identical either way
    /// (proven byte-equivalent); the flag only changes the resolution cost.
    ///
    /// CDC dedup is taken from the `FT_MOONSHOT_SCROLLBACK_CDC_DEDUP` env gate;
    /// use [`Self::new_with_options`] to choose both gates explicitly.
    #[must_use]
    pub fn new_with_prefix_index(config: ScrollbackConfig, prefix_index: bool) -> Self {
        Self::new_with_options(config, prefix_index, cdc_dedup_enabled_from_env())
    }

    /// Create a tiered scrollback, explicitly choosing both round-4 gates.
    ///
    /// `cdc_dedup = false` is the default behavior — warm pages are standalone
    /// zstd blobs, byte-for-byte the legacy representation. `true` enables M4
    /// content-defined-chunking dedup: warm pages are split into content-defined
    /// chunks, identical chunks are stored once (content-addressed, refcounted)
    /// in a shared store, and reconstruction is byte-identical. The flag only
    /// changes the storage representation, never the decoded content.
    ///
    /// This is the env-race-free entry point used by property proofs and the A/B
    /// bench harness; production toggles the same gates via the
    /// `FT_MOONSHOT_SCROLLBACK_PREFIX_INDEX` / `FT_MOONSHOT_SCROLLBACK_CDC_DEDUP`
    /// env vars through [`Self::new`].
    #[must_use]
    pub fn new_with_options(
        config: ScrollbackConfig,
        prefix_index: bool,
        cdc_dedup: bool,
    ) -> Self {
        let config = config.sanitized();
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
            activity_counter: 0,
            cold_uncompressed_bytes: 0,
            prefix_seq: 0,
            prefix: if prefix_index {
                Some(PrefixIndex::default())
            } else {
                None
            },
            cdc: if cdc_dedup {
                Some(CdcStore::default())
            } else {
                None
            },
        }
    }

    /// Push a single line into the hot tier, triggering overflow as needed.
    pub fn push_line(&mut self, line: String) {
        self.hot.push_back(line);
        self.total_lines_added += 1;
        self.activity_counter += 1;

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
        self.decode_page(page)
    }

    /// Decode a warm page back into its lines, reconstructing the raw
    /// `lines_to_bytes` buffer from either the standalone zstd blob (legacy) or
    /// the CDC recipe against the shared chunk store (M4). The decoded bytes are
    /// byte-identical to what was flushed. Returns `None` on decompression
    /// failure, a missing/short chunk, or a decoded line-count mismatch (the
    /// same corruption guard the legacy path used).
    fn decode_page(&self, page: &CompressedPage) -> Option<Vec<String>> {
        let raw = match &page.data {
            PageData::Plain(data) => self.compressor.decompress(data).ok()?,
            PageData::Cdc(recipe) => self.cdc.as_ref()?.reconstruct(recipe, &self.compressor)?,
        };
        let lines = bytes_to_lines(&raw);
        if lines.len() != page.line_count {
            return None;
        }
        Some(lines)
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

        // Q1 indexed fast path (gated; fail-closed to the linear sum below).
        if let Some(tier) = self.tier_for_offset_indexed(offset_from_end, hot_len) {
            return tier;
        }

        let warm_lines: usize = self.warm.iter().map(|p| p.line_count).sum();
        if offset_from_end < hot_len + warm_lines {
            return ScrollbackTier::Warm;
        }

        ScrollbackTier::Cold
    }

    /// Indexed (`O(1)`) tier classification, or `None` to fall back to the
    /// linear sum. Mirrors [`Self::tier_for_offset`] exactly, including its
    /// "everything beyond warm is Cold" convention (no upper bound check).
    ///
    /// Caller guarantees `offset_from_end >= hot_len`.
    fn tier_for_offset_indexed(
        &self,
        offset_from_end: usize,
        hot_len: usize,
    ) -> Option<ScrollbackTier> {
        let idx = self.prefix.as_ref()?;
        if !idx.is_consistent(self.warm.len(), self.cold.len(), self.prefix_seq) {
            return None;
        }
        let remaining = (offset_from_end - hot_len) as u64;
        if remaining < idx.warm_lines() {
            Some(ScrollbackTier::Warm)
        } else {
            Some(ScrollbackTier::Cold)
        }
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

        // Q1 indexed fast path (gated; fail-closed to the linear walk below).
        if let IndexedLocate::Resolved(result) =
            self.locate_warm_cold_indexed(offset_from_end, hot_len)
        {
            return result;
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

    /// Indexed resolution of a warm/cold offset via binary search over the
    /// absolute cumulative prefix, or [`IndexedLocate::Fallback`] when the gate
    /// is off or the seqlock check fails.
    ///
    /// Produces hints byte-identical to the linear walk in
    /// [`Self::locate_offset`]. Caller guarantees `offset_from_end >= hot_len`.
    ///
    /// The warm/cold pages occupy contiguous, *stable* ranges in a global line
    /// numbering (line 0 = first line ever flushed): cold owns
    /// `[0, cold_total)` and warm owns `[cold_total, flushed_total)`. An offset
    /// counted backward from the newest flushed line maps to the global
    /// coordinate `target = flushed_total - 1 - remaining`, and the containing
    /// page is found by a single `partition_point` over the monotone cumulative
    /// ends.
    fn locate_warm_cold_indexed(
        &self,
        offset_from_end: usize,
        hot_len: usize,
    ) -> IndexedLocate {
        let Some(idx) = self.prefix.as_ref() else {
            return IndexedLocate::Fallback;
        };
        if !idx.is_consistent(self.warm.len(), self.cold.len(), self.prefix_seq) {
            return IndexedLocate::Fallback;
        }

        let remaining = (offset_from_end - hot_len) as u64;
        let flushed_total = idx.flushed_total();
        if remaining >= flushed_total {
            // Beyond every flushed line — exactly the linear walk's `None`.
            return IndexedLocate::Resolved(None);
        }
        let target = flushed_total - 1 - remaining;

        let hint = if target >= idx.cold_total() {
            // Warm tier: first page whose absolute end exceeds `target`.
            let i = idx.warm_cum.partition_point(|&end| end <= target);
            let page = &self.warm[i];
            let start = idx.warm_cum[i] - page.line_count as u64;
            ScrollbackLocationHint::Warm {
                page_index: page.page_index,
                page_offset_from_newest: self.warm.len() - 1 - i,
                line_index_in_page: (target - start) as usize,
                page_line_count: page.line_count,
            }
        } else {
            // Cold tier.
            let j = idx.cold_cum.partition_point(|&end| end <= target);
            let page = &self.cold[j];
            let start = idx.cold_cum[j] - page.line_count as u64;
            ScrollbackLocationHint::Cold {
                page_index: page.page_index,
                page_offset_from_newest: self.cold.len() - 1 - j,
                line_index_in_page: (target - start) as usize,
                page_line_count: page.line_count,
            }
        };
        IndexedLocate::Resolved(Some(hint))
    }

    /// Bump the seqlock version and republish it into the prefix index. Call
    /// after every structural mutation of the warm/cold deques so the index's
    /// `seq` tracks the live `prefix_seq`.
    fn republish_prefix_seq(&mut self) {
        self.prefix_seq = self.prefix_seq.wrapping_add(1);
        if let Some(idx) = self.prefix.as_mut() {
            idx.seq = self.prefix_seq;
        }
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
            activity_counter: self.activity_counter,
            cold_uncompressed_bytes: self.cold_uncompressed_bytes,
        }
    }

    /// Current activity counter value (monotonic, for idle detection).
    #[must_use]
    pub fn activity_counter(&self) -> u64 {
        self.activity_counter
    }

    /// Diagnostic: is the Q1 prefix index enabled **and** currently passing its
    /// seqlock + structural consistency check — i.e., is offset resolution
    /// taking the binary-search fast path? `false` when the gate is off or the
    /// index has fallen back to the linear walk. Hidden from the public docs;
    /// exposed so the byte-equivalence proof / A/B bench can assert the fast
    /// path is genuinely exercised rather than silently falling back.
    #[doc(hidden)]
    #[must_use]
    pub fn prefix_index_active(&self) -> bool {
        self.prefix
            .as_ref()
            .map(|idx| idx.is_consistent(self.warm.len(), self.cold.len(), self.prefix_seq))
            .unwrap_or(false)
    }

    /// Diagnostic: M4 CDC dedup store stats as `(unique_chunks,
    /// total_compressed_bytes)`, or `None` when the `cdc_dedup` gate is off.
    /// Hidden from the public docs; exposed so the round-trip proof / A/B bench
    /// can assert dedup is engaged and accounting tracks the live chunk set.
    #[doc(hidden)]
    #[must_use]
    pub fn cdc_stats(&self) -> Option<(usize, usize)> {
        self.cdc
            .as_ref()
            .map(|cdc| (cdc.chunks.len(), cdc.total_compressed))
    }

    /// Evict up to `count` warm pages to cold tier (proportional eviction).
    ///
    /// Evicts oldest pages first. Returns the number of pages actually evicted.
    pub fn evict_warm_pages(&mut self, count: usize) -> usize {
        let mut evicted = 0;
        for _ in 0..count {
            if let Some(page) = self.warm.pop_front() {
                self.evict_warm_page(page);
                evicted += 1;
            } else {
                break;
            }
        }
        evicted
    }

    /// Evict warm pages until warm tier uses at most `target_bytes` compressed.
    ///
    /// Returns the number of pages evicted.
    pub fn evict_warm_to_target(&mut self, target_bytes: usize) -> usize {
        let mut evicted = 0;
        while self.warm_bytes > target_bytes {
            if let Some(page) = self.warm.pop_front() {
                self.evict_warm_page(page);
                evicted += 1;
            } else {
                break;
            }
        }
        evicted
    }

    /// Estimated total memory footprint (hot + warm) in bytes.
    ///
    /// Does not include cold tier (evicted from memory).
    #[must_use]
    pub fn estimated_memory_bytes(&self) -> usize {
        let hot_bytes: usize = self
            .hot
            .iter()
            .map(|s| s.len() + std::mem::size_of::<String>())
            .sum();
        hot_bytes + self.warm_bytes
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
        self.activity_counter = 0;
        self.cold_uncompressed_bytes = 0;
        if let Some(idx) = self.prefix.as_mut() {
            idx.warm_cum.clear();
            idx.cold_cum.clear();
        }
        if let Some(cdc) = self.cdc.as_mut() {
            cdc.clear();
        }
        self.republish_prefix_seq();
    }

    /// Retrieve a line from the cold tier via a [`ColdTierRetriever`].
    ///
    /// `hint` must be a `ScrollbackLocationHint::Cold` obtained from
    /// [`locate_offset`](Self::locate_offset).
    ///
    /// Returns the specific line from the cold page, or an error if the
    /// page cannot be retrieved.
    pub fn cold_line(
        &self,
        hint: &ScrollbackLocationHint,
        retriever: &dyn ColdTierRetriever,
    ) -> Result<String, ColdRetrievalError> {
        match hint {
            ScrollbackLocationHint::Cold {
                page_index,
                line_index_in_page,
                ..
            } => {
                let data = retriever.retrieve_page(*page_index)?;
                data.lines.get(*line_index_in_page).cloned().ok_or(
                    ColdRetrievalError::PageNotFound {
                        page_index: *page_index,
                    },
                )
            }
            _ => Err(ColdRetrievalError::PageNotFound { page_index: 0 }),
        }
    }

    /// Retrieve a full cold page via a [`ColdTierRetriever`].
    ///
    /// `page_index` is the stable page identifier from eviction time.
    pub fn cold_page(
        &self,
        page_index: u64,
        retriever: &dyn ColdTierRetriever,
    ) -> Result<ColdPageData, ColdRetrievalError> {
        retriever.retrieve_page(page_index)
    }

    /// Number of cold pages that would need to be fetched to resolve all
    /// cold-tier lines.
    #[must_use]
    pub fn cold_page_count(&self) -> u64 {
        self.cold_page_count
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

        // Serialize lines to bytes (identical raw buffer in both modes).
        let raw = lines_to_bytes(&page_lines);
        let uncompressed_size = raw.len();

        // Build the page backing: legacy standalone zstd blob, or M4 CDC dedup
        // into the shared content-addressed store. Both add only the bytes new
        // to this instance to `warm_bytes`.
        let compressor = &self.compressor;
        let (data, added_compressed) = if let Some(cdc) = self.cdc.as_mut() {
            let (recipe, added) = cdc.intern_page(&raw, compressor);
            (PageData::Cdc(recipe), added)
        } else {
            let compressed = compressor.compress(&raw);
            let len = compressed.len();
            (PageData::Plain(compressed), len)
        };

        let page = CompressedPage {
            page_index: self.next_page_index,
            line_count,
            data,
            uncompressed_size,
        };
        self.next_page_index += 1;
        self.warm_bytes += added_compressed;
        self.warm.push_back(page);

        // Prefix index: append the new warm page's absolute global end
        // (previous flushed_total + this page's lines). Done before the cap
        // enforcement below so the per-page eviction sees a consistent index.
        let cold_total = self.cold_line_count;
        if let Some(idx) = self.prefix.as_mut() {
            let new_end = idx.warm_cum.back().copied().unwrap_or(cold_total) + line_count as u64;
            idx.warm_cum.push_back(new_end);
        }
        self.republish_prefix_seq();

        // Enforce warm cap
        if self.config.cold_eviction_enabled {
            self.enforce_warm_cap();
        }
    }

    fn evict_warm_page(&mut self, page: CompressedPage) {
        // Release this page's storage. A Plain page owns its blob; a CDC page
        // drops chunk references and frees only chunks that reach zero refs
        // (chunks still shared by resident warm pages stay).
        let freed = match &page.data {
            PageData::Plain(data) => data.len(),
            PageData::Cdc(recipe) => self
                .cdc
                .as_mut()
                .map(|cdc| cdc.release_recipe(recipe))
                .unwrap_or(0),
        };
        self.warm_bytes = self.warm_bytes.saturating_sub(freed);
        self.cold_line_count += page.line_count as u64;
        self.cold_uncompressed_bytes += page.uncompressed_size as u64;
        self.cold_page_count += 1;
        self.cold.push_back(ColdPageMeta {
            page_index: page.page_index,
            line_count: page.line_count,
        });

        // Prefix index: the caller already popped the warm front; mirror it and
        // append the cold page's absolute end (== running cold line total).
        // Absolute coordinates make this rebase-free for the surviving entries.
        let cold_total = self.cold_line_count;
        if let Some(idx) = self.prefix.as_mut() {
            idx.warm_cum.pop_front();
            idx.cold_cum.push_back(cold_total);
        }
        self.republish_prefix_seq();
    }
}

impl Default for TieredScrollback {
    fn default() -> Self {
        Self::new(ScrollbackConfig::default())
    }
}

/// Whether the Q1 `scrollback.prefix_index` gate is enabled via the environment.
///
/// Default **off**: only `1`/`true`/`yes`/`on` (case-insensitive) on
/// `FT_MOONSHOT_SCROLLBACK_PREFIX_INDEX` enable the indexed resolution path,
/// mirroring the existing `FT_MOONSHOT_*` gating convention. Anything else
/// (unset, empty, `0`, `false`, …) leaves the deterministic linear walk active.
fn prefix_index_enabled_from_env() -> bool {
    env_flag_enabled("FT_MOONSHOT_SCROLLBACK_PREFIX_INDEX")
}

/// Whether the M4 `scrollback.cdc_dedup` gate is enabled via the environment.
///
/// Default **off**: only `1`/`true`/`yes`/`on` (case-insensitive) on
/// `FT_MOONSHOT_SCROLLBACK_CDC_DEDUP` enable CDC dedup of warm pages, mirroring
/// the existing `FT_MOONSHOT_*` gating convention.
fn cdc_dedup_enabled_from_env() -> bool {
    env_flag_enabled("FT_MOONSHOT_SCROLLBACK_CDC_DEDUP")
}

/// Shared truthiness parse for the `FT_MOONSHOT_SCROLLBACK_*` gates.
fn env_flag_enabled(var: &str) -> bool {
    std::env::var(var)
        .ok()
        .map(|v| {
            let v = v.trim();
            v == "1"
                || v.eq_ignore_ascii_case("true")
                || v.eq_ignore_ascii_case("yes")
                || v.eq_ignore_ascii_case("on")
        })
        .unwrap_or(false)
}

// =============================================================================
// Cold tier retrieval
// =============================================================================

/// Error type for cold tier page retrieval.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ColdRetrievalError {
    /// The requested page index was not found in cold storage.
    PageNotFound { page_index: u64 },
    /// Cold storage is temporarily unavailable (e.g., database locked).
    StorageUnavailable { reason: String },
    /// Decompression of the cold page data failed.
    DecompressionFailed { page_index: u64, reason: String },
}

impl std::fmt::Display for ColdRetrievalError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::PageNotFound { page_index } => {
                write!(f, "cold page {page_index} not found")
            }
            Self::StorageUnavailable { reason } => {
                write!(f, "cold storage unavailable: {reason}")
            }
            Self::DecompressionFailed { page_index, reason } => {
                write!(f, "cold page {page_index} decompression failed: {reason}")
            }
        }
    }
}

impl std::error::Error for ColdRetrievalError {}

/// Result of retrieving a cold tier page.
#[derive(Debug, Clone)]
pub struct ColdPageData {
    /// The stable page index that was requested.
    pub page_index: u64,
    /// Lines from the page, ordered oldest to newest.
    pub lines: Vec<String>,
}

/// Trait for backends that can serve cold-tier scrollback page reads.
///
/// Implementations fetch page data from the capture pipeline's storage
/// (typically SQLite via the `storage` module). The trait is object-safe
/// and designed for synchronous use — callers that need async should wrap
/// in `spawn_blocking` or equivalent.
///
/// # Contract
///
/// - `retrieve_page` must return the *exact* lines that were in the page
///   when it was evicted, in the same order.
/// - If the page was never written to cold storage (e.g., cold eviction
///   is disabled or the storage pipeline hasn't flushed), return
///   `Err(ColdRetrievalError::PageNotFound)`.
/// - Implementations must be safe to call concurrently from multiple
///   panes' access paths.
pub trait ColdTierRetriever: Send + Sync {
    /// Retrieve a single cold page by its stable page index.
    fn retrieve_page(&self, page_index: u64) -> Result<ColdPageData, ColdRetrievalError>;
}

/// A no-op cold tier retriever that always returns `PageNotFound`.
///
/// Used as a default when cold storage is not configured or available.
#[derive(Debug, Clone, Copy, Default)]
pub struct NullColdRetriever;

impl ColdTierRetriever for NullColdRetriever {
    fn retrieve_page(&self, page_index: u64) -> Result<ColdPageData, ColdRetrievalError> {
        Err(ColdRetrievalError::PageNotFound { page_index })
    }
}

// =============================================================================
// Serialization helpers
// =============================================================================

/// Serialize lines to a byte buffer (u64-LE length-prefixed records).
///
/// Each line is encoded as `<u64 LE byte length><utf-8 bytes>`. An earlier
/// newline-delimited encoding could not round-trip lines that themselves
/// contained `\n`: `bytes_to_lines` split such a line into several on decode,
/// inflating the decoded count past the page's recorded
/// `CompressedPage::line_count` and corrupting every warm/cold offset computed
/// by `locate_offset` / `tier_for_offset`. The length-prefixed form is the
/// exact inverse of [`bytes_to_lines`] for all `String` contents.
fn lines_to_bytes(lines: &[String]) -> Vec<u8> {
    const PREFIX: usize = std::mem::size_of::<u64>();
    let total_len: usize = lines.iter().map(|l| l.len() + PREFIX).sum();
    let mut buf = Vec::with_capacity(total_len);
    for line in lines {
        buf.extend_from_slice(&(line.len() as u64).to_le_bytes());
        buf.extend_from_slice(line.as_bytes());
    }
    buf
}

/// Deserialize lines from a length-prefixed byte buffer.
///
/// Exact inverse of [`lines_to_bytes`]. Decoding is allocation-bounded by the
/// input: a record's length prefix is only honoured when that many bytes are
/// actually present in the buffer, so a truncated or corrupt buffer can never
/// trigger an oversized allocation (the unbounded-length-prefix DoS class).
/// Decoding stops at the first malformed record; `decode_page`'s
/// line-count guard then rejects the page as corrupt.
fn bytes_to_lines(data: &[u8]) -> Vec<String> {
    const PREFIX: usize = std::mem::size_of::<u64>();
    let mut lines = Vec::new();
    let mut pos = 0usize;
    while data.len() - pos >= PREFIX {
        let mut prefix = [0u8; PREFIX];
        prefix.copy_from_slice(&data[pos..pos + PREFIX]);
        let Ok(len) = usize::try_from(u64::from_le_bytes(prefix)) else {
            break;
        };
        let start = pos + PREFIX;
        let Some(end) = start.checked_add(len) else {
            break;
        };
        if end > data.len() {
            break;
        }
        lines.push(String::from_utf8_lossy(&data[start..end]).into_owned());
        pos = end;
    }
    lines
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
            activity_counter: 42,
            cold_uncompressed_bytes: 10240,
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

    #[test]
    fn zero_page_size_is_clamped_and_hot_tier_stays_bounded() {
        let config = ScrollbackConfig {
            hot_lines: 4,
            page_size: 0,
            ..ScrollbackConfig::default()
        };
        let mut sb = TieredScrollback::new(config);
        assert_eq!(sb.config.page_size, 1, "sanitized() must clamp page_size");

        sb.push_lines((0..100).map(|i| format!("line {i}")));
        assert!(
            sb.hot.len() <= 4 + sb.config.page_size,
            "hot tier must stay bounded ({} lines retained)",
            sb.hot.len()
        );
        assert!(
            sb.warm.iter().all(|page| page.line_count > 0),
            "no empty warm pages may be created"
        );
        assert_eq!(sb.total_line_count(), 100);
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

    #[test]
    fn lines_bytes_roundtrip_embedded_newlines() {
        // Regression: the old newline-delimited encoding split a line that
        // itself contained '\n' into multiple lines on decode, so the decoded
        // count diverged from CompressedPage::line_count.
        let original: Vec<String> = vec![
            "plain".to_string(),
            "embedded\nnewline".to_string(),
            String::new(),
            "trailing-newline\n".to_string(),
            "crlf\r\nmiddle".to_string(),
            "\n".to_string(),
        ];
        let bytes = lines_to_bytes(&original);
        let back = bytes_to_lines(&bytes);
        assert_eq!(original, back);
    }

    #[test]
    fn bytes_to_lines_truncated_buffer_stops_cleanly() {
        // A truncated record (length prefix promises more bytes than remain)
        // must terminate decoding without over-allocating or panicking.
        let original: Vec<String> = vec!["complete".to_string(), "cut-off".to_string()];
        let mut bytes = lines_to_bytes(&original);
        bytes.truncate(bytes.len() - 3);
        let back = bytes_to_lines(&bytes);
        assert_eq!(back, vec!["complete".to_string()]);
    }

    #[test]
    fn warm_page_roundtrip_preserves_embedded_newlines() {
        // End-to-end regression for the hot→warm→decode path: a flushed page
        // whose lines contain embedded newlines must decode to exactly the
        // lines that were drained from the hot tier, with the decoded length
        // matching the page's recorded line_count.
        let mut sb = TieredScrollback::new(small_config()); // hot 10, page 5
        let lines: Vec<String> = (0..16)
            .map(|i| format!("line-{i}\nwith-embedded-newline-{i}"))
            .collect();
        sb.push_lines(lines.clone());
        assert_eq!(sb.warm_page_count(), 1);
        let page = sb.warm_page_lines(0).expect("warm page should decode");
        assert_eq!(page.len(), 5);
        assert_eq!(page, lines[..5].to_vec());
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

    // ── Proportional eviction ──────────────────────────────────────

    #[test]
    fn evict_warm_pages_partial() {
        let mut sb = TieredScrollback::new(small_config());
        for i in 0..50 {
            sb.push_line(line(i));
        }
        let initial_warm = sb.warm_page_count();
        assert!(initial_warm >= 2, "need at least 2 warm pages");

        let evicted = sb.evict_warm_pages(1);
        assert_eq!(evicted, 1);
        assert_eq!(sb.warm_page_count(), initial_warm - 1);
        assert!(sb.cold_line_count() > 0);
    }

    #[test]
    fn evict_warm_pages_all() {
        let mut sb = TieredScrollback::new(small_config());
        for i in 0..50 {
            sb.push_line(line(i));
        }
        let initial_warm = sb.warm_page_count();
        let evicted = sb.evict_warm_pages(initial_warm + 10);
        assert_eq!(evicted, initial_warm);
        assert_eq!(sb.warm_page_count(), 0);
    }

    #[test]
    fn evict_warm_pages_zero() {
        let mut sb = TieredScrollback::new(small_config());
        for i in 0..50 {
            sb.push_line(line(i));
        }
        let before = sb.warm_page_count();
        let evicted = sb.evict_warm_pages(0);
        assert_eq!(evicted, 0);
        assert_eq!(sb.warm_page_count(), before);
    }

    #[test]
    fn evict_warm_to_target_partial() {
        let mut sb = TieredScrollback::new(small_config());
        for i in 0..50 {
            sb.push_line(line(i));
        }
        let initial_bytes = sb.warm_total_bytes();
        assert!(initial_bytes > 0);

        let target = initial_bytes / 2;
        let evicted = sb.evict_warm_to_target(target);
        assert!(evicted > 0);
        assert!(sb.warm_total_bytes() <= target);
    }

    #[test]
    fn evict_warm_to_target_zero_evicts_all() {
        let mut sb = TieredScrollback::new(small_config());
        for i in 0..50 {
            sb.push_line(line(i));
        }
        let evicted = sb.evict_warm_to_target(0);
        assert!(evicted > 0);
        assert_eq!(sb.warm_total_bytes(), 0);
        assert_eq!(sb.warm_page_count(), 0);
    }

    // ── Activity counter ───────────────────────────────────────────

    #[test]
    fn activity_counter_increments_on_push() {
        let mut sb = TieredScrollback::new(small_config());
        assert_eq!(sb.activity_counter(), 0);

        sb.push_line("hello".to_string());
        assert_eq!(sb.activity_counter(), 1);

        sb.push_lines((0..5).map(line));
        assert_eq!(sb.activity_counter(), 6);
    }

    #[test]
    fn activity_counter_in_snapshot() {
        let mut sb = TieredScrollback::new(small_config());
        sb.push_lines((0..10).map(line));

        let snap = sb.snapshot();
        assert_eq!(snap.activity_counter, 10);
    }

    #[test]
    fn activity_counter_resets_on_clear() {
        let mut sb = TieredScrollback::new(small_config());
        sb.push_lines((0..10).map(line));
        sb.clear();
        assert_eq!(sb.activity_counter(), 0);
    }

    // ── Cold uncompressed bytes tracking ───────────────────────────

    #[test]
    fn cold_uncompressed_bytes_tracked() {
        let config = ScrollbackConfig {
            hot_lines: 10,
            page_size: 5,
            warm_max_bytes: 50,
            compression: CompressionLevel::Fast,
            cold_eviction_enabled: true,
        };
        let mut sb = TieredScrollback::new(config);
        for i in 0..100 {
            sb.push_line(line(i));
        }

        let snap = sb.snapshot();
        assert!(snap.cold_uncompressed_bytes > 0);
        assert!(snap.cold_lines > 0);
    }

    // ── Estimated memory bytes ─────────────────────────────────────

    #[test]
    fn estimated_memory_bytes_includes_hot_and_warm() {
        let mut sb = TieredScrollback::new(small_config());
        for i in 0..30 {
            sb.push_line(line(i));
        }

        let est = sb.estimated_memory_bytes();
        assert!(est > 0);
        // Should include both hot line bytes and warm compressed bytes
        assert!(est >= sb.warm_total_bytes());
    }

    #[test]
    fn estimated_memory_bytes_empty() {
        let sb = TieredScrollback::new(small_config());
        assert_eq!(sb.estimated_memory_bytes(), 0);
    }

    // ── Cold tier retrieval ───────────────────────────────────────────

    /// Mock cold retriever that serves a fixed set of pages.
    struct MockColdRetriever {
        pages: std::collections::HashMap<u64, Vec<String>>,
    }

    impl MockColdRetriever {
        fn new() -> Self {
            Self {
                pages: std::collections::HashMap::new(),
            }
        }

        fn insert(&mut self, page_index: u64, lines: Vec<String>) {
            self.pages.insert(page_index, lines);
        }
    }

    impl ColdTierRetriever for MockColdRetriever {
        fn retrieve_page(&self, page_index: u64) -> Result<ColdPageData, ColdRetrievalError> {
            self.pages
                .get(&page_index)
                .cloned()
                .map(|lines| ColdPageData { page_index, lines })
                .ok_or(ColdRetrievalError::PageNotFound { page_index })
        }
    }

    #[test]
    fn null_retriever_returns_page_not_found() {
        let retriever = NullColdRetriever;
        let result = retriever.retrieve_page(0);
        assert!(matches!(
            result,
            Err(ColdRetrievalError::PageNotFound { page_index: 0 })
        ));
    }

    #[test]
    fn mock_retriever_serves_inserted_pages() {
        let mut retriever = MockColdRetriever::new();
        retriever.insert(0, vec!["hello".to_string(), "world".to_string()]);

        let page = retriever.retrieve_page(0).expect("page 0 should exist");
        assert_eq!(page.page_index, 0);
        assert_eq!(page.lines, vec!["hello", "world"]);

        // Non-existent page
        assert!(matches!(
            retriever.retrieve_page(99),
            Err(ColdRetrievalError::PageNotFound { page_index: 99 })
        ));
    }

    #[test]
    fn cold_line_retrieves_from_evicted_page() {
        let mut sb = TieredScrollback::new(ScrollbackConfig {
            hot_lines: 10,
            page_size: 5,
            warm_max_bytes: 50, // tiny cap forces cold eviction
            ..small_config()
        });

        // Push enough to create cold pages
        for i in 0..100 {
            sb.push_line(line(i));
        }

        assert!(sb.cold_line_count() > 0, "Should have cold lines");

        // Find a cold offset
        let total = sb.total_line_count() as usize;
        let cold_offset = total - 1; // oldest line

        if let Some(hint) = sb.locate_offset(cold_offset) {
            if matches!(hint, ScrollbackLocationHint::Cold { .. }) {
                // Use mock retriever with the expected page data
                let mut retriever = MockColdRetriever::new();
                if let ScrollbackLocationHint::Cold { page_index, .. } = &hint {
                    retriever.insert(*page_index, (0..5).map(line).collect());
                }

                let result = sb.cold_line(&hint, &retriever);
                assert!(result.is_ok(), "Should retrieve cold line: {result:?}");
            }
        }
    }

    #[test]
    fn cold_line_with_null_retriever_returns_error() {
        let sb = TieredScrollback::default();
        let hint = ScrollbackLocationHint::Cold {
            page_index: 42,
            page_offset_from_newest: 0,
            line_index_in_page: 0,
            page_line_count: 5,
        };

        let result = sb.cold_line(&hint, &NullColdRetriever);
        assert!(matches!(
            result,
            Err(ColdRetrievalError::PageNotFound { page_index: 42 })
        ));
    }

    #[test]
    fn cold_page_delegates_to_retriever() {
        let sb = TieredScrollback::default();
        let mut retriever = MockColdRetriever::new();
        retriever.insert(7, vec!["a".to_string(), "b".to_string()]);

        let page = sb.cold_page(7, &retriever).expect("should retrieve");
        assert_eq!(page.page_index, 7);
        assert_eq!(page.lines.len(), 2);
    }

    #[test]
    fn cold_retrieval_error_display() {
        let e1 = ColdRetrievalError::PageNotFound { page_index: 42 };
        assert!(e1.to_string().contains("42"));

        let e2 = ColdRetrievalError::StorageUnavailable {
            reason: "locked".to_string(),
        };
        assert!(e2.to_string().contains("locked"));

        let e3 = ColdRetrievalError::DecompressionFailed {
            page_index: 5,
            reason: "corrupt".to_string(),
        };
        assert!(e3.to_string().contains("5"));
        assert!(e3.to_string().contains("corrupt"));
    }

    #[test]
    fn cold_retrieval_error_serde_roundtrip() {
        let errors = vec![
            ColdRetrievalError::PageNotFound { page_index: 42 },
            ColdRetrievalError::StorageUnavailable {
                reason: "db locked".to_string(),
            },
            ColdRetrievalError::DecompressionFailed {
                page_index: 7,
                reason: "bad zstd".to_string(),
            },
        ];

        for err in &errors {
            let json = serde_json::to_string(err).expect("serialize");
            let back: ColdRetrievalError = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(*err, back);
        }
    }

    // ── Q1: seqlock warm-tier prefix-sum byte-equivalence ───────────────

    /// Deterministic xorshift64 PRNG (no external dep; reproducible).
    fn xorshift(state: &mut u64) -> u64 {
        let mut x = *state;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        *state = x;
        x
    }

    #[test]
    fn prefix_index_matches_linear_walk_property() {
        // Q1 (round-4 gauntlet): the gated binary-search resolution of
        // `locate_offset` / `tier_for_offset` must be byte-identical to the
        // deterministic linear walk over random push/evict histories and a
        // 10k-offset sweep. `indexed` runs with the prefix index ON; `linear`
        // is the same op stream with it OFF (legacy path). Identical op streams
        // ⇒ identical tier structure, so any divergence is the index's fault.
        let config = ScrollbackConfig {
            hot_lines: 12,
            page_size: 4,
            warm_max_bytes: 400, // tight cap → real warm + cold structure
            compression: CompressionLevel::Fast,
            cold_eviction_enabled: true,
        };

        let mut indexed = TieredScrollback::new_with_prefix_index(config.clone(), true);
        let mut linear = TieredScrollback::new_with_prefix_index(config, false);
        assert!(indexed.prefix_index_active());
        assert!(!linear.prefix_index_active());

        let mut rng = 0x9E37_79B9_7F4A_7C15_u64;
        let mut counter = 0usize;
        let mut produced_warm = false;
        let mut produced_cold = false;

        for round in 0..400u64 {
            match xorshift(&mut rng) % 12 {
                0 => {
                    let n = (xorshift(&mut rng) % 3 + 1) as usize;
                    assert_eq!(indexed.evict_warm_pages(n), linear.evict_warm_pages(n));
                }
                1 if round > 20 => {
                    indexed.evict_all_warm();
                    linear.evict_all_warm();
                }
                2 => {
                    let target = (xorshift(&mut rng) % 300) as usize;
                    assert_eq!(
                        indexed.evict_warm_to_target(target),
                        linear.evict_warm_to_target(target)
                    );
                }
                _ => {
                    let batch = (xorshift(&mut rng) % 6 + 1) as usize;
                    for _ in 0..batch {
                        let s = format!("r{round}-l{counter}-{}", xorshift(&mut rng) % 1000);
                        indexed.push_line(s.clone());
                        linear.push_line(s);
                        counter += 1;
                    }
                }
            }

            // Structural parity must hold at every step.
            assert_eq!(indexed.hot_len(), linear.hot_len());
            assert_eq!(indexed.warm_page_count(), linear.warm_page_count());
            assert_eq!(indexed.cold_page_count(), linear.cold_page_count());
            assert_eq!(indexed.total_line_count(), linear.total_line_count());
            produced_warm |= indexed.warm_page_count() > 0;
            produced_cold |= indexed.cold_page_count() > 0;

            // The indexed instance must keep resolving via the prefix index.
            assert!(
                indexed.prefix_index_active(),
                "prefix index must stay live + consistent (round {round})"
            );

            // Spot-check a slice of offsets each round (boundaries + interior +
            // out-of-range tail).
            let total = indexed.total_line_count() as usize;
            let span = total + 8;
            for _ in 0..40 {
                let o = (xorshift(&mut rng) as usize) % span;
                assert_eq!(
                    indexed.locate_offset(o),
                    linear.locate_offset(o),
                    "locate_offset mismatch at offset {o} (round {round})"
                );
                assert_eq!(
                    indexed.tier_for_offset(o),
                    linear.tier_for_offset(o),
                    "tier_for_offset mismatch at offset {o} (round {round})"
                );
            }
        }

        let total = indexed.total_line_count() as usize;
        assert!(produced_warm, "test must exercise the warm tier");
        assert!(produced_cold, "test must exercise the cold tier");
        assert!(total > 0);

        // Exhaustive sweep over every offset 0..total plus an out-of-range tail.
        for o in 0..total + 16 {
            assert_eq!(
                indexed.locate_offset(o),
                linear.locate_offset(o),
                "exhaustive locate_offset mismatch at {o}"
            );
            assert_eq!(
                indexed.tier_for_offset(o),
                linear.tier_for_offset(o),
                "exhaustive tier_for_offset mismatch at {o}"
            );
        }

        // 10k randomized offsets over the final structure (assignment mandate).
        let span = total + 16;
        for _ in 0..10_000 {
            let o = (xorshift(&mut rng) as usize) % span;
            assert_eq!(
                indexed.locate_offset(o),
                linear.locate_offset(o),
                "10k-sweep locate_offset mismatch at {o}"
            );
            assert_eq!(
                indexed.tier_for_offset(o),
                linear.tier_for_offset(o),
                "10k-sweep tier_for_offset mismatch at {o}"
            );
        }
    }

    #[test]
    fn prefix_index_env_gate_defaults_off() {
        // `new()` honors the env gate; with the var unset the index is off and
        // the legacy walk is used. (Proof runs with the gate default-off.)
        // Guard against a polluted env so the assertion is meaningful.
        if std::env::var_os("FT_MOONSHOT_SCROLLBACK_PREFIX_INDEX").is_none() {
            let sb = TieredScrollback::new(small_config());
            assert!(!sb.prefix_index_active(), "prefix index must default off");
        }
    }

    #[test]
    fn prefix_index_survives_clear_and_reuse() {
        let mut indexed = TieredScrollback::new_with_prefix_index(small_config(), true);
        let mut linear = TieredScrollback::new_with_prefix_index(small_config(), false);
        for i in 0..120 {
            indexed.push_line(line(i));
            linear.push_line(line(i));
        }
        indexed.clear();
        linear.clear();
        assert!(indexed.prefix_index_active());
        // Refill after clear: index must rebuild cleanly and stay equivalent.
        for i in 0..80 {
            indexed.push_line(line(i));
            linear.push_line(line(i));
        }
        let total = indexed.total_line_count() as usize;
        for o in 0..total + 4 {
            assert_eq!(indexed.locate_offset(o), linear.locate_offset(o));
            assert_eq!(indexed.tier_for_offset(o), linear.tier_for_offset(o));
        }
        assert!(indexed.prefix_index_active());
    }

    // ── M4: CDC dedup byte-equivalence ──────────────────────────────────

    /// All resident warm pages decoded, newest-first, as a flat vec of pages.
    fn warm_dump(sb: &TieredScrollback) -> Vec<Vec<String>> {
        (0..sb.warm_page_count())
            .map(|i| sb.warm_page_lines(i).expect("warm page must decode"))
            .collect()
    }

    #[test]
    fn cdc_chunk_bounds_tile_input_exactly() {
        // The chunker must partition the buffer into contiguous, gapless,
        // in-order ranges that reconstruct the input byte-for-byte.
        for len in [0usize, 1, 63, 64, 65, 511, 512, 4096, 4097, 20_000] {
            let raw: Vec<u8> = (0..len).map(|i| (i * 31 + 7) as u8).collect();
            let bounds = cdc_chunk_bounds(&raw);
            let mut pos = 0;
            let mut rebuilt = Vec::new();
            for (s, e) in &bounds {
                assert_eq!(*s, pos, "chunks must be contiguous (len {len})");
                assert!(e > s || len == 0, "chunks must be non-empty (len {len})");
                rebuilt.extend_from_slice(&raw[*s..*e]);
                pos = *e;
            }
            assert_eq!(pos, len, "chunks must cover the whole buffer (len {len})");
            assert_eq!(rebuilt, raw, "concatenated chunks must equal input (len {len})");
        }
    }

    #[test]
    fn cdc_round_trip_is_byte_identical_to_legacy() {
        // Round-trip byte-identity + golden default-mode unchanged: a CDC page's
        // decoded lines must match the legacy standalone-zstd page's decoded
        // lines exactly, over a diverse corpus (repeats, unicode, embedded
        // newlines, empty + long lines). Eviction off so every page stays warm.
        let config = ScrollbackConfig {
            hot_lines: 16,
            page_size: 8,
            warm_max_bytes: usize::MAX,
            compression: CompressionLevel::Fast,
            cold_eviction_enabled: false,
        };
        let mut cdc = TieredScrollback::new_with_options(config.clone(), false, true);
        let mut legacy = TieredScrollback::new_with_options(config, false, false);
        assert!(cdc.cdc_stats().is_some(), "cdc arm must have a store");
        assert!(legacy.cdc_stats().is_none(), "legacy arm must be plain");

        let prompt = "user@host:~/proj$ ";
        for i in 0..300usize {
            let lines = [
                format!("{prompt}run task {}", i % 7),
                "=== redraw banner: status OK ===".to_string(),
                format!("output {i}: unicode ✓ café ★ — padded to a reasonable terminal width here"),
                if i % 5 == 0 {
                    "multi\nline\nembedded\ncontent".to_string()
                } else {
                    format!("line {i}")
                },
                if i % 11 == 0 { String::new() } else { "tail".to_string() },
            ];
            for l in lines {
                cdc.push_line(l.clone());
                legacy.push_line(l);
            }
        }

        assert_eq!(cdc.hot_len(), legacy.hot_len());
        assert_eq!(cdc.warm_page_count(), legacy.warm_page_count());
        assert_eq!(cdc.total_line_count(), legacy.total_line_count());
        assert!(cdc.warm_page_count() > 4, "must exercise multiple warm pages");

        assert_eq!(warm_dump(&cdc), warm_dump(&legacy), "warm pages must decode identically");
        assert_eq!(cdc.tail(cdc.hot_len()), legacy.tail(legacy.hot_len()));

        // Accounting invariant: warm_bytes == live CAS compressed bytes.
        let (chunks, bytes) = cdc.cdc_stats().unwrap();
        assert!(chunks > 0 && bytes > 0);
        assert_eq!(cdc.warm_total_bytes(), bytes);
    }

    #[test]
    fn cdc_dedup_saves_bytes_on_repeated_pages() {
        // Identical repeated content must dedup: pushing the same block twice
        // adds far fewer unique chunks than references, and CDC warm bytes stay
        // well under the legacy (non-deduped) size.
        let config = ScrollbackConfig {
            hot_lines: 8,
            page_size: 8,
            warm_max_bytes: usize::MAX,
            compression: CompressionLevel::Fast,
            cold_eviction_enabled: false,
        };
        let mut cdc = TieredScrollback::new_with_options(config.clone(), false, true);
        let mut legacy = TieredScrollback::new_with_options(config, false, false);

        let block: Vec<String> = (0..40)
            .map(|i| format!("identical repeated prompt and output content row {i} — stable bytes"))
            .collect();
        for _ in 0..12 {
            for l in &block {
                cdc.push_line(l.clone());
                legacy.push_line(l.clone());
            }
        }

        assert_eq!(warm_dump(&cdc), warm_dump(&legacy), "dedup must stay byte-identical");
        assert!(
            cdc.warm_total_bytes() < legacy.warm_total_bytes(),
            "dedup must shrink warm bytes: cdc={} legacy={}",
            cdc.warm_total_bytes(),
            legacy.warm_total_bytes()
        );
    }

    /// Resident (warm oldest→newest, then hot) lines, decoded.
    fn resident_lines(sb: &TieredScrollback) -> Vec<String> {
        let mut out = Vec::new();
        for i in (0..sb.warm_page_count()).rev() {
            out.extend(sb.warm_page_lines(i).expect("warm page must decode"));
        }
        out.extend(sb.tail(sb.hot_len()).into_iter().map(|s| s.to_string()));
        out
    }

    #[test]
    fn cdc_eviction_keeps_resident_pages_decodable() {
        // Under a tight warm cap with eviction on, the CAS refcount must free
        // only chunks no resident page still references. CDC's smaller
        // warm_bytes legitimately keeps MORE pages warm than legacy, so we
        // verify byte-identity against a ground-truth reconstruction (not
        // legacy page parity): every resident line must equal what was pushed.
        let config = ScrollbackConfig {
            hot_lines: 10,
            page_size: 5,
            warm_max_bytes: 300, // tight → forces cold eviction
            compression: CompressionLevel::Fast,
            cold_eviction_enabled: true,
        };
        let mut cdc = TieredScrollback::new_with_options(config, false, true);

        let mut pushed = Vec::new();
        for i in 0..400usize {
            let l = format!("evt {} :: {}", i % 9, if i % 3 == 0 { "REPEATED" } else { "x" });
            cdc.push_line(l.clone());
            pushed.push(l);
        }

        assert!(cdc.cold_page_count() > 0, "must have evicted to cold");
        assert_eq!(cdc.total_line_count() as usize, pushed.len());
        let evicted = cdc.cold_line_count() as usize;
        assert_eq!(
            resident_lines(&cdc).as_slice(),
            &pushed[evicted..],
            "resident lines must reconstruct byte-identically under eviction"
        );

        // Accounting stays exact across eviction (no leak, no double-free).
        let (_chunks, bytes) = cdc.cdc_stats().unwrap();
        assert_eq!(cdc.warm_total_bytes(), bytes);

        // After clear, the store is empty and reusable.
        cdc.clear();
        assert_eq!(cdc.cdc_stats(), Some((0, 0)));
        assert_eq!(cdc.warm_total_bytes(), 0);
        cdc.push_lines((0..60).map(line));
        assert!(cdc.cdc_stats().unwrap().0 > 0);
    }

    #[test]
    fn cdc_gate_defaults_off() {
        if std::env::var_os("FT_MOONSHOT_SCROLLBACK_CDC_DEDUP").is_none() {
            let sb = TieredScrollback::new(small_config());
            assert!(sb.cdc_stats().is_none(), "cdc dedup must default off");
        }
    }
}
