//! Property-based tests for ARS parameter generalization and PAC-Bayesian bounds.
//!
//! Verifies invariants of parameter detection, template generation,
//! safety regex validation, PAC-Bayesian concentration, and serde roundtrips.

use proptest::prelude::*;

use std::collections::HashMap;

use frankenterm_core::ars_generalize::{
    GeneralizationResult, GeneralizationStats, GeneralizeConfig, GeneralizedCommand, Generalizer,
    PacBayesianBound, ParamKind, TemplateVar,
};
use frankenterm_core::mdl_extraction::CommandBlock;

// =============================================================================
// Strategies
// =============================================================================

fn arb_param_kind() -> impl Strategy<Value = ParamKind> {
    prop_oneof![
        Just(ParamKind::FilePath),
        Just(ParamKind::LineNumber),
        Just(ParamKind::Identifier),
        Just(ParamKind::Numeric),
        Just(ParamKind::Custom),
    ]
}

fn arb_file_path() -> impl Strategy<Value = String> {
    prop_oneof![
        Just("src/main.rs".to_string()),
        Just("src/lib.rs".to_string()),
        Just("tests/test_foo.rs".to_string()),
        Just("lib/utils.ts".to_string()),
        Just("app/config/db.yml".to_string()),
        Just("README.md".to_string()),
        Just("Cargo.toml".to_string()),
        Just("package.json".to_string()),
    ]
}

fn arb_line_number() -> impl Strategy<Value = u32> {
    1..10000u32
}

fn arb_identifier() -> impl Strategy<Value = String> {
    prop_oneof![
        Just("MyStruct".to_string()),
        Just("foo_bar".to_string()),
        Just("DatabaseConnection".to_string()),
        Just("parse_config".to_string()),
        Just("HttpClient".to_string()),
        Just("UserRepo".to_string()),
    ]
}

fn arb_clean_command() -> impl Strategy<Value = String> {
    prop_oneof![
        Just("cargo build".to_string()),
        Just("cargo test".to_string()),
        Just("git status".to_string()),
        Just("npm install".to_string()),
        Just("make clean".to_string()),
    ]
}

fn arb_injection_attempt() -> impl Strategy<Value = String> {
    prop_oneof![
        Just("src/main.rs; rm -rf /".to_string()),
        Just("file$(whoami)".to_string()),
        Just("path`id`".to_string()),
        Just("foo | cat /etc/passwd".to_string()),
        Just("test && echo pwned".to_string()),
    ]
}

fn make_cmd_block(index: u32, command: String) -> CommandBlock {
    CommandBlock {
        index,
        command,
        exit_code: Some(0),
        duration_us: Some(1000),
        output_preview: None,
        timestamp_us: (index as u64 + 1) * 1_000_000,
    }
}

fn arb_config() -> impl Strategy<Value = GeneralizeConfig> {
    (
        2..10usize,      // min_param_len
        1..16usize,      // max_params_per_command
        4..64usize,      // max_total_params
        0.1..10.0f64,    // pac_prior_weight
        0.01..0.5f64,    // pac_confidence_delta
        prop::bool::ANY, // detect_file_paths
        prop::bool::ANY, // detect_line_numbers
        prop::bool::ANY, // detect_identifiers
        prop::bool::ANY, // detect_numerics
    )
        .prop_map(
            |(min_len, max_per, max_total, prior, delta, fp, ln, id, nu)| GeneralizeConfig {
                min_param_len: min_len,
                max_params_per_command: max_per,
                max_total_params: max_total,
                pac_prior_weight: prior,
                pac_confidence_delta: delta,
                detect_file_paths: fp,
                detect_line_numbers: ln,
                detect_identifiers: id,
                detect_numerics: nu,
                custom_safety_patterns: HashMap::new(),
            },
        )
}

// =============================================================================
// ParamKind invariants
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn param_kind_safety_regex_is_nonempty(kind in arb_param_kind()) {
        prop_assert!(!kind.safety_regex().is_empty());
    }

    #[test]
    fn param_kind_template_prefix_is_nonempty(kind in arb_param_kind()) {
        prop_assert!(!kind.template_prefix().is_empty());
    }

    #[test]
    fn param_kind_serde_roundtrip(kind in arb_param_kind()) {
        let json = serde_json::to_string(&kind).unwrap();
        let decoded: ParamKind = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(decoded, kind);
    }
}

