//! Context-aware suggestion system for actionable error messages.
//!
//! Provides intelligent suggestions based on current system state:
//! - Typo detection using Levenshtein distance
//! - Available resources display
//! - Platform-specific command suggestions
//! - Recent state hints for temporal context

use std::fmt;

/// Information about a pane for suggestions.
#[derive(Debug, Clone)]
pub struct PaneInfo {
    pub id: u64,
    pub title: Option<String>,
    pub domain: Option<String>,
    pub is_alt_screen: bool,
}

impl PaneInfo {
    /// Create a new PaneInfo.
    pub fn new(id: u64) -> Self {
        Self {
            id,
            title: None,
            domain: None,
            is_alt_screen: false,
        }
    }

    /// Set the title.
    pub fn with_title(mut self, title: impl Into<String>) -> Self {
        self.title = Some(title.into());
        self
    }

    /// Set the domain.
    pub fn with_domain(mut self, domain: impl Into<String>) -> Self {
        self.domain = Some(domain.into());
        self
    }

    /// Set alt screen state.
    pub fn with_alt_screen(mut self, is_alt: bool) -> Self {
        self.is_alt_screen = is_alt;
        self
    }
}

impl fmt::Display for PaneInfo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.id)?;
        if let Some(ref title) = self.title {
            write!(f, " ({title})")?;
        }
        Ok(())
    }
}

/// A recent state change for temporal context.
#[derive(Debug, Clone)]
pub struct StateChange {
    pub pane_id: u64,
    pub description: String,
    pub timestamp: std::time::SystemTime,
}

impl StateChange {
    /// Create a new state change.
    pub fn new(pane_id: u64, description: impl Into<String>) -> Self {
        Self {
            pane_id,
            description: description.into(),
            timestamp: std::time::SystemTime::now(),
        }
    }

    /// Create with explicit timestamp.
    pub fn with_timestamp(mut self, ts: std::time::SystemTime) -> Self {
        self.timestamp = ts;
        self
    }

    /// Get human-readable time ago.
    pub fn time_ago(&self) -> String {
        let Ok(elapsed) = self.timestamp.elapsed() else {
            return "just now".to_string();
        };

        let secs = elapsed.as_secs();
        if secs < 60 {
            format!("{secs} seconds ago")
        } else if secs < 3600 {
            let mins = secs / 60;
            if mins == 1 {
                "1 minute ago".to_string()
            } else {
                format!("{mins} minutes ago")
            }
        } else {
            let hours = secs / 3600;
            if hours == 1 {
                "1 hour ago".to_string()
            } else {
                format!("{hours} hours ago")
            }
        }
    }
}

/// Detected platform for environment-specific suggestions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Platform {
    MacOS,
    Linux,
    Windows,
    Container,
    Unknown,
}

impl Platform {
    /// Detect current platform.
    pub fn detect() -> Self {
        // Check for container first
        if std::path::Path::new("/.dockerenv").exists()
            || std::env::var("container").is_ok()
            || std::env::var("KUBERNETES_SERVICE_HOST").is_ok()
        {
            return Platform::Container;
        }

        #[cfg(target_os = "macos")]
        {
            Platform::MacOS
        }
        #[cfg(target_os = "linux")]
        {
            Platform::Linux
        }
        #[cfg(target_os = "windows")]
        {
            Platform::Windows
        }
        #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
        {
            Platform::Unknown
        }
    }

