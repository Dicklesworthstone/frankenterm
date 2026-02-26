//! Agent configuration template generation (ft-dr6zv.2.4).
//!
//! Generates agent-specific config snippets that teach each AI coding agent
//! about FrankenTerm's capabilities — robot mode commands, MCP server, pane
//! management, and search.  Templates are embedded as constants and merged
//! idempotently into existing config files.
//!
//! # Design
//!
//! - **No filesystem I/O** — this module produces strings and plans.  The
//!   binary crate handles actual reads/writes.
//! - **Idempotent merge** — content is fenced with start/end markers.  Merging
//!   into a file that already contains the markers replaces the fenced section.
//! - **Backup-before-write** — callers must create `.bak` before modifying.
//! - **Dry-run** — generate a plan without writing anything.

use serde::{Deserialize, Serialize};

use crate::agent_provider::AgentProvider;

// ---------------------------------------------------------------------------
// Marker constants
// ---------------------------------------------------------------------------

/// Start marker for FrankenTerm-managed sections in agent config files.
pub const SECTION_START_MARKER: &str = "<!-- frankenterm:start -->";

/// End marker for FrankenTerm-managed sections in agent config files.
pub const SECTION_END_MARKER: &str = "<!-- frankenterm:end -->";

// ---------------------------------------------------------------------------
// Enums
// ---------------------------------------------------------------------------

/// The kind of config file generated for an agent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentConfigKind {
    /// `CLAUDE.md` — Claude Code project instructions
    ClaudeMd,
    /// `AGENTS.md` — Multi-agent project instructions (Codex, Gemini, Cline, etc.)
    AgentsMd,
    /// `.cursorrules` — Cursor project rules
    CursorRules,
    /// `CONVENTIONS.md` — Aider conventions file
    ConventionsMd,
    /// `.github/copilot-instructions.md` — GitHub Copilot instructions
    CopilotInstructions,
}

impl AgentConfigKind {
    /// The filename (relative to project root) for project-scope placement.
    pub fn project_filename(&self) -> &'static str {
        match self {
            Self::ClaudeMd => "CLAUDE.md",
            Self::AgentsMd => "AGENTS.md",
            Self::CursorRules => ".cursorrules",
            Self::ConventionsMd => "CONVENTIONS.md",
            Self::CopilotInstructions => ".github/copilot-instructions.md",
        }
    }
}

/// Where to place the generated config.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConfigScope {
    /// Place in the current working directory (project-level).
    Project,
    /// Place in the agent's global config directory.
    Global,
}

// ---------------------------------------------------------------------------
// Template content
// ---------------------------------------------------------------------------

/// A generated config template ready to be written or previewed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentConfigTemplate {
    /// Which agent this template is for.
    pub provider: AgentProvider,
    /// The config file type.
    pub kind: AgentConfigKind,
    /// The generated content (Markdown or plain text).
    pub content: String,
    /// Project-relative filename.
    pub filename: String,
}

/// A plan item for dry-run preview.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConfigGenerationPlanItem {
    /// Target agent slug.
    pub slug: String,
    /// Human-readable agent name.
    pub display_name: String,
    /// Config file type.
    pub kind: AgentConfigKind,
    /// Where the file would be placed.
    pub scope: ConfigScope,
    /// Relative file path.
    pub filename: String,
    /// Whether the target file already exists (caller provides).
    pub file_exists: bool,
    /// Whether FrankenTerm section already present (caller provides).
    pub section_exists: bool,
    /// What action would be taken.
    pub action: ConfigAction,
    /// Preview of the content that would be written.
    pub content_preview: String,
}

/// The action that would be taken on a config file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConfigAction {
    /// Create a new file.
    Create,
    /// Append to existing file.
    Append,
    /// Replace existing FrankenTerm section.
    Replace,
    /// Skip — section already up to date.
    Skip,
}

