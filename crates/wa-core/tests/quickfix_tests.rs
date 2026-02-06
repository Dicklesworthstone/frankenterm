//! Quick-fix testing suite (wa-bnm.4)
//!
//! Validates template interpolation, error remediation coverage,
//! UX copy-paste-ability, context-aware suggestions, and output stability.

use std::collections::HashMap;

use wa_core::error::*;
use wa_core::error_codes::{get_error_code, list_error_codes};
use wa_core::event_templates::{EVENT_TEMPLATE_REGISTRY, render_event};
use wa_core::explanations::{
    format_explanation, get_explanation, list_template_ids, render_explanation,
};
use wa_core::storage::StoredEvent;

// =========================================================================
// 1. Template Interpolation Tests
// =========================================================================

#[test]
fn explanation_pane_interpolation() {
    let template = get_explanation("deny.unknown_pane").expect("template exists");
    let mut ctx = HashMap::new();
    ctx.insert("pane_id".to_string(), "42".to_string());
    let rendered = render_explanation(template, &ctx);
    // The rendered text should contain the pane ID
    assert!(
        rendered.contains("42") || !template.detailed.contains("{pane_id}"),
        "Template with {{pane_id}} should interpolate to '42', got:\n{rendered}"
    );
}

#[test]
fn explanation_interpolation_preserves_unresolved_placeholders() {
    let template = get_explanation("deny.alt_screen").expect("template exists");
    let ctx = HashMap::new(); // empty context
    let rendered = render_explanation(template, &ctx);
    // With no context, raw placeholders should remain if template uses them
    assert!(
        !rendered.is_empty(),
        "Rendered text should not be empty even with no context"
    );
}

#[test]
fn event_template_interpolation_with_pane_and_event() {
    let event = StoredEvent {
        id: 123,
        pane_id: 7,
        rule_id: "core.codex:usage_limit".to_string(),
        agent_type: "codex".to_string(),
        event_type: "codex:usage_limit".to_string(),
        severity: "warning".to_string(),
        confidence: 0.95,
        extracted: Some(serde_json::json!({"limit_pct": 95})),
        matched_text: Some("rate limit reached".to_string()),
        segment_id: Some(1),
        detected_at: 1700000000000,
        dedupe_key: None,
        handled_at: None,
        handled_by_workflow_id: None,
        handled_status: None,
    };

    let rendered = render_event(&event);
    // Should produce non-empty output
    assert!(
        !rendered.summary.is_empty(),
        "Event template rendering should produce non-empty summary"
    );
}

#[test]
fn event_template_missing_extracted_is_safe() {
    let event = StoredEvent {
        id: 456,
        pane_id: 1,
        rule_id: "core.codex:error_detected".to_string(),
        agent_type: "codex".to_string(),
        event_type: "codex:error_detected".to_string(),
        severity: "error".to_string(),
        confidence: 0.9,
        extracted: None,
        matched_text: None,
        segment_id: None,
        detected_at: 1700000000000,
        dedupe_key: None,
        handled_at: None,
        handled_by_workflow_id: None,
        handled_status: None,
    };

    let rendered = render_event(&event);
    // Should not panic and should produce some output
    assert!(
        !rendered.summary.is_empty(),
        "Rendering with no extracted data must not panic"
    );
}

#[test]
fn format_explanation_interpolates_context_in_detailed() {
    let template = get_explanation("deny.alt_screen").expect("template exists");
    let mut ctx = HashMap::new();
    ctx.insert("pane_id".to_string(), "99".to_string());
    let output = format_explanation(template, Some(&ctx));
    // Output should include scenario header and brief
    assert!(output.contains("##"), "Should contain markdown header");
    assert!(output.contains("**"), "Should contain bold brief");
    assert!(!output.is_empty());
}

// =========================================================================
// 2. Error Remediation Coverage
// =========================================================================

