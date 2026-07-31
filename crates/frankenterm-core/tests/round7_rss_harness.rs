//! Round-7 — deterministic fleet-resident-bytes RSS harness for the adaptive-M4
//! CDC-dedup candidate (bead `ft-6aban`, epic `ft-yjihu`).
//!
//! ## Why this exists
//!
//! M4 content-defined-chunking (CDC) dedup of warm scrollback pages was measured
//! in earlier rounds on a *compute* axis (per-flush nanoseconds), where it can
//! only lose: chunking + hashing is pure overhead on top of the legacy
//! standalone-zstd path. But CDC's *purpose* is not speed — it is **resident
//! memory**: a 200-pane fleet whose panes keep re-drawing the same screenful
//! (status dashboards, progress bars, `top`/`htop`, re-painted TUIs) stores the
//! same warm page over and over. CDC stores each unique chunk once, so the
//! fleet's resident warm-tier bytes collapse toward the *distinct* content,
//! not the *emitted* content.
//!
//! The key property — exactly as the round-6 A5 quality harness exploited for
//! hit-rate and reclaim-oscillation — is that **resident bytes are a
//! deterministic, pure function of (workload trace, scrollback config)**. The
//! warm-tier accounting (`warm_bytes`, surfaced by
//! [`TieredScrollback::estimated_memory_bytes`]) adds a page's *newly added*
//! compressed bytes and 0 for fully-deduplicated pages, so summing
//! `estimated_memory_bytes()` across a synthetic fleet yields the exact,
//! host-independent resident footprint. It needs **no quiet host**: one run is
//! the final word. That is what makes adaptive-M4 *adjudicable* where the noisy
//! compute benches never could.
//!
//! ## What it adjudicates
//!
//! Three arms, mirroring the `FT_MOONSHOT_SCROLLBACK_CDC_DEDUP` env gate but
//! selected **env-free** (so the function is pure and immune to concurrent-test
//! env races):
//!
//! * **off** (`unset`/`0`) — legacy standalone-zstd warm pages, the baseline.
//! * **always** (`1`/`true`) — CDC unconditionally on. Wins on redundant traces
//!   but *regresses* on incompressible/unique traces (chunk-store + per-chunk
//!   compression overhead with nothing to dedup).
//! * **adaptive** (`adaptive`) — the round-7 candidate: a cheap redundancy probe
//!   over the first sampled pages decides whether to engage CDC. It should win
//!   the redundant trace like `always`, yet tie `off` on the low-redundancy
//!   trace (the probe declines and falls back to standalone-zstd).
//!
//! Two deterministic traces drive every arm:
//!
//! * a **redundant terminal-redraw** trace (a fixed dashboard frame re-emitted
//!   with only a small cycling status header) — CDC's home turf, and
//!   adaptive must *win*.
//! * a **low-redundancy** trace (every line unique, high-entropy) — where
//!   `always` regresses and adaptive must *tie* `off` (no regression).
//!
//! The `#[test]` bodies are fail-closed proofs of the *harness* (both arms ran,
//! the probe actually engaged on the redundant trace and declined on the
//! low-redundancy trace, bytes are well-formed). The win/tie/regression verdict
//! itself is the emitted, exact data the orchestrator (cod_1) adjudicates.
//!
//! ```text
//! cargo test -p frankenterm-core --test round7_rss_harness -- --nocapture
//! ```
//!
//! ## Public harness API (consumed by cod_1 to adjudicate adaptive-M4)
//!
//! * [`CdcArm`] — `Off` | `Always` | `Adaptive`; `.env_label()` maps to the
//!   `FT_MOONSHOT_SCROLLBACK_CDC_DEDUP` value; `.construct(config)` builds a
//!   `TieredScrollback` env-free.
//! * [`harness_config`] returns the fixed [`ScrollbackConfig`] the verdict is
//!   computed under (cold eviction off so the full warm tier stays resident;
//!   small pages so the fleet flushes many warm pages).
//! * [`redundant_redraw_trace`] and [`low_redundancy_trace`] return the two
//!   deterministic `Vec<String>` traces.
//! * [`fleet_resident_bytes`] is the **pure metric**: total resident scrollback
//!   bytes for a `panes`-wide fleet, each pane fed `trace`, summing
//!   `estimated_memory_bytes()`. It returns [`FleetResident`] with
//!   `total_bytes`, `panes`, `per_pane_bytes`, `adaptive_engaged_panes`, and
//!   `sample_cdc_chunks`.

