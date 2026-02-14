//! Property-based tests for the backup module.
//!
//! Tests cover: BackupSchedule (parse, display_label, next_after),
//! BackupManifest, BackupStats, ExportResult, ImportResult,
//! ExportOptions, ImportOptions serde roundtrips and defaults,
//! and scheduling math properties.

use chrono::{Datelike, Local, TimeZone, Timelike, Weekday};
use proptest::prelude::*;

use frankenterm_core::backup::{
    BackupManifest, BackupSchedule, BackupStats, ExportOptions, ExportResult, ImportOptions,
    ImportResult,
};

// ============================================================================
// Strategies
// ============================================================================

fn arb_backup_stats() -> impl Strategy<Value = BackupStats> {
    (
        any::<u64>(),
        any::<u64>(),
        any::<u64>(),
        any::<u64>(),
        any::<u64>(),
    )
        .prop_map(
            |(panes, segments, events, audit_actions, workflow_executions)| BackupStats {
                panes,
                segments,
                events,
                audit_actions,
                workflow_executions,
            },
        )
}

fn arb_backup_manifest() -> impl Strategy<Value = BackupManifest> {
    (
        "[a-z0-9.]{1,10}",
        1..100i32,
        "[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}Z",
        "/[a-z/]{1,20}",
        any::<u64>(),
        "[a-f0-9]{64}",
        arb_backup_stats(),
    )
        .prop_map(
            |(
                wa_version,
                schema_version,
                created_at,
                workspace,
                db_size_bytes,
                db_checksum,
                stats,
            )| {
                BackupManifest {
                    wa_version,
                    schema_version,
                    created_at,
                    workspace,
                    db_size_bytes,
                    db_checksum,
                    stats,
                }
            },
        )
}

/// Generate a valid chrono Local datetime in a reasonable range
fn arb_local_datetime() -> impl Strategy<Value = chrono::DateTime<Local>> {
    (2020..2035i32, 1..13u32, 1..29u32, 0..24u32, 0..60u32).prop_filter_map(
        "valid datetime",
        |(year, month, day, hour, minute)| {
            Local
                .with_ymd_and_hms(year, month, day, hour, minute, 0)
                .single()
        },
    )
}

// ============================================================================
// BackupStats properties
// ============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// BackupStats serde roundtrip
    #[test]
    fn prop_backup_stats_serde_roundtrip(stats in arb_backup_stats()) {
        let json = serde_json::to_string(&stats).unwrap();
        let decoded: BackupStats = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(stats.panes, decoded.panes);
        prop_assert_eq!(stats.segments, decoded.segments);
        prop_assert_eq!(stats.events, decoded.events);
        prop_assert_eq!(stats.audit_actions, decoded.audit_actions);
        prop_assert_eq!(stats.workflow_executions, decoded.workflow_executions);
    }

    /// BackupStats Default is all zeros
    #[test]
    fn prop_backup_stats_default(_dummy in 0..1u8) {
        let stats = BackupStats::default();
        prop_assert_eq!(stats.panes, 0);
        prop_assert_eq!(stats.segments, 0);
        prop_assert_eq!(stats.events, 0);
        prop_assert_eq!(stats.audit_actions, 0);
        prop_assert_eq!(stats.workflow_executions, 0);
    }
}

// ============================================================================
// BackupManifest properties
// ============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// BackupManifest serde roundtrip
    #[test]
    fn prop_backup_manifest_serde_roundtrip(manifest in arb_backup_manifest()) {
        let json = serde_json::to_string(&manifest).unwrap();
        let decoded: BackupManifest = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&manifest.wa_version, &decoded.wa_version);
        prop_assert_eq!(manifest.schema_version, decoded.schema_version);
        prop_assert_eq!(&manifest.created_at, &decoded.created_at);
        prop_assert_eq!(&manifest.workspace, &decoded.workspace);
        prop_assert_eq!(manifest.db_size_bytes, decoded.db_size_bytes);
        prop_assert_eq!(&manifest.db_checksum, &decoded.db_checksum);
        prop_assert_eq!(manifest.stats.panes, decoded.stats.panes);
    }

    /// BackupManifest JSON contains all required fields
    #[test]
    fn prop_backup_manifest_json_fields(manifest in arb_backup_manifest()) {
        let json = serde_json::to_string(&manifest).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        prop_assert!(parsed.get("wa_version").is_some());
        prop_assert!(parsed.get("schema_version").is_some());
        prop_assert!(parsed.get("created_at").is_some());
        prop_assert!(parsed.get("workspace").is_some());
        prop_assert!(parsed.get("db_size_bytes").is_some());
        prop_assert!(parsed.get("db_checksum").is_some());
        prop_assert!(parsed.get("stats").is_some());
    }
}

