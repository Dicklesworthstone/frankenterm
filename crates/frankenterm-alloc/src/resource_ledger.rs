//! Live process resource ledger: GPU textures and buffers by purpose
//! (ft-yccm0.2.5, shared with the M.6 footprint gate ft-yccm0.1.7).
//!
//! A leaked GPU texture is invisible to the CPU heap: the frozen 0.15.2 GUI
//! held about 3 GB of "owned unmapped" GPU memory while its jemalloc heap
//! looked unremarkable. Every GPU allocation therefore registers here and
//! receives a [`GpuResourceGuard`] that the owning object stores. Dropping the
//! guard (that is, dropping the GPU object itself) releases the accounting, so
//! a leak shows up as monotonic growth of `live_count`/`live_bytes` rather than
//! needing a code audit to find.
//!
//! The counters are atomics, so any thread can take a [`GpuResourceSnapshot`].
//! A GUI process publishes its snapshot with [`ResourceSnapshotPublisher`] into
//! its runtime directory, and `ft doctor --json` (a separate process) reads
//! every published snapshot back with [`collect_resource_snapshots`].

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Schema identifier written into every published snapshot file.
pub const RESOURCE_SNAPSHOT_SCHEMA: &str = "frankenterm.resource_snapshot.v1";

/// File-name prefix of a published snapshot: `<prefix><pid>.json`.
pub const RESOURCE_SNAPSHOT_FILE_PREFIX: &str = "frankenterm-resources-";

const RESOURCE_SNAPSHOT_FILE_SUFFIX: &str = ".json";

/// A publisher rewrites an unchanged snapshot at least this often, so a
/// reader can tell a live process from one that exited without cleanup.
pub const RESOURCE_SNAPSHOT_HEARTBEAT: Duration = Duration::from_secs(30);

/// Snapshots older than this are reported as stale by
/// [`collect_resource_snapshots`]: three missed heartbeats.
pub const RESOURCE_SNAPSHOT_FRESHNESS: Duration = Duration::from_secs(90);

/// Overrides the publisher's change-check interval, in milliseconds
/// (ft-yccm0.1.4). The GUI throughput harness sets it so the presented-frame
/// count is published often enough to bracket a drain window tightly.
pub const RESOURCE_SNAPSHOT_INTERVAL_ENV: &str = "FT_RESOURCE_SNAPSHOT_INTERVAL_MS";

/// Accepted range for [`RESOURCE_SNAPSHOT_INTERVAL_ENV`], in milliseconds.
const RESOURCE_SNAPSHOT_INTERVAL_MS_RANGE: std::ops::RangeInclusive<u64> = 50..=60_000;

/// The publisher interval: `default`, unless [`RESOURCE_SNAPSHOT_INTERVAL_ENV`]
/// holds a millisecond count within 50..=60000.
#[must_use]
pub fn resource_snapshot_interval(default: Duration) -> Duration {
    resource_snapshot_interval_from(
        std::env::var(RESOURCE_SNAPSHOT_INTERVAL_ENV)
            .ok()
            .as_deref(),
        default,
    )
}

fn resource_snapshot_interval_from(value: Option<&str>, default: Duration) -> Duration {
    let Some(value) = value else {
        return default;
    };
    match value.trim().parse::<u64>() {
        Ok(ms) if RESOURCE_SNAPSHOT_INTERVAL_MS_RANGE.contains(&ms) => Duration::from_millis(ms),
        _ => {
            log::warn!(
                "ignoring {RESOURCE_SNAPSHOT_INTERVAL_ENV}={value:?}: expected milliseconds in \
                 {}..={}; publishing every {} ms",
                RESOURCE_SNAPSHOT_INTERVAL_MS_RANGE.start(),
                RESOURCE_SNAPSHOT_INTERVAL_MS_RANGE.end(),
                default.as_millis()
            );
            default
        }
    }
}

/// What a GPU texture is used for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum GpuTexturePurpose {
    /// Glyph/sprite atlas pages.
    Atlas,
    /// Presentation drawables (swapchain / CAMetalLayer images).
    Drawable,
    /// Decoded inline images uploaded outside the atlas.
    Image,
    /// Anything else (offscreen targets, readback staging textures).
    Other,
}

impl GpuTexturePurpose {
    /// Every purpose, in snapshot order.
    pub const ALL: [Self; 4] = [Self::Atlas, Self::Drawable, Self::Image, Self::Other];

    /// Stable snapshot key.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Atlas => "atlas",
            Self::Drawable => "drawable",
            Self::Image => "image",
            Self::Other => "other",
        }
    }

    const fn index(self) -> usize {
        self as usize
    }
}

/// What a GPU buffer is used for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum GpuBufferPurpose {
    /// Quad vertex buffers.
    Vertex,
    /// Quad index buffers.
    Index,
    /// Shader uniform buffers.
    Uniform,
    /// CPU readback staging buffers.
    Readback,
    /// Anything else.
    Other,
}

impl GpuBufferPurpose {
    /// Every purpose, in snapshot order.
    pub const ALL: [Self; 5] = [
        Self::Vertex,
        Self::Index,
        Self::Uniform,
        Self::Readback,
        Self::Other,
    ];

    /// Stable snapshot key.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Vertex => "vertex",
            Self::Index => "index",
            Self::Uniform => "uniform",
            Self::Readback => "readback",
            Self::Other => "other",
        }
    }

    const fn index(self) -> usize {
        self as usize
    }
}

/// Atomic live/peak accounting for one resource class.
#[derive(Debug)]
struct ResourceCounter {
    live_count: AtomicU64,
    live_bytes: AtomicU64,
    created_total: AtomicU64,
    released_total: AtomicU64,
    peak_live_bytes: AtomicU64,
}

impl ResourceCounter {
    const fn new() -> Self {
        Self {
            live_count: AtomicU64::new(0),
            live_bytes: AtomicU64::new(0),
            created_total: AtomicU64::new(0),
            released_total: AtomicU64::new(0),
            peak_live_bytes: AtomicU64::new(0),
        }
    }

    fn acquire(&self, bytes: u64) {
        self.created_total.fetch_add(1, Ordering::Relaxed);
        self.live_count.fetch_add(1, Ordering::Relaxed);
        let live = self
            .live_bytes
            .fetch_add(bytes, Ordering::Relaxed)
            .saturating_add(bytes);
        self.peak_live_bytes.fetch_max(live, Ordering::Relaxed);
    }

    fn release(&self, bytes: u64) {
        self.released_total.fetch_add(1, Ordering::Relaxed);
        // Every release pairs with exactly one earlier acquire of the same
        // byte count (the guard carries it), so neither value can underflow.
        self.live_count.fetch_sub(1, Ordering::Relaxed);
        self.live_bytes.fetch_sub(bytes, Ordering::Relaxed);
    }

    fn snapshot(&self) -> ResourceCounterSnapshot {
        ResourceCounterSnapshot {
            live_count: self.live_count.load(Ordering::Relaxed),
            live_bytes: self.live_bytes.load(Ordering::Relaxed),
            created_total: self.created_total.load(Ordering::Relaxed),
            released_total: self.released_total.load(Ordering::Relaxed),
            peak_live_bytes: self.peak_live_bytes.load(Ordering::Relaxed),
        }
    }
}

/// Process-wide (or, in tests, isolated) GPU resource accounting.
#[derive(Debug)]
pub struct GpuResourceLedger {
    textures: [ResourceCounter; GpuTexturePurpose::ALL.len()],
    texture_total: ResourceCounter,
    buffers: [ResourceCounter; GpuBufferPurpose::ALL.len()],
    buffer_total: ResourceCounter,
    atlas_generations: AtomicU64,
}

static GLOBAL_GPU_RESOURCE_LEDGER: GpuResourceLedger = GpuResourceLedger::new();

impl Default for GpuResourceLedger {
    fn default() -> Self {
        Self::new()
    }
}

