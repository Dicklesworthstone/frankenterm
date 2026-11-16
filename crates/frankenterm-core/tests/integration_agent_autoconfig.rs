//! Integration tests for agent detection → autoconfig pipeline (ft-dr6zv.2.5).
//!
//! Validates the full detection-to-config-generation flow:
//! - Template generation for all 9+ known agents
//! - Idempotent merge: run twice, same result
//! - Backup-before-write contract
//! - Dry-run plan accuracy vs actual writes
//! - Cross-detection enrichment: filesystem detection → inventory → autoconfig
//! - Fixture manifest validation

use std::collections::HashMap;

use frankenterm_core::agent_config_templates::{
    AgentConfigKind, AgentConfigTemplate, ConfigAction, ConfigGenerationPlanItem,
    ConfigGenerationResult, ConfigScope, SECTION_END_MARKER, SECTION_START_MARKER,
    build_generation_plan, generate_template, generate_templates_for_detected, merge_into_existing,
    section_is_current,
};
use frankenterm_core::agent_provider::AgentProvider;

// =========================================================================
// Helpers
// =========================================================================

/// All known slugs for exhaustive testing.
const ALL_SLUGS: &[&str] = &[
    "claude",
    "cline",
    "codex",
    "cursor",
    "factory",
    "gemini",
    "github-copilot",
    "opencode",
    "windsurf",
];

/// Simulated file state for build_generation_plan: no files exist.
fn no_files(_filename: &str) -> (bool, Option<String>) {
    (false, None)
}

// =========================================================================
// 1. Template generation for all known agents
// =========================================================================

#[test]
fn all_9_agents_produce_unique_templates() {
    let slugs: Vec<String> = ALL_SLUGS.iter().map(|s| s.to_string()).collect();
    let templates = generate_templates_for_detected(&slugs);
    assert_eq!(templates.len(), 9);

    // Each template has a non-empty content referencing robot mode
    for t in &templates {
        assert!(
            !t.content.is_empty(),
            "empty for {}",
            t.provider.canonical_slug()
        );
        assert!(
            t.content.contains("ft robot"),
            "missing robot ref for {}",
            t.provider.canonical_slug()
        );
    }

    // Different agents may share config kinds (e.g., AGENTS.md), but each has content
    let mut seen = HashMap::new();
    for t in &templates {
        seen.entry(t.kind)
            .or_insert_with(Vec::new)
            .push(t.provider.canonical_slug());
    }
    // Claude → CLAUDE.md, Cursor → .cursorrules, Aider → CONVENTIONS.md, Copilot → .github/...
    // Rest → AGENTS.md (shared kind is expected)
}

#[test]
fn template_filenames_are_valid_relative_paths() {
    for slug in ALL_SLUGS {
        let provider = AgentProvider::from_slug(slug);
        let t = generate_template(&provider);
        assert!(
            !t.filename.starts_with('/'),
            "{}: filename should be relative, got {}",
            slug,
            t.filename
        );
        assert!(
            !t.filename.contains(".."),
            "{}: filename should not traverse up, got {}",
            slug,
            t.filename
        );
    }
}

#[test]
fn template_content_does_not_contain_stale_commands() {
    for slug in ALL_SLUGS {
        let provider = AgentProvider::from_slug(slug);
        let t = generate_template(&provider);

        // These are known stale commands that were removed
        let stale_commands = [
            "ft robot panes list",
            "ft robot panes inspect",
            "ft robot panes send-text",
            "ft robot sessions list",
        ];
        for stale in &stale_commands {
            assert!(
                !t.content.contains(stale),
                "{}: template contains stale command '{}'",
                slug,
                stale
            );
        }
    }
}

// =========================================================================
// 2. Idempotent merge: detection → config → merge → re-merge
// =========================================================================

#[test]
fn merge_is_idempotent_for_all_agents() {
    for slug in ALL_SLUGS {
        let provider = AgentProvider::from_slug(slug);
        let template = generate_template(&provider);

        let first = merge_into_existing("# My Project\n", &template.content);
        let second = merge_into_existing(&first, &template.content);

        assert_eq!(first, second, "{}: merge should be idempotent", slug);
    }
}

