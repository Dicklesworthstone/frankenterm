//! Property-based tests for the `frankenterm_core::config` module.
//!
//! Validates serde roundtrips, default invariants, enum semantics, and
//! filter/matching logic for the core configuration types:
//!
//! 1. LogFormat: serde roundtrip, Display, FromStr, case-insensitivity
//! 2. SyncDirection: serde roundtrip, default
//! 3. DistributedAuthMode: serde roundtrip, requires_token/requires_mtls, default
//! 4. SnapshotSchedulingMode: serde roundtrip, default
//! 5. PaneFilterRule: serde roundtrip, default, builder, matches logic, validate
//! 6. PaneFilterConfig: check_pane exclude-wins, include semantics, has_rules
//! 7. PanePriorityConfig: default priority, first-match-wins
//! 8. CaptureBudgetConfig: serde roundtrip, default values
//! 9. RetentionTier: serde roundtrip with optional fields
//! 10. StorageConfig: resolve_retention_days tier matching
//! 11. Config: default roundtrip, empty JSON parses OK
//! 12. SnapshotConfig: serde roundtrip, default values
//! 13. SnapshotSchedulingConfig: serde roundtrip, default values

use proptest::prelude::*;

use frankenterm_core::config::{
    CaptureBudgetConfig, Config, DistributedAuthMode, LogFormat, PaneFilterConfig, PaneFilterRule,
    PanePriorityConfig, PanePriorityRule, RetentionTier, SnapshotConfig, SnapshotSchedulingConfig,
    SnapshotSchedulingMode, StorageConfig, SyncDirection,
};

// =============================================================================
// Strategies
// =============================================================================

fn arb_log_format() -> impl Strategy<Value = LogFormat> {
    prop_oneof![Just(LogFormat::Pretty), Just(LogFormat::Json),]
}

fn arb_sync_direction() -> impl Strategy<Value = SyncDirection> {
    prop_oneof![Just(SyncDirection::Push), Just(SyncDirection::Pull),]
}

fn arb_distributed_auth_mode() -> impl Strategy<Value = DistributedAuthMode> {
    prop_oneof![
        Just(DistributedAuthMode::Token),
        Just(DistributedAuthMode::Mtls),
        Just(DistributedAuthMode::TokenAndMtls),
    ]
}

fn arb_snapshot_scheduling_mode() -> impl Strategy<Value = SnapshotSchedulingMode> {
    prop_oneof![
        Just(SnapshotSchedulingMode::Periodic),
        Just(SnapshotSchedulingMode::Intelligent),
    ]
}

/// A non-empty alphanumeric identifier for rule IDs and tier names.
fn arb_id() -> impl Strategy<Value = String> {
    "[a-z][a-z0-9_]{0,15}"
}

/// Domain-like string for pane matching.
fn arb_domain() -> impl Strategy<Value = String> {
    prop_oneof![
        Just("local".to_string()),
        Just("SSH:myhost".to_string()),
        Just("unix:foo".to_string()),
    ]
}

/// Title-like string for pane matching.
fn arb_title() -> impl Strategy<Value = String> {
    prop_oneof![
        Just("vim".to_string()),
        Just("bash - /home/user".to_string()),
        Just("python3".to_string()),
        Just("htop".to_string()),
    ]
}

/// CWD-like path for pane matching.
fn arb_cwd() -> impl Strategy<Value = String> {
    prop_oneof![
        Just("/home/user".to_string()),
        Just("/tmp/scratch".to_string()),
        Just("/home/user/private".to_string()),
        Just("/var/log".to_string()),
    ]
}

fn arb_filter_rule_with_matchers() -> impl Strategy<Value = PaneFilterRule> {
    (
        arb_id(),
        prop::option::of(arb_domain()),
        prop::option::of(arb_title()),
        prop::option::of(arb_cwd()),
    )
        .prop_filter("at least one matcher must be set", |(_id, d, t, c)| {
            d.is_some() || t.is_some() || c.is_some()
        })
        .prop_map(|(id, domain, title, cwd)| PaneFilterRule {
            id,
            domain,
            title,
            cwd,
        })
}

