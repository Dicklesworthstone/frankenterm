//! Property tests for robot_sdk_contracts module (ft-3681t.4.4).
//!
//! Covers serde roundtrips, arithmetic invariants, SDK generation consistency,
//! NtmCompatShim readiness summary correctness, and CompatLevel ordering.

use frankenterm_core::robot_sdk_contracts::*;
use proptest::prelude::*;

// =============================================================================
// Strategies
// =============================================================================

fn arb_http_method() -> impl Strategy<Value = HttpMethod> {
    prop_oneof![
        Just(HttpMethod::Get),
        Just(HttpMethod::Post),
        Just(HttpMethod::Put),
        Just(HttpMethod::Delete),
    ]
}

fn arb_compat_level() -> impl Strategy<Value = CompatLevel> {
    prop_oneof![
        Just(CompatLevel::Full),
        Just(CompatLevel::MappedCompat),
        Just(CompatLevel::Partial),
        Just(CompatLevel::Incompatible),
        Just(CompatLevel::NoEquivalent),
    ]
}

fn arb_sdk_language() -> impl Strategy<Value = SdkLanguage> {
    prop_oneof![
        Just(SdkLanguage::Python),
        Just(SdkLanguage::TypeScript),
        Just(SdkLanguage::Rust),
        Just(SdkLanguage::Go),
    ]
}

fn arb_field_type_leaf() -> impl Strategy<Value = FieldType> {
    prop_oneof![
        Just(FieldType::String),
        Just(FieldType::Integer),
        Just(FieldType::Float),
        Just(FieldType::Boolean),
        Just(FieldType::Json),
    ]
}

fn arb_field_type() -> impl Strategy<Value = FieldType> {
    arb_field_type_leaf().prop_recursive(2, 8, 3, |inner| {
        prop_oneof![
            inner.clone().prop_map(|t| FieldType::Array(Box::new(t))),
            inner.prop_map(|t| FieldType::Optional(Box::new(t))),
        ]
    })
}

fn arb_field_spec() -> impl Strategy<Value = FieldSpec> {
    ("[a-z_]{1,20}", arb_field_type_leaf(), ".*", any::<bool>()).prop_map(
        |(name, field_type, desc, required)| {
            if required {
                FieldSpec::required(name, field_type, desc)
            } else {
                FieldSpec::optional(name, field_type, desc)
            }
        },
    )
}

fn arb_endpoint_spec() -> impl Strategy<Value = EndpointSpec> {
    (
        "[a-z][a-z-]{0,15}",
        arb_http_method(),
        ".{0,40}",
        prop::collection::vec(arb_field_spec(), 0..4),
        prop::collection::vec(arb_field_spec(), 0..4),
    )
        .prop_map(|(cmd, method, desc, req_fields, resp_fields)| {
            let mut spec = EndpointSpec::new(cmd, method, desc);
            for f in req_fields {
                spec.add_request_field(f);
            }
            for f in resp_fields {
                spec.add_response_field(f);
            }
            spec
        })
}

fn arb_ntm_compat_entry() -> impl Strategy<Value = NtmCompatEntry> {
    (
        "[a-z][a-z-]{0,15}",
        "[a-z][a-z-]{0,15}",
        arb_compat_level(),
        ".{0,30}",
    )
        .prop_map(|(ft_cmd, ntm_cmd, compat_level, notes)| NtmCompatEntry {
            ft_command: ft_cmd,
            ntm_command: ntm_cmd,
            compat_level,
            field_mappings: Vec::new(),
            ntm_only_fields: Vec::new(),
            ft_only_fields: Vec::new(),
            notes,
        })
}

// =============================================================================
// Serde roundtrip tests
// =============================================================================