    /// Get package manager command for this platform.
    pub fn package_manager(&self) -> Option<&'static str> {
        match self {
            Platform::MacOS => Some("brew"),
            Platform::Linux => {
                // Try to detect specific distro
                if std::path::Path::new("/usr/bin/apt").exists() {
                    Some("apt")
                } else if std::path::Path::new("/usr/bin/dnf").exists() {
                    Some("dnf")
                } else if std::path::Path::new("/usr/bin/pacman").exists() {
                    Some("pacman")
                } else if std::path::Path::new("/usr/bin/apk").exists() {
                    Some("apk")
                } else {
                    None
                }
            }
            Platform::Windows => Some("winget"),
            Platform::Container => Some("apt"), // Most containers are Debian-based
            Platform::Unknown => None,
        }
    }

    /// Get install command prefix for a package.
    pub fn install_command(&self, package: &str) -> Option<String> {
        let pm = self.package_manager()?;
        let cmd = match pm {
            "brew" => format!("brew install {package}"),
            "apt" => format!("sudo apt install {package}"),
            "dnf" => format!("sudo dnf install {package}"),
            "pacman" => format!("sudo pacman -S {package}"),
            "apk" => format!("apk add {package}"),
            "winget" => format!("winget install {package}"),
            _ => return None,
        };
        Some(cmd)
    }
}

impl fmt::Display for Platform {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self {
            Platform::MacOS => "macOS",
            Platform::Linux => "Linux",
            Platform::Windows => "Windows",
            Platform::Container => "Container",
            Platform::Unknown => "Unknown",
        };
        write!(f, "{name}")
    }
}

/// Context for generating intelligent suggestions.
#[derive(Debug, Clone, Default)]
pub struct SuggestionContext {
    /// Available panes in the system.
    pub available_panes: Vec<PaneInfo>,
    /// Available workflow names.
    pub available_workflows: Vec<String>,
    /// Available rule IDs.
    pub available_rules: Vec<String>,
    /// Detected platform.
    pub platform: Platform,
    /// Recent state changes for temporal hints.
    pub recent_state: Vec<StateChange>,
}

impl Default for Platform {
    fn default() -> Self {
        Platform::detect()
    }
}

impl SuggestionContext {
    /// Create a new empty context.
    pub fn new() -> Self {
        Self {
            platform: Platform::detect(),
            ..Default::default()
        }
    }

    /// Add a pane to available panes.
    pub fn add_pane(&mut self, pane: PaneInfo) {
        self.available_panes.push(pane);
    }

    /// Add a workflow name.
    pub fn add_workflow(&mut self, name: impl Into<String>) {
        self.available_workflows.push(name.into());
    }

    /// Add a rule ID.
    pub fn add_rule(&mut self, rule_id: impl Into<String>) {
        self.available_rules.push(rule_id.into());
    }

    /// Add a state change.
    pub fn add_state_change(&mut self, change: StateChange) {
        self.recent_state.push(change);
    }

    /// Find closest matching pane ID.
    pub fn suggest_pane(&self, input: u64) -> Option<&PaneInfo> {
        if self.available_panes.is_empty() {
            return None;
        }

        // For numeric IDs, find the closest number
        let mut closest: Option<&PaneInfo> = None;
        let mut min_diff = u64::MAX;

        for pane in &self.available_panes {
            let diff = input.abs_diff(pane.id);
            if diff < min_diff {
                min_diff = diff;
                closest = Some(pane);
            }
        }

        closest
    }

    /// Find closest matching workflow name.
    pub fn suggest_workflow(&self, input: &str) -> Option<&str> {
        suggest_closest(input, &self.available_workflows)
    }

    /// Find closest matching rule ID.
    pub fn suggest_rule(&self, input: &str) -> Option<&str> {
        suggest_closest(input, &self.available_rules)
    }

    /// Get recent state changes for a pane.
    pub fn recent_state_for_pane(&self, pane_id: u64) -> Vec<&StateChange> {
        self.recent_state
            .iter()
            .filter(|s| s.pane_id == pane_id)
            .collect()
    }

    /// Format available panes as a string.
    pub fn format_available_panes(&self) -> String {
        if self.available_panes.is_empty() {
            return "No panes available".to_string();
        }
        format_available(&self.available_panes)
    }

    /// Format available workflows as a string.
    pub fn format_available_workflows(&self) -> String {
        if self.available_workflows.is_empty() {
            return "No workflows available".to_string();
        }
        format_available(&self.available_workflows)
    }

