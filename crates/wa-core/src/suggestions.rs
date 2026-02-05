//! Context-aware suggestion system for actionable error messages.
//!
//! Provides intelligent suggestions based on current system state:
//! - Typo detection using Levenshtein distance
//! - Available resources display
//! - Platform-specific command suggestions
//! - Recent state hints for temporal context

use std::fmt;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::accounts::AccountRecord;
use crate::storage::StoredEvent;

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
    #[must_use]
    pub fn new(id: u64) -> Self {
        Self {
            id,
            title: None,
            domain: None,
            is_alt_screen: false,
        }
    }

    /// Set the title.
    #[must_use]
    pub fn with_title(mut self, title: impl Into<String>) -> Self {
        self.title = Some(title.into());
        self
    }

    /// Set the domain.
    #[must_use]
    pub fn with_domain(mut self, domain: impl Into<String>) -> Self {
        self.domain = Some(domain.into());
        self
    }

    /// Set alt screen state.
    #[must_use]
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
    pub timestamp: SystemTime,
}

impl StateChange {
    /// Create a new state change.
    #[must_use]
    pub fn new(pane_id: u64, description: impl Into<String>) -> Self {
        Self {
            pane_id,
            description: description.into(),
            timestamp: SystemTime::now(),
        }
    }

    /// Create with explicit timestamp.
    #[must_use]
    pub fn with_timestamp(mut self, ts: SystemTime) -> Self {
        self.timestamp = ts;
        self
    }

    /// Get human-readable time ago.
    #[must_use]
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

/// User history for detecting first-time or unused feature suggestions.
#[derive(Debug, Clone, Default)]
pub struct UserHistory {
    pub used_commands: Vec<String>,
    pub used_features: Vec<String>,
    pub last_workflow_at: Option<SystemTime>,
}

impl UserHistory {
    /// Record a command invocation.
    pub fn record_command(&mut self, command: impl Into<String>) {
        self.used_commands.push(command.into());
    }

    /// Record a feature usage.
    pub fn record_feature(&mut self, feature: impl Into<String>) {
        self.used_features.push(feature.into());
    }

    /// Returns true if a command substring has been used.
    #[must_use]
    pub fn has_used_command(&self, needle: &str) -> bool {
        self.used_commands.iter().any(|c| c.contains(needle))
    }

    /// Returns true if a feature has been used.
    #[must_use]
    pub fn has_used_feature(&self, feature: &str) -> bool {
        self.used_features.iter().any(|f| f == feature)
    }
}

/// A feature hint for unused feature suggestions.
#[derive(Debug, Clone)]
pub struct FeatureHint {
    pub feature: String,
    pub message: String,
    pub command: String,
    pub used: bool,
    pub learn_more: Option<String>,
}

impl FeatureHint {
    /// Create a new feature hint.
    #[must_use]
    pub fn new(
        feature: impl Into<String>,
        message: impl Into<String>,
        command: impl Into<String>,
    ) -> Self {
        Self {
            feature: feature.into(),
            message: message.into(),
            command: command.into(),
            used: false,
            learn_more: None,
        }
    }

    /// Mark the feature as used.
    #[must_use]
    pub fn with_used(mut self, used: bool) -> Self {
        self.used = used;
        self
    }

    /// Add a learn-more link.
    #[must_use]
    pub fn with_learn_more(mut self, url: impl Into<String>) -> Self {
        self.learn_more = Some(url.into());
        self
    }
}

/// System metrics for optimization suggestions.
#[derive(Debug, Clone, Default)]
pub struct SystemMetrics {
    pub poll_interval_ms: Option<u64>,
    pub storage_size_bytes: Option<u64>,
    pub enabled_pattern_packs: Vec<String>,
    pub unused_pattern_packs: Vec<String>,
}

impl SystemMetrics {
    /// Set poll interval in milliseconds.
    #[must_use]
    pub fn with_poll_interval_ms(mut self, poll_interval_ms: u64) -> Self {
        self.poll_interval_ms = Some(poll_interval_ms);
        self
    }

    /// Set storage size in bytes.
    #[must_use]
    pub fn with_storage_size_bytes(mut self, size_bytes: u64) -> Self {
        self.storage_size_bytes = Some(size_bytes);
        self
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
    #[must_use]
    pub fn detect() -> Self {
        // Check for container first
        if std::path::Path::new("/.dockerenv").exists()
            || std::env::var("container").is_ok()
            || std::env::var("KUBERNETES_SERVICE_HOST").is_ok()
        {
            return Self::Container;
        }

        #[cfg(target_os = "macos")]
        {
            Self::MacOS
        }
        #[cfg(target_os = "linux")]
        {
            Self::Linux
        }
        #[cfg(target_os = "windows")]
        {
            Self::Windows
        }
        #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
        {
            Self::Unknown
        }
    }

    /// Get package manager command for this platform.
    #[must_use]
    pub fn package_manager(&self) -> Option<&'static str> {
        match self {
            Self::MacOS => Some("brew"),
            Self::Linux => {
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
            Self::Windows => Some("winget"),
            Self::Container => Some("apt"), // Most containers are Debian-based
            Self::Unknown => None,
        }
    }

    /// Get install command prefix for a package.
    #[must_use]
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
            Self::MacOS => "macOS",
            Self::Linux => "Linux",
            Self::Windows => "Windows",
            Self::Container => "Container",
            Self::Unknown => "Unknown",
        };
        write!(f, "{name}")
    }
}

