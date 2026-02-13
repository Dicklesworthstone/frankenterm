//! Property-based tests for command_guard.rs (destructive command blocking).
//!
//! Bead: wa-7mik
//!
//! Validates:
//! 1. GuardDecision: exactly one of is_blocked/is_warning/is_allowed
//! 2. ReadOnly trust: always allows all commands
//! 3. Strict mode: known destructive commands blocked (not warned)
//! 4. Permissive mode: known destructive commands warned (not blocked)
//! 5. Safe whitelist: rm -rf node_modules/target always allowed
//! 6. evaluate_stateless matches evaluate for strict guard
//! 7. Preflight does not modify audit count
//! 8. Audit log count matches evaluate calls (up to capacity)
//! 9. Audit ring buffer never exceeds capacity
//! 10. clear_audit_log resets count to 0
//! 11. GuardDecision serde roundtrip
//! 12. TrustLevel serde roundtrip
//! 13. TrustLevel Display non-empty snake_case
//! 14. PaneGuardConfig serde roundtrip
//! 15. GuardPolicy serde roundtrip
//! 16. AuditEntry serde roundtrip
//! 17. Per-pane config overrides default trust
//! 18. Disabled packs allow their patterns
//! 19. Block decisions have non-empty rule_id
//! 20. Warn decisions have non-empty rule_id
//! 21. Available packs returns exactly 8 packs
//! 22. Commands without pack keywords always allowed

use proptest::prelude::*;

use frankenterm_core::command_guard::{
    evaluate_stateless, AuditEntry, CommandGuard, GuardDecision, GuardPolicy, PaneGuardConfig,
    TrustLevel,
};

// =============================================================================
// Strategies
// =============================================================================

fn arb_trust_level() -> impl Strategy<Value = TrustLevel> {
    prop_oneof![
        Just(TrustLevel::Strict),
        Just(TrustLevel::Permissive),
        Just(TrustLevel::ReadOnly),
    ]
}

/// Known destructive commands that should be blocked/warned.
fn arb_destructive_command() -> impl Strategy<Value = &'static str> {
    prop_oneof![
        Just("rm -rf /tmp/data"),
        Just("rm -rf ~"),
        Just("git push --force origin main"),
        Just("git reset --hard HEAD~1"),
        Just("git clean -fd"),
        Just("git branch -D feature-old"),
        Just("git stash clear"),
        Just("DROP TABLE users"),
        Just("DROP DATABASE production"),
        Just("TRUNCATE TABLE sessions"),
        Just("DELETE FROM users;"),
        Just("docker system prune -af"),
        Just("docker volume prune"),
        Just("kubectl delete namespace production"),
        Just("kubectl delete pods --all"),
        Just("helm uninstall my-release"),
        Just("terraform destroy -auto-approve"),
        Just("aws s3 rm s3://bucket --recursive"),
        Just("kill -9 1234"),
        Just("sudo reboot"),
        Just("npm unpublish my-package@1.0.0"),
        Just("cargo yank --version 1.0.0 my-crate"),
        Just("chmod -R 777 /var/www"),
    ]
}

/// Commands that should never be blocked (no pack keywords).
fn arb_safe_command() -> impl Strategy<Value = &'static str> {
    prop_oneof![
        Just("ls -la"),
        Just("cat file.txt"),
        Just("echo hello world"),
        Just("pwd"),
        Just("whoami"),
        Just("date"),
        Just("uname -a"),
        Just("env"),
        Just("printenv HOME"),
        Just("wc -l file.txt"),
        Just("head -n 10 log.txt"),
        Just("tail -f output.log"),
        Just("sort data.csv"),
        Just("uniq -c"),
        Just("diff a.txt b.txt"),
        Just("grep pattern file"),
        Just("sed 's/old/new/g' file"),
        Just("awk '{print $1}' data"),
        Just("curl https://example.com"),
        Just("wget https://example.com/file"),
    ]
}

/// Safe filesystem deletions (whitelisted: node_modules, target, etc.).
fn arb_whitelisted_rm() -> impl Strategy<Value = &'static str> {
    prop_oneof![
        Just("rm -rf node_modules"),
        Just("rm -rf target"),
        Just("rm -rf __pycache__"),
        Just("rm -rf .cache"),
        Just("rm -rf dist"),
        Just("rm -rf build"),
        Just("rm -rf .next"),
        Just("rm -rf .turbo"),
        Just("rm -rf tmp"),
    ]
}