impl GpuResourceLedger {
    /// An empty ledger. Production code uses [`Self::global`]; tests leak a
    /// private one so concurrent tests cannot disturb each other's counts.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            textures: [
                ResourceCounter::new(),
                ResourceCounter::new(),
                ResourceCounter::new(),
                ResourceCounter::new(),
            ],
            texture_total: ResourceCounter::new(),
            buffers: [
                ResourceCounter::new(),
                ResourceCounter::new(),
                ResourceCounter::new(),
                ResourceCounter::new(),
                ResourceCounter::new(),
            ],
            buffer_total: ResourceCounter::new(),
            atlas_generations: AtomicU64::new(0),
        }
    }

    /// The ledger every production GPU allocation registers with.
    #[must_use]
    pub fn global() -> &'static Self {
        &GLOBAL_GPU_RESOURCE_LEDGER
    }

    /// Register a texture of `bytes` GPU bytes. Store the guard inside the
    /// object that owns the texture so the accounting ends exactly when the
    /// texture is dropped.
    #[must_use = "dropping the guard immediately releases the accounting"]
    pub fn track_texture(
        &'static self,
        purpose: GpuTexturePurpose,
        bytes: u64,
    ) -> GpuResourceGuard {
        self.textures[purpose.index()].acquire(bytes);
        self.texture_total.acquire(bytes);
        GpuResourceGuard {
            ledger: self,
            class: GuardClass::Texture(purpose),
            bytes,
        }
    }

    /// Register a buffer of `bytes` GPU bytes; see [`Self::track_texture`].
    #[must_use = "dropping the guard immediately releases the accounting"]
    pub fn track_buffer(&'static self, purpose: GpuBufferPurpose, bytes: u64) -> GpuResourceGuard {
        self.buffers[purpose.index()].acquire(bytes);
        self.buffer_total.acquire(bytes);
        GpuResourceGuard {
            ledger: self,
            class: GuardClass::Buffer(purpose),
            bytes,
        }
    }

    /// Count one glyph-atlas replacement (a rebuild after overflow or a grow).
    pub fn record_atlas_generation(&self) {
        self.atlas_generations.fetch_add(1, Ordering::Relaxed);
    }

    /// Live textures of one purpose.
    #[must_use]
    pub fn texture_counter(&self, purpose: GpuTexturePurpose) -> ResourceCounterSnapshot {
        self.textures[purpose.index()].snapshot()
    }

    /// Live buffers of one purpose.
    #[must_use]
    pub fn buffer_counter(&self, purpose: GpuBufferPurpose) -> ResourceCounterSnapshot {
        self.buffers[purpose.index()].snapshot()
    }

    /// A point-in-time copy of every counter.
    #[must_use]
    pub fn snapshot(&self) -> GpuResourceSnapshot {
        GpuResourceSnapshot {
            textures: GpuTexturePurpose::ALL
                .iter()
                .map(|purpose| (purpose.as_str().to_string(), self.texture_counter(*purpose)))
                .collect(),
            texture_total: self.texture_total.snapshot(),
            buffers: GpuBufferPurpose::ALL
                .iter()
                .map(|purpose| (purpose.as_str().to_string(), self.buffer_counter(*purpose)))
                .collect(),
            buffer_total: self.buffer_total.snapshot(),
            atlas_generations: self.atlas_generations.load(Ordering::Relaxed),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GuardClass {
    Texture(GpuTexturePurpose),
    Buffer(GpuBufferPurpose),
}

/// RAII registration of one live GPU resource; dropping it releases the
/// resource's accounting. Not `Clone`: one guard per GPU object.
#[derive(Debug)]
pub struct GpuResourceGuard {
    ledger: &'static GpuResourceLedger,
    class: GuardClass,
    bytes: u64,
}

impl GpuResourceGuard {
    /// GPU bytes this guard accounts for.
    #[must_use]
    pub const fn bytes(&self) -> u64 {
        self.bytes
    }
}

impl Drop for GpuResourceGuard {
    fn drop(&mut self) {
        match self.class {
            GuardClass::Texture(purpose) => {
                self.ledger.textures[purpose.index()].release(self.bytes);
                self.ledger.texture_total.release(self.bytes);
            }
            GuardClass::Buffer(purpose) => {
                self.ledger.buffers[purpose.index()].release(self.bytes);
                self.ledger.buffer_total.release(self.bytes);
            }
        }
    }
}

/// Bytes of a `width` x `height` texture with `bytes_per_texel` per texel.
#[must_use]
pub fn texture_bytes(width: u64, height: u64, bytes_per_texel: u64) -> u64 {
    width.saturating_mul(height).saturating_mul(bytes_per_texel)
}

/// Serializable copy of one resource counter.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourceCounterSnapshot {
    /// Resources currently alive.
    pub live_count: u64,
    /// GPU bytes currently alive.
    pub live_bytes: u64,
    /// Resources ever created.
    pub created_total: u64,
    /// Resources ever released.
    pub released_total: u64,
    /// High-water mark of `live_bytes`.
    pub peak_live_bytes: u64,
}

/// Serializable copy of a whole [`GpuResourceLedger`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GpuResourceSnapshot {
    /// Texture counters keyed by [`GpuTexturePurpose::as_str`].
    pub textures: BTreeMap<String, ResourceCounterSnapshot>,
    /// All textures together (its peak is the peak of the sum).
    pub texture_total: ResourceCounterSnapshot,
    /// Buffer counters keyed by [`GpuBufferPurpose::as_str`].
    pub buffers: BTreeMap<String, ResourceCounterSnapshot>,
    /// All buffers together (its peak is the peak of the sum).
    pub buffer_total: ResourceCounterSnapshot,
    /// Glyph-atlas replacements since process start.
    pub atlas_generations: u64,
}

/// CPU-side cache sizes that GUI windows report (ft-yccm0.1.7). Entries are
/// exact counts; bytes are the cache's own accounting of the heap its entries
/// own (see each reporter), not an RSS measurement.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum CacheGauge {
    ShapeCacheEntries,
    ShapeCacheBytes,
    LineShapeCacheEntries,
    LineShapeCacheBytes,
    LineQuadCacheEntries,
    LineQuadCacheBytes,
    GlyphCacheEntries,
    ImageCacheBytes,
}

impl CacheGauge {
    /// Every gauge, in snapshot order.
    pub const ALL: [Self; 8] = [
        Self::ShapeCacheEntries,
        Self::ShapeCacheBytes,
        Self::LineShapeCacheEntries,
        Self::LineShapeCacheBytes,
        Self::LineQuadCacheEntries,
        Self::LineQuadCacheBytes,
        Self::GlyphCacheEntries,
        Self::ImageCacheBytes,
    ];

    /// Stable snapshot key.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ShapeCacheEntries => "shape_cache_entries",
            Self::ShapeCacheBytes => "shape_cache_bytes",
            Self::LineShapeCacheEntries => "line_shape_cache_entries",
            Self::LineShapeCacheBytes => "line_shape_cache_bytes",
            Self::LineQuadCacheEntries => "line_quad_cache_entries",
            Self::LineQuadCacheBytes => "line_quad_cache_bytes",
            Self::GlyphCacheEntries => "glyph_cache_entries",
            Self::ImageCacheBytes => "image_cache_bytes",
        }
    }

    const fn index(self) -> usize {
        self as usize
    }
}

/// Process-wide sums of [`CacheGauge`]s over every reporting window.
#[derive(Debug)]
pub struct CacheGauges {
    values: [AtomicU64; CacheGauge::ALL.len()],
}

static GLOBAL_CACHE_GAUGES: CacheGauges = CacheGauges::new();

impl Default for CacheGauges {
    fn default() -> Self {
        Self::new()
    }
}

impl CacheGauges {
    /// All gauges at zero.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            values: [
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
            ],
        }
    }

    /// The gauges production windows report into.
    #[must_use]
    pub fn global() -> &'static Self {
        &GLOBAL_CACHE_GAUGES
    }

    /// One reporter's share of the sums (one per window).
    #[must_use]
    pub fn contribution(&'static self) -> CacheGaugeContribution {
        CacheGaugeContribution {
            gauges: self,
            reported: [0; CacheGauge::ALL.len()],
        }
    }

    /// Current sum of one gauge.
    #[must_use]
    pub fn value(&self, gauge: CacheGauge) -> u64 {
        self.values[gauge.index()].load(Ordering::Relaxed)
    }

    /// Every gauge keyed by [`CacheGauge::as_str`].
    #[must_use]
    pub fn snapshot(&self) -> BTreeMap<String, u64> {
        CacheGauge::ALL
            .iter()
            .map(|gauge| (gauge.as_str().to_string(), self.value(*gauge)))
            .collect()
    }
}

/// A reporter's current values; the global sums move by the delta on each
/// [`Self::set`], and dropping the reporter (a closed window) withdraws
/// everything it reported.
#[derive(Debug)]
pub struct CacheGaugeContribution {
    gauges: &'static CacheGauges,
    reported: [u64; CacheGauge::ALL.len()],
}

impl CacheGaugeContribution {
    /// Report `value` as this reporter's current `gauge`.
    pub fn set(&mut self, gauge: CacheGauge, value: u64) {
        let index = gauge.index();
        let previous = std::mem::replace(&mut self.reported[index], value);
        let sum = &self.gauges.values[index];
        if value >= previous {
            sum.fetch_add(value - previous, Ordering::Relaxed);
        } else {
            sum.fetch_sub(previous - value, Ordering::Relaxed);
        }
    }
}

impl Drop for CacheGaugeContribution {
    fn drop(&mut self) {
        for gauge in CacheGauge::ALL {
            self.set(gauge, 0);
        }
    }
}

/// Scrollback residency of one pane (ft-yccm0.1.7). Rows are exact; the warm
/// and cold byte counts are the term crate's own tier accounting.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneResourceSnapshot {
    pub pane_id: u64,
    /// Rows in the in-memory (hot) tier: visible rows plus in-memory scrollback.
    pub hot_rows: u64,
    pub warm_resident_lines: u64,
    pub warm_resident_bytes: u64,
    pub cold_retained_lines: u64,
    pub cold_retained_bytes: u64,
}

/// Allocator statistics at publish time.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AllocatorSnapshot {
    /// [`crate::AllocatorBackend::as_str`].
    pub backend: String,
    /// jemalloc `stats.*` in bytes; `None` when unavailable.
    pub stats: Option<AllocatorStatsSnapshot>,
    /// Why `stats` is `None`.
    pub unavailable: Option<String>,
    /// jemalloc's run-time options (ft-yccm0.2.8), so a bundle records which
    /// tuning produced it; `None` when unavailable.
    #[serde(default)]
    pub options: Option<crate::JemallocOptions>,
}

/// Serializable [`crate::AllocatorStats`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AllocatorStatsSnapshot {
    pub allocated: u64,
    pub active: u64,
    pub resident: u64,
    pub mapped: u64,
    pub retained: u64,
}

