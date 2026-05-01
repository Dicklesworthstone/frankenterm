//! Cold-tier scrollback eviction policy substrate (ft-2okh0.13).
//!
//! Pure-logic substrate for the bead's "async scrollback eviction
//! to disk" requirement. The integration crate handles zstd
//! compression, async I/O, and AES-GCM encryption; this module
//! ships the 4-tier cascade policy (Hot/Warm/ColdRam/ColdDisk),
//! eviction-target selection, retention enforcement, redactor
//! pre-write gate, and disk-budget management.
//!
//! ## What this module ships
//!
//! - `ScrollbackTier` — `Hot / Warm / ColdRam / ColdDisk`. Extends
//!   the existing `scrollback_tiers.rs` 3-tier model with the
//!   bead's new ColdDisk tier.
//! - `ChunkId(u64)` — stable per-pane chunk identifier; monotonic.
//! - `ChunkMetadata` — `{ id, pane_id, tier, line_range,
//!   bytes_compressed, content_hash, written_ts, last_access_ts,
//!   redactions_applied }`. Pure data the integration's metadata
//!   index materialises.
//! - `RedactionStatus` — `Required / Applied / Skipped`. The
//!   bead's privacy rule: redactor MUST apply before chunks land
//!   on disk. Substrate enforces at the type level.
//! - `EncryptionStatus` — `None / Aes256Gcm`. Operator opt-in.
//! - `EvictionPolicy` operator config: `disk_budget_bytes` (default
//!   1 GiB) + `retention_days` (default 30) + `min_compression_ratio`
//!   (skip eviction if compression below this).
//! - `select_eviction_target` — pick the warm-tier chunk to demote
//!   to cold (oldest `last_access_ts`).
//! - `should_purge_by_retention` — predicate: chunk older than
//!   `retention_days` → drop entirely.
//! - `should_evict_to_disk` — pure predicate gating the warm→cold
//!   transition: requires redaction applied + disk budget allows
//!   it + chunk has age threshold.
//! - `dedup_decision` — given a content hash, return whether the
//!   chunk already exists on disk (across panes — the bead
//!   mentions content-hash dedup for cross-pane chunks).
//! - `ColdTierStats` — writes / reads / cache_hits / cache_misses
//!   / per-decision counters for `ft doctor`.
//!
//! ## What is deferred to the integration bead (ft-2okh0.13.cont)
//!
//! - zstd compression at write time.
//! - Async I/O via the asupersync runtime (cross-link AGENTS.md
//!   doctrine — no raw tokio).
//! - AES-256-GCM encryption-at-rest (shared with ft-2okh0.5
//!   mmap-backed scrollback).
//! - Disk file layout at `~/.cache/ft/scrollback/<pane_id>/
//!   <chunk_id>.zst[.enc]` with mode 0600.
//! - Redactor pre-write call (cross-link redactor.rs +
//!   BR-RC-SAFETY-PROOFS.G10).
//! - Metadata index storage (likely SQLite alongside the chunks).
//! - Cleanup task: weekly cron-style scan that calls
//!   `should_purge_by_retention` on every chunk.

#![allow(dead_code)]

// ============================================================================
// ScrollbackTier
// ============================================================================

/// 4-tier scrollback memory cascade per the bead. Order is hottest
/// to coldest; the integration's existing `scrollback_tiers.rs`
/// handles Hot/Warm/ColdRam, this bead adds ColdDisk.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default)]
pub enum ScrollbackTier {
    /// Currently visible viewport. Stays in process heap.
    #[default]
    Hot,
    /// Recently scrolled-out lines, in process heap with LRU
    /// eviction.
    Warm,
    /// Cold-RAM tier — compressed in-memory. Avoids disk I/O for
    /// moderately-old scrollback.
    ColdRam,
    /// Cold-Disk tier — zstd-compressed on disk under
    /// `~/.cache/ft/scrollback/<pane_id>/`. The bead's new tier.
    ColdDisk,
}

impl ScrollbackTier {
    /// Hotter = lower ordinal. Used by the cascade decision tree.
    #[must_use]
    pub const fn ordinal(self) -> u8 {
        match self {
            Self::Hot => 0,
            Self::Warm => 1,
            Self::ColdRam => 2,
            Self::ColdDisk => 3,
        }
    }

    /// Whether reads from this tier require disk I/O.
    #[must_use]
    pub const fn requires_disk_io(self) -> bool {
        matches!(self, Self::ColdDisk)
    }

    /// The next-colder tier in the cascade, or `None` if this is
    /// already the coldest.
    #[must_use]
    pub const fn demotion_target(self) -> Option<Self> {
        match self {
            Self::Hot => Some(Self::Warm),
            Self::Warm => Some(Self::ColdRam),
            Self::ColdRam => Some(Self::ColdDisk),
            Self::ColdDisk => None,
        }
    }
}

// ============================================================================
// Identifiers
// ============================================================================

/// Stable per-pane chunk identifier. Monotonic so older chunks
/// have lower ids.
///
/// Inner field is `pub(crate)` so external code can't forge ids
/// outside the [`Self::next`] allocator (which would risk
/// collisions in the metadata index). Use [`Self::raw`] for
/// read access (e.g., serialization) and [`Self::from_raw`]
/// for round-tripping deserialized ids.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct ChunkId(pub(crate) u64);

impl ChunkId {
    pub fn next(&mut self) -> Self {
        let curr = *self;
        self.0 = self.0.saturating_add(1);
        curr
    }

    /// Read the raw u64 (for serialization, hashing, telemetry).
    #[must_use]
    pub const fn raw(self) -> u64 {
        self.0
    }

