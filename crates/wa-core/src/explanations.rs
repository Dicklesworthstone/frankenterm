//! Explanation templates: reusable reason patterns for `wa why` and errors.
//!
//! This module provides consistent, helpful explanations for common scenarios
//! through a template system. Templates include brief descriptions, detailed
//! explanations, suggestions for resolution, and cross-references.
//!
//! # Usage
//!
//! ```rust,ignore
//! use wa_core::explanations::{get_explanation, render_explanation};
//! use std::collections::HashMap;
//!
//! if let Some(template) = get_explanation("deny.alt_screen") {
//!     println!("{}", template.brief);
//!
//!     // With context interpolation
//!     let mut ctx = HashMap::new();
//!     ctx.insert("pane_id".to_string(), "42".to_string());
//!     let rendered = render_explanation(template, &ctx);
//! }
//! ```

use serde::Serialize;
use std::collections::HashMap;
use std::sync::LazyLock;

/// A reusable explanation template for common scenarios.
///
/// Templates provide structured information for user-facing messages,
/// including context, suggestions, and cross-references.
/// Note: This type cannot derive Deserialize due to static string references.
/// Templates are defined statically at compile time.
#[derive(Debug, Clone, Serialize)]
pub struct ExplanationTemplate {
    /// Unique identifier (e.g., "deny.alt_screen", "workflow.usage_limit")
    pub id: &'static str,
    /// Brief scenario description (shown in headers)
    pub scenario: &'static str,
    /// One-line summary for compact output
    pub brief: &'static str,
    /// Multi-line detailed explanation
    pub detailed: &'static str,
    /// Actionable suggestions for resolution
    pub suggestions: &'static [&'static str],
    /// Related commands or documentation
    pub see_also: &'static [&'static str],
}

// ============================================================================
// Policy Denial Templates
// ============================================================================

/// Explanation for alt-screen blocking.
pub static DENY_ALT_SCREEN: ExplanationTemplate = ExplanationTemplate {
    id: "deny.alt_screen",
    scenario: "Send denied because alt-screen is active",
    brief: "Pane is in full-screen mode (vim, less, etc.)",
    detailed: r"The pane is currently displaying an alternate screen buffer, which typically
means a full-screen application like vim, less, htop, or similar is running.

Sending text while alt-screen is active could:
- Corrupt the application state
- Cause unintended keystrokes
- Interfere with user interaction

The safety policy blocks sends to alt-screen panes by default.",
    suggestions: &[
        "Exit the full-screen application first",
        "Use --force if you're certain this is safe",
        "Configure policy to allow specific alt-screen apps",
    ],
    see_also: &["wa policy", "wa status --pane <id>"],
};

/// Explanation for command-running blocking.
pub static DENY_COMMAND_RUNNING: ExplanationTemplate = ExplanationTemplate {
    id: "deny.command_running",
    scenario: "Send denied because a command is running",
    brief: "Another command is currently executing in the pane",
    detailed: r"The pane has an active command running (detected via OSC 133 markers or
heuristics). Sending text while a command runs could:

- Interrupt the running command
- Queue input for later (confusing)
- Cause the shell to misinterpret input

wa waits for command completion before sending unless overridden.",
    suggestions: &[
        "Wait for the current command to finish",
        "Use Ctrl-C to cancel the running command first",
        "Use --wait-for to send after a specific pattern",
    ],
    see_also: &["wa status", "wa send --wait-for"],
};

/// Explanation for recent gap blocking.
pub static DENY_RECENT_GAP: ExplanationTemplate = ExplanationTemplate {
    id: "deny.recent_gap",
    scenario: "Send denied due to recent output gap",
    brief: "Pane had no output recently, possibly waiting for input",
    detailed: r"wa detected a gap in pane output that suggests the pane might be:
- Waiting for user input at a prompt
- Displaying a confirmation dialog
- In an unknown state

The policy requires a prompt marker (OSC 133) or manual confirmation.",
    suggestions: &[
        "Check the pane manually to see its state",
        "Use --force if you've verified the pane is ready",
        "Enable OSC 133 support in your shell for better detection",
    ],
    see_also: &["wa capabilities --pane <id>"],
};