fn arb_retention_tier() -> impl Strategy<Value = RetentionTier> {
    (
        arb_id(),
        0u32..365,
        prop::collection::vec("[a-z]{3,10}", 0..3),
        prop::collection::vec("[a-z_]{3,10}", 0..3),
        prop::option::of(any::<bool>()),
    )
        .prop_map(
            |(name, retention_days, severities, event_types, handled)| RetentionTier {
                name,
                retention_days,
                severities,
                event_types,
                handled,
            },
        )
}

fn arb_capture_budget() -> impl Strategy<Value = CaptureBudgetConfig> {
    (0u32..1000, 0u64..1_000_000).prop_map(|(max_captures_per_sec, max_bytes_per_sec)| {
        CaptureBudgetConfig {
            max_captures_per_sec,
            max_bytes_per_sec,
        }
    })
}

// =============================================================================
// 1. LogFormat
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn log_format_serde_json_roundtrip(fmt in arb_log_format()) {
        let json = serde_json::to_string(&fmt).unwrap();
        let back: LogFormat = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(fmt, back);
    }

    #[test]
    fn log_format_display_matches_serde(fmt in arb_log_format()) {
        let display = format!("{}", fmt);
        let serde_val: String = serde_json::from_str(&serde_json::to_string(&fmt).unwrap()).unwrap();
        prop_assert_eq!(display, serde_val);
    }

    #[test]
    fn log_format_fromstr_roundtrip(fmt in arb_log_format()) {
        let display = format!("{}", fmt);
        let parsed: LogFormat = display.parse().unwrap();
        prop_assert_eq!(fmt, parsed);
    }

    #[test]
    fn log_format_fromstr_case_insensitive(fmt in arb_log_format()) {
        let display = format!("{}", fmt);
        // Test uppercase
        let upper = display.to_uppercase();
        let parsed: LogFormat = upper.parse().unwrap();
        prop_assert_eq!(fmt, parsed);
        // Test mixed case
        let mixed: String = display.chars().enumerate()
            .map(|(i, c)| if i % 2 == 0 { c.to_uppercase().next().unwrap() } else { c })
            .collect();
        let parsed_mixed: LogFormat = mixed.parse().unwrap();
        prop_assert_eq!(fmt, parsed_mixed);
    }

    #[test]
    fn log_format_fromstr_invalid(s in "[a-z]{5,10}") {
        // Filter out the valid values
        if s != "pretty" && s != "json" {
            let result: Result<LogFormat, _> = s.parse();
            prop_assert!(result.is_err());
        }
    }
}

#[test]
fn log_format_default_is_pretty() {
    assert_eq!(LogFormat::default(), LogFormat::Pretty);
}

// =============================================================================
// 2. SyncDirection
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn sync_direction_serde_json_roundtrip(dir in arb_sync_direction()) {
        let json = serde_json::to_string(&dir).unwrap();
        let back: SyncDirection = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(dir, back);
    }

    #[test]
    fn sync_direction_serde_toml_roundtrip(dir in arb_sync_direction()) {
        // Wrap in a struct because TOML requires a table at the top level
        #[derive(serde::Serialize, serde::Deserialize, PartialEq, Debug)]
        struct Wrapper { d: SyncDirection }
        let w = Wrapper { d: dir };
        let toml_str = toml::to_string(&w).unwrap();
        let back: Wrapper = toml::from_str(&toml_str).unwrap();
        prop_assert_eq!(w, back);
    }
}

#[test]
fn sync_direction_default_is_push() {
    assert_eq!(SyncDirection::default(), SyncDirection::Push);
}

// =============================================================================
// 3. DistributedAuthMode
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn auth_mode_serde_json_roundtrip(mode in arb_distributed_auth_mode()) {
        let json = serde_json::to_string(&mode).unwrap();
        let back: DistributedAuthMode = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(mode, back);
    }

    #[test]
    fn auth_mode_requires_token_semantics(mode in arb_distributed_auth_mode()) {
        let needs_token = mode.requires_token();
        let is_token = mode == DistributedAuthMode::Token;
        let is_both = mode == DistributedAuthMode::TokenAndMtls;
        prop_assert_eq!(needs_token, is_token || is_both,
            "requires_token mismatch for {:?}", mode);
    }

    #[test]
    fn auth_mode_requires_mtls_semantics(mode in arb_distributed_auth_mode()) {
        let needs_mtls = mode.requires_mtls();
        let is_mtls = mode == DistributedAuthMode::Mtls;
        let is_both = mode == DistributedAuthMode::TokenAndMtls;
        prop_assert_eq!(needs_mtls, is_mtls || is_both,
            "requires_mtls mismatch for {:?}", mode);
    }

    #[test]
    fn auth_mode_token_and_mtls_requires_both(_unused in 0u8..1) {
        let mode = DistributedAuthMode::TokenAndMtls;
        prop_assert!(mode.requires_token());
        prop_assert!(mode.requires_mtls());
    }
}

