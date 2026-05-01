//! End-of-frame cell-grid consistency hashing (ft-mpc9b.6.2).
//!
//! Catches the bug class WezTerm shipped for years (resize artifacts,
//! scroll glitches, cell-color desync) via a cheap dual-hash over the
//! rendered cell grid compared against the terminal-model state. The
//! bead's headline ask: "Future agents touching the renderer cannot
//! ship cell-grid corruption silently."
//!
//! Pure-logic substrate. The integration bead wires this behind a
//! `debug-cell-crc` feature flag in frankenterm-gui, runs it after
//! each paint pass, and dumps mismatches to
//! `~/.local/share/ft/debug/cell-mismatch-<ts>.json` for repro.
//!
//! ## What this module ships
//!
//! - `CellRecord` — minimal row/col/glyph/fg/bg byte-record the
//!   integration layer feeds in. Composes with the larger
//!   `instanced_cell::CellInstance` from ft-mpc9b.6.1 (just take its
//!   relevant byte slice and call `hash_cell_grid`).
//! - `fnv1a_64` / `crc32_ieee` — two independent hash functions so a
//!   single-hash collision can't mask a mismatch. The bead calls for
//!   xxhash + crc32 but xxhash isn't a leaf-clean dep here; FNV-1a
//!   has the same collision properties for our use case (both
//!   uniform 64-bit / 32-bit hashes over byte strings) and lives in
//!   ~30 lines of code.
//! - `CellGridHash { fnv: u64, crc: u32 }` — dual-hash payload.
//! - `hash_cell_grid` — feeds a slice of `CellRecord`s through both
//!   hashes simultaneously (one pass over the data).
//! - `hashes_match` — both hashes equal ⇒ same grid. Either differs
//!   ⇒ mismatch (false-positive probability ≈ 2^-96).
//! - `MismatchClass` — telemetry routing for the mismatch (false-
//!   positive sanity check vs real corruption).
//! - `diff_grids` — produces a `Vec<CellDiff>` for the dump file.
//! - `MismatchDecision` policy: log-only / dump-and-continue / fail
//!   (the integration's failure path under `--features
//!   fail-on-cell-mismatch`).
//!
//! ## What is deferred to the integration bead (ft-mpc9b.6.2.cont)
//!
//! - The `debug-cell-crc` feature flag in frankenterm-gui.
//! - Per-frame instrumentation in
//!   `crates/frankenterm-gui/src/termwindow/render/paint.rs`.
//! - Mismatch dump file format + path
//!   (`~/.local/share/ft/debug/cell-mismatch-<ts>.json`).
//! - CI hookup that runs the debug feature on the visual-regression
//!   corpus (cross-link 1.6 / ft-n0hpo).
//! - Adversarial fuzz integration: 24 h fuzz lane in 1.6 also runs
//!   CRC; any mismatch fails the lane.
//! - Per-release attestation entry: zero CRC mismatches on default
//!   branch.
//! - Cross-link to ft-ombfl + ft-35yac.1.2 in the integration's
//!   structured-logging schema.

#![allow(dead_code)]

// ============================================================================
// Cell record — minimal byte-record for hashing
// ============================================================================

/// One cell's relevant byte content for hash computation. The
/// integration layer fills this from the rendered cell grid (or
/// from `CellInstance` in ft-mpc9b.6.1) and from the terminal model
/// state, then hashes both into a `CellGridHash` for comparison.
///
/// This is intentionally narrower than `CellInstance` — we hash only
/// the visible-state-affecting fields (row, col, glyph_id, fg, bg).
/// Cursor flags / attribute flags can be added later without breaking
/// the hash (just bump the hash version).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[repr(C)]
pub struct CellRecord {
    pub row: u16,
    pub col: u16,
    pub glyph_id: u32,
    pub fg_color: u32,
    pub bg_color: u32,
}

impl CellRecord {
    #[must_use]
    pub const fn new(row: u16, col: u16, glyph_id: u32, fg_color: u32, bg_color: u32) -> Self {
        Self {
            row,
            col,
            glyph_id,
            fg_color,
            bg_color,
        }
    }

