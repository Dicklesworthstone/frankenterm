//! Property-based tests for `agent_provider` — unified agent identification.

use proptest::prelude::*;

use frankenterm_core::agent_provider::AgentProvider;

// ---------------------------------------------------------------------------
// Strategies
// ---------------------------------------------------------------------------

fn arb_known_provider() -> impl Strategy<Value = AgentProvider> {
    prop_oneof![
        Just(AgentProvider::Claude),
        Just(AgentProvider::Cline),
        Just(AgentProvider::Codex),
        Just(AgentProvider::Cursor),
        Just(AgentProvider::Devin),
        Just(AgentProvider::Factory),
        Just(AgentProvider::Gemini),
        Just(AgentProvider::GithubCopilot),
        Just(AgentProvider::Grok),
        Just(AgentProvider::Opencode),
        Just(AgentProvider::Aider),
        Just(AgentProvider::Windsurf),
    ]
}

fn arb_provider() -> impl Strategy<Value = AgentProvider> {
    prop_oneof![
        10 => arb_known_provider(),
        2 => "[a-z]{5,20}".prop_map(AgentProvider::Unknown),
    ]
}

// Known binary names that map to providers
fn arb_known_binary_name() -> impl Strategy<Value = (&'static str, AgentProvider)> {
    prop_oneof![
        Just(("claude", AgentProvider::Claude)),
        Just(("claude-code", AgentProvider::Claude)),
        Just(("claude_code", AgentProvider::Claude)),
        Just(("codex", AgentProvider::Codex)),
        Just(("codex-cli", AgentProvider::Codex)),
        Just(("gemini", AgentProvider::Gemini)),
        Just(("gemini-cli", AgentProvider::Gemini)),
        Just(("cursor", AgentProvider::Cursor)),
        Just(("windsurf", AgentProvider::Windsurf)),
        Just(("cline", AgentProvider::Cline)),
        Just(("copilot", AgentProvider::GithubCopilot)),
        Just(("github-copilot", AgentProvider::GithubCopilot)),
        Just(("devin", AgentProvider::Devin)),
        Just(("grok", AgentProvider::Grok)),
        Just(("grok-cli", AgentProvider::Grok)),
        Just(("aider", AgentProvider::Aider)),
        Just(("opencode", AgentProvider::Opencode)),
        Just(("open-code", AgentProvider::Opencode)),
        Just(("factory", AgentProvider::Factory)),
        Just(("factory-droid", AgentProvider::Factory)),
    ]
}

// Process names containing known substrings
fn arb_known_process_name() -> impl Strategy<Value = (String, AgentProvider)> {
    prop_oneof![
        Just(("claude".to_string(), AgentProvider::Claude)),
        Just(("claude-code".to_string(), AgentProvider::Claude)),
        Just(("my-codex-agent".to_string(), AgentProvider::Codex)),
        Just(("gemini-cli-v2".to_string(), AgentProvider::Gemini)),
        Just(("cursor-helper".to_string(), AgentProvider::Cursor)),
        Just(("windsurf-main".to_string(), AgentProvider::Windsurf)),
        Just(("cline-worker".to_string(), AgentProvider::Cline)),
        Just(("copilot-agent".to_string(), AgentProvider::GithubCopilot)),
        Just(("devin-proc".to_string(), AgentProvider::Devin)),
        Just(("grok-runner".to_string(), AgentProvider::Grok)),
        Just(("aider-session".to_string(), AgentProvider::Aider)),
        Just(("opencode-cli".to_string(), AgentProvider::Opencode)),
        Just(("factory-droid".to_string(), AgentProvider::Factory)),
    ]
}

