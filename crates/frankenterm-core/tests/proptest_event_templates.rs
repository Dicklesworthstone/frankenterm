//! Property-based tests for the `event_templates` module.
//!
//! Covers `TemplateRegistry` rendering invariants (fallback, known templates,
//! extracted field interpolation), `EventTemplate` builder composition,
//! `RenderedEvent` structural properties, and `StoredEvent` context extraction.

use std::collections::HashMap;

use frankenterm_core::event_templates::{ContextKey, EventTemplate, Suggestion, TemplateRegistry};
use frankenterm_core::patterns::Severity;
use frankenterm_core::storage::StoredEvent;
use proptest::prelude::*;

// =========================================================================
// Strategies
// =========================================================================

fn arb_severity() -> impl Strategy<Value = Severity> {
    prop_oneof![
        Just(Severity::Info),
        Just(Severity::Warning),
        Just(Severity::Critical),
    ]
}

fn arb_stored_event() -> impl Strategy<Value = StoredEvent> {
    (
        1_i64..10_000,           // id
        1_u64..1000,             // pane_id
        "[a-z.]{3,15}",          // rule_id
        "[a-z_]{3,10}",          // agent_type
        "[a-z.]{3,15}",          // event_type
        "info|warning|critical", // severity
        0.0_f64..1.0,            // confidence
        proptest::option::of(proptest::collection::hash_map(
            "[a-z_]{3,10}",
            "[a-z0-9 ]{1,20}",
            0..5,
        )), // extracted
        0_i64..100_000_000,      // detected_at
    )
        .prop_map(
            |(
                id,
                pane_id,
                rule_id,
                agent_type,
                event_type,
                severity,
                confidence,
                extracted,
                detected_at,
            )| {
                let extracted_json = extracted.map(|map| {
                    let obj: serde_json::Map<String, serde_json::Value> = map
                        .into_iter()
                        .map(|(k, v)| (k, serde_json::Value::String(v)))
                        .collect();
                    serde_json::Value::Object(obj)
                });

                StoredEvent {
                    id,
                    pane_id,
                    rule_id,
                    agent_type,
                    event_type,
                    severity,
                    confidence,
                    extracted: extracted_json,
                    matched_text: None,
                    segment_id: None,
                    detected_at,
                    dedupe_key: None,
                    handled_at: None,
                    handled_by_workflow_id: None,
                    handled_status: None,
                }
            },
        )
}

fn make_fallback_template() -> EventTemplate {
    EventTemplate::new(
        "fallback",
        "Unknown event {event_type}",
        "Fallback: {event_type} in pane {pane_id}",
        Severity::Info,
    )
}

// =========================================================================
// TemplateRegistry — fallback behavior
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(80))]

    /// Unknown event types always use the fallback template.
    #[test]
    fn prop_unknown_event_uses_fallback(event in arb_stored_event()) {
        let registry = TemplateRegistry::new(HashMap::new(), make_fallback_template());
        let rendered = registry.render(&event);
        // Fallback summary template is "Unknown event {event_type}"
        prop_assert!(
            rendered.summary.contains(&event.event_type)
                || rendered.summary.contains("Unknown"),
            "fallback summary should mention event_type or 'Unknown'"
        );
    }

    /// Registered templates take precedence over fallback.
    #[test]
    fn prop_registered_template_used(
        event in arb_stored_event(),
        custom_summary in "[A-Za-z ]{3,20}",
    ) {
        let template = EventTemplate::new(
            event.event_type.clone(),
            custom_summary.clone(),
            "custom description",
            Severity::Warning,
        );
        let mut templates = HashMap::new();
        templates.insert(event.event_type.clone(), template);
        let registry = TemplateRegistry::new(templates, make_fallback_template());

        let rendered = registry.render(&event);
        // If summary has no placeholders, it should match exactly
        if !custom_summary.contains('{') {
            prop_assert_eq!(&rendered.summary, &custom_summary);
        }
    }

    /// has_template returns true for registered types and false for unknown.
    #[test]
    fn prop_has_template_consistent(
        event_type in "[a-z.]{3,15}",
        registered in any::<bool>(),
    ) {
        let mut templates = HashMap::new();
        if registered {
            templates.insert(
                event_type.clone(),
                EventTemplate::new(&event_type, "s", "d", Severity::Info),
            );
        }
        let registry = TemplateRegistry::new(templates, make_fallback_template());
        prop_assert_eq!(registry.has_template(&event_type), registered);
    }
}

