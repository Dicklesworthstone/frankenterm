//! Property tests for traceability_verification module.
//!
//! Covers serde roundtrips for enums and structs, domain derivation
//! determinism, status/severity parsing, coverage computation invariants,
//! risk score bounds, and VerificationPack structural properties.

use frankenterm_core::traceability_verification::*;
use proptest::prelude::*;

// =============================================================================
// Strategies
// =============================================================================

fn arb_capability_domain() -> impl Strategy<Value = CapabilityDomain> {
    prop_oneof![
        Just(CapabilityDomain::FrankenTerm),
        Just(CapabilityDomain::Ntm),
        Just(CapabilityDomain::Fcp),
    ]
}

fn arb_implementation_status() -> impl Strategy<Value = ImplementationStatus> {
    prop_oneof![
        Just(ImplementationStatus::Implemented),
        Just(ImplementationStatus::Partial),
        Just(ImplementationStatus::Gap),
    ]
}

fn arb_gap_severity() -> impl Strategy<Value = GapSeverity> {
    prop_oneof![
        Just(GapSeverity::None),
        Just(GapSeverity::Low),
        Just(GapSeverity::Medium),
        Just(GapSeverity::High),
    ]
}

fn arb_verification_category() -> impl Strategy<Value = VerificationCategory> {
    prop_oneof![
        Just(VerificationCategory::Schema),
        Just(VerificationCategory::Completeness),
        Just(VerificationCategory::Consistency),
        Just(VerificationCategory::AnchorValidity),
        Just(VerificationCategory::BeadMapping),
        Just(VerificationCategory::Coverage),
    ]
}

fn arb_pack_verdict() -> impl Strategy<Value = PackVerdict> {
    prop_oneof![
        Just(PackVerdict::Complete),
        Just(PackVerdict::ConditionalPass),
        Just(PackVerdict::Incomplete),
    ]
}

fn arb_status_string() -> impl Strategy<Value = String> {
    prop_oneof![
        Just("implemented".to_string()),
        Just("partial".to_string()),
        Just("gap".to_string()),
        Just("unknown".to_string()),
    ]
}

fn arb_severity_string() -> impl Strategy<Value = String> {
    prop_oneof![
        Just("none".to_string()),
        Just("low".to_string()),
        Just("medium".to_string()),
        Just("high".to_string()),
        Just("unknown".to_string()),
    ]
}

/// Generate a CapabilityEntry with a domain-appropriate ID prefix.
fn arb_capability_entry() -> impl Strategy<Value = CapabilityEntry> {
    (
        prop_oneof![
            Just("ft.".to_string()),
            Just("ntm.".to_string()),
            Just("fcp.".to_string()),
        ],
        "[a-z_]{3,12}",
        "[A-Za-z ]{3,30}",
        arb_status_string(),
        arb_severity_string(),
        proptest::collection::vec("[a-z0-9-]{3,12}", 0..3),
        proptest::collection::vec("[a-z_ ]{3,15}", 1..3),
        proptest::collection::vec("[a-z_/.]{5,20}", 1..3),
        ".{0,30}",
    )
        .prop_map(
            |(prefix, suffix, name, status, gap_severity, beads, surfaces, anchors, notes)| {
                CapabilityEntry {
                    capability_id: format!("{prefix}{suffix}"),
                    capability_name: name,
                    source_domain: prefix.trim_end_matches('.').to_string(),
                    status,
                    gap_severity,
                    mapped_bead_ids: beads,
                    surfaces,
                    implementation_anchors: anchors,
                    evidence_notes: notes,
                }
            },
        )
}