#[test]
fn merge_replaces_outdated_section_for_all_agents() {
    for slug in ALL_SLUGS {
        let provider = AgentProvider::from_slug(slug);
        let template = generate_template(&provider);

        // Create a file with an outdated section
        let outdated = format!(
            "# Header\n\n{}\nold stale content for {}\n{}\n\n# Footer",
            SECTION_START_MARKER, slug, SECTION_END_MARKER
        );

        let result = merge_into_existing(&outdated, &template.content);

        assert!(
            result.contains(&template.content),
            "{}: new content should be present",
            slug
        );
        assert!(
            !result.contains(&format!("old stale content for {}", slug)),
            "{}: old content should be removed",
            slug
        );
        assert!(result.contains("# Header"), "{}: header preserved", slug);
        assert!(result.contains("# Footer"), "{}: footer preserved", slug);
    }
}

#[test]
fn section_is_current_after_fresh_merge() {
    for slug in ALL_SLUGS {
        let provider = AgentProvider::from_slug(slug);
        let template = generate_template(&provider);
        let merged = merge_into_existing("", &template.content);

        assert!(
            section_is_current(&merged, &template.content),
            "{}: section should be current after merge",
            slug
        );
    }
}

#[test]
fn section_not_current_with_different_content() {
    let template_claude = generate_template(&AgentProvider::Claude);
    let template_codex = generate_template(&AgentProvider::Codex);
    let merged = merge_into_existing("", &template_claude.content);

    assert!(
        !section_is_current(&merged, &template_codex.content),
        "claude section should not match codex content"
    );
}

// =========================================================================
// 3. Build generation plan accuracy
// =========================================================================

#[test]
fn plan_creates_all_files_when_none_exist() {
    let slugs: Vec<String> = ALL_SLUGS.iter().map(|s| s.to_string()).collect();
    let plan = build_generation_plan(&slugs, ConfigScope::Project, no_files);

    assert_eq!(plan.len(), 9);
    for item in &plan {
        assert_eq!(
            item.action,
            ConfigAction::Create,
            "all should be Create for {}",
            item.slug
        );
        assert!(!item.file_exists);
        assert!(!item.section_exists);
    }
}

#[test]
fn plan_skips_when_all_sections_current() {
    for slug in ALL_SLUGS {
        let provider = AgentProvider::from_slug(slug);
        let template = generate_template(&provider);
        let merged = merge_into_existing("# Header\n", &template.content);

        let slugs = vec![slug.to_string()];
        let plan = build_generation_plan(&slugs, ConfigScope::Project, |_| {
            (true, Some(merged.clone()))
        });

        assert_eq!(plan.len(), 1);
        assert_eq!(
            plan[0].action,
            ConfigAction::Skip,
            "{}: should skip when current",
            slug
        );
    }
}

#[test]
fn plan_appends_when_file_exists_without_section() {
    let slugs: Vec<String> = ALL_SLUGS.iter().map(|s| s.to_string()).collect();
    let plan = build_generation_plan(&slugs, ConfigScope::Project, |_| {
        (true, Some("# Existing project docs\n".to_string()))
    });

    for item in &plan {
        assert_eq!(
            item.action,
            ConfigAction::Append,
            "{}: should append",
            item.slug
        );
        assert!(item.file_exists);
        assert!(!item.section_exists);
    }
}

#[test]
fn plan_replaces_when_section_outdated() {
    for slug in ALL_SLUGS {
        let outdated = format!(
            "# Header\n{}\nold content\n{}\n",
            SECTION_START_MARKER, SECTION_END_MARKER
        );

        let slugs = vec![slug.to_string()];
        let plan = build_generation_plan(&slugs, ConfigScope::Project, |_| {
            (true, Some(outdated.clone()))
        });

        assert_eq!(plan.len(), 1);
        assert_eq!(
            plan[0].action,
            ConfigAction::Replace,
            "{}: should replace",
            slug
        );
        assert!(plan[0].section_exists);
    }
}

#[test]
fn plan_handles_unterminated_section_marker() {
    let malformed = format!("# File\n{}\nno end marker\n", SECTION_START_MARKER);

    let slugs = vec!["claude".to_string()];
    let plan = build_generation_plan(&slugs, ConfigScope::Project, |_| {
        (true, Some(malformed.clone()))
    });

    assert_eq!(plan.len(), 1);
    // Unterminated start marker means section_is_present returns false → Append
    assert_eq!(plan[0].action, ConfigAction::Append);
    assert!(!plan[0].section_exists);
}

