//! Property-based tests for MetricsSnapshot Prometheus rendering.
//!
//! Verifies structural invariants of the Prometheus text exposition
//! format produced by `MetricsSnapshot::render_prometheus`.

#![cfg(feature = "metrics")]

use frankenterm_core::metrics::{EventBusSnapshot, MetricsSnapshot};
use proptest::prelude::*;

// ---------------------------------------------------------------------------
// Strategies
// ---------------------------------------------------------------------------

fn arb_prefix() -> impl Strategy<Value = String> {
    prop_oneof![
        Just(String::new()),
        "[a-z_]{1,8}",
        "[a-zA-Z0-9_]{1,12}",
        // Include special chars that need sanitization
        "[a-z.\\-/]{1,8}",
    ]
}

fn arb_event_bus_snapshot() -> impl Strategy<Value = EventBusSnapshot> {
    (
        (
            any::<u64>(),
            any::<u64>(),
            any::<u64>(),
            any::<u64>(),
            0_usize..10_000,
            0_usize..10_000,
            0_usize..10_000,
        ),
        (
            0_usize..10_000,
            0_usize..100,
            0_usize..100,
            0_usize..100,
            proptest::option::of(any::<u64>()),
            proptest::option::of(any::<u64>()),
            proptest::option::of(any::<u64>()),
        ),
    )
        .prop_map(
            |(
                (pub_count, dropped, active, lag, cap, dq, det_q),
                (sig_q, ds, det_s, sig_s, d_lag, det_lag, sig_lag),
            )| {
                EventBusSnapshot {
                    events_published: pub_count,
                    events_dropped_no_subscribers: dropped,
                    active_subscribers: active,
                    subscriber_lag_events: lag,
                    capacity: cap,
                    delta_queued: dq,
                    detection_queued: det_q,
                    signal_queued: sig_q,
                    delta_subscribers: ds,
                    detection_subscribers: det_s,
                    signal_subscribers: sig_s,
                    delta_oldest_lag_ms: d_lag,
                    detection_oldest_lag_ms: det_lag,
                    signal_oldest_lag_ms: sig_lag,
                }
            },
        )
}

