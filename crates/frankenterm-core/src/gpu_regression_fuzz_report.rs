//! Failure-artifact emitter contract for the GPU regression
//! fuzz lane
//! ([BR-TERM-EMULATOR-UPLIFT.1.6.cont] / `ft-n0hpo`).
//!
//! The fuzz generator (`FuzzSeed` / `FuzzStream` /
//! `FuzzInputEvent` / `FuzzConfig`) lives in
//! `crates/frankenterm-gui/src/gpu_regression_fuzz.rs`
//! (shipped at `1f2a44dd3` under the parent bead). This
//! module ships the **failure-artifact contract** — the
//! `runs/<run_id>/` layout, the violation classification
//! taxonomy from `tests/renderer_golden/fuzz/README.md`, and
//! the harness CLI flag envelope.
//!
//! ## Why a separate module
//!
//! The contract is **dependency-free** — pure types + pure
//! classification logic. The frankenterm-gui crate's harness
//! consumes these types to write the artifact tree on disk;
//! the renderer-fuzz CI lane reads the same types from
//! aggregated `violations.jsonl` files. Putting the contract
//! in `frankenterm-core` keeps the harness binary small and
//! lets non-GPU consumers (e.g., a future violation-triage
//! tool) consume the artifact format without pulling in the
//! GPU stack.
//!
//! ## Headline classification (from fuzz/README.md)
//!
//! A frame is **critical** if any of these hold:
//!
//! 1. **BlankFrame** — entire frame is blank when the
//!    previous frame was non-blank.
//! 2. **StaleFullFrame** — frame is byte-identical to a
//!    frame ≥ 200 events earlier (missed-Present indicator).
//! 3. **TearBand** — a pristine area (no dirty mark) shows
//!    pixel divergence ΔL∞ ≥ 32.
//!
//! A frame is **minor** if it fails the comparator's standard
//! thresholds (SSIM < 0.99 or changed-pixel fraction >
//! 0.001) but matches no critical class. The bead's 24h
//! budget for minor artifacts is 0.1% of resize-class
//! events; the 24h budget for critical artifacts is **zero**
//! (RQ-S4).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

// ============================================================================
// Run identity
// ============================================================================

/// A run id — short hash of `(seed, started_at_ms, host)`.
/// The frankenterm-gui harness emits this on disk under
/// `runs/<run_id>/`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct RunId(pub String);

impl RunId {
    /// Compose from inputs. Production uses BLAKE3; the
    /// model uses a deterministic short-hash so test fixtures
    /// produce stable ids.
    #[must_use]
    pub fn from_parts(seed: u64, started_at_ms: u64, host: &str) -> Self {
        let mut h: u64 = 0xcbf29ce484222325;
        for byte in seed.to_le_bytes() {
            h ^= u64::from(byte);
            h = h.wrapping_mul(0x100000001b3);
        }
        for byte in started_at_ms.to_le_bytes() {
            h ^= u64::from(byte);
            h = h.wrapping_mul(0x100000001b3);
        }
        for &byte in host.as_bytes() {
            h ^= u64::from(byte);
            h = h.wrapping_mul(0x100000001b3);
        }
        Self(format!("{h:016x}"))
    }
}

/// `runs/<run_id>/meta.json` — per-run metadata.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunMeta {
    pub run_id: RunId,
    pub seed: u64,
    pub started_at_ms: u64,
    pub finished_at_ms: Option<u64>,
    pub host: String,
    pub harness_version: String,
    /// Cap on event index this run reached (whether by
    /// duration timeout or generator exhaustion).
    pub events_processed: u64,
    /// Total violations recorded — sum of critical + minor.
    pub violations_total: u32,
    /// Critical-class count — `0` is the bead's headline
    /// pass criterion (RQ-S4).
    pub critical_count: u32,
}

// ============================================================================
// Violation classification
// ============================================================================

/// Kind of failure observed at a frame. Critical kinds are
/// the bead's RQ-S4 hard fails; Minor kinds count against
/// the 0.1% budget.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ViolationKind {
    /// Critical — entire frame blank when prior was non-blank.
    BlankFrame,
    /// Critical — frame byte-identical to a frame ≥ 200
    /// events earlier (missed Present).
    StaleFullFrame { stale_distance: u32 },
    /// Critical — pristine area (no dirty mark) shows pixel
    /// divergence ΔL∞ ≥ 32.
    TearBand { delta_l_inf: u32 },
    /// Minor — SSIM < 0.99.
    SsimBelowThreshold { ssim: f64, threshold: f64 },
    /// Minor — changed-pixel fraction > 0.001.
    ExcessivePixelChange { fraction: f64, threshold: f64 },
}