#[test]
fn auth_mode_default_is_token() {
    assert_eq!(DistributedAuthMode::default(), DistributedAuthMode::Token);
}

// =============================================================================
// 4. SnapshotSchedulingMode
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn snapshot_scheduling_mode_serde_roundtrip(mode in arb_snapshot_scheduling_mode()) {
        let json = serde_json::to_string(&mode).unwrap();
        let back: SnapshotSchedulingMode = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(mode, back);
    }
}

#[test]
fn snapshot_scheduling_mode_default_is_intelligent() {
    assert_eq!(
        SnapshotSchedulingMode::default(),
        SnapshotSchedulingMode::Intelligent
    );
}

// =============================================================================
// 5. PaneFilterRule
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn filter_rule_serde_json_roundtrip(rule in arb_filter_rule_with_matchers()) {
        let json = serde_json::to_string(&rule).unwrap();
        let back: PaneFilterRule = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(rule, back);
    }

    #[test]
    fn filter_rule_default_values(_unused in 0u8..1) {
        let rule = PaneFilterRule::default();
        prop_assert_eq!(&rule.id, "unnamed_rule");
        prop_assert!(rule.domain.is_none());
        prop_assert!(rule.title.is_none());
        prop_assert!(rule.cwd.is_none());
    }

    #[test]
    fn filter_rule_builder_sets_fields(
        id in arb_id(),
        domain in arb_domain(),
        title in arb_title(),
        cwd in arb_cwd(),
    ) {
        let rule = PaneFilterRule::new(id.clone())
            .with_domain(domain.clone())
            .with_title(title.clone())
            .with_cwd(cwd.clone());

        prop_assert_eq!(&rule.id, &id);
        prop_assert_eq!(rule.domain.as_deref(), Some(domain.as_str()));
        prop_assert_eq!(rule.title.as_deref(), Some(title.as_str()));
        prop_assert_eq!(rule.cwd.as_deref(), Some(cwd.as_str()));
    }

    #[test]
    fn filter_rule_no_matchers_matches_nothing(
        domain in arb_domain(),
        title in arb_title(),
        cwd in arb_cwd(),
    ) {
        let rule = PaneFilterRule::new("empty_rule");
        // No matchers set, so should never match
        prop_assert!(!rule.matches(&domain, &title, &cwd));
    }

    #[test]
    fn filter_rule_domain_glob_star_matches_all(
        domain in arb_domain(),
    ) {
        let rule = PaneFilterRule::new("r1").with_domain("*");
        prop_assert!(rule.matches(&domain, "anytitle", "/any/cwd"));
    }

    #[test]
    fn filter_rule_domain_exact_match(domain in arb_domain()) {
        let rule = PaneFilterRule::new("r1").with_domain(domain.clone());
        prop_assert!(rule.matches(&domain, "anytitle", "/any/cwd"));
    }

    #[test]
    fn filter_rule_domain_glob_prefix(
        host in "[a-z]{3,8}",
    ) {
        let domain = format!("SSH:{}", host);
        let rule = PaneFilterRule::new("r1").with_domain("SSH:*");
        prop_assert!(rule.matches(&domain, "anytitle", "/any/cwd"));
    }

    #[test]
    fn filter_rule_title_substring_case_insensitive(
        base_title in "[a-z]{3,8}",
    ) {
        // Title contains the pattern as substring, case-insensitively
        let title = format!("running-{}-session", base_title);
        let rule = PaneFilterRule::new("r1").with_title(base_title.to_uppercase());
        prop_assert!(rule.matches("local", &title, "/tmp"));
    }

    #[test]
    fn filter_rule_title_regex_match(
        word in "[a-z]{3,6}",
    ) {
        let pattern = format!("re:^{}$", word);
        let rule = PaneFilterRule::new("r1").with_title(pattern);
        prop_assert!(rule.matches("local", &word, "/tmp"));
    }

    #[test]
    fn filter_rule_title_regex_no_match(
        word in "[a-z]{3,6}",
    ) {
        let pattern = format!("re:^{}$", word);
        let rule = PaneFilterRule::new("r1").with_title(pattern);
        // "x_" prefix means it should not match the exact regex
        let non_matching = format!("x_{}", word);
        prop_assert!(!rule.matches("local", &non_matching, "/tmp"));
    }

    #[test]
    fn filter_rule_cwd_prefix_match(
        base in "[a-z]{3,8}",
        sub in "[a-z]{3,8}",
    ) {
        let parent = format!("/home/{}", base);
        let child = format!("/home/{}/{}", base, sub);
        let rule = PaneFilterRule::new("r1").with_cwd(parent);
        prop_assert!(rule.matches("local", "anytitle", &child));
    }

    #[test]
    fn filter_rule_validate_empty_id_errors(_unused in 0u8..1) {
        let rule = PaneFilterRule {
            id: String::new(),
            domain: Some("local".to_string()),
            title: None,
            cwd: None,
        };
        let result = rule.validate();
        prop_assert!(result.is_err());
    }

    #[test]
    fn filter_rule_validate_no_matchers_errors(id in arb_id()) {
        let rule = PaneFilterRule::new(id);
        let result = rule.validate();
        prop_assert!(result.is_err());
    }

    #[test]
    fn filter_rule_validate_bad_regex_errors(id in arb_id()) {
        let rule = PaneFilterRule::new(id).with_title("re:[invalid((");
        let result = rule.validate();
        prop_assert!(result.is_err());
    }

    #[test]
    fn filter_rule_validate_good_rule_ok(id in arb_id(), domain in arb_domain()) {
        let rule = PaneFilterRule::new(id).with_domain(domain);
        let result = rule.validate();
        prop_assert!(result.is_ok());
    }

    #[test]
    fn filter_rule_and_logic_all_must_match(
        id in arb_id(),
    ) {
        // Rule requires both domain="local" AND title containing "vim"
        let rule = PaneFilterRule::new(id)
            .with_domain("local")
            .with_title("vim");

        // Both match
        prop_assert!(rule.matches("local", "vim editor", "/tmp"));
        // Domain matches but title does not
        prop_assert!(!rule.matches("local", "bash", "/tmp"));
        // Title matches but domain does not
        prop_assert!(!rule.matches("SSH:host", "vim editor", "/tmp"));
    }
}