/// Explanation for rate limit blocking.
pub static DENY_RATE_LIMITED: ExplanationTemplate = ExplanationTemplate {
    id: "deny.rate_limited",
    scenario: "Send denied due to rate limiting",
    brief: "Too many actions in a short period",
    detailed: r"The rate limiter has blocked this action to prevent overwhelming the
target pane or external services. Rate limits protect against:

- Accidental infinite loops
- Runaway automation
- API abuse

Current rate limits are configured in wa.toml under [safety.rate_limits].",
    suggestions: &[
        "Wait a moment and retry",
        "Check rate limit configuration in wa.toml",
        "Use --dry-run to test without hitting limits",
    ],
    see_also: &["wa config show", "wa policy"],
};

/// Explanation for unknown pane blocking.
pub static DENY_UNKNOWN_PANE: ExplanationTemplate = ExplanationTemplate {
    id: "deny.unknown_pane",
    scenario: "Action denied for unknown pane",
    brief: "Pane ID not found in active pane list",
    detailed: r"The specified pane ID does not exist in the current WezTerm session.
This could mean:

- The pane was closed
- The pane ID was mistyped
- WezTerm session changed

wa tracks panes discovered via 'wezterm cli list'.",
    suggestions: &[
        "Run 'wa robot state' to see active panes",
        "Run 'wezterm cli list' to verify pane exists",
        "Check if pane was recently closed",
    ],
    see_also: &["wa robot state", "wa status"],
};

/// Explanation for insufficient permissions.
pub static DENY_PERMISSION: ExplanationTemplate = ExplanationTemplate {
    id: "deny.permission",
    scenario: "Action denied due to insufficient permissions",
    brief: "Required capability not granted",
    detailed: r"This action requires a capability that is not enabled in the current
policy configuration. Capabilities gate potentially dangerous operations:

- send_text: Sending keystrokes to panes
- execute: Running shell commands
- workflow: Triggering automated workflows

Configure capabilities in wa.toml under [safety.capabilities].",
    suggestions: &[
        "Review required capabilities for this action",
        "Update policy configuration to grant capability",
        "Use --dry-run to see what would happen",
    ],
    see_also: &["wa policy", "wa config show"],
};

// ============================================================================
// Workflow Explanation Templates
// ============================================================================

/// Explanation for usage limit workflow trigger.
pub static WORKFLOW_USAGE_LIMIT: ExplanationTemplate = ExplanationTemplate {
    id: "workflow.usage_limit",
    scenario: "Why handle_usage_limits workflow was triggered",
    brief: "Codex hit its daily token usage limit",
    detailed: r"The Codex agent reported it has reached its usage limit. This typically
happens when:

- Daily token quota exceeded
- Account-level rate limiting triggered

The handle_usage_limits workflow will:
1. Gracefully exit the current Codex session
2. Parse the session summary for resume ID
3. Select an alternate OpenAI account
4. Complete device auth flow
5. Resume the session with new credentials",
    suggestions: &[
        "Let the workflow complete automatically",
        "Check account status with: caut status",
        "Configure account pool in wa.toml",
    ],
    see_also: &["wa workflow status", "caut"],
};

/// Explanation for compaction workflow trigger.
pub static WORKFLOW_COMPACTION: ExplanationTemplate = ExplanationTemplate {
    id: "workflow.compaction",
    scenario: "Why handle_compaction workflow was triggered",
    brief: "Agent detected context compaction event",
    detailed: r#"The AI agent indicated it is compacting or summarizing its context
window. This typically happens when:

- Claude Code emits "Compacting conversation..."
- Codex emits context management messages
- Context length approaches model limits

The handle_compaction workflow can:
1. Log the compaction event for analysis
2. Notify other agents of reduced context
3. Optionally checkpoint current state"#,
    suggestions: &[
        "Review captured output before compaction",
        "Consider shorter task batches to avoid compaction",
        "Use 'wa search' to find pre-compaction context",
    ],
    see_also: &["wa workflow status", "wa search"],
};

/// Explanation for error workflow trigger.
pub static WORKFLOW_ERROR_DETECTED: ExplanationTemplate = ExplanationTemplate {
    id: "workflow.error_detected",
    scenario: "Why error recovery workflow was triggered",
    brief: "Agent encountered an error condition",
    detailed: r"A pattern matched indicating an error condition in the agent:

- Compilation errors
- Runtime exceptions
- API failures
- Authentication issues

The error recovery workflow can:
1. Capture error context for debugging
2. Attempt automatic recovery
3. Notify operators of persistent failures",
    suggestions: &[
        "Check 'wa robot events' for error details",
        "Review agent output with 'wa get-text <pane>'",
        "Verify external service connectivity",
    ],
    see_also: &["wa robot events", "wa get-text"],
};