proptest! {
    #[test]
    fn serde_roundtrip_http_method(method in arb_http_method()) {
        let json = serde_json::to_string(&method).unwrap();
        let back: HttpMethod = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(method, back);
    }

    #[test]
    fn serde_roundtrip_compat_level(level in arb_compat_level()) {
        let json = serde_json::to_string(&level).unwrap();
        let back: CompatLevel = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(level, back);
    }

    #[test]
    fn serde_roundtrip_sdk_language(lang in arb_sdk_language()) {
        let json = serde_json::to_string(&lang).unwrap();
        let back: SdkLanguage = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(lang, back);
    }

    #[test]
    fn serde_roundtrip_field_type(ft in arb_field_type()) {
        let json = serde_json::to_string(&ft).unwrap();
        let back: FieldType = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(ft, back);
    }

    #[test]
    fn serde_roundtrip_field_spec(spec in arb_field_spec()) {
        let json = serde_json::to_string(&spec).unwrap();
        let back: FieldSpec = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(spec, back);
    }

    #[test]
    fn serde_roundtrip_endpoint_spec(spec in arb_endpoint_spec()) {
        let json = serde_json::to_string(&spec).unwrap();
        let back: EndpointSpec = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(spec.command, back.command);
        prop_assert_eq!(spec.method, back.method);
        prop_assert_eq!(spec.is_mutation, back.is_mutation);
        prop_assert_eq!(spec.request_fields.len(), back.request_fields.len());
        prop_assert_eq!(spec.response_fields.len(), back.response_fields.len());
    }

    #[test]
    fn serde_roundtrip_compat_summary(
        total in 0..100usize,
        full in 0..50usize,
        mapped in 0..50usize,
        partial in 0..50usize,
        incompatible in 0..50usize,
        no_equiv in 0..50usize,
    ) {
        let coverage = if total > 0 { (full + mapped + partial) as f64 / total as f64 } else { 0.0 };
        let summary = CompatSummary {
            total,
            full,
            mapped,
            partial,
            incompatible,
            no_equivalent: no_equiv,
            migration_coverage: coverage,
        };
        let json = serde_json::to_string(&summary).unwrap();
        let back: CompatSummary = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(summary.total, back.total);
        prop_assert_eq!(summary.full, back.full);
        prop_assert_eq!(summary.mapped, back.mapped);
        // f64 roundtrip with tolerance
        prop_assert!((summary.migration_coverage - back.migration_coverage).abs() < 1e-10);
    }
}

// =============================================================================
// Behavioral invariants
// =============================================================================

proptest! {
    #[test]
    fn endpoint_mutation_consistency(method in arb_http_method(), cmd in "[a-z-]{3,15}") {
        let spec = EndpointSpec::new(cmd, method, "test");
        let expected_mutation = matches!(method, HttpMethod::Post | HttpMethod::Put | HttpMethod::Delete);
        prop_assert_eq!(spec.is_mutation, expected_mutation,
            "is_mutation should match method semantics: {:?}", method);
    }

    #[test]
    fn required_field_filter_accuracy(
        fields in prop::collection::vec(arb_field_spec(), 0..10)
    ) {
        let mut spec = EndpointSpec::new("test", HttpMethod::Get, "test");
        let expected_required = fields.iter().filter(|f| f.required).count();
        for f in fields {
            spec.add_request_field(f);
        }
        let actual = spec.required_request_fields();
        prop_assert_eq!(actual.len(), expected_required);
        for f in &actual {
            prop_assert!(f.required);
        }
    }

    #[test]
    fn compat_level_allows_migration_partitions(level in arb_compat_level()) {
        let allows = level.allows_migration();
        match level {
            CompatLevel::Full | CompatLevel::MappedCompat | CompatLevel::Partial => {
                prop_assert!(allows, "Full/MappedCompat/Partial should allow migration");
            }
            CompatLevel::Incompatible | CompatLevel::NoEquivalent => {
                prop_assert!(!allows, "Incompatible/NoEquivalent should not allow migration");
            }
        }
    }

    #[test]
    fn compat_level_label_nonempty(level in arb_compat_level()) {
        prop_assert!(!level.label().is_empty());
    }

    #[test]
    fn http_method_label_uppercase(method in arb_http_method()) {
        let label = method.label();
        prop_assert!(label.chars().all(|c| c.is_ascii_uppercase()),
            "label '{}' should be uppercase", label);
    }

    #[test]
    fn sdk_language_extension_has_dot(lang in arb_sdk_language()) {
        prop_assert!(lang.extension().starts_with('.'),
            "extension '{}' should start with '.'", lang.extension());
    }

    #[test]
    fn sdk_language_label_nonempty(lang in arb_sdk_language()) {
        prop_assert!(!lang.label().is_empty());
    }

    #[test]
    fn field_type_label_nonempty(ft in arb_field_type()) {
        prop_assert!(!ft.label().is_empty());
    }

    #[test]
    fn field_type_array_label_contains_inner(inner in arb_field_type_leaf()) {
        let array_type = FieldType::Array(Box::new(inner.clone()));
        let label = array_type.label();
        prop_assert!(label.contains("array<"), "array label '{}' should contain 'array<'", label);
        prop_assert!(label.contains(&inner.label()),
            "array label '{}' should contain inner label '{}'", label, inner.label());
    }

    #[test]
    fn field_type_optional_label_has_question_mark(inner in arb_field_type_leaf()) {
        let opt_type = FieldType::Optional(Box::new(inner));
        let label = opt_type.label();
        prop_assert!(label.ends_with('?'), "optional label '{}' should end with '?'", label);
    }
}