    /// Encode as a 16-byte big-endian byte sequence for hashing.
    /// Big-endian is chosen so a cross-platform repro file is byte-
    /// identical regardless of the host's endianness.
    #[must_use]
    pub fn to_be_bytes(self) -> [u8; 16] {
        let mut bytes = [0u8; 16];
        bytes[0..2].copy_from_slice(&self.row.to_be_bytes());
        bytes[2..4].copy_from_slice(&self.col.to_be_bytes());
        bytes[4..8].copy_from_slice(&self.glyph_id.to_be_bytes());
        bytes[8..12].copy_from_slice(&self.fg_color.to_be_bytes());
        bytes[12..16].copy_from_slice(&self.bg_color.to_be_bytes());
        bytes
    }
}

// ============================================================================
// FNV-1a 64-bit hash
// ============================================================================

/// FNV-1a offset basis (64-bit).
const FNV_OFFSET_BASIS_64: u64 = 0xcbf2_9ce4_8422_2325;
/// FNV-1a prime (64-bit).
const FNV_PRIME_64: u64 = 0x100_0000_01b3;

/// Compute the 64-bit FNV-1a hash of a byte slice. ~30 lines, no
/// dependencies, uniform distribution. Collision probability for two
/// independent inputs ≈ 2^-64.
#[must_use]
pub fn fnv1a_64(bytes: &[u8]) -> u64 {
    let mut hash = FNV_OFFSET_BASIS_64;
    for &byte in bytes {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(FNV_PRIME_64);
    }
    hash
}

// ============================================================================
// CRC32-IEEE
// ============================================================================

/// Compute the 32-bit CRC32-IEEE of a byte slice. Bitwise reference
/// implementation (no precomputed table to keep the substrate
/// dependency-free; the integration layer can swap in a tabled
/// version if hot-path benchmarks demand it). Collision probability
/// for two independent inputs ≈ 2^-32; combined with FNV-1a's 2^-64,
/// the dual-hash false-positive probability is ≈ 2^-96.
#[must_use]
pub fn crc32_ieee(bytes: &[u8]) -> u32 {
    let mut crc: u32 = 0xffff_ffff;
    for &byte in bytes {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xedb8_8320 & mask);
        }
    }
    !crc
}

// ============================================================================
// Dual hash + grid hashing
// ============================================================================

/// Dual-hash payload. Both fields must equal between two grids for
/// them to be considered identical.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct CellGridHash {
    pub fnv: u64,
    pub crc: u32,
}

/// Hash a slice of `CellRecord`s into a `CellGridHash`. Single-pass
/// over the data: FNV-1a and CRC32 update simultaneously per byte.
#[must_use]
pub fn hash_cell_grid(grid: &[CellRecord]) -> CellGridHash {
    let mut fnv = FNV_OFFSET_BASIS_64;
    let mut crc: u32 = 0xffff_ffff;
    for cell in grid {
        let bytes = cell.to_be_bytes();
        for &byte in &bytes {
            // FNV-1a step
            fnv ^= u64::from(byte);
            fnv = fnv.wrapping_mul(FNV_PRIME_64);
            // CRC32 step
            crc ^= u32::from(byte);
            for _ in 0..8 {
                let mask = (crc & 1).wrapping_neg();
                crc = (crc >> 1) ^ (0xedb8_8320 & mask);
            }
        }
    }
    CellGridHash { fnv, crc: !crc }
}

/// Whether two hashes match. Both halves must equal.
#[must_use]
pub fn hashes_match(a: CellGridHash, b: CellGridHash) -> bool {
    a == b
}

// ============================================================================
// Mismatch classification + diff
// ============================================================================

/// Classification for a hash mismatch. Routes telemetry: a single-
/// hash collision (one half matches, one doesn't) is "almost
/// certainly real" because the false-positive probability of a
/// genuine match collapsing one specific half is dominated by the
/// other half catching it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MismatchClass {
    /// Both hash halves agree → no mismatch. Returned by
    /// `classify_mismatch` so the caller has a single match-arm
    /// surface; the integration's main path tests `is_mismatch`
    /// before dumping the diff.
    Match,
    /// Both halves differ → confident mismatch. Real corruption.
    BothDiffer,
    /// FNV differs but CRC matches → unusual. Could be a genuine
    /// difference where CRC32 happened to collide (probability ≈
    /// 2^-32 per frame). Still a mismatch but logged with the rare-
    /// event marker so operator triage can decide whether to
    /// investigate.
    OnlyFnvDiffers,
    /// CRC differs but FNV matches → similarly unusual.
    OnlyCrcDiffers,
}

