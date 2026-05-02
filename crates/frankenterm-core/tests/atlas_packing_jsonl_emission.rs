//! Integration test: end-to-end JSONL telemetry emission for one
//! atlas-packing scenario (ft-gtcm9.4).
//!
//! Runs a uniform-glyph fill scenario through
//! [`PackingScenarioRecorder`], writes the JSONL stream to a
//! tempfile, and asserts every emitted line satisfies the
//! ft-i1y15 schema:
//!
//! ```text
//! { ts, glyph_id, glyph_size, allocation_pos, packer,
//!   wasted_space_after_alloc, total_atlas_bytes }
//! ```
//!
//! The file path the original bead names
//! (`tests/atlas_packing/logs/<scenario>.jsonl`) is honoured at
//! the *operator* level — a CI step that wants to keep the file
//! around can copy it from the test's tempdir output. The test
//! itself uses a tempdir so parallel test runs do not contend on
//! the same path.

use std::fs::File;
use std::io::{BufRead, BufReader, BufWriter};

use frankenterm_core::atlas_bin_packing::{
    AllocationOutcome, Atlas2DSize, GlyphSize, PackerKind, make_packer,
};
use frankenterm_core::atlas_packing_telemetry::{PackingEvent, PackingScenarioRecorder};
use tempfile::tempdir;

const SCENARIO: &str = "uniform_8x8";

#[test]
fn jsonl_emission_for_uniform_glyph_scenario_matches_schema() {
    let tmp = tempdir().expect("tempdir");
    let logs_dir = tmp.path().join("atlas_packing").join("logs");
    std::fs::create_dir_all(&logs_dir).expect("logs dir");
    let path = logs_dir.join(format!("{SCENARIO}.jsonl"));

    // 64x64 atlas, 8x8 glyphs — a clean fill with a known alloc count.
    let atlas = Atlas2DSize::try_new(64, 64).expect("atlas size");
    let total_atlas_bytes: u64 = atlas.area();
    let glyph = GlyphSize::new(8, 8);
    let glyph_count: u64 = 64; // 8*8 grid, fits exactly

    let mut placed_count: u64 = 0;
    {
        let file = File::create(&path).expect("create jsonl");
        let mut rec = PackingScenarioRecorder::new(
            make_packer(PackerKind::Skyline, atlas),
            BufWriter::new(file),
        );
        for id in 0..glyph_count {
            match rec.record_alloc(id, glyph).expect("record") {
                AllocationOutcome::Placed(_) => placed_count += 1,
                AllocationOutcome::Rejected(_) => {}
            }
        }
        rec.flush().expect("flush");
    }

    let file = File::open(&path).expect("reopen jsonl");
    let lines: Vec<String> = BufReader::new(file)
        .lines()
        .map(|l| l.expect("read line"))
        .collect();

    assert_eq!(
        lines.len() as u64,
        glyph_count,
        "one JSONL line per recorded alloc; got {} lines for {} allocs",
        lines.len(),
        glyph_count,
    );
    assert!(placed_count > 0, "scenario must place at least one glyph");

    for (idx, line) in lines.iter().enumerate() {
        let event: PackingEvent = serde_json::from_str(line)
            .unwrap_or_else(|e| panic!("line {idx} not valid JSONL: {e}; raw: {line}"));
        // ft-i1y15 schema fields:
        assert_eq!(event.glyph_id, idx as u64, "glyph_id ordering");
        assert_eq!(event.glyph_size.width, 8);
        assert_eq!(event.glyph_size.height, 8);
        assert_eq!(event.packer, "Skyline");
        assert_eq!(event.total_atlas_bytes, total_atlas_bytes);
        // wasted_space_after_alloc is monotone non-increasing across
        // a uniform-fill scenario.
        if idx > 0 {
            let prev: PackingEvent = serde_json::from_str(&lines[idx - 1]).unwrap();
            assert!(
                event.wasted_space_after_alloc <= prev.wasted_space_after_alloc,
                "uniform fill must not see wasted space increase between events {} and {}",
                idx - 1,
                idx,
            );
        }
    }

    // Last event's wasted space + sum of placed bytes equals total
    // atlas bytes (the fill consumes 64 * 8*8 = 4096 = atlas area).
    let last: PackingEvent = serde_json::from_str(lines.last().unwrap()).unwrap();
    let placed_bytes: u64 = placed_count * 8 * 8;
    assert_eq!(
        last.wasted_space_after_alloc + placed_bytes,
        total_atlas_bytes,
        "wasted + used must close the atlas-area accounting"
    );
}

#[test]
fn jsonl_emission_records_rejection_with_typed_reason() {
    let tmp = tempdir().expect("tempdir");
    let path = tmp.path().join("rejection.jsonl");
    let atlas = Atlas2DSize::try_new(32, 32).expect("atlas");
    {
        let file = File::create(&path).expect("create");
        let mut rec = PackingScenarioRecorder::new(
            make_packer(PackerKind::Shelf, atlas),
            BufWriter::new(file),
        );
        // First an oversized glyph (rejected), then a fitting one.
        rec.record_alloc(0, GlyphSize::new(64, 8)).expect("record"); // wider
        rec.record_alloc(1, GlyphSize::new(8, 8)).expect("record");
    }
    let lines: Vec<String> = BufReader::new(File::open(&path).expect("reopen"))
        .lines()
        .map(|l| l.expect("line"))
        .collect();
    assert_eq!(lines.len(), 2);
    let rejected: PackingEvent = serde_json::from_str(&lines[0]).unwrap();
    assert_eq!(rejected.allocation_pos, None);
    assert_eq!(
        rejected.reject_reason.as_deref(),
        Some("GlyphWiderThanAtlas"),
        "rejection reason carried as stable string label"
    );
    let placed: PackingEvent = serde_json::from_str(&lines[1]).unwrap();
    assert!(placed.allocation_pos.is_some());
    assert_eq!(placed.reject_reason, None);
}
