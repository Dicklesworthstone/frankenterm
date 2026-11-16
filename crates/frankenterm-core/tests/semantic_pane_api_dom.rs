//! W1.T (ft-7h5da.2.7) semantic-pane-API tests — standalone integration file,
//! PUBLIC API only, no source edits.
//!
//! Covers the W1.1 (closed) Terminal Semantic Pane API surface — the pure
//! `robot_dom` builder that maps an OSC-133 `MuxSemanticSnapshot` to the
//! `ft robot dom` / `wa.dom` `DomData` envelope — which is unit-testable without
//! a live pane:
//!   * SemanticZone projection: prompt/input/output kind mapping + boundary
//!     (start/end x/y) passthrough, with zone text redacted;
//!   * `build_dom_data` happy path: source=osc133, confidence=1.0, zones
//!     projected; LastCommand / ExitCode / OutputOf populate their fields;
//!   * the HONESTY guard: a pane with no OSC-133 prompt/input markers (or a
//!     missing exit code) yields an explicit `semantic_data_unavailable`
//!     envelope (source="unavailable", no fabricated command/output/exit_code) —
//!     never a heuristic guess;
//!   * `dom_unavailable` / `dom_alt_screen_unavailable` envelope shape +
//!     redacted reason;
//!   * `resolve_dom_command_index` (-1 = most recent; in-range else None);
//!   * DomData serde round-trip.
//!
//! Authored zero-RCH (the dep-upgrade meta-blocker prevents compiling
//! frankenterm-core; the bead claim is refused since ft-7h5da.2.7 is blocked-by
//! W1.2..W1.6). The agent-driven e2e (tests/e2e/semantic_pane_api.sh: real
//! OSC-133 shell pane, alt-screen grid queries, spawn-path shell integration +
//! doctor availability, zone-scoped detection killing quoted-error FPs) needs
//! live panes + W1.2/1.4/1.5/1.6 and is the gated remainder.

use frankenterm_core::redactor::Redactor;
use frankenterm_core::robot_dom::{
    build_dom_data, dom_alt_screen_unavailable, dom_unavailable, dom_zone_from_mux,
    resolve_dom_command_index,
};
use frankenterm_core::robot_types::{DomQueryKind, DomSemanticZoneKind};
use frankenterm_core::wezterm::{MuxSemanticSnapshot, MuxSemanticZone, MuxSemanticZoneKind};

/// A GitLab PAT-shaped secret the live catalog redacts (for redaction asserts).
const SECRET: &str = "glpat-ABCDEFGHIJKLMNOPQRST";

fn zone(kind: MuxSemanticZoneKind, start_y: isize, text: &str) -> MuxSemanticZone {
    MuxSemanticZone {
        start_y,
        start_x: 0,
        end_y: start_y,
        end_x: text.len(),
        semantic_type: kind,
        text: text.to_string(),
    }
}

/// A prompt+input+output snapshot with an exit code — a complete OSC-133 pane.
fn complete_snapshot() -> MuxSemanticSnapshot {
    MuxSemanticSnapshot {
        zones: vec![
            zone(MuxSemanticZoneKind::Prompt, 0, "$ "),
            zone(MuxSemanticZoneKind::Input, 0, "echo hi"),
            zone(MuxSemanticZoneKind::Output, 1, "hi"),
        ],
        last_exit_code: Some(0),
    }
}

#[test]
fn resolve_dom_command_index_selects_last_and_bounds() {
    assert_eq!(
        resolve_dom_command_index(0, -1),
        None,
        "no commands => None"
    );
    assert_eq!(resolve_dom_command_index(0, 0), None);
    assert_eq!(
        resolve_dom_command_index(3, -1),
        Some(2),
        "-1 selects most recent"
    );
    assert_eq!(resolve_dom_command_index(3, 0), Some(0));
    assert_eq!(resolve_dom_command_index(3, 2), Some(2));
    assert_eq!(
        resolve_dom_command_index(3, 3),
        None,
        "out of range => None"
    );
    assert_eq!(
        resolve_dom_command_index(3, -2),
        None,
        "only -1 is special; other negatives => None"
    );
}

#[test]
fn dom_zone_from_mux_maps_kinds_boundaries_and_redacts() {
    let redactor = Redactor::new();
    for (mux_kind, dom_kind) in [
        (MuxSemanticZoneKind::Prompt, DomSemanticZoneKind::Prompt),
        (MuxSemanticZoneKind::Input, DomSemanticZoneKind::Input),
        (MuxSemanticZoneKind::Output, DomSemanticZoneKind::Output),
    ] {
        let label = format!("{mux_kind:?}");
        let mz = MuxSemanticZone {
            start_y: 3,
            start_x: 1,
            end_y: 4,
            end_x: 9,
            semantic_type: mux_kind,
            text: format!("token {SECRET} end"),
        };
        let dz = dom_zone_from_mux(&mz, &redactor);
        assert_eq!(dz.semantic_type, dom_kind, "zone kind mapping for {label}");
        // Boundaries pass through unchanged.
        assert_eq!((dz.start_y, dz.start_x, dz.end_y, dz.end_x), (3, 1, 4, 9));
        // Zone text is redacted (the secret never leaves the builder).
        assert!(
            !dz.text.contains(SECRET),
            "zone text must be redacted: {}",
            dz.text
        );
    }
}