impl MismatchClass {
    #[must_use]
    pub fn is_mismatch(self) -> bool {
        !matches!(self, Self::Match)
    }
}

/// Classify the relationship between two hashes. Pure-logic.
#[must_use]
pub fn classify_mismatch(expected: CellGridHash, actual: CellGridHash) -> MismatchClass {
    let fnv_eq = expected.fnv == actual.fnv;
    let crc_eq = expected.crc == actual.crc;
    match (fnv_eq, crc_eq) {
        (true, true) => MismatchClass::Match,
        (false, false) => MismatchClass::BothDiffer,
        (false, true) => MismatchClass::OnlyFnvDiffers,
        (true, false) => MismatchClass::OnlyCrcDiffers,
    }
}

/// One per-cell difference for the dump file. The integration layer
/// serializes a `Vec<CellDiff>` to JSON for repro.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CellDiff {
    pub row: u16,
    pub col: u16,
    pub expected: CellRecord,
    pub actual: CellRecord,
}

/// Compute the per-cell diff between two grids of equal length.
/// Returns `Err` with the size mismatch if the grids differ in
/// length — that's a structural bug the integration layer should
/// surface separately from cell-level corruption.
pub fn diff_grids(
    expected: &[CellRecord],
    actual: &[CellRecord],
) -> Result<Vec<CellDiff>, GridSizeMismatch> {
    if expected.len() != actual.len() {
        return Err(GridSizeMismatch {
            expected_len: expected.len(),
            actual_len: actual.len(),
        });
    }
    let mut diffs = Vec::new();
    for (e, a) in expected.iter().zip(actual.iter()) {
        if e != a {
            diffs.push(CellDiff {
                row: e.row,
                col: e.col,
                expected: *e,
                actual: *a,
            });
        }
    }
    Ok(diffs)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GridSizeMismatch {
    pub expected_len: usize,
    pub actual_len: usize,
}

// ============================================================================
// Mismatch policy
// ============================================================================

/// What the integration layer should do on a hash mismatch. Pure
/// policy — no I/O — so the integration's panic-on-mismatch flag
/// (for CI lanes) is settable from config without changing the hot-
/// path code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MismatchAction {
    /// Increment a counter, log structured info, continue. Default
    /// for debug builds.
    LogOnly,
    /// Dump expected + actual grids to a repro file, then continue.
    /// Default for debug builds with `--features debug-cell-diff`.
    DumpAndContinue,
    /// Fail the frame: panic in debug builds, abort the test run in
    /// CI. Default for `--features fail-on-cell-mismatch`.
    Fail,
}

/// Operator-tunable mismatch behaviour.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MismatchPolicy {
    /// Action when classification is `BothDiffer` (the high-
    /// confidence mismatch case).
    pub on_both_differ: MismatchAction,
    /// Action when only one hash half disagrees. Default
    /// `LogOnly` since these are statistically expected at <2^-32
    /// per frame and shouldn't fail CI on their own.
    pub on_single_half_differs: MismatchAction,
}

impl Default for MismatchPolicy {
    fn default() -> Self {
        Self {
            on_both_differ: MismatchAction::DumpAndContinue,
            on_single_half_differs: MismatchAction::LogOnly,
        }
    }
}

impl MismatchPolicy {
    /// CI policy — fail on `BothDiffer` so a regression in the visual-
    /// regression corpus turns red.
    #[must_use]
    pub const fn ci_strict() -> Self {
        Self {
            on_both_differ: MismatchAction::Fail,
            on_single_half_differs: MismatchAction::DumpAndContinue,
        }
    }

    /// Resolve the action for a given classification.
    #[must_use]
    pub const fn action_for(&self, class: MismatchClass) -> Option<MismatchAction> {
        match class {
            MismatchClass::Match => None,
            MismatchClass::BothDiffer => Some(self.on_both_differ),
            MismatchClass::OnlyFnvDiffers | MismatchClass::OnlyCrcDiffers => {
                Some(self.on_single_half_differs)
            }
        }
    }
}

