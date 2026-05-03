//! Triple-buffer regression fixture (`ft-d0ol8` / `ft-2okh0.3.1`).
//!
//! Pins the Petersen 2005 three-state-mailbox invariants via
//! property-based testing over arbitrary writer/reader interleavings.
//! The full Loom state-space exploration is the cross-link
//! `BR-RC-FOUNDATION.G8.2` follow-on (`ft-2okh0.3.4`); this fixture
//! is the always-on regression net.
//!
//! ## Goldens
//!
//! `crates/frankenterm-core/tests/triple_buffer/golden/<scenario>.jsonl`.
//! `FT_TRIPLE_BUFFER_BLESS=1` regenerates with the same
//! deliberate-bless flow used by the a11y_tree / color_management /
//! ime_caret / atlas_stability fixtures.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::Duration;

use frankenterm_core::triple_buffer::{
    TripleBuffer, TripleBufferEvent, TripleBufferHealth, TripleBufferOp, TripleBufferState,
    parse_events_jsonl, render_events_jsonl,
};
use proptest::prelude::*;

fn golden_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("triple_buffer")
        .join("golden")
}

fn golden_path(scenario: &str) -> PathBuf {
    golden_dir().join(format!("{scenario}.jsonl"))
}

fn bless_enabled() -> bool {
    std::env::var("FT_TRIPLE_BUFFER_BLESS")
        .map(|v| v == "1")
        .unwrap_or(false)
}

fn ensure_golden_dir_exists() {
    std::fs::create_dir_all(golden_dir()).expect("create golden dir");
}

// ============================================================================
// Test 1 — single-threaded driver: pin state-machine transitions.
// ============================================================================

fn typing_burst_stream() -> Vec<TripleBufferEvent> {
    let tb: TripleBuffer<u32> = TripleBuffer::new(0);
    let mut events = Vec::new();
    let mut generation = 0u64;
    let mut ts = 0u64;
    // Initial acquire — returns the seed.
    let _seed = tb.acquire();
    generation += 1;
    events.push(TripleBufferEvent {
        ts_ms: ts,
        kind: TripleBufferOp::Acquire,
        state: tb.debug_state(),
        generation,
    });
    // Typing burst: writer publishes 3 keystrokes, reader catches up
    // between each.
    for ch in 1..=3u32 {
        ts += 5;
        tb.publish(ch);
        generation += 1;
        events.push(TripleBufferEvent {
            ts_ms: ts,
            kind: TripleBufferOp::Publish,
            state: tb.debug_state(),
            generation,
        });
        ts += 1;
        let _snap = tb.acquire();
        generation += 1;
        events.push(TripleBufferEvent {
            ts_ms: ts,
            kind: TripleBufferOp::Acquire,
            state: tb.debug_state(),
            generation,
        });
    }
    events
}

fn writer_overrun_stream() -> Vec<TripleBufferEvent> {
    let tb: TripleBuffer<u32> = TripleBuffer::new(0);
    let mut events = Vec::new();
    let mut generation = 0u64;
    let mut ts = 0u64;
    // Three publishes back-to-back without an acquire.
    for v in 1..=3u32 {
        let outcome = tb.publish(v);
        generation += 1;
        let kind = if outcome.overrun {
            TripleBufferOp::Overrun
        } else {
            TripleBufferOp::Publish
        };
        events.push(TripleBufferEvent {
            ts_ms: ts,
            kind,
            state: tb.debug_state(),
            generation,
        });
        ts += 1;
    }
    // One acquire collapses to the latest.
    let _snap = tb.acquire();
    generation += 1;
    events.push(TripleBufferEvent {
        ts_ms: ts,
        kind: TripleBufferOp::Acquire,
        state: tb.debug_state(),
        generation,
    });
    events
}