impl AllocatorSnapshot {
    /// Read the process allocator now.
    #[must_use]
    pub fn read() -> Self {
        let backend = crate::allocator_backend().as_str().to_string();
        let options = crate::jemalloc_options().ok();
        match crate::read_allocator_stats() {
            Ok(stats) => Self {
                backend,
                stats: Some(AllocatorStatsSnapshot {
                    allocated: stats.allocated as u64,
                    active: stats.active as u64,
                    resident: stats.resident as u64,
                    mapped: stats.mapped as u64,
                    retained: stats.retained as u64,
                }),
                unavailable: None,
                options,
            },
            Err(error) => Self {
                backend,
                stats: None,
                unavailable: Some(error.to_string()),
                options,
            },
        }
    }
}

/// Sub-buckets per power of two in [`LatencyHistogram`]: two mantissa bits,
/// so a reported quantile overstates its true value by at most 25%.
const LATENCY_SUB_BUCKETS: usize = 4;
/// Enough buckets for every `u64` nanosecond value.
const LATENCY_BUCKETS: usize = 63 * LATENCY_SUB_BUCKETS;

/// Lock-free log-linear histogram of nanosecond durations (ft-yccm0.1.5).
///
/// Recording is a few relaxed atomic adds, so any thread may record without
/// a lock. Values below 4 ns are exact; above that each power of two splits
/// into four buckets.
#[derive(Debug)]
pub struct LatencyHistogram {
    buckets: [AtomicU64; LATENCY_BUCKETS],
    total_ns: AtomicU64,
    max_ns: AtomicU64,
}

impl Default for LatencyHistogram {
    fn default() -> Self {
        Self::new()
    }
}

impl LatencyHistogram {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            buckets: [const { AtomicU64::new(0) }; LATENCY_BUCKETS],
            total_ns: AtomicU64::new(0),
            max_ns: AtomicU64::new(0),
        }
    }

    /// The bucket holding `value`.
    const fn bucket_index(value: u64) -> usize {
        if value < LATENCY_SUB_BUCKETS as u64 {
            return value as usize;
        }
        let msb = 63 - value.leading_zeros() as usize;
        let sub = ((value >> (msb - 2)) & (LATENCY_SUB_BUCKETS as u64 - 1)) as usize;
        (msb - 1) * LATENCY_SUB_BUCKETS + sub
    }

    /// The largest value that lands in bucket `index`.
    const fn bucket_upper_bound(index: usize) -> u64 {
        if index < LATENCY_SUB_BUCKETS {
            return index as u64;
        }
        let msb = index / LATENCY_SUB_BUCKETS + 1;
        let sub = (index % LATENCY_SUB_BUCKETS) as u64;
        let width = 1u64 << (msb - 2);
        let lower = (LATENCY_SUB_BUCKETS as u64 + sub) << (msb - 2);
        lower + (width - 1)
    }

    pub fn record(&self, value_ns: u64) {
        self.buckets[Self::bucket_index(value_ns)].fetch_add(1, Ordering::Relaxed);
        self.total_ns.fetch_add(value_ns, Ordering::Relaxed);
        self.max_ns.fetch_max(value_ns, Ordering::Relaxed);
    }

    /// Quantiles report the upper bound of the bucket the rank falls in,
    /// capped at the true maximum: conservative, never an understatement.
    #[must_use]
    pub fn snapshot(&self) -> LatencySnapshot {
        let counts: Vec<u64> = self
            .buckets
            .iter()
            .map(|bucket| bucket.load(Ordering::Relaxed))
            .collect();
        let count: u64 = counts.iter().sum();
        let max_ns = self.max_ns.load(Ordering::Relaxed);
        let quantile = |numerator: u64, denominator: u64| -> u64 {
            if count == 0 {
                return 0;
            }
            // The smallest rank r with r / count >= numerator / denominator.
            let rank = (count.saturating_mul(numerator))
                .div_ceil(denominator)
                .max(1);
            let mut seen = 0u64;
            for (index, bucket) in counts.iter().enumerate() {
                seen += bucket;
                if seen >= rank {
                    return Self::bucket_upper_bound(index).min(max_ns);
                }
            }
            max_ns
        };
        LatencySnapshot {
            count,
            total_ns: self.total_ns.load(Ordering::Relaxed),
            p50_ns: quantile(50, 100),
            p95_ns: quantile(95, 100),
            p99_ns: quantile(99, 100),
            max_ns,
        }
    }
}

/// Summary of a [`LatencyHistogram`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LatencySnapshot {
    pub count: u64,
    pub total_ns: u64,
    pub p50_ns: u64,
    pub p95_ns: u64,
    pub p99_ns: u64,
    pub max_ns: u64,
}

/// Which code path holds or waits for a pane's terminal mutex (ft-yccm0.1.5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum TerminalLockHolder {
    /// Applying parsed output, deferred-scrollback trims, checkpoint capture.
    Parser,
    /// Paint reads: lines, damage, cursor, dimensions, palette, surface.
    Paint,
    Mouse,
    Resize,
    /// Logical-line reads for selection and search.
    Selection,
    Other,
}

impl TerminalLockHolder {
    pub const ALL: [Self; 6] = [
        Self::Parser,
        Self::Paint,
        Self::Mouse,
        Self::Resize,
        Self::Selection,
        Self::Other,
    ];

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Parser => "parser",
            Self::Paint => "paint",
            Self::Mouse => "mouse",
            Self::Resize => "resize",
            Self::Selection => "selection",
            Self::Other => "other",
        }
    }

    const fn index(self) -> usize {
        self as usize
    }
}

/// Process-wide terminal-mutex wait and hold times by holder kind, plus the
/// time the main (UI) thread spent blocked on a terminal mutex
/// (ft-yccm0.1.5). Every pane's mutex records here.
#[derive(Debug)]
pub struct TerminalLockLedger {
    wait: [LatencyHistogram; TerminalLockHolder::ALL.len()],
    hold: [LatencyHistogram; TerminalLockHolder::ALL.len()],
    main_thread_blocked_ns: AtomicU64,
    main_thread_blocked_waits: AtomicU64,
    main_thread_max_wait_ns: AtomicU64,
}

static GLOBAL_TERMINAL_LOCK_LEDGER: TerminalLockLedger = TerminalLockLedger::new();

impl Default for TerminalLockLedger {
    fn default() -> Self {
        Self::new()
    }
}

impl TerminalLockLedger {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            wait: [const { LatencyHistogram::new() }; TerminalLockHolder::ALL.len()],
            hold: [const { LatencyHistogram::new() }; TerminalLockHolder::ALL.len()],
            main_thread_blocked_ns: AtomicU64::new(0),
            main_thread_blocked_waits: AtomicU64::new(0),
            main_thread_max_wait_ns: AtomicU64::new(0),
        }
    }

    /// The ledger every pane's terminal mutex records into.
    #[must_use]
    pub fn global() -> &'static Self {
        &GLOBAL_TERMINAL_LOCK_LEDGER
    }

    /// One acquisition: `wait_ns` is zero when the mutex was free.
    pub fn record_wait(&self, holder: TerminalLockHolder, wait_ns: u64, on_main_thread: bool) {
        self.wait[holder.index()].record(wait_ns);
        if on_main_thread && wait_ns > 0 {
            self.main_thread_blocked_ns
                .fetch_add(wait_ns, Ordering::Relaxed);
            self.main_thread_blocked_waits
                .fetch_add(1, Ordering::Relaxed);
            self.main_thread_max_wait_ns
                .fetch_max(wait_ns, Ordering::Relaxed);
        }
    }

    pub fn record_hold(&self, holder: TerminalLockHolder, hold_ns: u64) {
        self.hold[holder.index()].record(hold_ns);
    }

    #[must_use]
    pub fn snapshot(&self) -> TerminalLocksSnapshot {
        let holders = TerminalLockHolder::ALL
            .iter()
            .filter_map(|holder| {
                let wait = self.wait[holder.index()].snapshot();
                let hold = self.hold[holder.index()].snapshot();
                (wait.count > 0 || hold.count > 0).then(|| {
                    (
                        holder.as_str().to_string(),
                        TerminalLockHolderSnapshot { wait, hold },
                    )
                })
            })
            .collect();
        TerminalLocksSnapshot {
            holders,
            main_thread_blocked_ns: self.main_thread_blocked_ns.load(Ordering::Relaxed),
            main_thread_blocked_waits: self.main_thread_blocked_waits.load(Ordering::Relaxed),
            main_thread_max_wait_ns: self.main_thread_max_wait_ns.load(Ordering::Relaxed),
        }
    }
}

/// Wait and hold summaries for one holder kind.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerminalLockHolderSnapshot {
    pub wait: LatencySnapshot,
    pub hold: LatencySnapshot,
}

/// The `terminal_locks` section of a published snapshot.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerminalLocksSnapshot {
    /// Keyed by [`TerminalLockHolder::as_str`]; kinds never seen are absent.
    pub holders: BTreeMap<String, TerminalLockHolderSnapshot>,
    /// Total time the main thread spent waiting for a terminal mutex.
    pub main_thread_blocked_ns: u64,
    /// Main-thread acquisitions that had to wait.
    pub main_thread_blocked_waits: u64,
    pub main_thread_max_wait_ns: u64,
}

/// Durable scrollback writer health (ft-yccm0.2.1.1): panes whose durability
/// is degraded, and the writer's failure counters.
#[derive(Debug)]
pub struct DurabilityLedger {
    degraded: std::sync::Mutex<BTreeMap<String, String>>,
    writer_failures_total: AtomicU64,
    writer_panics_total: AtomicU64,
    rows_abandoned_total: AtomicU64,
}

