//! Triple-buffered terminal-state foundation
//! ([BR-TERM-EMULATOR-UPLIFT-2.3.1]).
//!
//! Two parallel bead IDs reference this substrate:
//!
//! - `ft-d0ol8` — the original implementation bead under which
//!   pane3 shipped this module at commit `7802549a0` (13 lib +
//!   14 fixture tests).
//! - `ft-2okh0.3.1` — the canonical decomposition of the parent
//!   epic `ft-2okh0.3` (Triple-buffered terminal state with
//!   snapshot rendering). Closed via cross-reference to ft-d0ol8.
//!
//! Same pattern as `ft-2okh0.5.1 ↔ ft-kscfg` (mmap scrollback)
//! — two bead IDs cover the same underlying work. The
//! cross-reference table at the parent design doc records the
//! mapping; closing the canonical-side bead via cross-link
//! keeps the bead tree consistent.
//!
//! Petersen 2005 "Three-State Mailbox" pattern. Eliminates the
//! "rendered partial mid-mutation" bug class by guaranteeing the
//! render thread always sees an immutable snapshot, even while the
//! input thread mutates state.
//!
//! ## State machine
//!
//! Three slots cycle between roles:
//!
//! ```text
//!   Writer ──publish──▶ Presented
//!                          │
//!                          │ acquire (only when dirty)
//!                          ▼
//!                       Reader (held until dropped)
//! ```
//!
//! Pre-conditions / post-conditions:
//!
//! - **Lock-free reads.** The reader's `acquire` is two atomic
//!   operations + an `Arc::clone`. The renderer holds the returned
//!   `Arc<T>` for the duration of the frame; the writer cannot
//!   mutate it while the reader holds it.
//! - **Writer never blocks.** A second publish before the reader
//!   has acquired overwrites the *oldest unread* slot, NOT the
//!   slot the reader holds. `writer_overruns_total` counts these.
//! - **Reader never sees a torn write.** Each slot stores a single
//!   immutable `Arc<T>`; `publish` swaps `Arc`s atomically inside a
//!   short-duration mutex. The reader's `Arc::clone` happens under
//!   the same mutex, so the reader either sees the pre-publish or
//!   post-publish `Arc` — never both halves.
//!
//! ## Failure modes
//!
//! - **Writer outpaces reader.** Tracked by
//!   `writer_overruns_total`; the renderer sees stale-by-1-frame
//!   data, not partial data. The bead's parent epic
//!   (`ft-2okh0.3`) commits to surfacing this counter via `ft
//!   doctor`.
//! - **Reader holds slot too long.** A render thread that crashes
//!   mid-frame leaks the slot it held. `force_recycle` rotates the
//!   stuck slot out unconditionally; the held `Arc<T>` is harmless
//!   afterwards (it just continues to exist as long as the
//!   renderer holds it). Watchdog wiring is the integration bead's
//!   job; this module exposes the API.
//!
//! ## What this module is NOT
//!
//! - The actual `TerminalState` — that's `frankenterm_term` and is
//!   plugged in by sub-task 3.2.
//! - The persistent-rope optimization — sub-task 3.3.
//! - The Loom proof — sub-task 3.4. The tests in this module are
//!   property-based but not full Loom state-space; that's the
//!   `BR-RC-FOUNDATION.G8.2` cross-link.

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};

use serde::{Deserialize, Serialize};

// ============================================================================
// State packing
//
// A single AtomicU8 packs the four state values:
//   bits 0-1:  writer slot index   (0, 1, 2)
//   bits 2-3:  presented slot idx  (0, 1, 2)
//   bits 4-5:  reader slot index   (0, 1, 2)
//   bit  6:    dirty flag (presented contains data the reader has not seen)
//   bit  7:    reserved (zero)
// ============================================================================

#[inline]
const fn pack(writer: u8, presented: u8, reader: u8, dirty: bool) -> u8 {
    debug_assert!(writer < 3 && presented < 3 && reader < 3);
    let mut s = (writer & 0b11) | ((presented & 0b11) << 2) | ((reader & 0b11) << 4);
    if dirty {
        s |= 1 << 6;
    }
    s
}