#![allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]

use frankenterm_core::scrollback_tiers::{ScrollbackConfig, TieredScrollback};

/// Relative margin below which a delta is "tie" (not a meaningful win/loss).
const MEANINGFUL_MARGIN: f64 = 0.01;

/// Panes in the synthetic fleet the resident bytes are summed across.
const FLEET_PANES: usize = 200;

// Scrollback geometry. Small hot/page sizes so each pane flushes *many* warm
// pages from a modest trace, making the resident warm tier — not the hot tier —
// dominate the footprint (CDC only dedups warm pages).
const HOT_LINES: usize = 64;
const PAGE_SIZE: usize = 64;

// Redundant terminal-redraw trace geometry.
const FRAME_LINES: usize = PAGE_SIZE; // one rendered frame == one flushed warm page
const REDRAW_FRAMES: usize = 128; // frames re-emitted; >> the adaptive probe window
/// Distinct status-header variants the redrawn frame cycles through. Low
/// cardinality => the fleet's warm tier holds ~this many distinct pages no
/// matter how many frames are drawn.
const STATUS_PERIOD: usize = 8;

/// Total lines per pane (kept identical across both traces for a fair compare).
const LINES_PER_PANE: usize = REDRAW_FRAMES * FRAME_LINES;

// ── Scorecard (same deterministic shape as the round-6 A5 harness) ──────────

/// A single adjudicated comparison: baseline arm vs candidate arm on one
/// deterministic metric.
struct Scorecard {
    candidate: &'static str,
    metric: &'static str,
    higher_is_better: bool,
    baseline_arm: &'static str,
    baseline_value: f64,
    candidate_arm: &'static str,
    candidate_value: f64,
}

impl Scorecard {
    fn rel_delta(&self) -> f64 {
        if self.baseline_value == 0.0 {
            if self.candidate_value == 0.0 {
                0.0
            } else {
                f64::INFINITY
            }
        } else {
            (self.candidate_value - self.baseline_value) / self.baseline_value
        }
    }

    fn verdict(&self) -> &'static str {
        let rel = self.rel_delta();
        if !rel.is_finite() || rel.abs() < MEANINGFUL_MARGIN {
            return "TIE";
        }
        let improved = if self.higher_is_better {
            self.candidate_value > self.baseline_value
        } else {
            self.candidate_value < self.baseline_value
        };
        if improved { "WIN" } else { "REGRESSION" }
    }

    fn print(&self) {
        println!(
            "{:<14} {:<28} {:<10}={:>16.1}  {:<10}={:>16.1}  rel={:>+9.2}%  -> {}",
            self.candidate,
            self.metric,
            self.baseline_arm,
            self.baseline_value,
            self.candidate_arm,
            self.candidate_value,
            self.rel_delta() * 100.0,
            self.verdict(),
        );
    }

    fn to_json(&self) -> String {
        format!(
            "{{\"candidate\":\"{}\",\"metric\":\"{}\",\"higher_is_better\":{},\
             \"baseline_arm\":\"{}\",\"baseline\":{:.1},\"candidate_arm\":\"{}\",\
             \"candidate\":{:.1},\"rel_delta\":{:.6},\"verdict\":\"{}\"}}",
            self.candidate,
            self.metric,
            self.higher_is_better,
            self.baseline_arm,
            self.baseline_value,
            self.candidate_arm,
            self.candidate_value,
            self.rel_delta(),
            self.verdict(),
        )
    }
}

// ── Harness API ─────────────────────────────────────────────────────────────

/// Which CDC dedup arm a pane runs under. Mirrors the
/// `FT_MOONSHOT_SCROLLBACK_CDC_DEDUP` env gate, but the construction is env-free
/// so [`fleet_resident_bytes`] stays a pure function of (trace, config).
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum CdcArm {
    /// `unset`/`0` — legacy standalone-zstd warm pages (the baseline).
    Off,
    /// `1`/`true` — CDC unconditionally engaged.
    Always,
    /// `adaptive` — the round-7 candidate: probe-then-decide.
    Adaptive,
}

