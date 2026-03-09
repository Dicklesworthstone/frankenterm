//! Property-based tests for connector_sdk devkit module.
//!
//! Coverage targets:
//! - ManifestBuilder roundtrip and validation invariants
//! - TrustPolicyBuilder consistency guarantees
//! - LintFinding/LintReport serde roundtrip
//! - CertificationReport serde roundtrip
//! - Linter idempotency (same manifest → same report)
//! - Certification pipeline determinism
//! - SimulationEvent serde roundtrip
//! - SHA-256 digest determinism and uniqueness
//!
//! ft-3681t.5.10 quality support slice.

use frankenterm_core::connector_host_runtime::{ConnectorCapability, ConnectorHostConfig};
use frankenterm_core::connector_registry::TrustLevel;
use frankenterm_core::connector_sdk::{
    CertificationPipeline, CertificationPhase, CertificationReport, CertificationTelemetry,
    CertificationTelemetrySnapshot, CertificationVerdict, ConnectorSimulator, LintFinding,
    LintReport, LintRule, LintSeverity, ManifestBuilder, ManifestLinter, PhaseResult,
    PhaseVerdict, SimulationEvent, SimulationEventType, TrustPolicyBuilder, compute_sha256_hex,
};
use proptest::prelude::*;

// ---------------------------------------------------------------------------
// Strategies
// ---------------------------------------------------------------------------

fn arb_package_id() -> impl Strategy<Value = String> {
    "[a-z][a-z0-9\\-]{0,30}".prop_filter("non-empty", |s| !s.is_empty())
}

fn arb_semver() -> impl Strategy<Value = String> {
    (0u32..100, 0u32..100, 0u32..100).prop_map(|(a, b, c)| format!("{a}.{b}.{c}"))
}

fn arb_capability() -> impl Strategy<Value = ConnectorCapability> {
    prop_oneof![
        Just(ConnectorCapability::Invoke),
        Just(ConnectorCapability::ReadState),
        Just(ConnectorCapability::StreamEvents),
        Just(ConnectorCapability::FilesystemRead),
        Just(ConnectorCapability::FilesystemWrite),
        Just(ConnectorCapability::NetworkEgress),
        Just(ConnectorCapability::SecretBroker),
        Just(ConnectorCapability::ProcessExec),
    ]
}

fn arb_capabilities() -> impl Strategy<Value = Vec<ConnectorCapability>> {
    proptest::collection::vec(arb_capability(), 0..5).prop_map(|mut caps| {
        caps.sort_by_key(|c| format!("{c:?}"));
        caps.dedup();
        caps
    })
}

fn arb_lint_severity() -> impl Strategy<Value = LintSeverity> {
    prop_oneof![
        Just(LintSeverity::Info),
        Just(LintSeverity::Warning),
        Just(LintSeverity::Error),
    ]
}

fn arb_trust_level() -> impl Strategy<Value = TrustLevel> {
    prop_oneof![
        Just(TrustLevel::Blocked),
        Just(TrustLevel::Untrusted),
        Just(TrustLevel::Conditional),
        Just(TrustLevel::Trusted),
    ]
}

fn arb_lint_finding() -> impl Strategy<Value = LintFinding> {
    (
        "[a-z\\.]{1,30}",
        arb_lint_severity(),
        "[a-zA-Z0-9 ]{1,50}",
        proptest::option::of("[a-zA-Z0-9 ]{1,50}"),
    )
        .prop_map(|(rule_id, severity, message, remediation)| LintFinding {
            rule_id,
            severity,
            message,
            remediation,
        })
}

fn arb_lint_report() -> impl Strategy<Value = LintReport> {
    (
        arb_package_id(),
        proptest::collection::vec(arb_lint_finding(), 0..10),
    )
        .prop_map(|(package_id, findings)| {
            let error_count = findings
                .iter()
                .filter(|f| f.severity == LintSeverity::Error)
                .count();
            let warning_count = findings
                .iter()
                .filter(|f| f.severity == LintSeverity::Warning)
                .count();
            let info_count = findings
                .iter()
                .filter(|f| f.severity == LintSeverity::Info)
                .count();
            LintReport {
                package_id,
                findings,
                error_count,
                warning_count,
                info_count,
            }
        })
}

