use std::collections::BTreeMap;

use proptest::prelude::*;

use frankenterm_core::kitty_graphics::{ImageRejectionReason, KittyAction, KittyImageFormat};
use frankenterm_core::kitty_graphics_compositor::{
    CompositorLayer, KittyErrorCode, KittyQueryOutcome, StructuredLogRow,
};
use frankenterm_core::kitty_graphics_session_telemetry::{
    action_slug, admitted_row, eviction_row, format_slug, layer_slug, query_response_row,
    query_response_slug, rejected_row, rejection_reason_slug, render_session_summary_jsonl,
    KittySessionAggregator, KittySessionSummary,
};

#[derive(Debug, Clone)]
enum SessionEvent {
    Admitted {
        ts_ms: u64,
        image_id: u32,
        format: KittyImageFormat,
        bytes_in: u32,
        bytes_out: u32,
        decode_ns: u64,
        layer: CompositorLayer,
    },
    Rejected {
        ts_ms: u64,
        image_id: u32,
        reason: ImageRejectionReason,
    },
    Query {
        ts_ms: u64,
        action: KittyAction,
        outcome: KittyQueryOutcome,
    },
    Eviction {
        ts_ms: u64,
        evicted_count: u32,
        freed_bytes: u64,
    },
}

fn arb_format() -> impl Strategy<Value = KittyImageFormat> {
    prop_oneof![
        Just(KittyImageFormat::Png),
        Just(KittyImageFormat::Rgb24),
        Just(KittyImageFormat::Rgba32),
        Just(KittyImageFormat::ZlibCompressed),
    ]
}

fn arb_rejection_reason() -> impl Strategy<Value = ImageRejectionReason> {
    prop_oneof![
        Just(ImageRejectionReason::Oversized),
        Just(ImageRejectionReason::DecodeTimeout),
        Just(ImageRejectionReason::Malformed),
        Just(ImageRejectionReason::DimensionsOverflow),
    ]
}

fn arb_action() -> impl Strategy<Value = KittyAction> {
    prop_oneof![
        Just(KittyAction::Transmit),
        Just(KittyAction::TransmitDisplay),
        Just(KittyAction::Place),
        Just(KittyAction::Delete),
        Just(KittyAction::Query),
    ]
}

fn arb_layer() -> impl Strategy<Value = CompositorLayer> {
    prop_oneof![
        Just(CompositorLayer::Background),
        Just(CompositorLayer::Selection),
        Just(CompositorLayer::Text),
        Just(CompositorLayer::Cursor),
        Just(CompositorLayer::Overlay),
    ]
}

fn arb_error_code() -> impl Strategy<Value = KittyErrorCode> {
    prop_oneof![
        Just(KittyErrorCode::Enofile),
        Just(KittyErrorCode::Eninput),
        Just(KittyErrorCode::Eimagedata),
        Just(KittyErrorCode::Eformat),
        Just(KittyErrorCode::Enoimg),
        Just(KittyErrorCode::Eunsupp),
    ]
}

fn arb_query_outcome() -> impl Strategy<Value = KittyQueryOutcome> {
    prop_oneof![
        (0u32..=10_000).prop_map(|image_id| KittyQueryOutcome::Ok { image_id }),
        (0u32..=10_000, arb_error_code()).prop_map(|(image_id, error_code)| {
            KittyQueryOutcome::Error {
                image_id,
                error_code,
            }
        }),
    ]
}

fn arb_event() -> impl Strategy<Value = SessionEvent> {
    prop_oneof![
        (
            any::<u64>(),
            0u32..=10_000,
            arb_format(),
            0u32..=1_000_000,
            0u32..=1_000_000,
            0u64..=1_000_000_000,
            arb_layer(),
        )
            .prop_map(
                |(ts_ms, image_id, format, bytes_in, bytes_out, decode_ns, layer)| {
                    SessionEvent::Admitted {
                        ts_ms,
                        image_id,
                        format,
                        bytes_in,
                        bytes_out,
                        decode_ns,
                        layer,
                    }
                },
            ),
        (any::<u64>(), 0u32..=10_000, arb_rejection_reason()).prop_map(
            |(ts_ms, image_id, reason)| SessionEvent::Rejected {
                ts_ms,
                image_id,
                reason,
            },
        ),
        (any::<u64>(), arb_action(), arb_query_outcome()).prop_map(|(ts_ms, action, outcome)| {
            SessionEvent::Query {
                ts_ms,
                action,
                outcome,
            }
        },),
        (any::<u64>(), 0u32..=1_000, 0u64..=1_000_000_000).prop_map(
            |(ts_ms, evicted_count, freed_bytes)| SessionEvent::Eviction {
                ts_ms,
                evicted_count,
                freed_bytes,
            },
        ),
    ]
}

