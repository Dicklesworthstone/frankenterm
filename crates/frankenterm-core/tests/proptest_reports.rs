//! Property-based tests for reports module pure helper functions.
//!
//! Verifies invariants of:
//! - `days_to_ymd`: Gregorian calendar conversion (Hinnant algorithm)
//!   - month in 1..=12, day in 1..=31
//!   - leap year correctness (Feb 29 only in leap years)
//!   - roundtrip: ymd_to_days(days_to_ymd(d)) == d
//!   - monotonicity: consecutive days advance correctly
//! - `format_ts`: timestamp formatting
//!   - always ends with 'Z'
//!   - contains expected field separators
//!   - fixed output length for positive timestamps
//! - `format_duration`: duration tier formatting
//!   - tier boundaries: <1000 ms, <60000 seconds, >=60000 minutes
//!   - suffix correctness per tier
//!   - non-negative ms always produces non-empty output
//! - `truncate`: string truncation (byte-based, ASCII-safe)
//!   - short strings unchanged (idempotent)
//!   - output byte length bounded
//!   - ellipsis present only when truncated

use proptest::prelude::*;

use frankenterm_core::reports::{days_to_ymd, format_duration, format_ts, truncate};

// ────────────────────────────────────────────────────────────────────
// Oracle: inverse of days_to_ymd for roundtrip verification
// ────────────────────────────────────────────────────────────────────

/// Convert (year, month, day) back to days since Unix epoch.
/// Inverse of the Hinnant algorithm used in days_to_ymd.
fn ymd_to_days(year: i64, month: u32, day: u32) -> i64 {
    // From http://howardhinnant.github.io/date_algorithms.html
    let y = if month <= 2 { year - 1 } else { year };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = (y - era * 400) as u32;
    let m = month;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe as i64 - 719468
}

fn is_leap_year(y: i64) -> bool {
    (y % 4 == 0 && y % 100 != 0) || y % 400 == 0
}

fn days_in_month(y: i64, m: u32) -> u32 {
    match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => {
            if is_leap_year(y) {
                29
            } else {
                28
            }
        }
        _ => unreachable!(),
    }
}

// ────────────────────────────────────────────────────────────────────
// Strategies
// ────────────────────────────────────────────────────────────────────

/// Days spanning from ~year 0 to ~year 4000
fn arb_days() -> impl Strategy<Value = i64> {
    -719468i64..800000i64
}

/// Positive epoch-ms timestamps (1970 to ~2200)
fn arb_epoch_ms() -> impl Strategy<Value = i64> {
    0i64..7_258_118_400_000i64 // up to ~year 2200
}

/// Arbitrary ASCII strings for truncation tests
fn arb_ascii_string() -> impl Strategy<Value = String> {
    prop::collection::vec(32u8..127, 0..200)
        .prop_map(|v| v.into_iter().map(|b| b as char).collect::<String>())
}

fn arb_max_len() -> impl Strategy<Value = usize> {
    0usize..300
}

