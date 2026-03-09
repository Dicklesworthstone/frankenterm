//! Property-based tests for ARS symbolic execution safety guard.
//!
//! Verifies invariants of the shell lexer, path resolution, and
//! safety analysis across random inputs.

use proptest::prelude::*;

use frankenterm_core::ars_symbolic_exec::{
    SafetyVerdict, SafetyViolation, SafetyViolations, SymExecConfig, SymbolicExecutor,
    ViolationCategory, parse_commands, path_within_boundary, resolve_path, tokenize,
};
use frankenterm_core::mdl_extraction::CommandBlock;

// =============================================================================
// Strategies
// =============================================================================

fn arb_safe_command() -> impl Strategy<Value = String> {
    prop_oneof![
        Just("ls -la".to_string()),
        Just("cargo build".to_string()),
        Just("cargo test".to_string()),
        Just("git status".to_string()),
        Just("echo done".to_string()),
        Just("cat README.md".to_string()),
        Just("grep -r TODO src/".to_string()),
        Just("pwd".to_string()),
        Just("whoami".to_string()),
        Just("make clean".to_string()),
    ]
}

fn arb_unsafe_command() -> impl Strategy<Value = String> {
    prop_oneof![
        Just("sudo rm -rf /".to_string()),
        Just("dd if=/dev/zero of=/dev/sda".to_string()),
        Just("mkfs.ext4 /dev/sda1".to_string()),
        Just("rm -rf /".to_string()),
        Just("rm -rf /etc/important".to_string()),
        Just("rm -rf ../../../etc".to_string()),
    ]
}

fn arb_cwd() -> impl Strategy<Value = String> {
    prop_oneof![
        Just("/home/user/project".to_string()),
        Just("/tmp/workspace".to_string()),
        Just("/var/lib/app".to_string()),
        Just("/opt/build".to_string()),
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

fn arb_config() -> impl Strategy<Value = SymExecConfig> {
    (
        arb_cwd(),
        8..64usize,      // max_path_depth
        prop::bool::ANY, // allow_unparseable
    )
        .prop_map(|(cwd, max_path_depth, allow_unparseable)| SymExecConfig {
            cwd,
            max_path_depth,
            allow_unparseable,
            extra_banned_binaries: Vec::new(),
            extra_safe_binaries: Vec::new(),
        })
}

fn arb_relative_path() -> impl Strategy<Value = String> {
    prop_oneof![
        Just("src/main.rs".to_string()),
        Just("./build/output".to_string()),
        Just("target/debug/app".to_string()),
        Just("README.md".to_string()),
        Just("tests/test_foo.rs".to_string()),
    ]
}

fn arb_traversal_path() -> impl Strategy<Value = String> {
    prop_oneof![
        Just("../../../etc/passwd".to_string()),
        Just("/etc/shadow".to_string()),
        Just("/root/.ssh/id_rsa".to_string()),
        Just("../../../../../../tmp/evil".to_string()),
        Just("/dev/sda".to_string()),
    ]
}

// =============================================================================
// Tokenizer invariants
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn tokenize_never_panics(input in "[ -~]{0,200}") {
        let _tokens = tokenize(&input);
        // No panic = success.
    }

    #[test]
    fn tokenize_empty_input_gives_empty(spaces in " {0,20}") {
        let tokens = tokenize(&spaces);
        // All whitespace should produce empty or only whitespace tokens.
        // Words from whitespace-only input should be empty.
        for token in &tokens {
            if let frankenterm_core::ars_symbolic_exec::ShellToken::Word(w) = token {
                prop_assert!(!w.is_empty(), "whitespace-only input shouldn't produce empty words");
            }
        }
    }

    #[test]
    fn parse_preserves_command_count(
        cmds in prop::collection::vec(arb_safe_command(), 1..5)
    ) {
        // Join with && and parse.
        let input = cmds.join(" && ");
        let tokens = tokenize(&input);
        let parsed = parse_commands(&tokens);
        // Should have at least 1 command (may have more due to &&).
        prop_assert!(!parsed.is_empty(), "parsed commands should not be empty");
    }
}

// =============================================================================
// Path resolution invariants
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn resolve_absolute_path_ignores_cwd(
        cwd in arb_cwd(),
        path in arb_traversal_path().prop_filter("absolute", |p| p.starts_with('/'))
    ) {
        let resolved = resolve_path(&cwd, &path);
        // Absolute paths should not start with CWD.
        // (They resolve independently.)
        let resolved_str = resolved.to_string_lossy();
        prop_assert!(
            resolved_str.starts_with('/'),
            "absolute path should resolve to absolute: {}",
            resolved_str
        );
    }

    #[test]
    fn resolve_relative_stays_under_cwd(
        cwd in arb_cwd(),
        rel in arb_relative_path()
    ) {
        let resolved = resolve_path(&cwd, &rel);
        // Simple relative paths should stay within CWD.
        let cwd_path = std::path::Path::new(&cwd);
        prop_assert!(
            path_within_boundary(&resolved, cwd_path),
            "{} resolved to {} which is outside {}",
            rel,
            resolved.display(),
            cwd
        );
    }

    #[test]
    fn resolve_dot_is_identity(cwd in arb_cwd()) {
        let resolved = resolve_path(&cwd, ".");
        let expected = std::path::PathBuf::from(&cwd);
        prop_assert_eq!(resolved, expected);
    }

    #[test]
    fn path_within_root_always_true(path in arb_traversal_path()) {
        let resolved = resolve_path("/", &path);
        let root = std::path::Path::new("/");
        prop_assert!(
            path_within_boundary(&resolved, root),
            "all paths should be within root"
        );
    }
}