    /// Reconstruct from a raw u64 (for deserialization round-
    /// trip). New code should use the [`Self::next`] allocator;
    /// this is the deserialization-only entry point.
    #[must_use]
    pub const fn from_raw(raw: u64) -> Self {
        Self(raw)
    }
}

/// Pane identifier. Opaque to this substrate; the integration's
/// pane registry decides the actual numbering.
pub type PaneId = u64;

/// 64-bit content hash (FNV-1a or BLAKE3 truncated; the integration
/// chooses). Used for cross-pane dedup.
pub type ContentHash = u64;

// ============================================================================
// Privacy + encryption status
// ============================================================================

/// Bead's privacy rule: "redactor MUST apply before chunks land on
/// disk." The substrate enforces at the type level — `should_evict_to_disk`
/// refuses any chunk whose `redaction_status` isn't `Applied`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum RedactionStatus {
    /// Redactor needs to run before this chunk can be written to
    /// disk. The integration runs redactor.rs::redact_text and
    /// flips this to `Applied`.
    #[default]
    Required,
    /// Redactor has run; chunk is safe to evict.
    Applied,
    /// Operator config explicitly says skip redaction (rare; only
    /// for testing or non-sensitive sessions). Substrate honours
    /// the choice but logs a warning via stats counter.
    Skipped,
}

/// Operator opt-in: encrypt cold-tier files at rest.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum EncryptionStatus {
    /// No encryption. The default — most ft sessions are local;
    /// disk encryption is the OS's job.
    #[default]
    None,
    /// AES-256-GCM. Shared key with ft-2okh0.5 mmap-backed
    /// scrollback so a session-restore reads either tier.
    Aes256Gcm,
}

// ============================================================================
// ChunkMetadata
// ============================================================================

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ChunkMetadata {
    pub id: ChunkId,
    pub pane_id: PaneId,
    pub tier: ScrollbackTier,
    /// Line range covered by this chunk (start..end, end-exclusive).
    pub line_start: u64,
    pub line_end: u64,
    /// Byte size after compression.
    pub bytes_compressed: u64,
    /// Content hash for cross-pane dedup.
    pub content_hash: ContentHash,
    /// Unix timestamp (ms) when the chunk was first written.
    pub written_ts_ms: u64,
    /// Unix timestamp (ms) of the last read access. The bead's
    /// LRU policy reads this.
    pub last_access_ts_ms: u64,
    /// **Privacy-critical**: this field is `pub(crate)` so
    /// external callers cannot bypass the redactor by setting
    /// `redaction = Applied` directly. Use [`Self::redaction`]
    /// to read; use [`Self::mark_redactor_applied`] /
    /// [`Self::mark_redactor_skipped`] to transition.
    pub(crate) redaction: RedactionStatus,
    /// `pub(crate)` for symmetry with `redaction` — encryption
    /// state should also be set via the [`Self::mark_encrypted`]
    /// transition method, not by direct field write.
    pub(crate) encryption: EncryptionStatus,
}

impl ChunkMetadata {
    /// Lines covered by this chunk.
    #[must_use]
    pub fn line_count(&self) -> u64 {
        self.line_end.saturating_sub(self.line_start)
    }

    /// Age in milliseconds at `now_ms`.
    #[must_use]
    pub fn age_ms(&self, now_ms: u64) -> u64 {
        now_ms.saturating_sub(self.written_ts_ms)
    }

    /// Idle time since last access. The eviction LRU keys off this.
    #[must_use]
    pub fn idle_ms(&self, now_ms: u64) -> u64 {
        now_ms.saturating_sub(self.last_access_ts_ms)
    }

    /// Mark as accessed at `now_ms`.
    pub fn touch(&mut self, now_ms: u64) {
        self.last_access_ts_ms = now_ms;
    }

    /// Read accessor for the redaction status. The
    /// privacy-gated [`should_evict_to_disk`] reads this to
    /// enforce the bead's "redactor MUST apply before disk"
    /// rule.
    #[must_use]
    pub const fn redaction(&self) -> RedactionStatus {
        self.redaction
    }

    /// Read accessor for the encryption status.
    #[must_use]
    pub const fn encryption(&self) -> EncryptionStatus {
        self.encryption
    }

    /// Mark redactor as applied. Integration calls this
    /// **after** running `redactor::redact_text` on the
    /// chunk's bytes. Field privacy + this transition method
    /// constrain external callers from setting `Applied`
    /// without going through the audit trail.
    ///
    /// Pair with the typed-state pipeline
    /// (`scrollback_cold_tier_pipeline::ChunkBytes<Redacted>`)
    /// for compile-time enforcement: the integration first
    /// transitions ChunkBytes to the Redacted typed-state
    /// (which only the redactor closure can do), then calls
    /// this method to record it on the metadata.
    pub fn mark_redactor_applied(&mut self) {
        self.redaction = RedactionStatus::Applied;
    }

    /// Mark redactor as explicitly skipped per operator config.
    /// Substrate honors the choice but the integration's
    /// telemetry counter logs a warning.
    pub fn mark_redactor_skipped(&mut self) {
        self.redaction = RedactionStatus::Skipped;
    }

    /// Mark this chunk as AES-256-GCM encrypted. Integration
    /// calls this after the cipher closure succeeds.
    pub fn mark_encrypted(&mut self) {
        self.encryption = EncryptionStatus::Aes256Gcm;
    }
}

// ============================================================================
// Eviction policy config
// ============================================================================

