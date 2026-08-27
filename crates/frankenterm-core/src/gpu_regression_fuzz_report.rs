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

use crate::a11y_tree::{
    AccessibilityEvent, AccessibilityPlatform, AccessibilityScenario, AnnouncePriority,
    InvariantViolation, check_invariants,
};

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
    /// A related fixture exists, but it does not exercise the
    /// exact scenario path from the renderer catalog.
    Partial,
    /// A follow-on bead must ship this fixture.
    Gap,
    /// Cross-references another bead's harness (e.g.,
    /// screen-reader-active needs the A11y comparator).
    BlockedOnSubBead,
    /// A platform-agnostic, headless contract scenario ships and
    /// runs in CI (e.g. `screen-reader-active` via the
    /// [`crate::a11y_tree`] event-stream comparator). Native
    /// per-platform recorder proof is represented by an explicit
    /// pass/fail/skipped comparison result so unavailable OS AT
    /// services cannot be mistaken for green proof.
    HeadlessShipped,
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
    /// Optional follow-on bead handle for partial, gap, or blocked entries.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub blocked_on: Option<String>,
}

/// The full 18-scenario manifest, projected from the bead
/// description.
#[must_use]
pub fn scenario_manifest() -> Vec<ScenarioRecord> {
    use ScenarioStatus::{Gap, HeadlessShipped, Partial, Shipped};
    let req = |s: &str| s.to_string();
    let scenario_corpus = Some("ft-ruona");
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
        // ----- 18-row renderer-overhaul scenario catalog -----
        row("steady-typing", &["RQ-S8"], Gap, scenario_corpus),
        row("vim-edit", &["RQ-S6"], Gap, scenario_corpus),
        row("htop-top", &["RQ-S5", "RQ-S8"], Gap, scenario_corpus),
        row("neofetch-banner", &["RQ-S11"], Gap, scenario_corpus),
        row("resize-step", &["RQ-S1"], Partial, scenario_corpus),
        row("resize-burst", &["RQ-S1", "RQ-S10"], Gap, scenario_corpus),
        row("scroll-stress", &["RQ-S6"], Shipped, None),
        row("selection-drag", &[], Partial, scenario_corpus),
        row("scrollback-search", &[], Gap, scenario_corpus),
        row("multi-pane-split", &["RQ-S12"], Shipped, None),
        row("dpi-change", &["RQ-S10"], Gap, scenario_corpus),
        row("font-change", &["RQ-S10"], Gap, scenario_corpus),
        row("alt-screen", &[], Gap, scenario_corpus),
        row("mouse-tracking", &[], Gap, scenario_corpus),
        row("wide-gamut", &[], Gap, scenario_corpus),
        row("rtl-script", &[], Shipped, None),
        row("cjk-mixed", &[], Shipped, None),
        // The headless a11y event-stream contract for this scenario
        // ships here (see `screen_reader_active_golden` /
        // `screen_reader_active_violations`) alongside the native
        // per-platform comparator result contract.
        row("screen-reader-active", &[], HeadlessShipped, None),
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
    /// Scenarios whose headless contract ships in CI while their
    /// native per-platform comparator is a tracked follow-up.
    pub headless_shipped: u32,
    pub scenarios: Vec<ScenarioRecord>,
}

#[must_use]
pub fn coverage_snapshot() -> ScenarioCoverageSnapshot {
    let scenarios = scenario_manifest();
    let mut shipped = 0;
    let mut partial = 0;
    let mut gap = 0;
    let mut blocked = 0;
    let mut headless_shipped = 0;
    for s in &scenarios {
        match s.status {
            ScenarioStatus::Shipped => shipped += 1,
            ScenarioStatus::Partial => partial += 1,
            ScenarioStatus::Gap => gap += 1,
            ScenarioStatus::BlockedOnSubBead => blocked += 1,
            ScenarioStatus::HeadlessShipped => headless_shipped += 1,
        }
    }
    ScenarioCoverageSnapshot {
        scenarios_total: scenarios.len() as u32,
        shipped,
        partial,
        gap,
        blocked,
        headless_shipped,
        scenarios,
    }
}

