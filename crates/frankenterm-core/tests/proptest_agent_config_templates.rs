//! Property-based tests for agent config template generation (ft-dr6zv.2.4).

use proptest::prelude::*;

use frankenterm_core::agent_config_templates::{
    build_generation_plan, config_kind_for_provider, generate_template,
    generate_templates_for_detected, merge_into_existing, section_is_current, AgentConfigKind,
    AgentConfigTemplate, ConfigAction, ConfigGenerationPlanItem, ConfigGenerationResult,
    ConfigScope, SECTION_END_MARKER, SECTION_START_MARKER,
};
use frankenterm_core::agent_provider::AgentProvider;

// ---------------------------------------------------------------------------
// Strategies
// ---------------------------------------------------------------------------

fn arb_known_provider() -> impl Strategy<Value = AgentProvider> {
    prop_oneof![
        Just(AgentProvider::Claude),
        Just(AgentProvider::Codex),
        Just(AgentProvider::Gemini),
        Just(AgentProvider::Cursor),
        Just(AgentProvider::Cline),
        Just(AgentProvider::Windsurf),
        Just(AgentProvider::Aider),
        Just(AgentProvider::Opencode),
        Just(AgentProvider::GithubCopilot),
        Just(AgentProvider::Grok),
        Just(AgentProvider::Devin),
        Just(AgentProvider::Factory),
    ]
}

fn arb_slug() -> impl Strategy<Value = String> {
    prop_oneof![
        Just("claude".to_string()),
        Just("codex".to_string()),
        Just("gemini".to_string()),
        Just("cursor".to_string()),
        Just("cline".to_string()),
        Just("windsurf".to_string()),
        Just("aider".to_string()),
        Just("opencode".to_string()),
        Just("github-copilot".to_string()),
        Just("grok".to_string()),
        Just("devin".to_string()),
        Just("factory".to_string()),
    ]
}

fn arb_scope() -> impl Strategy<Value = ConfigScope> {
    prop_oneof![Just(ConfigScope::Project), Just(ConfigScope::Global),]
}

fn arb_action() -> impl Strategy<Value = ConfigAction> {
    prop_oneof![
        Just(ConfigAction::Create),
        Just(ConfigAction::Append),
        Just(ConfigAction::Replace),
        Just(ConfigAction::Skip),
    ]
}

fn arb_existing_content() -> impl Strategy<Value = String> {
    prop_oneof![
        Just(String::new()),
        Just("# My Project\n\nSome existing content.\n".to_string()),
        "# [A-Z][a-z]{3,15}\n\n[a-z ]{10,50}\n".prop_map(|s| s),
    ]
}

// ---------------------------------------------------------------------------
// ACT-1: generate_template produces valid content for all known providers
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn act_1_all_known_produce_nonempty(provider in arb_known_provider()) {
        let template = generate_template(&provider);
        prop_assert!(!template.content.is_empty());
        prop_assert!(!template.filename.is_empty());
        prop_assert!(template.content.contains("ft robot"));
    }
}

// ---------------------------------------------------------------------------
// ACT-2: config_kind_for_provider is deterministic
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn act_2_kind_deterministic(provider in arb_known_provider()) {
        let kind1 = config_kind_for_provider(&provider);
        let kind2 = config_kind_for_provider(&provider);
        prop_assert_eq!(kind1, kind2);
    }
}

// ---------------------------------------------------------------------------
// ACT-3: merge is idempotent
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn act_3_merge_idempotent(
        existing in arb_existing_content(),
        provider in arb_known_provider(),
    ) {
        let template = generate_template(&provider);
        let first = merge_into_existing(&existing, &template.content);
        let second = merge_into_existing(&first, &template.content);
        prop_assert_eq!(&first, &second, "merge should be idempotent");
    }
}

// ---------------------------------------------------------------------------
// ACT-4: merged content contains exactly one start/end marker pair
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn act_4_single_marker_pair(
        existing in arb_existing_content(),
        provider in arb_known_provider(),
    ) {
        let template = generate_template(&provider);
        let merged = merge_into_existing(&existing, &template.content);
        let start_count = merged.matches(SECTION_START_MARKER).count();
        let end_count = merged.matches(SECTION_END_MARKER).count();
        prop_assert_eq!(start_count, 1, "should have exactly one start marker");
        prop_assert_eq!(end_count, 1, "should have exactly one end marker");
    }
}

// ---------------------------------------------------------------------------
// ACT-5: merged content preserves existing prefix
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn act_5_preserves_prefix(
        prefix in "[a-z ]{5,30}",
        provider in arb_known_provider(),
    ) {
        let existing = format!("{}\n", prefix);
        let merged = merge_into_existing(&existing, &generate_template(&provider).content);
        prop_assert!(
            merged.contains(&prefix),
            "merged content should contain original prefix"
        );
    }
}

// ---------------------------------------------------------------------------
// ACT-6: section_is_current matches after merge
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn act_6_section_current_after_merge(provider in arb_known_provider()) {
        let template = generate_template(&provider);
        let merged = merge_into_existing("", &template.content);
        prop_assert!(section_is_current(&merged, &template.content));
    }
}