fn arb_simulation_event() -> impl Strategy<Value = SimulationEvent> {
    (
        arb_package_id(),
        0u64..1_000_000,
        prop_oneof![
            Just(SimulationEventType::Registered),
            Just(SimulationEventType::Started),
            Just(SimulationEventType::Stopped),
            Just(SimulationEventType::Heartbeat),
            Just(SimulationEventType::OperationDenied),
            Just(SimulationEventType::CertificationRun),
            Just(SimulationEventType::FailureRecorded),
            Just(SimulationEventType::Restarted),
        ],
    )
        .prop_map(|(connector_id, timestamp_ms, event_type)| SimulationEvent {
            connector_id,
            timestamp_ms,
            event_type,
            detail: String::new(),
        })
}

// ---------------------------------------------------------------------------
// ManifestBuilder property tests
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// A builder with valid package_id + version always produces a valid manifest.
    #[test]
    fn manifest_builder_valid_inputs_always_succeed(
        id in arb_package_id(),
        ver in arb_semver(),
        caps in arb_capabilities(),
    ) {
        let payload = b"proptest-payload";
        let mut builder = ManifestBuilder::new(&id).version(&ver);
        for cap in &caps {
            builder = builder.capability(*cap);
        }
        let result = builder.build_with_digest(payload);
        prop_assert!(result.is_ok(), "builder should succeed for valid inputs");
        let manifest = result.unwrap();
        prop_assert_eq!(manifest.package_id, id);
        prop_assert_eq!(manifest.version, ver);
        // Capabilities should be deduplicated
        let unique_caps: std::collections::HashSet<_> = caps.iter().collect();
        prop_assert_eq!(manifest.required_capabilities.len(), unique_caps.len());
    }

    /// Builder without version always fails.
    #[test]
    fn manifest_builder_missing_version_always_fails(id in arb_package_id()) {
        let payload = b"any-payload";
        let result = ManifestBuilder::new(id).build_with_digest(payload);
        prop_assert!(result.is_err());
    }

    /// Builder with empty package_id always fails.
    #[test]
    fn manifest_builder_empty_id_always_fails(ver in arb_semver()) {
        let payload = b"any-payload";
        let result = ManifestBuilder::new("").version(ver).build_with_digest(payload);
        prop_assert!(result.is_err());
    }

    /// Same payload → same digest in the manifest.
    #[test]
    fn manifest_builder_digest_deterministic(
        id in arb_package_id(),
        ver in arb_semver(),
        payload in proptest::collection::vec(any::<u8>(), 1..256),
    ) {
        let m1 = ManifestBuilder::new(&id)
            .version(&ver)
            .build_with_digest(&payload)
            .unwrap();
        let m2 = ManifestBuilder::new(&id)
            .version(&ver)
            .build_with_digest(&payload)
            .unwrap();
        prop_assert_eq!(m1.sha256_digest, m2.sha256_digest);
    }

    /// Precomputed digest is forwarded verbatim.
    #[test]
    fn manifest_builder_precomputed_digest_passthrough(
        id in arb_package_id(),
        ver in arb_semver(),
        digest in "[0-9a-f]{64}",
    ) {
        let m = ManifestBuilder::new(&id)
            .version(&ver)
            .build_with_precomputed_digest(&digest)
            .unwrap();
        prop_assert_eq!(m.sha256_digest, digest);
    }

    /// display_name defaults to package_id when not set.
    #[test]
    fn manifest_builder_display_name_defaults(
        id in arb_package_id(),
        ver in arb_semver(),
    ) {
        let m = ManifestBuilder::new(&id)
            .version(&ver)
            .build_with_digest(b"x")
            .unwrap();
        prop_assert_eq!(m.display_name, id);
    }

    /// Explicit display_name overrides default.
    #[test]
    fn manifest_builder_explicit_display_name(
        id in arb_package_id(),
        ver in arb_semver(),
        name in "[A-Z][a-z]{2,20}",
    ) {
        let m = ManifestBuilder::new(&id)
            .version(&ver)
            .display_name(&name)
            .build_with_digest(b"x")
            .unwrap();
        prop_assert_eq!(m.display_name, name);
    }
}

