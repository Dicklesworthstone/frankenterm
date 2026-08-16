#![forbid(unsafe_code)]

//! Independent public-API model and planted-negative gauntlet for the bounded
//! interaction flight recorder. This integration test intentionally cannot
//! reach recorder-private queues, counters, or admission helpers.

use std::io::{self, Write};

use frankenterm_core_audit_types::interaction_flight_recorder_v1::{
    PlatformMarkerAccountingV1, PlatformMarkerAuthorityV1, RECORDER_EPOCH_MANIFEST_SCHEMA_VERSION,
    RecorderAccountingAuthority, RecorderCertificationClass, RecorderCertificationVerdict,
    RecorderEpochCloseReason, RecorderEpochId, RecorderEpochManifestV1, RecorderEpochStartReason,
    RecorderEventAccountingV1, RecorderExportStatusV1, RecorderLifecycleState, RecorderMode,
    RecorderSamplerAlgorithm, RecorderSamplerConfigV1, RecorderShutdownStatusV1,
    RecorderTraceAccountingV1, SAMPLED_TRACE_CONTEXT_SCHEMA_VERSION, SampledTraceContextV1,
};
use frankenterm_core_audit_types::interaction_trace_v2::{
    INTERACTION_TRACE_V2_SCHEMA_VERSION, InteractionTraceClockDomain, InteractionTraceCorrelation,
    InteractionTraceCounterUnavailability, InteractionTraceCounters, InteractionTraceDecodeError,
    InteractionTraceGenerations, InteractionTraceIdAllocator, InteractionTraceObservationBoundary,
    InteractionTracePath, InteractionTraceProducer, InteractionTraceRunId, InteractionTraceStage,
    InteractionTraceStageOutcome, InteractionTraceTimestamp, InteractionTraceTopology,
    InteractionTraceV2, MAX_INTERACTION_TRACE_JSON_BYTES, TraceContractError,
};
use frankenterm_flight_recorder::platform_markers::{
    MarkerUnavailableReason, PlatformMarkerEmitter, PlatformMarkerFinishOutcome,
    PlatformMarkerOutcome, UnsupportedPlatformMarkerAdapter,
};
use frankenterm_flight_recorder::{
    ClockStamp, CloseOutcome, EventFields, ExportOutcome, ExportWriteOutcome, FlightRecorder,
    FrozenBatch, RecordOutcome, RecorderConfig, TraceAdmission, TraceToken,
};
use proptest::prelude::*;

const TEST_BYTE_CEILING: u64 = 32 * 1024 * 1024;

fn epoch(nonce: u64) -> RecorderEpochId {
    RecorderEpochId::new(nonce, nonce.rotate_left(17)).expect("test epoch is nonzero")
}

fn run(nonce: u64) -> InteractionTraceRunId {
    InteractionTraceRunId::new(nonce, nonce.rotate_left(29)).expect("test run is nonzero")
}

fn stage(path: InteractionTracePath, ordinal: u8) -> InteractionTraceStage {
    InteractionTraceStage::from_ordinal(path, ordinal).expect("test stage ordinal is valid")
}

fn fields_for(path: InteractionTracePath, ordinal: u8, thread_id: u64) -> EventFields {
    fields_for_with_completed_clock_id(path, ordinal, thread_id, 33)
}

fn fields_for_with_completed_clock_id(
    path: InteractionTracePath,
    ordinal: u8,
    thread_id: u64,
    completed_clock_id: u64,
) -> EventFields {
    let stage = stage(path, ordinal);
    let clock_domain = InteractionTraceClockDomain {
        host_id: 11,
        process_generation: 22,
        clock_id: 33,
    };
    let display_completion = stage.is_display_completion();
    EventFields::new(
        u64::from(ordinal),
        u64::from(ordinal) + 1,
        (ordinal > 0).then_some(u64::from(ordinal)),
        stage,
        InteractionTraceStageOutcome::Performed,
        InteractionTraceProducer {
            host_id: 11,
            process_id: 44,
            process_generation: 22,
            thread_id,
            connection_generation: stage.requires_connection_generation().then_some(55),
        },
        InteractionTraceTopology {
            window_id: 66,
            tab_id: 77,
            pane_id: 88,
        },
        ClockStamp {
            started_at: InteractionTraceTimestamp {
                clock_domain,
                monotonic_ns: 100 + u64::from(ordinal),
                wall_time_unix_ns: None,
            },
            completed_at: InteractionTraceTimestamp {
                clock_domain: InteractionTraceClockDomain {
                    clock_id: completed_clock_id,
                    ..clock_domain
                },
                monotonic_ns: 101 + u64::from(ordinal),
                wall_time_unix_ns: None,
            },
        },
        InteractionTraceCorrelation::Uncorrelated,
        InteractionTraceCounters::default(),
        InteractionTraceCounterUnavailability::all_available(),
        InteractionTraceGenerations {
            terminal_generation: Some(1),
            snapshot_generation: Some(2),
            frame_generation: display_completion.then_some(3),
        },
        if display_completion {
            InteractionTraceObservationBoundary::DisplayPresented
        } else {
            InteractionTraceObservationBoundary::InternalState
        },
        None,
    )
    .expect("gauntlet event satisfies intrinsic invariants")
}

