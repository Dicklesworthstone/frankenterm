//! Allocator and per-pane arena helpers for FrankenTerm.
//!
//! This crate keeps allocator wiring out of `frankenterm-core`, which forbids
//! unsafe code and should remain business-logic only.

#![forbid(unsafe_code)]

use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, Ordering};

#[cfg(feature = "jemalloc")]
#[global_allocator]
static GLOBAL_ALLOCATOR: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

/// Active allocator backend selected at compile time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AllocatorBackend {
    /// Default Rust/system allocator.
    System,
    /// Jemalloc via `tikv-jemallocator`.
    Jemalloc,
}

impl AllocatorBackend {
    /// Stable string identifier for diagnostics.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::System => "system",
            Self::Jemalloc => "jemalloc",
        }
    }
}

/// Compile-time allocator backend.
#[must_use]
pub const fn allocator_backend() -> AllocatorBackend {
    #[cfg(feature = "jemalloc")]
    {
        AllocatorBackend::Jemalloc
    }
    #[cfg(not(feature = "jemalloc"))]
    {
        AllocatorBackend::System
    }
}

/// Whether jemalloc is active in this build.
#[must_use]
pub const fn jemalloc_enabled() -> bool {
    matches!(allocator_backend(), AllocatorBackend::Jemalloc)
}

/// Snapshot of allocator-level memory statistics.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct AllocatorStats {
    /// Bytes currently allocated to application objects.
    pub allocated: usize,
    /// Bytes in active pages.
    pub active: usize,
    /// Bytes resident in physical memory.
    pub resident: usize,
    /// Bytes mapped from the OS.
    pub mapped: usize,
    /// Bytes retained by the allocator.
    pub retained: usize,
}

/// Logical per-pane arena accounting snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PaneArenaStats {
    /// Current logical bytes tracked against the pane arena.
    pub tracked_bytes: usize,
    /// Peak logical bytes observed for the pane arena.
    pub peak_tracked_bytes: usize,
    /// Number of accounting updates applied to the pane arena.
    pub updates: u64,
}

/// Errors returned by allocator stats reads.
#[derive(Debug, thiserror::Error)]
pub enum AllocatorStatsError {
    /// Jemalloc stats access failed.
    #[cfg(feature = "jemalloc")]
    #[error("jemalloc stats read failed: {0:?}")]
    Jemalloc(tikv_jemalloc_ctl::Error),
    /// Stats are unavailable because jemalloc support is not compiled in.
    #[error("jemalloc support is disabled (enable feature `jemalloc`)")]
    JemallocNotEnabled,
}

/// Read allocator statistics.
pub fn read_allocator_stats() -> Result<AllocatorStats, AllocatorStatsError> {
    #[cfg(feature = "jemalloc")]
    {
        use tikv_jemalloc_ctl::{epoch, stats};

        epoch::advance().map_err(AllocatorStatsError::Jemalloc)?;
        Ok(AllocatorStats {
            allocated: stats::allocated::read().map_err(AllocatorStatsError::Jemalloc)?,
            active: stats::active::read().map_err(AllocatorStatsError::Jemalloc)?,
            resident: stats::resident::read().map_err(AllocatorStatsError::Jemalloc)?,
            mapped: stats::mapped::read().map_err(AllocatorStatsError::Jemalloc)?,
            retained: stats::retained::read().map_err(AllocatorStatsError::Jemalloc)?,
        })
    }

    #[cfg(not(feature = "jemalloc"))]
    {
        Err(AllocatorStatsError::JemallocNotEnabled)
    }
}

/// Opaque logical arena identifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ArenaId(u32);

impl ArenaId {
    /// Raw numeric identifier.
    #[must_use]
    pub const fn raw(self) -> u32 {
        self.0
    }
}

/// Logical pane arena handle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PaneArena {
    pane_id: u64,
    arena_id: ArenaId,
}

impl PaneArena {
    /// Construct a logical pane arena.
    #[must_use]
    pub const fn new(pane_id: u64, arena_id: ArenaId) -> Self {
        Self { pane_id, arena_id }
    }

    /// Pane owner.
    #[must_use]
    pub const fn pane_id(&self) -> u64 {
        self.pane_id
    }

    /// Logical arena id.
    #[must_use]
    pub const fn arena_id(&self) -> ArenaId {
        self.arena_id
    }
}

/// Snapshot of a logical pane arena plus its accounting state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PaneArenaSnapshot {
    /// Logical pane arena identity.
    pub arena: PaneArena,
    /// Current tracked accounting state.
    pub stats: PaneArenaStats,
}

/// Safe typed ownership wrapper for per-pane arena allocations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Arena<T> {
    owner: PaneArena,
    value: T,
}