// =========================================================================
// TemplateRegistry::render — variable interpolation
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(60))]

    /// Standard context fields (pane_id, event_type, agent) are interpolated.
    #[test]
    fn prop_standard_fields_interpolated(event in arb_stored_event()) {
        let template = EventTemplate::new(
            event.event_type.clone(),
            "Agent {agent} in pane {pane_id}",
            "Event {event_type} rule {rule_id}",
            Severity::Info,
        );
        let mut templates = HashMap::new();
        templates.insert(event.event_type.clone(), template);
        let registry = TemplateRegistry::new(templates, make_fallback_template());

        let rendered = registry.render(&event);
        prop_assert!(
            rendered.summary.contains(&event.agent_type),
            "summary '{}' should contain agent '{}'", rendered.summary, event.agent_type
        );
        prop_assert!(
            rendered.summary.contains(&event.pane_id.to_string()),
            "summary '{}' should contain pane_id '{}'", rendered.summary, event.pane_id
        );
        prop_assert!(
            rendered.description.contains(&event.event_type),
            "desc '{}' should contain event_type '{}'", rendered.description, event.event_type
        );
        prop_assert!(
            rendered.description.contains(&event.rule_id),
            "desc '{}' should contain rule_id '{}'", rendered.description, event.rule_id
        );
    }

    /// Extracted fields from StoredEvent are available for interpolation.
    #[test]
    fn prop_extracted_fields_available(
        pane_id in 1_u64..100,
        key in "[a-z]{3,8}",
        value in "[a-z0-9]{3,15}",
    ) {
        let event = StoredEvent {
            id: 1,
            pane_id,
            rule_id: "test.rule".to_string(),
            agent_type: "test".to_string(),
            event_type: "test.event".to_string(),
            severity: "info".to_string(),
            confidence: 0.9,
            extracted: Some(serde_json::json!({key.clone(): value.clone()})),
            matched_text: None,
            segment_id: None,
            detected_at: 0,
            dedupe_key: None,
            handled_at: None,
            handled_by_workflow_id: None,
            handled_status: None,
        };
        let template = EventTemplate::new(
            "test.event",
            format!("Val: {{{key}}}"),
            "d",
            Severity::Info,
        );
        let mut templates = HashMap::new();
        templates.insert("test.event".to_string(), template);
        let registry = TemplateRegistry::new(templates, make_fallback_template());

        let rendered = registry.render(&event);
        prop_assert!(
            rendered.summary.contains(&value),
            "summary '{}' should contain extracted value '{}'", rendered.summary, value
        );
    }
}

// =========================================================================
// TemplateRegistry::render — severity preserved
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    /// Rendered event severity always matches the template's severity.
    #[test]
    fn prop_rendered_severity_matches_template(
        event in arb_stored_event(),
        sev in arb_severity(),
    ) {
        let template = EventTemplate::new(
            event.event_type.clone(),
            "summary",
            "desc",
            sev,
        );
        let mut templates = HashMap::new();
        templates.insert(event.event_type.clone(), template);
        let registry = TemplateRegistry::new(templates, make_fallback_template());

        let rendered = registry.render(&event);
        prop_assert_eq!(rendered.severity, sev);
    }
}