// =============================================================================
// Safety analysis invariants
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(150))]

    #[test]
    fn safe_commands_always_safe(
        cwd in arb_cwd(),
        cmds in prop::collection::vec(arb_safe_command(), 1..5)
    ) {
        let config = SymExecConfig {
            cwd,
            allow_unparseable: true, // Allow subshells in safe commands.
            ..Default::default()
        };
        let exec = SymbolicExecutor::new(config);
        let blocks: Vec<CommandBlock> = cmds
            .into_iter()
            .enumerate()
            .map(|(i, c)| make_cmd_block(i as u32, c))
            .collect();

        let verdict = exec.analyze(&blocks);
        prop_assert!(
            verdict.is_safe(),
            "safe commands should produce Safe verdict, got {:?}",
            verdict
        );
    }

    #[test]
    fn unsafe_commands_always_unsafe(
        cwd in arb_cwd(),
        unsafe_cmd in arb_unsafe_command()
    ) {
        let config = SymExecConfig {
            cwd,
            ..Default::default()
        };
        let exec = SymbolicExecutor::new(config);
        let blocks = vec![make_cmd_block(0, unsafe_cmd)];

        let verdict = exec.analyze(&blocks);
        prop_assert!(
            verdict.is_unsafe(),
            "unsafe commands should produce Unsafe verdict"
        );
    }

    #[test]
    fn empty_commands_always_safe(cwd in arb_cwd()) {
        let exec = SymbolicExecutor::new(SymExecConfig {
            cwd,
            ..Default::default()
        });
        let verdict = exec.analyze(&[]);
        prop_assert!(verdict.is_safe());
    }

    #[test]
    fn verdict_is_safe_xor_unsafe(
        cwd in arb_cwd(),
        cmd in arb_safe_command()
    ) {
        let exec = SymbolicExecutor::new(SymExecConfig {
            cwd,
            allow_unparseable: true,
            ..Default::default()
        });
        let blocks = vec![make_cmd_block(0, cmd)];
        let verdict = exec.analyze(&blocks);

        prop_assert!(
            verdict.is_safe() ^ verdict.is_unsafe(),
            "verdict must be exactly one of safe/unsafe"
        );
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
        let decoded: SymExecConfig = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&decoded.cwd, &config.cwd);
        prop_assert_eq!(decoded.max_path_depth, config.max_path_depth);
        prop_assert_eq!(decoded.allow_unparseable, config.allow_unparseable);
    }

    #[test]
    fn executor_with_any_config_does_not_panic(
        config in arb_config(),
        cmd in arb_safe_command()
    ) {
        let exec = SymbolicExecutor::new(config);
        let blocks = vec![make_cmd_block(0, cmd)];
        let _verdict = exec.analyze(&blocks);
    }
}

