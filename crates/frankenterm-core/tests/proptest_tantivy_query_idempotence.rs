//! Property tests for `tantivy_query`'s pure functions —
//! `tokenize_query` + `extract_snippets` — focused on invariants
//! that complement the existing `proptest_tantivy_query.rs`:
//!
//! 1. **Idempotence / fixed-point**: re-tokenizing
//!    `tokens.join(" ")` reproduces the same token list. This is
//!    the user-requested "tokenizer is idempotent" property
//!    (cc_2 PHASE 4 storage proptest sweep).
//! 2. **No-panic over full ASCII byte range**: arbitrary ASCII
//!    strings (chars 0..=127) must not cause `tokenize_query` to
//!    panic. This is the user-requested "query parser handles
//!    all ASCII ranges without panic".
//! 3. **No-panic over arbitrary UTF-8**: same property over any
//!    valid UTF-8 string. Catches char-boundary regressions in
//!    the snippet extractor's window-clamping math.
//! 4. **Determinism**: `tokenize_query` is pure; identical
//!    inputs always produce identical outputs (no hidden global
//!    state).
//! 5. **Token charset invariant**: every output token consists
//!    only of `[A-Za-z0-9_./:-]+` per the documented regex
//!    pattern at `recorder_lexical_schema.rs::TERMINAL_TOKEN_PATTERN`.
//! 6. **`extract_snippets` no-panic over UTF-8 + arbitrary terms**:
//!    the char-boundary-clamp logic at lines 502-543 of
//!    `tantivy_query.rs` is the likely regression vector
//!    because UTF-8 char boundaries interact with the `start`
//!    and `end` computations.
//!
//! Logs are emitted as structured tracing-json events so a
//! failing case lands a parseable record of the input + observed
//! tokens — same shape as
//! `proptest_storage_backend_param_binding` (br-ft-l1jgo phase-3).
//!
//! Existing `proptest_tantivy_query.rs` covers basic invariants
//! (empty → empty, whitespace → empty, single word preserved,
//! paths/namespaces preserved, no-whitespace-in-tokens). This
//! file adds the gap: idempotence + ASCII fuzz + UTF-8 fuzz +
//! charset invariant + extract_snippets fuzz.

use std::sync::Once;

use frankenterm_core_tantivy::tantivy_query::{SnippetConfig, extract_snippets, tokenize_query};
use proptest::prelude::*;
use tracing::info;

fn init_test_tracing_json() {
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        let _ = tracing_subscriber::fmt()
            .json()
            .with_target(true)
            .with_test_writer()
            .try_init();
    });
}

/// True iff `c` is in the documented terminal-token charset
/// `[A-Za-z0-9_./:-]+` — exactly matches both `tokenize_query`'s
/// post-ft-j5szx ASCII-only acceptance and the index-side
/// `RegexTokenizer` at
/// `recorder_lexical_schema::TERMINAL_TOKEN_PATTERN`.
///
/// br-ft-j5szx: tightened from `is_alphanumeric` (Unicode-aware)
/// to `is_ascii_alphanumeric` together with the implementation
/// fix at `tantivy_query.rs::tokenize_query`. The two are now
/// consistent — Unicode code points that previously slipped
/// through the storage tokenizer (superscript digits, circled
/// digits, Greek/Cyrillic/CJK letters, etc.) are now stripped
/// as separators at both sides.
fn is_token_char(c: char) -> bool {
    c.is_ascii_alphanumeric()
        || c == '_'
        || c == '.'
        || c == '/'
        || c == ':'
        || c == '-'
}

/// ASCII string covering the full 0x00..=0x7F byte range. Used
/// to fuzz `tokenize_query` and `extract_snippets` for panic
/// resistance across every ASCII control character, every
/// punctuation character, and the printable range.
fn arbitrary_ascii() -> impl Strategy<Value = String> {
    prop::collection::vec(0u8..=127u8, 0..96)
        .prop_map(|bytes| String::from_utf8(bytes).expect("ASCII is always valid UTF-8"))
}

/// Arbitrary UTF-8 string up to 128 chars. Catches char-boundary
/// math regressions in `extract_snippets` and any non-ASCII
/// blow-ups in `tokenize_query`.
fn arbitrary_utf8() -> impl Strategy<Value = String> {
    "\\PC{0,128}".prop_map(String::from)
}

/// Vector of "valid token" strings — each consists only of the
/// documented charset, so they are guaranteed to round-trip
/// through `tokenize_query` un-split.
fn valid_token() -> impl Strategy<Value = String> {
    "[A-Za-z0-9_./:-]{1,16}".prop_map(String::from)
}