// ---------------------------------------------------------------------------
// TrustPolicyBuilder property tests
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// Builder always produces a policy with the specified trust level.
    #[test]
    fn trust_policy_builder_preserves_level(level in arb_trust_level()) {
        let policy = TrustPolicyBuilder::new().min_install_level(level).build();
        prop_assert_eq!(policy.min_install_level, level);
    }

    /// Capabilities added to the policy are preserved.
    #[test]
    fn trust_policy_builder_preserves_capabilities(caps in arb_capabilities()) {
        let policy = TrustPolicyBuilder::new().allow_capabilities(&caps).build();
        for cap in &caps {
            prop_assert!(
                policy.max_allowed_capabilities.contains(cap),
                "capability {:?} should be in policy",
                cap
            );
        }
    }

    /// Strict builder always requires signature and high trust.
    #[test]
    fn trust_policy_strict_invariants(_seed in 0u32..1000) {
        let policy = TrustPolicyBuilder::strict().build();
        prop_assert!(policy.require_signature);
        prop_assert!(policy.require_transparency_proof);
        prop_assert_eq!(policy.min_install_level, TrustLevel::Trusted);
    }
}

// ---------------------------------------------------------------------------
// Serde roundtrip tests
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// LintFinding serde roundtrip is lossless.
    #[test]
    fn lint_finding_serde_roundtrip(finding in arb_lint_finding()) {
        let json = serde_json::to_string(&finding).unwrap();
        let decoded: LintFinding = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(finding, decoded);
    }

    /// LintReport serde roundtrip is lossless.
    #[test]
    fn lint_report_serde_roundtrip(report in arb_lint_report()) {
        let json = serde_json::to_string(&report).unwrap();
        let decoded: LintReport = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(report, decoded);
    }

    /// SimulationEvent serde roundtrip is lossless.
    #[test]
    fn simulation_event_serde_roundtrip(event in arb_simulation_event()) {
        let json = serde_json::to_string(&event).unwrap();
        let decoded: SimulationEvent = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(event, decoded);
    }
}

// ---------------------------------------------------------------------------
// Linter idempotency and consistency
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// Linting the same manifest twice yields identical reports.
    #[test]
    fn linter_idempotent(
        id in arb_package_id(),
        ver in arb_semver(),
        caps in arb_capabilities(),
    ) {
        let payload = b"lint-payload";
        let mut builder = ManifestBuilder::new(&id).version(&ver).publisher_signature("sig");
        for cap in &caps {
            builder = builder.capability(*cap);
        }
        let manifest = builder.build_with_digest(payload).unwrap();

        let mut linter = ManifestLinter::new();
        let r1 = linter.lint(&manifest);
        let r2 = linter.lint(&manifest);
        prop_assert_eq!(r1.error_count, r2.error_count);
        prop_assert_eq!(r1.warning_count, r2.warning_count);
        prop_assert_eq!(r1.info_count, r2.info_count);
        prop_assert_eq!(r1.findings.len(), r2.findings.len());
    }

    /// Linter report counts are consistent with findings.
    #[test]
    fn linter_counts_consistent(
        id in arb_package_id(),
        ver in arb_semver(),
    ) {
        let payload = b"count-payload";
        let manifest = ManifestBuilder::new(&id)
            .version(&ver)
            .publisher_signature("sig")
            .build_with_digest(payload)
            .unwrap();

        let mut linter = ManifestLinter::new();
        let report = linter.lint(&manifest);

        let actual_errors = report.findings.iter().filter(|f| f.severity == LintSeverity::Error).count();
        let actual_warnings = report.findings.iter().filter(|f| f.severity == LintSeverity::Warning).count();
        let actual_info = report.findings.iter().filter(|f| f.severity == LintSeverity::Info).count();

        prop_assert_eq!(report.error_count, actual_errors);
        prop_assert_eq!(report.warning_count, actual_warnings);
        prop_assert_eq!(report.info_count, actual_info);
    }

    /// Linter.passed() ↔ error_count == 0 invariant.
    #[test]
    fn linter_passed_iff_no_errors(
        id in arb_package_id(),
        ver in arb_semver(),
    ) {
        let payload = b"passed-payload";
        let manifest = ManifestBuilder::new(&id)
            .version(&ver)
            .publisher_signature("sig")
            .build_with_digest(payload)
            .unwrap();

        let mut linter = ManifestLinter::new();
        let report = linter.lint(&manifest);
        prop_assert_eq!(report.passed(), report.error_count == 0);
    }

    /// Linter history is bounded.
    #[test]
    fn linter_history_bounded(
        ids in proptest::collection::vec(arb_package_id(), 1..20),
    ) {
        let mut linter = ManifestLinter::new();
        for id in &ids {
            let manifest = ManifestBuilder::new(id)
                .version("1.0.0")
                .publisher_signature("sig")
                .build_with_digest(b"x")
                .unwrap();
            linter.lint(&manifest);
        }
        prop_assert!(linter.history().len() <= 256);
    }
}

