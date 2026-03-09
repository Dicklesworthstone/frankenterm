//! Property-based tests for ARS secret & PII scanning.
//!
//! Verifies invariants of the Aho-Corasick scanner, Shannon entropy,
//! verdict classification, statistics aggregation, serde roundtrips,
//! and detection correctness.

use proptest::prelude::*;

use frankenterm_core::ars_secret_scan::{
    ArsScanConfig, ArsSecretScanner, DetectionMethod, ScanContamination, ScanFinding, ScanStats,
    ScanVerdict, shannon_entropy,
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

fn arb_known_prefix() -> impl Strategy<Value = &'static str> {
    prop_oneof![
        Just("sk-"),
        Just("sk-proj-"),
        Just("sk-ant-"),
        Just("ghp_"),
        Just("gho_"),
        Just("ghs_"),
        Just("ghr_"),
        Just("AKIA"),
        Just("xoxb-"),
        Just("xoxp-"),
        Just("sk_live_"),
        Just("sk_test_"),
        Just("postgres://"),
        Just("mysql://"),
        Just("redis://"),
        Just("Bearer "),
        Just("Basic "),
        Just("password="),
        Just("secret="),
        Just("token="),
        Just("api_key="),
        Just("-----BEGIN"),
        Just("npm_"),
        Just("pypi-"),
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

fn arb_command_block_with_output(
    index: u32,
    command: String,
    output: String,
) -> CommandBlock {
    CommandBlock {
        index,
        command,
        exit_code: Some(0),
        duration_us: Some(1000),
        output_preview: Some(output),
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

fn arb_detection_method() -> impl Strategy<Value = DetectionMethod> {
    prop_oneof![
        Just(DetectionMethod::PatternMatch),
        Just(DetectionMethod::EntropyThreshold),
    ]
}

fn arb_scan_finding() -> impl Strategy<Value = ScanFinding> {
    (
        "[a-z_]{3,15}",
        0u32..100,
        prop_oneof![Just("command".to_string()), Just("output".to_string())],
        0usize..10000,
        1usize..100,
        "[a-z.]{3,20}",
        arb_detection_method(),
        prop_oneof![Just(None), (1.0..8.0f64).prop_map(Some)],
    )
        .prop_map(
            |(pattern_name, block_index, source, byte_offset, match_len, context, method, entropy)| {
                ScanFinding {
                    pattern_name,
                    block_index,
                    source,
                    byte_offset,
                    match_len,
                    context_redacted: context,
                    detection_method: method,
                    entropy,
                }
            },
        )
}

// =============================================================================
// Shannon entropy invariants
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(300))]

    /// AS-01: Entropy is always non-negative.
    #[test]
    fn as01_entropy_is_non_negative(s in arb_printable_string(0, 100)) {
        let e = shannon_entropy(&s);
        prop_assert!(e >= 0.0, "entropy {} should be >= 0", e);
    }

    /// AS-02: Entropy bounded by log2(256) = 8.
    #[test]
    fn as02_entropy_bounded_by_log2_alphabet(s in arb_printable_string(1, 100)) {
        let e = shannon_entropy(&s);
        prop_assert!(e <= 8.0, "entropy {} should be <= 8.0", e);
    }

    /// AS-03: Single repeated char has zero entropy.
    #[test]
    fn as03_entropy_zero_for_single_char(c in 32u8..127) {
        let s = String::from_utf8(vec![c; 20]).unwrap();
        let e = shannon_entropy(&s);
        prop_assert!(
            e.abs() < f64::EPSILON,
            "single-char string should have entropy 0, got {}", e
        );
    }

    /// AS-04: More diverse characters = higher entropy.
    #[test]
    fn as04_entropy_increases_with_diversity(
        base in 32u8..100,
        extra_chars in 1..10usize,
    ) {
        let mono = String::from_utf8(vec![base; 20]).unwrap();
        let e_mono = shannon_entropy(&mono);

        let mut diverse = vec![base; 20];
        for i in 0..extra_chars {
            diverse.push(base.wrapping_add(i as u8 + 1));
        }
        let diverse_s = String::from_utf8(diverse).unwrap_or_default();
        let e_diverse = shannon_entropy(&diverse_s);

        prop_assert!(
            e_diverse >= e_mono,
            "more diverse ({:.3}) should have >= entropy than mono ({:.3})",
            e_diverse, e_mono
        );
    }

    /// AS-05: Entropy is deterministic (same input = same output).
    #[test]
    fn as05_entropy_deterministic(s in arb_printable_string(0, 100)) {
        let e1 = shannon_entropy(&s);
        let e2 = shannon_entropy(&s);
        prop_assert!((e1 - e2).abs() < f64::EPSILON);
    }

    /// AS-06: Empty string has zero entropy.
    #[test]
    fn as06_entropy_empty_is_zero(_dummy in 0u8..1) {
        let e = shannon_entropy("");
        prop_assert!((e - 0.0).abs() < f64::EPSILON);
    }

    /// AS-07: Entropy is invariant to character order (same bytes, different order).
    #[test]
    fn as07_entropy_permutation_invariant(
        bytes in prop::collection::vec(32u8..127, 2..50),
    ) {
        let s1 = String::from_utf8(bytes.clone()).unwrap_or_default();
        let mut reversed = bytes;
        reversed.reverse();
        let s2 = String::from_utf8(reversed).unwrap_or_default();
        let e1 = shannon_entropy(&s1);
        let e2 = shannon_entropy(&s2);
        prop_assert!(
            (e1 - e2).abs() < 1e-10,
            "entropy should be permutation-invariant: {e1} vs {e2}"
        );
    }
}

// =============================================================================
// Clean commands always produce Clean verdict
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// AS-08: Clean commands with no entropy produce Clean verdict.
    #[test]
    fn as08_clean_commands_produce_clean_verdict(
        window in arb_clean_window(1, 10)
    ) {
        let config = ArsScanConfig {
            entropy_detection_enabled: false,
            ..Default::default()
        };
        let scanner = ArsSecretScanner::new(config);
        let verdict = scanner.scan_commands(&window);
        prop_assert!(
            verdict.is_clean(),
            "clean commands should produce Clean verdict, got {:?}", verdict
        );
    }
}