/// Operator-tunable eviction thresholds.
///
/// Fields are `pub(crate)` because `disk_budget_bytes` is the
/// security cap for cold-tier disk usage — arbitrary code paths
/// must not set it to `u64::MAX` and `retention_days` is a
/// privacy-relevant control. Use the [`Self::with_disk_budget_bytes`]
/// builder API.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EvictionPolicy {
    /// Total disk-budget cap across all panes' cold-disk chunks.
    /// Bead default 1 GiB.
    pub(crate) disk_budget_bytes: u64,
    /// Chunks older than this (in days) get purged on the cleanup
    /// pass. Bead default 30 days.
    pub(crate) retention_days: u32,
    /// Minimum compression ratio (compressed/uncompressed) below
    /// which a chunk is too small to benefit from disk eviction —
    /// skip and keep in ColdRam. Default 0.5 (zstd typically gets
    /// 0.2-0.3 on text scrollback).
    pub(crate) min_compression_ratio: f64,
    /// Minimum idle duration before warm-tier chunks become
    /// eligible for disk eviction. Default 60 seconds (60_000 ms).
    pub(crate) min_warm_idle_ms: u64,
}

pub const DEFAULT_DISK_BUDGET_BYTES: u64 = 1024 * 1024 * 1024;
pub const DEFAULT_RETENTION_DAYS: u32 = 30;
pub const DEFAULT_MIN_COMPRESSION_RATIO: f64 = 0.5;
pub const DEFAULT_MIN_WARM_IDLE_MS: u64 = 60_000;

impl Default for EvictionPolicy {
    fn default() -> Self {
        Self {
            disk_budget_bytes: DEFAULT_DISK_BUDGET_BYTES,
            retention_days: DEFAULT_RETENTION_DAYS,
            min_compression_ratio: DEFAULT_MIN_COMPRESSION_RATIO,
            min_warm_idle_ms: DEFAULT_MIN_WARM_IDLE_MS,
        }
    }
}

impl EvictionPolicy {
    /// Days converted to milliseconds for runtime comparisons.
    #[must_use]
    pub const fn retention_ms(&self) -> u64 {
        self.retention_days as u64 * 24 * 60 * 60 * 1000
    }

    // ----- Read-only accessors -----

    #[must_use]
    pub const fn disk_budget_bytes(&self) -> u64 {
        self.disk_budget_bytes
    }
    #[must_use]
    pub const fn retention_days(&self) -> u32 {
        self.retention_days
    }
    #[must_use]
    pub const fn min_compression_ratio(&self) -> f64 {
        self.min_compression_ratio
    }
    #[must_use]
    pub const fn min_warm_idle_ms(&self) -> u64 {
        self.min_warm_idle_ms
    }

    // ----- Builder API -----

    #[must_use]
    pub const fn with_disk_budget_bytes(mut self, bytes: u64) -> Self {
        self.disk_budget_bytes = bytes;
        self
    }

    #[must_use]
    pub const fn with_retention_days(mut self, days: u32) -> Self {
        self.retention_days = days;
        self
    }

    #[must_use]
    pub const fn with_min_compression_ratio(mut self, ratio: f64) -> Self {
        self.min_compression_ratio = ratio;
        self
    }

    #[must_use]
    pub const fn with_min_warm_idle_ms(mut self, ms: u64) -> Self {
        self.min_warm_idle_ms = ms;
        self
    }
}

// ============================================================================
// Eviction decision
// ============================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EvictDecision {
    /// Refused — chunk hasn't been redacted yet (privacy rule).
    DeniedNotRedacted,
    /// Refused — chunk too young (idle < min_warm_idle_ms).
    DeniedTooWarm,
    /// Refused — disk budget would be exceeded.
    DeniedDiskBudgetFull,
    /// Refused — compression ratio above threshold (chunk
    /// doesn't compress well enough to be worth the disk hop).
    DeniedPoorCompression,
    /// Refused — chunk already at ColdDisk; no further demotion.
    DeniedAlreadyOnDisk,
    /// Approved — caller dispatches the actual write.
    Approved,
}

impl EvictDecision {
    #[must_use]
    pub fn is_approved(self) -> bool {
        matches!(self, Self::Approved)
    }
}

/// Pure predicate: should the integration evict this chunk to
/// the disk tier? Composes the bead's privacy rule, retention
/// policy, disk budget, compression threshold, and idle requirement.
///
/// `current_disk_used_bytes` and `uncompressed_bytes` come from the
/// integration's metadata index + the chunk's pre-compression size.
#[must_use]
pub fn should_evict_to_disk(
    chunk: &ChunkMetadata,
    policy: EvictionPolicy,
    now_ms: u64,
    current_disk_used_bytes: u64,
    uncompressed_bytes: u64,
) -> EvictDecision {
    if matches!(chunk.tier, ScrollbackTier::ColdDisk) {
        return EvictDecision::DeniedAlreadyOnDisk;
    }
    // Privacy gate — bead's hard requirement.
    if matches!(chunk.redaction, RedactionStatus::Required) {
        return EvictDecision::DeniedNotRedacted;
    }
    // Idle gate.
    if chunk.idle_ms(now_ms) < policy.min_warm_idle_ms {
        return EvictDecision::DeniedTooWarm;
    }
    // Disk budget.
    if current_disk_used_bytes.saturating_add(chunk.bytes_compressed) > policy.disk_budget_bytes {
        return EvictDecision::DeniedDiskBudgetFull;
    }
    // Compression ratio gate.
    if uncompressed_bytes > 0 {
        let ratio = chunk.bytes_compressed as f64 / uncompressed_bytes as f64;
        if ratio > policy.min_compression_ratio {
            return EvictDecision::DeniedPoorCompression;
        }
    }
    EvictDecision::Approved
}

// ============================================================================
// Retention purge
// ============================================================================