// =============================================================================
// PAC-Bayesian bound invariants
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn pac_bound_risk_in_unit_interval(
        n in 1..500u64,
        m_frac in 0.0..1.0f64,
        delta in 0.01..0.5f64,
        prior in 0.1..10.0f64,
    ) {
        let m = ((n as f64) * m_frac) as u64;
        let bound = PacBayesianBound::compute(n, m.min(n), delta, prior);
        prop_assert!(bound.risk_bound >= 0.0);
        prop_assert!(bound.risk_bound <= 1.0);
    }

    #[test]
    fn pac_bound_empirical_risk_in_unit_interval(
        n in 1..500u64,
        m_frac in 0.0..1.0f64,
    ) {
        let m = ((n as f64) * m_frac) as u64;
        let bound = PacBayesianBound::compute(n, m.min(n), 0.05, 1.0);
        prop_assert!(bound.empirical_risk >= 0.0);
        prop_assert!(bound.empirical_risk <= 1.0);
    }

    #[test]
    fn pac_bound_risk_geq_empirical(
        n in 1..200u64,
        m_frac in 0.0..1.0f64,
    ) {
        let m = ((n as f64) * m_frac) as u64;
        let bound = PacBayesianBound::compute(n, m.min(n), 0.05, 1.0);
        prop_assert!(
            bound.risk_bound >= bound.empirical_risk - 1e-10,
            "risk_bound {} should be >= empirical_risk {}",
            bound.risk_bound,
            bound.empirical_risk
        );
    }

    #[test]
    fn pac_bound_more_data_tighter_bound(
        m_rate in 0.8..1.0f64,
    ) {
        // With high match rate, more data should give tighter bounds.
        let bound10 = PacBayesianBound::compute(10, (10.0 * m_rate) as u64, 0.05, 1.0);
        let bound100 = PacBayesianBound::compute(100, (100.0 * m_rate) as u64, 0.05, 1.0);
        prop_assert!(
            bound100.risk_bound <= bound10.risk_bound + 0.01,
            "100 obs bound {} should be tighter than 10 obs bound {}",
            bound100.risk_bound,
            bound10.risk_bound
        );
    }

    #[test]
    fn pac_bound_zero_observations_returns_max_risk(_dummy in 0..1u8) {
        let bound = PacBayesianBound::compute(0, 0, 0.05, 1.0);
        let close = (bound.risk_bound - 1.0).abs() < 1e-10;
        prop_assert!(close);
        prop_assert!(!bound.is_trustworthy);
    }

    #[test]
    fn pac_bound_serde_roundtrip(
        n in 1..100u64,
        m_frac in 0.0..1.0f64,
    ) {
        let m = ((n as f64) * m_frac) as u64;
        let bound = PacBayesianBound::compute(n, m.min(n), 0.05, 1.0);
        let json = serde_json::to_string(&bound).unwrap();
        let decoded: PacBayesianBound = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(decoded.n_observations, bound.n_observations);
        prop_assert_eq!(decoded.n_matches, bound.n_matches);
        let diff = (decoded.risk_bound - bound.risk_bound).abs();
        prop_assert!(diff < 1e-10, "risk_bound drift: {}", diff);
    }
}

