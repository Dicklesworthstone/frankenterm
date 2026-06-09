//! Shared builder for the semantic-pane API envelope (`ft robot dom` /
//! `wa.dom`) — ft-7h5da.2.1 / .2.6.
//!
//! Maps a [`MuxSemanticSnapshot`] of OSC 133 zones to a [`DomData`] envelope for
//! a given [`DomQueryKind`], so the robot CLI and the MCP tool produce
//! **byte-equal** envelopes from one implementation. The full contract (honest
//! naming: a flat zone list, NOT a DOM tree) lives at
//! `docs/robot-contracts/semantic-pane-api.md`.
//!
//! This module is pure: given the snapshot + a [`Redactor`] it computes the
//! envelope with no I/O, which makes the verb→envelope mapping unit-testable
//! (it previously lived inline in the CLI binary and had no test coverage).

use crate::redactor::Redactor;
use crate::robot_types::{
    DomCommandData, DomCommandOutputData, DomData, DomExitCodeData, DomQueryKind, DomSemanticZone,
    DomSemanticZoneKind,
};
use crate::wezterm::{MuxSemanticSnapshot, MuxSemanticZone, MuxSemanticZoneKind};

/// Convert a mux semantic zone into the robot envelope zone, redacting text.
#[must_use]
pub fn dom_zone_from_mux(zone: &MuxSemanticZone, redactor: &Redactor) -> DomSemanticZone {
    let semantic_type = match zone.semantic_type {
        MuxSemanticZoneKind::Prompt => DomSemanticZoneKind::Prompt,
        MuxSemanticZoneKind::Input => DomSemanticZoneKind::Input,
        MuxSemanticZoneKind::Output => DomSemanticZoneKind::Output,
    };
    DomSemanticZone {
        start_y: zone.start_y,
        start_x: zone.start_x,
        end_y: zone.end_y,
        end_x: zone.end_x,
        semantic_type,
        text: redactor.redact(&zone.text),
    }
}

/// Build an honest "semantic data unavailable" envelope — never a fabricated
/// structure. Shared with the CLI dispatch path (pane-not-found / remote pane).
pub fn dom_unavailable(
    pane_id: u64,
    query: DomQueryKind,
    requested_command_index: Option<i64>,
    zones: Vec<DomSemanticZone>,
    reason: impl Into<String>,
    redactor: &Redactor,
) -> DomData {
    DomData {
        pane_id,
        query,
        source: "unavailable".to_string(),
        confidence: 0.0,
        semantic_data_unavailable: true,
        unavailable_reason: Some(redactor.redact(&reason.into())),
        requested_command_index,
        zones,
        command: None,
        output: None,
        exit_code: None,
    }
}

/// Resolve a requested command index against the count of input (command) zones.
/// `-1` selects the most recent command; otherwise the index must be in range.
#[must_use]
pub fn resolve_dom_command_index(input_count: usize, requested: i64) -> Option<usize> {
    if input_count == 0 {
        return None;
    }
    if requested == -1 {
        return input_count.checked_sub(1);
    }
    usize::try_from(requested)
        .ok()
        .filter(|index| *index < input_count)
}

