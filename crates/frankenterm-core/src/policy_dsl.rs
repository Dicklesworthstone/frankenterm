//! Composable rule predicate AST for policy DSL.
//!
//! Extends the flat `PolicyRuleMatch` criteria with a recursive boolean
//! predicate tree. This allows expressing complex policy logic:
//!
//! ```text
//! (action=spawn AND actor=robot) OR (action=delete AND title="*critical*")
//! NOT (surface=mcp AND actor=human)
//! ```
//!
//! Part of ft-2h2hp (support for ft-3681t.6.1 policy DSL).

use serde::{Deserialize, Serialize};

use crate::policy::{ActionKind, ActorKind, PolicyInput, PolicySurface};

// =============================================================================
// Predicate AST
// =============================================================================

/// A composable boolean predicate over [`PolicyInput`].
///
/// Predicates form a tree of boolean logic. Atomic predicates test individual
/// fields; composite predicates combine them with AND/OR/NOT.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RulePredicate {
    // ---- Atomic matchers ----
    /// Matches if the input action is in the given set.
    ActionIn { actions: Vec<ActionKind> },

    /// Matches if the input actor is in the given set.
    ActorIn { actors: Vec<ActorKind> },

    /// Matches if the input surface is in the given set.
    SurfaceIn { surfaces: Vec<PolicySurface> },

    /// Matches if the input pane ID is in the given set.
    PaneIdIn { pane_ids: Vec<u64> },

    /// Matches if the pane title matches any of the given glob patterns.
    PaneTitleGlob { patterns: Vec<String> },

    /// Matches if the pane CWD matches any of the given glob patterns.
    PaneCwdGlob { patterns: Vec<String> },

    /// Matches if the pane domain is in the given set (exact, case-insensitive).
    DomainIn { domains: Vec<String> },

    /// Matches if the command text matches any of the given regex patterns.
    CommandRegex { patterns: Vec<String> },

    /// Matches if the inferred agent type is in the given set (case-insensitive).
    AgentTypeIn { agent_types: Vec<String> },

    /// Always matches.
    Always,

    /// Never matches.
    Never,

    // ---- Composite predicates ----
    /// Logical AND: both children must match.
    And {
        left: Box<RulePredicate>,
        right: Box<RulePredicate>,
    },

    /// Logical OR: at least one child must match.
    Or {
        left: Box<RulePredicate>,
        right: Box<RulePredicate>,
    },

    /// Logical NOT: the child must not match.
    Not { inner: Box<RulePredicate> },
}

impl RulePredicate {
    // ---- Constructors ----

    /// Creates an action-in predicate.
    #[must_use]
    pub fn action_in(actions: Vec<ActionKind>) -> Self {
        Self::ActionIn { actions }
    }

    /// Creates an actor-in predicate.
    #[must_use]
    pub fn actor_in(actors: Vec<ActorKind>) -> Self {
        Self::ActorIn { actors }
    }

    /// Creates a surface-in predicate.
    #[must_use]
    pub fn surface_in(surfaces: Vec<PolicySurface>) -> Self {
        Self::SurfaceIn { surfaces }
    }

    /// Creates a pane-id-in predicate.
    #[must_use]
    pub fn pane_id_in(pane_ids: Vec<u64>) -> Self {
        Self::PaneIdIn { pane_ids }
    }

    /// Creates a pane-title-glob predicate.
    #[must_use]
    pub fn pane_title_glob(patterns: Vec<String>) -> Self {
        Self::PaneTitleGlob { patterns }
    }

    /// Creates a pane-cwd-glob predicate.
    #[must_use]
    pub fn pane_cwd_glob(patterns: Vec<String>) -> Self {
        Self::PaneCwdGlob { patterns }
    }

    /// Creates a domain-in predicate.
    #[must_use]
    pub fn domain_in(domains: Vec<String>) -> Self {
        Self::DomainIn { domains }
    }

    /// Creates a command-regex predicate.
    #[must_use]
    pub fn command_regex(patterns: Vec<String>) -> Self {
        Self::CommandRegex { patterns }
    }

    /// Creates an agent-type-in predicate.
    #[must_use]
    pub fn agent_type_in(agent_types: Vec<String>) -> Self {
        Self::AgentTypeIn { agent_types }
    }

    // ---- Combinators ----

    /// AND this predicate with another.
    #[must_use]
    pub fn and(self, other: Self) -> Self {
        Self::And {
            left: Box::new(self),
            right: Box::new(other),
        }
    }

    /// OR this predicate with another.
    #[must_use]
    pub fn or(self, other: Self) -> Self {
        Self::Or {
            left: Box::new(self),
            right: Box::new(other),
        }
    }

    /// Negate this predicate.
    #[must_use]
    pub fn not(self) -> Self {
        Self::Not {
            inner: Box::new(self),
        }
    }

    // ---- Analysis ----

    /// Returns the depth of the predicate tree.
    #[must_use]
    pub fn depth(&self) -> usize {
        match self {
            Self::And { left, right } | Self::Or { left, right } => {
                1 + left.depth().max(right.depth())
            }
            Self::Not { inner } => 1 + inner.depth(),
            _ => 0,
        }
    }

    /// Returns the total number of nodes in the predicate tree.
    #[must_use]
    pub fn node_count(&self) -> usize {
        match self {
            Self::And { left, right } | Self::Or { left, right } => {
                1 + left.node_count() + right.node_count()
            }
            Self::Not { inner } => 1 + inner.node_count(),
            _ => 1,
        }
    }