static GLOBAL_DURABILITY_LEDGER: DurabilityLedger = DurabilityLedger::new();

impl Default for DurabilityLedger {
    fn default() -> Self {
        Self::new()
    }
}

impl DurabilityLedger {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            degraded: std::sync::Mutex::new(BTreeMap::new()),
            writer_failures_total: AtomicU64::new(0),
            writer_panics_total: AtomicU64::new(0),
            rows_abandoned_total: AtomicU64::new(0),
        }
    }

    /// The ledger the process's durability writer records into.
    #[must_use]
    pub fn global() -> &'static Self {
        &GLOBAL_DURABILITY_LEDGER
    }

    /// A failed (or panicked) drain degrades `pane` until it next succeeds.
    pub fn record_failure(&self, pane: &str, error: &str, panicked: bool) {
        self.writer_failures_total.fetch_add(1, Ordering::Relaxed);
        if panicked {
            self.writer_panics_total.fetch_add(1, Ordering::Relaxed);
        }
        crate::recover_poisoned(self.degraded.lock()).insert(pane.to_string(), error.to_string());
    }

    pub fn record_recovered(&self, pane: &str) {
        crate::recover_poisoned(self.degraded.lock()).remove(pane);
    }

    /// `pane` was dropped with `rows` admitted rows that never became durable.
    pub fn record_abandoned(&self, pane: &str, rows: u64) {
        self.rows_abandoned_total.fetch_add(rows, Ordering::Relaxed);
        crate::recover_poisoned(self.degraded.lock()).remove(pane);
    }

    #[must_use]
    pub fn snapshot(&self) -> DurabilitySnapshot {
        DurabilitySnapshot {
            degraded_panes: crate::recover_poisoned(self.degraded.lock()).clone(),
            writer_failures_total: self.writer_failures_total.load(Ordering::Relaxed),
            writer_panics_total: self.writer_panics_total.load(Ordering::Relaxed),
            rows_abandoned_total: self.rows_abandoned_total.load(Ordering::Relaxed),
        }
    }
}

/// The `durability` section of a published snapshot.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DurabilitySnapshot {
    /// Durable pane id -> the writer failure that degraded it.
    pub degraded_panes: BTreeMap<String, String>,
    pub writer_failures_total: u64,
    pub writer_panics_total: u64,
    pub rows_abandoned_total: u64,
}

/// Frames the GUI handed to its graphics backend for presentation, and the
/// intervals between them (ft-yccm0.1.4). This is FrankenTerm's own frame
/// count: `scripts/mac-gui-throughput.sh` validates its terminal-agnostic
/// screen-capture FPS meter against it.
#[derive(Debug)]
pub struct FrameLedger {
    presented_total: AtomicU64,
    present_failures_total: AtomicU64,
    present_interval: LatencyHistogram,
    /// Monotonic nanoseconds of the latest present; 0 before the first.
    last_present_ns: AtomicU64,
    max_fps: AtomicU64,
}

static GLOBAL_FRAME_LEDGER: FrameLedger = FrameLedger::new();

impl Default for FrameLedger {
    fn default() -> Self {
        Self::new()
    }
}

impl FrameLedger {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            presented_total: AtomicU64::new(0),
            present_failures_total: AtomicU64::new(0),
            present_interval: LatencyHistogram::new(),
            last_present_ns: AtomicU64::new(0),
            max_fps: AtomicU64::new(0),
        }
    }

    /// The ledger the GUI's paint path records into.
    #[must_use]
    pub fn global() -> &'static Self {
        &GLOBAL_FRAME_LEDGER
    }

    /// The repaint-rate cap (`max_fps`) the presenting window runs with.
    pub fn set_max_fps(&self, max_fps: u64) {
        self.max_fps.store(max_fps, Ordering::Relaxed);
    }

    /// One frame accepted by the backend for presentation, now.
    pub fn record_present(&self) {
        self.record_present_at(monotonic_ns());
    }

    /// One frame accepted for presentation at `at_ns` on a monotonic
    /// nanosecond clock; the interval since the previous one is recorded.
    pub fn record_present_at(&self, at_ns: u64) {
        let at_ns = at_ns.max(1);
        self.presented_total.fetch_add(1, Ordering::Relaxed);
        let previous = self.last_present_ns.swap(at_ns, Ordering::Relaxed);
        if previous != 0 {
            self.present_interval.record(at_ns.saturating_sub(previous));
        }
    }

    /// A frame the backend refused to present.
    pub fn record_present_failure(&self) {
        self.present_failures_total.fetch_add(1, Ordering::Relaxed);
    }

    #[must_use]
    pub fn snapshot(&self) -> FramesSnapshot {
        FramesSnapshot {
            presented_total: self.presented_total.load(Ordering::Relaxed),
            present_failures_total: self.present_failures_total.load(Ordering::Relaxed),
            present_interval: self.present_interval.snapshot(),
            max_fps: self.max_fps.load(Ordering::Relaxed),
        }
    }
}

/// Nanoseconds since this process first asked, on the monotonic clock.
fn monotonic_ns() -> u64 {
    static EPOCH: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
    let epoch = *EPOCH.get_or_init(std::time::Instant::now);
    u64::try_from(epoch.elapsed().as_nanos()).unwrap_or(u64::MAX)
}

/// The `frames` section of a published snapshot.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FramesSnapshot {
    pub presented_total: u64,
    pub present_failures_total: u64,
    /// Present-to-present intervals over the process lifetime, idle gaps
    /// included.
    pub present_interval: LatencySnapshot,
    /// The configured repaint-rate cap; 0 until a frame reports it.
    pub max_fps: u64,
}

impl FramesSnapshot {
    /// One human-readable line for the GUI debug overlay.
    #[must_use]
    pub fn summary_line(&self) -> String {
        let ms = |ns: u64| ns as f64 / 1_000_000.0;
        format!(
            "Frames: presented {} (failed {}); present interval p50 {:.2} ms, p95 {:.2} ms, max {:.2} ms; max_fps {}",
            self.presented_total,
            self.present_failures_total,
            ms(self.present_interval.p50_ns),
            ms(self.present_interval.p95_ns),
            ms(self.present_interval.max_ns),
            self.max_fps
        )
    }
}

/// The change-detected part of a published snapshot.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourceSnapshotBody {
    pub gpu: GpuResourceSnapshot,
    /// [`CacheGauges::snapshot`].
    pub caches: BTreeMap<String, u64>,
    /// Per-pane scrollback residency, sorted by pane id.
    pub panes: Vec<PaneResourceSnapshot>,
    /// [`TerminalLockLedger::global`] (ft-yccm0.1.5).
    #[serde(default)]
    pub terminal_locks: TerminalLocksSnapshot,
    /// [`DurabilityLedger::global`] (ft-yccm0.2.1.1).
    #[serde(default)]
    pub durability: DurabilitySnapshot,
    /// [`FrameLedger::global`] (ft-yccm0.1.4).
    #[serde(default)]
    pub frames: FramesSnapshot,
}

impl ResourceSnapshotBody {
    /// GPU and cache sections from `gpu` and `caches`, the process's terminal
    /// lock, durability and frame ledgers; no panes.
    #[must_use]
    pub fn from_ledgers(gpu: &GpuResourceLedger, caches: &CacheGauges) -> Self {
        Self {
            gpu: gpu.snapshot(),
            caches: caches.snapshot(),
            panes: Vec::new(),
            terminal_locks: TerminalLockLedger::global().snapshot(),
            durability: DurabilityLedger::global().snapshot(),
            frames: FrameLedger::global().snapshot(),
        }
    }

    /// Human-readable lines for the GUI debug overlay.
    #[must_use]
    pub fn summary_lines(&self) -> Vec<String> {
        let mib = |bytes: u64| bytes as f64 / (1024.0 * 1024.0);
        let texture = |purpose: GpuTexturePurpose| {
            let counter = self
                .gpu
                .textures
                .get(purpose.as_str())
                .copied()
                .unwrap_or_default();
            format!(
                "{} {} / {:.1} MiB",
                purpose.as_str(),
                counter.live_count,
                mib(counter.live_bytes)
            )
        };
        let cache = |entries: CacheGauge, bytes: CacheGauge, label: &str| {
            format!(
                "{label} {} / {:.1} MiB",
                self.caches.get(entries.as_str()).copied().unwrap_or(0),
                mib(self.caches.get(bytes.as_str()).copied().unwrap_or(0))
            )
        };
        let textures: Vec<String> = GpuTexturePurpose::ALL.iter().map(|p| texture(*p)).collect();
        vec![
            format!(
                "GPU textures {} / {:.1} MiB (peak {:.1} MiB): {}; atlas generations {}",
                self.gpu.texture_total.live_count,
                mib(self.gpu.texture_total.live_bytes),
                mib(self.gpu.texture_total.peak_live_bytes),
                textures.join(", "),
                self.gpu.atlas_generations,
            ),
            format!(
                "GPU buffers {} / {:.1} MiB (peak {:.1} MiB)",
                self.gpu.buffer_total.live_count,
                mib(self.gpu.buffer_total.live_bytes),
                mib(self.gpu.buffer_total.peak_live_bytes),
            ),
            format!(
                "Caches: {}; {}; {}; glyphs {}; images {:.1} MiB",
                cache(
                    CacheGauge::ShapeCacheEntries,
                    CacheGauge::ShapeCacheBytes,
                    "shapes"
                ),
                cache(
                    CacheGauge::LineShapeCacheEntries,
                    CacheGauge::LineShapeCacheBytes,
                    "lines"
                ),
                cache(
                    CacheGauge::LineQuadCacheEntries,
                    CacheGauge::LineQuadCacheBytes,
                    "quads"
                ),
                self.caches
                    .get(CacheGauge::GlyphCacheEntries.as_str())
                    .copied()
                    .unwrap_or(0),
                mib(self
                    .caches
                    .get(CacheGauge::ImageCacheBytes.as_str())
                    .copied()
                    .unwrap_or(0)),
            ),
        ]
    }

