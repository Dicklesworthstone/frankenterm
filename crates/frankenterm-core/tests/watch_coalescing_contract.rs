// =============================================================================
// Coalescing contract for the `runtime_async::watch` channel.
//
// A watch channel is single-value and *coalescing*: it retains only the most
// recent value, so a burst of sends collapses to the latest and every receiver
// converges on it (last-write-wins, O(1) buffer). The live suite covers the
// 2-send "sees latest" case; these pin the coalescing/bounded-buffer property
// at burst scale, the multi-receiver convergence, and last-write-wins ordering
// (it tracks the most recent send, not a max/accumulation).
//
// `watch::channel`, `Sender::send`, `Receiver::borrow`, and `Receiver::clone`
// are synchronous, so no runtime/feature gate is required — this proves under
// the default `cargo test -p frankenterm-core` set.
// =============================================================================

use frankenterm_core::runtime_async::watch;

/// A burst of sends with no interleaved read collapses to the latest value —
/// the channel coalesces (O(1) retained value), it does not buffer the burst.
#[test]
fn watch_coalesces_burst_to_latest() {
    let (tx, rx) = watch::channel(0u64);
    for v in 1..=1000u64 {
        tx.send(v).expect("watch send should succeed");
    }
    assert_eq!(
        *rx.borrow(),
        1000,
        "1000 rapid sends must coalesce to the latest value"
    );
}

/// Every receiver (original + clones) converges on the coalesced latest after
/// a burst — fan-out is consistent, not per-receiver-buffered.
#[test]
fn watch_all_receivers_observe_coalesced_latest() {
    let (tx, rx1) = watch::channel(0u64);
    let rx2 = rx1.clone();
    let rx3 = rx1.clone();

    for v in 1..=500u64 {
        tx.send(v).expect("watch send should succeed");
    }

    assert_eq!(*rx1.borrow(), 500, "rx1 must see the coalesced latest");
    assert_eq!(*rx2.borrow(), 500, "rx2 must see the coalesced latest");
    assert_eq!(*rx3.borrow(), 500, "rx3 must see the coalesced latest");
}

/// `borrow` reflects the most recent committed send — last-write-wins, not a
/// max or accumulation. A decreasing send is still the value observed.
#[test]
fn watch_borrow_tracks_most_recent_send_not_max() {
    let (tx, rx) = watch::channel(0u64);

    tx.send(10).expect("send 10");
    assert_eq!(*rx.borrow(), 10);

    tx.send(20).expect("send 20");
    assert_eq!(*rx.borrow(), 20);

    // A smaller value must still win — coalescing is last-write, not max.
    tx.send(5).expect("send 5");
    assert_eq!(
        *rx.borrow(),
        5,
        "watch must reflect the most recent send, even when it decreases"
    );
}

/// A receiver created after a burst immediately observes the coalesced latest
/// (the retained value), never a stale intermediate.
#[test]
fn watch_late_receiver_sees_coalesced_latest() {
    let (tx, rx1) = watch::channel(0u64);
    for v in 1..=250u64 {
        tx.send(v).expect("watch send should succeed");
    }
    // Subscribe after the burst.
    let rx_late = rx1.clone();
    assert_eq!(
        *rx_late.borrow(),
        250,
        "a receiver cloned after the burst must see the coalesced latest"
    );
}
