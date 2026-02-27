//! Property-based tests for code_scanner.rs.
//!
//! Covers serde roundtrips for all scanner types, FindingSeverity ordering,
//! ScanTotals arithmetic invariants, ScanClassification logic (classify
//! function precedence rules), forward-compatibility with unknown fields,
//! and deterministic classification.

#![cfg(feature = "subprocess-bridge")]

use std::collections::HashMap;

use frankenterm_core::code_scanner::{
    CodeScanner, FindingSeverity, ScanClassification, ScanFinding, ScanReport, ScanTotals,
    ScannerSummary,
};
use proptest::prelude::*;

// ── Strategies ──────────────────────────────────────────────────────────────

fn arb_severity() -> impl Strategy<Value = FindingSeverity> {
    prop_oneof![
        Just(FindingSeverity::Info),
        Just(FindingSeverity::Warning),
        Just(FindingSeverity::Critical),
    ]
}

fn arb_totals() -> impl Strategy<Value = ScanTotals> {
    (0..=100usize, 0..=500usize, 0..=200usize, 0..=50usize).prop_map(
        |(critical, warning, info, files)| ScanTotals {
            critical,
            warning,
            info,
            files,
        },
    )
}

fn arb_finding() -> impl Strategy<Value = ScanFinding> {
    (
        arb_severity(),
        "[a-z-]{3,12}",
        "[A-Za-z ]{5,30}",
        proptest::option::of("[a-z_]+\\.rs"),
        proptest::option::of(1..=1000u32),
        proptest::option::of("[A-Za-z ]{5,20}"),
    )
        .prop_map(
            |(severity, category, message, file, line, suggestion)| ScanFinding {
                severity,
                category,
                message,
                file,
                line,
                suggestion,
                extra: HashMap::new(),
            },
        )
}

fn arb_scanner_summary() -> impl Strategy<Value = ScannerSummary> {
    (
        proptest::option::of("[a-z]{2,10}"),
        0..=500usize,
        0..=100usize,
        0..=500usize,
        0..=200usize,
    )
        .prop_map(
            |(language, files, critical, warning, info)| ScannerSummary {
                language,
                files,
                critical,
                warning,
                info,
                extra: HashMap::new(),
            },
        )
}

fn arb_report() -> impl Strategy<Value = ScanReport> {
    (
        proptest::option::of("/[a-z/]{3,20}"),
        prop::collection::vec(arb_scanner_summary(), 0..=3),
        arb_totals(),
    )
        .prop_map(|(project, scanners, totals)| ScanReport {
            project,
            scanners,
            totals,
            extra: HashMap::new(),
        })
}

// ── FindingSeverity ─────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    // 1. Serde roundtrip
    #[test]
    fn severity_serde_roundtrip(sev in arb_severity()) {
        let json = serde_json::to_string(&sev).unwrap();
        let restored: FindingSeverity = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(restored, sev);
    }

    // 2. Total ordering: Info < Warning < Critical
    #[test]
    fn severity_total_ordering(a in arb_severity(), b in arb_severity()) {
        let a_rank = match a {
            FindingSeverity::Info => 0,
            FindingSeverity::Warning => 1,
            FindingSeverity::Critical => 2,
        };
        let b_rank = match b {
            FindingSeverity::Info => 0,
            FindingSeverity::Warning => 1,
            FindingSeverity::Critical => 2,
        };
        prop_assert_eq!(a.cmp(&b), a_rank.cmp(&b_rank));
    }

    // 3. Display matches serde name
    #[test]
    fn severity_display_matches_serde(sev in arb_severity()) {
        let display = sev.to_string();
        let json_str = serde_json::to_string(&sev).unwrap();
        // json_str is like "\"info\"", display is like "info"
        prop_assert_eq!(format!("\"{}\"", display), json_str);
    }
}

