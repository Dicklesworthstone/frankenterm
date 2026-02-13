//! Property-based tests for the `explanations` module.
//!
//! Covers `get_explanation` lookup consistency, `list_template_ids` ordering
//! and completeness, `list_templates_by_category` filtering correctness,
//! `render_explanation` idempotence and placeholder substitution, and
//! `format_explanation` structural invariants.

use std::collections::HashMap;

use frankenterm_core::explanations::{
    format_explanation, get_explanation, list_template_ids, list_templates_by_category,
    render_explanation,
};
use proptest::prelude::*;

// =========================================================================
// get_explanation — lookup properties
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    /// All registered template IDs resolve via get_explanation.
    #[test]
    fn prop_all_ids_resolve(_dummy in 0..1_u8) {
        let ids = list_template_ids();
        for id in &ids {
            prop_assert!(
                get_explanation(id).is_some(),
                "template '{}' should be resolvable", id
            );
        }
    }

    /// get_explanation for unknown IDs returns None.
    #[test]
    fn prop_unknown_id_none(id in "[x]{1}[a-z.]{5,20}") {
        // Prefix with 'x' to avoid colliding with real template ids
        prop_assert!(get_explanation(&id).is_none(), "'{}' should not resolve", id);
    }

    /// get_explanation is deterministic.
    #[test]
    fn prop_get_deterministic(_dummy in 0..1_u8) {
        let ids = list_template_ids();
        for id in &ids {
            let a = get_explanation(id);
            let b = get_explanation(id);
            prop_assert!(a.is_some() && b.is_some());
            prop_assert_eq!(a.unwrap().id, b.unwrap().id);
        }
    }

    /// Resolved template ID matches the query ID.
    #[test]
    fn prop_resolved_id_matches(_dummy in 0..1_u8) {
        let ids = list_template_ids();
        for id in &ids {
            let tmpl = get_explanation(id).unwrap();
            prop_assert_eq!(tmpl.id, *id, "template.id should match lookup key");
        }
    }
}

// =========================================================================
// list_template_ids — ordering and completeness
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(10))]

    /// list_template_ids returns a sorted list.
    #[test]
    fn prop_ids_sorted(_dummy in 0..1_u8) {
        let ids = list_template_ids();
        for window in ids.windows(2) {
            prop_assert!(
                window[0] <= window[1],
                "IDs should be sorted: '{}' > '{}'", window[0], window[1]
            );
        }
    }

    /// list_template_ids returns at least 10 templates.
    #[test]
    fn prop_ids_minimum_count(_dummy in 0..1_u8) {
        let ids = list_template_ids();
        prop_assert!(ids.len() >= 10, "should have >= 10 templates, got {}", ids.len());
    }

    /// All IDs follow the category.name convention.
    #[test]
    fn prop_ids_have_dot_convention(_dummy in 0..1_u8) {
        let ids = list_template_ids();
        for id in &ids {
            prop_assert!(
                id.contains('.'),
                "ID '{}' should follow category.name convention", id
            );
        }
    }

    /// No duplicate IDs.
    #[test]
    fn prop_ids_unique(_dummy in 0..1_u8) {
        let ids = list_template_ids();
        let mut seen = std::collections::HashSet::new();
        for id in &ids {
            prop_assert!(seen.insert(id), "duplicate ID: '{}'", id);
        }
    }
}

// =========================================================================
// list_templates_by_category — filtering
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(20))]

    /// Known categories return non-empty results.
    #[test]
    fn prop_known_categories_nonempty(cat in "deny|workflow|event|risk") {
        let templates = list_templates_by_category(&cat);
        prop_assert!(
            !templates.is_empty(),
            "category '{}' should have templates", cat
        );
    }

    /// All returned templates have IDs starting with the category prefix.
    #[test]
    fn prop_category_filter_correct(cat in "deny|workflow|event|risk") {
        let templates = list_templates_by_category(&cat);
        for tmpl in &templates {
            prop_assert!(
                tmpl.id.starts_with(&format!("{}.", cat)),
                "template '{}' should start with '{}.''", tmpl.id, cat
            );
        }
    }

    /// Unknown categories return empty results.
    #[test]
    fn prop_unknown_category_empty(cat in "[x]{1}[a-z]{3,10}") {
        let templates = list_templates_by_category(&cat);
        prop_assert!(templates.is_empty(), "unknown category '{}' should be empty", cat);
    }
}

