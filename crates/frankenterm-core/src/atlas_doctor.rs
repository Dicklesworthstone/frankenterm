//! `ft doctor` surface for atlas packing efficiency (ft-gtcm9.3).
//!
//! Defines the per-atlas + aggregate report shape the CLI's `ft
//! doctor` command consumes. The substrate's packers and their
//! [`crate::atlas_bin_packing::PackingStats`] expose every primitive
//! the report needs (`efficiency_pct`, `wasted_pct`,
//! `MaximalRectanglesPacker::free_rect_count`); this module wires
//! those primitives into a serde-serializable report and a
//! diagnostic-check translation that `main.rs` can render.
//!
//! ## Why a separate module
//!
//! Doctor surfaces want stable, serde-shaped types so the JSON
//! output mode (`ft doctor --json`) is regression-locked. The
//! substrate's stats type is intentionally serde-free so the
//! pure-logic packing layer never picks up the dependency. This
//! module bridges the two: it borrows from the stats and emits
//! types that downstream tooling (CI dashboards, attestation
//! pipelines, the JSONL logs from
//! [`crate::atlas_packing_telemetry`]) can join against.
//!
//! ## CLI mode vs in-process mode
//!
//! `ft doctor` runs as a standalone CLI invocation; the live
//! atlases are owned by the GUI process and are not visible here.
//! In CLI mode the report has zero rows, and
//! [`AtlasDoctorReport::no_atlases_in_process`] returns the
//! "no atlases" sentinel that the CLI surface translates into a
//! single informational diagnostic check. The same module produces
//! a populated report when called from inside the GUI process (or
//! from a test that owns its own atlases).

use serde::{Deserialize, Serialize};

use crate::atlas_bin_packing::{Atlas2DSize, PackerKind, PackingStats};
use crate::atlas_packing_telemetry::packer_label;

/// Operator-friendly per-atlas snapshot. Every field is computed
/// from the corresponding `PackingStats` + the atlas's
/// `(width, height)` + the packer's `kind()` — the GUI never needs
/// to thread anything else through this surface.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AtlasDoctorRow {
    /// Operator-supplied identifier (e.g., "main", "alt-screen",
    /// "glyph-cache-#3"). The substrate does not interpret it; it
    /// is carried verbatim into the diagnostic line so log readers
    /// can correlate against application-specific atlas pools.
    pub label: String,
    /// Stable packer label from
    /// [`crate::atlas_packing_telemetry::packer_label`].
    pub packer_in_use: String,
    pub atlas_width: u32,
    pub atlas_height: u32,
    pub atlas_bytes: u64,
    pub used_bytes: u64,
    pub wasted_bytes: u64,
    pub packing_efficiency_pct: u32,
    pub fragmentation_pct: u32,
    pub allocs_total: u64,
    pub rejects_total: u64,
    /// Only present for the maximal-rectangles packer. Captures the
    /// number of maximal free rectangles still tracked by the BSSF
    /// algorithm; a steadily growing count is a leading indicator
    /// of fragmentation pressure on long-lived atlases.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub free_rect_count: Option<u64>,
}

impl AtlasDoctorRow {
    /// Build a row from raw stats. Computes wasted_bytes /
    /// fragmentation_pct from the same primitives PackingStats
    /// already exposes so the report shape stays consistent with
    /// the substrate's notion of efficiency.
    #[must_use]
    pub fn from_stats(
        label: impl Into<String>,
        kind: PackerKind,
        size: Atlas2DSize,
        stats: &PackingStats,
        free_rect_count: Option<u64>,
    ) -> Self {
        let atlas_bytes = size.area();
        let used_bytes = stats.used_bytes;
        let wasted_bytes = atlas_bytes.saturating_sub(used_bytes);
        let packing_efficiency_pct = stats.efficiency_pct();
        let fragmentation_pct = stats.wasted_pct();
        Self {
            label: label.into(),
            packer_in_use: packer_label(kind).to_string(),
            atlas_width: size.width,
            atlas_height: size.height,
            atlas_bytes,
            used_bytes,
            wasted_bytes,
            packing_efficiency_pct,
            fragmentation_pct,
            allocs_total: stats.alloc_total,
            rejects_total: stats.reject_total,
            free_rect_count,
        }
    }

    /// Threshold-based status classification used by the CLI to map
    /// the row onto the [`DiagnosticStatus`] enum. Pure on the
    /// row's existing fields so there is no second source of truth.
    #[must_use]
    pub fn status(&self) -> AtlasDoctorStatus {
        // The thresholds mirror the substrate's headline claims:
        //   - shelf  ≈ 90% efficient (≈10% wasted)
        //   - skyline ≈ 95% efficient (≈5% wasted)
        //   - maximal_rectangles ≈ 98% efficient (≈2% wasted)
        // We pick the loosest gate (90%) for "ok", warn at 70%,
        // and fail at 50% — operators get a meaningful signal
        // before the atlas saturates.
        if self.packing_efficiency_pct >= 90 {
            AtlasDoctorStatus::Ok
        } else if self.packing_efficiency_pct >= 70 {
            AtlasDoctorStatus::Warn
        } else {
            AtlasDoctorStatus::Fail
        }
    }
}

