//! Unified agent provider identification.
//!
//! [`AgentProvider`] is the canonical enum for identifying AI coding agent
//! providers across FrankenTerm subsystems.  It unifies:
//!
//! - `patterns::AgentType` (runtime pattern detection)
//! - `franken-agent-detection` connector slugs (filesystem probes)
//! - `casr_types` provider slugs (session portability)
//! - `agent_correlator` detection sources (pane titles, process names)
//!
//! Every subsystem that needs to name an agent should use this enum rather than
//! ad-hoc strings or module-local enums.

use serde::{Deserialize, Serialize};

/// Canonical agent provider identity.
///
/// Covers all AI coding agent CLIs known to FrankenTerm.  The `Unknown(String)`
/// variant provides forward-compatibility for newly discovered agents.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AgentProvider {
    /// Anthropic Claude Code CLI
    Claude,
    /// Cline (VS Code agent, formerly Continue)
    Cline,
    /// OpenAI Codex CLI
    Codex,
    /// Cursor AI editor
    Cursor,
    /// Devin autonomous agent
    Devin,
    /// Factory Droid CLI
    Factory,
    /// Google Gemini CLI
    Gemini,
    /// GitHub Copilot (CLI and editor integration)
    GithubCopilot,
    /// xAI Grok CLI
    Grok,
    /// OpenCode CLI
    Opencode,
    /// Aider pair-programming CLI
    Aider,
    /// Windsurf (Codeium) AI editor
    Windsurf,
    /// Agent not in the known set.
    Unknown(String),
}

/// Source used when resolving an [`AgentProvider`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderResolutionSource {
    ProcessName,
    BinaryName,
    Slug,
    AgentType,
}

/// Structured diagnostics for provider-resolution decisions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderResolutionDiagnostics {
    pub source: ProviderResolutionSource,
    pub input: String,
    pub normalized: String,
    pub candidates: Vec<AgentProvider>,
    pub selected: Option<AgentProvider>,
    pub ambiguous: bool,
}

impl ProviderResolutionDiagnostics {
    fn new(
        source: ProviderResolutionSource,
        input: String,
        normalized: String,
        candidates: Vec<AgentProvider>,
        selected: Option<AgentProvider>,
    ) -> Self {
        Self {
            source,
            input,
            normalized,
            ambiguous: candidates.len() > 1,
            candidates,
            selected,
        }
    }

    /// True when resolution did not produce a known provider.
    pub fn is_unknown(&self) -> bool {
        match &self.selected {
            None => true,
            Some(AgentProvider::Unknown(_)) => true,
            Some(_) => false,
        }
    }
}

impl AgentProvider {
    /// Identify a provider from a running process name (case-insensitive substring match).
    ///
    /// Returns `None` when the name does not match any known agent pattern.
    pub fn from_process_name(name: &str) -> Option<Self> {
        Self::diagnostics_from_process_name(name).selected
    }

    /// Identify a provider from a binary/executable name (case-insensitive exact or prefix match).
    ///
    /// Handles common binary names such as `claude`, `codex-cli`, `gemini-cli`, etc.
    /// Returns `None` when the binary name is unrecognized.
    pub fn from_binary_name(name: &str) -> Option<Self> {
        Self::diagnostics_from_binary_name(name).selected
    }

    /// Resolve from a process name and return structured diagnostics.
    pub fn diagnostics_from_process_name(name: &str) -> ProviderResolutionDiagnostics {
        let normalized = name.to_ascii_lowercase();
        let candidates = process_name_candidates(&normalized);
        let selected = candidates.first().cloned();
        ProviderResolutionDiagnostics::new(
            ProviderResolutionSource::ProcessName,
            name.to_string(),
            normalized,
            candidates,
            selected,
        )
    }