    /// Debug-overlay lines for the terminal-lock and durability sections
    /// (ft-yccm0.1.5, ft-yccm0.2.1.1), kept apart from [`Self::summary_lines`].
    #[must_use]
    pub fn lock_and_durability_lines(&self) -> Vec<String> {
        vec![
            self.terminal_locks.summary_line(),
            self.durability.summary_line(),
        ]
    }
}

impl TerminalLocksSnapshot {
    /// One human-readable line for the GUI debug overlay.
    #[must_use]
    pub fn summary_line(&self) -> String {
        let ms = |ns: u64| ns as f64 / 1_000_000.0;
        let holders: Vec<String> = self
            .holders
            .iter()
            .map(|(holder, stats)| {
                format!(
                    "{holder} hold p99 {:.2} ms / wait p99 {:.2} ms",
                    ms(stats.hold.p99_ns),
                    ms(stats.wait.p99_ns)
                )
            })
            .collect();
        format!(
            "Terminal locks: main thread blocked {:.1} ms over {} waits (max {:.2} ms); {}",
            ms(self.main_thread_blocked_ns),
            self.main_thread_blocked_waits,
            ms(self.main_thread_max_wait_ns),
            if holders.is_empty() {
                "no acquisitions".to_string()
            } else {
                holders.join(", ")
            }
        )
    }
}

impl DurabilitySnapshot {
    /// One human-readable line for the GUI debug overlay.
    #[must_use]
    pub fn summary_line(&self) -> String {
        format!(
            "Scrollback durability: {} degraded panes; writer failures {}, panics {}, abandoned rows {}",
            self.degraded_panes.len(),
            self.writer_failures_total,
            self.writer_panics_total,
            self.rows_abandoned_total
        )
    }
}

impl AllocatorSnapshot {
    /// One human-readable line for the GUI debug overlay.
    #[must_use]
    pub fn summary_line(&self) -> String {
        let mib = |bytes: u64| bytes as f64 / (1024.0 * 1024.0);
        let line = match (&self.stats, &self.unavailable) {
            (Some(stats), _) => format!(
                "Allocator {}: allocated {:.1} MiB, active {:.1}, resident {:.1}, mapped {:.1}, retained {:.1}",
                self.backend,
                mib(stats.allocated),
                mib(stats.active),
                mib(stats.resident),
                mib(stats.mapped),
                mib(stats.retained),
            ),
            (None, Some(reason)) => {
                format!("Allocator {}: stats unavailable ({reason})", self.backend)
            }
            (None, None) => format!("Allocator {}: stats unavailable", self.backend),
        };
        match self.options {
            Some(options) => format!(
                "{line}; background_thread {}, dirty_decay_ms {}, muzzy_decay_ms {}, narenas {}",
                options
                    .background_thread
                    .map_or_else(|| "unsupported".to_string(), |enabled| enabled.to_string()),
                options.dirty_decay_ms,
                options.muzzy_decay_ms,
                options.narenas
            ),
            None => line,
        }
    }
}

/// One process's published snapshot file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourceSnapshotEnvelope {
    /// Always [`RESOURCE_SNAPSHOT_SCHEMA`].
    pub schema: String,
    /// Publishing process id.
    pub pid: u32,
    /// Publishing process label, e.g. `frankenterm-gui`.
    pub process: String,
    /// Wall-clock publish time, milliseconds since the Unix epoch.
    pub published_unix_ms: u64,
    /// The GPU ledger at publish time.
    pub gpu: GpuResourceSnapshot,
    /// Cache gauges at publish time.
    #[serde(default)]
    pub caches: BTreeMap<String, u64>,
    /// Per-pane scrollback residency at publish time.
    #[serde(default)]
    pub panes: Vec<PaneResourceSnapshot>,
    /// Allocator statistics at publish time.
    #[serde(default)]
    pub allocator: AllocatorSnapshot,
    /// Terminal-mutex wait/hold times by holder kind (ft-yccm0.1.5).
    #[serde(default)]
    pub terminal_locks: TerminalLocksSnapshot,
    /// Durable scrollback writer health (ft-yccm0.2.1.1).
    #[serde(default)]
    pub durability: DurabilitySnapshot,
    /// Presented frames and the frame-rate cap (ft-yccm0.1.4).
    #[serde(default)]
    pub frames: FramesSnapshot,
}

impl ResourceSnapshotEnvelope {
    /// An envelope for this process, published now.
    #[must_use]
    pub fn now(process: &str, body: ResourceSnapshotBody, allocator: AllocatorSnapshot) -> Self {
        Self {
            schema: RESOURCE_SNAPSHOT_SCHEMA.to_string(),
            pid: std::process::id(),
            process: process.to_string(),
            published_unix_ms: unix_now_ms(),
            gpu: body.gpu,
            caches: body.caches,
            panes: body.panes,
            allocator,
            terminal_locks: body.terminal_locks,
            durability: body.durability,
            frames: body.frames,
        }
    }
}

/// Milliseconds since the Unix epoch (0 if the clock is before it).
#[must_use]
pub fn unix_now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| {
            u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
        })
}

/// Path of the snapshot file `pid` publishes into `dir`.
#[must_use]
pub fn resource_snapshot_path(dir: &Path, pid: u32) -> PathBuf {
    dir.join(format!(
        "{RESOURCE_SNAPSHOT_FILE_PREFIX}{pid}{RESOURCE_SNAPSHOT_FILE_SUFFIX}"
    ))
}

/// Atomically (write + rename) publish `envelope` as its pid's snapshot file
/// in `dir`, creating `dir` if needed. Returns the published path.
pub fn publish_resource_snapshot(
    dir: &Path,
    envelope: &ResourceSnapshotEnvelope,
) -> std::io::Result<PathBuf> {
    std::fs::create_dir_all(dir)?;
    let path = resource_snapshot_path(dir, envelope.pid);
    // The leading dot keeps a half-written file out of the collector's view.
    let staging = dir.join(format!(
        ".{RESOURCE_SNAPSHOT_FILE_PREFIX}{}{RESOURCE_SNAPSHOT_FILE_SUFFIX}.tmp",
        envelope.pid
    ));
    let bytes = serde_json::to_vec_pretty(envelope).map_err(std::io::Error::other)?;
    std::fs::write(&staging, bytes)?;
    std::fs::rename(&staging, &path)?;
    Ok(path)
}

/// One snapshot file read back by [`collect_resource_snapshots`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CollectedResourceSnapshot {
    /// The file the snapshot was read from.
    pub path: PathBuf,
    /// Milliseconds between publication and collection.
    pub age_ms: u64,
    /// True when the snapshot is older than the freshness window, which means
    /// its process most likely exited without removing it.
    pub stale: bool,
    /// The published content.
    pub snapshot: ResourceSnapshotEnvelope,
}

/// A snapshot file that could not be read or parsed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnreadableResourceSnapshot {
    /// The offending file.
    pub path: PathBuf,
    /// Why it was rejected.
    pub error: String,
}

/// Every snapshot published into one directory.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourceSnapshotCollection {
    /// The directory that was scanned.
    pub dir: PathBuf,
    /// Parsed snapshots, sorted by pid.
    pub snapshots: Vec<CollectedResourceSnapshot>,
    /// Files that matched the naming contract but could not be used.
    pub unreadable: Vec<UnreadableResourceSnapshot>,
}

impl ResourceSnapshotCollection {
    /// Snapshots inside the freshness window.
    pub fn fresh(&self) -> impl Iterator<Item = &CollectedResourceSnapshot> {
        self.snapshots.iter().filter(|snapshot| !snapshot.stale)
    }
}

/// Read every `<prefix><pid>.json` snapshot in `dir`. A missing directory is
/// an empty collection: no process has published yet.
pub fn collect_resource_snapshots(
    dir: &Path,
    now_unix_ms: u64,
    freshness: Duration,
) -> std::io::Result<ResourceSnapshotCollection> {
    let mut collection = ResourceSnapshotCollection {
        dir: dir.to_path_buf(),
        ..ResourceSnapshotCollection::default()
    };
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(collection),
        Err(error) => return Err(error),
    };
    let freshness_ms = u64::try_from(freshness.as_millis()).unwrap_or(u64::MAX);
    for entry in entries {
        let path = entry?.path();
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if !name.starts_with(RESOURCE_SNAPSHOT_FILE_PREFIX)
            || !name.ends_with(RESOURCE_SNAPSHOT_FILE_SUFFIX)
        {
            continue;
        }
        let parsed = std::fs::read(&path)
            .map_err(|error| error.to_string())
            .and_then(|bytes| {
                serde_json::from_slice::<ResourceSnapshotEnvelope>(&bytes)
                    .map_err(|error| error.to_string())
            })
            .and_then(|snapshot| {
                if snapshot.schema == RESOURCE_SNAPSHOT_SCHEMA {
                    Ok(snapshot)
                } else {
                    Err(format!("unsupported schema {:?}", snapshot.schema))
                }
            });
        match parsed {
            Ok(snapshot) => {
                let age_ms = now_unix_ms.saturating_sub(snapshot.published_unix_ms);
                collection.snapshots.push(CollectedResourceSnapshot {
                    path,
                    age_ms,
                    stale: age_ms > freshness_ms,
                    snapshot,
                });
            }
            Err(error) => collection
                .unreadable
                .push(UnreadableResourceSnapshot { path, error }),
        }
    }
    collection
        .snapshots
        .sort_by_key(|snapshot| snapshot.snapshot.pid);
    collection.unreadable.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(collection)
}