/// Diagnostic outcome bucket for one atlas. Mirrors the wire-level
/// `DiagnosticStatus` shape so callers do not have to introduce a
/// frankenterm-core → frankenterm dependency edge.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AtlasDoctorStatus {
    Ok,
    Warn,
    Fail,
}

/// Aggregate view computed from a non-empty row set. Mirrors the
/// bead's "aggregate row shows total wasted bytes + mean efficiency"
/// requirement, plus a row count so log consumers can detect the
/// "no atlases observed" case without joining against the rows
/// vector.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AtlasDoctorAggregate {
    pub atlas_count: u64,
    pub total_atlas_bytes: u64,
    pub total_used_bytes: u64,
    pub total_wasted_bytes: u64,
    /// Arithmetic mean of `packing_efficiency_pct` across rows,
    /// rounded down. `0` when `atlas_count == 0`.
    pub mean_efficiency_pct: u32,
}

impl AtlasDoctorAggregate {
    fn from_rows(rows: &[AtlasDoctorRow]) -> Self {
        if rows.is_empty() {
            return Self {
                atlas_count: 0,
                total_atlas_bytes: 0,
                total_used_bytes: 0,
                total_wasted_bytes: 0,
                mean_efficiency_pct: 0,
            };
        }
        let mut total_atlas = 0u64;
        let mut total_used = 0u64;
        let mut total_wasted = 0u64;
        let mut eff_sum = 0u64;
        for row in rows {
            total_atlas = total_atlas.saturating_add(row.atlas_bytes);
            total_used = total_used.saturating_add(row.used_bytes);
            total_wasted = total_wasted.saturating_add(row.wasted_bytes);
            eff_sum = eff_sum.saturating_add(u64::from(row.packing_efficiency_pct));
        }
        let count = rows.len() as u64;
        Self {
            atlas_count: count,
            total_atlas_bytes: total_atlas,
            total_used_bytes: total_used,
            total_wasted_bytes: total_wasted,
            mean_efficiency_pct: u32::try_from(eff_sum / count).unwrap_or(u32::MAX),
        }
    }
}

/// The full `ft doctor --atlas` (or top-level `ft doctor`) atlas
/// section. Emitted as JSON under `result["atlas_packing"]` in the
/// CLI's `--json` mode and rendered as one diagnostic line per
/// atlas + one aggregate line in plain-text mode.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AtlasDoctorReport {
    pub atlases: Vec<AtlasDoctorRow>,
    pub aggregate: AtlasDoctorAggregate,
}

impl AtlasDoctorReport {
    /// Build a report from the supplied per-atlas rows. The
    /// aggregate is recomputed from the rows so the two sides stay
    /// consistent under all addition / removal scenarios.
    #[must_use]
    pub fn from_rows(rows: Vec<AtlasDoctorRow>) -> Self {
        let aggregate = AtlasDoctorAggregate::from_rows(&rows);
        Self {
            atlases: rows,
            aggregate,
        }
    }

    /// CLI-mode sentinel: zero rows, zero aggregate. The CLI
    /// surface translates this into a single informational
    /// diagnostic ("no in-process atlases — start the GUI to
    /// populate").
    #[must_use]
    pub fn no_atlases_in_process() -> Self {
        Self::from_rows(Vec::new())
    }