fn apply_event(aggregator: &mut KittySessionAggregator, event: &SessionEvent) {
    match event {
        SessionEvent::Admitted {
            ts_ms,
            image_id,
            format,
            bytes_in,
            bytes_out,
            decode_ns,
            layer,
        } => aggregator.record_admitted(
            *ts_ms, *image_id, *format, *bytes_in, *bytes_out, *decode_ns, *layer,
        ),
        SessionEvent::Rejected {
            ts_ms,
            image_id,
            reason,
        } => aggregator.record_rejected(*ts_ms, *image_id, *reason),
        SessionEvent::Query {
            ts_ms,
            action,
            outcome,
        } => aggregator.record_query(*ts_ms, *action, outcome),
        SessionEvent::Eviction {
            ts_ms,
            evicted_count,
            freed_bytes,
        } => aggregator.record_eviction(*ts_ms, *evicted_count, *freed_bytes),
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(96))]

    #[test]
    fn proptest_kitty_graphics_session_telemetry_slugs_are_stable_ascii(
        format in arb_format(),
        reason in arb_rejection_reason(),
        action in arb_action(),
        layer in arb_layer(),
        outcome in arb_query_outcome(),
    ) {
        for slug in [
            format_slug(format),
            rejection_reason_slug(reason),
            action_slug(action),
            layer_slug(layer),
            query_response_slug(&outcome),
        ] {
            prop_assert!(!slug.is_empty());
            prop_assert!(slug.chars().all(|ch| ch.is_ascii_lowercase() || ch == '_' || ch.is_ascii_digit()));
        }
    }

    #[test]
    fn proptest_kitty_graphics_session_telemetry_row_builders_preserve_payloads(
        ts_ms in any::<u64>(),
        image_id in any::<u32>(),
        format in arb_format(),
        reason in arb_rejection_reason(),
        action in arb_action(),
        outcome in arb_query_outcome(),
        bytes_in in any::<u32>(),
        bytes_out in any::<u32>(),
        decode_ns in any::<u64>(),
        layer in arb_layer(),
        evicted_count in any::<u32>(),
        freed_bytes in any::<u64>(),
    ) {
        let admitted = admitted_row(ts_ms, image_id, format, bytes_in, bytes_out, decode_ns, layer);
        prop_assert_eq!(
            admitted,
            StructuredLogRow::ImageAdmitted {
                ts_ms,
                image_id,
                format_slug: format_slug(format).to_string(),
                bytes_in,
                bytes_out,
                decode_ns,
                layer_slug: layer_slug(layer).to_string(),
            },
        );

        let rejected = rejected_row(ts_ms, image_id, reason);
        prop_assert_eq!(
            rejected,
            StructuredLogRow::ImageRejected {
                ts_ms,
                image_id,
                reason_slug: rejection_reason_slug(reason).to_string(),
            },
        );

        let query = query_response_row(ts_ms, action, &outcome);
        let expected_query_image_id = match outcome {
            KittyQueryOutcome::Ok { image_id } => image_id,
            KittyQueryOutcome::Error { image_id, .. } => image_id,
        };
        prop_assert_eq!(
            query,
            StructuredLogRow::QueryResponse {
                ts_ms,
                action_slug: action_slug(action).to_string(),
                image_id: expected_query_image_id,
                response_slug: query_response_slug(&outcome).to_string(),
            },
        );

        prop_assert_eq!(
            eviction_row(ts_ms, evicted_count, freed_bytes),
            StructuredLogRow::EvictionCycle {
                ts_ms,
                evicted_count,
                freed_bytes,
            },
        );
    }

    #[test]
    fn proptest_kitty_graphics_session_telemetry_flush_drains_rows_without_resetting_summary(
        session_id in any::<u64>(),
        started_ts_ms in any::<u64>(),
        events in prop::collection::vec(arb_event(), 0..64),
    ) {
        let mut aggregator = KittySessionAggregator::new(session_id, started_ts_ms);
        for event in &events {
            apply_event(&mut aggregator, event);
        }

        let before = aggregator.summary().clone();
        prop_assert_eq!(aggregator.pending_row_count(), events.len());
        let rows = aggregator.flush_log_rows();
        prop_assert_eq!(rows.len(), events.len());
        prop_assert_eq!(aggregator.pending_row_count(), 0);
        prop_assert_eq!(aggregator.summary(), &before);

        let second_flush = aggregator.flush_log_rows();
        prop_assert!(second_flush.is_empty());
        prop_assert_eq!(aggregator.summary(), &before);
    }

    #[test]
    fn proptest_kitty_graphics_session_telemetry_aggregator_summary_matches_events(
        session_id in any::<u64>(),
        started_ts_ms in any::<u64>(),
        ended_ts_ms in any::<u64>(),
        events in prop::collection::vec(arb_event(), 0..64),
    ) {
        let mut aggregator = KittySessionAggregator::new(session_id, started_ts_ms);
        let mut admitted_by_format = BTreeMap::new();
        let mut rejected_by_reason = BTreeMap::new();
        let mut total_admitted = 0u64;
        let mut total_rejected = 0u64;
        let mut total_queries = 0u64;
        let mut total_evictions = 0u64;
        let mut total_evicted_images = 0u64;
        let mut total_bytes_in = 0u64;
        let mut total_bytes_out = 0u64;
        let mut total_decode_ns = 0u64;
        let mut total_freed_bytes = 0u64;

        for event in &events {
            apply_event(&mut aggregator, event);
            match event {
                SessionEvent::Admitted { format, bytes_in, bytes_out, decode_ns, .. } => {
                    total_admitted += 1;
                    total_bytes_in += u64::from(*bytes_in);
                    total_bytes_out += u64::from(*bytes_out);
                    total_decode_ns += *decode_ns;
                    *admitted_by_format.entry(format_slug(*format).to_string()).or_insert(0) += 1;
                }
                SessionEvent::Rejected { reason, .. } => {
                    total_rejected += 1;
                    *rejected_by_reason.entry(rejection_reason_slug(*reason).to_string()).or_insert(0) += 1;
                }
                SessionEvent::Query { .. } => total_queries += 1,
                SessionEvent::Eviction { evicted_count, freed_bytes, .. } => {
                    total_evictions += 1;
                    total_evicted_images += u64::from(*evicted_count);
                    total_freed_bytes += *freed_bytes;
                }
            }
        }

        let summary = aggregator.finalize(ended_ts_ms);
        prop_assert_eq!(summary.session_id, session_id);
        prop_assert_eq!(summary.started_ts_ms, started_ts_ms);
        prop_assert_eq!(summary.ended_ts_ms, ended_ts_ms);
        prop_assert_eq!(summary.total_admitted, total_admitted);
        prop_assert_eq!(summary.total_rejected, total_rejected);
        prop_assert_eq!(summary.total_queries, total_queries);
        prop_assert_eq!(summary.total_evictions, total_evictions);
        prop_assert_eq!(summary.total_evicted_images, total_evicted_images);
        prop_assert_eq!(summary.total_bytes_in, total_bytes_in);
        prop_assert_eq!(summary.total_bytes_out, total_bytes_out);
        prop_assert_eq!(summary.total_decode_ns, total_decode_ns);
        prop_assert_eq!(summary.total_freed_bytes, total_freed_bytes);
        prop_assert_eq!(summary.admitted_by_format, admitted_by_format);
        prop_assert_eq!(summary.rejected_by_reason, rejected_by_reason);

        let rendered = render_session_summary_jsonl(&summary);
        prop_assert!(!rendered.ends_with('\n'));
        let reparsed: KittySessionSummary = serde_json::from_str(&rendered).unwrap();
        prop_assert_eq!(reparsed, summary);
    }

    #[test]
    fn proptest_kitty_graphics_session_telemetry_summary_ratio_helpers_are_integer_safe(
        admitted in 0u64..=1_000_000,
        bytes_in in 0u64..=1_000_000_000,
        bytes_out in 0u64..=1_000_000_000,
        decode_ns in 0u64..=1_000_000_000,
    ) {
        let summary = KittySessionSummary {
            session_id: 1,
            started_ts_ms: 2,
            ended_ts_ms: 3,
            total_admitted: admitted,
            total_rejected: 0,
            total_queries: 0,
            total_evictions: 0,
            total_evicted_images: 0,
            total_bytes_in: bytes_in,
            total_bytes_out: bytes_out,
            total_decode_ns: decode_ns,
            total_freed_bytes: 0,
            admitted_by_format: BTreeMap::new(),
            rejected_by_reason: BTreeMap::new(),
        };

        prop_assert_eq!(
            summary.avg_decode_ns(),
            if admitted == 0 { 0 } else { decode_ns / admitted },
        );
        prop_assert_eq!(
            summary.compression_ratio_bp(),
            if bytes_in == 0 { 0 } else { (bytes_out * 10_000) / bytes_in },
        );
    }
}
