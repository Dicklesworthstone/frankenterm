#![no_main]

//! [ft-hfbsp] Fuzz target for `simd_scan::scan_newlines_and_ansi`.
//!
//! Every byte of every captured pane delta passes through this
//! function, including bytes from agent processes whose output is
//! treated as untrusted. The SIMD/memchr path has the classic UB
//! hotspots fuzzing exists to catch — chunk-tail off-by-one,
//! integer overflow on adversarial ANSI parameters, alignment
//! crashes on stricter targets, quadratic blowup on
//! `\x1b[\x1b[\x1b[…` recursive escape introducer prefixes.
//!
//! ## Contracts pinned
//!
//! 1. **SIMD vs scalar equivalence.** The byte-for-byte scalar
//!    `reference_scan` mirrors the SIMD path's ANSI state machine.
//!    Any divergence (`SIMD/memchr scan drifted from scalar
//!    reference`) is a cross-architecture regression.
//! 2. **`logical_line_count` is consistent with newline_count.**
//!    Final-newline edge case (`...\n` vs `...`) asserted both ways.
//! 3. **`ansi_density` is finite and bounded to `[0.0, 1.0]`.** No
//!    NaN, no Inf, no >1 (would imply more ANSI bytes than total).
//! 4. **Chunk-stitching equivalence.** Splitting the input at a
//!    fuzzer-controlled index and feeding each half through
//!    `scan_newlines_and_ansi_with_state` must produce metrics
//!    that sum to the full-buffer scan. Multi-split mode (added in
//!    ft-hfbsp) extends this from one split point to up to N,
//!    amplifying chunk-boundary coverage where SIMD tail-loop bugs
//!    typically hide.
//!
//! Matches the harness shape of ipc_auth_envelope and wire_envelope
//! (ft-h8v8v): Archetype 5 structure-aware Arbitrary + Archetype 1
//! crash detector.

use arbitrary::Arbitrary;
use frankenterm_core::simd_scan::{
    OutputScanMetrics, OutputScanState, scan_newlines_and_ansi, scan_newlines_and_ansi_with_state,
};
use libfuzzer_sys::fuzz_target;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum ReferenceStringPhase {
    #[default]
    None,
    Body,
    SawEsc,
}

#[derive(Clone, Copy, Debug, Default)]
struct ReferenceAnsiState {
    in_escape: bool,
    string_phase: ReferenceStringPhase,
}

fn reference_is_string_intro_byte(byte: u8) -> bool {
    matches!(byte, b'P' | b'X' | b']' | b'^' | b'_')
}

fn reference_ansi_step(byte: u8, state: &mut ReferenceAnsiState) -> bool {
    if byte == 0x1b {
        if state.in_escape
            && matches!(
                state.string_phase,
                ReferenceStringPhase::Body | ReferenceStringPhase::SawEsc
            )
        {
            state.string_phase = ReferenceStringPhase::SawEsc;
        }
        state.in_escape = true;
        return true;
    }

    if !state.in_escape {
        return false;
    }

    match state.string_phase {
        ReferenceStringPhase::None => {
            if reference_is_string_intro_byte(byte) {
                state.string_phase = ReferenceStringPhase::Body;
            } else if byte == b'[' {
                // CSI introducer; stay in the escape sequence.
            } else if (0x40..=0x7e).contains(&byte) || byte >= 0x7f {
                state.in_escape = false;
            }
        }
        ReferenceStringPhase::Body => {
            if byte == 0x07 {
                state.string_phase = ReferenceStringPhase::None;
                state.in_escape = false;
            }
        }
        ReferenceStringPhase::SawEsc => {
            if byte == b'\\' {
                state.string_phase = ReferenceStringPhase::None;
                state.in_escape = false;
            } else {
                state.string_phase = ReferenceStringPhase::Body;
            }
        }
    }

    true
}

fn reference_scan(bytes: &[u8]) -> OutputScanMetrics {
    let mut metrics = OutputScanMetrics::default();
    let mut state = ReferenceAnsiState::default();

    for &byte in bytes {
        if byte == b'\n' {
            metrics.newline_count += 1;
        }
        if reference_ansi_step(byte, &mut state) {
            metrics.ansi_byte_count += 1;
        }
    }

    metrics
}

