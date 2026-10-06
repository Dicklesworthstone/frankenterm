//! CI runner for the differential terminal-engine harness (ft-yccm0.1.8).
//!
//! The self-tests plant deliberately divergent engines and require the
//! harness to catch and minimize them, so the harness cannot pass vacuously.
//! The remaining tests check every registered candidate against the legacy
//! engine on the M.1 corpora, the escape-parser fuzz seeds, adversarial cases
//! and a fixed-seed random campaign. See `tests/differential/mod.rs` for the
//! long-campaign settings.

#[path = "differential/mod.rs"]
#[allow(dead_code)]
mod differential;

use std::path::PathBuf;
use std::time::Duration;

use differential::ddmin::ddmin;
use differential::engine::{
    candidates, new_terminal, Engine, EngineFactory, Geometry, Legacy, GEOMETRIES,
};
use differential::harness::{self, minimize, run_lockstep, Repro};
use differential::snapshot::{self, EngineSnapshot};
use differential::streams::{self, ChunkPlan};
use frankenterm_escape_parser::parser::Parser;
use frankenterm_term::Terminal;

/// A broken candidate that never sees `ESC [ 7 m` (reverse video) within a
/// chunk.
struct IgnoresReverseVideo;

struct IgnoresReverseVideoEngine {
    terminal: Terminal,
}

impl Engine for IgnoresReverseVideoEngine {
    fn feed(&mut self, bytes: &[u8]) {
        let mut filtered = Vec::with_capacity(bytes.len());
        let mut rest = bytes;
        while !rest.is_empty() {
            if rest.starts_with(b"\x1b[7m") {
                rest = &rest[4..];
            } else {
                filtered.push(rest[0]);
                rest = &rest[1..];
            }
        }
        self.terminal.advance_bytes(&filtered);
    }

    fn snapshot(&self) -> EngineSnapshot {
        snapshot::capture(&self.terminal)
    }
}

impl EngineFactory for IgnoresReverseVideo {
    fn name(&self) -> &'static str {
        "mock_ignores_reverse_video"
    }

    fn build(&self, geometry: &Geometry) -> Box<dyn Engine> {
        Box::new(IgnoresReverseVideoEngine {
            terminal: new_terminal(geometry),
        })
    }
}

/// A broken candidate that forgets parser state at every chunk boundary, so
/// it diverges only when a sequence is split across chunks.
struct FreshParserEachChunk;

struct FreshParserEachChunkEngine {
    terminal: Terminal,
}

impl Engine for FreshParserEachChunkEngine {
    fn feed(&mut self, bytes: &[u8]) {
        let actions = Parser::new().parse_as_vec(bytes);
        self.terminal.perform_actions(actions);
    }

    fn snapshot(&self) -> EngineSnapshot {
        snapshot::capture(&self.terminal)
    }
}

impl EngineFactory for FreshParserEachChunk {
    fn name(&self) -> &'static str {
        "mock_fresh_parser_each_chunk"
    }

    fn build(&self, geometry: &Geometry) -> Box<dyn Engine> {
        Box::new(FreshParserEachChunkEngine {
            terminal: new_terminal(geometry),
        })
    }
}

fn total_bytes(chunks: &[Vec<u8>]) -> usize {
    chunks.iter().map(Vec::len).sum()
}

fn test_out_dir(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!(
        "engine-differential-{}-{}",
        name,
        std::process::id()
    ))
}

#[test]
fn the_harness_catches_and_minimizes_a_content_divergence() {
    let geometry = GEOMETRIES[1];
    // The mock filters the trigger only within one chunk, so keep it whole.
    let mut chunks = Vec::new();
    for _ in 0..20 {
        chunks.push(b"plain output line\r\n".to_vec());
    }
    chunks.push(b"\x1b[7mREVERSED\x1b[0m tail\r\n".to_vec());
    for _ in 0..20 {
        chunks.push(b"more plain output\r\n".to_vec());
    }

    // Identical engines never diverge; the broken one does.
    assert_eq!(run_lockstep(&Legacy, &Legacy, &geometry, &chunks), None);
    let divergence = run_lockstep(&Legacy, &IgnoresReverseVideo, &geometry, &chunks)
        .expect("the harness must catch the planted divergence");
    assert_eq!(divergence.candidate, "mock_ignores_reverse_video");
    assert!(!divergence.description.is_empty());

    let minimized = minimize(&Legacy, &IgnoresReverseVideo, &geometry, chunks);
    let bytes: Vec<u8> = minimized.concat();
    assert!(
        run_lockstep(&Legacy, &IgnoresReverseVideo, &geometry, &minimized).is_some(),
        "the minimized input must still diverge"
    );
    assert!(
        bytes.windows(4).any(|window| window == b"\x1b[7m"),
        "the trigger survives minimization: {:?}",
        bytes
    );
    assert!(
        total_bytes(&minimized) <= 5,
        "ddmin should shrink the input to the trigger, got {:?}",
        minimized
    );
}