fn token_list() -> impl Strategy<Value = Vec<String>> {
    prop::collection::vec(valid_token(), 0..8)
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    /// **Idempotence / fixed-point**: re-tokenize the join of a
    /// token list and recover the same list. This is the
    /// user-requested "tokenizer is idempotent" property.
    ///
    /// `tokens.join(" ")` produces a string whose only
    /// non-token characters are the joining spaces; re-tokenizing
    /// must then split exactly on those spaces and recover the
    /// original list. Property holds because every token in the
    /// input is already in the charset that `tokenize_query`
    /// preserves.
    #[test]
    fn proptest_tantivy_tokenize_query_idempotent(tokens in token_list()) {
        init_test_tracing_json();
        let joined = tokens.join(" ");
        let re_tokenized = tokenize_query(&joined);

        info!(
            test = "tokenize_query_idempotent",
            input_token_count = tokens.len(),
            joined_len = joined.len(),
            re_tokenized = ?re_tokenized,
            "tokenize idempotence case"
        );

        prop_assert_eq!(
            re_tokenized, tokens,
            "tokenize_query(tokens.join(\" \")) must reproduce tokens"
        );
    }

    /// **Pure / deterministic**: same input always produces the
    /// same output. No hidden global state, no time-dependence.
    /// Run twice in succession; results must be byte-identical.
    #[test]
    fn proptest_tantivy_tokenize_query_deterministic(input in arbitrary_utf8()) {
        init_test_tracing_json();
        let first = tokenize_query(&input);
        let second = tokenize_query(&input);
        prop_assert_eq!(first, second, "tokenize_query must be deterministic");
    }

    /// **No-panic over full ASCII byte range**: this is the
    /// user-requested "query parser handles all ASCII ranges
    /// without panic". Generates strings covering every byte in
    /// 0x00..=0x7F (control chars + printable ASCII).
    #[test]
    fn proptest_tantivy_tokenize_query_no_panic_on_ascii(input in arbitrary_ascii()) {
        init_test_tracing_json();
        // Any ASCII input must complete without panic.
        let tokens = tokenize_query(&input);
        info!(
            test = "tokenize_query_no_panic_on_ascii",
            input_len = input.len(),
            output_token_count = tokens.len(),
            "ASCII fuzz case"
        );
        // Trivial post-condition so the assertion machinery
        // engages — the real property here is the absence of a
        // panic.
        prop_assert!(tokens.iter().all(|t| !t.is_empty()));
    }

    /// **No-panic over arbitrary UTF-8**: extends the ASCII
    /// property to the full Unicode range. Catches non-ASCII
    /// regressions in any future tokenizer rework that touches
    /// the byte-walk logic.
    #[test]
    fn proptest_tantivy_tokenize_query_no_panic_on_utf8(input in arbitrary_utf8()) {
        init_test_tracing_json();
        let tokens = tokenize_query(&input);
        prop_assert!(tokens.iter().all(|t| !t.is_empty()));
    }

    /// **Token charset invariant**: every output token contains
    /// ONLY characters from the documented `[A-Za-z0-9_./:-]+`
    /// charset. Holds for any input — the tokenizer's job is to
    /// strip non-charset characters as separators.
    #[test]
    fn proptest_tantivy_tokenize_query_output_in_charset(input in arbitrary_utf8()) {
        init_test_tracing_json();
        let tokens = tokenize_query(&input);
        for token in &tokens {
            for c in token.chars() {
                prop_assert!(
                    is_token_char(c),
                    "token {token:?} contains out-of-charset char {c:?}"
                );
            }
        }
    }

    /// **No-panic for `extract_snippets` over UTF-8 + arbitrary
    /// terms**: the char-boundary-clamp math at
    /// `tantivy_query.rs:502-543` handles the case where
    /// `start = pos.saturating_sub(half_window)` lands inside a
    /// multi-byte UTF-8 sequence; same for `end = m.end() +
    /// half_window`. Both are walked back/forward to the nearest
    /// char boundary, but the math is intricate enough that any
    /// future refactor risks an off-by-one panic.
    #[test]
    fn proptest_tantivy_extract_snippets_no_panic(
        text in arbitrary_utf8(),
        terms in prop::collection::vec(valid_token(), 0..4),
    ) {
        init_test_tracing_json();
        let snippets = extract_snippets(&text, &terms, &SnippetConfig::default());
        info!(
            test = "extract_snippets_no_panic",
            text_len = text.len(),
            term_count = terms.len(),
            snippet_count = snippets.len(),
            "extract_snippets fuzz case"
        );
        // No panic = test passes. Trivial post-condition for
        // the assertion machinery.
        for snip in &snippets {
            prop_assert!(!snip.field.is_empty());
        }
    }
}