/// Context for generating intelligent suggestions.
#[derive(Debug, Clone, Default)]
pub struct SuggestionContext {
    /// Available panes in the system.
    pub available_panes: Vec<PaneInfo>,
    /// Accounts available for usage-aware suggestions.
    pub accounts: Vec<AccountRecord>,
    /// Available workflow names.
    pub available_workflows: Vec<String>,
    /// Active workflow names (currently running).
    pub active_workflows: Vec<String>,
    /// Available rule IDs.
    pub available_rules: Vec<String>,
    /// Detected platform.
    pub platform: Platform,
    /// Recent state changes for temporal hints.
    pub recent_state: Vec<StateChange>,
    /// Recent stored events for rate-limit/error suggestions.
    pub recent_events: Vec<StoredEvent>,
    /// User history for first-time/unused feature hints.
    pub user_history: UserHistory,
    /// Feature hints for unused feature suggestions.
    pub feature_hints: Vec<FeatureHint>,
    /// System metrics for optimization suggestions.
    pub system_metrics: SystemMetrics,
}

impl Default for Platform {
    fn default() -> Self {
        Self::detect()
    }
}

impl SuggestionContext {
    /// Create a new empty context.
    #[must_use]
    pub fn new() -> Self {
        Self {
            platform: Platform::detect(),
            user_history: UserHistory::default(),
            system_metrics: SystemMetrics::default(),
            ..Default::default()
        }
    }

    /// Add a pane to available panes.
    pub fn add_pane(&mut self, pane: PaneInfo) {
        self.available_panes.push(pane);
    }

    /// Add an account record.
    pub fn add_account(&mut self, account: AccountRecord) {
        self.accounts.push(account);
    }

    /// Add a workflow name.
    pub fn add_workflow(&mut self, name: impl Into<String>) {
        self.available_workflows.push(name.into());
    }

    /// Add an active workflow name.
    pub fn add_active_workflow(&mut self, name: impl Into<String>) {
        self.active_workflows.push(name.into());
    }

    /// Add a rule ID.
    pub fn add_rule(&mut self, rule_id: impl Into<String>) {
        self.available_rules.push(rule_id.into());
    }

    /// Add a state change.
    pub fn add_state_change(&mut self, change: StateChange) {
        self.recent_state.push(change);
    }

    /// Add a recent stored event.
    pub fn add_recent_event(&mut self, event: StoredEvent) {
        self.recent_events.push(event);
    }

    /// Record a command usage in history.
    pub fn record_command(&mut self, command: impl Into<String>) {
        self.user_history.record_command(command);
    }

    /// Record a feature usage in history.
    pub fn record_feature(&mut self, feature: impl Into<String>) {
        self.user_history.record_feature(feature);
    }

    /// Add a feature hint for unused feature suggestions.
    pub fn add_feature_hint(&mut self, hint: FeatureHint) {
        self.feature_hints.push(hint);
    }

    /// Set system metrics for optimization suggestions.
    pub fn set_system_metrics(&mut self, metrics: SystemMetrics) {
        self.system_metrics = metrics;
    }

    /// Find closest matching pane ID.
    #[must_use]
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
    #[must_use]
    pub fn suggest_workflow(&self, input: &str) -> Option<&str> {
        suggest_closest(input, &self.available_workflows)
    }

    /// Find closest matching rule ID.
    #[must_use]
    pub fn suggest_rule(&self, input: &str) -> Option<&str> {
        suggest_closest(input, &self.available_rules)
    }

    /// Get recent state changes for a pane.
    #[must_use]
    pub fn recent_state_for_pane(&self, pane_id: u64) -> Vec<&StateChange> {
        self.recent_state
            .iter()
            .filter(|s| s.pane_id == pane_id)
            .collect()
    }

    /// Format available panes as a string.
    #[must_use]
    pub fn format_available_panes(&self) -> String {
        if self.available_panes.is_empty() {
            return "No panes available".to_string();
        }
        format_available(&self.available_panes)
    }

    /// Format available workflows as a string.
    #[must_use]
    pub fn format_available_workflows(&self) -> String {
        if self.available_workflows.is_empty() {
            return "No workflows available".to_string();
        }
        format_available(&self.available_workflows)
    }

    /// Format available rules as a string.
    #[must_use]
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
#[must_use]
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
            let cost = usize::from(a_char != b_char);

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
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(", ")
    } else {
        let shown: Vec<String> = items
            .iter()
            .take(MAX_SHOWN)
            .map(ToString::to_string)
            .collect();
        let remaining = items.len() - MAX_SHOWN;
        format!("{}, ... and {} more", shown.join(", "), remaining)
    }
}

/// Generate a "Did you mean?" suggestion for a pane not found error.
#[must_use]
pub fn pane_not_found_suggestion(requested: u64, ctx: &SuggestionContext) -> Option<String> {
    let closest = ctx.suggest_pane(requested)?;

    let mut suggestion = format!("Did you mean pane {closest}?");

    // Add available panes
    if !ctx.available_panes.is_empty() {
        suggestion.push_str(&format!("\nAvailable: {}", ctx.format_available_panes()));
    }

    Some(suggestion)
}