// =============================================================================
// Secret commands always produce Contaminated verdict
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// AS-09: Any known secret prefix injected into commands is detected.
    #[test]
    fn as09_secret_commands_detected(
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

    /// AS-10: Injecting a known prefix into random text always detects it.
    #[test]
    fn as10_injected_prefix_always_detected(
        prefix in arb_known_prefix(),
        before in "[a-z ]{5,30}",
        after in "[a-z0-9]{5,30}",
    ) {
        let config = ArsScanConfig {
            entropy_detection_enabled: false,
            ..Default::default()
        };
        let scanner = ArsSecretScanner::new(config);
        let text = format!("{before}{prefix}{after}");
        let findings = scanner.scan_text_standalone(&text);
        let has_pattern = findings.iter().any(|f| f.detection_method == DetectionMethod::PatternMatch);
        prop_assert!(has_pattern, "prefix '{}' not detected in '{}'", prefix, text);
    }
}

// =============================================================================
// Verdict invariants
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// AS-11: is_clean XOR is_contaminated always holds.
    #[test]
    fn as11_verdict_is_clean_xor_contaminated(
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

    /// AS-12: ScanVerdict::Clean serde roundtrip.
    #[test]
    fn as12_verdict_clean_serde_roundtrip(_dummy in 0..1u8) {
        let v = ScanVerdict::Clean;
        let json = serde_json::to_string(&v).unwrap();
        let decoded: ScanVerdict = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(decoded, v);
    }

    /// AS-13: ScanVerdict::Contaminated serde roundtrip.
    #[test]
    fn as13_verdict_contaminated_serde_roundtrip(
        findings in prop::collection::vec(arb_scan_finding(), 0..5),
        reason in "[a-z ]{10,50}",
    ) {
        let v = ScanVerdict::Contaminated(ScanContamination {
            findings: findings.clone(),
            abort_reason: reason.clone(),
        });
        let json = serde_json::to_string(&v).unwrap();
        let decoded: ScanVerdict = serde_json::from_str(&json).unwrap();
        // Can't use eq due to f64 precision loss in entropy field
        if let ScanVerdict::Contaminated(dc) = &decoded {
            prop_assert_eq!(dc.findings.len(), findings.len());
            prop_assert_eq!(&dc.abort_reason, &reason);
            for (orig, dec) in findings.iter().zip(dc.findings.iter()) {
                prop_assert_eq!(&orig.pattern_name, &dec.pattern_name);
                prop_assert_eq!(orig.block_index, dec.block_index);
                prop_assert_eq!(&orig.source, &dec.source);
                prop_assert_eq!(&orig.detection_method, &dec.detection_method);
                match (orig.entropy, dec.entropy) {
                    (Some(a), Some(b)) => prop_assert!((a - b).abs() < 1e-10),
                    (None, None) => {}
                    _ => prop_assert!(false, "entropy mismatch"),
                }
            }
        } else {
            prop_assert!(false, "expected Contaminated verdict");
        }
    }
}