// =============================================================================
// Property 1: GuardDecision exactly one predicate true
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn decision_exactly_one_predicate(
        cmd in prop_oneof![arb_destructive_command(), arb_safe_command()],
        trust in arb_trust_level(),
    ) {
        let mut guard = CommandGuard::new(GuardPolicy {
            default_trust: trust,
            ..GuardPolicy::default()
        });
        let d = guard.evaluate(cmd, 1);

        let blocked = d.is_blocked() as u8;
        let warning = d.is_warning() as u8;
        let allowed = d.is_allowed() as u8;
        prop_assert_eq!(blocked + warning + allowed, 1,
            "exactly one predicate should be true: blocked={}, warning={}, allowed={}",
            d.is_blocked(), d.is_warning(), d.is_allowed());
    }
}

// =============================================================================
// Property 2: ReadOnly trust always allows
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn readonly_always_allows(
        cmd in prop_oneof![arb_destructive_command(), arb_safe_command()],
    ) {
        let mut guard = CommandGuard::new(GuardPolicy {
            default_trust: TrustLevel::ReadOnly,
            ..GuardPolicy::default()
        });
        let d = guard.evaluate(cmd, 1);
        prop_assert!(d.is_allowed(),
            "ReadOnly trust should always allow, but got blocked/warned for: {}", cmd);
    }
}

// =============================================================================
// Property 3: Strict mode blocks destructive commands
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn strict_blocks_destructive(
        cmd in arb_destructive_command(),
    ) {
        let mut guard = CommandGuard::new(GuardPolicy {
            default_trust: TrustLevel::Strict,
            ..GuardPolicy::default()
        });
        let d = guard.evaluate(cmd, 1);
        prop_assert!(d.is_blocked(),
            "Strict trust should block destructive command: {}", cmd);
    }
}

// =============================================================================
// Property 4: Permissive mode warns on destructive commands
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn permissive_warns_destructive(
        cmd in arb_destructive_command(),
    ) {
        let mut guard = CommandGuard::new(GuardPolicy {
            default_trust: TrustLevel::Permissive,
            ..GuardPolicy::default()
        });
        let d = guard.evaluate(cmd, 1);
        prop_assert!(d.is_warning(),
            "Permissive trust should warn on destructive command: {}", cmd);
    }
}

// =============================================================================
// Property 5: Whitelisted rm patterns always allowed
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn whitelisted_rm_always_allowed(
        cmd in arb_whitelisted_rm(),
        trust in prop_oneof![Just(TrustLevel::Strict), Just(TrustLevel::Permissive)],
    ) {
        let mut guard = CommandGuard::new(GuardPolicy {
            default_trust: trust,
            ..GuardPolicy::default()
        });
        let d = guard.evaluate(cmd, 1);
        prop_assert!(d.is_allowed(),
            "whitelisted rm '{}' should be allowed even with {:?} trust", cmd, trust);
    }
}

// =============================================================================
// Property 6: evaluate_stateless agrees with strict guard
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn stateless_agrees_with_strict(
        cmd in prop_oneof![arb_destructive_command(), arb_safe_command(), arb_whitelisted_rm()],
    ) {
        let mut guard = CommandGuard::new(GuardPolicy {
            default_trust: TrustLevel::Strict,
            ..GuardPolicy::default()
        });
        let guard_decision = guard.evaluate(cmd, 1);
        let stateless_result = evaluate_stateless(cmd);

        // If stateless says destructive, guard should block (Strict mode)
        if let Some((rule_id, _, _, _)) = &stateless_result {
            prop_assert!(guard_decision.is_blocked(),
                "stateless detected rule '{}' but guard didn't block for: {}", rule_id, cmd);
        }

        // If guard says block, stateless should detect it
        if guard_decision.is_blocked() {
            prop_assert!(stateless_result.is_some(),
                "guard blocked '{}' but stateless allowed it", cmd);
        }
    }
}

// =============================================================================
// Property 7: Preflight does not modify audit count
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn preflight_no_audit(
        cmd in prop_oneof![arb_destructive_command(), arb_safe_command()],
        pane_id in 1u64..100,
    ) {
        let guard = CommandGuard::with_defaults();
        let count_before = guard.audit_count();
        let _d = guard.preflight(cmd, pane_id);
        prop_assert_eq!(guard.audit_count(), count_before,
            "preflight should not modify audit count");
    }
}