/// Generate a suggestion for a workflow not found error.
#[must_use]
pub fn workflow_not_found_suggestion(requested: &str, ctx: &SuggestionContext) -> Option<String> {
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
#[must_use]
pub fn rule_not_found_suggestion(requested: &str, ctx: &SuggestionContext) -> Option<String> {
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
#[must_use]
pub fn state_hint_for_pane(pane_id: u64, ctx: &SuggestionContext) -> Option<String> {
    let recent = ctx.recent_state_for_pane(pane_id);
    if recent.is_empty() {
        return None;
    }

    // Get most recent state change
    let latest = recent.iter().max_by_key(|s| s.timestamp)?;

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
#[must_use]
pub fn enhance_pane_not_found(
    mut remediation: Remediation,
    requested: u64,
    ctx: &SuggestionContext,
) -> Remediation {
    // Add closest match suggestion
    if let Some(closest) = ctx.suggest_pane(requested) {
        let suggestion = format!("Did you mean pane {closest}?");
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
#[must_use]
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
#[must_use]
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
#[must_use]
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

// ============================================================================
// Suggestion Engine - Rule-Based Proactive Recommendations
// ============================================================================

/// Priority levels for suggestions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub enum Priority {
    Low = 0,
    #[default]
    Medium = 1,
    High = 2,
    Critical = 3,
}

/// Types of suggestions the engine can generate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SuggestionType {
    /// What to do next
    NextStep,
    /// How to improve
    Optimization,
    /// Upcoming issue warning
    Warning,
    /// Useful feature discovery
    Tip,
    /// Error recovery guidance
    Recovery,
}

impl fmt::Display for SuggestionType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self {
            Self::NextStep => "Next Step",
            Self::Optimization => "Optimization",
            Self::Warning => "Warning",
            Self::Tip => "Tip",
            Self::Recovery => "Recovery",
        };
        write!(f, "{name}")
    }
}

/// An action that can be taken in response to a suggestion.
#[derive(Debug, Clone)]
pub struct SuggestedAction {
    /// Display label for the action
    pub label: String,
    /// Command to execute
    pub command: String,
}

impl SuggestedAction {
    /// Create a new suggested action.
    #[must_use]
    pub fn new(label: impl Into<String>, command: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            command: command.into(),
        }
    }
}

/// A unique identifier for a suggestion (for dismissal tracking).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SuggestionId(String);

impl SuggestionId {
    /// Create a new suggestion ID.
    #[must_use]
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    /// Get the ID as a string slice.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for SuggestionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// A proactive suggestion generated by the engine.
#[derive(Debug, Clone)]
pub struct Suggestion {
    /// Unique identifier for dismissal tracking
    pub id: SuggestionId,
    /// Type of suggestion
    pub suggestion_type: SuggestionType,
    /// Main message
    pub message: String,
    /// Optional action to take
    pub action: Option<SuggestedAction>,
    /// Optional link to documentation
    pub learn_more: Option<String>,
    /// Priority level
    pub priority: Priority,
    /// Whether user can dismiss this suggestion
    pub dismissable: bool,
    /// Rule that generated this suggestion
    pub rule_id: String,
}

impl Suggestion {
    /// Create a new suggestion.
    #[must_use]
    pub fn new(
        id: impl Into<String>,
        suggestion_type: SuggestionType,
        message: impl Into<String>,
        rule_id: impl Into<String>,
    ) -> Self {
        Self {
            id: SuggestionId::new(id),
            suggestion_type,
            message: message.into(),
            action: None,
            learn_more: None,
            priority: Priority::default(),
            dismissable: true,
            rule_id: rule_id.into(),
        }
    }

    /// Set the action for this suggestion.
    #[must_use]
    pub fn with_action(mut self, action: SuggestedAction) -> Self {
        self.action = Some(action);
        self
    }

    /// Set the learn more link.
    #[must_use]
    pub fn with_learn_more(mut self, url: impl Into<String>) -> Self {
        self.learn_more = Some(url.into());
        self
    }

    /// Set the priority.
    #[must_use]
    pub fn with_priority(mut self, priority: Priority) -> Self {
        self.priority = priority;
        self
    }

    /// Set whether this suggestion is dismissable.
    #[must_use]
    pub fn with_dismissable(mut self, dismissable: bool) -> Self {
        self.dismissable = dismissable;
        self
    }
}

/// Trait for suggestion rules.
///
/// Each rule checks if it applies to the current context and generates
/// a suggestion if applicable.
pub trait SuggestionRule: Send + Sync {
    /// Rule identifier.
    fn id(&self) -> &'static str;

    /// Check if this rule applies to the current context.
    fn applies(&self, ctx: &SuggestionContext) -> bool;

    /// Generate a suggestion if applicable.
    fn generate(&self, ctx: &SuggestionContext) -> Option<Suggestion>;

    /// Priority of this rule.
    fn priority(&self) -> Priority {
        Priority::Medium
    }

    /// Whether this rule is enabled.
    fn enabled(&self) -> bool {
        true
    }
}

/// Tracks dismissed suggestions with optional cooldown.
#[derive(Debug, Default)]
pub struct DismissedStore {
    /// Permanently dismissed suggestions
    permanent: std::collections::HashSet<SuggestionId>,
    /// Temporarily dismissed with expiry time
    temporary: std::collections::HashMap<SuggestionId, std::time::Instant>,
}

impl DismissedStore {
    /// Create a new empty dismissed store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Dismiss a suggestion permanently.
    pub fn dismiss_permanent(&mut self, id: &SuggestionId) {
        self.permanent.insert(id.clone());
        self.temporary.remove(id);
    }

    /// Dismiss a suggestion temporarily with a cooldown.
    pub fn dismiss_temporary(&mut self, id: &SuggestionId, cooldown: std::time::Duration) {
        let expiry = std::time::Instant::now() + cooldown;
        self.temporary.insert(id.clone(), expiry);
    }

    /// Check if a suggestion is currently dismissed.
    #[must_use]
    pub fn is_dismissed(&self, id: &SuggestionId) -> bool {
        if self.permanent.contains(id) {
            return true;
        }
        if let Some(expiry) = self.temporary.get(id) {
            if std::time::Instant::now() < *expiry {
                return true;
            }
        }
        false
    }

    /// Clean up expired temporary dismissals.
    pub fn cleanup_expired(&mut self) {
        let now = std::time::Instant::now();
        self.temporary.retain(|_, expiry| *expiry > now);
    }

    /// Get count of dismissed suggestions.
    #[must_use]
    pub fn count(&self) -> usize {
        self.permanent.len() + self.temporary.len()
    }
}

/// Configuration for the suggestion engine.
#[derive(Debug, Clone)]
pub struct SuggestionConfig {
    /// Whether suggestions are enabled
    pub enabled: bool,
    /// Maximum suggestions to return per context
    pub max_suggestions: usize,
    /// Default cooldown after dismissal
    pub dismiss_cooldown: std::time::Duration,
    /// Minimum priority threshold to include
    pub min_priority: Priority,
}

impl Default for SuggestionConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_suggestions: 3,
            dismiss_cooldown: std::time::Duration::from_secs(3600), // 1 hour
            min_priority: Priority::Low,
        }
    }
}

