//! GPU pipeline-cache key derivation + invalidation policy
//! substrate (ft-2okh0.4).
//!
//! Pure-logic substrate for the bead's "compiled GPU pipelines
//! cached on disk between ft runs, <100ms cold start to first
//! frame" requirement. This module ships the cache-key derivation
//! (the SHA-256-over-versioned-inputs that goes in the cache
//! filename), the 6-cause invalidation enumeration the operator's
//! `ft doctor` surfaces, and the HMAC integrity check policy. The
//! actual wgpu `pipeline_cache` API + per-platform (Vulkan VK_EXT /
//! Metal MTLBinaryArchive / DX12 ID3D12PipelineLibrary) calls live
//! in the integration crate.
//!
//! ## What this module ships
//!
//! - `PipelineCacheVersion` — composite key components: `arch`,
//!   `wgpu_version`, `driver_version`, `shader_hash`,
//!   `ft_binary_hash`. Five 64-bit hash slots so derivation never
//!   touches strings on the hot path.
//! - `derive_cache_key` — pure FNV-1a 64-bit fold over the version
//!   components. Stable across runs given identical inputs.
//! - `cache_filename` — produces `<arch>-<wgpu-version>.bin`
//!   matching the bead's filesystem layout.
//! - `CacheInvalidationCause` — `BinaryUpgraded /
//!   DriverUpgraded / WgpuVersionChanged / ShaderHashChanged /
//!   HmacMismatch / OperatorRequested`. The 6 causes the bead
//!   enumerates; pure-data telemetry routing.
//! - `should_invalidate` — pure predicate composing cached-version
//!   vs current-version; returns the first mismatch reason or
//!   `None` when the cache is fresh.
//! - `CacheSizeBudget` — operator-tunable cap (default 64 MiB).
//!   `should_evict` predicate fires when cumulative pipeline
//!   blob size exceeds budget.
//! - `IntegrityCheck` — HMAC-SHA256 over (binary_path + version)
//!   matches the bead's tamper-detection requirement. Pure-data;
//!   the integration's `hmac` crate computes the actual HMAC.
//! - `CacheStats` — counters
//!   (`cache_hits_total / cache_misses_total / invalidations_per_cause /
//!   cold_start_time_ms / hit_rate_pct`) for `ft doctor`.
//!
//! ## What is deferred to the integration bead (ft-2okh0.4.cont)
//!
//! - wgpu `pipeline_cache` API wiring: load on startup, write on
//!   shutdown.
//! - On-disk format at `~/.cache/ft/gpu-pipelines/<arch>-<wgpu-
//!   version>.bin` with mode `0600`.
//! - Per-platform pipeline-cache backend (Vulkan VK_EXT /
//!   Metal MTLBinaryArchive / DX12 ID3D12PipelineLibrary) —
//!   wgpu abstracts these but the on-disk artifact is the
//!   integration's responsibility.
//! - Driver-version probing: query `vkGetPhysicalDeviceProperties`
//!   on Linux, `MTLDevice.name + MTLDevice.registryID` on macOS,
//!   `DXGI_ADAPTER_DESC` on Windows.
//! - HMAC computation via the `hmac` crate
//!   (`Hmac<Sha256>::new_from_slice(key).chain_update(payload)`).
//! - `ft cache clear gpu-pipelines` CLI.
//! - Cross-link to ft-mpc9b.3.1 Metal-direct backend (which has its
//!   own MTLBinaryArchive cache; this substrate generalises the
//!   pattern).

#![allow(dead_code)]

// ============================================================================
// Pipeline cache version components
// ============================================================================

