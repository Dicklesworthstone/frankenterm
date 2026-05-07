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

/// Direction of a staging transfer per br-ft-ktd19.1
/// substrate-pass.
///
/// `Demote`: VRAM → host staging (region was evicted under
/// budget pressure; allocation is now ready for the wgpu
/// download dispatch to copy the GPU-side bytes into the
/// staging slot).
///
/// `Promote`: staging → VRAM (region is being swapped back in
/// for a near-future render; allocation carries the staging
/// offset the wgpu upload dispatch should read from).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum StagingTransferDirection {
    /// VRAM → host staging.
    Demote,
    /// Staging → VRAM.
    Promote,
}

/// Per-transfer event the wgpu copy layer consumes.
///
/// br-ft-ktd19.1 substrate-pass: bridges the in-core
/// [`HostRamStagingBuffer`] state changes to the wgpu copy-
/// command emitter without coupling the substrate to wgpu
/// types. The integration layer subscribes to a queue of
/// these events; each staging op (stage_region for `Demote`,
/// release_region for `Promote`) drives one
/// [`StagingTransferEvent`] onto the queue.
///
/// Byte-accurate accounting per the bead's spec: every event
/// carries `bytes` so the wgpu layer can size its copy buffer
/// and the test harness can assert the total upload/download
/// volume matches the staging buffer's `used_bytes` delta.
///
/// `frame_id` is the monotonic frame counter — lets the wgpu
/// layer batch transfers per frame + lets the test harness
/// pin transfer ordering across frames.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct StagingTransferEvent {
    pub region_id: u64,
    pub direction: StagingTransferDirection,
    pub allocation: HostRamStagingAllocation,
    pub frame_id: u64,
}

impl StagingTransferEvent {
    /// Convenience: total bytes the wgpu copy will move.
    #[must_use]
    pub const fn bytes(&self) -> u64 {
        self.allocation.bytes
    }

    /// Convenience: starting offset within the staging buffer.
    #[must_use]
    pub const fn offset(&self) -> u64 {
        self.allocation.offset
    }
}

/// br-ft-ktd19.3 substrate-pass slice 2: disk-tier handoff
/// request the staging-buffer integration pushes when host-RAM
/// overflows.
///
/// The bead's spec calls for "Integrate Disk-tier handoff with
/// the cold-disk eviction infrastructure" — this type is the
/// pure-data bridge between a `HostRamStagingError::OutOfSpace`
/// signal and the cold-tier pipeline at
/// `crates/frankenterm-core/src/cold_tier_pipeline.rs`.
///
/// Producer side: the staging integration's `stage_region`
/// wrapper detects `OutOfSpace`, picks the LRU cold region
/// from the existing tier registry, and emits a `Demote`
/// `DiskTierHandoff` for that region. The cold-tier driver
/// writes the bytes to disk via its existing `WritePipelineStep`
/// machinery + responds with the persisted offset.
///
/// Consumer side: the staging integration's `release_region`
/// wrapper, when called for a region that's been demoted to
/// disk, emits a `Promote` `DiskTierHandoff`. The cold-tier
/// driver reads from disk back into a new staging slot.
///
/// Pure data — no cold_tier_pipeline type coupling. The
/// integration layer translates each variant into the
/// appropriate `WritePipelineStep` / read request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct DiskTierHandoff {
    pub region_id: u64,
    pub direction: DiskHandoffDirection,
    pub bytes: u64,
    /// Frame at which the handoff was queued. Lets the
    /// integration batch handoffs per cold-tier write batch.
    pub frame_id: u64,
}

/// Direction of a disk-tier handoff per br-ft-ktd19.3.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DiskHandoffDirection {
    /// Host-RAM staging → disk (cold-tier write).
    Demote,
    /// Disk → host-RAM staging (cold-tier read).
    Promote,
}

/// Pending disk-handoff queue for the cold-tier pipeline
/// (br-ft-ktd19.3 slice 2). Per-frame the integration drains
/// this queue + dispatches each handoff into the cold-tier
/// pipeline's write/read driver. Same shape as
/// [`StagingTransferQueue`] — pure data, preserves push order.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DiskTierHandoffQueue {
    handoffs: Vec<DiskTierHandoff>,
}

impl DiskTierHandoffQueue {
    /// New empty queue.
    #[must_use]
    pub fn new() -> Self {
        Self {
            handoffs: Vec::new(),
        }
    }

    /// Push a handoff.
    pub fn push(&mut self, handoff: DiskTierHandoff) {
        self.handoffs.push(handoff);
    }

    /// Drain pending handoffs in push order. Resets to empty.
    pub fn drain_pending(&mut self) -> Vec<DiskTierHandoff> {
        let mut drained = Vec::with_capacity(self.handoffs.len());
        drained.append(&mut self.handoffs);
        drained
    }

    /// Drain pending handoffs into caller-owned direction batches in one pass.
    ///
    /// This is the hot-path batching API for the cold-tier driver: callers keep
    /// `demotes` and `promotes` as scratch buffers across frames, and this
    /// method moves every queued handoff exactly once while preserving push
    /// order within each direction.
    pub fn drain_by_direction_into(
        &mut self,
        demotes: &mut Vec<DiskTierHandoff>,
        promotes: &mut Vec<DiskTierHandoff>,
    ) {
        demotes.reserve(self.handoffs.len());
        promotes.reserve(self.handoffs.len());
        for handoff in self.handoffs.drain(..) {
            match handoff.direction {
                DiskHandoffDirection::Demote => demotes.push(handoff),
                DiskHandoffDirection::Promote => promotes.push(handoff),
            }
        }
    }

    /// Drain pending handoffs into demote/promote batches in one pass.
    #[must_use]
    pub fn drain_partitioned_by_direction(
        &mut self,
    ) -> (Vec<DiskTierHandoff>, Vec<DiskTierHandoff>) {
        let mut demotes = Vec::new();
        let mut promotes = Vec::new();
        self.drain_by_direction_into(&mut demotes, &mut promotes);
        (demotes, promotes)
    }

    /// Peek without consuming.
    #[must_use]
    pub fn pending(&self) -> &[DiskTierHandoff] {
        &self.handoffs
    }

    /// Total pending bytes across all handoffs (saturating).
    #[must_use]
    pub fn pending_bytes(&self) -> u64 {
        self.handoffs
            .iter()
            .map(|h| h.bytes)
            .fold(0_u64, u64::saturating_add)
    }

    /// Pending handoff count.
    #[must_use]
    pub fn len(&self) -> usize {
        self.handoffs.len()
    }

    /// Whether the queue has no pending handoffs.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.handoffs.is_empty()
    }

    /// Filter by direction. Used when the cold-tier pipeline
    /// batches all writes (Demote) before all reads (Promote)
    /// to keep the disk head moving in one direction.
    #[must_use]
    pub fn by_direction(&self, direction: DiskHandoffDirection) -> Vec<DiskTierHandoff> {
        self.handoffs
            .iter()
            .filter(|h| h.direction == direction)
            .copied()
            .collect()
    }
}

/// br-ft-ktd19.3 substrate slice: disk-throughput budget gate
/// over [`DiskTierHandoff`]s. Sibling to [`FrameBudgetSwapDeferrer`]
/// (VRAM↔staging side) per the runbook at
/// `docs/render/atlas-disk-tier-handoff-integration.md`.
///
/// The cold-tier pipeline can only dispatch as many bytes per
/// frame as the storage-class throughput permits. This estimator
/// admits handoffs while accumulated cost stays under the
/// frame's disk-handoff budget (default 10% of the 16.67ms
/// frame, ≈ 1667µs at 60fps); over-budget handoffs defer to
/// the next frame via the queue's natural carry-over (the
/// integration calls `drain_pending` only after every admitted
/// handoff dispatches, so deferred ones stay queued).
///
/// Storage-class throughput defaults (operator-tunable):
/// - NVMe SSD (default): 3000 B/µs ≈ 3 GB/s
/// - SATA SSD: 500 B/µs ≈ 500 MB/s
/// - HDD (legacy): 150 B/µs ≈ 150 MB/s
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DiskBudgetEstimator {
    /// Configurable disk write throughput. Default `3000`
    /// (NVMe SSD class). Operators tune per platform / storage
    /// class — a lower value = more conservative budget = more
    /// deferrals.
    pub bytes_per_microsecond: u64,
    /// Bytes admitted in the current frame. Reset on
    /// [`Self::reset_for_new_frame`].
    admitted_bytes: u64,
}