/// Explanation for approval workflow trigger.
pub static WORKFLOW_APPROVAL_NEEDED: ExplanationTemplate = ExplanationTemplate {
    id: "workflow.approval_needed",
    scenario: "Why approval workflow was triggered",
    brief: "Agent is waiting for user approval",
    detailed: r"The AI agent is requesting approval before proceeding. Common reasons:

- Destructive operation (file deletion, force push)
- External API call with side effects
- Cost-incurring operation
- First action in new environment

wa's approval system allows:
1. Interactive approval via prompt
2. One-time allow tokens
3. Policy-based auto-approval",
    suggestions: &[
        "Review the requested action carefully",
        "Use 'wa approve <token>' to grant one-time approval",
        "Configure auto-approval policies for trusted operations",
    ],
    see_also: &["wa approve", "wa policy"],
};

// ============================================================================
// Event Explanation Templates
// ============================================================================

/// Explanation for pattern detection event.
pub static EVENT_PATTERN_DETECTED: ExplanationTemplate = ExplanationTemplate {
    id: "event.pattern_detected",
    scenario: "Pattern match triggered detection event",
    brief: "Configured pattern matched in pane output",
    detailed: r"The pattern detection engine found a match in pane output. Events are
generated when patterns from enabled packs match terminal content:

- core pack: Codex, Claude, Gemini state transitions
- custom pack: User-defined patterns in patterns.toml

Events are stored in the database and can trigger workflows.",
    suggestions: &[
        "Use 'wa robot events' to see recent detections",
        "Configure pattern packs in wa.toml",
        "Add custom patterns in ~/.config/wa/patterns.toml",
    ],
    see_also: &["wa robot events", "wa config show"],
};

/// Explanation for gap detection event.
pub static EVENT_GAP_DETECTED: ExplanationTemplate = ExplanationTemplate {
    id: "event.gap_detected",
    scenario: "Output gap detected during capture",
    brief: "Discontinuity in captured terminal output",
    detailed: r"wa detected a gap in the capture stream, meaning some output may have
been missed. Gaps occur when:

- Poll interval too slow for output rate
- System under heavy load
- WezTerm scrollback overwritten

Gap markers in storage indicate where discontinuities exist.",
    suggestions: &[
        "Reduce poll_interval_ms in wa.toml for fast-output panes",
        "Check system load during gaps",
        "Increase WezTerm scrollback buffer",
    ],
    see_also: &["wa config show", "wa status"],
};

// ============================================================================
// Risk Explanation Templates (wa-upg.6.3)
// ============================================================================

/// Explanation for elevated risk score.
pub static RISK_ELEVATED: ExplanationTemplate = ExplanationTemplate {
    id: "risk.elevated",
    scenario: "Action has elevated risk score requiring approval",
    brief: "Risk factors combined to exceed the automatic allow threshold",
    detailed: r"wa's risk scoring model evaluated multiple factors and determined this action
has elevated risk (score 51-70). Risk factors can include:

- State factors: alt-screen mode, running commands, capture gaps
- Action factors: mutating operations, control characters, destructive actions
- Context factors: untrusted actor, broadcast targets
- Content factors: destructive tokens (rm -rf, DROP, etc.), sudo elevation

The combined score exceeds the configured threshold for automatic allow.",
    suggestions: &[
        "Review the contributing factors in the risk breakdown",
        "Use 'wa approve <token>' to grant one-time approval",
        "Configure risk thresholds in wa.toml under [policy.risk]",
        "Add weight overrides for trusted operations",
    ],
    see_also: &["wa policy", "wa config show"],
};

/// Explanation for high risk score denial.
pub static RISK_HIGH: ExplanationTemplate = ExplanationTemplate {
    id: "risk.high",
    scenario: "Action denied due to high risk score",
    brief: "Risk factors combined to exceed the denial threshold",
    detailed: r"wa's risk scoring model evaluated multiple factors and determined this action
has high risk (score 71-100), triggering automatic denial.

High-risk scenarios typically involve multiple concerning factors:
- Sending to alternate screen + destructive command content
- Unknown pane state + sudo elevation
- Reserved pane + untrusted actor

These combinations represent genuinely dangerous operations that require
explicit override or configuration changes.",
    suggestions: &[
        "Review the contributing factors carefully",
        "Wait for pane to reach a safer state",
        "Configure hard overrides in wa.toml if this is a false positive",
        "Consider using a workflow with explicit approval gates",
    ],
    see_also: &["wa policy", "wa config show"],
};

