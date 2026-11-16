#![cfg(feature = "asupersync-runtime")]

//! Golden and property coverage for Cx error-propagation regressions.
//!
//! Bead: ft-npkn6
//!
//! Locks the ft-ati70 and ft-l8uk7 fixes to externally visible behavior:
//! Cx cancellation is observed, typed cancellation counters are recorded
//! instead of swallowed, and tailer joins drain no outcome once cancelled.

mod common;

use common::fixtures::RuntimeFixture;

use frankenterm_core::cx::for_testing;
use frankenterm_core::outcome::CancelKind;
use frankenterm_core::runtime_async;
use frankenterm_core::tailer::{PollOutcome, PollTaskSet, TailerPollTaskSet};
use frankenterm_core::telemetry::{TelemetryCollector, TelemetryConfig};

use proptest::prelude::*;
use serde_json::{Value, json};
use std::future::{Future, poll_fn};
use std::task::Poll;
use std::time::Duration;

const GOLDEN_MATRIX: &str = include_str!("fixtures/cx_error_propagation/golden_matrix.json");

fn golden_case(name: &str) -> Result<Value, String> {
    let matrix: Value = serde_json::from_str(GOLDEN_MATRIX)
        .map_err(|err| format!("cx error propagation golden matrix did not parse: {err}"))?;
    let cases = matrix
        .get("cases")
        .and_then(Value::as_array)
        .ok_or_else(|| String::from("cx error propagation golden matrix has no cases array"))?;
    cases
        .iter()
        .find(|case| case.get("case").and_then(Value::as_str) == Some(name))
        .cloned()
        .ok_or_else(|| format!("golden case {name:?} is missing"))
}

fn assert_golden_case(name: &str, actual: Value) -> Result<(), String> {
    let expected = golden_case(name)?;
    if actual == expected {
        return Ok(());
    }
    Err(format!(
        "golden case {name} drifted\nexpected: {expected:#}\nactual: {actual:#}"
    ))
}

async fn cancel_pending_tailer_join(
    pane_id: u64,
    bytes: u64,
    cancel_ms: u64,
    slack_ms: u64,
) -> Result<Option<(u64, PollOutcome)>, String> {
    let mut set = TailerPollTaskSet::new();
    let (complete_tx, complete_rx) = runtime_async::oneshot::channel();
    set.spawn_poll_task(async move {
        runtime_async::oneshot_recv(complete_rx)
            .await
            .expect("completion sender must release the pending poll task");
        (pane_id, PollOutcome::Changed { bytes })
    });
    let cx = for_testing();
    let observed = {
        let mut join = std::pin::pin!(set.join_next_with_cx(&cx));
        // Enter the real join before cancellation. Relative timer deadlines do
        // not establish this order: a busy executor may run the producer before
        // it ever starts the separately spawned canceller's shorter timer.
        let first_poll = poll_fn(|task_cx| Poll::Ready(join.as_mut().poll(task_cx))).await;
        if !first_poll.is_pending() {
            return Err(format!(
                "unreleased producer must leave join pending: {first_poll:?}"
            ));
        }
        runtime_async::sleep(Duration::from_millis(cancel_ms)).await;
        cx.cancel_with(CancelKind::User, Some("ft-npkn6 tailer midflight cancel"));
        runtime_async::sleep(Duration::from_millis(slack_ms)).await;
        // Now the producer is ready, but the already-started cancelled join
        // must check cancellation before consuming its outcome.
        runtime_async::oneshot_send(complete_tx, ())?;
        runtime_async::timeout_with_cx(&for_testing(), Duration::from_secs(2), join)
            .await
            .map_err(|err| format!("cancelled tailer join did not resolve: {err}"))?
    };
    if cx.checkpoint().is_ok() {
        return Err(String::from("midflight cancellation must trip the Cx"));
    }
    if observed.is_none() {
        let retained = runtime_async::timeout_with_cx(
            &for_testing(),
            Duration::from_secs(2),
            set.join_next_with_cx(&for_testing()),
        )
        .await
        .map_err(|err| format!("live join could not recover retained outcome: {err}"))?;
        if !matches!(retained, Some((id, PollOutcome::Changed { bytes: count })) if id == pane_id && count == bytes)
        {
            return Err(format!(
                "cancelled join lost or altered the outcome: {retained:?}"
            ));
        }
    }
    Ok(observed)
}

#[test]
fn telemetry_run_cx_precancel_records_cancelled_counter_golden() -> Result<(), String> {
    let fixture = RuntimeFixture::current_thread();
    let actual = fixture.block_on(async {
        let collector = TelemetryCollector::new(TelemetryConfig {
            sample_interval: Duration::from_secs(60),
            buffer_capacity: 4,
            mux_server_pid: 0,
            ..Default::default()
        });
        let cx = for_testing();
        cx.cancel_with(
            CancelKind::User,
            Some("ft-npkn6 telemetry golden pre-cancel"),
        );

        collector.run_cx(&cx).await;

        let registry = collector.registry();
        json!({
            "case": "telemetry_precancel",
            "surface": "TelemetryCollector::run_cx",
            "cancel_observed": cx.checkpoint().is_err(),
            "outcome": "collector_cancelled",
            "sample_failure_count": registry.counter_value("telemetry.sample_failure"),
            "collector_cancelled_count": registry.counter_value("telemetry.collector.cancelled"),
            "samples_collected": collector.sample_count(),
            "buffer_samples": collector.buffer().len(),
        })
    });

    assert_golden_case("telemetry_precancel", actual)
}