/// The suggestion engine evaluates rules and generates contextual suggestions.
pub struct SuggestionEngine {
    rules: Vec<Box<dyn SuggestionRule>>,
    dismissed: DismissedStore,
    config: SuggestionConfig,
}

impl Default for SuggestionEngine {
    fn default() -> Self {
        Self::new(SuggestionConfig::default())
    }
}

impl SuggestionEngine {
    /// Create a new suggestion engine with the given configuration.
    #[must_use]
    pub fn new(config: SuggestionConfig) -> Self {
        Self {
            rules: Self::default_rules(),
            dismissed: DismissedStore::new(),
            config,
        }
    }

    /// Create an engine with no default rules (for testing).
    #[must_use]
    pub fn empty(config: SuggestionConfig) -> Self {
        Self {
            rules: Vec::new(),
            dismissed: DismissedStore::new(),
            config,
        }
    }

    /// Add a custom rule to the engine.
    pub fn add_rule(&mut self, rule: Box<dyn SuggestionRule>) {
        self.rules.push(rule);
    }

    /// Generate suggestions for the given context.
    #[must_use]
    pub fn suggest(&self, ctx: &SuggestionContext) -> Vec<Suggestion> {
        if !self.config.enabled {
            return vec![];
        }

        let mut suggestions: Vec<Suggestion> = self
            .rules
            .iter()
            .filter(|r| r.enabled())
            .filter(|r| r.applies(ctx))
            .filter_map(|r| r.generate(ctx))
            .filter(|s| !self.dismissed.is_dismissed(&s.id))
            .filter(|s| s.priority >= self.config.min_priority)
            .collect();

        // Sort by priority (highest first)
        suggestions.sort_by_key(|s| std::cmp::Reverse(s.priority));

        // Limit to max
        suggestions.truncate(self.config.max_suggestions);

        suggestions
    }

    /// Dismiss a suggestion permanently.
    pub fn dismiss(&mut self, id: &SuggestionId) {
        self.dismissed.dismiss_permanent(id);
    }

    /// Dismiss a suggestion temporarily with the configured cooldown.
    pub fn dismiss_temporarily(&mut self, id: &SuggestionId) {
        self.dismissed
            .dismiss_temporary(id, self.config.dismiss_cooldown);
    }

    /// Dismiss a suggestion temporarily with a custom cooldown.
    pub fn dismiss_for(&mut self, id: &SuggestionId, duration: std::time::Duration) {
        self.dismissed.dismiss_temporary(id, duration);
    }

    /// Check if a suggestion is dismissed.
    #[must_use]
    pub fn is_dismissed(&self, id: &SuggestionId) -> bool {
        self.dismissed.is_dismissed(id)
    }

    /// Clean up expired dismissals.
    pub fn cleanup(&mut self) {
        self.dismissed.cleanup_expired();
    }

    /// Get the number of registered rules.
    #[must_use]
    pub fn rule_count(&self) -> usize {
        self.rules.len()
    }

    /// Get the current configuration.
    #[must_use]
    pub fn config(&self) -> &SuggestionConfig {
        &self.config
    }

    /// Update the configuration.
    pub fn set_config(&mut self, config: SuggestionConfig) {
        self.config = config;
    }

    /// Default built-in rules.
    fn default_rules() -> Vec<Box<dyn SuggestionRule>> {
        vec![
            Box::new(AltScreenWarningRule),
            Box::new(AccountLowRule::default()),
            Box::new(RateLimitFrequencyRule::default()),
            Box::new(ErrorRecoveryRule),
            Box::new(FirstWorkflowRule),
            Box::new(UnusedFeatureRule),
            Box::new(OptimizationRule),
            Box::new(NoPanesAvailableRule),
            Box::new(FirstTimeUserRule),
        ]
    }
}

// ============================================================================
// Built-in Rules
// ============================================================================

fn now_epoch_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

fn is_rate_limit_event(event: &StoredEvent) -> bool {
    let rule = event.rule_id.as_str();
    let event_type = event.event_type.as_str();
    event_type.contains("rate_limit")
        || rule.contains("rate_limit")
        || rule.contains("usage_reached")
}

fn is_error_event(event: &StoredEvent) -> bool {
    event.event_type.starts_with("error")
        || event.rule_id.contains("error")
        || event.severity.eq_ignore_ascii_case("critical")
}

fn error_code_from_event(event: &StoredEvent) -> String {
    if let Some(ref extracted) = event.extracted {
        if let Some(code) = extracted.get("code").and_then(|v| v.as_str()) {
            return code.to_string();
        }
    }
    if !event.rule_id.is_empty() {
        return event.rule_id.clone();
    }
    event.event_type.clone()
}

/// Warns when an active account is below a usage threshold.
#[derive(Debug, Clone)]
struct AccountLowRule {
    threshold_percent: f64,
}

impl Default for AccountLowRule {
    fn default() -> Self {
        Self {
            threshold_percent: 20.0,
        }
    }
}