impl CdcArm {
    /// The `FT_MOONSHOT_SCROLLBACK_CDC_DEDUP` value this arm corresponds to.
    fn env_label(self) -> &'static str {
        match self {
            CdcArm::Off => "unset",
            CdcArm::Always => "1",
            CdcArm::Adaptive => "adaptive",
        }
    }

    /// Short scorecard label.
    fn arm_label(self) -> &'static str {
        match self {
            CdcArm::Off => "off",
            CdcArm::Always => "always",
            CdcArm::Adaptive => "adaptive",
        }
    }

    /// Construct a `TieredScrollback` for this arm, env-free. The promoted Q1
    /// prefix index is enabled (production default) — it does not affect
    /// `estimated_memory_bytes` (only hot + warm bytes are counted).
    fn construct(self, config: &ScrollbackConfig) -> TieredScrollback {
        match self {
            CdcArm::Off => TieredScrollback::new_with_options(config.clone(), true, false),
            CdcArm::Always => TieredScrollback::new_with_options(config.clone(), true, true),
            CdcArm::Adaptive => TieredScrollback::new_with_adaptive_cdc(config.clone(), true),
        }
    }
}

/// Resident-bytes verdict for one (trace, arm) cell of the fleet.
pub struct FleetResident {
    /// Sum of `estimated_memory_bytes()` across the whole fleet.
    pub total_bytes: usize,
    /// Number of panes summed.
    pub panes: usize,
    /// `total_bytes / panes` (panes are identical, so this is the per-pane RSS).
    pub per_pane_bytes: usize,
    /// How many panes had their adaptive CDC probe decide to *engage* dedup.
    /// `0` for non-adaptive arms; `panes` when adaptive fully engaged.
    pub adaptive_engaged_panes: usize,
    /// Distinct interned CDC chunks on a sample pane (`None` when CDC is off /
    /// the adaptive probe declined and never allocated a store).
    pub sample_cdc_chunks: Option<usize>,
}

/// The fixed scrollback config the resident-bytes verdict is computed under.
///
/// Cold eviction is **disabled** so every flushed warm page stays resident: the
/// metric is then the pure "bytes the warm tier holds for this workload",
/// uncontaminated by eviction policy. `warm_max_bytes` is set high for the same
/// reason. Pages are small so a modest trace flushes many warm pages.
#[must_use]
pub fn harness_config() -> ScrollbackConfig {
    ScrollbackConfig {
        hot_lines: HOT_LINES,
        page_size: PAGE_SIZE,
        warm_max_bytes: usize::MAX,
        cold_eviction_enabled: false,
        ..ScrollbackConfig::default()
    }
}

/// Run one pane: feed the whole trace, then read its resident footprint and CDC
/// engagement. Deterministic and env-free.
fn run_pane(
    trace: &[String],
    arm: CdcArm,
    config: &ScrollbackConfig,
) -> (usize, bool, Option<usize>) {
    let mut sb = arm.construct(config);
    for line in trace {
        sb.push_line(line.clone());
    }
    let resident = sb.estimated_memory_bytes();
    let engaged = sb
        .cdc_adaptive_snapshot()
        .map(|snap| snap.enabled)
        .unwrap_or(false);
    let chunks = sb.cdc_stats().map(|(unique, _total)| unique);
    (resident, engaged, chunks)
}

/// **Pure metric.** Total resident scrollback bytes for a `panes`-wide fleet,
/// every pane fed the same `trace`, under CDC `arm`, summed from
/// [`TieredScrollback::estimated_memory_bytes`]. No host timing, no env reads —
/// the same inputs always yield the same bytes.
#[must_use]
pub fn fleet_resident_bytes(
    trace: &[String],
    arm: CdcArm,
    panes: usize,
    config: &ScrollbackConfig,
) -> FleetResident {
    let mut total = 0usize;
    let mut adaptive_engaged_panes = 0usize;
    let mut sample_cdc_chunks = None;
    for _ in 0..panes {
        let (resident, engaged, chunks) = run_pane(trace, arm, config);
        total = total.saturating_add(resident);
        if engaged {
            adaptive_engaged_panes += 1;
        }
        if sample_cdc_chunks.is_none() {
            sample_cdc_chunks = chunks;
        }
    }
    FleetResident {
        total_bytes: total,
        panes,
        per_pane_bytes: total.checked_div(panes).unwrap_or(0),
        adaptive_engaged_panes,
        sample_cdc_chunks,
    }
}

// ── Deterministic traces ────────────────────────────────────────────────────