impl ViolationKind {
    /// Stable slug for the violations.jsonl `kind` field.
    #[must_use]
    pub const fn slug(&self) -> &'static str {
        match self {
            Self::BlankFrame => "blank_frame",
            Self::StaleFullFrame { .. } => "stale_full_frame",
            Self::TearBand { .. } => "tear_band",
            Self::SsimBelowThreshold { .. } => "ssim_below_threshold",
            Self::ExcessivePixelChange { .. } => "excessive_pixel_change",
        }
    }

    /// Whether this kind is critical (counts against the
    /// bead's zero-criticals budget).
    #[must_use]
    pub const fn is_critical(&self) -> bool {
        matches!(
            self,
            Self::BlankFrame | Self::StaleFullFrame { .. } | Self::TearBand { .. }
        )
    }
}

/// Classification predicate — convenience.
#[must_use]
pub const fn is_critical(kind: &ViolationKind) -> bool {
    kind.is_critical()
}

/// One line in `violations.jsonl`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ViolationRecord {
    pub event_index: u32,
    /// Which frame in the run this corresponds to (0-indexed,
    /// as the harness emits one frame every N events).
    pub frame_index: u32,
    pub kind: ViolationKind,
    /// Reproducer hash — composed from the run's seed +
    /// event_index so a triager can recover the exact prefix.
    pub reproducer_seed: u64,
    pub start_at_event_idx: u32,
    /// Optional log slice excerpt for the event. The actual
    /// log file is at `violations/<event_idx>/log.jsonl`.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub log_excerpt: Option<String>,
}

impl ViolationRecord {
    /// Whether this record is critical.
    #[must_use]
    pub fn is_critical(&self) -> bool {
        self.kind.is_critical()
    }

    /// Filesystem path within `runs/<run_id>/violations/`
    /// where this record's artifacts live.
    #[must_use]
    pub fn artifact_subdir(&self) -> String {
        format!("violations/{:08}", self.event_index)
    }
}

// ============================================================================
// Run layout — runs/<run_id>/
// ============================================================================

/// Filesystem layout helpers for a single run. Pure-string
/// operations — no actual I/O. The harness binary calls these
/// to construct paths it then writes via std::fs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunLayout {
    pub run_id: RunId,
}

impl RunLayout {
    #[must_use]
    pub fn new(run_id: RunId) -> Self {
        Self { run_id }
    }

    /// `runs/<run_id>/`.
    #[must_use]
    pub fn root_dir(&self) -> String {
        format!("runs/{}", self.run_id.0)
    }

    /// `runs/<run_id>/meta.json`.
    #[must_use]
    pub fn meta_json_path(&self) -> String {
        format!("{}/meta.json", self.root_dir())
    }

    /// `runs/<run_id>/violations.jsonl`.
    #[must_use]
    pub fn violations_jsonl_path(&self) -> String {
        format!("{}/violations.jsonl", self.root_dir())
    }

    /// `runs/<run_id>/violations/<event_idx>/`.
    #[must_use]
    pub fn violation_artifact_dir(&self, event_index: u32) -> String {
        format!("{}/violations/{:08}", self.root_dir(), event_index)
    }

    /// `runs/<run_id>/violations/<event_idx>/before.png`.
    #[must_use]
    pub fn before_png(&self, event_index: u32) -> String {
        format!("{}/before.png", self.violation_artifact_dir(event_index))
    }

    #[must_use]
    pub fn after_png(&self, event_index: u32) -> String {
        format!("{}/after.png", self.violation_artifact_dir(event_index))
    }

    #[must_use]
    pub fn diff_png(&self, event_index: u32) -> String {
        format!("{}/diff.png", self.violation_artifact_dir(event_index))
    }

    #[must_use]
    pub fn log_jsonl(&self, event_index: u32) -> String {
        format!("{}/log.jsonl", self.violation_artifact_dir(event_index))
    }

    #[must_use]
    pub fn reproducer_sh(&self, event_index: u32) -> String {
        format!("{}/reproducer.sh", self.violation_artifact_dir(event_index))
    }
}

// ============================================================================
// Harness CLI flags
// ============================================================================