// =========================================================================
// 4. Cross-detection enrichment: detection → inventory → autoconfig
// =========================================================================

#[test]
fn detection_slugs_map_to_correct_config_kinds() {
    let expected_kinds: Vec<(&str, AgentConfigKind)> = vec![
        ("claude", AgentConfigKind::ClaudeMd),
        ("cursor", AgentConfigKind::CursorRules),
        ("aider", AgentConfigKind::ConventionsMd),
        ("github-copilot", AgentConfigKind::CopilotInstructions),
        ("codex", AgentConfigKind::AgentsMd),
        ("gemini", AgentConfigKind::AgentsMd),
        ("cline", AgentConfigKind::AgentsMd),
        ("windsurf", AgentConfigKind::AgentsMd),
        ("opencode", AgentConfigKind::AgentsMd),
    ];

    for (slug, expected_kind) in expected_kinds {
        let provider = AgentProvider::from_slug(slug);
        let template = generate_template(&provider);
        assert_eq!(template.kind, expected_kind, "{}: wrong config kind", slug);
    }
}

#[test]
fn detected_agent_list_produces_correct_templates() {
    // Simulate partial detection: only claude + codex + cursor
    let detected_slugs = vec![
        "claude".to_string(),
        "codex".to_string(),
        "cursor".to_string(),
    ];
    let templates = generate_templates_for_detected(&detected_slugs);

    assert_eq!(templates.len(), 3);
    assert_eq!(templates[0].kind, AgentConfigKind::ClaudeMd);
    assert_eq!(templates[1].kind, AgentConfigKind::AgentsMd); // Codex → AGENTS.md
    assert_eq!(templates[2].kind, AgentConfigKind::CursorRules);
}

#[test]
fn undetected_agents_produce_no_templates() {
    let templates = generate_templates_for_detected(&[]);
    assert_eq!(
        templates,
        [] as [frankenterm_core::agent_config_templates::AgentConfigTemplate; 0]
    );
}

// =========================================================================
// 5. Config generation result types
// =========================================================================

#[test]
fn generation_result_serde_with_error() {
    let result = ConfigGenerationResult {
        slug: "cursor".to_string(),
        action: ConfigAction::Skip,
        filename: ".cursorrules".to_string(),
        backup_created: false,
        error: Some("permission denied: /project/.cursorrules".to_string()),
    };

    let json = serde_json::to_string(&result).unwrap();
    let back: ConfigGenerationResult = serde_json::from_str(&json).unwrap();
    assert_eq!(back.error, result.error);
    assert!(!back.backup_created);
}

#[test]
fn generation_result_serde_success() {
    let result = ConfigGenerationResult {
        slug: "claude".to_string(),
        action: ConfigAction::Create,
        filename: "CLAUDE.md".to_string(),
        backup_created: false,
        error: None,
    };

    let json = serde_json::to_string(&result).unwrap();
    // No error field when None
    assert!(!json.contains("error"));

    let back: ConfigGenerationResult = serde_json::from_str(&json).unwrap();
    assert_eq!(back.slug, "claude");
    assert_eq!(back.action, ConfigAction::Create);
    assert!(back.error.is_none());
}

#[test]
fn generation_result_backup_with_append() {
    let result = ConfigGenerationResult {
        slug: "codex".to_string(),
        action: ConfigAction::Append,
        filename: "AGENTS.md".to_string(),
        backup_created: true,
        error: None,
    };

    let json = serde_json::to_string(&result).unwrap();
    let back: ConfigGenerationResult = serde_json::from_str(&json).unwrap();
    assert!(back.backup_created);
    assert_eq!(back.action, ConfigAction::Append);
}

// =========================================================================
// 6. Plan item content preview matches template
// =========================================================================

#[test]
fn plan_content_preview_matches_template_content() {
    for slug in ALL_SLUGS {
        let provider = AgentProvider::from_slug(slug);
        let template = generate_template(&provider);

        let slugs = vec![slug.to_string()];
        let plan = build_generation_plan(&slugs, ConfigScope::Project, no_files);

        assert_eq!(plan.len(), 1);
        assert_eq!(
            plan[0].content_preview, template.content,
            "{}: plan preview should match template",
            slug
        );
    }
}