impl DiskBudgetEstimator {
    /// New estimator with the default NVMe throughput model
    /// (3 GB/s ≈ 3000 B/µs).
    #[must_use]
    pub const fn new() -> Self {
        Self {
            bytes_per_microsecond: 3000,
            admitted_bytes: 0,
        }
    }

    /// New estimator with a caller-supplied throughput model
    /// (500 for SATA SSD, 150 for HDD per the runbook table).
    #[must_use]
    pub const fn with_throughput(bytes_per_microsecond: u64) -> Self {
        Self {
            bytes_per_microsecond,
            admitted_bytes: 0,
        }
    }

    /// Estimate disk-write cost in microseconds for `bytes`.
    /// Saturating semantics on a zero-throughput configuration
    /// (defensive — division by zero would panic; zero throughput
    /// surfaces as "always defer" via [`u64::MAX`]).
    #[must_use]
    pub const fn cost_microseconds(&self, bytes: u64) -> u64 {
        if bytes == 0 {
            return 0;
        }
        if self.bytes_per_microsecond == 0 {
            return u64::MAX;
        }
        let whole = bytes / self.bytes_per_microsecond;
        let remainder = bytes % self.bytes_per_microsecond;
        if remainder == 0 {
            whole
        } else {
            whole.saturating_add(1)
        }
    }

    /// Decide whether `handoff`'s disk-dispatch cost fits the
    /// remaining `disk_budget_us`. On Admit, the estimator's
    /// `admitted_bytes` accumulator advances; on Defer it stays
    /// put + the caller leaves the handoff queued for next
    /// frame.
    pub fn admit_or_defer(
        &mut self,
        handoff: &DiskTierHandoff,
        disk_budget_us: u64,
    ) -> SwapDeferralOutcome {
        if disk_budget_us == 0 && handoff.bytes > 0 {
            return SwapDeferralOutcome::Defer;
        }
        let next_admitted_bytes = self.admitted_bytes.saturating_add(handoff.bytes);
        if self.cost_microseconds(next_admitted_bytes) > disk_budget_us {
            SwapDeferralOutcome::Defer
        } else {
            self.admitted_bytes = next_admitted_bytes;
            SwapDeferralOutcome::Admit
        }
    }

    /// Drive a list of pending handoffs through the estimator
    /// gate. Returns `(admitted, deferred)` where `admitted`
    /// is the prefix that fit within `disk_budget_us` + `deferred`
    /// is the suffix to leave queued for the next frame.
    /// Preserves handoff order — the cold-tier driver dispatches
    /// admitted handoffs in the same order the integration
    /// queued them.
    pub fn partition(
        &mut self,
        handoffs: &[DiskTierHandoff],
        disk_budget_us: u64,
    ) -> (Vec<DiskTierHandoff>, Vec<DiskTierHandoff>) {
        let mut admitted = Vec::with_capacity(handoffs.len());
        let mut deferred = Vec::new();
        for (index, handoff) in handoffs.iter().enumerate() {
            match self.admit_or_defer(handoff, disk_budget_us) {
                SwapDeferralOutcome::Admit => admitted.push(*handoff),
                SwapDeferralOutcome::Defer => {
                    deferred.extend_from_slice(&handoffs[index..]);
                    break;
                }
            }
        }
        (admitted, deferred)
    }

    /// Reset the per-frame accumulator. Integration calls this
    /// at frame boundaries (after the cold-tier driver has
    /// fired its per-frame batch).
    pub const fn reset_for_new_frame(&mut self) {
        self.admitted_bytes = 0;
    }

    /// Read-only accessor for the per-frame admitted bytes.
    #[must_use]
    pub const fn admitted_bytes(&self) -> u64 {
        self.admitted_bytes
    }
}

impl Default for DiskBudgetEstimator {
    fn default() -> Self {
        Self::new()
    }
}

/// br-ft-ktd19.3 substrate-pass: per-event upload-cost hint.
///
/// `StagingTransferEvent` carries `bytes` but not the wgpu-
/// specific copy duration; the frame-budget swap deferrer
/// estimates duration via a configurable bytes-per-microsecond
/// throughput model. Operators can tune
/// [`FrameBudgetSwapDeferrer::bytes_per_microsecond`] per
/// platform / GPU class.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SwapDeferralOutcome {
    /// Event fits within the remaining frame budget.
    Admit,
    /// Event would overrun the frame budget; defer to next
    /// frame. The integration's tick-loop re-enqueues deferred
    /// events on the next frame's drain.
    Defer,
}

/// br-ft-ktd19.3 substrate-pass: frame-budget gate over
/// [`StagingTransferEvent`]s.
///
/// The bead's spec: "enforce frame-budget compliance by
/// deferring swap-in work that would overrun the current
/// frame." Pure-logic gate — the integration computes
/// remaining frame budget (16.67ms@60fps − elapsed render
/// time), passes it to [`Self::admit_or_defer`], and the
/// gate returns `Admit` while accumulated upload cost stays
/// under budget, `Defer` once admission would overrun.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameBudgetSwapDeferrer {
    /// Configurable upload throughput. Default `12_000`
    /// (12 GB/s ≈ PCIe 4.0 x16) — operators can override per
    /// platform / GPU class. Lower values = more conservative
    /// budget = more deferrals.
    pub bytes_per_microsecond: u64,
    /// Bytes admitted in the current frame. Reset on
    /// [`Self::reset_for_new_frame`].
    admitted_bytes: u64,
}

impl FrameBudgetSwapDeferrer {
    /// New deferrer with the default 12 GB/s throughput model.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            bytes_per_microsecond: 12_000,
            admitted_bytes: 0,
        }
    }

    /// New deferrer with a caller-supplied throughput model.
    #[must_use]
    pub const fn with_throughput(bytes_per_microsecond: u64) -> Self {
        Self {
            bytes_per_microsecond,
            admitted_bytes: 0,
        }
    }

    /// Estimate the upload cost in microseconds for `bytes`.
    /// Saturating semantics on a zero throughput configuration
    /// (defensive — division by zero would panic).
    #[must_use]
    pub const fn cost_microseconds(&self, bytes: u64) -> u64 {
        if bytes == 0 {
            return 0;
        }
        if self.bytes_per_microsecond == 0 {
            return u64::MAX;
        }
        let whole = bytes / self.bytes_per_microsecond;
        let remainder = bytes % self.bytes_per_microsecond;
        if remainder == 0 {
            whole
        } else {
            whole.saturating_add(1)
        }
    }

    /// Decide whether `event`'s upload cost fits the remaining
    /// `frame_budget_us`. On Admit, the deferrer's
    /// `admitted_bytes` accumulator advances; on Defer it
    /// stays put + the caller re-enqueues the event for the
    /// next frame.
    pub fn admit_or_defer(
        &mut self,
        event: &StagingTransferEvent,
        frame_budget_us: u64,
    ) -> SwapDeferralOutcome {
        if frame_budget_us == 0 && event.bytes() > 0 {
            return SwapDeferralOutcome::Defer;
        }
        let next_admitted_bytes = self.admitted_bytes.saturating_add(event.bytes());
        if self.cost_microseconds(next_admitted_bytes) > frame_budget_us {
            SwapDeferralOutcome::Defer
        } else {
            self.admitted_bytes = next_admitted_bytes;
            SwapDeferralOutcome::Admit
        }
    }

    /// Drive a list of pending events through the deferrer
    /// gate. Returns `(admitted, deferred)` where `admitted`
    /// is the prefix of events that fit + `deferred` is the
    /// suffix to re-enqueue on the next frame.
    ///
    /// Preserves event order — the wgpu copy layer dispatches
    /// admitted events in the same order the tiered-swap
    /// decision queued them.
    pub fn partition(
        &mut self,
        events: &[StagingTransferEvent],
        frame_budget_us: u64,
    ) -> (Vec<StagingTransferEvent>, Vec<StagingTransferEvent>) {
        let mut admitted = Vec::with_capacity(events.len());
        let mut deferred = Vec::new();
        for (index, event) in events.iter().enumerate() {
            match self.admit_or_defer(event, frame_budget_us) {
                SwapDeferralOutcome::Admit => admitted.push(*event),
                SwapDeferralOutcome::Defer => {
                    deferred.extend_from_slice(&events[index..]);
                    break;
                }
            }
        }
        (admitted, deferred)
    }

    /// Reset the per-frame accumulator. Integration calls this
    /// at frame boundaries (after the wgpu queue has fired its
    /// per-frame copy commands).
    pub const fn reset_for_new_frame(&mut self) {
        self.admitted_bytes = 0;
    }

    /// Read-only accessor for the per-frame admitted bytes.
    #[must_use]
    pub const fn admitted_bytes(&self) -> u64 {
        self.admitted_bytes
    }
}