impl SuggestionRule for AccountLowRule {
    fn id(&self) -> &'static str {
        "builtin.account_low"
    }

    fn applies(&self, ctx: &SuggestionContext) -> bool {
        ctx.accounts
            .iter()
            .any(|a| a.percent_remaining < self.threshold_percent)
    }

    fn generate(&self, ctx: &SuggestionContext) -> Option<Suggestion> {
        let low_account = ctx
            .accounts
            .iter()
            .filter(|a| a.percent_remaining < self.threshold_percent)
            .min_by(|a, b| {
                a.percent_remaining
                    .partial_cmp(&b.percent_remaining)
                    .unwrap_or(std::cmp::Ordering::Equal)
            });

        let account = low_account?;
        let name = account
            .name
            .as_deref()
            .unwrap_or(account.account_id.as_str());
        let percent = account.percent_remaining.round() as i64;

        Some(
            Suggestion::new(
                format!("account_low:{}", account.account_id),
                SuggestionType::Warning,
                format!(
                    "Account \"{name}\" is at {percent}% - consider switching before it runs out."
                ),
                self.id(),
            )
            .with_priority(Priority::High)
            .with_action(SuggestedAction::new("Switch account", "wa accounts switch")),
        )
    }

    fn priority(&self) -> Priority {
        Priority::High
    }
}

/// Warns when rate limits are frequent within the last hour.
#[derive(Debug, Clone)]
struct RateLimitFrequencyRule {
    max_hits_per_hour: u32,
}

impl Default for RateLimitFrequencyRule {
    fn default() -> Self {
        Self {
            max_hits_per_hour: 3,
        }
    }
}

impl SuggestionRule for RateLimitFrequencyRule {
    fn id(&self) -> &'static str {
        "builtin.rate_limit_frequency"
    }

    fn applies(&self, ctx: &SuggestionContext) -> bool {
        if ctx.recent_events.is_empty() {
            return false;
        }
        let now_ms = now_epoch_ms();
        let cutoff = now_ms.saturating_sub(60 * 60 * 1000);
        let count = ctx
            .recent_events
            .iter()
            .filter(|e| e.detected_at >= cutoff && is_rate_limit_event(e))
            .count();
        count as u32 > self.max_hits_per_hour
    }

    fn generate(&self, ctx: &SuggestionContext) -> Option<Suggestion> {
        let now_ms = now_epoch_ms();
        let cutoff = now_ms.saturating_sub(60 * 60 * 1000);
        let count = ctx
            .recent_events
            .iter()
            .filter(|e| e.detected_at >= cutoff && is_rate_limit_event(e))
            .count();

        Some(
            Suggestion::new(
                "rate_limit_frequency",
                SuggestionType::Optimization,
                format!(
                    "Frequent rate limits detected ({count} in the last hour). Consider slowing polling or pacing workflows."
                ),
                self.id(),
            )
            .with_priority(Priority::Medium)
            .with_action(SuggestedAction::new(
                "Adjust polling",
                "wa config set ingest.poll_interval_ms 200",
            )),
        )
    }

    fn priority(&self) -> Priority {
        Priority::Medium
    }
}

/// Suggests using dry-run when the first workflow is active.
struct FirstWorkflowRule;

impl SuggestionRule for FirstWorkflowRule {
    fn id(&self) -> &'static str {
        "builtin.first_workflow"
    }

    fn applies(&self, ctx: &SuggestionContext) -> bool {
        !ctx.active_workflows.is_empty() && !ctx.user_history.has_used_command("workflow")
    }

    fn generate(&self, ctx: &SuggestionContext) -> Option<Suggestion> {
        let workflow = ctx.active_workflows.first()?;
        Some(
            Suggestion::new(
                "first_workflow",
                SuggestionType::Tip,
                "This looks like your first automated workflow. Try a dry-run preview before executing.",
                self.id(),
            )
            .with_priority(Priority::Low)
            .with_action(SuggestedAction::new(
                "Preview workflow",
                format!("wa workflow run \"{workflow}\" --dry-run"),
            ))
            .with_dismissable(true),
        )
    }

    fn priority(&self) -> Priority {
        Priority::Low
    }
}

/// Suggests recovery steps when recent error events are detected.
struct ErrorRecoveryRule;

impl SuggestionRule for ErrorRecoveryRule {
    fn id(&self) -> &'static str {
        "builtin.error_recovery"
    }

    fn applies(&self, ctx: &SuggestionContext) -> bool {
        ctx.recent_events.iter().any(is_error_event)
    }

    fn generate(&self, ctx: &SuggestionContext) -> Option<Suggestion> {
        let event = ctx
            .recent_events
            .iter()
            .filter(|e| is_error_event(e))
            .max_by_key(|e| e.detected_at)?;
        let code = error_code_from_event(event);
        Some(
            Suggestion::new(
                format!("error_recovery:{code}"),
                SuggestionType::Recovery,
                format!("Error {code} occurred. Run `wa why {code}` to understand what happened and how to fix it."),
                self.id(),
            )
            .with_priority(Priority::High)
            .with_action(SuggestedAction::new("Explain error", format!("wa why {code}"))),
        )
    }

    fn priority(&self) -> Priority {
        Priority::High
    }
}

/// Suggests unused features based on available hints.
struct UnusedFeatureRule;

impl SuggestionRule for UnusedFeatureRule {
    fn id(&self) -> &'static str {
        "builtin.unused_feature"
    }

    fn applies(&self, ctx: &SuggestionContext) -> bool {
        ctx.feature_hints.iter().any(|h| !h.used)
    }

    fn generate(&self, ctx: &SuggestionContext) -> Option<Suggestion> {
        let hint = ctx.feature_hints.iter().find(|h| !h.used)?;
        let mut suggestion = Suggestion::new(
            format!("unused_feature:{}", hint.feature),
            SuggestionType::Tip,
            hint.message.clone(),
            self.id(),
        )
        .with_priority(Priority::Low)
        .with_action(SuggestedAction::new(
            format!("Try {}", hint.feature),
            hint.command.clone(),
        ))
        .with_dismissable(true);

        if let Some(ref url) = hint.learn_more {
            suggestion = suggestion.with_learn_more(url.clone());
        }

        Some(suggestion)
    }

    fn priority(&self) -> Priority {
        Priority::Low
    }
}

/// Suggests optimizations based on system metrics.
struct OptimizationRule;

