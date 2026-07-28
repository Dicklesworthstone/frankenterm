//! Golden + property tests for event_stream Lagged-gap handling (ft-1vca6).
//!
//! Invariant under test: a broadcast-buffer OVERFLOW — the subscriber fell
//! behind the bounded `EventBus` and the bus dropped events before the
//! subscriber drained them — is surfaced as an EXPLICIT gap, never a silent
//! message drop. Two production surfaces uphold this no-silent-gaps doctrine:
//!
//!   * [`FilteredEventStream::next_item_cx`] yields
//!     [`StreamItem::Gap { missed_count }`] when the live subscriber overflows,
//!     so a cursor-resuming follower re-baselines the missed (but still
//!     persisted) detections via the storage cursor instead of skipping them
//!     unnoticed (ft-ubuw2).
//!   * [`EventWaiter::wait_with_cx`] returns
//!     [`WaitResult::Lagged { missed_count, .. }`] — an indeterminate outcome
//!     the caller must re-baseline on — instead of a FALSE
//!     [`WaitResult::Timeout`] when the awaited event was in the dropped window
//!     (ft-dkxp4). Collapsing a lag into a timeout would let a one-shot waiter
//!     conclude "the event never happened" when in fact it was dropped.
//!
//! These are integration-level tests that drive a *real* `EventBus` overflow
//! (publish > capacity without draining), complementing the
//! variant/serde unit tests inlined in `event_stream.rs`. They are
//! intentionally NOT gated behind the `asupersync-runtime` feature so the
//! default `cargo test -p frankenterm-core` exercises them.

use std::time::Duration;

use proptest::prelude::*;

use frankenterm_core::cx::for_request;
use frankenterm_core::event_stream::{
    EventStreamFilter, EventWaiter, FilteredEventStream, StreamCursor, StreamItem, WaitCondition,
    WaitResult,
};
use frankenterm_core::events::{Event, EventBus};
use frankenterm_core::runtime_async::join;

/// Run an async body on an ungated current-thread compat runtime. Mirrors the
/// harness used by `tests/web.rs`, so these tests run without requiring the
/// `asupersync-runtime` feature (and therefore aren't silently skipped by a
/// default `cargo test`).
fn run_async<F>(future: F)
where
    F: std::future::Future<Output = ()>,
{
    use frankenterm_core::runtime_async::CompatRuntime;
    let runtime = frankenterm_core::runtime_async::RuntimeBuilder::current_thread()
        .enable_all()
        .build()
        .expect("failed to build test runtime");
    runtime.block_on(future);
}

/// Outcome of draining an overflowed [`FilteredEventStream`] to completion.
struct OverflowOutcome {
    /// Count of [`StreamItem::Event`] items the stream yielded.
    delivered: u64,
    /// Sum of `missed_count` across every [`StreamItem::Gap`] yielded.
    gap_total: u64,
    /// Whether at least one explicit gap was surfaced.
    saw_gap: bool,
    /// Whether the FIRST item the overflowed stream yielded was the gap (the
    /// overflowed receiver must report the lag before any retained event).
    first_is_gap: bool,
}

