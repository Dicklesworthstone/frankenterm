//! Property tests for `recorder_replay::ReplayScheduler` —
//! the deterministic replay kernel (ft-og6q6.3.1).
//!
//! The existing `proptest_recorder_replay.rs` (1032 lines, 15
//! property blocks) covers `ReplaySession` / `ReplayConfig` /
//! `ReplayState` / `ReplayStats` and serde round-trips for the
//! shape types. It does NOT cover the scheduler itself —
//! `ReplayScheduler::next_step`, `run_to_completion`,
//! `checkpoint`/`resume`, `decision_trace_bytes`, and the
//! `VirtualClock` it drives.
//!
//! This file pins the scheduler invariants under randomized
//! event sequences:
//!
//! 1. **Determinism / idempotency on replay**: two schedulers
//!    constructed over the same input + config emit the same
//!    decision trace, byte-identical. This is the user-requested
//!    "idempotency on replay" property.
//! 2. **Event ordering preserved**: emitted steps appear in
//!    `RecorderMergeKey` order regardless of input order. This
//!    is the user-requested "event ordering preserved".
//! 3. **Total events match**: `run_to_completion()` yields
//!    exactly `total_events()` steps when no filter discards.
//! 4. **Cursor-step lockstep**: after N steps from a fresh
//!    scheduler, `cursor() == N` (no filter case).
//! 5. **VirtualClock is monotone**: across emitted steps,
//!    `clock.recorded_at_ms` is non-decreasing.
//! 6. **Decision trace serde round-trip**: each line of
//!    `decision_trace_bytes()` is a valid JSON
//!    `ReplayDecisionRecord` whose fields match the in-memory
//!    decisions vector.
//! 7. **Checkpoint/resume preserves observable state**: take a
//!    checkpoint mid-run; spin up a fresh scheduler over the
//!    same events + config; resume from the checkpoint;
//!    verify the remaining `next_step` outputs match the
//!    original's. This is the user-requested "journal
//!    compaction preserves observable state" applied to the
//!    replay kernel's checkpoint/resume contract.
//!
//! Logs are emitted as structured tracing-json events so a
//! failing case lands a parseable record of the input + observed
//! step sequence.

use std::sync::Once;

use frankenterm_core::event_id::{RecorderMergeKey, StreamKind};
use frankenterm_core::recorder_replay::{ReplayConfig, ReplayDecisionRecord, ReplayScheduler};
use frankenterm_core::recording::{RecorderEvent, RecorderEventCausality, RecorderEventPayload};
use frankenterm_core_replay_types::recorder_metadata::{
    RECORDER_EVENT_SCHEMA_VERSION_V1, RecorderEventSource, RecorderRedactionLevel,
    RecorderSegmentKind, RecorderTextEncoding,
};
use proptest::prelude::*;
use tracing::info;

fn init_test_tracing_json() {
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        let _ = tracing_subscriber::fmt()
            .json()
            .with_target(true)
            .with_test_writer()
            .try_init();
    });
}

/// Construct a minimum-viable EgressOutput RecorderEvent. Keeps
/// the scheduler-relevant fields varied while pinning the
/// payload-specific fields to deterministic defaults — the
/// scheduler doesn't inspect payload contents beyond the merge
/// key + decision-record build.
fn build_event(
    pane_id: u64,
    sequence: u64,
    recorded_at_ms: u64,
    occurred_at_ms: u64,
    event_id_seed: u32,
) -> RecorderEvent {
    RecorderEvent {
        schema_version: RECORDER_EVENT_SCHEMA_VERSION_V1.to_string(),
        event_id: format!("evt-{event_id_seed:012x}"),
        pane_id,
        session_id: None,
        workflow_id: None,
        correlation_id: None,
        source: RecorderEventSource::WeztermMux,
        occurred_at_ms,
        recorded_at_ms,
        sequence,
        causality: RecorderEventCausality {
            parent_event_id: None,
            trigger_event_id: None,
            root_event_id: None,
        },
        payload: RecorderEventPayload::EgressOutput {
            text: "x".to_string(),
            encoding: RecorderTextEncoding::Utf8,
            redaction: RecorderRedactionLevel::None,
            segment_kind: RecorderSegmentKind::Delta,
            is_gap: false,
        },
    }
}

/// Strategy producing an event with sortable but unpredictable
/// merge-key ordering. Each event has independently-sampled
/// pane_id / sequence / recorded_at_ms / event_id_seed so the
/// scheduler's sort step has work to do.
fn arb_event() -> impl Strategy<Value = RecorderEvent> {
    (
        1u64..=4u64,            // pane_id (small range to force collisions on this axis)
        0u64..=64u64,            // sequence (small range to force ties on this axis)
        0u64..=1_000_000u64,     // recorded_at_ms
        0u64..=1_000_000u64,     // occurred_at_ms (independent — clock anomalies allowed)
        any::<u32>(),            // event_id_seed (uniqueness driver)
    )
        .prop_map(|(pane_id, sequence, recorded_at_ms, occurred_at_ms, seed)| {
            build_event(pane_id, sequence, recorded_at_ms, occurred_at_ms, seed)
        })
}