fn force_recycle_stream() -> Vec<TripleBufferEvent> {
    let tb: TripleBuffer<u32> = TripleBuffer::new(0);
    let mut events = Vec::new();
    let mut generation = 0u64;
    let mut ts = 0u64;
    tb.publish(1);
    generation += 1;
    events.push(TripleBufferEvent {
        ts_ms: ts,
        kind: TripleBufferOp::Publish,
        state: tb.debug_state(),
        generation,
    });
    ts += 5;
    // Watchdog fires — reader was stuck.
    tb.force_recycle();
    generation += 1;
    events.push(TripleBufferEvent {
        ts_ms: ts,
        kind: TripleBufferOp::ForceRecycle,
        state: tb.debug_state(),
        generation,
    });
    events
}

#[test]
fn typing_burst_satisfies_invariants() {
    let events = typing_burst_stream();
    for ev in &events {
        assert!(
            ev.state.slots_are_distinct(),
            "slots collided in typing burst: {:?}",
            ev
        );
    }
}

#[test]
fn writer_overrun_stream_satisfies_invariants() {
    let events = writer_overrun_stream();
    let overrun_count = events
        .iter()
        .filter(|e| matches!(e.kind, TripleBufferOp::Overrun))
        .count();
    assert!(
        overrun_count >= 2,
        "expected ≥2 overruns, got {overrun_count}"
    );
    for ev in &events {
        assert!(ev.state.slots_are_distinct());
    }
}

#[test]
fn force_recycle_stream_satisfies_invariants() {
    let events = force_recycle_stream();
    let recycles = events
        .iter()
        .filter(|e| matches!(e.kind, TripleBufferOp::ForceRecycle))
        .count();
    assert_eq!(recycles, 1);
    for ev in &events {
        assert!(ev.state.slots_are_distinct());
    }
}

// ============================================================================
// Test 2 — golden snapshots.
// ============================================================================

#[test]
fn golden_typing_burst() {
    snapshot_golden("typing_burst", &typing_burst_stream());
}

#[test]
fn golden_writer_overrun() {
    snapshot_golden("writer_overrun", &writer_overrun_stream());
}

#[test]
fn golden_force_recycle() {
    snapshot_golden("force_recycle", &force_recycle_stream());
}

fn snapshot_golden(scenario: &str, events: &[TripleBufferEvent]) {
    let rendered = render_events_jsonl(events);
    let path = golden_path(scenario);

    if bless_enabled() {
        ensure_golden_dir_exists();
        std::fs::write(&path, &rendered).expect("write blessed golden");
        panic!(
            "{scenario}: golden blessed at {}; re-run without FT_TRIPLE_BUFFER_BLESS to validate",
            path.display()
        );
    }

    let expected = std::fs::read_to_string(&path).unwrap_or_else(|err| {
        panic!(
            "missing golden for {scenario} at {}: {err} \
             (re-run with FT_TRIPLE_BUFFER_BLESS=1 to generate)",
            path.display()
        )
    });

    assert_eq!(
        rendered,
        expected,
        "{scenario} drifted from golden at {}",
        path.display()
    );

    let parsed = parse_events_jsonl(&rendered).expect("parse");
    assert_eq!(parsed, events, "JSONL roundtrip drift for {scenario}");
}

// ============================================================================
// Test 3 — concurrent writer+reader stress (no Loom; real OS threads).
//
// Pins the headline correctness rule under multi-threaded use:
// arbitrary interleavings of publish + acquire produce slot states
// that always satisfy `slots_are_distinct`, and the reader's held
// Arc never tears under concurrent publish.
// ============================================================================