impl Default for FrameBudgetSwapDeferrer {
    fn default() -> Self {
        Self::new()
    }
}

/// Pending transfer queue for the wgpu copy-dispatch layer
/// (br-ft-ktd19.1). The tiered-swap integration pushes events
/// here whenever a region demotes or promotes; the renderer
/// drains the queue once per frame and emits the matching
/// wgpu commands.
///
/// Pure data — no wgpu types. Object-safe via
/// `&dyn StagingTransferSink` if the integration grows a
/// trait-shaped consumer.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StagingTransferQueue {
    events: Vec<StagingTransferEvent>,
}

impl StagingTransferQueue {
    /// New empty queue.
    #[must_use]
    pub fn new() -> Self {
        Self { events: Vec::new() }
    }

    /// Push a transfer event. The integration layer's
    /// stage_region / release_region wrappers call this after
    /// the staging buffer's mutating call returns Ok.
    pub fn push(&mut self, event: StagingTransferEvent) {
        self.events.push(event);
    }

    /// Drain every pending event. Returns them in push order
    /// so the wgpu layer dispatches in the original tiered-
    /// swap-decision order. Resets the queue to empty.
    pub fn drain_pending(&mut self) -> Vec<StagingTransferEvent> {
        let mut drained = Vec::with_capacity(self.events.len());
        drained.append(&mut self.events);
        drained
    }

    /// Peek without consuming. Used by `ft doctor` /
    /// telemetry surfaces.
    #[must_use]
    pub fn pending(&self) -> &[StagingTransferEvent] {
        &self.events
    }

    /// Total pending bytes — useful for the bead's "byte-
    /// accurate accounting" assertion.
    #[must_use]
    pub fn pending_bytes(&self) -> u64 {
        self.events
            .iter()
            .map(StagingTransferEvent::bytes)
            .fold(0_u64, u64::saturating_add)
    }

    /// Pending event count.
    #[must_use]
    pub fn len(&self) -> usize {
        self.events.len()
    }

    /// Whether the queue has no pending events.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.events.is_empty()
    }
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
// br-ft-ktd19.1 substrate-pass: stage_region / release_region wrappers
// that drive `StagingTransferEvent`s onto the wgpu copy queue.
//
// The substrate's two halves — `HostRamStagingBuffer` (allocation
// state) + `StagingTransferQueue` (transfer events the wgpu layer
// drains) — compose at every staging op. This driver wraps both
// so the integration calls a single method (`stage_region` /
// `release_region`) that updates the buffer + pushes the matching
// event in one step. Without the wrapper each call site duplicates
// the "after the staging buffer's mutating call returns Ok, push
// the event" sequence the queue's docstring describes.
//
// Pure data — no wgpu coupling. The wired-pass renderer drains
// `pending_events()` once per frame + emits matching wgpu copy
// commands per the doc at `docs/render/atlas-tiered-swap-wgpu-
// integration.md` (cc_1, d4701405d).
// ============================================================================

/// Unified driver for the host-RAM staging tier: owns the
/// allocation state ([`HostRamStagingBuffer`]) + the per-frame
/// transfer queue ([`StagingTransferQueue`]) and the bookkeeping
/// that connects them.
///
/// Each successful `stage_region` call emits one
/// [`StagingTransferDirection::Demote`] event (VRAM → staging);
/// each successful `release_region` call emits one
/// [`StagingTransferDirection::Promote`] event (staging → VRAM).
/// Failed calls (zero-size, capacity exhaustion, unknown region)
/// do **not** push an event — keeping the queue's bytes-accounting
/// faithful to the underlying buffer's `used_bytes` delta.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AtlasStagingTransferDriver {
    staging: HostRamStagingBuffer,
    queue: StagingTransferQueue,
    /// Cumulative bytes that have round-tripped through this
    /// driver (sum of every successful `stage_region` call's
    /// allocation size). Useful for the bead's byte-accurate
    /// accounting tests + operator telemetry surfaces — distinct
    /// from `used_bytes()` which fluctuates as regions release.
    cumulative_demoted_bytes: u64,
    /// Cumulative bytes that have promoted back out via
    /// `release_region`. The pair (`cumulative_demoted_bytes`,
    /// `cumulative_promoted_bytes`) tracks total VRAM↔staging
    /// volume across the driver's lifetime.
    cumulative_promoted_bytes: u64,
}

impl AtlasStagingTransferDriver {
    /// New driver wrapping a fresh `HostRamStagingBuffer` of
    /// `capacity_bytes` + an empty queue.
    #[must_use]
    pub fn new(capacity_bytes: u64) -> Self {
        Self {
            staging: HostRamStagingBuffer::new(capacity_bytes),
            queue: StagingTransferQueue::new(),
            cumulative_demoted_bytes: 0,
            cumulative_promoted_bytes: 0,
        }
    }

    /// New driver wrapping a pre-built staging buffer (e.g. a
    /// fixture with allocations already staged). The queue
    /// starts empty.
    #[must_use]
    pub fn from_staging(staging: HostRamStagingBuffer) -> Self {
        Self {
            staging,
            queue: StagingTransferQueue::new(),
            cumulative_demoted_bytes: 0,
            cumulative_promoted_bytes: 0,
        }
    }

    /// Stage `bytes` for `region_id` (VRAM → staging on demote)
    /// and push the matching [`StagingTransferDirection::Demote`]
    /// event onto the queue. The event carries the allocation
    /// the wgpu copy command writes to.
    ///
    /// Errors propagate from [`HostRamStagingBuffer::stage_region`]
    /// without pushing an event — the queue stays in sync with
    /// the buffer.
    pub fn stage_region(
        &mut self,
        region_id: u64,
        bytes: u64,
        frame_id: u64,
    ) -> Result<HostRamStagingAllocation, HostRamStagingError> {
        let allocation = self.staging.stage_region(region_id, bytes)?;
        self.queue.push(StagingTransferEvent {
            region_id,
            direction: StagingTransferDirection::Demote,
            allocation,
            frame_id,
        });
        self.cumulative_demoted_bytes = self
            .cumulative_demoted_bytes
            .saturating_add(allocation.bytes);
        Ok(allocation)
    }