/// Subscribe a [`FilteredEventStream`] (empty filter — matches every event),
/// publish `published` filler events into a bus of the given `capacity`
/// WITHOUT draining (so the subscriber overflows), then drain the stream to
/// completion and tally what came out.
///
/// The bus is dropped before draining so the broadcast channel closes once its
/// retained buffer is exhausted — that gives the async drain a deterministic
/// terminating `None` instead of blocking forever on an open-but-empty channel.
fn drive_overflow_and_drain(capacity: usize, published: u64) -> OverflowOutcome {
    let bus = EventBus::new(capacity);
    // `FilteredEventStream::new` takes an owned subscriber, so it does not keep
    // borrowing `bus` after construction — we are free to drop `bus` below.
    let mut stream = FilteredEventStream::new(
        &bus,
        EventStreamFilter::default(),
        StreamCursor::from_beginning(),
        16,
    );

    // Publish more than the buffer can hold while the subscriber sits idle: the
    // oldest events are evicted and the subscriber's next receive must report a
    // lag. `PaneDisappeared` matches the empty filter, so every published event
    // is either delivered or accounted for in a gap.
    for i in 0..published {
        let _ = bus.publish(Event::PaneDisappeared { pane_id: i });
    }
    drop(bus);

    let mut delivered = 0u64;
    let mut gap_total = 0u64;
    let mut saw_gap = false;
    let mut first_is_gap = false;
    let mut first = true;

    run_async(async {
        let cx = for_request();
        loop {
            match stream.next_item_cx(&cx).await {
                Some(StreamItem::Event(_)) => {
                    delivered += 1;
                }
                Some(StreamItem::Gap { missed_count }) => {
                    gap_total += missed_count;
                    saw_gap = true;
                    if first {
                        first_is_gap = true;
                    }
                }
                None => break,
            }
            first = false;
        }
    });

    OverflowOutcome {
        delivered,
        gap_total,
        saw_gap,
        first_is_gap,
    }
}

// ===========================================================================
// Golden: WaitResult::Lagged serializes with its own explicit discriminant.
// ===========================================================================

#[test]
fn wait_result_lagged_golden_serde() {
    // GOLDEN: the operator-facing wait outcome carries an explicit `lagged`
    // discriminant plus the missed count, so a consumer can re-baseline. If a
    // refactor renamed the tag or collapsed the variant this exact-value
    // assertion fails.
    let lagged = WaitResult::Lagged {
        missed_count: 7,
        elapsed_ms: 250,
    };
    let value = serde_json::to_value(&lagged).expect("WaitResult serializes");
    assert_eq!(
        value,
        serde_json::json!({
            "outcome": "lagged",
            "missed_count": 7,
            "elapsed_ms": 250,
        }),
        "WaitResult::Lagged golden JSON shape drifted"
    );

    // Round-trip preserves the explicit gap outcome.
    let back: WaitResult = serde_json::from_value(value.clone()).expect("WaitResult deserializes");
    assert!(back.is_lagged());
    assert!(!back.is_timeout());
    assert!(!back.is_matched());

    // The whole point of ft-dkxp4: a lag must NOT share a discriminant with a
    // timeout, or a caller would mistake a dropped-window gap for "the event
    // never happened". Pin that the two tags are distinct.
    let timeout = serde_json::to_value(WaitResult::Timeout { elapsed_ms: 250 })
        .expect("WaitResult serializes");
    assert_eq!(timeout["outcome"], serde_json::json!("timeout"));
    assert_ne!(
        timeout["outcome"], value["outcome"],
        "Lagged and Timeout must be distinguishable outcomes"
    );
}

// ===========================================================================
// FilteredEventStream::next_item_cx surfaces an explicit gap on overflow.
// ===========================================================================

#[test]
fn filtered_stream_next_item_cx_surfaces_explicit_gap() {
    // Capacity 2, publish 12: a guaranteed overflow even if the underlying
    // broadcast rounds the requested capacity up.
    let outcome = drive_overflow_and_drain(2, 12);

    assert!(
        outcome.saw_gap,
        "overflow must surface an explicit StreamItem::Gap, not a silent drop"
    );
    assert!(
        outcome.first_is_gap,
        "the overflowed receiver must report the gap before any retained event"
    );
    assert!(outcome.gap_total > 0, "gap missed_count must be positive");

    // No silent message loss: every published event is either delivered as an
    // event or accounted for inside a gap.
    assert_eq!(
        outcome.delivered + outcome.gap_total,
        12,
        "events were lost without being accounted for in a gap"
    );
    // Overflow really happened — some events were dropped (and surfaced).
    assert!(
        outcome.delivered < 12,
        "expected the buffer to overflow and drop events"
    );
}

// ===========================================================================
// EventWaiter::wait_with_cx surfaces Lagged (not a false Timeout) when the
// awaited event was in the dropped window.
// ===========================================================================

