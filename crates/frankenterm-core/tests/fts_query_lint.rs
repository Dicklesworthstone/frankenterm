//! Tests for the FTS query linter `lint_fts_query`
//! (`crates/frankenterm-core/src/storage.rs`).
//!
//! The linter classifies common FTS5 query mistakes (unbalanced quotes/parens,
//! misplaced boolean operators, unsupported wildcards). It had no integration
//! coverage; these tests pin the wildcard classification (including the
//! leading-wildcard fix) and the core rule set, plus a panic-safety property
//! over arbitrary input.

use frankenterm_core::storage::{SearchLintSeverity, lint_fts_query};
use proptest::prelude::*;

fn codes(query: &str) -> Vec<String> {
    lint_fts_query(query)
        .into_iter()
        .map(|lint| lint.code)
        .collect()
}

#[test]
fn empty_query_is_flagged() {
    assert_eq!(codes(""), vec!["empty_query".to_string()]);
    assert_eq!(codes("   "), vec!["empty_query".to_string()]);
}

#[test]
fn clean_suffix_wildcard_has_no_wildcard_lint() {
    let c = codes("err*");
    assert!(
        !c.iter().any(|code| code.starts_with("wildcard")),
        "unexpected wildcard lint for suffix wildcard: {c:?}"
    );
}

#[test]
fn leading_only_wildcard_is_classified_as_prefix() {
    // Regression: "*foo" is a leading wildcard, not merely a non-suffix
    // wildcard. It must report wildcard_prefix, not wildcard_position.
    let c = codes("*foo");
    assert!(c.contains(&"wildcard_prefix".to_string()), "got {c:?}");
    assert!(!c.contains(&"wildcard_position".to_string()), "got {c:?}");
}

#[test]
fn double_wildcard_is_classified_as_prefix() {
    let c = codes("*foo*");
    assert!(c.contains(&"wildcard_prefix".to_string()), "got {c:?}");
}

#[test]
fn interior_wildcard_is_classified_as_position() {
    let c = codes("fo*o");
    assert!(c.contains(&"wildcard_position".to_string()), "got {c:?}");
    assert!(!c.contains(&"wildcard_prefix".to_string()), "got {c:?}");
}

#[test]
fn bare_wildcard_is_flagged() {
    assert!(codes("*").contains(&"wildcard_only".to_string()));
}

#[test]
fn unbalanced_quotes_are_flagged() {
    assert!(codes("\"foo").contains(&"unbalanced_quotes".to_string()));
}

#[test]
fn unmatched_closing_paren_is_flagged() {
    let c = codes("foo)");
    assert!(c.contains(&"unmatched_paren_close".to_string()), "got {c:?}");
}

#[test]
fn unbalanced_open_paren_is_warned() {
    let c = codes("(foo");
    assert!(c.contains(&"unbalanced_parentheses".to_string()), "got {c:?}");
}

#[test]
fn leading_operator_is_flagged() {
    assert!(codes("AND foo").contains(&"leading_operator".to_string()));
}

#[test]
fn trailing_operator_is_flagged() {
    assert!(codes("foo AND").contains(&"trailing_operator".to_string()));
}

#[test]
fn consecutive_operators_are_flagged() {
    assert!(codes("foo AND OR bar").contains(&"double_operator".to_string()));
}

#[test]
fn clean_query_has_no_lints() {
    assert!(codes("error timeout").is_empty());
    assert!(codes("\"exact phrase\" AND retry").is_empty());
}

proptest! {
    /// The linter must never panic and must emit well-formed lints (non-empty
    /// code and message) for any input, including arbitrary Unicode.
    #[test]
    fn lint_never_panics_and_is_well_formed(query in ".*") {
        let lints = lint_fts_query(&query);
        for lint in &lints {
            prop_assert!(!lint.code.is_empty(), "empty lint code");
            prop_assert!(!lint.message.is_empty(), "empty lint message");
            prop_assert!(matches!(
                lint.severity,
                SearchLintSeverity::Warning | SearchLintSeverity::Error
            ));
        }
    }

    /// A non-empty, balanced query of plain alphanumeric terms produces no
    /// error-severity lints (it is always syntactically valid).
    #[test]
    fn plain_term_queries_have_no_errors(
        terms in prop::collection::vec("[a-z]{1,8}", 1..6),
    ) {
        // Exclude tokens that spell boolean operators (AND/OR/NOT) — those are
        // legitimately flagged when leading, trailing, or doubled.
        prop_assume!(terms.iter().all(|t| !matches!(
            t.to_ascii_uppercase().as_str(),
            "AND" | "OR" | "NOT"
        )));
        let query = terms.join(" ");
        let lints = lint_fts_query(&query);
        let has_error = lints
            .iter()
            .any(|lint| lint.severity == SearchLintSeverity::Error);
        prop_assert!(!has_error, "unexpected error lint for {query:?}: {lints:?}");
    }
}