#[test]
fn tailer_join_next_with_cx_midflight_cancel_drains_no_outcome_golden() -> Result<(), String> {
    let fixture = RuntimeFixture::current_thread();
    let actual = fixture.block_on(async {
        let outcome = cancel_pending_tailer_join(7, 19, 5, 25).await?;

        Ok::<Value, String>(json!({
            "case": "tailer_midflight_cancel",
            "surface": "TailerPollTaskSet::join_next_with_cx",
            "cancel_observed": true,
            "outcome": if outcome.is_none() {
                "none_with_cancelled_cx"
            } else {
                "unexpected_poll_outcome"
            },
            "pending_task_completed": outcome.is_some(),
        }))
    })?;

    assert_golden_case("tailer_midflight_cancel", actual)
}

#[test]
fn tailer_midflight_cancel_retains_dsr_regression_outcome() -> Result<(), String> {
    let fixture = RuntimeFixture::current_thread();
    fixture.block_on(async {
        let observed = cancel_pending_tailer_join(12, 1718, 4, 23).await?;
        if observed.is_some() {
            return Err(format!(
                "cancelled join drained the regression outcome: {observed:?}"
            ));
        }
        Ok(())
    })
}

#[test]
fn tailer_join_next_with_cx_live_cx_returns_poll_outcome_golden() -> Result<(), String> {
    let fixture = RuntimeFixture::current_thread();
    let actual = fixture.block_on(async {
        let mut set = TailerPollTaskSet::new();
        set.spawn_poll_task(async { (7, PollOutcome::Changed { bytes: 19 }) });

        let cx = for_testing();
        let outcome = set.join_next_with_cx(&cx).await;
        let (outcome_name, pending_task_completed, pane_id, bytes) = match outcome {
            Some((pane_id, PollOutcome::Changed { bytes })) => (
                "poll_outcome_changed",
                true,
                Value::from(pane_id),
                Value::from(bytes),
            ),
            Some(_) => ("unexpected_poll_outcome", true, Value::Null, Value::Null),
            None => ("no_poll_outcome", false, Value::Null, Value::Null),
        };

        json!({
            "case": "tailer_live_cx",
            "surface": "TailerPollTaskSet::join_next_with_cx",
            "cancel_observed": cx.checkpoint().is_err(),
            "outcome": outcome_name,
            "pending_task_completed": pending_task_completed,
            "pane_id": pane_id,
            "bytes": bytes,
        })
    });

    assert_golden_case("tailer_live_cx", actual)
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    #[test]
    fn telemetry_precancel_property_records_exactly_one_cancelled_counter(
        pid in prop_oneof![Just(0_u32), Just(u32::MAX)],
        buffer_capacity in 1_usize..16,
        sample_interval_secs in 1_u64..4,
    ) {
        let fixture = RuntimeFixture::current_thread();
        let result: Result<(), TestCaseError> = fixture.block_on(async move {
            let collector = TelemetryCollector::new(TelemetryConfig {
                sample_interval: Duration::from_secs(sample_interval_secs),
                buffer_capacity,
                mux_server_pid: pid,
                ..Default::default()
            });
            let cx = for_testing();
            cx.cancel_with(
                CancelKind::User,
                Some("ft-npkn6 telemetry property pre-cancel"),
            );

            collector.run_cx(&cx).await;

            let registry = collector.registry();
            prop_assert!(cx.checkpoint().is_err());
            prop_assert_eq!(
                registry.counter_value("telemetry.collector.cancelled"),
                1
            );
            prop_assert_eq!(registry.counter_value("telemetry.sample_failure"), 0);
            prop_assert_eq!(collector.sample_count(), 0);
            prop_assert_eq!(collector.buffer().len(), 0);
            Ok(())
        });
        result?;
    }

    #[test]
    fn tailer_midflight_cancel_property_drains_no_completed_outcome(
        pane_id in 1_u64..64,
        bytes in 0_u64..4096,
        cancel_ms in 0_u64..5,
        slack_ms in 5_u64..35,
    ) {
        let fixture = RuntimeFixture::current_thread();
        let result: Result<(), TestCaseError> = fixture.block_on(async move {
            let observed = cancel_pending_tailer_join(pane_id, bytes, cancel_ms, slack_ms)
                .await
                .map_err(TestCaseError::fail)?;
            prop_assert!(
                observed.is_none(),
                "cancelled join must drain no later poll outcome: {observed:?}"
            );
            Ok(())
        });
        result?;
    }
}