/// Typed envelope for the harness binary's CLI flags. The
/// frankenterm-gui harness's clap layer constructs this from
/// argv; the contract layer keeps the field shapes
/// authoritative.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FuzzCliFlags {
    /// `--fuzz-seed=<u64>`.
    pub seed: Option<u64>,
    /// `--fuzz-duration=<secs>` — bounded by the run budget.
    pub duration_secs: Option<u32>,
    /// `--fuzz-start-at=<event_idx>` — for triager replay.
    pub start_at_event_idx: Option<u32>,
    /// `--fuzz-cols=<u16>` — overrides FuzzConfig default.
    pub cols: Option<u16>,
    /// `--fuzz-rows=<u16>`.
    pub rows: Option<u16>,
    /// `--runs-dir=<path>` — where to emit `runs/<run_id>/`.
    pub runs_dir: Option<String>,
}

impl Default for FuzzCliFlags {
    fn default() -> Self {
        Self::empty()
    }
}

impl FuzzCliFlags {
    #[must_use]
    pub const fn empty() -> Self {
        Self {
            seed: None,
            duration_secs: None,
            start_at_event_idx: None,
            cols: None,
            rows: None,
            runs_dir: None,
        }
    }

    /// True iff any fuzz flag is set — the harness uses this
    /// to decide whether to enter fuzz mode vs the standard
    /// scenario suite.
    #[must_use]
    pub fn fuzz_mode_active(&self) -> bool {
        self.seed.is_some() || self.duration_secs.is_some() || self.start_at_event_idx.is_some()
    }
}

// ============================================================================
// Scenario manifest — the 18-scenario plan
// ============================================================================

/// Status of a scenario fixture in the bead's 18-scenario
/// plan. Mirrors the SCENARIOS.md catalog at
/// `tests/renderer_golden/SCENARIOS.md`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScenarioStatus {
    /// Fixture (input.json + meta.json + expected.json +
    /// golden.png) shipped and runs in CI.
    Shipped,
    /// Fixture partially shipped (e.g., golden.png missing
    /// because the renderer wasn't on a Linux GPU CI lane
    /// when the parent bead landed).
    Partial,
    /// Bead's continuation must ship this fixture.
    Gap,
    /// Cross-references another bead's harness (e.g.,
    /// screen-reader-active needs A11y substrate).
    BlockedOnSubBead,
}

/// One scenario in the manifest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScenarioRecord {
    /// Stable slug used as the fixture directory name and the
    /// SCENARIOS.md anchor.
    pub slug: String,
    /// Cross-references the requirement(s) this scenario
    /// covers in `docs/perf/resize-quality-slo.md` (e.g.,
    /// `RQ-S8` for the frame-skip requirement).
    pub requirements: Vec<String>,
    pub status: ScenarioStatus,
    /// Optional follow-on bead handle for blocked entries.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub blocked_on: Option<String>,
}

/// The full 18-scenario manifest, projected from the bead
/// description.
#[must_use]
pub fn scenario_manifest() -> Vec<ScenarioRecord> {
    use ScenarioStatus::*;
    let req = |s: &str| s.to_string();
    let row = |slug: &str,
               reqs: &[&str],
               status: ScenarioStatus,
               blocked: Option<&str>|
     -> ScenarioRecord {
        ScenarioRecord {
            slug: slug.to_string(),
            requirements: reqs.iter().map(|r| req(r)).collect(),
            status,
            blocked_on: blocked.map(|s| s.to_string()),
        }
    };
    vec![
        // ----- the 7 shipped/partial in the parent bead -----
        row("bare-prompt", &["RQ-S0"], Shipped, None),
        row("ascii-burst", &["RQ-S6"], Shipped, None),
        row("scroll-jump", &["RQ-S2"], Shipped, None),
        row("color-table", &["RQ-S3"], Shipped, None),
        row("emoji-roundtrip", &["RQ-S7"], Shipped, None),
        row("focus-toggle", &["RQ-S9"], Partial, None),
        row("selection-extend", &["RQ-S12"], Partial, None),
        // ----- the 12 gap fixtures the bead's action #3 ships -----
        row("steady-typing", &["RQ-S8"], Gap, None),
        row("vim-edit", &["RQ-S6"], Gap, None),
        row("htop-top", &["RQ-S5", "RQ-S8"], Gap, None),
        row("neofetch-banner", &["RQ-S11"], Gap, None),
        row("resize-burst", &["RQ-S1", "RQ-S10"], Gap, None),
        row("dpi-change", &["RQ-S10"], Gap, None),
        row("font-change", &["RQ-S10"], Gap, None),
        row("alt-screen", &[], Gap, None),
        row("mouse-tracking", &[], Gap, None),
        row("wide-gamut", &[], Gap, None),
        row("scrollback-search", &[], Gap, None),
        row(
            "screen-reader-active",
            &[],
            BlockedOnSubBead,
            Some("a11y-harness-substrate"),
        ),
    ]
}