/// A redundant terminal-redraw trace: a fixed dashboard frame re-emitted
/// [`REDRAW_FRAMES`] times, with only a small cycling status header (one of
/// [`STATUS_PERIOD`] variants) changing between frames. The body is byte-stable
/// across every frame, so CDC chunks dedup down to a handful of distinct pages
/// regardless of how many frames are drawn — the exact pattern an idle TUI,
/// progress bar, or status dashboard produces.
#[must_use]
pub fn redundant_redraw_trace() -> Vec<String> {
    let body = dashboard_body();
    let mut trace = Vec::with_capacity(LINES_PER_PANE);
    for frame in 0..REDRAW_FRAMES {
        let variant = frame % STATUS_PERIOD;
        trace.push(format!(
            "┌─ session {variant:02} ─ pane render #{variant} ─ cpu {:>2}% mem {:>2}% ─ READY ┐",
            12 + variant * 3,
            41 + variant * 2,
        ));
        // body fills the frame out to FRAME_LINES total lines.
        for line in body.iter().take(FRAME_LINES - 1) {
            trace.push(line.clone());
        }
    }
    debug_assert_eq!(trace.len(), LINES_PER_PANE);
    trace
}

/// A low-redundancy trace: every line is unique, high-entropy content (a
/// deterministic LCG over a hex alphabet). No two pages share chunks, so CDC
/// finds nothing to dedup — the case `always` regresses on and `adaptive` must
/// detect and decline. Same total length as [`redundant_redraw_trace`].
#[must_use]
pub fn low_redundancy_trace() -> Vec<String> {
    let mut state: u64 = 0x9E37_79B9_7F4A_7C15; // fixed seed → reproducible
    let mut trace = Vec::with_capacity(LINES_PER_PANE);
    for _ in 0..LINES_PER_PANE {
        // ~72 hex chars of high-entropy content per line, all distinct.
        let mut line = String::with_capacity(72);
        for _ in 0..9 {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            line.push_str(&format!("{:08x}", (state >> 32) as u32));
        }
        trace.push(line);
    }
    trace
}

/// A fixed, content-rich dashboard body reused as the redundant frame's stable
/// region. Deterministic; identical on every call.
fn dashboard_body() -> Vec<String> {
    let mut body = Vec::with_capacity(FRAME_LINES - 1);
    for row in 0..(FRAME_LINES - 1) {
        let bar = "█".repeat(1 + row % 24);
        let dots = "·".repeat(24 - row % 24);
        body.push(format!(
            "│ worker_{:02} q={:>3} lat={:>3}ms {bar}{dots} state=running heartbeat=ok │",
            row,
            (row * 7) % 256,
            10 + (row * 13) % 90,
        ));
    }
    body
}

// ── Tests (fail-closed harness proofs) + scorecard emission ─────────────────

