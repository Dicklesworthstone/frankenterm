//! `ft doctor` surface for atlas tiered-swap telemetry (ft-ktd19.2).
//!
//! Substrate slice. Mirrors the [`crate::atlas_doctor`] shape but
//! operates on the `TierSwapStats` counters from
//! [`crate::atlas_tiered_swap`] — VRAM + host RAM peaks, swap-in /
//! swap-out tallies per tier, and disk-eviction count. Surfaces a
//! pressure-warning bucket so ops can spot a runaway eviction loop
//! before it manifests as visible flicker.
//!
//! ## Why a separate module
//!
//! The substrate's `TierSwapStats` is intentionally serde-free
//! (the pure-logic atlas-tiered-swap layer never picks up the
//! dependency). The doctor surface wants stable serde-shaped
//! types so the JSON output mode (`ft doctor --json`) is regression-
//! locked. This module bridges the two: a serde mirror record
//! (`TierSwapStatsRecord`) + per-row thresholds + a diagnostic-lines
//! translator.
//!
//! ## What is deferred
//!
//! - Platform memory probes (macOS sysctl / Linux `/proc/meminfo`
//!   / Windows `GlobalMemoryStatusEx`) populating VRAM + host RAM
//!   budgets — the bead's "platform memory probes" item is a
//!   per-OS slice that lands separately. The doctor surface here
//!   carries optional `vram_budget_bytes` / `host_ram_budget_bytes`
//!   fields so the probe can plug in once it ships.
//! - Standalone CLI invocations still emit the typed sentinel
//!   (`no_atlases_in_process`), matching the
//!   [`crate::atlas_doctor`] pattern. GUI-side doctor callers
//!   populate [`TierSwapDoctorReport::from_rows`] from the live
//!   glyph atlas registry before rendering this shared report shape.

use serde::{Deserialize, Serialize};

use crate::atlas_tiered_swap::TierSwapStats;

/// Serde mirror of [`TierSwapStats`]. `From<TierSwapStats>` carries
/// every field verbatim so the wire shape stays in lockstep with
/// the substrate counter set.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TierSwapStatsRecord {
    pub vram_peak_bytes: u64,
    pub host_ram_peak_bytes: u64,
    pub vram_swap_in_count: u64,
    pub host_ram_swap_in_count: u64,
    pub vram_swap_out_count: u64,
    pub host_ram_swap_out_count: u64,
    pub disk_eviction_count: u64,
    pub swap_total_bytes: u64,
}

impl From<TierSwapStats> for TierSwapStatsRecord {
    fn from(s: TierSwapStats) -> Self {
        Self {
            vram_peak_bytes: s.vram_peak_bytes,
            host_ram_peak_bytes: s.host_ram_peak_bytes,
            vram_swap_in_count: s.vram_swap_in_count,
            host_ram_swap_in_count: s.host_ram_swap_in_count,
            vram_swap_out_count: s.vram_swap_out_count,
            host_ram_swap_out_count: s.host_ram_swap_out_count,
            disk_eviction_count: s.disk_eviction_count,
            swap_total_bytes: s.swap_total_bytes,
        }
    }
}

impl TierSwapStatsRecord {
    /// Total swap-in events across both hot tiers.
    #[must_use]
    pub fn total_swap_in_count(&self) -> u64 {
        self.vram_swap_in_count
            .saturating_add(self.host_ram_swap_in_count)
    }

    /// Total swap-out events across both hot tiers.
    #[must_use]
    pub fn total_swap_out_count(&self) -> u64 {
        self.vram_swap_out_count
            .saturating_add(self.host_ram_swap_out_count)
    }
}

/// Diagnostic outcome bucket for one tier-swap row. Mirrors
/// [`crate::atlas_doctor::AtlasDoctorStatus`] so the CLI translator
/// can fold both sets of rows into the same `DiagnosticCheck`
/// pipeline without a second enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TierSwapDoctorStatus {
    Ok,
    Warn,
    Fail,
}