#[test]
fn the_harness_catches_and_minimizes_a_chunk_boundary_divergence() {
    let geometry = GEOMETRIES[1];
    let stream = b"start \x1b[31mred\x1b[0m \x1b]0;title\x07end \xe4\xb8\xad".to_vec();

    // Whole-input delivery never splits a sequence, so it cannot catch this.
    assert_eq!(
        run_lockstep(
            &Legacy,
            &FreshParserEachChunk,
            &geometry,
            &streams::chunk(&stream, ChunkPlan::Whole, 0)
        ),
        None
    );
    let chunks = streams::chunk(&stream, ChunkPlan::Bytewise, 0);
    let divergence = run_lockstep(&Legacy, &FreshParserEachChunk, &geometry, &chunks)
        .expect("splitting sequences must expose the planted divergence");
    assert!(divergence.chunk_index > 0);

    let minimized = minimize(&Legacy, &FreshParserEachChunk, &geometry, chunks);
    assert!(run_lockstep(&Legacy, &FreshParserEachChunk, &geometry, &minimized).is_some());
    assert!(
        minimized.len() >= 2,
        "a chunk-boundary bug needs at least two chunks: {:?}",
        minimized
    );
    assert!(
        total_bytes(&minimized) <= 4,
        "ddmin should shrink the input to a split sequence, got {:?}",
        minimized
    );
}

#[test]
fn reproducers_round_trip_and_replay() {
    let geometry = GEOMETRIES[0];
    let chunks = vec![b"\x1b".to_vec(), b"[".to_vec(), Vec::new(), b"7mX".to_vec()];
    let repro = Repro {
        candidate: "mock_fresh_parser_each_chunk".to_string(),
        geometry,
        chunks: chunks.clone(),
        description: "planted".to_string(),
    };
    let parsed = Repro::from_bytes(&repro.to_bytes()).expect("parse");
    assert_eq!(parsed.candidate, repro.candidate);
    assert_eq!(parsed.geometry, geometry);
    assert_eq!(parsed.chunks, chunks);

    let dir = test_out_dir("repro");
    let (repro_path, diff_path) = repro.write(&dir, "planted-case").expect("write");
    let from_disk = Repro::from_bytes(&std::fs::read(&repro_path).expect("read")).expect("parse");
    assert_eq!(from_disk.chunks, chunks);
    let diff = std::fs::read_to_string(diff_path).expect("read diff");
    assert!(diff.contains("planted"), "{}", diff);
    assert!(diff.contains("\\x1b"), "chunks are shown escaped: {}", diff);
    assert!(
        run_lockstep(
            &Legacy,
            &FreshParserEachChunk,
            &from_disk.geometry,
            &from_disk.chunks
        )
        .is_some(),
        "the replayed reproducer diverges again"
    );

    assert!(Repro::from_bytes(b"not a reproducer\n\n").is_err());
    assert!(Repro::from_bytes(
        b"ft-engine-differential-repro v1\ncandidate=x\ngeometry=1,2,3\nchunks=5\n\nabc"
    )
    .is_err());
}

#[test]
fn ddmin_reaches_a_one_minimal_subset() {
    let input: Vec<u32> = (0..40).collect();
    let mut calls = 0;
    let result = ddmin(input, 10_000, &mut |list: &[u32]| {
        calls += 1;
        list.contains(&3) && list.contains(&17) && list.contains(&31)
    });
    assert_eq!(result, vec![3, 17, 31]);
    assert!(calls > 0);

    let single = ddmin(vec![1, 2, 3, 4], 100, &mut |list: &[u32]| list.contains(&4));
    assert_eq!(single, vec![4]);

    // An exhausted budget still returns a failing input.
    let partial = ddmin((0..64).collect::<Vec<u32>>(), 3, &mut |list: &[u32]| {
        list.contains(&63)
    });
    assert!(partial.contains(&63));
}

