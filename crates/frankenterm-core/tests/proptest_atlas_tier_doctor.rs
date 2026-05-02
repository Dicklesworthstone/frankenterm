use frankenterm_core::atlas_tier_doctor::{
    TierSwapDoctorReport, TierSwapDoctorRow, TierSwapDoctorStatus, TierSwapStatsRecord,
};
use frankenterm_core::atlas_tiered_swap::TierSwapStats;
use proptest::prelude::*;

fn label() -> impl Strategy<Value = String> {
    "[A-Za-z0-9 _.-]{0,48}".prop_map(String::from)
}

fn stats() -> impl Strategy<Value = TierSwapStats> {
    (
        any::<u64>(),
        any::<u64>(),
        any::<u64>(),
        any::<u64>(),
        any::<u64>(),
        any::<u64>(),
        any::<u64>(),
        any::<u64>(),
    )
        .prop_map(
            |(
                vram_peak_bytes,
                host_ram_peak_bytes,
                vram_swap_in_count,
                host_ram_swap_in_count,
                vram_swap_out_count,
                host_ram_swap_out_count,
                disk_eviction_count,
                swap_total_bytes,
            )| TierSwapStats {
                vram_peak_bytes,
                host_ram_peak_bytes,
                vram_swap_in_count,
                host_ram_swap_in_count,
                vram_swap_out_count,
                host_ram_swap_out_count,
                disk_eviction_count,
                swap_total_bytes,
            },
        )
}

fn row() -> impl Strategy<Value = TierSwapDoctorRow> {
    (
        label(),
        stats(),
        prop::option::of(any::<u64>()),
        prop::option::of(any::<u64>()),
    )
        .prop_map(|(label, stats, vram_budget, host_budget)| {
            TierSwapDoctorRow::from_stats(label, stats, vram_budget, host_budget)
        })
}

fn rows() -> impl Strategy<Value = Vec<TierSwapDoctorRow>> {
    prop::collection::vec(row(), 0..24)
}

fn expected_pct(peak: u64, budget: Option<u64>) -> Option<u32> {
    let budget = budget?;
    if budget == 0 {
        return Some(0);
    }
    let pct = peak.saturating_mul(100) / budget;
    Some(u32::try_from(pct.min(100)).unwrap_or(100))
}