/// Result of applying a config generation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConfigGenerationResult {
    /// Agent slug.
    pub slug: String,
    /// What happened.
    pub action: ConfigAction,
    /// File path (relative).
    pub filename: String,
    /// Whether a backup was created.
    pub backup_created: bool,
    /// Error message if generation failed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

// ---------------------------------------------------------------------------
// Template resolution
// ---------------------------------------------------------------------------

/// Determine which config file kind an agent uses.
pub fn config_kind_for_provider(provider: &AgentProvider) -> AgentConfigKind {
    match provider {
        AgentProvider::Claude => AgentConfigKind::ClaudeMd,
        AgentProvider::Cursor => AgentConfigKind::CursorRules,
        AgentProvider::Aider => AgentConfigKind::ConventionsMd,
        AgentProvider::GithubCopilot => AgentConfigKind::CopilotInstructions,
        // Codex, Gemini, Cline, Windsurf, OpenCode, Grok, Devin, Factory, Unknown
        _ => AgentConfigKind::AgentsMd,
    }
}

/// Generate a config template for a specific agent.
pub fn generate_template(provider: &AgentProvider) -> AgentConfigTemplate {
    let kind = config_kind_for_provider(provider);
    let content = template_content(provider, kind);
    AgentConfigTemplate {
        provider: provider.clone(),
        kind,
        content,
        filename: kind.project_filename().to_string(),
    }
}

/// Generate templates for all detected agents.
pub fn generate_templates_for_detected(slugs: &[String]) -> Vec<AgentConfigTemplate> {
    slugs
        .iter()
        .map(|slug| {
            let provider = AgentProvider::from_slug(slug);
            generate_template(&provider)
        })
        .collect()
}