/// Per-atlas tiered-swap snapshot. Carries the substrate counters
/// plus the optional VRAM + host RAM budgets the platform probes
/// will populate when they land.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TierSwapDoctorRow {
    /// Operator-supplied identifier (typically the same `label`
    /// the atlas-packing surface uses for the same atlas).
    pub label: String,
    pub stats: TierSwapStatsRecord,
    /// VRAM ceiling the platform probe reported, when available.
    /// `None` means no probe ran; the row's `pressure_pct` is
    /// computed against `stats.vram_peak_bytes` only when this is
    /// `Some`.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub vram_budget_bytes: Option<u64>,
    /// Host RAM ceiling — same semantics as `vram_budget_bytes`.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub host_ram_budget_bytes: Option<u64>,
    /// br-ft-cjtac: snapshot from the prior observation window.
    /// When present, [`Self::status`] evaluates the swap-out
    /// runaway-loop threshold against the delta
    /// (`current - prev`) instead of the cumulative lifetime
    /// count. When `None`, the threshold is treated as 0 delta
    /// (the first observation has no baseline yet — pressure_pct
    /// + disk_eviction_count still fire). Callers that poll the
    /// doctor on a fixed cadence pass the previous tick's snapshot
    /// to define the observation window.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub prev_snapshot: Option<TierSwapStatsRecord>,
}

impl TierSwapDoctorRow {
    /// Build a row from the substrate counter set + optional
    /// per-tier budgets.
    ///
    /// The `prev_snapshot` field defaults to `None`, meaning the
    /// first observation cannot yet evaluate the runaway-loop
    /// threshold (no baseline). Use [`Self::from_stats_with_prev`]
    /// when polling on a fixed cadence to feed the prior tick's
    /// snapshot in and let the doctor evaluate the threshold
    /// against the per-window delta (br-ft-cjtac).
    #[must_use]
    pub fn from_stats(
        label: impl Into<String>,
        stats: TierSwapStats,
        vram_budget_bytes: Option<u64>,
        host_ram_budget_bytes: Option<u64>,
    ) -> Self {
        Self {
            label: label.into(),
            stats: TierSwapStatsRecord::from(stats),
            vram_budget_bytes,
            host_ram_budget_bytes,
            prev_snapshot: None,
        }
    }

    /// br-ft-cjtac: build a row carrying the previous observation
    /// snapshot so [`Self::status`] can evaluate the swap-out
    /// runaway-loop threshold against the per-window delta rather
    /// than the cumulative lifetime count. The prior snapshot is
    /// typically the [`TierSwapStatsRecord`] from the same atlas's
    /// `from_stats` row at the previous doctor-poll tick.
    #[must_use]
    pub fn from_stats_with_prev(
        label: impl Into<String>,
        stats: TierSwapStats,
        vram_budget_bytes: Option<u64>,
        host_ram_budget_bytes: Option<u64>,
        prev_snapshot: TierSwapStatsRecord,
    ) -> Self {
        Self {
            label: label.into(),
            stats: TierSwapStatsRecord::from(stats),
            vram_budget_bytes,
            host_ram_budget_bytes,
            prev_snapshot: Some(prev_snapshot),
        }
    }

    /// Per-observation-window swap-out delta — the count of
    /// swap-out events between [`Self::prev_snapshot`] and the
    /// current `stats`. Returns 0 when no prior snapshot is
    /// attached (first observation; no baseline). Saturating
    /// subtraction so an out-of-order prior (a counter that
    /// decreased — should never happen for monotonic counters
    /// but defensive) yields 0 rather than underflowing.
    #[must_use]
    pub fn swap_out_delta(&self) -> u64 {
        match self.prev_snapshot.as_ref() {
            Some(prev) => self
                .stats
                .total_swap_out_count()
                .saturating_sub(prev.total_swap_out_count()),
            None => 0,
        }
    }

    /// Pressure as integer percent against the configured budget.
    /// `None` when the budget is not set; the percent caps at 100
    /// when the peak exceeds the budget so the diagnostic line
    /// stays readable.
    #[must_use]
    pub fn vram_pressure_pct(&self) -> Option<u32> {
        let budget = self.vram_budget_bytes?;
        if budget == 0 {
            return Some(0);
        }
        let pct = self.stats.vram_peak_bytes.saturating_mul(100) / budget;
        Some(u32::try_from(pct.min(100)).unwrap_or(100))
    }

    /// Same as [`Self::vram_pressure_pct`] but for host RAM.
    #[must_use]
    pub fn host_ram_pressure_pct(&self) -> Option<u32> {
        let budget = self.host_ram_budget_bytes?;
        if budget == 0 {
            return Some(0);
        }
        let pct = self.stats.host_ram_peak_bytes.saturating_mul(100) / budget;
        Some(u32::try_from(pct.min(100)).unwrap_or(100))
    }

