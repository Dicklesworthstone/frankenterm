//! Property-based tests for ARS secret & PII scanning.
//!
//! Verifies invariants of the Aho-Corasick scanner, Shannon entropy,
//! verdict classification, and statistics aggregation.

use proptest::prelude::*;

use frankenterm_core::ars_secret_scan::{
    ArsScanConfig, ArsSecretScanner, DetectionMethod, ScanStats, ScanVerdict, shannon_entropy,
};
use frankenterm_core::mdl_extraction::CommandBlock;

// =============================================================================
// Strategies
// =============================================================================

fn arb_clean_command() -> impl Strategy<Value = String> {
    prop_oneof![
        Just("ls -la".to_string()),
        Just("cd /project".to_string()),
        Just("cargo build".to_string()),
        Just("cargo test".to_string()),
        Just("git status".to_string()),
        Just("cat README.md".to_string()),
        Just("mkdir -p build".to_string()),
        Just("echo done".to_string()),
        Just("make clean".to_string()),
        Just("npm install".to_string()),
    ]
}

fn arb_secret_command() -> impl Strategy<Value = String> {
    prop_oneof![
        Just("export KEY=sk-abc123456789abcdef".to_string()),
        Just("export GH=ghp_ABCDEFGHIJKLMNOP12345".to_string()),
        Just("export AWS=AKIAIOSFODNN7EXAMPLE".to_string()),
        Just("curl -H 'Authorization: Bearer eyJhbGciOi'".to_string()),
        Just("export DB=postgres://root:pass@db:5432/prod".to_string()),
        Just("export SLACK=xoxb-1234567890-abc".to_string()),
        Just("export STRIPE=sk_live_abc123xyz789".to_string()),
        Just("echo '-----BEGIN RSA PRIVATE KEY-----'".to_string()),
        Just("mysql -u admin password=hunter2".to_string()),
        Just("export ANT=sk-ant-api03-XXXXXX".to_string()),
    ]
}

fn arb_command_block(index: u32, command: String) -> CommandBlock {
    CommandBlock {
        index,
        command,
        exit_code: Some(0),
        duration_us: Some(1000),
        output_preview: None,
        timestamp_us: (index as u64 + 1) * 1_000_000,
    }
}

fn arb_clean_window(min: usize, max: usize) -> impl Strategy<Value = Vec<CommandBlock>> {
    prop::collection::vec(arb_clean_command(), min..=max).prop_map(|cmds| {
        cmds.into_iter()
            .enumerate()
            .map(|(i, cmd)| arb_command_block(i as u32, cmd))
            .collect()
    })
}

fn arb_config() -> impl Strategy<Value = ArsScanConfig> {
    (
        3.0..5.0f64,     // entropy_threshold
        8..32usize,      // min_entropy_token_len
        64..512usize,    // max_entropy_token_len
        prop::bool::ANY, // scan_output
        prop::bool::ANY, // entropy_detection_enabled
    )
        .prop_map(
            |(entropy_threshold, min_len, max_len, scan_output, entropy_enabled)| ArsScanConfig {
                entropy_threshold,
                min_entropy_token_len: min_len,
                max_entropy_token_len: max_len,
                scan_output,
                entropy_detection_enabled: entropy_enabled,
                extra_patterns: Vec::new(),
            },
        )
}

fn arb_printable_string(min: usize, max: usize) -> impl Strategy<Value = String> {
    prop::collection::vec(32u8..127, min..=max)
        .prop_map(|bytes| String::from_utf8(bytes).unwrap_or_default())
}