// =============================================================================
// Safety regex invariants
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn safety_regex_rejects_all_injections(
        injection in arb_injection_attempt(),
        kind in arb_param_kind(),
    ) {
        // All injection attempts should fail safety validation for all kinds.
        let gc = GeneralizedCommand {
            original: "test".to_string(),
            template: "test {{cap.p_0}}".to_string(),
            variables: vec![TemplateVar {
                name: "p_0".to_string(),
                placeholder: "{{cap.p_0}}".to_string(),
                original: "safe".to_string(),
                kind,
                safety_regex: kind.safety_regex().to_string(),
            }],
            block_index: 0,
        };
        let mut values = HashMap::new();
        values.insert("p_0".to_string(), injection);
        let result = gc.instantiate(&values);
        prop_assert!(
            result.is_none(),
            "injection should be rejected for kind {:?}",
            kind
        );
    }

    #[test]
    fn safety_regex_accepts_valid_file_paths(path in arb_file_path()) {
        let gc = GeneralizedCommand {
            original: "test".to_string(),
            template: "test {{cap.file_0}}".to_string(),
            variables: vec![TemplateVar {
                name: "file_0".to_string(),
                placeholder: "{{cap.file_0}}".to_string(),
                original: "src/main.rs".to_string(),
                kind: ParamKind::FilePath,
                safety_regex: ParamKind::FilePath.safety_regex().to_string(),
            }],
            block_index: 0,
        };
        let mut values = HashMap::new();
        values.insert("file_0".to_string(), path.clone());
        let result = gc.instantiate(&values);
        prop_assert!(
            result.is_some(),
            "valid file path {} should be accepted",
            path
        );
    }

    #[test]
    fn safety_regex_accepts_valid_line_numbers(line in arb_line_number()) {
        let line_str = line.to_string();
        if line_str.len() >= 2 {
            let gc = GeneralizedCommand {
                original: "test".to_string(),
                template: "test {{cap.line_0}}".to_string(),
                variables: vec![TemplateVar {
                    name: "line_0".to_string(),
                    placeholder: "{{cap.line_0}}".to_string(),
                    original: "42".to_string(),
                    kind: ParamKind::LineNumber,
                    safety_regex: ParamKind::LineNumber.safety_regex().to_string(),
                }],
                block_index: 0,
            };
            let mut values = HashMap::new();
            values.insert("line_0".to_string(), line_str);
            let result = gc.instantiate(&values);
            prop_assert!(result.is_some(), "valid line number should be accepted");
        }
    }

    #[test]
    fn safety_regex_accepts_valid_identifiers(ident in arb_identifier()) {
        let gc = GeneralizedCommand {
            original: "test".to_string(),
            template: "test {{cap.ident_0}}".to_string(),
            variables: vec![TemplateVar {
                name: "ident_0".to_string(),
                placeholder: "{{cap.ident_0}}".to_string(),
                original: "Foo".to_string(),
                kind: ParamKind::Identifier,
                safety_regex: ParamKind::Identifier.safety_regex().to_string(),
            }],
            block_index: 0,
        };
        let mut values = HashMap::new();
        values.insert("ident_0".to_string(), ident);
        let result = gc.instantiate(&values);
        prop_assert!(result.is_some(), "valid identifier should be accepted");
    }
}

// =============================================================================
// Detection invariants
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn detection_finds_file_in_both_command_and_error(
        file in arb_file_path(),
    ) {
        let gzr = Generalizer::with_defaults();
        let cmd_text = format!("cargo test {}", file);
        let error_text = format!("error in {}:42", file);
        let cmd = make_cmd_block(0, cmd_text);
        let detections = gzr.detect_params(&[cmd], &error_text);
        // Should find file path in at least some cases.
        let file_params: Vec<_> = detections
            .iter()
            .flat_map(|(_, p)| p)
            .filter(|p| p.kind == ParamKind::FilePath)
            .collect();
        // File should be detected if it meets min_param_len.
        if file.len() >= 2 {
            prop_assert!(
                !file_params.is_empty(),
                "should detect {} as file path parameter",
                file
            );
        }
    }

    #[test]
    fn detection_with_no_overlap_finds_nothing(
        cmd in arb_clean_command(),
    ) {
        let gzr = Generalizer::with_defaults();
        let error = "error in /completely/different/path.xyz:999";
        let block = make_cmd_block(0, cmd);
        let detections = gzr.detect_params(&[block], error);
        let file_params: Vec<_> = detections
            .iter()
            .flat_map(|(_, p)| p)
            .filter(|p| p.kind == ParamKind::FilePath)
            .collect();
        prop_assert!(
            file_params.is_empty(),
            "no file path overlap should produce no file detections"
        );
    }

    #[test]
    fn detection_respects_max_params_limit(
        file in arb_file_path(),
    ) {
        let config = GeneralizeConfig {
            max_params_per_command: 1,
            ..Default::default()
        };
        let gzr = Generalizer::new(config);
        let cmd = make_cmd_block(0, format!("test {} line 42 MyStruct", file));
        let error = format!("error in {}:42 MyStruct", file);
        let detections = gzr.detect_params(&[cmd], &error);
        for (_, params) in &detections {
            prop_assert!(
                params.len() <= 1,
                "should respect max_params_per_command=1, got {}",
                params.len()
            );
        }
    }
}