/// Background thread that keeps this process's snapshot file current.
///
/// It republishes whenever the ledger changes (checked every `interval`) and
/// at least every [`RESOURCE_SNAPSHOT_HEARTBEAT`]. Dropping the publisher
/// stops the thread and removes this process's own snapshot file.
#[derive(Debug)]
pub struct ResourceSnapshotPublisher {
    stop: Option<mpsc::Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
    path: PathBuf,
}

impl ResourceSnapshotPublisher {
    /// Publish this process's resources into `dir` under the `process` label.
    /// `collect` runs on the publisher thread every `interval`; the snapshot
    /// is rewritten when its result changes, with fresh allocator stats.
    pub fn spawn(
        dir: PathBuf,
        process: &str,
        interval: Duration,
        mut collect: impl FnMut() -> ResourceSnapshotBody + Send + 'static,
    ) -> std::io::Result<Self> {
        let path = resource_snapshot_path(&dir, std::process::id());
        let process = process.to_string();
        let envelope = move |body: ResourceSnapshotBody| {
            ResourceSnapshotEnvelope::now(&process, body, AllocatorSnapshot::read())
        };
        // Publish once synchronously so a startup failure is reported to the
        // caller instead of vanishing into the thread.
        let mut last = collect();
        publish_resource_snapshot(&dir, &envelope(last.clone()))?;
        let (stop, stopped) = mpsc::channel::<()>();
        let thread = std::thread::Builder::new()
            .name("resource-snapshot".to_string())
            .spawn(move || {
                let mut last_published = std::time::Instant::now();
                // Any message or a dropped sender ends the loop.
                while stopped.recv_timeout(interval) == Err(mpsc::RecvTimeoutError::Timeout) {
                    let current = collect();
                    if current == last && last_published.elapsed() < RESOURCE_SNAPSHOT_HEARTBEAT {
                        continue;
                    }
                    match publish_resource_snapshot(&dir, &envelope(current.clone())) {
                        Ok(_) => {
                            last = current;
                            last_published = std::time::Instant::now();
                        }
                        Err(error) => {
                            log::warn!(
                                "resource snapshot publish to {} failed: {error}",
                                dir.display()
                            );
                        }
                    }
                }
            })?;
        Ok(Self {
            stop: Some(stop),
            thread: Some(thread),
            path,
        })
    }