    /// Resolve from a binary name and return structured diagnostics.
    pub fn diagnostics_from_binary_name(name: &str) -> ProviderResolutionDiagnostics {
        let normalized = name.to_ascii_lowercase();
        let candidates = binary_name_candidates(&normalized);
        let selected = candidates.first().cloned();
        ProviderResolutionDiagnostics::new(
            ProviderResolutionSource::BinaryName,
            name.to_string(),
            normalized,
            candidates,
            selected,
        )
    }

    /// Resolve from a canonical slug or alias and return structured diagnostics.
    pub fn diagnostics_from_slug(slug: &str) -> ProviderResolutionDiagnostics {
        let normalized = slug.to_ascii_lowercase();
        let selected = Self::from_slug(slug.trim());
        let candidates = if matches!(selected, Self::Unknown(_)) {
            Vec::new()
        } else {
            vec![selected.clone()]
        };
        ProviderResolutionDiagnostics::new(
            ProviderResolutionSource::Slug,
            slug.to_string(),
            normalized,
            candidates,
            Some(selected),
        )
    }

    /// Resolve from legacy pattern `AgentType` and return structured diagnostics.
    pub fn diagnostics_from_agent_type(
        agent_type: &crate::patterns::AgentType,
    ) -> ProviderResolutionDiagnostics {
        let selected = Self::from_agent_type(agent_type);
        let candidates = if matches!(selected, Self::Unknown(_)) {
            Vec::new()
        } else {
            vec![selected.clone()]
        };
        ProviderResolutionDiagnostics::new(
            ProviderResolutionSource::AgentType,
            agent_type.to_string(),
            agent_type.to_string(),
            candidates,
            Some(selected),
        )
    }

    /// Human-readable display name suitable for UI labels.
    pub fn display_name(&self) -> &str {
        match self {
            Self::Claude => "Claude Code",
            Self::Cline => "Cline",
            Self::Codex => "Codex",
            Self::Cursor => "Cursor",
            Self::Devin => "Devin",
            Self::Factory => "Factory",
            Self::Gemini => "Gemini",
            Self::GithubCopilot => "GitHub Copilot",
            Self::Grok => "Grok",
            Self::Opencode => "OpenCode",
            Self::Aider => "Aider",
            Self::Windsurf => "Windsurf",
            Self::Unknown(s) => s.as_str(),
        }
    }

    /// Stable lowercase canonical identifier (matches `franken-agent-detection` slugs).
    pub fn canonical_slug(&self) -> &str {
        match self {
            Self::Claude => "claude",
            Self::Cline => "cline",
            Self::Codex => "codex",
            Self::Cursor => "cursor",
            Self::Devin => "devin",
            Self::Factory => "factory",
            Self::Gemini => "gemini",
            Self::GithubCopilot => "github-copilot",
            Self::Grok => "grok",
            Self::Opencode => "opencode",
            Self::Aider => "aider",
            Self::Windsurf => "windsurf",
            Self::Unknown(s) => s.as_str(),
        }
    }

    /// Stable lowercase canonical identifier.
    ///
    /// Alias for [`Self::canonical_slug`] kept for cross-module naming consistency.
    pub fn canonical_name(&self) -> &str {
        self.canonical_slug()
    }

    /// Parse a canonical slug or known alias into an `AgentProvider`.
    ///
    /// Returns `Unknown(slug)` when the slug is not in the known set.
    pub fn from_slug(slug: &str) -> Self {
        let lower = slug.to_ascii_lowercase();
        match lower.as_str() {
            "claude" | "claude-code" | "claude_code" => Self::Claude,
            "cline" => Self::Cline,
            "codex" | "codex-cli" => Self::Codex,
            "cursor" => Self::Cursor,
            "devin" => Self::Devin,
            "factory" | "factory-droid" => Self::Factory,
            "gemini" | "gemini-cli" => Self::Gemini,
            "github-copilot" | "copilot" | "gh-copilot" => Self::GithubCopilot,
            "grok" | "grok-cli" => Self::Grok,
            "opencode" | "open-code" => Self::Opencode,
            "aider" => Self::Aider,
            "windsurf" => Self::Windsurf,
            _ => Self::Unknown(slug.to_string()),
        }
    }