    /// Convenience for the CLI translator: pre-formatted summary
    /// strings, one per atlas plus an aggregate line. Each tuple is
    /// `(display_name, detail, status)` so the caller can wrap each
    /// in its own `DiagnosticCheck` variant. The aggregate line is
    /// appended last so the rendered output reads top-down.
    #[must_use]
    pub fn diagnostic_lines(&self) -> Vec<(String, String, AtlasDoctorStatus)> {
        let mut lines = Vec::with_capacity(self.atlases.len() + 1);
        if self.atlases.is_empty() {
            lines.push((
                "Atlas packing".to_string(),
                "no in-process atlases observed (run from the GUI to populate)".to_string(),
                AtlasDoctorStatus::Ok,
            ));
            return lines;
        }
        for row in &self.atlases {
            let detail = format!(
                "{packer} {w}x{h}: {eff}% efficient ({alloc} alloc, {reject} reject, {frag}% fragmentation)",
                packer = row.packer_in_use,
                w = row.atlas_width,
                h = row.atlas_height,
                eff = row.packing_efficiency_pct,
                alloc = row.allocs_total,
                reject = row.rejects_total,
                frag = row.fragmentation_pct,
            );
            lines.push((
                format!("Atlas packing — {}", row.label),
                detail,
                row.status(),
            ));
        }
        lines.push((
            "Atlas packing — aggregate".to_string(),
            format!(
                "{n} atlas(es); {wasted} byte(s) wasted; mean efficiency {mean}%",
                n = self.aggregate.atlas_count,
                wasted = self.aggregate.total_wasted_bytes,
                mean = self.aggregate.mean_efficiency_pct,
            ),
            // The aggregate line carries the worst per-atlas status
            // so the operator does not have to scan every row to
            // notice a degraded one.
            self.atlases
                .iter()
                .map(AtlasDoctorRow::status)
                .max_by_key(|s| match s {
                    AtlasDoctorStatus::Ok => 0,
                    AtlasDoctorStatus::Warn => 1,
                    AtlasDoctorStatus::Fail => 2,
                })
                .unwrap_or(AtlasDoctorStatus::Ok),
        ));
        lines
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::atlas_bin_packing::PackedRect;

    fn atlas(w: u32, h: u32) -> Atlas2DSize {
        Atlas2DSize::try_new(w, h).unwrap()
    }

    fn stats_with(used: u64, alloc: u64, reject: u64, atlas_size: Atlas2DSize) -> PackingStats {
        let mut s = PackingStats::default();
        s.record_atlas_size(atlas_size);
        // record_placed bumps alloc_total by 1 and adds rect.area to used_bytes.
        // Inject a single synthesized rect with the desired area, then top up
        // additional alloc_total to match the requested counter (used by tests
        // that want non-1 alloc_total without splitting the area).
        if used > 0 {
            s.record_placed(PackedRect {
                x: 0,
                y: 0,
                width: 1,
                height: u32::try_from(used).unwrap_or(u32::MAX),
            });
            for _ in 1..alloc {
                s.alloc_total = s.alloc_total.saturating_add(1);
            }
        } else {
            for _ in 0..alloc {
                s.alloc_total = s.alloc_total.saturating_add(1);
            }
        }
        for _ in 0..reject {
            s.record_reject();
        }
        s
    }

    #[test]
    fn row_from_stats_preserves_packer_label_and_dimensions() {
        let size = atlas(64, 64);
        let stats = stats_with(1024, 4, 1, size);
        let row = AtlasDoctorRow::from_stats("main", PackerKind::Skyline, size, &stats, None);
        assert_eq!(row.label, "main");
        assert_eq!(row.packer_in_use, "Skyline");
        assert_eq!(row.atlas_width, 64);
        assert_eq!(row.atlas_height, 64);
        assert_eq!(row.atlas_bytes, 4096);
        assert_eq!(row.used_bytes, 1024);
        assert_eq!(row.wasted_bytes, 4096 - 1024);
        assert_eq!(row.allocs_total, 4);
        assert_eq!(row.rejects_total, 1);
        assert_eq!(row.free_rect_count, None);
    }

    #[test]
    fn row_from_stats_threads_free_rect_count_for_maximal_rectangles() {
        let size = atlas(4096, 4096);
        let stats = stats_with(0, 0, 0, size);
        let row = AtlasDoctorRow::from_stats(
            "mr-cache",
            PackerKind::MaximalRectangles,
            size,
            &stats,
            Some(7),
        );
        assert_eq!(row.packer_in_use, "MaximalRectangles");
        assert_eq!(row.free_rect_count, Some(7));
    }

    #[test]
    fn row_status_buckets_efficiency() {
        let size = atlas(100, 100);
        // Build three rows with different efficiencies via raw construction.
        let make = |pct: u32| AtlasDoctorRow {
            label: "x".into(),
            packer_in_use: "Shelf".into(),
            atlas_width: size.width,
            atlas_height: size.height,
            atlas_bytes: size.area(),
            used_bytes: u64::from(pct) * size.area() / 100,
            wasted_bytes: size.area() - u64::from(pct) * size.area() / 100,
            packing_efficiency_pct: pct,
            fragmentation_pct: 100 - pct,
            allocs_total: 0,
            rejects_total: 0,
            free_rect_count: None,
        };
        assert_eq!(make(95).status(), AtlasDoctorStatus::Ok);
        assert_eq!(make(90).status(), AtlasDoctorStatus::Ok);
        assert_eq!(make(80).status(), AtlasDoctorStatus::Warn);
        assert_eq!(make(70).status(), AtlasDoctorStatus::Warn);
        assert_eq!(make(50).status(), AtlasDoctorStatus::Fail);
        assert_eq!(make(0).status(), AtlasDoctorStatus::Fail);
    }

    #[test]
    fn aggregate_from_empty_rows_is_zeros() {
        let agg = AtlasDoctorAggregate::from_rows(&[]);
        assert_eq!(agg.atlas_count, 0);
        assert_eq!(agg.total_atlas_bytes, 0);
        assert_eq!(agg.mean_efficiency_pct, 0);
    }

    #[test]
    fn aggregate_sums_bytes_and_means_efficiency() {
        let size = atlas(100, 100);
        let r1 = AtlasDoctorRow::from_stats(
            "a",
            PackerKind::Shelf,
            size,
            &stats_with(8000, 1, 0, size),
            None,
        );
        let r2 = AtlasDoctorRow::from_stats(
            "b",
            PackerKind::Skyline,
            size,
            &stats_with(6000, 1, 0, size),
            None,
        );
        let agg = AtlasDoctorAggregate::from_rows(&[r1.clone(), r2.clone()]);
        assert_eq!(agg.atlas_count, 2);
        assert_eq!(agg.total_atlas_bytes, 20_000);
        assert_eq!(agg.total_used_bytes, 14_000);
        assert_eq!(agg.total_wasted_bytes, 6_000);
        // Mean of 80 and 60 is 70.
        assert_eq!(agg.mean_efficiency_pct, 70);
    }

    #[test]
    fn report_no_atlases_in_process_carries_zero_aggregate() {
        let report = AtlasDoctorReport::no_atlases_in_process();
        assert!(report.atlases.is_empty());
        assert_eq!(report.aggregate.atlas_count, 0);
        assert_eq!(report.aggregate.total_atlas_bytes, 0);
    }

    #[test]
    fn report_diagnostic_lines_emit_one_per_atlas_plus_aggregate() {
        let size = atlas(64, 64);
        let row = AtlasDoctorRow::from_stats(
            "main",
            PackerKind::Skyline,
            size,
            &stats_with(2048, 16, 1, size),
            None,
        );
        let report = AtlasDoctorReport::from_rows(vec![row]);
        let lines = report.diagnostic_lines();
        assert_eq!(lines.len(), 2, "one per atlas + aggregate");
        let (name0, detail0, _) = &lines[0];
        assert!(name0.contains("main"));
        assert!(detail0.contains("Skyline"));
        assert!(detail0.contains("64x64"));
        let (name1, detail1, _) = &lines[1];
        assert_eq!(name1, "Atlas packing — aggregate");
        assert!(detail1.contains("1 atlas"));
    }

    #[test]
    fn report_diagnostic_lines_for_empty_report_emit_one_informational_line() {
        let lines = AtlasDoctorReport::no_atlases_in_process().diagnostic_lines();
        assert_eq!(lines.len(), 1);
        let (name, detail, status) = &lines[0];
        assert_eq!(name, "Atlas packing");
        assert!(detail.contains("no in-process atlases"));
        assert_eq!(*status, AtlasDoctorStatus::Ok);
    }

    #[test]
    fn report_aggregate_status_matches_worst_row() {
        let size = atlas(100, 100);
        let healthy = AtlasDoctorRow::from_stats(
            "good",
            PackerKind::Shelf,
            size,
            &stats_with(9500, 1, 0, size),
            None,
        );
        let degraded = AtlasDoctorRow::from_stats(
            "bad",
            PackerKind::Shelf,
            size,
            &stats_with(4000, 1, 0, size),
            None,
        );
        let report = AtlasDoctorReport::from_rows(vec![healthy, degraded]);
        let lines = report.diagnostic_lines();
        let (_, _, agg_status) = lines.last().unwrap();
        assert_eq!(*agg_status, AtlasDoctorStatus::Fail);
    }

    #[test]
    fn report_serde_roundtrips_through_json() {
        let size = atlas(100, 100);
        let row = AtlasDoctorRow::from_stats(
            "rt",
            PackerKind::MaximalRectangles,
            size,
            &stats_with(7000, 12, 3, size),
            Some(4),
        );
        let report = AtlasDoctorReport::from_rows(vec![row]);
        let s = serde_json::to_string(&report).unwrap();
        let parsed: AtlasDoctorReport = serde_json::from_str(&s).unwrap();
        assert_eq!(parsed, report);
    }

    #[test]
    fn report_serde_omits_free_rect_count_for_non_mr_packers() {
        let size = atlas(100, 100);
        let row = AtlasDoctorRow::from_stats(
            "shelf",
            PackerKind::Shelf,
            size,
            &stats_with(5000, 1, 0, size),
            None,
        );
        let report = AtlasDoctorReport::from_rows(vec![row]);
        let v: serde_json::Value = serde_json::to_value(&report).unwrap();
        let atlas0 = &v["atlases"][0];
        assert!(
            atlas0.get("free_rect_count").is_none(),
            "free_rect_count must be omitted when None: {atlas0}"
        );
    }
}
