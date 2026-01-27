//! E2E tests for wa explainability features (`wa why`, enriched errors, decision tracing).
//!
//! These tests validate:
//! - `wa why --list` returns all explanation templates
//! - `wa why <template_id>` returns structured explanations with remediation
//! - Policy decisions include actionable context
//! - Error messages include remediation hints
//!
//! # Test Approach
//!
//! Tests use the wa-core API directly rather than spawning CLI processes,
//! which allows testing in environments without the binary installed.
//! This validates the same code paths used by the CLI.
//!
//! # Artifact Capture
//!
//! On failure, tests log:
//! - Full explanation/error output
//! - Expected vs actual values
//! - Template IDs involved
//!
//! # Related Beads
//!
//! - bd-6qum: E2E: wa why + enriched errors with verbose logging
//! - wa-2ep: [EPIC] Deep Explainability

use std::collections::HashMap;

use wa_core::explanations::{
    format_explanation, get_explanation, list_template_ids, list_templates_by_category,
    render_explanation,
};
use wa_core::error_codes::{get_error_code, list_error_codes, ErrorCategory};

// ============================================================================
// wa why --list tests
// ============================================================================

#[test]
fn wa_why_list_returns_all_templates() {
    let ids = list_template_ids();

    // Should have substantial coverage
    assert!(
        ids.len() >= 10,
        "Expected at least 10 explanation templates, got {}",
        ids.len()
    );

    // Log for debugging on failure
    eprintln!("[ARTIFACT] Template count: {}", ids.len());
    for id in &ids {
        eprintln!("[ARTIFACT] Template ID: {}", id);
    }
}

#[test]
fn wa_why_list_includes_core_templates() {
    let ids = list_template_ids();

    // These are critical templates that must exist
    let required = [
        "deny.alt_screen",
        "deny.command_running",
        "workflow.usage_limit",
    ];

    for req in required {
        assert!(
            ids.contains(&req),
            "Missing required template: {}. Available: {:?}",
            req,
            ids
        );
    }
}

#[test]
fn wa_why_list_by_category_filters_correctly() {
    // Test deny category
    let denials = list_templates_by_category("deny");
    assert!(!denials.is_empty(), "deny category should not be empty");
    for tmpl in &denials {
        assert!(
            tmpl.id.starts_with("deny."),
            "Template {} in deny category should have deny. prefix",
            tmpl.id
        );
    }
    eprintln!("[ARTIFACT] deny category count: {}", denials.len());

    // Test workflow category
    let workflows = list_templates_by_category("workflow");
    assert!(!workflows.is_empty(), "workflow category should not be empty");
    for tmpl in &workflows {
        assert!(
            tmpl.id.starts_with("workflow."),
            "Template {} in workflow category should have workflow. prefix",
            tmpl.id
        );
    }
    eprintln!("[ARTIFACT] workflow category count: {}", workflows.len());

    // Test event category
    let events = list_templates_by_category("event");
    // Events may be empty in some configurations
    eprintln!("[ARTIFACT] event category count: {}", events.len());
}

// ============================================================================
// wa why <template_id> tests
// ============================================================================

#[test]
fn wa_why_template_returns_structured_output() {
    let template = get_explanation("deny.alt_screen")
        .expect("deny.alt_screen template should exist");

    // Verify all required fields are populated
    assert!(!template.id.is_empty(), "Template ID should not be empty");
    assert!(
        !template.scenario.is_empty(),
        "Scenario should not be empty"
    );
    assert!(!template.brief.is_empty(), "Brief should not be empty");
    assert!(
        !template.detailed.is_empty(),
        "Detailed explanation should not be empty"
    );
    assert!(
        !template.suggestions.is_empty(),
        "Suggestions should not be empty"
    );
    assert!(
        !template.see_also.is_empty(),
        "See-also references should not be empty"
    );

    // Log full output for artifact capture
    eprintln!("[ARTIFACT] Template ID: {}", template.id);
    eprintln!("[ARTIFACT] Scenario: {}", template.scenario);
    eprintln!("[ARTIFACT] Brief: {}", template.brief);
    eprintln!("[ARTIFACT] Detailed length: {}", template.detailed.len());
    eprintln!(
        "[ARTIFACT] Suggestions count: {}",
        template.suggestions.len()
    );
}