// ────────────────────────────────────────────────────────────────────
// days_to_ymd properties
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(2000))]

    /// Roundtrip: converting days→ymd→days recovers the original day count.
    #[test]
    fn days_to_ymd_roundtrip(days in arb_days()) {
        let (y, m, d) = days_to_ymd(days);
        let recovered = ymd_to_days(y, m, d);
        prop_assert_eq!(recovered, days, "roundtrip failed for day {}", days);
    }

    /// Month is always in [1, 12].
    #[test]
    fn days_to_ymd_month_range(days in arb_days()) {
        let (_, m, _) = days_to_ymd(days);
        prop_assert!(m >= 1 && m <= 12, "month {} out of range for day {}", m, days);
    }

    /// Day is always in [1, 31].
    #[test]
    fn days_to_ymd_day_range(days in arb_days()) {
        let (_, _, d) = days_to_ymd(days);
        prop_assert!(d >= 1 && d <= 31, "day {} out of range for day-count {}", d, days);
    }

    /// Day never exceeds the maximum for its month/year.
    #[test]
    fn days_to_ymd_day_within_month(days in arb_days()) {
        let (y, m, d) = days_to_ymd(days);
        let max_d = days_in_month(y, m);
        prop_assert!(d <= max_d, "day {} exceeds max {} for {}-{:02}", d, max_d, y, m);
    }

    /// Feb 29 only occurs in leap years.
    #[test]
    fn days_to_ymd_leap_year_feb29(days in arb_days()) {
        let (y, m, d) = days_to_ymd(days);
        if m == 2 && d == 29 {
            prop_assert!(is_leap_year(y), "Feb 29 in non-leap year {}", y);
        }
    }

    /// Consecutive days produce either the same or next date.
    #[test]
    fn days_to_ymd_monotonicity(days in -719468i64..799999i64) {
        let (y1, m1, d1) = days_to_ymd(days);
        let (y2, m2, d2) = days_to_ymd(days + 1);

        // Next day: either same month with d+1, or next month day 1, or next year
        let advanced = (y2 > y1)
            || (y2 == y1 && m2 > m1)
            || (y2 == y1 && m2 == m1 && d2 == d1 + 1);
        prop_assert!(
            advanced,
            "day {} -> ({},{},{}) to day {} -> ({},{},{}) not monotonic",
            days, y1, m1, d1, days + 1, y2, m2, d2
        );
    }

    /// Specific known dates: every Jan 1 from the epoch day count.
    #[test]
    fn days_to_ymd_jan1_identity(year in 1970i64..2200) {
        let day_count = ymd_to_days(year, 1, 1);
        let (y, m, d) = days_to_ymd(day_count);
        prop_assert_eq!(y, year, "year mismatch for Jan 1 {}", year);
        prop_assert_eq!(m, 1u32, "month mismatch for Jan 1 {}", year);
        prop_assert_eq!(d, 1u32, "day mismatch for Jan 1 {}", year);
    }

    /// Year 2000 divisible-by-400 leap year: Feb has 29 days.
    #[test]
    fn days_to_ymd_400_year_leap(century in prop::sample::select(vec![2000i64, 2400, 1600])) {
        let day_count = ymd_to_days(century, 2, 29);
        let (y, m, d) = days_to_ymd(day_count);
        prop_assert_eq!((y, m, d), (century, 2, 29), "400-year leap failed for {}", century);
    }

    /// Century years not divisible by 400 are NOT leap years.
    #[test]
    fn days_to_ymd_century_non_leap(century in prop::sample::select(vec![1900i64, 2100, 2200, 2300])) {
        // Mar 1 should be day after Feb 28 (no Feb 29)
        let feb28 = ymd_to_days(century, 2, 28);
        let (y, m, d) = days_to_ymd(feb28 + 1);
        prop_assert_eq!((y, m, d), (century, 3, 1), "non-leap century {} had Feb 29", century);
    }
}