#[test]
fn wait_with_cx_surfaces_lagged_not_timeout_when_awaited_event_dropped() {
    run_async(async {
        const CAPACITY: usize = 2;
        const FILLER: u64 = 16;

        let bus = EventBus::new(CAPACITY);
        let cx = for_request();

        // Wait for a specific pane to be discovered. We will drop that exact
        // event into the overflow window, so the waiter can never satisfy the
        // condition from the events the buffer retains.
        let waiter = EventWaiter::new(WaitCondition::pane_discovered(Some(777)))
            // A short, real timeout: in the FIXED build the waiter returns
            // immediately on the lag, so this never elapses. In a REGRESSED
            // build that silently swallows the lag and loops, this bounds the
            // test to ~2s and yields a Timeout (which the assertion rejects).
            .with_timeout(Duration::from_secs(2));

        let waiter_fut = waiter.wait_with_cx(&cx, &bus);

        let publish_fut = async {
            // Yield first so the waiter (polled first by `join!`) has
            // definitely subscribed and parked on its receive before we
            // publish — robust even if poll order were reversed.
            let _ =
                frankenterm_core::runtime_async::sleep_with_cx(&cx, Duration::from_millis(5)).await;

            // The awaited event, published FIRST so it lands in the window the
            // buffer will evict.
            let _ = bus.publish(Event::PaneDiscovered {
                pane_id: 777,
                domain: "local".to_string(),
                title: "awaited".to_string(),
            });
            // A flood of non-matching filler. The last CAPACITY retained events
            // are all `PaneDisappeared`, so the awaited `PaneDiscovered` is gone
            // and the only thing the waiter can observe is the lag itself.
            for i in 0..FILLER {
                let _ = bus.publish(Event::PaneDisappeared { pane_id: 1000 + i });
            }
        };

        // `join!` polls in argument order, so the waiter subscribes before the
        // publisher runs; the publisher's body has no awaits after its initial
        // yield, so all publishes land in one poll, after the waiter is parked.
        let (result, ()) = join!(waiter_fut, publish_fut);

        assert!(
            result.is_lagged(),
            "expected WaitResult::Lagged (explicit gap), got {result:?}"
        );
        assert!(
            !result.is_timeout(),
            "a dropped-window lag must NOT masquerade as a timeout"
        );
        assert!(
            !result.is_matched(),
            "the awaited event was dropped, not matched"
        );
        if let WaitResult::Lagged { missed_count, .. } = result {
            assert!(
                missed_count > 0,
                "Lagged must report how many events were missed, got {missed_count}"
            );
        }
    });
}

// ===========================================================================
// Property: an overflowed stream conserves every published event — each one is
// either delivered or accounted for in an explicit gap. No silent loss across
// a wide range of capacities and overflow magnitudes.
// ===========================================================================

proptest! {
    #![proptest_config(ProptestConfig { cases: 32, ..ProptestConfig::default() })]

    #[test]
    fn prop_overflow_conserves_every_event_via_explicit_gap(
        capacity in 1usize..6,
        // >= 8 extra events guarantees a real overflow even if the broadcast
        // rounds the small requested capacity up to the next power of two.
        overflow in 8u64..40,
    ) {
        let published = capacity as u64 + overflow;
        let outcome = drive_overflow_and_drain(capacity, published);

        // The gap was surfaced explicitly, never silently swallowed.
        prop_assert!(outcome.saw_gap, "overflow produced no explicit gap");
        prop_assert!(outcome.first_is_gap, "gap was not surfaced before retained events");
        prop_assert!(outcome.gap_total > 0, "gap reported a zero missed_count");

        // Conservation / no-silent-loss: delivered + gapped == published.
        prop_assert_eq!(
            outcome.delivered + outcome.gap_total,
            published,
            "events vanished without being counted in a gap"
        );
        // Some events were genuinely dropped (and accounted for).
        prop_assert!(outcome.delivered < published, "expected an overflow");
    }
}