fn arb_metrics_snapshot() -> impl Strategy<Value = MetricsSnapshot> {
    (
        (
            0.0_f64..1_000_000.0,
            0_usize..1_000,
            0_usize..10_000,
            0_usize..10_000,
            0_usize..10_000,
            any::<u64>(),
            any::<u64>(),
        ),
        (
            prop_oneof![
                0.0_f64..1000.0,
                Just(f64::NAN),
                Just(f64::INFINITY),
                Just(f64::NEG_INFINITY),
            ],
            any::<u64>(),
            any::<u64>(),
            any::<u64>(),
            proptest::option::of(any::<u64>()),
            any::<u64>(),
            any::<u64>(),
        ),
        (
            any::<u64>(),
            any::<u64>(),
            any::<u64>(),
            any::<u64>(),
            prop_oneof![0.0_f64..100.0, Just(f64::NAN), Just(0.0),],
            proptest::option::of(arb_event_bus_snapshot()),
        ),
    )
        .prop_map(
            |(
                (uptime, panes, cqd, cqc, wqd, seg, evt),
                (lag_avg, lag_max, lag_sum, lag_count, db_age, no_input, no_batch),
                (no_ibytes, no_ebytes, no_max_evt, no_max_bytes, coalesce, bus),
            )| {
                MetricsSnapshot {
                    uptime_seconds: uptime,
                    observed_panes: panes,
                    capture_queue_depth: cqd,
                    capture_queue_capacity: cqc,
                    write_queue_depth: wqd,
                    segments_persisted: seg,
                    events_recorded: evt,
                    ingest_lag_avg_ms: lag_avg,
                    ingest_lag_max_ms: lag_max,
                    ingest_lag_sum_ms: lag_sum,
                    ingest_lag_count: lag_count,
                    db_last_write_age_ms: db_age,
                    native_output_input_events: no_input,
                    native_output_batches_emitted: no_batch,
                    native_output_input_bytes: no_ibytes,
                    native_output_emitted_bytes: no_ebytes,
                    native_output_max_batch_events: no_max_evt,
                    native_output_max_batch_bytes: no_max_bytes,
                    native_output_coalesce_ratio: coalesce,
                    event_bus: bus,
                }
            },
        )
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Parse rendered Prometheus text into structured triples.
/// Each metric produces: (HELP line, TYPE line, value line).
fn parse_metric_blocks(rendered: &str) -> Vec<(String, String, String)> {
    let lines: Vec<&str> = rendered.lines().collect();
    let mut blocks = Vec::new();
    let mut i = 0;
    while i + 2 < lines.len() {
        if lines[i].starts_with("# HELP ") && lines[i + 1].starts_with("# TYPE ") {
            blocks.push((
                lines[i].to_string(),
                lines[i + 1].to_string(),
                lines[i + 2].to_string(),
            ));
            i += 3;
        } else {
            i += 1;
        }
    }
    blocks
}

/// Extract metric name from a value line like "ft_uptime_seconds 123.456".
fn metric_name_from_value_line(line: &str) -> Option<&str> {
    line.split_whitespace().next()
}

/// Sanitize a prefix the same way the production code does.
fn reference_sanitize(prefix: &str) -> String {
    prefix
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '_' {
                ch
            } else {
                '_'
            }
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Properties
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    // ---- Structural properties ----

    #[test]
    fn every_metric_has_help_type_value_triple(
        snap in arb_metrics_snapshot(),
        prefix in arb_prefix(),
    ) {
        let rendered = snap.render_prometheus(&prefix);
        let lines: Vec<&str> = rendered.lines().collect();

        // Lines come in triples: HELP, TYPE, value
        prop_assert_eq!(
            lines.len() % 3,
            0,
            "line count {} not divisible by 3",
            lines.len()
        );

        for chunk in lines.chunks(3) {
            prop_assert!(
                chunk[0].starts_with("# HELP "),
                "expected HELP line, got: {}",
                chunk[0]
            );
            prop_assert!(
                chunk[1].starts_with("# TYPE "),
                "expected TYPE line, got: {}",
                chunk[1]
            );
            prop_assert!(
                !chunk[2].starts_with('#'),
                "value line should not start with #: {}",
                chunk[2]
            );
        }
    }

    #[test]
    fn metric_names_use_sanitized_prefix(
        snap in arb_metrics_snapshot(),
        prefix in arb_prefix(),
    ) {
        let rendered = snap.render_prometheus(&prefix);
        let sanitized = reference_sanitize(&prefix);
        let blocks = parse_metric_blocks(&rendered);

        for (_, _, value_line) in &blocks {
            if let Some(name) = metric_name_from_value_line(value_line) {
                if sanitized.is_empty() {
                    // No prefix: name should not start with _
                    prop_assert!(
                        !name.starts_with('_'),
                        "metric {} starts with _ despite empty prefix",
                        name
                    );
                } else {
                    // With prefix: name should start with sanitized prefix + _
                    prop_assert!(
                        name.starts_with(&format!("{}_", sanitized)),
                        "metric {} does not start with {}_",
                        name,
                        sanitized
                    );
                }
            }
        }
    }

    #[test]
    fn metric_names_are_valid_prometheus_identifiers(
        snap in arb_metrics_snapshot(),
        prefix in arb_prefix(),
    ) {
        let rendered = snap.render_prometheus(&prefix);
        let blocks = parse_metric_blocks(&rendered);

        for (_, _, value_line) in &blocks {
            if let Some(name) = metric_name_from_value_line(value_line) {
                // Production sanitize_prefix allows digits at start.
                // Check only that chars are alphanumeric or underscore.
                let valid = name
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == ':');
                prop_assert!(
                    valid,
                    "metric name '{}' contains invalid chars",
                    name
                );
            }
        }
    }

    #[test]
    fn type_lines_contain_valid_metric_types(
        snap in arb_metrics_snapshot(),
        prefix in arb_prefix(),
    ) {
        let rendered = snap.render_prometheus(&prefix);
        for line in rendered.lines() {
            if line.starts_with("# TYPE ") {
                let parts: Vec<&str> = line.split_whitespace().collect();
                // "# TYPE metric_name gauge/counter"
                prop_assert!(
                    parts.len() >= 4,
                    "TYPE line too short: {}",
                    line
                );
                let mtype = parts[3];
                prop_assert!(
                    mtype == "gauge" || mtype == "counter",
                    "unexpected metric type '{}' in: {}",
                    mtype,
                    line
                );
            }
        }
    }

    #[test]
    fn value_lines_have_parseable_numbers(
        snap in arb_metrics_snapshot(),
        prefix in arb_prefix(),
    ) {
        let rendered = snap.render_prometheus(&prefix);
        let blocks = parse_metric_blocks(&rendered);

        for (_, _, value_line) in &blocks {
            let parts: Vec<&str> = value_line.split_whitespace().collect();
            prop_assert!(
                parts.len() == 2,
                "value line should have name + value: {}",
                value_line
            );
            let val = parts[1];
            let parseable = val.parse::<f64>().is_ok() || val.parse::<i64>().is_ok();
            prop_assert!(
                parseable,
                "value '{}' is not a valid number in: {}",
                val,
                value_line
            );
        }
    }

    // ---- Determinism ----

    #[test]
    fn render_is_deterministic(
        snap in arb_metrics_snapshot(),
        prefix in arb_prefix(),
    ) {
        let a = snap.render_prometheus(&prefix);
        let b = snap.render_prometheus(&prefix);
        prop_assert_eq!(a, b);
    }

    // ---- Event bus presence/absence ----

    #[test]
    fn event_bus_metrics_absent_when_none(
        snap_base in arb_metrics_snapshot(),
        prefix in arb_prefix(),
    ) {
        let snap = MetricsSnapshot {
            event_bus: None,
            ..snap_base
        };
        let rendered = snap.render_prometheus(&prefix);
        prop_assert!(
            !rendered.contains("event_bus_"),
            "event_bus metrics should be absent when event_bus is None"
        );
    }

    #[test]
    fn event_bus_metrics_present_when_some(
        snap_base in arb_metrics_snapshot(),
        bus in arb_event_bus_snapshot(),
        prefix in "[a-z]{1,4}",
    ) {
        let snap = MetricsSnapshot {
            event_bus: Some(bus),
            ..snap_base
        };
        let rendered = snap.render_prometheus(&prefix);
        prop_assert!(
            rendered.contains("event_bus_events_published_total"),
            "event_bus metrics should be present when event_bus is Some"
        );
        prop_assert!(
            rendered.contains("event_bus_capacity"),
            "event_bus capacity should be present"
        );
    }

    // ---- Metric count consistency ----

    #[test]
    fn metric_count_without_event_bus_is_constant(
        snap_base in arb_metrics_snapshot(),
        prefix in arb_prefix(),
    ) {
        let snap = MetricsSnapshot {
            event_bus: None,
            ..snap_base
        };
        let rendered = snap.render_prometheus(&prefix);
        let blocks = parse_metric_blocks(&rendered);
        // Without event bus: core metrics only (19 metrics)
        prop_assert_eq!(
            blocks.len(),
            19,
            "expected 19 core metrics, got {}",
            blocks.len()
        );
    }

    #[test]
    fn metric_count_with_event_bus_is_constant(
        snap_base in arb_metrics_snapshot(),
        bus in arb_event_bus_snapshot(),
        prefix in arb_prefix(),
    ) {
        let snap = MetricsSnapshot {
            event_bus: Some(bus),
            ..snap_base
        };
        let rendered = snap.render_prometheus(&prefix);
        let blocks = parse_metric_blocks(&rendered);
        // With event bus: 19 core + 14 event bus = 33
        prop_assert_eq!(
            blocks.len(),
            33,
            "expected 33 metrics (19 core + 14 bus), got {}",
            blocks.len()
        );
    }

    // ---- Non-finite float handling ----

    #[test]
    fn non_finite_floats_render_as_zero(prefix in "[a-z]{1,4}") {
        let snap = MetricsSnapshot {
            uptime_seconds: f64::NAN,
            ingest_lag_avg_ms: f64::INFINITY,
            native_output_coalesce_ratio: f64::NEG_INFINITY,
            ..MetricsSnapshot::default()
        };
        let rendered = snap.render_prometheus(&prefix);

        // Extract uptime value
        for line in rendered.lines() {
            if !line.starts_with('#') {
                if let Some(name) = metric_name_from_value_line(line) {
                    if name.ends_with("uptime_seconds")
                        || name.ends_with("ingest_lag_avg_ms")
                        || name.ends_with("native_output_coalesce_ratio")
                    {
                        let val = line.split_whitespace().nth(1).unwrap_or("");
                        prop_assert_eq!(
                            val,
                            "0",
                            "non-finite float should render as 0 for {}",
                            name
                        );
                    }
                }
            }
        }
    }

    // ---- db_last_write_age_ms ----

    #[test]
    fn db_write_age_none_renders_minus_one(prefix in "[a-z]{1,4}") {
        let snap = MetricsSnapshot {
            db_last_write_age_ms: None,
            ..MetricsSnapshot::default()
        };
        let rendered = snap.render_prometheus(&prefix);
        let expected = format!("{}_db_last_write_age_ms -1", prefix);
        prop_assert!(
            rendered.contains(&expected),
            "expected '{}' in rendered output",
            expected
        );
    }

    #[test]
    fn db_write_age_some_renders_value(
        age in any::<u64>(),
        prefix in "[a-z]{1,4}",
    ) {
        let snap = MetricsSnapshot {
            db_last_write_age_ms: Some(age),
            ..MetricsSnapshot::default()
        };
        let rendered = snap.render_prometheus(&prefix);
        // The code casts u64 → i64 via `ms as i64`
        let rendered_val = age as i64;
        let expected = format!("{}_db_last_write_age_ms {}", prefix, rendered_val);
        prop_assert!(
            rendered.contains(&expected),
            "expected '{}' in rendered output",
            expected
        );
    }

    // ---- Event bus lag None → -1 ----

    #[test]
    fn event_bus_lag_none_renders_minus_one(prefix in "[a-z]{1,4}") {
        let snap = MetricsSnapshot {
            event_bus: Some(EventBusSnapshot {
                delta_oldest_lag_ms: None,
                detection_oldest_lag_ms: None,
                signal_oldest_lag_ms: None,
                ..EventBusSnapshot::default()
            }),
            ..MetricsSnapshot::default()
        };
        let rendered = snap.render_prometheus(&prefix);
        let expected_delta = format!("{}_event_bus_delta_oldest_lag_ms -1", prefix);
        let expected_det = format!("{}_event_bus_detection_oldest_lag_ms -1", prefix);
        let expected_sig = format!("{}_event_bus_signal_oldest_lag_ms -1", prefix);
        prop_assert!(rendered.contains(&expected_delta));
        prop_assert!(rendered.contains(&expected_det));
        prop_assert!(rendered.contains(&expected_sig));
    }

    // ---- Default snapshot is all-zero ----

    #[test]
    fn default_snapshot_renders_zeroed_values(prefix in "[a-z]{1,4}") {
        let snap = MetricsSnapshot::default();
        let rendered = snap.render_prometheus(&prefix);
        let blocks = parse_metric_blocks(&rendered);

        for (_, type_line, value_line) in &blocks {
            let val = value_line.split_whitespace().nth(1).unwrap_or("");
            // All default values should be 0 or -1 (for db_last_write_age_ms)
            let is_zero_or_minus_one = val == "0" || val == "-1";
            // Allow "gauge" db_last_write_age_ms to be -1
            let is_db_age = value_line.contains("db_last_write_age_ms");
            if is_db_age {
                prop_assert_eq!(val, "-1", "default db_last_write_age_ms should be -1");
            } else {
                prop_assert!(
                    is_zero_or_minus_one,
                    "default metric should be 0 or -1: {} (type: {})",
                    value_line,
                    type_line
                );
            }
        }
    }

    // ---- Prefix sanitization reference check ----

    #[test]
    fn prefix_sanitization_matches_reference(prefix in "[ -~]{0,20}") {
        // Generate a snapshot with a known metric
        let snap = MetricsSnapshot::default();
        let rendered = snap.render_prometheus(&prefix);
        let sanitized = reference_sanitize(&prefix);

        // The first HELP line should contain the sanitized prefix
        if let Some(help_line) = rendered.lines().next() {
            if sanitized.is_empty() {
                prop_assert!(
                    help_line.starts_with("# HELP uptime_seconds"),
                    "empty prefix: first metric should be uptime_seconds, got: {}",
                    help_line
                );
            } else {
                let expected_start = format!("# HELP {}_uptime_seconds", sanitized);
                prop_assert!(
                    help_line.starts_with(&expected_start),
                    "prefix '{}' → sanitized '{}': expected '{}', got: {}",
                    prefix,
                    sanitized,
                    expected_start,
                    help_line
                );
            }
        }
    }

    // ---- Rendered output is non-empty ----

    #[test]
    fn rendered_output_is_always_nonempty(
        snap in arb_metrics_snapshot(),
        prefix in arb_prefix(),
    ) {
        let rendered = snap.render_prometheus(&prefix);
        prop_assert!(!rendered.is_empty(), "rendered output should never be empty");
        prop_assert!(
            rendered.ends_with('\n'),
            "rendered output should end with newline"
        );
    }

    // ---- Consistent HELP/TYPE names match value line name ----

    #[test]
    fn help_type_value_names_are_consistent(
        snap in arb_metrics_snapshot(),
        prefix in arb_prefix(),
    ) {
        let rendered = snap.render_prometheus(&prefix);
        let lines: Vec<&str> = rendered.lines().collect();

        for chunk in lines.chunks(3) {
            if chunk.len() < 3 {
                break;
            }
            // Extract names from each line
            let help_name = chunk[0]
                .strip_prefix("# HELP ")
                .and_then(|s| s.split_whitespace().next());
            let type_name = chunk[1]
                .strip_prefix("# TYPE ")
                .and_then(|s| s.split_whitespace().next());
            let value_name = chunk[2].split_whitespace().next();

            prop_assert_eq!(
                help_name, type_name,
                "HELP and TYPE names must match"
            );
            prop_assert_eq!(
                type_name, value_name,
                "TYPE and value names must match"
            );
        }
    }
}
