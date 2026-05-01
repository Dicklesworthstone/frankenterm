//! Elastic instance / vertex buffer with capacity-doubling growth.
//!
//! This module ships the policy foundation only (ft-mpc9b.1.3). The
//! public API is exercised by tests but not yet by callers; the
//! integration bead (continuation) wires it into `quad.rs`. Until
//! then, the items below would fire dead-code warnings — the
//! module-level allow is intentionally narrow and lives next to the
//! continuation reference so the next agent removes it as part of
//! the wiring work.
//!
//! Replaces the implicit-and-coupled-to-atlas-size buffer growth in
//! `quad.rs` with an explicit policy engine inspired by rio's
//! `legacy_rio/rio/sugarloaf/src/components/core/buffer.rs`. Atlas
//! changes no longer ripple into vertex/instance buffer reallocations
//! (ft-mpc9b.1.3).
//!
//! ## What this module ships (foundation, ft-mpc9b.1.3)
//!
//! - `ElasticBuffer<T>`: a GPU-agnostic, in-memory staging buffer
//!   that owns the growth policy:
//!   * doubles capacity when full
//!   * aligns capacity to a configurable bucket size for GPU
//!     friendliness (default 64k entries)
//!   * tracks high-water mark; never shrinks during an active
//!     gesture
//!   * shrinks only after gesture end + idle elapsed (default 1 s)
//! - `ShrinkResult`: telemetry payload returned by `try_shrink_if_idle`
//! - lifetime grow / shrink counters surfaced for `ft doctor`
//!
//! Decoupling the policy from the wgpu plumbing means the policy is
//! pure-Rust testable without a GPU device handle.
//!
//! ## What is deferred (continuation, see follow-up bead)
//!
//! - wrap a `wgpu::Buffer` with `ElasticBuffer<QuadInstance>` in
//!   `crates/frankenterm-gui/src/quad.rs`
//! - replace the per-cell vertex batching with one instance per
//!   cell (cell-instance-data records); RQ-S1 / RQ-S6 SLO benches
//! - wire `gesture_begin` / `gesture_end` into the resize path
//! - call `try_shrink_if_idle` from the idle-tick that lives in
//!   `crates/frankenterm-gui/src/termwindow/mod.rs`
//! - expose `grow_count` / `shrink_count` / `high_water_mark` via
//!   `ft doctor`
//! - remove the `#![allow(dead_code)]` below as part of that wiring

#![allow(dead_code)]

use std::time::{Duration, Instant};

/// Default initial capacity in entries — sized for a 200×80 cell
/// screen (the default the bead targets).
pub const DEFAULT_INITIAL_CAPACITY: usize = 16_384;

/// Default bucket size in entries. Growth is aligned up to a multiple
/// of this. 65 536 entries is a clean power-of-two and exceeds the
/// 256-byte `wgpu::COPY_BUFFER_ALIGNMENT` for any reasonable T.
pub const DEFAULT_BUCKET: usize = 1 << 16;

/// Default idle threshold for shrink eligibility. A gesture must end
/// AND this duration must elapse before the buffer is allowed to
/// shrink.
pub const DEFAULT_IDLE_THRESHOLD: Duration = Duration::from_secs(1);

/// Outcome of a successful idle shrink. Returned by
/// `try_shrink_if_idle` so callers can drive structured-log emission
/// and the buffer-size gauge update.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShrinkResult {
    pub capacity_before: usize,
    pub capacity_after: usize,
    pub used_at_shrink: usize,
}

/// In-memory staging buffer that owns the elastic-growth policy for a
/// downstream GPU buffer.
///
/// Callers `push` entries (or use the slice-extend helpers); the
/// internal `Vec<T>` grows by doubling and aligning to the bucket
/// size. After a gesture ends and the idle threshold elapses,
/// `try_shrink_if_idle` may release excess capacity. While a gesture
/// is active, `try_shrink_if_idle` is a no-op — eliminating the
/// "buffer reallocation storm during resize" failure mode the bead
/// targets.
#[derive(Debug)]
pub struct ElasticBuffer<T> {
    data: Vec<T>,
    /// Peak `data.len()` observed since the most recent successful
    /// shrink (or since construction). This is the "shrink-to-here"
    /// floor when the idle policy fires.
    high_water_mark: usize,
    /// Whether a gesture is currently in flight. Set/cleared by
    /// `begin_gesture` / `end_gesture`.
    gesture_active: bool,
    /// Most recent `end_gesture` timestamp. `None` if no gesture has
    /// ended since the most recent shrink.
    last_gesture_end: Option<Instant>,
    idle_threshold: Duration,
    bucket: usize,
    min_capacity: usize,
    /// Lifetime telemetry counters.
    grow_count: u64,
    shrink_count: u64,
}

