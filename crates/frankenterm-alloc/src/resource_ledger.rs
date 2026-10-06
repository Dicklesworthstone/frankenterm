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
    pub fn track_texture(&'static self, purpose: GpuTexturePurpose, bytes: u64) -> GpuResourceGuard {
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
            },
            Err(error) => Self {
                backend,
                stats: None,
                unavailable: Some(error.to_string()),
            },
        }
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
}

impl ResourceSnapshotBody {
    /// GPU and cache sections from `gpu` and `caches`; no panes.
    #[must_use]
    pub fn from_ledgers(gpu: &GpuResourceLedger, caches: &CacheGauges) -> Self {
        Self {
            gpu: gpu.snapshot(),
            caches: caches.snapshot(),
            panes: Vec::new(),
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
                cache(CacheGauge::ShapeCacheEntries, CacheGauge::ShapeCacheBytes, "shapes"),
                cache(
                    CacheGauge::LineShapeCacheEntries,
                    CacheGauge::LineShapeCacheBytes,
                    "lines"
                ),
                cache(CacheGauge::LineQuadCacheEntries, CacheGauge::LineQuadCacheBytes, "quads"),
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
}

impl AllocatorSnapshot {
    /// One human-readable line for the GUI debug overlay.
    #[must_use]
    pub fn summary_line(&self) -> String {
        let mib = |bytes: u64| bytes as f64 / (1024.0 * 1024.0);
        match (&self.stats, &self.unavailable) {
            (Some(stats), _) => format!(
                "Allocator {}: allocated {:.1} MiB, active {:.1}, resident {:.1}, mapped {:.1}, retained {:.1}",
                self.backend,
                mib(stats.allocated),
                mib(stats.active),
                mib(stats.resident),
                mib(stats.mapped),
                mib(stats.retained),
            ),
            (None, Some(reason)) => format!("Allocator {}: stats unavailable ({reason})", self.backend),
            (None, None) => format!("Allocator {}: stats unavailable", self.backend),
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
        }
    }
}

/// Milliseconds since the Unix epoch (0 if the clock is before it).
#[must_use]
pub fn unix_now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX))
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
    collection.snapshots.sort_by_key(|snapshot| snapshot.snapshot.pid);
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
        assert_eq!((atlas_counter.live_count, atlas_counter.live_bytes), (1, 16_384));
        let snapshot = ledger.snapshot();
        assert_eq!(snapshot.texture_total.live_bytes, 16_484);
        assert_eq!(snapshot.texture_total.live_count, 2);
        assert_eq!(snapshot.buffers["vertex"].live_bytes, 48);
        assert_eq!(snapshot.buffer_total.live_count, 1);
        assert_eq!(snapshot.textures["drawable"], ResourceCounterSnapshot::default());

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
        assert_eq!(counter.created_total - counter.released_total, counter.live_count);
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
            collect_resource_snapshots(dir.path(), 1_000_500, RESOURCE_SNAPSHOT_FRESHNESS)
                .unwrap();
        assert!(collection.unreadable.is_empty(), "{:?}", collection.unreadable);
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
        let collection = collect_resource_snapshots(dir.path(), 0, RESOURCE_SNAPSHOT_FRESHNESS)
            .unwrap();
        assert!(collection.snapshots.is_empty());
        assert_eq!(collection.unreadable.len(), 2);
        assert!(collection.unreadable[1].error.contains("unsupported schema"));
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
        assert!(!path.exists(), "publisher must remove its own snapshot file");
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
            lines[0].starts_with("GPU textures 2 / 3.0 MiB (peak 3.0 MiB): atlas 1 / 2.0 MiB, drawable 1 / 1.0 MiB"),
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
        };
        assert!(allocator.summary_line().contains("allocated 1.0 MiB"));
        assert!(
            AllocatorSnapshot::read()
                .summary_line()
                .starts_with(&format!("Allocator {}", crate::allocator_backend().as_str()))
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
}