fn opposite_path(path: InteractionTracePath) -> InteractionTracePath {
    match path {
        InteractionTracePath::Keypress => InteractionTracePath::ResizeZoom,
        InteractionTracePath::ResizeZoom => InteractionTracePath::Keypress,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ModelLifecycle {
    Active,
    Closing,
    Closed,
}

#[derive(Debug)]
struct RecorderModel {
    lifecycle: ModelLifecycle,
    shard_capacities: Vec<usize>,
    shard_lengths: Vec<usize>,
    sampled_in: u64,
    recorded: u64,
    queue_full: u64,
    clock_invalid: u64,
    epoch_mismatch: u64,
}

impl RecorderModel {
    fn new(shard_count: u16, total_slots: u32) -> Self {
        let base = total_slots / u32::from(shard_count);
        let remainder = total_slots % u32::from(shard_count);
        let shard_capacities = (0..u32::from(shard_count))
            .map(|index| {
                usize::try_from(base + u32::from(u8::from(index < remainder)))
                    .expect("small generated capacity fits usize")
            })
            .collect::<Vec<_>>();
        Self {
            lifecycle: ModelLifecycle::Active,
            shard_lengths: vec![0; usize::from(shard_count)],
            shard_capacities,
            sampled_in: 0,
            recorded: 0,
            queue_full: 0,
            clock_invalid: 0,
            epoch_mismatch: 0,
        }
    }

    fn queued_events(&self) -> usize {
        self.shard_lengths.iter().sum()
    }
}

proptest! {
    // This integration target has no sibling lib.rs/main.rs path for
    // SourceParallel persistence. Disable file persistence explicitly; a
    // failing seed is still printed and can be promoted into a planted case.
    #![proptest_config(ProptestConfig {
        cases: 64,
        failure_persistence: None,
        ..ProptestConfig::default()
    })]

    #[test]
    fn public_state_machine_matches_independent_accounting_model(
        shard_count in 1_u16..=4,
        extra_slots in 0_u32..=24,
        actions in prop::collection::vec((any::<u8>(), any::<u8>(), any::<u8>()), 1..96),
    ) {
        let total_slots = u32::from(shard_count) + extra_slots;
        let config = RecorderConfig::new(
            epoch(1),
            run(2),
            RecorderMode::Certification,
            RecorderSamplerConfigV1::certification(),
            shard_count,
            total_slots,
            TEST_BYTE_CEILING,
        ).expect("generated model config is valid");
        let recorder = FlightRecorder::new(config).expect("generated recorder allocates");
        let producers = (0..usize::from(shard_count))
            .map(|shard_index| recorder.register_producer(shard_index).expect("model producer registers"))
            .collect::<Vec<_>>();
        let mut tokens: Vec<TraceToken> = Vec::new();
        let mut model = RecorderModel::new(shard_count, total_slots);

        for (kind, producer_seed, token_seed) in actions {
            let producer_index = usize::from(producer_seed) % producers.len();
            let producer = &producers[producer_index];
            match kind % 6 {
                0 => {
                    let path = if token_seed & 1 == 0 {
                        InteractionTracePath::Keypress
                    } else {
                        InteractionTracePath::ResizeZoom
                    };
                    match model.lifecycle {
                        ModelLifecycle::Active => match recorder.admit_local_trace(producer, path) {
                            TraceAdmission::Admitted { token, accounting_authority: RecorderAccountingAuthority::Exact } => {
                                model.sampled_in += 1;
                                tokens.push(token);
                            }
                            other => prop_assert!(false, "active full-sampling admission diverged: {other:?}"),
                        },
                        ModelLifecycle::Closing | ModelLifecycle::Closed => {
                            prop_assert_eq!(recorder.admit_local_trace(producer, path), TraceAdmission::Closing);
                        }
                    }
                }
                1 | 2 | 3 if !tokens.is_empty() => {
                    let token = tokens[usize::from(token_seed) % tokens.len()];
                    let fields = match kind % 6 {
                        2 => fields_for_with_completed_clock_id(
                            token.path(),
                            0,
                            u64::from(producer_seed) + 1,
                            34,
                        ),
                        3 => fields_for(
                            opposite_path(token.path()),
                            0,
                            u64::from(producer_seed) + 1,
                        ),
                        _ => fields_for(token.path(), 0, u64::from(producer_seed) + 1),
                    };
                    let outcome = recorder.record(producer, token, &fields);
                    match model.lifecycle {
                        ModelLifecycle::Closing | ModelLifecycle::Closed => {
                            prop_assert_eq!(outcome, RecordOutcome::OutsideEpoch);
                        }
                        ModelLifecycle::Active if kind % 6 == 2 => {
                            model.clock_invalid += 1;
                            prop_assert_eq!(
                                outcome,
                                RecordOutcome::ClockInvalid {
                                    accounting_authority: RecorderAccountingAuthority::Exact,
                                }
                            );
                        }
                        ModelLifecycle::Active if kind % 6 == 3 => {
                            model.epoch_mismatch += 1;
                            prop_assert_eq!(
                                outcome,
                                RecordOutcome::EpochMismatch {
                                    accounting_authority: RecorderAccountingAuthority::Exact,
                                }
                            );
                        }
                        ModelLifecycle::Active => {
                            if model.shard_lengths[producer_index] < model.shard_capacities[producer_index] {
                                model.shard_lengths[producer_index] += 1;
                                model.recorded += 1;
                                prop_assert_eq!(
                                    outcome,
                                    RecordOutcome::Recorded {
                                        accounting_authority: RecorderAccountingAuthority::Exact,
                                    }
                                );
                            } else {
                                model.queue_full += 1;
                                prop_assert_eq!(
                                    outcome,
                                    RecordOutcome::QueueFull {
                                        accounting_authority: RecorderAccountingAuthority::Exact,
                                    }
                                );
                            }
                        }
                    }
                }
                4 => {
                    let outcome = recorder.begin_close();
                    match model.lifecycle {
                        ModelLifecycle::Active => {
                            prop_assert_eq!(outcome, CloseOutcome::Ready);
                            model.lifecycle = ModelLifecycle::Closing;
                        }
                        ModelLifecycle::Closing => prop_assert_eq!(outcome, CloseOutcome::Ready),
                        ModelLifecycle::Closed => prop_assert_eq!(outcome, CloseOutcome::AlreadyClosed),
                    }
                }
                5 => {
                    match model.lifecycle {
                        ModelLifecycle::Active | ModelLifecycle::Closing => {
                            let batch = recorder.try_freeze().expect("quiescent model freezes");
                            prop_assert_eq!(batch.len(), model.queued_events());
                            prop_assert_eq!(batch.accounting().trace.sampled_in, model.sampled_in);
                            prop_assert_eq!(batch.accounting().event.recorded, model.recorded);
                            prop_assert_eq!(batch.accounting().event.queue_full, model.queue_full);
                            prop_assert_eq!(batch.accounting().event.clock_invalid, model.clock_invalid);
                            prop_assert_eq!(batch.accounting().event.epoch_mismatch, model.epoch_mismatch);
                            model.shard_lengths.fill(0);
                            model.lifecycle = ModelLifecycle::Closed;
                        }
                        ModelLifecycle::Closed => {
                            prop_assert!(matches!(recorder.try_freeze(), Err(CloseOutcome::AlreadyClosed)));
                        }
                    }
                }
                _ => {}
            }

            let snapshot = recorder.accounting_snapshot();
            prop_assert_eq!(snapshot.authority, RecorderAccountingAuthority::Exact);
            prop_assert_eq!(snapshot.trace.sampled_in, model.sampled_in);
            prop_assert_eq!(snapshot.trace.sampled_out, 0);
            prop_assert_eq!(snapshot.event.recorded, model.recorded);
            prop_assert_eq!(snapshot.event.queue_full, model.queue_full);
            prop_assert_eq!(snapshot.event.clock_invalid, model.clock_invalid);
            prop_assert_eq!(snapshot.event.epoch_mismatch, model.epoch_mismatch);
            prop_assert_eq!(recorder.queued_events(), model.queued_events());
            let expected_lifecycle = match model.lifecycle {
                ModelLifecycle::Active => RecorderLifecycleState::Active,
                ModelLifecycle::Closing => RecorderLifecycleState::Closing,
                ModelLifecycle::Closed => RecorderLifecycleState::Closed,
            };
            prop_assert_eq!(recorder.lifecycle_state(), expected_lifecycle);
        }
    }

    #[test]
    fn whole_trace_sampling_matches_the_frozen_sampler_model(
        numerator in 0_u64..=64,
        denominator in 1_u64..=64,
        attempts in 1_u16..=128,
        seed_hi in any::<u64>(),
        seed_lo in any::<u64>(),
    ) {
        prop_assume!(numerator <= denominator);
        let sampler = RecorderSamplerConfigV1 {
            algorithm: RecorderSamplerAlgorithm::SplitMix64V1,
            numerator,
            denominator,
            seed_hi,
            seed_lo,
        };
        let local_run = run(9);
        let config = RecorderConfig::new(
            epoch(8),
            local_run,
            RecorderMode::Low,
            sampler,
            1,
            1,
            TEST_BYTE_CEILING,
        ).expect("generated sampling config is valid");
        let recorder = FlightRecorder::new(config).expect("sampling recorder allocates");
        let producer = recorder.register_producer(0).expect("sampling producer registers");
        let mut sampled_in = 0_u64;
        let mut sampled_out = 0_u64;
        for sequence in 1..=u64::from(attempts) {
            let trace_id = frankenterm_core_audit_types::interaction_trace_v2::InteractionTraceId::new(
                local_run,
                sequence,
            ).expect("generated trace id is valid");
            let expected = sampler.samples(trace_id).expect("generated sampler evaluates");
            match (expected, recorder.admit_local_trace(&producer, InteractionTracePath::Keypress)) {
                (true, TraceAdmission::Admitted { token, .. }) => {
                    sampled_in += 1;
                    prop_assert_eq!(token.trace_id(), trace_id);
                }
                (false, TraceAdmission::SampledOut { .. }) => sampled_out += 1,
                (_, other) => prop_assert!(false, "sampler model diverged: {other:?}"),
            }
        }
        let accounting = recorder.accounting_snapshot();
        prop_assert_eq!(accounting.trace.sampled_in, sampled_in);
        prop_assert_eq!(accounting.trace.sampled_out, sampled_out);
        prop_assert_eq!(accounting.trace.checked_enabled_trace_attempts(), Ok(u64::from(attempts)));
        prop_assert_eq!(accounting.event.checked_sampled_event_attempts(), Ok(0));
    }
}