    /// Returns true if this is a catch-all (always matches).
    #[must_use]
    pub fn is_always(&self) -> bool {
        matches!(self, Self::Always)
    }

    /// Returns true if this is a never-match predicate.
    #[must_use]
    pub fn is_never(&self) -> bool {
        matches!(self, Self::Never)
    }

    /// Returns true if this predicate is atomic (not composite).
    #[must_use]
    pub fn is_atomic(&self) -> bool {
        !matches!(self, Self::And { .. } | Self::Or { .. } | Self::Not { .. })
    }

    /// Returns a specificity score (number of concrete criteria).
    ///
    /// Higher specificity = more specific rule. Pane ID and command regex
    /// contribute +2 each (they are the most specific); other criteria +1.
    #[must_use]
    pub fn specificity(&self) -> u32 {
        match self {
            Self::ActionIn { actions } if !actions.is_empty() => 1,
            Self::ActorIn { actors } if !actors.is_empty() => 1,
            Self::SurfaceIn { surfaces } if !surfaces.is_empty() => 1,
            Self::PaneIdIn { pane_ids } if !pane_ids.is_empty() => 2,
            Self::PaneTitleGlob { patterns } if !patterns.is_empty() => 1,
            Self::PaneCwdGlob { patterns } if !patterns.is_empty() => 1,
            Self::DomainIn { domains } if !domains.is_empty() => 1,
            Self::CommandRegex { patterns } if !patterns.is_empty() => 2,
            Self::AgentTypeIn { agent_types } if !agent_types.is_empty() => 1,
            Self::And { left, right } | Self::Or { left, right } => {
                left.specificity() + right.specificity()
            }
            Self::Not { inner } => inner.specificity(),
            _ => 0,
        }
    }
}

// =============================================================================
// Predicate evaluation
// =============================================================================

/// Evaluates a [`RulePredicate`] against a [`PolicyInput`].
///
/// Returns `true` if the predicate matches the input.
/// Glob patterns use simple `*` and `?` wildcards.
/// Regex patterns match against the full command text.
#[must_use]
pub fn evaluate_predicate(pred: &RulePredicate, input: &PolicyInput) -> bool {
    match pred {
        RulePredicate::ActionIn { actions } => {
            if actions.is_empty() {
                return true;
            }
            actions.contains(&input.action)
        }

        RulePredicate::ActorIn { actors } => {
            if actors.is_empty() {
                return true;
            }
            actors.contains(&input.actor)
        }

        RulePredicate::SurfaceIn { surfaces } => {
            if surfaces.is_empty() {
                return true;
            }
            surfaces.contains(&input.surface)
        }

        RulePredicate::PaneIdIn { pane_ids } => {
            if pane_ids.is_empty() {
                return true;
            }
            match input.pane_id {
                Some(id) => pane_ids.contains(&id),
                None => false,
            }
        }

        RulePredicate::PaneTitleGlob { patterns } => {
            if patterns.is_empty() {
                return true;
            }
            match &input.pane_title {
                Some(title) => patterns.iter().any(|pat| match_glob(pat, title)),
                None => false,
            }
        }

        RulePredicate::PaneCwdGlob { patterns } => {
            if patterns.is_empty() {
                return true;
            }
            match &input.pane_cwd {
                Some(cwd) => patterns.iter().any(|pat| match_glob(pat, cwd)),
                None => false,
            }
        }

        RulePredicate::DomainIn { domains } => {
            if domains.is_empty() {
                return true;
            }
            match &input.domain {
                Some(d) => domains.iter().any(|dom| dom.eq_ignore_ascii_case(d)),
                None => false,
            }
        }

        RulePredicate::CommandRegex { patterns } => {
            if patterns.is_empty() {
                return true;
            }
            match &input.command_text {
                Some(text) => patterns.iter().any(|pat| match_regex(pat, text)),
                None => false,
            }
        }

        RulePredicate::AgentTypeIn { agent_types } => {
            if agent_types.is_empty() {
                return true;
            }
            match &input.agent_type {
                Some(at) => agent_types.iter().any(|t| t.eq_ignore_ascii_case(at)),
                None => false,
            }
        }

        RulePredicate::Always => true,

        RulePredicate::Never => false,

        RulePredicate::And { left, right } => {
            evaluate_predicate(left, input) && evaluate_predicate(right, input)
        }

        RulePredicate::Or { left, right } => {
            evaluate_predicate(left, input) || evaluate_predicate(right, input)
        }

        RulePredicate::Not { inner } => !evaluate_predicate(inner, input),
    }
}

// =============================================================================
// Predicate evaluation trace (for audit/explainability)
// =============================================================================

/// A trace of predicate evaluation for audit/explainability.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PredicateTrace {
    /// Human-readable description of the predicate node.
    pub description: String,
    /// Whether this node matched.
    pub matched: bool,
    /// Child traces (for composite predicates).
    pub children: Vec<PredicateTrace>,
}