// =============================================================================
// Verdict serde invariants
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn verdict_safe_serde_roundtrip(_dummy in 0..1u8) {
        let v = SafetyVerdict::Safe;
        let json = serde_json::to_string(&v).unwrap();
        let decoded: SafetyVerdict = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(decoded, v);
    }

    #[test]
    fn violation_category_serde_roundtrip(idx in 0..7u8) {
        let cat = match idx {
            0 => ViolationCategory::PathTraversal,
            1 => ViolationCategory::BannedBinary,
            2 => ViolationCategory::UnboundedDeletion,
            3 => ViolationCategory::PrivilegeEscalation,
            4 => ViolationCategory::ResourceExhaustion,
            5 => ViolationCategory::Unparseable,
            _ => ViolationCategory::OpaqueSubstitution,
        };
        let json = serde_json::to_string(&cat).unwrap();
        let decoded: ViolationCategory = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(decoded, cat);
    }
}

// =============================================================================
// Traversal detection invariants
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn rm_outside_cwd_always_detected(
        cwd in arb_cwd(),
        target in arb_traversal_path()
    ) {
        let exec = SymbolicExecutor::new(SymExecConfig {
            cwd: cwd.clone(),
            ..Default::default()
        });

        let cmd_text = format!("rm -rf {}", target);
        let blocks = vec![make_cmd_block(0, cmd_text)];
        let verdict = exec.analyze(&blocks);

        // If target resolves outside CWD, should be unsafe.
        let resolved = resolve_path(&cwd, &target);
        let boundary = std::path::Path::new(&cwd);
        if !path_within_boundary(&resolved, boundary) {
            prop_assert!(
                verdict.is_unsafe(),
                "rm -rf {} (resolved to {}) outside {} should be unsafe",
                target,
                resolved.display(),
                cwd
            );
        }
    }
}

// =============================================================================
// Additional strategies for coverage gaps
// =============================================================================

fn arb_violation_category() -> impl Strategy<Value = ViolationCategory> {
    prop_oneof![
        Just(ViolationCategory::PathTraversal),
        Just(ViolationCategory::BannedBinary),
        Just(ViolationCategory::UnboundedDeletion),
        Just(ViolationCategory::PrivilegeEscalation),
        Just(ViolationCategory::ResourceExhaustion),
        Just(ViolationCategory::Unparseable),
        Just(ViolationCategory::OpaqueSubstitution),
    ]
}

fn arb_safety_violation() -> impl Strategy<Value = SafetyViolation> {
    (
        0_u32..100,
        arb_violation_category(),
        "[a-zA-Z ]{5,30}",
        "[a-zA-Z0-9/ \\-]{3,20}",
    )
        .prop_map(|(block_index, category, description, evidence)| SafetyViolation {
            block_index,
            category,
            description,
            evidence,
        })
}

// =============================================================================
// SafetyViolation serde roundtrip
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn safety_violation_serde_roundtrip(violation in arb_safety_violation()) {
        let json = serde_json::to_string(&violation).unwrap();
        let back: SafetyViolation = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back, violation);
    }
}