// =============================================================================
// 6. PaneFilterConfig
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn filter_config_empty_is_allow_all(
        domain in arb_domain(),
        title in arb_title(),
        cwd in arb_cwd(),
    ) {
        let config = PaneFilterConfig::default();
        // Empty include list + empty exclude list = observe everything
        prop_assert!(config.check_pane(&domain, &title, &cwd).is_none());
    }

    #[test]
    fn filter_config_exclude_always_wins(
        domain in arb_domain(),
    ) {
        // Include rule matching "local" AND exclude rule matching "local"
        let config = PaneFilterConfig {
            include: vec![PaneFilterRule::new("inc").with_domain(domain.clone())],
            exclude: vec![PaneFilterRule::new("exc").with_domain(domain.clone())],
        };
        // Exclude wins: pane should be excluded
        let result = config.check_pane(&domain, "anytitle", "/any");
        prop_assert!(result.is_some());
        prop_assert_eq!(result.unwrap(), "exc".to_string());
    }

    #[test]
    fn filter_config_include_nonempty_must_match(_unused in 0u8..1) {
        let config = PaneFilterConfig {
            include: vec![PaneFilterRule::new("inc").with_domain("SSH:*")],
            exclude: vec![],
        };
        // "local" does not match SSH:* include rule
        let result = config.check_pane("local", "bash", "/home");
        prop_assert!(result.is_some());
        let val = result.unwrap();
        prop_assert_eq!(val, "no_include_match".to_string());
    }

    #[test]
    fn filter_config_include_nonempty_match_passes(_unused in 0u8..1) {
        let config = PaneFilterConfig {
            include: vec![PaneFilterRule::new("inc").with_domain("local")],
            exclude: vec![],
        };
        let result = config.check_pane("local", "bash", "/home");
        prop_assert!(result.is_none());
    }

    #[test]
    fn filter_config_has_rules_reflects_content(
        inc_count in 0usize..3,
        exc_count in 0usize..3,
    ) {
        let include: Vec<PaneFilterRule> = (0..inc_count)
            .map(|i| PaneFilterRule::new(format!("inc_{}", i)).with_domain("local"))
            .collect();
        let exclude: Vec<PaneFilterRule> = (0..exc_count)
            .map(|i| PaneFilterRule::new(format!("exc_{}", i)).with_domain("local"))
            .collect();
        let config = PaneFilterConfig { include, exclude };
        let expected = inc_count > 0 || exc_count > 0;
        prop_assert_eq!(config.has_rules(), expected);
    }
}