impl SuggestionRule for OptimizationRule {
    fn id(&self) -> &'static str {
        "builtin.optimization"
    }

    fn applies(&self, ctx: &SuggestionContext) -> bool {
        let metrics = &ctx.system_metrics;
        let poll_too_fast = metrics.poll_interval_ms.is_some_and(|ms| ms < 100);
        let storage_large = metrics
            .storage_size_bytes
            .is_some_and(|bytes| bytes > 500 * 1024 * 1024);
        let unused_packs = !metrics.unused_pattern_packs.is_empty();
        poll_too_fast || storage_large || unused_packs
    }

    fn generate(&self, ctx: &SuggestionContext) -> Option<Suggestion> {
        let metrics = &ctx.system_metrics;
        if let Some(ms) = metrics.poll_interval_ms {
            if ms < 100 {
                return Some(
                    Suggestion::new(
                        "optimization.poll_interval",
                        SuggestionType::Optimization,
                        format!(
                            "Your poll interval is very low ({ms}ms). Increasing it can reduce CPU usage."
                        ),
                        self.id(),
                    )
                    .with_priority(Priority::Medium)
                    .with_action(SuggestedAction::new(
                        "Adjust polling",
                        "wa config set ingest.poll_interval_ms 200",
                    )),
                );
            }
        }

        if !metrics.unused_pattern_packs.is_empty() {
            let message = format!(
                "Some pattern packs appear unused: {}. Disabling them can reduce overhead.",
                metrics.unused_pattern_packs.join(", ")
            );
            return Some(
                Suggestion::new(
                    "optimization.unused_packs",
                    SuggestionType::Optimization,
                    message,
                    self.id(),
                )
                .with_priority(Priority::Low)
                .with_action(SuggestedAction::new(
                    "Review pattern packs",
                    "wa config show --effective",
                )),
            );
        }

        if let Some(bytes) = metrics.storage_size_bytes {
            if bytes > 500 * 1024 * 1024 {
                return Some(
                    Suggestion::new(
                        "optimization.storage_size",
                        SuggestionType::Optimization,
                        "Your wa database is large. Consider reducing retention days to save disk space.",
                        self.id(),
                    )
                    .with_priority(Priority::Low)
                    .with_action(SuggestedAction::new(
                        "Adjust retention",
                        "wa config set storage.retention_days 30",
                    )),
                );
            }
        }

        None
    }

    fn priority(&self) -> Priority {
        Priority::Medium
    }
}

/// Warns when a pane is in AltScreen mode.
struct AltScreenWarningRule;

impl SuggestionRule for AltScreenWarningRule {
    fn id(&self) -> &'static str {
        "builtin.alt_screen_warning"
    }

    fn applies(&self, ctx: &SuggestionContext) -> bool {
        ctx.available_panes.iter().any(|p| p.is_alt_screen)
    }

    fn generate(&self, ctx: &SuggestionContext) -> Option<Suggestion> {
        let alt_panes: Vec<_> = ctx
            .available_panes
            .iter()
            .filter(|p| p.is_alt_screen)
            .collect();

        if alt_panes.is_empty() {
            return None;
        }

        let pane_ids: Vec<String> = alt_panes.iter().map(|p| p.id.to_string()).collect();
        let message = if pane_ids.len() == 1 {
            format!(
                "Pane {} is in AltScreen mode (vim/less/etc). Send operations may be blocked.",
                pane_ids[0]
            )
        } else {
            format!(
                "Panes {} are in AltScreen mode. Send operations may be blocked.",
                pane_ids.join(", ")
            )
        };

        Some(
            Suggestion::new(
                format!("alt_screen:{}", pane_ids.join(",")),
                SuggestionType::Warning,
                message,
                self.id(),
            )
            .with_priority(Priority::High)
            .with_action(SuggestedAction::new(
                "Check pane state",
                format!("wa robot state --pane {}", pane_ids[0]),
            )),
        )
    }

    fn priority(&self) -> Priority {
        Priority::High
    }
}

/// Suggests help when no panes are available.
struct NoPanesAvailableRule;

impl SuggestionRule for NoPanesAvailableRule {
    fn id(&self) -> &'static str {
        "builtin.no_panes"
    }

    fn applies(&self, ctx: &SuggestionContext) -> bool {
        ctx.available_panes.is_empty()
    }

    fn generate(&self, _ctx: &SuggestionContext) -> Option<Suggestion> {
        Some(
            Suggestion::new(
                "no_panes",
                SuggestionType::Tip,
                "No WezTerm panes detected. Is WezTerm running?",
                self.id(),
            )
            .with_priority(Priority::Medium)
            .with_action(SuggestedAction::new(
                "Check WezTerm status",
                "wezterm cli list",
            ))
            .with_learn_more("https://wezfurlong.org/wezterm/"),
        )
    }

    fn priority(&self) -> Priority {
        Priority::Medium
    }
}

/// Provides tips for first-time users.
struct FirstTimeUserRule;