#[test]
fn concurrent_writer_and_reader_stress() {
    const PUBLISHES: u64 = 5_000;
    const READS: u64 = 5_000;

    let tb: Arc<TripleBuffer<u64>> = Arc::new(TripleBuffer::new(0));
    let max_observed = Arc::new(AtomicU64::new(0));

    let writer = {
        let tb = Arc::clone(&tb);
        thread::spawn(move || {
            for i in 1..=PUBLISHES {
                tb.publish(i);
            }
        })
    };

    let reader = {
        let tb = Arc::clone(&tb);
        let max_observed = Arc::clone(&max_observed);
        thread::spawn(move || {
            for _ in 0..READS {
                let snap = tb.acquire();
                let v = *snap;
                let mut current = max_observed.load(Ordering::Relaxed);
                while v > current {
                    match max_observed.compare_exchange_weak(
                        current,
                        v,
                        Ordering::Relaxed,
                        Ordering::Relaxed,
                    ) {
                        Ok(_) => break,
                        Err(actual) => current = actual,
                    }
                }
                // Verify slot invariant after the acquire; a torn
                // state would surface here.
                let s = tb.debug_state();
                assert!(s.slots_are_distinct(), "torn state during stress: {s:?}");
            }
        })
    };

    writer.join().unwrap();
    reader.join().unwrap();

    // After the writer completes, one final acquire should see the
    // most recent value.
    let final_snap = tb.acquire();
    assert_eq!(
        *final_snap, PUBLISHES,
        "post-stress acquire should see the latest publish"
    );

    // Reader should have observed at least one value during the run
    // (likely many; we assert non-zero to catch a totally-stalled
    // reader).
    assert!(
        max_observed.load(Ordering::Relaxed) > 0,
        "reader never observed a published value"
    );

    // Sanity: counters are consistent.
    let h = tb.health();
    assert_eq!(
        h.publishes_total(),
        PUBLISHES,
        "writer reported {} publishes, expected {PUBLISHES}",
        h.publishes_total()
    );
    // overruns can be up to PUBLISHES depending on scheduling — only
    // verify the counter is well-formed.
    assert!(h.overruns_total() <= PUBLISHES);
}

// ============================================================================
// Test 4 — proptest properties on the state machine.
// ============================================================================

#[derive(Debug, Clone, Copy)]
enum Op {
    Publish,
    Acquire,
    ForceRecycle,
}

prop_compose! {
    fn arb_op()(
        choice in 0u8..3,
    ) -> Op {
        match choice {
            0 => Op::Publish,
            1 => Op::Acquire,
            _ => Op::ForceRecycle,
        }
    }
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 256,
        .. ProptestConfig::default()
    })]

    /// For ANY sequence of ops, the slot indices remain a
    /// permutation of [0, 1, 2]. This is the load-bearing
    /// invariant of the Petersen 2005 pattern.
    #[test]
    fn slot_distinctness_holds_under_arbitrary_op_sequences(
        ops in proptest::collection::vec(arb_op(), 0..64),
    ) {
        let tb: TripleBuffer<u32> = TripleBuffer::new(0);
        for (i, op) in ops.iter().enumerate() {
            match op {
                Op::Publish => { tb.publish(i as u32); }
                Op::Acquire => { tb.acquire(); }
                Op::ForceRecycle => { tb.force_recycle(); }
            }
            let s = tb.debug_state();
            prop_assert!(
                s.slots_are_distinct(),
                "slot collision at step {i}, op={op:?}, state={s:?}"
            );
        }
    }

    /// Held snapshots are immutable. A renderer that holds an Arc
    /// from `acquire` MUST see the same value forever, even if the
    /// writer floods publishes.
    #[test]
    fn held_snapshot_is_immutable(
        floods in 0u32..32,
    ) {
        let tb: TripleBuffer<u32> = TripleBuffer::new(7);
        tb.publish(100);
        let held = tb.acquire();
        let initial = *held;
        for j in 0..floods {
            tb.publish(200 + j);
        }
        prop_assert_eq!(*held, initial);
    }

    /// Counters are monotonic — they never decrease across an op.
    #[test]
    fn counters_monotonic(
        ops in proptest::collection::vec(arb_op(), 0..32),
    ) {
        let tb: TripleBuffer<u32> = TripleBuffer::new(0);
        let mut prior = TripleBufferHealth::baseline();
        for op in ops {
            match op {
                Op::Publish => { tb.publish(0); }
                Op::Acquire => { tb.acquire(); }
                Op::ForceRecycle => { tb.force_recycle(); }
            }
            let now = tb.health();
            prop_assert!(now.publishes_total() >= prior.publishes_total());
            prop_assert!(now.acquires_total() >= prior.acquires_total());
            prop_assert!(now.overruns_total() >= prior.overruns_total());
            prop_assert!(now.force_recycles_total() >= prior.force_recycles_total());
            prop_assert!(now.overruns_total() <= now.publishes_total());
            prior = now;
        }
    }

    /// JSONL render/parse identity.
    #[test]
    fn jsonl_roundtrip(
        ops in proptest::collection::vec(arb_op(), 0..16),
    ) {
        let tb: TripleBuffer<u32> = TripleBuffer::new(0);
        let mut events = Vec::new();
        let mut generation = 0u64;
        for (i, op) in ops.iter().enumerate() {
            let kind = match op {
                Op::Publish => {
                    let outcome = tb.publish(i as u32);
                    if outcome.overrun { TripleBufferOp::Overrun } else { TripleBufferOp::Publish }
                }
                Op::Acquire => { tb.acquire(); TripleBufferOp::Acquire }
                Op::ForceRecycle => { tb.force_recycle(); TripleBufferOp::ForceRecycle }
            };
            generation += 1;
            events.push(TripleBufferEvent {
                ts_ms: i as u64,
                kind,
                state: tb.debug_state(),
                generation,
            });
        }
        let rendered = render_events_jsonl(&events);
        let parsed = parse_events_jsonl(&rendered).expect("parse");
        prop_assert_eq!(parsed, events);
    }
}