// ---------------------------------------------------------------------------
// Properties
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    // 1. Known provider serde roundtrip
    #[test]
    fn known_provider_serde_roundtrip(provider in arb_known_provider()) {
        let json_str = serde_json::to_string(&provider).unwrap();
        let rt: AgentProvider = serde_json::from_str(&json_str).unwrap();
        prop_assert_eq!(provider, rt);
    }

    // 2. Unknown provider serde roundtrip
    #[test]
    fn unknown_provider_serde_roundtrip(slug in "[a-z]{5,20}") {
        let provider = AgentProvider::Unknown(slug);
        let json_str = serde_json::to_string(&provider).unwrap();
        let rt: AgentProvider = serde_json::from_str(&json_str).unwrap();
        prop_assert_eq!(provider, rt);
    }

    // 3. canonical_slug roundtrip through from_slug for known providers
    #[test]
    fn slug_roundtrip_known(provider in arb_known_provider()) {
        let slug = provider.canonical_slug();
        let rt = AgentProvider::from_slug(slug);
        prop_assert_eq!(provider, rt);
    }

    // 4. Display matches canonical_slug
    #[test]
    fn display_matches_slug(provider in arb_provider()) {
        let display = provider.to_string();
        let slug = provider.canonical_slug();
        prop_assert_eq!(display, slug);
    }

    // 5. display_name is non-empty
    #[test]
    fn display_name_non_empty(provider in arb_provider()) {
        prop_assert!(!provider.display_name().is_empty());
    }

    // 6. canonical_slug is non-empty
    #[test]
    fn canonical_slug_non_empty(provider in arb_provider()) {
        prop_assert!(!provider.canonical_slug().is_empty());
    }

    // 7. from_binary_name recognizes known binaries
    #[test]
    fn from_binary_name_known((binary, expected) in arb_known_binary_name()) {
        let result = AgentProvider::from_binary_name(binary);
        prop_assert_eq!(result, Some(expected));
    }

    // 8. from_binary_name is case-insensitive
    #[test]
    fn from_binary_name_case_insensitive((binary, expected) in arb_known_binary_name()) {
        let upper = binary.to_uppercase();
        let result = AgentProvider::from_binary_name(&upper);
        prop_assert_eq!(result, Some(expected));
    }

    // 9. from_process_name recognizes known substrings
    #[test]
    fn from_process_name_known((name, expected) in arb_known_process_name()) {
        let result = AgentProvider::from_process_name(&name);
        prop_assert_eq!(result, Some(expected));
    }

    // 10. from_process_name is case-insensitive
    #[test]
    fn from_process_name_case_insensitive((name, expected) in arb_known_process_name()) {
        let upper = name.to_uppercase();
        let result = AgentProvider::from_process_name(&upper);
        prop_assert_eq!(result, Some(expected));
    }

    // 11. from_process_name returns None for non-agent names
    #[test]
    fn from_process_name_non_agent(name in "[0-9]{5,20}") {
        let result = AgentProvider::from_process_name(&name);
        prop_assert!(result.is_none());
    }

    // 12. from_binary_name returns None for non-agent binaries
    #[test]
    fn from_binary_name_non_agent(name in "[0-9]{5,20}") {
        let result = AgentProvider::from_binary_name(&name);
        prop_assert!(result.is_none());
    }

    // 13. from_slug for unrecognized strings returns Unknown
    #[test]
    fn from_slug_unknown_preserves_string(slug in "[0-9]{5,20}") {
        let provider = AgentProvider::from_slug(&slug);
        let is_unknown = matches!(provider, AgentProvider::Unknown(_));
        prop_assert!(is_unknown);
    }

    // 14. from_slug preserves original casing in Unknown variant
    #[test]
    fn from_slug_preserves_casing(slug in "[A-Z][a-z]{4,15}") {
        let provider = AgentProvider::from_slug(&slug);
        if let AgentProvider::Unknown(ref inner) = provider {
            prop_assert_eq!(inner, &slug);
        }
    }

    // 15. Unknown variant display_name returns the inner string
    #[test]
    fn unknown_display_name_is_inner(slug in "[a-z]{5,20}") {
        let provider = AgentProvider::Unknown(slug.clone());
        prop_assert_eq!(provider.display_name(), slug.as_str());
    }

    // 16. Unknown variant canonical_slug returns the inner string
    #[test]
    fn unknown_canonical_slug_is_inner(slug in "[a-z]{5,20}") {
        let provider = AgentProvider::Unknown(slug.clone());
        prop_assert_eq!(provider.canonical_slug(), slug.as_str());
    }

    // 17. Hash consistency
    #[test]
    fn hash_consistent(provider in arb_provider()) {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};
        let mut h1 = DefaultHasher::new();
        let mut h2 = DefaultHasher::new();
        provider.hash(&mut h1);
        provider.hash(&mut h2);
        prop_assert_eq!(h1.finish(), h2.finish());
    }

    // 18. Equality is reflexive
    #[test]
    fn equality_reflexive(provider in arb_provider()) {
        prop_assert_eq!(&provider, &provider);
    }

    // 19. Clone produces equal value
    #[test]
    fn clone_equals_original(provider in arb_provider()) {
        let cloned = provider.clone();
        prop_assert_eq!(provider, cloned);
    }

    // 20. all_known count is 12
    #[test]
    fn all_known_count(_dummy in 0..1u8) {
        prop_assert_eq!(AgentProvider::all_known().len(), 12);
    }

    // 21. all_known slugs are all unique
    #[test]
    fn all_known_slugs_unique(_dummy in 0..1u8) {
        let slugs: Vec<&str> = AgentProvider::all_known().iter()
            .map(|p| p.canonical_slug())
            .collect();
        let mut deduped = slugs.clone();
        deduped.sort();
        deduped.dedup();
        prop_assert_eq!(slugs.len(), deduped.len());
    }

    // 22. all_known contains every known variant
    #[test]
    fn all_known_contains_variant(provider in arb_known_provider()) {
        let all = AgentProvider::all_known();
        let found = all.iter().any(|p| p == &provider);
        prop_assert!(found);
    }

    // 23. serde kebab-case: known variants serialize to lowercase strings
    #[test]
    fn serde_kebab_case_lowercase(provider in arb_known_provider()) {
        let json = serde_json::to_string(&provider).unwrap();
        // Remove quotes
        let inner = &json[1..json.len()-1];
        // Should be all lowercase with hyphens
        prop_assert!(inner.chars().all(|c| c.is_ascii_lowercase() || c == '-'));
    }

    // 24. to_agent_type/from_agent_type roundtrip for supported providers
    #[test]
    fn agent_type_roundtrip_supported(provider in prop_oneof![
        Just(AgentProvider::Claude),
        Just(AgentProvider::Codex),
        Just(AgentProvider::Gemini),
    ]) {
        let agent_type = provider.to_agent_type();
        let rt = AgentProvider::from_agent_type(&agent_type);
        prop_assert_eq!(provider, rt);
    }

    // 25. to_agent_type for unsupported providers returns Unknown
    #[test]
    fn agent_type_unsupported_is_unknown(provider in prop_oneof![
        Just(AgentProvider::Cursor),
        Just(AgentProvider::Windsurf),
        Just(AgentProvider::Cline),
        Just(AgentProvider::GithubCopilot),
    ]) {
        let agent_type = provider.to_agent_type();
        let is_unknown = matches!(agent_type, frankenterm_core::patterns::AgentType::Unknown);
        prop_assert!(is_unknown);
    }

    // 26. from_slug known aliases map correctly
    #[test]
    fn from_slug_alias_claude(alias in prop_oneof![
        Just("claude"),
        Just("claude-code"),
        Just("claude_code"),
    ]) {
        prop_assert_eq!(AgentProvider::from_slug(alias), AgentProvider::Claude);
    }

    // 27. from_slug known aliases for copilot
    #[test]
    fn from_slug_alias_copilot(alias in prop_oneof![
        Just("copilot"),
        Just("github-copilot"),
        Just("gh-copilot"),
    ]) {
        prop_assert_eq!(AgentProvider::from_slug(alias), AgentProvider::GithubCopilot);
    }

    // 28. Two different Unknown strings are not equal
    #[test]
    fn unknown_different_strings_not_equal(
        a in "[a-z]{5,10}",
        b in "[A-Z]{5,10}",
    ) {
        let pa = AgentProvider::Unknown(a);
        let pb = AgentProvider::Unknown(b);
        prop_assert_ne!(pa, pb);
    }

    // 29. from_process_name with prefix+suffix still detects agent
    #[test]
    fn from_process_name_embedded(
        prefix in "[0-9]{1,5}",
        suffix in "[0-9]{1,5}",
    ) {
        let name = format!("{}claude{}", prefix, suffix);
        let result = AgentProvider::from_process_name(&name);
        prop_assert_eq!(result, Some(AgentProvider::Claude));
    }

    // 30. serde roundtrip with arbitrary provider (known + unknown)
    #[test]
    fn any_provider_serde_roundtrip(provider in arb_provider()) {
        let json_str = serde_json::to_string(&provider).unwrap();
        let rt: AgentProvider = serde_json::from_str(&json_str).unwrap();
        prop_assert_eq!(provider, rt);
    }
}