// ============================================================================
// screen-reader-active scenario (#18) — headless a11y contract
// ============================================================================

/// Stable slug for the renderer-golden `screen-reader-active`
/// scenario (#18) — matches `tests/renderer_golden/SCENARIOS.md`.
pub const SCREEN_READER_ACTIVE_SLUG: &str = "screen-reader-active";

/// Active assistive-technology session state for the
/// `screen-reader-active` renderer scenario (#18).
///
/// The renderer/terminal accessibility path is gated on whether a
/// screen reader is actually attached: while one is active it must
/// surface focus, text, and announcement events through the
/// [`crate::a11y_tree`] contract; when none is attached it must NOT
/// spend work emitting announcements no AT client will consume.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScreenReaderSession {
    /// Whether an assistive-technology client is attached.
    pub active: bool,
    /// Which platform AT framework the session speaks. `Synthetic`
    /// is the headless contract this scenario runs against until the
    /// per-platform comparators (ft-5pk4h) land.
    pub platform: AccessibilityPlatform,
}

impl ScreenReaderSession {
    /// An attached (active) screen-reader session on `platform`.
    #[must_use]
    pub const fn active(platform: AccessibilityPlatform) -> Self {
        Self {
            active: true,
            platform,
        }
    }

    /// A detached (inactive) session on `platform`.
    #[must_use]
    pub const fn inactive(platform: AccessibilityPlatform) -> Self {
        Self {
            active: false,
            platform,
        }
    }
}

/// Contract violations specific to the `screen-reader-active`
/// scenario, layered on top of the structural
/// [`InvariantViolation`]s the shared a11y contract enforces.
// Externally tagged (no `tag = "kind"`): the `A11yInvariant`
// newtype wraps `InvariantViolation`, which is itself internally
// tagged on `kind`, so an internal tag here would collide on that
// key. External tagging nests the inner violation cleanly.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScreenReaderContractViolation {
    /// The session was active but the stream carried no
    /// announcement — a live screen reader would have nothing to
    /// speak.
    ActiveSessionMissingAnnouncement,
    /// The session was active but the terminal never reported
    /// focus, so no element is addressable by the AT client.
    ActiveSessionMissingFocus,
    /// An inactive session still emitted an announcement — wasted
    /// AT work for a detached client.
    InactiveSessionEmittedAnnouncement { index: usize },
    /// A shared a11y-contract invariant was violated.
    A11yInvariant(InvariantViolation),
}

/// Deterministic, headless golden event stream for the
/// `screen-reader-active` scenario.
///
/// Models a focused terminal pane under an attached screen reader:
/// the pane gains AT focus, its text value changes as the agent
/// types, and (only when a reader is attached) an assertive
/// announcement is surfaced. The structural focus/text events are
/// identical whether or not a reader is attached; only announcement
/// emission is gated on `session.active`.
#[must_use]
pub fn screen_reader_active_golden(session: ScreenReaderSession) -> Vec<AccessibilityEvent> {
    let mut events = vec![
        AccessibilityEvent::FocusChanged {
            ts_ms: 0,
            role: "Terminal".to_string(),
            name: "pane:1".to_string(),
        },
        AccessibilityEvent::TextValueChanged {
            ts_ms: 10,
            role: "Terminal".to_string(),
            name: "pane:1".to_string(),
            value: "build succeeded".to_string(),
        },
    ];
    if session.active {
        events.push(AccessibilityEvent::AnnounceMessage {
            ts_ms: 20,
            priority: AnnouncePriority::Assertive,
            value: "build succeeded".to_string(),
        });
    }
    events
}