// ---------------------------------------------------------------------------
// Certification pipeline determinism
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    /// Certifying the same (manifest, payload) twice yields same verdict.
    #[test]
    fn certification_deterministic(
        id in arb_package_id(),
        ver in arb_semver(),
        caps in arb_capabilities(),
    ) {
        let payload = b"cert-payload";
        let policy = TrustPolicyBuilder::new().allow_capabilities(&caps).build();
        let mut pipeline = CertificationPipeline::new(policy.clone());

        let mut builder = ManifestBuilder::new(&id).version(&ver).publisher_signature("sig");
        for cap in &caps {
            builder = builder.capability(*cap);
        }
        let manifest = builder.build_with_digest(payload).unwrap();

        let r1 = pipeline.certify(&manifest, payload);
        let r2 = pipeline.certify(&manifest, payload);
        prop_assert_eq!(r1.verdict, r2.verdict);
        prop_assert_eq!(r1.phases.len(), r2.phases.len());
    }

    /// CertificationReport serde roundtrip is lossless.
    #[test]
    fn certification_report_serde_roundtrip(
        id in arb_package_id(),
        ver in arb_semver(),
    ) {
        let payload = b"serde-payload";
        let policy = TrustPolicyBuilder::new().build();
        let mut pipeline = CertificationPipeline::new(policy);
        let manifest = ManifestBuilder::new(&id)
            .version(&ver)
            .publisher_signature("sig")
            .build_with_digest(payload)
            .unwrap();

        let report = pipeline.certify(&manifest, payload);
        let json = serde_json::to_string(&report).unwrap();
        let decoded: CertificationReport = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(report.verdict, decoded.verdict);
        prop_assert_eq!(report.phases.len(), decoded.phases.len());
    }

    /// Certification history is bounded.
    #[test]
    fn certification_history_bounded(
        ids in proptest::collection::vec(arb_package_id(), 1..20),
    ) {
        let policy = TrustPolicyBuilder::new().build();
        let mut pipeline = CertificationPipeline::new(policy);
        for id in &ids {
            let manifest = ManifestBuilder::new(id)
                .version("1.0.0")
                .publisher_signature("sig")
                .build_with_digest(b"x")
                .unwrap();
            pipeline.certify(&manifest, b"x");
        }
        prop_assert!(pipeline.history().len() <= 128);
    }
}