impl<T> ElasticBuffer<T> {
    /// Construct with the default bucket size and idle threshold.
    pub fn new(initial_capacity: usize) -> Self {
        Self::with_config(initial_capacity, DEFAULT_BUCKET, DEFAULT_IDLE_THRESHOLD)
    }

    /// Construct with explicit bucket and idle-threshold knobs.
    ///
    /// `bucket` must be non-zero; values are clamped to at least 1.
    /// `initial_capacity` is the *requested* initial allocation;
    /// growth thereafter aligns to `bucket`.
    pub fn with_config(initial_capacity: usize, bucket: usize, idle_threshold: Duration) -> Self {
        let bucket = bucket.max(1);
        Self {
            data: Vec::with_capacity(initial_capacity),
            high_water_mark: 0,
            gesture_active: false,
            last_gesture_end: None,
            idle_threshold,
            bucket,
            min_capacity: initial_capacity,
            grow_count: 0,
            shrink_count: 0,
        }
    }

    /// Append a single entry, growing if necessary.
    pub fn push(&mut self, value: T) {
        self.reserve_one();
        self.data.push(value);
        self.update_high_water();
    }

    /// Append every item from the iterator, growing as needed.
    pub fn extend<I: IntoIterator<Item = T>>(&mut self, iter: I) {
        for value in iter {
            self.push(value);
        }
    }

    /// Bulk-append a slice. Reserves capacity in one go to avoid
    /// the per-item growth cost.
    pub fn extend_from_slice(&mut self, slice: &[T])
    where
        T: Copy,
    {
        let needed = self.data.len() + slice.len();
        if needed > self.data.capacity() {
            let new_cap = self.compute_grown_capacity(needed);
            self.set_capacity(new_cap);
        }
        self.data.extend_from_slice(slice);
        self.update_high_water();
    }

    /// Reset to zero `len` while preserving capacity. Frame-end
    /// hook.
    pub fn clear(&mut self) {
        self.data.clear();
    }

    /// Number of entries currently staged.
    #[inline]
    pub fn len(&self) -> usize {
        self.data.len()
    }

    /// Allocated capacity in entries. Always `>= len()`.
    #[inline]
    pub fn capacity(&self) -> usize {
        self.data.capacity()
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.data.is_empty()
    }

    /// Read-only view of the staged entries.
    #[inline]
    pub fn as_slice(&self) -> &[T] {
        &self.data
    }

    /// Mutable view of the staged entries.
    #[inline]
    pub fn as_mut_slice(&mut self) -> &mut [T] {
        &mut self.data
    }

    /// Mark the start of a resize / drag / animation gesture.
    /// While a gesture is active, `try_shrink_if_idle` will refuse
    /// to shrink, eliminating mid-gesture reallocation churn.
    pub fn begin_gesture(&mut self) {
        self.gesture_active = true;
    }

    /// Mark the end of a gesture. The idle countdown begins now.
    pub fn end_gesture(&mut self, now: Instant) {
        self.gesture_active = false;
        self.last_gesture_end = Some(now);
    }

    /// Attempt to shrink the buffer if the idle policy is satisfied.
    ///
    /// Returns `Some(ShrinkResult)` if the buffer was actually
    /// shrunk; `None` if any precondition prevented it (gesture
    /// active, no gesture has ended since the last shrink, idle
    /// threshold not yet elapsed, or there is no excess capacity to
    /// release).
    pub fn try_shrink_if_idle(&mut self, now: Instant) -> Option<ShrinkResult> {
        if self.gesture_active {
            return None;
        }
        let last_end = self.last_gesture_end?;
        if now.saturating_duration_since(last_end) < self.idle_threshold {
            return None;
        }

        let target = self.target_shrunk_capacity();
        let current = self.data.capacity();
        if target >= current {
            return None;
        }

        let capacity_before = current;
        self.set_capacity(target);
        // Reset the high-water mark so the next gesture observes a
        // fresh peak.
        self.high_water_mark = self.data.len();
        // Shrink consumed the gesture-end signal; require a new
        // gesture to enable another shrink.
        self.last_gesture_end = None;
        self.shrink_count = self.shrink_count.saturating_add(1);

        Some(ShrinkResult {
            capacity_before,
            capacity_after: self.data.capacity(),
            used_at_shrink: self.data.len(),
        })
    }