/// Explanation for alt-screen risk factor.
pub static RISK_FACTOR_ALT_SCREEN: ExplanationTemplate = ExplanationTemplate {
    id: "risk.factor.alt_screen",
    scenario: "Alt-screen state contributes to risk",
    brief: "Pane is in alternate screen mode (vim, less, etc.)",
    detailed: r"The target pane is running a full-screen application that uses the alternate
screen buffer. Sending text input to such applications can cause unpredictable
behavior or data corruption.

This factor adds significant weight (default: 60) to the risk score because:
- Applications like vim interpret text as commands
- Screen-based apps don't have a shell prompt
- Text could trigger unintended operations",
    suggestions: &[
        "Wait for the application to exit",
        "Use the application's native input method",
        "Check 'wa status --pane <id>' for alt-screen state",
    ],
    see_also: &["wa status", "wa policy"],
};

/// Explanation for destructive content risk factor.
pub static RISK_FACTOR_DESTRUCTIVE: ExplanationTemplate = ExplanationTemplate {
    id: "risk.factor.destructive_tokens",
    scenario: "Destructive patterns detected in command",
    brief: "Command contains patterns commonly associated with destructive operations",
    detailed: r"The command text contains patterns that are commonly destructive:
- 'rm -rf' or 'rm -r' (recursive file deletion)
- 'DROP TABLE' or 'DROP DATABASE' (SQL destruction)
- 'git reset --hard' or 'git clean -fd' (git history/file loss)
- 'mkfs' or 'dd if=' (disk operations)

This factor adds weight (default: 40) to encourage human review of
potentially irreversible operations.",
    suggestions: &[
        "Review the command carefully before approving",
        "Consider using --dry-run if available",
        "Use explicit file paths instead of wildcards",
    ],
    see_also: &["wa policy"],
};

/// Explanation for sudo elevation risk factor.
pub static RISK_FACTOR_SUDO: ExplanationTemplate = ExplanationTemplate {
    id: "risk.factor.sudo_elevation",
    scenario: "Sudo/doas elevation detected in command",
    brief: "Command uses privilege elevation",
    detailed: r"The command includes sudo, doas, or similar privilege elevation. Elevated
commands have broader system access and can cause more damage if misused.

This factor adds moderate weight (default: 30) because:
- Root access bypasses normal permission checks
- Mistakes with elevated privileges are harder to undo
- Automation should be cautious about privileged operations",
    suggestions: &[
        "Verify the command needs elevated privileges",
        "Consider reducing weight in trusted environments",
        "Review sudo configuration for the target pane",
    ],
    see_also: &["wa policy"],
};

// ============================================================================
// Template Registry
// ============================================================================

/// Global registry of all explanation templates.
pub static EXPLANATION_TEMPLATES: LazyLock<HashMap<&'static str, &'static ExplanationTemplate>> =
    LazyLock::new(|| {
        let mut m = HashMap::new();

        // Policy denials
        m.insert(DENY_ALT_SCREEN.id, &DENY_ALT_SCREEN);
        m.insert(DENY_COMMAND_RUNNING.id, &DENY_COMMAND_RUNNING);
        m.insert(DENY_RECENT_GAP.id, &DENY_RECENT_GAP);
        m.insert(DENY_RATE_LIMITED.id, &DENY_RATE_LIMITED);
        m.insert(DENY_UNKNOWN_PANE.id, &DENY_UNKNOWN_PANE);
        m.insert(DENY_PERMISSION.id, &DENY_PERMISSION);

        // Workflows
        m.insert(WORKFLOW_USAGE_LIMIT.id, &WORKFLOW_USAGE_LIMIT);
        m.insert(WORKFLOW_COMPACTION.id, &WORKFLOW_COMPACTION);
        m.insert(WORKFLOW_ERROR_DETECTED.id, &WORKFLOW_ERROR_DETECTED);
        m.insert(WORKFLOW_APPROVAL_NEEDED.id, &WORKFLOW_APPROVAL_NEEDED);

        // Events
        m.insert(EVENT_PATTERN_DETECTED.id, &EVENT_PATTERN_DETECTED);
        m.insert(EVENT_GAP_DETECTED.id, &EVENT_GAP_DETECTED);

        // Risk (wa-upg.6.3)
        m.insert(RISK_ELEVATED.id, &RISK_ELEVATED);
        m.insert(RISK_HIGH.id, &RISK_HIGH);
        m.insert(RISK_FACTOR_ALT_SCREEN.id, &RISK_FACTOR_ALT_SCREEN);
        m.insert(RISK_FACTOR_DESTRUCTIVE.id, &RISK_FACTOR_DESTRUCTIVE);
        m.insert(RISK_FACTOR_SUDO.id, &RISK_FACTOR_SUDO);

        m
    });