// ---------------------------------------------------------------------------
// SHA-256 utility property tests
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// compute_sha256_hex is deterministic.
    #[test]
    fn sha256_deterministic(data in proptest::collection::vec(any::<u8>(), 0..512)) {
        let d1 = compute_sha256_hex(&data);
        let d2 = compute_sha256_hex(&data);
        prop_assert_eq!(d1, d2);
    }

    /// SHA-256 output is always 64 hex characters.
    #[test]
    fn sha256_always_64_hex(data in proptest::collection::vec(any::<u8>(), 0..512)) {
        let digest = compute_sha256_hex(&data);
        prop_assert_eq!(digest.len(), 64);
        prop_assert!(digest.chars().all(|c| c.is_ascii_hexdigit()));
    }

    /// Different inputs produce different digests (probabilistic).
    #[test]
    fn sha256_collision_resistant(
        a in proptest::collection::vec(any::<u8>(), 1..256),
        b in proptest::collection::vec(any::<u8>(), 1..256),
    ) {
        prop_assume!(a != b);
        let da = compute_sha256_hex(&a);
        let db = compute_sha256_hex(&b);
        prop_assert_ne!(da, db);
    }
}

// ---------------------------------------------------------------------------
// Simulator property tests
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    /// Simulator clock always advances monotonically.
    #[test]
    fn simulator_clock_monotonic(ticks in proptest::collection::vec(1u64..10_000, 1..20)) {
        let policy = TrustPolicyBuilder::new().build();
        let mut sim = ConnectorSimulator::new(policy);
        let mut prev = sim.now();
        for t in ticks {
            sim.tick(t);
            prop_assert!(sim.now() > prev, "clock must advance");
            prev = sim.now();
        }
    }

    /// Connector count matches number of successfully registered connectors.
    #[test]
    fn simulator_count_tracks_registrations(
        ids in proptest::collection::vec(arb_package_id(), 1..5),
    ) {
        let policy = TrustPolicyBuilder::new().build();
        let mut sim = ConnectorSimulator::new(policy);
        let mut registered = 0;
        for id in &ids {
            let manifest = ManifestBuilder::new(id)
                .version("1.0.0")
                .publisher_signature("sig")
                .build_with_digest(b"payload")
                .unwrap();
            let config = ConnectorHostConfig::default();
            if let Ok(report) = sim.register(&manifest, b"payload", config) {
                if report.passed() {
                    registered += 1;
                }
            }
        }
        prop_assert_eq!(sim.connector_count(), registered);
    }
}

// ---------------------------------------------------------------------------
// Additional strategies for coverage gaps
// ---------------------------------------------------------------------------

fn arb_certification_phase() -> impl Strategy<Value = CertificationPhase> {
    prop_oneof![
        Just(CertificationPhase::SchemaValidation),
        Just(CertificationPhase::LintCheck),
        Just(CertificationPhase::DigestVerification),
        Just(CertificationPhase::CapabilityAudit),
        Just(CertificationPhase::TrustPolicyGate),
        Just(CertificationPhase::IntegrationProbe),
    ]
}

fn arb_certification_verdict() -> impl Strategy<Value = CertificationVerdict> {
    prop_oneof![
        Just(CertificationVerdict::Certified),
        Just(CertificationVerdict::ConditionalPass),
        Just(CertificationVerdict::Rejected),
    ]
}

fn arb_simulation_event_type() -> impl Strategy<Value = SimulationEventType> {
    prop_oneof![
        Just(SimulationEventType::Registered),
        Just(SimulationEventType::Started),
        Just(SimulationEventType::Stopped),
        Just(SimulationEventType::Heartbeat),
        Just(SimulationEventType::UsageChecked),
        Just(SimulationEventType::OperationAuthorized),
        Just(SimulationEventType::OperationDenied),
        Just(SimulationEventType::CertificationRun),
        Just(SimulationEventType::FailureRecorded),
        Just(SimulationEventType::Restarted),
    ]
}