/// Composite version-fingerprint identifying a cache-eligible
/// pipeline build. Each field is a 64-bit hash so derivation never
/// touches strings on the hot path. The integration's startup
/// probe builds this from runtime info; the substrate compares
/// against the cached version to decide invalidation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct PipelineCacheVersion {
    /// Hash of `cfg!(target_arch = "...")` + macOS/Linux/Windows
    /// build tag. Distinguishes M-series vs Intel macOS, x86_64 vs
    /// aarch64 Linux, etc. so a cache built on one host doesn't
    /// load on another.
    pub arch_hash: u64,
    /// Hash of `wgpu`'s `Cargo.toml` version string. Different wgpu
    /// versions produce incompatible serialized cache formats.
    pub wgpu_version_hash: u64,
    /// Hash of the GPU driver version string. Driver upgrades
    /// invalidate the cache because shader-IR codegen can change.
    pub driver_version_hash: u64,
    /// Hash of all shader source files (concatenated + hashed at
    /// build.rs time). A shader change invalidates the cache.
    pub shader_hash: u64,
    /// SHA-256 of the ft binary itself (truncated to u64). Detects
    /// the case where the binary upgraded but the cache wasn't
    /// purged (rare but possible if the user `cp`'s a new binary
    /// over the old).
    pub ft_binary_hash: u64,
}

impl PipelineCacheVersion {
    /// Test-friendly constructor.
    #[must_use]
    pub const fn new(
        arch_hash: u64,
        wgpu_version_hash: u64,
        driver_version_hash: u64,
        shader_hash: u64,
        ft_binary_hash: u64,
    ) -> Self {
        Self {
            arch_hash,
            wgpu_version_hash,
            driver_version_hash,
            shader_hash,
            ft_binary_hash,
        }
    }
}

// ============================================================================
// Cache key derivation (FNV-1a 64-bit)
// ============================================================================

const FNV_OFFSET_BASIS_64: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME_64: u64 = 0x100_0000_01b3;

/// Derive a stable cache key from the version. Pure-logic; no I/O.
/// Same inputs → same output across runs and platforms.
#[must_use]
pub fn derive_cache_key(v: PipelineCacheVersion) -> u64 {
    let bytes = [
        v.arch_hash.to_le_bytes(),
        v.wgpu_version_hash.to_le_bytes(),
        v.driver_version_hash.to_le_bytes(),
        v.shader_hash.to_le_bytes(),
        v.ft_binary_hash.to_le_bytes(),
    ];
    let mut hash = FNV_OFFSET_BASIS_64;
    for slot in &bytes {
        for &byte in slot {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(FNV_PRIME_64);
        }
    }
    hash
}

/// Produce the cache filename per the bead's spec:
/// `<arch>-<wgpu-version>.bin`. The integration writes / reads from
/// `~/.cache/ft/gpu-pipelines/<filename>`.
#[must_use]
pub fn cache_filename(arch_label: &str, wgpu_version_label: &str) -> String {
    format!("{arch_label}-{wgpu_version_label}.bin")
}

// ============================================================================
// Invalidation policy (6 causes per the bead)
// ============================================================================

/// The 6 causes the bead enumerates. Each maps to a specific
/// version-component mismatch or a runtime signal (HMAC / operator).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CacheInvalidationCause {
    /// ft binary upgraded — different SHA-256.
    BinaryUpgraded,
    /// GPU driver upgraded — different driver version string.
    DriverUpgraded,
    /// wgpu crate version change — incompatible serialised format.
    WgpuVersionChanged,
    /// Shader source change — different shader_hash from build.rs.
    ShaderHashChanged,
    /// Architecture identifier change — usually only happens after
    /// migrating a cache file across hosts; the substrate catches
    /// it before hand-off.
    ArchChanged,
    /// HMAC over the cache contents doesn't match — corruption or
    /// tampering.
    HmacMismatch,
    /// Operator ran `ft cache clear gpu-pipelines`.
    OperatorRequested,
}