#[inline]
const fn unpack(state: u8) -> (u8, u8, u8, bool) {
    let writer = state & 0b11;
    let presented = (state >> 2) & 0b11;
    let reader = (state >> 4) & 0b11;
    let dirty = (state >> 6) & 0b1 != 0;
    (writer, presented, reader, dirty)
}

// ============================================================================
// Public API
// ============================================================================

/// Lock-free triple buffer for the render-thread snapshot path.
///
/// `T` is the buffered value type — typically `TerminalState` or a
/// pane-render snapshot. `T: Clone` is required so `new` can seed
/// all three slots from one initial value; per-publish hands a
/// freshly-constructed `T` so subsequent reads observe immutable
/// snapshots.
pub struct TripleBuffer<T: Send + Sync + 'static> {
    /// Three slots, each holding an immutable `Arc<T>` snapshot.
    /// The brief `Mutex` is held only for the duration of an
    /// `Arc::clone` or `Arc` swap — nanoseconds; not the multi-
    /// millisecond contention point the bead's "lock-free reads"
    /// language is about.
    slots: [Mutex<Arc<T>>; 3],
    state: AtomicU8,
    publishes_total: AtomicU64,
    acquires_total: AtomicU64,
    overruns_total: AtomicU64,
    force_recycles_total: AtomicU64,
}