// =============================================================================
// Property 8: Audit count matches evaluate calls
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn audit_count_matches_calls(
        n_calls in 1usize..20,
    ) {
        let mut guard = CommandGuard::with_defaults();
        for i in 0..n_calls {
            guard.evaluate("ls -la", i as u64);
        }
        prop_assert_eq!(guard.audit_count(), n_calls,
            "audit count should match number of evaluate calls");
    }
}

// =============================================================================
// Property 9: Audit ring buffer never exceeds capacity
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn audit_never_exceeds_capacity(
        capacity in 3usize..20,
        n_calls in 1usize..50,
    ) {
        let mut guard = CommandGuard::new(GuardPolicy {
            audit_capacity: capacity,
            ..GuardPolicy::default()
        });
        for i in 0..n_calls {
            guard.evaluate("ls", i as u64);
        }
        prop_assert!(guard.audit_count() <= capacity,
            "audit count {} should not exceed capacity {}", guard.audit_count(), capacity);
    }
}

// =============================================================================
// Property 10: clear_audit_log resets count
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    #[test]
    fn clear_audit_resets(
        n_calls in 1usize..20,
    ) {
        let mut guard = CommandGuard::with_defaults();
        for i in 0..n_calls {
            guard.evaluate("ls", i as u64);
        }
        prop_assert!(guard.audit_count() > 0);
        guard.clear_audit_log();
        prop_assert_eq!(guard.audit_count(), 0, "clear should reset audit count");
    }
}

// =============================================================================
// Property 11: GuardDecision serde roundtrip
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn decision_allow_serde(_dummy in 0..1u32) {
        let d = GuardDecision::Allow;
        let json = serde_json::to_string(&d).unwrap();
        let back: GuardDecision = serde_json::from_str(&json).unwrap();
        prop_assert!(back.is_allowed());
    }

    #[test]
    fn decision_block_serde(
        rule_id in "[a-z.:-]{5,20}",
        pack in "[a-z._]{3,15}",
        reason in "[a-zA-Z0-9 ]{5,40}",
    ) {
        let d = GuardDecision::Block {
            rule_id: rule_id.clone(),
            pack: pack.clone(),
            reason: reason.clone(),
            suggestions: vec!["Use safer alternative".to_string()],
        };
        let json = serde_json::to_string(&d).unwrap();
        let back: GuardDecision = serde_json::from_str(&json).unwrap();
        prop_assert!(back.is_blocked());
        prop_assert_eq!(back.rule_id(), Some(rule_id.as_str()));
    }

    #[test]
    fn decision_warn_serde(
        rule_id in "[a-z.:-]{5,20}",
        pack in "[a-z._]{3,15}",
        reason in "[a-zA-Z0-9 ]{5,40}",
    ) {
        let d = GuardDecision::Warn {
            rule_id: rule_id.clone(),
            pack: pack.clone(),
            reason: reason.clone(),
        };
        let json = serde_json::to_string(&d).unwrap();
        let back: GuardDecision = serde_json::from_str(&json).unwrap();
        prop_assert!(back.is_warning());
        prop_assert_eq!(back.rule_id(), Some(rule_id.as_str()));
    }
}

// =============================================================================
// Property 12: TrustLevel serde roundtrip
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    #[test]
    fn trust_level_serde_roundtrip(trust in arb_trust_level()) {
        let json = serde_json::to_string(&trust).unwrap();
        let back: TrustLevel = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back, trust);
    }
}

// =============================================================================
// Property 13: TrustLevel Display non-empty snake_case
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    #[test]
    fn trust_level_display(trust in arb_trust_level()) {
        let s = trust.to_string();
        prop_assert!(!s.is_empty());
        prop_assert!(s.chars().all(|c| c.is_ascii_lowercase() || c == '_'),
            "Display '{}' should be snake_case", s);
    }
}

// =============================================================================
// Property 14: PaneGuardConfig serde roundtrip
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn pane_config_serde(
        trust in arb_trust_level(),
        budget_us in 10u64..10_000,
    ) {
        let config = PaneGuardConfig {
            trust_level: trust,
            enabled_packs: None,
            allowlist_patterns: vec![],
            budget_us,
        };
        let json = serde_json::to_string(&config).unwrap();
        let back: PaneGuardConfig = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.trust_level, trust);
        prop_assert_eq!(back.budget_us, budget_us);
    }
}