fn severity(status: TierSwapDoctorStatus) -> u8 {
    match status {
        TierSwapDoctorStatus::Ok => 0,
        TierSwapDoctorStatus::Warn => 1,
        TierSwapDoctorStatus::Fail => 2,
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn proptest_atlas_tier_doctor_record_preserves_stats_and_sums(stats in stats()) {
        let record = TierSwapStatsRecord::from(stats);

        prop_assert_eq!(record.vram_peak_bytes, stats.vram_peak_bytes);
        prop_assert_eq!(record.host_ram_peak_bytes, stats.host_ram_peak_bytes);
        prop_assert_eq!(record.vram_swap_in_count, stats.vram_swap_in_count);
        prop_assert_eq!(record.host_ram_swap_in_count, stats.host_ram_swap_in_count);
        prop_assert_eq!(record.vram_swap_out_count, stats.vram_swap_out_count);
        prop_assert_eq!(record.host_ram_swap_out_count, stats.host_ram_swap_out_count);
        prop_assert_eq!(record.disk_eviction_count, stats.disk_eviction_count);
        prop_assert_eq!(record.swap_total_bytes, stats.swap_total_bytes);
        prop_assert_eq!(
            record.total_swap_in_count(),
            stats.vram_swap_in_count.saturating_add(stats.host_ram_swap_in_count)
        );
        prop_assert_eq!(
            record.total_swap_out_count(),
            stats.vram_swap_out_count.saturating_add(stats.host_ram_swap_out_count)
        );
    }

    #[test]
    fn proptest_atlas_tier_doctor_pressure_percent_matches_saturating_formula(
        stats in stats(),
        vram_budget in prop::option::of(any::<u64>()),
        host_budget in prop::option::of(any::<u64>()),
        label in label(),
    ) {
        let row = TierSwapDoctorRow::from_stats(label, stats, vram_budget, host_budget);

        prop_assert_eq!(
            row.vram_pressure_pct(),
            expected_pct(stats.vram_peak_bytes, vram_budget)
        );
        prop_assert_eq!(
            row.host_ram_pressure_pct(),
            expected_pct(stats.host_ram_peak_bytes, host_budget)
        );
    }

    #[test]
    fn proptest_atlas_tier_doctor_status_priority_matches_thresholds(
        mut stats in stats(),
        vram_pressure in 0_u64..=100,
        host_pressure in 0_u64..=100,
    ) {
        stats.vram_peak_bytes = vram_pressure;
        stats.host_ram_peak_bytes = host_pressure;
        let row = TierSwapDoctorRow::from_stats("atlas", stats, Some(100), Some(100));

        let expected = if stats.disk_eviction_count > 0
            || vram_pressure > 95
            || host_pressure > 95
        {
            TierSwapDoctorStatus::Fail
        } else if vram_pressure > 75
            || host_pressure > 75
            || stats
                .vram_swap_out_count
                .saturating_add(stats.host_ram_swap_out_count)
                > 64
        {
            TierSwapDoctorStatus::Warn
        } else {
            TierSwapDoctorStatus::Ok
        };

        prop_assert_eq!(row.status(), expected);
    }

    #[test]
    fn proptest_atlas_tier_doctor_report_aggregate_saturates_row_totals(rows in rows()) {
        let report = TierSwapDoctorReport::from_rows(rows.clone());

        prop_assert_eq!(report.aggregate.atlas_count, rows.len() as u64);
        prop_assert_eq!(
            report.aggregate.total_vram_peak_bytes,
            rows.iter().fold(0_u64, |acc, row| {
                acc.saturating_add(row.stats.vram_peak_bytes)
            })
        );
        prop_assert_eq!(
            report.aggregate.total_host_ram_peak_bytes,
            rows.iter().fold(0_u64, |acc, row| {
                acc.saturating_add(row.stats.host_ram_peak_bytes)
            })
        );
        prop_assert_eq!(
            report.aggregate.total_swap_in_count,
            rows.iter().fold(0_u64, |acc, row| {
                acc.saturating_add(row.stats.total_swap_in_count())
            })
        );
        prop_assert_eq!(
            report.aggregate.total_swap_out_count,
            rows.iter().fold(0_u64, |acc, row| {
                acc.saturating_add(row.stats.total_swap_out_count())
            })
        );
        prop_assert_eq!(
            report.aggregate.total_disk_eviction_count,
            rows.iter().fold(0_u64, |acc, row| {
                acc.saturating_add(row.stats.disk_eviction_count)
            })
        );
        prop_assert_eq!(
            report.aggregate.total_swap_bytes,
            rows.iter().fold(0_u64, |acc, row| {
                acc.saturating_add(row.stats.swap_total_bytes)
            })
        );
    }

    #[test]
    fn proptest_atlas_tier_doctor_diagnostic_lines_have_row_plus_worst_aggregate(
        rows in prop::collection::vec(row(), 1..16),
    ) {
        let report = TierSwapDoctorReport::from_rows(rows.clone());
        let lines = report.diagnostic_lines();

        prop_assert_eq!(lines.len(), rows.len() + 1);
        for (line, row) in lines.iter().take(rows.len()).zip(rows.iter()) {
            prop_assert!(line.0.contains(&row.label));
            prop_assert_eq!(line.2, row.status());
        }

        let aggregate = lines.last().expect("aggregate diagnostic line");
        let expected_worst = rows
            .iter()
            .map(TierSwapDoctorRow::status)
            .max_by_key(|status| severity(*status))
            .unwrap_or(TierSwapDoctorStatus::Ok);
        prop_assert!(aggregate.0.contains("aggregate"));
        prop_assert_eq!(aggregate.2, expected_worst);
    }
}