/// Evaluates a predicate and produces a trace of the evaluation.
#[must_use]
pub fn evaluate_with_trace(pred: &RulePredicate, input: &PolicyInput) -> PredicateTrace {
    match pred {
        RulePredicate::ActionIn { actions } => {
            let matched = if actions.is_empty() {
                true
            } else {
                actions.contains(&input.action)
            };
            PredicateTrace {
                description: format!("action_in({actions:?})"),
                matched,
                children: vec![],
            }
        }

        RulePredicate::ActorIn { actors } => {
            let matched = if actors.is_empty() {
                true
            } else {
                actors.contains(&input.actor)
            };
            PredicateTrace {
                description: format!("actor_in({actors:?})"),
                matched,
                children: vec![],
            }
        }

        RulePredicate::SurfaceIn { surfaces } => {
            let matched = if surfaces.is_empty() {
                true
            } else {
                surfaces.contains(&input.surface)
            };
            PredicateTrace {
                description: format!("surface_in({surfaces:?})"),
                matched,
                children: vec![],
            }
        }

        RulePredicate::PaneIdIn { pane_ids } => {
            let matched = if pane_ids.is_empty() {
                true
            } else {
                input.pane_id.map_or(false, |id| pane_ids.contains(&id))
            };
            PredicateTrace {
                description: format!("pane_id_in({pane_ids:?})"),
                matched,
                children: vec![],
            }
        }

        RulePredicate::PaneTitleGlob { patterns } => {
            let matched = if patterns.is_empty() {
                true
            } else {
                input
                    .pane_title
                    .as_ref()
                    .map_or(false, |title| patterns.iter().any(|p| match_glob(p, title)))
            };
            PredicateTrace {
                description: format!("pane_title_glob({patterns:?})"),
                matched,
                children: vec![],
            }
        }

        RulePredicate::PaneCwdGlob { patterns } => {
            let matched = if patterns.is_empty() {
                true
            } else {
                input
                    .pane_cwd
                    .as_ref()
                    .map_or(false, |cwd| patterns.iter().any(|p| match_glob(p, cwd)))
            };
            PredicateTrace {
                description: format!("pane_cwd_glob({patterns:?})"),
                matched,
                children: vec![],
            }
        }

        RulePredicate::DomainIn { domains } => {
            let matched = if domains.is_empty() {
                true
            } else {
                input.domain.as_ref().map_or(false, |d| {
                    domains.iter().any(|dom| dom.eq_ignore_ascii_case(d))
                })
            };
            PredicateTrace {
                description: format!("domain_in({domains:?})"),
                matched,
                children: vec![],
            }
        }

        RulePredicate::CommandRegex { patterns } => {
            let matched = if patterns.is_empty() {
                true
            } else {
                input
                    .command_text
                    .as_ref()
                    .map_or(false, |text| patterns.iter().any(|p| match_regex(p, text)))
            };
            PredicateTrace {
                description: format!("command_regex({patterns:?})"),
                matched,
                children: vec![],
            }
        }

        RulePredicate::AgentTypeIn { agent_types } => {
            let matched = if agent_types.is_empty() {
                true
            } else {
                input.agent_type.as_ref().map_or(false, |at| {
                    agent_types.iter().any(|t| t.eq_ignore_ascii_case(at))
                })
            };
            PredicateTrace {
                description: format!("agent_type_in({agent_types:?})"),
                matched,
                children: vec![],
            }
        }

        RulePredicate::Always => PredicateTrace {
            description: "always".to_owned(),
            matched: true,
            children: vec![],
        },

        RulePredicate::Never => PredicateTrace {
            description: "never".to_owned(),
            matched: false,
            children: vec![],
        },

        RulePredicate::And { left, right } => {
            let left_trace = evaluate_with_trace(left, input);
            let right_trace = evaluate_with_trace(right, input);
            let matched = left_trace.matched && right_trace.matched;
            PredicateTrace {
                description: "and".to_owned(),
                matched,
                children: vec![left_trace, right_trace],
            }
        }

        RulePredicate::Or { left, right } => {
            let left_trace = evaluate_with_trace(left, input);
            let right_trace = evaluate_with_trace(right, input);
            let matched = left_trace.matched || right_trace.matched;
            PredicateTrace {
                description: "or".to_owned(),
                matched,
                children: vec![left_trace, right_trace],
            }
        }

        RulePredicate::Not { inner } => {
            let inner_trace = evaluate_with_trace(inner, input);
            let matched = !inner_trace.matched;
            PredicateTrace {
                description: "not".to_owned(),
                matched,
                children: vec![inner_trace],
            }
        }
    }
}

// =============================================================================
// DSL rule: predicate + decision
// =============================================================================

/// A policy rule using the predicate DSL.
///
/// This is the DSL-based replacement for `PolicyRule` + `PolicyRuleMatch`.
/// The `predicate` field uses a composable boolean tree instead of flat criteria.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DslRule {
    /// Unique identifier for this rule.
    pub id: String,

    /// Human-readable description.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,

    /// Priority (lower = higher priority, default 100).
    #[serde(default = "default_priority")]
    pub priority: u32,

    /// Composable predicate tree.
    pub predicate: RulePredicate,

    /// Decision when this rule matches.
    pub decision: DslDecision,

    /// Message template (may contain `{action}`, `{actor}`, `{surface}`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

fn default_priority() -> u32 {
    100
}

/// Decision for a DSL rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DslDecision {
    Allow,
    Deny,
    RequireApproval,
}