// =============================================================================
// 7. PanePriorityConfig
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn priority_config_default_priority_is_100(_unused in 0u8..1) {
        let config = PanePriorityConfig::default();
        prop_assert_eq!(config.default_priority, 100);
    }

    #[test]
    fn priority_config_no_rules_returns_default(
        domain in arb_domain(),
        title in arb_title(),
        cwd in arb_cwd(),
    ) {
        let config = PanePriorityConfig::default();
        let p = config.priority_for_pane(&domain, &title, &cwd);
        prop_assert_eq!(p, 100);
    }

    #[test]
    fn priority_config_first_match_wins(prio1 in 1u32..50, prio2 in 51u32..99) {
        let config = PanePriorityConfig {
            default_priority: 100,
            rules: vec![
                PanePriorityRule {
                    matcher: PaneFilterRule::new("high").with_domain("local"),
                    priority: prio1,
                },
                PanePriorityRule {
                    matcher: PaneFilterRule::new("low").with_domain("*"),
                    priority: prio2,
                },
            ],
        };
        // "local" matches the first rule
        let p = config.priority_for_pane("local", "bash", "/home");
        prop_assert_eq!(p, prio1);
    }

    #[test]
    fn priority_config_second_match_when_first_fails(prio in 1u32..99) {
        let config = PanePriorityConfig {
            default_priority: 100,
            rules: vec![
                PanePriorityRule {
                    matcher: PaneFilterRule::new("ssh_only").with_domain("SSH:*"),
                    priority: 10,
                },
                PanePriorityRule {
                    matcher: PaneFilterRule::new("local_catch").with_domain("local"),
                    priority: prio,
                },
            ],
        };
        // "local" does not match SSH:*, falls through to second rule
        let p = config.priority_for_pane("local", "bash", "/home");
        prop_assert_eq!(p, prio);
    }
}

// =============================================================================
// 8. CaptureBudgetConfig
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn capture_budget_serde_json_roundtrip(budget in arb_capture_budget()) {
        let json = serde_json::to_string(&budget).unwrap();
        let back: CaptureBudgetConfig = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(budget, back);
    }

    #[test]
    fn capture_budget_serde_toml_roundtrip(budget in arb_capture_budget()) {
        #[derive(serde::Serialize, serde::Deserialize, PartialEq, Debug)]
        struct W { b: CaptureBudgetConfig }
        let w = W { b: budget };
        let toml_str = toml::to_string(&w).unwrap();
        let back: W = toml::from_str(&toml_str).unwrap();
        prop_assert_eq!(w, back);
    }
}

#[test]
fn capture_budget_default_is_unlimited() {
    let budget = CaptureBudgetConfig::default();
    assert_eq!(budget.max_captures_per_sec, 0);
    assert_eq!(budget.max_bytes_per_sec, 0);
}

// =============================================================================
// 9. RetentionTier
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn retention_tier_serde_json_roundtrip(tier in arb_retention_tier()) {
        let json = serde_json::to_string(&tier).unwrap();
        let back: RetentionTier = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(tier, back);
    }

    #[test]
    fn retention_tier_optional_fields_skipped_when_empty(_unused in 0u8..1) {
        let tier = RetentionTier {
            name: "minimal".to_string(),
            retention_days: 7,
            severities: vec![],
            event_types: vec![],
            handled: None,
        };
        let json = serde_json::to_string(&tier).unwrap();
        // skip_serializing_if = "Vec::is_empty" should omit these
        let has_severities = json.contains("severities");
        let has_event_types = json.contains("event_types");
        let has_handled = json.contains("handled");
        prop_assert!(!has_severities, "severities should be skipped when empty");
        prop_assert!(!has_event_types, "event_types should be skipped when empty");
        prop_assert!(!has_handled, "handled should be skipped when None");
    }

    #[test]
    fn retention_tier_optional_fields_present_when_set(
        sev in "[a-z]{3,8}",
        evt in "[a-z_]{3,8}",
        handled in any::<bool>(),
    ) {
        let tier = RetentionTier {
            name: "full".to_string(),
            retention_days: 30,
            severities: vec![sev],
            event_types: vec![evt],
            handled: Some(handled),
        };
        let json = serde_json::to_string(&tier).unwrap();
        let back: RetentionTier = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(tier, back);
        // All fields should be present
        prop_assert!(json.contains("severities"));
        prop_assert!(json.contains("event_types"));
        prop_assert!(json.contains("handled"));
    }
}