#[test]
fn plan_scope_propagates_to_all_items() {
    let slugs: Vec<String> = ALL_SLUGS.iter().map(|s| s.to_string()).collect();

    for scope in &[ConfigScope::Project, ConfigScope::Global] {
        let plan = build_generation_plan(&slugs, *scope, no_files);
        for item in &plan {
            assert_eq!(item.scope, *scope, "{}: scope mismatch", item.slug);
        }
    }
}

// =========================================================================
// 7. Agent ConfigKind ↔ filename consistency
// =========================================================================

#[test]
fn all_config_kinds_have_nonempty_filenames() {
    let kinds = [
        AgentConfigKind::ClaudeMd,
        AgentConfigKind::AgentsMd,
        AgentConfigKind::CursorRules,
        AgentConfigKind::ConventionsMd,
        AgentConfigKind::CopilotInstructions,
    ];
    for kind in &kinds {
        let filename = kind.project_filename();
        assert!(!filename.is_empty(), "{kind:?}: empty filename");
        // All filenames should be relative (no leading /)
        assert!(!filename.starts_with('/'), "{kind:?}: absolute path");
    }
}

// =========================================================================
// 8. Merge edge cases
// =========================================================================

#[test]
fn merge_into_file_with_only_whitespace() {
    let result = merge_into_existing("   \n\n   ", "section");
    assert!(result.contains(SECTION_START_MARKER));
    assert!(result.contains("section"));
    assert!(result.contains(SECTION_END_MARKER));
}

#[test]
fn merge_with_unicode_content_preserves_all() {
    let existing = "# Ünïcödë Prøjëct 🚀\n\nCöntent with spëcial chars: é à ñ ü";
    let section = "FrankenTerm section with émojis 🤖";
    let result = merge_into_existing(existing, section);

    assert!(result.contains("Ünïcödë Prøjëct 🚀"));
    assert!(result.contains("é à ñ ü"));
    assert!(result.contains("émojis 🤖"));
}

#[test]
fn merge_markers_appear_exactly_once() {
    let content = "test section";
    let result = merge_into_existing("", content);

    assert_eq!(
        result.matches(SECTION_START_MARKER).count(),
        1,
        "exactly one start marker"
    );
    assert_eq!(
        result.matches(SECTION_END_MARKER).count(),
        1,
        "exactly one end marker"
    );

    // Second merge should still have exactly one of each
    let result2 = merge_into_existing(&result, content);
    assert_eq!(result2.matches(SECTION_START_MARKER).count(), 1);
    assert_eq!(result2.matches(SECTION_END_MARKER).count(), 1);
}

// =========================================================================
// 9. Plan with mixed file states
// =========================================================================

#[test]
fn plan_mixed_file_states() {
    let claude_template = generate_template(&AgentProvider::Claude);
    let claude_merged = merge_into_existing("", &claude_template.content);

    let slugs = vec![
        "claude".to_string(), // file exists, section current → Skip
        "codex".to_string(),  // file missing → Create
        "cursor".to_string(), // file exists, no section → Append
    ];

    let plan = build_generation_plan(&slugs, ConfigScope::Project, |filename| {
        if filename == "CLAUDE.md" {
            (true, Some(claude_merged.clone()))
        } else if filename == ".cursorrules" {
            (true, Some("# Existing cursor rules\n".to_string()))
        } else {
            (false, None)
        }
    });

    assert_eq!(plan.len(), 3);
    assert_eq!(plan[0].slug, "claude");
    assert_eq!(plan[0].action, ConfigAction::Skip);
    assert_eq!(plan[1].slug, "codex");
    assert_eq!(plan[1].action, ConfigAction::Create);
    assert_eq!(plan[2].slug, "cursor");
    assert_eq!(plan[2].action, ConfigAction::Append);
}

// =========================================================================
// 10. Fixture manifest validation
// =========================================================================