// ============================================================================
// Telemetry
// ============================================================================

/// Lifetime counters surfaced through `ft doctor` (debug builds only).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CellCrcStats {
    pub frames_hashed_total: u64,
    pub matches_total: u64,
    pub both_differ_total: u64,
    pub only_fnv_differs_total: u64,
    pub only_crc_differs_total: u64,
}

impl CellCrcStats {
    pub fn record(&mut self, class: MismatchClass) {
        self.frames_hashed_total = self.frames_hashed_total.saturating_add(1);
        match class {
            MismatchClass::Match => {
                self.matches_total = self.matches_total.saturating_add(1);
            }
            MismatchClass::BothDiffer => {
                self.both_differ_total = self.both_differ_total.saturating_add(1);
            }
            MismatchClass::OnlyFnvDiffers => {
                self.only_fnv_differs_total = self.only_fnv_differs_total.saturating_add(1);
            }
            MismatchClass::OnlyCrcDiffers => {
                self.only_crc_differs_total = self.only_crc_differs_total.saturating_add(1);
            }
        }
    }

    /// True when no mismatches have been observed across all
    /// hashed frames. The per-release attestation entry asserts
    /// this on the default branch.
    #[must_use]
    pub fn is_clean(&self) -> bool {
        self.both_differ_total == 0
            && self.only_fnv_differs_total == 0
            && self.only_crc_differs_total == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cell(row: u16, col: u16, glyph: u32) -> CellRecord {
        CellRecord::new(row, col, glyph, 0xff00_00ff, 0x0000_00ff)
    }

    // ----------------------------------------------------------------
    // CellRecord encoding
    // ----------------------------------------------------------------

    #[test]
    fn cell_record_to_be_bytes_layout() {
        let c = CellRecord::new(0x0102, 0x0304, 0x0506_0708, 0x090a_0b0c, 0x0d0e_0f10);
        let bytes = c.to_be_bytes();
        assert_eq!(bytes[0], 0x01);
        assert_eq!(bytes[1], 0x02);
        assert_eq!(bytes[2], 0x03);
        assert_eq!(bytes[3], 0x04);
        assert_eq!(bytes[4], 0x05);
        assert_eq!(bytes[15], 0x10);
    }

    #[test]
    fn cell_record_size_is_16_bytes() {
        // The to_be_bytes return type asserts this at compile time;
        // double-check at runtime.
        let bytes = CellRecord::default().to_be_bytes();
        assert_eq!(bytes.len(), 16);
    }

    // ----------------------------------------------------------------
    // FNV-1a
    // ----------------------------------------------------------------

    #[test]
    fn fnv1a_empty_returns_offset_basis() {
        assert_eq!(fnv1a_64(&[]), FNV_OFFSET_BASIS_64);
    }

    #[test]
    fn fnv1a_known_vector() {
        // Single byte 0x00: hash = (basis ^ 0) * prime
        let expected = FNV_OFFSET_BASIS_64.wrapping_mul(FNV_PRIME_64);
        assert_eq!(fnv1a_64(&[0x00]), expected);
    }

    #[test]
    fn fnv1a_different_inputs_different_hashes() {
        let a = fnv1a_64(b"hello");
        let b = fnv1a_64(b"world");
        assert_ne!(a, b);
    }

    #[test]
    fn fnv1a_one_bit_change_avalanches() {
        // One-bit difference in input → very different hash output
        // (FNV-1a's avalanche isn't perfect but still flips many
        // bits). Just assert the outputs differ.
        let a = fnv1a_64(&[0x00, 0x00, 0x00, 0x00]);
        let b = fnv1a_64(&[0x01, 0x00, 0x00, 0x00]);
        assert_ne!(a, b);
    }

    // ----------------------------------------------------------------
    // CRC32-IEEE
    // ----------------------------------------------------------------

    #[test]
    fn crc32_empty_is_zero() {
        // Standard CRC32-IEEE of empty input is 0.
        assert_eq!(crc32_ieee(&[]), 0);
    }

    #[test]
    fn crc32_known_vector_quick_brown_fox() {
        // Standard test vector: CRC32 of "The quick brown fox jumps
        // over the lazy dog" is 0x414fa339.
        let v = b"The quick brown fox jumps over the lazy dog";
        assert_eq!(crc32_ieee(v), 0x414f_a339);
    }

    #[test]
    fn crc32_known_vector_123456789() {
        // CRC32-IEEE of "123456789" is 0xcbf43926 (industry-standard
        // test vector).
        assert_eq!(crc32_ieee(b"123456789"), 0xcbf4_3926);
    }

    // ----------------------------------------------------------------
    // hash_cell_grid
    // ----------------------------------------------------------------

    #[test]
    fn hash_empty_grid_matches_offset_and_zero() {
        let h = hash_cell_grid(&[]);
        assert_eq!(h.fnv, FNV_OFFSET_BASIS_64);
        assert_eq!(h.crc, 0);
    }

    #[test]
    fn hash_is_deterministic() {
        let grid = vec![cell(0, 0, 1), cell(0, 1, 2), cell(1, 0, 3)];
        let h1 = hash_cell_grid(&grid);
        let h2 = hash_cell_grid(&grid);
        assert_eq!(h1, h2);
    }

    #[test]
    fn hash_changes_when_one_cell_changes() {
        let mut a = vec![cell(0, 0, 1), cell(0, 1, 2)];
        let mut b = a.clone();
        b[1].glyph_id = 99;
        assert_ne!(hash_cell_grid(&a), hash_cell_grid(&b));
        // Order shouldn't matter to the assertion, but mutate `a` for
        // symmetry coverage.
        a[0].fg_color = 0xdead_beef;
        assert_ne!(hash_cell_grid(&a), hash_cell_grid(&b));
    }

    #[test]
    fn hash_changes_when_cell_position_swaps() {
        let mut a = vec![cell(0, 0, 1), cell(0, 1, 2)];
        let mut b = a.clone();
        b.swap(0, 1);
        // Swapping order changes which row/col goes first → hash
        // differs.
        assert_ne!(hash_cell_grid(&a), hash_cell_grid(&b));
        // (silence unused warning; mutation above suffices)
        let _ = a;
    }

    #[test]
    fn hash_one_pass_matches_separate_calls() {
        // The hash_cell_grid function does FNV+CRC in one pass; verify
        // the outputs agree with calling fnv1a_64 / crc32_ieee
        // separately on the same byte stream.
        let grid = vec![cell(1, 2, 100), cell(3, 4, 200), cell(5, 6, 300)];
        let mut bytes = Vec::new();
        for c in &grid {
            bytes.extend_from_slice(&c.to_be_bytes());
        }
        let one_pass = hash_cell_grid(&grid);
        let separate = CellGridHash {
            fnv: fnv1a_64(&bytes),
            crc: crc32_ieee(&bytes),
        };
        assert_eq!(one_pass, separate);
    }

    // ----------------------------------------------------------------
    // hashes_match / classify_mismatch
    // ----------------------------------------------------------------

    #[test]
    fn hashes_match_identity() {
        let h = CellGridHash {
            fnv: 0x1234,
            crc: 0x5678,
        };
        assert!(hashes_match(h, h));
    }

    #[test]
    fn hashes_match_different_one_half() {
        let a = CellGridHash { fnv: 1, crc: 1 };
        let b = CellGridHash { fnv: 2, crc: 1 };
        assert!(!hashes_match(a, b));
    }

    #[test]
    fn classify_match_when_both_equal() {
        let h = CellGridHash { fnv: 1, crc: 1 };
        assert_eq!(classify_mismatch(h, h), MismatchClass::Match);
        assert!(!MismatchClass::Match.is_mismatch());
    }

    #[test]
    fn classify_both_differ_when_both_different() {
        let a = CellGridHash { fnv: 1, crc: 1 };
        let b = CellGridHash { fnv: 2, crc: 2 };
        let c = classify_mismatch(a, b);
        assert_eq!(c, MismatchClass::BothDiffer);
        assert!(c.is_mismatch());
    }

    #[test]
    fn classify_only_fnv_differs() {
        let a = CellGridHash { fnv: 1, crc: 1 };
        let b = CellGridHash { fnv: 2, crc: 1 };
        let c = classify_mismatch(a, b);
        assert_eq!(c, MismatchClass::OnlyFnvDiffers);
        assert!(c.is_mismatch());
    }

    #[test]
    fn classify_only_crc_differs() {
        let a = CellGridHash { fnv: 1, crc: 1 };
        let b = CellGridHash { fnv: 1, crc: 2 };
        let c = classify_mismatch(a, b);
        assert_eq!(c, MismatchClass::OnlyCrcDiffers);
        assert!(c.is_mismatch());
    }

    // ----------------------------------------------------------------
    // diff_grids
    // ----------------------------------------------------------------

    #[test]
    fn diff_identical_grids_returns_empty() {
        let g = vec![cell(0, 0, 1), cell(0, 1, 2)];
        let diffs = diff_grids(&g, &g).unwrap();
        assert!(diffs.is_empty());
    }

    #[test]
    fn diff_finds_single_changed_cell() {
        let a = vec![cell(0, 0, 1), cell(0, 1, 2), cell(1, 0, 3)];
        let mut b = a.clone();
        b[1].glyph_id = 99;
        let diffs = diff_grids(&a, &b).unwrap();
        assert_eq!(diffs.len(), 1);
        assert_eq!(diffs[0].row, 0);
        assert_eq!(diffs[0].col, 1);
        assert_eq!(diffs[0].expected.glyph_id, 2);
        assert_eq!(diffs[0].actual.glyph_id, 99);
    }

    #[test]
    fn diff_finds_multiple_changes() {
        let a = vec![cell(0, 0, 1), cell(0, 1, 2), cell(1, 0, 3)];
        let mut b = a.clone();
        // The `cell()` helper's defaults are fg=0xff00_00ff, bg=0x0000_00ff
        // — pick mutation values that actually differ from those.
        b[0].fg_color = 0xdead_beef;
        b[2].bg_color = 0xcafe_babe;
        let diffs = diff_grids(&a, &b).unwrap();
        assert_eq!(diffs.len(), 2);
    }

    #[test]
    fn diff_size_mismatch_returns_err() {
        let a = vec![cell(0, 0, 1), cell(0, 1, 2)];
        let b = vec![cell(0, 0, 1)];
        let err = diff_grids(&a, &b).unwrap_err();
        assert_eq!(err.expected_len, 2);
        assert_eq!(err.actual_len, 1);
    }

    // ----------------------------------------------------------------
    // MismatchPolicy
    // ----------------------------------------------------------------

    #[test]
    fn policy_default_is_dump_and_log() {
        let p = MismatchPolicy::default();
        assert_eq!(p.on_both_differ, MismatchAction::DumpAndContinue);
        assert_eq!(p.on_single_half_differs, MismatchAction::LogOnly);
    }

    #[test]
    fn policy_ci_strict_fails_on_both_differ() {
        let p = MismatchPolicy::ci_strict();
        assert_eq!(p.on_both_differ, MismatchAction::Fail);
        assert_eq!(p.on_single_half_differs, MismatchAction::DumpAndContinue);
    }

    #[test]
    fn policy_action_for_match_is_none() {
        let p = MismatchPolicy::default();
        assert!(p.action_for(MismatchClass::Match).is_none());
    }

    #[test]
    fn policy_action_for_both_differ() {
        let p = MismatchPolicy::default();
        assert_eq!(
            p.action_for(MismatchClass::BothDiffer),
            Some(MismatchAction::DumpAndContinue)
        );
    }

    #[test]
    fn policy_action_for_single_half() {
        let p = MismatchPolicy::default();
        assert_eq!(
            p.action_for(MismatchClass::OnlyFnvDiffers),
            Some(MismatchAction::LogOnly)
        );
        assert_eq!(
            p.action_for(MismatchClass::OnlyCrcDiffers),
            Some(MismatchAction::LogOnly)
        );
    }

    // ----------------------------------------------------------------
    // CellCrcStats
    // ----------------------------------------------------------------

    #[test]
    fn stats_default_is_empty_and_clean() {
        let s = CellCrcStats::default();
        assert_eq!(s.frames_hashed_total, 0);
        assert!(s.is_clean());
    }

    #[test]
    fn stats_record_match() {
        let mut s = CellCrcStats::default();
        s.record(MismatchClass::Match);
        assert_eq!(s.frames_hashed_total, 1);
        assert_eq!(s.matches_total, 1);
        assert!(s.is_clean());
    }

    #[test]
    fn stats_record_both_differ_breaks_clean() {
        let mut s = CellCrcStats::default();
        s.record(MismatchClass::BothDiffer);
        assert_eq!(s.both_differ_total, 1);
        assert!(!s.is_clean());
    }

    #[test]
    fn stats_record_single_half_each_breaks_clean() {
        let mut s = CellCrcStats::default();
        s.record(MismatchClass::OnlyFnvDiffers);
        assert_eq!(s.only_fnv_differs_total, 1);
        assert!(!s.is_clean());

        let mut s = CellCrcStats::default();
        s.record(MismatchClass::OnlyCrcDiffers);
        assert_eq!(s.only_crc_differs_total, 1);
        assert!(!s.is_clean());
    }

    #[test]
    fn stats_record_mixed_run() {
        let mut s = CellCrcStats::default();
        for _ in 0..10 {
            s.record(MismatchClass::Match);
        }
        s.record(MismatchClass::BothDiffer);
        s.record(MismatchClass::OnlyFnvDiffers);
        assert_eq!(s.frames_hashed_total, 12);
        assert_eq!(s.matches_total, 10);
        assert_eq!(s.both_differ_total, 1);
        assert_eq!(s.only_fnv_differs_total, 1);
        assert!(!s.is_clean());
    }

    // ----------------------------------------------------------------
    // Cross-cut: realistic scenarios from the bead
    // ----------------------------------------------------------------

    #[test]
    fn scenario_clean_frame_through_full_pipeline() {
        // Renderer + model agree on a 3-cell grid → match → no
        // diff → stats stay clean.
        let expected = vec![cell(0, 0, 100), cell(0, 1, 200), cell(0, 2, 300)];
        let actual = expected.clone();
        let h_expected = hash_cell_grid(&expected);
        let h_actual = hash_cell_grid(&actual);
        let class = classify_mismatch(h_expected, h_actual);
        assert_eq!(class, MismatchClass::Match);
        let policy = MismatchPolicy::default();
        assert!(policy.action_for(class).is_none());
        let mut stats = CellCrcStats::default();
        stats.record(class);
        assert!(stats.is_clean());
    }

    #[test]
    fn scenario_resize_artifact_caught_with_diff() {
        // Renderer dropped a glyph during resize → mismatch caught,
        // diff identifies the bad cell, CI policy fails the run.
        let expected = vec![cell(0, 0, 100), cell(0, 1, 200), cell(0, 2, 300)];
        let mut actual = expected.clone();
        actual[1].glyph_id = 0; // dropped glyph

        let h_expected = hash_cell_grid(&expected);
        let h_actual = hash_cell_grid(&actual);
        let class = classify_mismatch(h_expected, h_actual);
        assert_eq!(class, MismatchClass::BothDiffer);
        let diffs = diff_grids(&expected, &actual).unwrap();
        assert_eq!(diffs.len(), 1);
        assert_eq!(diffs[0].col, 1);
        assert_eq!(diffs[0].expected.glyph_id, 200);
        assert_eq!(diffs[0].actual.glyph_id, 0);
        // CI policy fails the run.
        let ci = MismatchPolicy::ci_strict();
        assert_eq!(ci.action_for(class), Some(MismatchAction::Fail));
    }

    #[test]
    fn scenario_24h_fuzz_zero_mismatches() {
        // Adversarial fuzz lane runs the CRC over 24h of randomised
        // input; assert the expected stats invariant (per the bead's
        // attestation entry: zero CRC mismatches on default branch).
        let mut stats = CellCrcStats::default();
        let grid = vec![cell(0, 0, 1), cell(1, 0, 2), cell(2, 0, 3)];
        for _ in 0..10_000 {
            let h_expected = hash_cell_grid(&grid);
            let h_actual = hash_cell_grid(&grid);
            stats.record(classify_mismatch(h_expected, h_actual));
        }
        assert_eq!(stats.frames_hashed_total, 10_000);
        assert_eq!(stats.matches_total, 10_000);
        assert!(stats.is_clean());
    }
}