/// Generate a valid TraceabilityMatrix with entries covering all 3 domains.
fn arb_traceability_matrix() -> impl Strategy<Value = TraceabilityMatrix> {
    (
        proptest::collection::vec(arb_capability_entry(), 3..8),
        proptest::collection::vec("[a-z.]{5,15}", 0..3),
    )
        .prop_map(|(mut entries, required)| {
            // Ensure at least one entry per domain for coverage checks
            let has_ft = entries.iter().any(|e| e.capability_id.starts_with("ft."));
            let has_ntm = entries.iter().any(|e| e.capability_id.starts_with("ntm."));
            let has_fcp = entries.iter().any(|e| e.capability_id.starts_with("fcp."));
            if !has_ft {
                let mut e = entries[0].clone();
                e.capability_id = "ft.injected".to_string();
                entries.push(e);
            }
            if !has_ntm {
                let mut e = entries[0].clone();
                e.capability_id = "ntm.injected".to_string();
                entries.push(e);
            }
            if !has_fcp {
                let mut e = entries[0].clone();
                e.capability_id = "fcp.injected".to_string();
                entries.push(e);
            }
            // Deduplicate capability_ids
            let mut seen = std::collections::HashSet::new();
            entries.retain(|e| seen.insert(e.capability_id.clone()));

            TraceabilityMatrix {
                schema_version: "1.0.0".to_string(),
                artifact: "ntm-fcp-traceability-matrix".to_string(),
                bead_id: "ft-test".to_string(),
                generated_at_utc: "2026-03-12T00:00:00Z".to_string(),
                required_capability_ids: required,
                entries,
            }
        })
}

// =============================================================================
// Serde roundtrips — enums with PartialEq
// =============================================================================

proptest! {
    #[test]
    fn serde_roundtrip_capability_domain(d in arb_capability_domain()) {
        let json = serde_json::to_string(&d).unwrap();
        let back: CapabilityDomain = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(d, back);
    }

    #[test]
    fn serde_roundtrip_implementation_status(s in arb_implementation_status()) {
        let json = serde_json::to_string(&s).unwrap();
        let back: ImplementationStatus = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(s, back);
    }

    #[test]
    fn serde_roundtrip_gap_severity(g in arb_gap_severity()) {
        let json = serde_json::to_string(&g).unwrap();
        let back: GapSeverity = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(g, back);
    }

    #[test]
    fn serde_roundtrip_verification_category(c in arb_verification_category()) {
        let json = serde_json::to_string(&c).unwrap();
        let back: VerificationCategory = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(c, back);
    }

    #[test]
    fn serde_roundtrip_pack_verdict(v in arb_pack_verdict()) {
        let json = serde_json::to_string(&v).unwrap();
        let back: PackVerdict = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(v, back);
    }
}

// =============================================================================
// Serde roundtrip — CapabilityEntry (field-by-field, no PartialEq)
// =============================================================================

proptest! {
    #[test]
    fn serde_roundtrip_capability_entry(e in arb_capability_entry()) {
        let json = serde_json::to_string(&e).unwrap();
        let back: CapabilityEntry = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&e.capability_id, &back.capability_id);
        prop_assert_eq!(&e.capability_name, &back.capability_name);
        prop_assert_eq!(&e.status, &back.status);
        prop_assert_eq!(&e.gap_severity, &back.gap_severity);
        prop_assert_eq!(e.mapped_bead_ids.len(), back.mapped_bead_ids.len());
        prop_assert_eq!(e.surfaces.len(), back.surfaces.len());
        prop_assert_eq!(e.implementation_anchors.len(), back.implementation_anchors.len());
    }
}

// =============================================================================
// CapabilityEntry.domain() determinism
// =============================================================================

proptest! {
    #[test]
    fn domain_deterministic_from_prefix(e in arb_capability_entry()) {
        let domain = e.domain();
        if e.capability_id.starts_with("ft.") {
            prop_assert_eq!(domain, CapabilityDomain::FrankenTerm);
        } else if e.capability_id.starts_with("ntm.") {
            prop_assert_eq!(domain, CapabilityDomain::Ntm);
        } else {
            prop_assert_eq!(domain, CapabilityDomain::Fcp);
        }
    }

    #[test]
    fn domain_label_nonempty(d in arb_capability_domain()) {
        prop_assert!(!d.label().is_empty());
    }

    #[test]
    fn domain_label_matches_variant(d in arb_capability_domain()) {
        let label = d.label();
        match d {
            CapabilityDomain::FrankenTerm => prop_assert_eq!(label, "ft"),
            CapabilityDomain::Ntm => prop_assert_eq!(label, "ntm"),
            CapabilityDomain::Fcp => prop_assert_eq!(label, "fcp"),
        }
    }
}

// =============================================================================
// parsed_status determinism
// =============================================================================

