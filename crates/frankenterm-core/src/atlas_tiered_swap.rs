//! GPU atlas → host RAM → disk tiered-swap policy substrate
//! (ft-2okh0.11).
//!
//! Pure-logic substrate for the bead's "VRAM-budget-aware atlas
//! swap" requirement. The integration crate handles the actual
//! GPU buffer blits, host RAM staging, and disk I/O; this module
//! ships the per-region tier tracking, LRU eviction policy,
//! per-tier budget pressure detection, and the cascade decision
//! tree (VRAM → RAM → Disk) under combined memory pressure.
//!
//! ## What this module ships
//!
//! - `AtlasTier` — `Vram / HostRam / Disk` (3-tier cascade per the
//!   bead, with cross-link to ft-2okh0.13 cold-disk tier).
//! - `TieredAtlasRegion` — `{ id, tier, last_access_frame,
//!   bytes }`. LRU-tracked.
//! - `BudgetPressure` — `Nominal / Warning / Critical`. Per-tier
//!   pressure level the integration's memory probe sets.
//! - `MemoryBudget` — operator-tunable `vram_budget_bytes /
//!   host_ram_budget_bytes`. Defaults match modest integrated-GPU
//!   constraints.
//! - `should_evict_from` — pure predicate: should we evict from
//!   this tier given current pressure + size?
//! - `EvictionAction` — `Promote(region, target_tier) /
//!   Demote(region, target_tier) / Evict(region) / NoOp`. The
//!   integration dispatches on this.
//! - `select_eviction_target` — LRU pick: among regions in a tier,
//!   pick the coldest (oldest `last_access_frame`).
//! - `decide_cascade_action` — composes pressure across all 3
//!   tiers into a single `EvictionAction`. Pure-logic decision
//!   tree.
//! - `TierSwapStats` — per-tier swap-in/swap-out counters + peak
//!   bytes for `ft doctor`.
//! - `HostRamStagingBuffer` — allocator-owned host-RAM staging
//!   offset table for warm atlas regions. Integration code owns
//!   the actual bytes; this substrate owns deterministic placement
//!   and accounting.
//!
//! ## What is deferred to the integration bead (ft-2okh0.11.cont)
//!
//! - Actual GPU buffer-blit code (wgpu `Queue::write_texture` /
//!   `read_texture`).
//! - Binding `HostRamStagingBuffer` to a real per-window byte buffer.
//! - Disk-tier I/O (cross-link ft-2okh0.13 async scrollback eviction
//!   pattern).
//! - VRAM-budget probe via `wgpu::Adapter::get_info().backend` +
//!   per-platform memory hints.
//! - Host RAM probe via `sysctl` / `/proc/meminfo` /
//!   `GlobalMemoryStatusEx`.
//! - Integration with stable atlas (cross-link ft-mpc9b.1.1) +
//!   bin-packing (ft-mpc9b.1.4 substrate already shipped).
//! - Frame-budget compliance: swap-in deferred to next frame when
//!   mid-frame would overrun (cross-link ft-mpc9b.5.2 FrameBudget).

#![allow(dead_code)]

// ============================================================================
// Atlas tier
// ============================================================================

/// 3-tier cascade per the bead: VRAM (hot) → HostRam (warm) →
/// Disk (cold). Order matters: lower-numbered = faster access =
/// higher cost.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default)]
pub enum AtlasTier {
    /// GPU VRAM. Reads are 1-frame; writes via Queue::write_texture.
    /// Highest cost; smallest budget on integrated GPUs.
    #[default]
    Vram,
    /// Host RAM staging buffer. Reads cost 1 frame to swap back to
    /// VRAM; writes are cheap (memcpy).
    HostRam,
    /// Disk (file in `~/.cache/ft/atlas/`). Cross-link
    /// ft-2okh0.13 cold-disk-tier for the actual eviction
    /// machinery.
    Disk,
}

impl AtlasTier {
    /// Tier ordinal — lower = hotter.
    #[must_use]
    pub const fn ordinal(self) -> u8 {
        match self {
            Self::Vram => 0,
            Self::HostRam => 1,
            Self::Disk => 2,
        }
    }

    /// Whether this tier is hotter than another.
    #[must_use]
    pub const fn is_hotter_than(self, other: Self) -> bool {
        self.ordinal() < other.ordinal()
    }

    /// Promotion target — the next-hotter tier, or `None` if this
    /// is already VRAM.
    #[must_use]
    pub const fn promotion_target(self) -> Option<Self> {
        match self {
            Self::Vram => None,
            Self::HostRam => Some(Self::Vram),
            Self::Disk => Some(Self::HostRam),
        }
    }

    /// Demotion target — the next-colder tier, or `None` if this
    /// is already Disk.
    #[must_use]
    pub const fn demotion_target(self) -> Option<Self> {
        match self {
            Self::Vram => Some(Self::HostRam),
            Self::HostRam => Some(Self::Disk),
            Self::Disk => None,
        }
    }
}

// ============================================================================
// TieredAtlasRegion
// ============================================================================

/// Per-region atlas-cache record. The integration's atlas keeps a
/// registry of these; this substrate's eviction policy reads them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TieredAtlasRegion {
    /// Stable region identifier (the integration's atlas-key hash).
    pub id: u64,
    pub tier: AtlasTier,
    /// Frame index when this region was last accessed. The
    /// integration's tick scheduler increments a global frame
    /// counter and tags the region on every read.
    pub last_access_frame: u64,
    /// Region size in bytes (the GPU texel count × bytes-per-texel).
    pub bytes: u64,
}