// ============================================================================
// ExportResult / ImportResult serde
// ============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// ExportResult serde roundtrip
    #[test]
    fn prop_export_result_serde_roundtrip(
        output_path in "[a-z/]{1,30}",
        manifest in arb_backup_manifest(),
        total_size_bytes in any::<u64>(),
    ) {
        let result = ExportResult {
            output_path: output_path.clone(),
            manifest,
            total_size_bytes,
        };
        let json = serde_json::to_string(&result).unwrap();
        let decoded: ExportResult = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&result.output_path, &decoded.output_path);
        prop_assert_eq!(result.total_size_bytes, decoded.total_size_bytes);
        prop_assert_eq!(&result.manifest.wa_version, &decoded.manifest.wa_version);
    }

    /// ImportResult serde roundtrip
    #[test]
    fn prop_import_result_serde_roundtrip(
        source_path in "[a-z/]{1,30}",
        manifest in arb_backup_manifest(),
        has_safety in any::<bool>(),
        dry_run in any::<bool>(),
    ) {
        let result = ImportResult {
            source_path: source_path.clone(),
            manifest,
            safety_backup_path: if has_safety {
                Some("/tmp/safety".to_string())
            } else {
                None
            },
            dry_run,
        };
        let json = serde_json::to_string(&result).unwrap();
        let decoded: ImportResult = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&result.source_path, &decoded.source_path);
        prop_assert_eq!(result.dry_run, decoded.dry_run);
        prop_assert_eq!(result.safety_backup_path, decoded.safety_backup_path);
    }
}

// ============================================================================
// ExportOptions / ImportOptions defaults
// ============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(10))]

    /// ExportOptions default: output=None, include_sql_dump=false, verify=true
    #[test]
    fn prop_export_options_default(_dummy in 0..1u8) {
        let opts = ExportOptions::default();
        prop_assert!(opts.output.is_none());
        prop_assert!(!opts.include_sql_dump);
        prop_assert!(opts.verify);
    }

    /// ImportOptions default: all false
    #[test]
    fn prop_import_options_default(_dummy in 0..1u8) {
        let opts = ImportOptions::default();
        prop_assert!(!opts.dry_run);
        prop_assert!(!opts.yes);
        prop_assert!(!opts.no_safety_backup);
    }
}

