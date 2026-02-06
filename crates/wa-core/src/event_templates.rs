//! Event template system for human-readable event summaries and descriptions.
//!
//! Templates support:
//! - Variable interpolation: `{key}`
//! - Conditional blocks: `{?key}...{/?key}`
//! - Pluralization: `{count|singular|plural}`

use crate::patterns::{PatternEngine, RuleDef, Severity};
use crate::storage::StoredEvent;
use regex::{Captures, Regex};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::LazyLock;

/// Human-readable template for an event type.
#[derive(Debug, Clone)]
pub struct EventTemplate {
    /// Event type this template matches.
    pub event_type: String,
    /// Short summary (for lists, notifications).
    pub summary: String,
    /// Full description with context.
    pub description: String,
    /// Variables available for interpolation.
    pub context_keys: Vec<ContextKey>,
    /// Actionable suggestions.
    pub suggestions: Vec<Suggestion>,
    /// Severity level.
    pub severity: Severity,
}

impl EventTemplate {
    #[must_use]
    pub fn new(
        event_type: impl Into<String>,
        summary: impl Into<String>,
        description: impl Into<String>,
        severity: Severity,
    ) -> Self {
        Self {
            event_type: event_type.into(),
            summary: summary.into(),
            description: description.into(),
            context_keys: Vec::new(),
            suggestions: Vec::new(),
            severity,
        }
    }

    #[must_use]
    pub fn with_context_keys(mut self, keys: Vec<ContextKey>) -> Self {
        self.context_keys = keys;
        self
    }

    #[must_use]
    pub fn with_suggestions(mut self, suggestions: Vec<Suggestion>) -> Self {
        self.suggestions = suggestions;
        self
    }
}

/// Context metadata exposed to templates.
#[derive(Debug, Clone)]
pub struct ContextKey {
    pub key: String,
    pub description: String,
    pub example: String,
}

impl ContextKey {
    #[must_use]
    pub fn new(
        key: impl Into<String>,
        description: impl Into<String>,
        example: impl Into<String>,
    ) -> Self {
        Self {
            key: key.into(),
            description: description.into(),
            example: example.into(),
        }
    }
}

/// Suggestion rendered alongside an event description.
#[derive(Debug, Clone)]
pub struct Suggestion {
    pub text: String,
    pub command: Option<String>,
    pub doc_link: Option<String>,
}

impl Suggestion {
    #[must_use]
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            command: None,
            doc_link: None,
        }
    }

    #[must_use]
    pub fn with_command(text: impl Into<String>, command: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            command: Some(command.into()),
            doc_link: None,
        }
    }

    #[must_use]
    pub fn with_doc(text: impl Into<String>, doc_link: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            command: None,
            doc_link: Some(doc_link.into()),
        }
    }
}

/// Rendered output for a template.
#[derive(Debug, Clone)]
pub struct RenderedEvent {
    pub summary: String,
    pub description: String,
    pub suggestions: Vec<Suggestion>,
    pub severity: Severity,
}

/// Registry for event templates.
#[derive(Debug, Clone)]
pub struct TemplateRegistry {
    templates: HashMap<String, EventTemplate>,
    fallback: EventTemplate,
}

impl TemplateRegistry {
    #[must_use]
    pub fn new(templates: HashMap<String, EventTemplate>, fallback: EventTemplate) -> Self {
        Self {
            templates,
            fallback,
        }
    }

    #[must_use]
    pub fn get(&self, event_type: &str) -> &EventTemplate {
        self.templates.get(event_type).unwrap_or(&self.fallback)
    }

    #[must_use]
    pub fn has_template(&self, event_type: &str) -> bool {
        self.templates.contains_key(event_type)
    }

    #[must_use]
    pub fn render(&self, event: &StoredEvent) -> RenderedEvent {
        let template = self.get(&event.event_type);
        let context = event_context(event);

        RenderedEvent {
            summary: render_template(&template.summary, &context),
            description: render_template(&template.description, &context),
            suggestions: template
                .suggestions
                .iter()
                .map(|suggestion| render_suggestion(suggestion, &context))
                .collect(),
            severity: template.severity,
        }
    }
}

/// Global registry of built-in event templates.
pub static EVENT_TEMPLATE_REGISTRY: LazyLock<TemplateRegistry> = LazyLock::new(build_registry);