// =============================================================================
// 10. StorageConfig: resolve_retention_days
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn storage_default_tiers_critical_90(_unused in 0u8..1) {
        let config = StorageConfig::default();
        let days = config.resolve_retention_days("critical", "any.type", false);
        prop_assert_eq!(days, 90);
    }

    #[test]
    fn storage_default_tiers_warning_30(_unused in 0u8..1) {
        let config = StorageConfig::default();
        let days = config.resolve_retention_days("warning", "any.type", false);
        prop_assert_eq!(days, 30);
    }

    #[test]
    fn storage_default_tiers_info_7(_unused in 0u8..1) {
        let config = StorageConfig::default();
        let days = config.resolve_retention_days("info", "any.type", false);
        prop_assert_eq!(days, 7);
    }

    #[test]
    fn storage_default_tiers_unknown_falls_back(
        severity in "[a-z]{4,8}",
    ) {
        // Filter out the three known severities
        if severity != "critical" && severity != "warning" && severity != "info" {
            let config = StorageConfig::default();
            let days = config.resolve_retention_days(&severity, "any.type", false);
            // Falls back to global retention_days (30 by default)
            prop_assert_eq!(days, config.retention_days);
        }
    }

    #[test]
    fn storage_severity_match_case_insensitive(_unused in 0u8..1) {
        let config = StorageConfig::default();
        // "CRITICAL" should match "critical" tier
        let days = config.resolve_retention_days("CRITICAL", "some.event", false);
        prop_assert_eq!(days, 90);
    }

    #[test]
    fn storage_event_type_prefix_match(_unused in 0u8..1) {
        let config = StorageConfig {
            retention_tiers: vec![RetentionTier {
                name: "build_events".to_string(),
                retention_days: 14,
                severities: vec![],
                event_types: vec!["build.".to_string()],
                handled: None,
            }],
            ..StorageConfig::default()
        };
        let days = config.resolve_retention_days("info", "build.success", false);
        prop_assert_eq!(days, 14);
    }

    #[test]
    fn storage_event_type_prefix_no_match(_unused in 0u8..1) {
        let config = StorageConfig {
            retention_tiers: vec![RetentionTier {
                name: "build_events".to_string(),
                retention_days: 14,
                severities: vec![],
                event_types: vec!["build.".to_string()],
                handled: None,
            }],
            ..StorageConfig::default()
        };
        // "deploy.success" does not start with "build."
        let days = config.resolve_retention_days("info", "deploy.success", false);
        prop_assert_eq!(days, config.retention_days);
    }

    #[test]
    fn storage_handled_filter_matches(handled in any::<bool>()) {
        let config = StorageConfig {
            retention_tiers: vec![RetentionTier {
                name: "handled_filter".to_string(),
                retention_days: 3,
                severities: vec![],
                event_types: vec![],
                handled: Some(true),
            }],
            ..StorageConfig::default()
        };
        let days = config.resolve_retention_days("info", "any", handled);
        if handled {
            prop_assert_eq!(days, 3);
        } else {
            // Does not match the tier; falls back to global
            prop_assert_eq!(days, config.retention_days);
        }
    }

    #[test]
    fn storage_first_tier_wins(_unused in 0u8..1) {
        let config = StorageConfig {
            retention_tiers: vec![
                RetentionTier {
                    name: "first".to_string(),
                    retention_days: 1,
                    severities: vec![],
                    event_types: vec![],
                    handled: None,
                },
                RetentionTier {
                    name: "second".to_string(),
                    retention_days: 999,
                    severities: vec![],
                    event_types: vec![],
                    handled: None,
                },
            ],
            ..StorageConfig::default()
        };
        // Both tiers match (no filters = match all), but first wins
        let days = config.resolve_retention_days("info", "any", false);
        prop_assert_eq!(days, 1);
    }
}