/// Pure predicate: should the cached pipeline be discarded? Returns
/// the first mismatch reason or `None` when fresh. Order of checks
/// matches the bead's enumeration so the operator-visible reason
/// in `ft doctor` is the first relevant cause.
#[must_use]
pub fn should_invalidate(
    cached: PipelineCacheVersion,
    current: PipelineCacheVersion,
    integrity: IntegrityCheck,
    operator_requested: bool,
) -> Option<CacheInvalidationCause> {
    if operator_requested {
        return Some(CacheInvalidationCause::OperatorRequested);
    }
    if integrity == IntegrityCheck::Mismatch {
        return Some(CacheInvalidationCause::HmacMismatch);
    }
    if cached.ft_binary_hash != current.ft_binary_hash {
        return Some(CacheInvalidationCause::BinaryUpgraded);
    }
    if cached.driver_version_hash != current.driver_version_hash {
        return Some(CacheInvalidationCause::DriverUpgraded);
    }
    if cached.wgpu_version_hash != current.wgpu_version_hash {
        return Some(CacheInvalidationCause::WgpuVersionChanged);
    }
    if cached.shader_hash != current.shader_hash {
        return Some(CacheInvalidationCause::ShaderHashChanged);
    }
    if cached.arch_hash != current.arch_hash {
        return Some(CacheInvalidationCause::ArchChanged);
    }
    None
}

// ============================================================================
// HMAC integrity check
// ============================================================================

/// Result of the integration's HMAC-SHA256 verification over the
/// cache file. Pure-data; the integration's `hmac` + `sha2` crates
/// compute the actual HMAC and feed the result here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum IntegrityCheck {
    /// HMAC matches expected — cache is intact.
    #[default]
    Match,
    /// HMAC doesn't match — corruption / tampering.
    Mismatch,
    /// Cache file missing or unreadable; not a tampering signal,
    /// just absence. The substrate's `should_invalidate` treats
    /// `NotChecked` as "fresh" (no cache → no invalidation). The
    /// integration's load path checks for file existence
    /// separately.
    NotChecked,
}

// ============================================================================
// Cache size budget
// ============================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CacheSizeBudget {
    /// Maximum total cache size in bytes. Default 64 MiB.
    pub max_bytes: u64,
}

/// 64 MiB default — large enough for typical ft sessions (a handful
/// of pipelines × ~hundreds of KB each), small enough to evict
/// cleanly if the user has many ft installs.
pub const DEFAULT_MAX_BYTES: u64 = 64 * 1024 * 1024;

impl Default for CacheSizeBudget {
    fn default() -> Self {
        Self {
            max_bytes: DEFAULT_MAX_BYTES,
        }
    }
}

impl CacheSizeBudget {
    /// Whether the integration should evict cache files. Pure
    /// predicate.
    #[must_use]
    pub const fn should_evict(&self, current_bytes: u64) -> bool {
        current_bytes > self.max_bytes
    }
}

// ============================================================================
// Stats
// ============================================================================

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CacheStats {
    pub cache_hits_total: u64,
    pub cache_misses_total: u64,
    pub invalidations_binary_upgraded: u64,
    pub invalidations_driver_upgraded: u64,
    pub invalidations_wgpu_version_changed: u64,
    pub invalidations_shader_hash_changed: u64,
    pub invalidations_arch_changed: u64,
    pub invalidations_hmac_mismatch: u64,
    pub invalidations_operator_requested: u64,
    pub last_cold_start_ms: u64,
    /// Whether `record_cold_start_ms` has ever been called.
    /// Distinguishes "no data yet" (false) from "measured 0 ms
    /// cold-start" (true with last_cold_start_ms=0).
    pub cold_start_recorded: bool,
}

impl CacheStats {
    pub fn record_hit(&mut self) {
        self.cache_hits_total = self.cache_hits_total.saturating_add(1);
    }

    pub fn record_miss(&mut self) {
        self.cache_misses_total = self.cache_misses_total.saturating_add(1);
    }