    /// Format available rules as a string.
    pub fn format_available_rules(&self) -> String {
        if self.available_rules.is_empty() {
            return "No rules available".to_string();
        }
        // Group by prefix for cleaner output
        let prefixes: std::collections::HashSet<&str> = self
            .available_rules
            .iter()
            .filter_map(|r| r.split('.').next())
            .collect();

        let mut grouped: Vec<String> = prefixes.iter().map(|p| format!("{p}.*")).collect();
        grouped.sort();
        grouped.join(", ")
    }
}

/// Calculate Levenshtein distance between two strings.
///
/// This is the minimum number of single-character edits (insertions,
/// deletions, or substitutions) required to change one string into the other.
pub fn levenshtein_distance(a: &str, b: &str) -> usize {
    let a_chars: Vec<char> = a.chars().collect();
    let b_chars: Vec<char> = b.chars().collect();
    let a_len = a_chars.len();
    let b_len = b_chars.len();

    // Quick exits
    if a_len == 0 {
        return b_len;
    }
    if b_len == 0 {
        return a_len;
    }

    // Use two rows instead of full matrix for memory efficiency
    let mut prev_row: Vec<usize> = (0..=b_len).collect();
    let mut curr_row: Vec<usize> = vec![0; b_len + 1];

    for (i, a_char) in a_chars.iter().enumerate() {
        curr_row[0] = i + 1;

        for (j, b_char) in b_chars.iter().enumerate() {
            let cost = if a_char == b_char { 0 } else { 1 };

            curr_row[j + 1] = (prev_row[j + 1] + 1) // deletion
                .min(curr_row[j] + 1) // insertion
                .min(prev_row[j] + cost); // substitution
        }

        std::mem::swap(&mut prev_row, &mut curr_row);
    }

    prev_row[b_len]
}

/// Find the closest matching string from candidates using Levenshtein distance.
///
/// Returns None if candidates is empty or if the best match is too distant
/// (more than 50% of the input length different, unless it's a prefix match).
pub fn suggest_closest<'a, T: AsRef<str>>(input: &str, candidates: &'a [T]) -> Option<&'a str> {
    if candidates.is_empty() {
        return None;
    }

    let input_lower = input.to_lowercase();
    let mut best_match: Option<&str> = None;
    let mut best_distance = usize::MAX;
    let mut best_is_prefix = false;

    for candidate in candidates {
        let candidate_str = candidate.as_ref();
        let candidate_lower = candidate_str.to_lowercase();

        // Check for exact match first (case-insensitive)
        if input_lower == candidate_lower {
            return Some(candidate_str);
        }

        // Check for prefix match - these are always good suggestions
        let is_prefix =
            candidate_lower.starts_with(&input_lower) || input_lower.starts_with(&candidate_lower);

        let distance = levenshtein_distance(&input_lower, &candidate_lower);

        // Prefer prefix matches, then by distance
        if is_prefix && !best_is_prefix {
            // First prefix match found, take it
            best_distance = distance;
            best_match = Some(candidate_str);
            best_is_prefix = true;
        } else if is_prefix && best_is_prefix && distance < best_distance {
            // Better prefix match
            best_distance = distance;
            best_match = Some(candidate_str);
        } else if !best_is_prefix && distance < best_distance {
            // No prefix match yet, take best distance
            best_distance = distance;
            best_match = Some(candidate_str);
        }
    }

    // Prefix matches always pass; others need to be within threshold
    if best_is_prefix {
        return best_match;
    }

    // Only suggest if the match is reasonable (within 50% of input length or 3 edits)
    let threshold = (input.len() / 2).max(3);
    if best_distance <= threshold {
        best_match
    } else {
        None
    }
}