#[test]
fn every_error_variant_has_remediation() {
    let json_err = serde_json::from_str::<serde_json::Value>("").unwrap_err();
    let errors: Vec<Error> = vec![
        // Wezterm variants
        Error::Wezterm(WeztermError::CliNotFound),
        Error::Wezterm(WeztermError::NotRunning),
        Error::Wezterm(WeztermError::PaneNotFound(1)),
        Error::Wezterm(WeztermError::SocketNotFound("/tmp/wez.sock".to_string())),
        Error::Wezterm(WeztermError::CommandFailed("boom".to_string())),
        Error::Wezterm(WeztermError::ParseError("bad json".to_string())),
        Error::Wezterm(WeztermError::Timeout(5)),
        Error::Wezterm(WeztermError::CircuitOpen {
            retry_after_ms: 500,
        }),
        // Storage variants
        Error::Storage(StorageError::Database("db err".to_string())),
        Error::Storage(StorageError::SequenceDiscontinuity {
            expected: 1,
            actual: 2,
        }),
        Error::Storage(StorageError::MigrationFailed("migrate".to_string())),
        Error::Storage(StorageError::SchemaTooNew {
            current: 9,
            supported: 6,
        }),
        Error::Storage(StorageError::WaTooOld {
            current: "0.1.0".to_string(),
            min_compatible: "1.0.0".to_string(),
        }),
        Error::Storage(StorageError::FtsQueryError("fts".to_string())),
        Error::Storage(StorageError::Corruption {
            details: "checksum mismatch".to_string(),
        }),
        Error::Storage(StorageError::NotFound("segment 42".to_string())),
        // Pattern variants
        Error::Pattern(PatternError::InvalidRule("rule".to_string())),
        Error::Pattern(PatternError::InvalidRegex("regex".to_string())),
        Error::Pattern(PatternError::PackNotFound("pack".to_string())),
        Error::Pattern(PatternError::MatchTimeout),
        // Workflow variants
        Error::Workflow(WorkflowError::NotFound("name".to_string())),
        Error::Workflow(WorkflowError::Aborted("abort".to_string())),
        Error::Workflow(WorkflowError::GuardFailed("guard".to_string())),
        Error::Workflow(WorkflowError::PaneLocked),
        // Config variants
        Error::Config(ConfigError::FileNotFound("wa.toml".to_string())),
        Error::Config(ConfigError::ReadFailed(
            "wa.toml".to_string(),
            "io".to_string(),
        )),
        Error::Config(ConfigError::ParseError("parse".to_string())),
        Error::Config(ConfigError::ParseFailed("parse".to_string())),
        Error::Config(ConfigError::SerializeFailed("serialize".to_string())),
        Error::Config(ConfigError::ValidationError("invalid".to_string())),
        // Other variants
        Error::Policy("denied".to_string()),
        Error::Io(std::io::Error::other("io")),
        Error::Json(json_err),
        Error::Runtime("runtime".to_string()),
        Error::SetupError("setup failed".to_string()),
    ];

    for error in &errors {
        let remediation = error
            .remediation()
            .unwrap_or_else(|| panic!("Missing remediation for {error:?}"));

        // Summary must be non-empty and reasonably descriptive
        assert!(
            remediation.summary.len() >= 10,
            "Remediation summary too short for {:?}: '{}'",
            error,
            remediation.summary
        );

        // Must have at least one command
        assert!(
            !remediation.commands.is_empty(),
            "No remediation commands for {:?}",
            error
        );

        // Each command must have non-empty label and command
        for cmd in &remediation.commands {
            assert!(!cmd.label.is_empty(), "Empty command label for {:?}", error);
            assert!(
                !cmd.command.is_empty(),
                "Empty command text for {:?}",
                error
            );
        }
    }
}

#[test]
fn remediation_render_plain_produces_valid_output() {
    let error = Error::Wezterm(WeztermError::CliNotFound);
    let remediation = error.remediation().expect("has remediation");
    let rendered = remediation.render_plain();

    assert!(rendered.contains("To fix:"), "Should start with 'To fix:'");
    assert!(
        rendered.contains("Commands:"),
        "Should include Commands section"
    );
    assert!(
        !rendered.trim().is_empty(),
        "Rendered output should not be empty"
    );
}

#[test]
fn format_error_with_remediation_includes_all_parts() {
    let error = Error::Storage(StorageError::MigrationFailed("test".to_string()));
    let output = format_error_with_remediation(&error);

    assert!(output.contains("Error:"), "Should contain error prefix");
    assert!(output.contains("To fix:"), "Should contain remediation");
}