impl SuggestionRule for FirstTimeUserRule {
    fn id(&self) -> &'static str {
        "builtin.first_time_user"
    }

    fn applies(&self, ctx: &SuggestionContext) -> bool {
        // Apply when workflows are empty (likely first time)
        ctx.available_workflows.is_empty() && !ctx.available_panes.is_empty()
    }

    fn generate(&self, _ctx: &SuggestionContext) -> Option<Suggestion> {
        Some(
            Suggestion::new(
                "first_time_user",
                SuggestionType::Tip,
                "New to wa? Start with 'wa watch' to monitor your panes.",
                self.id(),
            )
            .with_priority(Priority::Low)
            .with_action(SuggestedAction::new(
                "Start watching",
                "wa watch --foreground",
            ))
            .with_dismissable(true),
        )
    }

    fn priority(&self) -> Priority {
        Priority::Low
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn make_event(
        event_type: &str,
        rule_id: &str,
        detected_at: i64,
        extracted: Option<serde_json::Value>,
    ) -> StoredEvent {
        StoredEvent {
            id: 1,
            pane_id: 1,
            rule_id: rule_id.to_string(),
            agent_type: "codex".to_string(),
            event_type: event_type.to_string(),
            severity: "info".to_string(),
            confidence: 0.9,
            extracted,
            matched_text: None,
            segment_id: None,
            detected_at,
            dedupe_key: None,
            handled_at: None,
            handled_by_workflow_id: None,
            handled_status: None,
        }
    }

    fn make_account(account_id: &str, percent: f64) -> AccountRecord {
        let now_ms = now_epoch_ms();
        AccountRecord {
            id: 1,
            account_id: account_id.to_string(),
            service: "openai".to_string(),
            name: Some(account_id.to_string()),
            percent_remaining: percent,
            reset_at: None,
            tokens_used: None,
            tokens_remaining: None,
            tokens_limit: None,
            last_refreshed_at: now_ms,
            last_used_at: None,
            created_at: now_ms,
            updated_at: now_ms,
        }
    }

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
            Platform::MacOS
                | Platform::Linux
                | Platform::Windows
                | Platform::Container
                | Platform::Unknown
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

    // ========================================================================
    // Suggestion Engine Tests
    // ========================================================================

    #[test]
    fn test_suggestion_engine_evaluates_rules() {
        let config = SuggestionConfig::default();
        let engine = SuggestionEngine::new(config);

        // Should have default rules
        assert!(engine.rule_count() > 0);

        // Empty context should trigger NoPanesAvailableRule
        let ctx = SuggestionContext::new();
        let suggestions = engine.suggest(&ctx);

        // Should get at least one suggestion
        assert!(!suggestions.is_empty());
    }

    #[test]
    fn test_suggestion_engine_disabled() {
        let config = SuggestionConfig {
            enabled: false,
            ..Default::default()
        };
        let engine = SuggestionEngine::new(config);

        let ctx = SuggestionContext::new();
        let suggestions = engine.suggest(&ctx);

        // Should be empty when disabled
        assert!(suggestions.is_empty());
    }

    #[test]
    fn test_suggestion_engine_dismissal() {
        let config = SuggestionConfig::default();
        let mut engine = SuggestionEngine::new(config);

        let ctx = SuggestionContext::new();
        let suggestions = engine.suggest(&ctx);
        assert!(!suggestions.is_empty());

        // Dismiss the first suggestion
        let first_id = suggestions[0].id.clone();
        engine.dismiss(&first_id);

        // Should be dismissed now
        assert!(engine.is_dismissed(&first_id));

        // Re-suggest should not include dismissed
        let suggestions_after = engine.suggest(&ctx);
        assert!(suggestions_after.iter().all(|s| s.id != first_id));
    }

    #[test]
    fn test_suggestion_engine_priority_ordering() {
        let config = SuggestionConfig {
            max_suggestions: 10,
            ..Default::default()
        };
        let engine = SuggestionEngine::new(config);

        // Create context with alt-screen pane (should trigger high priority warning)
        let mut ctx = SuggestionContext::new();
        ctx.add_pane(PaneInfo::new(1).with_alt_screen(true));

        let suggestions = engine.suggest(&ctx);

        // Should have suggestions, first one should be alt-screen warning (high priority)
        assert!(!suggestions.is_empty());

        // Verify ordering - higher priority should come first
        for i in 1..suggestions.len() {
            assert!(suggestions[i - 1].priority >= suggestions[i].priority);
        }
    }

    #[test]
    fn test_suggestion_engine_max_suggestions() {
        let config = SuggestionConfig {
            max_suggestions: 1,
            ..Default::default()
        };
        let engine = SuggestionEngine::new(config);

        let ctx = SuggestionContext::new();
        let suggestions = engine.suggest(&ctx);

        // Should be limited to 1
        assert!(suggestions.len() <= 1);
    }

    #[test]
    fn test_suggestion_engine_min_priority_filter() {
        let config = SuggestionConfig {
            min_priority: Priority::High,
            ..Default::default()
        };
        let engine = SuggestionEngine::new(config);

        let ctx = SuggestionContext::new();
        let suggestions = engine.suggest(&ctx);

        // All suggestions should be at least High priority
        for suggestion in &suggestions {
            assert!(suggestion.priority >= Priority::High);
        }
    }

    #[test]
    fn test_dismissed_store_permanent() {
        let mut store = DismissedStore::new();
        let id = SuggestionId::new("test_id");

        assert!(!store.is_dismissed(&id));

        store.dismiss_permanent(&id);
        assert!(store.is_dismissed(&id));
    }

    #[test]
    fn test_dismissed_store_temporary() {
        use std::time::Duration;

        let mut store = DismissedStore::new();
        let id = SuggestionId::new("test_id");

        // Dismiss for 1 second
        store.dismiss_temporary(&id, Duration::from_secs(1));
        assert!(store.is_dismissed(&id));

        // After cleanup (with no expired), should still be dismissed
        store.cleanup_expired();
        assert!(store.is_dismissed(&id));
    }

    #[test]
    fn test_alt_screen_warning_rule() {
        let rule = AltScreenWarningRule;

        // Should not apply when no panes
        let ctx = SuggestionContext::new();
        assert!(!rule.applies(&ctx));

        // Should not apply when panes are not in alt-screen
        let mut ctx = SuggestionContext::new();
        ctx.add_pane(PaneInfo::new(1));
        assert!(!rule.applies(&ctx));

        // Should apply when pane is in alt-screen
        let mut ctx = SuggestionContext::new();
        ctx.add_pane(PaneInfo::new(1).with_alt_screen(true));
        assert!(rule.applies(&ctx));

        let suggestion = rule.generate(&ctx);
        assert!(suggestion.is_some());
        let s = suggestion.unwrap();
        assert!(s.message.contains("AltScreen"));
        assert_eq!(s.suggestion_type, SuggestionType::Warning);
    }

    #[test]
    fn test_no_panes_rule() {
        let rule = NoPanesAvailableRule;

        // Should apply when no panes
        let ctx = SuggestionContext::new();
        assert!(rule.applies(&ctx));

        // Should not apply when panes exist
        let mut ctx = SuggestionContext::new();
        ctx.add_pane(PaneInfo::new(1));
        assert!(!rule.applies(&ctx));
    }

    #[test]
    fn test_first_time_user_rule() {
        let rule = FirstTimeUserRule;

        // Should not apply when no panes (NoPanesAvailableRule handles this)
        let ctx = SuggestionContext::new();
        assert!(!rule.applies(&ctx));

        // Should apply when panes exist but no workflows
        let mut ctx = SuggestionContext::new();
        ctx.add_pane(PaneInfo::new(1));
        assert!(rule.applies(&ctx));

        // Should not apply when workflows exist
        let mut ctx = SuggestionContext::new();
        ctx.add_pane(PaneInfo::new(1));
        ctx.add_workflow("handle_compaction");
        assert!(!rule.applies(&ctx));
    }

    #[test]
    fn test_suggestion_id_display() {
        let id = SuggestionId::new("test.suggestion");
        assert_eq!(format!("{id}"), "test.suggestion");
        assert_eq!(id.as_str(), "test.suggestion");
    }

    #[test]
    fn test_suggestion_builder() {
        let suggestion =
            Suggestion::new("test_id", SuggestionType::Tip, "Test message", "test.rule")
                .with_priority(Priority::High)
                .with_action(SuggestedAction::new("Do it", "wa do-it"))
                .with_learn_more("https://example.com")
                .with_dismissable(false);

        assert_eq!(suggestion.id.as_str(), "test_id");
        assert_eq!(suggestion.suggestion_type, SuggestionType::Tip);
        assert_eq!(suggestion.message, "Test message");
        assert_eq!(suggestion.rule_id, "test.rule");
        assert_eq!(suggestion.priority, Priority::High);
        assert!(suggestion.action.is_some());
        assert!(suggestion.learn_more.is_some());
        assert!(!suggestion.dismissable);
    }

    #[test]
    fn test_priority_ordering() {
        assert!(Priority::Critical > Priority::High);
        assert!(Priority::High > Priority::Medium);
        assert!(Priority::Medium > Priority::Low);
    }

    #[test]
    fn test_suggestion_type_display() {
        assert_eq!(format!("{}", SuggestionType::NextStep), "Next Step");
        assert_eq!(format!("{}", SuggestionType::Warning), "Warning");
        assert_eq!(format!("{}", SuggestionType::Recovery), "Recovery");
    }

    #[test]
    fn test_account_low_rule_triggers() {
        let rule = AccountLowRule::default();
        let mut ctx = SuggestionContext::new();
        ctx.add_account(make_account("acct-1", 10.0));

        assert!(rule.applies(&ctx));
        let suggestion = rule.generate(&ctx).expect("expected suggestion");
        assert_eq!(suggestion.suggestion_type, SuggestionType::Warning);
        assert!(suggestion.message.contains("10"));
        assert!(suggestion.action.is_some());
    }

    #[test]
    fn test_rate_limit_frequency_rule_triggers() {
        let rule = RateLimitFrequencyRule::default();
        let mut ctx = SuggestionContext::new();
        let now_ms = now_epoch_ms();

        for _ in 0..4 {
            ctx.add_recent_event(make_event(
                "rate_limit",
                "core.codex:rate_limit",
                now_ms,
                None,
            ));
        }

        assert!(rule.applies(&ctx));
        let suggestion = rule.generate(&ctx).expect("expected suggestion");
        assert_eq!(suggestion.suggestion_type, SuggestionType::Optimization);
    }

    #[test]
    fn test_first_workflow_rule_triggers() {
        let rule = FirstWorkflowRule;
        let mut ctx = SuggestionContext::new();
        ctx.add_active_workflow("handle_compaction");

        assert!(rule.applies(&ctx));
        let suggestion = rule.generate(&ctx).expect("expected suggestion");
        assert_eq!(suggestion.suggestion_type, SuggestionType::Tip);
    }

    #[test]
    fn test_error_recovery_rule_triggers() {
        let rule = ErrorRecoveryRule;
        let mut ctx = SuggestionContext::new();
        let now_ms = now_epoch_ms();
        ctx.add_recent_event(make_event(
            "error.timeout",
            "core.codex:error_timeout",
            now_ms,
            Some(json!({"code": "WA-4001"})),
        ));

        assert!(rule.applies(&ctx));
        let suggestion = rule.generate(&ctx).expect("expected suggestion");
        assert_eq!(suggestion.suggestion_type, SuggestionType::Recovery);
        assert!(suggestion.message.contains("WA-4001"));
    }

    #[test]
    fn test_unused_feature_rule_triggers() {
        let rule = UnusedFeatureRule;
        let mut ctx = SuggestionContext::new();
        ctx.add_feature_hint(
            FeatureHint::new(
                "wa search",
                "Try full-text search to find recent errors.",
                "wa search \"error\"",
            )
            .with_used(false),
        );

        assert!(rule.applies(&ctx));
        let suggestion = rule.generate(&ctx).expect("expected suggestion");
        assert_eq!(suggestion.suggestion_type, SuggestionType::Tip);
        assert!(suggestion.action.is_some());
    }

    #[test]
    fn test_optimization_rule_poll_interval() {
        let rule = OptimizationRule;
        let mut ctx = SuggestionContext::new();
        ctx.set_system_metrics(SystemMetrics::default().with_poll_interval_ms(50));

        assert!(rule.applies(&ctx));
        let suggestion = rule.generate(&ctx).expect("expected suggestion");
        assert_eq!(suggestion.suggestion_type, SuggestionType::Optimization);
    }
}