#[must_use]
pub fn get_event_template(event_type: &str) -> &'static EventTemplate {
    EVENT_TEMPLATE_REGISTRY.get(event_type)
}

#[must_use]
pub fn render_event(event: &StoredEvent) -> RenderedEvent {
    EVENT_TEMPLATE_REGISTRY.render(event)
}

fn build_registry() -> TemplateRegistry {
    let engine = PatternEngine::new();
    let mut templates = HashMap::new();

    for rule in engine.rules() {
        templates
            .entry(rule.event_type.clone())
            .or_insert_with(|| template_from_rule(rule));
    }

    // Non-pack events emitted by wa itself (not from pattern packs).
    templates.insert(
        "saved_search.alert".to_string(),
        EventTemplate::new(
            "saved_search.alert",
            "Saved search {search_name}: {match_count} {match_count|match|matches}",
            "Query: {query}\nScope: {scope}{?snippet}\nSnippet: {snippet}{/?snippet}",
            Severity::Info,
        )
        .with_context_keys({
            let mut keys = default_context_keys();
            keys.push(ContextKey::new(
                "search_name",
                "Saved search name",
                "errors",
            ));
            keys.push(ContextKey::new("query", "FTS query", "error OR warning"));
            keys.push(ContextKey::new("scope", "Pane scope", "pane 3"));
            keys.push(ContextKey::new("match_count", "Result count", "5"));
            keys.push(ContextKey::new("snippet", "Snippet preview", "..."));
            keys
        })
        .with_suggestions(vec![Suggestion::with_command(
            "Run saved search",
            "wa search saved run {search_name}".to_string(),
        )]),
    );

    let fallback = EventTemplate::new(
        "unknown",
        "Unknown event {event_type}",
        "An unknown event was detected in pane {pane_id}. Rule: {rule_id}.",
        Severity::Info,
    );

    TemplateRegistry::new(templates, fallback)
}

fn template_from_rule(rule: &RuleDef) -> EventTemplate {
    let mut suggestions = Vec::new();

    if let Some(remediation) = &rule.remediation {
        suggestions.push(Suggestion::text(remediation));
    }

    if let Some(manual_fix) = &rule.manual_fix {
        suggestions.push(Suggestion::text(manual_fix));
    }

    if let Some(command) = &rule.preview_command {
        suggestions.push(Suggestion::with_command(
            "Preview workflow",
            command.clone(),
        ));
    }

    if let Some(url) = &rule.learn_more_url {
        suggestions.push(Suggestion::with_doc("Learn more", url.clone()));
    }

    EventTemplate::new(
        rule.event_type.clone(),
        rule.description.clone(),
        "Detected {event_type} in pane {pane_id} for {agent}. Rule: {rule_id}.".to_string(),
        rule.severity,
    )
    .with_context_keys(default_context_keys())
    .with_suggestions(suggestions)
}

fn default_context_keys() -> Vec<ContextKey> {
    vec![
        ContextKey::new("pane_id", "Pane identifier", "42"),
        ContextKey::new("event_id", "Event id", "123"),
        ContextKey::new("rule_id", "Rule id", "codex.usage.reached"),
        ContextKey::new("event_type", "Event type", "usage.reached"),
        ContextKey::new("agent", "Agent type", "codex"),
        ContextKey::new("severity", "Severity", "warning"),
        ContextKey::new("confidence", "Confidence score", "0.95"),
    ]
}

fn event_context(event: &StoredEvent) -> HashMap<String, String> {
    let mut ctx = HashMap::new();
    ctx.insert("pane_id".to_string(), event.pane_id.to_string());
    ctx.insert("pane".to_string(), event.pane_id.to_string());
    ctx.insert("event_id".to_string(), event.id.to_string());
    ctx.insert("rule_id".to_string(), event.rule_id.clone());
    ctx.insert("event_type".to_string(), event.event_type.clone());
    ctx.insert("agent".to_string(), event.agent_type.clone());
    ctx.insert("severity".to_string(), event.severity.clone());
    ctx.insert("confidence".to_string(), format!("{:.2}", event.confidence));

    if let Some(Value::Object(map)) = &event.extracted {
        for (key, value) in map {
            ctx.entry(key.clone())
                .or_insert_with(|| value_to_string(value));
        }
    }

    ctx
}