    /// Convert from the legacy `AgentType` enum used in pattern detection.
    pub fn from_agent_type(agent_type: &crate::patterns::AgentType) -> Self {
        match agent_type {
            crate::patterns::AgentType::ClaudeCode => Self::Claude,
            crate::patterns::AgentType::Codex => Self::Codex,
            crate::patterns::AgentType::Gemini => Self::Gemini,
            crate::patterns::AgentType::Wezterm => Self::Unknown("wezterm".to_string()),
            crate::patterns::AgentType::Unknown => Self::Unknown("unknown".to_string()),
        }
    }

    /// Convert to the legacy `AgentType` for backwards-compatible pattern engine calls.
    ///
    /// Providers not represented in `AgentType` map to `Unknown`.
    pub fn to_agent_type(&self) -> crate::patterns::AgentType {
        match self {
            Self::Claude => crate::patterns::AgentType::ClaudeCode,
            Self::Codex => crate::patterns::AgentType::Codex,
            Self::Gemini => crate::patterns::AgentType::Gemini,
            _ => crate::patterns::AgentType::Unknown,
        }
    }

    /// All known (non-`Unknown`) provider variants.
    pub fn all_known() -> &'static [AgentProvider] {
        &[
            AgentProvider::Claude,
            AgentProvider::Cline,
            AgentProvider::Codex,
            AgentProvider::Cursor,
            AgentProvider::Devin,
            AgentProvider::Factory,
            AgentProvider::Gemini,
            AgentProvider::GithubCopilot,
            AgentProvider::Grok,
            AgentProvider::Opencode,
            AgentProvider::Aider,
            AgentProvider::Windsurf,
        ]
    }
}

fn process_name_candidates(normalized: &str) -> Vec<AgentProvider> {
    let mut candidates = Vec::new();
    // Order matters: more-specific priorities first.
    if normalized.contains("claude-code")
        || normalized.contains("claude_code")
        || normalized.contains("claude")
    {
        candidates.push(AgentProvider::Claude);
    }
    if normalized.contains("codex") {
        candidates.push(AgentProvider::Codex);
    }
    if normalized.contains("gemini") {
        candidates.push(AgentProvider::Gemini);
    }
    if normalized.contains("cursor") {
        candidates.push(AgentProvider::Cursor);
    }
    if normalized.contains("windsurf") {
        candidates.push(AgentProvider::Windsurf);
    }
    if normalized.contains("cline") {
        candidates.push(AgentProvider::Cline);
    }
    if normalized.contains("copilot") {
        candidates.push(AgentProvider::GithubCopilot);
    }
    if normalized.contains("devin") {
        candidates.push(AgentProvider::Devin);
    }
    if normalized.contains("grok") {
        candidates.push(AgentProvider::Grok);
    }
    if normalized.contains("aider") {
        candidates.push(AgentProvider::Aider);
    }
    if normalized.contains("opencode") {
        candidates.push(AgentProvider::Opencode);
    }
    if normalized.contains("factory") {
        candidates.push(AgentProvider::Factory);
    }
    candidates
}

fn binary_name_candidates(normalized: &str) -> Vec<AgentProvider> {
    match normalized {
        "claude" | "claude-code" | "claude_code" => vec![AgentProvider::Claude],
        "codex" | "codex-cli" => vec![AgentProvider::Codex],
        "gemini" | "gemini-cli" => vec![AgentProvider::Gemini],
        "cursor" => vec![AgentProvider::Cursor],
        "windsurf" => vec![AgentProvider::Windsurf],
        "cline" => vec![AgentProvider::Cline],
        "copilot" | "github-copilot" => vec![AgentProvider::GithubCopilot],
        "devin" => vec![AgentProvider::Devin],
        "grok" | "grok-cli" => vec![AgentProvider::Grok],
        "aider" => vec![AgentProvider::Aider],
        "opencode" | "open-code" => vec![AgentProvider::Opencode],
        "factory" | "factory-droid" => vec![AgentProvider::Factory],
        _ => Vec::new(),
    }
}