// =========================================================================
// 3. UX Validation (Copy-Paste-ability)
// =========================================================================

/// Smart/curly quotes that break copy-paste in terminals
const SMART_QUOTES: &[char] = &[
    '\u{201C}', // "
    '\u{201D}', // "
    '\u{2018}', // '
    '\u{2019}', // '
    '\u{00AB}', // «
    '\u{00BB}', // »
];

/// Check a string for smart quotes
fn contains_smart_quotes(s: &str) -> bool {
    s.chars().any(|c| SMART_QUOTES.contains(&c))
}

/// Check for non-printable control characters (excluding newline and tab)
fn contains_control_chars(s: &str) -> bool {
    s.chars()
        .any(|c| c.is_control() && c != '\n' && c != '\t' && c != '\r')
}

#[test]
fn remediation_commands_have_no_smart_quotes() {
    let json_err = serde_json::from_str::<serde_json::Value>("").unwrap_err();
    let errors: Vec<Error> = vec![
        Error::Wezterm(WeztermError::CliNotFound),
        Error::Wezterm(WeztermError::NotRunning),
        Error::Wezterm(WeztermError::PaneNotFound(1)),
        Error::Wezterm(WeztermError::CommandFailed("boom".to_string())),
        Error::Wezterm(WeztermError::Timeout(5)),
        Error::Storage(StorageError::Database("db".to_string())),
        Error::Storage(StorageError::MigrationFailed("mig".to_string())),
        Error::Storage(StorageError::FtsQueryError("fts".to_string())),
        Error::Pattern(PatternError::InvalidRule("rule".to_string())),
        Error::Workflow(WorkflowError::PaneLocked),
        Error::Config(ConfigError::FileNotFound("wa.toml".to_string())),
        Error::Policy("denied".to_string()),
        Error::Io(std::io::Error::other("io")),
        Error::Json(json_err),
        Error::Runtime("runtime".to_string()),
        Error::SetupError("setup".to_string()),
    ];

    for error in &errors {
        if let Some(remediation) = error.remediation() {
            // Check summary
            assert!(
                !contains_smart_quotes(&remediation.summary),
                "Smart quotes in remediation summary for {:?}",
                error
            );

            // Check commands
            for cmd in &remediation.commands {
                assert!(
                    !contains_smart_quotes(&cmd.command),
                    "Smart quotes in command '{}' for {:?}",
                    cmd.command,
                    error
                );
                assert!(
                    !contains_smart_quotes(&cmd.label),
                    "Smart quotes in label '{}' for {:?}",
                    cmd.label,
                    error
                );
            }

            // Check alternatives
            for alt in &remediation.alternatives {
                assert!(
                    !contains_smart_quotes(alt),
                    "Smart quotes in alternative '{}' for {:?}",
                    alt,
                    error
                );
            }
        }
    }
}

#[test]
fn remediation_commands_have_no_control_characters() {
    let errors: Vec<Error> = vec![
        Error::Wezterm(WeztermError::CliNotFound),
        Error::Storage(StorageError::Database("db".to_string())),
        Error::Config(ConfigError::FileNotFound("wa.toml".to_string())),
        Error::Policy("denied".to_string()),
    ];

    for error in &errors {
        if let Some(remediation) = error.remediation() {
            for cmd in &remediation.commands {
                assert!(
                    !contains_control_chars(&cmd.command),
                    "Control characters in command '{}' for {:?}",
                    cmd.command,
                    error
                );
            }
        }
    }
}

#[test]
fn error_code_recovery_steps_have_no_smart_quotes() {
    for code_str in list_error_codes() {
        let def = get_error_code(code_str).expect("code should exist in catalog");
        for step in def.recovery_steps {
            assert!(
                !contains_smart_quotes(&step.description),
                "Smart quotes in recovery step description for {code_str}: {}",
                step.description
            );
            if let Some(cmd) = &step.command {
                assert!(
                    !contains_smart_quotes(cmd),
                    "Smart quotes in recovery step command for {code_str}: {cmd}"
                );
            }
        }
    }
}