// =============================================================================
// Shannon entropy invariants
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(300))]

    #[test]
    fn entropy_is_non_negative(s in arb_printable_string(0, 100)) {
        let e = shannon_entropy(&s);
        prop_assert!(e >= 0.0, "entropy {} should be >= 0", e);
    }

    #[test]
    fn entropy_bounded_by_log2_alphabet(s in arb_printable_string(1, 100)) {
        let e = shannon_entropy(&s);
        // Maximum entropy is log2(256) = 8 bits for byte-level
        prop_assert!(e <= 8.0, "entropy {} should be <= 8.0", e);
    }

    #[test]
    fn entropy_zero_for_single_char(c in 32u8..127) {
        let s = String::from_utf8(vec![c; 20]).unwrap();
        let e = shannon_entropy(&s);
        prop_assert!(
            e.abs() < f64::EPSILON,
            "single-char string should have entropy 0, got {}",
            e
        );
    }

    #[test]
    fn entropy_increases_with_diversity(
        base in 32u8..100,
        extra_chars in 1..10usize,
    ) {
        // Single char repeated.
        let mono = String::from_utf8(vec![base; 20]).unwrap();
        let e_mono = shannon_entropy(&mono);

        // Add diverse chars.
        let mut diverse = vec![base; 20];
        for i in 0..extra_chars {
            diverse.push(base.wrapping_add(i as u8 + 1));
        }
        let diverse_s = String::from_utf8(diverse).unwrap_or_default();
        let e_diverse = shannon_entropy(&diverse_s);

        prop_assert!(
            e_diverse >= e_mono,
            "more diverse ({:.3}) should have >= entropy than mono ({:.3})",
            e_diverse,
            e_mono
        );
    }
}

// =============================================================================
// Clean commands always produce Clean verdict
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn clean_commands_produce_clean_verdict(
        window in arb_clean_window(1, 10)
    ) {
        let config = ArsScanConfig {
            entropy_detection_enabled: false, // disable to avoid false positives on test data
            ..Default::default()
        };
        let scanner = ArsSecretScanner::new(config);
        let verdict = scanner.scan_commands(&window);
        prop_assert!(
            verdict.is_clean(),
            "clean commands should produce Clean verdict, got {:?}",
            verdict
        );
    }
}

// =============================================================================
// Secret commands always produce Contaminated verdict
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn secret_commands_detected(
        secret_cmd in arb_secret_command(),
        clean_prefix in arb_clean_window(0, 3),
    ) {
        let scanner = ArsSecretScanner::with_defaults();
        let mut cmds = clean_prefix;
        let idx = cmds.len() as u32;
        cmds.push(arb_command_block(idx, secret_cmd));

        let verdict = scanner.scan_commands(&cmds);
        prop_assert!(
            verdict.is_contaminated(),
            "window with secret should be contaminated"
        );
    }
}

// =============================================================================
// Verdict invariants
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn verdict_is_clean_xor_contaminated(
        window in arb_clean_window(0, 5)
    ) {
        let scanner = ArsSecretScanner::new(ArsScanConfig {
            entropy_detection_enabled: false,
            ..Default::default()
        });
        let verdict = scanner.scan_commands(&window);
        let is_clean = verdict.is_clean();
        let is_contam = verdict.is_contaminated();
        prop_assert!(
            is_clean ^ is_contam,
            "verdict must be exactly one of clean/contaminated"
        );
    }

    #[test]
    fn verdict_clean_serde_roundtrip(_dummy in 0..1u8) {
        let v = ScanVerdict::Clean;
        let json = serde_json::to_string(&v).unwrap();
        let decoded: ScanVerdict = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(decoded, v);
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
        let decoded: ArsScanConfig = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(decoded.min_entropy_token_len, config.min_entropy_token_len);
        prop_assert_eq!(decoded.max_entropy_token_len, config.max_entropy_token_len);
        prop_assert_eq!(decoded.scan_output, config.scan_output);
        prop_assert_eq!(decoded.entropy_detection_enabled, config.entropy_detection_enabled);
        let diff = (decoded.entropy_threshold - config.entropy_threshold).abs();
        prop_assert!(diff < 1e-10, "entropy_threshold drift: {}", diff);
    }

    #[test]
    fn scanner_with_any_config_does_not_panic(
        config in arb_config(),
        window in arb_clean_window(0, 5)
    ) {
        let scanner = ArsSecretScanner::new(config);
        let _verdict = scanner.scan_commands(&window);
        // No panic = success.
    }
}