/// Compare a recorded `screen-reader-active` event stream against
/// the scenario contract. Returns the accumulated violations (empty
/// = the recorder honored the active/inactive announcement gate and
/// every shared a11y invariant).
///
/// The scenario maps onto the contract's steady-typing flow (a
/// focused element receiving text), so the shared
/// [`check_invariants`] pass runs against
/// [`AccessibilityScenario::SteadyTyping`].
#[must_use]
pub fn screen_reader_active_violations(
    session: ScreenReaderSession,
    events: &[AccessibilityEvent],
) -> Vec<ScreenReaderContractViolation> {
    let mut violations = Vec::new();

    let has_announcement = events
        .iter()
        .any(|e| matches!(e, AccessibilityEvent::AnnounceMessage { .. }));
    let has_focus = events
        .iter()
        .any(|e| matches!(e, AccessibilityEvent::FocusChanged { .. }));

    if session.active {
        if !has_focus {
            violations.push(ScreenReaderContractViolation::ActiveSessionMissingFocus);
        }
        if !has_announcement {
            violations.push(ScreenReaderContractViolation::ActiveSessionMissingAnnouncement);
        }
    } else {
        for (index, event) in events.iter().enumerate() {
            if matches!(event, AccessibilityEvent::AnnounceMessage { .. }) {
                violations.push(
                    ScreenReaderContractViolation::InactiveSessionEmittedAnnouncement { index },
                );
            }
        }
    }

    for violation in check_invariants(AccessibilityScenario::SteadyTyping, events) {
        violations.push(ScreenReaderContractViolation::A11yInvariant(violation));
    }

    violations
}

/// Availability of the native OS recorder that supplied an event
/// stream for [`compare_native_screen_reader_events`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum NativeScreenReaderRecorderState {
    /// The platform recorder ran and produced an event stream.
    Available,
    /// The platform recorder could not run; the comparison is
    /// retained as an explicit skip instead of silently passing.
    Unavailable { reason: String },
}

impl NativeScreenReaderRecorderState {
    /// Convenience constructor for available recorder state.
    #[must_use]
    pub const fn available() -> Self {
        Self::Available
    }

    /// Convenience constructor for skipped recorder state.
    #[must_use]
    pub fn unavailable(reason: impl Into<String>) -> Self {
        Self::Unavailable {
            reason: reason.into(),
        }
    }

    #[must_use]
    pub const fn is_available(&self) -> bool {
        matches!(self, Self::Available)
    }
}

/// Verdict for the native screen-reader comparator.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NativeScreenReaderComparisonStatus {
    /// Native recorder output satisfied the screen-reader contract.
    Pass,
    /// Native recorder output ran and violated the contract.
    Fail,
    /// Native recorder did not run; the missing platform proof is
    /// represented explicitly and must not be counted as green.
    Skipped,
}

/// Retained comparison result for `screen-reader-active` against a
/// native platform recorder (AT-SPI / `NSAccessibility` / UIAutomation).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NativeScreenReaderComparison {
    pub scenario_slug: String,
    pub platform: AccessibilityPlatform,
    pub framework: String,
    pub screen_reader_active: bool,
    pub recorder_state: NativeScreenReaderRecorderState,
    pub status: NativeScreenReaderComparisonStatus,
    pub violations: Vec<ScreenReaderContractViolation>,
}

impl NativeScreenReaderComparison {
    #[must_use]
    pub const fn passed(&self) -> bool {
        matches!(self.status, NativeScreenReaderComparisonStatus::Pass)
    }

    #[must_use]
    pub const fn skipped(&self) -> bool {
        matches!(self.status, NativeScreenReaderComparisonStatus::Skipped)
    }
}

