//! Unified search query contract shared across CLI, robot mode, and MCP.
//!
//! This module defines canonical parameter defaults, validation rules, and
//! `SearchOptions` mapping so search semantics stay consistent across surfaces.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::storage::{SearchLint, SearchLintSeverity, SearchOptions, lint_fts_query};

/// Default result limit across search interfaces.
pub const SEARCH_LIMIT_DEFAULT: usize = 20;
/// Maximum allowed result limit across search interfaces.
pub const SEARCH_LIMIT_MAX: usize = 1000;
/// Default snippet token budget.
pub const SEARCH_SNIPPET_MAX_TOKENS: usize = 30;
/// Default snippet highlight prefix.
pub const SEARCH_HIGHLIGHT_PREFIX: &str = ">>";
/// Default snippet highlight suffix.
pub const SEARCH_HIGHLIGHT_SUFFIX: &str = "<<";

/// Canonical query mode selector.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum UnifiedSearchMode {
    /// Lexical FTS query.
    #[default]
    Lexical,
    /// Semantic embedding query.
    Semantic,
    /// Hybrid lexical + semantic query.
    Hybrid,
}

impl UnifiedSearchMode {
    /// Stable mode label for JSON payloads and docs.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Lexical => "lexical",
            Self::Semantic => "semantic",
            Self::Hybrid => "hybrid",
        }
    }
}

/// Surface-level defaults used while parsing.
#[derive(Debug, Clone, Copy)]
pub struct SearchQueryDefaults {
    pub limit: usize,
    pub snippets: bool,
    pub mode: UnifiedSearchMode,
    pub max_limit: usize,
}

impl Default for SearchQueryDefaults {
    fn default() -> Self {
        Self {
            limit: SEARCH_LIMIT_DEFAULT,
            snippets: true,
            mode: UnifiedSearchMode::Lexical,
            max_limit: SEARCH_LIMIT_MAX,
        }
    }
}

/// Raw query input from a caller surface.
#[derive(Debug, Clone, Default)]
pub struct SearchQueryInput {
    pub query: String,
    pub limit: Option<usize>,
    pub pane: Option<u64>,
    pub since: Option<i64>,
    pub until: Option<i64>,
    pub snippets: Option<bool>,
    pub mode: Option<UnifiedSearchMode>,
}

/// Canonical validated query parameters.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnifiedSearchQuery {
    pub query: String,
    pub limit: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pane: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub since: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub until: Option<i64>,
    pub snippets: bool,
    pub mode: UnifiedSearchMode,
}

/// Parse result with the canonical query and lint warnings.
#[derive(Debug, Clone)]
pub struct SearchQueryParseOutput {
    pub query: UnifiedSearchQuery,
    pub lints: Vec<SearchLint>,
}

/// Validation failures for unified query parsing.
#[derive(Debug, Clone)]
pub enum SearchQueryValidationError {
    InvalidLimit {
        provided: usize,
        max_limit: usize,
    },
    InvalidTimeRange {
        since: i64,
        until: i64,
    },
    InvalidQuery {
        lints: Vec<SearchLint>,
    },
    UnsupportedMode {
        mode: UnifiedSearchMode,
        supported: Vec<UnifiedSearchMode>,
    },
}