/// Build a dry-run plan for a set of agents.
///
/// The caller provides `file_state` as a closure that returns `(file_exists,
/// existing_content)` for each target filename.  This keeps I/O out of the
/// core library.
pub fn build_generation_plan(
    slugs: &[String],
    scope: ConfigScope,
    file_state: impl Fn(&str) -> (bool, Option<String>),
) -> Vec<ConfigGenerationPlanItem> {
    slugs
        .iter()
        .map(|slug| {
            let provider = AgentProvider::from_slug(slug);
            let template = generate_template(&provider);
            let (file_exists, existing_content) = file_state(&template.filename);
            let section_exists = existing_content
                .as_deref()
                .is_some_and(|c| c.contains(SECTION_START_MARKER));

            let action = if !file_exists {
                ConfigAction::Create
            } else if section_exists {
                // Check if content is identical
                let merged = merge_into_existing(
                    existing_content.as_deref().unwrap_or(""),
                    &template.content,
                );
                if merged == existing_content.as_deref().unwrap_or("") {
                    ConfigAction::Skip
                } else {
                    ConfigAction::Replace
                }
            } else {
                ConfigAction::Append
            };

            ConfigGenerationPlanItem {
                slug: slug.clone(),
                display_name: provider.display_name().to_string(),
                kind: template.kind,
                scope,
                filename: template.filename,
                file_exists,
                section_exists,
                action,
                content_preview: template.content,
            }
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Merge logic
// ---------------------------------------------------------------------------

/// Merge FrankenTerm section into existing file content.
///
/// - If the file already contains the start/end markers, replaces the fenced
///   section.
/// - Otherwise, appends the section after a blank line.
///
/// Returns the full merged content.
pub fn merge_into_existing(existing: &str, new_section: &str) -> String {
    let fenced = format!(
        "{}\n{}\n{}",
        SECTION_START_MARKER, new_section, SECTION_END_MARKER
    );

    if let Some(start_idx) = existing.find(SECTION_START_MARKER) {
        if let Some(end_marker_start) = existing[start_idx..].find(SECTION_END_MARKER) {
            let end_idx = start_idx + end_marker_start + SECTION_END_MARKER.len();
            let mut result = String::with_capacity(existing.len());
            result.push_str(&existing[..start_idx]);
            result.push_str(&fenced);
            result.push_str(&existing[end_idx..]);
            return result;
        }
    }

    // No existing markers — append.
    let mut result = existing.to_string();
    if !result.is_empty() && !result.ends_with('\n') {
        result.push('\n');
    }
    if !result.is_empty() {
        result.push('\n');
    }
    result.push_str(&fenced);
    result.push('\n');
    result
}

/// Check whether the existing content already contains an up-to-date section.
pub fn section_is_current(existing: &str, new_section: &str) -> bool {
    let fenced = format!(
        "{}\n{}\n{}",
        SECTION_START_MARKER, new_section, SECTION_END_MARKER
    );
    existing.contains(&fenced)
}

// ---------------------------------------------------------------------------
// Template content generation
// ---------------------------------------------------------------------------

fn template_content(provider: &AgentProvider, kind: AgentConfigKind) -> String {
    let slug = provider.canonical_slug();
    let display = provider.display_name();

    match kind {
        AgentConfigKind::ClaudeMd => claude_md_template(slug, display),
        AgentConfigKind::AgentsMd => agents_md_template(slug, display),
        AgentConfigKind::CursorRules => cursor_rules_template(slug, display),
        AgentConfigKind::ConventionsMd => conventions_md_template(slug, display),
        AgentConfigKind::CopilotInstructions => copilot_instructions_template(slug, display),
    }
}

fn robot_mode_reference() -> &'static str {
    r#"## FrankenTerm Robot Mode Commands

FrankenTerm (`ft`) provides a robot-mode JSON API for agent automation:

### Search
- `ft robot search "query" --limit N --format json` — search terminal history
- `ft robot search "query" --explain` — search with scoring breakdown
- `ft robot search-index stats` — index health metrics

### Pane Management
- `ft robot panes list` — list all panes with metadata
- `ft robot panes inspect <id>` — detailed pane state
- `ft robot panes send-text <id> "command"` — send keystrokes to a pane

### Agent Inventory
- `ft robot agents list` — show detected/installed agents
- `ft robot agents running` — show active agents in panes
- `ft robot agents detect --refresh` — refresh detection cache

### Session & Status
- `ft robot status` — overall system status
- `ft robot sessions list` — list active sessions

All commands output JSON by default. Add `--format toon` for token-optimized output."#
}

fn claude_md_template(_slug: &str, _display: &str) -> String {
    format!(
        r"# FrankenTerm Integration

This project uses FrankenTerm (`ft`) as its terminal orchestration platform.

{}

## Usage Tips

- Use `ft robot search` to find relevant terminal output from other agents.
- Use `ft robot panes list` to discover other running agents and their panes.
- Use `ft robot agents running` to see which agents are currently active.
- Prefer `ft robot` commands over raw tmux/wezterm commands for automation.",
        robot_mode_reference()
    )
}

fn agents_md_template(slug: &str, display: &str) -> String {
    format!(
        r"# FrankenTerm Integration for {display}

This project uses FrankenTerm (`ft`) as its terminal orchestration platform.
Agent: {slug}

{ref_section}

## Usage Tips

- Use `ft robot search` to find relevant terminal output across all panes.
- Use `ft robot panes list` to discover other running agents.
- Use `ft robot agents running` to see which agents are currently active.
- Prefer structured `ft robot` JSON commands over raw terminal parsing.",
        display = display,
        slug = slug,
        ref_section = robot_mode_reference(),
    )
}

fn cursor_rules_template(_slug: &str, _display: &str) -> String {
    format!(
        r"# FrankenTerm Integration

This project uses FrankenTerm (`ft`) for terminal orchestration.

{}

## Rules

- When you need terminal output from other agents, use `ft robot search`.
- When you need to interact with other panes, use `ft robot panes send-text`.
- When checking system state, use `ft robot status`.
- All `ft robot` commands return structured JSON.",
        robot_mode_reference()
    )
}

fn conventions_md_template(_slug: &str, _display: &str) -> String {
    format!(
        r"# FrankenTerm Integration

This project uses FrankenTerm (`ft`) for terminal orchestration.

{}

## Conventions

- Use `ft robot` commands for automation instead of raw terminal commands.
- Check `ft robot agents running` before spawning new agent sessions.
- Search terminal history with `ft robot search` for context.",
        robot_mode_reference()
    )
}

fn copilot_instructions_template(_slug: &str, _display: &str) -> String {
    format!(
        r"# FrankenTerm Integration

This project uses FrankenTerm (`ft`) for terminal orchestration.

{}

## Instructions

- Use `ft robot` JSON API for terminal automation.
- Use `ft robot search` to find terminal output across agent sessions.
- Use `ft robot panes list` for pane discovery.",
        robot_mode_reference()
    )
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // config_kind_for_provider
    // -----------------------------------------------------------------------

    #[test]
    fn kind_claude_is_claude_md() {
        assert_eq!(
            config_kind_for_provider(&AgentProvider::Claude),
            AgentConfigKind::ClaudeMd
        );
    }

    #[test]
    fn kind_cursor_is_cursor_rules() {
        assert_eq!(
            config_kind_for_provider(&AgentProvider::Cursor),
            AgentConfigKind::CursorRules
        );
    }

    #[test]
    fn kind_aider_is_conventions_md() {
        assert_eq!(
            config_kind_for_provider(&AgentProvider::Aider),
            AgentConfigKind::ConventionsMd
        );
    }

    #[test]
    fn kind_copilot_is_copilot_instructions() {
        assert_eq!(
            config_kind_for_provider(&AgentProvider::GithubCopilot),
            AgentConfigKind::CopilotInstructions
        );
    }

    #[test]
    fn kind_codex_is_agents_md() {
        assert_eq!(
            config_kind_for_provider(&AgentProvider::Codex),
            AgentConfigKind::AgentsMd
        );
    }

    #[test]
    fn kind_gemini_is_agents_md() {
        assert_eq!(
            config_kind_for_provider(&AgentProvider::Gemini),
            AgentConfigKind::AgentsMd
        );
    }

    #[test]
    fn kind_cline_is_agents_md() {
        assert_eq!(
            config_kind_for_provider(&AgentProvider::Cline),
            AgentConfigKind::AgentsMd
        );
    }

    #[test]
    fn kind_windsurf_is_agents_md() {
        assert_eq!(
            config_kind_for_provider(&AgentProvider::Windsurf),
            AgentConfigKind::AgentsMd
        );
    }

    #[test]
    fn kind_opencode_is_agents_md() {
        assert_eq!(
            config_kind_for_provider(&AgentProvider::Opencode),
            AgentConfigKind::AgentsMd
        );
    }

    #[test]
    fn kind_unknown_is_agents_md() {
        assert_eq!(
            config_kind_for_provider(&AgentProvider::Unknown("custom".into())),
            AgentConfigKind::AgentsMd
        );
    }

    // -----------------------------------------------------------------------
    // project_filename
    // -----------------------------------------------------------------------

    #[test]
    fn filename_claude_md() {
        assert_eq!(AgentConfigKind::ClaudeMd.project_filename(), "CLAUDE.md");
    }

    #[test]
    fn filename_agents_md() {
        assert_eq!(AgentConfigKind::AgentsMd.project_filename(), "AGENTS.md");
    }

    #[test]
    fn filename_cursor_rules() {
        assert_eq!(
            AgentConfigKind::CursorRules.project_filename(),
            ".cursorrules"
        );
    }

    #[test]
    fn filename_conventions_md() {
        assert_eq!(
            AgentConfigKind::ConventionsMd.project_filename(),
            "CONVENTIONS.md"
        );
    }

    #[test]
    fn filename_copilot_instructions() {
        assert_eq!(
            AgentConfigKind::CopilotInstructions.project_filename(),
            ".github/copilot-instructions.md"
        );
    }

    // -----------------------------------------------------------------------
    // generate_template
    // -----------------------------------------------------------------------

    #[test]
    fn template_claude_contains_robot_mode() {
        let t = generate_template(&AgentProvider::Claude);
        assert_eq!(t.kind, AgentConfigKind::ClaudeMd);
        assert!(t.content.contains("ft robot search"));
        assert!(t.content.contains("ft robot panes list"));
        assert!(t.content.contains("ft robot agents"));
    }

    #[test]
    fn template_codex_contains_slug() {
        let t = generate_template(&AgentProvider::Codex);
        assert!(t.content.contains("codex"));
        assert!(t.content.contains("Codex"));
    }

    #[test]
    fn template_cursor_contains_rules() {
        let t = generate_template(&AgentProvider::Cursor);
        assert_eq!(t.kind, AgentConfigKind::CursorRules);
        assert!(t.content.contains("Rules"));
    }

    #[test]
    fn template_aider_contains_conventions() {
        let t = generate_template(&AgentProvider::Aider);
        assert_eq!(t.kind, AgentConfigKind::ConventionsMd);
        assert!(t.content.contains("Conventions"));
    }

    #[test]
    fn template_copilot_contains_instructions() {
        let t = generate_template(&AgentProvider::GithubCopilot);
        assert_eq!(t.kind, AgentConfigKind::CopilotInstructions);
        assert!(t.content.contains("Instructions"));
    }

    #[test]
    fn all_known_agents_generate_nonempty_templates() {
        for provider in AgentProvider::all_known() {
            let t = generate_template(provider);
            assert!(
                !t.content.is_empty(),
                "empty template for {}",
                provider.canonical_slug()
            );
            assert!(
                t.content.contains("ft robot"),
                "missing robot reference for {}",
                provider.canonical_slug()
            );
        }
    }

    // -----------------------------------------------------------------------
    // generate_templates_for_detected
    // -----------------------------------------------------------------------

    #[test]
    fn templates_for_detected_empty() {
        let ts = generate_templates_for_detected(&[]);
        assert!(ts.is_empty());
    }

    #[test]
    fn templates_for_detected_multiple() {
        let slugs = vec!["claude".to_string(), "codex".to_string()];
        let ts = generate_templates_for_detected(&slugs);
        assert_eq!(ts.len(), 2);
        assert_eq!(ts[0].provider, AgentProvider::Claude);
        assert_eq!(ts[1].provider, AgentProvider::Codex);
    }

    // -----------------------------------------------------------------------
    // merge_into_existing
    // -----------------------------------------------------------------------

    #[test]
    fn merge_into_empty_file() {
        let result = merge_into_existing("", "new content");
        assert!(result.contains(SECTION_START_MARKER));
        assert!(result.contains("new content"));
        assert!(result.contains(SECTION_END_MARKER));
    }

    #[test]
    fn merge_appends_to_existing() {
        let existing = "# My Project\n\nSome content.";
        let result = merge_into_existing(existing, "ft section");
        assert!(result.starts_with("# My Project"));
        assert!(result.contains("Some content."));
        assert!(result.contains(SECTION_START_MARKER));
        assert!(result.contains("ft section"));
        assert!(result.contains(SECTION_END_MARKER));
    }

    #[test]
    fn merge_replaces_existing_section() {
        let existing = format!(
            "# Header\n\n{}\nold content\n{}\n\n# Footer",
            SECTION_START_MARKER, SECTION_END_MARKER
        );
        let result = merge_into_existing(&existing, "new content");
        assert!(result.contains("# Header"));
        assert!(result.contains("# Footer"));
        assert!(result.contains("new content"));
        assert!(!result.contains("old content"));
        // Should have exactly one start marker
        assert_eq!(
            result.matches(SECTION_START_MARKER).count(),
            1,
            "should have exactly one start marker"
        );
    }

    #[test]
    fn merge_idempotent() {
        let content = "some section";
        let first = merge_into_existing("", content);
        let second = merge_into_existing(&first, content);
        assert_eq!(first, second, "merge should be idempotent");
    }

    #[test]
    fn merge_preserves_content_before_markers() {
        let prefix = "# Important\n\nDo not remove this.\n";
        let existing = format!(
            "{}{}\nold\n{}\n",
            prefix, SECTION_START_MARKER, SECTION_END_MARKER
        );
        let result = merge_into_existing(&existing, "updated");
        assert!(result.starts_with(prefix));
    }

    #[test]
    fn merge_preserves_content_after_markers() {
        let suffix = "\n# Footer\nKeep this.";
        let existing = format!(
            "{}\nold\n{}{}",
            SECTION_START_MARKER, SECTION_END_MARKER, suffix
        );
        let result = merge_into_existing(&existing, "updated");
        assert!(result.ends_with(suffix));
    }

    // -----------------------------------------------------------------------
    // section_is_current
    // -----------------------------------------------------------------------

    #[test]
    fn section_current_when_identical() {
        let content = "ft section";
        let merged = merge_into_existing("", content);
        assert!(section_is_current(&merged, content));
    }

    #[test]
    fn section_not_current_when_different() {
        let merged = merge_into_existing("", "old section");
        assert!(!section_is_current(&merged, "new section"));
    }

    #[test]
    fn section_not_current_when_absent() {
        assert!(!section_is_current("# Just a file", "ft section"));
    }

    // -----------------------------------------------------------------------
    // build_generation_plan
    // -----------------------------------------------------------------------

    #[test]
    fn plan_create_when_no_file() {
        let slugs = vec!["claude".to_string()];
        let plan = build_generation_plan(&slugs, ConfigScope::Project, |_| (false, None));
        assert_eq!(plan.len(), 1);
        assert_eq!(plan[0].action, ConfigAction::Create);
        assert!(!plan[0].file_exists);
        assert!(!plan[0].section_exists);
    }

    #[test]
    fn plan_append_when_file_exists_no_section() {
        let slugs = vec!["claude".to_string()];
        let plan = build_generation_plan(&slugs, ConfigScope::Project, |_| {
            (true, Some("# Existing content".to_string()))
        });
        assert_eq!(plan.len(), 1);
        assert_eq!(plan[0].action, ConfigAction::Append);
        assert!(plan[0].file_exists);
        assert!(!plan[0].section_exists);
    }

    #[test]
    fn plan_skip_when_section_current() {
        let slugs = vec!["claude".to_string()];
        let template = generate_template(&AgentProvider::Claude);
        let merged = merge_into_existing("# My Project\n", &template.content);
        let plan = build_generation_plan(&slugs, ConfigScope::Project, |_| {
            (true, Some(merged.clone()))
        });
        assert_eq!(plan.len(), 1);
        assert_eq!(plan[0].action, ConfigAction::Skip);
    }

    #[test]
    fn plan_replace_when_section_outdated() {
        let slugs = vec!["claude".to_string()];
        let outdated = format!(
            "# My Project\n\n{}\nold content\n{}\n",
            SECTION_START_MARKER, SECTION_END_MARKER
        );
        let plan = build_generation_plan(&slugs, ConfigScope::Project, |_| {
            (true, Some(outdated.clone()))
        });
        assert_eq!(plan.len(), 1);
        assert_eq!(plan[0].action, ConfigAction::Replace);
    }

    #[test]
    fn plan_multiple_agents() {
        let slugs = vec![
            "claude".to_string(),
            "codex".to_string(),
            "cursor".to_string(),
        ];
        let plan = build_generation_plan(&slugs, ConfigScope::Project, |_| (false, None));
        assert_eq!(plan.len(), 3);
        assert_eq!(plan[0].slug, "claude");
        assert_eq!(plan[1].slug, "codex");
        assert_eq!(plan[2].slug, "cursor");
    }

    // -----------------------------------------------------------------------
    // AgentConfigKind serde
    // -----------------------------------------------------------------------

    #[test]
    fn config_kind_serde_roundtrip() {
        let kinds = [
            AgentConfigKind::ClaudeMd,
            AgentConfigKind::AgentsMd,
            AgentConfigKind::CursorRules,
            AgentConfigKind::ConventionsMd,
            AgentConfigKind::CopilotInstructions,
        ];
        for kind in &kinds {
            let json = serde_json::to_string(kind).unwrap();
            let back: AgentConfigKind = serde_json::from_str(&json).unwrap();
            assert_eq!(&back, kind);
        }
    }

    #[test]
    fn config_kind_uses_snake_case() {
        let json = serde_json::to_string(&AgentConfigKind::ClaudeMd).unwrap();
        assert_eq!(json, "\"claude_md\"");
        let json = serde_json::to_string(&AgentConfigKind::CopilotInstructions).unwrap();
        assert_eq!(json, "\"copilot_instructions\"");
    }

    // -----------------------------------------------------------------------
    // ConfigScope serde
    // -----------------------------------------------------------------------

    #[test]
    fn config_scope_serde_roundtrip() {
        for scope in &[ConfigScope::Project, ConfigScope::Global] {
            let json = serde_json::to_string(scope).unwrap();
            let back: ConfigScope = serde_json::from_str(&json).unwrap();
            assert_eq!(&back, scope);
        }
    }

    // -----------------------------------------------------------------------
    // ConfigAction serde
    // -----------------------------------------------------------------------

    #[test]
    fn config_action_serde_roundtrip() {
        let actions = [
            ConfigAction::Create,
            ConfigAction::Append,
            ConfigAction::Replace,
            ConfigAction::Skip,
        ];
        for action in &actions {
            let json = serde_json::to_string(action).unwrap();
            let back: ConfigAction = serde_json::from_str(&json).unwrap();
            assert_eq!(&back, action);
        }
    }

    // -----------------------------------------------------------------------
    // AgentConfigTemplate serde
    // -----------------------------------------------------------------------

    #[test]
    fn agent_config_template_serde_roundtrip() {
        let t = generate_template(&AgentProvider::Claude);
        let json = serde_json::to_string(&t).unwrap();
        let back: AgentConfigTemplate = serde_json::from_str(&json).unwrap();
        assert_eq!(back.provider, t.provider);
        assert_eq!(back.kind, t.kind);
        assert_eq!(back.content, t.content);
        assert_eq!(back.filename, t.filename);
    }

    // -----------------------------------------------------------------------
    // ConfigGenerationResult serde
    // -----------------------------------------------------------------------

    #[test]
    fn generation_result_serde_roundtrip() {
        let result = ConfigGenerationResult {
            slug: "claude".to_string(),
            action: ConfigAction::Create,
            filename: "CLAUDE.md".to_string(),
            backup_created: false,
            error: None,
        };
        let json = serde_json::to_string(&result).unwrap();
        let back: ConfigGenerationResult = serde_json::from_str(&json).unwrap();
        assert_eq!(back.slug, result.slug);
        assert_eq!(back.action, result.action);
        assert_eq!(back.filename, result.filename);
        assert_eq!(back.backup_created, result.backup_created);
        assert!(back.error.is_none());
    }

    #[test]
    fn generation_result_error_skips_none() {
        let result = ConfigGenerationResult {
            slug: "codex".to_string(),
            action: ConfigAction::Append,
            filename: "AGENTS.md".to_string(),
            backup_created: true,
            error: None,
        };
        let json = serde_json::to_string(&result).unwrap();
        assert!(!json.contains("error"));
    }

    #[test]
    fn generation_result_with_error() {
        let result = ConfigGenerationResult {
            slug: "cursor".to_string(),
            action: ConfigAction::Skip,
            filename: ".cursorrules".to_string(),
            backup_created: false,
            error: Some("permission denied".to_string()),
        };
        let json = serde_json::to_string(&result).unwrap();
        assert!(json.contains("permission denied"));
    }
}