// =========================================================================
// EventTemplate builder properties
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    /// EventTemplate::new sets all fields correctly.
    #[test]
    fn prop_template_new_fields(
        event_type in "[a-z.]{3,15}",
        summary in "[A-Za-z ]{3,20}",
        description in "[A-Za-z ]{3,30}",
        sev in arb_severity(),
    ) {
        let t = EventTemplate::new(&event_type, &summary, &description, sev);
        prop_assert_eq!(&t.event_type, &event_type);
        prop_assert_eq!(&t.summary, &summary);
        prop_assert_eq!(&t.description, &description);
        prop_assert_eq!(t.severity, sev);
        prop_assert!(t.context_keys.is_empty());
        prop_assert!(t.suggestions.is_empty());
    }

    /// with_context_keys replaces (not appends) context keys.
    #[test]
    fn prop_with_context_keys_replaces(
        n_keys in 0_usize..5,
    ) {
        let keys: Vec<ContextKey> = (0..n_keys)
            .map(|i| ContextKey::new(format!("k{i}"), format!("desc{i}"), format!("ex{i}")))
            .collect();
        let t = EventTemplate::new("e", "s", "d", Severity::Info)
            .with_context_keys(keys.clone());
        prop_assert_eq!(t.context_keys.len(), n_keys);
        for (i, key) in t.context_keys.iter().enumerate() {
            prop_assert_eq!(&key.key, &format!("k{i}"));
        }
    }

    /// with_suggestions replaces (not appends) suggestions.
    #[test]
    fn prop_with_suggestions_replaces(
        n_sugs in 0_usize..5,
    ) {
        let sugs: Vec<Suggestion> = (0..n_sugs)
            .map(|i| Suggestion::text(format!("suggestion {i}")))
            .collect();
        let t = EventTemplate::new("e", "s", "d", Severity::Info)
            .with_suggestions(sugs.clone());
        prop_assert_eq!(t.suggestions.len(), n_sugs);
    }
}

// =========================================================================
// Suggestion constructors
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    /// Suggestion::text only has text field set.
    #[test]
    fn prop_suggestion_text_only(text in "[A-Za-z ]{3,20}") {
        let s = Suggestion::text(&text);
        prop_assert_eq!(&s.text, &text);
        prop_assert!(s.command.is_none());
        prop_assert!(s.doc_link.is_none());
    }

    /// Suggestion::with_command has text and command set.
    #[test]
    fn prop_suggestion_with_command(
        text in "[A-Za-z ]{3,20}",
        cmd in "[a-z ]{3,20}",
    ) {
        let s = Suggestion::with_command(&text, &cmd);
        prop_assert_eq!(&s.text, &text);
        prop_assert_eq!(s.command.as_deref(), Some(cmd.as_str()));
        prop_assert!(s.doc_link.is_none());
    }

    /// Suggestion::with_doc has text and doc_link set.
    #[test]
    fn prop_suggestion_with_doc(
        text in "[A-Za-z ]{3,20}",
        doc in "[a-z./:]{5,30}",
    ) {
        let s = Suggestion::with_doc(&text, &doc);
        prop_assert_eq!(&s.text, &text);
        prop_assert!(s.command.is_none());
        prop_assert_eq!(s.doc_link.as_deref(), Some(doc.as_str()));
    }
}

// =========================================================================
// Rendering determinism
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(40))]

    /// Rendering the same event twice produces identical output.
    #[test]
    fn prop_render_deterministic(event in arb_stored_event()) {
        let registry = TemplateRegistry::new(HashMap::new(), make_fallback_template());
        let r1 = registry.render(&event);
        let r2 = registry.render(&event);
        prop_assert_eq!(&r1.summary, &r2.summary);
        prop_assert_eq!(&r1.description, &r2.description);
        prop_assert_eq!(r1.severity, r2.severity);
        prop_assert_eq!(r1.suggestions.len(), r2.suggestions.len());
    }
}

// =========================================================================
// Unit tests
// =========================================================================

#[test]
fn context_key_fields() {
    let k = ContextKey::new("test_key", "A description", "example_val");
    assert_eq!(k.key, "test_key");
    assert_eq!(k.description, "A description");
    assert_eq!(k.example, "example_val");
}

#[test]
fn empty_registry_always_falls_back() {
    let registry = TemplateRegistry::new(HashMap::new(), make_fallback_template());
    assert!(!registry.has_template("anything"));
    let t = registry.get("anything");
    assert_eq!(t.event_type, "fallback");
}