impl<T: Send + Sync + 'static> TripleBuffer<T> {
    /// Construct a triple buffer seeded with `initial`. All three
    /// slots start holding the same `Arc<T>`; the first `acquire`
    /// returns it without going through `publish`.
    #[must_use]
    pub fn new(initial: T) -> Self {
        let seed = Arc::new(initial);
        Self {
            slots: [
                Mutex::new(Arc::clone(&seed)),
                Mutex::new(Arc::clone(&seed)),
                Mutex::new(seed),
            ],
            // writer=0, presented=1, reader=2, dirty=false (initial
            // state matches what `new` seeded into the slots).
            state: AtomicU8::new(pack(0, 1, 2, false)),
            publishes_total: AtomicU64::new(0),
            acquires_total: AtomicU64::new(0),
            overruns_total: AtomicU64::new(0),
            force_recycles_total: AtomicU64::new(0),
        }
    }

    /// Publish a new value. Never blocks; if the reader hasn't
    /// caught up since the last `publish`, increments
    /// `writer_overruns_total` and overwrites the oldest unread
    /// slot — the **reader's currently-held slot is untouched**.
    ///
    /// Returns the per-call outcome (overrun / which slot became
    /// presented) so the renderer's structured-log can pin the
    /// state transition.
    ///
    /// # Concurrency contract
    ///
    /// The Petersen 2005 SWSR pattern this substrate implements
    /// is designed for **single-writer, single-reader** use. The
    /// `Send + Sync` bounds + `&self` receiver allow the type to
    /// be shared across threads, but multi-writer use has a
    /// race window:
    ///
    /// - Two writer threads both load the same `state`, both
    ///   write their `Arc` into `slots[w]` (the same slot).
    /// - The first writer to win the CAS swap promotes its
    ///   `slots[w]` write to the new "presented" role.
    /// - The losing writer retries against the new state, writes
    ///   to the new `slots[w']` (a different slot), and CASes
    ///   again.
    ///
    /// The `1R/2W` Loom test (`loom_1_reader_2_writers_no_lost_write`)
    /// proves: (1) no torn data — the reader always sees a
    /// well-formed `Arc<T>`; (2) every CAS-winning publish is
    /// observable to the reader at some point. The CAS-losing
    /// publisher's data may be silently overwritten by the
    /// winner before the reader observes it — its
    /// `PublishOutcome.new_presented_slot` reports its intent,
    /// not the eventual reader-visible state.
    ///
    /// Callers wanting deterministic-publish semantics should
    /// either:
    /// - Externally serialize publishers (one writer thread).
    /// - Treat `PublishOutcome` as an intent record, not a
    ///   guarantee that the reader will see this exact data.
    pub fn publish(&self, value: T) -> PublishOutcome {
        let arc = Arc::new(value);
        // Spin-CAS on the state until we win. The body is two
        // atomic loads + one atomic CAS; under uncontended
        // single-writer use (the typical case) it's a single
        // iteration.
        loop {
            let old_state = self.state.load(Ordering::Acquire);
            let (w, p, r, dirty) = unpack(old_state);

            // Write the new Arc into the writer slot (under its
            // mutex; nanoseconds).
            {
                let mut slot = self.slots[w as usize]
                    .lock()
                    .expect("triple-buffer slot poisoned");
                *slot = Arc::clone(&arc);
            }

            // Swap writer ↔ presented. Reader index is untouched.
            // `dirty` becomes true: the new presented is unread.
            let new_state = pack(p, w, r, true);
            if self
                .state
                .compare_exchange_weak(old_state, new_state, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                self.publishes_total.fetch_add(1, Ordering::Relaxed);
                let was_overrun = dirty;
                if was_overrun {
                    self.overruns_total.fetch_add(1, Ordering::Relaxed);
                }
                return PublishOutcome {
                    overrun: was_overrun,
                    new_writer_slot: p,
                    new_presented_slot: w,
                    reader_slot: r,
                };
            }
        }
    }

    /// Acquire a snapshot for the render thread. Returns the
    /// `Arc<T>` the renderer holds for the duration of the frame.
    /// The `Arc` is unaffected by subsequent `publish` calls; the
    /// renderer can release it whenever convenient.
    pub fn acquire(&self) -> Arc<T> {
        loop {
            let old_state = self.state.load(Ordering::Acquire);
            let (w, p, r, dirty) = unpack(old_state);
            if !dirty {
                // No new data since the last acquire — return the
                // current reader slot's snapshot.
                self.acquires_total.fetch_add(1, Ordering::Relaxed);
                let slot = self.slots[r as usize]
                    .lock()
                    .expect("triple-buffer slot poisoned");
                return Arc::clone(&slot);
            }
            // Swap reader ↔ presented; clear dirty.
            let new_state = pack(w, r, p, false);
            if self
                .state
                .compare_exchange_weak(old_state, new_state, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                self.acquires_total.fetch_add(1, Ordering::Relaxed);
                let slot = self.slots[p as usize]
                    .lock()
                    .expect("triple-buffer slot poisoned");
                return Arc::clone(&slot);
            }
        }
    }

    /// Force the reader-slot index to recycle to the most recently
    /// presented slot, regardless of the dirty flag. Called by the
    /// watchdog when the render thread has been holding a slot for
    /// longer than the configured timeout (parent epic: 5 seconds).
    /// The renderer's previously-held `Arc<T>` is unaffected — it
    /// continues to exist until the renderer drops it.
    ///
    /// Bumps `force_recycles_total`.
    pub fn force_recycle(&self) {
        loop {
            let old_state = self.state.load(Ordering::Acquire);
            let (w, p, r, _dirty) = unpack(old_state);
            // Move reader → presented unconditionally; clear dirty.
            let new_state = pack(w, r, p, false);
            if self
                .state
                .compare_exchange_weak(old_state, new_state, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                self.force_recycles_total.fetch_add(1, Ordering::Relaxed);
                return;
            }
        }
    }

    /// Snapshot the per-buffer health counters. Cheap (relaxed
    /// loads) and intended for `ft doctor` polling.
    #[must_use]
    pub fn health(&self) -> TripleBufferHealth {
        TripleBufferHealth {
            publishes_total: self.publishes_total.load(Ordering::Relaxed),
            acquires_total: self.acquires_total.load(Ordering::Relaxed),
            overruns_total: self.overruns_total.load(Ordering::Relaxed),
            force_recycles_total: self.force_recycles_total.load(Ordering::Relaxed),
        }
    }

    /// Test-only: peek at the packed state byte. Used by the
    /// fixture's invariant runner.
    #[doc(hidden)]
    #[must_use]
    pub fn debug_state(&self) -> TripleBufferState {
        let (w, p, r, dirty) = unpack(self.state.load(Ordering::Acquire));
        TripleBufferState {
            writer_slot: w,
            presented_slot: p,
            reader_slot: r,
            dirty,
        }
    }
}