#[test]
fn snapshots_capture_pending_wrap_modes_and_cell_attributes() {
    let geometry = GEOMETRIES[0];
    let mut wrapped = Legacy.build(&geometry);
    wrapped.feed(b"123456789");
    let mut plain = Legacy.build(&geometry);
    plain.feed(b"12345678");
    let wrapped = wrapped.snapshot();
    let plain = plain.snapshot();
    let mode = |snapshot: &EngineSnapshot, name: &str| {
        snapshot
            .modes
            .iter()
            .find(|(key, _)| *key == name)
            .map(|(_, value)| value.clone())
            .unwrap_or_else(|| panic!("mode {} missing", name))
    };
    assert_eq!(mode(&wrapped, "wrap_next"), "true");
    assert_eq!(mode(&plain, "wrap_next"), "false");
    assert!(snapshot::describe_difference(&wrapped, &plain).is_some());

    let mut styled = Legacy.build(&geometry);
    styled.feed(b"\x1b[4hab\x1b[1;31mc");
    let styled = styled.snapshot();
    assert_eq!(mode(&styled, "insert"), "true");
    let row = styled.rows.last().expect("rows");
    let top = &styled.rows[styled.rows.len() - geometry.rows];
    assert!(row.runs.is_empty(), "the bottom row is blank");
    assert_eq!(top.runs.len(), 2, "plain and bold-red runs: {:?}", top.runs);
    assert_eq!(top.runs[0].text, "ab");
    assert_eq!(top.runs[1].text, "c");
    assert_ne!(top.runs[0].attrs, top.runs[1].attrs);
}

fn check_all(name: &str, geometry: &Geometry, chunks: &[Vec<u8>]) {
    if let Err(report) = harness::check(name, geometry, chunks) {
        panic!("{}", report);
    }
}

#[test]
fn every_candidate_matches_legacy_on_the_m1_corpora() {
    assert!(!candidates().is_empty());
    for (index, (name, bytes)) in streams::m1_corpora(4 * 1024).into_iter().enumerate() {
        let geometry = GEOMETRIES[index % GEOMETRIES.len()];
        check_all(
            name,
            &geometry,
            &streams::chunk(&bytes, ChunkPlan::Whole, 0),
        );
        for (seed, max) in [(1_u64, 7_usize), (2, 64), (3, 4096)].iter() {
            let chunks = streams::chunk(&bytes, ChunkPlan::Random { max: *max }, *seed);
            check_all(&format!("{}-random{}", name, max), &geometry, &chunks);
        }
        // Every byte split on a prefix keeps the debug-build runtime bounded.
        let prefix = &bytes[..bytes.len().min(512)];
        check_all(
            &format!("{}-bytewise", name),
            &geometry,
            &streams::chunk(prefix, ChunkPlan::Bytewise, 0),
        );
    }
}

#[test]
fn every_candidate_matches_legacy_on_the_escape_parser_fuzz_seeds() {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fuzz/corpus/escape_parser_raw");
    let seeds = streams::escape_parser_seeds(&dir).expect("read the escape-parser fuzz corpus");
    assert!(
        seeds.len() >= 100,
        "only {} seeds in {}",
        seeds.len(),
        dir.display()
    );
    for (index, (name, bytes)) in seeds.iter().enumerate() {
        let geometry = GEOMETRIES[index % GEOMETRIES.len()];
        check_all(name, &geometry, &streams::chunk(bytes, ChunkPlan::Whole, 0));
        check_all(
            name,
            &geometry,
            &streams::chunk(bytes, ChunkPlan::Bytewise, 0),
        );
    }
}

#[test]
fn every_candidate_matches_legacy_on_adversarial_cases() {
    for (index, (name, bytes)) in streams::adversarial_cases().into_iter().enumerate() {
        let geometry = GEOMETRIES[index % GEOMETRIES.len()];
        check_all(
            name,
            &geometry,
            &streams::chunk(&bytes, ChunkPlan::Whole, 0),
        );
        check_all(
            name,
            &geometry,
            &streams::chunk(&bytes, ChunkPlan::Random { max: 64 }, index as u64),
        );
        if bytes.len() <= 128 {
            check_all(
                name,
                &geometry,
                &streams::chunk(&bytes, ChunkPlan::Bytewise, 0),
            );
            for chunks in streams::every_split(&bytes) {
                check_all(name, &geometry, &chunks);
            }
        }
    }
}

fn env_number(name: &str, default: u64) -> u64 {
    match std::env::var(name) {
        Ok(value) => value
            .parse()
            .unwrap_or_else(|_| panic!("{} must be a number, got {:?}", name, value)),
        Err(_) => default,
    }
}

#[test]
fn fixed_seed_campaign() {
    let seconds = env_number("FT_DIFFERENTIAL_SECONDS", 20);
    let max_cases = env_number("FT_DIFFERENTIAL_CASES", 400) as usize;
    let first_seed = env_number("FT_DIFFERENTIAL_SEED", 1);
    let campaign = harness::run_campaign(first_seed, max_cases, Duration::from_secs(seconds))
        .unwrap_or_else(|report| panic!("{}", report));
    eprintln!(
        "[BENCH] engine_differential campaign: {} cases, {} bytes, seeds {}..{}, {} candidates",
        campaign.cases,
        campaign.bytes,
        first_seed,
        first_seed + campaign.cases as u64,
        candidates().len()
    );
    assert!(
        campaign.cases >= max_cases.min(10),
        "the campaign ran only {} cases",
        campaign.cases
    );
}