fn arb_phase_verdict() -> impl Strategy<Value = PhaseVerdict> {
    prop_oneof![
        Just(PhaseVerdict::Passed),
        proptest::collection::vec("[a-z ]{3,20}", 1..4)
            .prop_map(|warnings| PhaseVerdict::PassedWithWarnings { warnings }),
        proptest::collection::vec("[a-z ]{3,20}", 1..4)
            .prop_map(|reasons| PhaseVerdict::Failed { reasons }),
        "[a-z ]{3,20}".prop_map(|reason| PhaseVerdict::Skipped { reason }),
    ]
}

// ---------------------------------------------------------------------------
// CertificationPhase — serde + as_str + Display
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn certification_phase_serde_and_as_str(phase in arb_certification_phase()) {
        let json = serde_json::to_string(&phase).unwrap();
        let back: CertificationPhase = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back, phase);

        let expected_json = format!("\"{}\"", phase.as_str());
        prop_assert_eq!(json, expected_json);

        // Display matches as_str
        let display = phase.to_string();
        prop_assert_eq!(display.as_str(), phase.as_str());
    }
}

// ---------------------------------------------------------------------------
// CertificationVerdict — serde + as_str + Display
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    #[test]
    fn certification_verdict_serde_and_as_str(verdict in arb_certification_verdict()) {
        let json = serde_json::to_string(&verdict).unwrap();
        let back: CertificationVerdict = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back, verdict);

        let expected_json = format!("\"{}\"", verdict.as_str());
        prop_assert_eq!(json, expected_json);

        let display = verdict.to_string();
        prop_assert_eq!(display.as_str(), verdict.as_str());
    }
}

// ---------------------------------------------------------------------------
// PhaseVerdict — serde roundtrip + is_pass
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn phase_verdict_serde_and_is_pass(verdict in arb_phase_verdict()) {
        let expected_pass = matches!(verdict, PhaseVerdict::Passed | PhaseVerdict::PassedWithWarnings { .. });
        let is_pass = verdict.is_pass();
        let json = serde_json::to_string(&verdict).unwrap();
        let back: PhaseVerdict = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back, verdict);

        // is_pass matches Passed/PassedWithWarnings
        prop_assert_eq!(is_pass, expected_pass);
    }
}

// ---------------------------------------------------------------------------
// PhaseResult — serde roundtrip
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn phase_result_serde(
        phase in arb_certification_phase(),
        verdict in arb_phase_verdict(),
        elapsed_ms in 0_u64..10_000,
    ) {
        let result = PhaseResult {
            phase,
            verdict: verdict.clone(),
            elapsed_ms,
        };
        let json = serde_json::to_string(&result).unwrap();
        let back: PhaseResult = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.phase, result.phase);
        prop_assert_eq!(back.verdict, result.verdict);
        prop_assert_eq!(back.elapsed_ms, result.elapsed_ms);
    }
}

// ---------------------------------------------------------------------------
// LintRule — serde roundtrip
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn lint_rule_serde(
        rule_id in "[a-z\\.]{3,20}",
        severity in arb_lint_severity(),
        description in "[a-zA-Z0-9 ]{5,40}",
    ) {
        let rule = LintRule {
            rule_id: rule_id.clone(),
            severity,
            description: description.clone(),
        };
        let json = serde_json::to_string(&rule).unwrap();
        let back: LintRule = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&back.rule_id, &rule.rule_id);
        prop_assert_eq!(back.severity, rule.severity);
        prop_assert_eq!(&back.description, &rule.description);
    }
}

// ---------------------------------------------------------------------------
// LintSeverity — as_str + Display + ordering
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    #[test]
    fn lint_severity_as_str_and_display(severity in arb_lint_severity()) {
        let as_str = severity.as_str();
        let display = severity.to_string();
        prop_assert_eq!(as_str, display.as_str());

        let json = serde_json::to_string(&severity).unwrap();
        let expected = format!("\"{as_str}\"");
        prop_assert_eq!(json, expected);
    }

    #[test]
    fn lint_severity_ordering(_dummy in 0..1_u32) {
        // Info < Warning < Error
        prop_assert!(LintSeverity::Info < LintSeverity::Warning);
        prop_assert!(LintSeverity::Warning < LintSeverity::Error);
    }
}

