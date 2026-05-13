//! Regression guards for ft-e87u6.4 README/AGENTS hedge alignment.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use regex::Regex;
use serde::Deserialize;

#[derive(Debug, Deserialize)]
struct Manifest {
    slots: Vec<ManifestSlot>,
}

#[derive(Debug, Deserialize)]
struct ManifestSlot {
    category: String,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    deferred_to_bead: Option<String>,
}

struct HedgePattern {
    category: &'static str,
    regex: &'static str,
}

const HEDGE_PATTERNS: &[HedgePattern] = &[
    HedgePattern {
        category: "perf/headline-claims",
        regex: r"memory-envelope claims should be treated as benchmark-dependent",
    },
    HedgePattern {
        category: "perf/headline-claims",
        regex: r"200\+ pane memory-envelope claims still require passing target-hardware proof",
    },
    HedgePattern {
        category: "perf/headline-claims",
        regex: r"savings vary by payload and should be treated as workload-dependent until linked benchmark artifacts are published",
    },
    HedgePattern {
        category: "perf/resource-cockpit-target-class",
        regex: r"hardware- and workload-dependent until the run cites a live cockpit artifact",
    },
];

const ORPHAN_HEDGE_PATTERNS: &[&str] = &[
    r"should be treated as",
    r"treated as benchmark-dependent",
    r"should be treated as workload-dependent",
    r"workload-dependent until",
    r"benchmark-dependent until",
    r"until linked artifact(?:s)?(?: are)? published",
    r"until linked benchmark artifacts are published",
];

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

fn manifest_slots_by_category() -> HashMap<String, ManifestSlot> {
    let manifest: Manifest =
        serde_json::from_str(&read_workspace_file("docs/attestations/manifest.json"))
            .expect("docs/attestations/manifest.json parses");
    manifest
        .slots
        .into_iter()
        .map(|slot| (slot.category.clone(), slot))
        .collect()
}

fn docs() -> Vec<(&'static str, String)> {
    vec![
        ("README.md", read_workspace_file("README.md")),
        ("AGENTS.md", read_workspace_file("AGENTS.md")),
    ]
}

fn line_number(text: &str, byte_offset: usize) -> usize {
    text[..byte_offset]
        .bytes()
        .filter(|byte| *byte == b'\n')
        .count()
        + 1
}

fn surrounding_paragraph(text: &str, start: usize, end: usize) -> &str {
    let before = text[..start].rfind("\n\n").map(|idx| idx + 2).unwrap_or(0);
    let after = text[end..]
        .find("\n\n")
        .map(|idx| end + idx)
        .unwrap_or(text.len());
    &text[before..after]
}

#[test]
fn populated_manifest_slots_do_not_carry_legacy_linked_artifact_hedges() {
    let slots = manifest_slots_by_category();
    let docs = docs();

    for pattern in HEDGE_PATTERNS {
        let Some(slot) = slots.get(pattern.category) else {
            continue;
        };
        let regex = Regex::new(pattern.regex).expect("hedge regex compiles");
        let mut matches = Vec::new();
        for (file, text) in &docs {
            for found in regex.find_iter(text) {
                matches.push(format!(
                    "{file}:{}: {}",
                    line_number(text, found.start()),
                    surrounding_paragraph(text, found.start(), found.end())
                ));
            }
        }

        match (&slot.path, &slot.deferred_to_bead) {
            (Some(_), _) => assert!(
                matches.is_empty(),
                "slot {} has a populated path but still carries legacy hedge text:\n{}",
                pattern.category,
                matches.join("\n---\n")
            ),
            (None, Some(bead_id)) => {
                for entry in matches {
                    assert!(
                        entry.contains(bead_id),
                        "slot {} is deferred to {} but hedge does not cite it:\n{}",
                        pattern.category,
                        bead_id,
                        entry
                    );
                }
            }
            (None, None) => panic!(
                "manifest slot {} has neither path nor deferred_to_bead",
                pattern.category
            ),
        }
    }
}

#[test]
fn no_orphan_hedge_text_escapes_the_pattern_table() {
    let docs = docs();
    let known_patterns: Vec<Regex> = HEDGE_PATTERNS
        .iter()
        .map(|pattern| Regex::new(pattern.regex).expect("known hedge regex compiles"))
        .collect();

    for (file, text) in &docs {
        for needle in ORPHAN_HEDGE_PATTERNS {
            let regex = Regex::new(needle).expect("orphan hedge regex compiles");
            for found in regex.find_iter(text) {
                let paragraph = surrounding_paragraph(text, found.start(), found.end());
                let registered = known_patterns.iter().any(|known| known.is_match(paragraph));
                assert!(
                    registered,
                    "orphan hedge text at {file}:{} matches no HEDGE_PATTERNS entry; either add a manifest slot + table row, or rewrite the hedge:\n{}",
                    line_number(text, found.start()),
                    paragraph
                );
            }
        }
    }
}

#[test]
fn positive_attestation_breadcrumbs_are_visible_to_readers() {
    let readme = read_workspace_file("README.md");
    let agents = read_workspace_file("AGENTS.md");
    let worksheet = read_workspace_file("docs/attestations/hedge-lift-worksheet.md");

    for doc in [&readme, &agents] {
        assert!(
            doc.contains("docs/attestations/manifest.json"),
            "README.md and AGENTS.md must both expose the manifest doorway"
        );
    }

    for required in [
        "ft-tf6g3.14",
        "ft-tf6g3.1",
        "ft-0zoq3",
        "docs/perf-ledger/toon-encoding.md",
        "tests/artifacts/perf/toon-encoding-ft-0zoq3/",
    ] {
        assert!(
            readme.contains(required) || worksheet.contains(required),
            "hedge audit evidence is missing required breadcrumb {required}"
        );
    }
}