fn split_index(bytes: &[u8]) -> usize {
    if bytes.is_empty() {
        return 0;
    }
    usize::from(bytes[0]) % (bytes.len() + 1)
}

/// Compute up to `MAX_SPLITS` strictly-increasing split offsets in
/// `[0, bytes.len()]` from `seeds`. Returns offsets in ascending
/// order so the for-loop in the multi-split harness can advance a
/// cursor. Bounded so libFuzzer doesn't spend coverage budget on
/// pathologically-many tiny chunks.
fn multi_split_offsets(bytes: &[u8], seeds: &[u8]) -> Vec<usize> {
    const MAX_SPLITS: usize = 8;
    if bytes.is_empty() || seeds.is_empty() {
        return Vec::new();
    }
    let mut offsets: Vec<usize> = seeds
        .iter()
        .take(MAX_SPLITS)
        .map(|seed| usize::from(*seed) % (bytes.len() + 1))
        .collect();
    offsets.sort_unstable();
    offsets.dedup();
    offsets
}

#[derive(Arbitrary, Debug)]
struct FuzzInput<'a> {
    /// The bytes the SIMD scanner consumes.
    bytes: &'a [u8],
    /// Up to MAX_SPLITS offsets at which to chunk the input for the
    /// multi-split state-stitching invariant. Bytes are interpreted
    /// modulo `bytes.len()+1` and sorted/deduped.
    multi_split_seeds: &'a [u8],
}

fuzz_target!(|input: FuzzInput| {
    let bytes = input.bytes;
    let scan = scan_newlines_and_ansi(bytes);
    let reference = reference_scan(bytes);
    assert_eq!(
        scan, reference,
        "SIMD/memchr scan drifted from scalar reference"
    );

    let logical_lines = scan.logical_line_count(bytes);
    let expected_lines = if bytes.is_empty() {
        0
    } else if bytes.last() == Some(&b'\n') {
        scan.newline_count
    } else {
        scan.newline_count + 1
    };
    assert_eq!(logical_lines, expected_lines);

    let density = scan.ansi_density(bytes.len());
    assert!(density.is_finite());
    assert!((0.0..=1.0).contains(&density));

    // Single-split state-stitching (original coverage).
    let split = split_index(bytes);
    let (left, right) = bytes.split_at(split);
    let mut state = OutputScanState::default();
    let left_scan = scan_newlines_and_ansi_with_state(left, &mut state);
    let right_scan = scan_newlines_and_ansi_with_state(right, &mut state);

    let stitched = OutputScanMetrics {
        newline_count: left_scan.newline_count + right_scan.newline_count,
        ansi_byte_count: left_scan.ansi_byte_count + right_scan.ansi_byte_count,
    };
    assert_eq!(
        stitched, scan,
        "chunked stateful scan drifted from full-buffer scan at split {split}"
    );

    // Multi-split state-stitching (ft-hfbsp). Drives the same invariant
    // across up to MAX_SPLITS chunk boundaries — amplifies coverage of
    // the SIMD tail-loop where chunk-tail off-by-one bugs typically
    // hide. Reuses ONE OutputScanState across all chunks so the
    // ANSI state machine carries forward correctly across boundaries
    // (the production path's pane-output streaming model).
    let offsets = multi_split_offsets(bytes, input.multi_split_seeds);
    if !offsets.is_empty() {
        let mut multi_state = OutputScanState::default();
        let mut cursor = 0usize;
        let mut multi_metrics = OutputScanMetrics::default();
        for &offset in &offsets {
            let chunk = &bytes[cursor..offset];
            let chunk_scan = scan_newlines_and_ansi_with_state(chunk, &mut multi_state);
            multi_metrics.newline_count += chunk_scan.newline_count;
            multi_metrics.ansi_byte_count += chunk_scan.ansi_byte_count;
            cursor = offset;
        }
        // Final tail chunk (after the last split).
        let tail = &bytes[cursor..];
        let tail_scan = scan_newlines_and_ansi_with_state(tail, &mut multi_state);
        multi_metrics.newline_count += tail_scan.newline_count;
        multi_metrics.ansi_byte_count += tail_scan.ansi_byte_count;
        assert_eq!(
            multi_metrics, scan,
            "multi-split stateful scan drifted from full-buffer scan across offsets {offsets:?}"
        );
    }
});