    /// Release the staging slot for `region_id` (staging → VRAM
    /// on promote) and push the matching
    /// [`StagingTransferDirection::Promote`] event. The event
    /// carries the allocation the wgpu copy command reads from
    /// — the slot is freed for reuse on the same call.
    ///
    /// Errors propagate from
    /// [`HostRamStagingBuffer::release_region`] without pushing
    /// an event.
    pub fn release_region(
        &mut self,
        region_id: u64,
        frame_id: u64,
    ) -> Result<HostRamStagingAllocation, HostRamStagingError> {
        let allocation = self.staging.release_region(region_id)?;
        self.queue.push(StagingTransferEvent {
            region_id,
            direction: StagingTransferDirection::Promote,
            allocation,
            frame_id,
        });
        self.cumulative_promoted_bytes = self
            .cumulative_promoted_bytes
            .saturating_add(allocation.bytes);
        Ok(allocation)
    }

    /// Drain every pending transfer event in push order. Called
    /// once per frame by the wgpu integration before emitting
    /// copy commands.
    pub fn drain_pending(&mut self) -> Vec<StagingTransferEvent> {
        self.queue.drain_pending()
    }

    /// Peek pending events without consuming. Used by `ft doctor`
    /// + telemetry surfaces.
    #[must_use]
    pub fn pending_events(&self) -> &[StagingTransferEvent] {
        self.queue.pending()
    }

    /// Total bytes pending in the queue.
    #[must_use]
    pub fn pending_bytes(&self) -> u64 {
        self.queue.pending_bytes()
    }

    /// Number of pending events.
    #[must_use]
    pub fn pending_count(&self) -> usize {
        self.queue.len()
    }

    /// True when the queue has no pending events.
    #[must_use]
    pub fn pending_is_empty(&self) -> bool {
        self.queue.is_empty()
    }

    /// Borrow the underlying staging buffer for read-only
    /// inspection (capacity / allocations / pressure).
    #[must_use]
    pub const fn staging(&self) -> &HostRamStagingBuffer {
        &self.staging
    }

    /// Forward to [`HostRamStagingBuffer::capacity_bytes`].
    #[must_use]
    pub const fn capacity_bytes(&self) -> u64 {
        self.staging.capacity_bytes()
    }

    /// Forward to [`HostRamStagingBuffer::used_bytes`].
    #[must_use]
    pub const fn used_bytes(&self) -> u64 {
        self.staging.used_bytes()
    }

    /// Forward to [`HostRamStagingBuffer::available_bytes`].
    #[must_use]
    pub fn available_bytes(&self) -> u64 {
        self.staging.available_bytes()
    }

    /// Cumulative bytes that have demoted via `stage_region`
    /// since this driver was constructed. Monotonic counter —
    /// distinct from `used_bytes()` which fluctuates with
    /// `release_region`.
    #[must_use]
    pub const fn cumulative_demoted_bytes(&self) -> u64 {
        self.cumulative_demoted_bytes
    }

    /// Cumulative bytes that have promoted via `release_region`
    /// since this driver was constructed. Monotonic counter.
    #[must_use]
    pub const fn cumulative_promoted_bytes(&self) -> u64 {
        self.cumulative_promoted_bytes
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
// br-ft-ktd19.3 substrate slice: bandwidth-starved eviction-selector
// configuration. Operators on deployments where host-RAM staging
// throughput is the bottleneck (legacy buses, virtualized hosts with
// thin VRAM↔system-RAM links) want VRAM evictions to skip the
// HostRam intermediate and demote straight to Disk so the cold-tier
// pipeline takes the load instead. Per the operator playbook at
// `docs/render/atlas-tiered-swap-operator-playbook.md`.
// ============================================================================

/// Configuration knobs that gate the eviction-selector's tier-walk
/// behavior. Pure data — the tier-walk free functions below
/// consume this config to pick the right demotion target.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct EvictionSelectorConfig {
    /// When true, the tier-walk skips `HostRam` on demote so VRAM
    /// regions evict straight to `Disk`. The operator playbook's
    /// "bandwidth-starved deployment" profile flips this on to
    /// move pressure onto the cold-tier pipeline.
    pub skip_host_ram_on_demote: bool,
}

impl EvictionSelectorConfig {
    /// Default profile: keep the standard 3-tier walk
    /// (VRAM → HostRam → Disk).
    #[must_use]
    pub const fn new() -> Self {
        Self {
            skip_host_ram_on_demote: false,
        }
    }

