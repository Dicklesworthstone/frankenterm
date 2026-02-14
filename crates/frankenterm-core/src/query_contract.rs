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

    // ── UnifiedSearchMode ──────────────────────────────────────────────

    #[test]
    fn unified_search_mode_as_str_all_variants() {
        assert_eq!(UnifiedSearchMode::Lexical.as_str(), "lexical");
        assert_eq!(UnifiedSearchMode::Semantic.as_str(), "semantic");
        assert_eq!(UnifiedSearchMode::Hybrid.as_str(), "hybrid");
    }

    #[test]
    fn unified_search_mode_default_is_lexical() {
        assert_eq!(UnifiedSearchMode::default(), UnifiedSearchMode::Lexical);
    }

    #[test]
    fn unified_search_mode_serde_roundtrip() {
        for mode in [
            UnifiedSearchMode::Lexical,
            UnifiedSearchMode::Semantic,
            UnifiedSearchMode::Hybrid,
        ] {
            let json = serde_json::to_string(&mode).unwrap();
            let restored: UnifiedSearchMode = serde_json::from_str(&json).unwrap();
            assert_eq!(restored, mode);
        }
    }

    #[test]
    fn unified_search_mode_serde_snake_case() {
        let json = serde_json::to_string(&UnifiedSearchMode::Lexical).unwrap();
        assert_eq!(json, "\"lexical\"");
        let json = serde_json::to_string(&UnifiedSearchMode::Semantic).unwrap();
        assert_eq!(json, "\"semantic\"");
        let json = serde_json::to_string(&UnifiedSearchMode::Hybrid).unwrap();
        assert_eq!(json, "\"hybrid\"");
    }

    // ── SearchQueryDefaults ────────────────────────────────────────────

    #[test]
    fn search_query_defaults_values() {
        let defaults = SearchQueryDefaults::default();
        assert_eq!(defaults.limit, SEARCH_LIMIT_DEFAULT);
        assert!(defaults.snippets);
        assert_eq!(defaults.mode, UnifiedSearchMode::Lexical);
        assert_eq!(defaults.max_limit, SEARCH_LIMIT_MAX);
    }

    // ── Constants ──────────────────────────────────────────────────────

    #[test]
    fn search_constants_are_reasonable() {
        assert!(SEARCH_LIMIT_DEFAULT > 0);
        assert!(SEARCH_LIMIT_DEFAULT <= SEARCH_LIMIT_MAX);
        assert!(SEARCH_SNIPPET_MAX_TOKENS > 0);
        assert!(!SEARCH_HIGHLIGHT_PREFIX.is_empty());
        assert!(!SEARCH_HIGHLIGHT_SUFFIX.is_empty());
    }

    // ── SearchQueryValidationError ─────────────────────────────────────

    #[test]
    fn validation_error_invalid_limit_code_message_hint() {
        let err = SearchQueryValidationError::InvalidLimit {
            provided: 5000,
            max_limit: 1000,
        };
        assert_eq!(err.code(), "search.invalid_limit");
        assert!(err.message().contains("5000"));
        assert!(err.message().contains("1000"));
        let hint = err.hint().expect("hint present");
        assert!(hint.contains("1000"));
        assert!(!err.is_query_lint_error());
        assert!(err.lints().is_none());
    }

    #[test]
    fn validation_error_invalid_time_range_code_message_hint() {
        let err = SearchQueryValidationError::InvalidTimeRange {
            since: 200,
            until: 100,
        };
        assert_eq!(err.code(), "search.invalid_time_range");
        assert!(err.message().contains("200"));
        assert!(err.message().contains("100"));
        let hint = err.hint().expect("hint present");
        assert!(hint.contains("--since"));
        assert!(!err.is_query_lint_error());
        assert!(err.lints().is_none());
    }

    #[test]
    fn validation_error_invalid_query_code_message_hint() {
        let lint = SearchLint {
            code: "fts.leading_operator".to_string(),
            severity: SearchLintSeverity::Error,
            message: "Leading AND".to_string(),
            suggestion: Some("Remove leading AND".to_string()),
        };
        let err = SearchQueryValidationError::InvalidQuery { lints: vec![lint] };
        assert_eq!(err.code(), "search.invalid_query");
        assert_eq!(err.message(), "Invalid search query.");
        assert!(err.is_query_lint_error());
        let lints = err.lints().expect("lints present");
        assert_eq!(lints.len(), 1);
        let hint = err.hint().expect("hint present");
        assert!(hint.contains("Leading AND"));
        assert!(hint.contains("suggestion: Remove leading AND"));
    }

    #[test]
    fn validation_error_unsupported_mode_with_empty_supported() {
        let err = SearchQueryValidationError::UnsupportedMode {
            mode: UnifiedSearchMode::Hybrid,
            supported: vec![],
        };
        assert_eq!(err.code(), "search.unsupported_mode");
        assert!(err.message().contains("hybrid"));
        assert!(err.hint().is_none());
    }

    #[test]
    fn validation_error_unsupported_mode_with_supported_list() {
        let err = SearchQueryValidationError::UnsupportedMode {
            mode: UnifiedSearchMode::Hybrid,
            supported: vec![UnifiedSearchMode::Lexical, UnifiedSearchMode::Semantic],
        };
        let hint = err.hint().expect("hint present");
        assert!(hint.contains("lexical"));
        assert!(hint.contains("semantic"));
    }

    #[test]
    fn validation_error_display_matches_message() {
        let err = SearchQueryValidationError::InvalidLimit {
            provided: 0,
            max_limit: 1000,
        };
        assert_eq!(format!("{err}"), err.message());
    }

    #[test]
    fn validation_error_is_std_error() {
        let err = SearchQueryValidationError::InvalidLimit {
            provided: 0,
            max_limit: 1000,
        };
        let _: &dyn std::error::Error = &err;
    }

    // ── lints_have_errors ──────────────────────────────────────────────

    #[test]
    fn lints_have_errors_empty_is_false() {
        assert!(!lints_have_errors(&[]));
    }

    #[test]
    fn lints_have_errors_warnings_only_is_false() {
        let lints = vec![SearchLint {
            code: "w1".to_string(),
            severity: SearchLintSeverity::Warning,
            message: "warn".to_string(),
            suggestion: None,
        }];
        assert!(!lints_have_errors(&lints));
    }

    #[test]
    fn lints_have_errors_mixed_is_true() {
        let lints = vec![
            SearchLint {
                code: "w1".to_string(),
                severity: SearchLintSeverity::Warning,
                message: "warn".to_string(),
                suggestion: None,
            },
            SearchLint {
                code: "e1".to_string(),
                severity: SearchLintSeverity::Error,
                message: "err".to_string(),
                suggestion: None,
            },
        ];
        assert!(lints_have_errors(&lints));
    }

    // ── format_lint_hint ───────────────────────────────────────────────

    #[test]
    fn format_lint_hint_empty_returns_none() {
        assert!(format_lint_hint(&[]).is_none());
    }

    #[test]
    fn format_lint_hint_single_without_suggestion() {
        let lints = vec![SearchLint {
            code: "c1".to_string(),
            severity: SearchLintSeverity::Warning,
            message: "Unbalanced paren".to_string(),
            suggestion: None,
        }];
        let hint = format_lint_hint(&lints).expect("hint present");
        assert_eq!(hint, "Unbalanced paren");
    }

    #[test]
    fn format_lint_hint_single_with_suggestion() {
        let lints = vec![SearchLint {
            code: "c1".to_string(),
            severity: SearchLintSeverity::Warning,
            message: "Unbalanced paren".to_string(),
            suggestion: Some("Add closing )".to_string()),
        }];
        let hint = format_lint_hint(&lints).expect("hint present");
        assert!(hint.contains("Unbalanced paren"));
        assert!(hint.contains("suggestion: Add closing )"));
    }

    #[test]
    fn format_lint_hint_truncates_at_three() {
        let lints: Vec<SearchLint> = (0..5)
            .map(|i| SearchLint {
                code: format!("c{i}"),
                severity: SearchLintSeverity::Warning,
                message: format!("lint{i}"),
                suggestion: None,
            })
            .collect();
        let hint = format_lint_hint(&lints).expect("hint present");
        assert!(hint.contains("lint0"));
        assert!(hint.contains("lint1"));
        assert!(hint.contains("lint2"));
        assert!(!hint.contains("lint3"));
        assert!(!hint.contains("lint4"));
        // Pipe-separated
        assert_eq!(hint.matches(" | ").count(), 2);
    }

    // ── ensure_mode_supported ──────────────────────────────────────────

    #[test]
    fn ensure_mode_supported_succeeds() {
        ensure_mode_supported(
            UnifiedSearchMode::Lexical,
            &[UnifiedSearchMode::Lexical, UnifiedSearchMode::Hybrid],
        )
        .expect("lexical should be supported");
    }

    #[test]
    fn ensure_mode_supported_all_modes() {
        let all = [
            UnifiedSearchMode::Lexical,
            UnifiedSearchMode::Semantic,
            UnifiedSearchMode::Hybrid,
        ];
        for mode in &all {
            ensure_mode_supported(*mode, &all).expect("all modes supported");
        }
    }

    // ── to_storage_search_options ──────────────────────────────────────

    #[test]
    fn to_storage_search_options_maps_fields() {
        let query = UnifiedSearchQuery {
            query: "test".to_string(),
            limit: 50,
            pane: Some(7),
            since: Some(100),
            until: Some(200),
            snippets: true,
            mode: UnifiedSearchMode::Lexical,
        };
        let opts = to_storage_search_options(&query);
        assert_eq!(opts.limit, Some(50));
        assert_eq!(opts.pane_id, Some(7));
        assert_eq!(opts.since, Some(100));
        assert_eq!(opts.until, Some(200));
        assert_eq!(opts.include_snippets, Some(true));
        assert_eq!(opts.snippet_max_tokens, Some(SEARCH_SNIPPET_MAX_TOKENS));
        assert_eq!(
            opts.highlight_prefix.as_deref(),
            Some(SEARCH_HIGHLIGHT_PREFIX)
        );
        assert_eq!(
            opts.highlight_suffix.as_deref(),
            Some(SEARCH_HIGHLIGHT_SUFFIX)
        );
    }

    #[test]
    fn to_storage_search_options_none_fields() {
        let query = UnifiedSearchQuery {
            query: "test".to_string(),
            limit: 20,
            pane: None,
            since: None,
            until: None,
            snippets: false,
            mode: UnifiedSearchMode::Lexical,
        };
        let opts = to_storage_search_options(&query);
        assert!(opts.pane_id.is_none());
        assert!(opts.since.is_none());
        assert!(opts.until.is_none());
        assert_eq!(opts.include_snippets, Some(false));
    }

    // ── parse_unified_search_query extras ──────────────────────────────

    #[test]
    fn parse_trims_whitespace() {
        let parsed = parse_unified_search_query(
            SearchQueryInput {
                query: "  hello world  ".to_string(),
                ..SearchQueryInput::default()
            },
            SearchQueryDefaults::default(),
        )
        .expect("parse query");
        assert_eq!(parsed.query.query, "hello world");
    }

    #[test]
    fn parse_allows_equal_since_and_until() {
        let parsed = parse_unified_search_query(
            SearchQueryInput {
                query: "test".to_string(),
                since: Some(100),
                until: Some(100),
                ..SearchQueryInput::default()
            },
            SearchQueryDefaults::default(),
        )
        .expect("equal since/until should be valid");
        assert_eq!(parsed.query.since, Some(100));
        assert_eq!(parsed.query.until, Some(100));
    }

    #[test]
    fn parse_limit_at_max_boundary_succeeds() {
        let parsed = parse_unified_search_query(
            SearchQueryInput {
                query: "test".to_string(),
                limit: Some(SEARCH_LIMIT_MAX),
                ..SearchQueryInput::default()
            },
            SearchQueryDefaults::default(),
        )
        .expect("max limit should be valid");
        assert_eq!(parsed.query.limit, SEARCH_LIMIT_MAX);
    }

    #[test]
    fn parse_limit_one_succeeds() {
        let parsed = parse_unified_search_query(
            SearchQueryInput {
                query: "test".to_string(),
                limit: Some(1),
                ..SearchQueryInput::default()
            },
            SearchQueryDefaults::default(),
        )
        .expect("limit 1 should be valid");
        assert_eq!(parsed.query.limit, 1);
    }

    // ── UnifiedSearchQuery serde ───────────────────────────────────────

    #[test]
    fn unified_search_query_serde_roundtrip() {
        let query = UnifiedSearchQuery {
            query: "hello world".to_string(),
            limit: 50,
            pane: Some(7),
            since: Some(100),
            until: Some(200),
            snippets: true,
            mode: UnifiedSearchMode::Hybrid,
        };
        let json = serde_json::to_string(&query).unwrap();
        let restored: UnifiedSearchQuery = serde_json::from_str(&json).unwrap();
        assert_eq!(restored, query);
    }

    #[test]
    fn unified_search_query_skip_serializing_none_fields() {
        let query = UnifiedSearchQuery {
            query: "test".to_string(),
            limit: 20,
            pane: None,
            since: None,
            until: None,
            snippets: false,
            mode: UnifiedSearchMode::Lexical,
        };
        let json = serde_json::to_string(&query).unwrap();
        assert!(!json.contains("pane"));
        assert!(!json.contains("since"));
        assert!(!json.contains("until"));
    }

    // ── SearchQueryInput ───────────────────────────────────────────────

    #[test]
    fn search_query_input_default() {
        let input = SearchQueryInput::default();
        assert!(input.query.is_empty());
        assert!(input.limit.is_none());
        assert!(input.pane.is_none());
        assert!(input.since.is_none());
        assert!(input.until.is_none());
        assert!(input.snippets.is_none());
        assert!(input.mode.is_none());
    }
}