/// Look up an explanation template by ID.
///
/// # Arguments
///
/// * `id` - Template identifier (e.g., "deny.alt_screen")
///
/// # Returns
///
/// The template if found, or `None` if the ID is unknown.
///
/// # Example
///
/// ```rust,ignore
/// use wa_core::explanations::get_explanation;
///
/// if let Some(tmpl) = get_explanation("deny.alt_screen") {
///     println!("Brief: {}", tmpl.brief);
/// }
/// ```
pub fn get_explanation(id: &str) -> Option<&'static ExplanationTemplate> {
    EXPLANATION_TEMPLATES.get(id).copied()
}

/// List all available template IDs.
///
/// Useful for help text and auto-completion.
pub fn list_template_ids() -> Vec<&'static str> {
    let mut ids: Vec<_> = EXPLANATION_TEMPLATES.keys().copied().collect();
    ids.sort_unstable();
    ids
}

/// List templates by category prefix.
///
/// # Arguments
///
/// * `prefix` - Category prefix (e.g., "deny", "workflow", "event", "risk")
///
/// # Returns
///
/// All templates whose ID starts with the given prefix.
pub fn list_templates_by_category(prefix: &str) -> Vec<&'static ExplanationTemplate> {
    EXPLANATION_TEMPLATES
        .iter()
        .filter(|(id, _)| id.starts_with(prefix))
        .map(|(_, tmpl)| *tmpl)
        .collect()
}

/// Render an explanation template with context interpolation.
///
/// Replaces `{key}` placeholders in the detailed text with values from
/// the context map.
///
/// # Arguments
///
/// * `template` - The template to render
/// * `context` - Key-value pairs for placeholder substitution
///
/// # Example
///
/// ```rust,ignore
/// use wa_core::explanations::{get_explanation, render_explanation};
/// use std::collections::HashMap;
///
/// let tmpl = get_explanation("deny.unknown_pane").unwrap();
/// let mut ctx = HashMap::new();
/// ctx.insert("pane_id".to_string(), "42".to_string());
///
/// let rendered = render_explanation(tmpl, &ctx);
/// ```
#[must_use]
#[allow(clippy::implicit_hasher)]
pub fn render_explanation(
    template: &ExplanationTemplate,
    context: &HashMap<String, String>,
) -> String {
    let mut output = template.detailed.to_string();
    for (key, value) in context {
        output = output.replace(&format!("{{{key}}}"), value);
    }
    output
}