/// Strategy for non-empty event vectors.
fn arb_event_vec() -> impl Strategy<Value = Vec<RecorderEvent>> {
    prop::collection::vec(arb_event(), 1..=24)
}

/// Use the instant config (speed=infinity, max_delay=0) for all
/// proptests so timing slop doesn't enter the picture — the
/// scheduler emits identical decisions either way; the choice
/// just keeps property-test runtime sub-millisecond per case.
fn instant_config() -> ReplayConfig {
    ReplayConfig::instant()
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(48))]

    /// **Property 1 — determinism / idempotency on replay**:
    /// two schedulers over the same events + config emit the
    /// byte-identical decision trace. The kernel is documented
    /// as deterministic; this pins it under randomized inputs.
    #[test]
    fn proptest_recorder_replay_scheduler_decisions_are_deterministic(
        events in arb_event_vec(),
    ) {
        init_test_tracing_json();
        let mut a = ReplayScheduler::new(events.clone(), instant_config())
            .expect("scheduler over non-empty events");
        let mut b = ReplayScheduler::new(events, instant_config())
            .expect("scheduler over the same events");
        let _ = a.run_to_completion();
        let _ = b.run_to_completion();

        let trace_a = a.decision_trace_bytes().expect("trace bytes a");
        let trace_b = b.decision_trace_bytes().expect("trace bytes b");

        info!(
            test = "decisions_are_deterministic",
            event_count = a.total_events(),
            decisions_a = a.decisions().len(),
            decisions_b = b.decisions().len(),
            trace_bytes_a = trace_a.len(),
            "determinism case"
        );

        prop_assert_eq!(trace_a, trace_b,
            "two schedulers over the same input must emit byte-identical decision traces");
        prop_assert_eq!(a.decisions().len(), b.decisions().len());
    }

    /// **Property 2 — event ordering preserved**: emitted steps
    /// are sorted by `RecorderMergeKey` regardless of input
    /// order. The scheduler's `new()` sorts on construction; we
    /// verify the sorted invariant holds end-to-end.
    #[test]
    fn proptest_recorder_replay_scheduler_emits_in_merge_key_order(
        events in arb_event_vec(),
    ) {
        init_test_tracing_json();
        let mut s = ReplayScheduler::new(events.clone(), instant_config())
            .expect("scheduler over non-empty events");
        let steps = s.run_to_completion();

        info!(
            test = "emits_in_merge_key_order",
            event_count = events.len(),
            steps_emitted = steps.len(),
            "ordering case"
        );

        // Reconstruct the merge key from each emitted step's
        // (recorded_at_ms, pane_id, stream_kind, sequence,
        // event_id) fields and verify monotone-non-decreasing.
        for window in steps.windows(2) {
            let prev_key = RecorderMergeKey {
                recorded_at_ms: window[0].merge_recorded_at_ms,
                pane_id: window[0].merge_pane_id,
                stream_kind: window[0].merge_stream_kind,
                sequence: window[0].merge_sequence,
                event_id: window[0].merge_event_id.clone(),
            };
            let next_key = RecorderMergeKey {
                recorded_at_ms: window[1].merge_recorded_at_ms,
                pane_id: window[1].merge_pane_id,
                stream_kind: window[1].merge_stream_kind,
                sequence: window[1].merge_sequence,
                event_id: window[1].merge_event_id.clone(),
            };
            prop_assert!(prev_key < next_key,
                "merge-key order violated: {:?} not < {:?}", prev_key, next_key);
        }
    }

    /// **Property 3 — total events match**: with no filter,
    /// run_to_completion emits exactly total_events() steps.
    #[test]
    fn proptest_recorder_replay_scheduler_run_to_completion_emits_total(
        events in arb_event_vec(),
    ) {
        init_test_tracing_json();
        let mut s = ReplayScheduler::new(events.clone(), instant_config())
            .expect("scheduler");
        let total = s.total_events();
        let steps = s.run_to_completion();
        prop_assert_eq!(steps.len(), total,
            "run_to_completion must emit one step per event when no filter");
        prop_assert_eq!(s.cursor(), total,
            "cursor must equal total_events after run_to_completion");
    }

    /// **Property 4 — cursor-step lockstep**: after N calls to
    /// next_step from a fresh scheduler, cursor() == N (no
    /// filter discards).
    #[test]
    fn proptest_recorder_replay_scheduler_cursor_advances_one_per_step(
        events in arb_event_vec(),
    ) {
        init_test_tracing_json();
        let mut s = ReplayScheduler::new(events, instant_config())
            .expect("scheduler");
        let total = s.total_events();
        for n in 1..=total {
            let step = s.next_step().expect("step within bound");
            prop_assert_eq!(step.cursor + 1, s.cursor());
            prop_assert_eq!(s.cursor(), n,
                "cursor must advance exactly one per next_step call");
        }
        prop_assert!(s.next_step().is_none(),
            "next_step past total must return None");
    }

    /// **Property 5 — VirtualClock is monotone**: across
    /// emitted steps, the clock's recorded_at_ms field is
    /// non-decreasing. (Equality is allowed — multiple events
    /// at the same recorded timestamp tie-break on pane_id /
    /// stream_kind / sequence / event_id but share clock state
    /// at the boundary.)
    #[test]
    fn proptest_recorder_replay_scheduler_virtual_clock_monotone(
        events in arb_event_vec(),
    ) {
        init_test_tracing_json();
        let mut s = ReplayScheduler::new(events, instant_config())
            .expect("scheduler");
        let steps = s.run_to_completion();
        let mut prev_recorded_at: u64 = 0;
        for step in &steps {
            prop_assert!(step.clock.recorded_at_ms >= prev_recorded_at,
                "clock recorded_at_ms must not decrease: prev={prev_recorded_at}, now={}",
                step.clock.recorded_at_ms);
            prop_assert!(step.clock.initialized,
                "clock must be initialized after the first advance");
            prev_recorded_at = step.clock.recorded_at_ms;
        }
    }

    /// **Property 6 — decision trace serde round-trip**: every
    /// line of `decision_trace_bytes` is a valid JSON
    /// `ReplayDecisionRecord` whose fields equal the in-memory
    /// decisions in order.
    #[test]
    fn proptest_recorder_replay_scheduler_decision_trace_round_trips(
        events in arb_event_vec(),
    ) {
        init_test_tracing_json();
        let mut s = ReplayScheduler::new(events, instant_config())
            .expect("scheduler");
        let _ = s.run_to_completion();
        let trace = s.decision_trace_bytes().expect("trace bytes");
        let live_decisions = s.decisions();

        // Decode each newline-delimited JSON line.
        let mut decoded: Vec<ReplayDecisionRecord> = Vec::new();
        for line in trace.split(|b| *b == b'\n') {
            if line.is_empty() {
                continue;
            }
            let rec: ReplayDecisionRecord =
                serde_json::from_slice(line).expect("decision JSON parses");
            decoded.push(rec);
        }

        prop_assert_eq!(decoded.len(), live_decisions.len(),
            "decoded line count must equal in-memory decisions count");
        for (decoded_rec, live_rec) in decoded.iter().zip(live_decisions.iter()) {
            prop_assert_eq!(decoded_rec, live_rec,
                "round-tripped decision must equal in-memory decision");
        }
    }

    /// **Property 7 — checkpoint/resume preserves observable
    /// state**: mid-run, take a checkpoint; spin up a fresh
    /// scheduler; resume from the checkpoint; the remaining
    /// next_step outputs must equal the continuation of the
    /// original. This is the journal-compaction-preserves-state
    /// invariant applied to the replay kernel's checkpoint
    /// surface.
    #[test]
    fn proptest_recorder_replay_scheduler_checkpoint_resume_preserves_state(
        events in arb_event_vec(),
        cut_index in 0usize..=24,
    ) {
        init_test_tracing_json();
        let mut original = ReplayScheduler::new(events.clone(), instant_config())
            .expect("scheduler over events");

        // Run forward `cut_index` steps (saturating at the
        // total event count).
        let n_total = original.total_events();
        let n_cut = cut_index.min(n_total);
        for _ in 0..n_cut {
            let _ = original.next_step();
        }
        let checkpoint = original.checkpoint();

        // Drain remaining steps from the original.
        let original_remaining = original.run_to_completion();

        // Fresh scheduler + resume from the checkpoint.
        let mut resumed = ReplayScheduler::new(events, instant_config())
            .expect("scheduler over events (resume)");
        resumed.resume(checkpoint).expect("resume from checkpoint");
        let resumed_remaining = resumed.run_to_completion();

        info!(
            test = "checkpoint_resume_preserves_state",
            n_total,
            n_cut,
            original_remaining = original_remaining.len(),
            resumed_remaining = resumed_remaining.len(),
            "checkpoint/resume case"
        );

        prop_assert_eq!(original_remaining.len(), resumed_remaining.len(),
            "remaining step count must match between drain-original and resume-from-checkpoint");
        for (orig_step, resumed_step) in original_remaining.iter().zip(resumed_remaining.iter()) {
            prop_assert_eq!(&orig_step.merge_event_id, &resumed_step.merge_event_id,
                "resumed step's event_id must match original's");
            prop_assert_eq!(orig_step.cursor, resumed_step.cursor,
                "resumed step's cursor must match original's");
            prop_assert_eq!(&orig_step.decision, &resumed_step.decision,
                "resumed step's decision record must match original's");
        }
    }
}