fn complete_keypress_trace() -> InteractionTraceV2 {
    let (frozen, token) = complete_keypress_batch();
    let stage_count = InteractionTraceStage::stage_count(InteractionTracePath::Keypress);
    let mut events = Vec::with_capacity(usize::from(stage_count));
    assert_eq!(
        frozen.export_into(&mut events),
        ExportOutcome::Completed {
            exported_events: usize::from(stage_count),
        }
    );
    InteractionTraceV2 {
        schema_version: INTERACTION_TRACE_V2_SCHEMA_VERSION.to_owned(),
        trace_id: token.trace_id(),
        path: token.path(),
        events,
    }
}

fn complete_keypress_batch() -> (FrozenBatch, TraceToken) {
    let stage_count = InteractionTraceStage::stage_count(InteractionTracePath::Keypress);
    let config = RecorderConfig::new(
        epoch(21),
        run(22),
        RecorderMode::Certification,
        RecorderSamplerConfigV1::certification(),
        1,
        u32::from(stage_count),
        TEST_BYTE_CEILING,
    )
    .expect("complete trace config is valid");
    let recorder = FlightRecorder::new(config).expect("complete trace recorder allocates");
    let producer = recorder
        .register_producer(0)
        .expect("complete trace producer registers");
    let token = match recorder.admit_local_trace(&producer, InteractionTracePath::Keypress) {
        TraceAdmission::Admitted { token, .. } => token,
        other => panic!("complete trace admission failed: {other:?}"),
    };
    for ordinal in 0..stage_count {
        assert!(matches!(
            recorder.record(
                &producer,
                token,
                &fields_for(InteractionTracePath::Keypress, ordinal, 1),
            ),
            RecordOutcome::Recorded { .. }
        ));
    }
    (
        recorder.try_freeze().expect("complete trace freezes"),
        token,
    )
}