impl SearchQueryValidationError {
    /// Stable machine-readable error category.
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::InvalidLimit { .. } => "search.invalid_limit",
            Self::InvalidTimeRange { .. } => "search.invalid_time_range",
            Self::InvalidQuery { .. } => "search.invalid_query",
            Self::UnsupportedMode { .. } => "search.unsupported_mode",
        }
    }

    /// Human-readable summary.
    #[must_use]
    pub fn message(&self) -> String {
        match self {
            Self::InvalidLimit {
                provided,
                max_limit,
            } => format!(
                "Invalid search limit: {provided}. Limit must be between 1 and {max_limit}."
            ),
            Self::InvalidTimeRange { since, until } => format!(
                "Invalid time range: since ({since}) cannot be greater than until ({until})."
            ),
            Self::InvalidQuery { .. } => "Invalid search query.".to_string(),
            Self::UnsupportedMode { mode, supported } => {
                let supported = supported
                    .iter()
                    .map(|mode| mode.as_str())
                    .collect::<Vec<_>>()
                    .join(", ");
                format!(
                    "Search mode '{}' is not supported for this interface (supported: {}).",
                    mode.as_str(),
                    supported
                )
            }
        }
    }

    /// Optional actionable hint.
    #[must_use]
    pub fn hint(&self) -> Option<String> {
        match self {
            Self::InvalidLimit { max_limit, .. } => {
                Some(format!("Use --limit between 1 and {max_limit}."))
            }
            Self::InvalidTimeRange { .. } => {
                Some("Set --since <= --until (both epoch milliseconds).".to_string())
            }
            Self::InvalidQuery { lints } => format_lint_hint(lints),
            Self::UnsupportedMode { supported, .. } => {
                if supported.is_empty() {
                    None
                } else {
                    Some(format!(
                        "Try mode={}.",
                        supported
                            .iter()
                            .map(|mode| mode.as_str())
                            .collect::<Vec<_>>()
                            .join(" or ")
                    ))
                }
            }
        }
    }

    /// Lint findings when the query itself is invalid.
    #[must_use]
    pub fn lints(&self) -> Option<&[SearchLint]> {
        match self {
            Self::InvalidQuery { lints } => Some(lints),
            _ => None,
        }
    }

    /// Whether this error came from FTS query linting.
    #[must_use]
    pub const fn is_query_lint_error(&self) -> bool {
        matches!(self, Self::InvalidQuery { .. })
    }
}

impl fmt::Display for SearchQueryValidationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.message())
    }
}

impl std::error::Error for SearchQueryValidationError {}

/// Returns true when lint findings contain one or more hard errors.
#[must_use]
pub fn lints_have_errors(lints: &[SearchLint]) -> bool {
    lints
        .iter()
        .any(|lint| lint.severity == SearchLintSeverity::Error)
}

/// Build a compact hint from lint findings.
#[must_use]
pub fn format_lint_hint(lints: &[SearchLint]) -> Option<String> {
    let mut hint_lines = Vec::new();
    for lint in lints.iter().take(3) {
        let mut line = lint.message.clone();
        if let Some(suggestion) = &lint.suggestion {
            line.push_str(&format!(" (suggestion: {suggestion})"));
        }
        hint_lines.push(line);
    }
    if hint_lines.is_empty() {
        None
    } else {
        Some(hint_lines.join(" | "))
    }
}

/// Parse and validate a unified query contract.
pub fn parse_unified_search_query(
    input: SearchQueryInput,
    defaults: SearchQueryDefaults,
) -> std::result::Result<SearchQueryParseOutput, SearchQueryValidationError> {
    let query = input.query.trim().to_string();
    let limit = input.limit.unwrap_or(defaults.limit);
    if limit == 0 || limit > defaults.max_limit {
        return Err(SearchQueryValidationError::InvalidLimit {
            provided: limit,
            max_limit: defaults.max_limit,
        });
    }

    if let (Some(since), Some(until)) = (input.since, input.until)
        && since > until
    {
        return Err(SearchQueryValidationError::InvalidTimeRange { since, until });
    }

    let lints = lint_fts_query(&query);
    if lints_have_errors(&lints) {
        return Err(SearchQueryValidationError::InvalidQuery { lints });
    }

    Ok(SearchQueryParseOutput {
        query: UnifiedSearchQuery {
            query,
            limit,
            pane: input.pane,
            since: input.since,
            until: input.until,
            snippets: input.snippets.unwrap_or(defaults.snippets),
            mode: input.mode.unwrap_or(defaults.mode),
        },
        lints,
    })
}

/// Enforce that the selected mode is supported by a surface.
pub fn ensure_mode_supported(
    mode: UnifiedSearchMode,
    supported: &[UnifiedSearchMode],
) -> std::result::Result<(), SearchQueryValidationError> {
    if supported.contains(&mode) {
        return Ok(());
    }
    Err(SearchQueryValidationError::UnsupportedMode {
        mode,
        supported: supported.to_vec(),
    })
}