#[test]
fn wa_why_template_includes_remediation() {
    // Test several templates to ensure they all have actionable remediation
    let template_ids = ["deny.alt_screen", "deny.command_running", "workflow.usage_limit"];

    for id in template_ids {
        let template = get_explanation(id).unwrap_or_else(|| panic!("Template {} should exist", id));

        // Suggestions must be actionable (start with verbs)
        for suggestion in template.suggestions {
            let first_word = suggestion.split_whitespace().next().unwrap_or("");
            let actionable_verbs = [
                "Exit", "Use", "Wait", "Check", "Run", "Configure", "Review",
                "Let", "Consider", "Reduce", "Increase", "Verify", "Add",
                "Update", "Enable",
            ];

            let is_actionable = actionable_verbs.iter().any(|v| first_word.starts_with(v));
            assert!(
                is_actionable,
                "Suggestion '{}' in template {} should start with actionable verb. Got: '{}'",
                suggestion,
                id,
                first_word
            );
        }

        // See-also must reference valid commands
        for reference in template.see_also {
            assert!(
                reference.starts_with("wa ") || reference == &"caut",
                "See-also '{}' in template {} should be wa command or known tool",
                reference,
                id
            );
        }

        eprintln!("[ARTIFACT] Template {} has {} suggestions", id, template.suggestions.len());
    }
}

#[test]
fn wa_why_unknown_template_returns_none() {
    assert!(
        get_explanation("nonexistent.template").is_none(),
        "Unknown template should return None"
    );
    assert!(
        get_explanation("").is_none(),
        "Empty template ID should return None"
    );
    assert!(
        get_explanation("deny").is_none(),
        "Partial template ID should return None"
    );
}

// ============================================================================
// Explanation rendering tests
// ============================================================================

#[test]
fn format_explanation_produces_readable_output() {
    let template = get_explanation("deny.alt_screen").unwrap();
    let formatted = format_explanation(template, None);

    // Should have markdown structure
    assert!(
        formatted.contains("##"),
        "Formatted output should have markdown headers"
    );
    assert!(
        formatted.contains(template.scenario),
        "Formatted output should include scenario"
    );
    assert!(
        formatted.contains(template.brief),
        "Formatted output should include brief"
    );
    assert!(
        formatted.contains("Suggestions") || formatted.contains("suggestions"),
        "Formatted output should have suggestions section"
    );

    // Log full output for manual inspection
    eprintln!("[ARTIFACT] Formatted explanation:\n{}", formatted);
}

#[test]
fn render_explanation_interpolates_context() {
    let template = get_explanation("deny.alt_screen").unwrap();

    let mut context = HashMap::new();
    context.insert("pane_id".to_string(), "42".to_string());
    context.insert("app_name".to_string(), "vim".to_string());

    let rendered = render_explanation(template, &context);

    // Should not panic and should return non-empty content
    assert!(!rendered.is_empty(), "Rendered explanation should not be empty");

    eprintln!("[ARTIFACT] Rendered with context: {}", rendered.len());
}

#[test]
fn render_explanation_handles_empty_context() {
    let template = get_explanation("deny.alt_screen").unwrap();
    let context = HashMap::new();

    let rendered = render_explanation(template, &context);

    // Should return the detailed explanation unchanged
    assert_eq!(
        rendered, template.detailed,
        "Empty context should return detailed explanation unchanged"
    );
}

// ============================================================================
// Error code tests
// ============================================================================

#[test]
fn error_codes_are_documented() {
    let codes = list_error_codes();

    // Should have substantial coverage
    assert!(
        codes.len() >= 5,
        "Expected at least 5 documented error codes, got {}",
        codes.len()
    );

    eprintln!("[ARTIFACT] Documented error codes: {}", codes.len());
    for code in &codes {
        eprintln!("[ARTIFACT] Error code: {}", code);
    }
}

#[test]
fn error_code_includes_remediation() {
    // Get any documented error code
    let codes = list_error_codes();
    if codes.is_empty() {
        eprintln!("[ARTIFACT] No error codes documented yet, skipping test");
        return;
    }

    for code in codes.iter().take(5) {
        if let Some(def) = get_error_code(code) {
            // Error definition should have description
            assert!(
                !def.description.is_empty(),
                "Error {} should have description",
                code
            );

            // Log for debugging
            eprintln!("[ARTIFACT] Error {}: {}", code, def.description);
            if !def.causes.is_empty() {
                eprintln!("[ARTIFACT]   Causes: {:?}", def.causes);
            }
            if !def.recovery_steps.is_empty() {
                eprintln!("[ARTIFACT]   Recovery steps: {}", def.recovery_steps.len());
            }
        }
    }
}