impl TieredAtlasRegion {
    #[must_use]
    pub fn new(id: u64, tier: AtlasTier, last_access_frame: u64, bytes: u64) -> Self {
        Self {
            id,
            tier,
            last_access_frame,
            bytes,
        }
    }

    /// Mark this region as accessed at `frame`. Pure-logic mutation.
    pub fn touch(&mut self, frame: u64) {
        self.last_access_frame = frame;
    }

    /// Frames since last access (for the LRU policy). Saturating
    /// subtraction so out-of-order frame indices don't underflow.
    #[must_use]
    pub fn idle_frames(&self, current_frame: u64) -> u64 {
        current_frame.saturating_sub(self.last_access_frame)
    }
}

// ============================================================================
// Budget + pressure
// ============================================================================

/// Per-tier pressure level. The integration's memory probe sets
/// this each frame; the substrate's decision tree reads it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default)]
pub enum BudgetPressure {
    /// Plenty of budget left; no eviction needed.
    #[default]
    Nominal,
    /// 80%+ of budget consumed; start opportunistic demotion.
    Warning,
    /// 95%+ of budget consumed; aggressive eviction required.
    Critical,
}

/// Operator-tunable per-tier byte budgets. Defaults are
/// conservative for integrated GPUs; tuning_config can override.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MemoryBudget {
    /// VRAM budget — cap on the hottest tier. Default 256 MiB
    /// (matches an integrated GPU sharing 8 GB system RAM).
    pub vram_budget_bytes: u64,
    /// Host RAM budget for the staging buffer. Default 1 GiB.
    pub host_ram_budget_bytes: u64,
    /// Pressure thresholds as percentages of budget. Warning fires
    /// at `warning_pct` consumed; Critical at `critical_pct`.
    pub warning_pct: u8,
    pub critical_pct: u8,
}

pub const DEFAULT_VRAM_BUDGET_BYTES: u64 = 256 * 1024 * 1024;
pub const DEFAULT_HOST_RAM_BUDGET_BYTES: u64 = 1024 * 1024 * 1024;
pub const DEFAULT_WARNING_PCT: u8 = 80;
pub const DEFAULT_CRITICAL_PCT: u8 = 95;

impl Default for MemoryBudget {
    fn default() -> Self {
        Self {
            vram_budget_bytes: DEFAULT_VRAM_BUDGET_BYTES,
            host_ram_budget_bytes: DEFAULT_HOST_RAM_BUDGET_BYTES,
            warning_pct: DEFAULT_WARNING_PCT,
            critical_pct: DEFAULT_CRITICAL_PCT,
        }
    }
}

/// Compute pressure for a tier given current usage + budget.
/// Pure-math; the integration's probe feeds in `current_bytes`.
#[must_use]
pub fn compute_pressure(
    current_bytes: u64,
    budget_bytes: u64,
    warning_pct: u8,
    critical_pct: u8,
) -> BudgetPressure {
    if budget_bytes == 0 {
        return BudgetPressure::Critical;
    }
    let pct = (current_bytes.saturating_mul(100)) / budget_bytes;
    if pct >= u64::from(critical_pct) {
        BudgetPressure::Critical
    } else if pct >= u64::from(warning_pct) {
        BudgetPressure::Warning
    } else {
        BudgetPressure::Nominal
    }
}

/// Whether a tier needs eviction this frame. `Critical` always
/// returns true; `Warning` returns true only if there's at least
/// one cold region (idle for `min_idle_frames`).
#[must_use]
pub fn should_evict_from(pressure: BudgetPressure, has_cold_regions: bool) -> bool {
    match pressure {
        BudgetPressure::Critical => true,
        BudgetPressure::Warning => has_cold_regions,
        BudgetPressure::Nominal => false,
    }
}