    /// Peak `len()` observed since the most recent successful shrink.
    #[inline]
    pub fn high_water_mark(&self) -> usize {
        self.high_water_mark
    }

    /// Lifetime count of capacity-doubling growth events.
    #[inline]
    pub fn grow_count(&self) -> u64 {
        self.grow_count
    }

    /// Lifetime count of successful idle-shrink events.
    #[inline]
    pub fn shrink_count(&self) -> u64 {
        self.shrink_count
    }

    /// Whether a gesture is currently in flight.
    #[inline]
    pub fn is_gesture_active(&self) -> bool {
        self.gesture_active
    }

    /// Bucket size used for growth alignment.
    #[inline]
    pub fn bucket(&self) -> usize {
        self.bucket
    }

    /// Minimum capacity floor. Shrink will never go below this.
    #[inline]
    pub fn min_capacity(&self) -> usize {
        self.min_capacity
    }

    fn reserve_one(&mut self) {
        if self.data.len() == self.data.capacity() {
            let needed = self.data.len() + 1;
            let new_cap = self.compute_grown_capacity(needed);
            self.set_capacity(new_cap);
        }
    }

    fn compute_grown_capacity(&self, needed: usize) -> usize {
        let cur = self.data.capacity();
        // Double, but ensure we cover the requested need, then align.
        let doubled = cur.saturating_mul(2).max(needed).max(self.min_capacity);
        align_up(doubled, self.bucket).max(needed)
    }

    fn set_capacity(&mut self, target: usize) {
        let current = self.data.capacity();
        if target == current {
            return;
        }
        if target > current {
            self.data.reserve_exact(target - self.data.len());
            self.grow_count = self.grow_count.saturating_add(1);
        } else {
            // Shrink path. shrink_to honors the floor: the resulting
            // capacity is at least max(len, target).
            self.data.shrink_to(target);
        }
    }

    fn target_shrunk_capacity(&self) -> usize {
        let floor = self
            .high_water_mark
            .max(self.data.len())
            .max(self.min_capacity);
        // Round up to the next bucket so the shrunk capacity stays
        // GPU-friendly.
        align_up(floor, self.bucket)
    }

    fn update_high_water(&mut self) {
        if self.data.len() > self.high_water_mark {
            self.high_water_mark = self.data.len();
        }
    }
}