#[test]
fn error_category_parsing_works() {
    // Test valid codes
    assert_eq!(
        ErrorCategory::from_code("WA-1001"),
        Some(ErrorCategory::Wezterm)
    );
    assert_eq!(
        ErrorCategory::from_code("WA-2500"),
        Some(ErrorCategory::Storage)
    );
    assert_eq!(
        ErrorCategory::from_code("WA-4001"),
        Some(ErrorCategory::Policy)
    );
    assert_eq!(
        ErrorCategory::from_code("WA-9999"),
        Some(ErrorCategory::Internal)
    );

    // Test invalid codes
    assert!(ErrorCategory::from_code("WA-0001").is_none());
    assert!(ErrorCategory::from_code("WA-8000").is_none());
    assert!(ErrorCategory::from_code("INVALID").is_none());
    assert!(ErrorCategory::from_code("").is_none());
}

// ============================================================================
// Policy decision context tests
// ============================================================================

#[test]
fn policy_denial_templates_cover_common_cases() {
    // These are common denial scenarios that must have explanations
    let denial_scenarios = [
        "deny.alt_screen",
        "deny.command_running",
        "deny.recent_gap",
        "deny.unknown_pane",
    ];

    for scenario in denial_scenarios {
        let template = get_explanation(scenario);
        assert!(
            template.is_some(),
            "Missing explanation for common denial scenario: {}",
            scenario
        );

        if let Some(tmpl) = template {
            // Each denial should explain WHY it's denied
            assert!(
                tmpl.detailed.len() > 50,
                "Denial explanation for {} should be substantial (got {} chars)",
                scenario,
                tmpl.detailed.len()
            );

            // Each denial should have at least one suggestion
            assert!(
                !tmpl.suggestions.is_empty(),
                "Denial {} should have at least one suggestion",
                scenario
            );
        }
    }
}

#[test]
fn workflow_templates_cover_rate_limit_scenarios() {
    // Rate limit handling is critical for multi-agent coordination
    let rate_limit_template = get_explanation("workflow.usage_limit");
    assert!(
        rate_limit_template.is_some(),
        "workflow.usage_limit template must exist"
    );

    if let Some(tmpl) = rate_limit_template {
        // Should mention rate limits or usage
        let detailed_lower = tmpl.detailed.to_lowercase();
        assert!(
            detailed_lower.contains("limit")
                || detailed_lower.contains("quota")
                || detailed_lower.contains("rate")
                || detailed_lower.contains("usage"),
            "Usage limit template should mention limits/quotas"
        );

        eprintln!("[ARTIFACT] Usage limit template brief: {}", tmpl.brief);
    }
}

// ============================================================================
// Template consistency tests
// ============================================================================

#[test]
fn all_templates_follow_naming_convention() {
    for id in list_template_ids() {
        // Must have category.name format
        assert!(
            id.contains('.'),
            "Template ID '{}' should have category.name format",
            id
        );

        let parts: Vec<_> = id.split('.').collect();
        assert_eq!(
            parts.len(),
            2,
            "Template ID '{}' should have exactly one dot",
            id
        );

        // Category must be known
        let valid_categories = ["deny", "workflow", "event"];
        assert!(
            valid_categories.contains(&parts[0]),
            "Template '{}' has unknown category '{}'. Valid: {:?}",
            id,
            parts[0],
            valid_categories
        );
    }
}

#[test]
fn all_templates_have_unique_ids() {
    let ids = list_template_ids();
    let unique_count = ids.iter().collect::<std::collections::HashSet<_>>().len();

    assert_eq!(
        ids.len(),
        unique_count,
        "Found duplicate template IDs. Total: {}, Unique: {}",
        ids.len(),
        unique_count
    );
}

// ============================================================================
// Verbose logging helper for CI artifacts
// ============================================================================

#[test]
fn artifact_dump_all_templates() {
    // This test always passes but dumps all templates for CI artifact capture
    eprintln!("\n========== EXPLAINABILITY ARTIFACT DUMP ==========\n");

    let ids = list_template_ids();
    eprintln!("Total templates: {}\n", ids.len());

    for id in ids {
        if let Some(tmpl) = get_explanation(id) {
            eprintln!("--- {} ---", id);
            eprintln!("Scenario: {}", tmpl.scenario);
            eprintln!("Brief: {}", tmpl.brief);
            eprintln!("Suggestions: {:?}", tmpl.suggestions);
            eprintln!("See also: {:?}", tmpl.see_also);
            eprintln!();
        }
    }

    eprintln!("========== END ARTIFACT DUMP ==========\n");
}