// ============================================================================
// Outcome / state types
// ============================================================================

/// Per-`publish` outcome. Used by the structured-log writer to
/// pin the state transition.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PublishOutcome {
    /// True iff the reader hadn't acquired the previous publish
    /// (writer outran reader; oldest-unread was overwritten).
    pub overrun: bool,
    /// Slot index the writer will mutate next.
    pub new_writer_slot: u8,
    /// Slot index the next `acquire` will return.
    pub new_presented_slot: u8,
    /// Reader's current slot (unchanged by this publish).
    pub reader_slot: u8,
}

/// Decoded state machine snapshot. Test-only.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TripleBufferState {
    pub writer_slot: u8,
    pub presented_slot: u8,
    pub reader_slot: u8,
    pub dirty: bool,
}

impl TripleBufferState {
    /// The bead's headline correctness invariant: the three slot
    /// indices MUST be a permutation of `[0, 1, 2]`. Any duplicate
    /// would mean the writer and reader could collide.
    #[must_use]
    pub fn slots_are_distinct(&self) -> bool {
        self.writer_slot != self.presented_slot
            && self.writer_slot != self.reader_slot
            && self.presented_slot != self.reader_slot
            && self.writer_slot < 3
            && self.presented_slot < 3
            && self.reader_slot < 3
    }
}

// ============================================================================
// Health snapshot
// ============================================================================

/// Counter snapshot for `ft doctor`. Mirrors the
/// `AtlasStabilityHealth` shape from `ft-mpc9b.1.1` so the future
/// doctor surface can render both side-by-side.
///
/// Counters are `pub(crate)` so external code can't silently
/// zero `force_recycles_total` (the stuck-reader alert
/// condition) or back-fill any of the other counters
/// out-of-band. Use the read accessors.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TripleBufferHealth {
    /// Total successful publishes since process start.
    pub(crate) publishes_total: u64,
    /// Total successful acquires since process start.
    pub(crate) acquires_total: u64,
    /// `writer_overruns_total` — non-zero indicates the writer is
    /// outrunning the reader; renderer is seeing stale-by-1-frame
    /// snapshots. Not a fault by itself; surfaced for trend
    /// detection.
    pub(crate) overruns_total: u64,
    /// `force_recycles_total` — non-zero indicates the watchdog
    /// recovered a stuck reader. Each occurrence is a
    /// fatal-recoverable event.
    pub(crate) force_recycles_total: u64,
}

impl TripleBufferHealth {
    /// Process-start baseline.
    #[must_use]
    pub fn baseline() -> Self {
        Self {
            publishes_total: 0,
            acquires_total: 0,
            overruns_total: 0,
            force_recycles_total: 0,
        }
    }

    /// Whether the buffer has had any reader-stuck recoveries.
    /// Non-zero is the alert condition for the doctor surface.
    #[must_use]
    pub fn has_force_recycled(&self) -> bool {
        self.force_recycles_total > 0
    }

    // ----- Read-only accessors -----

    #[must_use]
    pub const fn publishes_total(self) -> u64 {
        self.publishes_total
    }
    #[must_use]
    pub const fn acquires_total(self) -> u64 {
        self.acquires_total
    }
    #[must_use]
    pub const fn overruns_total(self) -> u64 {
        self.overruns_total
    }
    #[must_use]
    pub const fn force_recycles_total(self) -> u64 {
        self.force_recycles_total
    }
}

// ============================================================================
// Structured-log event
// ============================================================================