// ── ScanTotals ──────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    // 4. total() = critical + warning + info
    #[test]
    fn totals_sum_correct(totals in arb_totals()) {
        prop_assert_eq!(totals.total(), totals.critical + totals.warning + totals.info);
    }

    // 5. has_critical() iff critical > 0
    #[test]
    fn totals_has_critical_iff_positive(totals in arb_totals()) {
        prop_assert_eq!(totals.has_critical(), totals.critical > 0);
    }

    // 6. Serde roundtrip
    #[test]
    fn totals_serde_roundtrip(totals in arb_totals()) {
        let json = serde_json::to_string(&totals).unwrap();
        let restored: ScanTotals = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(restored.total(), totals.total());
        prop_assert_eq!(restored.critical, totals.critical);
        prop_assert_eq!(restored.warning, totals.warning);
        prop_assert_eq!(restored.info, totals.info);
        prop_assert_eq!(restored.files, totals.files);
    }

    // 7. Default is all zeros
    #[test]
    fn totals_default_zero(_seed in 0..=10u32) {
        let t = ScanTotals::default();
        prop_assert_eq!(t.total(), 0);
        prop_assert!(!t.has_critical());
    }
}

// ── ScanFinding serde ───────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    // 8. ScanFinding serde roundtrip
    #[test]
    fn finding_serde_roundtrip(finding in arb_finding()) {
        let json = serde_json::to_string(&finding).unwrap();
        let restored: ScanFinding = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(restored.severity, finding.severity);
        prop_assert_eq!(&restored.category, &finding.category);
        prop_assert_eq!(&restored.message, &finding.message);
        prop_assert_eq!(&restored.file, &finding.file);
        prop_assert_eq!(restored.line, finding.line);
        prop_assert_eq!(&restored.suggestion, &finding.suggestion);
    }
}

// ── ScannerSummary serde ────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    // 9. ScannerSummary serde roundtrip
    #[test]
    fn scanner_summary_serde_roundtrip(summary in arb_scanner_summary()) {
        let json = serde_json::to_string(&summary).unwrap();
        let restored: ScannerSummary = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&restored.language, &summary.language);
        prop_assert_eq!(restored.files, summary.files);
        prop_assert_eq!(restored.critical, summary.critical);
        prop_assert_eq!(restored.warning, summary.warning);
        prop_assert_eq!(restored.info, summary.info);
    }
}

// ── ScanReport serde ────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    // 10. ScanReport serde roundtrip
    #[test]
    fn report_serde_roundtrip(report in arb_report()) {
        let json = serde_json::to_string(&report).unwrap();
        let restored: ScanReport = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&restored.project, &report.project);
        prop_assert_eq!(restored.scanners.len(), report.scanners.len());
        prop_assert_eq!(restored.totals.total(), report.totals.total());
    }
}