// ============================================================================
// BackupSchedule::parse properties
// ============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// "hourly" (case-insensitive) always parses to Hourly
    #[test]
    fn prop_schedule_parse_hourly(
        padding in "[\\s]{0,3}",
    ) {
        for variant in &["hourly", "Hourly", "HOURLY", "HoUrLy"] {
            let input = format!("{}{}{}", padding, variant, padding);
            let schedule = BackupSchedule::parse(&input).unwrap();
            prop_assert!(
                matches!(schedule, BackupSchedule::Hourly { minute: 0 }),
                "Expected Hourly, got {:?}", schedule
            );
        }
    }

    /// "daily" (case-insensitive) always parses to Daily with defaults
    #[test]
    fn prop_schedule_parse_daily(
        padding in "[\\s]{0,3}",
    ) {
        for variant in &["daily", "Daily", "DAILY"] {
            let input = format!("{}{}{}", padding, variant, padding);
            let schedule = BackupSchedule::parse(&input).unwrap();
            prop_assert!(
                matches!(schedule, BackupSchedule::Daily { hour: 3, minute: 0 }),
                "Expected Daily(3,0), got {:?}", schedule
            );
        }
    }

    /// "weekly" (case-insensitive) always parses to Weekly with defaults
    #[test]
    fn prop_schedule_parse_weekly(
        padding in "[\\s]{0,3}",
    ) {
        for variant in &["weekly", "Weekly", "WEEKLY"] {
            let input = format!("{}{}{}", padding, variant, padding);
            let schedule = BackupSchedule::parse(&input).unwrap();
            match &schedule {
                BackupSchedule::Weekly { weekday, hour, minute } => {
                    prop_assert_eq!(*weekday, Weekday::Sun);
                    prop_assert_eq!(*hour, 3);
                    prop_assert_eq!(*minute, 0);
                }
                _ => prop_assert!(false, "Expected Weekly, got {:?}", schedule),
            }
        }
    }

    /// Valid 5-field cron always parses to Cron variant
    #[test]
    fn prop_schedule_parse_valid_cron(
        minute in 0..60u32,
        hour in 0..24u32,
    ) {
        let input = format!("{} {} * * *", minute, hour);
        let schedule = BackupSchedule::parse(&input).unwrap();
        prop_assert!(
            matches!(schedule, BackupSchedule::Cron(_)),
            "Expected Cron, got {:?}", schedule
        );
    }

    /// Cron with all wildcards parses successfully
    #[test]
    fn prop_schedule_parse_all_wildcard(_dummy in 0..1u8) {
        let schedule = BackupSchedule::parse("* * * * *").unwrap();
        prop_assert!(matches!(schedule, BackupSchedule::Cron(_)));
    }

    /// Invalid field count (not 1 keyword, not 5 fields) returns error
    #[test]
    fn prop_schedule_parse_invalid_count(
        extra_field in "[a-z]{1,5}",
    ) {
        let input = format!("0 3 * * * {}", extra_field);
        prop_assert!(BackupSchedule::parse(&input).is_err());
    }

    /// Cron minute out of range (60+) returns error
    #[test]
    fn prop_schedule_parse_minute_out_of_range(minute in 60..200u32) {
        let input = format!("{} 0 * * *", minute);
        prop_assert!(BackupSchedule::parse(&input).is_err());
    }

    /// Cron hour out of range (24+) returns error
    #[test]
    fn prop_schedule_parse_hour_out_of_range(hour in 24..200u32) {
        let input = format!("0 {} * * *", hour);
        prop_assert!(BackupSchedule::parse(&input).is_err());
    }

    /// Cron month out of range (13+) returns error
    #[test]
    fn prop_schedule_parse_month_out_of_range(month in 13..200u32) {
        let input = format!("0 0 * {} *", month);
        prop_assert!(BackupSchedule::parse(&input).is_err());
    }

    /// Non-numeric cron field returns error
    #[test]
    fn prop_schedule_parse_non_numeric_field(
        bad in "[a-z]{2,5}",
    ) {
        let input = format!("{} 0 * * *", bad);
        prop_assert!(BackupSchedule::parse(&input).is_err());
    }

    /// Random gibberish is rejected
    #[test]
    fn prop_schedule_parse_gibberish(
        gibberish in "[a-z]{6,20}",
    ) {
        // Anything that's not hourly/daily/weekly and not 5 fields should fail
        if !["hourly", "daily", "weekly"].contains(&gibberish.as_str()) {
            prop_assert!(BackupSchedule::parse(&gibberish).is_err());
        }
    }
}

// ============================================================================
// BackupSchedule::display_label properties
// ============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// Keyword schedules have matching display labels
    #[test]
    fn prop_schedule_display_keyword(_dummy in 0..1u8) {
        let hourly = BackupSchedule::parse("hourly").unwrap();
        prop_assert_eq!(hourly.display_label(), "hourly");

        let daily = BackupSchedule::parse("daily").unwrap();
        prop_assert_eq!(daily.display_label(), "daily");

        let weekly = BackupSchedule::parse("weekly").unwrap();
        prop_assert_eq!(weekly.display_label(), "weekly");
    }

    /// Cron display label starts with "cron:"
    #[test]
    fn prop_schedule_display_cron(
        minute in 0..60u32,
        hour in 0..24u32,
    ) {
        let input = format!("{} {} * * *", minute, hour);
        let schedule = BackupSchedule::parse(&input).unwrap();
        let label = schedule.display_label();
        prop_assert!(
            label.starts_with("cron:"),
            "Expected 'cron:' prefix, got: {}", label
        );
    }

    /// display_label is never empty
    #[test]
    fn prop_schedule_display_nonempty(
        minute in 0..60u32,
        hour in 0..24u32,
    ) {
        for input in &[
            "hourly".to_string(),
            "daily".to_string(),
            "weekly".to_string(),
            format!("{} {} * * *", minute, hour),
        ] {
            let schedule = BackupSchedule::parse(input).unwrap();
            prop_assert!(!schedule.display_label().is_empty());
        }
    }
}