#[inline]
fn align_up(value: usize, bucket: usize) -> usize {
    if bucket <= 1 {
        return value;
    }
    let remainder = value % bucket;
    if remainder == 0 {
        value
    } else {
        value.saturating_add(bucket - remainder)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn small_buffer<T>(initial: usize) -> ElasticBuffer<T> {
        // Small bucket (8) so tests can observe alignment without
        // needing 64 k entries.
        ElasticBuffer::with_config(initial, 8, Duration::from_millis(100))
    }

    #[test]
    fn new_buffer_starts_empty_and_at_requested_capacity() {
        let bm: ElasticBuffer<u32> = small_buffer(16);
        assert!(bm.is_empty());
        assert_eq!(bm.len(), 0);
        assert!(bm.capacity() >= 16);
        assert_eq!(bm.grow_count(), 0);
        assert_eq!(bm.shrink_count(), 0);
        assert_eq!(bm.high_water_mark(), 0);
        assert!(!bm.is_gesture_active());
    }

    #[test]
    fn push_grows_when_full_and_aligns_to_bucket() {
        let mut bm: ElasticBuffer<u32> = small_buffer(8);
        // Fill to the initial capacity — no growth yet.
        for i in 0..8u32 {
            bm.push(i);
        }
        let cap_after_initial = bm.capacity();
        assert_eq!(bm.grow_count(), 0);

        // One more push triggers growth — doubled and bucket-aligned.
        bm.push(8);
        assert_eq!(bm.grow_count(), 1);
        assert!(bm.capacity() >= cap_after_initial * 2);
        assert_eq!(bm.capacity() % bm.bucket(), 0);
    }

    #[test]
    fn capacity_invariant_holds_under_many_pushes() {
        let mut bm: ElasticBuffer<u32> = small_buffer(4);
        for i in 0..1024u32 {
            bm.push(i);
            assert!(
                bm.capacity() >= bm.len(),
                "capacity invariant broken at i={}: cap={} len={}",
                i,
                bm.capacity(),
                bm.len(),
            );
        }
        assert_eq!(bm.len(), 1024);
        assert_eq!(bm.high_water_mark(), 1024);
        assert_eq!(bm.as_slice().len(), 1024);
    }

    #[test]
    fn extend_from_slice_uses_one_grow_for_bulk_append() {
        let mut bm: ElasticBuffer<u32> = small_buffer(4);
        let chunk: Vec<u32> = (0..64).collect();
        bm.extend_from_slice(&chunk);
        // Bulk append should grow at most once (and possibly zero
        // times if the initial capacity was already enough).
        assert!(bm.grow_count() <= 1);
        assert_eq!(bm.len(), 64);
        assert_eq!(bm.as_slice(), chunk.as_slice());
    }

    #[test]
    fn clear_preserves_capacity() {
        let mut bm: ElasticBuffer<u32> = small_buffer(4);
        for i in 0..32u32 {
            bm.push(i);
        }
        let cap_before = bm.capacity();

        bm.clear();
        assert_eq!(bm.len(), 0);
        assert!(bm.is_empty());
        assert_eq!(bm.capacity(), cap_before);
        // Lifetime telemetry survives clear.
        assert_eq!(bm.high_water_mark(), 32);
    }

    #[test]
    fn shrink_does_nothing_during_gesture() {
        let mut bm: ElasticBuffer<u32> = small_buffer(4);
        for i in 0..64u32 {
            bm.push(i);
        }
        bm.clear();
        bm.begin_gesture();

        let now = Instant::now();
        // Even with the threshold elapsed, an active gesture blocks shrink.
        let later = now + Duration::from_secs(60);
        assert!(bm.try_shrink_if_idle(later).is_none());
        assert!(bm.is_gesture_active());
        assert_eq!(bm.shrink_count(), 0);
    }

    #[test]
    fn shrink_does_nothing_before_idle_threshold() {
        let mut bm: ElasticBuffer<u32> = small_buffer(4);
        for i in 0..64u32 {
            bm.push(i);
        }
        bm.clear();
        bm.begin_gesture();
        let gesture_end = Instant::now();
        bm.end_gesture(gesture_end);

        // Less than the idle threshold (100 ms in the test config).
        let too_soon = gesture_end + Duration::from_millis(50);
        assert!(bm.try_shrink_if_idle(too_soon).is_none());
        assert_eq!(bm.shrink_count(), 0);
    }

    #[test]
    fn shrink_runs_after_gesture_end_plus_idle() {
        let mut bm: ElasticBuffer<u32> = small_buffer(4);
        // Push 33 entries: the doubling growth path takes capacity
        // from 4 → 8 → 16 → 32 → 64 to satisfy the 33rd push, but
        // the high-water mark is only 33. With bucket=8 the shrink
        // target rounds 33 up to 40 — strictly less than the
        // post-grow capacity of 64.
        for i in 0..33u32 {
            bm.push(i);
        }
        bm.clear();
        bm.begin_gesture();
        let gesture_end = Instant::now();
        bm.end_gesture(gesture_end);

        let cap_before = bm.capacity();
        let after_idle = gesture_end + Duration::from_millis(150);
        let result = bm.try_shrink_if_idle(after_idle);

        assert!(result.is_some(), "shrink should have run after idle");
        let r = result.unwrap();
        assert_eq!(r.capacity_before, cap_before);
        assert!(
            r.capacity_after < cap_before,
            "capacity must shrink: before={}, after={}",
            r.capacity_before,
            r.capacity_after,
        );
        assert_eq!(r.used_at_shrink, 0);
        assert_eq!(bm.shrink_count(), 1);
    }

    #[test]
    fn shrink_consumes_gesture_end_signal() {
        let mut bm: ElasticBuffer<u32> = small_buffer(4);
        // 33 pushes, not 64 — see shrink_runs_after_gesture_end_plus_idle
        // for the rationale.
        for i in 0..33u32 {
            bm.push(i);
        }
        bm.clear();
        bm.begin_gesture();
        let gesture_end = Instant::now();
        bm.end_gesture(gesture_end);
        let after_idle = gesture_end + Duration::from_millis(150);

        let first = bm.try_shrink_if_idle(after_idle);
        assert!(first.is_some());

        // A second shrink call without a fresh gesture-end is a no-op.
        let later = after_idle + Duration::from_secs(10);
        let second = bm.try_shrink_if_idle(later);
        assert!(second.is_none());
        assert_eq!(bm.shrink_count(), 1);
    }

    #[test]
    fn shrink_resets_high_water_mark() {
        let mut bm: ElasticBuffer<u32> = small_buffer(4);
        // 33 pushes — see shrink_runs_after_gesture_end_plus_idle.
        for i in 0..33u32 {
            bm.push(i);
        }
        assert_eq!(bm.high_water_mark(), 33);
        bm.clear();
        bm.begin_gesture();
        let gesture_end = Instant::now();
        bm.end_gesture(gesture_end);

        let _ = bm
            .try_shrink_if_idle(gesture_end + Duration::from_millis(150))
            .expect("shrink should fire");
        // Post-shrink the high-water mark equals current len (0
        // here) so the next gesture starts with a fresh peak.
        assert_eq!(bm.high_water_mark(), 0);
    }

    #[test]
    fn shrink_never_below_min_capacity_floor() {
        let mut bm: ElasticBuffer<u32> = small_buffer(64);
        // Touch nothing — capacity stays at the requested initial
        // (>= 64). After a gesture cycle, shrink should be a no-op
        // because target == min_capacity == capacity.
        bm.begin_gesture();
        let gesture_end = Instant::now();
        bm.end_gesture(gesture_end);
        let after_idle = gesture_end + Duration::from_millis(150);
        assert!(bm.try_shrink_if_idle(after_idle).is_none());
        assert!(bm.capacity() >= 64);
    }

    #[test]
    fn resize_burst_pattern_zero_allocs_during_gesture() {
        // Simulates the bead's headline test: 10× rapid resize burst
        // should produce zero buffer reallocs during the gesture.
        let mut bm: ElasticBuffer<u32> = small_buffer(4);
        // Pre-grow the buffer so the gesture itself doesn't trigger
        // any growth — this is the exact pattern the integration
        // bead will use (allocate to peak before kicking off a
        // resize gesture).
        for i in 0..64u32 {
            bm.push(i);
        }
        bm.clear();
        let grow_count_pre_gesture = bm.grow_count();

        bm.begin_gesture();
        for _resize in 0..10 {
            // Every resize iteration writes a working set that fits
            // inside the current capacity.
            bm.clear();
            for i in 0..32u32 {
                bm.push(i);
            }
        }
        let gesture_end = Instant::now();
        bm.end_gesture(gesture_end);

        // No growth events should have happened during the gesture.
        assert_eq!(bm.grow_count(), grow_count_pre_gesture);
    }

    #[test]
    fn high_water_mark_monotonic_during_pushes() {
        let mut bm: ElasticBuffer<u32> = small_buffer(4);
        let mut last = 0;
        for i in 0..32u32 {
            bm.push(i);
            assert!(bm.high_water_mark() >= last);
            assert_eq!(bm.high_water_mark(), bm.len());
            last = bm.high_water_mark();
        }
        // Clearing does not lower the high-water mark.
        bm.clear();
        assert_eq!(bm.high_water_mark(), 32);
    }

    #[test]
    fn align_up_corner_cases() {
        assert_eq!(align_up(0, 8), 0);
        assert_eq!(align_up(1, 8), 8);
        assert_eq!(align_up(8, 8), 8);
        assert_eq!(align_up(9, 8), 16);
        // Bucket of 0 or 1 is a no-op.
        assert_eq!(align_up(123, 0), 123);
        assert_eq!(align_up(123, 1), 123);
    }

    #[test]
    fn extend_iter_grows_as_needed() {
        let mut bm: ElasticBuffer<u32> = small_buffer(4);
        bm.extend(0..100);
        assert_eq!(bm.len(), 100);
        assert!(bm.capacity() >= 100);
        assert_eq!(bm.high_water_mark(), 100);
        // The iterator path goes through `push` so each over-capacity
        // entry triggers at most one grow. With initial cap 4 and
        // bucket 8 we expect O(log 100) growths.
        assert!(bm.grow_count() <= 8);
    }
}