// ---------------------------------------------------------------------------
// SimulationEventType — all 10 variants serde
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn simulation_event_type_serde_all_variants(evt in arb_simulation_event_type()) {
        let json = serde_json::to_string(&evt).unwrap();
        let back: SimulationEventType = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back, evt);
        // snake_case encoding
        prop_assert!(json.starts_with('"') && json.ends_with('"'));
    }
}

// ---------------------------------------------------------------------------
// CertificationTelemetry — serde + Default
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn certification_telemetry_serde(
        total in 0_u64..1000,
        certified in 0_u64..500,
        conditional in 0_u64..500,
        rejections in 0_u64..500,
        failure_keys in proptest::collection::vec("[a-z_]{3,10}", 0..5),
    ) {
        let mut phase_failures = std::collections::BTreeMap::new();
        for (i, key) in failure_keys.iter().enumerate() {
            phase_failures.insert(key.clone(), i as u64 + 1);
        }
        let telem = CertificationTelemetry {
            total_runs: total,
            certified,
            conditional_passes: conditional,
            rejections,
            phase_failures: phase_failures.clone(),
        };
        let json = serde_json::to_string(&telem).unwrap();
        let back: CertificationTelemetry = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.total_runs, telem.total_runs);
        prop_assert_eq!(back.certified, telem.certified);
        prop_assert_eq!(back.conditional_passes, telem.conditional_passes);
        prop_assert_eq!(back.rejections, telem.rejections);
        prop_assert_eq!(back.phase_failures.len(), telem.phase_failures.len());
    }

    #[test]
    fn certification_telemetry_default(_dummy in 0..1_u32) {
        let d = CertificationTelemetry::default();
        prop_assert_eq!(d.total_runs, 0);
        prop_assert_eq!(d.certified, 0);
        prop_assert_eq!(d.conditional_passes, 0);
        prop_assert_eq!(d.rejections, 0);
        prop_assert!(d.phase_failures.is_empty());
    }
}

// ---------------------------------------------------------------------------
// CertificationTelemetrySnapshot — serde roundtrip
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn certification_telemetry_snapshot_serde(
        total in 0_u64..1000,
        certified in 0_u64..500,
        conditional in 0_u64..500,
        rejections in 0_u64..500,
    ) {
        let snap = CertificationTelemetrySnapshot {
            total_runs: total,
            certified,
            conditional_passes: conditional,
            rejections,
            phase_failures: std::collections::BTreeMap::new(),
        };
        let json = serde_json::to_string(&snap).unwrap();
        let back: CertificationTelemetrySnapshot = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back, snap);
    }
}

// ---------------------------------------------------------------------------
// CertificationReport — passed() invariant
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    #[test]
    fn certification_report_passed_invariant(verdict in arb_certification_verdict()) {
        let report = CertificationReport {
            package_id: "test-pkg".to_string(),
            version: "1.0.0".to_string(),
            verdict,
            phases: vec![],
            trust_level: None,
            total_elapsed_ms: 0,
        };
        let expected_passed = matches!(verdict, CertificationVerdict::Certified | CertificationVerdict::ConditionalPass);
        prop_assert_eq!(report.passed(), expected_passed);
    }

    #[test]
    fn certification_report_display_not_empty(verdict in arb_certification_verdict()) {
        let report = CertificationReport {
            package_id: "test-pkg".to_string(),
            version: "1.0.0".to_string(),
            verdict,
            phases: vec![],
            trust_level: None,
            total_elapsed_ms: 42,
        };
        let display = report.to_string();
        prop_assert!(!display.is_empty());
        prop_assert!(display.contains("test-pkg"));
    }
}

// ---------------------------------------------------------------------------
// LintFinding Display
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn lint_finding_display_contains_rule_id(finding in arb_lint_finding()) {
        let display = finding.to_string();
        prop_assert!(display.contains(&finding.rule_id));
        prop_assert!(display.contains(&finding.message));
    }
}