// =============================================================================
// Config serde invariants
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// AS-14: ArsScanConfig serde roundtrip.
    #[test]
    fn as14_config_serde_roundtrip(config in arb_config()) {
        let json = serde_json::to_string(&config).unwrap();
        let decoded: ArsScanConfig = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(decoded.min_entropy_token_len, config.min_entropy_token_len);
        prop_assert_eq!(decoded.max_entropy_token_len, config.max_entropy_token_len);
        prop_assert_eq!(decoded.scan_output, config.scan_output);
        prop_assert_eq!(decoded.entropy_detection_enabled, config.entropy_detection_enabled);
        let diff = (decoded.entropy_threshold - config.entropy_threshold).abs();
        prop_assert!(diff < 1e-10, "entropy_threshold drift: {}", diff);
    }

    /// AS-15: Any valid config produces a working scanner (no panics).
    #[test]
    fn as15_scanner_with_any_config_does_not_panic(
        config in arb_config(),
        window in arb_clean_window(0, 5)
    ) {
        let scanner = ArsSecretScanner::new(config);
        let _verdict = scanner.scan_commands(&window);
    }

    /// AS-16: Config with extra_patterns serde roundtrip.
    #[test]
    fn as16_config_extra_patterns_serde(
        patterns in prop::collection::vec("[A-Z_]{3,10}", 0..5),
    ) {
        let config = ArsScanConfig {
            extra_patterns: patterns.clone(),
            ..Default::default()
        };
        let json = serde_json::to_string(&config).unwrap();
        let decoded: ArsScanConfig = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(decoded.extra_patterns, patterns);
    }

    /// AS-17: Default config has expected default values.
    #[test]
    fn as17_default_config_values(_dummy in 0u8..1) {
        let cfg = ArsScanConfig::default();
        prop_assert!((cfg.entropy_threshold - 4.0).abs() < f64::EPSILON);
        prop_assert_eq!(cfg.min_entropy_token_len, 16);
        prop_assert_eq!(cfg.max_entropy_token_len, 256);
        prop_assert!(cfg.scan_output);
        prop_assert!(cfg.entropy_detection_enabled);
        prop_assert!(cfg.extra_patterns.is_empty());
    }
}