fn value_to_string(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Number(num) => num.to_string(),
        Value::Bool(value) => value.to_string(),
        _ => value.to_string(),
    }
}

static PLURAL_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\{([A-Za-z0-9_.-]+)\|([^|}]*)\|([^}]*)\}").expect("plural regex")
});

fn render_suggestion(suggestion: &Suggestion, context: &HashMap<String, String>) -> Suggestion {
    Suggestion {
        text: render_template(&suggestion.text, context),
        command: suggestion
            .command
            .as_ref()
            .map(|command| render_template(command, context)),
        doc_link: suggestion
            .doc_link
            .as_ref()
            .map(|doc| render_template(doc, context)),
    }
}

fn render_template(template: &str, context: &HashMap<String, String>) -> String {
    let with_conditionals = render_conditionals(template, context);
    let with_plurals = render_plurals(&with_conditionals, context);
    render_variables(&with_plurals, context)
}

fn render_conditionals(template: &str, context: &HashMap<String, String>) -> String {
    let mut output = String::new();
    let mut rest = template;

    loop {
        let Some(start) = rest.find("{?") else {
            output.push_str(rest);
            break;
        };

        output.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let Some(end_key) = after.find('}') else {
            output.push_str(&rest[start..]);
            break;
        };

        let key = &after[..end_key];
        let after_key = &after[end_key + 1..];
        let close_tag = format!("{{/?{key}}}");

        let Some(close_pos) = after_key.find(&close_tag) else {
            output.push_str(&rest[start..]);
            break;
        };

        let body = &after_key[..close_pos];
        if is_truthy(context.get(key)) {
            output.push_str(body);
        }

        rest = &after_key[close_pos + close_tag.len()..];
    }

    output
}

fn render_plurals(template: &str, context: &HashMap<String, String>) -> String {
    PLURAL_RE
        .replace_all(template, |caps: &Captures| {
            let key = caps.get(1).map(|m| m.as_str()).unwrap_or_default();
            let singular = caps.get(2).map(|m| m.as_str()).unwrap_or_default();
            let plural = caps.get(3).map(|m| m.as_str()).unwrap_or_default();
            let count = context.get(key).and_then(|value| parse_count(value));
            if count == Some(1) {
                singular.to_string()
            } else {
                plural.to_string()
            }
        })
        .into_owned()
}

fn render_variables(template: &str, context: &HashMap<String, String>) -> String {
    let mut output = template.to_string();
    for (key, value) in context {
        output = output.replace(&format!("{{{key}}}"), value);
    }
    output
}

fn is_truthy(value: Option<&String>) -> bool {
    match value {
        None => false,
        Some(text) => {
            let trimmed = text.trim();
            !(trimmed.is_empty() || trimmed == "0" || trimmed.eq_ignore_ascii_case("false"))
        }
    }
}