#[test]
fn rendered_event_from_fallback_has_info_severity() {
    let registry = TemplateRegistry::new(HashMap::new(), make_fallback_template());
    let event = StoredEvent {
        id: 1,
        pane_id: 1,
        rule_id: "r".to_string(),
        agent_type: "a".to_string(),
        event_type: "unknown.event".to_string(),
        severity: "info".to_string(),
        confidence: 0.5,
        extracted: None,
        matched_text: None,
        segment_id: None,
        detected_at: 0,
        dedupe_key: None,
        handled_at: None,
        handled_by_workflow_id: None,
        handled_status: None,
    };
    let rendered = registry.render(&event);
    assert_eq!(rendered.severity, Severity::Info);
}

// =========================================================================
// EventTemplate — Clone, Debug
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn prop_template_clone_preserves(
        event_type in "[a-z.]{3,15}",
        summary in "[A-Za-z ]{3,20}",
        description in "[A-Za-z ]{3,30}",
        sev in arb_severity(),
    ) {
        let t = EventTemplate::new(&event_type, &summary, &description, sev);
        let cloned = t.clone();
        prop_assert_eq!(&cloned.event_type, &t.event_type);
        prop_assert_eq!(&cloned.summary, &t.summary);
        prop_assert_eq!(&cloned.description, &t.description);
        prop_assert_eq!(cloned.severity, t.severity);
        prop_assert_eq!(cloned.context_keys.len(), t.context_keys.len());
        prop_assert_eq!(cloned.suggestions.len(), t.suggestions.len());
    }

    #[test]
    fn prop_template_debug_non_empty(
        event_type in "[a-z.]{3,15}",
        sev in arb_severity(),
    ) {
        let t = EventTemplate::new(&event_type, "s", "d", sev);
        let debug = format!("{:?}", t);
        prop_assert!(!debug.is_empty());
    }
}

// =========================================================================
// ContextKey — Clone, Debug, fields
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn prop_context_key_clone(
        key in "[a-z_]{3,10}",
        desc in "[A-Za-z ]{3,20}",
        example in "[a-z0-9]{3,10}",
    ) {
        let k = ContextKey::new(&key, &desc, &example);
        let cloned = k.clone();
        prop_assert_eq!(&cloned.key, &k.key);
        prop_assert_eq!(&cloned.description, &k.description);
        prop_assert_eq!(&cloned.example, &k.example);
    }

    #[test]
    fn prop_context_key_debug(
        key in "[a-z_]{3,10}",
    ) {
        let k = ContextKey::new(&key, "desc", "example");
        let debug = format!("{:?}", k);
        prop_assert!(!debug.is_empty());
    }
}

// =========================================================================
// Suggestion — Clone, Debug
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn prop_suggestion_clone(text in "[A-Za-z ]{3,20}") {
        let s = Suggestion::text(&text);
        let cloned = s.clone();
        prop_assert_eq!(&cloned.text, &s.text);
        prop_assert_eq!(&cloned.command, &s.command);
        prop_assert_eq!(&cloned.doc_link, &s.doc_link);
    }

    #[test]
    fn prop_suggestion_debug(text in "[A-Za-z ]{3,20}") {
        let s = Suggestion::text(&text);
        let debug = format!("{:?}", s);
        prop_assert!(!debug.is_empty());
    }

    #[test]
    fn prop_suggestion_with_both(
        text in "[A-Za-z ]{3,20}",
        cmd in "[a-z ]{3,20}",
        doc in "[a-z./:]{5,30}",
    ) {
        let s1 = Suggestion::with_command(&text, &cmd);
        let s2 = Suggestion::with_doc(&text, &doc);
        // with_command sets command, not doc_link
        prop_assert!(s1.command.is_some());
        prop_assert!(s1.doc_link.is_none());
        // with_doc sets doc_link, not command
        prop_assert!(s2.command.is_none());
        prop_assert!(s2.doc_link.is_some());
    }
}