#[derive(Debug)]
struct FailAfterWriter {
    limit: usize,
    written: Vec<u8>,
}

#[derive(Debug)]
struct PanicWriter;

impl Write for PanicWriter {
    fn write(&mut self, _buffer: &[u8]) -> io::Result<usize> {
        panic!("planted writer panic")
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl FailAfterWriter {
    fn new(limit: usize) -> Self {
        Self {
            limit,
            written: Vec::new(),
        }
    }
}

impl Write for FailAfterWriter {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        let remaining = self.limit.saturating_sub(self.written.len());
        if remaining == 0 {
            return Err(io::Error::other("planted byte-boundary failure"));
        }
        let accepted = remaining.min(buffer.len());
        self.written.extend_from_slice(&buffer[..accepted]);
        Ok(accepted)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[test]
fn public_export_retries_after_every_planted_byte_boundary() {
    let (mut frozen, _) = complete_keypress_batch();
    let mut canonical = Vec::new();
    assert!(matches!(
        frozen.write_json_lines(&mut canonical),
        ExportWriteOutcome::Completed { .. }
    ));
    assert_ne!(canonical.len(), 0);

    for limit in 0..canonical.len() {
        let mut failing = FailAfterWriter::new(limit);
        let ExportWriteOutcome::WriterFailed { exported_bytes, .. } =
            frozen.write_json_lines(&mut failing)
        else {
            panic!("planted writer boundary {limit} did not fail");
        };
        assert_eq!(
            exported_bytes,
            u64::try_from(limit).expect("fixture length fits u64")
        );
        assert_eq!(failing.written, canonical[..limit]);

        let mut retry = Vec::new();
        assert!(matches!(
            frozen.write_json_lines(&mut retry),
            ExportWriteOutcome::Completed { .. }
        ));
        assert_eq!(retry, canonical);
    }
}

#[test]
fn public_export_retains_retry_authority_after_writer_panic() {
    let (mut frozen, _) = complete_keypress_batch();
    let (mut expected_batch, _) = complete_keypress_batch();
    let mut expected = Vec::new();
    assert!(matches!(
        expected_batch.write_json_lines(&mut expected),
        ExportWriteOutcome::Completed { .. }
    ));
    let mut panicking_writer = PanicWriter;
    let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        frozen.write_json_lines(&mut panicking_writer)
    }));
    assert!(panicked.is_err());