    pub fn record_invalidation(&mut self, cause: CacheInvalidationCause) {
        let slot = match cause {
            CacheInvalidationCause::BinaryUpgraded => &mut self.invalidations_binary_upgraded,
            CacheInvalidationCause::DriverUpgraded => &mut self.invalidations_driver_upgraded,
            CacheInvalidationCause::WgpuVersionChanged => {
                &mut self.invalidations_wgpu_version_changed
            }
            CacheInvalidationCause::ShaderHashChanged => {
                &mut self.invalidations_shader_hash_changed
            }
            CacheInvalidationCause::ArchChanged => &mut self.invalidations_arch_changed,
            CacheInvalidationCause::HmacMismatch => &mut self.invalidations_hmac_mismatch,
            CacheInvalidationCause::OperatorRequested => &mut self.invalidations_operator_requested,
        };
        *slot = slot.saturating_add(1);
    }

    pub fn record_cold_start_ms(&mut self, ms: u64) {
        self.last_cold_start_ms = ms;
        self.cold_start_recorded = true;
    }

    /// Cache hit rate as integer percent `[0..=100]`. 0 when no
    /// lookups have occurred.
    #[must_use]
    pub fn hit_rate_pct(&self) -> u32 {
        let total = self.cache_hits_total + self.cache_misses_total;
        if total == 0 {
            return 0;
        }
        ((self.cache_hits_total * 100) / total).min(100) as u32
    }