proptest! {
    #[test]
    fn parsed_status_deterministic(status in arb_status_string()) {
        let e = CapabilityEntry {
            capability_id: "ft.x".to_string(),
            capability_name: "test".to_string(),
            source_domain: "ft".to_string(),
            status: status.clone(),
            gap_severity: "none".to_string(),
            mapped_bead_ids: vec![],
            surfaces: vec!["s".to_string()],
            implementation_anchors: vec!["a.rs".to_string()],
            evidence_notes: String::new(),
        };
        let parsed = e.parsed_status();
        match status.as_str() {
            "implemented" => prop_assert_eq!(parsed, ImplementationStatus::Implemented),
            "partial" => prop_assert_eq!(parsed, ImplementationStatus::Partial),
            _ => prop_assert_eq!(parsed, ImplementationStatus::Gap),
        }
    }
}

// =============================================================================
// parsed_gap_severity determinism
// =============================================================================

proptest! {
    #[test]
    fn parsed_gap_severity_deterministic(sev in arb_severity_string()) {
        let e = CapabilityEntry {
            capability_id: "ft.x".to_string(),
            capability_name: "test".to_string(),
            source_domain: "ft".to_string(),
            status: "partial".to_string(),
            gap_severity: sev.clone(),
            mapped_bead_ids: vec![],
            surfaces: vec!["s".to_string()],
            implementation_anchors: vec!["a.rs".to_string()],
            evidence_notes: String::new(),
        };
        let parsed = e.parsed_gap_severity();
        match sev.as_str() {
            "none" => prop_assert_eq!(parsed, GapSeverity::None),
            "low" => prop_assert_eq!(parsed, GapSeverity::Low),
            "medium" => prop_assert_eq!(parsed, GapSeverity::Medium),
            _ => prop_assert_eq!(parsed, GapSeverity::High),
        }
    }
}

// =============================================================================
// GapSeverity ordering properties
// =============================================================================

proptest! {
    #[test]
    fn gap_severity_total_order(a in arb_gap_severity(), b in arb_gap_severity()) {
        // Antisymmetry: if a <= b and b <= a then a == b
        if a <= b && b <= a {
            prop_assert_eq!(a, b);
        }
    }

    #[test]
    fn gap_severity_none_is_minimum(g in arb_gap_severity()) {
        prop_assert!(GapSeverity::None <= g, "None must be minimum severity");
    }

    #[test]
    fn gap_severity_high_is_maximum(g in arb_gap_severity()) {
        prop_assert!(g <= GapSeverity::High, "High must be maximum severity");
    }
}

// =============================================================================
// DomainCoverage invariants (via VerificationPack public API)
// =============================================================================

proptest! {
    #[test]
    fn domain_coverage_status_partition(matrix in arb_traceability_matrix()) {
        let pack = VerificationPack::from_matrix("cov-test", 0, matrix);
        for cov in &pack.domain_coverage {
            prop_assert_eq!(cov.implemented + cov.partial + cov.gap, cov.total,
                "implemented + partial + gap must equal total for domain '{}'", cov.domain);
        }
    }

    #[test]
    fn domain_coverage_pct_bounds(matrix in arb_traceability_matrix()) {
        let pack = VerificationPack::from_matrix("cov-test", 0, matrix);
        for cov in &pack.domain_coverage {
            prop_assert!(cov.coverage_pct >= 0.0,
                "coverage_pct must be >= 0 for domain '{}'", cov.domain);
            prop_assert!(cov.coverage_pct <= 100.0,
                "coverage_pct must be <= 100 for domain '{}'", cov.domain);
        }
    }

    #[test]
    fn domain_coverage_all_implemented_is_100(n in 1..10usize) {
        let entries: Vec<CapabilityEntry> = (0..n)
            .map(|i| CapabilityEntry {
                capability_id: format!("ft.item{i}"),
                capability_name: format!("Item {i}"),
                source_domain: "ft".to_string(),
                status: "implemented".to_string(),
                gap_severity: "none".to_string(),
                mapped_bead_ids: vec![],
                surfaces: vec!["s".to_string()],
                implementation_anchors: vec!["a.rs".to_string()],
                evidence_notes: String::new(),
            })
            .collect();
        let matrix = TraceabilityMatrix {
            schema_version: "1.0.0".to_string(),
            artifact: "ntm-fcp-traceability-matrix".to_string(),
            bead_id: "ft-test".to_string(),
            generated_at_utc: "2026-01-01T00:00:00Z".to_string(),
            required_capability_ids: vec![],
            entries,
        };
        let pack = VerificationPack::from_matrix("impl-test", 0, matrix);
        let ft_cov = pack.domain_coverage.iter().find(|d| d.domain == "ft");
        if let Some(cov) = ft_cov {
            prop_assert!((cov.coverage_pct - 100.0).abs() < 0.001,
                "all-implemented must be 100%, got {}", cov.coverage_pct);
        }
    }

    #[test]
    fn domain_coverage_all_gap_is_0(n in 1..10usize) {
        let entries: Vec<CapabilityEntry> = (0..n)
            .map(|i| CapabilityEntry {
                capability_id: format!("ft.gap{i}"),
                capability_name: format!("Gap {i}"),
                source_domain: "ft".to_string(),
                status: "gap".to_string(),
                gap_severity: "high".to_string(),
                mapped_bead_ids: vec!["ft-x".to_string()],
                surfaces: vec!["s".to_string()],
                implementation_anchors: vec!["a.rs".to_string()],
                evidence_notes: String::new(),
            })
            .collect();
        let matrix = TraceabilityMatrix {
            schema_version: "1.0.0".to_string(),
            artifact: "ntm-fcp-traceability-matrix".to_string(),
            bead_id: "ft-test".to_string(),
            generated_at_utc: "2026-01-01T00:00:00Z".to_string(),
            required_capability_ids: vec![],
            entries,
        };
        let pack = VerificationPack::from_matrix("gap-test", 0, matrix);
        let ft_cov = pack.domain_coverage.iter().find(|d| d.domain == "ft");
        if let Some(cov) = ft_cov {
            prop_assert!((cov.coverage_pct).abs() < 0.001,
                "all-gap must be 0%, got {}", cov.coverage_pct);
        }
    }
}