    let mut retry = Vec::new();
    assert!(matches!(
        frozen.write_json_lines(&mut retry),
        ExportWriteOutcome::Completed { .. }
    ));
    assert_eq!(retry, expected);
}

#[test]
fn dropped_producer_releases_only_its_shard_claim() {
    let config = RecorderConfig::new(
        epoch(78),
        run(79),
        RecorderMode::Certification,
        RecorderSamplerConfigV1::certification(),
        2,
        2,
        TEST_BYTE_CEILING,
    )
    .expect("two-shard config is valid");
    let recorder = FlightRecorder::new(config).expect("two-shard recorder allocates");
    let first = recorder
        .register_producer(0)
        .expect("first shard claim succeeds");
    let second = recorder
        .register_producer(1)
        .expect("second shard claim succeeds");
    assert!(recorder.register_producer(0).is_err());
    assert!(recorder.register_producer(1).is_err());

    drop(first);
    let replacement = recorder
        .register_producer(0)
        .expect("dropping first handle releases its exact claim");
    assert!(recorder.register_producer(1).is_err());
    drop(replacement);
    drop(second);
    assert!(recorder.register_producer(0).is_ok());
    assert!(recorder.register_producer(1).is_ok());
}

#[test]
fn public_close_race_never_admits_or_accounts_after_the_seal() {
    use std::sync::{Arc, Barrier};

    for round in 0_u64..64 {
        let config = RecorderConfig::new(
            epoch(80 + round),
            run(200 + round),
            RecorderMode::Certification,
            RecorderSamplerConfigV1::certification(),
            1,
            1,
            TEST_BYTE_CEILING,
        )
        .expect("race config is valid");
        let recorder = FlightRecorder::new(config).expect("race recorder allocates");
        let release = Arc::new(Barrier::new(2));
        let worker_recorder = Arc::clone(&recorder);
        let worker_release = Arc::clone(&release);
        let worker = std::thread::spawn(move || {
            let producer = worker_recorder
                .register_producer(0)
                .expect("race producer registers before release");
            worker_release.wait();
            match worker_recorder.admit_local_trace(&producer, InteractionTracePath::Keypress) {
                TraceAdmission::Admitted { token, .. } => Some(worker_recorder.record(
                    &producer,
                    token,
                    &fields_for(InteractionTracePath::Keypress, 0, round + 1),
                )),
                TraceAdmission::Closing => None,
                other => panic!("unexpected close-race admission: {other:?}"),
            }
        });
        release.wait();
        assert!(matches!(
            recorder.begin_close(),
            CloseOutcome::Ready | CloseOutcome::Draining { .. }
        ));
        let worker_outcome = worker.join().expect("close-race worker does not panic");
        assert!(matches!(
            worker_outcome,
            None | Some(
                RecordOutcome::Recorded { .. }
                    | RecordOutcome::Closing { .. }
                    | RecordOutcome::OutsideEpoch
            )
        ));

        let frozen = recorder.try_freeze().expect("settled close race freezes");
        let accounting = frozen.accounting();
        assert_eq!(
            accounting.trace.sampled_in,
            u64::from(u8::from(worker_outcome.is_some()))
        );
        assert_eq!(
            accounting.event.recorded,
            u64::from(u8::from(matches!(
                worker_outcome,
                Some(RecordOutcome::Recorded { .. })
            )))
        );
        assert_eq!(
            accounting.event.closing,
            u64::from(u8::from(matches!(
                worker_outcome,
                Some(RecordOutcome::Closing { .. })
            )))
        );
        assert_eq!(accounting.event.clock_invalid, 0);
        assert_eq!(accounting.event.epoch_mismatch, 0);
        assert_eq!(accounting.event.queue_full, 0);
        assert_eq!(
            frozen.len(),
            usize::try_from(accounting.event.recorded).expect("at most one event was recorded")
        );
        assert_eq!(recorder.lifecycle_state(), RecorderLifecycleState::Closed);
    }
}

#[test]
fn public_trace_id_allocator_exhaustion_is_sticky_and_nonwrapping() {
    let mut allocator = InteractionTraceIdAllocator::resume(run(71), u64::MAX - 2)
        .expect("final two usable trace sequences resume");
    assert_eq!(
        allocator
            .allocate()
            .expect("penultimate usable trace id allocates")
            .sequence,
        u64::MAX - 2
    );
    assert_eq!(
        allocator
            .allocate()
            .expect("last usable trace id allocates")
            .sequence,
        u64::MAX - 1
    );
    assert_eq!(
        allocator.allocate(),
        Err(TraceContractError::TraceSequenceExhausted)
    );
    assert!(allocator.is_exhausted());
    assert_eq!(
        allocator.allocate(),
        Err(TraceContractError::TraceSequenceExhausted)
    );
}