// ────────────────────────────────────────────────────────────────────
// format_ts properties
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(2000))]

    /// Output always ends with 'Z' (UTC indicator).
    #[test]
    fn format_ts_ends_with_z(ms in arb_epoch_ms()) {
        let s = format_ts(ms);
        prop_assert!(s.ends_with('Z'), "format_ts({}) = '{}' doesn't end with Z", ms, s);
    }

    /// Output contains the expected separators: dash, colon, dot, space.
    #[test]
    fn format_ts_has_separators(ms in arb_epoch_ms()) {
        let s = format_ts(ms);
        prop_assert!(s.contains('-'), "missing dash in '{}'", s);
        prop_assert!(s.contains(':'), "missing colon in '{}'", s);
        prop_assert!(s.contains('.'), "missing dot in '{}'", s);
        prop_assert!(s.contains(' '), "missing space in '{}'", s);
    }

    /// Output has fixed format: "YYYY-MM-DD HH:MM:SS.mmmZ" = 24 chars for 4-digit years.
    #[test]
    fn format_ts_length_for_4digit_year(ms in 0i64..4_102_444_800_000i64) {
        let s = format_ts(ms);
        // Year range 1970-2099 → always 4-digit year → 24 chars
        prop_assert_eq!(s.len(), 24, "unexpected length for format_ts({}) = '{}'", ms, s);
    }

    /// Millisecond component is always 0..999.
    #[test]
    fn format_ts_millis_component(ms in arb_epoch_ms()) {
        let s = format_ts(ms);
        // Extract millis: last 4 chars are "mmmZ"
        let millis_str = &s[s.len() - 4..s.len() - 1];
        let millis: u32 = millis_str.parse().unwrap_or(9999);
        prop_assert!(millis <= 999, "millis {} out of range in '{}'", millis, s);
    }

    /// Hour is 0..23, minute 0..59, second 0..59.
    #[test]
    fn format_ts_time_components_valid(ms in arb_epoch_ms()) {
        let s = format_ts(ms);
        // Format: "YYYY-MM-DD HH:MM:SS.mmmZ"
        //          0123456789012345678901234
        let hh: u32 = s[11..13].parse().unwrap_or(99);
        let mm: u32 = s[14..16].parse().unwrap_or(99);
        let ss: u32 = s[17..19].parse().unwrap_or(99);
        prop_assert!(hh <= 23, "hour {} invalid in '{}'", hh, s);
        prop_assert!(mm <= 59, "minute {} invalid in '{}'", mm, s);
        prop_assert!(ss <= 59, "second {} invalid in '{}'", ss, s);
    }

    /// Month component is 01..12, day component is 01..31.
    #[test]
    fn format_ts_date_components_valid(ms in arb_epoch_ms()) {
        let s = format_ts(ms);
        let month: u32 = s[5..7].parse().unwrap_or(0);
        let day: u32 = s[8..10].parse().unwrap_or(0);
        prop_assert!(month >= 1 && month <= 12, "month {} invalid in '{}'", month, s);
        prop_assert!(day >= 1 && day <= 31, "day {} invalid in '{}'", day, s);
    }

    /// Monotonicity: later timestamps produce lexicographically >= output.
    #[test]
    fn format_ts_monotonic(a in 0i64..4_000_000_000_000i64, delta in 0i64..1_000_000_000i64) {
        let b = a + delta;
        let sa = format_ts(a);
        let sb = format_ts(b);
        prop_assert!(sb >= sa, "format_ts not monotonic: {} ('{}') vs {} ('{}')", a, sa, b, sb);
    }

    /// Epoch zero is the known value.
    #[test]
    fn format_ts_epoch_consistency(millis in 0i64..1000) {
        let s = format_ts(millis);
        prop_assert!(s.starts_with("1970-01-01 00:00:00."), "epoch millis {} gave '{}'", millis, s);
    }

}