// ============================================================================
// Test 5 — small writer-1000:reader-1 ratio stress.
//
// The bead's "writer outpaces reader 1000:1" stress case. Sleeps
// briefly between reads so the writer always runs ahead.
// ============================================================================

#[test]
fn writer_outpaces_reader_thousand_to_one_no_panic() {
    const PUBLISHES: u64 = 1_000;
    let tb: Arc<TripleBuffer<u64>> = Arc::new(TripleBuffer::new(0));

    let writer = {
        let tb = Arc::clone(&tb);
        thread::spawn(move || {
            for i in 1..=PUBLISHES {
                tb.publish(i);
            }
        })
    };

    let reader = {
        let tb = Arc::clone(&tb);
        thread::spawn(move || {
            // One read total — simulating a render thread that's
            // way behind. The writer should not panic; counters
            // should reflect a high overrun count.
            thread::sleep(Duration::from_millis(5));
            let snap = tb.acquire();
            assert!(*snap > 0, "reader saw no value");
        })
    };

    writer.join().unwrap();
    reader.join().unwrap();

    let h = tb.health();
    assert_eq!(h.publishes_total(), PUBLISHES);
    // We can't assert an exact lower bound on overruns (depends on
    // scheduling), but the run should not have panicked and the
    // final state must be consistent.
    let s = tb.debug_state();
    assert!(s.slots_are_distinct(), "post-stress state is torn: {s:?}");
}

// ============================================================================
// Test 6 — TripleBufferState slots_are_distinct edge cases.
// ============================================================================

#[test]
fn slots_are_distinct_rejects_duplicates() {
    let bad = TripleBufferState {
        writer_slot: 1,
        presented_slot: 1,
        reader_slot: 0,
        dirty: false,
    };
    assert!(!bad.slots_are_distinct());
}

#[test]
fn slots_are_distinct_rejects_out_of_range() {
    let bad = TripleBufferState {
        writer_slot: 5,
        presented_slot: 0,
        reader_slot: 1,
        dirty: false,
    };
    assert!(!bad.slots_are_distinct());
}