/// Format an explanation for terminal display.
///
/// Produces a complete, human-readable explanation including scenario,
/// brief, detailed text, suggestions, and see-also references.
///
/// # Arguments
///
/// * `template` - The template to format
/// * `context` - Optional context for interpolation
///
/// # Returns
///
/// A formatted multi-line string suitable for terminal output.
#[must_use]
#[allow(clippy::implicit_hasher)]
pub fn format_explanation(
    template: &ExplanationTemplate,
    context: Option<&HashMap<String, String>>,
) -> String {
    let mut lines = Vec::new();

    // Header
    lines.push(format!("## {}", template.scenario));
    lines.push(String::new());

    // Brief
    lines.push(format!("**{}**", template.brief));
    lines.push(String::new());

    // Detailed (with optional interpolation)
    let detailed = context.map_or_else(
        || template.detailed.to_string(),
        |ctx| render_explanation(template, ctx),
    );
    lines.push(detailed);
    lines.push(String::new());

    // Suggestions
    if !template.suggestions.is_empty() {
        lines.push("### Suggestions".to_string());
        for suggestion in template.suggestions {
            lines.push(format!("- {suggestion}"));
        }
        lines.push(String::new());
    }

    // See also
    if !template.see_also.is_empty() {
        lines.push(format!("**See also:** {}", template.see_also.join(", ")));
    }

    lines.join("\n")
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_templates_have_valid_structure() {
        for (id, template) in EXPLANATION_TEMPLATES.iter() {
            assert!(!id.is_empty(), "Template ID should not be empty");
            assert_eq!(*id, template.id, "Registry key should match template ID");
            assert!(
                !template.scenario.is_empty(),
                "Scenario should not be empty"
            );
            assert!(!template.brief.is_empty(), "Brief should not be empty");
            assert!(
                !template.detailed.is_empty(),
                "Detailed should not be empty"
            );
        }
    }

    #[test]
    fn get_explanation_returns_known_templates() {
        assert!(get_explanation("deny.alt_screen").is_some());
        assert!(get_explanation("deny.command_running").is_some());
        assert!(get_explanation("workflow.usage_limit").is_some());
    }

    #[test]
    fn get_explanation_returns_none_for_unknown() {
        assert!(get_explanation("nonexistent.template").is_none());
        assert!(get_explanation("").is_none());
    }

    #[test]
    fn list_template_ids_returns_all() {
        let ids = list_template_ids();
        assert!(ids.len() >= 10, "Should have at least 10 templates");
        assert!(ids.contains(&"deny.alt_screen"));
        assert!(ids.contains(&"workflow.usage_limit"));
    }

    #[test]
    fn list_templates_by_category_filters_correctly() {
        let denials = list_templates_by_category("deny");
        assert!(denials.len() >= 4);
        for tmpl in denials {
            assert!(tmpl.id.starts_with("deny."));
        }

        let workflows = list_templates_by_category("workflow");
        assert!(workflows.len() >= 3);
        for tmpl in workflows {
            assert!(tmpl.id.starts_with("workflow."));
        }
    }

    #[test]
    fn render_explanation_interpolates_placeholders() {
        let template = &DENY_UNKNOWN_PANE;
        let mut context = HashMap::new();
        context.insert("pane_id".to_string(), "42".to_string());

        let rendered = render_explanation(template, &context);
        // The template doesn't have {pane_id} placeholder currently,
        // but the function should handle it gracefully
        assert!(!rendered.is_empty());
    }

    #[test]
    fn render_explanation_handles_empty_context() {
        let template = &DENY_ALT_SCREEN;
        let context = HashMap::new();

        let rendered = render_explanation(template, &context);
        assert_eq!(rendered, template.detailed);
    }

    #[test]
    fn format_explanation_produces_readable_output() {
        let template = &DENY_ALT_SCREEN;
        let formatted = format_explanation(template, None);

        assert!(formatted.contains("##"));
        assert!(formatted.contains(template.scenario));
        assert!(formatted.contains(template.brief));
        assert!(formatted.contains("Suggestions"));
        assert!(formatted.contains("See also"));
    }

    #[test]
    fn template_ids_follow_naming_convention() {
        for id in list_template_ids() {
            assert!(
                id.contains('.'),
                "Template ID '{id}' should have category.name format"
            );
            let parts: Vec<_> = id.split('.').collect();
            assert!(
                parts.len() >= 2,
                "Template ID '{id}' should have at least one dot"
            );
            assert!(
                ["deny", "workflow", "event", "risk"].contains(&parts[0]),
                "Template ID '{}' has unknown category '{}'",
                id,
                parts[0]
            );
        }
    }

    #[test]
    fn suggestions_are_actionable() {
        for (_id, template) in EXPLANATION_TEMPLATES.iter() {
            for suggestion in template.suggestions {
                // Suggestions should start with a verb or "Use"
                let first_word = suggestion.split_whitespace().next().unwrap_or("");
                let valid_starts = [
                    "Exit",
                    "Use",
                    "Wait",
                    "Check",
                    "Run",
                    "Configure",
                    "Review",
                    "Let",
                    "Consider",
                    "Reduce",
                    "Increase",
                    "Verify",
                    "Add",
                    "Update",
                    "Enable",
                ];
                assert!(
                    valid_starts.iter().any(|s| first_word.starts_with(s)),
                    "Suggestion '{suggestion}' should start with actionable verb"
                );
            }
        }
    }

    #[test]
    fn see_also_references_valid_commands() {
        for (_id, template) in EXPLANATION_TEMPLATES.iter() {
            for reference in template.see_also {
                // Should be a wa command or external tool
                assert!(
                    reference.starts_with("wa ") || reference == &"caut",
                    "See-also '{reference}' should be a wa command or known tool"
                );
            }
        }
    }
}