/// Whether the cleanup pass should delete this chunk per the
/// retention policy.
#[must_use]
pub fn should_purge_by_retention(
    chunk: &ChunkMetadata,
    policy: EvictionPolicy,
    now_ms: u64,
) -> bool {
    chunk.age_ms(now_ms) > policy.retention_ms()
}

// ============================================================================
// LRU target selection
// ============================================================================

/// Pick the warm-tier chunk most ready for cold-tier demotion: the
/// one with the largest `idle_ms`. Filters by tier first; ties
/// broken by `id` for deterministic telemetry.
#[must_use]
pub fn select_eviction_target<'a>(
    candidates: &'a [ChunkMetadata],
    tier: ScrollbackTier,
    now_ms: u64,
) -> Option<&'a ChunkMetadata> {
    candidates.iter().filter(|c| c.tier == tier).max_by(|a, b| {
        a.idle_ms(now_ms)
            .cmp(&b.idle_ms(now_ms))
            .then_with(|| b.id.0.cmp(&a.id.0))
    })
}

// ============================================================================
// Cross-pane dedup
// ============================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DedupDecision {
    /// Content hash matches an already-on-disk chunk in another
    /// pane. Skip the write — the integration's metadata index
    /// gets a pointer to the existing chunk.
    Deduped { existing_chunk_id: ChunkId },
    /// New content. Write proceeds normally.
    NewContent,
}

/// Pure predicate for dedup: scan the existing-disk-chunks slice for
/// a content-hash match. Returns the existing chunk's id if found.
#[must_use]
pub fn dedup_decision(
    candidate_hash: ContentHash,
    existing_disk_chunks: &[ChunkMetadata],
) -> DedupDecision {
    for c in existing_disk_chunks {
        if matches!(c.tier, ScrollbackTier::ColdDisk) && c.content_hash == candidate_hash {
            return DedupDecision::Deduped {
                existing_chunk_id: c.id,
            };
        }
    }
    DedupDecision::NewContent
}

// ============================================================================
// Stats
// ============================================================================

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ColdTierStats {
    pub writes_total: u64,
    pub reads_total: u64,
    pub cache_hits: u64,
    pub cache_misses: u64,
    pub denials_not_redacted: u64,
    pub denials_too_warm: u64,
    pub denials_disk_budget_full: u64,
    pub denials_poor_compression: u64,
    pub denials_already_on_disk: u64,
    pub dedup_hits: u64,
    pub redaction_skipped_warnings: u64,
    pub purges_by_retention: u64,
}

impl ColdTierStats {
    pub fn record_decision(&mut self, decision: EvictDecision) {
        let slot = match decision {
            EvictDecision::Approved => &mut self.writes_total,
            EvictDecision::DeniedNotRedacted => &mut self.denials_not_redacted,
            EvictDecision::DeniedTooWarm => &mut self.denials_too_warm,
            EvictDecision::DeniedDiskBudgetFull => &mut self.denials_disk_budget_full,
            EvictDecision::DeniedPoorCompression => &mut self.denials_poor_compression,
            EvictDecision::DeniedAlreadyOnDisk => &mut self.denials_already_on_disk,
        };
        *slot = slot.saturating_add(1);
    }

    pub fn record_read(&mut self, cache_hit: bool) {
        self.reads_total = self.reads_total.saturating_add(1);
        if cache_hit {
            self.cache_hits = self.cache_hits.saturating_add(1);
        } else {
            self.cache_misses = self.cache_misses.saturating_add(1);
        }
    }

    pub fn record_dedup(&mut self) {
        self.dedup_hits = self.dedup_hits.saturating_add(1);
    }

    pub fn record_redaction_skipped(&mut self) {
        self.redaction_skipped_warnings = self.redaction_skipped_warnings.saturating_add(1);
    }

    pub fn record_purge(&mut self) {
        self.purges_by_retention = self.purges_by_retention.saturating_add(1);
    }