// =============================================================================
// GapRiskAssessment properties (via VerificationPack public API)
// =============================================================================

proptest! {
    #[test]
    fn gap_risk_none_severity_excluded(n in 1..5usize) {
        // All entries have gap_severity "none" → no gap risks should be generated
        let entries: Vec<CapabilityEntry> = (0..n)
            .map(|i| CapabilityEntry {
                capability_id: format!("ft.safe{i}"),
                capability_name: format!("Safe {i}"),
                source_domain: "ft".to_string(),
                status: "implemented".to_string(),
                gap_severity: "none".to_string(),
                mapped_bead_ids: vec![],
                surfaces: vec!["s".to_string()],
                implementation_anchors: vec!["a.rs".to_string()],
                evidence_notes: String::new(),
            })
            .collect();
        let matrix = TraceabilityMatrix {
            schema_version: "1.0.0".to_string(),
            artifact: "ntm-fcp-traceability-matrix".to_string(),
            bead_id: "ft-test".to_string(),
            generated_at_utc: "2026-01-01T00:00:00Z".to_string(),
            required_capability_ids: vec![],
            entries,
        };
        let pack = VerificationPack::from_matrix("norisk", 0, matrix);
        prop_assert!(pack.gap_risks.is_empty(),
            "entries with none severity should produce no gap risks");
    }

    #[test]
    fn gap_risk_score_bounded(matrix in arb_traceability_matrix()) {
        let pack = VerificationPack::from_matrix("bound-test", 0, matrix);
        for risk in &pack.gap_risks {
            prop_assert!(risk.risk_score >= 0.0, "risk_score must be >= 0.0");
            prop_assert!(risk.risk_score <= 1.0, "risk_score must be <= 1.0");
        }
    }

    #[test]
    fn gap_risk_remediation_flag_matches_beads(matrix in arb_traceability_matrix()) {
        let pack = VerificationPack::from_matrix("rem-test", 0, matrix);
        for risk in &pack.gap_risks {
            if risk.mapped_beads > 0 {
                prop_assert!(risk.has_remediation_path,
                    "has_remediation_path must be true when mapped_beads > 0");
            } else {
                prop_assert!(!risk.has_remediation_path,
                    "has_remediation_path must be false when mapped_beads == 0");
            }
        }
    }
}

// =============================================================================
// VerificationPack structural properties
// =============================================================================