// =========================================================================
// render_explanation — idempotence and substitution
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    /// render_explanation with empty context returns the detailed text unchanged.
    #[test]
    fn prop_render_empty_context_identity(_dummy in 0..1_u8) {
        let ids = list_template_ids();
        let ctx = HashMap::new();
        for id in &ids {
            let tmpl = get_explanation(id).unwrap();
            let rendered = render_explanation(tmpl, &ctx);
            prop_assert_eq!(
                rendered, tmpl.detailed,
                "empty context should not change template '{}'", id
            );
        }
    }

    /// render_explanation is idempotent for templates without placeholders.
    #[test]
    fn prop_render_idempotent(_dummy in 0..1_u8) {
        let ids = list_template_ids();
        let ctx = HashMap::new();
        for id in &ids {
            let tmpl = get_explanation(id).unwrap();
            let r1 = render_explanation(tmpl, &ctx);
            let r2 = render_explanation(tmpl, &ctx);
            prop_assert_eq!(&r1, &r2);
        }
    }

    /// render_explanation performs placeholder substitution.
    #[test]
    fn prop_render_substitutes_placeholder(key in "[a-z]{3,8}", value in "[a-z]{3,8}") {
        // Use the first template (just testing the substitution mechanism)
        let ids = list_template_ids();
        if let Some(id) = ids.first() {
            let tmpl = get_explanation(id).unwrap();
            let mut ctx = HashMap::new();
            ctx.insert(key.clone(), value.clone());
            let rendered = render_explanation(tmpl, &ctx);
            // If the detailed text contained {key}, it should now contain value
            if tmpl.detailed.contains(&format!("{{{}}}", key)) {
                prop_assert!(
                    rendered.contains(&value),
                    "rendered should contain substituted value '{}'", value
                );
            }
        }
    }
}

// =========================================================================
// format_explanation — structural invariants
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(20))]

    /// format_explanation always contains the scenario heading.
    #[test]
    fn prop_format_contains_scenario(_dummy in 0..1_u8) {
        let ids = list_template_ids();
        for id in &ids {
            let tmpl = get_explanation(id).unwrap();
            let formatted = format_explanation(tmpl, None);
            prop_assert!(
                formatted.contains(tmpl.scenario),
                "formatted output for '{}' should contain scenario", id
            );
        }
    }

    /// format_explanation always contains the brief.
    #[test]
    fn prop_format_contains_brief(_dummy in 0..1_u8) {
        let ids = list_template_ids();
        for id in &ids {
            let tmpl = get_explanation(id).unwrap();
            let formatted = format_explanation(tmpl, None);
            prop_assert!(
                formatted.contains(tmpl.brief),
                "formatted output for '{}' should contain brief", id
            );
        }
    }

    /// format_explanation with None context and Some context both succeed.
    #[test]
    fn prop_format_both_context_modes(_dummy in 0..1_u8) {
        let ids = list_template_ids();
        if let Some(id) = ids.first() {
            let tmpl = get_explanation(id).unwrap();

            let with_none = format_explanation(tmpl, None);
            prop_assert!(!with_none.is_empty());

            let ctx = HashMap::new();
            let with_some = format_explanation(tmpl, Some(&ctx));
            prop_assert!(!with_some.is_empty());
        }
    }

    /// format_explanation contains "Suggestions" section when template has suggestions.
    #[test]
    fn prop_format_includes_suggestions(_dummy in 0..1_u8) {
        let ids = list_template_ids();
        for id in &ids {
            let tmpl = get_explanation(id).unwrap();
            let formatted = format_explanation(tmpl, None);
            if !tmpl.suggestions.is_empty() {
                prop_assert!(
                    formatted.contains("Suggestions"),
                    "formatted '{}' should contain Suggestions section", id
                );
            }
        }
    }

    /// format_explanation contains "See also" when template has see_also.
    #[test]
    fn prop_format_includes_see_also(_dummy in 0..1_u8) {
        let ids = list_template_ids();
        for id in &ids {
            let tmpl = get_explanation(id).unwrap();
            let formatted = format_explanation(tmpl, None);
            if !tmpl.see_also.is_empty() {
                prop_assert!(
                    formatted.contains("See also"),
                    "formatted '{}' should contain See also section", id
                );
            }
        }
    }
}

// =========================================================================
// Template structural properties
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(10))]

    /// All templates have non-empty scenario, brief, and detailed fields.
    #[test]
    fn prop_all_templates_nonempty(_dummy in 0..1_u8) {
        let ids = list_template_ids();
        for id in &ids {
            let tmpl = get_explanation(id).unwrap();
            prop_assert!(!tmpl.scenario.is_empty(), "'{}' scenario empty", id);
            prop_assert!(!tmpl.brief.is_empty(), "'{}' brief empty", id);
            prop_assert!(!tmpl.detailed.is_empty(), "'{}' detailed empty", id);
        }
    }

    /// All template categories are from the known set.
    #[test]
    fn prop_all_categories_known(_dummy in 0..1_u8) {
        let known = ["deny", "workflow", "event", "risk"];
        let ids = list_template_ids();
        for id in &ids {
            let cat = id.split('.').next().unwrap();
            prop_assert!(
                known.contains(&cat),
                "template '{}' has unknown category '{}'", id, cat
            );
        }
    }
}

// =========================================================================
// Unit tests
// =========================================================================

#[test]
fn get_explanation_known_ids() {
    assert!(get_explanation("deny.alt_screen").is_some());
    assert!(get_explanation("workflow.usage_limit").is_some());
    assert!(get_explanation("event.pattern_detected").is_some());
}

#[test]
fn get_explanation_unknown_returns_none() {
    assert!(get_explanation("nonexistent").is_none());
    assert!(get_explanation("").is_none());
}

#[test]
fn render_with_empty_context_unchanged() {
    let tmpl = get_explanation("deny.alt_screen").unwrap();
    let ctx = HashMap::new();
    let rendered = render_explanation(tmpl, &ctx);
    assert_eq!(rendered, tmpl.detailed);
}