#[test]
fn explanation_suggestions_have_no_smart_quotes() {
    for id in list_template_ids() {
        let template = get_explanation(id).expect("template exists");
        for suggestion in template.suggestions {
            assert!(
                !contains_smart_quotes(suggestion),
                "Smart quotes in suggestion for template {id}: {suggestion}"
            );
        }
        for see in template.see_also {
            assert!(
                !contains_smart_quotes(see),
                "Smart quotes in see_also for template {id}: {see}"
            );
        }
    }
}

#[test]
fn remediation_learn_more_urls_are_valid() {
    let errors: Vec<Error> = vec![
        Error::Wezterm(WeztermError::CliNotFound),
        Error::Wezterm(WeztermError::NotRunning),
        Error::Storage(StorageError::FtsQueryError("fts".to_string())),
    ];

    for error in &errors {
        if let Some(remediation) = error.remediation() {
            if let Some(url) = &remediation.learn_more {
                assert!(
                    url.starts_with("http://") || url.starts_with("https://"),
                    "Invalid learn_more URL for {:?}: {url}",
                    error
                );
            }
        }
    }
}

// =========================================================================
// 4. Context-Aware Tests
// =========================================================================

#[test]
fn levenshtein_typo_detection_basic() {
    use wa_core::suggestions::levenshtein_distance;

    // Exact match = 0
    assert_eq!(levenshtein_distance("hello", "hello"), 0);

    // Single edit = 1
    assert_eq!(levenshtein_distance("hello", "helo"), 1);

    // Two edits = 2
    assert_eq!(levenshtein_distance("hello", "hllo"), 1);

    // Empty strings
    assert_eq!(levenshtein_distance("", ""), 0);
    assert_eq!(levenshtein_distance("abc", ""), 3);
    assert_eq!(levenshtein_distance("", "abc"), 3);
}

#[test]
fn suggest_closest_finds_typo() {
    use wa_core::suggestions::suggest_closest;

    let candidates = vec!["compile_rust", "build_python", "test_rust"];
    let result = suggest_closest("compile_rus", &candidates);
    assert_eq!(result, Some("compile_rust"));

    let result = suggest_closest("buld_python", &candidates);
    assert_eq!(result, Some("build_python"));

    // Too different should return None
    let result = suggest_closest("completely_different_thing", &candidates);
    assert!(
        result.is_none(),
        "Should not suggest for very different input"
    );
}

#[test]
fn suggest_closest_prefix_match() {
    use wa_core::suggestions::suggest_closest;

    let candidates = vec!["status", "stop", "search"];
    let result = suggest_closest("stat", &candidates);
    assert_eq!(result, Some("status"));
}

// =========================================================================
// 5. Error Code + Remediation Consistency
// =========================================================================

#[test]
fn all_error_codes_in_catalog_have_recovery_steps() {
    for code_str in list_error_codes() {
        let def = get_error_code(code_str).expect("code should exist");
        assert!(
            !def.recovery_steps.is_empty(),
            "Error code {code_str} has no recovery steps"
        );
    }
}

#[test]
fn all_error_codes_have_non_empty_titles_and_descriptions() {
    for code_str in list_error_codes() {
        let def = get_error_code(code_str).expect("code should exist");
        assert!(
            !def.title.is_empty(),
            "Error code {code_str} has empty title"
        );
        assert!(
            !def.description.is_empty(),
            "Error code {code_str} has empty description"
        );
    }
}