// =============================================================================
// ScanStats invariants
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// AS-18: total_scans = clean_count + contaminated_count.
    #[test]
    fn as18_stats_total_equals_clean_plus_contaminated(
        clean_count in 0..10u64,
        contam_count in 0..10u64,
    ) {
        let mut stats = ScanStats::new();
        for _ in 0..clean_count {
            stats.record(&ScanVerdict::Clean);
        }
        for _ in 0..contam_count {
            let v = ScanVerdict::Contaminated(ScanContamination {
                findings: vec![ScanFinding {
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
        );
        prop_assert_eq!(stats.clean_count, clean_count);
        prop_assert_eq!(stats.contaminated_count, contam_count);
        prop_assert_eq!(stats.total_findings, contam_count);
    }

    /// AS-19: findings_by_pattern sum equals total_findings.
    #[test]
    fn as19_stats_findings_by_pattern_sum(
        n_findings in 1..20usize,
    ) {
        let mut stats = ScanStats::new();
        let patterns = ["openai_key", "aws_key", "github_token"];

        for i in 0..n_findings {
            let pattern = patterns[i % patterns.len()];
            let v = ScanVerdict::Contaminated(ScanContamination {
                findings: vec![ScanFinding {
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
        prop_assert_eq!(pattern_sum, stats.total_findings);
    }

    /// AS-20: findings_by_method sum equals total_findings.
    #[test]
    fn as20_stats_findings_by_method_sum(
        n_pattern in 0..10usize,
        n_entropy in 0..10usize,
    ) {
        let mut stats = ScanStats::new();
        for _ in 0..n_pattern {
            let v = ScanVerdict::Contaminated(ScanContamination {
                findings: vec![ScanFinding {
                    pattern_name: "test_pat".to_string(),
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
        for _ in 0..n_entropy {
            let v = ScanVerdict::Contaminated(ScanContamination {
                findings: vec![ScanFinding {
                    pattern_name: "high_entropy".to_string(),
                    block_index: 0,
                    source: "command".to_string(),
                    byte_offset: 0,
                    match_len: 20,
                    context_redacted: "...".to_string(),
                    detection_method: DetectionMethod::EntropyThreshold,
                    entropy: Some(4.5),
                }],
                abort_reason: "test".to_string(),
            });
            stats.record(&v);
        }

        let method_sum: u64 = stats.findings_by_method.values().sum();
        prop_assert_eq!(method_sum, stats.total_findings);
    }

    /// AS-21: New ScanStats starts at all zeros.
    #[test]
    fn as21_stats_starts_empty(_dummy in 0u8..1) {
        let stats = ScanStats::new();
        prop_assert_eq!(stats.total_scans, 0);
        prop_assert_eq!(stats.clean_count, 0);
        prop_assert_eq!(stats.contaminated_count, 0);
        prop_assert_eq!(stats.total_findings, 0);
        prop_assert!(stats.findings_by_pattern.is_empty());
        prop_assert!(stats.findings_by_method.is_empty());
    }

    /// AS-22: ScanStats serde roundtrip.
    #[test]
    fn as22_stats_serde_roundtrip(
        total_scans in 0u64..100,
        clean_count in 0u64..100,
        contaminated_count in 0u64..100,
        total_findings in 0u64..100,
    ) {
        let stats = ScanStats {
            total_scans,
            clean_count,
            contaminated_count,
            total_findings,
            findings_by_pattern: std::collections::HashMap::new(),
            findings_by_method: std::collections::HashMap::new(),
        };
        let json = serde_json::to_string(&stats).unwrap();
        let back: ScanStats = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.total_scans, total_scans);
        prop_assert_eq!(back.clean_count, clean_count);
        prop_assert_eq!(back.contaminated_count, contaminated_count);
        prop_assert_eq!(back.total_findings, total_findings);
    }
}

// =============================================================================
// ScanFinding & ScanContamination serde
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    /// AS-23: ScanFinding serde roundtrip.
    #[test]
    fn as23_finding_serde_roundtrip(finding in arb_scan_finding()) {
        let json = serde_json::to_string(&finding).unwrap();
        let back: ScanFinding = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(finding.pattern_name, back.pattern_name);
        prop_assert_eq!(finding.block_index, back.block_index);
        prop_assert_eq!(finding.source, back.source);
        prop_assert_eq!(finding.byte_offset, back.byte_offset);
        prop_assert_eq!(finding.match_len, back.match_len);
        prop_assert_eq!(finding.detection_method, back.detection_method);
        // f64 entropy needs tolerance
        match (finding.entropy, back.entropy) {
            (Some(a), Some(b)) => prop_assert!((a - b).abs() < 1e-10),
            (None, None) => {}
            _ => prop_assert!(false, "entropy mismatch"),
        }
    }

    /// AS-24: ScanContamination serde roundtrip.
    #[test]
    fn as24_contamination_serde_roundtrip(
        findings in prop::collection::vec(arb_scan_finding(), 0..5),
        reason in "[a-z ]{10,50}",
    ) {
        let c = ScanContamination {
            findings: findings.clone(),
            abort_reason: reason.clone(),
        };
        let json = serde_json::to_string(&c).unwrap();
        let back: ScanContamination = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.findings.len(), findings.len());
        prop_assert_eq!(back.abort_reason, reason);
    }

    /// AS-25: DetectionMethod serde roundtrip.
    #[test]
    fn as25_detection_method_serde(method in arb_detection_method()) {
        let json = serde_json::to_string(&method).unwrap();
        let back: DetectionMethod = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(method, back);
    }
}

// =============================================================================
// Findings contain no raw secrets
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    /// AS-26: Finding contexts are redacted (short or masked).
    #[test]
    fn as26_findings_context_is_redacted(
        secret_cmd in arb_secret_command()
    ) {
        let scanner = ArsSecretScanner::with_defaults();
        let cmds = vec![arb_command_block(0, secret_cmd.clone())];
        let verdict = scanner.scan_commands(&cmds);

        if let ScanVerdict::Contaminated(c) = &verdict {
            for finding in &c.findings {
                let ctx_len = finding.context_redacted.len();
                prop_assert!(
                    ctx_len <= 50 || finding.context_redacted.contains('['),
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

    /// AS-27: Standalone scan consistent with command scan.
    #[test]
    fn as27_standalone_scan_consistent_with_command_scan(
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

        let standalone_clean = standalone_findings.iter()
            .all(|f| f.detection_method != DetectionMethod::PatternMatch);
        let is_verdict_clean = matches!(verdict, ScanVerdict::Clean);

        if standalone_clean {
            prop_assert!(is_verdict_clean, "standalone and command scan should agree");
        }
    }

    /// AS-28: Standalone scan on known prefix always finds at least one match.
    #[test]
    fn as28_standalone_finds_known_prefix(prefix in arb_known_prefix()) {
        let config = ArsScanConfig {
            entropy_detection_enabled: false,
            ..Default::default()
        };
        let scanner = ArsSecretScanner::new(config);
        let text = format!("some text {prefix}abcdef more text");
        let findings = scanner.scan_text_standalone(&text);
        let has_pattern = findings.iter().any(|f| f.detection_method == DetectionMethod::PatternMatch);
        prop_assert!(has_pattern, "prefix '{}' not found by standalone scan", prefix);
    }
}

// =============================================================================
// Scanner extra patterns
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    /// AS-29: Extra patterns are detected in scan.
    #[test]
    fn as29_extra_patterns_detected(
        pattern in "[A-Z_]{5,12}",
    ) {
        let config = ArsScanConfig {
            extra_patterns: vec![pattern.clone()],
            entropy_detection_enabled: false,
            ..Default::default()
        };
        let scanner = ArsSecretScanner::new(config);
        let text = format!("some text {pattern} more text");
        let cmds = vec![arb_command_block(0, text)];
        let verdict = scanner.scan_commands(&cmds);
        prop_assert!(verdict.is_contaminated(), "extra pattern '{}' should be detected", pattern);
    }

    /// AS-30: scan_output=false skips output field.
    #[test]
    fn as30_scan_output_disabled_skips_output(prefix in arb_known_prefix()) {
        let config = ArsScanConfig {
            scan_output: false,
            entropy_detection_enabled: false,
            ..Default::default()
        };
        let scanner = ArsSecretScanner::new(config);
        let cmds = vec![arb_command_block_with_output(
            0,
            "echo hello".to_string(),
            format!("leaked {prefix}abcdef"),
        )];
        let verdict = scanner.scan_commands(&cmds);
        // Command is clean, secret is only in output, output scanning disabled
        prop_assert!(verdict.is_clean(), "should not scan output when disabled");
    }

    /// AS-31: scan_output=true detects secrets in output field.
    #[test]
    fn as31_scan_output_enabled_detects(prefix in arb_known_prefix()) {
        let config = ArsScanConfig {
            scan_output: true,
            entropy_detection_enabled: false,
            ..Default::default()
        };
        let scanner = ArsSecretScanner::new(config);
        let cmds = vec![arb_command_block_with_output(
            0,
            "echo hello".to_string(),
            format!("leaked {prefix}abcdef"),
        )];
        let verdict = scanner.scan_commands(&cmds);
        prop_assert!(verdict.is_contaminated(), "should detect secret in output when enabled");
    }
}

// =============================================================================
// Multi-finding count invariants
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    /// AS-32: Number of findings >= number of injected secret commands.
    #[test]
    fn as32_finding_count_at_least_injected(
        n_secrets in 1usize..5,
        secrets in prop::collection::vec(arb_secret_command(), 1..5),
    ) {
        let actual_secrets = &secrets[..n_secrets.min(secrets.len())];
        let scanner = ArsSecretScanner::new(ArsScanConfig {
            entropy_detection_enabled: false,
            ..Default::default()
        });
        let cmds: Vec<CommandBlock> = actual_secrets.iter().enumerate()
            .map(|(i, s)| arb_command_block(i as u32, s.clone()))
            .collect();
        let verdict = scanner.scan_commands(&cmds);
        if let ScanVerdict::Contaminated(c) = &verdict {
            let pattern_findings = c.findings.iter()
                .filter(|f| f.detection_method == DetectionMethod::PatternMatch)
                .count();
            prop_assert!(
                pattern_findings >= actual_secrets.len(),
                "expected >= {} pattern findings, got {}",
                actual_secrets.len(), pattern_findings
            );
        } else {
            prop_assert!(false, "should be contaminated with {} secret commands", actual_secrets.len());
        }
    }

    /// AS-33: Empty command list always produces Clean verdict.
    #[test]
    fn as33_empty_commands_clean(config in arb_config()) {
        let scanner = ArsSecretScanner::new(config);
        let verdict = scanner.scan_commands(&[]);
        prop_assert!(verdict.is_clean());
    }
}

// =============================================================================
// CommandBlock with various fields
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    /// AS-34: Scanner handles commands with None exit_code and output.
    #[test]
    fn as34_none_fields_no_panic(cmd in arb_clean_command()) {
        let block = CommandBlock {
            index: 0,
            command: cmd,
            exit_code: None,
            duration_us: None,
            output_preview: None,
            timestamp_us: 1_000_000,
        };
        let scanner = ArsSecretScanner::with_defaults();
        let _verdict = scanner.scan_commands(&[block]);
    }

    /// AS-35: Scanner handles large command sequences without panic.
    #[test]
    fn as35_large_sequence_no_panic(
        window in arb_clean_window(10, 50),
    ) {
        let scanner = ArsSecretScanner::new(ArsScanConfig {
            entropy_detection_enabled: false,
            ..Default::default()
        });
        let _verdict = scanner.scan_commands(&window);
    }

    /// AS-36: with_defaults() creates a working scanner.
    #[test]
    fn as36_with_defaults_works(_dummy in 0u8..1) {
        let scanner = ArsSecretScanner::with_defaults();
        let verdict = scanner.scan_commands(&[]);
        prop_assert!(verdict.is_clean());
    }

    /// AS-37: ScanStats record preserves monotonic increments.
    #[test]
    fn as37_stats_monotonic_increments(n in 1u64..20) {
        let mut stats = ScanStats::new();
        for i in 0..n {
            stats.record(&ScanVerdict::Clean);
            prop_assert_eq!(stats.total_scans, i + 1);
            prop_assert_eq!(stats.clean_count, i + 1);
        }
    }

    /// AS-38: Multi-finding contaminated verdict has correct finding count.
    #[test]
    fn as38_multi_finding_verdict(n_findings in 1usize..8) {
        let findings: Vec<ScanFinding> = (0..n_findings).map(|i| ScanFinding {
            pattern_name: format!("pat_{i}"),
            block_index: 0,
            source: "command".to_string(),
            byte_offset: i * 10,
            match_len: 5,
            context_redacted: "...".to_string(),
            detection_method: DetectionMethod::PatternMatch,
            entropy: None,
        }).collect();
        let v = ScanVerdict::Contaminated(ScanContamination {
            findings: findings.clone(),
            abort_reason: "test".to_string(),
        });
        if let ScanVerdict::Contaminated(c) = &v {
            prop_assert_eq!(c.findings.len(), n_findings);
        }
        let is_contam = v.is_contaminated();
        let is_clean = v.is_clean();
        prop_assert!(is_contam);
        prop_assert!(!is_clean);
    }
}