impl DslDecision {
    /// Severity ordering for tie-breaking (lower = stricter).
    #[must_use]
    pub const fn severity(&self) -> u32 {
        match self {
            Self::Deny => 0,
            Self::RequireApproval => 1,
            Self::Allow => 2,
        }
    }
}

// =============================================================================
// DSL rule evaluation
// =============================================================================

/// Result of evaluating a set of DSL rules.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DslEvalResult {
    /// The winning rule (if any matched).
    pub matched_rule: Option<DslRuleMatch>,
    /// All rules that were evaluated with their match status.
    pub evaluations: Vec<DslRuleEvaluation>,
}

/// A matched DSL rule.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DslRuleMatch {
    /// Rule ID.
    pub rule_id: String,
    /// Decision.
    pub decision: DslDecision,
    /// Resolved message (after template interpolation).
    pub message: Option<String>,
    /// Predicate trace for explainability.
    pub trace: PredicateTrace,
}

/// Evaluation record for a single DSL rule.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DslRuleEvaluation {
    /// Rule ID.
    pub rule_id: String,
    /// Whether the predicate matched.
    pub matched: bool,
    /// The decision (always present, even if not matched).
    pub decision: DslDecision,
    /// Priority.
    pub priority: u32,
}

/// Evaluates a set of DSL rules against a [`PolicyInput`].
///
/// Rules are evaluated in order. The winning rule is selected by:
/// 1. Priority (lower wins)
/// 2. Decision severity (Deny > RequireApproval > Allow)
/// 3. Specificity (higher wins)
#[must_use]
pub fn evaluate_dsl_rules(rules: &[DslRule], input: &PolicyInput) -> DslEvalResult {
    let mut evaluations = Vec::with_capacity(rules.len());
    let mut best_match: Option<(usize, &DslRule, PredicateTrace)> = None;

    for (idx, rule) in rules.iter().enumerate() {
        let trace = evaluate_with_trace(&rule.predicate, input);
        let matched = trace.matched;

        evaluations.push(DslRuleEvaluation {
            rule_id: rule.id.clone(),
            matched,
            decision: rule.decision,
            priority: rule.priority,
        });

        if matched {
            let dominated = match &best_match {
                None => false,
                Some((_, best, _)) => {
                    if rule.priority != best.priority {
                        rule.priority >= best.priority
                    } else if rule.decision.severity() != best.decision.severity() {
                        rule.decision.severity() >= best.decision.severity()
                    } else {
                        rule.predicate.specificity() <= best.predicate.specificity()
                    }
                }
            };

            if !dominated || best_match.is_none() {
                best_match = Some((idx, rule, trace));
            }
        }
    }

    let matched_rule = best_match.map(|(_, rule, trace)| {
        let message = rule
            .message
            .as_ref()
            .map(|tmpl| interpolate_message(tmpl, input));
        DslRuleMatch {
            rule_id: rule.id.clone(),
            decision: rule.decision,
            message,
            trace,
        }
    });

    DslEvalResult {
        matched_rule,
        evaluations,
    }
}

// =============================================================================
// Telemetry
// =============================================================================

/// Telemetry counters for DSL rule evaluation.
#[derive(Debug, Default)]
pub struct DslTelemetry {
    /// Total evaluations performed.
    pub evaluations_total: u64,
    /// Total rules matched.
    pub rules_matched: u64,
    /// Total rules not matched.
    pub rules_not_matched: u64,
    /// Total deny decisions.
    pub deny_decisions: u64,
    /// Total allow decisions.
    pub allow_decisions: u64,
    /// Total require-approval decisions.
    pub require_approval_decisions: u64,
}

/// Snapshot of DSL telemetry (serializable).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DslTelemetrySnapshot {
    pub evaluations_total: u64,
    pub rules_matched: u64,
    pub rules_not_matched: u64,
    pub deny_decisions: u64,
    pub allow_decisions: u64,
    pub require_approval_decisions: u64,
}

impl DslTelemetry {
    /// Records the result of a DSL evaluation.
    pub fn record(&mut self, result: &DslEvalResult) {
        self.evaluations_total += 1;
        for eval in &result.evaluations {
            if eval.matched {
                self.rules_matched += 1;
            } else {
                self.rules_not_matched += 1;
            }
        }
        if let Some(m) = &result.matched_rule {
            match m.decision {
                DslDecision::Deny => self.deny_decisions += 1,
                DslDecision::Allow => self.allow_decisions += 1,
                DslDecision::RequireApproval => self.require_approval_decisions += 1,
            }
        }
    }

    /// Returns a serializable snapshot.
    #[must_use]
    pub fn snapshot(&self) -> DslTelemetrySnapshot {
        DslTelemetrySnapshot {
            evaluations_total: self.evaluations_total,
            rules_matched: self.rules_matched,
            rules_not_matched: self.rules_not_matched,
            deny_decisions: self.deny_decisions,
            allow_decisions: self.allow_decisions,
            require_approval_decisions: self.require_approval_decisions,
        }
    }
}

// =============================================================================
// Internal helpers
// =============================================================================