/// Convert canonical query params into storage search options.
#[must_use]
pub fn to_storage_search_options(query: &UnifiedSearchQuery) -> SearchOptions {
    SearchOptions {
        limit: Some(query.limit),
        pane_id: query.pane,
        since: query.since,
        until: query.until,
        include_snippets: Some(query.snippets),
        snippet_max_tokens: Some(SEARCH_SNIPPET_MAX_TOKENS),
        highlight_prefix: Some(SEARCH_HIGHLIGHT_PREFIX.to_string()),
        highlight_suffix: Some(SEARCH_HIGHLIGHT_SUFFIX.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_uses_defaults() {
        let parsed = parse_unified_search_query(
            SearchQueryInput {
                query: "error".to_string(),
                ..SearchQueryInput::default()
            },
            SearchQueryDefaults::default(),
        )
        .expect("parse query");

        assert_eq!(parsed.query.query, "error");
        assert_eq!(parsed.query.limit, SEARCH_LIMIT_DEFAULT);
        assert!(parsed.query.snippets);
        assert_eq!(parsed.query.mode, UnifiedSearchMode::Lexical);
        assert!(parsed.lints.is_empty());
    }

    #[test]
    fn parse_preserves_explicit_values() {
        let parsed = parse_unified_search_query(
            SearchQueryInput {
                query: "warning".to_string(),
                limit: Some(7),
                pane: Some(42),
                since: Some(100),
                until: Some(200),
                snippets: Some(false),
                mode: Some(UnifiedSearchMode::Hybrid),
            },
            SearchQueryDefaults::default(),
        )
        .expect("parse query");

        assert_eq!(parsed.query.limit, 7);
        assert_eq!(parsed.query.pane, Some(42));
        assert_eq!(parsed.query.since, Some(100));
        assert_eq!(parsed.query.until, Some(200));
        assert!(!parsed.query.snippets);
        assert_eq!(parsed.query.mode, UnifiedSearchMode::Hybrid);
    }

    #[test]
    fn parse_rejects_invalid_limit() {
        let err = parse_unified_search_query(
            SearchQueryInput {
                query: "error".to_string(),
                limit: Some(0),
                ..SearchQueryInput::default()
            },
            SearchQueryDefaults::default(),
        )
        .expect_err("limit=0 should fail");
        assert_eq!(err.code(), "search.invalid_limit");

        let err = parse_unified_search_query(
            SearchQueryInput {
                query: "error".to_string(),
                limit: Some(SEARCH_LIMIT_MAX + 1),
                ..SearchQueryInput::default()
            },
            SearchQueryDefaults::default(),
        )
        .expect_err("limit too large should fail");
        assert_eq!(err.code(), "search.invalid_limit");
    }

    #[test]
    fn parse_rejects_invalid_time_range() {
        let err = parse_unified_search_query(
            SearchQueryInput {
                query: "error".to_string(),
                since: Some(200),
                until: Some(100),
                ..SearchQueryInput::default()
            },
            SearchQueryDefaults::default(),
        )
        .expect_err("since > until should fail");
        assert_eq!(err.code(), "search.invalid_time_range");
    }

    #[test]
    fn parse_rejects_invalid_query_lints() {
        let err = parse_unified_search_query(
            SearchQueryInput {
                query: "AND error".to_string(),
                ..SearchQueryInput::default()
            },
            SearchQueryDefaults::default(),
        )
        .expect_err("invalid query should fail");
        assert!(err.is_query_lint_error());
        assert_eq!(err.code(), "search.invalid_query");
        assert!(err.lints().is_some());
    }

    #[test]
    fn parse_keeps_warning_lints() {
        let parsed = parse_unified_search_query(
            SearchQueryInput {
                query: "(error".to_string(),
                ..SearchQueryInput::default()
            },
            SearchQueryDefaults::default(),
        )
        .expect("warning-only query should pass");

        assert!(!parsed.lints.is_empty());
        assert!(!lints_have_errors(&parsed.lints));
    }

    #[test]
    fn mode_support_check() {
        let err = ensure_mode_supported(UnifiedSearchMode::Hybrid, &[UnifiedSearchMode::Lexical])
            .expect_err("hybrid should be unsupported");
        assert_eq!(err.code(), "search.unsupported_mode");
    }
}