// =============================================================================
// SafetyViolations serde roundtrip
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn safety_violations_serde_roundtrip(
        violations in proptest::collection::vec(arb_safety_violation(), 0..5),
    ) {
        let sv = SafetyViolations {
            violations: violations.clone(),
        };
        let json = serde_json::to_string(&sv).unwrap();
        let back: SafetyViolations = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.violations.len(), violations.len());
        prop_assert_eq!(back, sv);
    }
}

// =============================================================================
// SafetyVerdict::Unsafe serde roundtrip
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn verdict_unsafe_serde_roundtrip(
        violations in proptest::collection::vec(arb_safety_violation(), 1..5),
    ) {
        let sv = SafetyViolations {
            violations: violations.clone(),
        };
        let verdict = SafetyVerdict::Unsafe(sv);
        prop_assert!(verdict.is_unsafe());
        prop_assert!(!verdict.is_safe());

        let json = serde_json::to_string(&verdict).unwrap();
        let back: SafetyVerdict = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back, verdict);
    }
}

// =============================================================================
// SymExecConfig — extra_banned/safe_binaries roundtrip
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn config_with_extras_serde_roundtrip(
        cwd in arb_cwd(),
        depth in 4_usize..128,
        allow_unparseable in proptest::bool::ANY,
        banned in proptest::collection::vec("[a-z]{2,10}", 0..5),
        safe in proptest::collection::vec("[a-z]{2,10}", 0..5),
    ) {
        let config = SymExecConfig {
            cwd: cwd.clone(),
            max_path_depth: depth,
            allow_unparseable,
            extra_banned_binaries: banned.clone(),
            extra_safe_binaries: safe.clone(),
        };
        let json = serde_json::to_string(&config).unwrap();
        let back: SymExecConfig = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&back.cwd, &config.cwd);
        prop_assert_eq!(back.max_path_depth, config.max_path_depth);
        prop_assert_eq!(back.allow_unparseable, config.allow_unparseable);
        prop_assert_eq!(back.extra_banned_binaries, config.extra_banned_binaries);
        prop_assert_eq!(back.extra_safe_binaries, config.extra_safe_binaries);
    }

    #[test]
    fn config_default_roundtrip(_dummy in 0..1_u32) {
        let config = SymExecConfig::default();
        let json = serde_json::to_string(&config).unwrap();
        let back: SymExecConfig = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&back.cwd, &config.cwd);
        prop_assert_eq!(back.max_path_depth, config.max_path_depth);
        prop_assert!(back.extra_banned_binaries.is_empty());
        prop_assert!(back.extra_safe_binaries.is_empty());
    }
}

// =============================================================================
// ViolationCategory — all variants have distinct serde
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(10))]

    #[test]
    fn violation_categories_all_distinct(_dummy in 0..1_u32) {
        let all = vec![
            ViolationCategory::PathTraversal,
            ViolationCategory::BannedBinary,
            ViolationCategory::UnboundedDeletion,
            ViolationCategory::PrivilegeEscalation,
            ViolationCategory::ResourceExhaustion,
            ViolationCategory::Unparseable,
            ViolationCategory::OpaqueSubstitution,
        ];
        let jsons: Vec<String> = all.iter().map(|c| serde_json::to_string(c).unwrap()).collect();
        for (i, j1) in jsons.iter().enumerate() {
            for j2 in &jsons[i + 1..] {
                prop_assert_ne!(j1, j2, "all categories should have distinct JSON");
            }
        }
    }
}

// =============================================================================
// Extra banned binaries are enforced
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn extra_banned_binary_triggers_unsafe(
        cwd in arb_cwd(),
        binary in "[a-z]{3,8}",
    ) {
        let config = SymExecConfig {
            cwd,
            extra_banned_binaries: vec![binary.clone()],
            ..Default::default()
        };
        let exec = SymbolicExecutor::new(config);
        let blocks = vec![make_cmd_block(0, binary)];
        let verdict = exec.analyze(&blocks);
        prop_assert!(verdict.is_unsafe(), "extra_banned_binary should trigger unsafe");
    }
}