fn parse_count(value: &str) -> Option<i64> {
    let cleaned = value.replace(',', "");
    cleaned.trim().parse::<i64>().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_template_replaces_variables() {
        let mut ctx = HashMap::new();
        ctx.insert("name".to_string(), "wa".to_string());
        let rendered = render_template("Hello {name}", &ctx);
        assert_eq!(rendered, "Hello wa");
    }

    #[test]
    fn render_template_conditionals() {
        let mut ctx = HashMap::new();
        let template = "Start {?flag}Enabled{/?flag} End";
        let rendered_missing = render_template(template, &ctx);
        assert_eq!(rendered_missing, "Start  End");
        ctx.insert("flag".to_string(), "yes".to_string());
        let rendered_present = render_template(template, &ctx);
        assert_eq!(rendered_present, "Start Enabled End");
    }

    #[test]
    fn render_template_plurals() {
        let mut ctx = HashMap::new();
        ctx.insert("count".to_string(), "1".to_string());
        let rendered_single = render_template("{count} {count|item|items}", &ctx);
        assert_eq!(rendered_single, "1 item");
        ctx.insert("count".to_string(), "2".to_string());
        let rendered_plural = render_template("{count} {count|item|items}", &ctx);
        assert_eq!(rendered_plural, "2 items");
    }

    #[test]
    fn render_template_missing_variable_preserves_placeholder() {
        let ctx = HashMap::new();
        let rendered = render_template("Value: {missing}", &ctx);
        assert_eq!(rendered, "Value: {missing}");
    }

    #[test]
    fn render_template_conditionals_falsey_values() {
        let template = "Start {?flag}Enabled{/?flag} End";

        for value in ["0", "false", "  ", ""] {
            let mut ctx = HashMap::new();
            ctx.insert("flag".to_string(), value.to_string());
            let rendered = render_template(template, &ctx);
            assert_eq!(rendered, "Start  End");
        }
    }

    #[test]
    fn render_template_plurals_handle_missing_or_comma_counts() {
        let template = "Found {count|item|items}";

        let rendered_missing = render_template(template, &HashMap::new());
        assert_eq!(rendered_missing, "Found items");

        let mut ctx = HashMap::new();
        ctx.insert("count".to_string(), "1,234".to_string());
        let rendered_plural = render_template("{count} {count|item|items}", &ctx);
        assert_eq!(rendered_plural, "1,234 items");
    }

    #[test]
    fn registry_covers_builtin_event_types() {
        let engine = PatternEngine::new();
        let registry = build_registry();
        for rule in engine.rules() {
            assert!(
                registry.has_template(&rule.event_type),
                "Missing template for event type {}",
                rule.event_type
            );
        }
    }

    #[test]
    fn render_event_includes_extracted_fields() {
        let template = EventTemplate::new(
            "usage.warning",
            "Usage {percent}%",
            "Remaining {?reset_time}{reset_time}{/?reset_time}",
            Severity::Warning,
        );
        let mut templates = HashMap::new();
        templates.insert(template.event_type.clone(), template);
        let registry = TemplateRegistry::new(
            templates,
            EventTemplate::new("fallback", "fallback", "fallback", Severity::Info),
        );

        let event = StoredEvent {
            id: 1,
            pane_id: 10,
            rule_id: "codex.usage.warning".to_string(),
            agent_type: "codex".to_string(),
            event_type: "usage.warning".to_string(),
            severity: "warning".to_string(),
            confidence: 0.99,
            extracted: Some(serde_json::json!({"percent": "75", "reset_time": "1h"})),
            matched_text: None,
            segment_id: None,
            detected_at: 0,
            dedupe_key: None,
            handled_at: None,
            handled_by_workflow_id: None,
            handled_status: None,
        };

        let rendered = registry.render(&event);
        assert_eq!(rendered.summary, "Usage 75%");
        assert_eq!(rendered.description, "Remaining 1h");
    }

    #[test]
    fn registry_fallback_used_for_unknown_event() {
        let registry = TemplateRegistry::new(
            HashMap::new(),
            EventTemplate::new(
                "fallback",
                "Unknown event {event_type}",
                "Fallback for {event_type}",
                Severity::Info,
            ),
        );

        let event = StoredEvent {
            id: 7,
            pane_id: 3,
            rule_id: "unknown.rule".to_string(),
            agent_type: "codex".to_string(),
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
        assert!(rendered.summary.contains("unknown.event"));
        assert!(rendered.description.contains("unknown.event"));
    }

    #[test]
    fn render_event_summary_matches_rule_description() {
        let engine = PatternEngine::new();
        let mut by_event: HashMap<String, Vec<&RuleDef>> = HashMap::new();
        for rule in engine.rules() {
            by_event
                .entry(rule.event_type.clone())
                .or_default()
                .push(rule);
        }

        let rule = by_event
            .values()
            .find(|rules| rules.len() == 1)
            .and_then(|rules| rules.first().copied())
            .expect("expected at least one unique event type");

        let event = StoredEvent {
            id: 99,
            pane_id: 12,
            rule_id: rule.id.clone(),
            agent_type: rule.agent_type.to_string(),
            event_type: rule.event_type.clone(),
            severity: "warning".to_string(),
            confidence: 0.9,
            extracted: Some(serde_json::json!({"remaining": "10"})),
            matched_text: None,
            segment_id: None,
            detected_at: 0,
            dedupe_key: None,
            handled_at: None,
            handled_by_workflow_id: None,
            handled_status: None,
        };

        let rendered = render_event(&event);
        assert_eq!(rendered.summary, rule.description);
        assert!(!rendered.summary.contains(&rule.event_type));
    }
}