#[test]
fn full_queue_never_overwrites_the_retained_event() {
    let config = RecorderConfig::new(
        epoch(72),
        run(73),
        RecorderMode::Certification,
        RecorderSamplerConfigV1::certification(),
        1,
        1,
        TEST_BYTE_CEILING,
    )
    .expect("one-slot recorder config is valid");
    let recorder = FlightRecorder::new(config).expect("one-slot recorder allocates");
    let producer = recorder
        .register_producer(0)
        .expect("one-slot producer registers");
    let token = match recorder.admit_local_trace(&producer, InteractionTracePath::Keypress) {
        TraceAdmission::Admitted { token, .. } => token,
        other => panic!("one-slot trace admission failed: {other:?}"),
    };
    assert!(matches!(
        recorder.record(
            &producer,
            token,
            &fields_for(InteractionTracePath::Keypress, 0, 1),
        ),
        RecordOutcome::Recorded { .. }
    ));
    assert!(matches!(
        recorder.record(
            &producer,
            token,
            &fields_for(InteractionTracePath::Keypress, 1, 1),
        ),
        RecordOutcome::QueueFull { .. }
    ));

    let frozen = recorder.try_freeze().expect("one-slot recorder freezes");
    assert_eq!(frozen.len(), 1);
    assert_eq!(frozen.accounting().event.recorded, 1);
    assert_eq!(frozen.accounting().event.queue_full, 1);
    let mut exported = Vec::with_capacity(1);
    assert_eq!(
        frozen.export_into(&mut exported),
        ExportOutcome::Completed { exported_events: 1 }
    );
    assert_eq!(exported[0].stage, stage(InteractionTracePath::Keypress, 0));
    assert_eq!(exported[0].sampling_loss.dropped_events, 0);
    assert_eq!(exported[0].sampling_loss.overwritten_events, 0);
}

#[test]
fn remote_origin_is_preserved_and_cross_recorder_authority_fails_closed() {
    let origin_config = RecorderConfig::new(
        epoch(74),
        run(75),
        RecorderMode::Certification,
        RecorderSamplerConfigV1::certification(),
        1,
        1,
        TEST_BYTE_CEILING,
    )
    .expect("origin config is valid");
    let origin = FlightRecorder::new(origin_config).expect("origin recorder allocates");
    let origin_producer = origin
        .register_producer(0)
        .expect("origin producer registers");
    let origin_token =
        match origin.admit_local_trace(&origin_producer, InteractionTracePath::Keypress) {
            TraceAdmission::Admitted { token, .. } => token,
            other => panic!("origin admission failed: {other:?}"),
        };

    let receiver_config = RecorderConfig::new(
        epoch(76),
        run(77),
        RecorderMode::Certification,
        RecorderSamplerConfigV1::certification(),
        1,
        2,
        TEST_BYTE_CEILING,
    )
    .expect("receiver config is valid");
    let receiver = FlightRecorder::new(receiver_config).expect("receiver recorder allocates");
    let receiver_producer = receiver
        .register_producer(0)
        .expect("receiver producer registers");

    assert_eq!(
        receiver.admit_local_trace(&origin_producer, InteractionTracePath::Keypress),
        TraceAdmission::WrongRecorder
    );
    assert_eq!(
        receiver.record(
            &origin_producer,
            origin_token,
            &fields_for(InteractionTracePath::Keypress, 0, 1),
        ),
        RecordOutcome::WrongRecorder
    );

    let remote_token =
        match receiver.admit_remote_trace(&receiver_producer, origin_token.sampled_context()) {
            TraceAdmission::Admitted { token, .. } => token,
            other => panic!("valid remote context was rejected: {other:?}"),
        };
    assert_eq!(remote_token.trace_id(), origin_token.trace_id());
    assert_eq!(
        remote_token.sampled_context().origin_recorder_epoch_id,
        origin_token.local_epoch_id()
    );
    assert_eq!(remote_token.local_epoch_id(), receiver_config.epoch_id());
    assert!(matches!(
        receiver.record(
            &receiver_producer,
            remote_token,
            &fields_for(InteractionTracePath::Keypress, 0, 2),
        ),
        RecordOutcome::Recorded { .. }
    ));

    let invalid_context = SampledTraceContextV1 {
        schema_version: SAMPLED_TRACE_CONTEXT_SCHEMA_VERSION.wrapping_add(1),
        ..origin_token.sampled_context()
    };
    assert_eq!(
        receiver.admit_remote_trace(&receiver_producer, invalid_context),
        TraceAdmission::InvalidRemoteContext
    );
    assert!(matches!(
        receiver.record(
            &receiver_producer,
            origin_token,
            &fields_for(InteractionTracePath::Keypress, 0, 2),
        ),
        RecordOutcome::EpochMismatch { .. }
    ));
    let accounting = receiver.accounting_snapshot();
    assert_eq!(accounting.trace.sampled_in, 1);
    assert_eq!(accounting.event.recorded, 1);
    assert_eq!(accounting.event.epoch_mismatch, 1);
}

