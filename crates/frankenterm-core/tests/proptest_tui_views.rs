//! Property-based tests for the TUI views module.
//!
//! Covers View enum circular navigation (next/prev), name consistency,
//! all() completeness, and default variant.
//!
//! Works with both ratatui and ftui View (rollout-aware).

#![cfg(any(feature = "tui", feature = "ftui"))]

use frankenterm_core::tui::View;
use proptest::prelude::*;

// ── Strategies ──────────────────────────────────────────────────────────────

fn arb_view() -> impl Strategy<Value = View> {
    prop_oneof![
        Just(View::Home),
        Just(View::Panes),
        Just(View::Events),
        Just(View::Triage),
        Just(View::History),
        Just(View::Search),
        Just(View::Help),
        Just(View::Timeline),
    ]
}

// ── View circular navigation ────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    // Property 1: next().prev() is identity (circular navigation roundtrip).
    #[test]
    fn view_next_prev_roundtrip(view in arb_view()) {
        let v: View = view;
        prop_assert_eq!(v.next().prev(), v);
    }

    // Property 2: prev().next() is identity.
    #[test]
    fn view_prev_next_roundtrip(view in arb_view()) {
        let v: View = view;
        prop_assert_eq!(v.prev().next(), v);
    }

    // Property 3: Applying next() N times (N = len) returns to start.
    #[test]
    fn view_next_full_cycle(view in arb_view()) {
        let n = View::all().len();
        let mut current: View = view;
        for _ in 0..n {
            current = current.next();
        }
        prop_assert_eq!(current, view);
    }

    // Property 4: Applying prev() N times (N = len) returns to start.
    #[test]
    fn view_prev_full_cycle(view in arb_view()) {
        let n = View::all().len();
        let mut current: View = view;
        for _ in 0..n {
            current = current.prev();
        }
        prop_assert_eq!(current, view);
    }

    // Property 5: name() is non-empty for every view.
    #[test]
    fn view_name_non_empty(view in arb_view()) {
        let v: View = view;
        let name = v.name();
        prop_assert!(!name.is_empty(), "view name should not be empty");
    }

    // Property 6: next() always produces a different view (no fixed points).
    #[test]
    fn view_next_different_from_self(view in arb_view()) {
        let v: View = view;
        prop_assert_ne!(v.next(), v);
    }

    // Property 7: prev() always produces a different view.
    #[test]
    fn view_prev_different_from_self(view in arb_view()) {
        let v: View = view;
        prop_assert_ne!(v.prev(), v);
    }

    // Property 8: next() result is always a member of all().
    #[test]
    fn view_next_in_all(view in arb_view()) {
        let v: View = view;
        let all = View::all();
        let found = all.contains(&v.next());
        prop_assert!(found, "next() produced view not in all()");
    }

    // Property 9: prev() result is always a member of all().
    #[test]
    fn view_prev_in_all(view in arb_view()) {
        let v: View = view;
        let all = View::all();
        let found = all.contains(&v.prev());
        prop_assert!(found, "prev() produced view not in all()");
    }

    // Property 10: N-1 applications of next() yields prev() of original.
    #[test]
    fn view_n_minus_1_next_equals_prev(view in arb_view()) {
        let v: View = view;
        let n = View::all().len();
        let mut current: View = v;
        for _ in 0..n - 1 {
            current = current.next();
        }
        prop_assert_eq!(current, v.prev());
    }

    // Property 11: N-1 applications of prev() yields next() of original.
    #[test]
    fn view_n_minus_1_prev_equals_next(view in arb_view()) {
        let v: View = view;
        let n = View::all().len();
        let mut current: View = v;
        for _ in 0..n - 1 {
            current = current.prev();
        }
        prop_assert_eq!(current, v.next());
    }

    // Property 12: Arbitrary number of next() calls stays in all().
    #[test]
    fn view_next_stays_in_all(view in arb_view(), steps in 0usize..50) {
        let v: View = view;
        let all = View::all();
        let mut current: View = v;
        for _ in 0..steps {
            current = current.next();
        }
        let found = all.contains(&current);
        prop_assert!(found, "after {} next() calls, got unknown view", steps);
    }

    // Property 13: Two views are equal IFF they have the same name.
    #[test]
    fn view_eq_iff_same_name(a in arb_view(), b in arb_view()) {
        let va: View = a;
        let vb: View = b;
        if va == vb {
            prop_assert_eq!(va.name(), vb.name());
        } else {
            let check = va.name() != vb.name();
            prop_assert!(check, "different views should have different names");
        }
    }

    // Property 14: Copy semantics — view equals its copy.
    #[test]
    fn view_copy_eq(view in arb_view()) {
        let v: View = view;
        let copy = v;
        prop_assert_eq!(v, copy);
    }
}

// ── View::all() properties ──────────────────────────────────────────────────

#[test]
fn view_all_covers_every_variant() {
    let views = View::all();
    assert_eq!(views.len(), 8);
    let mut seen = std::collections::HashSet::new();
    for v in views {
        assert!(seen.insert(v.name()), "duplicate view: {}", v.name());
    }
}

#[test]
fn view_all_names_unique() {
    let views = View::all();
    let names: Vec<&str> = views.iter().map(|v: &View| v.name()).collect();
    let unique: std::collections::HashSet<&str> = names.iter().copied().collect();
    assert_eq!(names.len(), unique.len(), "view names must be unique");
}

#[test]
fn view_default_is_home() {
    assert_eq!(View::default(), View::Home);
}

#[test]
fn view_all_starts_with_default() {
    let views = View::all();
    assert_eq!(views[0], View::default());
}