// ============================================================================
// BackupSchedule::next_after properties
// ============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// Hourly next_after always returns a time > now
    #[test]
    fn prop_schedule_hourly_next_after_future(now in arb_local_datetime()) {
        let schedule = BackupSchedule::parse("hourly").unwrap();
        let next = schedule.next_after(now).unwrap();
        prop_assert!(next > now, "next_after should be in the future");
    }

    /// Hourly next_after is within 1 hour of now
    #[test]
    fn prop_schedule_hourly_next_within_hour(now in arb_local_datetime()) {
        let schedule = BackupSchedule::parse("hourly").unwrap();
        let next = schedule.next_after(now).unwrap();
        let diff = next.signed_duration_since(now);
        prop_assert!(
            diff.num_minutes() <= 60,
            "Hourly should be within 60 minutes, got {} minutes", diff.num_minutes()
        );
    }

    /// Hourly next_after minute is always 0 (the default)
    #[test]
    fn prop_schedule_hourly_next_minute(now in arb_local_datetime()) {
        let schedule = BackupSchedule::parse("hourly").unwrap();
        let next = schedule.next_after(now).unwrap();
        prop_assert_eq!(next.minute(), 0, "Hourly default should fire at minute 0");
    }

    /// Daily next_after always returns a time > now
    #[test]
    fn prop_schedule_daily_next_after_future(now in arb_local_datetime()) {
        let schedule = BackupSchedule::parse("daily").unwrap();
        let next = schedule.next_after(now).unwrap();
        prop_assert!(next > now, "next_after should be in the future");
    }

    /// Daily next_after is within ~28 hours of now.  The default daily
    /// schedule fires at 03:00, so if `now` is 23:00 the next fire is
    /// tomorrow's 03:00 (≈28h).  Spring-forward DST can shift this by ±1h.
    #[test]
    fn prop_schedule_daily_next_within_day(now in arb_local_datetime()) {
        let schedule = BackupSchedule::parse("daily").unwrap();
        let next = schedule.next_after(now).unwrap();
        let diff = next.signed_duration_since(now);
        prop_assert!(
            diff.num_hours() <= 28,
            "Daily should be within 28 hours, got {} hours", diff.num_hours()
        );
    }

    /// Daily default fires at 03:00
    #[test]
    fn prop_schedule_daily_next_time(now in arb_local_datetime()) {
        let schedule = BackupSchedule::parse("daily").unwrap();
        let next = schedule.next_after(now).unwrap();
        prop_assert_eq!(next.hour(), 3, "Daily default should fire at hour 3");
        prop_assert_eq!(next.minute(), 0, "Daily default should fire at minute 0");
    }

    /// Weekly next_after always returns a time > now
    #[test]
    fn prop_schedule_weekly_next_after_future(now in arb_local_datetime()) {
        let schedule = BackupSchedule::parse("weekly").unwrap();
        let next = schedule.next_after(now).unwrap();
        prop_assert!(next > now, "next_after should be in the future");
    }

    /// Weekly next_after is within 7 days of now
    #[test]
    fn prop_schedule_weekly_next_within_week(now in arb_local_datetime()) {
        let schedule = BackupSchedule::parse("weekly").unwrap();
        let next = schedule.next_after(now).unwrap();
        let diff = next.signed_duration_since(now);
        prop_assert!(
            diff.num_days() <= 7,
            "Weekly should be within 7 days, got {} days", diff.num_days()
        );
    }

    /// Weekly default fires on Sunday
    #[test]
    fn prop_schedule_weekly_next_day(now in arb_local_datetime()) {
        let schedule = BackupSchedule::parse("weekly").unwrap();
        let next = schedule.next_after(now).unwrap();
        prop_assert_eq!(next.weekday(), Weekday::Sun, "Weekly default should fire on Sunday");
    }

    /// Cron next_after is always > now
    #[test]
    fn prop_schedule_cron_next_after_future(
        now in arb_local_datetime(),
        minute in 0..60u32,
        hour in 0..24u32,
    ) {
        let input = format!("{} {} * * *", minute, hour);
        let schedule = BackupSchedule::parse(&input).unwrap();
        let next = schedule.next_after(now).unwrap();
        prop_assert!(next > now, "Cron next_after should be in the future");
    }

    /// Cron with specific minute/hour matches those values
    #[test]
    fn prop_schedule_cron_specific_time(
        now in arb_local_datetime(),
        minute in 0..60u32,
        hour in 0..24u32,
    ) {
        let input = format!("{} {} * * *", minute, hour);
        let schedule = BackupSchedule::parse(&input).unwrap();
        let next = schedule.next_after(now).unwrap();
        prop_assert_eq!(next.minute(), minute, "Cron should match specified minute");
        prop_assert_eq!(next.hour(), hour, "Cron should match specified hour");
    }

    /// All-wildcard cron fires within 1 minute
    #[test]
    fn prop_schedule_cron_all_wildcard_soon(now in arb_local_datetime()) {
        let schedule = BackupSchedule::parse("* * * * *").unwrap();
        let next = schedule.next_after(now).unwrap();
        let diff = next.signed_duration_since(now);
        prop_assert!(
            diff.num_minutes() <= 1,
            "All-wildcard cron should fire within 1 minute, got {} minutes", diff.num_minutes()
        );
    }
}