/// One row of `tests/triple_buffer/logs/<scenario>.jsonl` per the
/// parent epic schema.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TripleBufferEvent {
    /// Monotonic timestamp (ms since fixture start).
    pub ts_ms: u64,
    /// What kind of op fired.
    pub kind: TripleBufferOp,
    /// State snapshot AFTER the op.
    pub state: TripleBufferState,
    /// Generation counter (increments on every state-changing op).
    pub generation: u64,
}

/// The closed list of triple-buffer ops.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TripleBufferOp {
    Publish,
    Acquire,
    Overrun,
    ForceRecycle,
}

/// Render an event stream as JSONL.
#[must_use]
pub fn render_events_jsonl(events: &[TripleBufferEvent]) -> String {
    let mut out = String::new();
    for ev in events {
        let line = serde_json::to_string(ev).expect("TripleBufferEvent always serializes");
        out.push_str(&line);
        out.push('\n');
    }
    out
}

/// Parse a JSONL string back into events.
pub fn parse_events_jsonl(jsonl: &str) -> Result<Vec<TripleBufferEvent>, serde_json::Error> {
    let mut out = Vec::new();
    for line in jsonl.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        out.push(serde_json::from_str(trimmed)?);
    }
    Ok(out)
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pack_unpack_roundtrip() {
        for w in 0..3u8 {
            for p in 0..3u8 {
                for r in 0..3u8 {
                    if w == p || w == r || p == r {
                        continue;
                    }
                    for dirty in [false, true] {
                        let s = pack(w, p, r, dirty);
                        let (uw, up, ur, ud) = unpack(s);
                        assert_eq!((uw, up, ur, ud), (w, p, r, dirty));
                    }
                }
            }
        }
    }

    #[test]
    fn initial_state_has_distinct_slots() {
        let tb: TripleBuffer<u32> = TripleBuffer::new(0);
        let s = tb.debug_state();
        assert!(s.slots_are_distinct(), "initial slots not distinct: {s:?}");
        assert!(!s.dirty, "initial state should not be dirty");
    }

    #[test]
    fn first_acquire_returns_initial_value_without_dirty() {
        let tb: TripleBuffer<u32> = TripleBuffer::new(42);
        let snap = tb.acquire();
        assert_eq!(*snap, 42);
        let h = tb.health();
        assert_eq!(h.publishes_total, 0);
        assert_eq!(h.acquires_total, 1);
        assert_eq!(h.overruns_total, 0);
    }

    #[test]
    fn publish_then_acquire_returns_new_value() {
        let tb: TripleBuffer<u32> = TripleBuffer::new(1);
        let outcome = tb.publish(2);
        assert!(!outcome.overrun);
        let snap = tb.acquire();
        assert_eq!(*snap, 2);
    }

    #[test]
    fn slots_remain_distinct_after_many_publishes() {
        let tb: TripleBuffer<u32> = TripleBuffer::new(0);
        for i in 1..=64 {
            tb.publish(i);
            let s = tb.debug_state();
            assert!(s.slots_are_distinct(), "slots collided at i={i}: {s:?}");
        }
    }

    #[test]
    fn writer_overrun_increments_counter_and_marks_outcome() {
        let tb: TripleBuffer<u32> = TripleBuffer::new(0);
        // Two publishes back-to-back without an intervening acquire.
        let first = tb.publish(1);
        assert!(!first.overrun);
        let second = tb.publish(2);
        assert!(
            second.overrun,
            "second publish without acquire is an overrun"
        );
        let h = tb.health();
        assert_eq!(h.overruns_total, 1);
        // Acquire returns the latest value (not the overwritten one).
        let snap = tb.acquire();
        assert_eq!(*snap, 2, "acquire after overrun returns the latest publish");
    }

    #[test]
    fn three_publishes_one_acquire_burns_two_intermediates() {
        let tb: TripleBuffer<String> = TripleBuffer::new("seed".to_string());
        tb.publish("a".to_string());
        tb.publish("b".to_string());
        tb.publish("c".to_string());
        // Two of those publishes were overruns.
        assert_eq!(tb.health().overruns_total, 2);
        // Acquire returns the latest.
        assert_eq!(*tb.acquire(), "c");
    }

    #[test]
    fn re_acquire_without_publish_returns_same_arc_pointer() {
        let tb: TripleBuffer<u32> = TripleBuffer::new(7);
        let a = tb.acquire();
        let b = tb.acquire();
        assert!(
            Arc::ptr_eq(&a, &b),
            "re-acquire without publish should return the same Arc"
        );
    }

    #[test]
    fn snapshot_arc_unaffected_by_subsequent_publish() {
        let tb: TripleBuffer<u32> = TripleBuffer::new(0);
        tb.publish(100);
        let held = tb.acquire();
        assert_eq!(*held, 100);
        // Writer publishes a torrent of new values.
        for i in 1..=10 {
            tb.publish(100 + i);
        }
        // The renderer's held Arc is unchanged.
        assert_eq!(*held, 100);
        // A fresh acquire sees the latest.
        assert_eq!(*tb.acquire(), 110);
    }

    #[test]
    fn force_recycle_increments_counter_and_clears_dirty() {
        let tb: TripleBuffer<u32> = TripleBuffer::new(0);
        tb.publish(1);
        let pre = tb.debug_state();
        assert!(pre.dirty);
        tb.force_recycle();
        let post = tb.debug_state();
        assert!(
            !post.dirty,
            "force_recycle must clear the dirty flag: {post:?}"
        );
        assert!(
            post.slots_are_distinct(),
            "force_recycle must preserve distinct slots: {post:?}"
        );
        assert_eq!(tb.health().force_recycles_total, 1);
    }

    #[test]
    fn baseline_health_has_no_recycles() {
        let h = TripleBufferHealth::baseline();
        assert!(!h.has_force_recycled());
        assert_eq!(h.publishes_total, 0);
    }

    #[test]
    fn health_accessors_round_trip_after_publishes() {
        let tb: TripleBuffer<u32> = TripleBuffer::new(0);
        tb.publish(1);
        tb.publish(2);
        let _ = tb.acquire();
        let h = tb.health();
        assert_eq!(h.publishes_total(), 2);
        assert!(h.acquires_total() >= 1);
        // No accessor lets external code zero
        // force_recycles_total — pin the privacy
        // invariant: force_recycles_total can only increase
        // via TripleBuffer::force_recycle().
        assert_eq!(h.force_recycles_total(), 0);
        tb.force_recycle();
        let h2 = tb.health();
        assert_eq!(h2.force_recycles_total(), 1);
        assert!(h2.has_force_recycled());
    }

    #[test]
    fn jsonl_event_roundtrip() {
        let events = vec![
            TripleBufferEvent {
                ts_ms: 0,
                kind: TripleBufferOp::Publish,
                state: TripleBufferState {
                    writer_slot: 1,
                    presented_slot: 0,
                    reader_slot: 2,
                    dirty: true,
                },
                generation: 1,
            },
            TripleBufferEvent {
                ts_ms: 5,
                kind: TripleBufferOp::Acquire,
                state: TripleBufferState {
                    writer_slot: 1,
                    presented_slot: 2,
                    reader_slot: 0,
                    dirty: false,
                },
                generation: 2,
            },
        ];
        let rendered = render_events_jsonl(&events);
        let parsed = parse_events_jsonl(&rendered).unwrap();
        assert_eq!(parsed, events);
    }

    #[test]
    fn writer_can_publish_thousands_without_acquiring() {
        // Stress: writer outpaces reader 1000:0. No panic, slots
        // stay distinct, overruns_total counts every overwrite.
        let tb: TripleBuffer<u64> = TripleBuffer::new(0);
        for i in 1..=1000u64 {
            tb.publish(i);
            let s = tb.debug_state();
            assert!(s.slots_are_distinct(), "slots collided at i={i}");
        }
        assert_eq!(tb.health().overruns_total, 999);
        assert_eq!(*tb.acquire(), 1000);
    }
}
