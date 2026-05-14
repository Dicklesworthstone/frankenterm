//! Regression guards for ft-e87u6.7 README attestation cross-links.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use regex::Regex;
use serde::Deserialize;

#[derive(Debug, Deserialize)]
struct Manifest {
    slots: Vec<ManifestSlot>,
}

#[derive(Clone, Debug, Deserialize)]
struct ManifestSlot {
    category: String,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    produced_by_bead: Option<String>,
    #[serde(default)]
    deferred_to_bead: Option<String>,
}

#[derive(Debug)]
struct ClaimMapRow {
    index: usize,
    claim: String,
    slot_cell: String,
    bead_cell: String,
}

fn workspace_root() -> PathBuf {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest_dir
        .parent()
        .and_then(Path::parent)
        .expect("workspace root exists")
        .to_path_buf()
}

fn read_workspace_file(path: &str) -> String {
    let full_path = workspace_root().join(path);
    fs::read_to_string(&full_path)
        .unwrap_or_else(|err| panic!("failed to read {}: {err}", full_path.display()))
}

fn manifest_slots_by_category() -> BTreeMap<String, Vec<ManifestSlot>> {
    let manifest: Manifest =
        serde_json::from_str(&read_workspace_file("docs/attestations/manifest.json"))
            .expect("docs/attestations/manifest.json parses");
    let mut slots_by_category: BTreeMap<String, Vec<ManifestSlot>> = BTreeMap::new();
    for slot in manifest.slots {
        slots_by_category
            .entry(slot.category.clone())
            .or_default()
            .push(slot);
    }
    slots_by_category
}

fn valid_bead_ids() -> BTreeSet<String> {
    manifest_slots_by_category()
        .into_values()
        .flatten()
        .filter_map(|slot| slot.produced_by_bead)
        .collect()
}

fn why_use_section(readme: &str) -> &str {
    let heading = "### Why Use ft?";
    let start = readme
        .find(heading)
        .unwrap_or_else(|| panic!("README.md is missing {heading:?}"));
    let after_heading = &readme[start + heading.len()..];
    let end = after_heading
        .find("\n---")
        .expect("README.md Why Use ft? section terminator exists");
    &after_heading[..end]
}

fn footnote_references_in_why_use(readme: &str) -> BTreeSet<String> {
    let refs = Regex::new(r"\[\^([A-Za-z0-9_.-]+)\]").expect("footnote ref regex compiles");
    let mut anchors = BTreeSet::new();
    for line in why_use_section(readme).lines() {
        if line.trim_start().starts_with("[^") {
            continue;
        }
        for cap in refs.captures_iter(line) {
            anchors.insert(cap[1].to_string());
        }
    }
    anchors
}

fn footnote_definitions(readme: &str) -> BTreeMap<String, String> {
    let defs =
        Regex::new(r"^\[\^([A-Za-z0-9_.-]+)\]:\s*(.+)$").expect("footnote def regex compiles");
    let mut definitions = BTreeMap::new();
    for line in readme.lines() {
        if let Some(cap) = defs.captures(line) {
            definitions.insert(cap[1].to_string(), cap[2].trim().to_string());
        }
    }
    definitions
}

fn manifest_categories_in_text(
    text: &str,
    slots_by_category: &BTreeMap<String, Vec<ManifestSlot>>,
) -> BTreeSet<String> {
    let code_spans = Regex::new(r"`([^`]+)`").expect("code-span regex compiles");
    code_spans
        .captures_iter(text)
        .filter_map(|cap| {
            let candidate = cap[1].trim();
            slots_by_category
                .contains_key(candidate)
                .then(|| candidate.to_string())
        })
        .collect()
}

fn assert_populated_manifest_category(
    category: &str,
    slots_by_category: &BTreeMap<String, Vec<ManifestSlot>>,
) {
    let Some(slots) = slots_by_category.get(category) else {
        panic!("README cites non-existent manifest slot category {category}");
    };
    let deferred: Vec<String> = slots
        .iter()
        .filter(|slot| slot.path.is_none())
        .map(|slot| {
            slot.deferred_to_bead
                .clone()
                .unwrap_or_else(|| "missing deferred_to_bead".to_string())
        })
        .collect();
    assert!(
        deferred.is_empty(),
        "README cites manifest slot category {category}, but at least one matching slot is deferred: {}",
        deferred.join(", ")
    );
    assert!(
        slots.iter().all(|slot| slot.path.is_some()),
        "README cites manifest slot category {category}, but it has no populated path"
    );
}