// =============================================================================
// NtmCompatShim invariants
// =============================================================================

proptest! {
    #[test]
    fn shim_readiness_summary_arithmetic(
        entries in prop::collection::vec(arb_ntm_compat_entry(), 0..20)
    ) {
        let mut shim = NtmCompatShim::new();
        for entry in &entries {
            shim.register(entry.clone());
        }

        let summary = shim.readiness_summary();

        // Total should match unique ft_command count
        prop_assert_eq!(summary.total, shim.entries.len());

        // Category counts should sum to total
        let category_sum = summary.full + summary.mapped + summary.partial
            + summary.incompatible + summary.no_equivalent;
        prop_assert_eq!(category_sum, summary.total,
            "categories must sum to total: {} + {} + {} + {} + {} = {} vs total {}",
            summary.full, summary.mapped, summary.partial,
            summary.incompatible, summary.no_equivalent, category_sum, summary.total);

        // Coverage formula: (full + mapped + partial) / total
        let migratable = summary.full + summary.mapped + summary.partial;
        if summary.total > 0 {
            let expected_coverage = migratable as f64 / summary.total as f64;
            prop_assert!((summary.migration_coverage - expected_coverage).abs() < 1e-10,
                "coverage {:.6} vs expected {:.6}", summary.migration_coverage, expected_coverage);
        } else {
            prop_assert!((summary.migration_coverage - 0.0).abs() < 1e-10);
        }
    }

    #[test]
    fn shim_filter_consistency(
        entries in prop::collection::vec(arb_ntm_compat_entry(), 1..15)
    ) {
        let mut shim = NtmCompatShim::new();
        for entry in &entries {
            shim.register(entry.clone());
        }

        let fully = shim.fully_compatible();
        let mapped = shim.needs_mapping();
        let not_migratable = shim.not_migratable();

        // fully_compatible should be subset that has Full compat
        for cmd in &fully {
            prop_assert_eq!(shim.compat_level(cmd), CompatLevel::Full);
        }

        // needs_mapping should be subset that has MappedCompat
        for cmd in &mapped {
            prop_assert_eq!(shim.compat_level(cmd), CompatLevel::MappedCompat);
        }

        // not_migratable should be subset where allows_migration is false
        for cmd in &not_migratable {
            prop_assert!(!shim.compat_level(cmd).allows_migration());
        }
    }

    #[test]
    fn shim_unknown_command_returns_no_equivalent(
        entries in prop::collection::vec(arb_ntm_compat_entry(), 0..5)
    ) {
        let mut shim = NtmCompatShim::new();
        for entry in &entries {
            shim.register(entry.clone());
        }
        // Query for a command that was never registered
        let level = shim.compat_level("__nonexistent_command_xyz__");
        prop_assert_eq!(level, CompatLevel::NoEquivalent);
    }

    #[test]
    fn shim_register_overwrites_duplicate(
        cmd in "[a-z]{3,10}",
        level1 in arb_compat_level(),
        level2 in arb_compat_level(),
    ) {
        let mut shim = NtmCompatShim::new();

        let entry1 = NtmCompatEntry {
            ft_command: cmd.clone(),
            ntm_command: cmd.clone(),
            compat_level: level1,
            field_mappings: Vec::new(),
            ntm_only_fields: Vec::new(),
            ft_only_fields: Vec::new(),
            notes: String::new(),
        };
        shim.register(entry1);

        let entry2 = NtmCompatEntry {
            ft_command: cmd.clone(),
            ntm_command: cmd.clone(),
            compat_level: level2,
            field_mappings: Vec::new(),
            ntm_only_fields: Vec::new(),
            ft_only_fields: Vec::new(),
            notes: String::new(),
        };
        shim.register(entry2);

        // Should have exactly one entry
        prop_assert_eq!(shim.entries.len(), 1);
        // Should be the last one registered
        prop_assert_eq!(shim.compat_level(&cmd), level2);
    }
}

