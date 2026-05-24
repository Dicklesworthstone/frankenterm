//! Unicode/multibyte property tests for `extract_snippets` and
//! `tokenize_query` (`frankenterm-core-tantivy/src/tantivy_query.rs`).
//!
//! The existing `proptest_tantivy_query.rs` suite exercises these with ASCII
//! `[a-z ]` text only, which never stresses the byte-offset slicing and
//! char-boundary clamping in `extract_snippets`. These properties feed
//! multibyte text (accented Latin, CJK, emoji, combining marks) to guard the
//! highlight slicing against UTF-8 boundary panics and to confirm a multibyte
//! match is still highlighted.

use frankenterm_core_tantivy::tantivy_query::{SnippetConfig, extract_snippets, tokenize_query};
use proptest::prelude::*;

fn arb_unicode_char() -> impl Strategy<Value = char> {
    prop_oneof![
        Just('a'),
        Just('Z'),
        Just(' '),
        Just('9'),
        Just('_'),
        Just('é'),  // 2-byte
        Just('ñ'),  // 2-byte
        Just('Ω'),  // 2-byte
        Just('日'), // 3-byte
        Just('本'), // 3-byte
        Just('語'), // 3-byte
        Just('🦀'), // 4-byte
        Just('🎉'), // 4-byte
        Just('\u{0301}'), // combining acute accent
    ]
}

fn arb_unicode_text() -> impl Strategy<Value = String> {
    prop::collection::vec(arb_unicode_char(), 0..40).prop_map(|v| v.into_iter().collect())
}

fn arb_term() -> impl Strategy<Value = String> {
    prop_oneof![
        Just(String::new()),
        Just("a".to_string()),
        Just("é".to_string()),
        Just("日本".to_string()),
        Just("🦀".to_string()),
        Just("café".to_string()),
        prop::collection::vec(arb_unicode_char(), 1..5).prop_map(|v| v.into_iter().collect()),
    ]
}

fn enabled_config(max_fragment_len: usize, max_fragments: usize) -> SnippetConfig {
    SnippetConfig {
        max_fragment_len,
        max_fragments,
        highlight_pre: "<b>".to_string(),
        highlight_post: "</b>".to_string(),
        enabled: true,
    }
}

proptest! {
    /// `extract_snippets` must never panic on arbitrary multibyte text and
    /// terms, regardless of fragment window — the byte slicing must always land
    /// on char boundaries.
    #[test]
    fn extract_snippets_never_panics_on_unicode(
        text in arb_unicode_text(),
        terms in prop::collection::vec(arb_term(), 0..4),
        max_fragment_len in 0usize..200,
        max_fragments in 1usize..6,
    ) {
        let config = enabled_config(max_fragment_len, max_fragments);
        let snippets = extract_snippets(&text, &terms, &config);
        // The number of fragments never exceeds the configured cap.
        prop_assert!(snippets.len() <= config.max_fragments);
    }

    /// A multibyte term present in the text is highlighted: a snippet is
    /// produced and carries both highlight markers, with no boundary panic.
    #[test]
    fn multibyte_term_is_highlighted(
        prefix in "[a-z ]{0,20}",
        suffix in "[a-z ]{0,20}",
        term in prop_oneof![
            Just("é"),
            Just("café"),
            Just("日本語"),
            Just("🦀"),
            Just("naïve"),
        ],
        max_fragment_len in 0usize..120,
    ) {
        let text = format!("{prefix}{term}{suffix}");
        let config = enabled_config(max_fragment_len, 4);
        let snippets = extract_snippets(&text, &[term.to_string()], &config);

        prop_assert!(
            !snippets.is_empty(),
            "expected a snippet for term {term:?} in {text:?}"
        );
        let fragment = &snippets[0].fragment;
        let has_markers = fragment.contains("<b>") && fragment.contains("</b>");
        prop_assert!(has_markers, "missing highlight markers in {fragment:?}");
    }

    /// `tokenize_query` never panics on multibyte input and only ever emits
    /// non-empty tokens drawn from the documented `[A-Za-z0-9_./:-]` alphabet
    /// (Unicode code points act as separators).
    #[test]
    fn tokenize_query_tokens_match_ascii_pattern(text in arb_unicode_text()) {
        let tokens = tokenize_query(&text);
        for token in &tokens {
            prop_assert!(!token.is_empty(), "tokenizer emitted an empty token");
            let allowed = token
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '/' | ':' | '-'));
            prop_assert!(allowed, "token {token:?} contains disallowed characters");
        }
    }

    /// `tokenize_query` is idempotent: re-tokenizing its own space-joined output
    /// yields the same tokens (the output alphabet is a fixed point).
    #[test]
    fn tokenize_query_is_idempotent(text in arb_unicode_text()) {
        let once = tokenize_query(&text);
        let twice = tokenize_query(&once.join(" "));
        prop_assert_eq!(once, twice);
    }
}