// =============================================================================
// 11. Config: default roundtrip
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(10))]

    #[test]
    fn config_default_serde_json_roundtrip(_unused in 0u8..1) {
        let config = Config::default();
        let json = serde_json::to_string(&config).unwrap();
        let back: Config = serde_json::from_str(&json).unwrap();
        // Check a few key fields to confirm roundtrip
        prop_assert_eq!(back.snapshots.enabled, config.snapshots.enabled);
        prop_assert_eq!(back.snapshots.interval_seconds, config.snapshots.interval_seconds);
        prop_assert_eq!(back.storage.retention_days, config.storage.retention_days);
        prop_assert_eq!(back.ingest.poll_interval_ms, config.ingest.poll_interval_ms);
    }

    #[test]
    fn config_empty_json_parses_ok(_unused in 0u8..1) {
        // All sections have serde(default), so "{}" should parse
        let config: Config = serde_json::from_str("{}").unwrap();
        prop_assert!(config.snapshots.enabled);
        prop_assert_eq!(config.snapshots.interval_seconds, 300);
    }
}

// =============================================================================
// 12. SnapshotConfig
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn snapshot_config_serde_json_roundtrip(_unused in 0u8..1) {
        let config = SnapshotConfig::default();
        let json = serde_json::to_string(&config).unwrap();
        let back: SnapshotConfig = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.enabled, config.enabled);
        prop_assert_eq!(back.interval_seconds, config.interval_seconds);
        prop_assert_eq!(back.retention_count, config.retention_count);
        prop_assert_eq!(back.retention_days, config.retention_days);
    }
}

#[test]
fn snapshot_config_default_values() {
    let config = SnapshotConfig::default();
    assert!(config.enabled);
    assert_eq!(config.interval_seconds, 300);
    assert_eq!(config.retention_count, 10);
    assert_eq!(config.retention_days, 7);
}

// =============================================================================
// 13. SnapshotSchedulingConfig
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn scheduling_config_serde_json_roundtrip(_unused in 0u8..1) {
        let config = SnapshotSchedulingConfig::default();
        let json = serde_json::to_string(&config).unwrap();
        let back: SnapshotSchedulingConfig = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(config, back);
    }

    #[test]
    fn scheduling_config_custom_values(
        threshold in 0.1f64..100.0,
        work_val in 0.1f64..50.0,
        fallback in 1u64..120,
    ) {
        let config = SnapshotSchedulingConfig {
            mode: SnapshotSchedulingMode::Periodic,
            snapshot_threshold: threshold,
            work_completed_value: work_val,
            state_transition_value: 1.0,
            idle_window_value: 3.0,
            memory_pressure_value: 4.0,
            hazard_trigger_value: 10.0,
            periodic_fallback_minutes: fallback,
        };
        let json = serde_json::to_string(&config).unwrap();
        let back: SnapshotSchedulingConfig = serde_json::from_str(&json).unwrap();
        // f64 roundtrip through JSON can lose precision at the last bit,
        // so compare with tolerance rather than exact equality.
        prop_assert_eq!(back.mode, config.mode);
        let thresh_ok = (back.snapshot_threshold - config.snapshot_threshold).abs() < 1e-10;
        prop_assert!(thresh_ok, "threshold drift: {} vs {}", back.snapshot_threshold, config.snapshot_threshold);
        let work_ok = (back.work_completed_value - config.work_completed_value).abs() < 1e-10;
        prop_assert!(work_ok, "work_completed_value drift: {} vs {}", back.work_completed_value, config.work_completed_value);
        prop_assert_eq!(back.periodic_fallback_minutes, config.periodic_fallback_minutes);
    }
}

#[test]
fn scheduling_config_default_values() {
    let config = SnapshotSchedulingConfig::default();
    assert_eq!(config.mode, SnapshotSchedulingMode::Intelligent);
    assert!((config.snapshot_threshold - 5.0).abs() < f64::EPSILON);
    assert!((config.work_completed_value - 2.0).abs() < f64::EPSILON);
    assert_eq!(config.periodic_fallback_minutes, 30);
}