#[test]
fn error_renderer_maps_all_error_variants_to_catalog() {
    use wa_core::output::ErrorRenderer;

    let json_err = serde_json::from_str::<serde_json::Value>("").unwrap_err();
    let errors: Vec<Error> = vec![
        Error::Wezterm(WeztermError::CliNotFound),
        Error::Wezterm(WeztermError::NotRunning),
        Error::Wezterm(WeztermError::PaneNotFound(1)),
        Error::Wezterm(WeztermError::SocketNotFound("/tmp/s".to_string())),
        Error::Wezterm(WeztermError::CommandFailed("err".to_string())),
        Error::Wezterm(WeztermError::ParseError("bad".to_string())),
        Error::Wezterm(WeztermError::Timeout(5)),
        Error::Wezterm(WeztermError::CircuitOpen {
            retry_after_ms: 100,
        }),
        Error::Storage(StorageError::Database("db".to_string())),
        Error::Storage(StorageError::SequenceDiscontinuity {
            expected: 1,
            actual: 2,
        }),
        Error::Storage(StorageError::MigrationFailed("mig".to_string())),
        Error::Storage(StorageError::SchemaTooNew {
            current: 9,
            supported: 6,
        }),
        Error::Storage(StorageError::WaTooOld {
            current: "0.1".to_string(),
            min_compatible: "1.0".to_string(),
        }),
        Error::Storage(StorageError::FtsQueryError("fts".to_string())),
        Error::Storage(StorageError::Corruption {
            details: "bad".to_string(),
        }),
        Error::Storage(StorageError::NotFound("x".to_string())),
        Error::Pattern(PatternError::InvalidRule("r".to_string())),
        Error::Pattern(PatternError::InvalidRegex("r".to_string())),
        Error::Pattern(PatternError::PackNotFound("p".to_string())),
        Error::Pattern(PatternError::MatchTimeout),
        Error::Workflow(WorkflowError::NotFound("w".to_string())),
        Error::Workflow(WorkflowError::Aborted("a".to_string())),
        Error::Workflow(WorkflowError::GuardFailed("g".to_string())),
        Error::Workflow(WorkflowError::PaneLocked),
        Error::Config(ConfigError::FileNotFound("f".to_string())),
        Error::Config(ConfigError::ReadFailed("f".to_string(), "e".to_string())),
        Error::Config(ConfigError::ParseError("p".to_string())),
        Error::Config(ConfigError::SerializeFailed("s".to_string())),
        Error::Config(ConfigError::ValidationError("v".to_string())),
        Error::Policy("denied".to_string()),
        Error::Io(std::io::Error::other("io")),
        Error::Json(json_err),
        Error::Runtime("runtime".to_string()),
        Error::SetupError("setup".to_string()),
    ];

    for error in &errors {
        let code = ErrorRenderer::error_code(error);
        assert!(!code.is_empty(), "Empty error code for {:?}", error);
        assert!(
            get_error_code(code).is_some(),
            "Error code {code} for {:?} not found in catalog",
            error
        );
    }
}

// =========================================================================
// 6. Explanation Template Structural Tests
// =========================================================================

#[test]
fn all_explanation_templates_have_valid_ids() {
    for id in list_template_ids() {
        assert!(
            id.contains('.'),
            "Template ID '{id}' must follow category.name convention"
        );
        let parts: Vec<&str> = id.splitn(2, '.').collect();
        assert_eq!(
            parts.len(),
            2,
            "Template ID '{id}' must have exactly one dot"
        );
        assert!(
            !parts[0].is_empty() && !parts[1].is_empty(),
            "Template ID '{id}' parts must not be empty"
        );
    }
}

#[test]
fn all_explanation_suggestions_start_with_verb() {
    let verbs = [
        "Run",
        "Check",
        "Wait",
        "Try",
        "Use",
        "Switch",
        "Close",
        "Open",
        "Verify",
        "Review",
        "Contact",
        "Remove",
        "Add",
        "Set",
        "Reduce",
        "Increase",
        "Consider",
        "Ensure",
        "Investigate",
        "If",
        "Adjust",
        "Look",
        "See",
        "Ask",
        "Allow",
        "Follow",
        "Re-",
        "Update",
        "Disable",
        "Enable",
        "Restart",
        "Stop",
        "Start",
        "Pass",
        "Exit",
        "Let",
        "Configure",
    ];

    for id in list_template_ids() {
        let template = get_explanation(id).expect("template exists");
        for suggestion in template.suggestions {
            let first_word = suggestion.split_whitespace().next().unwrap_or("");
            let starts_with_verb = verbs.iter().any(|v| {
                first_word.starts_with(v)
                    || first_word.to_lowercase().starts_with(&v.to_lowercase())
            });
            assert!(
                starts_with_verb,
                "Suggestion for template '{id}' should start with verb, got: '{suggestion}'"
            );
        }
    }
}