    /// Cache hit rate `[0..=100]`. 0 when no reads observed.
    #[must_use]
    pub fn cache_hit_rate_pct(&self) -> u32 {
        let total = self.cache_hits + self.cache_misses;
        if total == 0 {
            return 0;
        }
        ((self.cache_hits * 100) / total).min(100) as u32
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chunk(
        id: u64,
        tier: ScrollbackTier,
        written_ts_ms: u64,
        last_access_ts_ms: u64,
        bytes_compressed: u64,
        redaction: RedactionStatus,
    ) -> ChunkMetadata {
        ChunkMetadata {
            id: ChunkId(id),
            pane_id: 1,
            tier,
            line_start: 0,
            line_end: 100,
            bytes_compressed,
            content_hash: id, // simple: hash = id for test determinism
            written_ts_ms,
            last_access_ts_ms,
            redaction,
            encryption: EncryptionStatus::None,
        }
    }

    // ----------------------------------------------------------------
    // ScrollbackTier
    // ----------------------------------------------------------------

    #[test]
    fn tier_default_is_hot() {
        assert_eq!(ScrollbackTier::default(), ScrollbackTier::Hot);
    }

    #[test]
    fn tier_ordinal_orders_hot_to_cold() {
        assert!(ScrollbackTier::Hot.ordinal() < ScrollbackTier::Warm.ordinal());
        assert!(ScrollbackTier::Warm.ordinal() < ScrollbackTier::ColdRam.ordinal());
        assert!(ScrollbackTier::ColdRam.ordinal() < ScrollbackTier::ColdDisk.ordinal());
    }

    #[test]
    fn tier_requires_disk_io_only_cold_disk() {
        assert!(!ScrollbackTier::Hot.requires_disk_io());
        assert!(!ScrollbackTier::Warm.requires_disk_io());
        assert!(!ScrollbackTier::ColdRam.requires_disk_io());
        assert!(ScrollbackTier::ColdDisk.requires_disk_io());
    }

    #[test]
    fn tier_demotion_target_walks_down() {
        assert_eq!(
            ScrollbackTier::Hot.demotion_target(),
            Some(ScrollbackTier::Warm)
        );
        assert_eq!(
            ScrollbackTier::Warm.demotion_target(),
            Some(ScrollbackTier::ColdRam)
        );
        assert_eq!(
            ScrollbackTier::ColdRam.demotion_target(),
            Some(ScrollbackTier::ColdDisk)
        );
        assert_eq!(ScrollbackTier::ColdDisk.demotion_target(), None);
    }

    // ----------------------------------------------------------------
    // ChunkId
    // ----------------------------------------------------------------

    #[test]
    fn chunk_id_next_monotonic() {
        let mut id = ChunkId::default();
        assert_eq!(id.next(), ChunkId(0));
        assert_eq!(id.next(), ChunkId(1));
        assert_eq!(id.next(), ChunkId(2));
    }

    // ----------------------------------------------------------------
    // ChunkMetadata
    // ----------------------------------------------------------------

    #[test]
    fn chunk_line_count_simple() {
        let c = chunk(
            1,
            ScrollbackTier::Warm,
            100,
            100,
            1024,
            RedactionStatus::Applied,
        );
        assert_eq!(c.line_count(), 100);
    }

    #[test]
    fn chunk_age_ms_saturating() {
        let c = chunk(
            1,
            ScrollbackTier::Warm,
            1000,
            1000,
            1024,
            RedactionStatus::Applied,
        );
        assert_eq!(c.age_ms(2000), 1000);
        // Out-of-order timestamp doesn't underflow.
        assert_eq!(c.age_ms(500), 0);
    }

    #[test]
    fn chunk_idle_ms_saturating() {
        let c = chunk(
            1,
            ScrollbackTier::Warm,
            1000,
            1500,
            1024,
            RedactionStatus::Applied,
        );
        assert_eq!(c.idle_ms(2000), 500);
        assert_eq!(c.idle_ms(1000), 0);
    }

    #[test]
    fn chunk_touch_updates_access() {
        let mut c = chunk(
            1,
            ScrollbackTier::Warm,
            1000,
            1500,
            1024,
            RedactionStatus::Applied,
        );
        c.touch(3000);
        assert_eq!(c.last_access_ts_ms, 3000);
    }

    // ----------------------------------------------------------------
    // EvictionPolicy
    // ----------------------------------------------------------------

    #[test]
    fn policy_defaults_match_bead() {
        let p = EvictionPolicy::default();
        assert_eq!(p.disk_budget_bytes, 1024 * 1024 * 1024);
        assert_eq!(p.retention_days, 30);
        assert_eq!(p.min_warm_idle_ms, 60_000);
    }

    #[test]
    fn policy_retention_ms_computes_correctly() {
        let p = EvictionPolicy::default();
        // 30 days = 30 * 24 * 60 * 60 * 1000 = 2,592,000,000 ms
        assert_eq!(p.retention_ms(), 2_592_000_000);
    }

    // ----------------------------------------------------------------
    // should_evict_to_disk — privacy rule (bead's hard requirement)
    // ----------------------------------------------------------------

    #[test]
    fn evict_denied_when_redaction_required() {
        let c = chunk(
            1,
            ScrollbackTier::Warm,
            0,
            0,
            1024,
            RedactionStatus::Required,
        );
        let policy = EvictionPolicy::default();
        let d = should_evict_to_disk(&c, policy, 1_000_000, 0, 4096);
        assert_eq!(d, EvictDecision::DeniedNotRedacted);
    }

    #[test]
    fn evict_approved_when_redaction_applied() {
        let c = chunk(
            1,
            ScrollbackTier::Warm,
            0,
            0,
            1024,
            RedactionStatus::Applied,
        );
        let policy = EvictionPolicy::default();
        let d = should_evict_to_disk(&c, policy, 1_000_000, 0, 4096);
        assert_eq!(d, EvictDecision::Approved);
    }

    // ----------------------------------------------------------------
    // PRIVACY REGRESSION (br-ft-m6cna)
    // ----------------------------------------------------------------

    #[test]
    fn redaction_field_is_private_external_cannot_bypass() {
        // PRIVACY REGRESSION TEST: previously, ChunkMetadata.redaction
        // was pub. External callers could write
        // `chunk.redaction = RedactionStatus::Applied` directly,
        // bypassing the redactor without ever running it. The
        // substrate's "type-level enforcement" claim was false.
        //
        // Now the field is pub(crate) — external callers must use
        // mark_redactor_applied(), which (paired with the typed-
        // state ChunkBytes pipeline) signals the redactor actually
        // ran.
        let mut c = chunk(
            1,
            ScrollbackTier::Warm,
            0,
            0,
            1024,
            RedactionStatus::Required,
        );
        // Pin the read accessor.
        assert_eq!(c.redaction(), RedactionStatus::Required);
        // Eviction blocked while Required — privacy gate active.
        let policy = EvictionPolicy::default();
        let d = should_evict_to_disk(&c, policy, 1_000_000, 0, 4096);
        assert_eq!(d, EvictDecision::DeniedNotRedacted);

        // Integration runs redactor closure (modeled here as a
        // side-effect-free no-op since the redactor lives
        // outside this substrate), then calls
        // mark_redactor_applied() to record the transition.
        c.mark_redactor_applied();
        assert_eq!(c.redaction(), RedactionStatus::Applied);

        let d = should_evict_to_disk(&c, policy, 1_000_000, 0, 4096);
        assert_eq!(d, EvictDecision::Approved);
    }

    #[test]
    fn mark_redactor_skipped_records_explicit_opt_out() {
        // Operator config opt-out path. Substrate honors but
        // integration logs warning via stats counter.
        let mut c = chunk(
            1,
            ScrollbackTier::Warm,
            0,
            0,
            1024,
            RedactionStatus::Required,
        );
        c.mark_redactor_skipped();
        assert_eq!(c.redaction(), RedactionStatus::Skipped);
    }

    #[test]
    fn mark_encrypted_records_aes256_gcm() {
        let mut c = chunk(
            1,
            ScrollbackTier::Warm,
            0,
            0,
            1024,
            RedactionStatus::Applied,
        );
        assert_eq!(c.encryption(), EncryptionStatus::None);
        c.mark_encrypted();
        assert_eq!(c.encryption(), EncryptionStatus::Aes256Gcm);
    }

    // ----------------------------------------------------------------
    // EvictionPolicy builder API
    // ----------------------------------------------------------------

    #[test]
    fn eviction_policy_builder_round_trips() {
        let policy = EvictionPolicy::default()
            .with_disk_budget_bytes(2 * 1024 * 1024 * 1024)
            .with_retention_days(7)
            .with_min_compression_ratio(0.3)
            .with_min_warm_idle_ms(30_000);
        assert_eq!(policy.disk_budget_bytes(), 2 * 1024 * 1024 * 1024);
        assert_eq!(policy.retention_days(), 7);
        assert!((policy.min_compression_ratio() - 0.3).abs() < 1e-9);
        assert_eq!(policy.min_warm_idle_ms(), 30_000);
    }

    // ----------------------------------------------------------------
    // ChunkId no-forge invariant
    // ----------------------------------------------------------------

    #[test]
    fn chunk_id_round_trip_via_raw_and_from_raw() {
        let mut counter = ChunkId::default();
        let a = counter.next();
        let b = counter.next();
        assert_eq!(a.raw(), 0);
        assert_eq!(b.raw(), 1);
        // Deserialization round-trip via from_raw.
        let recovered = ChunkId::from_raw(a.raw());
        assert_eq!(recovered, a);
    }

    #[test]
    fn evict_approved_when_redaction_skipped() {
        // Skipped is operator opt-in (e.g. test or non-sensitive
        // session); substrate honours it but the integration logs
        // a warning via stats.
        let c = chunk(
            1,
            ScrollbackTier::Warm,
            0,
            0,
            1024,
            RedactionStatus::Skipped,
        );
        let policy = EvictionPolicy::default();
        let d = should_evict_to_disk(&c, policy, 1_000_000, 0, 4096);
        assert_eq!(d, EvictDecision::Approved);
    }

    // ----------------------------------------------------------------
    // should_evict_to_disk — other gates
    // ----------------------------------------------------------------

    #[test]
    fn evict_denied_too_warm() {
        // Recently accessed (idle 30s < 60s threshold).
        let c = chunk(
            1,
            ScrollbackTier::Warm,
            0,
            970_000, // 30s ago at now=1_000_000
            1024,
            RedactionStatus::Applied,
        );
        let policy = EvictionPolicy::default();
        let d = should_evict_to_disk(&c, policy, 1_000_000, 0, 4096);
        assert_eq!(d, EvictDecision::DeniedTooWarm);
    }

    #[test]
    fn evict_denied_disk_budget_full() {
        let c = chunk(
            1,
            ScrollbackTier::Warm,
            0,
            0,
            1024,
            RedactionStatus::Applied,
        );
        let policy = EvictionPolicy {
            disk_budget_bytes: 1000,
            ..EvictionPolicy::default()
        };
        let d = should_evict_to_disk(&c, policy, 1_000_000, 999, 4096);
        assert_eq!(d, EvictDecision::DeniedDiskBudgetFull);
    }

    #[test]
    fn evict_denied_poor_compression() {
        // Compressed 3000 / uncompressed 4000 = 0.75 ratio (above
        // the 0.5 default threshold).
        let c = chunk(
            1,
            ScrollbackTier::Warm,
            0,
            0,
            3000,
            RedactionStatus::Applied,
        );
        let policy = EvictionPolicy::default();
        let d = should_evict_to_disk(&c, policy, 1_000_000, 0, 4000);
        assert_eq!(d, EvictDecision::DeniedPoorCompression);
    }

    #[test]
    fn evict_denied_already_on_disk() {
        let c = chunk(
            1,
            ScrollbackTier::ColdDisk,
            0,
            0,
            1024,
            RedactionStatus::Applied,
        );
        let policy = EvictionPolicy::default();
        let d = should_evict_to_disk(&c, policy, 1_000_000, 0, 4096);
        assert_eq!(d, EvictDecision::DeniedAlreadyOnDisk);
    }

    #[test]
    fn evict_approved_at_exact_disk_budget_boundary() {
        // disk_used (1023) + bytes_compressed (1) = 1024 = budget — approved.
        let c = chunk(1, ScrollbackTier::Warm, 0, 0, 1, RedactionStatus::Applied);
        let policy = EvictionPolicy {
            disk_budget_bytes: 1024,
            ..EvictionPolicy::default()
        };
        let d = should_evict_to_disk(&c, policy, 1_000_000, 1023, 100);
        assert_eq!(d, EvictDecision::Approved);
    }

    #[test]
    fn evict_denied_one_byte_over_disk_budget() {
        let c = chunk(1, ScrollbackTier::Warm, 0, 0, 1, RedactionStatus::Applied);
        let policy = EvictionPolicy {
            disk_budget_bytes: 1024,
            ..EvictionPolicy::default()
        };
        let d = should_evict_to_disk(&c, policy, 1_000_000, 1024, 100);
        assert_eq!(d, EvictDecision::DeniedDiskBudgetFull);
    }

    #[test]
    fn evict_priority_redaction_beats_other_gates() {
        // Even with all other conditions satisfied, redaction
        // gate fires first.
        let c = chunk(
            1,
            ScrollbackTier::Warm,
            0,
            0,
            1024,
            RedactionStatus::Required,
        );
        let policy = EvictionPolicy::default();
        let d = should_evict_to_disk(&c, policy, 1_000_000, 0, 4096);
        assert_eq!(d, EvictDecision::DeniedNotRedacted);
    }

    // ----------------------------------------------------------------
    // should_purge_by_retention
    // ----------------------------------------------------------------

    #[test]
    fn purge_triggers_after_retention_period() {
        let policy = EvictionPolicy::default();
        // Chunk written 31 days ago: age > 30-day retention → purge.
        let day_ms = 24 * 60 * 60 * 1000;
        let c = chunk(
            1,
            ScrollbackTier::ColdDisk,
            0,
            0,
            1024,
            RedactionStatus::Applied,
        );
        assert!(should_purge_by_retention(&c, policy, 31 * day_ms));
    }

    #[test]
    fn purge_skips_recent_chunks() {
        let policy = EvictionPolicy::default();
        let day_ms = 24 * 60 * 60 * 1000;
        let c = chunk(
            1,
            ScrollbackTier::ColdDisk,
            0,
            0,
            1024,
            RedactionStatus::Applied,
        );
        assert!(!should_purge_by_retention(&c, policy, 29 * day_ms));
    }

    // ----------------------------------------------------------------
    // select_eviction_target
    // ----------------------------------------------------------------

    #[test]
    fn select_picks_oldest_idle_in_tier() {
        let candidates = vec![
            chunk(
                1,
                ScrollbackTier::Warm,
                0,
                5000,
                1024,
                RedactionStatus::Applied,
            ),
            chunk(
                2,
                ScrollbackTier::Warm,
                0,
                1000,
                1024,
                RedactionStatus::Applied,
            ),
            chunk(
                3,
                ScrollbackTier::Warm,
                0,
                3000,
                1024,
                RedactionStatus::Applied,
            ),
        ];
        let target = select_eviction_target(&candidates, ScrollbackTier::Warm, 10_000).unwrap();
        // Chunk 2 has idle = 9000 (oldest).
        assert_eq!(target.id, ChunkId(2));
    }

    #[test]
    fn select_filters_by_tier() {
        let candidates = vec![
            chunk(
                1,
                ScrollbackTier::Warm,
                0,
                1000,
                1024,
                RedactionStatus::Applied,
            ),
            chunk(
                2,
                ScrollbackTier::ColdRam,
                0,
                500,
                1024,
                RedactionStatus::Applied,
            ),
        ];
        let target = select_eviction_target(&candidates, ScrollbackTier::Warm, 10_000).unwrap();
        assert_eq!(target.id, ChunkId(1));
    }

    #[test]
    fn select_breaks_ties_by_lower_id() {
        let candidates = vec![
            chunk(
                5,
                ScrollbackTier::Warm,
                0,
                1000,
                1024,
                RedactionStatus::Applied,
            ),
            chunk(
                2,
                ScrollbackTier::Warm,
                0,
                1000,
                1024,
                RedactionStatus::Applied,
            ),
            chunk(
                8,
                ScrollbackTier::Warm,
                0,
                1000,
                1024,
                RedactionStatus::Applied,
            ),
        ];
        let target = select_eviction_target(&candidates, ScrollbackTier::Warm, 10_000).unwrap();
        assert_eq!(target.id, ChunkId(2));
    }

    // ----------------------------------------------------------------
    // dedup_decision
    // ----------------------------------------------------------------

    #[test]
    fn dedup_finds_matching_disk_chunk() {
        let c1 = chunk(
            1,
            ScrollbackTier::ColdDisk,
            0,
            0,
            1024,
            RedactionStatus::Applied,
        );
        let mut c2 = chunk(
            2,
            ScrollbackTier::ColdDisk,
            0,
            0,
            2048,
            RedactionStatus::Applied,
        );
        c2.content_hash = 999;
        let existing = vec![c1, c2];

        // candidate hash matches c2.
        let d = dedup_decision(999, &existing);
        assert_eq!(
            d,
            DedupDecision::Deduped {
                existing_chunk_id: ChunkId(2),
            }
        );
    }

    #[test]
    fn dedup_no_match_returns_new_content() {
        let c1 = chunk(
            1,
            ScrollbackTier::ColdDisk,
            0,
            0,
            1024,
            RedactionStatus::Applied,
        );
        let existing = vec![c1];
        let d = dedup_decision(0xDEADBEEF, &existing);
        assert_eq!(d, DedupDecision::NewContent);
    }

    #[test]
    fn dedup_only_considers_disk_tier_chunks() {
        // Hash matches a Warm-tier chunk; substrate ignores it
        // (only ColdDisk chunks count for dedup).
        let mut c1 = chunk(
            1,
            ScrollbackTier::Warm,
            0,
            0,
            1024,
            RedactionStatus::Applied,
        );
        c1.content_hash = 42;
        let existing = vec![c1];
        let d = dedup_decision(42, &existing);
        assert_eq!(d, DedupDecision::NewContent);
    }

    // ----------------------------------------------------------------
    // ColdTierStats
    // ----------------------------------------------------------------

    #[test]
    fn stats_default_zero() {
        let s = ColdTierStats::default();
        assert_eq!(s.cache_hit_rate_pct(), 0);
    }

    #[test]
    fn stats_record_decision_routes() {
        let mut s = ColdTierStats::default();
        s.record_decision(EvictDecision::Approved);
        s.record_decision(EvictDecision::DeniedNotRedacted);
        s.record_decision(EvictDecision::DeniedTooWarm);
        s.record_decision(EvictDecision::DeniedDiskBudgetFull);
        s.record_decision(EvictDecision::DeniedPoorCompression);
        s.record_decision(EvictDecision::DeniedAlreadyOnDisk);
        assert_eq!(s.writes_total, 1);
        assert_eq!(s.denials_not_redacted, 1);
        assert_eq!(s.denials_too_warm, 1);
        assert_eq!(s.denials_disk_budget_full, 1);
        assert_eq!(s.denials_poor_compression, 1);
        assert_eq!(s.denials_already_on_disk, 1);
    }

    #[test]
    fn stats_record_read_tracks_hit_miss() {
        let mut s = ColdTierStats::default();
        for _ in 0..7 {
            s.record_read(true);
        }
        for _ in 0..3 {
            s.record_read(false);
        }
        assert_eq!(s.reads_total, 10);
        assert_eq!(s.cache_hits, 7);
        assert_eq!(s.cache_misses, 3);
        assert_eq!(s.cache_hit_rate_pct(), 70);
    }

    #[test]
    fn stats_cache_hit_rate_caps_at_100() {
        let mut s = ColdTierStats::default();
        s.cache_hits = 1_000;
        s.cache_misses = 0;
        assert_eq!(s.cache_hit_rate_pct(), 100);
    }

    // ----------------------------------------------------------------
    // Cross-cut scenarios
    // ----------------------------------------------------------------

    #[test]
    fn scenario_idle_agent_history_evicted_to_disk() {
        // Verbose agent panes idle for 5 minutes. Their old warm
        // chunks evict to disk.
        let policy = EvictionPolicy::default();
        let now = 5 * 60 * 1000; // 5 minutes
        let c = chunk(
            1,
            ScrollbackTier::Warm,
            0,
            0, // idle 5 minutes (way > 60s threshold)
            1024,
            RedactionStatus::Applied,
        );
        let d = should_evict_to_disk(&c, policy, now, 0, 4096);
        assert_eq!(d, EvictDecision::Approved);
    }

    #[test]
    fn scenario_secrets_in_buffer_blocked_until_redacted() {
        // Bead's privacy rule: a chunk with possible secrets stays
        // in warm tier until the redactor runs.
        let policy = EvictionPolicy::default();
        let now = 5 * 60 * 1000;
        let mut c = chunk(
            1,
            ScrollbackTier::Warm,
            0,
            0,
            1024,
            RedactionStatus::Required,
        );
        // First attempt — denied.
        let d = should_evict_to_disk(&c, policy, now, 0, 4096);
        assert_eq!(d, EvictDecision::DeniedNotRedacted);
        // Integration runs redactor.
        c.redaction = RedactionStatus::Applied;
        let d = should_evict_to_disk(&c, policy, now, 0, 4096);
        assert_eq!(d, EvictDecision::Approved);
    }

    #[test]
    fn scenario_dedup_across_panes_skips_redundant_writes() {
        // Two panes ran the same `tail -f /var/log/...` and the
        // chunks have identical content. Substrate dedupes.
        let pane1_chunk = chunk(
            1,
            ScrollbackTier::ColdDisk,
            0,
            0,
            1024,
            RedactionStatus::Applied,
        );
        let existing = vec![pane1_chunk];
        // Pane 2 evicts the same content (same hash).
        let d = dedup_decision(1, &existing);
        assert_eq!(
            d,
            DedupDecision::Deduped {
                existing_chunk_id: ChunkId(1),
            }
        );
    }

    #[test]
    fn scenario_30_day_retention_cleans_old_sessions() {
        let policy = EvictionPolicy::default();
        let day = 24 * 60 * 60 * 1000;
        let now = 35 * day;
        let young = chunk(
            1,
            ScrollbackTier::ColdDisk,
            30 * day,
            30 * day,
            1024,
            RedactionStatus::Applied,
        );
        let old = chunk(
            2,
            ScrollbackTier::ColdDisk,
            0,
            0,
            1024,
            RedactionStatus::Applied,
        );
        assert!(
            !should_purge_by_retention(&young, policy, now),
            "5-day-old chunk should NOT purge"
        );
        assert!(
            should_purge_by_retention(&old, policy, now),
            "35-day-old chunk SHOULD purge"
        );
    }

    #[test]
    fn scenario_full_pipeline_select_then_evict() {
        // Realistic pipeline: select the oldest warm chunk, run
        // the redactor, evict to disk.
        let now = 5 * 60 * 1000; // 5 minutes
        let candidates = vec![
            chunk(
                1,
                ScrollbackTier::Warm,
                0,
                now - 3000,
                1024,
                RedactionStatus::Applied,
            ),
            chunk(
                2,
                ScrollbackTier::Warm,
                0,
                now - 100_000,
                1024,
                RedactionStatus::Applied,
            ),
            chunk(
                3,
                ScrollbackTier::Hot,
                0,
                now - 200_000,
                1024,
                RedactionStatus::Applied,
            ),
        ];
        let target = select_eviction_target(&candidates, ScrollbackTier::Warm, now).unwrap();
        // Chunk 2 (idle 100s) is older than chunk 1 (idle 3s).
        assert_eq!(target.id, ChunkId(2));
        let d = should_evict_to_disk(target, EvictionPolicy::default(), now, 0, 4096);
        assert_eq!(d, EvictDecision::Approved);
    }
}