// ────────────────────────────────────────────────────────────────────
// format_duration properties
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(2000))]

    /// Tier 1: ms < 1000 → output ends with "ms".
    #[test]
    fn format_duration_tier_ms(ms in 0i64..1000) {
        let s = format_duration(ms);
        prop_assert!(s.ends_with("ms"), "tier1: format_duration({}) = '{}' should end with ms", ms, s);
        // Should be just the number + "ms"
        let num_part = &s[..s.len() - 2];
        let parsed: i64 = num_part.parse().unwrap_or(-1);
        prop_assert_eq!(parsed, ms, "tier1: numeric mismatch in '{}'", s);
    }

    /// Tier 2: 1000 <= ms < 60000 → output ends with "s" but not "ms".
    #[test]
    fn format_duration_tier_seconds(ms in 1000i64..60000) {
        let s = format_duration(ms);
        prop_assert!(s.ends_with('s'), "tier2: format_duration({}) = '{}' should end with s", ms, s);
        prop_assert!(!s.ends_with("ms"), "tier2: format_duration({}) = '{}' should not end with ms", ms, s);
    }

    /// Tier 3: ms >= 60000 → output contains "m" and "s".
    #[test]
    fn format_duration_tier_minutes(ms in 60000i64..10_000_000) {
        let s = format_duration(ms);
        prop_assert!(s.contains('m'), "tier3: format_duration({}) = '{}' should contain m", ms, s);
        prop_assert!(s.contains('s'), "tier3: format_duration({}) = '{}' should contain s", ms, s);
    }

    /// Output is never empty for non-negative input.
    #[test]
    fn format_duration_non_empty(ms in 0i64..i64::MAX / 2) {
        let s = format_duration(ms);
        prop_assert!(!s.is_empty(), "format_duration({}) produced empty string", ms);
    }

    /// Tier boundaries are exact.
    #[test]
    fn format_duration_boundary_exact(offset in 0i64..10) {
        // Just below 1000
        let below = format_duration(999 - offset.min(999));
        prop_assert!(below.ends_with("ms"), "below 1000 should be ms: '{}'", below);

        // At 1000
        let at = format_duration(1000 + offset);
        prop_assert!(at.ends_with('s') && !at.ends_with("ms"), "at 1000+{} should be s: '{}'", offset, at);

        // Just below 60000
        let below60 = format_duration(59999 - offset.min(58999));
        prop_assert!(!below60.contains('m') || below60.ends_with("ms"), "below 60000 tier check: '{}'", below60);

        // At 60000
        let at60 = format_duration(60000 + offset * 1000);
        prop_assert!(at60.contains('m'), "at 60000+{} should contain m: '{}'", offset * 1000, at60);
    }

    /// Tier 2 value can be parsed as a float.
    #[test]
    fn format_duration_tier2_parseable(ms in 1000i64..60000) {
        let s = format_duration(ms);
        // Remove trailing 's'
        let num_str = &s[..s.len() - 1];
        let val: f64 = num_str.parse().unwrap_or(f64::NAN);
        prop_assert!(!val.is_nan(), "tier2 '{}' not parseable as float", s);
        prop_assert!(val >= 1.0 && val <= 60.0, "tier2 value {} out of expected range", val);
    }

    /// Tier 3 minutes and seconds are consistent.
    #[test]
    fn format_duration_tier3_components(ms in 60000i64..10_000_000) {
        let s = format_duration(ms);
        // Parse "Xm Ys" format
        let parts: Vec<&str> = s.split('m').collect();
        prop_assert_eq!(parts.len(), 2, "tier3 '{}' should split on m into 2 parts", s);
        let mins: i64 = parts[0].trim().parse().unwrap_or(-1);
        let secs_str = parts[1].trim().trim_end_matches('s').trim();
        let secs: i64 = secs_str.parse().unwrap_or(-1);
        prop_assert!(mins >= 1, "minutes should be >= 1: '{}'", s);
        prop_assert!(secs >= 0 && secs < 60, "seconds {} out of range in '{}'", secs, s);
        // Verify: mins * 60 + secs == ms / 1000 (integer division)
        let expected_total_secs = ms / 1000;
        let actual_total_secs = mins * 60 + secs;
        prop_assert_eq!(
            actual_total_secs,
            expected_total_secs,
            "component mismatch: {}m {}s vs {} total secs from {}ms",
            mins, secs, expected_total_secs, ms
        );
    }
}