/// Format a list of items for display.
///
/// Shows up to 10 items, then summarizes remaining count.
pub fn format_available<T: fmt::Display>(items: &[T]) -> String {
    const MAX_SHOWN: usize = 10;

    if items.is_empty() {
        return String::new();
    }

    if items.len() <= MAX_SHOWN {
        items
            .iter()
            .map(|i| i.to_string())
            .collect::<Vec<_>>()
            .join(", ")
    } else {
        let shown: Vec<String> = items.iter().take(MAX_SHOWN).map(|i| i.to_string()).collect();
        let remaining = items.len() - MAX_SHOWN;
        format!("{}, ... and {} more", shown.join(", "), remaining)
    }
}

/// Generate a "Did you mean?" suggestion for a pane not found error.
pub fn pane_not_found_suggestion(
    requested: u64,
    ctx: &SuggestionContext,
) -> Option<String> {
    let closest = ctx.suggest_pane(requested)?;

    let mut suggestion = format!(
        "Did you mean pane {}?",
        closest
    );

    // Add available panes
    if !ctx.available_panes.is_empty() {
        suggestion.push_str(&format!(
            "\nAvailable: {}",
            ctx.format_available_panes()
        ));
    }

    Some(suggestion)
}

/// Generate a suggestion for a workflow not found error.
pub fn workflow_not_found_suggestion(
    requested: &str,
    ctx: &SuggestionContext,
) -> Option<String> {
    let mut suggestion = String::new();

    if let Some(closest) = ctx.suggest_workflow(requested) {
        suggestion.push_str(&format!("Did you mean: {closest}?\n"));
    }

    if !ctx.available_workflows.is_empty() {
        suggestion.push_str(&format!(
            "Available workflows: {}",
            ctx.format_available_workflows()
        ));
    }

    if suggestion.is_empty() {
        None
    } else {
        Some(suggestion)
    }
}

/// Generate a suggestion for a rule not found error.
pub fn rule_not_found_suggestion(
    requested: &str,
    ctx: &SuggestionContext,
) -> Option<String> {
    let mut suggestion = String::new();

    if let Some(closest) = ctx.suggest_rule(requested) {
        suggestion.push_str(&format!("Did you mean: {closest}?\n"));
    }

    if !ctx.available_rules.is_empty() {
        suggestion.push_str(&format!(
            "Available rules: {}",
            ctx.format_available_rules()
        ));
    }

    if suggestion.is_empty() {
        None
    } else {
        Some(suggestion)
    }
}

/// Generate state hint for a pane operation that was blocked.
pub fn state_hint_for_pane(pane_id: u64, ctx: &SuggestionContext) -> Option<String> {
    let recent = ctx.recent_state_for_pane(pane_id);
    if recent.is_empty() {
        return None;
    }

    // Get most recent state change
    let latest = recent
        .iter()
        .max_by_key(|s| s.timestamp)?;

    Some(format!(
        "Hint: Pane {} {} {}",
        pane_id,
        latest.description,
        latest.time_ago()
    ))
}

// ============================================================================
// Remediation Integration
// ============================================================================

use crate::error::Remediation;

/// Enhance a remediation with context-aware suggestions for pane not found.
///
/// Adds "Did you mean?" suggestions and lists available panes.
pub fn enhance_pane_not_found(
    mut remediation: Remediation,
    requested: u64,
    ctx: &SuggestionContext,
) -> Remediation {
    // Add closest match suggestion
    if let Some(closest) = ctx.suggest_pane(requested) {
        let suggestion = format!(
            "Did you mean pane {}?",
            closest
        );
        remediation = remediation.alternative(suggestion);
    }

    // Add available panes
    if !ctx.available_panes.is_empty() {
        let available = format!("Available panes: {}", ctx.format_available_panes());
        remediation = remediation.alternative(available);
    }

    // Add state hint if the pane was recently closed or changed
    if let Some(hint) = state_hint_for_pane(requested, ctx) {
        remediation = remediation.alternative(hint);
    }

    remediation
}