/// Snapshot of the manifest's status — useful for the
/// per-release attestation entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScenarioCoverageSnapshot {
    pub scenarios_total: u32,
    pub shipped: u32,
    pub partial: u32,
    pub gap: u32,
    pub blocked: u32,
    pub scenarios: Vec<ScenarioRecord>,
}

#[must_use]
pub fn coverage_snapshot() -> ScenarioCoverageSnapshot {
    let scenarios = scenario_manifest();
    let mut shipped = 0;
    let mut partial = 0;
    let mut gap = 0;
    let mut blocked = 0;
    for s in &scenarios {
        match s.status {
            ScenarioStatus::Shipped => shipped += 1,
            ScenarioStatus::Partial => partial += 1,
            ScenarioStatus::Gap => gap += 1,
            ScenarioStatus::BlockedOnSubBead => blocked += 1,
        }
    }
    ScenarioCoverageSnapshot {
        scenarios_total: scenarios.len() as u32,
        shipped,
        partial,
        gap,
        blocked,
        scenarios,
    }
}

// ============================================================================
// Health snapshot
// ============================================================================

/// `ft doctor` snapshot for the GPU regression fuzz lane.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GpuFuzzHealth {
    /// Most recent run summary.
    pub last_run: Option<RunMeta>,
    /// Per-kind violation counters across the rolling 24h
    /// window.
    pub critical_24h: BTreeMap<String, u32>,
    pub minor_24h: BTreeMap<String, u32>,
    /// True iff RQ-S4 holds (zero criticals over 24h).
    pub rq_s4_ok: bool,
}

impl GpuFuzzHealth {
    #[must_use]
    pub fn baseline() -> Self {
        Self {
            last_run: None,
            critical_24h: BTreeMap::new(),
            minor_24h: BTreeMap::new(),
            rq_s4_ok: true,
        }
    }

    /// True iff the GPU regression fuzz lane has produced at
    /// least one run AND that run satisfies RQ-S4 (zero
    /// criticals across the rolling 24h window).
    ///
    /// Per ft-qpi11 fix: previously returned `rq_s4_ok` alone,
    /// which is true on cold baseline (no fuzz run yet). Doctor
    /// would surface RQ-S4 green for a process where the fuzz
    /// harness had never been wired or had silently failed to
    /// fold a snapshot.
    #[must_use]
    pub fn is_safe(&self) -> bool {
        self.last_run.is_some() && self.rq_s4_ok
    }

    /// Total criticals across all kinds in the 24h window.
    #[must_use]
    pub fn critical_total(&self) -> u32 {
        self.critical_24h.values().sum()
    }
}

/// Update the snapshot with one new violation record. Called
/// by the harness every time a frame's classification yields
/// a violation; aggregation is the doctor surface's job.
pub fn fold_violation(health: &mut GpuFuzzHealth, record: &ViolationRecord) {
    let slug = record.kind.slug().to_string();
    if record.is_critical() {
        *health.critical_24h.entry(slug).or_insert(0) += 1;
        health.rq_s4_ok = false;
    } else {
        *health.minor_24h.entry(slug).or_insert(0) += 1;
    }
}

// ============================================================================
// JSONL render
// ============================================================================

#[must_use]
pub fn render_violations_jsonl(records: &[ViolationRecord]) -> String {
    let mut out = String::new();
    for r in records {
        let line = serde_json::to_string(r).expect("ViolationRecord always serializes");
        out.push_str(&line);
        out.push('\n');
    }
    out
}