proptest! {
    #[test]
    fn pack_checks_nonempty(matrix in arb_traceability_matrix()) {
        let pack = VerificationPack::from_matrix("test", 0, matrix);
        prop_assert!(!pack.checks.is_empty(),
            "verification pack must produce at least one check");
    }

    #[test]
    fn pack_checks_by_category_partitions_all(matrix in arb_traceability_matrix()) {
        let pack = VerificationPack::from_matrix("test", 0, matrix);
        let by_cat = pack.checks_by_category();
        let total_in_groups: usize = by_cat.values().map(|v| v.len()).sum();
        prop_assert_eq!(total_in_groups, pack.checks.len(),
            "checks_by_category must partition all checks");
    }

    #[test]
    fn pack_failing_checks_subset(matrix in arb_traceability_matrix()) {
        let pack = VerificationPack::from_matrix("test", 0, matrix);
        let failing = pack.failing_checks();
        for check in &failing {
            prop_assert!(!check.passed, "failing_checks must only return failed checks");
        }
        let expected_fail_count = pack.checks.iter().filter(|c| !c.passed).count();
        prop_assert_eq!(failing.len(), expected_fail_count);
    }

    #[test]
    fn pack_all_checks_pass_consistent(matrix in arb_traceability_matrix()) {
        let pack = VerificationPack::from_matrix("test", 0, matrix);
        let actual_all_pass = pack.checks.iter().all(|c| c.passed);
        prop_assert_eq!(pack.all_checks_pass, actual_all_pass,
            "all_checks_pass must match actual check results");
    }

    #[test]
    fn pack_required_met_bounded(matrix in arb_traceability_matrix()) {
        let pack = VerificationPack::from_matrix("test", 0, matrix);
        prop_assert!(pack.required_met <= pack.required_total,
            "required_met must be <= required_total");
    }

    #[test]
    fn pack_summary_nonempty(matrix in arb_traceability_matrix()) {
        let pack = VerificationPack::from_matrix("test", 0, matrix);
        let summary = pack.summary();
        prop_assert!(!summary.is_empty(), "summary must be non-empty");
        prop_assert!(summary.contains("test"), "summary must contain pack_id");
    }

    #[test]
    fn pack_verdict_incomplete_when_checks_fail(matrix in arb_traceability_matrix()) {
        let pack = VerificationPack::from_matrix("test", 0, matrix);
        if !pack.all_checks_pass {
            prop_assert_eq!(pack.pack_verdict, PackVerdict::Incomplete,
                "failed checks must yield Incomplete verdict");
        }
    }

    #[test]
    fn pack_total_capabilities_matches_entries(matrix in arb_traceability_matrix()) {
        let entry_count = matrix.entries.len();
        let pack = VerificationPack::from_matrix("test", 0, matrix);
        prop_assert_eq!(pack.total_capabilities, entry_count,
            "total_capabilities must match input entry count");
    }

    #[test]
    fn pack_domain_coverage_covers_all_domains(matrix in arb_traceability_matrix()) {
        let pack = VerificationPack::from_matrix("test", 0, matrix);
        // Sum of domain coverage totals should equal total entries
        let cov_total: usize = pack.domain_coverage.iter().map(|d| d.total).sum();
        prop_assert_eq!(cov_total, pack.total_capabilities,
            "sum of domain coverage totals must equal total_capabilities");
    }

    #[test]
    fn pack_gap_risks_only_for_non_none_severity(matrix in arb_traceability_matrix()) {
        let pack = VerificationPack::from_matrix("test", 0, matrix);
        for risk in &pack.gap_risks {
            prop_assert!(risk.gap_severity != "none",
                "gap_risks should not contain entries with none severity");
        }
    }
}

// =============================================================================
// Serde roundtrip — TraceabilityMatrix (field-by-field)
// =============================================================================

proptest! {
    #[test]
    fn serde_roundtrip_traceability_matrix(matrix in arb_traceability_matrix()) {
        let json = serde_json::to_string(&matrix).unwrap();
        let back: TraceabilityMatrix = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&matrix.schema_version, &back.schema_version);
        prop_assert_eq!(&matrix.artifact, &back.artifact);
        prop_assert_eq!(&matrix.bead_id, &back.bead_id);
        prop_assert_eq!(matrix.entries.len(), back.entries.len());
        prop_assert_eq!(matrix.required_capability_ids.len(), back.required_capability_ids.len());
    }
}