// ============================================================================
// BackupSchedule equality and clone
// ============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// parse(x) == parse(x) (deterministic)
    #[test]
    fn prop_schedule_parse_deterministic(
        minute in 0..60u32,
        hour in 0..24u32,
    ) {
        let input = format!("{} {} * * *", minute, hour);
        let a = BackupSchedule::parse(&input).unwrap();
        let b = BackupSchedule::parse(&input).unwrap();
        prop_assert_eq!(a, b);
    }

    /// Keyword schedules have correct equality
    #[test]
    fn prop_schedule_keyword_eq(_dummy in 0..1u8) {
        let h1 = BackupSchedule::parse("hourly").unwrap();
        let h2 = BackupSchedule::parse("hourly").unwrap();
        prop_assert_eq!(h1, h2);

        let d1 = BackupSchedule::parse("daily").unwrap();
        let d2 = BackupSchedule::parse("daily").unwrap();
        prop_assert_eq!(d1, d2);

        let w1 = BackupSchedule::parse("weekly").unwrap();
        let w2 = BackupSchedule::parse("weekly").unwrap();
        prop_assert_eq!(w1, w2);
    }

    /// Different schedules are not equal
    #[test]
    fn prop_schedule_different_not_eq(_dummy in 0..1u8) {
        let hourly = BackupSchedule::parse("hourly").unwrap();
        let daily = BackupSchedule::parse("daily").unwrap();
        let weekly = BackupSchedule::parse("weekly").unwrap();
        prop_assert_ne!(hourly.clone(), daily.clone());
        prop_assert_ne!(hourly, weekly.clone());
        prop_assert_ne!(daily, weekly);
    }

    /// Clone preserves equality
    #[test]
    fn prop_schedule_clone_eq(
        minute in 0..60u32,
        hour in 0..24u32,
    ) {
        let input = format!("{} {} * * *", minute, hour);
        let a = BackupSchedule::parse(&input).unwrap();
        let b = a.clone();
        prop_assert_eq!(a, b);
    }
}

// ============================================================================
// Cross-module / integration properties
// ============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// Two successive next_after calls produce monotonically increasing results
    #[test]
    fn prop_schedule_next_monotonic(
        now in arb_local_datetime(),
    ) {
        let schedule = BackupSchedule::parse("hourly").unwrap();
        let next1 = schedule.next_after(now).unwrap();
        let next2 = schedule.next_after(next1).unwrap();
        prop_assert!(next2 > next1, "Second next should be after first");
    }

    /// Parsing and then display_label produces a non-empty string for all valid inputs
    #[test]
    fn prop_schedule_parse_then_display(
        minute in 0..60u32,
        hour in 0..24u32,
    ) {
        for input in &[
            "hourly".to_string(),
            "daily".to_string(),
            "weekly".to_string(),
            format!("{} {} * * *", minute, hour),
        ] {
            let schedule = BackupSchedule::parse(input).unwrap();
            let label = schedule.display_label();
            prop_assert!(!label.is_empty(), "Label should never be empty for valid schedule");
        }
    }

    /// BackupStats serializes to JSON with all 5 fields
    #[test]
    fn prop_backup_stats_json_fields(stats in arb_backup_stats()) {
        let json = serde_json::to_string(&stats).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        prop_assert!(parsed.get("panes").is_some());
        prop_assert!(parsed.get("segments").is_some());
        prop_assert!(parsed.get("events").is_some());
        prop_assert!(parsed.get("audit_actions").is_some());
        prop_assert!(parsed.get("workflow_executions").is_some());
    }

    /// BackupManifest round-trip preserves stats nested structure
    #[test]
    fn prop_manifest_preserves_nested_stats(manifest in arb_backup_manifest()) {
        let json = serde_json::to_string(&manifest).unwrap();
        let decoded: BackupManifest = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(manifest.stats.panes, decoded.stats.panes);
        prop_assert_eq!(manifest.stats.segments, decoded.stats.segments);
        prop_assert_eq!(manifest.stats.events, decoded.stats.events);
        prop_assert_eq!(manifest.stats.audit_actions, decoded.stats.audit_actions);
        prop_assert_eq!(
            manifest.stats.workflow_executions,
            decoded.stats.workflow_executions
        );
    }
}