/// Build the `DomData` envelope for `query` from a semantic-zone `snapshot`.
///
/// Returns an honest `semantic_data_unavailable` envelope (never a fabricated
/// structure) when the pane has no OSC 133 markers or the requested selection
/// is out of range / unavailable.
#[must_use]
pub fn build_dom_data(
    pane_id: u64,
    query: DomQueryKind,
    requested_command_index: Option<i64>,
    snapshot: &MuxSemanticSnapshot,
    redactor: &Redactor,
) -> DomData {
    let zones: Vec<DomSemanticZone> = snapshot
        .zones
        .iter()
        .map(|z| dom_zone_from_mux(z, redactor))
        .collect();
    let has_osc133_markers = zones.iter().any(|zone| {
        matches!(
            zone.semantic_type,
            DomSemanticZoneKind::Prompt | DomSemanticZoneKind::Input
        )
    });

    if !has_osc133_markers {
        return dom_unavailable(
            pane_id,
            query,
            requested_command_index,
            Vec::new(),
            "semantic data unavailable: pane has no OSC 133 prompt/input markers",
            redactor,
        );
    }

    // (zone_index, command_index) for each Input zone, command_index dense 0..N.
    let input_positions: Vec<(usize, i64)> = zones
        .iter()
        .enumerate()
        .filter(|(_, zone)| zone.semantic_type == DomSemanticZoneKind::Input)
        .enumerate()
        .map(|(command_index, (zone_index, _))| (zone_index, command_index as i64))
        .collect();

    let mut data = DomData {
        pane_id,
        query,
        source: "osc133".to_string(),
        confidence: 1.0,
        semantic_data_unavailable: false,
        unavailable_reason: None,
        requested_command_index,
        zones: zones.clone(),
        command: None,
        output: None,
        exit_code: None,
    };

    match query {
        DomQueryKind::Zones => data,
        DomQueryKind::LastCommand => {
            let Some((zone_index, command_index)) = input_positions.last().copied() else {
                return dom_unavailable(
                    pane_id,
                    query,
                    requested_command_index,
                    zones,
                    "semantic data unavailable: OSC 133 markers contain no command input zone",
                    redactor,
                );
            };
            let zone = zones[zone_index].clone();
            data.command = Some(DomCommandData {
                command_index,
                text: zone.text.clone(),
                zone,
            });
            data
        }
        DomQueryKind::OutputOf => {
            let requested = requested_command_index.unwrap_or(-1);
            let Some(selected_input_index) =
                resolve_dom_command_index(input_positions.len(), requested)
            else {
                return dom_unavailable(
                    pane_id,
                    query,
                    requested_command_index,
                    zones,
                    format!("semantic data unavailable: command index {requested} is out of range"),
                    redactor,
                );
            };
            let (input_zone_index, command_index) = input_positions[selected_input_index];
            let input_zone = zones[input_zone_index].clone();
            data.command = Some(DomCommandData {
                command_index,
                text: input_zone.text.clone(),
                zone: input_zone,
            });
            let output_zone = zones
                .iter()
                .skip(input_zone_index + 1)
                .take_while(|zone| zone.semantic_type != DomSemanticZoneKind::Input)
                .find(|zone| zone.semantic_type == DomSemanticZoneKind::Output)
                .cloned();
            let Some(zone) = output_zone else {
                return dom_unavailable(
                    pane_id,
                    query,
                    requested_command_index,
                    zones,
                    format!(
                        "semantic data unavailable: command index {command_index} has no output zone"
                    ),
                    redactor,
                );
            };
            data.output = Some(DomCommandOutputData {
                command_index,
                text: zone.text.clone(),
                zone,
            });
            data
        }
        DomQueryKind::ExitCode => {
            let requested = requested_command_index.unwrap_or(-1);
            let selected_input_index = resolve_dom_command_index(input_positions.len(), requested);
            let command_index = match selected_input_index {
                Some(index) => input_positions[index].1,
                None if requested == -1 => -1,
                None => {
                    return dom_unavailable(
                        pane_id,
                        query,
                        requested_command_index,
                        zones,
                        format!(
                            "semantic data unavailable: command index {requested} is out of range"
                        ),
                        redactor,
                    );
                }
            };
            if requested != -1 && selected_input_index != input_positions.len().checked_sub(1) {
                return dom_unavailable(
                    pane_id,
                    query,
                    requested_command_index,
                    zones,
                    "semantic data unavailable: only the latest OSC 133 exit status is retained",
                    redactor,
                );
            }
            let Some(code) = snapshot.last_exit_code else {
                return dom_unavailable(
                    pane_id,
                    query,
                    requested_command_index,
                    zones,
                    "semantic data unavailable: pane has no retained OSC 133 exit status",
                    redactor,
                );
            };
            data.exit_code = Some(DomExitCodeData {
                command_index,
                code,
            });
            data
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn zone(kind: MuxSemanticZoneKind, y: isize, text: &str) -> MuxSemanticZone {
        MuxSemanticZone {
            start_y: y,
            start_x: 0,
            end_y: y,
            end_x: text.len(),
            semantic_type: kind,
            text: text.to_string(),
        }
    }

    /// prompt, input "ls", output "a b", prompt, input "echo hi", output "hi"
    fn two_command_snapshot(last_exit: Option<i32>) -> MuxSemanticSnapshot {
        MuxSemanticSnapshot {
            zones: vec![
                zone(MuxSemanticZoneKind::Prompt, 0, "$ "),
                zone(MuxSemanticZoneKind::Input, 0, "ls"),
                zone(MuxSemanticZoneKind::Output, 1, "a b"),
                zone(MuxSemanticZoneKind::Prompt, 2, "$ "),
                zone(MuxSemanticZoneKind::Input, 2, "echo hi"),
                zone(MuxSemanticZoneKind::Output, 3, "hi"),
            ],
            last_exit_code: last_exit,
        }
    }

    #[test]
    fn zones_query_returns_flat_list_not_tree() {
        let r = Redactor::new();
        let d = build_dom_data(7, DomQueryKind::Zones, None, &two_command_snapshot(Some(0)), &r);
        assert_eq!(d.pane_id, 7);
        assert_eq!(d.source, "osc133");
        assert!(!d.semantic_data_unavailable);
        assert_eq!(d.zones.len(), 6);
        assert!(d.command.is_none() && d.output.is_none() && d.exit_code.is_none());
    }

    #[test]
    fn last_command_picks_most_recent_input() {
        let r = Redactor::new();
        let d = build_dom_data(0, DomQueryKind::LastCommand, None, &two_command_snapshot(None), &r);
        let cmd = d.command.expect("command");
        assert_eq!(cmd.command_index, 1);
        assert_eq!(cmd.text, "echo hi");
    }

    #[test]
    fn output_of_index_selects_matching_output_zone() {
        let r = Redactor::new();
        let d = build_dom_data(0, DomQueryKind::OutputOf, Some(0), &two_command_snapshot(None), &r);
        let out = d.output.expect("output");
        assert_eq!(out.command_index, 0);
        assert_eq!(out.text, "a b");
    }

    #[test]
    fn output_of_out_of_range_is_unavailable_not_error() {
        let r = Redactor::new();
        let d = build_dom_data(0, DomQueryKind::OutputOf, Some(99), &two_command_snapshot(None), &r);
        assert!(d.semantic_data_unavailable);
        assert_eq!(d.source, "unavailable");
        assert!(d.output.is_none());
    }

    #[test]
    fn exit_code_returns_latest_retained_status() {
        let r = Redactor::new();
        let d = build_dom_data(0, DomQueryKind::ExitCode, None, &two_command_snapshot(Some(3)), &r);
        let ec = d.exit_code.expect("exit_code");
        assert_eq!(ec.code, 3);
    }

    #[test]
    fn exit_code_unavailable_when_no_status_retained() {
        let r = Redactor::new();
        let d = build_dom_data(0, DomQueryKind::ExitCode, None, &two_command_snapshot(None), &r);
        assert!(d.semantic_data_unavailable);
        assert!(d.exit_code.is_none());
    }

    #[test]
    fn no_osc133_markers_is_unavailable_with_empty_zones() {
        let r = Redactor::new();
        let snap = MuxSemanticSnapshot {
            zones: vec![zone(MuxSemanticZoneKind::Output, 0, "plain output")],
            last_exit_code: None,
        };
        let d = build_dom_data(0, DomQueryKind::Zones, None, &snap, &r);
        assert!(d.semantic_data_unavailable);
        assert_eq!(d.source, "unavailable");
        assert!(d.zones.is_empty());
        assert!(d.unavailable_reason.is_some());
    }

    #[test]
    fn zone_text_is_redacted() {
        let r = Redactor::new();
        let snap = MuxSemanticSnapshot {
            zones: vec![
                zone(MuxSemanticZoneKind::Prompt, 0, "$ "),
                zone(MuxSemanticZoneKind::Input, 0, "export K=AKIAIOSFODNN7EXAMPLE"),
            ],
            last_exit_code: None,
        };
        let d = build_dom_data(0, DomQueryKind::LastCommand, None, &snap, &r);
        let cmd = d.command.expect("command");
        assert!(!cmd.text.contains("AKIAIOSFODNN7EXAMPLE"), "secret must be redacted in zone text");
    }
}