impl<T> Arena<T> {
    /// Wrap a value with pane arena ownership metadata.
    #[must_use]
    pub const fn new(owner: PaneArena, value: T) -> Self {
        Self { owner, value }
    }

    /// Owning pane arena metadata.
    #[must_use]
    pub const fn owner(&self) -> PaneArena {
        self.owner
    }

    /// Borrow the wrapped value.
    #[must_use]
    pub const fn as_ref(&self) -> &T {
        &self.value
    }

    /// Mutably borrow the wrapped value.
    #[must_use]
    pub const fn as_mut(&mut self) -> &mut T {
        &mut self.value
    }

    /// Consume wrapper and return inner value.
    #[must_use]
    pub fn into_inner(self) -> T {
        self.value
    }

    /// Map to another value while preserving ownership metadata.
    #[must_use]
    pub fn map<U>(self, f: impl FnOnce(T) -> U) -> Arena<U> {
        Arena {
            owner: self.owner,
            value: f(self.value),
        }
    }
}

/// Outcome when reserving a pane arena.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReserveOutcome {
    /// Brand new logical arena created.
    Created(PaneArena),
    /// Existing reservation reused.
    Existing(PaneArena),
}

impl ReserveOutcome {
    /// Access the resolved pane arena.
    #[must_use]
    pub const fn arena(self) -> PaneArena {
        match self {
            Self::Created(arena) | Self::Existing(arena) => arena,
        }
    }

    /// Whether this call created a fresh reservation.
    #[must_use]
    pub const fn is_created(self) -> bool {
        matches!(self, Self::Created(_))
    }
}

/// Concurrent registry of logical per-pane arena reservations.
#[derive(Debug, Default)]
pub struct PaneArenaRegistry {
    next_arena_id: AtomicU32,
    arenas_by_pane: Mutex<HashMap<u64, PaneArenaState>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PaneArenaState {
    arena: PaneArena,
    stats: PaneArenaStats,
}

impl PaneArenaState {
    fn new(arena: PaneArena) -> Self {
        Self {
            arena,
            stats: PaneArenaStats::default(),
        }
    }