// =============================================================================
// SDK generation invariants
// =============================================================================

proptest! {
    #[test]
    fn sdk_method_count_matches_specs(
        specs in prop::collection::vec(arb_endpoint_spec(), 0..8),
        lang in arb_sdk_language(),
    ) {
        let mut surface = SdkSurface::new(lang, "test-pkg");
        surface.generate_from_specs(&specs);
        prop_assert_eq!(surface.method_count(), specs.len(),
            "SDK method count should match input spec count");
    }

    #[test]
    fn sdk_param_count_matches_request_fields(
        spec in arb_endpoint_spec(),
        lang in arb_sdk_language(),
    ) {
        let expected_params = spec.request_fields.len();
        let mut surface = SdkSurface::new(lang, "test-pkg");
        surface.generate_from_specs(&[spec]);
        if surface.methods.len() == 1 {
            prop_assert_eq!(surface.methods[0].params.len(), expected_params);
        }
    }

    #[test]
    fn sdk_all_methods_async(
        specs in prop::collection::vec(arb_endpoint_spec(), 1..5),
        lang in arb_sdk_language(),
    ) {
        let mut surface = SdkSurface::new(lang, "test-pkg");
        surface.generate_from_specs(&specs);
        for method in &surface.methods {
            prop_assert!(method.is_async, "all generated methods should be async");
        }
    }

    #[test]
    fn sdk_artifact_filename_deterministic(lang in arb_sdk_language()) {
        let s1 = SdkSurface::new(lang, "pkg");
        let s2 = SdkSurface::new(lang, "pkg");
        prop_assert_eq!(s1.artifact_filename(), s2.artifact_filename());
    }

    #[test]
    fn sdk_artifact_filename_contains_extension(lang in arb_sdk_language()) {
        let surface = SdkSurface::new(lang, "pkg");
        let filename = surface.artifact_filename();
        prop_assert!(filename.ends_with(lang.extension()),
            "filename '{}' should end with '{}'", filename, lang.extension());
    }

    #[test]
    fn sdk_render_produces_nonempty_source(
        specs in prop::collection::vec(arb_endpoint_spec(), 1..4),
        lang in arb_sdk_language(),
    ) {
        let mut surface = SdkSurface::new(lang, "test-pkg");
        surface.generate_from_specs(&specs);
        let source = surface.render_client_source();
        prop_assert!(!source.is_empty(), "rendered source should be non-empty");
        // Should contain the class/struct name
        prop_assert!(source.contains("FrankentermClient"),
            "rendered source should contain 'FrankentermClient'");
    }
}

// =============================================================================
// ReplayTestSuiteResult invariants
// =============================================================================

proptest! {
    #[test]
    fn suite_result_pass_rate_arithmetic(
        n_pass in 0..20usize,
        n_fail in 0..20usize,
    ) {
        let total = n_pass + n_fail;
        if total == 0 {
            return Ok(());
        }

        let mut results = Vec::new();
        let mut tests = Vec::new();

        for i in 0..n_pass {
            let id = format!("pass-{i}");
            results.push(ReplayTestResult {
                test_id: id.clone(),
                passed: true,
                diff_summary: String::new(),
                diff_count: 0,
                duration_ms: 100,
            });
            tests.push(ReplayContractTest::new(&id, "cmd", "test"));
        }

        for i in 0..n_fail {
            let id = format!("fail-{i}");
            results.push(ReplayTestResult {
                test_id: id.clone(),
                passed: false,
                diff_summary: "diff".into(),
                diff_count: 1,
                duration_ms: 100,
            });
            tests.push(ReplayContractTest::new(&id, "cmd", "test"));
        }

        let suite = ReplayTestSuiteResult::from_results("test-suite", results, &tests);

        prop_assert_eq!(suite.total, total);
        prop_assert_eq!(suite.passed, n_pass);
        prop_assert_eq!(suite.failed, n_fail);

        let expected_rate = n_pass as f64 / total as f64;
        prop_assert!((suite.pass_rate - expected_rate).abs() < 1e-10,
            "pass_rate {:.6} vs expected {:.6}", suite.pass_rate, expected_rate);

        // If any test failed and all tests are blocking, blocking_pass should be false
        if n_fail > 0 {
            // All tests created via ::new have blocking=true by default
            prop_assert!(!suite.blocking_pass,
                "blocking_pass should be false when blocking tests fail");
        } else {
            prop_assert!(suite.blocking_pass,
                "blocking_pass should be true when all tests pass");
        }
    }

    #[test]
    fn suite_result_non_blocking_failures_dont_block(
        n_pass in 1..10usize,
        n_non_blocking_fail in 1..5usize,
    ) {
        let mut results = Vec::new();
        let mut tests = Vec::new();

        for i in 0..n_pass {
            let id = format!("pass-{i}");
            results.push(ReplayTestResult {
                test_id: id.clone(),
                passed: true,
                diff_summary: String::new(),
                diff_count: 0,
                duration_ms: 100,
            });
            tests.push(ReplayContractTest::new(&id, "cmd", "test"));
        }

        for i in 0..n_non_blocking_fail {
            let id = format!("nb-fail-{i}");
            results.push(ReplayTestResult {
                test_id: id.clone(),
                passed: false,
                diff_summary: "diff".into(),
                diff_count: 1,
                duration_ms: 100,
            });
            let mut test = ReplayContractTest::new(&id, "cmd", "test");
            test.blocking = false;
            tests.push(test);
        }

        let suite = ReplayTestSuiteResult::from_results("suite", results, &tests);

        prop_assert!(suite.blocking_pass,
            "blocking_pass should be true when only non-blocking tests fail");
        prop_assert!(suite.failed > 0, "there should be failures");
    }
}