// ── ScanClassification (classify) ───────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    // 11. Critical > 0 ⟹ Classification::Critical (highest precedence)
    #[test]
    fn classify_critical_overrides_all(
        critical in 1..=100usize,
        warning in 0..=500usize,
        info in 0..=200usize,
    ) {
        let report = ScanReport {
            project: None,
            scanners: Vec::new(),
            totals: ScanTotals { critical, warning, info, files: 1 },
            extra: HashMap::new(),
        };
        prop_assert_eq!(CodeScanner::classify(&report), ScanClassification::Critical);
    }

    // 12. No critical + warning > 100 ⟹ HighWarning
    #[test]
    fn classify_high_warning(
        warning in 101..=1000usize,
        info in 0..=200usize,
    ) {
        let report = ScanReport {
            project: None,
            scanners: Vec::new(),
            totals: ScanTotals { critical: 0, warning, info, files: 1 },
            extra: HashMap::new(),
        };
        prop_assert_eq!(CodeScanner::classify(&report), ScanClassification::HighWarning);
    }

    // 13. No critical + warning in [1, 100] ⟹ Warning
    #[test]
    fn classify_warning(
        warning in 1..=100usize,
        info in 0..=200usize,
    ) {
        let report = ScanReport {
            project: None,
            scanners: Vec::new(),
            totals: ScanTotals { critical: 0, warning, info, files: 1 },
            extra: HashMap::new(),
        };
        prop_assert_eq!(CodeScanner::classify(&report), ScanClassification::Warning);
    }

    // 14. No critical + no warning ⟹ Clean (regardless of info count)
    #[test]
    fn classify_clean(info in 0..=200usize) {
        let report = ScanReport {
            project: None,
            scanners: Vec::new(),
            totals: ScanTotals { critical: 0, warning: 0, info, files: 0 },
            extra: HashMap::new(),
        };
        prop_assert_eq!(CodeScanner::classify(&report), ScanClassification::Clean);
    }

    // 15. classify is deterministic
    #[test]
    fn classify_deterministic(report in arb_report()) {
        let c1 = CodeScanner::classify(&report);
        let c2 = CodeScanner::classify(&report);
        prop_assert_eq!(c1, c2);
    }

    // 16. classify precedence: exactly one of {Clean, Warning, HighWarning, Critical}
    #[test]
    fn classify_exhaustive(report in arb_report()) {
        let class = CodeScanner::classify(&report);
        let is_valid = matches!(
            class,
            ScanClassification::Clean
                | ScanClassification::Warning
                | ScanClassification::HighWarning
                | ScanClassification::Critical
        );
        prop_assert!(is_valid);
    }

    // 17. classify monotonicity: adding criticals never reduces severity
    #[test]
    fn classify_monotonic_critical(
        warning in 0..=200usize,
        info in 0..=100usize,
        extra_critical in 1..=10usize,
    ) {
        let base = ScanReport {
            project: None,
            scanners: Vec::new(),
            totals: ScanTotals { critical: 0, warning, info, files: 1 },
            extra: HashMap::new(),
        };
        let elevated = ScanReport {
            project: None,
            scanners: Vec::new(),
            totals: ScanTotals { critical: extra_critical, warning, info, files: 1 },
            extra: HashMap::new(),
        };
        let base_class = CodeScanner::classify(&base);
        let elevated_class = CodeScanner::classify(&elevated);

        // Critical always >= any other classification
        prop_assert_eq!(elevated_class, ScanClassification::Critical);

        // Base can never be MORE severe than elevated
        fn rank(c: ScanClassification) -> u8 {
            match c {
                ScanClassification::Clean => 0,
                ScanClassification::Warning => 1,
                ScanClassification::HighWarning => 2,
                ScanClassification::Critical => 3,
            }
        }
        prop_assert!(rank(base_class) <= rank(elevated_class));
    }
}

// ── Forward compatibility ───────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(16))]

    // 18. Unknown JSON fields preserved in `extra`
    #[test]
    fn report_forward_compat(_seed in 0..=10u32) {
        let json = r#"{
            "project": "/tmp/test",
            "scanners": [],
            "totals": {"critical":0,"warning":0,"info":0,"files":0},
            "future_field": "preserved"
        }"#;
        let report: ScanReport = serde_json::from_str(json).unwrap();
        prop_assert!(report.extra.contains_key("future_field"));
        prop_assert_eq!(&report.extra["future_field"], "preserved");
    }

    // 19. ScanFinding forward compat
    #[test]
    fn finding_forward_compat(_seed in 0..=10u32) {
        let json = r#"{
            "severity": "info",
            "category": "test",
            "message": "msg",
            "new_field_2027": 42
        }"#;
        let finding: ScanFinding = serde_json::from_str(json).unwrap();
        prop_assert!(finding.extra.contains_key("new_field_2027"));
    }

    // 20. ScannerSummary forward compat
    #[test]
    fn scanner_summary_forward_compat(_seed in 0..=10u32) {
        let json = r#"{
            "language": "rust",
            "files": 100,
            "critical": 0,
            "warning": 10,
            "info": 5,
            "extra_scanner_field": true
        }"#;
        let summary: ScannerSummary = serde_json::from_str(json).unwrap();
        prop_assert!(summary.extra.contains_key("extra_scanner_field"));
    }
}
