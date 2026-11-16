//! W3.T net-new conformance: composite `await` any/all semantics (ft-7h5da.4.5).
//!
//! `ft robot await --any/--all` composes wait conditions over rule hits, pane
//! lifecycle, and quiescence (workflow completion). The existing coverage pins
//! the leaves (proptest_event_stream), the basic AllOfTracker over rule_id +
//! pane_detection (in-module event_stream tests), and the full binary flow
//! (robot_watch_await_e2e). What no INTEGRATION test pins is the composite
//! combinator itself over the no-Detection event types — pane-state + workflow
//! — exercised through the public API:
//!   * AnyOf is OR over leaves (resolves on the first matching event),
//!   * AllOf is never satisfied by a single event at the match level (that would
//!     be a false-positive AND) — it requires the stateful AllOfTracker,
//!   * AllOfTracker is cumulative across events, order-independent, lets one
//!     event clear every condition it matches, and is vacuously complete when
//!     empty.
//!
//! Zero-RCH, public API only, no source edits. The IPC-dependent W3.T e2e items
//! (watcher-up real-time delivery, kill-watcher DB-cursor fallback,
//! bounded-broadcast-lag under a real socket) and the bin-side max_hz /
//! cursor_expired surfaces ride .4.1/.4.3/.4.4 and are out of scope here.

use frankenterm_core::event_stream::{AllOfTracker, WaitCondition};
use frankenterm_core::events::Event;

fn pane_up(pane_id: u64) -> Event {
    Event::PaneDiscovered {
        pane_id,
        domain: "local".to_string(),
        title: "shell".to_string(),
    }
}

fn pane_gone(pane_id: u64) -> Event {
    Event::PaneDisappeared { pane_id }
}

fn workflow_done(id: &str) -> Event {
    Event::WorkflowCompleted {
        workflow_id: id.to_string(),
        success: true,
        reason: None,
    }
}

/// `await --any`: AnyOf resolves when an event matches ANY leaf (OR), and does
/// not resolve when no leaf matches.
#[test]
fn any_of_resolves_on_any_matching_leaf() {
    let cond = WaitCondition::AnyOf {
        conditions: vec![
            WaitCondition::pane_discovered(Some(1)),
            WaitCondition::workflow_completed(Some("deploy".to_string())),
        ],
    };
    assert!(cond.matches(&pane_up(1)), "pane-state leaf satisfies AnyOf");
    assert!(
        cond.matches(&workflow_done("deploy")),
        "quiescence leaf satisfies AnyOf"
    );
    assert!(
        !cond.matches(&pane_gone(9)),
        "no leaf matches => AnyOf must not resolve"
    );
    assert!(
        !cond.matches(&workflow_done("other")),
        "a different workflow id must not match"
    );
}

/// `await --all`: AllOf is NEVER satisfied by a single event at the per-event
/// match level — that would be a false-positive AND. The AND must be evaluated
/// cumulatively via AllOfTracker, so `matches()` always returns false here.
#[test]
fn all_of_is_never_satisfied_by_a_single_event() {
    let cond = WaitCondition::AllOf {
        conditions: vec![
            WaitCondition::pane_discovered(Some(1)),
            WaitCondition::workflow_completed(None),
        ],
    };
    // Even an event that satisfies one leaf must not satisfy the whole AllOf.
    assert!(!cond.matches(&pane_up(1)));
    assert!(!cond.matches(&workflow_done("x")));
    assert!(!cond.matches(&pane_gone(2)));
}

/// AllOfTracker completes only after EVERY condition has been satisfied by some
/// event (mixed pane-state + quiescence), and non-matching noise never advances.
#[test]
fn all_of_tracker_completes_only_after_every_condition_across_events() {
    let mut tracker = AllOfTracker::new(vec![
        WaitCondition::pane_discovered(Some(1)),
        WaitCondition::workflow_completed(Some("deploy".to_string())),
        WaitCondition::PaneDisappeared { pane_id: 2 },
    ]);
    assert_eq!(tracker.remaining_count(), 3);
    assert!(!tracker.is_complete());

    // Noise: an event matching no remaining condition must not advance.
    assert!(!tracker.check(&pane_gone(99)));
    assert_eq!(tracker.remaining_count(), 3);

    assert!(!tracker.check(&pane_up(1)));
    assert_eq!(tracker.remaining_count(), 2);
    assert!(!tracker.check(&workflow_done("deploy")));
    assert_eq!(tracker.remaining_count(), 1);
    assert!(
        tracker.check(&pane_gone(2)),
        "the event completing the final condition reports complete"
    );
    assert!(tracker.is_complete());
    assert_eq!(tracker.remaining_count(), 0);
}

/// AllOfTracker is order-independent: the same condition set completes whether
/// the satisfying events arrive in declaration order or reversed.
#[test]
fn all_of_tracker_is_order_independent() {
    let conditions = || {
        vec![
            WaitCondition::pane_discovered(Some(1)),
            WaitCondition::workflow_completed(Some("deploy".to_string())),
        ]
    };

    let mut forward = AllOfTracker::new(conditions());
    assert!(!forward.check(&pane_up(1)));
    assert!(forward.check(&workflow_done("deploy")));
    assert!(forward.is_complete());

    let mut reverse = AllOfTracker::new(conditions());
    assert!(!reverse.check(&workflow_done("deploy")));
    assert!(reverse.check(&pane_up(1)));
    assert!(reverse.is_complete());
}

/// A single event clears EVERY remaining condition it matches in one step
/// (here a wildcard `PaneDiscovered(None)` and an exact `PaneDiscovered(5)` are
/// both cleared by one `PaneDiscovered{5}`).
#[test]
fn all_of_tracker_single_event_clears_all_conditions_it_matches() {
    let mut tracker = AllOfTracker::new(vec![
        WaitCondition::pane_discovered(None),
        WaitCondition::pane_discovered(Some(5)),
    ]);
    assert_eq!(tracker.remaining_count(), 2);
    assert!(tracker.check(&pane_up(5)));
    assert!(tracker.is_complete());
    assert_eq!(tracker.remaining_count(), 0);
}

/// An empty AllOf is vacuously complete — it must never block forever waiting
/// for conditions that do not exist.
#[test]
fn all_of_tracker_empty_is_vacuously_complete() {
    let mut tracker = AllOfTracker::new(vec![]);
    assert!(tracker.is_complete(), "empty AllOf is vacuously satisfied");
    assert_eq!(tracker.remaining_count(), 0);
    assert!(
        tracker.check(&pane_up(1)),
        "first check on an empty tracker reports complete"
    );
}