/// Enhance a remediation with context-aware suggestions for workflow not found.
pub fn enhance_workflow_not_found(
    mut remediation: Remediation,
    requested: &str,
    ctx: &SuggestionContext,
) -> Remediation {
    // Add closest match suggestion
    if let Some(closest) = ctx.suggest_workflow(requested) {
        let suggestion = format!("Did you mean: {closest}?");
        remediation = remediation.alternative(suggestion);
    }

    // Add available workflows
    if !ctx.available_workflows.is_empty() {
        let available = format!("Available workflows: {}", ctx.format_available_workflows());
        remediation = remediation.alternative(available);
    }

    remediation
}

/// Enhance a remediation with context-aware suggestions for rule not found.
pub fn enhance_rule_not_found(
    mut remediation: Remediation,
    requested: &str,
    ctx: &SuggestionContext,
) -> Remediation {
    // Add closest match suggestion
    if let Some(closest) = ctx.suggest_rule(requested) {
        let suggestion = format!("Did you mean: {closest}?");
        remediation = remediation.alternative(suggestion);
    }

    // Add available rules
    if !ctx.available_rules.is_empty() {
        let available = format!("Available rules: {}", ctx.format_available_rules());
        remediation = remediation.alternative(available);
    }

    remediation
}

/// Enhance a remediation with platform-specific install commands.
pub fn enhance_with_platform_commands(
    mut remediation: Remediation,
    package: &str,
    ctx: &SuggestionContext,
) -> Remediation {
    if let Some(cmd) = ctx.platform.install_command(package) {
        let label = format!("Install {package}");
        let platform_name = ctx.platform.to_string();
        remediation = remediation.platform_command(label, cmd, platform_name);
    }
    remediation
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_levenshtein_distance_identical() {
        assert_eq!(levenshtein_distance("hello", "hello"), 0);
    }

    #[test]
    fn test_levenshtein_distance_empty() {
        assert_eq!(levenshtein_distance("", "hello"), 5);
        assert_eq!(levenshtein_distance("hello", ""), 5);
        assert_eq!(levenshtein_distance("", ""), 0);
    }

    #[test]
    fn test_levenshtein_distance_single_edit() {
        assert_eq!(levenshtein_distance("hello", "hallo"), 1); // substitution
        assert_eq!(levenshtein_distance("hello", "hell"), 1); // deletion
        assert_eq!(levenshtein_distance("hello", "helloo"), 1); // insertion
    }

    #[test]
    fn test_levenshtein_distance_multiple_edits() {
        assert_eq!(levenshtein_distance("kitten", "sitting"), 3);
        assert_eq!(levenshtein_distance("saturday", "sunday"), 3);
    }

    #[test]
    fn test_suggest_closest_exact_match() {
        let candidates = vec!["foo", "bar", "baz"];
        assert_eq!(suggest_closest("foo", &candidates), Some("foo"));
    }

    #[test]
    fn test_suggest_closest_case_insensitive() {
        let candidates = vec!["Foo", "Bar", "Baz"];
        assert_eq!(suggest_closest("FOO", &candidates), Some("Foo"));
    }

    #[test]
    fn test_suggest_closest_typo() {
        let candidates = vec!["handle_compaction", "handle_usage_limits", "run_tests"];
        assert_eq!(
            suggest_closest("handle_compactin", &candidates),
            Some("handle_compaction")
        );
    }

    #[test]
    fn test_suggest_closest_prefix() {
        let candidates = vec!["handle_compaction", "handle_usage_limits", "run_tests"];
        assert_eq!(
            suggest_closest("handle_c", &candidates),
            Some("handle_compaction")
        );
    }

    #[test]
    fn test_suggest_closest_too_different() {
        let candidates = vec!["alpha", "beta", "gamma"];
        assert_eq!(suggest_closest("completely_different", &candidates), None);
    }

    #[test]
    fn test_suggest_closest_empty_candidates() {
        let candidates: Vec<&str> = vec![];
        assert_eq!(suggest_closest("foo", &candidates), None);
    }

    #[test]
    fn test_format_available_short_list() {
        let items = vec!["a", "b", "c"];
        assert_eq!(format_available(&items), "a, b, c");
    }

    #[test]
    fn test_format_available_long_list() {
        let items: Vec<i32> = (1..=15).collect();
        let formatted = format_available(&items);
        assert!(formatted.contains("... and 5 more"));
    }

    #[test]
    fn test_format_available_empty() {
        let items: Vec<&str> = vec![];
        assert_eq!(format_available(&items), "");
    }

    #[test]
    fn test_pane_info_display() {
        let pane = PaneInfo::new(3).with_title("codex");
        assert_eq!(format!("{pane}"), "3 (codex)");
    }

    #[test]
    fn test_pane_info_display_no_title() {
        let pane = PaneInfo::new(5);
        assert_eq!(format!("{pane}"), "5");
    }

    #[test]
    fn test_suggestion_context_suggest_pane() {
        let mut ctx = SuggestionContext::new();
        ctx.add_pane(PaneInfo::new(1).with_title("claude"));
        ctx.add_pane(PaneInfo::new(5).with_title("codex"));
        ctx.add_pane(PaneInfo::new(10).with_title("gemini"));

        // Closest to 4 should be 5
        let suggested = ctx.suggest_pane(4);
        assert!(suggested.is_some());
        assert_eq!(suggested.unwrap().id, 5);

        // Closest to 8 should be 10
        let suggested = ctx.suggest_pane(8);
        assert!(suggested.is_some());
        assert_eq!(suggested.unwrap().id, 10);
    }

    #[test]
    fn test_suggestion_context_suggest_workflow() {
        let mut ctx = SuggestionContext::new();
        ctx.add_workflow("handle_compaction");
        ctx.add_workflow("handle_usage_limits");
        ctx.add_workflow("run_tests");

        assert_eq!(
            ctx.suggest_workflow("handle_compactin"),
            Some("handle_compaction")
        );
    }

    #[test]
    fn test_platform_detect() {
        let platform = Platform::detect();
        // Should detect something valid
        assert!(matches!(
            platform,
            Platform::MacOS | Platform::Linux | Platform::Windows | Platform::Container | Platform::Unknown
        ));
    }

    #[test]
    fn test_platform_install_command_linux() {
        // Test apt-based install command format
        let cmd = Platform::Linux.install_command("wezterm");
        // Will vary by system, but if apt exists it should work
        if std::path::Path::new("/usr/bin/apt").exists() {
            assert_eq!(cmd, Some("sudo apt install wezterm".to_string()));
        }
    }

    #[test]
    fn test_state_change_time_ago() {
        use std::time::Duration;

        let now = std::time::SystemTime::now();

        // Just now
        let change = StateChange::new(1, "test").with_timestamp(now);
        let ago = change.time_ago();
        assert!(ago.contains("second") || ago == "just now");

        // 2 minutes ago
        let two_mins_ago = now - Duration::from_secs(120);
        let change = StateChange::new(1, "test").with_timestamp(two_mins_ago);
        assert!(change.time_ago().contains("2 minutes"));

        // 1 hour ago
        let one_hour_ago = now - Duration::from_secs(3600);
        let change = StateChange::new(1, "test").with_timestamp(one_hour_ago);
        assert!(change.time_ago().contains("1 hour"));
    }

    #[test]
    fn test_pane_not_found_suggestion() {
        let mut ctx = SuggestionContext::new();
        ctx.add_pane(PaneInfo::new(1).with_title("claude"));
        ctx.add_pane(PaneInfo::new(3).with_title("codex"));
        ctx.add_pane(PaneInfo::new(7).with_title("gemini"));

        let suggestion = pane_not_found_suggestion(4, &ctx);
        assert!(suggestion.is_some());
        let text = suggestion.unwrap();
        assert!(text.contains("Did you mean pane 3"));
        assert!(text.contains("Available:"));
    }

    #[test]
    fn test_workflow_not_found_suggestion() {
        let mut ctx = SuggestionContext::new();
        ctx.add_workflow("handle_compaction");
        ctx.add_workflow("handle_usage_limits");

        let suggestion = workflow_not_found_suggestion("handle_compactin", &ctx);
        assert!(suggestion.is_some());
        let text = suggestion.unwrap();
        assert!(text.contains("Did you mean: handle_compaction"));
    }

    #[test]
    fn test_state_hint_for_pane() {
        use std::time::Duration;

        let mut ctx = SuggestionContext::new();
        let change = StateChange::new(3, "entered AltScreen (vim)")
            .with_timestamp(std::time::SystemTime::now() - Duration::from_secs(120));
        ctx.add_state_change(change);

        let hint = state_hint_for_pane(3, &ctx);
        assert!(hint.is_some());
        let text = hint.unwrap();
        assert!(text.contains("Pane 3"));
        assert!(text.contains("AltScreen"));
        assert!(text.contains("2 minutes ago"));
    }

    #[test]
    fn test_format_available_rules_grouped() {
        let mut ctx = SuggestionContext::new();
        ctx.add_rule("codex.usage.reached");
        ctx.add_rule("codex.usage.warning");
        ctx.add_rule("claude.rate.limited");
        ctx.add_rule("gemini.error");

        let formatted = ctx.format_available_rules();
        assert!(formatted.contains("codex.*"));
        assert!(formatted.contains("claude.*"));
        assert!(formatted.contains("gemini.*"));
    }

    // ========================================================================
    // Remediation Integration Tests
    // ========================================================================

    #[test]
    fn test_enhance_pane_not_found() {
        let mut ctx = SuggestionContext::new();
        ctx.add_pane(PaneInfo::new(1).with_title("claude"));
        ctx.add_pane(PaneInfo::new(3).with_title("codex"));
        ctx.add_pane(PaneInfo::new(7).with_title("gemini"));

        let remediation = Remediation::new("Pane not found");
        let enhanced = enhance_pane_not_found(remediation, 4, &ctx);

        // Should have alternatives for "did you mean" and "available panes"
        assert!(!enhanced.alternatives.is_empty());

        let alts_text = enhanced.alternatives.join(" ");
        assert!(alts_text.contains("Did you mean"));
        assert!(alts_text.contains("Available panes"));
    }

    #[test]
    fn test_enhance_workflow_not_found() {
        let mut ctx = SuggestionContext::new();
        ctx.add_workflow("handle_compaction");
        ctx.add_workflow("handle_usage_limits");

        let remediation = Remediation::new("Workflow not found");
        let enhanced = enhance_workflow_not_found(remediation, "handle_compactin", &ctx);

        assert!(!enhanced.alternatives.is_empty());
        let alts_text = enhanced.alternatives.join(" ");
        assert!(alts_text.contains("handle_compaction"));
    }

    #[test]
    fn test_enhance_rule_not_found() {
        let mut ctx = SuggestionContext::new();
        ctx.add_rule("codex.usage.reached");
        ctx.add_rule("claude.rate.limited");

        let remediation = Remediation::new("Rule not found");
        let enhanced = enhance_rule_not_found(remediation, "codex.usage.reched", &ctx);

        assert!(!enhanced.alternatives.is_empty());
        let alts_text = enhanced.alternatives.join(" ");
        assert!(alts_text.contains("codex.usage.reached"));
    }

    #[test]
    fn test_enhance_with_platform_commands() {
        let ctx = SuggestionContext::new();
        let remediation = Remediation::new("Package not found");
        let enhanced = enhance_with_platform_commands(remediation, "wezterm", &ctx);

        // Should have a platform-specific install command (on Linux/macOS)
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        {
            assert!(!enhanced.commands.is_empty());
            let cmd = &enhanced.commands[0];
            assert!(cmd.command.contains("wezterm"));
            assert!(cmd.platform.is_some());
        }
    }
}