/// Simple glob pattern matching (`*` matches any sequence, `?` matches one char).
fn match_glob(pattern: &str, text: &str) -> bool {
    let pat_bytes = pattern.as_bytes();
    let text_bytes = text.as_bytes();
    let mut pi = 0;
    let mut ti = 0;
    let mut star_pi = usize::MAX;
    let mut star_ti = 0;

    while ti < text_bytes.len() {
        if pi < pat_bytes.len() && (pat_bytes[pi] == b'?' || pat_bytes[pi] == text_bytes[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < pat_bytes.len() && pat_bytes[pi] == b'*' {
            star_pi = pi;
            star_ti = ti;
            pi += 1;
        } else if star_pi != usize::MAX {
            pi = star_pi + 1;
            star_ti += 1;
            ti = star_ti;
        } else {
            return false;
        }
    }

    while pi < pat_bytes.len() && pat_bytes[pi] == b'*' {
        pi += 1;
    }

    pi == pat_bytes.len()
}

/// Simple regex matching (wraps `regex::Regex`).
fn match_regex(pattern: &str, text: &str) -> bool {
    regex::Regex::new(pattern)
        .map(|re| re.is_match(text))
        .unwrap_or(false)
}

/// Interpolates `{action}`, `{actor}`, `{surface}`, `{pane_id}` in a message template.
fn interpolate_message(template: &str, input: &PolicyInput) -> String {
    template
        .replace("{action}", &format!("{:?}", input.action))
        .replace("{actor}", &format!("{:?}", input.actor))
        .replace("{surface}", &format!("{:?}", input.surface))
        .replace(
            "{pane_id}",
            &input
                .pane_id
                .map_or_else(|| "none".to_owned(), |id| id.to_string()),
        )
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::PaneCapabilities;

    fn make_input(action: ActionKind, actor: ActorKind) -> PolicyInput {
        PolicyInput::new(action, actor)
    }

    fn make_input_with_surface(
        action: ActionKind,
        actor: ActorKind,
        surface: PolicySurface,
    ) -> PolicyInput {
        let mut input = PolicyInput::new(action, actor);
        input.surface = surface;
        input
    }

    fn make_input_full(
        action: ActionKind,
        actor: ActorKind,
        pane_id: Option<u64>,
        title: Option<&str>,
        cwd: Option<&str>,
        domain: Option<&str>,
        command: Option<&str>,
        agent_type: Option<&str>,
    ) -> PolicyInput {
        PolicyInput {
            action,
            actor,
            surface: PolicySurface::Mux,
            pane_id,
            domain: domain.map(|s| s.to_owned()),
            capabilities: PaneCapabilities::default(),
            text_summary: None,
            workflow_id: None,
            command_text: command.map(|s| s.to_owned()),
            trauma_decision: None,
            pane_title: title.map(|s| s.to_owned()),
            pane_cwd: cwd.map(|s| s.to_owned()),
            agent_type: agent_type.map(|s| s.to_owned()),
        }
    }

    // ---- Atomic predicate tests ----

    #[test]
    fn action_in_matches() {
        let pred = RulePredicate::action_in(vec![ActionKind::Spawn, ActionKind::Close]);
        let input = make_input(ActionKind::Spawn, ActorKind::Robot);
        assert!(evaluate_predicate(&pred, &input));
    }

    #[test]
    fn action_in_no_match() {
        let pred = RulePredicate::action_in(vec![ActionKind::Spawn]);
        let input = make_input(ActionKind::SendText, ActorKind::Robot);
        assert!(!evaluate_predicate(&pred, &input));
    }

    #[test]
    fn action_in_empty_matches_all() {
        let pred = RulePredicate::action_in(vec![]);
        let input = make_input(ActionKind::SendText, ActorKind::Robot);
        assert!(evaluate_predicate(&pred, &input));
    }

    #[test]
    fn actor_in_matches() {
        let pred = RulePredicate::actor_in(vec![ActorKind::Robot]);
        let input = make_input(ActionKind::Spawn, ActorKind::Robot);
        assert!(evaluate_predicate(&pred, &input));
    }

    #[test]
    fn actor_in_no_match() {
        let pred = RulePredicate::actor_in(vec![ActorKind::Human]);
        let input = make_input(ActionKind::Spawn, ActorKind::Robot);
        assert!(!evaluate_predicate(&pred, &input));
    }

    #[test]
    fn surface_in_matches() {
        let pred = RulePredicate::surface_in(vec![PolicySurface::Mux]);
        let input =
            make_input_with_surface(ActionKind::Spawn, ActorKind::Robot, PolicySurface::Mux);
        assert!(evaluate_predicate(&pred, &input));
    }

    #[test]
    fn pane_id_in_matches() {
        let pred = RulePredicate::pane_id_in(vec![42, 99]);
        let input = make_input_full(
            ActionKind::Spawn,
            ActorKind::Robot,
            Some(42),
            None,
            None,
            None,
            None,
            None,
        );
        assert!(evaluate_predicate(&pred, &input));
    }

    #[test]
    fn pane_id_in_no_pane_id() {
        let pred = RulePredicate::pane_id_in(vec![42]);
        let input = make_input(ActionKind::Spawn, ActorKind::Robot);
        assert!(!evaluate_predicate(&pred, &input));
    }

    #[test]
    fn pane_title_glob_matches() {
        let pred = RulePredicate::pane_title_glob(vec!["*critical*".to_owned()]);
        let input = make_input_full(
            ActionKind::Spawn,
            ActorKind::Robot,
            None,
            Some("my-critical-pane"),
            None,
            None,
            None,
            None,
        );
        assert!(evaluate_predicate(&pred, &input));
    }

    #[test]
    fn pane_title_glob_no_title() {
        let pred = RulePredicate::pane_title_glob(vec!["*critical*".to_owned()]);
        let input = make_input(ActionKind::Spawn, ActorKind::Robot);
        assert!(!evaluate_predicate(&pred, &input));
    }

    #[test]
    fn pane_cwd_glob_matches() {
        let pred = RulePredicate::pane_cwd_glob(vec!["/home/*/projects/*".to_owned()]);
        let input = make_input_full(
            ActionKind::Spawn,
            ActorKind::Robot,
            None,
            None,
            Some("/home/user/projects/app"),
            None,
            None,
            None,
        );
        assert!(evaluate_predicate(&pred, &input));
    }

    #[test]
    fn domain_in_case_insensitive() {
        let pred = RulePredicate::domain_in(vec!["SSH-PROD".to_owned()]);
        let input = make_input_full(
            ActionKind::Spawn,
            ActorKind::Robot,
            None,
            None,
            None,
            Some("ssh-prod"),
            None,
            None,
        );
        assert!(evaluate_predicate(&pred, &input));
    }

    #[test]
    fn command_regex_matches() {
        let pred = RulePredicate::command_regex(vec!["^rm\\s+-rf".to_owned()]);
        let input = make_input_full(
            ActionKind::SendText,
            ActorKind::Robot,
            None,
            None,
            None,
            None,
            Some("rm -rf /tmp/data"),
            None,
        );
        assert!(evaluate_predicate(&pred, &input));
    }

    #[test]
    fn command_regex_bad_pattern_returns_false() {
        let pred = RulePredicate::command_regex(vec!["[invalid".to_owned()]);
        let input = make_input_full(
            ActionKind::SendText,
            ActorKind::Robot,
            None,
            None,
            None,
            None,
            Some("anything"),
            None,
        );
        assert!(!evaluate_predicate(&pred, &input));
    }

    #[test]
    fn agent_type_in_case_insensitive() {
        let pred = RulePredicate::agent_type_in(vec!["Claude".to_owned()]);
        let input = make_input_full(
            ActionKind::Spawn,
            ActorKind::Robot,
            None,
            None,
            None,
            None,
            None,
            Some("claude"),
        );
        assert!(evaluate_predicate(&pred, &input));
    }

    #[test]
    fn always_matches() {
        let pred = RulePredicate::Always;
        let input = make_input(ActionKind::Spawn, ActorKind::Robot);
        assert!(evaluate_predicate(&pred, &input));
    }

    #[test]
    fn never_does_not_match() {
        let pred = RulePredicate::Never;
        let input = make_input(ActionKind::Spawn, ActorKind::Robot);
        assert!(!evaluate_predicate(&pred, &input));
    }

    // ---- Composite predicate tests ----

    #[test]
    fn and_both_true() {
        let pred = RulePredicate::action_in(vec![ActionKind::Spawn])
            .and(RulePredicate::actor_in(vec![ActorKind::Robot]));
        let input = make_input(ActionKind::Spawn, ActorKind::Robot);
        assert!(evaluate_predicate(&pred, &input));
    }

    #[test]
    fn and_one_false() {
        let pred = RulePredicate::action_in(vec![ActionKind::Spawn])
            .and(RulePredicate::actor_in(vec![ActorKind::Human]));
        let input = make_input(ActionKind::Spawn, ActorKind::Robot);
        assert!(!evaluate_predicate(&pred, &input));
    }

    #[test]
    fn or_one_true() {
        let pred = RulePredicate::action_in(vec![ActionKind::Spawn])
            .or(RulePredicate::action_in(vec![ActionKind::Close]));
        let input = make_input(ActionKind::Close, ActorKind::Robot);
        assert!(evaluate_predicate(&pred, &input));
    }

    #[test]
    fn or_both_false() {
        let pred = RulePredicate::action_in(vec![ActionKind::Spawn])
            .or(RulePredicate::action_in(vec![ActionKind::Close]));
        let input = make_input(ActionKind::SendText, ActorKind::Robot);
        assert!(!evaluate_predicate(&pred, &input));
    }

    #[test]
    fn not_inverts() {
        let pred = RulePredicate::actor_in(vec![ActorKind::Robot]).not();
        let input = make_input(ActionKind::Spawn, ActorKind::Human);
        assert!(evaluate_predicate(&pred, &input));
    }

    #[test]
    fn complex_nested() {
        // (action=spawn AND actor=robot) OR (action=delete AND NOT surface=mcp)
        let pred = RulePredicate::action_in(vec![ActionKind::Spawn])
            .and(RulePredicate::actor_in(vec![ActorKind::Robot]))
            .or(RulePredicate::action_in(vec![ActionKind::DeleteFile])
                .and(RulePredicate::surface_in(vec![PolicySurface::Mcp]).not()));

        // Should match: spawn + robot
        let input1 = make_input(ActionKind::Spawn, ActorKind::Robot);
        assert!(evaluate_predicate(&pred, &input1));

        // Should match: delete + non-mcp
        let input2 =
            make_input_with_surface(ActionKind::DeleteFile, ActorKind::Human, PolicySurface::Mux);
        assert!(evaluate_predicate(&pred, &input2));

        // Should not match: delete + mcp
        let input3 =
            make_input_with_surface(ActionKind::DeleteFile, ActorKind::Human, PolicySurface::Mcp);
        assert!(!evaluate_predicate(&pred, &input3));

        // Should not match: send_text + robot
        let input4 = make_input(ActionKind::SendText, ActorKind::Robot);
        assert!(!evaluate_predicate(&pred, &input4));
    }

    // ---- Trace tests ----

    #[test]
    fn trace_records_match_status() {
        let pred = RulePredicate::action_in(vec![ActionKind::Spawn])
            .and(RulePredicate::actor_in(vec![ActorKind::Robot]));
        let input = make_input(ActionKind::Spawn, ActorKind::Robot);
        let trace = evaluate_with_trace(&pred, &input);
        assert!(trace.matched);
        assert_eq!(trace.description, "and");
        assert_eq!(trace.children.len(), 2);
        assert!(trace.children[0].matched);
        assert!(trace.children[1].matched);
    }

    #[test]
    fn trace_records_not_match() {
        let pred = RulePredicate::actor_in(vec![ActorKind::Human]);
        let input = make_input(ActionKind::Spawn, ActorKind::Robot);
        let trace = evaluate_with_trace(&pred, &input);
        assert!(!trace.matched);
    }

    // ---- DSL rule evaluation tests ----

    #[test]
    fn dsl_eval_no_rules() {
        let input = make_input(ActionKind::Spawn, ActorKind::Robot);
        let result = evaluate_dsl_rules(&[], &input);
        assert!(result.matched_rule.is_none());
        assert!(result.evaluations.is_empty());
    }

    #[test]
    fn dsl_eval_single_match() {
        let rules = vec![DslRule {
            id: "deny-robot-spawn".to_owned(),
            description: Some("No robot spawns".to_owned()),
            priority: 10,
            predicate: RulePredicate::action_in(vec![ActionKind::Spawn])
                .and(RulePredicate::actor_in(vec![ActorKind::Robot])),
            decision: DslDecision::Deny,
            message: Some("Robots cannot spawn".to_owned()),
        }];

        let input = make_input(ActionKind::Spawn, ActorKind::Robot);
        let result = evaluate_dsl_rules(&rules, &input);
        assert!(result.matched_rule.is_some());
        let m = result.matched_rule.unwrap();
        assert_eq!(m.rule_id, "deny-robot-spawn");
        assert_eq!(m.decision, DslDecision::Deny);
    }

    #[test]
    fn dsl_eval_priority_wins() {
        let rules = vec![
            DslRule {
                id: "allow-all".to_owned(),
                description: None,
                priority: 50,
                predicate: RulePredicate::Always,
                decision: DslDecision::Allow,
                message: None,
            },
            DslRule {
                id: "deny-spawn".to_owned(),
                description: None,
                priority: 10,
                predicate: RulePredicate::action_in(vec![ActionKind::Spawn]),
                decision: DslDecision::Deny,
                message: None,
            },
        ];

        let input = make_input(ActionKind::Spawn, ActorKind::Robot);
        let result = evaluate_dsl_rules(&rules, &input);
        let m = result.matched_rule.unwrap();
        assert_eq!(m.rule_id, "deny-spawn");
    }

    #[test]
    fn dsl_eval_severity_tiebreaks() {
        let rules = vec![
            DslRule {
                id: "allow-spawn".to_owned(),
                description: None,
                priority: 10,
                predicate: RulePredicate::action_in(vec![ActionKind::Spawn]),
                decision: DslDecision::Allow,
                message: None,
            },
            DslRule {
                id: "deny-spawn".to_owned(),
                description: None,
                priority: 10,
                predicate: RulePredicate::action_in(vec![ActionKind::Spawn]),
                decision: DslDecision::Deny,
                message: None,
            },
        ];

        let input = make_input(ActionKind::Spawn, ActorKind::Robot);
        let result = evaluate_dsl_rules(&rules, &input);
        let m = result.matched_rule.unwrap();
        assert_eq!(m.rule_id, "deny-spawn"); // Deny has lower severity number
    }

    #[test]
    fn dsl_eval_message_interpolation() {
        let rules = vec![DslRule {
            id: "deny-msg".to_owned(),
            description: None,
            priority: 10,
            predicate: RulePredicate::Always,
            decision: DslDecision::Deny,
            message: Some("Denied {action} by {actor}".to_owned()),
        }];

        let input = make_input(ActionKind::Spawn, ActorKind::Robot);
        let result = evaluate_dsl_rules(&rules, &input);
        let m = result.matched_rule.unwrap();
        assert!(m.message.as_ref().unwrap().contains("Spawn"));
        assert!(m.message.as_ref().unwrap().contains("Robot"));
    }

    // ---- Analysis tests ----

    #[test]
    fn depth_of_atomic() {
        assert_eq!(RulePredicate::Always.depth(), 0);
        assert_eq!(RulePredicate::action_in(vec![ActionKind::Spawn]).depth(), 0);
    }

    #[test]
    fn depth_of_composite() {
        let pred = RulePredicate::Always.and(RulePredicate::Never);
        assert_eq!(pred.depth(), 1);
    }

    #[test]
    fn depth_nested() {
        let pred = RulePredicate::Always.and(RulePredicate::Never.or(RulePredicate::Always));
        assert_eq!(pred.depth(), 2);
    }

    #[test]
    fn node_count() {
        let pred = RulePredicate::Always
            .and(RulePredicate::Never)
            .or(RulePredicate::Always);
        assert_eq!(pred.node_count(), 5); // or + and + Always + Never + Always
    }

    #[test]
    fn specificity_atomic() {
        assert_eq!(RulePredicate::Always.specificity(), 0);
        assert_eq!(
            RulePredicate::action_in(vec![ActionKind::Spawn]).specificity(),
            1
        );
        assert_eq!(RulePredicate::pane_id_in(vec![42]).specificity(), 2);
        assert_eq!(
            RulePredicate::command_regex(vec![".*".to_owned()]).specificity(),
            2
        );
    }

    #[test]
    fn specificity_composite() {
        let pred = RulePredicate::action_in(vec![ActionKind::Spawn])
            .and(RulePredicate::pane_id_in(vec![42]));
        assert_eq!(pred.specificity(), 3); // 1 + 2
    }

    #[test]
    fn is_atomic() {
        assert!(RulePredicate::Always.is_atomic());
        assert!(!RulePredicate::Always.and(RulePredicate::Never).is_atomic());
        assert!(!RulePredicate::Always.not().is_atomic());
    }

    // ---- Glob tests ----

    #[test]
    fn glob_star() {
        assert!(match_glob("*", "anything"));
        assert!(match_glob("hello*", "hello world"));
        assert!(match_glob("*world", "hello world"));
        assert!(match_glob("*llo*wor*", "hello world"));
    }

    #[test]
    fn glob_question() {
        assert!(match_glob("h?llo", "hello"));
        assert!(!match_glob("h?llo", "hllo"));
    }

    #[test]
    fn glob_exact() {
        assert!(match_glob("hello", "hello"));
        assert!(!match_glob("hello", "world"));
    }

    #[test]
    fn glob_empty() {
        assert!(match_glob("", ""));
        assert!(!match_glob("", "a"));
        assert!(match_glob("*", ""));
    }

    // ---- Telemetry tests ----

    #[test]
    fn telemetry_records() {
        let mut telem = DslTelemetry::default();
        let result = DslEvalResult {
            matched_rule: Some(DslRuleMatch {
                rule_id: "test".to_owned(),
                decision: DslDecision::Deny,
                message: None,
                trace: PredicateTrace {
                    description: "always".to_owned(),
                    matched: true,
                    children: vec![],
                },
            }),
            evaluations: vec![DslRuleEvaluation {
                rule_id: "test".to_owned(),
                matched: true,
                decision: DslDecision::Deny,
                priority: 10,
            }],
        };
        telem.record(&result);
        assert_eq!(telem.evaluations_total, 1);
        assert_eq!(telem.rules_matched, 1);
        assert_eq!(telem.deny_decisions, 1);
    }

    #[test]
    fn telemetry_snapshot_roundtrip() {
        let snap = DslTelemetrySnapshot {
            evaluations_total: 10,
            rules_matched: 5,
            rules_not_matched: 5,
            deny_decisions: 2,
            allow_decisions: 2,
            require_approval_decisions: 1,
        };
        let json = serde_json::to_string(&snap).unwrap();
        let back: DslTelemetrySnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(snap, back);
    }

    // ---- Serde roundtrip tests ----

    #[test]
    fn predicate_serde_roundtrip_atomic() {
        let pred = RulePredicate::action_in(vec![ActionKind::Spawn, ActionKind::Close]);
        let json = serde_json::to_string(&pred).unwrap();
        let back: RulePredicate = serde_json::from_str(&json).unwrap();
        assert_eq!(pred, back);
    }

    #[test]
    fn predicate_serde_roundtrip_composite() {
        let pred = RulePredicate::action_in(vec![ActionKind::Spawn])
            .and(RulePredicate::actor_in(vec![ActorKind::Robot]))
            .or(RulePredicate::surface_in(vec![PolicySurface::Connector]).not());
        let json = serde_json::to_string(&pred).unwrap();
        let back: RulePredicate = serde_json::from_str(&json).unwrap();
        assert_eq!(pred, back);
    }

    #[test]
    fn dsl_rule_serde_roundtrip() {
        let rule = DslRule {
            id: "test-rule".to_owned(),
            description: Some("A test rule".to_owned()),
            priority: 42,
            predicate: RulePredicate::action_in(vec![ActionKind::Spawn])
                .and(RulePredicate::actor_in(vec![ActorKind::Robot])),
            decision: DslDecision::Deny,
            message: Some("Blocked".to_owned()),
        };
        let json = serde_json::to_string(&rule).unwrap();
        let back: DslRule = serde_json::from_str(&json).unwrap();
        assert_eq!(rule, back);
    }

    #[test]
    fn dsl_eval_result_serde_roundtrip() {
        let result = DslEvalResult {
            matched_rule: Some(DslRuleMatch {
                rule_id: "test".to_owned(),
                decision: DslDecision::Allow,
                message: Some("ok".to_owned()),
                trace: PredicateTrace {
                    description: "always".to_owned(),
                    matched: true,
                    children: vec![],
                },
            }),
            evaluations: vec![DslRuleEvaluation {
                rule_id: "test".to_owned(),
                matched: true,
                decision: DslDecision::Allow,
                priority: 100,
            }],
        };
        let json = serde_json::to_string(&result).unwrap();
        let back: DslEvalResult = serde_json::from_str(&json).unwrap();
        assert_eq!(result, back);
    }
}