#[test]
fn planted_trace_faults_fail_for_their_own_reason_and_recover_independently() {
    let baseline = complete_keypress_trace();
    assert!(baseline.validate_qualifying().is_ok());

    let mut missing_final = baseline.clone();
    let removed = missing_final
        .events
        .pop()
        .expect("baseline has a final stage");
    assert!(matches!(
        missing_final.validate_qualifying(),
        Err(TraceContractError::MissingStage { .. })
    ));
    missing_final.events.push(removed);
    assert!(missing_final.validate_qualifying().is_ok());

    let mut unavailable = baseline.clone();
    unavailable.events[0].counter_unavailability.queue_depth = true;
    assert!(matches!(
        unavailable.validate_qualifying(),
        Err(TraceContractError::CountersUnavailable { event_ordinal: 0 })
    ));
    unavailable.events[0].counter_unavailability.queue_depth = false;
    assert!(unavailable.validate_qualifying().is_ok());

    let mut duplicate = baseline.clone();
    duplicate.events[1].stage = duplicate.events[0].stage;
    assert!(matches!(
        duplicate.validate_structure(),
        Err(TraceContractError::DuplicateStage { .. })
    ));
    duplicate.events[1].stage = baseline.events[1].stage;
    assert!(duplicate.validate_qualifying().is_ok());

    let mut out_of_order = baseline.clone();
    out_of_order.events[1].stage = baseline.events[2].stage;
    assert!(matches!(
        out_of_order.validate_structure(),
        Err(TraceContractError::StageOutOfOrder { .. })
    ));
    out_of_order.events[1].stage = baseline.events[1].stage;
    assert!(out_of_order.validate_qualifying().is_ok());

    let mut topology_change = baseline.clone();
    topology_change.events[1].topology.pane_id += 1;
    assert!(matches!(
        topology_change.validate_structure(),
        Err(TraceContractError::TraceTopologyChanged { .. })
    ));
    topology_change.events[1].topology = baseline.events[1].topology;
    assert!(topology_change.validate_qualifying().is_ok());

    let mut invalid_generation = baseline.clone();
    invalid_generation.events[0].generations.terminal_generation = Some(0);
    assert!(matches!(
        invalid_generation.validate_structure(),
        Err(TraceContractError::InvalidGeneration {
            field: "terminal_generation"
        })
    ));
    invalid_generation.events[0].generations = baseline.events[0].generations;
    assert!(invalid_generation.validate_qualifying().is_ok());

    let from = baseline.events[0].started_at;
    let mut foreign_clock = baseline.events[1].started_at;
    foreign_clock.clock_domain.clock_id += 1;
    assert!(matches!(
        from.duration_until(foreign_clock),
        Err(TraceContractError::CrossClockArithmetic { .. })
    ));
    foreign_clock.clock_domain = from.clock_domain;
    assert!(from.duration_until(foreign_clock).is_ok());
}

#[test]
fn bounded_decoder_rejects_seeded_content_and_trailing_documents() {
    let baseline = complete_keypress_trace();
    let encoded = serde_json::to_vec(&baseline).expect("baseline trace encodes");
    let decoded = InteractionTraceV2::decode_json_bounded(&encoded)
        .expect("bounded decoder accepts the canonical trace");
    assert_eq!(decoded, baseline);

    let planted_secret = "sk_live_planted_privacy_negative";
    assert!(!String::from_utf8_lossy(&encoded).contains(planted_secret));
    let mut hostile = serde_json::to_value(&baseline).expect("baseline converts to JSON");
    hostile
        .as_object_mut()
        .expect("trace is a JSON object")
        .insert("pane_text".to_owned(), serde_json::json!(planted_secret));
    let hostile = serde_json::to_vec(&hostile).expect("hostile fixture encodes");
    assert!(InteractionTraceV2::decode_json_bounded(&hostile).is_err());

    let mut trailing = encoded;
    trailing.extend_from_slice(b" {}");
    assert!(InteractionTraceV2::decode_json_bounded(&trailing).is_err());

    let oversized = vec![b' '; MAX_INTERACTION_TRACE_JSON_BYTES + 1];
    assert_eq!(
        InteractionTraceV2::decode_json_bounded(&oversized),
        Err(InteractionTraceDecodeError::PayloadTooLarge {
            actual_bytes: oversized.len(),
            max_bytes: MAX_INTERACTION_TRACE_JSON_BYTES,
        })
    );
}