    /// The file this publisher maintains.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for ResourceSnapshotPublisher {
    fn drop(&mut self) {
        drop(self.stop.take());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        // Only ever this process's own file; a crash leaves it behind and the
        // collector then reports it as stale.
        if let Err(error) = std::fs::remove_file(&self.path)
            && error.kind() != std::io::ErrorKind::NotFound
        {
            log::warn!(
                "resource snapshot cleanup of {} failed: {error}",
                self.path.display()
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn private_ledger() -> &'static GpuResourceLedger {
        Box::leak(Box::new(GpuResourceLedger::new()))
    }

    #[test]
    fn guards_account_live_bytes_by_purpose_and_release_on_drop() {
        let ledger = private_ledger();
        let atlas = ledger.track_texture(GpuTexturePurpose::Atlas, texture_bytes(64, 64, 4));
        let image = ledger.track_texture(GpuTexturePurpose::Image, 100);
        let vertex = ledger.track_buffer(GpuBufferPurpose::Vertex, 48);
        assert_eq!(atlas.bytes(), 16_384);

        let atlas_counter = ledger.texture_counter(GpuTexturePurpose::Atlas);
        assert_eq!(
            (atlas_counter.live_count, atlas_counter.live_bytes),
            (1, 16_384)
        );
        let snapshot = ledger.snapshot();
        assert_eq!(snapshot.texture_total.live_bytes, 16_484);
        assert_eq!(snapshot.texture_total.live_count, 2);
        assert_eq!(snapshot.buffers["vertex"].live_bytes, 48);
        assert_eq!(snapshot.buffer_total.live_count, 1);
        assert_eq!(
            snapshot.textures["drawable"],
            ResourceCounterSnapshot::default()
        );

        drop(atlas);
        let atlas_counter = ledger.texture_counter(GpuTexturePurpose::Atlas);
        assert_eq!(
            atlas_counter,
            ResourceCounterSnapshot {
                live_count: 0,
                live_bytes: 0,
                created_total: 1,
                released_total: 1,
                peak_live_bytes: 16_384,
            }
        );
        drop((image, vertex));
        let snapshot = ledger.snapshot();
        assert_eq!(snapshot.texture_total.live_bytes, 0);
        assert_eq!(snapshot.texture_total.peak_live_bytes, 16_484);
        assert_eq!(snapshot.buffer_total.live_bytes, 0);
        assert_eq!(snapshot.buffer_total.released_total, 1);
    }

    #[test]
    fn peak_tracks_the_high_water_mark_not_the_last_value() {
        let ledger = private_ledger();
        let first = ledger.track_texture(GpuTexturePurpose::Atlas, 10);
        let second = ledger.track_texture(GpuTexturePurpose::Atlas, 30);
        drop(first);
        drop(second);
        let third = ledger.track_texture(GpuTexturePurpose::Atlas, 5);
        let counter = ledger.texture_counter(GpuTexturePurpose::Atlas);
        assert_eq!(counter.live_bytes, 5);
        assert_eq!(counter.peak_live_bytes, 40);
        assert_eq!(
            counter.created_total - counter.released_total,
            counter.live_count
        );
        drop(third);
    }

    #[test]
    fn snapshot_lists_every_purpose_and_counts_atlas_generations() {
        let ledger = private_ledger();
        ledger.record_atlas_generation();
        ledger.record_atlas_generation();
        let snapshot = ledger.snapshot();
        assert_eq!(snapshot.atlas_generations, 2);
        let texture_keys: Vec<_> = snapshot.textures.keys().map(String::as_str).collect();
        assert_eq!(texture_keys, ["atlas", "drawable", "image", "other"]);
        let buffer_keys: Vec<_> = snapshot.buffers.keys().map(String::as_str).collect();
        assert_eq!(
            buffer_keys,
            ["index", "other", "readback", "uniform", "vertex"]
        );
    }

    #[test]
    fn publish_then_collect_round_trips_and_classifies_staleness() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = private_ledger();
        let _atlas = ledger.track_texture(GpuTexturePurpose::Atlas, 4096);
        let fresh = ResourceSnapshotEnvelope {
            pid: 41,
            published_unix_ms: 1_000_000,
            ..ResourceSnapshotEnvelope::now(
                "frankenterm-gui",
                ResourceSnapshotBody::from_ledgers(ledger, private_gauges()),
                AllocatorSnapshot::default(),
            )
        };
        let old = ResourceSnapshotEnvelope {
            pid: 7,
            published_unix_ms: 1_000_000 - 120_000,
            ..fresh.clone()
        };
        let path = publish_resource_snapshot(dir.path(), &fresh).unwrap();
        assert_eq!(path, resource_snapshot_path(dir.path(), 41));
        publish_resource_snapshot(dir.path(), &old).unwrap();
        // Neither a foreign file nor a staging file is part of the contract.
        std::fs::write(dir.path().join("unrelated.json"), b"{}").unwrap();
        std::fs::write(
            dir.path().join(".frankenterm-resources-9.json.tmp"),
            b"partial",
        )
        .unwrap();

        let collection =
            collect_resource_snapshots(dir.path(), 1_000_500, RESOURCE_SNAPSHOT_FRESHNESS).unwrap();
        assert!(
            collection.unreadable.is_empty(),
            "{:?}",
            collection.unreadable
        );
        let pids: Vec<_> = collection
            .snapshots
            .iter()
            .map(|collected| (collected.snapshot.pid, collected.stale, collected.age_ms))
            .collect();
        assert_eq!(pids, [(7, true, 120_500), (41, false, 500)]);
        assert_eq!(collection.snapshots[1].snapshot, fresh);
        assert_eq!(collection.fresh().count(), 1);
    }

    #[test]
    fn collect_reports_corrupt_and_foreign_schema_files_without_failing() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("frankenterm-resources-1.json"), b"not json").unwrap();
        let foreign = ResourceSnapshotEnvelope {
            schema: "frankenterm.resource_snapshot.v0".to_string(),
            pid: 2,
            ..ResourceSnapshotEnvelope::now(
                "x",
                ResourceSnapshotBody::default(),
                AllocatorSnapshot::default(),
            )
        };
        std::fs::write(
            dir.path().join("frankenterm-resources-2.json"),
            serde_json::to_vec(&foreign).unwrap(),
        )
        .unwrap();
        let collection =
            collect_resource_snapshots(dir.path(), 0, RESOURCE_SNAPSHOT_FRESHNESS).unwrap();
        assert!(collection.snapshots.is_empty());
        assert_eq!(collection.unreadable.len(), 2);
        assert!(
            collection.unreadable[1]
                .error
                .contains("unsupported schema")
        );
    }

    #[test]
    fn missing_directory_is_an_empty_collection() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("never-created");
        let collection =
            collect_resource_snapshots(&missing, 0, RESOURCE_SNAPSHOT_FRESHNESS).unwrap();
        assert_eq!(collection.dir, missing);
        assert!(collection.snapshots.is_empty() && collection.unreadable.is_empty());
    }

    #[test]
    fn publisher_writes_its_pid_file_republishes_changes_and_removes_it_on_drop() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = private_ledger();
        let gauges = private_gauges();
        let pane_rows = std::sync::Arc::new(AtomicU64::new(24));
        let collected_rows = std::sync::Arc::clone(&pane_rows);
        let publisher = ResourceSnapshotPublisher::spawn(
            dir.path().to_path_buf(),
            "test-process",
            Duration::from_millis(10),
            move || ResourceSnapshotBody {
                panes: vec![PaneResourceSnapshot {
                    pane_id: 3,
                    hot_rows: collected_rows.load(Ordering::Relaxed),
                    ..PaneResourceSnapshot::default()
                }],
                ..ResourceSnapshotBody::from_ledgers(ledger, gauges)
            },
        )
        .unwrap();
        let path = publisher.path().to_path_buf();
        assert_eq!(path, resource_snapshot_path(dir.path(), std::process::id()));
        let read = || -> ResourceSnapshotEnvelope {
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap()
        };
        let first = read();
        assert_eq!(first.process, "test-process");
        assert_eq!(first.gpu.texture_total.live_count, 0);
        assert_eq!(first.panes[0].hot_rows, 24);
        assert_eq!(first.caches["shape_cache_entries"], 0);
        assert_eq!(first.allocator.backend, crate::allocator_backend().as_str());
        assert_eq!(
            first.allocator.stats.is_some(),
            crate::jemalloc_enabled(),
            "allocator stats are present exactly when jemalloc is compiled in"
        );

        let _atlas = ledger.track_texture(GpuTexturePurpose::Atlas, 256);
        let mut reporter = gauges.contribution();
        reporter.set(CacheGauge::ShapeCacheEntries, 12);
        pane_rows.store(80, Ordering::Relaxed);
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            let current = read();
            if current.gpu.textures["atlas"].live_bytes == 256
                && current.caches["shape_cache_entries"] == 12
                && current.panes[0].hot_rows == 80
            {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "publisher never republished the changed resources"
            );
            std::thread::sleep(Duration::from_millis(5));
        }

        drop(publisher);
        assert!(
            !path.exists(),
            "publisher must remove its own snapshot file"
        );
    }

    fn private_gauges() -> &'static CacheGauges {
        Box::leak(Box::new(CacheGauges::new()))
    }

    #[test]
    fn cache_gauge_contributions_sum_across_reporters_and_withdraw_on_drop() {
        let gauges = private_gauges();
        let mut first = gauges.contribution();
        let mut second = gauges.contribution();
        first.set(CacheGauge::ShapeCacheEntries, 100);
        second.set(CacheGauge::ShapeCacheEntries, 40);
        second.set(CacheGauge::ImageCacheBytes, 4096);
        assert_eq!(gauges.value(CacheGauge::ShapeCacheEntries), 140);

        // A shrinking report moves the sum down by the delta only.
        first.set(CacheGauge::ShapeCacheEntries, 30);
        assert_eq!(gauges.value(CacheGauge::ShapeCacheEntries), 70);

        drop(second);
        assert_eq!(gauges.value(CacheGauge::ShapeCacheEntries), 30);
        assert_eq!(gauges.value(CacheGauge::ImageCacheBytes), 0);
        let snapshot = gauges.snapshot();
        assert_eq!(snapshot.len(), CacheGauge::ALL.len());
        assert_eq!(snapshot["shape_cache_entries"], 30);
        drop(first);
        assert!(gauges.snapshot().values().all(|value| *value == 0));
    }

    #[test]
    fn summary_lines_report_live_counters_by_purpose() {
        let ledger = private_ledger();
        let gauges = private_gauges();
        let _atlas = ledger.track_texture(GpuTexturePurpose::Atlas, 2 * 1024 * 1024);
        let _drawable = ledger.track_texture(GpuTexturePurpose::Drawable, 1024 * 1024);
        let _vertex = ledger.track_buffer(GpuBufferPurpose::Vertex, 512 * 1024);
        ledger.record_atlas_generation();
        let mut reporter = gauges.contribution();
        reporter.set(CacheGauge::ShapeCacheEntries, 7);
        reporter.set(CacheGauge::ShapeCacheBytes, 3 * 1024 * 1024);
        reporter.set(CacheGauge::GlyphCacheEntries, 99);

        let lines = ResourceSnapshotBody::from_ledgers(ledger, gauges).summary_lines();
        assert_eq!(lines.len(), 3);
        assert!(
            lines[0].starts_with(
                "GPU textures 2 / 3.0 MiB (peak 3.0 MiB): atlas 1 / 2.0 MiB, drawable 1 / 1.0 MiB"
            ),
            "{}",
            lines[0]
        );
        assert!(lines[0].ends_with("atlas generations 1"), "{}", lines[0]);
        assert_eq!(lines[1], "GPU buffers 1 / 0.5 MiB (peak 0.5 MiB)");
        assert!(lines[2].contains("shapes 7 / 3.0 MiB"), "{}", lines[2]);
        assert!(lines[2].contains("glyphs 99"), "{}", lines[2]);

        let allocator = AllocatorSnapshot {
            backend: "jemalloc".to_string(),
            stats: Some(AllocatorStatsSnapshot {
                allocated: 1024 * 1024,
                ..AllocatorStatsSnapshot::default()
            }),
            unavailable: None,
            options: Some(crate::JemallocOptions {
                background_thread: Some(true),
                dirty_decay_ms: 5000,
                muzzy_decay_ms: 0,
                narenas: 8,
            }),
        };
        assert!(allocator.summary_line().contains("allocated 1.0 MiB"));
        assert!(
            allocator.summary_line().ends_with(
                "background_thread true, dirty_decay_ms 5000, muzzy_decay_ms 0, narenas 8"
            ),
            "{}",
            allocator.summary_line()
        );
        // macOS jemalloc has no background threads to report.
        let apple = AllocatorSnapshot {
            options: Some(crate::JemallocOptions {
                background_thread: None,
                dirty_decay_ms: 5000,
                muzzy_decay_ms: 0,
                narenas: 56,
            }),
            ..allocator.clone()
        };
        assert!(
            apple.summary_line().ends_with(
                "background_thread unsupported, dirty_decay_ms 5000, muzzy_decay_ms 0, narenas 56"
            ),
            "{}",
            apple.summary_line()
        );
        assert!(
            AllocatorSnapshot::read()
                .summary_line()
                .starts_with(&format!(
                    "Allocator {}",
                    crate::allocator_backend().as_str()
                ))
        );
    }

    #[test]
    fn envelope_from_an_older_publisher_without_new_sections_still_parses() {
        // A v1 file written before the cache/pane/allocator sections existed.
        let legacy = serde_json::json!({
            "schema": RESOURCE_SNAPSHOT_SCHEMA,
            "pid": 5,
            "process": "frankenterm-gui",
            "published_unix_ms": 9,
            "gpu": GpuResourceSnapshot::default(),
        });
        let parsed: ResourceSnapshotEnvelope = serde_json::from_value(legacy).unwrap();
        assert!(parsed.caches.is_empty() && parsed.panes.is_empty());
        assert_eq!(parsed.allocator, AllocatorSnapshot::default());
    }

    #[test]
    fn latency_buckets_tile_the_value_range_with_bounded_error() {
        let mut previous_upper = None;
        for index in 0..LATENCY_BUCKETS {
            let upper = LatencyHistogram::bucket_upper_bound(index);
            assert_eq!(LatencyHistogram::bucket_index(upper), index);
            if let Some(previous) = previous_upper {
                // Contiguous: each bucket starts right after the last one.
                assert_eq!(LatencyHistogram::bucket_index(previous + 1), index);
                // Two mantissa bits: a bucket is at most a quarter of its lower bound wide.
                let lower = previous + 1;
                assert!(
                    upper - lower <= lower / 4,
                    "bucket {index}: {lower}..={upper}"
                );
            }
            previous_upper = Some(upper);
        }
        assert_eq!(previous_upper, Some(u64::MAX));
        assert_eq!(LatencyHistogram::bucket_index(0), 0);
        assert_eq!(LatencyHistogram::bucket_index(3), 3);
        assert_eq!(
            LatencyHistogram::bucket_index(u64::MAX),
            LATENCY_BUCKETS - 1
        );
    }

    #[test]
    fn latency_quantiles_are_conservative_bucket_bounds_capped_at_the_maximum() {
        let histogram = LatencyHistogram::new();
        assert_eq!(histogram.snapshot(), LatencySnapshot::default());
        for value in 1..=100u64 {
            histogram.record(value * 1_000);
        }
        let snapshot = histogram.snapshot();
        assert_eq!(snapshot.count, 100);
        assert_eq!(snapshot.total_ns, 5_050_000);
        assert_eq!(snapshot.max_ns, 100_000);
        for (reported, exact) in [
            (snapshot.p50_ns, 50_000),
            (snapshot.p95_ns, 95_000),
            (snapshot.p99_ns, 99_000),
        ] {
            assert!(
                reported >= exact && reported <= exact + exact / 4,
                "{reported} vs {exact}"
            );
        }
        // A single outlier owns the top quantiles but never exceeds the max.
        // 4 ns sits in an exact (width one) bucket.
        let spike = LatencyHistogram::new();
        for _ in 0..99 {
            spike.record(4);
        }
        spike.record(7_000_000);
        let snapshot = spike.snapshot();
        assert_eq!(snapshot.p50_ns, 4);
        assert_eq!(snapshot.p95_ns, 4);
        assert_eq!(snapshot.p99_ns, 4);
        assert_eq!(snapshot.max_ns, 7_000_000);
        spike.record(7_000_000);
        assert_eq!(spike.snapshot().p99_ns, 7_000_000);
    }

    #[test]
    fn terminal_lock_ledger_tags_holders_and_counts_main_thread_blocking() {
        let ledger: &'static TerminalLockLedger = Box::leak(Box::new(TerminalLockLedger::new()));
        assert_eq!(ledger.snapshot(), TerminalLocksSnapshot::default());
        ledger.record_wait(TerminalLockHolder::Parser, 0, false);
        ledger.record_hold(TerminalLockHolder::Parser, 2_000_000);
        ledger.record_wait(TerminalLockHolder::Paint, 1_500_000, true);
        ledger.record_hold(TerminalLockHolder::Paint, 40_000);
        ledger.record_wait(TerminalLockHolder::Mouse, 0, true);
        ledger.record_wait(TerminalLockHolder::Resize, 300, false);

        let snapshot = ledger.snapshot();
        assert_eq!(
            snapshot.holders.keys().collect::<Vec<_>>(),
            ["mouse", "paint", "parser", "resize"],
            "only holders that acquired the lock are reported"
        );
        assert_eq!(snapshot.holders["parser"].hold.max_ns, 2_000_000);
        assert_eq!(snapshot.holders["parser"].wait.count, 1);
        assert_eq!(snapshot.holders["paint"].wait.max_ns, 1_500_000);
        assert_eq!(snapshot.holders["resize"].hold.count, 0);
        // An uncontended main-thread acquisition is not blocking.
        assert_eq!(snapshot.main_thread_blocked_waits, 1);
        assert_eq!(snapshot.main_thread_blocked_ns, 1_500_000);
        assert_eq!(snapshot.main_thread_max_wait_ns, 1_500_000);

        let body = ResourceSnapshotBody {
            terminal_locks: snapshot,
            ..ResourceSnapshotBody::default()
        };
        let lines = body.lock_and_durability_lines();
        assert!(
            lines[0].starts_with("Terminal locks: main thread blocked 1.5 ms over 1 waits"),
            "{}",
            lines[0]
        );
        assert!(lines[0].contains("parser hold p99 2.00 ms"), "{}", lines[0]);
    }

    #[test]
    fn durability_ledger_tracks_degraded_panes_until_recovery_or_abandonment() {
        let ledger: &'static DurabilityLedger = Box::leak(Box::new(DurabilityLedger::new()));
        ledger.record_failure("pane-a", "storage unavailable", false);
        ledger.record_failure("pane-b", "storage unavailable", true);
        ledger.record_failure("pane-a", "commit outcome indeterminate", false);
        let snapshot = ledger.snapshot();
        assert_eq!(snapshot.writer_failures_total, 3);
        assert_eq!(snapshot.writer_panics_total, 1);
        assert_eq!(
            snapshot.degraded_panes["pane-a"], "commit outcome indeterminate",
            "the latest failure is reported"
        );
        ledger.record_recovered("pane-a");
        ledger.record_abandoned("pane-b", 12);
        let snapshot = ledger.snapshot();
        assert!(snapshot.degraded_panes.is_empty());
        assert_eq!(snapshot.rows_abandoned_total, 12);
        assert_eq!(
            snapshot.summary_line(),
            "Scrollback durability: 0 degraded panes; writer failures 3, panics 1, abandoned rows 12"
        );
    }

    #[test]
    fn lock_and_durability_sections_round_trip_and_default_when_absent() {
        let envelope = ResourceSnapshotEnvelope {
            terminal_locks: TerminalLocksSnapshot {
                main_thread_blocked_ns: 9,
                ..TerminalLocksSnapshot::default()
            },
            durability: DurabilitySnapshot {
                writer_failures_total: 2,
                ..DurabilitySnapshot::default()
            },
            ..ResourceSnapshotEnvelope::now(
                "x",
                ResourceSnapshotBody::default(),
                AllocatorSnapshot::default(),
            )
        };
        let value = serde_json::to_value(&envelope).unwrap();
        assert_eq!(value["terminal_locks"]["main_thread_blocked_ns"], 9);
        assert_eq!(value["durability"]["writer_failures_total"], 2);
        let parsed: ResourceSnapshotEnvelope = serde_json::from_value(value.clone()).unwrap();
        assert_eq!(parsed, envelope);
        // Files written before these sections existed still parse.
        let mut legacy = value;
        let object = legacy.as_object_mut().unwrap();
        object.remove("terminal_locks");
        object.remove("durability");
        let parsed: ResourceSnapshotEnvelope = serde_json::from_value(legacy).unwrap();
        assert_eq!(parsed.terminal_locks, TerminalLocksSnapshot::default());
        assert_eq!(parsed.durability, DurabilitySnapshot::default());
    }

    #[test]
    fn frame_ledger_counts_presents_failures_and_present_intervals() {
        let ledger: &'static FrameLedger = Box::leak(Box::new(FrameLedger::new()));
        assert_eq!(ledger.snapshot(), FramesSnapshot::default());

        // Three presents one 60 Hz period apart, then one after a 250 ms gap.
        ledger.record_present_at(5_000_000);
        ledger.record_present_at(21_666_667);
        ledger.record_present_at(38_333_334);
        ledger.record_present_at(288_333_334);
        ledger.record_present_failure();
        ledger.set_max_fps(120);
        let snapshot = ledger.snapshot();
        assert_eq!(snapshot.presented_total, 4);
        assert_eq!(snapshot.present_failures_total, 1);
        assert_eq!(snapshot.max_fps, 120);
        // The first present has no predecessor, so three intervals.
        assert_eq!(snapshot.present_interval.count, 3);
        assert_eq!(snapshot.present_interval.max_ns, 250_000_000);
        assert_eq!(
            snapshot.present_interval.total_ns,
            288_333_334 - 5_000_000,
            "intervals tile the span from the first to the last present"
        );
        assert!(
            (16_666_667..=16_666_667 * 5 / 4).contains(&snapshot.present_interval.p50_ns),
            "{:?}",
            snapshot.present_interval
        );
        assert_eq!(
            snapshot.summary_line(),
            format!(
                "Frames: presented 4 (failed 1); present interval p50 {:.2} ms, p95 250.00 ms, \
                 max 250.00 ms; max_fps 120",
                snapshot.present_interval.p50_ns as f64 / 1_000_000.0
            )
        );

        // The global ledger records against its own monotonic clock.
        let global = FrameLedger::global();
        let before = global.snapshot().presented_total;
        global.record_present();
        global.record_present();
        assert!(global.snapshot().presented_total >= before + 2);
    }

    #[test]
    fn frames_section_round_trips_and_defaults_when_absent() {
        let envelope = ResourceSnapshotEnvelope {
            frames: FramesSnapshot {
                presented_total: 7_200,
                max_fps: 120,
                ..FramesSnapshot::default()
            },
            ..ResourceSnapshotEnvelope::now(
                "x",
                ResourceSnapshotBody::default(),
                AllocatorSnapshot::default(),
            )
        };
        let value = serde_json::to_value(&envelope).unwrap();
        assert_eq!(value["frames"]["presented_total"], 7_200);
        assert_eq!(value["frames"]["max_fps"], 120);
        assert_eq!(value["frames"]["present_interval"]["count"], 0);
        let parsed: ResourceSnapshotEnvelope = serde_json::from_value(value.clone()).unwrap();
        assert_eq!(parsed, envelope);
        // Files written before the section existed still parse.
        let mut legacy = value;
        legacy.as_object_mut().unwrap().remove("frames");
        let parsed: ResourceSnapshotEnvelope = serde_json::from_value(legacy).unwrap();
        assert_eq!(parsed.frames, FramesSnapshot::default());
        // from_ledgers carries the global frame section into the body (other
        // tests may record into the global ledger concurrently).
        FrameLedger::global().record_present();
        let body = ResourceSnapshotBody::from_ledgers(private_ledger(), private_gauges());
        assert!(body.frames.presented_total >= 1);
        assert!(body.frames.presented_total <= FrameLedger::global().snapshot().presented_total);
    }

    #[test]
    fn snapshot_interval_override_accepts_only_milliseconds_in_range() {
        let default = Duration::from_secs(2);
        assert_eq!(resource_snapshot_interval_from(None, default), default);
        assert_eq!(
            resource_snapshot_interval_from(Some("500"), default),
            Duration::from_millis(500)
        );
        assert_eq!(
            resource_snapshot_interval_from(Some(" 250\n"), default),
            Duration::from_millis(250)
        );
        assert_eq!(
            resource_snapshot_interval_from(Some("50"), default),
            Duration::from_millis(50)
        );
        assert_eq!(
            resource_snapshot_interval_from(Some("60000"), default),
            Duration::from_secs(60)
        );
        for rejected in ["49", "0", "60001", "-5", "1.5", "fast", ""] {
            assert_eq!(
                resource_snapshot_interval_from(Some(rejected), default),
                default,
                "{rejected:?}"
            );
        }
    }
}