    fn update_tracked_bytes(&mut self, tracked_bytes: usize) -> PaneArenaStats {
        self.stats.tracked_bytes = tracked_bytes;
        self.stats.peak_tracked_bytes = self.stats.peak_tracked_bytes.max(tracked_bytes);
        self.stats.updates = self.stats.updates.saturating_add(1);
        self.stats
    }
}

impl PaneArenaRegistry {
    /// Create an empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self {
            next_arena_id: AtomicU32::new(1),
            arenas_by_pane: Mutex::new(HashMap::new()),
        }
    }

    /// Reserve (or reuse) a logical arena for the given pane.
    pub fn reserve(&self, pane_id: u64) -> ReserveOutcome {
        let mut guard = self
            .arenas_by_pane
            .lock()
            .expect("pane arena registry lock poisoned");

        if let Some(existing) = guard.get(&pane_id).copied() {
            return ReserveOutcome::Existing(existing.arena);
        }

        let arena = PaneArena::new(pane_id, self.allocate_arena_id());
        guard.insert(pane_id, PaneArenaState::new(arena));
        ReserveOutcome::Created(arena)
    }

    /// Release a pane reservation.
    pub fn release(&self, pane_id: u64) -> Option<PaneArena> {
        self.arenas_by_pane
            .lock()
            .expect("pane arena registry lock poisoned")
            .remove(&pane_id)
            .map(|state| state.arena)
    }

    /// Update the logical tracked byte count for a pane arena.
    pub fn set_tracked_bytes(&self, pane_id: u64, tracked_bytes: usize) -> Option<PaneArenaStats> {
        self.arenas_by_pane
            .lock()
            .expect("pane arena registry lock poisoned")
            .get_mut(&pane_id)
            .map(|state| state.update_tracked_bytes(tracked_bytes))
    }

    /// Lookup the accounting state for a pane reservation.
    pub fn stats(&self, pane_id: u64) -> Option<PaneArenaStats> {
        self.arenas_by_pane
            .lock()
            .expect("pane arena registry lock poisoned")
            .get(&pane_id)
            .map(|state| state.stats)
    }

    /// Lookup an existing pane reservation.
    pub fn get(&self, pane_id: u64) -> Option<PaneArena> {
        self.arenas_by_pane
            .lock()
            .expect("pane arena registry lock poisoned")
            .get(&pane_id)
            .map(|state| state.arena)
    }

    /// Number of tracked pane reservations.
    #[must_use]
    pub fn count(&self) -> usize {
        self.arenas_by_pane
            .lock()
            .expect("pane arena registry lock poisoned")
            .len()
    }

    /// Whether the registry is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.count() == 0
    }

    /// Sorted snapshot of all pane reservations.
    #[must_use]
    pub fn snapshot(&self) -> Vec<PaneArena> {
        let mut values: Vec<_> = self
            .arenas_by_pane
            .lock()
            .expect("pane arena registry lock poisoned")
            .values()
            .map(|state| state.arena)
            .collect();
        values.sort_by_key(PaneArena::pane_id);
        values
    }

    /// Sorted snapshot of all pane reservations with accounting state.
    #[must_use]
    pub fn stats_snapshot(&self) -> Vec<PaneArenaSnapshot> {
        let mut values: Vec<_> = self
            .arenas_by_pane
            .lock()
            .expect("pane arena registry lock poisoned")
            .values()
            .map(|state| PaneArenaSnapshot {
                arena: state.arena,
                stats: state.stats,
            })
            .collect();
        values.sort_by_key(|snapshot| snapshot.arena.pane_id());
        values
    }

    fn allocate_arena_id(&self) -> ArenaId {
        let raw = self.next_arena_id.fetch_add(1, Ordering::Relaxed);
        assert_ne!(raw, 0, "pane arena id space exhausted");
        ArenaId(raw)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allocator_backend_matches_feature_gate() {
        #[cfg(feature = "jemalloc")]
        assert_eq!(allocator_backend(), AllocatorBackend::Jemalloc);

        #[cfg(not(feature = "jemalloc"))]
        assert_eq!(allocator_backend(), AllocatorBackend::System);
    }

    #[test]
    fn arena_wrapper_preserves_owner_metadata() {
        let owner = PaneArena::new(7, ArenaId(42));
        let arena = Arena::new(owner, String::from("value"));
        assert_eq!(arena.owner(), owner);
        assert_eq!(arena.as_ref(), "value");

        let mapped = arena.map(|value| value.len());
        assert_eq!(mapped.owner(), owner);
        assert_eq!(mapped.into_inner(), 5);
    }

    #[test]
    fn registry_reserve_reuse_release_cycle() {
        let registry = PaneArenaRegistry::new();
        assert!(registry.is_empty());

        let first = registry.reserve(11);
        assert!(first.is_created());
        assert_eq!(registry.count(), 1);

        let second = registry.reserve(11);
        assert!(!second.is_created());
        assert_eq!(first.arena(), second.arena());

        let released = registry.release(11).expect("reservation should exist");
        assert_eq!(released, first.arena());
        assert!(registry.is_empty());
    }

    #[test]
    fn registry_snapshot_is_sorted_and_stable() {
        let registry = PaneArenaRegistry::new();
        let _ = registry.reserve(30);
        let _ = registry.reserve(10);
        let _ = registry.reserve(20);

        let panes: Vec<_> = registry
            .snapshot()
            .into_iter()
            .map(|arena| arena.pane_id())
            .collect();
        assert_eq!(panes, vec![10, 20, 30]);
    }

    #[test]
    fn registry_tracks_logical_bytes_and_peak_usage() {
        let registry = PaneArenaRegistry::new();
        let arena = registry.reserve(17).arena();

        let first = registry
            .set_tracked_bytes(17, 64)
            .expect("pane arena stats should exist");
        assert_eq!(first.tracked_bytes, 64);
        assert_eq!(first.peak_tracked_bytes, 64);
        assert_eq!(first.updates, 1);

        let second = registry
            .set_tracked_bytes(17, 32)
            .expect("pane arena stats should exist");
        assert_eq!(second.tracked_bytes, 32);
        assert_eq!(second.peak_tracked_bytes, 64);
        assert_eq!(second.updates, 2);

        let snapshot = registry.stats_snapshot();
        assert_eq!(snapshot.len(), 1);
        assert_eq!(snapshot[0].arena, arena);
        assert_eq!(snapshot[0].stats, second);

        let released = registry.release(17).expect("pane arena should exist");
        assert_eq!(released, arena);
        assert!(registry.stats(17).is_none());
        assert!(registry.stats_snapshot().is_empty());
    }

    #[test]
    fn allocator_stats_api_matches_feature_mode() {
        #[cfg(feature = "jemalloc")]
        {
            let stats = read_allocator_stats().expect("jemalloc stats should be readable");
            assert!(stats.resident >= stats.allocated);
            assert!(stats.mapped >= stats.resident);
        }

        #[cfg(not(feature = "jemalloc"))]
        {
            let err = read_allocator_stats().expect_err("stats should be unavailable");
            assert!(matches!(err, AllocatorStatsError::JemallocNotEnabled));
        }
    }
}