// =========================================================================
// RenderedEvent — Clone, Debug, non-empty fields
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn prop_rendered_clone(event in arb_stored_event()) {
        let registry = TemplateRegistry::new(HashMap::new(), make_fallback_template());
        let rendered = registry.render(&event);
        let cloned = rendered.clone();
        prop_assert_eq!(&cloned.summary, &rendered.summary);
        prop_assert_eq!(&cloned.description, &rendered.description);
        prop_assert_eq!(cloned.severity, rendered.severity);
        prop_assert_eq!(cloned.suggestions.len(), rendered.suggestions.len());
    }

    #[test]
    fn prop_rendered_debug(event in arb_stored_event()) {
        let registry = TemplateRegistry::new(HashMap::new(), make_fallback_template());
        let rendered = registry.render(&event);
        let debug = format!("{:?}", rendered);
        prop_assert!(!debug.is_empty());
    }

    #[test]
    fn prop_rendered_summary_non_empty(event in arb_stored_event()) {
        let registry = TemplateRegistry::new(HashMap::new(), make_fallback_template());
        let rendered = registry.render(&event);
        prop_assert!(!rendered.summary.is_empty(), "summary should be non-empty");
        prop_assert!(!rendered.description.is_empty(), "description should be non-empty");
    }
}

// =========================================================================
// TemplateRegistry — Clone, Debug, multiple templates
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn prop_registry_clone(
        event_types in prop::collection::vec("[a-z.]{3,15}", 1..5),
    ) {
        let mut templates = HashMap::new();
        for et in &event_types {
            templates.insert(
                et.clone(),
                EventTemplate::new(et, "summary", "desc", Severity::Info),
            );
        }
        let registry = TemplateRegistry::new(templates, make_fallback_template());
        let cloned = registry.clone();
        for et in &event_types {
            prop_assert_eq!(registry.has_template(et), cloned.has_template(et));
        }
    }

    #[test]
    fn prop_registry_debug(
        n in 0_usize..3,
    ) {
        let mut templates = HashMap::new();
        for i in 0..n {
            let et = format!("type.{}", i);
            templates.insert(
                et.clone(),
                EventTemplate::new(&et, "s", "d", Severity::Info),
            );
        }
        let registry = TemplateRegistry::new(templates, make_fallback_template());
        let debug = format!("{:?}", registry);
        prop_assert!(!debug.is_empty());
    }

    #[test]
    fn prop_registry_get_returns_correct_template(
        event_type in "[a-z.]{3,15}",
        summary in "[A-Za-z]{3,15}",
    ) {
        let template = EventTemplate::new(&event_type, &summary, "desc", Severity::Warning);
        let mut templates = HashMap::new();
        templates.insert(event_type.clone(), template);
        let registry = TemplateRegistry::new(templates, make_fallback_template());
        let got = registry.get(&event_type);
        prop_assert_eq!(&got.event_type, &event_type);
        prop_assert_eq!(&got.summary, &summary);
    }

    #[test]
    fn prop_registry_get_unknown_returns_fallback(
        unknown_type in "[a-z]{10,20}",
    ) {
        let registry = TemplateRegistry::new(HashMap::new(), make_fallback_template());
        let got = registry.get(&unknown_type);
        prop_assert_eq!(&got.event_type, "fallback");
    }
}

// =========================================================================
// Severity — serde roundtrip
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn prop_severity_serde_roundtrip(sev in arb_severity()) {
        let json = serde_json::to_string(&sev).unwrap();
        let back: Severity = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(sev, back);
    }

    #[test]
    fn prop_severity_debug(sev in arb_severity()) {
        let debug = format!("{:?}", sev);
        prop_assert!(!debug.is_empty());
    }
}

// =========================================================================
// Global registry
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(16))]

    #[test]
    fn prop_global_registry_exists(_dummy in 0..1u8) {
        // get_event_template should not panic for unknown types
        let t = frankenterm_core::event_templates::get_event_template("nonexistent.type");
        prop_assert!(!t.event_type.is_empty());
    }

    #[test]
    fn prop_render_event_global(event in arb_stored_event()) {
        let rendered = frankenterm_core::event_templates::render_event(&event);
        prop_assert!(!rendered.summary.is_empty());
    }
}