// =============================================================================
// Standard factory invariants
// =============================================================================

#[test]
fn standard_shim_has_expected_coverage() {
    let shim = standard_ntm_compat_shim();
    let summary = shim.readiness_summary();

    // Must have at least some entries
    assert!(summary.total > 0, "standard shim should have entries");

    // Category sums match
    let sum = summary.full
        + summary.mapped
        + summary.partial
        + summary.incompatible
        + summary.no_equivalent;
    assert_eq!(sum, summary.total);

    // Coverage should be > 0
    assert!(
        summary.migration_coverage > 0.0,
        "should have some coverage"
    );

    // The standard shim registers 7 full + 2 mapped + 10 no-equivalent = 19 total
    assert_eq!(summary.total, 19);
    assert_eq!(summary.full, 7);
    assert_eq!(summary.mapped, 2);
    assert_eq!(summary.no_equivalent, 10);

    // migration_ready should be true
    assert!(shim.migration_ready);
}

#[test]
fn standard_replay_tests_are_blocking() {
    let tests = standard_replay_contract_tests();
    assert!(!tests.is_empty());
    for test in &tests {
        assert!(
            test.blocking,
            "standard tests should be blocking: {}",
            test.test_id
        );
        assert!(!test.test_id.is_empty());
        assert!(!test.command.is_empty());
    }
}

#[test]
fn standard_contract_artifacts_render_successfully() {
    let bundle = standard_contract_artifacts().unwrap();
    assert!(bundle.sdk_count() == 4, "should have 4 SDK languages");
    assert!(!bundle.endpoint_specs_json.is_empty());
    assert!(!bundle.ntm_compat_markdown.is_empty());
    assert!(!bundle.replay_tests_json.is_empty());

    // Each SDK source should contain the class name
    for (filename, source) in &bundle.sdk_sources {
        assert!(!filename.is_empty());
        assert!(
            source.contains("FrankentermClient"),
            "SDK source for {} should contain FrankentermClient",
            filename
        );
    }
}

#[test]
fn core_endpoint_specs_have_sane_structure() {
    let specs = core_endpoint_specs();
    assert!(!specs.is_empty());

    for spec in &specs {
        assert!(!spec.command.is_empty());
        assert!(!spec.description.is_empty());

        // GET should not be a mutation
        if spec.method == HttpMethod::Get {
            assert!(
                !spec.is_mutation,
                "GET endpoints should not be mutations: {}",
                spec.command
            );
        }

        // Mutations should have POST/PUT/DELETE
        if spec.is_mutation {
            assert!(
                matches!(
                    spec.method,
                    HttpMethod::Post | HttpMethod::Put | HttpMethod::Delete
                ),
                "mutations should use POST/PUT/DELETE: {}",
                spec.command
            );
        }
    }
}

#[test]
fn markdown_summary_has_table_headers() {
    let shim = standard_ntm_compat_shim();
    let md = shim.render_markdown_summary();
    assert!(md.contains("# NTM Compatibility Summary"));
    assert!(md.contains("ft command"));
    assert!(md.contains("NTM command"));
    assert!(md.contains("compatibility"));
}