fn claim_map_rows(readme: &str) -> Vec<ClaimMapRow> {
    let start_marker = "<!-- attestation-claim-map:start -->";
    let end_marker = "<!-- attestation-claim-map:end -->";
    let start = readme
        .find(start_marker)
        .unwrap_or_else(|| panic!("README.md is missing {start_marker}"));
    let after_start = &readme[start + start_marker.len()..];
    let end = after_start
        .find(end_marker)
        .unwrap_or_else(|| panic!("README.md is missing {end_marker}"));
    let mut rows = Vec::new();
    for line in after_start[..end].lines() {
        let trimmed = line.trim();
        if !trimmed.starts_with('|')
            || trimmed.contains("README claim")
            || trimmed
                .chars()
                .all(|ch| matches!(ch, '|' | '-' | ':' | ' '))
        {
            continue;
        }
        let cells: Vec<String> = trimmed
            .trim_matches('|')
            .split('|')
            .map(|cell| cell.trim().to_string())
            .collect();
        assert_eq!(
            cells.len(),
            3,
            "attestation claim-map row must have exactly 3 cells: {line}"
        );
        rows.push(ClaimMapRow {
            index: rows.len() + 1,
            claim: cells[0].clone(),
            slot_cell: cells[1].clone(),
            bead_cell: cells[2].clone(),
        });
    }
    rows
}

fn bead_ids_in_text(text: &str) -> BTreeSet<String> {
    let bead = Regex::new(r"\bft-[a-z0-9]+(?:\.[0-9]+)*\b").expect("bead regex compiles");
    bead.captures_iter(text)
        .map(|cap| cap[0].to_string())
        .collect()
}

#[test]
fn every_why_use_footnote_anchors_to_resolved_manifest_slot() {
    let readme = read_workspace_file("README.md");
    let slots_by_category = manifest_slots_by_category();
    let anchors = footnote_references_in_why_use(&readme);
    let definitions = footnote_definitions(&readme);

    assert!(
        !anchors.is_empty(),
        "README.md Why Use ft? section must carry attestation footnote references"
    );

    for anchor in anchors {
        let def = definitions
            .get(&anchor)
            .unwrap_or_else(|| panic!("orphan footnote reference in Why Use ft?: {anchor}"));
        let categories = manifest_categories_in_text(def, &slots_by_category);
        assert!(
            !categories.is_empty(),
            "footnote {anchor} does not cite a manifest slot category: {def:?}"
        );
        for category in &categories {
            assert_populated_manifest_category(category, &slots_by_category);
        }
        println!(
            "footnote.resolve.{anchor} categories={}",
            categories.into_iter().collect::<Vec<_>>().join(",")
        );
    }
}

#[test]
fn trust_attestation_claim_map_matches_manifest() {
    let readme = read_workspace_file("README.md");
    let slots_by_category = manifest_slots_by_category();
    let bead_ids = valid_bead_ids();
    let rows = claim_map_rows(&readme);

    assert!(
        !rows.is_empty(),
        "README.md Trust & Attestation claim-map table must have at least one row"
    );

    for row in rows {
        let categories = manifest_categories_in_text(&row.slot_cell, &slots_by_category);
        assert_eq!(
            categories.len(),
            1,
            "Trust & Attestation row {} must cite exactly one manifest slot category: {:?}",
            row.index,
            row.slot_cell
        );
        let row_beads = bead_ids_in_text(&row.bead_cell);
        assert_eq!(
            row_beads.len(),
            1,
            "Trust & Attestation row {} must cite exactly one producing bead: {:?}",
            row.index,
            row.bead_cell
        );
        let category = categories
            .iter()
            .next()
            .expect("category length was asserted");
        let bead = row_beads.iter().next().expect("bead length was asserted");
        assert!(
            bead_ids.contains(bead),
            "Trust & Attestation row {} cites bead {bead}, which is not a produced_by_bead value in manifest.json",
            row.index
        );
        let slots = slots_by_category
            .get(category)
            .expect("category was extracted from manifest categories");
        let matching_slots: Vec<&ManifestSlot> = slots
            .iter()
            .filter(|slot| slot.produced_by_bead.as_deref() == Some(bead.as_str()))
            .collect();
        assert!(
            !matching_slots.is_empty(),
            "Trust & Attestation row {} claims {category} is produced by {bead}, but manifest.json disagrees",
            row.index
        );
        assert!(
            matching_slots.iter().all(|slot| slot.path.is_some()),
            "Trust & Attestation row {} links to deferred slot {category} produced by {bead}",
            row.index
        );
        println!(
            "trust_attestation_table.row.{} claim={} category={} bead={}",
            row.index, row.claim, category, bead
        );
    }
}