    /// Threshold-based status classifier. The buckets are
    /// intentionally conservative because tiered-swap pressure
    /// manifests as glitchy redraws, not crashes:
    ///
    /// - `Fail` — any disk evictions (the cache lost a region) OR
    ///   either pressure_pct > 95.
    /// - `Warn` — pressure_pct > 75 (either tier) OR more than 64
    ///   swap-out events in a single observation window (a runaway
    ///   demote loop is a leading indicator of insufficient
    ///   budget). The "single observation window" is the delta
    ///   between [`Self::prev_snapshot`] and the current `stats`
    ///   (br-ft-cjtac); when no prior snapshot is attached the
    ///   delta is 0 and this clause never fires.
    /// - `Ok`  — none of the above.
    ///
    /// br-ft-cjtac note: the swap-out threshold previously read
    /// the cumulative lifetime count via `total_swap_out_count()`,
    /// which permanently locked any long-running session that had
    /// ever seen a brief swap-out burst into Warn. The threshold
    /// now reads the per-window delta via [`Self::swap_out_delta`]
    /// so a quiesced session reports Ok again once the burst is
    /// behind it.
    #[must_use]
    pub fn status(&self) -> TierSwapDoctorStatus {
        const FAIL_PRESSURE_PCT: u32 = 95;
        const WARN_PRESSURE_PCT: u32 = 75;
        const WARN_SWAP_OUT_COUNT: u64 = 64;

        if self.stats.disk_eviction_count > 0 {
            return TierSwapDoctorStatus::Fail;
        }
        let vram_pressure = self.vram_pressure_pct().unwrap_or(0);
        let host_pressure = self.host_ram_pressure_pct().unwrap_or(0);
        if vram_pressure > FAIL_PRESSURE_PCT || host_pressure > FAIL_PRESSURE_PCT {
            return TierSwapDoctorStatus::Fail;
        }
        if vram_pressure > WARN_PRESSURE_PCT
            || host_pressure > WARN_PRESSURE_PCT
            || self.swap_out_delta() > WARN_SWAP_OUT_COUNT
        {
            return TierSwapDoctorStatus::Warn;
        }
        TierSwapDoctorStatus::Ok
    }
}

/// Aggregate view across rows. Mirrors
/// [`crate::atlas_doctor::AtlasDoctorAggregate`] so dashboards can
/// render both surfaces uniformly.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TierSwapDoctorAggregate {
    pub atlas_count: u64,
    pub total_vram_peak_bytes: u64,
    pub total_host_ram_peak_bytes: u64,
    pub total_swap_in_count: u64,
    pub total_swap_out_count: u64,
    pub total_disk_eviction_count: u64,
    pub total_swap_bytes: u64,
}

impl TierSwapDoctorAggregate {
    fn from_rows(rows: &[TierSwapDoctorRow]) -> Self {
        let mut agg = Self {
            atlas_count: rows.len() as u64,
            total_vram_peak_bytes: 0,
            total_host_ram_peak_bytes: 0,
            total_swap_in_count: 0,
            total_swap_out_count: 0,
            total_disk_eviction_count: 0,
            total_swap_bytes: 0,
        };
        for row in rows {
            agg.total_vram_peak_bytes = agg
                .total_vram_peak_bytes
                .saturating_add(row.stats.vram_peak_bytes);
            agg.total_host_ram_peak_bytes = agg
                .total_host_ram_peak_bytes
                .saturating_add(row.stats.host_ram_peak_bytes);
            agg.total_swap_in_count = agg
                .total_swap_in_count
                .saturating_add(row.stats.total_swap_in_count());
            agg.total_swap_out_count = agg
                .total_swap_out_count
                .saturating_add(row.stats.total_swap_out_count());
            agg.total_disk_eviction_count = agg
                .total_disk_eviction_count
                .saturating_add(row.stats.disk_eviction_count);
            agg.total_swap_bytes = agg
                .total_swap_bytes
                .saturating_add(row.stats.swap_total_bytes);
        }
        agg
    }
}

/// Full tier-swap section the CLI doctor consumes.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TierSwapDoctorReport {
    pub atlases: Vec<TierSwapDoctorRow>,
    pub aggregate: TierSwapDoctorAggregate,
}

