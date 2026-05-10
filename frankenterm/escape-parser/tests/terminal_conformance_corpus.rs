//! Parser-level consumer for the terminal conformance transcript corpus.
//!
//! This is intentionally a parser test, not a no-mock mux harness. The manifest
//! records the deferred no-mock beads for rows that need PTY, mux, or render
//! state proof beyond parser-visible actions.

use frankenterm_escape_parser::Action;
use frankenterm_escape_parser::parser::Parser;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

const MIN_SCENARIOS: usize = 6;

type TestResult<T = ()> = Result<T, String>;

fn fixture_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/terminal-conformance")
}

fn load_json(path: &Path) -> TestResult<Value> {
    let text = std::fs::read_to_string(path)
        .map_err(|err| format!("failed to read {}: {err}", path.display()))?;
    serde_json::from_str(&text)
        .map_err(|err| format!("failed to parse JSON {}: {err}", path.display()))
}

fn string_field<'a>(scenario_id: &str, value: &'a Value, key: &str) -> TestResult<&'a str> {
    value
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("{scenario_id}: missing string field {key}"))
}

fn optional_string_array<'a>(
    scenario_id: &str,
    value: &'a Value,
    key: &str,
) -> TestResult<Vec<&'a str>> {
    let Some(items) = value.get(key) else {
        return Ok(Vec::new());
    };
    let items = items
        .as_array()
        .ok_or_else(|| format!("{scenario_id}: {key} must be an array"))?;
    items
        .iter()
        .map(|item| {
            item.as_str()
                .ok_or_else(|| format!("{scenario_id}: {key} contains a non-string item"))
        })
        .collect()
}

fn relative_artifact_path(scenario_id: &str, root: &Path, rel: &str) -> TestResult<PathBuf> {
    let rel_path = Path::new(rel);
    let escapes_root = rel_path
        .components()
        .any(|component| matches!(component, std::path::Component::ParentDir));
    if !rel_path.is_relative() || escapes_root {
        return Err(format!(
            "{scenario_id}: artifact path must stay fixture-root relative: {rel}"
        ));
    }
    Ok(root.join(rel_path))
}

fn decode_hex(scenario_id: &str, path: &Path) -> TestResult<Vec<u8>> {
    let hex = std::fs::read_to_string(path)
        .map_err(|err| format!("{scenario_id}: failed to read {}: {err}", path.display()))?;
    decode_hex_text(scenario_id, &hex, &path.display().to_string())
}

fn hex_nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn decode_hex_text(scenario_id: &str, hex: &str, label: &str) -> TestResult<Vec<u8>> {
    let clean = hex
        .bytes()
        .filter(|byte| !byte.is_ascii_whitespace())
        .collect::<Vec<_>>();
    if clean.len() % 2 != 0 {
        return Err(format!("{scenario_id}: odd-length hex in {label}"));
    }

    let mut out = Vec::with_capacity(clean.len() / 2);
    for (idx, chunk) in clean.chunks_exact(2).enumerate() {
        let [hi_byte, lo_byte] = chunk else {
            return Err(format!(
                "{scenario_id}: invalid hex byte pair length at pair {idx} in {label}"
            ));
        };
        let hi_byte = *hi_byte;
        let lo_byte = *lo_byte;
        let hi = hex_nibble(hi_byte).ok_or_else(|| {
            format!(
                "{scenario_id}: invalid high nibble {:?} at byte pair {idx} in {label}",
                hi_byte as char
            )
        })?;
        let lo = hex_nibble(lo_byte).ok_or_else(|| {
            format!(
                "{scenario_id}: invalid low nibble {:?} at byte pair {idx} in {label}",
                lo_byte as char
            )
        })?;
        out.push((hi << 4) | lo);
    }
    Ok(out)
}

fn contains_subslice(haystack: &[u8], needle: &[u8]) -> bool {
    needle.is_empty()
        || haystack
            .windows(needle.len())
            .any(|window| window == needle)
}