#[test]
fn unavailable_platform_marker_stays_outside_internal_recorder_authority() {
    let config = RecorderConfig::new(
        epoch(31),
        run(32),
        RecorderMode::CertificationWithMarkers,
        RecorderSamplerConfigV1::certification(),
        1,
        1,
        TEST_BYTE_CEILING,
    )
    .expect("marker gauntlet config is valid");
    let recorder = FlightRecorder::new(config).expect("marker gauntlet recorder allocates");
    let producer = recorder
        .register_producer(0)
        .expect("marker gauntlet producer registers");
    let token = match recorder.admit_local_trace(&producer, InteractionTracePath::Keypress) {
        TraceAdmission::Admitted { token, .. } => token,
        other => panic!("marker gauntlet admission failed: {other:?}"),
    };
    let fields = fields_for(InteractionTracePath::Keypress, 0, 1);
    let (record_outcome, payload) = recorder
        .record_and_prepare_platform_marker(&producer, token, &fields)
        .expect("recorded event prepares a marker payload");
    assert!(matches!(record_outcome, RecordOutcome::Recorded { .. }));
    let payload = payload.expect("marker mode prepares a payload");

    let emitter = PlatformMarkerEmitter::for_recorder(
        &recorder,
        UnsupportedPlatformMarkerAdapter::new(MarkerUnavailableReason::PermissionDenied),
    );
    assert_eq!(
        emitter.emit(payload),
        PlatformMarkerOutcome::Unavailable(MarkerUnavailableReason::PermissionDenied)
    );
    let PlatformMarkerFinishOutcome::Ready(marker_snapshot) = emitter.finish(1) else {
        panic!("synchronous unavailable adapter must finish without draining");
    };
    assert_eq!(
        marker_snapshot.authority,
        PlatformMarkerAuthorityV1::Inexact
    );
    assert_eq!(marker_snapshot.accounting.attempted, 1);
    assert_eq!(marker_snapshot.accounting.unavailable, 1);

    let frozen = recorder
        .try_freeze()
        .expect("internal recorder still freezes");
    assert_eq!(frozen.len(), 1);
    assert_eq!(frozen.accounting().event.recorded, 1);
    assert_eq!(
        frozen.accounting().authority,
        RecorderAccountingAuthority::Exact
    );
}

#[test]
fn external_marker_loss_cannot_demote_or_promote_internal_certification() {
    let config = RecorderConfig::new(
        epoch(41),
        run(42),
        RecorderMode::CertificationWithMarkers,
        RecorderSamplerConfigV1::certification(),
        1,
        1,
        TEST_BYTE_CEILING,
    )
    .expect("manifest gauntlet config is valid");
    let clock_domain = InteractionTraceClockDomain {
        host_id: 1,
        process_generation: 2,
        clock_id: 3,
    };
    let manifest = RecorderEpochManifestV1 {
        schema_version: RECORDER_EPOCH_MANIFEST_SCHEMA_VERSION,
        epoch_id: config.epoch_id(),
        previous_epoch_id: None,
        mode: config.mode(),
        sampler: config.sampler(),
        start_reason: RecorderEpochStartReason::ProcessStart,
        close_reason: Some(RecorderEpochCloseReason::NormalShutdown),
        lifecycle: RecorderLifecycleState::Closed,
        started_at: InteractionTraceTimestamp {
            clock_domain,
            monotonic_ns: 1,
            wall_time_unix_ns: None,
        },
        closed_at: Some(InteractionTraceTimestamp {
            clock_domain,
            monotonic_ns: 2,
            wall_time_unix_ns: None,
        }),
        capacity: config.capacity(),
        trace_accounting: RecorderTraceAccountingV1 {
            sampled_in: 1,
            sampled_out: 0,
            trace_id_exhausted: 0,
        },
        event_accounting: RecorderEventAccountingV1 {
            recorded: 1,
            queue_full: 0,
            closing: 0,
            clock_invalid: 0,
            epoch_mismatch: 0,
        },
        accounting_authority: RecorderAccountingAuthority::Exact,
        shutdown: RecorderShutdownStatusV1::Completed { frozen_events: 1 },
        export: RecorderExportStatusV1::Completed { exported_events: 1 },
        marker_authority: PlatformMarkerAuthorityV1::Inexact,
        marker_accounting: PlatformMarkerAccountingV1 {
            attempted: 1,
            emitted: 1,
            unavailable: 0,
            dropped: 0,
            loss_unknown: true,
        },
    };
    assert_eq!(
        manifest.certification_verdict(RecorderCertificationClass::InternalRecorderCertification),
        Ok(RecorderCertificationVerdict::Qualifying)
    );
    assert_eq!(
        manifest.certification_verdict(RecorderCertificationClass::MarkerAssistedCertification),
        Ok(RecorderCertificationVerdict::NonQualifying)
    );

    let mut exact_markers = manifest;
    exact_markers.marker_authority = PlatformMarkerAuthorityV1::ExactEveryRecordedEvent;
    exact_markers.marker_accounting.loss_unknown = false;
    assert_eq!(
        exact_markers
            .certification_verdict(RecorderCertificationClass::MarkerAssistedCertification),
        Ok(RecorderCertificationVerdict::Qualifying)
    );
}