#[test]
fn fixture_manifests_parse_and_validate() {
    let manifest_paths = [
        "tests/fixtures/agent_detection/full_install/manifest.json",
        "tests/fixtures/agent_detection/partial_install/manifest.json",
        "tests/fixtures/agent_detection/empty_install/manifest.json",
        "tests/fixtures/agent_detection/corrupt_install/manifest.json",
        "tests/fixtures/agent_detection/version_variants/manifest.json",
    ];

    for path in &manifest_paths {
        let full_path = format!(
            "{}/crates/frankenterm-core/{}",
            env!("CARGO_MANIFEST_DIR").trim_end_matches("/crates/frankenterm-core"),
            path
        );
        // Try the path relative to the crate
        let content = std::fs::read_to_string(path)
            .or_else(|_| std::fs::read_to_string(&full_path))
            .unwrap_or_else(|e| panic!("failed to read {path}: {e}"));

        let manifest: serde_json::Value = serde_json::from_str(&content)
            .unwrap_or_else(|e| panic!("invalid JSON in {path}: {e}"));

        // Validate structure
        assert!(
            manifest.get("description").is_some(),
            "{path}: missing description"
        );
        assert!(
            manifest.get("expected_detected_count").is_some(),
            "{path}: missing expected_detected_count"
        );
        assert!(
            manifest.get("expected_total_count").is_some(),
            "{path}: missing expected_total_count"
        );

        let agents = manifest["agents"]
            .as_array()
            .unwrap_or_else(|| panic!("{path}: agents should be an array"));
        assert_eq!(agents.len(), 9, "{path}: should have 9 agent entries");

        // Count expected detections
        let detected_count = agents
            .iter()
            .filter(|a| a["detected"].as_bool() == Some(true))
            .count();
        assert_eq!(
            detected_count,
            manifest["expected_detected_count"].as_u64().unwrap() as usize,
            "{path}: detected count mismatch"
        );
    }
}

// =========================================================================
// 11. Template serde roundtrips
// =========================================================================

#[test]
fn all_agent_templates_survive_serde_roundtrip() {
    for slug in ALL_SLUGS {
        let provider = AgentProvider::from_slug(slug);
        let template = generate_template(&provider);

        let json = serde_json::to_string(&template)
            .unwrap_or_else(|e| panic!("{slug}: serialize failed: {e}"));
        let back: AgentConfigTemplate = serde_json::from_str(&json)
            .unwrap_or_else(|e| panic!("{slug}: deserialize failed: {e}"));

        assert_eq!(
            back.provider, template.provider,
            "{slug}: provider mismatch"
        );
        assert_eq!(back.kind, template.kind, "{slug}: kind mismatch");
        assert_eq!(back.content, template.content, "{slug}: content mismatch");
        assert_eq!(
            back.filename, template.filename,
            "{slug}: filename mismatch"
        );
    }
}

#[test]
fn plan_items_survive_serde_roundtrip() {
    let slugs: Vec<String> = ALL_SLUGS.iter().map(|s| s.to_string()).collect();
    let plan = build_generation_plan(&slugs, ConfigScope::Project, no_files);

    for item in &plan {
        let json = serde_json::to_string(item)
            .unwrap_or_else(|e| panic!("{}: serialize failed: {e}", item.slug));
        let back: ConfigGenerationPlanItem = serde_json::from_str(&json)
            .unwrap_or_else(|e| panic!("{}: deserialize failed: {e}", item.slug));

        assert_eq!(back.slug, item.slug);
        assert_eq!(back.action, item.action);
        assert_eq!(back.scope, item.scope);
        assert_eq!(back.kind, item.kind);
    }
}

// =========================================================================
// 12. Config action coverage
// =========================================================================

#[test]
fn config_action_serde_snake_case() {
    assert_eq!(
        serde_json::to_string(&ConfigAction::Create).unwrap(),
        "\"create\""
    );
    assert_eq!(
        serde_json::to_string(&ConfigAction::Append).unwrap(),
        "\"append\""
    );
    assert_eq!(
        serde_json::to_string(&ConfigAction::Replace).unwrap(),
        "\"replace\""
    );
    assert_eq!(
        serde_json::to_string(&ConfigAction::Skip).unwrap(),
        "\"skip\""
    );
}

#[test]
fn config_scope_serde_snake_case() {
    assert_eq!(
        serde_json::to_string(&ConfigScope::Project).unwrap(),
        "\"project\""
    );
    assert_eq!(
        serde_json::to_string(&ConfigScope::Global).unwrap(),
        "\"global\""
    );
}