impl std::fmt::Display for AgentProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.canonical_slug())
    }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // -------------------------------------------------------------------------
    // from_process_name
    // -------------------------------------------------------------------------

    #[test]
    fn test_from_process_name_claude_code() {
        assert_eq!(
            AgentProvider::from_process_name("claude-code"),
            Some(AgentProvider::Claude)
        );
    }

    #[test]
    fn test_from_process_name_claude_uppercase() {
        assert_eq!(
            AgentProvider::from_process_name("Claude"),
            Some(AgentProvider::Claude)
        );
    }

    #[test]
    fn test_from_process_name_claude_code_underscore() {
        assert_eq!(
            AgentProvider::from_process_name("claude_code"),
            Some(AgentProvider::Claude)
        );
    }

    #[test]
    fn test_from_process_name_codex() {
        assert_eq!(
            AgentProvider::from_process_name("codex"),
            Some(AgentProvider::Codex)
        );
    }

    #[test]
    fn test_from_process_name_codex_mixed_case() {
        assert_eq!(
            AgentProvider::from_process_name("Codex-CLI"),
            Some(AgentProvider::Codex)
        );
    }

    #[test]
    fn test_from_process_name_gemini() {
        assert_eq!(
            AgentProvider::from_process_name("gemini-cli"),
            Some(AgentProvider::Gemini)
        );
    }

    #[test]
    fn test_from_process_name_cursor() {
        assert_eq!(
            AgentProvider::from_process_name("cursor"),
            Some(AgentProvider::Cursor)
        );
    }

    #[test]
    fn test_from_process_name_windsurf() {
        assert_eq!(
            AgentProvider::from_process_name("windsurf"),
            Some(AgentProvider::Windsurf)
        );
    }

    #[test]
    fn test_from_process_name_cline() {
        assert_eq!(
            AgentProvider::from_process_name("cline"),
            Some(AgentProvider::Cline)
        );
    }

    #[test]
    fn test_from_process_name_copilot() {
        assert_eq!(
            AgentProvider::from_process_name("copilot-agent"),
            Some(AgentProvider::GithubCopilot)
        );
    }

    #[test]
    fn test_from_process_name_devin() {
        assert_eq!(
            AgentProvider::from_process_name("devin-worker"),
            Some(AgentProvider::Devin)
        );
    }

    #[test]
    fn test_from_process_name_grok() {
        assert_eq!(
            AgentProvider::from_process_name("grok"),
            Some(AgentProvider::Grok)
        );
    }

    #[test]
    fn test_from_process_name_aider() {
        assert_eq!(
            AgentProvider::from_process_name("aider"),
            Some(AgentProvider::Aider)
        );
    }

    #[test]
    fn test_from_process_name_opencode() {
        assert_eq!(
            AgentProvider::from_process_name("opencode"),
            Some(AgentProvider::Opencode)
        );
    }

    #[test]
    fn test_from_process_name_factory() {
        assert_eq!(
            AgentProvider::from_process_name("factory-droid"),
            Some(AgentProvider::Factory)
        );
    }

    #[test]
    fn test_from_process_name_unknown() {
        assert_eq!(AgentProvider::from_process_name("bash"), None);
    }

    #[test]
    fn test_from_process_name_empty() {
        assert_eq!(AgentProvider::from_process_name(""), None);
    }

    // -------------------------------------------------------------------------
    // from_binary_name
    // -------------------------------------------------------------------------

    #[test]
    fn test_from_binary_name_claude() {
        assert_eq!(
            AgentProvider::from_binary_name("claude"),
            Some(AgentProvider::Claude)
        );
    }

    #[test]
    fn test_from_binary_name_claude_code() {
        assert_eq!(
            AgentProvider::from_binary_name("claude-code"),
            Some(AgentProvider::Claude)
        );
    }

    #[test]
    fn test_from_binary_name_codex() {
        assert_eq!(
            AgentProvider::from_binary_name("codex"),
            Some(AgentProvider::Codex)
        );
    }

    #[test]
    fn test_from_binary_name_codex_cli() {
        assert_eq!(
            AgentProvider::from_binary_name("codex-cli"),
            Some(AgentProvider::Codex)
        );
    }

    #[test]
    fn test_from_binary_name_gemini() {
        assert_eq!(
            AgentProvider::from_binary_name("gemini"),
            Some(AgentProvider::Gemini)
        );
    }

    #[test]
    fn test_from_binary_name_gemini_cli() {
        assert_eq!(
            AgentProvider::from_binary_name("gemini-cli"),
            Some(AgentProvider::Gemini)
        );
    }

    #[test]
    fn test_from_binary_name_case_insensitive() {
        assert_eq!(
            AgentProvider::from_binary_name("Claude"),
            Some(AgentProvider::Claude)
        );
        assert_eq!(
            AgentProvider::from_binary_name("CODEX"),
            Some(AgentProvider::Codex)
        );
    }

    #[test]
    fn test_from_binary_name_unknown() {
        assert_eq!(AgentProvider::from_binary_name("vim"), None);
    }

    #[test]
    fn test_from_binary_name_grok_cli() {
        assert_eq!(
            AgentProvider::from_binary_name("grok-cli"),
            Some(AgentProvider::Grok)
        );
    }

    #[test]
    fn test_from_binary_name_factory_droid() {
        assert_eq!(
            AgentProvider::from_binary_name("factory-droid"),
            Some(AgentProvider::Factory)
        );
    }

    #[test]
    fn test_from_binary_name_github_copilot() {
        assert_eq!(
            AgentProvider::from_binary_name("github-copilot"),
            Some(AgentProvider::GithubCopilot)
        );
    }

    // -------------------------------------------------------------------------
    // display_name
    // -------------------------------------------------------------------------

    #[test]
    fn test_display_name_known_variants() {
        assert_eq!(AgentProvider::Claude.display_name(), "Claude Code");
        assert_eq!(AgentProvider::Codex.display_name(), "Codex");
        assert_eq!(AgentProvider::Gemini.display_name(), "Gemini");
        assert_eq!(AgentProvider::Cursor.display_name(), "Cursor");
        assert_eq!(AgentProvider::Windsurf.display_name(), "Windsurf");
        assert_eq!(
            AgentProvider::GithubCopilot.display_name(),
            "GitHub Copilot"
        );
        assert_eq!(AgentProvider::Grok.display_name(), "Grok");
        assert_eq!(AgentProvider::Aider.display_name(), "Aider");
        assert_eq!(AgentProvider::Devin.display_name(), "Devin");
        assert_eq!(AgentProvider::Factory.display_name(), "Factory");
        assert_eq!(AgentProvider::Opencode.display_name(), "OpenCode");
        assert_eq!(AgentProvider::Cline.display_name(), "Cline");
    }

    #[test]
    fn test_display_name_unknown_preserves_original() {
        let provider = AgentProvider::Unknown("my-custom-agent".to_string());
        assert_eq!(provider.display_name(), "my-custom-agent");
    }

    // -------------------------------------------------------------------------
    // canonical_slug
    // -------------------------------------------------------------------------

    #[test]
    fn test_canonical_slug_all_known() {
        assert_eq!(AgentProvider::Claude.canonical_slug(), "claude");
        assert_eq!(AgentProvider::Codex.canonical_slug(), "codex");
        assert_eq!(AgentProvider::Gemini.canonical_slug(), "gemini");
        assert_eq!(AgentProvider::Cursor.canonical_slug(), "cursor");
        assert_eq!(AgentProvider::Windsurf.canonical_slug(), "windsurf");
        assert_eq!(
            AgentProvider::GithubCopilot.canonical_slug(),
            "github-copilot"
        );
        assert_eq!(AgentProvider::Grok.canonical_slug(), "grok");
        assert_eq!(AgentProvider::Aider.canonical_slug(), "aider");
        assert_eq!(AgentProvider::Devin.canonical_slug(), "devin");
        assert_eq!(AgentProvider::Factory.canonical_slug(), "factory");
        assert_eq!(AgentProvider::Opencode.canonical_slug(), "opencode");
        assert_eq!(AgentProvider::Cline.canonical_slug(), "cline");
    }

    #[test]
    fn test_canonical_slug_unknown() {
        let provider = AgentProvider::Unknown("custom".to_string());
        assert_eq!(provider.canonical_slug(), "custom");
    }

    #[test]
    fn test_canonical_name_aliases_canonical_slug() {
        let known = AgentProvider::Codex;
        assert_eq!(known.canonical_name(), known.canonical_slug());

        let unknown = AgentProvider::Unknown("custom-provider".to_string());
        assert_eq!(unknown.canonical_name(), "custom-provider");
    }

    // -------------------------------------------------------------------------
    // from_slug
    // -------------------------------------------------------------------------

    #[test]
    fn test_from_slug_canonical() {
        assert_eq!(AgentProvider::from_slug("claude"), AgentProvider::Claude);
        assert_eq!(AgentProvider::from_slug("codex"), AgentProvider::Codex);
        assert_eq!(AgentProvider::from_slug("gemini"), AgentProvider::Gemini);
    }

    #[test]
    fn test_from_slug_aliases() {
        assert_eq!(
            AgentProvider::from_slug("claude-code"),
            AgentProvider::Claude
        );
        assert_eq!(
            AgentProvider::from_slug("claude_code"),
            AgentProvider::Claude
        );
        assert_eq!(AgentProvider::from_slug("codex-cli"), AgentProvider::Codex);
        assert_eq!(
            AgentProvider::from_slug("gemini-cli"),
            AgentProvider::Gemini
        );
        assert_eq!(
            AgentProvider::from_slug("copilot"),
            AgentProvider::GithubCopilot
        );
        assert_eq!(
            AgentProvider::from_slug("gh-copilot"),
            AgentProvider::GithubCopilot
        );
        assert_eq!(
            AgentProvider::from_slug("factory-droid"),
            AgentProvider::Factory
        );
        assert_eq!(
            AgentProvider::from_slug("open-code"),
            AgentProvider::Opencode
        );
        assert_eq!(AgentProvider::from_slug("grok-cli"), AgentProvider::Grok);
    }

    #[test]
    fn test_from_slug_case_insensitive() {
        assert_eq!(AgentProvider::from_slug("Claude"), AgentProvider::Claude);
        assert_eq!(AgentProvider::from_slug("CODEX"), AgentProvider::Codex);
        assert_eq!(
            AgentProvider::from_slug("Gemini-CLI"),
            AgentProvider::Gemini
        );
    }

    #[test]
    fn test_from_slug_unknown() {
        let provider = AgentProvider::from_slug("some-new-agent");
        assert_eq!(
            provider,
            AgentProvider::Unknown("some-new-agent".to_string())
        );
    }

    // -------------------------------------------------------------------------
    // from_agent_type / to_agent_type roundtrip
    // -------------------------------------------------------------------------

    #[test]
    fn test_from_agent_type_claude_code() {
        let provider = AgentProvider::from_agent_type(&crate::patterns::AgentType::ClaudeCode);
        assert_eq!(provider, AgentProvider::Claude);
    }

    #[test]
    fn test_from_agent_type_codex() {
        let provider = AgentProvider::from_agent_type(&crate::patterns::AgentType::Codex);
        assert_eq!(provider, AgentProvider::Codex);
    }

    #[test]
    fn test_from_agent_type_gemini() {
        let provider = AgentProvider::from_agent_type(&crate::patterns::AgentType::Gemini);
        assert_eq!(provider, AgentProvider::Gemini);
    }

    #[test]
    fn test_from_agent_type_wezterm() {
        let provider = AgentProvider::from_agent_type(&crate::patterns::AgentType::Wezterm);
        assert_eq!(provider, AgentProvider::Unknown("wezterm".to_string()));
    }

    #[test]
    fn test_from_agent_type_unknown() {
        let provider = AgentProvider::from_agent_type(&crate::patterns::AgentType::Unknown);
        assert_eq!(provider, AgentProvider::Unknown("unknown".to_string()));
    }

    #[test]
    fn test_to_agent_type_roundtrip() {
        assert_eq!(
            AgentProvider::Claude.to_agent_type(),
            crate::patterns::AgentType::ClaudeCode
        );
        assert_eq!(
            AgentProvider::Codex.to_agent_type(),
            crate::patterns::AgentType::Codex
        );
        assert_eq!(
            AgentProvider::Gemini.to_agent_type(),
            crate::patterns::AgentType::Gemini
        );
        assert_eq!(
            AgentProvider::Cursor.to_agent_type(),
            crate::patterns::AgentType::Unknown
        );
        assert_eq!(
            AgentProvider::Windsurf.to_agent_type(),
            crate::patterns::AgentType::Unknown
        );
    }

    // -------------------------------------------------------------------------
    // serde roundtrip
    // -------------------------------------------------------------------------

    #[test]
    fn test_serde_roundtrip_all_known() {
        for provider in AgentProvider::all_known() {
            let json = serde_json::to_string(provider).expect("serialize");
            let back: AgentProvider = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(
                &back,
                provider,
                "roundtrip failed for {}",
                provider.canonical_slug()
            );
        }
    }

    #[test]
    fn test_serde_roundtrip_unknown() {
        let provider = AgentProvider::Unknown("my-agent".to_string());
        let json = serde_json::to_string(&provider).expect("serialize");
        let back: AgentProvider = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, provider);
    }

    #[test]
    fn test_serde_uses_kebab_case() {
        let json = serde_json::to_string(&AgentProvider::GithubCopilot).expect("serialize");
        assert_eq!(json, "\"github-copilot\"");
    }

    #[test]
    fn test_serde_claude_is_lowercase() {
        let json = serde_json::to_string(&AgentProvider::Claude).expect("serialize");
        assert_eq!(json, "\"claude\"");
    }

    // -------------------------------------------------------------------------
    // Display
    // -------------------------------------------------------------------------

    #[test]
    fn test_display_matches_canonical_slug() {
        for provider in AgentProvider::all_known() {
            assert_eq!(format!("{}", provider), provider.canonical_slug());
        }
    }

    #[test]
    fn test_display_unknown() {
        let provider = AgentProvider::Unknown("foo".to_string());
        assert_eq!(format!("{}", provider), "foo");
    }

    // -------------------------------------------------------------------------
    // all_known
    // -------------------------------------------------------------------------

    #[test]
    fn test_all_known_contains_expected_count() {
        assert_eq!(AgentProvider::all_known().len(), 12);
    }

    #[test]
    fn test_all_known_no_duplicates() {
        let slugs: Vec<&str> = AgentProvider::all_known()
            .iter()
            .map(|p| p.canonical_slug())
            .collect();
        let mut deduped = slugs.clone();
        deduped.sort();
        deduped.dedup();
        assert_eq!(slugs.len(), deduped.len(), "duplicate slugs in all_known()");
    }

    // -------------------------------------------------------------------------
    // Edge cases
    // -------------------------------------------------------------------------

    #[test]
    fn test_from_process_name_substring_priority() {
        // "claude-code" should match Claude, not be ambiguous
        assert_eq!(
            AgentProvider::from_process_name("claude-code-v2"),
            Some(AgentProvider::Claude)
        );
    }

    #[test]
    fn test_from_slug_preserves_casing_in_unknown() {
        let provider = AgentProvider::from_slug("MyCustomAgent");
        assert_eq!(
            provider,
            AgentProvider::Unknown("MyCustomAgent".to_string())
        );
    }

    #[test]
    fn test_hash_and_eq_for_collections() {
        use std::collections::HashSet;
        let mut set = HashSet::new();
        set.insert(AgentProvider::Claude);
        set.insert(AgentProvider::Claude);
        assert_eq!(set.len(), 1);
        set.insert(AgentProvider::Codex);
        assert_eq!(set.len(), 2);
    }

    #[test]
    fn test_unknown_variants_differ() {
        assert_ne!(
            AgentProvider::Unknown("a".to_string()),
            AgentProvider::Unknown("b".to_string()),
        );
    }

    #[test]
    fn test_clone_equality() {
        let original = AgentProvider::Claude;
        let cloned = original.clone();
        assert_eq!(original, cloned);
    }

    // -------------------------------------------------------------------------
    // Structured diagnostics
    // -------------------------------------------------------------------------

    #[test]
    fn test_diagnostics_from_process_name_unknown() {
        let diag = AgentProvider::diagnostics_from_process_name("bash");
        assert_eq!(diag.source, ProviderResolutionSource::ProcessName);
        assert_eq!(diag.input, "bash");
        assert_eq!(diag.normalized, "bash");
        assert!(diag.candidates.is_empty());
        assert_eq!(diag.selected, None);
        assert!(!diag.ambiguous);
        assert!(diag.is_unknown());
    }

    #[test]
    fn test_diagnostics_from_process_name_ambiguous() {
        let diag = AgentProvider::diagnostics_from_process_name("claude-codex bridge");
        assert_eq!(
            diag.candidates,
            vec![AgentProvider::Claude, AgentProvider::Codex]
        );
        assert_eq!(diag.selected, Some(AgentProvider::Claude));
        assert!(diag.ambiguous);
        assert!(!diag.is_unknown());
    }

    #[test]
    fn test_diagnostics_from_binary_name_known() {
        let diag = AgentProvider::diagnostics_from_binary_name("codex-cli");
        assert_eq!(diag.source, ProviderResolutionSource::BinaryName);
        assert_eq!(diag.selected, Some(AgentProvider::Codex));
        assert_eq!(diag.candidates, vec![AgentProvider::Codex]);
        assert!(!diag.ambiguous);
    }

    #[test]
    fn test_diagnostics_from_slug_unknown() {
        let diag = AgentProvider::diagnostics_from_slug("x-new-agent");
        assert_eq!(diag.source, ProviderResolutionSource::Slug);
        assert!(diag.candidates.is_empty());
        assert_eq!(
            diag.selected,
            Some(AgentProvider::Unknown("x-new-agent".to_string()))
        );
        assert!(diag.is_unknown());
    }

    #[test]
    fn test_diagnostics_from_agent_type_known() {
        let diag =
            AgentProvider::diagnostics_from_agent_type(&crate::patterns::AgentType::ClaudeCode);
        assert_eq!(diag.source, ProviderResolutionSource::AgentType);
        assert_eq!(diag.input, "claude_code");
        assert_eq!(diag.selected, Some(AgentProvider::Claude));
        assert_eq!(diag.candidates, vec![AgentProvider::Claude]);
        assert!(!diag.ambiguous);
        assert!(!diag.is_unknown());
    }

    #[test]
    fn test_diagnostics_from_agent_type_unknown() {
        let diag = AgentProvider::diagnostics_from_agent_type(&crate::patterns::AgentType::Unknown);
        assert_eq!(diag.source, ProviderResolutionSource::AgentType);
        assert_eq!(diag.input, "unknown");
        assert_eq!(
            diag.selected,
            Some(AgentProvider::Unknown("unknown".to_string()))
        );
        assert!(diag.candidates.is_empty());
        assert!(diag.is_unknown());
    }
}
