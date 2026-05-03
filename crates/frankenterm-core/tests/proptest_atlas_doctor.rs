use proptest::prelude::*;

use frankenterm_core::atlas_bin_packing::{Atlas2DSize, PackerKind, PackingStats};
use frankenterm_core::atlas_doctor::{AtlasDoctorReport, AtlasDoctorRow, AtlasDoctorStatus};
use frankenterm_core::atlas_packing_telemetry::packer_label;

fn packer_kind_strategy() -> impl Strategy<Value = PackerKind> {
    prop::sample::select(vec![
        PackerKind::Shelf,
        PackerKind::Skyline,
        PackerKind::MaximalRectangles,
    ])
}

fn label_strategy() -> impl Strategy<Value = String> {
    "[A-Za-z0-9_-]{1,24}"
}

fn stats_for(
    size: Atlas2DSize,
    used_bytes: u64,
    alloc_total: u64,
    reject_total: u64,
) -> PackingStats {
    PackingStats {
        alloc_total,
        reject_total,
        used_bytes,
        atlas_bytes: size.area(),
    }
}

fn row_with_efficiency(label: String, efficiency_pct: u32) -> AtlasDoctorRow {
    let atlas_bytes = 10_000;
    let used_bytes = u64::from(efficiency_pct) * atlas_bytes / 100;
    AtlasDoctorRow {
        label,
        packer_in_use: "Shelf".to_string(),
        atlas_width: 100,
        atlas_height: 100,
        atlas_bytes,
        used_bytes,
        wasted_bytes: atlas_bytes.saturating_sub(used_bytes),
        packing_efficiency_pct: efficiency_pct,
        fragmentation_pct: 100_u32.saturating_sub(efficiency_pct),
        allocs_total: 1,
        rejects_total: 0,
        free_rect_count: None,
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    #[test]
    fn proptest_atlas_doctor_row_from_stats_preserves_accounting_and_labels(
        label in label_strategy(),
        kind in packer_kind_strategy(),
        width in 1_u32..=4096,
        height in 1_u32..=4096,
        used_bytes in 0_u64..=20_000_000,
        alloc_total in 0_u64..=1_000,
        reject_total in 0_u64..=1_000,
        free_rect_count in prop::option::of(0_u64..=10_000),
    ) {
        let size = Atlas2DSize::try_new(width, height).expect("non-zero atlas");
        let stats = stats_for(size, used_bytes, alloc_total, reject_total);
        let row = AtlasDoctorRow::from_stats(
            label.clone(),
            kind,
            size,
            &stats,
            free_rect_count,
        );

        prop_assert_eq!(&row.label, &label);
        prop_assert_eq!(row.packer_in_use.as_str(), packer_label(kind));
        prop_assert_eq!(row.atlas_width, width);
        prop_assert_eq!(row.atlas_height, height);
        prop_assert_eq!(row.atlas_bytes, size.area());
        prop_assert_eq!(row.used_bytes, used_bytes);
        prop_assert_eq!(row.wasted_bytes, size.area().saturating_sub(used_bytes));
        prop_assert_eq!(row.packing_efficiency_pct, stats.efficiency_pct());
        prop_assert_eq!(row.fragmentation_pct, stats.wasted_pct());
        prop_assert_eq!(row.allocs_total, alloc_total);
        prop_assert_eq!(row.rejects_total, reject_total);
        prop_assert_eq!(row.free_rect_count, free_rect_count);
    }

    #[test]
    fn proptest_atlas_doctor_row_status_thresholds_are_exact(
        label in label_strategy(),
        efficiency_pct in 0_u32..=100,
    ) {
        let row = row_with_efficiency(label, efficiency_pct);
        let expected = if efficiency_pct >= 90 {
            AtlasDoctorStatus::Ok
        } else if efficiency_pct >= 70 {
            AtlasDoctorStatus::Warn
        } else {
            AtlasDoctorStatus::Fail
        };

        prop_assert_eq!(row.status(), expected);
    }

    #[test]
    fn proptest_atlas_doctor_report_aggregate_sums_rows_and_floors_mean(
        efficiencies in prop::collection::vec(0_u32..=100, 1..=16),
    ) {
        let rows: Vec<_> = efficiencies
            .iter()
            .enumerate()
            .map(|(idx, pct)| row_with_efficiency(format!("atlas_{idx}"), *pct))
            .collect();
        let report = AtlasDoctorReport::from_rows(rows.clone());
        let expected_mean = efficiencies.iter().map(|pct| u64::from(*pct)).sum::<u64>()
            / efficiencies.len() as u64;

        prop_assert_eq!(&report.atlases, &rows);
        prop_assert_eq!(report.aggregate.atlas_count, efficiencies.len() as u64);
        prop_assert_eq!(report.aggregate.total_atlas_bytes, 10_000 * efficiencies.len() as u64);
        prop_assert_eq!(
            report.aggregate.total_used_bytes,
            rows.iter().map(|row| row.used_bytes).sum::<u64>(),
        );
        prop_assert_eq!(
            report.aggregate.total_wasted_bytes,
            rows.iter().map(|row| row.wasted_bytes).sum::<u64>(),
        );
        prop_assert_eq!(report.aggregate.mean_efficiency_pct, expected_mean as u32);
    }

    #[test]
    fn proptest_atlas_doctor_diagnostic_lines_keep_row_order_and_worst_status(
        efficiencies in prop::collection::vec(0_u32..=100, 1..=12),
    ) {
        let rows: Vec<_> = efficiencies
            .iter()
            .enumerate()
            .map(|(idx, pct)| row_with_efficiency(format!("atlas_{idx}"), *pct))
            .collect();
        let expected_worst = rows
            .iter()
            .map(AtlasDoctorRow::status)
            .max_by_key(|status| match status {
                AtlasDoctorStatus::Ok => 0,
                AtlasDoctorStatus::Warn => 1,
                AtlasDoctorStatus::Fail => 2,
            })
            .unwrap();
        let report = AtlasDoctorReport::from_rows(rows);
        let lines = report.diagnostic_lines();

        prop_assert_eq!(lines.len(), efficiencies.len() + 1);
        for idx in 0..efficiencies.len() {
            let expected_label = format!("atlas_{idx}");
            prop_assert!(lines[idx].0.contains(&expected_label));
            prop_assert!(lines[idx].1.contains("Shelf"));
            let expected_status = row_with_efficiency(expected_label, efficiencies[idx]).status();
            prop_assert_eq!(lines[idx].2, expected_status);
        }
        let aggregate = lines.last().expect("aggregate line");
        prop_assert_eq!(aggregate.0.as_str(), "Atlas packing — aggregate");
        prop_assert_eq!(aggregate.2, expected_worst);
    }

    #[test]
    fn proptest_atlas_doctor_report_json_roundtrips_and_omits_absent_free_rect_count(
        label in label_strategy(),
        kind in packer_kind_strategy(),
        width in 1_u32..=1024,
        height in 1_u32..=1024,
        used_bytes in 0_u64..=1_000_000,
        free_rect_count in prop::option::of(0_u64..=128),
    ) {
        let size = Atlas2DSize::try_new(width, height).expect("non-zero atlas");
        let stats = stats_for(size, used_bytes, 3, 1);
        let row = AtlasDoctorRow::from_stats(label, kind, size, &stats, free_rect_count);
        let report = AtlasDoctorReport::from_rows(vec![row.clone()]);

        let json = serde_json::to_string(&report).expect("serialize report");
        let value: serde_json::Value = serde_json::from_str(&json).expect("json value");
        let parsed: AtlasDoctorReport = serde_json::from_str(&json).expect("roundtrip report");

        prop_assert_eq!(&parsed, &report);
        let row_value = &value["atlases"][0];
        prop_assert_eq!(
            row_value.get("free_rect_count").is_some(),
            row.free_rect_count.is_some(),
        );
    }

    #[test]
    fn proptest_atlas_doctor_empty_report_emits_single_ok_sentinel(_dummy in 0_u8..=0) {
        let report = AtlasDoctorReport::no_atlases_in_process();
        let lines = report.diagnostic_lines();

        prop_assert!(report.atlases.is_empty());
        prop_assert_eq!(report.aggregate.atlas_count, 0);
        prop_assert_eq!(report.aggregate.total_atlas_bytes, 0);
        prop_assert_eq!(lines.len(), 1);
        prop_assert_eq!(lines[0].0.as_str(), "Atlas packing");
        prop_assert!(lines[0].1.contains("no in-process atlases"));
        prop_assert_eq!(lines[0].2, AtlasDoctorStatus::Ok);
    }
}