// ────────────────────────────────────────────────────────────────────
// truncate properties
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(2000))]

    /// Short strings (len <= max) are returned unchanged.
    #[test]
    fn truncate_short_unchanged(s in arb_ascii_string(), extra in 0usize..50) {
        let max = s.len() + extra;
        let result = truncate(&s, max);
        prop_assert_eq!(&result, &s, "short string should be unchanged: max={}, len={}", max, s.len());
    }

    /// Truncated output byte length is bounded by max + ellipsis bytes.
    #[test]
    fn truncate_length_bounded(s in arb_ascii_string(), max in arb_max_len()) {
        let result = truncate(&s, max);
        if s.len() <= max {
            prop_assert_eq!(result.len(), s.len());
        } else {
            // Ellipsis '…' is 3 bytes in UTF-8
            prop_assert!(
                result.len() <= max + 3,
                "truncate output too long: {} bytes for max={} input_len={}",
                result.len(), max, s.len()
            );
        }
    }

    /// Ellipsis is present if and only if the string was truncated.
    #[test]
    fn truncate_ellipsis_iff_truncated(s in arb_ascii_string(), max in arb_max_len()) {
        let result = truncate(&s, max);
        if s.len() > max {
            prop_assert!(
                result.ends_with('…'),
                "truncated string should have ellipsis: '{}' (max={}, len={})",
                result, max, s.len()
            );
        } else {
            prop_assert!(
                !result.ends_with('…') || s.ends_with('…'),
                "non-truncated string should not gain ellipsis: '{}' (max={}, len={})",
                result, max, s.len()
            );
        }
    }

    /// Truncated output starts with the same prefix as the input.
    #[test]
    fn truncate_preserves_prefix(s in arb_ascii_string(), max in 1usize..200) {
        let result = truncate(&s, max);
        if s.len() > max {
            let prefix = &s[..max];
            prop_assert!(
                result.starts_with(prefix),
                "truncated result should start with first {} chars of input",
                max
            );
        }
    }

    /// Idempotence: truncating an already-short-enough string is a no-op.
    #[test]
    fn truncate_idempotent_short(s in "[a-z]{0,20}", max in 20usize..50) {
        let first = truncate(&s, max);
        let second = truncate(&first, max);
        prop_assert_eq!(&first, &second, "truncate should be idempotent for short strings");
    }

    /// Empty string always returns empty regardless of max.
    #[test]
    fn truncate_empty(max in arb_max_len()) {
        let result = truncate("", max);
        prop_assert_eq!(result, "", "empty string should always produce empty result");
    }

    /// max=0 always produces ellipsis for non-empty strings.
    #[test]
    fn truncate_max_zero(s in "[a-z]{1,20}") {
        let result = truncate(&s, 0);
        prop_assert!(result.ends_with('…'), "max=0 should produce ellipsis: '{}'", result);
    }

}

// ────────────────────────────────────────────────────────────────────
// Cross-function integration properties
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(500))]

    /// format_ts output can be truncated and still has valid structure.
    #[test]
    fn format_ts_truncate_composable(ms in arb_epoch_ms(), max in 5usize..30) {
        let ts = format_ts(ms);
        let truncated = truncate(&ts, max);
        if max >= ts.len() {
            prop_assert_eq!(&truncated, &ts);
        } else {
            prop_assert!(truncated.ends_with('…'));
            // The prefix should be a valid start of the timestamp
            prop_assert!(truncated.len() <= max + 3);
        }
    }

    /// format_duration output can be truncated without panic.
    #[test]
    fn format_duration_truncate_composable(ms in 0i64..10_000_000, max in 1usize..20) {
        let dur = format_duration(ms);
        let truncated = truncate(&dur, max);
        // Just verify no panic and basic invariants
        prop_assert!(!truncated.is_empty());
    }

    /// days_to_ymd feeds into format_ts consistently: same day → same date prefix.
    #[test]
    fn days_ymd_format_ts_consistent(days in -25567i64..20000i64) {
        let (y, m, d) = days_to_ymd(days);
        let ms = days * 86_400_000; // midnight of that day
        let formatted = format_ts(ms);
        let expected_date = format!("{:04}-{:02}-{:02}", y, m, d);
        prop_assert!(
            formatted.starts_with(&expected_date),
            "day {} → ({},{},{}) but format_ts({}) = '{}'",
            days, y, m, d, ms, formatted
        );
    }
}