#[test]
fn adaptive_m4_rss_win_on_redundant_redraw_adjudicated() {
    let config = harness_config();
    let trace = redundant_redraw_trace();

    let off = fleet_resident_bytes(&trace, CdcArm::Off, FLEET_PANES, &config);
    let always = fleet_resident_bytes(&trace, CdcArm::Always, FLEET_PANES, &config);
    let adaptive = fleet_resident_bytes(&trace, CdcArm::Adaptive, FLEET_PANES, &config);

    let cards = [
        Scorecard {
            candidate: "M4-always",
            metric: "fleet_rss@redundant_redraw",
            higher_is_better: false,
            baseline_arm: CdcArm::Off.arm_label(),
            baseline_value: off.total_bytes as f64,
            candidate_arm: CdcArm::Always.arm_label(),
            candidate_value: always.total_bytes as f64,
        },
        Scorecard {
            candidate: "M4-adaptive",
            metric: "fleet_rss@redundant_redraw",
            higher_is_better: false,
            baseline_arm: CdcArm::Off.arm_label(),
            baseline_value: off.total_bytes as f64,
            candidate_arm: CdcArm::Adaptive.arm_label(),
            candidate_value: adaptive.total_bytes as f64,
        },
    ];

    println!(
        "\n=== ROUND-7 RSS scorecard: adaptive-M4 on redundant terminal-redraw (deterministic) ==="
    );
    println!(
        "fleet_panes={FLEET_PANES} lines/pane={LINES_PER_PANE} frame_lines={FRAME_LINES} \
         redraw_frames={REDRAW_FRAMES} status_period={STATUS_PERIOD} \
         | env_gate FT_MOONSHOT_SCROLLBACK_CDC_DEDUP: off={} always={} adaptive={}",
        CdcArm::Off.env_label(),
        CdcArm::Always.env_label(),
        CdcArm::Adaptive.env_label(),
    );
    println!(
        "  per-pane RSS: off={}B always={}B adaptive={}B | adaptive engaged {}/{} panes \
         | distinct CDC chunks: always={:?} adaptive={:?}",
        off.per_pane_bytes,
        always.per_pane_bytes,
        adaptive.per_pane_bytes,
        adaptive.adaptive_engaged_panes,
        FLEET_PANES,
        always.sample_cdc_chunks,
        adaptive.sample_cdc_chunks,
    );
    for c in &cards {
        c.print();
        println!("ROUND7_RSS_JSON {}", c.to_json());
    }

    // Fail-closed harness invariants (NOT the verdict — that is the emitted data).
    assert!(
        off.total_bytes > 0,
        "off arm stored no resident bytes — trace did not flush warm pages"
    );
    assert_eq!(
        off.per_pane_bytes * off.panes,
        off.total_bytes,
        "fleet sum must be panes * per-pane"
    );
    assert!(
        adaptive.adaptive_engaged_panes == FLEET_PANES,
        "adaptive probe failed to engage on a maximally-redundant trace ({}/{} panes) — \
         the harness cannot adjudicate an RSS win it never measured",
        adaptive.adaptive_engaged_panes,
        FLEET_PANES,
    );
    assert!(
        adaptive.sample_cdc_chunks.unwrap_or(0) > 0,
        "adaptive engaged but interned no CDC chunks"
    );
}

#[test]
fn adaptive_m4_no_regression_on_low_redundancy_adjudicated() {
    let config = harness_config();
    let trace = low_redundancy_trace();

    let off = fleet_resident_bytes(&trace, CdcArm::Off, FLEET_PANES, &config);
    let always = fleet_resident_bytes(&trace, CdcArm::Always, FLEET_PANES, &config);
    let adaptive = fleet_resident_bytes(&trace, CdcArm::Adaptive, FLEET_PANES, &config);

    let cards = [
        Scorecard {
            candidate: "M4-always",
            metric: "fleet_rss@low_redundancy",
            higher_is_better: false,
            baseline_arm: CdcArm::Off.arm_label(),
            baseline_value: off.total_bytes as f64,
            candidate_arm: CdcArm::Always.arm_label(),
            candidate_value: always.total_bytes as f64,
        },
        Scorecard {
            candidate: "M4-adaptive",
            metric: "fleet_rss@low_redundancy",
            higher_is_better: false,
            baseline_arm: CdcArm::Off.arm_label(),
            baseline_value: off.total_bytes as f64,
            candidate_arm: CdcArm::Adaptive.arm_label(),
            candidate_value: adaptive.total_bytes as f64,
        },
    ];

    println!(
        "\n=== ROUND-7 RSS scorecard: adaptive-M4 on low-redundancy trace (deterministic) ==="
    );
    println!(
        "fleet_panes={FLEET_PANES} lines/pane={LINES_PER_PANE} (all unique) \
         | adaptive engaged {}/{} panes (expect 0: probe declines)",
        adaptive.adaptive_engaged_panes, FLEET_PANES,
    );
    println!(
        "  per-pane RSS: off={}B always={}B adaptive={}B",
        off.per_pane_bytes, always.per_pane_bytes, adaptive.per_pane_bytes,
    );
    for c in &cards {
        c.print();
        println!("ROUND7_RSS_JSON {}", c.to_json());
    }

    // Fail-closed harness invariants.
    assert!(
        off.total_bytes > 0,
        "off arm stored no resident bytes on low-redundancy trace"
    );
    assert_eq!(
        adaptive.adaptive_engaged_panes, 0,
        "adaptive probe engaged CDC on a low-redundancy trace — it must decline to avoid the \
         overhead that makes `always` regress here"
    );
    // The whole point of adaptive: declining the probe means it falls back to the
    // exact standalone-zstd representation, so its bytes equal `off` byte-for-byte.
    assert_eq!(
        adaptive.total_bytes, off.total_bytes,
        "adaptive declined CDC yet its resident bytes differ from off — fallback is not byte-identical"
    );
}