#[test]
fn build_dom_data_zones_happy_path_is_osc133() {
    let redactor = Redactor::new();
    let data = build_dom_data(
        1,
        DomQueryKind::Zones,
        None,
        &complete_snapshot(),
        &redactor,
    );
    assert_eq!(data.source, "osc133");
    assert!(!data.semantic_data_unavailable);
    assert!((data.confidence - 1.0).abs() < f64::EPSILON);
    assert_eq!(data.zones.len(), 3);
    assert_eq!(data.zones[0].semantic_type, DomSemanticZoneKind::Prompt);
    assert_eq!(data.zones[1].semantic_type, DomSemanticZoneKind::Input);
    assert_eq!(data.zones[2].semantic_type, DomSemanticZoneKind::Output);
    // Zones query selects no specific command/output/exit-code.
    assert!(data.command.is_none() && data.output.is_none() && data.exit_code.is_none());
}

#[test]
fn build_dom_data_last_command_and_exit_code_populate() {
    let redactor = Redactor::new();
    let snap = complete_snapshot();

    let last = build_dom_data(1, DomQueryKind::LastCommand, None, &snap, &redactor);
    assert_eq!(last.source, "osc133");
    assert!(!last.semantic_data_unavailable);
    assert!(
        last.command.is_some(),
        "LastCommand must populate the command field"
    );

    let exit = build_dom_data(1, DomQueryKind::ExitCode, None, &snap, &redactor);
    assert!(!exit.semantic_data_unavailable);
    assert!(
        exit.exit_code.is_some(),
        "ExitCode must populate from snapshot.last_exit_code"
    );
}

#[test]
fn build_dom_data_missing_exit_code_is_honestly_unavailable() {
    let redactor = Redactor::new();
    let mut snap = complete_snapshot();
    snap.last_exit_code = None; // command ran but no OSC 133;D exit code captured.
    let data = build_dom_data(1, DomQueryKind::ExitCode, None, &snap, &redactor);
    assert!(
        data.semantic_data_unavailable,
        "missing exit code must be typed-unavailable, not a guess"
    );
    assert!(data.exit_code.is_none());
}

#[test]
fn build_dom_data_without_osc133_markers_never_guesses() {
    let redactor = Redactor::new();
    // A pane with only output (no prompt/input markers) — e.g. a raw stream.
    let output_only = MuxSemanticSnapshot {
        zones: vec![zone(
            MuxSemanticZoneKind::Output,
            0,
            "raw output with no shell integration",
        )],
        last_exit_code: None,
    };
    let empty = MuxSemanticSnapshot {
        zones: vec![],
        last_exit_code: None,
    };

    for (label, snap) in [("output-only", &output_only), ("empty", &empty)] {
        for query in [
            DomQueryKind::Zones,
            DomQueryKind::LastCommand,
            DomQueryKind::OutputOf,
            DomQueryKind::ExitCode,
        ] {
            let data = build_dom_data(1, query, Some(-1), snap, &redactor);
            assert!(
                data.semantic_data_unavailable,
                "{label} pane must be typed-unavailable (never a guess)",
            );
            assert_eq!(data.source, "unavailable");
            assert!(data.command.is_none() && data.output.is_none() && data.exit_code.is_none());
        }
    }
}

#[test]
fn unavailable_envelopes_are_honest_and_redacted() {
    let redactor = Redactor::new();
    let reason = format!("pane gone; last token {SECRET}");

    let u = dom_unavailable(
        5,
        DomQueryKind::Zones,
        None,
        Vec::new(),
        reason.clone(),
        &redactor,
    );
    assert!(u.semantic_data_unavailable);
    assert_eq!(u.source, "unavailable");
    assert!((u.confidence - 0.0).abs() < f64::EPSILON);
    assert!(u.command.is_none() && u.output.is_none() && u.exit_code.is_none());
    // The unavailable reason is redacted (no secret leaks through the error path).
    let reason_text = u.unavailable_reason.clone().unwrap_or_default();
    assert!(
        !reason_text.contains(SECRET),
        "unavailable reason must be redacted: {reason_text}"
    );

    let alt = dom_alt_screen_unavailable(5, DomQueryKind::LastCommand, Some(-1), &redactor);
    assert!(alt.semantic_data_unavailable);
    assert_eq!(alt.source, "unavailable");
}

#[test]
fn dom_data_serde_round_trips() {
    let redactor = Redactor::new();
    let data = build_dom_data(
        1,
        DomQueryKind::Zones,
        None,
        &complete_snapshot(),
        &redactor,
    );
    let first = serde_json::to_value(&data).expect("serialize DomData");
    let back: frankenterm_core::robot_types::DomData =
        serde_json::from_value(first.clone()).expect("deserialize DomData");
    let second = serde_json::to_value(&back).expect("re-serialize DomData");
    assert_eq!(first, second, "DomData JSON round-trip must be stable");
    assert_eq!(first["source"], serde_json::json!("osc133"));
    assert_eq!(first["semantic_data_unavailable"], serde_json::json!(false));
}