// =============================================================================
// Property 15: GuardPolicy serde roundtrip
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    #[test]
    fn guard_policy_serde(
        trust in arb_trust_level(),
        capacity in 100usize..10_000,
    ) {
        let policy = GuardPolicy {
            default_trust: trust,
            audit_capacity: capacity,
            ..GuardPolicy::default()
        };
        let json = serde_json::to_string(&policy).unwrap();
        let back: GuardPolicy = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.default_trust, trust);
        prop_assert_eq!(back.audit_capacity, capacity);
    }
}

// =============================================================================
// Property 16: AuditEntry serde roundtrip
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn audit_entry_serde(
        pane_id in 0u64..1000,
        command in "[a-zA-Z0-9 /_-]{3,50}",
        decision in prop_oneof![Just("allow"), Just("block"), Just("warn")],
        eval_us in 0u64..1_000_000,
        timestamp_s in 1_000_000_000u64..2_000_000_000,
    ) {
        let entry = AuditEntry {
            pane_id,
            command: command.clone(),
            decision: decision.to_string(),
            rule_id: Some("test:rule".to_string()),
            pack: Some("test".to_string()),
            eval_us,
            timestamp_s,
        };
        let json = serde_json::to_string(&entry).unwrap();
        let back: AuditEntry = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.pane_id, pane_id);
        prop_assert_eq!(back.command, command);
        prop_assert_eq!(back.eval_us, eval_us);
        prop_assert_eq!(back.timestamp_s, timestamp_s);
    }
}

// =============================================================================
// Property 17: Per-pane config overrides default trust
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn pane_override_applied(
        pane_id in 1u64..100,
        default_trust in prop_oneof![Just(TrustLevel::Strict), Just(TrustLevel::Permissive)],
    ) {
        let override_trust = if default_trust == TrustLevel::Strict {
            TrustLevel::ReadOnly
        } else {
            TrustLevel::Strict
        };

        let mut guard = CommandGuard::new(GuardPolicy {
            default_trust,
            ..GuardPolicy::default()
        });
        guard.set_pane_config(pane_id, PaneGuardConfig {
            trust_level: override_trust,
            ..PaneGuardConfig::default()
        });

        let destructive_cmd = "rm -rf /tmp/important";

        // Override pane should use override trust
        let d_override = guard.evaluate(destructive_cmd, pane_id);
        // Non-override pane should use default trust
        let d_default = guard.evaluate(destructive_cmd, pane_id + 1000);

        if override_trust == TrustLevel::ReadOnly {
            prop_assert!(d_override.is_allowed(),
                "ReadOnly override should allow");
        } else if override_trust == TrustLevel::Strict {
            prop_assert!(d_override.is_blocked(),
                "Strict override should block");
        }

        if default_trust == TrustLevel::Strict {
            prop_assert!(d_default.is_blocked(),
                "default Strict should block non-override pane");
        } else if default_trust == TrustLevel::Permissive {
            prop_assert!(d_default.is_warning(),
                "default Permissive should warn non-override pane");
        }
    }
}

// =============================================================================
// Property 18: Disabled packs allow their patterns
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    #[test]
    fn disabled_pack_allows(
        pack_to_disable in prop_oneof![
            Just("core.git"),
            Just("database"),
            Just("containers"),
            Just("kubernetes"),
            Just("cloud"),
            Just("system"),
            Just("package_managers"),
        ],
    ) {
        let cmd = match pack_to_disable {
            "core.git" => "git reset --hard HEAD",
            "database" => "DROP TABLE users",
            "containers" => "docker system prune",
            "kubernetes" => "kubectl delete namespace prod",
            "cloud" => "terraform destroy",
            "system" => "kill -9 1234",
            "package_managers" => "npm unpublish my-pkg",
            _ => unreachable!(),
        };

        let mut guard = CommandGuard::new(GuardPolicy {
            default_trust: TrustLevel::Strict,
            disabled_packs: vec![pack_to_disable.to_string()],
            ..GuardPolicy::default()
        });
        let d = guard.evaluate(cmd, 1);
        prop_assert!(d.is_allowed(),
            "command '{}' should be allowed when pack '{}' is disabled", cmd, pack_to_disable);
    }
}