/// Compare a native screen-reader recorder stream against the
/// `screen-reader-active` contract.
///
/// This is intentionally split from live OS probing: macOS VoiceOver,
/// Linux Orca/AT-SPI, and Windows Narrator/UIA availability varies by
/// worker. Callers pass the recorder availability they observed, and
/// this function records `skipped` when native proof was unavailable.
#[must_use]
pub fn compare_native_screen_reader_events(
    session: ScreenReaderSession,
    recorder_state: NativeScreenReaderRecorderState,
    events: &[AccessibilityEvent],
) -> NativeScreenReaderComparison {
    let Some(framework) = session.platform.native_screen_reader_framework() else {
        return NativeScreenReaderComparison {
            scenario_slug: SCREEN_READER_ACTIVE_SLUG.to_string(),
            platform: session.platform,
            framework: "synthetic".to_string(),
            screen_reader_active: session.active,
            recorder_state: NativeScreenReaderRecorderState::unavailable(
                "synthetic platform is covered by the headless contract, not the native comparator",
            ),
            status: NativeScreenReaderComparisonStatus::Skipped,
            violations: Vec::new(),
        };
    };

    if !recorder_state.is_available() {
        return NativeScreenReaderComparison {
            scenario_slug: SCREEN_READER_ACTIVE_SLUG.to_string(),
            platform: session.platform,
            framework: framework.to_string(),
            screen_reader_active: session.active,
            recorder_state,
            status: NativeScreenReaderComparisonStatus::Skipped,
            violations: Vec::new(),
        };
    }

    let violations = screen_reader_active_violations(session, events);
    let status = if violations.is_empty() {
        NativeScreenReaderComparisonStatus::Pass
    } else {
        NativeScreenReaderComparisonStatus::Fail
    };
    NativeScreenReaderComparison {
        scenario_slug: SCREEN_READER_ACTIVE_SLUG.to_string(),
        platform: session.platform,
        framework: framework.to_string(),
        screen_reader_active: session.active,
        recorder_state,
        status,
        violations,
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
    fn scenario_manifest_has_18_entries() {
        // Keep the machine-readable manifest aligned with the
        // renderer-overhaul catalog, not with stale continuation
        // bookkeeping from older beads.
        let m = scenario_manifest();
        assert_eq!(m.len(), 18);
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
    fn scenario_manifest_matches_renderer_catalog_order() {
        let manifest = scenario_manifest();
        let slugs: Vec<&str> = manifest.iter().map(|s| s.slug.as_str()).collect();
        assert_eq!(
            slugs,
            vec![
                "steady-typing",
                "vim-edit",
                "htop-top",
                "neofetch-banner",
                "resize-step",
                "resize-burst",
                "scroll-stress",
                "selection-drag",
                "scrollback-search",
                "multi-pane-split",
                "dpi-change",
                "font-change",
                "alt-screen",
                "mouse-tracking",
                "wide-gamut",
                "rtl-script",
                "cjk-mixed",
                "screen-reader-active",
            ]
        );
    }

    #[test]
    fn scenario_manifest_points_gaps_at_live_beads() {
        for scenario in scenario_manifest() {
            match scenario.status {
                ScenarioStatus::Shipped => {
                    assert!(
                        scenario.blocked_on.is_none(),
                        "shipped scenario {} should not point at a follow-on",
                        scenario.slug
                    );
                }
                ScenarioStatus::Partial | ScenarioStatus::Gap => {
                    assert_eq!(
                        scenario.blocked_on.as_deref(),
                        Some("ft-ruona"),
                        "scenario {} should point at the non-a11y corpus bead",
                        scenario.slug
                    );
                }
                ScenarioStatus::BlockedOnSubBead => {
                    assert_eq!(
                        scenario.blocked_on.as_deref(),
                        Some("ft-0q5zm"),
                        "scenario {} should point at the a11y comparator bead",
                        scenario.slug
                    );
                }
                ScenarioStatus::HeadlessShipped => {
                    assert_eq!(
                        scenario.blocked_on.as_deref(),
                        None,
                        "headless scenario {} has its native comparator contract",
                        scenario.slug
                    );
                }
            }
        }
    }

    #[test]
    fn screen_reader_active_golden_satisfies_contract() {
        let session = ScreenReaderSession::active(AccessibilityPlatform::Synthetic);
        let events = screen_reader_active_golden(session);
        assert!(
            events
                .iter()
                .any(|e| matches!(e, AccessibilityEvent::AnnounceMessage { .. })),
            "active session must surface an announcement"
        );
        let violations = screen_reader_active_violations(session, &events);
        assert!(
            violations.is_empty(),
            "active golden must satisfy the contract: {violations:?}"
        );
    }

    #[test]
    fn screen_reader_inactive_suppresses_announcements() {
        let session = ScreenReaderSession::inactive(AccessibilityPlatform::Synthetic);
        let events = screen_reader_active_golden(session);
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, AccessibilityEvent::AnnounceMessage { .. })),
            "inactive session must not emit announcements"
        );
        let violations = screen_reader_active_violations(session, &events);
        assert!(
            violations.is_empty(),
            "inactive golden must satisfy the contract: {violations:?}"
        );
    }

    #[test]
    fn active_session_without_announcement_is_a_violation() {
        let session = ScreenReaderSession::active(AccessibilityPlatform::Synthetic);
        // A live reader that gets focus but no announcement would be
        // silent — the comparator must catch it.
        let events = vec![AccessibilityEvent::FocusChanged {
            ts_ms: 0,
            role: "Terminal".to_string(),
            name: "pane:1".to_string(),
        }];
        let violations = screen_reader_active_violations(session, &events);
        assert!(
            violations.contains(&ScreenReaderContractViolation::ActiveSessionMissingAnnouncement),
            "missing announcement under an active session must be flagged: {violations:?}"
        );
    }

    #[test]
    fn inactive_session_emitting_announcement_is_a_violation() {
        let session = ScreenReaderSession::inactive(AccessibilityPlatform::Synthetic);
        let events = vec![
            AccessibilityEvent::FocusChanged {
                ts_ms: 0,
                role: "Terminal".to_string(),
                name: "pane:1".to_string(),
            },
            AccessibilityEvent::AnnounceMessage {
                ts_ms: 10,
                priority: AnnouncePriority::Polite,
                value: "leaked".to_string(),
            },
        ];
        let violations = screen_reader_active_violations(session, &events);
        assert_eq!(
            violations,
            vec![ScreenReaderContractViolation::InactiveSessionEmittedAnnouncement { index: 1 }],
            "an announcement with no attached reader must be the sole violation"
        );
    }

    #[test]
    fn screen_reader_active_scenario_is_headless_shipped() {
        let row = scenario_manifest()
            .into_iter()
            .find(|s| s.slug == SCREEN_READER_ACTIVE_SLUG)
            .expect("manifest must contain screen-reader-active");
        assert_eq!(row.status, ScenarioStatus::HeadlessShipped);
        assert_eq!(
            row.blocked_on.as_deref(),
            None,
            "native comparator contract has landed; live recorder availability is explicit"
        );
        // No live native AT recorder is wired yet; the comparator
        // reports skipped when the platform recorder is unavailable.
        assert!(!AccessibilityPlatform::MacosNsAccessibility.is_wired());
        assert!(!AccessibilityPlatform::LinuxAtSpi.is_wired());
        assert!(!AccessibilityPlatform::WindowsUiAutomation.is_wired());
        assert!(AccessibilityPlatform::Synthetic.is_wired());
    }

    #[test]
    fn native_screen_reader_frameworks_are_named() {
        assert_eq!(
            AccessibilityPlatform::MacosNsAccessibility.native_screen_reader_framework(),
            Some("NSAccessibility")
        );
        assert_eq!(
            AccessibilityPlatform::LinuxAtSpi.native_screen_reader_framework(),
            Some("AT-SPI")
        );
        assert_eq!(
            AccessibilityPlatform::WindowsUiAutomation.native_screen_reader_framework(),
            Some("UIAutomation")
        );
        assert_eq!(
            AccessibilityPlatform::Synthetic.native_screen_reader_framework(),
            None
        );
    }

    #[test]
    fn native_screen_reader_comparator_passes_platform_golden() {
        let session = ScreenReaderSession::active(AccessibilityPlatform::MacosNsAccessibility);
        let events = screen_reader_active_golden(session);

        let comparison = compare_native_screen_reader_events(
            session,
            NativeScreenReaderRecorderState::available(),
            &events,
        );

        assert!(comparison.passed(), "{comparison:?}");
        assert_eq!(comparison.framework, "NSAccessibility");
        assert_eq!(comparison.violations, [] as [gpu_regression_fuzz_report::ScreenReaderContractViolation; 0]);
    }

    #[test]
    fn native_screen_reader_comparator_fails_missing_announcement() {
        let session = ScreenReaderSession::active(AccessibilityPlatform::LinuxAtSpi);
        let events = vec![AccessibilityEvent::FocusChanged {
            ts_ms: 0,
            role: "Terminal".to_string(),
            name: "pane:1".to_string(),
        }];

        let comparison = compare_native_screen_reader_events(
            session,
            NativeScreenReaderRecorderState::available(),
            &events,
        );

        assert_eq!(comparison.status, NativeScreenReaderComparisonStatus::Fail);
        assert_eq!(comparison.framework, "AT-SPI");
        assert!(
            comparison
                .violations
                .contains(&ScreenReaderContractViolation::ActiveSessionMissingAnnouncement)
        );
    }

    #[test]
    fn native_screen_reader_comparator_skips_unavailable_recorder() {
        let session = ScreenReaderSession::active(AccessibilityPlatform::WindowsUiAutomation);
        let events = screen_reader_active_golden(session);

        let comparison = compare_native_screen_reader_events(
            session,
            NativeScreenReaderRecorderState::unavailable("Narrator is not running on this worker"),
            &events,
        );

        assert!(comparison.skipped(), "{comparison:?}");
        assert_eq!(comparison.framework, "UIAutomation");
        assert_eq!(comparison.violations, [] as [gpu_regression_fuzz_report::ScreenReaderContractViolation; 0]);
    }

    #[test]
    fn native_screen_reader_comparator_rejects_synthetic_platform() {
        let session = ScreenReaderSession::active(AccessibilityPlatform::Synthetic);
        let events = screen_reader_active_golden(session);

        let comparison = compare_native_screen_reader_events(
            session,
            NativeScreenReaderRecorderState::available(),
            &events,
        );

        assert!(comparison.skipped(), "{comparison:?}");
        assert_eq!(comparison.framework, "synthetic");
    }

    #[test]
    fn coverage_snapshot_counts_headless_shipped() {
        let snap = coverage_snapshot();
        assert_eq!(
            snap.headless_shipped, 1,
            "exactly screen-reader-active is headless-shipped"
        );
        assert_eq!(
            snap.blocked, 0,
            "no scenario should remain blocked on an unnamed a11y sub-bead"
        );
    }

    #[test]
    fn coverage_snapshot_counts_match_bead() {
        let s = coverage_snapshot();
        assert_eq!(s.scenarios_total, 18);
        // The catalog has 4 fully shipped rows, 2 partial rows
        // that need additive coverage, 11 non-a11y gap rows, and
        // one a11y scenario with a headless + native comparator
        // contract.
        assert_eq!(s.shipped, 4);
        assert_eq!(s.partial, 2);
        assert_eq!(s.gap, 11);
        assert_eq!(s.blocked, 0);
        assert_eq!(s.headless_shipped, 1);
        assert_eq!(s.shipped + s.partial, 6);
        assert_eq!(s.partial + s.gap, 13);
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
        assert!(!h.is_safe(), "cold baseline must be unsafe (no run yet)");
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