fn action_kind(action: &Action) -> &'static str {
    match action {
        Action::Print(_) => "Print",
        Action::PrintString(_) => "PrintString",
        Action::Control(_) => "Control",
        Action::DeviceControl(_) => "DeviceControl",
        Action::OperatingSystemCommand(_) => "OperatingSystemCommand",
        Action::CSI(_) => "CSI",
        Action::Esc(_) => "Esc",
        Action::Sixel(_) => "Sixel",
        Action::XtGetTcap(_) => "XtGetTcap",
        Action::KittyImage(_) => "KittyImage",
    }
}

fn rendered_debug(actions: &[Action]) -> String {
    actions
        .iter()
        .enumerate()
        .map(|(idx, action)| format!("{idx:02}: {action:?}"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn rendered_display(actions: &[Action]) -> String {
    actions.iter().map(ToString::to_string).collect()
}

fn kind_counts(actions: &[Action]) -> BTreeMap<&'static str, usize> {
    let mut counts = BTreeMap::new();
    for action in actions {
        *counts.entry(action_kind(action)).or_insert(0) += 1;
    }
    counts
}

fn coalesce_print_actions(actions: Vec<Action>) -> Vec<Action> {
    let mut coalesced = Vec::with_capacity(actions.len());
    for action in actions {
        action.append_to(&mut coalesced);
    }
    coalesced
}

fn assert_expected_actions(scenario_id: &str, actions: &[Action], expected: &Value) -> TestResult {
    let min_actions = expected
        .get("min_actions")
        .and_then(Value::as_u64)
        .ok_or_else(|| format!("{scenario_id}: missing min_actions"))?;
    let min_actions = usize::try_from(min_actions)
        .map_err(|err| format!("{scenario_id}: min_actions is too large: {err}"))?;
    let debug = rendered_debug(actions);
    if actions.len() < min_actions {
        return Err(format!(
            "{scenario_id}: expected at least {min_actions} actions, got {}\n{debug}",
            actions.len()
        ));
    }

    let counts = kind_counts(actions);
    let required_kinds = expected
        .get("required_action_kinds")
        .and_then(Value::as_object)
        .ok_or_else(|| format!("{scenario_id}: missing required_action_kinds"))?;
    for (kind, min_count) in required_kinds {
        let min_count = min_count
            .as_u64()
            .ok_or_else(|| format!("{scenario_id}: count for {kind} must be an integer"))?;
        let min_count = usize::try_from(min_count)
            .map_err(|err| format!("{scenario_id}: count for {kind} is too large: {err}"))?;
        let actual = counts.get(kind.as_str()).copied().unwrap_or(0);
        if actual < min_count {
            return Err(format!(
                "{scenario_id}: expected at least {min_count} {kind} actions, got {actual}\n{debug}"
            ));
        }
    }

    for forbidden in optional_string_array(scenario_id, expected, "forbidden_action_kinds")? {
        let actual = counts.get(forbidden).copied().unwrap_or(0);
        if actual != 0 {
            return Err(format!(
                "{scenario_id}: forbidden action kind {forbidden} appeared {actual} time(s)\n{debug}"
            ));
        }
    }

    for fragment in optional_string_array(scenario_id, expected, "required_debug_fragments")? {
        if !debug.contains(fragment) {
            return Err(format!(
                "{scenario_id}: missing debug fragment {fragment:?}\n{debug}"
            ));
        }
    }

    let display = rendered_display(actions);
    for fragment in optional_string_array(scenario_id, expected, "required_display_utf8")? {
        if !display.contains(fragment) {
            return Err(format!(
                "{scenario_id}: missing display fragment {fragment:?}\n{debug}"
            ));
        }
    }

    let display_bytes = display.as_bytes();
    for fragment_hex in optional_string_array(scenario_id, expected, "required_display_hex")? {
        let needle = decode_hex_text(scenario_id, fragment_hex, "inline expected hex fragment")?;
        if !contains_subslice(display_bytes, &needle) {
            return Err(format!(
                "{scenario_id}: missing display hex fragment {fragment_hex}\n{debug}"
            ));
        }
    }
    Ok(())
}

fn assert_expected_input(scenario_id: &str, input: &[u8], expected: &Value) -> TestResult {
    for fragment_hex in optional_string_array(scenario_id, expected, "required_input_hex")? {
        let needle = decode_hex_text(scenario_id, fragment_hex, "inline expected input fragment")?;
        if !contains_subslice(input, &needle) {
            return Err(format!(
                "{scenario_id}: missing input hex fragment {fragment_hex}"
            ));
        }
    }
    Ok(())
}

#[test]
fn terminal_conformance_manifest_is_well_formed() -> TestResult {
    let root = fixture_root();
    let manifest = load_json(&root.join("manifest.json"))?;
    let proof_command = string_field("manifest", &manifest, "proof_command")?;
    if !proof_command.contains("RCH_REQUIRE_REMOTE=1 rch exec --") {
        return Err("manifest proof command must require remote RCH execution".into());
    }

    let scenarios = manifest
        .get("scenarios")
        .and_then(Value::as_array)
        .ok_or_else(|| "manifest scenarios must be an array".to_string())?;
    if scenarios.len() < MIN_SCENARIOS {
        return Err(format!(
            "manifest must contain at least {MIN_SCENARIOS} scenarios"
        ));
    }

    let mut seen = BTreeSet::new();
    for scenario in scenarios {
        let scenario_id = string_field("manifest scenario", scenario, "scenario_id")?;
        if !seen.insert(scenario_id.to_owned()) {
            return Err(format!("{scenario_id}: duplicate scenario_id"));
        }
        if !scenario_id.starts_with("tc-") {
            return Err(format!("{scenario_id}: scenario_id must use tc- prefix"));
        }

        for key in [
            "family",
            "source",
            "input_artifact",
            "expected_artifact",
            "proof_command",
            "no_mock_boundary",
            "redaction_status",
        ] {
            let value = string_field(scenario_id, scenario, key)?;
            if value.trim().is_empty() {
                return Err(format!("{scenario_id}: {key} is empty"));
            }
        }

        if !string_field(scenario_id, scenario, "proof_command")?
            .contains("RCH_REQUIRE_REMOTE=1 rch exec --")
        {
            return Err(format!(
                "{scenario_id}: proof command must require remote RCH execution"
            ));
        }
        if !string_field(scenario_id, scenario, "redaction_status")?.contains("No secrets") {
            return Err(format!(
                "{scenario_id}: redaction status must explicitly rule out secrets"
            ));
        }

        let input = relative_artifact_path(
            scenario_id,
            &root,
            string_field(scenario_id, scenario, "input_artifact")?,
        )?;
        let expected = relative_artifact_path(
            scenario_id,
            &root,
            string_field(scenario_id, scenario, "expected_artifact")?,
        )?;
        if !input.is_file() {
            return Err(format!("{scenario_id}: missing {}", input.display()));
        }
        if !expected.is_file() {
            return Err(format!("{scenario_id}: missing {}", expected.display()));
        }
    }
    Ok(())
}

#[test]
fn terminal_conformance_transcripts_match_expected_actions() -> TestResult {
    let root = fixture_root();
    let manifest = load_json(&root.join("manifest.json"))?;
    let scenarios = manifest
        .get("scenarios")
        .and_then(Value::as_array)
        .ok_or_else(|| "manifest scenarios must be an array".to_string())?;

    for scenario in scenarios {
        let scenario_id = string_field("manifest scenario", scenario, "scenario_id")?;
        let input_path = relative_artifact_path(
            scenario_id,
            &root,
            string_field(scenario_id, scenario, "input_artifact")?,
        )?;
        let expected_path = relative_artifact_path(
            scenario_id,
            &root,
            string_field(scenario_id, scenario, "expected_artifact")?,
        )?;
        let expected = load_json(&expected_path)?;
        let expected_scenario_id = string_field(scenario_id, &expected, "scenario_id")?;
        if scenario_id != expected_scenario_id {
            return Err(format!(
                "{scenario_id}: expected artifact scenario_id mismatch: {expected_scenario_id}"
            ));
        }

        let input = decode_hex(scenario_id, &input_path)?;
        assert_expected_input(scenario_id, &input, &expected)?;
        let mut parser = Parser::new();
        let actions = coalesce_print_actions(parser.parse_as_vec(&input));
        assert_expected_actions(scenario_id, &actions, &expected)?;
    }
    Ok(())
}