// =============================================================================
// Generalization invariants
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn generalization_preserves_command_count(
        cmds in prop::collection::vec(arb_clean_command(), 1..5),
    ) {
        let gzr = Generalizer::with_defaults();
        let blocks: Vec<CommandBlock> = cmds
            .into_iter()
            .enumerate()
            .map(|(i, c)| make_cmd_block(i as u32, c))
            .collect();
        let result = gzr.generalize(&blocks, "some error", 10, 10);
        prop_assert_eq!(
            result.commands.len(),
            blocks.len(),
            "should have one generalized command per input"
        );
    }

    #[test]
    fn generalization_template_contains_original_or_placeholder(
        file in arb_file_path(),
    ) {
        let gzr = Generalizer::with_defaults();
        let cmd = make_cmd_block(0, format!("cat {}", file));
        let error = format!("error in {}", file);
        let result = gzr.generalize(&[cmd], &error, 10, 10);
        let gc = &result.commands[0];
        // Template should either be original (no generalization) or contain placeholder.
        let is_original = gc.template == gc.original;
        let has_placeholder = gc.template.contains("{{cap.");
        prop_assert!(
            is_original || has_placeholder,
            "template should be original or have placeholder"
        );
    }

    #[test]
    fn generalized_command_variables_match_placeholders(
        file in arb_file_path(),
    ) {
        let gzr = Generalizer::with_defaults();
        let cmd = make_cmd_block(0, format!("cat {}", file));
        let error = format!("error in {}", file);
        let result = gzr.generalize(&[cmd], &error, 10, 10);
        for gc in &result.commands {
            for var in &gc.variables {
                prop_assert!(
                    gc.template.contains(&var.placeholder),
                    "template should contain placeholder {}",
                    var.placeholder
                );
            }
        }
    }

    #[test]
    fn generalization_instantiate_with_originals_recovers_command(
        file in arb_file_path(),
    ) {
        let gzr = Generalizer::with_defaults();
        let cmd_text = format!("cat {}", file);
        let cmd = make_cmd_block(0, cmd_text.clone());
        let error = format!("error in {}", file);
        let result = gzr.generalize(&[cmd], &error, 10, 10);
        let gc = &result.commands[0];

        if gc.is_generalized() {
            let mut values = HashMap::new();
            for var in &gc.variables {
                values.insert(var.name.clone(), var.original.clone());
            }
            let instantiated = gc.instantiate(&values);
            prop_assert!(
                instantiated.is_some(),
                "instantiate with originals should succeed"
            );
            prop_assert_eq!(
                instantiated.unwrap(),
                cmd_text,
                "instantiate with originals should recover original command"
            );
        }
    }
}

// =============================================================================
// Config serde invariants
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn config_serde_roundtrip(config in arb_config()) {
        let json = serde_json::to_string(&config).unwrap();
        let decoded: GeneralizeConfig = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(decoded.min_param_len, config.min_param_len);
        prop_assert_eq!(decoded.max_params_per_command, config.max_params_per_command);
        prop_assert_eq!(decoded.max_total_params, config.max_total_params);
        prop_assert_eq!(decoded.detect_file_paths, config.detect_file_paths);
        prop_assert_eq!(decoded.detect_line_numbers, config.detect_line_numbers);
        prop_assert_eq!(decoded.detect_identifiers, config.detect_identifiers);
        prop_assert_eq!(decoded.detect_numerics, config.detect_numerics);
        let prior_diff = (decoded.pac_prior_weight - config.pac_prior_weight).abs();
        prop_assert!(prior_diff < 1e-10, "pac_prior_weight drift: {}", prior_diff);
        let delta_diff = (decoded.pac_confidence_delta - config.pac_confidence_delta).abs();
        prop_assert!(delta_diff < 1e-10, "pac_confidence_delta drift: {}", delta_diff);
    }

    #[test]
    fn generalizer_with_any_config_does_not_panic(
        config in arb_config(),
        cmd in arb_clean_command(),
    ) {
        let gzr = Generalizer::new(config);
        let block = make_cmd_block(0, cmd);
        let _result = gzr.generalize(&[block], "some error text", 5, 5);
        // No panic = success.
    }
}