// =============================================================================
// ScanStats invariants
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn stats_total_equals_clean_plus_contaminated(
        clean_count in 0..10u64,
        contam_count in 0..10u64,
    ) {
        let mut stats = ScanStats::new();
        for _ in 0..clean_count {
            stats.record(&ScanVerdict::Clean);
        }
        for _ in 0..contam_count {
            let v = ScanVerdict::Contaminated(frankenterm_core::ars_secret_scan::ScanContamination {
                findings: vec![frankenterm_core::ars_secret_scan::ScanFinding {
                    pattern_name: "test".to_string(),
                    block_index: 0,
                    source: "command".to_string(),
                    byte_offset: 0,
                    match_len: 3,
                    context_redacted: "...".to_string(),
                    detection_method: DetectionMethod::PatternMatch,
                    entropy: None,
                }],
                abort_reason: "test".to_string(),
            });
            stats.record(&v);
        }

        prop_assert_eq!(
            stats.total_scans,
            clean_count + contam_count,
            "total should equal clean + contaminated"
        );
        prop_assert_eq!(stats.clean_count, clean_count);
        prop_assert_eq!(stats.contaminated_count, contam_count);
        prop_assert_eq!(stats.total_findings, contam_count);
    }

    #[test]
    fn stats_findings_by_pattern_sum_equals_total(
        n_findings in 1..20usize,
    ) {
        let mut stats = ScanStats::new();
        let patterns = ["openai_key", "aws_key", "github_token"];

        for i in 0..n_findings {
            let pattern = patterns[i % patterns.len()];
            let v = ScanVerdict::Contaminated(frankenterm_core::ars_secret_scan::ScanContamination {
                findings: vec![frankenterm_core::ars_secret_scan::ScanFinding {
                    pattern_name: pattern.to_string(),
                    block_index: 0,
                    source: "command".to_string(),
                    byte_offset: 0,
                    match_len: 3,
                    context_redacted: "...".to_string(),
                    detection_method: DetectionMethod::PatternMatch,
                    entropy: None,
                }],
                abort_reason: "test".to_string(),
            });
            stats.record(&v);
        }

        let pattern_sum: u64 = stats.findings_by_pattern.values().sum();
        prop_assert_eq!(
            pattern_sum,
            stats.total_findings,
            "findings_by_pattern sum should equal total_findings"
        );
    }
}

// =============================================================================
// Findings contain no raw secrets
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn findings_context_is_redacted(
        secret_cmd in arb_secret_command()
    ) {
        let scanner = ArsSecretScanner::with_defaults();
        let cmds = vec![arb_command_block(0, secret_cmd.clone())];
        let verdict = scanner.scan_commands(&cmds);

        if let ScanVerdict::Contaminated(c) = &verdict {
            for finding in &c.findings {
                // Context should be redacted (short or masked).
                let ctx_len = finding.context_redacted.len();
                prop_assert!(
                    ctx_len <= 50 || finding.context_redacted.contains("["),
                    "context should be redacted, got: {}",
                    finding.context_redacted
                );
            }
        }
    }
}

// =============================================================================
// Scan standalone invariants
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn standalone_scan_consistent_with_command_scan(
        cmd in arb_clean_command()
    ) {
        let config = ArsScanConfig {
            entropy_detection_enabled: false,
            ..Default::default()
        };
        let scanner = ArsSecretScanner::new(config);

        let standalone_findings = scanner.scan_text_standalone(&cmd);
        let cmd_block = arb_command_block(0, cmd);
        let verdict = scanner.scan_commands(&[cmd_block]);

        // Both should agree on clean/contaminated.
        let standalone_clean = standalone_findings.iter()
            .all(|f| f.detection_method != DetectionMethod::PatternMatch);
        let is_verdict_clean = matches!(verdict, ScanVerdict::Clean);

        // If standalone found no pattern matches, verdict should be clean.
        if standalone_clean {
            prop_assert!(is_verdict_clean, "standalone and command scan should agree");
        }
    }
}