pub fn parse_violations_jsonl(jsonl: &str) -> Result<Vec<ViolationRecord>, serde_json::Error> {
    let mut out = Vec::new();
    for line in jsonl.lines() {
        if line.trim().is_empty() {
            continue;
        }
        out.push(serde_json::from_str(line)?);
    }
    Ok(out)
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_id_is_deterministic_per_inputs() {
        let a = RunId::from_parts(42, 1_000_000, "host1");
        let b = RunId::from_parts(42, 1_000_000, "host1");
        assert_eq!(a, b);
    }

    #[test]
    fn run_id_differs_per_seed() {
        let a = RunId::from_parts(42, 1_000_000, "host");
        let b = RunId::from_parts(43, 1_000_000, "host");
        assert_ne!(a, b);
    }

    #[test]
    fn critical_kinds_are_critical() {
        for kind in [
            ViolationKind::BlankFrame,
            ViolationKind::StaleFullFrame {
                stale_distance: 250,
            },
            ViolationKind::TearBand { delta_l_inf: 64 },
        ] {
            assert!(kind.is_critical(), "{kind:?} should be critical");
            assert!(is_critical(&kind));
        }
    }

    #[test]
    fn minor_kinds_are_not_critical() {
        for kind in [
            ViolationKind::SsimBelowThreshold {
                ssim: 0.95,
                threshold: 0.99,
            },
            ViolationKind::ExcessivePixelChange {
                fraction: 0.005,
                threshold: 0.001,
            },
        ] {
            assert!(!kind.is_critical(), "{kind:?} should not be critical");
        }
    }

    #[test]
    fn violation_slugs_are_distinct() {
        use std::collections::HashSet;
        let mut seen = HashSet::new();
        for kind in [
            ViolationKind::BlankFrame,
            ViolationKind::StaleFullFrame { stale_distance: 0 },
            ViolationKind::TearBand { delta_l_inf: 0 },
            ViolationKind::SsimBelowThreshold {
                ssim: 0.0,
                threshold: 0.0,
            },
            ViolationKind::ExcessivePixelChange {
                fraction: 0.0,
                threshold: 0.0,
            },
        ] {
            assert!(seen.insert(kind.slug()), "duplicate slug: {}", kind.slug());
        }
    }

    #[test]
    fn run_layout_paths_are_well_formed() {
        let layout = RunLayout::new(RunId("abc123".to_string()));
        assert_eq!(layout.root_dir(), "runs/abc123");
        assert_eq!(layout.meta_json_path(), "runs/abc123/meta.json");
        assert_eq!(
            layout.violations_jsonl_path(),
            "runs/abc123/violations.jsonl"
        );
        assert_eq!(
            layout.violation_artifact_dir(42),
            "runs/abc123/violations/00000042"
        );
        assert_eq!(
            layout.before_png(42),
            "runs/abc123/violations/00000042/before.png"
        );
        assert_eq!(
            layout.after_png(42),
            "runs/abc123/violations/00000042/after.png"
        );
        assert_eq!(
            layout.diff_png(42),
            "runs/abc123/violations/00000042/diff.png"
        );
        assert_eq!(
            layout.log_jsonl(42),
            "runs/abc123/violations/00000042/log.jsonl"
        );
        assert_eq!(
            layout.reproducer_sh(42),
            "runs/abc123/violations/00000042/reproducer.sh"
        );
    }

    #[test]
    fn fuzz_cli_flags_default_is_inactive() {
        let f = FuzzCliFlags::default();
        assert!(!f.fuzz_mode_active());
    }

    #[test]
    fn fuzz_cli_flags_active_when_seed_set() {
        let f = FuzzCliFlags {
            seed: Some(42),
            ..FuzzCliFlags::default()
        };
        assert!(f.fuzz_mode_active());
    }

    #[test]
    fn fuzz_cli_flags_active_when_start_at_set() {
        let f = FuzzCliFlags {
            start_at_event_idx: Some(100),
            ..FuzzCliFlags::default()
        };
        assert!(f.fuzz_mode_active());
    }

    #[test]
    fn scenario_manifest_has_19_entries() {
        // The bead text references "the 18-scenario plan" but
        // enumerates 7 shipped/partial + 12 gaps = 19. The
        // enumeration is the source of truth (the "18" is an
        // off-by-one in the bead text predating the
        // screen-reader-active addition); the manifest matches
        // the enumeration.
        let m = scenario_manifest();
        assert_eq!(m.len(), 19);
    }

    #[test]
    fn scenario_slugs_are_unique() {
        use std::collections::HashSet;
        let mut seen = HashSet::new();
        for s in scenario_manifest() {
            assert!(seen.insert(s.slug.clone()), "dup {}", s.slug);
        }
    }

    #[test]
    fn coverage_snapshot_counts_match_bead() {
        let s = coverage_snapshot();
        assert_eq!(s.scenarios_total, 19);
        // Bead description: 7 shipped/partial, 12 gaps. The
        // shipping split is 5 fully shipped + 2 partial = 7
        // existing; 11 gap + 1 blocked = 12 gaps.
        assert_eq!(s.shipped, 5);
        assert_eq!(s.partial, 2);
        assert_eq!(s.gap, 11);
        assert_eq!(s.blocked, 1);
        assert_eq!(s.shipped + s.partial, 7);
        assert_eq!(s.gap + s.blocked, 12);
    }

    /// ft-qpi11 helper: build a stub RunMeta so tests can attach
    /// `last_run` and exercise the post-fix is_safe gate.
    fn stub_run_meta() -> RunMeta {
        RunMeta {
            run_id: RunId::from_parts(42, 0, "test"),
            seed: 42,
            started_at_ms: 0,
            finished_at_ms: Some(1_000),
            host: "test".to_string(),
            harness_version: "test".to_string(),
            events_processed: 100,
            violations_total: 0,
            critical_count: 0,
        }
    }

    #[test]
    fn gpu_fuzz_health_baseline_is_unsafe_until_run_recorded() {
        // Per ft-qpi11 fix: cold baseline must NOT report safe.
        // Previously rubber-stamped because rq_s4_ok defaults to
        // true with no run recorded.
        let h = GpuFuzzHealth::baseline();
        assert!(
            !h.is_safe(),
            "cold baseline must be unsafe (no run yet)",
        );
        assert_eq!(h.critical_total(), 0);
    }

    #[test]
    fn gpu_fuzz_health_clean_run_marks_safe() {
        // Per ft-qpi11 fix: once a run is recorded with no
        // critical violations, is_safe == true.
        let mut h = GpuFuzzHealth::baseline();
        h.last_run = Some(stub_run_meta());
        assert!(h.is_safe(), "post-clean-run must be safe");
    }

    #[test]
    fn fold_violation_critical_marks_rq_s4_violated() {
        let mut h = GpuFuzzHealth::baseline();
        h.last_run = Some(stub_run_meta());
        let r = ViolationRecord {
            event_index: 100,
            frame_index: 5,
            kind: ViolationKind::BlankFrame,
            reproducer_seed: 42,
            start_at_event_idx: 0,
            log_excerpt: None,
        };
        fold_violation(&mut h, &r);
        assert!(!h.is_safe());
        assert_eq!(h.critical_total(), 1);
    }

    #[test]
    fn fold_violation_minor_does_not_mark_rq_s4_violated() {
        let mut h = GpuFuzzHealth::baseline();
        // Per ft-qpi11 fix: a fold without a recorded run cannot
        // make is_safe true. Attach a stub run so this test
        // exercises the minor-only path post-fix.
        h.last_run = Some(stub_run_meta());
        let r = ViolationRecord {
            event_index: 100,
            frame_index: 5,
            kind: ViolationKind::SsimBelowThreshold {
                ssim: 0.95,
                threshold: 0.99,
            },
            reproducer_seed: 42,
            start_at_event_idx: 0,
            log_excerpt: None,
        };
        fold_violation(&mut h, &r);
        assert!(h.is_safe());
        assert_eq!(h.critical_total(), 0);
        assert_eq!(h.minor_24h.values().sum::<u32>(), 1);
    }

    #[test]
    fn jsonl_violations_roundtrip() {
        let records = vec![
            ViolationRecord {
                event_index: 100,
                frame_index: 5,
                kind: ViolationKind::BlankFrame,
                reproducer_seed: 42,
                start_at_event_idx: 0,
                log_excerpt: None,
            },
            ViolationRecord {
                event_index: 250,
                frame_index: 12,
                kind: ViolationKind::StaleFullFrame {
                    stale_distance: 220,
                },
                reproducer_seed: 42,
                start_at_event_idx: 0,
                log_excerpt: Some("snapshot taken".to_string()),
            },
        ];
        let jsonl = render_violations_jsonl(&records);
        let parsed = parse_violations_jsonl(&jsonl).unwrap();
        assert_eq!(parsed, records);
    }

    #[test]
    fn artifact_subdir_zero_pads_event_index() {
        let r = ViolationRecord {
            event_index: 7,
            frame_index: 0,
            kind: ViolationKind::BlankFrame,
            reproducer_seed: 0,
            start_at_event_idx: 0,
            log_excerpt: None,
        };
        assert_eq!(r.artifact_subdir(), "violations/00000007");
    }
}