// =============================================================================
// Stats invariants
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn stats_attempts_equal_successes_plus_failures(
        successes in 0..50u64,
        failures in 0..50u64,
    ) {
        let mut stats = GeneralizationStats::new();
        for _ in 0..successes {
            stats.record_instantiation(true);
        }
        for _ in 0..failures {
            stats.record_instantiation(false);
        }
        prop_assert_eq!(
            stats.instantiation_attempts,
            successes + failures
        );
        prop_assert_eq!(stats.instantiation_successes, successes);
        prop_assert_eq!(stats.instantiation_failures, failures);
    }

    #[test]
    fn stats_success_rate_in_unit_interval(
        successes in 0..50u64,
        failures in 0..50u64,
    ) {
        let mut stats = GeneralizationStats::new();
        for _ in 0..successes {
            stats.record_instantiation(true);
        }
        for _ in 0..failures {
            stats.record_instantiation(false);
        }
        let rate = stats.instantiation_success_rate();
        if successes + failures == 0 {
            let close = (rate - 0.0).abs() < 1e-10;
            prop_assert!(close);
        } else {
            prop_assert!(rate >= 0.0);
            prop_assert!(rate <= 1.0);
        }
    }

    #[test]
    fn stats_serde_roundtrip(
        sessions in 0..100u64,
        params in 0..100u64,
        cmds in 0..100u64,
    ) {
        let stats = GeneralizationStats {
            total_sessions: sessions,
            total_params_detected: params,
            total_commands_generalized: cmds,
            params_by_kind: HashMap::new(),
            instantiation_attempts: 0,
            instantiation_successes: 0,
            instantiation_failures: 0,
        };
        let json = serde_json::to_string(&stats).unwrap();
        let decoded: GeneralizationStats = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(decoded.total_sessions, stats.total_sessions);
        prop_assert_eq!(decoded.total_params_detected, stats.total_params_detected);
        prop_assert_eq!(decoded.total_commands_generalized, stats.total_commands_generalized);
    }
}

// =============================================================================
// GeneralizationResult invariants
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn result_serde_roundtrip(
        file in arb_file_path(),
    ) {
        let gzr = Generalizer::with_defaults();
        let cmd = make_cmd_block(0, format!("cat {}", file));
        let error = format!("error in {}", file);
        let result = gzr.generalize(&[cmd], &error, 10, 9);

        let json = serde_json::to_string(&result).unwrap();
        let decoded: GeneralizationResult = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(decoded.commands.len(), result.commands.len());
        prop_assert_eq!(decoded.params_detected, result.params_detected);
        prop_assert_eq!(decoded.commands_generalized, result.commands_generalized);
    }

    #[test]
    fn result_all_variables_have_safety_regexes(
        file in arb_file_path(),
    ) {
        let gzr = Generalizer::with_defaults();
        let cmd = make_cmd_block(0, format!("cat {}", file));
        let error = format!("error in {}", file);
        let result = gzr.generalize(&[cmd], &error, 10, 10);

        for var in &result.all_variables {
            prop_assert!(!var.safety_regex.is_empty(), "all variables should have safety regex");
        }
    }

    #[test]
    fn result_commands_generalized_count_matches(
        file in arb_file_path(),
    ) {
        let gzr = Generalizer::with_defaults();
        let cmds = vec![
            make_cmd_block(0, format!("cat {}", file)),
            make_cmd_block(1, "cargo build".to_string()),
        ];
        let error = format!("error in {}", file);
        let result = gzr.generalize(&cmds, &error, 10, 10);

        let actual_generalized = result.commands.iter().filter(|c| c.is_generalized()).count();
        prop_assert_eq!(
            actual_generalized,
            result.commands_generalized,
            "commands_generalized should match actual count"
        );
    }
}