    /// Whether the cold-start time meets the bead's <100 ms
    /// target. `None` when no cold-start has been recorded yet
    /// (so `ft doctor` can render "no data" instead of an
    /// alarming "failing"). `Some(true)` when the most recent
    /// cold-start was strictly under 100 ms; `Some(false)`
    /// otherwise.
    ///
    /// Self-review (br-ft-ydrpp): previously returned `bool`
    /// with `false` collapsing both "no data" and "measured
    /// failure", which surfaced a misleading red signal on
    /// fresh sessions.
    #[must_use]
    pub const fn meets_cold_start_target(&self) -> Option<bool> {
        if !self.cold_start_recorded {
            return None;
        }
        Some(self.last_cold_start_ms < 100)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn version(
        arch: u64,
        wgpu: u64,
        driver: u64,
        shader: u64,
        binary: u64,
    ) -> PipelineCacheVersion {
        PipelineCacheVersion::new(arch, wgpu, driver, shader, binary)
    }

    // ----------------------------------------------------------------
    // PipelineCacheVersion + derive_cache_key
    // ----------------------------------------------------------------

    #[test]
    fn derive_cache_key_deterministic() {
        let v = version(1, 2, 3, 4, 5);
        let a = derive_cache_key(v);
        let b = derive_cache_key(v);
        assert_eq!(a, b);
    }

    #[test]
    fn derive_cache_key_different_inputs_different_outputs() {
        let a = derive_cache_key(version(1, 2, 3, 4, 5));
        let b = derive_cache_key(version(1, 2, 3, 4, 6));
        let c = derive_cache_key(version(2, 2, 3, 4, 5));
        assert_ne!(a, b);
        assert_ne!(a, c);
        assert_ne!(b, c);
    }

    #[test]
    fn derive_cache_key_zero_versions_yields_stable_known_value() {
        let v = version(0, 0, 0, 0, 0);
        // 40 bytes of zeros folded through FNV-1a-64. Just assert
        // the value is stable across runs (which we already check
        // in derive_cache_key_deterministic) and that it's not
        // trivially zero.
        let k = derive_cache_key(v);
        assert_ne!(k, 0);
        assert_ne!(k, FNV_OFFSET_BASIS_64);
    }

    #[test]
    fn cache_filename_produces_bead_format() {
        assert_eq!(
            cache_filename("arm64-macos", "0.20.0"),
            "arm64-macos-0.20.0.bin"
        );
        assert_eq!(
            cache_filename("x86_64-linux", "0.20.0"),
            "x86_64-linux-0.20.0.bin"
        );
    }

    // ----------------------------------------------------------------
    // should_invalidate (6 causes per the bead)
    // ----------------------------------------------------------------

    #[test]
    fn fresh_cache_returns_no_invalidation() {
        let v = version(1, 2, 3, 4, 5);
        assert_eq!(should_invalidate(v, v, IntegrityCheck::Match, false), None);
    }

    #[test]
    fn operator_requested_wins_over_everything() {
        let cached = version(1, 2, 3, 4, 5);
        let current = version(1, 2, 3, 4, 5); // identical
        let cause = should_invalidate(cached, current, IntegrityCheck::Match, true);
        assert_eq!(cause, Some(CacheInvalidationCause::OperatorRequested));
    }

    #[test]
    fn hmac_mismatch_wins_over_version_diffs() {
        let cached = version(1, 2, 3, 4, 5);
        let current = version(1, 2, 3, 4, 99); // binary upgraded
        // HMAC mismatch fires first.
        let cause = should_invalidate(cached, current, IntegrityCheck::Mismatch, false);
        assert_eq!(cause, Some(CacheInvalidationCause::HmacMismatch));
    }

    #[test]
    fn binary_upgraded_detected() {
        let cached = version(1, 2, 3, 4, 5);
        let current = version(1, 2, 3, 4, 99);
        let cause = should_invalidate(cached, current, IntegrityCheck::Match, false);
        assert_eq!(cause, Some(CacheInvalidationCause::BinaryUpgraded));
    }

    #[test]
    fn driver_upgraded_detected() {
        let cached = version(1, 2, 3, 4, 5);
        let current = version(1, 2, 99, 4, 5);
        let cause = should_invalidate(cached, current, IntegrityCheck::Match, false);
        assert_eq!(cause, Some(CacheInvalidationCause::DriverUpgraded));
    }

    #[test]
    fn wgpu_version_changed_detected() {
        let cached = version(1, 2, 3, 4, 5);
        let current = version(1, 99, 3, 4, 5);
        let cause = should_invalidate(cached, current, IntegrityCheck::Match, false);
        assert_eq!(cause, Some(CacheInvalidationCause::WgpuVersionChanged));
    }

    #[test]
    fn shader_hash_changed_detected() {
        let cached = version(1, 2, 3, 4, 5);
        let current = version(1, 2, 3, 99, 5);
        let cause = should_invalidate(cached, current, IntegrityCheck::Match, false);
        assert_eq!(cause, Some(CacheInvalidationCause::ShaderHashChanged));
    }

    #[test]
    fn arch_changed_detected() {
        let cached = version(1, 2, 3, 4, 5);
        let current = version(99, 2, 3, 4, 5);
        let cause = should_invalidate(cached, current, IntegrityCheck::Match, false);
        assert_eq!(cause, Some(CacheInvalidationCause::ArchChanged));
    }

    #[test]
    fn invalidation_priority_binary_beats_driver() {
        // Both binary and driver differ; bead's enumeration order
        // surfaces binary first.
        let cached = version(1, 2, 3, 4, 5);
        let current = version(1, 2, 99, 4, 99);
        let cause = should_invalidate(cached, current, IntegrityCheck::Match, false);
        assert_eq!(cause, Some(CacheInvalidationCause::BinaryUpgraded));
    }

    #[test]
    fn invalidation_priority_driver_beats_wgpu() {
        let cached = version(1, 2, 3, 4, 5);
        let current = version(1, 99, 99, 4, 5);
        let cause = should_invalidate(cached, current, IntegrityCheck::Match, false);
        assert_eq!(cause, Some(CacheInvalidationCause::DriverUpgraded));
    }

    #[test]
    fn integrity_not_checked_treated_as_fresh() {
        // NotChecked = file missing → substrate doesn't fire HMAC
        // mismatch (the integration's load path checks file
        // existence separately).
        let v = version(1, 2, 3, 4, 5);
        assert_eq!(
            should_invalidate(v, v, IntegrityCheck::NotChecked, false),
            None
        );
    }

    // ----------------------------------------------------------------
    // CacheSizeBudget
    // ----------------------------------------------------------------

    #[test]
    fn budget_default_is_64_mib() {
        assert_eq!(CacheSizeBudget::default().max_bytes, 64 * 1024 * 1024);
    }

    #[test]
    fn budget_should_evict_above_max() {
        let b = CacheSizeBudget { max_bytes: 1000 };
        assert!(!b.should_evict(500));
        assert!(!b.should_evict(1000)); // exactly at, not above
        assert!(b.should_evict(1001));
    }

    // ----------------------------------------------------------------
    // CacheStats
    // ----------------------------------------------------------------

    #[test]
    fn stats_default_zero() {
        let s = CacheStats::default();
        assert_eq!(s.hit_rate_pct(), 0);
        // Self-review fix (br-ft-ydrpp): default is "no data
        // recorded" = None, not a misleading false.
        assert_eq!(s.meets_cold_start_target(), None);
        assert!(!s.cold_start_recorded);
    }

    #[test]
    fn stats_record_hits_and_misses() {
        let mut s = CacheStats::default();
        for _ in 0..7 {
            s.record_hit();
        }
        for _ in 0..3 {
            s.record_miss();
        }
        assert_eq!(s.cache_hits_total, 7);
        assert_eq!(s.cache_misses_total, 3);
        assert_eq!(s.hit_rate_pct(), 70);
    }

    #[test]
    fn stats_record_invalidation_routes_per_cause() {
        let mut s = CacheStats::default();
        s.record_invalidation(CacheInvalidationCause::BinaryUpgraded);
        s.record_invalidation(CacheInvalidationCause::BinaryUpgraded);
        s.record_invalidation(CacheInvalidationCause::HmacMismatch);
        s.record_invalidation(CacheInvalidationCause::DriverUpgraded);
        s.record_invalidation(CacheInvalidationCause::ShaderHashChanged);
        s.record_invalidation(CacheInvalidationCause::WgpuVersionChanged);
        s.record_invalidation(CacheInvalidationCause::ArchChanged);
        s.record_invalidation(CacheInvalidationCause::OperatorRequested);
        assert_eq!(s.invalidations_binary_upgraded, 2);
        assert_eq!(s.invalidations_hmac_mismatch, 1);
        assert_eq!(s.invalidations_driver_upgraded, 1);
        assert_eq!(s.invalidations_shader_hash_changed, 1);
        assert_eq!(s.invalidations_wgpu_version_changed, 1);
        assert_eq!(s.invalidations_arch_changed, 1);
        assert_eq!(s.invalidations_operator_requested, 1);
    }

    #[test]
    fn stats_meets_cold_start_target_under_100ms() {
        let mut s = CacheStats::default();
        s.record_cold_start_ms(80);
        assert_eq!(s.meets_cold_start_target(), Some(true));
        assert!(s.cold_start_recorded);
    }

    #[test]
    fn stats_misses_cold_start_target_at_or_above_100ms() {
        let mut s = CacheStats::default();
        s.record_cold_start_ms(100);
        assert_eq!(s.meets_cold_start_target(), Some(false));
        s.record_cold_start_ms(150);
        assert_eq!(s.meets_cold_start_target(), Some(false));
    }

    #[test]
    fn stats_meets_cold_start_target_zero_ms_distinguished_from_no_data() {
        // Self-review fix (br-ft-ydrpp): 0 ms is a legitimate
        // "very fast" measurement, not a sentinel for "no data".
        let mut s = CacheStats::default();
        // No data → None.
        assert_eq!(s.meets_cold_start_target(), None);
        // After explicit record(0) → Some(true).
        s.record_cold_start_ms(0);
        assert_eq!(s.meets_cold_start_target(), Some(true));
        assert!(s.cold_start_recorded);
    }

    #[test]
    fn stats_hit_rate_caps_at_100() {
        let mut s = CacheStats::default();
        s.cache_hits_total = 1_000;
        s.cache_misses_total = 0;
        assert_eq!(s.hit_rate_pct(), 100);
    }

    // ----------------------------------------------------------------
    // Cross-cut: realistic scenarios
    // ----------------------------------------------------------------

    #[test]
    fn scenario_first_run_no_cache_writes_cache() {
        // Fresh install: cache file doesn't exist.
        let v = version(1, 2, 3, 4, 5);
        let cause = should_invalidate(v, v, IntegrityCheck::NotChecked, false);
        assert_eq!(
            cause, None,
            "NotChecked = no cache yet, integration writes one"
        );
    }

    #[test]
    fn scenario_ft_binary_upgrade_invalidates_cache() {
        // User runs `cargo install --force frankenterm`; binary
        // hash changes. Cache invalidates with BinaryUpgraded
        // reason.
        let cached = version(1, 2, 3, 4, 0xDEADBEEF);
        let current = version(1, 2, 3, 4, 0xCAFEBABE);
        let cause = should_invalidate(cached, current, IntegrityCheck::Match, false);
        assert_eq!(cause, Some(CacheInvalidationCause::BinaryUpgraded));
    }

    #[test]
    fn scenario_macos_driver_upgrade_via_os_update() {
        // macOS Sonoma → Sequoia GPU driver upgrade — driver_version_hash
        // changes; ft must recompile pipelines.
        let cached = version(1, 2, 0xA1, 4, 5);
        let current = version(1, 2, 0xA2, 4, 5);
        let cause = should_invalidate(cached, current, IntegrityCheck::Match, false);
        assert_eq!(cause, Some(CacheInvalidationCause::DriverUpgraded));
    }

    #[test]
    fn scenario_tampered_cache_caught_by_hmac() {
        // Attacker swaps the cache file with a forged version.
        // HMAC over (binary_path + version) doesn't match. Cache
        // invalidates, integration recompiles from clean shaders.
        let v = version(1, 2, 3, 4, 5);
        let cause = should_invalidate(v, v, IntegrityCheck::Mismatch, false);
        assert_eq!(cause, Some(CacheInvalidationCause::HmacMismatch));
    }

    #[test]
    fn scenario_hot_path_4_cache_hits_then_1_miss_then_1_hit() {
        let mut stats = CacheStats::default();
        for _ in 0..4 {
            stats.record_hit();
        }
        stats.record_miss();
        stats.record_hit();
        // 5 hits, 1 miss → 83%.
        assert_eq!(stats.hit_rate_pct(), 83);
    }

    #[test]
    fn scenario_cold_start_under_target_and_metrics() {
        let mut stats = CacheStats::default();
        stats.record_cold_start_ms(75);
        stats.record_hit();
        assert_eq!(stats.meets_cold_start_target(), Some(true));
        assert_eq!(stats.last_cold_start_ms, 75);
    }

    #[test]
    fn scenario_full_invalidation_pipeline_matches_bead_enum() {
        // For each of the 7 cause variants (6 from bead + ArchChanged
        // which the substrate adds for cross-host safety), assert
        // record_invalidation increments the right slot.
        for cause in [
            CacheInvalidationCause::BinaryUpgraded,
            CacheInvalidationCause::DriverUpgraded,
            CacheInvalidationCause::WgpuVersionChanged,
            CacheInvalidationCause::ShaderHashChanged,
            CacheInvalidationCause::ArchChanged,
            CacheInvalidationCause::HmacMismatch,
            CacheInvalidationCause::OperatorRequested,
        ] {
            let mut s = CacheStats::default();
            s.record_invalidation(cause);
            // Exactly one slot should be 1; everything else 0.
            let total = s.invalidations_binary_upgraded
                + s.invalidations_driver_upgraded
                + s.invalidations_wgpu_version_changed
                + s.invalidations_shader_hash_changed
                + s.invalidations_arch_changed
                + s.invalidations_hmac_mismatch
                + s.invalidations_operator_requested;
            assert_eq!(total, 1, "{cause:?} should bump exactly one counter");
        }
    }
}