// ============================================================================
// Host RAM staging buffer
// ============================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct HostRamStagingAllocation {
    pub region_id: u64,
    pub offset: u64,
    pub bytes: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HostRamStagingError {
    ZeroSizedRegion {
        region_id: u64,
    },
    RegionTooLarge {
        region_id: u64,
        bytes: u64,
        capacity: u64,
    },
    RegionAlreadyStaged {
        region_id: u64,
    },
    OutOfSpace {
        region_id: u64,
        bytes: u64,
        available: u64,
    },
    UnknownRegion {
        region_id: u64,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct HostRamFreeSpan {
    offset: u64,
    bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostRamStagingBuffer {
    capacity_bytes: u64,
    used_bytes: u64,
    allocations: Vec<HostRamStagingAllocation>,
    free_spans: Vec<HostRamFreeSpan>,
}

impl HostRamStagingBuffer {
    #[must_use]
    pub fn new(capacity_bytes: u64) -> Self {
        let free_spans = if capacity_bytes == 0 {
            Vec::new()
        } else {
            vec![HostRamFreeSpan {
                offset: 0,
                bytes: capacity_bytes,
            }]
        };
        Self {
            capacity_bytes,
            used_bytes: 0,
            allocations: Vec::new(),
            free_spans,
        }
    }

    #[must_use]
    pub const fn capacity_bytes(&self) -> u64 {
        self.capacity_bytes
    }

    #[must_use]
    pub const fn used_bytes(&self) -> u64 {
        self.used_bytes
    }

    #[must_use]
    pub fn available_bytes(&self) -> u64 {
        self.capacity_bytes.saturating_sub(self.used_bytes)
    }

    #[must_use]
    pub fn allocation_for(&self, region_id: u64) -> Option<HostRamStagingAllocation> {
        self.allocations
            .iter()
            .find(|allocation| allocation.region_id == region_id)
            .copied()
    }

    #[must_use]
    pub fn pressure(&self, budget: MemoryBudget) -> BudgetPressure {
        let effective_budget = self.capacity_bytes.min(budget.host_ram_budget_bytes);
        compute_pressure(
            self.used_bytes,
            effective_budget,
            budget.warning_pct,
            budget.critical_pct,
        )
    }

    pub fn stage_region(
        &mut self,
        region_id: u64,
        bytes: u64,
    ) -> Result<HostRamStagingAllocation, HostRamStagingError> {
        if bytes == 0 {
            return Err(HostRamStagingError::ZeroSizedRegion { region_id });
        }
        if bytes > self.capacity_bytes {
            return Err(HostRamStagingError::RegionTooLarge {
                region_id,
                bytes,
                capacity: self.capacity_bytes,
            });
        }
        if self.allocation_for(region_id).is_some() {
            return Err(HostRamStagingError::RegionAlreadyStaged { region_id });
        }

        let Some(span_index) = self.free_spans.iter().position(|span| span.bytes >= bytes) else {
            return Err(HostRamStagingError::OutOfSpace {
                region_id,
                bytes,
                available: self.available_bytes(),
            });
        };

        let span = &mut self.free_spans[span_index];
        let allocation = HostRamStagingAllocation {
            region_id,
            offset: span.offset,
            bytes,
        };
        span.offset = span.offset.saturating_add(bytes);
        span.bytes = span.bytes.saturating_sub(bytes);
        if span.bytes == 0 {
            self.free_spans.remove(span_index);
        }
        self.used_bytes = self.used_bytes.saturating_add(bytes);
        self.allocations.push(allocation);
        Ok(allocation)
    }

    pub fn release_region(
        &mut self,
        region_id: u64,
    ) -> Result<HostRamStagingAllocation, HostRamStagingError> {
        let Some(index) = self
            .allocations
            .iter()
            .position(|allocation| allocation.region_id == region_id)
        else {
            return Err(HostRamStagingError::UnknownRegion { region_id });
        };
        let allocation = self.allocations.swap_remove(index);
        self.used_bytes = self.used_bytes.saturating_sub(allocation.bytes);
        self.insert_free_span(HostRamFreeSpan {
            offset: allocation.offset,
            bytes: allocation.bytes,
        });
        Ok(allocation)
    }

    fn insert_free_span(&mut self, span: HostRamFreeSpan) {
        if span.bytes == 0 {
            return;
        }
        self.free_spans.push(span);
        self.free_spans.sort_by_key(|span| span.offset);

        let mut merged: Vec<HostRamFreeSpan> = Vec::with_capacity(self.free_spans.len());
        for span in self.free_spans.drain(..) {
            if let Some(last) = merged.last_mut() {
                let last_end = last.offset.saturating_add(last.bytes);
                if last_end >= span.offset {
                    let span_end = span.offset.saturating_add(span.bytes);
                    last.bytes = span_end.saturating_sub(last.offset).max(last.bytes);
                    continue;
                }
            }
            merged.push(span);
        }
        self.free_spans = merged;
    }
}

// ============================================================================
// LRU target selection
// ============================================================================

/// Pick the coldest region in `tier` from `candidates`. Returns
/// the region with the largest `idle_frames`; `None` if no
/// candidates in that tier.
///
/// On ties, lower `id` wins for determinism (operator-visible
/// telemetry stays stable across runs).
#[must_use]
pub fn select_eviction_target(
    candidates: &[TieredAtlasRegion],
    tier: AtlasTier,
    current_frame: u64,
) -> Option<TieredAtlasRegion> {
    candidates
        .iter()
        .filter(|r| r.tier == tier)
        .copied()
        .max_by(|a, b| {
            a.idle_frames(current_frame)
                .cmp(&b.idle_frames(current_frame))
                .then_with(|| b.id.cmp(&a.id))
        })
}

// ============================================================================
// Eviction action
// ============================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EvictionAction {
    /// Move region to a hotter tier (e.g. swap-in from HostRam to
    /// Vram).
    Promote {
        region_id: u64,
        from: AtlasTier,
        to: AtlasTier,
    },
    /// Move region to a colder tier (e.g. swap-out from Vram to
    /// HostRam under VRAM pressure).
    Demote {
        region_id: u64,
        from: AtlasTier,
        to: AtlasTier,
    },
    /// Drop the region entirely (already on Disk and disk pressure
    /// hit Critical, OR region size exceeds the disk budget).
    Evict { region_id: u64, from: AtlasTier },
    /// No action this frame.
    NoOp,
}

/// Decide the next eviction action given the current pressure
/// across all 3 tiers + the candidate region set. Pure-logic.
///
/// Decision tree:
/// 1. If VRAM pressure is Critical, demote the coldest VRAM
///    region to HostRam.
/// 2. Else if HostRam pressure is Critical, demote the coldest
///    HostRam region to Disk.
/// 3. Else if Disk is Critical, evict the coldest Disk region
///    entirely.
/// 4. Else if VRAM pressure is Warning AND at least one cold VRAM
///    region exists, opportunistic demote.
/// 5. Otherwise NoOp.
#[must_use]
pub fn decide_cascade_action(
    candidates: &[TieredAtlasRegion],
    current_frame: u64,
    vram_pressure: BudgetPressure,
    host_ram_pressure: BudgetPressure,
    disk_pressure: BudgetPressure,
) -> EvictionAction {
    // Critical at the hottest tier wins first.
    if matches!(vram_pressure, BudgetPressure::Critical) {
        if let Some(target) = select_eviction_target(candidates, AtlasTier::Vram, current_frame) {
            return EvictionAction::Demote {
                region_id: target.id,
                from: AtlasTier::Vram,
                to: AtlasTier::HostRam,
            };
        }
    }
    if matches!(host_ram_pressure, BudgetPressure::Critical) {
        if let Some(target) = select_eviction_target(candidates, AtlasTier::HostRam, current_frame)
        {
            return EvictionAction::Demote {
                region_id: target.id,
                from: AtlasTier::HostRam,
                to: AtlasTier::Disk,
            };
        }
    }
    if matches!(disk_pressure, BudgetPressure::Critical) {
        if let Some(target) = select_eviction_target(candidates, AtlasTier::Disk, current_frame) {
            return EvictionAction::Evict {
                region_id: target.id,
                from: AtlasTier::Disk,
            };
        }
    }
    // Opportunistic demote on Warning.
    if matches!(vram_pressure, BudgetPressure::Warning) {
        let target = select_eviction_target(candidates, AtlasTier::Vram, current_frame);
        if let Some(t) = target {
            // Only demote if the target is genuinely cold (idle
            // ≥ min threshold). The integration tunes the threshold;
            // the substrate uses 60 frames (≈ 1 second at 60Hz) as
            // a sane default.
            const MIN_IDLE_FRAMES: u64 = 60;
            if t.idle_frames(current_frame) >= MIN_IDLE_FRAMES {
                return EvictionAction::Demote {
                    region_id: t.id,
                    from: AtlasTier::Vram,
                    to: AtlasTier::HostRam,
                };
            }
        }
    }
    EvictionAction::NoOp
}

// ============================================================================
// Stats
// ============================================================================

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TierSwapStats {
    /// VRAM peak bytes observed.
    pub vram_peak_bytes: u64,
    /// Host RAM peak bytes observed.
    pub host_ram_peak_bytes: u64,
    /// Swap-in counters: region moved to a hotter tier.
    pub vram_swap_in_count: u64,
    pub host_ram_swap_in_count: u64,
    /// Swap-out counters: region moved to a colder tier.
    pub vram_swap_out_count: u64,
    pub host_ram_swap_out_count: u64,
    /// Region dropped entirely (cache miss next time).
    pub disk_eviction_count: u64,
    /// Total bytes moved across all swap ops.
    pub swap_total_bytes: u64,
}

impl TierSwapStats {
    pub fn record_action(&mut self, action: EvictionAction, region_bytes: u64) {
        match action {
            EvictionAction::Promote { to, .. } => {
                match to {
                    AtlasTier::Vram => {
                        self.vram_swap_in_count = self.vram_swap_in_count.saturating_add(1);
                    }
                    AtlasTier::HostRam => {
                        self.host_ram_swap_in_count = self.host_ram_swap_in_count.saturating_add(1);
                    }
                    AtlasTier::Disk => {}
                }
                self.swap_total_bytes = self.swap_total_bytes.saturating_add(region_bytes);
            }
            EvictionAction::Demote { from, .. } => {
                match from {
                    AtlasTier::Vram => {
                        self.vram_swap_out_count = self.vram_swap_out_count.saturating_add(1);
                    }
                    AtlasTier::HostRam => {
                        self.host_ram_swap_out_count =
                            self.host_ram_swap_out_count.saturating_add(1);
                    }
                    AtlasTier::Disk => {}
                }
                self.swap_total_bytes = self.swap_total_bytes.saturating_add(region_bytes);
            }
            EvictionAction::Evict { .. } => {
                self.disk_eviction_count = self.disk_eviction_count.saturating_add(1);
            }
            EvictionAction::NoOp => {}
        }
    }

    pub fn record_peak(&mut self, vram_bytes: u64, host_ram_bytes: u64) {
        if vram_bytes > self.vram_peak_bytes {
            self.vram_peak_bytes = vram_bytes;
        }
        if host_ram_bytes > self.host_ram_peak_bytes {
            self.host_ram_peak_bytes = host_ram_bytes;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn region(id: u64, tier: AtlasTier, last_frame: u64, bytes: u64) -> TieredAtlasRegion {
        TieredAtlasRegion::new(id, tier, last_frame, bytes)
    }

    // ----------------------------------------------------------------
    // AtlasTier
    // ----------------------------------------------------------------

    #[test]
    fn tier_default_is_vram() {
        assert_eq!(AtlasTier::default(), AtlasTier::Vram);
    }

    #[test]
    fn tier_ordinal_orders_hot_to_cold() {
        assert!(AtlasTier::Vram.ordinal() < AtlasTier::HostRam.ordinal());
        assert!(AtlasTier::HostRam.ordinal() < AtlasTier::Disk.ordinal());
    }

    #[test]
    fn tier_is_hotter_than() {
        assert!(AtlasTier::Vram.is_hotter_than(AtlasTier::HostRam));
        assert!(AtlasTier::HostRam.is_hotter_than(AtlasTier::Disk));
        assert!(!AtlasTier::Disk.is_hotter_than(AtlasTier::Vram));
    }

    #[test]
    fn tier_promotion_target_walks_up() {
        assert_eq!(AtlasTier::Disk.promotion_target(), Some(AtlasTier::HostRam));
        assert_eq!(AtlasTier::HostRam.promotion_target(), Some(AtlasTier::Vram));
        assert_eq!(AtlasTier::Vram.promotion_target(), None);
    }

    #[test]
    fn tier_demotion_target_walks_down() {
        assert_eq!(AtlasTier::Vram.demotion_target(), Some(AtlasTier::HostRam));
        assert_eq!(AtlasTier::HostRam.demotion_target(), Some(AtlasTier::Disk));
        assert_eq!(AtlasTier::Disk.demotion_target(), None);
    }

    // ----------------------------------------------------------------
    // TieredAtlasRegion
    // ----------------------------------------------------------------

    #[test]
    fn region_touch_updates_last_access() {
        let mut r = region(1, AtlasTier::Vram, 100, 1024);
        r.touch(200);
        assert_eq!(r.last_access_frame, 200);
    }

    #[test]
    fn region_idle_frames_saturating() {
        let r = region(1, AtlasTier::Vram, 100, 1024);
        assert_eq!(r.idle_frames(150), 50);
        // Out-of-order frame index shouldn't underflow.
        assert_eq!(r.idle_frames(50), 0);
    }

    // ----------------------------------------------------------------
    // BudgetPressure + compute_pressure
    // ----------------------------------------------------------------

    #[test]
    fn pressure_default_is_nominal() {
        assert_eq!(BudgetPressure::default(), BudgetPressure::Nominal);
    }

    #[test]
    fn compute_pressure_at_boundaries() {
        // 80% = Warning, 95% = Critical, with default thresholds.
        assert_eq!(compute_pressure(0, 100, 80, 95), BudgetPressure::Nominal);
        assert_eq!(compute_pressure(50, 100, 80, 95), BudgetPressure::Nominal);
        assert_eq!(compute_pressure(79, 100, 80, 95), BudgetPressure::Nominal);
        assert_eq!(compute_pressure(80, 100, 80, 95), BudgetPressure::Warning);
        assert_eq!(compute_pressure(94, 100, 80, 95), BudgetPressure::Warning);
        assert_eq!(compute_pressure(95, 100, 80, 95), BudgetPressure::Critical);
        assert_eq!(compute_pressure(120, 100, 80, 95), BudgetPressure::Critical);
    }

    #[test]
    fn compute_pressure_zero_budget_critical() {
        assert_eq!(compute_pressure(50, 0, 80, 95), BudgetPressure::Critical);
    }

    #[test]
    fn compute_pressure_zero_usage_nominal() {
        assert_eq!(compute_pressure(0, 1000, 80, 95), BudgetPressure::Nominal);
    }

    // ----------------------------------------------------------------
    // should_evict_from
    // ----------------------------------------------------------------

    #[test]
    fn evict_critical_always() {
        assert!(should_evict_from(BudgetPressure::Critical, false));
        assert!(should_evict_from(BudgetPressure::Critical, true));
    }

    #[test]
    fn evict_warning_only_with_cold() {
        assert!(!should_evict_from(BudgetPressure::Warning, false));
        assert!(should_evict_from(BudgetPressure::Warning, true));
    }

    #[test]
    fn evict_nominal_never() {
        assert!(!should_evict_from(BudgetPressure::Nominal, false));
        assert!(!should_evict_from(BudgetPressure::Nominal, true));
    }

    // ----------------------------------------------------------------
    // MemoryBudget
    // ----------------------------------------------------------------

    #[test]
    fn budget_default_matches_constants() {
        let b = MemoryBudget::default();
        assert_eq!(b.vram_budget_bytes, 256 * 1024 * 1024);
        assert_eq!(b.host_ram_budget_bytes, 1024 * 1024 * 1024);
        assert_eq!(b.warning_pct, 80);
        assert_eq!(b.critical_pct, 95);
    }

    // ----------------------------------------------------------------
    // HostRamStagingBuffer
    // ----------------------------------------------------------------

    #[test]
    fn host_ram_staging_starts_empty_with_single_free_span() {
        let staging = HostRamStagingBuffer::new(4096);

        assert_eq!(staging.capacity_bytes(), 4096);
        assert_eq!(staging.used_bytes(), 0);
        assert_eq!(staging.available_bytes(), 4096);
        assert_eq!(
            staging.free_spans,
            vec![HostRamFreeSpan {
                offset: 0,
                bytes: 4096
            }]
        );
    }

    #[test]
    fn host_ram_staging_allocates_first_fit_offsets() {
        let mut staging = HostRamStagingBuffer::new(4096);

        let first = staging.stage_region(10, 1024).unwrap();
        let second = staging.stage_region(20, 512).unwrap();

        assert_eq!(
            first,
            HostRamStagingAllocation {
                region_id: 10,
                offset: 0,
                bytes: 1024
            }
        );
        assert_eq!(
            second,
            HostRamStagingAllocation {
                region_id: 20,
                offset: 1024,
                bytes: 512
            }
        );
        assert_eq!(staging.used_bytes(), 1536);
        assert_eq!(staging.available_bytes(), 2560);
        assert_eq!(staging.allocation_for(10), Some(first));
    }

    #[test]
    fn host_ram_staging_rejects_invalid_allocations() {
        let mut staging = HostRamStagingBuffer::new(1024);

        assert_eq!(
            staging.stage_region(1, 0),
            Err(HostRamStagingError::ZeroSizedRegion { region_id: 1 })
        );
        assert_eq!(
            staging.stage_region(2, 2048),
            Err(HostRamStagingError::RegionTooLarge {
                region_id: 2,
                bytes: 2048,
                capacity: 1024,
            })
        );
        staging.stage_region(3, 256).unwrap();
        assert_eq!(
            staging.stage_region(3, 128),
            Err(HostRamStagingError::RegionAlreadyStaged { region_id: 3 })
        );
    }

    #[test]
    fn host_ram_staging_reports_out_of_space_with_available_bytes() {
        let mut staging = HostRamStagingBuffer::new(1024);
        staging.stage_region(1, 768).unwrap();

        assert_eq!(
            staging.stage_region(2, 512),
            Err(HostRamStagingError::OutOfSpace {
                region_id: 2,
                bytes: 512,
                available: 256,
            })
        );
    }

    #[test]
    fn host_ram_staging_releases_and_reuses_first_fit_span() {
        let mut staging = HostRamStagingBuffer::new(4096);
        staging.stage_region(1, 1024).unwrap();
        let released = staging.release_region(1).unwrap();
        let reused = staging.stage_region(2, 512).unwrap();

        assert_eq!(released.region_id, 1);
        assert_eq!(released.offset, 0);
        assert_eq!(reused.offset, 0);
        assert_eq!(staging.used_bytes(), 512);
    }

    #[test]
    fn host_ram_staging_merges_adjacent_free_spans() {
        let mut staging = HostRamStagingBuffer::new(4096);
        staging.stage_region(1, 1024).unwrap();
        staging.stage_region(2, 1024).unwrap();
        staging.stage_region(3, 1024).unwrap();

        staging.release_region(2).unwrap();
        staging.release_region(1).unwrap();

        assert_eq!(
            staging.free_spans,
            vec![
                HostRamFreeSpan {
                    offset: 0,
                    bytes: 2048,
                },
                HostRamFreeSpan {
                    offset: 3072,
                    bytes: 1024,
                },
            ]
        );
    }

    #[test]
    fn host_ram_staging_rejects_unknown_release() {
        let mut staging = HostRamStagingBuffer::new(4096);

        assert_eq!(
            staging.release_region(99),
            Err(HostRamStagingError::UnknownRegion { region_id: 99 })
        );
    }

    #[test]
    fn host_ram_staging_pressure_uses_effective_capacity() {
        let mut staging = HostRamStagingBuffer::new(1000);
        staging.stage_region(1, 800).unwrap();

        assert_eq!(
            staging.pressure(MemoryBudget::default()),
            BudgetPressure::Warning
        );
        staging.stage_region(2, 150).unwrap();
        assert_eq!(
            staging.pressure(MemoryBudget::default()),
            BudgetPressure::Critical
        );
    }

    // ----------------------------------------------------------------
    // select_eviction_target (LRU)
    // ----------------------------------------------------------------

    #[test]
    fn select_picks_oldest_in_tier() {
        let candidates = vec![
            region(1, AtlasTier::Vram, 100, 1024),
            region(2, AtlasTier::Vram, 50, 1024),
            region(3, AtlasTier::Vram, 200, 1024),
        ];
        let target = select_eviction_target(&candidates, AtlasTier::Vram, 300).unwrap();
        // Region 2 has last_access_frame 50, idle_frames(300) = 250 (oldest).
        assert_eq!(target.id, 2);
    }

    #[test]
    fn select_filters_by_tier() {
        let candidates = vec![
            region(1, AtlasTier::Vram, 100, 1024),
            region(2, AtlasTier::HostRam, 50, 1024), // older but wrong tier
            region(3, AtlasTier::Vram, 200, 1024),
        ];
        let target = select_eviction_target(&candidates, AtlasTier::Vram, 300).unwrap();
        assert_eq!(target.id, 1); // VRAM region with idle=200 (oldest VRAM)
    }

    #[test]
    fn select_returns_none_when_no_candidates_in_tier() {
        let candidates = vec![region(1, AtlasTier::Vram, 100, 1024)];
        assert_eq!(
            select_eviction_target(&candidates, AtlasTier::HostRam, 300),
            None
        );
    }

    #[test]
    fn select_breaks_ties_by_lower_id_for_determinism() {
        // Same last_access_frame for both candidates.
        let candidates = vec![
            region(5, AtlasTier::Vram, 100, 1024),
            region(2, AtlasTier::Vram, 100, 1024),
            region(8, AtlasTier::Vram, 100, 1024),
        ];
        let target = select_eviction_target(&candidates, AtlasTier::Vram, 200).unwrap();
        // Tie-break: lower id wins (id 2).
        assert_eq!(target.id, 2);
    }

    // ----------------------------------------------------------------
    // decide_cascade_action
    // ----------------------------------------------------------------

    #[test]
    fn cascade_no_pressure_no_op() {
        let candidates = vec![region(1, AtlasTier::Vram, 0, 1024)];
        let action = decide_cascade_action(
            &candidates,
            100,
            BudgetPressure::Nominal,
            BudgetPressure::Nominal,
            BudgetPressure::Nominal,
        );
        assert_eq!(action, EvictionAction::NoOp);
    }

    #[test]
    fn cascade_vram_critical_demotes_to_host_ram() {
        let candidates = vec![region(1, AtlasTier::Vram, 0, 1024)];
        let action = decide_cascade_action(
            &candidates,
            100,
            BudgetPressure::Critical,
            BudgetPressure::Nominal,
            BudgetPressure::Nominal,
        );
        assert_eq!(
            action,
            EvictionAction::Demote {
                region_id: 1,
                from: AtlasTier::Vram,
                to: AtlasTier::HostRam,
            }
        );
    }

    #[test]
    fn cascade_host_ram_critical_demotes_to_disk() {
        let candidates = vec![region(2, AtlasTier::HostRam, 0, 1024)];
        let action = decide_cascade_action(
            &candidates,
            100,
            BudgetPressure::Nominal,
            BudgetPressure::Critical,
            BudgetPressure::Nominal,
        );
        assert_eq!(
            action,
            EvictionAction::Demote {
                region_id: 2,
                from: AtlasTier::HostRam,
                to: AtlasTier::Disk,
            }
        );
    }

    #[test]
    fn cascade_disk_critical_evicts() {
        let candidates = vec![region(3, AtlasTier::Disk, 0, 1024)];
        let action = decide_cascade_action(
            &candidates,
            100,
            BudgetPressure::Nominal,
            BudgetPressure::Nominal,
            BudgetPressure::Critical,
        );
        assert_eq!(
            action,
            EvictionAction::Evict {
                region_id: 3,
                from: AtlasTier::Disk,
            }
        );
    }

    #[test]
    fn cascade_priority_vram_critical_beats_disk_critical() {
        // Both tiers Critical — VRAM wins (hottest tier first).
        let candidates = vec![
            region(1, AtlasTier::Vram, 0, 1024),
            region(2, AtlasTier::Disk, 0, 1024),
        ];
        let action = decide_cascade_action(
            &candidates,
            100,
            BudgetPressure::Critical,
            BudgetPressure::Nominal,
            BudgetPressure::Critical,
        );
        // VRAM's Critical fires first.
        assert!(matches!(
            action,
            EvictionAction::Demote {
                from: AtlasTier::Vram,
                ..
            }
        ));
    }

    #[test]
    fn cascade_warning_demotes_only_for_cold_region() {
        // VRAM Warning + region idle 100 frames (≥ 60 threshold) → demote.
        let candidates = vec![region(1, AtlasTier::Vram, 0, 1024)];
        let action = decide_cascade_action(
            &candidates,
            100,
            BudgetPressure::Warning,
            BudgetPressure::Nominal,
            BudgetPressure::Nominal,
        );
        assert!(matches!(
            action,
            EvictionAction::Demote { region_id: 1, .. }
        ));
    }

    #[test]
    fn cascade_warning_skips_warm_regions() {
        // VRAM Warning + region idle only 30 frames (< 60 threshold) → NoOp.
        let candidates = vec![region(1, AtlasTier::Vram, 70, 1024)];
        let action = decide_cascade_action(
            &candidates,
            100,
            BudgetPressure::Warning,
            BudgetPressure::Nominal,
            BudgetPressure::Nominal,
        );
        assert_eq!(action, EvictionAction::NoOp);
    }

    #[test]
    fn cascade_critical_with_no_candidates_in_tier_falls_through() {
        // VRAM Critical but no VRAM regions — substrate doesn't
        // crash; falls through to next-pressure check.
        let candidates = vec![region(1, AtlasTier::HostRam, 0, 1024)];
        let action = decide_cascade_action(
            &candidates,
            100,
            BudgetPressure::Critical,
            BudgetPressure::Critical,
            BudgetPressure::Nominal,
        );
        // Falls through to HostRam Critical demote.
        assert_eq!(
            action,
            EvictionAction::Demote {
                region_id: 1,
                from: AtlasTier::HostRam,
                to: AtlasTier::Disk,
            }
        );
    }

    // ----------------------------------------------------------------
    // TierSwapStats
    // ----------------------------------------------------------------

    #[test]
    fn stats_default_zero() {
        let s = TierSwapStats::default();
        assert_eq!(s.vram_swap_in_count, 0);
        assert_eq!(s.swap_total_bytes, 0);
    }

    #[test]
    fn stats_record_demote_from_vram() {
        let mut s = TierSwapStats::default();
        s.record_action(
            EvictionAction::Demote {
                region_id: 1,
                from: AtlasTier::Vram,
                to: AtlasTier::HostRam,
            },
            2048,
        );
        assert_eq!(s.vram_swap_out_count, 1);
        assert_eq!(s.swap_total_bytes, 2048);
    }

    #[test]
    fn stats_record_promote_to_vram() {
        let mut s = TierSwapStats::default();
        s.record_action(
            EvictionAction::Promote {
                region_id: 1,
                from: AtlasTier::HostRam,
                to: AtlasTier::Vram,
            },
            2048,
        );
        assert_eq!(s.vram_swap_in_count, 1);
        assert_eq!(s.swap_total_bytes, 2048);
    }

    #[test]
    fn stats_record_disk_eviction() {
        let mut s = TierSwapStats::default();
        s.record_action(
            EvictionAction::Evict {
                region_id: 1,
                from: AtlasTier::Disk,
            },
            2048,
        );
        assert_eq!(s.disk_eviction_count, 1);
    }

    #[test]
    fn stats_record_peak_takes_max() {
        let mut s = TierSwapStats::default();
        s.record_peak(1000, 5000);
        s.record_peak(500, 6000);
        s.record_peak(2000, 4000);
        assert_eq!(s.vram_peak_bytes, 2000);
        assert_eq!(s.host_ram_peak_bytes, 6000);
    }

    // ----------------------------------------------------------------
    // Cross-cut scenarios
    // ----------------------------------------------------------------

    #[test]
    fn scenario_cjk_emoji_corpus_vram_pressure_demotes_oldest() {
        // 50k glyphs in VRAM, integrated GPU sharing 8GB system RAM.
        // VRAM hits Critical; substrate demotes the coldest glyph first.
        let candidates = vec![
            region(1, AtlasTier::Vram, 1000, 4096), // recently used
            region(2, AtlasTier::Vram, 100, 4096),  // cold (oldest)
            region(3, AtlasTier::Vram, 800, 4096),  // moderately recent
        ];
        let action = decide_cascade_action(
            &candidates,
            2000,
            BudgetPressure::Critical,
            BudgetPressure::Nominal,
            BudgetPressure::Nominal,
        );
        assert!(matches!(
            action,
            EvictionAction::Demote {
                region_id: 2,
                from: AtlasTier::Vram,
                to: AtlasTier::HostRam,
            }
        ));
    }

    #[test]
    fn scenario_multi_window_ft_shares_budget_via_cascade() {
        // 4 ft windows competing for VRAM. Both VRAM and HostRam
        // hit Critical simultaneously. Substrate handles the
        // cascade: demote VRAM region first (hottest tier), let
        // the next frame handle HostRam if still pressured.
        let candidates = vec![
            region(1, AtlasTier::Vram, 0, 4096),
            region(2, AtlasTier::HostRam, 0, 4096),
        ];
        let action = decide_cascade_action(
            &candidates,
            500,
            BudgetPressure::Critical,
            BudgetPressure::Critical,
            BudgetPressure::Nominal,
        );
        // VRAM wins.
        assert!(matches!(
            action,
            EvictionAction::Demote {
                region_id: 1,
                from: AtlasTier::Vram,
                to: AtlasTier::HostRam,
            }
        ));
    }

    #[test]
    fn scenario_disk_full_drops_coldest_disk_region() {
        // All 3 tiers Critical. The cascade walks VRAM → HostRam
        // → Disk; with only Disk regions, it evicts entirely.
        let candidates = vec![
            region(7, AtlasTier::Disk, 100, 4096),
            region(8, AtlasTier::Disk, 50, 4096), // older
            region(9, AtlasTier::Disk, 200, 4096),
        ];
        let action = decide_cascade_action(
            &candidates,
            300,
            BudgetPressure::Critical,
            BudgetPressure::Critical,
            BudgetPressure::Critical,
        );
        // VRAM Critical fires first but no VRAM candidates — falls
        // through. HostRam same. Disk Critical → evict region 8 (oldest).
        assert!(matches!(
            action,
            EvictionAction::Evict {
                region_id: 8,
                from: AtlasTier::Disk,
            }
        ));
    }

    #[test]
    fn scenario_warm_session_stays_in_vram_no_eviction() {
        // Active typing session — all glyphs touched recently.
        // Even at Warning pressure, no demotion fires because
        // none are idle ≥ 60 frames.
        let candidates = vec![
            region(1, AtlasTier::Vram, 95, 4096), // idle 5 frames
            region(2, AtlasTier::Vram, 90, 4096), // idle 10 frames
            region(3, AtlasTier::Vram, 80, 4096), // idle 20 frames
        ];
        let action = decide_cascade_action(
            &candidates,
            100,
            BudgetPressure::Warning,
            BudgetPressure::Nominal,
            BudgetPressure::Nominal,
        );
        assert_eq!(action, EvictionAction::NoOp);
    }

    #[test]
    fn scenario_idle_session_warning_demotes_progressively() {
        // After 10 minutes idle, regions are stale; Warning fires
        // for the coldest one.
        let candidates = vec![
            region(1, AtlasTier::Vram, 0, 4096),     // idle 36000 frames
            region(2, AtlasTier::Vram, 35900, 4096), // idle 100 frames
        ];
        let action = decide_cascade_action(
            &candidates,
            36000,
            BudgetPressure::Warning,
            BudgetPressure::Nominal,
            BudgetPressure::Nominal,
        );
        // The very-oldest region demotes first.
        assert!(matches!(
            action,
            EvictionAction::Demote {
                region_id: 1,
                from: AtlasTier::Vram,
                to: AtlasTier::HostRam,
            }
        ));
    }
}