// ---------------------------------------------------------------------------
// ACT-7: AgentConfigTemplate serde roundtrip
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn act_7_template_serde_roundtrip(provider in arb_known_provider()) {
        let template = generate_template(&provider);
        let json = serde_json::to_string(&template).unwrap();
        let back: AgentConfigTemplate = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.provider, template.provider);
        prop_assert_eq!(back.kind, template.kind);
        prop_assert_eq!(back.content, template.content);
        prop_assert_eq!(back.filename, template.filename);
    }
}

// ---------------------------------------------------------------------------
// ACT-8: AgentConfigKind serde roundtrip
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    #[test]
    fn act_8_config_kind_serde(provider in arb_known_provider()) {
        let kind = config_kind_for_provider(&provider);
        let json = serde_json::to_string(&kind).unwrap();
        let back: AgentConfigKind = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back, kind);
    }
}

// ---------------------------------------------------------------------------
// ACT-9: ConfigScope serde roundtrip
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(16))]

    #[test]
    fn act_9_scope_serde(scope in arb_scope()) {
        let json = serde_json::to_string(&scope).unwrap();
        let back: ConfigScope = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back, scope);
    }
}

// ---------------------------------------------------------------------------
// ACT-10: ConfigAction serde roundtrip
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(16))]

    #[test]
    fn act_10_action_serde(action in arb_action()) {
        let json = serde_json::to_string(&action).unwrap();
        let back: ConfigAction = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back, action);
    }
}

// ---------------------------------------------------------------------------
// ACT-11: ConfigGenerationResult serde roundtrip
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn act_11_result_serde(
        slug in arb_slug(),
        action in arb_action(),
        backup in any::<bool>(),
        error in proptest::option::of("[a-z ]{5,20}"),
    ) {
        let result = ConfigGenerationResult {
            slug: slug.clone(),
            action,
            filename: "AGENTS.md".to_string(),
            backup_created: backup,
            error: error.clone(),
        };
        let json = serde_json::to_string(&result).unwrap();
        let back: ConfigGenerationResult = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.slug, slug);
        prop_assert_eq!(back.action, action);
        prop_assert_eq!(back.backup_created, backup);
        prop_assert_eq!(back.error, error);
    }
}

// ---------------------------------------------------------------------------
// ACT-12: generate_templates_for_detected count matches input
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    #[test]
    fn act_12_detected_count_matches(
        slugs in proptest::collection::vec(arb_slug(), 0..10),
    ) {
        let templates = generate_templates_for_detected(&slugs);
        prop_assert_eq!(templates.len(), slugs.len());
    }
}

// ---------------------------------------------------------------------------
// ACT-13: build_generation_plan creates correct actions for new files
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    #[test]
    fn act_13_plan_new_files(
        slugs in proptest::collection::vec(arb_slug(), 1..6),
        scope in arb_scope(),
    ) {
        let plan = build_generation_plan(&slugs, scope, |_| (false, None));
        prop_assert_eq!(plan.len(), slugs.len());
        for item in &plan {
            prop_assert_eq!(item.action, ConfigAction::Create);
            prop_assert!(!item.file_exists);
        }
    }
}

// ---------------------------------------------------------------------------
// ACT-14: plan items have correct scope
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    #[test]
    fn act_14_plan_scope(
        slug in arb_slug(),
        scope in arb_scope(),
    ) {
        let plan = build_generation_plan(&[slug], scope, |_| (false, None));
        prop_assert_eq!(plan.len(), 1);
        prop_assert_eq!(plan[0].scope, scope);
    }
}

// ---------------------------------------------------------------------------
// ACT-15: ConfigGenerationPlanItem serde roundtrip
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    #[test]
    fn act_15_plan_item_serde(
        slug in arb_slug(),
        scope in arb_scope(),
    ) {
        let plan = build_generation_plan(&[slug], scope, |_| (false, None));
        prop_assert_eq!(plan.len(), 1);
        let json = serde_json::to_string(&plan[0]).unwrap();
        let back: ConfigGenerationPlanItem = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&back.slug, &plan[0].slug);
        prop_assert_eq!(back.action, plan[0].action);
        prop_assert_eq!(back.scope, plan[0].scope);
        prop_assert_eq!(&back.filename, &plan[0].filename);
    }
}

// ---------------------------------------------------------------------------
// ACT-16: merge replaces outdated section correctly
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    #[test]
    fn act_16_replace_outdated_section(
        provider in arb_known_provider(),
        old_content in "[a-z ]{10,40}",
    ) {
        // Create a file with an old section.
        let existing = format!(
            "# Header\n\n{}\n{}\n{}\n\n# Footer\n",
            SECTION_START_MARKER, old_content, SECTION_END_MARKER
        );

        let template = generate_template(&provider);
        let merged = merge_into_existing(&existing, &template.content);

        // Old content should be gone.
        prop_assert!(!merged.contains(&old_content));
        // New content should be present.
        prop_assert!(merged.contains(&template.content));
        // Header and footer preserved.
        prop_assert!(merged.contains("# Header"));
        prop_assert!(merged.contains("# Footer"));
    }
}