// =============================================================================
// Property 19: Block decisions have non-empty rule_id
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn block_has_rule_id(
        cmd in arb_destructive_command(),
    ) {
        let mut guard = CommandGuard::new(GuardPolicy {
            default_trust: TrustLevel::Strict,
            ..GuardPolicy::default()
        });
        let d = guard.evaluate(cmd, 1);
        if d.is_blocked() {
            let rule = d.rule_id();
            prop_assert!(rule.is_some(), "blocked decision should have rule_id");
            prop_assert!(!rule.unwrap().is_empty(), "rule_id should not be empty");
        }
    }
}

// =============================================================================
// Property 20: Warn decisions have non-empty rule_id
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn warn_has_rule_id(
        cmd in arb_destructive_command(),
    ) {
        let mut guard = CommandGuard::new(GuardPolicy {
            default_trust: TrustLevel::Permissive,
            ..GuardPolicy::default()
        });
        let d = guard.evaluate(cmd, 1);
        if d.is_warning() {
            let rule = d.rule_id();
            prop_assert!(rule.is_some(), "warn decision should have rule_id");
            prop_assert!(!rule.unwrap().is_empty(), "rule_id should not be empty");
        }
    }
}

// =============================================================================
// Property 21: Available packs returns exactly 8 packs
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(5))]

    #[test]
    fn available_packs_count(_dummy in 0..1u32) {
        let packs = CommandGuard::available_packs();
        prop_assert_eq!(packs.len(), 8, "should have exactly 8 security packs");
        prop_assert!(packs.contains(&"core.filesystem"));
        prop_assert!(packs.contains(&"core.git"));
        prop_assert!(packs.contains(&"database"));
        prop_assert!(packs.contains(&"containers"));
        prop_assert!(packs.contains(&"kubernetes"));
        prop_assert!(packs.contains(&"cloud"));
        prop_assert!(packs.contains(&"system"));
        prop_assert!(packs.contains(&"package_managers"));
    }
}

// =============================================================================
// Property 22: Commands without pack keywords always allowed
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn no_keyword_always_allowed(
        cmd in arb_safe_command(),
        trust in prop_oneof![Just(TrustLevel::Strict), Just(TrustLevel::Permissive)],
    ) {
        let mut guard = CommandGuard::new(GuardPolicy {
            default_trust: trust,
            ..GuardPolicy::default()
        });
        let d = guard.evaluate(cmd, 1);
        prop_assert!(d.is_allowed(),
            "safe command '{}' should always be allowed with {:?} trust", cmd, trust);
    }
}

// =============================================================================
// Property 23: Rule IDs follow pack:rule format
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn rule_id_format_valid(
        cmd in arb_destructive_command(),
    ) {
        let mut guard = CommandGuard::new(GuardPolicy {
            default_trust: TrustLevel::Strict,
            ..GuardPolicy::default()
        });
        let d = guard.evaluate(cmd, 1);
        if let Some(rule_id) = d.rule_id() {
            // Rule IDs should contain a colon separator
            prop_assert!(rule_id.contains(':'),
                "rule_id '{}' should contain ':' separator", rule_id);
            let parts: Vec<&str> = rule_id.splitn(2, ':').collect();
            prop_assert_eq!(parts.len(), 2, "rule_id should have exactly 2 parts");
            prop_assert!(!parts[0].is_empty(), "pack part should not be empty");
            prop_assert!(!parts[1].is_empty(), "rule part should not be empty");
        }
    }
}

// =============================================================================
// Property 24: Allow decision has no rule_id
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn allow_no_rule_id(
        cmd in arb_safe_command(),
    ) {
        let mut guard = CommandGuard::with_defaults();
        let d = guard.evaluate(cmd, 1);
        prop_assert!(d.is_allowed());
        prop_assert!(d.rule_id().is_none(),
            "allowed decision should have no rule_id");
    }
}

// =============================================================================
// Property 25: set_policy invalidates cached state
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(20))]

    #[test]
    fn set_policy_changes_behavior(
        _dummy in 0..1u32,
    ) {
        let mut guard = CommandGuard::new(GuardPolicy {
            default_trust: TrustLevel::Strict,
            ..GuardPolicy::default()
        });

        // Initially blocks
        let d = guard.evaluate("rm -rf /tmp/data", 1);
        prop_assert!(d.is_blocked());

        // Switch to ReadOnly — now allows
        guard.set_policy(GuardPolicy {
            default_trust: TrustLevel::ReadOnly,
            ..GuardPolicy::default()
        });
        let d = guard.evaluate("rm -rf /tmp/data", 1);
        prop_assert!(d.is_allowed(),
            "after set_policy to ReadOnly, should allow");
    }
}
