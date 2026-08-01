//! Contract, schema, source-binding, and mutation tests for the renderer catalog.
//!
//! These tests are deliberately file-only. They do not launch the GUI, contact a
//! mux domain, read an active pane, or qualify any native target.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fs;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};

use frankenterm_core::renderer_scenario_catalog::{
    MAX_RENDERER_SCENARIO_CATALOG_BYTES, REQUIRED_RENDERER_CHECKPOINT_BINDING_COUNT,
    REQUIRED_RENDERER_COVERAGE_OVERLAY_COUNT, REQUIRED_RENDERER_SCENARIO_COUNT,
    REQUIRED_RENDERER_TERMINAL_FEATURE_COUNT, RENDERER_OUTPUT_AUTHORITY_TRACKING_REF,
    RENDERER_SCENARIO_CATALOG_REVISION, RQ_S11_COMPARATOR_POLICY_REF,
    RQ_S13_COMPARATOR_POLICY_REF,
    RendererCatalogAuthority,
    RendererAccessibilityGeometryState, RendererCheckpointDetectorId, RendererCheckpointRole,
    RendererContentApplicationBoundary, RendererContentDecoder,
    RendererContentCompositionOperation, RendererContentDeterministicIdentity,
    RendererContentEncoding, RendererContentInputAvailability, RendererContentPayloadSelector,
    RendererCoverageOverlayId, RendererDynamicRangeMode, RendererFleetPoint, RendererGesture,
    RendererKeyModifier, RendererNegativeControlId,
    RendererMutationTarget, RendererOutputRateOverride, RendererPaneOrdinalSelector,
    RendererPixelCoordinateSpace, RendererPixelRect, RendererResolvedCheckpointAnchor,
    RendererResolvedScenarioOverlay, RendererScenarioCatalog, RendererScenarioDecodeCode,
    RendererScenarioGapCode, RendererScenarioResolveError, RendererScenarioValidationCode,
    RendererResolverPreparationStats, RendererSelectionState,
    RendererTerminalBufferKind, RendererTimelineAction,
    expected_renderer_scenario_id, expected_renderer_scenario_seed,
    prepare_renderer_scenario_catalog, resolve_renderer_scenario_overlay,
    validate_renderer_repository_reference,
};
use jsonschema::{Draft, Validator};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

const CATALOG_RELATIVE_PATH: &str = "docs/design/renderer-scenario-catalog.v1.json";
const SCHEMA_RELATIVE_PATH: &str = "docs/json-schema/ft-renderer-scenario-catalog.json";
const ISSUES_RELATIVE_PATH: &str = ".beads/issues.jsonl";

type CatalogMutationCase = (&'static str, fn(&mut RendererScenarioCatalog));

fn repository_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("repository root should resolve")
}

fn read_bytes(path: &Path) -> Vec<u8> {
    fs::read(path).unwrap_or_else(|error| panic!("failed to read {}: {error}", path.display()))
}

fn read_json(path: &Path) -> Value {
    let bytes = read_bytes(path);
    serde_json::from_slice(&bytes)
        .unwrap_or_else(|error| panic!("failed to parse JSON {}: {error}", path.display()))
}

fn load_catalog_bytes(root: &Path) -> Vec<u8> {
    read_bytes(&root.join(CATALOG_RELATIVE_PATH))
}

fn load_catalog_value(root: &Path) -> Value {
    read_json(&root.join(CATALOG_RELATIVE_PATH))
}

fn load_catalog(root: &Path) -> RendererScenarioCatalog {
    let bytes = load_catalog_bytes(root);
    assert!(
        bytes.len() <= MAX_RENDERER_SCENARIO_CATALOG_BYTES,
        "checked-in catalog exceeds its public bounded-decoder limit"
    );
    RendererScenarioCatalog::decode_json_bounded(&bytes).unwrap_or_else(|error| {
        panic!(
            "catalog {} failed bounded typed decode: {error}",
            root.join(CATALOG_RELATIVE_PATH).display()
        )
    })
}

fn load_schema_validator(root: &Path) -> Validator {
    let path = root.join(SCHEMA_RELATIVE_PATH);
    let schema = read_json(&path);
    Validator::options()
        .with_draft(Draft::Draft202012)
        .should_validate_formats(true)
        .build(&schema)
        .unwrap_or_else(|error| panic!("schema {} failed to compile: {error}", path.display()))
}

fn schema_errors(validator: &Validator, instance: &Value) -> Vec<String> {
    validator
        .iter_errors(instance)
        .map(|error| format!("{error} at {}", error.instance_path()))
        .collect()
}

fn load_schema_clean_catalog(root: &Path, validator: &Validator) -> Value {
    let catalog = load_catalog_value(root);
    let errors = schema_errors(validator, &catalog);
    assert!(
        errors.is_empty(),
        "canonical renderer catalog must be schema-clean before a mutation can prove rejection:\n{}",
        errors.join("\n")
    );
    catalog
}

fn validation_errors(catalog: &RendererScenarioCatalog) -> String {
    catalog
        .validate()
        .errors
        .iter()
        .map(|error| format!("{} {}: {}", error.code.as_str(), error.path, error.detail))
        .collect::<Vec<_>>()
        .join("\n")
}

fn assert_has_code(catalog: &RendererScenarioCatalog, code: RendererScenarioValidationCode) {
    let report = catalog.validate();
    assert!(
        report.contains_code(code),
        "expected validation code {} ({code:?}), got:\n{}",
        code.as_str(),
        report
            .errors
            .iter()
            .map(|error| format!("{} {}: {}", error.code.as_str(), error.path, error.detail))
            .collect::<Vec<_>>()
            .join("\n")
    );
}

fn assert_invalid(catalog: &RendererScenarioCatalog, mutation: &str) {
    let report = catalog.validate();
    assert!(
        !report.valid && !report.errors.is_empty(),
        "{mutation} must fail closed, but validation returned valid with no errors"
    );
}

fn assert_invalid_without_panic(catalog: &RendererScenarioCatalog, mutation: &str) {
    let outcome = catch_unwind(AssertUnwindSafe(|| catalog.validate()));
    let report = outcome.unwrap_or_else(|_| panic!("{mutation} made semantic validation panic"));
    assert!(
        !report.valid && !report.errors.is_empty(),
        "{mutation} must fail closed with a structured validation error"
    );
}

fn issue_ids(root: &Path) -> HashSet<String> {
    let path = root.join(ISSUES_RELATIVE_PATH);
    let raw = fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("failed to read {}: {error}", path.display()));
    raw.lines()
        .enumerate()
        .filter_map(|(line_index, line)| {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                return None;
            }
            let value = serde_json::from_str::<Value>(trimmed).unwrap_or_else(|error| {
                panic!(
                    "invalid JSON on line {} of {}: {error}",
                    line_index + 1,
                    path.display()
                )
            });
            Some(
                value
                    .get("id")
                    .and_then(Value::as_str)
                    .unwrap_or_else(|| {
                        panic!(
                            "missing string id on line {} of {}",
                            line_index + 1,
                            path.display()
                        )
                    })
                    .to_string(),
            )
        })
        .collect()
}

fn is_bead_reference(value: &str) -> bool {
    value.starts_with("ft-") || value.starts_with("wa-")
}