impl TierSwapDoctorReport {
    /// Build from a row vector. Aggregate is recomputed so the two
    /// sides cannot drift.
    #[must_use]
    pub fn from_rows(rows: Vec<TierSwapDoctorRow>) -> Self {
        let aggregate = TierSwapDoctorAggregate::from_rows(&rows);
        Self {
            atlases: rows,
            aggregate,
        }
    }

    /// CLI-mode sentinel matching
    /// [`crate::atlas_doctor::AtlasDoctorReport::no_atlases_in_process`].
    #[must_use]
    pub fn no_atlases_in_process() -> Self {
        Self::from_rows(Vec::new())
    }

    /// Pre-formatted (display_name, detail, status) tuples for the
    /// CLI translator. One per row plus a final aggregate row;
    /// when the report is empty, a single informational line.
    #[must_use]
    pub fn diagnostic_lines(&self) -> Vec<(String, String, TierSwapDoctorStatus)> {
        let mut lines = Vec::with_capacity(self.atlases.len() + 1);
        if self.atlases.is_empty() {
            lines.push((
                "Atlas tiered-swap".to_string(),
                "no in-process atlases observed (run from the GUI to populate)".to_string(),
                TierSwapDoctorStatus::Ok,
            ));
            return lines;
        }
        for row in &self.atlases {
            let detail = format!(
                "VRAM peak {vram} B (pressure {vp}%), host RAM peak {host} B (pressure {hp}%); \
                 swap in/out {swin}/{swout}; disk evictions {evict}",
                vram = row.stats.vram_peak_bytes,
                host = row.stats.host_ram_peak_bytes,
                vp = row
                    .vram_pressure_pct()
                    .map(|p| p.to_string())
                    .unwrap_or_else(|| "n/a".to_string()),
                hp = row
                    .host_ram_pressure_pct()
                    .map(|p| p.to_string())
                    .unwrap_or_else(|| "n/a".to_string()),
                swin = row.stats.total_swap_in_count(),
                swout = row.stats.total_swap_out_count(),
                evict = row.stats.disk_eviction_count,
            );
            lines.push((
                format!("Atlas tiered-swap — {}", row.label),
                detail,
                row.status(),
            ));
        }
        let agg_status = self
            .atlases
            .iter()
            .map(TierSwapDoctorRow::status)
            .max_by_key(|s| match s {
                TierSwapDoctorStatus::Ok => 0,
                TierSwapDoctorStatus::Warn => 1,
                TierSwapDoctorStatus::Fail => 2,
            })
            .unwrap_or(TierSwapDoctorStatus::Ok);
        lines.push((
            "Atlas tiered-swap — aggregate".to_string(),
            format!(
                "{n} atlas(es); swap in/out {swin}/{swout}; disk evictions {evict}; \
                 total swap bytes {bytes}",
                n = self.aggregate.atlas_count,
                swin = self.aggregate.total_swap_in_count,
                swout = self.aggregate.total_swap_out_count,
                evict = self.aggregate.total_disk_eviction_count,
                bytes = self.aggregate.total_swap_bytes,
            ),
            agg_status,
        ));
        lines
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::atlas_tiered_swap::{AtlasTier, EvictionAction};

    fn synthesize_stats(swap_in: u64, swap_out: u64, evictions: u64) -> TierSwapStats {
        let mut s = TierSwapStats::default();
        for _ in 0..swap_in {
            s.record_action(
                EvictionAction::Promote {
                    region_id: 0,
                    from: AtlasTier::HostRam,
                    to: AtlasTier::Vram,
                },
                1024,
            );
        }
        for _ in 0..swap_out {
            s.record_action(
                EvictionAction::Demote {
                    region_id: 0,
                    from: AtlasTier::Vram,
                    to: AtlasTier::HostRam,
                },
                1024,
            );
        }
        for _ in 0..evictions {
            s.record_action(
                EvictionAction::Evict {
                    region_id: 0,
                    from: AtlasTier::Disk,
                },
                1024,
            );
        }
        s
    }

    #[test]
    fn record_from_stats_carries_every_field() {
        let mut stats = synthesize_stats(2, 3, 1);
        stats.record_peak(4096, 8192);
        let rec = TierSwapStatsRecord::from(stats);
        assert_eq!(rec.vram_swap_in_count, 2);
        assert_eq!(rec.vram_swap_out_count, 3);
        assert_eq!(rec.disk_eviction_count, 1);
        assert_eq!(rec.vram_peak_bytes, 4096);
        assert_eq!(rec.host_ram_peak_bytes, 8192);
        // 2 promotions + 3 demotions × 1024 each
        assert_eq!(rec.swap_total_bytes, 5 * 1024);
    }

    #[test]
    fn record_total_swap_in_out_sums_both_tiers() {
        let mut stats = TierSwapStats::default();
        stats.record_action(
            EvictionAction::Promote {
                region_id: 0,
                from: AtlasTier::HostRam,
                to: AtlasTier::Vram,
            },
            1,
        );
        stats.record_action(
            EvictionAction::Promote {
                region_id: 0,
                from: AtlasTier::Disk,
                to: AtlasTier::HostRam,
            },
            1,
        );
        let rec: TierSwapStatsRecord = stats.into();
        assert_eq!(rec.total_swap_in_count(), 2);
        assert_eq!(rec.total_swap_out_count(), 0);
    }

    #[test]
    fn row_pressure_pct_is_none_when_budget_unset() {
        let row = TierSwapDoctorRow::from_stats("a", synthesize_stats(0, 0, 0), None, None);
        assert_eq!(row.vram_pressure_pct(), None);
        assert_eq!(row.host_ram_pressure_pct(), None);
    }

    #[test]
    fn row_pressure_pct_caps_at_100_on_overshoot() {
        let mut stats = synthesize_stats(0, 0, 0);
        stats.record_peak(2_000, 0);
        let row = TierSwapDoctorRow::from_stats("a", stats, Some(1_000), None);
        assert_eq!(row.vram_pressure_pct(), Some(100));
    }

    #[test]
    fn row_pressure_pct_zero_budget_returns_zero() {
        let stats = synthesize_stats(0, 0, 0);
        let row = TierSwapDoctorRow::from_stats("a", stats, Some(0), None);
        assert_eq!(row.vram_pressure_pct(), Some(0));
    }

    #[test]
    fn row_status_fail_on_any_disk_eviction() {
        let stats = synthesize_stats(0, 0, 1);
        let row = TierSwapDoctorRow::from_stats("a", stats, None, None);
        assert_eq!(row.status(), TierSwapDoctorStatus::Fail);
    }

    #[test]
    fn row_status_fail_when_vram_pressure_above_95() {
        let mut stats = synthesize_stats(0, 0, 0);
        stats.record_peak(96, 0);
        let row = TierSwapDoctorRow::from_stats("a", stats, Some(100), None);
        assert_eq!(row.status(), TierSwapDoctorStatus::Fail);
    }

    #[test]
    fn row_status_warn_when_pressure_above_75_and_no_eviction() {
        let mut stats = synthesize_stats(0, 0, 0);
        stats.record_peak(80, 0);
        let row = TierSwapDoctorRow::from_stats("a", stats, Some(100), None);
        assert_eq!(row.status(), TierSwapDoctorStatus::Warn);
    }

    #[test]
    fn row_status_warn_on_runaway_swap_out_in_window() {
        // br-ft-cjtac: 100 swap-outs IN THE CURRENT WINDOW
        // (delta from a quiet prior snapshot) must Warn.
        let prev: TierSwapStatsRecord = synthesize_stats(0, 0, 0).into();
        let stats = synthesize_stats(0, 100, 0);
        let row = TierSwapDoctorRow::from_stats_with_prev("a", stats, None, None, prev);
        assert_eq!(row.swap_out_delta(), 100);
        assert_eq!(row.status(), TierSwapDoctorStatus::Warn);
    }

    #[test]
    fn row_status_ok_when_long_session_quiesces_after_burst() {
        // br-ft-cjtac acceptance: a session that crossed the
        // cumulative threshold long ago but has quiesced (no new
        // swap-outs since the prior tick) must report Ok, not the
        // permanent-Warn the cumulative-count threshold produced.
        let prev: TierSwapStatsRecord = synthesize_stats(0, 1_000, 0).into();
        let stats = synthesize_stats(0, 1_000, 0);
        let row = TierSwapDoctorRow::from_stats_with_prev("a", stats, None, None, prev);
        assert_eq!(row.swap_out_delta(), 0);
        assert_eq!(row.status(), TierSwapDoctorStatus::Ok);
    }

    #[test]
    fn row_status_ok_when_no_prior_snapshot_and_only_cumulative_count() {
        // br-ft-cjtac: the cumulative-only path (no prior snapshot
        // attached) MUST NOT trip the swap-out threshold — that
        // was the original defect. Even with a huge cumulative
        // count, the first observation has no baseline so delta=0.
        let stats = synthesize_stats(0, 10_000, 0);
        let row = TierSwapDoctorRow::from_stats("a", stats, None, None);
        assert_eq!(row.swap_out_delta(), 0);
        assert_eq!(row.status(), TierSwapDoctorStatus::Ok);
    }

    #[test]
    fn row_swap_out_delta_saturates_on_decreasing_counter() {
        // Defensive: monotonic counters should never decrease, but
        // if they do (e.g. a stats reset between snapshots), the
        // delta must saturate at 0 rather than wrap.
        let prev: TierSwapStatsRecord = synthesize_stats(0, 100, 0).into();
        let stats = synthesize_stats(0, 50, 0);
        let row = TierSwapDoctorRow::from_stats_with_prev("a", stats, None, None, prev);
        assert_eq!(row.swap_out_delta(), 0);
    }

    #[test]
    fn row_status_ok_when_quiet() {
        let stats = synthesize_stats(2, 1, 0);
        let row = TierSwapDoctorRow::from_stats("a", stats, None, None);
        assert_eq!(row.status(), TierSwapDoctorStatus::Ok);
    }

    #[test]
    fn aggregate_sums_every_counter() {
        let r1 = TierSwapDoctorRow::from_stats("a", synthesize_stats(2, 1, 0), None, None);
        let r2 = TierSwapDoctorRow::from_stats("b", synthesize_stats(3, 2, 1), None, None);
        let agg = TierSwapDoctorAggregate::from_rows(&[r1, r2]);
        assert_eq!(agg.atlas_count, 2);
        assert_eq!(agg.total_swap_in_count, 5);
        assert_eq!(agg.total_swap_out_count, 3);
        assert_eq!(agg.total_disk_eviction_count, 1);
    }

    #[test]
    fn aggregate_empty_rows_is_zeros() {
        let agg = TierSwapDoctorAggregate::from_rows(&[]);
        assert_eq!(agg.atlas_count, 0);
        assert_eq!(agg.total_swap_bytes, 0);
    }

    #[test]
    fn report_no_atlases_in_process_emits_one_informational_line() {
        let lines = TierSwapDoctorReport::no_atlases_in_process().diagnostic_lines();
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].2, TierSwapDoctorStatus::Ok);
        assert!(lines[0].1.contains("no in-process atlases"));
    }

    #[test]
    fn report_diagnostic_lines_emit_one_per_row_plus_aggregate() {
        let r = TierSwapDoctorRow::from_stats(
            "main",
            synthesize_stats(1, 0, 0),
            Some(4096),
            Some(8192),
        );
        let report = TierSwapDoctorReport::from_rows(vec![r]);
        let lines = report.diagnostic_lines();
        assert_eq!(lines.len(), 2);
        assert!(lines[0].0.contains("main"));
        assert_eq!(lines[1].0, "Atlas tiered-swap — aggregate");
    }

    #[test]
    fn report_aggregate_status_takes_worst_row() {
        let healthy = TierSwapDoctorRow::from_stats("good", synthesize_stats(1, 0, 0), None, None);
        let degraded = TierSwapDoctorRow::from_stats("bad", synthesize_stats(0, 0, 5), None, None);
        let report = TierSwapDoctorReport::from_rows(vec![healthy, degraded]);
        let (_, _, agg_status) = report.diagnostic_lines().pop().unwrap();
        assert_eq!(agg_status, TierSwapDoctorStatus::Fail);
    }

    #[test]
    fn report_serde_roundtrips_through_json() {
        let row = TierSwapDoctorRow::from_stats("rt", synthesize_stats(2, 1, 0), Some(1024), None);
        let report = TierSwapDoctorReport::from_rows(vec![row]);
        let s = serde_json::to_string(&report).unwrap();
        let parsed: TierSwapDoctorReport = serde_json::from_str(&s).unwrap();
        assert_eq!(parsed, report);
    }

    #[test]
    fn report_serde_omits_unset_budgets() {
        let row = TierSwapDoctorRow::from_stats("a", synthesize_stats(0, 0, 0), None, None);
        let v: serde_json::Value =
            serde_json::to_value(TierSwapDoctorReport::from_rows(vec![row])).unwrap();
        let atlas0 = &v["atlases"][0];
        assert!(atlas0.get("vram_budget_bytes").is_none());
        assert!(atlas0.get("host_ram_budget_bytes").is_none());
    }
}