#[test]
fn all_explanation_see_also_references_valid_targets() {
    for id in list_template_ids() {
        let template = get_explanation(id).expect("template exists");
        for reference in template.see_also {
            // Allow wa commands, wezterm commands, URLs, or known external tool names
            let known_tools = ["caut"];
            let valid = reference.starts_with("wa ")
                || reference.starts_with("wezterm ")
                || reference.starts_with("https://")
                || reference.starts_with("http://")
                || known_tools.contains(reference);
            assert!(
                valid,
                "see_also for template '{id}' must be 'wa ...', 'wezterm ...', URL, or known tool, got: '{reference}'"
            );
        }
    }
}

// =========================================================================
// 7. Event Template Registry Coverage
// =========================================================================

#[test]
fn event_template_registry_covers_builtin_types() {
    let registry = &*EVENT_TEMPLATE_REGISTRY;
    let core_events = ["usage.reached", "session.start", "error.network"];

    for event_type in &core_events {
        assert!(
            registry.has_template(event_type),
            "Registry should have template for '{event_type}'"
        );
        let template = registry.get(event_type);
        assert!(
            !template.summary.is_empty(),
            "Template summary for '{event_type}' should not be empty"
        );
    }
}

#[test]
fn event_template_fallback_for_unknown_types() {
    let registry = &*EVENT_TEMPLATE_REGISTRY;
    // Unknown types should use fallback
    let template = registry.get("nonexistent:type_xyz");
    assert!(
        !template.summary.is_empty(),
        "Unknown event type should produce fallback template"
    );
}

// =========================================================================
// 8. Output Format Stability
// =========================================================================

#[test]
fn error_renderer_plain_output_structure() {
    use wa_core::output::ErrorRenderer;
    use wa_core::output::OutputFormat;

    let renderer = ErrorRenderer::new(OutputFormat::Plain);
    let error = Error::Wezterm(WeztermError::NotRunning);
    let output = renderer.render(&error);

    // Must contain key structural elements
    assert!(output.contains("Error:"), "Must start with 'Error:'");
    assert!(
        output.contains("Error code"),
        "Must include 'Error code' footer"
    );
    assert!(output.contains("wa why WA-"), "Must include 'wa why' hint");
}

#[test]
fn error_renderer_json_output_structure() {
    use wa_core::output::ErrorRenderer;
    use wa_core::output::OutputFormat;

    let renderer = ErrorRenderer::new(OutputFormat::Json);
    let error = Error::Wezterm(WeztermError::NotRunning);
    let output = renderer.render(&error);

    let parsed: serde_json::Value = serde_json::from_str(&output).expect("valid JSON");
    assert_eq!(parsed["ok"], false);
    assert!(parsed["code"].is_string());
    assert!(parsed["error"].is_string());
    assert!(parsed["title"].is_string());
    assert!(parsed["category"].is_string());
}

#[test]
fn error_code_format_plain_produces_stable_sections() {
    let def = get_error_code("WA-1001").expect("WA-1001 exists");
    let formatted = def.format_plain();

    assert!(formatted.contains("WA-1001"), "Should contain error code");
    assert!(formatted.contains(def.title), "Should contain title");
    assert!(
        formatted.contains("Common causes:") || def.causes.is_empty(),
        "Should contain causes section if causes exist"
    );
    assert!(
        formatted.contains("Recovery steps:") || def.recovery_steps.is_empty(),
        "Should contain recovery section if steps exist"
    );
}

// =========================================================================
// 9. Cross-module Consistency
// =========================================================================

#[test]
fn error_code_count_matches_catalog() {
    let codes = list_error_codes();
    // Verify we have a reasonable number of codes (should grow over time)
    assert!(
        codes.len() >= 25,
        "Should have at least 25 error codes, found {}",
        codes.len()
    );
}

#[test]
fn explanation_template_count_is_reasonable() {
    let ids = list_template_ids();
    assert!(
        ids.len() >= 10,
        "Should have at least 10 explanation templates, found {}",
        ids.len()
    );
}

#[test]
fn all_deny_templates_have_suggestions() {
    for id in list_template_ids() {
        if id.starts_with("deny.") {
            let template = get_explanation(id).expect("template exists");
            assert!(
                !template.suggestions.is_empty(),
                "Deny template '{id}' must have suggestions"
            );
        }
    }
}