fn collect_catalog_references(
    value: &Value,
    field_name: Option<&str>,
    repository_refs: &mut BTreeSet<String>,
    bead_refs: &mut BTreeSet<String>,
) {
    match value {
        Value::Object(object) => {
            for (key, child) in object {
                collect_catalog_references(child, Some(key), repository_refs, bead_refs);
            }
        }
        Value::Array(array) => {
            for child in array {
                collect_catalog_references(child, field_name, repository_refs, bead_refs);
            }
        }
        Value::String(string) => {
            if is_bead_reference(string) {
                bead_refs.insert(string.clone());
            }
            if field_name.is_some_and(|field| {
                field == "repository_ref"
                    || field.ends_with("_ref")
                    || field.ends_with("_refs")
            }) && !is_bead_reference(string)
            {
                repository_refs.insert(string.clone());
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
    }
}

fn reference_path(reference: &str) -> &str {
    reference
        .split_once('#')
        .map_or(reference, |(path, _fragment)| path)
}

fn github_heading_slug(heading: &str) -> String {
    heading
        .trim()
        .trim_end_matches('#')
        .trim_end()
        .chars()
        .filter_map(|character| {
            if character.is_alphanumeric() || matches!(character, '-' | '_') {
                Some(character.to_ascii_lowercase())
            } else if character.is_whitespace() {
                Some('-')
            } else {
                None
            }
        })
        .collect()
}

fn markdown_heading_match_count(document: &str, fragment: &str) -> usize {
    let mut fence_marker = None;
    document
        .lines()
        .filter(|line| {
            let trimmed = line.trim_end_matches('\r');
            let fence_candidate = trimmed.trim_start();
            let marker = if fence_candidate.starts_with("```") {
                Some('`')
            } else if fence_candidate.starts_with("~~~") {
                Some('~')
            } else {
                None
            };
            if let Some(marker) = marker {
                match fence_marker {
                    Some(open) if open == marker => fence_marker = None,
                    None => fence_marker = Some(marker),
                    Some(_) => {}
                }
                return false;
            }
            if fence_marker.is_some() {
                return false;
            }
            let level = trimmed.bytes().take_while(|byte| *byte == b'#').count();
            level != 0
                && level <= 6
                && trimmed
                    .as_bytes()
                    .get(level)
                    .is_some_and(u8::is_ascii_whitespace)
                && github_heading_slug(&trimmed[level..]) == fragment
        })
        .count()
}

fn json_object_id_match_count(value: &Value, fragment: &str) -> usize {
    match value {
        Value::Object(object) => {
            let has_own_match = object.get("id").and_then(Value::as_str) == Some(fragment);
            let own_match = usize::from(has_own_match);
            own_match
                + object
                    .values()
                    .map(|child| json_object_id_match_count(child, fragment))
                    .sum::<usize>()
        }
        Value::Array(array) => array
            .iter()
            .map(|child| json_object_id_match_count(child, fragment))
            .sum(),
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => 0,
    }
}

fn unresolved_repository_refs(
    root: &Path,
    references: impl IntoIterator<Item = String>,
) -> Vec<String> {
    let mut unresolved = Vec::new();
    for reference in references {
        if validate_renderer_repository_reference(&reference).is_err() {
            unresolved.push(reference);
            continue;
        }
        let (relative_path, fragment) = reference
            .split_once('#')
            .map_or((reference.as_str(), None), |(path, fragment)| {
                (path, Some(fragment))
            });
        let path = root.join(relative_path);
        if !path.exists() {
            unresolved.push(reference);
            continue;
        }
        let Some(fragment) = fragment else {
            continue;
        };
        let extension = path
            .extension()
            .and_then(|extension| extension.to_str())
            .unwrap_or_default();
        let unique = if extension.eq_ignore_ascii_case("md") {
            let document = fs::read_to_string(&path)
                .unwrap_or_else(|error| panic!("failed to read {}: {error}", path.display()));
            markdown_heading_match_count(&document, fragment) == 1
        } else if extension.eq_ignore_ascii_case("json") {
            json_object_id_match_count(&read_json(&path), fragment) == 1
        } else {
            false
        };
        if !unique {
            unresolved.push(reference);
        }
    }
    unresolved
}

fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

fn decode_hex_v1(path: &Path, encoded: &[u8]) -> Vec<u8> {
    let compact = encoded
        .iter()
        .copied()
        .filter(|byte| !byte.is_ascii_whitespace())
        .collect::<Vec<_>>();
    let text = std::str::from_utf8(&compact)
        .unwrap_or_else(|error| panic!("{} is not ASCII/UTF-8 hex: {error}", path.display()));
    hex::decode(text).unwrap_or_else(|error| panic!("{} is malformed hex: {error}", path.display()))
}

fn assert_manifest_row_binds_payload(
    root: &Path,
    manifest_ref: &str,
    manifest_row_id: &str,
    payload_ref: &str,
) {
    assert!(
        !manifest_ref.contains('#'),
        "manifest_ref stays fragmentless; manifest_row_id is the sole row selector"
    );
    let manifest_path = root.join(reference_path(manifest_ref));
    let manifest = read_json(&manifest_path);
    let rows = manifest
        .get("scenarios")
        .and_then(Value::as_array)
        .unwrap_or_else(|| panic!("{} must contain a scenarios array", manifest_path.display()));
    let matches = rows
        .iter()
        .filter(|row| row.get("scenario_id").and_then(Value::as_str) == Some(manifest_row_id))
        .collect::<Vec<_>>();
    let [row] = matches.as_slice() else {
        panic!(
            "{} must contain exactly one scenario_id `{manifest_row_id}`, found {}",
            manifest_path.display(),
            matches.len()
        );
    };
    let input_artifact = row
        .get("input_artifact")
        .and_then(Value::as_str)
        .unwrap_or_else(|| panic!("manifest row `{manifest_row_id}` lacks input_artifact"));
    let declared = manifest_path
        .parent()
        .expect("manifest has a parent directory")
        .join(input_artifact)
        .canonicalize()
        .unwrap_or_else(|error| {
            panic!(
                "manifest row `{manifest_row_id}` input {} does not resolve: {error}",
                input_artifact
            )
        });
    let payload = root
        .join(reference_path(payload_ref))
        .canonicalize()
        .unwrap_or_else(|error| panic!("payload `{payload_ref}` does not resolve: {error}"));
    assert_eq!(
        declared, payload,
        "manifest row `{manifest_row_id}` does not bind its declared payload"
    );
}

fn assert_payload_digests(root: &Path, catalog: &RendererScenarioCatalog) {
    let canonical_unavailable_generators = BTreeSet::from([
        "content.ligature_enabled_gap.v1",
        "content.live_ime_gap.v1",
        "content.a11y_geometry_gap.v1",
    ]);
    for corpus in &catalog.content_corpus_references {
        match &corpus.deterministic_identity {
            RendererContentDeterministicIdentity::Generator {
                input_manifest_ref,
                output_decoder,
                ..
            } => {
                assert_eq!(
                    *output_decoder,
                    RendererContentDecoder::GeneratorV1,
                    "{} generator must pin generator_v1",
                    corpus.content_corpus_id
                );
                assert!(
                    root.join(reference_path(input_manifest_ref)).exists(),
                    "{} generator input manifest is missing",
                    corpus.content_corpus_id
                );
                if canonical_unavailable_generators.contains(corpus.content_corpus_id.as_str()) {
                    assert!(
                        matches!(
                            &corpus.availability,
                            RendererContentInputAvailability::Unavailable { .. }
                        ),
                        "{} is a canonical tracked input gap",
                        corpus.content_corpus_id
                    );
                }
            }
            RendererContentDeterministicIdentity::Payload {
                payload_ref,
                selector,
                encoding,
                decoder,
                encoded_payload_sha256,
                decoded_payload_sha256,
                ..
            } => {
                let payload_path = root.join(reference_path(payload_ref));
                let encoded = read_bytes(&payload_path);
                assert_eq!(
                    sha256_hex(&encoded),
                    encoded_payload_sha256.as_str(),
                    "{} encoded payload digest drifted",
                    corpus.content_corpus_id
                );
                let decoded = match decoder {
                    RendererContentDecoder::Identity => encoded.clone(),
                    RendererContentDecoder::Utf8ValidateV1 => {
                        std::str::from_utf8(&encoded).unwrap_or_else(|error| {
                            panic!("{} is not valid UTF-8: {error}", payload_path.display())
                        });
                        encoded.clone()
                    }
                    RendererContentDecoder::HexDecodeV1 => decode_hex_v1(&payload_path, &encoded),
                    RendererContentDecoder::JsonFixtureStateV1 => {
                        serde_json::from_slice::<Value>(&encoded).unwrap_or_else(|error| {
                            panic!("{} is not valid fixture JSON: {error}", payload_path.display())
                        });
                        encoded.clone()
                    }
                    RendererContentDecoder::GeneratorV1 => {
                        panic!(
                            "{} payload identity cannot use generator_v1",
                            corpus.content_corpus_id
                        )
                    }
                };
                let selected = match selector {
                    RendererContentPayloadSelector::WholePayload => decoded.as_slice(),
                    RendererContentPayloadSelector::ManifestRowSegment {
                        manifest_ref,
                        manifest_row_id,
                        decoded_byte_start,
                        decoded_byte_end_exclusive,
                    } => {
                        assert_manifest_row_binds_payload(
                            root,
                            manifest_ref,
                            manifest_row_id,
                            payload_ref,
                        );
                        let start = usize::try_from(*decoded_byte_start)
                            .expect("decoded segment start fits usize");
                        let end = usize::try_from(*decoded_byte_end_exclusive)
                            .expect("decoded segment end fits usize");
                        decoded.get(start..end).unwrap_or_else(|| {
                            panic!(
                                "{} decoded selector {start}..{end} exceeds {} bytes",
                                corpus.content_corpus_id,
                                decoded.len()
                            )
                        })
                    }
                };
                assert_eq!(
                    sha256_hex(selected),
                    decoded_payload_sha256.as_str(),
                    "{} decoded/pre-framing digest drifted",
                    corpus.content_corpus_id
                );
                match encoding {
                    RendererContentEncoding::HexTranscriptV1 => {
                        assert_eq!(*decoder, RendererContentDecoder::HexDecodeV1);
                    }
                    RendererContentEncoding::GpuFixtureStateV1 => {
                        assert_eq!(*decoder, RendererContentDecoder::JsonFixtureStateV1);
                    }
                    RendererContentEncoding::RawTerminalBytes => {
                        assert_eq!(*decoder, RendererContentDecoder::Identity);
                    }
                    RendererContentEncoding::Utf8Text
                    | RendererContentEncoding::GeneratedTerminalBytesV1
                    | RendererContentEncoding::GeneratedTypedStateV1 => {}
                }
            }
        }
    }
}

fn expected_all_frame_detectors(
    overlay_id: RendererCoverageOverlayId,
) -> Vec<RendererCheckpointDetectorId> {
    let mut expected = vec![
        RendererCheckpointDetectorId::NoMissingGlyphs,
        RendererCheckpointDetectorId::CoherentCellWidths,
        RendererCheckpointDetectorId::ExactRowWidth,
        RendererCheckpointDetectorId::CoherentRendererGeneration,
        RendererCheckpointDetectorId::NoMixedGenerationTearBand,
        RendererCheckpointDetectorId::NoStaleOrDuplicateFrame,
        RendererCheckpointDetectorId::NonblankAfterBaseline,
        RendererCheckpointDetectorId::ExactTerminalState,
        RendererCheckpointDetectorId::CursorGeometry,
    ];
    match overlay_id {
        RendererCoverageOverlayId::ProductionDefault
        | RendererCoverageOverlayId::UnicodeMaximal
        | RendererCoverageOverlayId::LigatureEnabled => {}
        RendererCoverageOverlayId::AlternateScreen => {
            expected.push(RendererCheckpointDetectorId::AlternateScreenState);
        }
        RendererCoverageOverlayId::ImeComposing => {
            expected.push(RendererCheckpointDetectorId::ImeGeometry);
        }
        RendererCoverageOverlayId::ImageHyperlink => {
            expected.push(RendererCheckpointDetectorId::HyperlinkGeometry);
            expected.push(RendererCheckpointDetectorId::ImageGeometry);
        }
        RendererCoverageOverlayId::Selection => {
            expected.push(RendererCheckpointDetectorId::SelectionGeometry);
        }
        RendererCoverageOverlayId::A11yGeometry => {
            expected.push(RendererCheckpointDetectorId::AccessibilityGeometry);
        }
    }
    expected
}

fn is_live_resize_gesture(gesture: RendererGesture) -> bool {
    matches!(
        gesture,
        RendererGesture::SameGridDrag
            | RendererGesture::GridChangingDrag
            | RendererGesture::Reflow80To200
            | RendererGesture::Reflow200To80
            | RendererGesture::OutputOverlapResize
    )
}

fn rect_bounds(rect: RendererPixelRect) -> (i64, i64, i64, i64) {
    (
        i64::from(rect.x),
        i64::from(rect.y),
        i64::from(rect.x) + i64::from(rect.width),
        i64::from(rect.y) + i64::from(rect.height),
    )
}

fn rect_contains(container: RendererPixelRect, child: RendererPixelRect) -> bool {
    let outer = rect_bounds(container);
    let inner = rect_bounds(child);
    child.width > 0
        && child.height > 0
        && inner.0 >= outer.0
        && inner.1 >= outer.1
        && inner.2 <= outer.2
        && inner.3 <= outer.3
}

fn rects_are_disjoint(left: RendererPixelRect, right: RendererPixelRect) -> bool {
    let first = rect_bounds(left);
    let second = rect_bounds(right);
    first.2 <= second.0
        || second.2 <= first.0
        || first.3 <= second.1
        || second.3 <= first.1
}

fn assert_resolved_anchor_topology(
    anchor: &RendererResolvedCheckpointAnchor,
    fleet_point: RendererFleetPoint,
    known_content_ids: &BTreeSet<&str>,
) {
    assert_eq!(anchor.windows.len(), usize::from(fleet_point.window_count()));
    assert_eq!(anchor.tabs.len(), usize::from(fleet_point.tab_count()));
    assert_eq!(anchor.panes.len(), usize::from(fleet_point.pane_count()));
    assert!(!anchor.layout_profile_id.is_empty());
    assert!(!anchor.layout_stable_id_revision.is_empty());
    assert!(!anchor.content_distribution_profile_id.is_empty());
    assert!(anchor.content_distribution_profile_revision > 0);

    for (position, window) in anchor.windows.iter().enumerate() {
        let ordinal = u16::try_from(position).expect("window position fits u16");
        assert_eq!(window.window_ordinal, ordinal);
        assert_eq!(window.window_id, format!("window-{ordinal:03}"));
        assert_eq!(
            window.coordinate_space,
            RendererPixelCoordinateSpace::WindowDrawable
        );
        assert!(window.drawable_rect.width > 0 && window.drawable_rect.height > 0);
        let expected_tabs = anchor
            .tabs
            .iter()
            .filter(|tab| tab.window_ordinal == ordinal)
            .map(|tab| tab.tab_id.clone())
            .collect::<Vec<_>>();
        assert_eq!(window.ordered_tab_ids, expected_tabs);
        assert_eq!(
            window.active_tab_id,
            window.ordered_tab_ids[usize::from(window.active_tab_ordinal)]
        );
    }
    assert_eq!(
        anchor.windows.iter().filter(|window| window.focused).count(),
        1
    );
    assert_eq!(
        anchor
            .windows
            .iter()
            .find(|window| window.focused)
            .map(|window| window.window_id.as_str()),
        Some(anchor.focused_window_id.as_str())
    );

    for (position, tab) in anchor.tabs.iter().enumerate() {
        let ordinal = u16::try_from(position).expect("tab position fits u16");
        assert_eq!(tab.tab_ordinal, ordinal);
        assert_eq!(tab.tab_id, format!("tab-{ordinal:03}"));
        let window = &anchor.windows[usize::from(tab.window_ordinal)];
        assert_eq!(tab.window_id, window.window_id);
        assert_eq!(
            window.ordered_tab_ids[usize::from(tab.window_local_tab_ordinal)],
            tab.tab_id
        );
        assert_eq!(tab.active, window.active_tab_id == tab.tab_id);
        let expected_panes = anchor
            .panes
            .iter()
            .filter(|pane| pane.tab_ordinal == ordinal)
            .map(|pane| pane.pane_id.clone())
            .collect::<Vec<_>>();
        assert_eq!(tab.ordered_pane_ids, expected_panes);
        assert!(!tab.ordered_pane_ids.is_empty());
    }
    assert_eq!(
        anchor.tabs.iter().filter(|tab| tab.active).count(),
        anchor.windows.len(),
        "every window has exactly one active tab"
    );

    for (position, pane) in anchor.panes.iter().enumerate() {
        let ordinal = u16::try_from(position).expect("pane position fits u16");
        assert_eq!(pane.pane_ordinal, ordinal);
        assert_eq!(pane.pane_id, format!("pane-{ordinal:03}"));
        let window = &anchor.windows[usize::from(pane.window_ordinal)];
        let tab = &anchor.tabs[usize::from(pane.tab_ordinal)];
        assert_eq!(pane.window_id, window.window_id);
        assert_eq!(pane.tab_id, tab.tab_id);
        assert_eq!(pane.active_tab, tab.active);
        assert_eq!(
            pane.coordinate_space,
            RendererPixelCoordinateSpace::WindowDrawable
        );
        assert!(rect_contains(window.drawable_rect, pane.window_content_rect));
        assert_eq!(
            pane.surface_state.display.viewport_width_px,
            pane.window_content_rect.width
        );
        assert_eq!(
            pane.surface_state.display.viewport_height_px,
            pane.window_content_rect.height
        );
        assert!(
            !pane.applied_materialization_steps.is_empty(),
            "{} must expose applied content replay at {}",
            pane.pane_id,
            anchor.checkpoint_id
        );
        for step in &pane.applied_materialization_steps {
            assert!(
                known_content_ids.contains(step.content_corpus_id.as_str()),
                "{} resolved unknown materialization input {}",
                pane.pane_id,
                step.content_corpus_id
            );
        }
        let mut visible_content_ids = pane
            .surface_state
            .terminal
            .primary_buffer
            .content_corpus_ids
            .iter()
            .chain(
                pane.surface_state
                    .terminal
                    .alternate_buffer
                    .content_corpus_ids
                    .iter(),
            )
            .peekable();
        assert!(
            visible_content_ids.peek().is_some(),
            "{} resolved with no materialized terminal buffer content",
            pane.pane_id
        );
        for content_id in visible_content_ids {
            assert!(known_content_ids.contains(content_id.as_str()));
            assert!(
                pane.applied_materialization_steps
                    .iter()
                    .any(|step| step.content_corpus_id.as_str() == content_id.as_str()),
                "{} visible content {} lacks an applied materialization step",
                pane.pane_id,
                content_id
            );
        }
    }
    assert_eq!(
        anchor.panes.iter().filter(|pane| pane.focused).count(),
        1
    );
    let focused_pane = anchor
        .panes
        .iter()
        .find(|pane| pane.focused)
        .expect("one focused pane exists");
    assert_eq!(focused_pane.pane_id, anchor.focused_pane_id);
    assert_eq!(focused_pane.window_id, anchor.focused_window_id);
    assert!(focused_pane.active_tab);

    for tab in &anchor.tabs {
        let window = &anchor.windows[usize::from(tab.window_ordinal)];
        let panes = anchor
            .panes
            .iter()
            .filter(|pane| pane.tab_ordinal == tab.tab_ordinal)
            .collect::<Vec<_>>();
        for (left_index, left) in panes.iter().enumerate() {
            for right in panes.iter().skip(left_index + 1) {
                assert!(
                    rects_are_disjoint(left.window_content_rect, right.window_content_rect),
                    "{} and {} overlap in {}",
                    left.pane_id,
                    right.pane_id,
                    tab.tab_id
                );
            }
        }
        let pane_area = panes
            .iter()
            .map(|pane| {
                u64::from(pane.window_content_rect.width)
                    * u64::from(pane.window_content_rect.height)
            })
            .sum::<u64>();
        let window_area =
            u64::from(window.drawable_rect.width) * u64::from(window.drawable_rect.height);
        assert_eq!(
            pane_area, window_area,
            "{} split leaves must exactly cover the drawable region",
            tab.tab_id
        );
    }
}

fn assert_resolved_overlay_topology(
    resolved: &RendererResolvedScenarioOverlay,
    fleet_point: RendererFleetPoint,
    known_content_ids: &BTreeSet<&str>,
) {
    assert_eq!(resolved.fleet_point, fleet_point);
    assert_eq!(resolved.overlay_id, RendererCoverageOverlayId::ProductionDefault);
    assert_eq!(
        resolved.catalog_revision,
        RENDERER_SCENARIO_CATALOG_REVISION
    );
    assert!(resolved.workload_revision > 0);
    assert!(resolved.overlay_profile_revision > 0);
    assert!(resolved.renderer_config_profile_revision > 0);
    assert_eq!(resolved.anchors.len(), 4);
    for anchor in &resolved.anchors {
        assert_resolved_anchor_topology(anchor, fleet_point, known_content_ids);
    }
    if fleet_point == RendererFleetPoint::P001 {
        assert!(resolved.anchors.iter().all(|anchor| {
            anchor.panes.len() == 1
                && anchor.panes[0].split_path.is_empty()
                && anchor.panes[0].window_content_rect == anchor.windows[0].drawable_rect
        }));
    } else if fleet_point == RendererFleetPoint::P200 {
        assert!(resolved.anchors.iter().all(|anchor| {
            anchor
                .panes
                .iter()
                .all(|pane| !pane.split_path.is_empty())
        }));
    }
}

#[test]
fn checked_in_catalog_matches_draft_2020_12_schema() {
    let root = repository_root();
    let validator = load_schema_validator(&root);
    let _catalog = load_schema_clean_catalog(&root, &validator);
}

#[test]
fn checked_in_catalog_passes_typed_semantic_validation() {
    let catalog = load_catalog(&repository_root());
    let report = catalog.validate();
    assert!(
        report.valid,
        "checked-in renderer catalog failed semantic validation:\n{}",
        validation_errors(&catalog)
    );
    assert_eq!(
        catalog.catalog_revision,
        RENDERER_SCENARIO_CATALOG_REVISION
    );
    assert_eq!(catalog.authority, RendererCatalogAuthority::ContractOnly);
    assert!(
        !catalog
            .accessibility_authority_boundary
            .machine_geometry_authorizes_native_accessibility,
        "contract geometry must never mint native accessibility authority"
    );
}

#[test]
fn checked_in_catalog_uses_bounded_deterministic_round_trip() {
    let root = repository_root();
    let original = load_catalog_value(&root);
    let catalog = load_catalog(&root);
    let encoded_value =
        serde_json::to_value(&catalog).expect("public renderer catalog DTO should serialize");
    assert_eq!(
        encoded_value, original,
        "serde DTO shape drifted from the checked-in public JSON contract"
    );
    let first = serde_json::to_vec(&catalog).expect("renderer catalog should encode");
    let second = serde_json::to_vec(&catalog).expect("renderer catalog should encode again");
    assert_eq!(first, second, "typed encoding must be deterministic");
    assert_eq!(
        RendererScenarioCatalog::decode_json_bounded(&first)
            .expect("deterministic encoding should decode"),
        catalog
    );
}

#[test]
fn exact_matrix_overlay_readiness_and_checkpoint_counts_are_frozen() {
    let catalog = load_catalog(&repository_root());
    let report = catalog.validate();
    assert!(report.valid, "catalog must validate before count assertions");
    assert_eq!(catalog.scenarios.len(), REQUIRED_RENDERER_SCENARIO_COUNT);
    assert_eq!(
        catalog.coverage_overlay_profiles.len(),
        REQUIRED_RENDERER_COVERAGE_OVERLAY_COUNT
    );
    assert_eq!(
        report.overlay_readiness.len(),
        REQUIRED_RENDERER_SCENARIO_COUNT * REQUIRED_RENDERER_COVERAGE_OVERLAY_COUNT,
        "readiness must contain every scenario-overlay pair"
    );

    let readiness_keys = report
        .overlay_readiness
        .iter()
        .map(|entry| (entry.scenario_id.as_str(), entry.overlay_id))
        .collect::<BTreeSet<_>>();
    assert_eq!(
        readiness_keys.len(),
        REQUIRED_RENDERER_SCENARIO_COUNT * REQUIRED_RENDERER_COVERAGE_OVERLAY_COUNT,
        "readiness pairs must be unique"
    );

    let mut live_cells = 0;
    let mut steady_cells = 0;
    let checkpoint_count = catalog
        .scenarios
        .iter()
        .map(|scenario| {
            assert_eq!(
                scenario.coverage_overlay_profile_ids.len(),
                REQUIRED_RENDERER_COVERAGE_OVERLAY_COUNT,
                "{} must bind all eight overlays",
                scenario.scenario_id
            );
            let expected = if is_live_resize_gesture(scenario.gesture) {
                live_cells += 1;
                REQUIRED_RENDERER_COVERAGE_OVERLAY_COUNT * 4
            } else {
                steady_cells += 1;
                REQUIRED_RENDERER_COVERAGE_OVERLAY_COUNT * 3
            };
            assert_eq!(
                scenario.visual_checkpoints.len(),
                expected,
                "{} checkpoint cardinality drifted",
                scenario.scenario_id
            );
            scenario.visual_checkpoints.len()
        })
        .sum::<usize>();
    assert_eq!((live_cells, steady_cells), (20, 12));
    assert_eq!(
        checkpoint_count, REQUIRED_RENDERER_CHECKPOINT_BINDING_COUNT,
        "20*8*4 + 12*8*3 must remain exactly 928"
    );
}

#[test]
fn scenario_ids_seeds_and_coverage_cells_are_exact_and_unique() {
    let root = repository_root();
    let catalog_value = load_catalog_value(&root);
    let catalog = load_catalog(&root);
    let wire_scenarios = catalog_value["scenarios"]
        .as_array()
        .expect("canonical scenarios must be an array");
    let mut cells = BTreeSet::new();
    let mut ids = BTreeSet::new();
    let mut seeds = BTreeSet::new();
    for (position, scenario) in catalog.scenarios.iter().enumerate() {
        assert_eq!(
            scenario.scenario_id,
            expected_renderer_scenario_id(scenario.gesture, scenario.fleet_point)
        );
        let expected_seed =
            expected_renderer_scenario_seed(scenario.gesture, scenario.fleet_point);
        assert_eq!(scenario.seed, expected_seed);
        let expected_wire_seed = format!("0x{expected_seed:016x}");
        assert_eq!(
            wire_scenarios[position]["seed"].as_str(),
            Some(expected_wire_seed.as_str()),
            "scenario seed wire encoding drifted at index {position}"
        );
        assert!(cells.insert((scenario.gesture, scenario.fleet_point)));
        assert!(ids.insert(scenario.scenario_id.as_str()));
        assert!(seeds.insert(scenario.seed));
    }
    let expected = RendererGesture::ALL
        .into_iter()
        .flat_map(|gesture| {
            RendererFleetPoint::ALL
                .into_iter()
                .map(move |fleet| (gesture, fleet))
        })
        .collect::<BTreeSet<_>>();
    assert_eq!(cells, expected);
}

#[test]
fn checkpoint_comparator_policies_are_exact_for_every_role() {
    let catalog = load_catalog(&repository_root());
    let cases: &[(&str, RendererCheckpointRole, &[&str])] = &[
        (
            "last Draft cannot acquire comparison authority",
            RendererCheckpointRole::LastDraftProvenance,
            &[RQ_S13_COMPARATOR_POLICY_REF],
        ),
        (
            "initial baseline cannot lose its comparator",
            RendererCheckpointRole::InitialBaseline,
            &[],
        ),
        (
            "intermediate checkpoint cannot lose its comparator",
            RendererCheckpointRole::Intermediate,
            &[],
        ),
        (
            "final checkpoint cannot lose its comparator",
            RendererCheckpointRole::FinalSteadyState,
            &[],
        ),
        (
            "snap-back subject requires both comparators",
            RendererCheckpointRole::StandardSnapBackSubject,
            &[RQ_S13_COMPARATOR_POLICY_REF],
        ),
        (
            "snap-back comparator order is canonical",
            RendererCheckpointRole::StandardSnapBackSubject,
            &[
                RQ_S13_COMPARATOR_POLICY_REF,
                RQ_S11_COMPARATOR_POLICY_REF,
            ],
        ),
        (
            "snap-back comparators cannot be replaced",
            RendererCheckpointRole::StandardSnapBackSubject,
            &[
                RQ_S11_COMPARATOR_POLICY_REF,
                "docs/perf/resize-quality-slo.json#wrong-policy",
            ],
        ),
    ];
    for &(label, role, policies) in cases {
        let mut mutated = catalog.clone();
        let checkpoint = mutated
            .scenarios
            .iter_mut()
            .flat_map(|scenario| scenario.visual_checkpoints.iter_mut())
            .find(|checkpoint| checkpoint.role == role)
            .unwrap_or_else(|| panic!("canonical catalog must contain {role:?}"));
        checkpoint.comparator_policy_refs = policies
            .iter()
            .map(|&policy| policy.to_owned())
            .collect();
        let report = mutated.validate();
        assert!(
            report.contains_code(RendererScenarioValidationCode::InvalidCheckpoint),
            "{label}: role-specific comparator mutation was not rejected:\n{}",
            validation_errors(&mutated)
        );
    }
}

#[test]
fn scenario_order_and_overlay_conditional_invariants_fail_closed() {
    let catalog = load_catalog(&repository_root());
    let mut swapped = catalog.clone();
    swapped.scenarios.swap(0, 1);
    assert_has_code(
        &swapped,
        RendererScenarioValidationCode::MissingRequiredCoverage,
    );

    let scenario_id = expected_renderer_scenario_id(
        RendererGesture::SameGridDrag,
        RendererFleetPoint::P001,
    );
    for overlay_id in RendererCoverageOverlayId::ALL {
        let resolved = resolve_renderer_scenario_overlay(&catalog, &scenario_id, overlay_id)
            .unwrap_or_else(|error| panic!("failed to resolve {}: {error}", overlay_id.as_str()));
        for anchor in &resolved.anchors {
            let derives_alternate = anchor.panes.iter().any(|pane| {
                pane.surface_state.terminal.active_buffer
                    == RendererTerminalBufferKind::Alternate
            });
            let derives_accessibility = anchor.panes.iter().any(|pane| {
                matches!(
                    pane.surface_state.terminal.accessibility_geometry,
                    RendererAccessibilityGeometryState::Active { .. }
                )
            });
            assert_eq!(
                anchor.expected_invariant_ids.iter().any(|id| id == "alternate_screen_isolation"),
                derives_alternate
            );
            assert_eq!(
                anchor.expected_invariant_ids.iter().any(|id| id == "accessibility_focus_geometry"),
                derives_accessibility
            );
        }
    }

    for (overlay_id, invariant_id, remove) in [
        (RendererCoverageOverlayId::ProductionDefault, "alternate_screen_isolation", false),
        (RendererCoverageOverlayId::AlternateScreen, "alternate_screen_isolation", true),
        (RendererCoverageOverlayId::ProductionDefault, "accessibility_focus_geometry", false),
        (RendererCoverageOverlayId::A11yGeometry, "accessibility_focus_geometry", true),
    ] {
        let mut mutated = catalog.clone();
        let checkpoint = mutated.scenarios[0]
            .visual_checkpoints
            .iter_mut()
            .find(|checkpoint| checkpoint.overlay_id == overlay_id)
            .expect("required overlay checkpoint exists");
        if remove {
            checkpoint.expected_invariant_ids.retain(|id| id != invariant_id);
        } else {
            checkpoint.expected_invariant_ids.push(invariant_id.to_string());
        }
        assert_has_code(&mutated, RendererScenarioValidationCode::InvalidCheckpoint);
    }
}

#[test]
fn hold_through_rejects_early_alternate_exit_and_replacement() {
    let catalog = load_catalog(&repository_root());
    for destructive_operation in [
        RendererContentCompositionOperation::ExitAlternateBuffer,
        RendererContentCompositionOperation::ReplaceActiveBuffer,
    ] {
        let mut mutated = catalog.clone();
        let profile = mutated
            .content_distribution_profiles
            .iter_mut()
            .find(|profile| profile.content_distribution_profile_id == "renderer.content.sg.p1.alt")
            .expect("canonical p001 alternate distribution exists");
        let steps = &mut profile.assignments[0].materialization_steps;
        let enter_position = steps
            .iter()
            .position(|step| {
                step.operation == RendererContentCompositionOperation::EnterAlternateBuffer
            })
            .expect("alternate distribution enters the alternate buffer");
        let mut destructive = steps[enter_position].clone();
        destructive.operation = destructive_operation;
        destructive.hold_through_checkpoint_ids.clear();
        destructive.step_ordinal = u16::try_from(enter_position + 1)
            .expect("small canonical materialization position fits u16");
        for step in steps.iter_mut().skip(enter_position + 1) {
            step.step_ordinal = step
                .step_ordinal
                .checked_add(1)
                .expect("canonical step ordinal has headroom");
        }
        steps.insert(enter_position + 1, destructive);
        assert_has_code(&mutated, RendererScenarioValidationCode::InvalidState);
    }

    let mut remove_then_reintroduce = catalog;
    let profile = remove_then_reintroduce
        .content_distribution_profiles
        .iter_mut()
        .find(|profile| profile.content_distribution_profile_id == "renderer.content.sg.p1.uni")
        .expect("canonical p001 Unicode distribution exists");
    let steps = &mut profile.assignments[0].materialization_steps;
    let settle_id = "renderer.cp.sg.p1.uni.settle".to_string();
    steps[1].hold_through_checkpoint_ids = vec![settle_id];
    let mut replace_away = steps[1].clone();
    replace_away.step_ordinal = 2;
    replace_away.operation = RendererContentCompositionOperation::ReplaceActiveBuffer;
    replace_away.application_boundary =
        RendererContentApplicationBoundary::AtEvent { event_ordinal: 1 };
    replace_away.hold_through_checkpoint_ids.clear();
    let mut reintroduce = steps[1].clone();
    reintroduce.step_ordinal = 3;
    reintroduce.application_boundary =
        RendererContentApplicationBoundary::AtEvent { event_ordinal: 101 };
    reintroduce.hold_through_checkpoint_ids.clear();
    steps[2].step_ordinal = 4;
    steps.insert(2, replace_away);
    steps.insert(3, reintroduce);
    let report = remove_then_reintroduce.validate();
    assert!(
        report.errors.iter().any(|error| {
            error.detail.contains("does not survive continuously through promised checkpoint")
        }),
        "remove-then-reintroduce must not satisfy a continuous hold promise"
    );
}

#[test]
fn out_of_range_explicit_rate_override_fails_without_panicking() {
    let mut catalog = load_catalog(&repository_root());
    let workload = catalog
        .workloads
        .iter_mut()
        .find(|workload| workload.workload_id == "renderer.workload.out.p1")
        .expect("canonical one-pane output workload exists");
    workload
        .output_stream
        .as_mut()
        .expect("output workload has a stream")
        .rate_overrides
        .push(RendererOutputRateOverride {
            selector: RendererPaneOrdinalSelector::Explicit { ordinals: vec![1] },
            bytes_per_second: 1_000_000,
        });
    assert_invalid_without_panic(&catalog, "out-of-range explicit rate override");
}

#[test]
fn foreground_key_target_must_be_the_exact_focused_pane() {
    let canonical = load_catalog(&repository_root());
    for (workload_id, invalid_target) in [
        ("renderer.workload.out.p1", "pane-999"),
        ("renderer.workload.out.p20", "pane-001"),
    ] {
        let mut mutated = canonical.clone();
        let key = mutated
            .workloads
            .iter_mut()
            .find(|workload| workload.workload_id == workload_id)
            .and_then(|workload| workload.foreground_key_events.first_mut())
            .expect("canonical output workload has a foreground key");
        key.target_pane_id = invalid_target.to_string();
        assert_has_code(&mutated, RendererScenarioValidationCode::InvalidWorkload);
    }
}

#[test]
fn foreground_key_tuple_is_frozen_jointly() {
    let canonical = load_catalog(&repository_root());
    for mutation in 0..3 {
        let mut catalog = canonical.clone();
        let key = catalog
            .workloads
            .iter_mut()
            .find(|workload| workload.workload_id == "renderer.workload.out.p1")
            .and_then(|workload| workload.foreground_key_events.first_mut())
            .expect("canonical output workload has a foreground key");
        match mutation {
            0 => key.logical_key = "X".to_string(),
            1 => key.modifiers = vec![RendererKeyModifier::Shift],
            2 => key.encoded_bytes_hex = "79".to_string(),
            _ => unreachable!(),
        }
        assert_has_code(&catalog, RendererScenarioValidationCode::InvalidWorkload);
    }
}

#[test]
fn public_resolver_is_deterministic_for_p001_and_p200() {
    let catalog = load_catalog(&repository_root());
    let known_content_ids = catalog
        .content_corpus_references
        .iter()
        .map(|entry| entry.content_corpus_id.as_str())
        .collect::<BTreeSet<_>>();
    for fleet_point in [RendererFleetPoint::P001, RendererFleetPoint::P200] {
        let scenario_id = expected_renderer_scenario_id(RendererGesture::SameGridDrag, fleet_point);
        let first = resolve_renderer_scenario_overlay(
            &catalog,
            &scenario_id,
            RendererCoverageOverlayId::ProductionDefault,
        )
        .unwrap_or_else(|error| panic!("failed to resolve {scenario_id}: {error}"));
        let second = resolve_renderer_scenario_overlay(
            &catalog,
            &scenario_id,
            RendererCoverageOverlayId::ProductionDefault,
        )
        .unwrap_or_else(|error| panic!("failed to resolve {scenario_id} again: {error}"));
        assert_eq!(first, second, "resolver output must be deterministic");
        assert_eq!(first.scenario_id, scenario_id);
        assert_eq!(first.gesture, RendererGesture::SameGridDrag);
        assert_resolved_overlay_topology(&first, fleet_point, &known_content_ids);
    }
}

#[test]
fn prepared_resolver_expands_all_256_pairs_with_one_validation_and_index_build() {
    let catalog = load_catalog(&repository_root());
    let prepared = prepare_renderer_scenario_catalog(&catalog)
        .unwrap_or_else(|error| panic!("failed to prepare canonical catalog: {error}"));
    let expected_stats = RendererResolverPreparationStats {
        semantic_validation_passes: 1,
        index_builds: 1,
    };
    assert_eq!(prepared.preparation_stats(), expected_stats);

    let batch = prepared
        .resolve_all_overlays()
        .unwrap_or_else(|error| panic!("failed to resolve canonical catalog batch: {error}"));
    assert_eq!(
        batch.len(),
        REQUIRED_RENDERER_SCENARIO_COUNT * REQUIRED_RENDERER_COVERAGE_OVERLAY_COUNT
    );

    let mut expected_pairs = Vec::with_capacity(
        REQUIRED_RENDERER_SCENARIO_COUNT * REQUIRED_RENDERER_COVERAGE_OVERLAY_COUNT,
    );
    for gesture in RendererGesture::ALL {
        for fleet_point in RendererFleetPoint::ALL {
            let scenario_id = expected_renderer_scenario_id(gesture, fleet_point);
            for overlay_id in RendererCoverageOverlayId::ALL {
                expected_pairs.push((scenario_id.clone(), overlay_id));
            }
        }
    }
    let actual_pairs = batch
        .iter()
        .map(|resolved| (resolved.scenario_id.clone(), resolved.overlay_id))
        .collect::<Vec<_>>();
    assert_eq!(actual_pairs, expected_pairs);

    for (batched, (scenario_id, overlay_id)) in batch.iter().zip(&expected_pairs) {
        let pairwise = prepared
            .resolve(scenario_id, *overlay_id)
            .unwrap_or_else(|error| {
                panic!(
                    "failed prepared pair resolution for {scenario_id}/{}: {error}",
                    overlay_id.as_str()
                )
            });
        assert_eq!(batched, &pairwise);
    }
    assert_eq!(prepared.preparation_stats(), expected_stats);
}

#[test]
fn p200_display_move_keeps_metadata_and_split_derived_geometry_disjoint() {
    let catalog = load_catalog(&repository_root());
    let scenario = catalog
        .scenarios
        .iter()
        .find(|scenario| {
            scenario.gesture == RendererGesture::DpiDisplayMove
                && scenario.fleet_point == RendererFleetPoint::P200
        })
        .expect("canonical p200 DPI scenario exists");
    let event = scenario
        .timeline
        .iter()
        .find(|event| {
            event
                .actions
                .iter()
                .any(|action| matches!(action, RendererTimelineAction::MoveToDisplay { .. }))
        })
        .expect("DPI scenario has a display mutation");
    let (target, width_px, height_px, display) = match event.actions.as_slice() {
        [
            RendererTimelineAction::SetWindowSize {
                target,
                width_px,
                height_px,
            },
            RendererTimelineAction::MoveToDisplay {
                target: display_target,
                display,
            },
            RendererTimelineAction::SetRevisions { .. },
        ] => {
            assert_eq!(target, display_target, "atomic display actions must share one target");
            (target, *width_px, *height_px, display)
        }
        actions => panic!("unexpected DPI mutation action order: {actions:?}"),
    };
    let serialized_display = serde_json::to_value(display).expect("transition serializes");
    let transition_keys = serialized_display
        .as_object()
        .expect("transition is an object")
        .keys()
        .map(String::as_str)
        .collect::<BTreeSet<_>>();
    assert_eq!(
        transition_keys,
        BTreeSet::from([
            "color_profile_ref",
            "color_space_id",
            "display_id",
            "dpi_milli",
            "dynamic_range_mode",
            "edr_available",
            "edr_headroom_milli",
            "scale_factor_milli",
        ]),
        "display transition must not grow viewport or padding fields"
    );

    let resolved = resolve_renderer_scenario_overlay(
        &catalog,
        &scenario.scenario_id,
        RendererCoverageOverlayId::ProductionDefault,
    )
    .expect("canonical p200 DPI production overlay resolves");
    let anchor = resolved
        .anchors
        .iter()
        .find(|anchor| anchor.event_ordinal == event.event_ordinal)
        .expect("display mutation has an exact resolved anchor");
    let window = anchor
        .windows
        .iter()
        .find(|window| window.window_id == target.window_id)
        .expect("display target window resolves");
    assert_eq!((window.drawable_rect.width, window.drawable_rect.height), (width_px, height_px));
    for pane in anchor
        .panes
        .iter()
        .filter(|pane| target.affected_pane_ids.contains(&pane.pane_id))
    {
        let state = &pane.surface_state.display;
        assert_eq!(state.display_id, display.display_id);
        assert_eq!(state.dpi_milli, display.dpi_milli);
        assert_eq!(state.scale_factor_milli, display.scale_factor_milli);
        assert_eq!(state.color_space_id, display.color_space_id);
        assert_eq!(state.color_profile_ref, display.color_profile_ref);
        assert_eq!(state.dynamic_range_mode, display.dynamic_range_mode);
        assert_eq!(state.edr_available, display.edr_available);
        assert_eq!(state.edr_headroom_milli, display.edr_headroom_milli);
        assert_eq!(state.viewport_width_px, pane.window_content_rect.width);
        assert_eq!(state.viewport_height_px, pane.window_content_rect.height);
    }
}

#[test]
fn retina_scale_applies_once_to_logical_dpi_cell_metrics_and_padding() {
    let catalog = load_catalog(&repository_root());
    let scenario_id = expected_renderer_scenario_id(
        RendererGesture::DpiDisplayMove,
        RendererFleetPoint::P001,
    );
    let resolved = resolve_renderer_scenario_overlay(
        &catalog,
        &scenario_id,
        RendererCoverageOverlayId::ProductionDefault,
    )
    .expect("canonical p001 DPI overlay resolves");
    let initial = &resolved
        .anchors
        .first()
        .expect("DPI overlay has an initial anchor")
        .panes[0]
        .surface_state;
    let retina = &resolved
        .anchors
        .last()
        .expect("DPI overlay has a final anchor")
        .panes[0]
        .surface_state;
    assert_eq!(initial.font.base_cell_width_milli_px, 8_000);
    assert_eq!(initial.font.metric_reference_dpi_milli, 96_000);
    assert_eq!(initial.display.dpi_milli, 96_000);
    assert_eq!(retina.display.dpi_milli, 96_000);
    assert_eq!(initial.display.scale_factor_milli, 1_000);
    assert_eq!(retina.display.scale_factor_milli, 2_000);

    let cell_width_milli = |state: &frankenterm_core::renderer_scenario_catalog::RendererSurfaceState| {
        u64::from(state.font.base_cell_width_milli_px)
            * u64::from(state.font.scale_milli)
            * u64::from(state.display.dpi_milli)
            * u64::from(state.display.scale_factor_milli)
            / (1_000_u64
                * 1_000
                * u64::from(state.font.metric_reference_dpi_milli))
    };
    assert_eq!(cell_width_milli(initial), 8_000);
    assert_eq!(cell_width_milli(retina), 16_000, "Retina must be 2x, not 4x");

    for (state, expected_cell_milli) in [(initial, 8_000_u64), (retina, 16_000_u64)] {
        let grid_width_px = u64::from(state.grid.columns) * expected_cell_milli / 1_000;
        assert_eq!(
            u64::from(state.display.content_padding_left_px)
                + grid_width_px
                + u64::from(state.display.content_padding_right_px),
            u64::from(state.display.viewport_width_px),
            "padding must consume the exact residual after the independently scaled grid"
        );
    }
}

#[test]
fn p200_atomic_surface_and_revision_targets_cannot_diverge() {
    let canonical = load_catalog(&repository_root());
    for gesture in RendererGesture::ALL {
        let scenario_id = expected_renderer_scenario_id(gesture, RendererFleetPoint::P200);
        let resolved = resolve_renderer_scenario_overlay(
            &canonical,
            &scenario_id,
            RendererCoverageOverlayId::ProductionDefault,
        )
        .unwrap_or_else(|error| panic!("failed to resolve {scenario_id}: {error}"));
        let topology = resolved.anchors.first().expect("p200 overlay has an anchor");
        let scenario_position = canonical
            .scenarios
            .iter()
            .position(|scenario| scenario.scenario_id == scenario_id)
            .expect("canonical p200 scenario exists");
        let event_position = canonical.scenarios[scenario_position]
            .timeline
            .iter()
            .position(|event| {
                let has_revision = event.actions.iter().any(|action| {
                    matches!(action, RendererTimelineAction::SetRevisions { .. })
                });
                let has_surface_change = event.actions.iter().any(|action| {
                    matches!(
                        action,
                        RendererTimelineAction::SetWindowSize { .. }
                            | RendererTimelineAction::SetGrid { .. }
                            | RendererTimelineAction::SetFontScale { .. }
                            | RendererTimelineAction::SetQualityMode { .. }
                            | RendererTimelineAction::MoveToDisplay { .. }
                    )
                });
                has_revision && has_surface_change
            })
            .unwrap_or_else(|| panic!("{scenario_id} has a surface+revision atomic event"));
        let revision_target = canonical.scenarios[scenario_position].timeline[event_position]
            .actions
            .iter()
            .find_map(|action| match action {
                RendererTimelineAction::SetRevisions { target, .. } => Some(target),
                _ => None,
            })
            .expect("selected event has revisions");
        let alternate_window = topology
            .windows
            .iter()
            .find(|window| window.window_id != revision_target.window_id)
            .expect("p200 has another valid window");
        let alternate_target = if revision_target.tab_id.is_some() {
            let tab = topology
                .tabs
                .iter()
                .find(|tab| tab.window_id == alternate_window.window_id)
                .expect("alternate window has a tab");
            RendererMutationTarget {
                window_id: alternate_window.window_id.clone(),
                tab_id: Some(tab.tab_id.clone()),
                affected_pane_ids: tab.ordered_pane_ids.clone(),
            }
        } else {
            RendererMutationTarget {
                window_id: alternate_window.window_id.clone(),
                tab_id: None,
                affected_pane_ids: topology
                    .panes
                    .iter()
                    .filter(|pane| pane.window_id == alternate_window.window_id)
                    .map(|pane| pane.pane_id.clone())
                    .collect(),
            }
        };

        let mut mutated = canonical.clone();
        let action = mutated.scenarios[scenario_position].timeline[event_position]
            .actions
            .iter_mut()
            .find(|action| matches!(action, RendererTimelineAction::SetRevisions { .. }))
            .expect("selected event has mutable revisions");
        let RendererTimelineAction::SetRevisions { target, .. } = action else {
            unreachable!();
        };
        *target = alternate_target;
        assert_has_code(&mutated, RendererScenarioValidationCode::InvalidTimeline);
    }
}

#[test]
fn structural_resolution_exposes_pair_scoped_execution_readiness() {
    let catalog = load_catalog(&repository_root());
    let report = catalog.validate();
    let scenario_id = expected_renderer_scenario_id(
        RendererGesture::SameGridDrag,
        RendererFleetPoint::P001,
    );
    let mut observed_not_ready = false;
    for overlay_id in RendererCoverageOverlayId::ALL {
        let resolved = resolve_renderer_scenario_overlay(&catalog, &scenario_id, overlay_id)
            .expect("valid structural pair must resolve");
        let expected = report
            .overlay_readiness
            .iter()
            .find(|entry| entry.scenario_id == scenario_id && entry.overlay_id == overlay_id)
            .expect("validation report contains every pair");
        assert_eq!(resolved.execution_ready, expected.execution_ready);
        assert_eq!(resolved.blocking_gap_codes, expected.blocking_gap_codes);
        assert_eq!(resolved.execution_ready, resolved.blocking_gap_codes.is_empty());
        assert_eq!(resolved.execution_ready, resolved.blocking_gaps.is_empty());
        observed_not_ready |= !resolved.execution_ready;
    }
    assert!(
        observed_not_ready,
        "canonical target-dependent overlays must not look runnable merely because structural resolution returned Ok"
    );
}

#[test]
fn output_and_key_authority_gaps_have_exact_pair_scope() {
    let catalog = load_catalog(&repository_root());
    let report = catalog.validate();
    assert!(report.valid, "authority gaps must not make the contract malformed");
    let output_scenarios = catalog
        .scenarios
        .iter()
        .filter(|scenario| scenario.gesture == RendererGesture::OutputOverlapResize)
        .map(|scenario| scenario.scenario_id.as_str())
        .collect::<BTreeSet<_>>();
    assert_eq!(output_scenarios.len(), 4);

    let output_gaps = report
        .gaps
        .iter()
        .filter(|gap| gap.code == RendererScenarioGapCode::DeterministicOutputStreamUnavailable)
        .collect::<Vec<_>>();
    assert_eq!(output_gaps.len(), 32, "four output cells x eight overlays");
    assert_eq!(
        output_gaps
            .iter()
            .map(|gap| {
                (
                    gap.scenario_id.as_deref().expect("pair gap has scenario"),
                    gap.overlay_id.expect("pair gap has overlay"),
                )
            })
            .collect::<BTreeSet<_>>()
            .len(),
        32
    );
    assert!(output_gaps.iter().all(|gap| {
        gap.tracking_ref == RENDERER_OUTPUT_AUTHORITY_TRACKING_REF
            && gap
                .scenario_id
                .as_deref()
                .is_some_and(|id| output_scenarios.contains(id))
    }));

    let key_gaps = report
        .gaps
        .iter()
        .filter(|gap| gap.code == RendererScenarioGapCode::KeyEffectOracleUnavailable)
        .collect::<Vec<_>>();
    assert_eq!(key_gaps.len(), 4, "one production-default key gap per output cell");
    assert!(key_gaps.iter().all(|gap| {
        gap.tracking_ref == RENDERER_OUTPUT_AUTHORITY_TRACKING_REF
            && gap.overlay_id == Some(RendererCoverageOverlayId::ProductionDefault)
            && gap
                .scenario_id
                .as_deref()
                .is_some_and(|id| output_scenarios.contains(id))
    }));

    for readiness in &report.overlay_readiness {
        let is_output = output_scenarios.contains(readiness.scenario_id.as_str());
        assert_eq!(
            readiness
                .blocking_gap_codes
                .contains(&RendererScenarioGapCode::DeterministicOutputStreamUnavailable),
            is_output
        );
        assert_eq!(
            readiness
                .blocking_gap_codes
                .contains(&RendererScenarioGapCode::KeyEffectOracleUnavailable),
            is_output && readiness.overlay_id == RendererCoverageOverlayId::ProductionDefault
        );
        if is_output {
            assert!(!readiness.execution_ready);
        }
    }

    let output_id = expected_renderer_scenario_id(
        RendererGesture::OutputOverlapResize,
        RendererFleetPoint::P001,
    );
    for (overlay_id, expects_key_gap) in [
        (RendererCoverageOverlayId::ProductionDefault, true),
        (RendererCoverageOverlayId::UnicodeMaximal, false),
    ] {
        let resolved = resolve_renderer_scenario_overlay(&catalog, &output_id, overlay_id)
            .expect("gap-blocked pair still resolves structurally");
        assert!(!resolved.execution_ready);
        assert!(resolved
            .blocking_gap_codes
            .contains(&RendererScenarioGapCode::DeterministicOutputStreamUnavailable));
        assert_eq!(
            resolved
                .blocking_gap_codes
                .contains(&RendererScenarioGapCode::KeyEffectOracleUnavailable),
            expects_key_gap
        );
    }
    let non_output_id = expected_renderer_scenario_id(
        RendererGesture::SameGridDrag,
        RendererFleetPoint::P001,
    );
    let non_output = resolve_renderer_scenario_overlay(
        &catalog,
        &non_output_id,
        RendererCoverageOverlayId::ProductionDefault,
    )
    .expect("non-output pair resolves structurally");
    assert!(!non_output
        .blocking_gap_codes
        .contains(&RendererScenarioGapCode::DeterministicOutputStreamUnavailable));
    assert!(!non_output
        .blocking_gap_codes
        .contains(&RendererScenarioGapCode::KeyEffectOracleUnavailable));
}

#[test]
fn hdr_capability_is_derived_per_overlay_not_per_scenario() {
    let mut catalog = load_catalog(&repository_root());
    let manifest_ids = catalog.scenarios[0]
        .visual_checkpoints
        .iter()
        .filter(|checkpoint| checkpoint.overlay_id == RendererCoverageOverlayId::A11yGeometry)
        .map(|checkpoint| checkpoint.phase_manifest_id.clone())
        .collect::<BTreeSet<_>>();
    let template_ids = catalog
        .phase_manifests
        .iter()
        .filter(|manifest| manifest_ids.contains(&manifest.phase_manifest_id))
        .flat_map(|manifest| {
            std::iter::once(manifest.default_surface_state_template_id.as_str()).chain(
                manifest
                    .pane_overrides
                    .iter()
                    .map(|pane| pane.surface_state_template_id.as_str()),
            )
        })
        .map(str::to_string)
        .collect::<BTreeSet<_>>();
    for template in &mut catalog.surface_state_templates {
        if template_ids.contains(&template.surface_state_template_id) {
            template.surface_state.display.dynamic_range_mode = RendererDynamicRangeMode::Hdr;
        }
    }
    let report = catalog.validate();
    let hdr_gap_overlays = report
        .gaps
        .iter()
        .filter(|gap| gap.detail.contains("`hdr_edr_output`"))
        .filter_map(|gap| gap.overlay_id)
        .collect::<BTreeSet<_>>();
    assert_eq!(
        hdr_gap_overlays,
        BTreeSet::from([RendererCoverageOverlayId::A11yGeometry]),
        "one HDR overlay must not promote HDR capability to all eight pairs"
    );
}

#[test]
fn public_resolver_rejects_unknown_scenario_and_reordered_window_rows() {
    let root = repository_root();
    let catalog = load_catalog(&root);
    let unknown = "renderer.same_grid_drag.p999";
    assert!(matches!(
        resolve_renderer_scenario_overlay(
            &catalog,
            unknown,
            RendererCoverageOverlayId::ProductionDefault
        ),
        Err(RendererScenarioResolveError::ScenarioNotFound { scenario_id })
            if scenario_id == unknown
    ));

    let scenario_id = expected_renderer_scenario_id(
        RendererGesture::SameGridDrag,
        RendererFleetPoint::P200,
    );
    let resolved = resolve_renderer_scenario_overlay(
        &catalog,
        &scenario_id,
        RendererCoverageOverlayId::ProductionDefault,
    )
    .expect("canonical p200 scenario should resolve");
    let manifest_id = resolved
        .anchors
        .first()
        .expect("live-resize overlay has anchors")
        .phase_manifest_id
        .clone();
    let mut reordered = catalog;
    let manifest = reordered
        .phase_manifests
        .iter_mut()
        .find(|manifest| manifest.phase_manifest_id == manifest_id)
        .expect("resolved manifest exists in the source catalog");
    assert!(manifest.window_states.len() > 1);
    manifest.window_states.reverse();
    let error = resolve_renderer_scenario_overlay(
        &reordered,
        &scenario_id,
        RendererCoverageOverlayId::ProductionDefault,
    )
    .expect_err("reordered window rows must fail closed before expansion");
    match error {
        RendererScenarioResolveError::InvalidCatalog { report } => {
            assert!(!report.valid && !report.errors.is_empty());
        }
        other => panic!("expected invalid-catalog error, got {other:?}"),
    }
}

#[test]
fn all_frame_detector_sets_are_exact_per_overlay() {
    let catalog = load_catalog(&repository_root());
    for scenario in &catalog.scenarios {
        assert_eq!(
            scenario.observed_frame_policies.len(),
            REQUIRED_RENDERER_COVERAGE_OVERLAY_COUNT,
            "{} must carry one observation policy per overlay",
            scenario.scenario_id
        );
        let actual = scenario
            .observed_frame_policies
            .iter()
            .map(|policy| (policy.overlay_id, policy.all_frame_detector_ids.as_slice()))
            .collect::<BTreeMap<_, _>>();
        for overlay_id in RendererCoverageOverlayId::ALL {
            let expected = expected_all_frame_detectors(overlay_id);
            assert_eq!(
                actual.get(&overlay_id).copied(),
                Some(expected.as_slice()),
                "{} {} all-frame detector subset drifted",
                scenario.scenario_id,
                overlay_id.as_str()
            );
        }
    }
}

#[test]
fn exact_negative_control_catalog_is_frozen() {
    let catalog = load_catalog(&repository_root());
    assert_eq!(catalog.negative_controls.len(), 13);
    assert_eq!(
        catalog.feature_evidence_bindings.len(),
        REQUIRED_RENDERER_TERMINAL_FEATURE_COUNT,
        "feature evidence must account for all 13 terminal features"
    );
    let actual_ids = catalog
        .negative_controls
        .iter()
        .map(|control| control.control_id)
        .collect::<Vec<_>>();
    assert_eq!(actual_ids.as_slice(), RendererNegativeControlId::ALL);
    let actual_bindings = catalog
        .negative_controls
        .iter()
        .map(|control| (control.control_id, control.bound_detector_id))
        .collect::<Vec<_>>();
    let expected_bindings = [
        (
            RendererNegativeControlId::MissingGlyph,
            RendererCheckpointDetectorId::NoMissingGlyphs,
        ),
        (
            RendererNegativeControlId::MixedRendererGeneration,
            RendererCheckpointDetectorId::CoherentRendererGeneration,
        ),
        (
            RendererNegativeControlId::CursorDisplacement,
            RendererCheckpointDetectorId::CursorGeometry,
        ),
        (
            RendererNegativeControlId::SelectionLoss,
            RendererCheckpointDetectorId::SelectionGeometry,
        ),
        (
            RendererNegativeControlId::StaleImage,
            RendererCheckpointDetectorId::ImageGeometry,
        ),
        (
            RendererNegativeControlId::ImeGeometryDisplacement,
            RendererCheckpointDetectorId::ImeGeometry,
        ),
        (
            RendererNegativeControlId::HyperlinkRangeCorruption,
            RendererCheckpointDetectorId::HyperlinkGeometry,
        ),
        (
            RendererNegativeControlId::AlternateScreenFlip,
            RendererCheckpointDetectorId::AlternateScreenState,
        ),
        (
            RendererNegativeControlId::GridDimensionMismatch,
            RendererCheckpointDetectorId::ExactRowWidth,
        ),
        (
            RendererNegativeControlId::DuplicateStaleFrame,
            RendererCheckpointDetectorId::NoStaleOrDuplicateFrame,
        ),
        (
            RendererNegativeControlId::AccessibilityGeometryDisplacement,
            RendererCheckpointDetectorId::AccessibilityGeometry,
        ),
        (
            RendererNegativeControlId::BlankFrameAfterNonblank,
            RendererCheckpointDetectorId::NonblankAfterBaseline,
        ),
        (
            RendererNegativeControlId::MixedGenerationTearBand,
            RendererCheckpointDetectorId::NoMixedGenerationTearBand,
        ),
    ];
    assert_eq!(actual_bindings.as_slice(), expected_bindings);
    for control in &catalog.negative_controls {
        assert_eq!(
            control.expected_failure_code,
            control.control_id.expected_failure_code(),
            "{} failure-code binding drifted",
            control.control_id.as_str()
        );
    }
}

#[test]
fn every_repository_and_bead_reference_resolves() {
    let root = repository_root();
    let value = load_catalog_value(&root);
    let mut repository_refs = BTreeSet::new();
    let mut bead_refs = BTreeSet::new();
    collect_catalog_references(
        &value,
        None,
        &mut repository_refs,
        &mut bead_refs,
    );
    let unresolved = unresolved_repository_refs(&root, repository_refs);
    assert!(
        unresolved.is_empty(),
        "catalog contains missing paths or absent/ambiguous fragments: {}",
        unresolved.join(", ")
    );

    let known = issue_ids(&root);
    let missing = bead_refs
        .into_iter()
        .filter(|bead_id| !known.contains(bead_id))
        .collect::<Vec<_>>();
    assert!(
        missing.is_empty(),
        "catalog references Beads absent from {ISSUES_RELATIVE_PATH}: {}",
        missing.join(", ")
    );

    let broken = unresolved_repository_refs(
        &root,
        ["README.md#definitely-not-a-real-heading".to_string()],
    );
    assert_eq!(broken, ["README.md#definitely-not-a-real-heading"]);
}

#[test]
fn canonical_payload_sources_selectors_decoders_and_digests_match_bytes() {
    let root = repository_root();
    let catalog = load_catalog(&root);
    assert_eq!(
        catalog.content_corpus_references.len(),
        9,
        "version 1 freezes six checked-in payloads plus three tracked generators"
    );
    let payload_count = catalog
        .content_corpus_references
        .iter()
        .filter(|entry| {
            matches!(
                &entry.deterministic_identity,
                RendererContentDeterministicIdentity::Payload { .. }
            )
        })
        .count();
    assert_eq!(payload_count, 6);
    let generator_ids = catalog
        .content_corpus_references
        .iter()
        .filter_map(|entry| {
            matches!(
                &entry.deterministic_identity,
                RendererContentDeterministicIdentity::Generator { .. }
            )
            .then_some(entry.content_corpus_id.as_str())
        })
        .collect::<BTreeSet<_>>();
    assert_eq!(
        generator_ids,
        BTreeSet::from([
            "content.a11y_geometry_gap.v1",
            "content.ligature_enabled_gap.v1",
            "content.live_ime_gap.v1",
        ]),
        "only the three named canonical generators are v1 input gaps"
    );
    assert_payload_digests(&root, &catalog);
}

#[test]
fn bounded_decoder_rejects_every_ambiguous_document_shape() {
    let root = repository_root();
    let raw = load_catalog_bytes(&root);

    for malformed in [b"".as_slice(), b"{".as_slice(), b"null".as_slice()] {
        assert_eq!(
            RendererScenarioCatalog::decode_json_bounded(malformed)
                .expect_err("malformed document must be rejected")
                .code(),
            RendererScenarioDecodeCode::InvalidJson
        );
    }

    let mut unknown = load_catalog_value(&root);
    unknown["silent_native_pass"] = json!(true);
    assert_eq!(
        RendererScenarioCatalog::decode_json_bounded(
            &serde_json::to_vec(&unknown).expect("unknown-field mutation should encode")
        )
        .expect_err("unknown root field must be rejected")
        .code(),
        RendererScenarioDecodeCode::InvalidJson
    );

    let raw_text = std::str::from_utf8(&raw).expect("checked-in catalog JSON is UTF-8");
    let object_start = raw_text
        .find('{')
        .expect("checked-in catalog contains a root object");
    let duplicate = format!(
        "{}{{\"contract_id\":\"ft.renderer_scenario_catalog.v1\",{}",
        &raw_text[..object_start],
        &raw_text[object_start + 1..]
    );
    assert_eq!(
        RendererScenarioCatalog::decode_json_bounded(duplicate.as_bytes())
            .expect_err("duplicate struct field must be rejected")
            .code(),
        RendererScenarioDecodeCode::InvalidJson
    );

    let mut trailing = raw.clone();
    trailing.extend_from_slice(b"\n{}");
    assert_eq!(
        RendererScenarioCatalog::decode_json_bounded(&trailing)
            .expect_err("trailing second value must be rejected")
            .code(),
        RendererScenarioDecodeCode::TrailingData
    );

    let oversized = vec![b' '; MAX_RENDERER_SCENARIO_CATALOG_BYTES + 1];
    assert_eq!(
        RendererScenarioCatalog::decode_json_bounded(&oversized)
            .expect_err("oversized document must be rejected before parsing")
            .code(),
        RendererScenarioDecodeCode::PayloadTooLarge
    );
}

#[test]
fn every_schema_required_nullable_field_rejects_omission() {
    let root = repository_root();
    let schema = load_schema_validator(&root);
    let canonical = load_schema_clean_catalog(&root, &schema);
    for pointer in [
        "/surface_state_templates/9/surface_state/terminal/ime/candidate_window_geometry",
        "/surface_state_templates/21/surface_state/terminal/accessibility_geometry/caret_rect",
        "/scenarios/0/timeline/1/actions/0/target/tab_id",
        "/workloads/28/output_stream/stream_id",
        "/scenarios/0/visual_checkpoints/0/independent_standard_oracle_ref",
        "/evidence_sources/0/limitation",
        "/feature_evidence_bindings/0/sources/0/limitation",
        "/gesture_authority_map/0/sources/0/limitation",
        "/workloads/0/output_stream",
        "/negative_controls/0/required_feature",
        "/scenarios/0/output_overlap_resize_mode",
        "/rq_s1_synthetic_substrate",
    ] {
        let mut value = canonical.clone();
        let (parent_pointer, key) = pointer
            .rsplit_once('/')
            .expect("test pointer names a parent and field");
        value
            .pointer_mut(parent_pointer)
            .and_then(Value::as_object_mut)
            .and_then(|object| object.remove(key))
            .unwrap_or_else(|| panic!("canonical catalog contains `{pointer}`"));
        assert!(
            !schema_errors(&schema, &value).is_empty(),
            "schema accepted omitted required-nullable field `{pointer}`"
        );
        let encoded = serde_json::to_vec(&value).expect("omission mutation encodes");
        assert_eq!(
            RendererScenarioCatalog::decode_json_bounded(&encoded)
                .expect_err("typed decoder must reject omitted required-nullable field")
                .code(),
            RendererScenarioDecodeCode::InvalidJson,
            "typed decoder accepted omission at `{pointer}`"
        );
    }
}

#[test]
fn schema_and_decoder_reject_checkpoint_detector_authority_drift() {
    let root = repository_root();
    let validator = load_schema_validator(&root);
    let canonical = load_schema_clean_catalog(&root, &validator);

    let mut checkpoint_field = canonical.clone();
    checkpoint_field["scenarios"][0]["visual_checkpoints"][0]
        .as_object_mut()
        .expect("first checkpoint is an object")
        .insert(
            "expected_detector_ids".to_string(),
            json!(["no_missing_glyphs"]),
        );
    assert!(
        !schema_errors(&validator, &checkpoint_field).is_empty(),
        "checkpoint rows must reject the removed second detector authority"
    );
    assert_eq!(
        RendererScenarioCatalog::decode_json_bounded(
            &serde_json::to_vec(&checkpoint_field).expect("checkpoint mutation should encode")
        )
        .expect_err("typed decoder must reject checkpoint expected_detector_ids")
        .code(),
        RendererScenarioDecodeCode::InvalidJson
    );

    let mut all_frames_binding = canonical;
    all_frames_binding["scenarios"][0]["detector_bindings"]
        .as_array_mut()
        .expect("detector_bindings is an array")
        .push(json!({
            "scope": "all_observed_frames",
            "overlay_id": "production_default",
            "detector_id": "no_missing_glyphs"
        }));
    assert!(
        !schema_errors(&validator, &all_frames_binding).is_empty(),
        "all-observed-frame detectors belong only to observation policies"
    );
    assert_eq!(
        RendererScenarioCatalog::decode_json_bounded(
            &serde_json::to_vec(&all_frames_binding).expect("binding mutation should encode")
        )
        .expect_err("typed decoder must reject a serialized nonlocal all-frame binding")
        .code(),
        RendererScenarioDecodeCode::InvalidJson
    );
}

#[test]
fn schema_rejects_unknown_versions_fields_and_missing_required_collections() {
    let root = repository_root();
    let validator = load_schema_validator(&root);
    let canonical = load_schema_clean_catalog(&root, &validator);

    let mut unknown_version = canonical.clone();
    unknown_version["schema_version"] = json!(2);
    assert!(!schema_errors(&validator, &unknown_version).is_empty());

    let mut old_catalog_revision = canonical.clone();
    old_catalog_revision["catalog_revision"] = json!(1);
    assert!(!schema_errors(&validator, &old_catalog_revision).is_empty());

    let mut unknown_field = canonical.clone();
    unknown_field["native_target_passed"] = json!(true);
    assert!(!schema_errors(&validator, &unknown_field).is_empty());

    let mut missing_scenarios = canonical.clone();
    missing_scenarios
        .as_object_mut()
        .expect("catalog is an object")
        .remove("scenarios");
    assert!(!schema_errors(&validator, &missing_scenarios).is_empty());

    let mut bad_overlay = canonical;
    bad_overlay["coverage_overlay_profiles"][0]["overlay_id"] = json!("future_overlay");
    assert!(!schema_errors(&validator, &bad_overlay).is_empty());
}

#[test]
fn schema_and_decoder_reject_ambiguous_renderer_seed_encodings() {
    let root = repository_root();
    let validator = load_schema_validator(&root);
    let canonical = load_schema_clean_catalog(&root, &validator);
    for invalid_seed in [
        json!(5_067_765_997_134_473_000_u64),
        json!("5067765997134479361"),
        json!("0X4654525300010001"),
        json!("0x465452530001000A"),
        json!("0x465452530001001"),
    ] {
        let mut value = canonical.clone();
        value["scenarios"][0]["seed"] = invalid_seed.clone();
        assert!(
            !schema_errors(&validator, &value).is_empty(),
            "schema accepted ambiguous renderer seed {invalid_seed}"
        );
        let encoded = serde_json::to_vec(&value).expect("seed mutation should encode");
        assert_eq!(
            RendererScenarioCatalog::decode_json_bounded(&encoded)
                .expect_err("typed decoder must reject ambiguous renderer seed")
                .code(),
            RendererScenarioDecodeCode::InvalidJson,
            "typed decoder accepted ambiguous renderer seed {invalid_seed}"
        );
    }
}

#[test]
fn semantic_validator_rejects_identity_layout_content_and_timeline_drift() {
    let root = repository_root();

    let mut revision = load_catalog(&root);
    revision.catalog_revision = 1;
    assert_has_code(
        &revision,
        RendererScenarioValidationCode::UnknownCatalogRevision,
    );

    let mut identity = load_catalog(&root);
    identity.scenarios[0].seed ^= 1;
    assert_has_code(&identity, RendererScenarioValidationCode::InvalidSeed);

    let mut layout = load_catalog(&root);
    layout.layout_profiles[0].pane_count += 1;
    assert_invalid(&layout, "layout pane-count drift");

    let mut content = load_catalog(&root);
    let payload = content
        .content_corpus_references
        .iter_mut()
        .find_map(|entry| match &mut entry.deterministic_identity {
            RendererContentDeterministicIdentity::Payload {
                encoded_payload_sha256,
                ..
            } => Some(encoded_payload_sha256),
            RendererContentDeterministicIdentity::Generator { .. } => None,
        })
        .expect("catalog contains a payload identity");
    let replacement = if payload.starts_with('0') { "1" } else { "0" };
    payload.replace_range(..1, replacement);
    assert_has_code(
        &content,
        RendererScenarioValidationCode::InvalidCorpusReference,
    );

    let mut timeline = load_catalog(&root);
    let first_ordinal = timeline.scenarios[0].timeline[0].event_ordinal;
    timeline.scenarios[0].timeline[1].event_ordinal = first_ordinal;
    assert_has_code(&timeline, RendererScenarioValidationCode::InvalidTimeline);
}

#[test]
fn semantic_validator_rejects_detector_rq_capability_authority_and_control_drift() {
    let root = repository_root();

    let mut detector = load_catalog(&root);
    detector.scenarios[0].observed_frame_policies[0]
        .all_frame_detector_ids
        .remove(0);
    assert_invalid(&detector, "all-frame detector subset drift");

    let mut requirement = load_catalog(&root);
    requirement.scenarios[0].requirement_bindings.clear();
    assert_has_code(
        &requirement,
        RendererScenarioValidationCode::InvalidRequirementCrosswalk,
    );

    let mut capability = load_catalog(&root);
    capability.scenarios[0].capabilities.pop();
    assert_has_code(
        &capability,
        RendererScenarioValidationCode::InvalidCapabilityMatrix,
    );

    let mut authority = load_catalog(&root);
    authority
        .accessibility_authority_boundary
        .machine_geometry_authorizes_native_accessibility = true;
    assert_has_code(
        &authority,
        RendererScenarioValidationCode::InvalidGestureAuthority,
    );

    let mut control = load_catalog(&root);
    control.negative_controls[0].expected_failure_code = "RSC-CONTROL-999".to_string();
    assert_has_code(
        &control,
        RendererScenarioValidationCode::InvalidNegativeControl,
    );
}

#[test]
fn backward_selection_is_a_valid_positive_case() {
    let root = repository_root();
    let catalog = load_catalog(&root);
    let has_backward_selection = catalog.surface_state_templates.iter().any(|template| {
        matches!(
            &template.surface_state.terminal.selection,
            RendererSelectionState::Active { anchor, focus, .. }
                if (anchor.row, anchor.column) > (focus.row, focus.column)
        )
    });
    assert!(
        has_backward_selection,
        "catalog must retain an in-bounds backward anchor-to-focus selection fixture"
    );
    let report = catalog.validate();
    assert!(
        report.valid,
        "backward anchor-to-focus selections are valid and must not be sorted: {}",
        validation_errors(&catalog)
    );
}

#[test]
fn zero_counts_and_empty_core_vectors_fail_closed_without_panicking() {
    let root = repository_root();
    let mutations: [CatalogMutationCase; 25] = [
        ("zero scenario pane count", |catalog| {
            catalog.scenarios[0].pane_count = 0;
        }),
        ("zero scenario tab count", |catalog| {
            catalog.scenarios[0].tab_count = 0;
        }),
        ("zero scenario window count", |catalog| {
            catalog.scenarios[0].window_count = 0;
        }),
        ("zero layout pane count", |catalog| {
            catalog.layout_profiles[0].pane_count = 0;
        }),
        ("zero layout tab count", |catalog| {
            catalog.layout_profiles[0].tab_count = 0;
        }),
        ("zero layout window count", |catalog| {
            catalog.layout_profiles[0].window_count = 0;
        }),
        ("zero workload pane count", |catalog| {
            catalog.workloads[0].pane_count = 0;
        }),
        ("zero workload tab count", |catalog| {
            catalog.workloads[0].tab_count = 0;
        }),
        ("zero workload window count", |catalog| {
            catalog.workloads[0].window_count = 0;
        }),
        ("empty content corpus", |catalog| {
            catalog.content_corpus_references.clear();
        }),
        ("empty workloads", |catalog| {
            catalog.workloads.clear();
        }),
        ("empty renderer configurations", |catalog| {
            catalog.renderer_config_profiles.clear();
        }),
        ("empty layout profiles", |catalog| {
            catalog.layout_profiles.clear();
        }),
        ("empty surface templates", |catalog| {
            catalog.surface_state_templates.clear();
        }),
        ("empty content distributions", |catalog| {
            catalog.content_distribution_profiles.clear();
        }),
        ("empty phase manifests", |catalog| {
            catalog.phase_manifests.clear();
        }),
        ("empty overlay profiles", |catalog| {
            catalog.coverage_overlay_profiles.clear();
        }),
        ("empty detector contracts", |catalog| {
            catalog.detector_contracts.clear();
        }),
        ("empty scenarios", |catalog| {
            catalog.scenarios.clear();
        }),
        ("empty scenario timeline", |catalog| {
            catalog.scenarios[0].timeline.clear();
        }),
        ("empty scenario checkpoints", |catalog| {
            catalog.scenarios[0].visual_checkpoints.clear();
        }),
        ("empty scenario observation policies", |catalog| {
            catalog.scenarios[0].observed_frame_policies.clear();
        }),
        ("empty scenario requirement crosswalk", |catalog| {
            catalog.scenarios[0].requirement_bindings.clear();
        }),
        ("empty scenario capability matrix", |catalog| {
            catalog.scenarios[0].capabilities.clear();
        }),
        ("empty negative controls", |catalog| {
            catalog.negative_controls.clear();
        }),
    ];
    for (label, mutate) in mutations {
        let mut catalog = load_catalog(&root);
        mutate(&mut catalog);
        assert_invalid_without_panic(&catalog, label);
    }
}

#[test]
fn every_finite_scenario_and_control_mutation_is_exhaustive() {
    let canonical = load_catalog(&repository_root());
    assert_eq!(canonical.scenarios.len(), REQUIRED_RENDERER_SCENARIO_COUNT);
    for index in 0..canonical.scenarios.len() {
        let mut removed = canonical.clone();
        removed.scenarios.remove(index);
        assert_has_code(&removed, RendererScenarioValidationCode::MissingRequiredCoverage);

        let mut duplicated = canonical.clone();
        duplicated.scenarios.push(canonical.scenarios[index].clone());
        let report = duplicated.validate();
        assert!(report.contains_code(RendererScenarioValidationCode::DuplicateCoverageCell));
        assert!(report.contains_code(RendererScenarioValidationCode::DuplicateId));
    }

    assert_eq!(canonical.negative_controls.len(), 13);
    for index in 0..canonical.negative_controls.len() {
        let mut removed = canonical.clone();
        removed.negative_controls.remove(index);
        assert_has_code(&removed, RendererScenarioValidationCode::InvalidNegativeControl);

        let mut duplicated = canonical.clone();
        duplicated
            .negative_controls
            .push(canonical.negative_controls[index].clone());
        assert_has_code(&duplicated, RendererScenarioValidationCode::InvalidNegativeControl);
    }
}