    /// Bandwidth-starved profile: VRAM demotes straight to Disk;
    /// HostRam still demotes to Disk normally.
    #[must_use]
    pub const fn bandwidth_starved() -> Self {
        Self {
            skip_host_ram_on_demote: true,
        }
    }
}

impl Default for EvictionSelectorConfig {
    fn default() -> Self {
        Self::new()
    }
}

/// Resolve the demotion target for `tier` under `config`. Wraps
/// [`AtlasTier::demotion_target`] with the bandwidth-starved
/// override: when `skip_host_ram_on_demote` is set, VRAM walks
/// straight to Disk.
///
/// HostRam and Disk demotion targets are unchanged regardless of
/// config — only the VRAM step has a bandwidth-aware variant.
#[must_use]
pub const fn bandwidth_aware_demotion_target(
    tier: AtlasTier,
    config: EvictionSelectorConfig,
) -> Option<AtlasTier> {
    if config.skip_host_ram_on_demote && matches!(tier, AtlasTier::Vram) {
        return Some(AtlasTier::Disk);
    }
    tier.demotion_target()
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

    // ----------------------------------------------------------------
    // br-ft-ktd19.1 substrate-pass: StagingTransferQueue + events.
    // ----------------------------------------------------------------

    fn alloc(region_id: u64, offset: u64, bytes: u64) -> HostRamStagingAllocation {
        HostRamStagingAllocation {
            region_id,
            offset,
            bytes,
        }
    }

    #[test]
    fn staging_transfer_queue_starts_empty() {
        let q = StagingTransferQueue::new();
        assert!(q.is_empty());
        assert_eq!(q.len(), 0);
        assert_eq!(q.pending_bytes(), 0);
        assert!(q.pending().is_empty());
    }

    #[test]
    fn staging_transfer_queue_records_demote_and_promote() {
        let mut q = StagingTransferQueue::new();
        q.push(StagingTransferEvent {
            region_id: 1,
            direction: StagingTransferDirection::Demote,
            allocation: alloc(1, 0, 1024),
            frame_id: 100,
        });
        q.push(StagingTransferEvent {
            region_id: 2,
            direction: StagingTransferDirection::Promote,
            allocation: alloc(2, 1024, 2048),
            frame_id: 101,
        });
        assert_eq!(q.len(), 2);
        assert_eq!(q.pending_bytes(), 1024 + 2048);
        assert_eq!(q.pending()[0].direction, StagingTransferDirection::Demote);
        assert_eq!(q.pending()[1].direction, StagingTransferDirection::Promote);
    }

    #[test]
    fn staging_transfer_queue_drain_pending_resets_to_empty() {
        let mut q = StagingTransferQueue::new();
        q.push(StagingTransferEvent {
            region_id: 1,
            direction: StagingTransferDirection::Demote,
            allocation: alloc(1, 0, 512),
            frame_id: 0,
        });
        let drained = q.drain_pending();
        assert_eq!(drained.len(), 1);
        assert_eq!(drained[0].region_id, 1);
        assert!(q.is_empty());
        assert_eq!(q.pending_bytes(), 0);
    }

    #[test]
    fn staging_transfer_queue_drain_preserves_push_order() {
        let mut q = StagingTransferQueue::new();
        for i in 0..5_u64 {
            q.push(StagingTransferEvent {
                region_id: i,
                direction: StagingTransferDirection::Demote,
                allocation: alloc(i, i * 100, 100),
                frame_id: i,
            });
        }
        let drained = q.drain_pending();
        for (i, event) in drained.iter().enumerate() {
            assert_eq!(event.region_id, i as u64);
            assert_eq!(event.frame_id, i as u64);
        }
    }

    #[test]
    fn staging_transfer_queue_drain_preserves_queue_capacity() {
        let mut q = StagingTransferQueue::new();
        for i in 0..32_u64 {
            q.push(StagingTransferEvent {
                region_id: i,
                direction: StagingTransferDirection::Demote,
                allocation: alloc(i, i * 100, 100),
                frame_id: i,
            });
        }
        let warm_capacity = q.events.capacity();
        assert!(warm_capacity >= 32);

        let drained = q.drain_pending();

        assert_eq!(drained.len(), 32);
        assert!(q.is_empty());
        assert_eq!(q.events.capacity(), warm_capacity);

        for i in 0..16_u64 {
            q.push(StagingTransferEvent {
                region_id: i,
                direction: StagingTransferDirection::Promote,
                allocation: alloc(i, i * 100, 100),
                frame_id: i,
            });
        }
        assert_eq!(q.events.capacity(), warm_capacity);
    }

    #[test]
    fn staging_transfer_event_byte_and_offset_accessors() {
        let event = StagingTransferEvent {
            region_id: 42,
            direction: StagingTransferDirection::Promote,
            allocation: alloc(42, 4096, 8192),
            frame_id: 99,
        };
        assert_eq!(event.bytes(), 8192);
        assert_eq!(event.offset(), 4096);
    }

    #[test]
    fn staging_transfer_queue_pending_bytes_saturates_on_overflow() {
        // Defensive: if a buggy caller pushes events whose
        // bytes sum to > u64::MAX, pending_bytes should
        // saturate rather than wrap.
        let mut q = StagingTransferQueue::new();
        q.push(StagingTransferEvent {
            region_id: 1,
            direction: StagingTransferDirection::Demote,
            allocation: alloc(1, 0, u64::MAX - 100),
            frame_id: 0,
        });
        q.push(StagingTransferEvent {
            region_id: 2,
            direction: StagingTransferDirection::Demote,
            allocation: alloc(2, 0, 200),
            frame_id: 0,
        });
        // Saturating sum: u64::MAX - 100 + 200 saturates at u64::MAX.
        assert_eq!(q.pending_bytes(), u64::MAX);
    }

    #[test]
    fn staging_transfer_event_round_trips_allocation_offsets_from_stage_region() {
        // End-to-end: stage a region, build an event from the
        // returned allocation, drain via the queue. The wgpu
        // layer's offset+bytes match what the staging buffer
        // returned — proves the bead's "byte-accurate
        // accounting around allocation offsets" contract.
        let mut staging = HostRamStagingBuffer::new(8192);
        let allocation = staging.stage_region(7, 1024).unwrap();
        let mut q = StagingTransferQueue::new();
        q.push(StagingTransferEvent {
            region_id: 7,
            direction: StagingTransferDirection::Demote,
            allocation,
            frame_id: 1,
        });
        let drained = q.drain_pending();
        assert_eq!(drained.len(), 1);
        assert_eq!(drained[0].allocation.offset, allocation.offset);
        assert_eq!(drained[0].allocation.bytes, 1024);
        // Releasing the region returns the same allocation
        // shape; the next event the queue carries can pin
        // identical bytes for the upload-side assertion.
        let released = staging.release_region(7).unwrap();
        assert_eq!(released.offset, allocation.offset);
        assert_eq!(released.bytes, 1024);
    }

    // ----------------------------------------------------------------
    // br-ft-ktd19.3 substrate-pass: FrameBudgetSwapDeferrer.
    // ----------------------------------------------------------------

    fn event(region_id: u64, bytes: u64) -> StagingTransferEvent {
        StagingTransferEvent {
            region_id,
            direction: StagingTransferDirection::Promote,
            allocation: alloc(region_id, 0, bytes),
            frame_id: 0,
        }
    }

    #[test]
    fn deferrer_admits_when_under_budget() {
        let mut d = FrameBudgetSwapDeferrer::new();
        // 1 MB at 12 GB/s = 1_048_576 / 12_000 ≈ 87 µs.
        // 1ms frame budget should easily admit.
        let outcome = d.admit_or_defer(&event(1, 1_048_576), 1000);
        assert_eq!(outcome, SwapDeferralOutcome::Admit);
        assert_eq!(d.admitted_bytes(), 1_048_576);
    }

    #[test]
    fn deferrer_defers_when_event_alone_overruns_budget() {
        let mut d = FrameBudgetSwapDeferrer::new();
        // 1 GB at 12 GB/s ≈ 89_478 µs.
        // 50ms frame budget = 50_000 µs → defer.
        let outcome = d.admit_or_defer(&event(1, 1_073_741_824), 50_000);
        assert_eq!(outcome, SwapDeferralOutcome::Defer);
        assert_eq!(d.admitted_bytes(), 0); // unchanged
    }

    #[test]
    fn deferrer_admits_then_defers_when_running_total_exceeds_budget() {
        let mut d = FrameBudgetSwapDeferrer::with_throughput(1000); // 1 KB/µs
        // Frame budget 100 µs = admit ~100 KB total before defer.
        // First event: 50 KB → 50 µs cost → admit (running 50 µs).
        // Second event: 60 KB → 60 µs cost → admit would push to
        //   110 µs > 100 µs budget → defer.
        // (Note: 50 + 60 = 110 > 100. Defer the 60 KB.)
        let r1 = d.admit_or_defer(&event(1, 50_000), 100);
        assert_eq!(r1, SwapDeferralOutcome::Admit);
        let r2 = d.admit_or_defer(&event(2, 60_000), 100);
        assert_eq!(r2, SwapDeferralOutcome::Defer);
        // First-admit accumulator preserved.
        assert_eq!(d.admitted_bytes(), 50_000);
    }

    #[test]
    fn deferrer_reset_for_new_frame_clears_accumulator() {
        let mut d = FrameBudgetSwapDeferrer::with_throughput(1000);
        d.admit_or_defer(&event(1, 50_000), 100);
        assert_eq!(d.admitted_bytes(), 50_000);
        d.reset_for_new_frame();
        assert_eq!(d.admitted_bytes(), 0);
        // After reset, a previously-deferred event can admit.
        let r = d.admit_or_defer(&event(2, 60_000), 100);
        assert_eq!(r, SwapDeferralOutcome::Admit);
    }

    #[test]
    fn deferrer_partition_preserves_admit_order_and_defer_order() {
        let mut d = FrameBudgetSwapDeferrer::with_throughput(1000); // 1 KB/µs
        let events = vec![
            event(1, 30_000),
            event(2, 30_000),
            event(3, 50_000),  // would push total to 110_000 → defer
            event(4, 30_000),  // remains deferred behind #3
            event(5, 100_000), // remains deferred behind #3
        ];
        let (admitted, deferred) = d.partition(&events, 100);
        // Admitted is the fitting prefix.
        assert_eq!(admitted.len(), 2);
        assert_eq!(admitted[0].region_id, 1);
        assert_eq!(admitted[1].region_id, 2);
        // Deferred is the original suffix starting at the first over-budget event.
        assert_eq!(deferred.len(), 3);
        assert_eq!(deferred[0].region_id, 3);
        assert_eq!(deferred[1].region_id, 4);
        assert_eq!(deferred[2].region_id, 5);
    }

    #[test]
    fn deferrer_zero_throughput_defers_everything() {
        let mut d = FrameBudgetSwapDeferrer::with_throughput(0);
        // cost_microseconds returns u64::MAX → always defer.
        let outcome = d.admit_or_defer(&event(1, 1), 1_000_000);
        assert_eq!(outcome, SwapDeferralOutcome::Defer);
    }

    #[test]
    fn deferrer_zero_byte_event_admits_under_any_budget() {
        let mut d = FrameBudgetSwapDeferrer::new();
        // 0 bytes → 0 µs cost → admit even on a 1µs budget.
        let outcome = d.admit_or_defer(&event(1, 0), 1);
        assert_eq!(outcome, SwapDeferralOutcome::Admit);
    }

    #[test]
    fn deferrer_default_throughput_is_pcie4_class() {
        let d = FrameBudgetSwapDeferrer::default();
        assert_eq!(d.bytes_per_microsecond, 12_000);
    }

    // ========================================================================
    // br-ft-ktd19.1 substrate: AtlasStagingTransferDriver wrappers
    // ========================================================================

    #[test]
    fn driver_starts_empty_with_full_capacity() {
        let driver = AtlasStagingTransferDriver::new(1024);
        assert_eq!(driver.capacity_bytes(), 1024);
        assert_eq!(driver.used_bytes(), 0);
        assert_eq!(driver.available_bytes(), 1024);
        assert!(driver.pending_is_empty());
        assert_eq!(driver.cumulative_demoted_bytes(), 0);
        assert_eq!(driver.cumulative_promoted_bytes(), 0);
    }

    #[test]
    fn driver_stage_region_pushes_demote_event_with_allocation() {
        let mut driver = AtlasStagingTransferDriver::new(1024);
        let allocation = driver.stage_region(7, 256, 42).expect("stage");
        assert_eq!(allocation.region_id, 7);
        assert_eq!(allocation.bytes, 256);
        assert_eq!(driver.used_bytes(), 256);
        assert_eq!(driver.cumulative_demoted_bytes(), 256);

        let pending = driver.pending_events();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].region_id, 7);
        assert_eq!(pending[0].direction, StagingTransferDirection::Demote);
        assert_eq!(pending[0].frame_id, 42);
        assert_eq!(pending[0].allocation, allocation);
    }

    #[test]
    fn driver_release_region_pushes_promote_event_and_frees_slot() {
        let mut driver = AtlasStagingTransferDriver::new(1024);
        let staged = driver.stage_region(7, 256, 1).expect("stage");
        let _ = driver.drain_pending();

        let released = driver.release_region(7, 2).expect("release");
        assert_eq!(released, staged);
        assert_eq!(driver.used_bytes(), 0);
        assert_eq!(driver.available_bytes(), 1024);
        assert_eq!(driver.cumulative_promoted_bytes(), 256);

        let pending = driver.pending_events();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].region_id, 7);
        assert_eq!(pending[0].direction, StagingTransferDirection::Promote);
        assert_eq!(pending[0].frame_id, 2);
        assert_eq!(pending[0].allocation, staged);
    }

    #[test]
    fn driver_failed_stage_region_does_not_push_event() {
        let mut driver = AtlasStagingTransferDriver::new(1024);
        // Zero-size — should fail without pushing.
        let err = driver.stage_region(1, 0, 5).unwrap_err();
        assert!(matches!(err, HostRamStagingError::ZeroSizedRegion { .. }));
        assert!(driver.pending_is_empty());
        assert_eq!(driver.cumulative_demoted_bytes(), 0);

        // Capacity overflow — should fail without pushing.
        let err = driver.stage_region(1, 4096, 5).unwrap_err();
        assert!(matches!(err, HostRamStagingError::RegionTooLarge { .. }));
        assert!(driver.pending_is_empty());
    }

    #[test]
    fn driver_double_stage_same_region_fails_without_double_event() {
        let mut driver = AtlasStagingTransferDriver::new(1024);
        driver.stage_region(7, 256, 1).expect("first stage");
        let err = driver.stage_region(7, 256, 2).unwrap_err();
        assert!(matches!(
            err,
            HostRamStagingError::RegionAlreadyStaged { .. }
        ));
        assert_eq!(driver.pending_count(), 1);
        assert_eq!(driver.cumulative_demoted_bytes(), 256);
    }

    #[test]
    fn driver_release_unknown_region_does_not_push_event() {
        let mut driver = AtlasStagingTransferDriver::new(1024);
        let err = driver.release_region(999, 1).unwrap_err();
        assert!(matches!(err, HostRamStagingError::UnknownRegion { .. }));
        assert!(driver.pending_is_empty());
        assert_eq!(driver.cumulative_promoted_bytes(), 0);
    }

    #[test]
    fn driver_round_trip_emits_demote_then_promote_in_order() {
        let mut driver = AtlasStagingTransferDriver::new(1024);
        driver.stage_region(1, 100, 10).unwrap();
        driver.stage_region(2, 200, 11).unwrap();
        driver.release_region(1, 12).unwrap();
        driver.release_region(2, 13).unwrap();

        let events = driver.drain_pending();
        assert_eq!(events.len(), 4);
        assert_eq!(
            (events[0].region_id, events[0].direction, events[0].frame_id),
            (1, StagingTransferDirection::Demote, 10)
        );
        assert_eq!(
            (events[1].region_id, events[1].direction, events[1].frame_id),
            (2, StagingTransferDirection::Demote, 11)
        );
        assert_eq!(
            (events[2].region_id, events[2].direction, events[2].frame_id),
            (1, StagingTransferDirection::Promote, 12)
        );
        assert_eq!(
            (events[3].region_id, events[3].direction, events[3].frame_id),
            (2, StagingTransferDirection::Promote, 13)
        );
    }

    #[test]
    fn driver_drain_pending_resets_queue() {
        let mut driver = AtlasStagingTransferDriver::new(1024);
        driver.stage_region(1, 100, 1).unwrap();
        driver.stage_region(2, 200, 1).unwrap();
        let drained = driver.drain_pending();
        assert_eq!(drained.len(), 2);
        assert!(driver.pending_is_empty());
        assert_eq!(driver.pending_bytes(), 0);
    }

    #[test]
    fn driver_byte_accurate_accounting_matches_event_volume() {
        // The bead's "byte-accurate accounting" requirement: total
        // event bytes drained must equal the sum of every successful
        // stage + release call's allocation size.
        let mut driver = AtlasStagingTransferDriver::new(8192);
        driver.stage_region(1, 100, 1).unwrap();
        driver.stage_region(2, 250, 1).unwrap();
        driver.release_region(1, 2).unwrap();
        driver.stage_region(3, 75, 2).unwrap();

        let events = driver.drain_pending();
        let total_event_bytes: u64 = events.iter().map(StagingTransferEvent::bytes).sum();
        // Demotes: 100+250+75 = 425. Promotes: 100. Total = 525.
        assert_eq!(total_event_bytes, 425 + 100);
        assert_eq!(driver.cumulative_demoted_bytes(), 425);
        assert_eq!(driver.cumulative_promoted_bytes(), 100);
    }

    #[test]
    fn driver_pending_bytes_matches_queue_state() {
        let mut driver = AtlasStagingTransferDriver::new(8192);
        driver.stage_region(1, 100, 1).unwrap();
        driver.stage_region(2, 250, 1).unwrap();
        assert_eq!(driver.pending_bytes(), 350);
        assert_eq!(driver.pending_count(), 2);
        let _ = driver.drain_pending();
        assert_eq!(driver.pending_bytes(), 0);
    }

    #[test]
    fn driver_offsets_are_distinct_across_two_demotes() {
        // Bead spec calls out "byte-accurate accounting" around
        // allocation offsets — two consecutive demotes must hand
        // back non-overlapping allocations.
        let mut driver = AtlasStagingTransferDriver::new(1024);
        let a = driver.stage_region(1, 256, 1).unwrap();
        let b = driver.stage_region(2, 256, 1).unwrap();
        let a_end = a.offset.saturating_add(a.bytes);
        let b_end = b.offset.saturating_add(b.bytes);
        // Either a ends before b starts, or b ends before a starts.
        assert!(a_end <= b.offset || b_end <= a.offset);
    }

    #[test]
    fn driver_from_staging_inherits_buffer_state() {
        let mut buffer = HostRamStagingBuffer::new(1024);
        let _ = buffer.stage_region(1, 256).unwrap();
        let driver = AtlasStagingTransferDriver::from_staging(buffer);
        // The buffer state carries over — but the queue starts empty
        // (no events were pushed by the bare-buffer call site).
        assert_eq!(driver.used_bytes(), 256);
        assert!(driver.pending_is_empty());
        assert_eq!(driver.cumulative_demoted_bytes(), 0);
    }

    #[test]
    fn driver_release_after_drain_carries_correct_allocation() {
        // Bead spec: events carry the allocation the wgpu copy
        // dispatch uses. After draining, release_region must still
        // surface the right offset/bytes for the wgpu upload to
        // read from.
        let mut driver = AtlasStagingTransferDriver::new(1024);
        let staged = driver.stage_region(7, 256, 1).unwrap();
        let _ = driver.drain_pending();

        let released = driver.release_region(7, 9).unwrap();
        assert_eq!(released.offset, staged.offset);
        assert_eq!(released.bytes, staged.bytes);
        let pending = driver.pending_events();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].allocation, staged);
    }

    // ========================================================================
    // br-ft-ktd19.3 slice 2: DiskTierHandoffQueue (cold-tier bridge).
    // ========================================================================

    fn handoff(
        region_id: u64,
        direction: DiskHandoffDirection,
        bytes: u64,
        frame_id: u64,
    ) -> DiskTierHandoff {
        DiskTierHandoff {
            region_id,
            direction,
            bytes,
            frame_id,
        }
    }

    #[test]
    fn disk_handoff_queue_starts_empty() {
        let q = DiskTierHandoffQueue::new();
        assert!(q.is_empty());
        assert_eq!(q.len(), 0);
        assert_eq!(q.pending_bytes(), 0);
    }

    #[test]
    fn disk_handoff_queue_records_demote_and_promote() {
        let mut q = DiskTierHandoffQueue::new();
        q.push(handoff(1, DiskHandoffDirection::Demote, 4096, 100));
        q.push(handoff(2, DiskHandoffDirection::Promote, 8192, 101));
        assert_eq!(q.len(), 2);
        assert_eq!(q.pending_bytes(), 4096 + 8192);
    }

    #[test]
    fn disk_handoff_queue_drain_resets_to_empty() {
        let mut q = DiskTierHandoffQueue::new();
        q.push(handoff(1, DiskHandoffDirection::Demote, 4096, 0));
        let drained = q.drain_pending();
        assert_eq!(drained.len(), 1);
        assert!(q.is_empty());
    }

    #[test]
    fn disk_handoff_queue_by_direction_filters_correctly() {
        let mut q = DiskTierHandoffQueue::new();
        q.push(handoff(1, DiskHandoffDirection::Demote, 1024, 0));
        q.push(handoff(2, DiskHandoffDirection::Promote, 2048, 0));
        q.push(handoff(3, DiskHandoffDirection::Demote, 4096, 0));

        let demotes = q.by_direction(DiskHandoffDirection::Demote);
        assert_eq!(demotes.len(), 2);
        assert_eq!(demotes[0].region_id, 1);
        assert_eq!(demotes[1].region_id, 3);

        let promotes = q.by_direction(DiskHandoffDirection::Promote);
        assert_eq!(promotes.len(), 1);
        assert_eq!(promotes[0].region_id, 2);
    }

    #[test]
    fn disk_handoff_queue_drain_partitioned_by_direction_consumes_once() {
        let mut q = DiskTierHandoffQueue::new();
        q.push(handoff(1, DiskHandoffDirection::Demote, 1024, 0));
        q.push(handoff(2, DiskHandoffDirection::Promote, 2048, 0));
        q.push(handoff(3, DiskHandoffDirection::Demote, 4096, 0));
        q.push(handoff(4, DiskHandoffDirection::Promote, 8192, 0));

        let (demotes, promotes) = q.drain_partitioned_by_direction();

        assert!(q.is_empty());
        assert_eq!(
            demotes
                .iter()
                .map(|handoff| handoff.region_id)
                .collect::<Vec<_>>(),
            vec![1, 3]
        );
        assert_eq!(
            promotes
                .iter()
                .map(|handoff| handoff.region_id)
                .collect::<Vec<_>>(),
            vec![2, 4]
        );
    }

    #[test]
    fn disk_handoff_queue_drain_by_direction_into_reuses_scratch_capacity() {
        let mut q = DiskTierHandoffQueue::new();
        let mut demotes = Vec::with_capacity(16);
        let mut promotes = Vec::with_capacity(16);
        let demote_capacity = demotes.capacity();
        let promote_capacity = promotes.capacity();

        for i in 0..8_u64 {
            q.push(handoff(i, DiskHandoffDirection::Demote, 100, i));
            q.push(handoff(i + 100, DiskHandoffDirection::Promote, 200, i));
        }

        q.drain_by_direction_into(&mut demotes, &mut promotes);

        assert!(q.is_empty());
        assert_eq!(demotes.len(), 8);
        assert_eq!(promotes.len(), 8);
        assert_eq!(demotes.capacity(), demote_capacity);
        assert_eq!(promotes.capacity(), promote_capacity);
        assert_eq!(demotes[0].region_id, 0);
        assert_eq!(demotes[7].region_id, 7);
        assert_eq!(promotes[0].region_id, 100);
        assert_eq!(promotes[7].region_id, 107);
    }

    #[test]
    fn disk_handoff_queue_drain_preserves_push_order() {
        let mut q = DiskTierHandoffQueue::new();
        for i in 0..5_u64 {
            q.push(handoff(i, DiskHandoffDirection::Demote, 100, i));
        }
        let drained = q.drain_pending();
        for (i, h) in drained.iter().enumerate() {
            assert_eq!(h.region_id, i as u64);
            assert_eq!(h.frame_id, i as u64);
        }
    }

    #[test]
    fn disk_handoff_queue_drain_preserves_queue_capacity() {
        let mut q = DiskTierHandoffQueue::new();
        for i in 0..32_u64 {
            q.push(handoff(i, DiskHandoffDirection::Demote, 100, i));
        }
        let warm_capacity = q.handoffs.capacity();
        assert!(warm_capacity >= 32);

        let drained = q.drain_pending();

        assert_eq!(drained.len(), 32);
        assert!(q.is_empty());
        assert_eq!(q.handoffs.capacity(), warm_capacity);

        for i in 0..16_u64 {
            q.push(handoff(i, DiskHandoffDirection::Promote, 100, i));
        }
        assert_eq!(q.handoffs.capacity(), warm_capacity);
    }

    #[test]
    fn disk_handoff_queue_pending_bytes_saturates_on_overflow() {
        let mut q = DiskTierHandoffQueue::new();
        q.push(handoff(1, DiskHandoffDirection::Demote, u64::MAX - 100, 0));
        q.push(handoff(2, DiskHandoffDirection::Demote, 200, 0));
        assert_eq!(q.pending_bytes(), u64::MAX);
    }

    // ========================================================================
    // br-ft-ktd19.3 substrate slice: DiskBudgetEstimator (sibling to
    // FrameBudgetSwapDeferrer; gates DiskTierHandoffs on disk-throughput
    // budget per the runbook at docs/render/atlas-disk-tier-handoff-
    // integration.md)
    // ========================================================================

    fn demote(region_id: u64, bytes: u64) -> DiskTierHandoff {
        handoff(region_id, DiskHandoffDirection::Demote, bytes, 0)
    }

    #[test]
    fn disk_budget_default_throughput_is_nvme_class() {
        // Per the runbook table: NVMe SSD ≈ 3 GB/s ≈ 3000 B/µs.
        let d = DiskBudgetEstimator::default();
        assert_eq!(d.bytes_per_microsecond, 3000);
        assert_eq!(d.admitted_bytes(), 0);
    }

    #[test]
    fn disk_budget_with_throughput_overrides_default() {
        let sata = DiskBudgetEstimator::with_throughput(500);
        assert_eq!(sata.bytes_per_microsecond, 500);
        let hdd = DiskBudgetEstimator::with_throughput(150);
        assert_eq!(hdd.bytes_per_microsecond, 150);
    }

    #[test]
    fn disk_budget_cost_microseconds_uses_configured_throughput() {
        let nvme = DiskBudgetEstimator::new(); // 3000 B/µs
        // 3 KiB at 3000 B/µs needs a second microsecond for the
        // fractional tail.
        assert_eq!(nvme.cost_microseconds(3072), 2);
        // 3 MB at 3000 B/µs = 1000 µs.
        assert_eq!(nvme.cost_microseconds(3_000_000), 1_000);
    }

    #[test]
    fn disk_budget_zero_throughput_returns_u64_max() {
        // Defensive — zero throughput would otherwise divide by zero.
        let d = DiskBudgetEstimator::with_throughput(0);
        assert_eq!(d.cost_microseconds(1), u64::MAX);
    }

    #[test]
    fn disk_budget_admit_returns_admit_when_under_budget() {
        let mut d = DiskBudgetEstimator::new();
        // 3000 bytes / 3000 B/µs = 1 µs cost; budget 100 µs.
        let outcome = d.admit_or_defer(&demote(1, 3000), 100);
        assert_eq!(outcome, SwapDeferralOutcome::Admit);
        assert_eq!(d.admitted_bytes(), 3000);
    }

    #[test]
    fn disk_budget_defer_returns_defer_when_over_budget() {
        let mut d = DiskBudgetEstimator::new();
        // First admit: 3000 bytes (1 µs) ≤ budget 1 µs.
        d.admit_or_defer(&demote(1, 3000), 1);
        // Second admit would exceed budget — defer.
        let outcome = d.admit_or_defer(&demote(2, 3000), 1);
        assert_eq!(outcome, SwapDeferralOutcome::Defer);
        // Accumulator unchanged on defer.
        assert_eq!(d.admitted_bytes(), 3000);
    }

    #[test]
    fn disk_budget_zero_throughput_defers_everything() {
        let mut d = DiskBudgetEstimator::with_throughput(0);
        let outcome = d.admit_or_defer(&demote(1, 1), 1_000_000);
        assert_eq!(outcome, SwapDeferralOutcome::Defer);
    }

    #[test]
    fn disk_budget_zero_byte_handoff_admits_under_any_budget() {
        let mut d = DiskBudgetEstimator::new();
        let outcome = d.admit_or_defer(&demote(1, 0), 1);
        assert_eq!(outcome, SwapDeferralOutcome::Admit);
    }

    #[test]
    fn disk_budget_reset_for_new_frame_zeros_accumulator() {
        let mut d = DiskBudgetEstimator::new();
        d.admit_or_defer(&demote(1, 3000), 100);
        assert_eq!(d.admitted_bytes(), 3000);
        d.reset_for_new_frame();
        assert_eq!(d.admitted_bytes(), 0);
    }

    #[test]
    fn disk_budget_partition_preserves_order_and_splits_correctly() {
        let mut d = DiskBudgetEstimator::new(); // 3000 B/µs
        let handoffs = vec![
            demote(1, 3000), // 1 µs (cum 1)
            demote(2, 3000), // 1 µs (cum 2)
            demote(3, 6000), // 2 µs → would push cum to 4 > 3 → defer
            demote(4, 3000), // remains deferred behind #3
            demote(5, 9000), // remains deferred behind #3
        ];
        let (admitted, deferred) = d.partition(&handoffs, 3);
        // Admitted is the fitting prefix.
        assert_eq!(admitted.len(), 2);
        assert_eq!(admitted[0].region_id, 1);
        assert_eq!(admitted[1].region_id, 2);
        // Deferred is the original suffix starting at the first over-budget handoff.
        assert_eq!(deferred.len(), 3);
        assert_eq!(deferred[0].region_id, 3);
        assert_eq!(deferred[1].region_id, 4);
        assert_eq!(deferred[2].region_id, 5);
    }

    #[test]
    fn disk_budget_partition_admits_all_under_generous_budget() {
        let mut d = DiskBudgetEstimator::new();
        let handoffs = vec![demote(1, 100), demote(2, 200), demote(3, 300)];
        let (admitted, deferred) = d.partition(&handoffs, 1_000_000);
        assert_eq!(admitted.len(), 3);
        assert!(deferred.is_empty());
    }

    #[test]
    fn disk_budget_hdd_throughput_defers_on_tight_frame_budget() {
        // HDD class: 150 B/µs (≈150 MB/s). A 1 MB handoff costs
        // ~6700 µs — overruns the 10% disk budget at 60fps (1667 µs).
        let mut d = DiskBudgetEstimator::with_throughput(150);
        let outcome = d.admit_or_defer(&demote(1, 1_000_000), 1_667);
        assert_eq!(outcome, SwapDeferralOutcome::Defer);
    }

    #[test]
    fn disk_budget_promote_handoff_admits_same_as_demote() {
        // Direction is informational from the estimator's view —
        // throughput is the same on read and write at this layer.
        let mut d = DiskBudgetEstimator::new();
        let promote = handoff(1, DiskHandoffDirection::Promote, 3000, 0);
        let outcome = d.admit_or_defer(&promote, 100);
        assert_eq!(outcome, SwapDeferralOutcome::Admit);
        assert_eq!(d.admitted_bytes(), 3000);
    }

    // ========================================================================
    // br-ft-ktd19.3 substrate: EvictionSelectorConfig +
    // bandwidth_aware_demotion_target (eviction-selector configuration
    // plumbing for bandwidth-starved deployments)
    // ========================================================================

    #[test]
    fn eviction_config_default_keeps_host_ram_in_chain() {
        let cfg = EvictionSelectorConfig::default();
        assert!(!cfg.skip_host_ram_on_demote);
        assert_eq!(
            bandwidth_aware_demotion_target(AtlasTier::Vram, cfg),
            Some(AtlasTier::HostRam)
        );
    }

    #[test]
    fn eviction_config_bandwidth_starved_skips_host_ram_on_vram_demote() {
        let cfg = EvictionSelectorConfig::bandwidth_starved();
        assert!(cfg.skip_host_ram_on_demote);
        // VRAM walks straight to Disk under the bandwidth-starved
        // profile.
        assert_eq!(
            bandwidth_aware_demotion_target(AtlasTier::Vram, cfg),
            Some(AtlasTier::Disk)
        );
    }

    #[test]
    fn eviction_config_bandwidth_starved_does_not_change_host_ram_demote() {
        // HostRam already demotes to Disk under the standard chain;
        // the config doesn't shift it.
        let cfg = EvictionSelectorConfig::bandwidth_starved();
        assert_eq!(
            bandwidth_aware_demotion_target(AtlasTier::HostRam, cfg),
            Some(AtlasTier::Disk)
        );
    }

    #[test]
    fn eviction_config_bandwidth_starved_keeps_disk_terminal() {
        // Disk has no demotion target regardless of config — it's
        // the terminal cold tier.
        let cfg = EvictionSelectorConfig::bandwidth_starved();
        assert_eq!(bandwidth_aware_demotion_target(AtlasTier::Disk, cfg), None);
    }

    #[test]
    fn eviction_config_default_matches_atlas_tier_demotion_target_for_every_tier() {
        // The default profile must be a no-op wrapper around the
        // tier's own demotion_target — guards against silent drift
        // when the tier chain changes.
        let cfg = EvictionSelectorConfig::default();
        for tier in [AtlasTier::Vram, AtlasTier::HostRam, AtlasTier::Disk] {
            assert_eq!(
                bandwidth_aware_demotion_target(tier, cfg),
                tier.demotion_target(),
                "default config should pass through tier {tier:?}"
            );
        }
    }
}
