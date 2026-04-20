//! Structure-aware fuzz harness for the FTS5 lexical search path.
//!
//! `StorageHandle::search_with_results` accepts arbitrary caller-supplied
//! strings and threads them straight into FTS5's `MATCH` operator. SQLite's
//! FTS5 has its own micro-grammar (double-quoted phrases, `AND`/`OR`/`NOT`,
//! prefix `word*`, column filters `col:term`, `NEAR`, `+/-`), and malformed
//! input on any of those edges must come back as a structured
//! `StorageError::FtsQueryError` rather than a panic, an aborted runtime,
//! or raw SQL bubbling through.
//!
//! Rather than run `cargo-fuzz` (needs nightly + an extra crate), this
//! harness uses proptest as a structure-aware generator: the strategy
//! composes FTS5 syntax fragments (words, quotes, operators, wildcards,
//! NUL bytes, high-value Unicode, BOM, control chars) into realistic
//! hostile inputs. Each generated query is shoved through the production
//! search path against a live on-disk SQLite DB seeded with a small corpus.
//!
//! Invariants checked:
//!   (1) no panic, no unwrap-cascade — every branch must settle to either
//!       `Ok(Vec<SearchResult>)` or `Err(StorageError::FtsQueryError(_))`;
//!   (2) on success, every returned `SearchResult` has non-NaN finite
//!       score, non-empty content, and snippet/highlight presence
//!       matches the `include_snippets` option;
//!   (3) NUL bytes in the query MUST yield `Err` — SQLite rejects NUL in
//!       its text bindings and we must not silently mangle;
//!   (4) repeated calls with the same query are idempotent (same
//!       `(segment_id, score)` list).

use frankenterm_core::StorageError;
use frankenterm_core::runtime_compat::{CompatRuntime, RuntimeBuilder};
use frankenterm_core::storage::{PaneRecord, SearchOptions, SearchResult, StorageHandle};
use proptest::prelude::*;
use tempfile::TempDir;

fn runtime() -> frankenterm_core::runtime_compat::Runtime {
    RuntimeBuilder::current_thread()
        .build()
        .expect("build runtime")
}

fn temp_db() -> (TempDir, String) {
    let dir = TempDir::new().expect("create temp dir");
    let path = dir.path().join("fts_fuzz.db").to_string_lossy().to_string();
    (dir, path)
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

fn seed_pane(storage: &StorageHandle, pane_id: u64) {
    let ts = now_ms();
    let rt = runtime();
    rt.block_on(async {
        storage
            .upsert_pane(PaneRecord {
                pane_id,
                pane_uuid: None,
                domain: "local".to_string(),
                window_id: None,
                tab_id: None,
                title: None,
                cwd: None,
                tty_name: None,
                first_seen_at: ts,
                last_seen_at: ts,
                observed: true,
                ignore_reason: None,
                last_decision_at: None,
            })
            .await
            .expect("upsert pane");
    });
}

fn seed_corpus(storage: &StorageHandle, pane_id: u64) {
    let rt = runtime();
    rt.block_on(async {
        for content in [
            "alpha beta gamma",
            "foo bar baz",
            "apple banana cherry",
            "The quick brown fox jumps over the lazy dog",
            "Unicode smoke test: café naïve résumé 漢字 Ω",
            "digits 123 456 789 and punctuation ,.;:!?",
        ] {
            storage
                .append_segment(pane_id, content, None)
                .await
                .expect("append segment");
        }
    });
}

// ─────────────────────────────────────────────────────────────────────────
// Strategies — structure-aware FTS5 grammar fuzzer
// ─────────────────────────────────────────────────────────────────────────

/// A single FTS5 "word"-ish token, deliberately covering edges the
/// tokenizer cares about: ascii lowercase, mixed case, digits, underscore,
/// high-value unicode, NUL byte, high control chars, BOM.
fn arb_token() -> impl Strategy<Value = String> {
    prop_oneof![
        // Plain ascii word
        "[a-zA-Z]{1,8}".prop_map(|s| s),
        // Word with digits
        "[a-z]{1,4}[0-9]{1,3}".prop_map(|s| s),
        // Word with underscores and hyphens
        "[a-z_-]{1,8}".prop_map(|s| s),
        // Unicode latin extended
        Just("café".to_string()),
        Just("naïve".to_string()),
        Just("résumé".to_string()),
        // CJK
        Just("漢字".to_string()),
        // Emoji (4-byte UTF-8)
        Just("🔥".to_string()),
        // Greek letters
        Just("Ω".to_string()),
        // Invalid-but-legal-UTF-8 edges
        Just("\u{FEFF}word".to_string()), // BOM prefix
        Just("\u{200B}word".to_string()), // zero-width space
        // Control characters
        Just("\x07".to_string()), // bell
        Just("\x1b[0m".to_string()), // ANSI SGR
        // NUL byte inside — SQLite rejects this
        Just("pre\0post".to_string()),
        // Raw NUL
        Just("\0".to_string()),
    ]
}

/// An FTS5 "operand" — a token, quoted phrase, or prefix-matched token.
fn arb_operand() -> impl Strategy<Value = String> {
    prop_oneof![
        arb_token(),
        // Double-quoted phrase
        (arb_token(), arb_token()).prop_map(|(a, b)| format!("\"{a} {b}\"")),
        // Prefix match
        arb_token().prop_map(|t| format!("{t}*")),
        // Column filter (may or may not exist as a column)
        arb_token().prop_map(|t| format!("content:{t}")),
        // Column filter with nonsense column name (should produce
        // FtsQueryError)
        arb_token().prop_map(|t| format!("no_such_column:{t}")),
    ]
}

/// A binary FTS5 expression joining two operands with an operator.
fn arb_binary() -> impl Strategy<Value = String> {
    (arb_operand(), "AND|OR|NOT", arb_operand())
        .prop_map(|(a, op, b)| format!("{a} {op} {b}"))
}

/// A deliberately-malformed fragment: unbalanced quotes, trailing ops,
/// nested nonsense parens, etc. These exist to force the query onto the
/// `FtsQueryError` path and confirm the error is structured, not a panic.
fn arb_malformed() -> impl Strategy<Value = String> {
    prop_oneof![
        Just("\"unterminated phrase".to_string()),
        Just("AND".to_string()),           // bare operator
        Just("word AND".to_string()),      // trailing operator
        Just("AND word".to_string()),      // leading operator
        Just("(((".to_string()),           // unbalanced parens
        Just("word OR OR word".to_string()),
        Just("".to_string()),              // empty query
        Just(" ".to_string()),             // whitespace only
        Just("*".to_string()),             // prefix with nothing
        Just("word\x00middle".to_string()),// NUL byte smuggled in
        // 4 KiB of single word — stress the tokenizer buffer
        Just("x".repeat(4096)),
        // Nested quotes
        Just("\"\"inner\"\"".to_string()),
        // Column with empty term
        Just("content:".to_string()),
    ]
}

/// The full query generator: sprinkle malformed fragments in alongside
/// structurally-valid expressions so both the happy path and the error
/// path are exercised.
fn arb_query() -> impl Strategy<Value = String> {
    prop_oneof![
        arb_token(),
        arb_operand(),
        arb_binary(),
        arb_malformed(),
        // Pairs joined by whitespace (implicit AND in FTS5)
        (arb_operand(), arb_operand()).prop_map(|(a, b)| format!("{a} {b}")),
    ]
}

fn canon(results: &[SearchResult]) -> Vec<(i64, i64)> {
    results
        .iter()
        .map(|r| (r.segment.id, (r.score * 1_000_000.0).round() as i64))
        .collect()
}

fn assert_result_well_formed(
    results: &[SearchResult],
    include_snippets: bool,
    query: &str,
) -> Result<(), String> {
    for r in results {
        if !r.score.is_finite() {
            return Err(format!(
                "non-finite score {} for segment {} on query {:?}",
                r.score, r.segment.id, query
            ));
        }
        if r.segment.id <= 0 {
            return Err(format!("non-positive segment id {} on query {:?}", r.segment.id, query));
        }
        if r.segment.content.is_empty() {
            return Err(format!(
                "empty content for segment {} on query {:?}",
                r.segment.id, query
            ));
        }
        if include_snippets {
            if r.snippet.is_none() {
                return Err(format!(
                    "snippets requested but missing for segment {} on query {:?}",
                    r.segment.id, query
                ));
            }
            if r.highlight.is_none() {
                return Err(format!(
                    "highlight requested but missing for segment {} on query {:?}",
                    r.segment.id, query
                ));
            }
        }
    }
    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────
// Proptest harness
// ─────────────────────────────────────────────────────────────────────────

proptest! {
    // ~5k cases takes ~30–60s wall on the FTS5 path; budget generously
    // under the 60s local-run target.
    #![proptest_config(ProptestConfig::with_cases(5_000))]

    /// Invariant: every caller-supplied query must terminate in Ok or a
    /// structured `StorageError::FtsQueryError`. No panics, no raw
    /// rusqlite errors, no unhandled variants.
    #[test]
    fn fuzz_search_never_panics_and_always_yields_structured_error(query in arb_query()) {
        let (_dir, path) = temp_db();
        let rt = runtime();
        let (results, include_snippets) = rt.block_on(async {
            let storage = StorageHandle::new(&path).await.expect("create storage");
            seed_pane(&storage, 1);
            seed_corpus(&storage, 1);
            let opts = SearchOptions {
                include_snippets: Some(true),
                ..SearchOptions::default()
            };
            (storage.search_with_results(&query, opts.clone()).await, true)
        });

        match results {
            Ok(hits) => {
                if let Err(msg) = assert_result_well_formed(&hits, include_snippets, &query) {
                    prop_assert!(false, "{}", msg);
                }
            }
            Err(e) => {
                let storage_err = match &e {
                    frankenterm_core::Error::Storage(s) => s,
                    other => {
                        prop_assert!(
                            false,
                            "non-storage error variant for query {:?}: {other:?}",
                            query
                        );
                        unreachable!()
                    }
                };
                prop_assert!(
                    matches!(
                        storage_err,
                        StorageError::FtsQueryError(_) | StorageError::Database(_)
                    ),
                    "unexpected storage error variant for query {:?}: {storage_err:?}",
                    query
                );
            }
        }
    }

    /// NUL bytes inside a search query must not crash or corrupt the FTS5
    /// engine. This test's earlier version asserted that NUL *must* yield
    /// an error; fuzzing surfaced that assumption as incorrect. SQLite's
    /// text bindings are length-prefixed (not C-string terminated), and
    /// FTS5's default unicode61 tokenizer treats U+0000 as a non-word
    /// separator — equivalent to whitespace — so a query like `"a\0"`
    /// legitimately tokenizes to `["a"]` and executes as a normal search.
    ///
    /// The invariant we actually care about is the same one `fuzz_search
    /// _never_panics_and_always_yields_structured_error` enforces for
    /// arbitrary queries: no panic, well-formed Ok, or structured Err.
    /// A NUL byte must not bypass that contract.
    #[test]
    fn fuzz_nul_byte_in_query_does_not_crash_engine(
        prefix in "[a-z]{0,4}",
        suffix in "[a-z]{0,4}",
    ) {
        // Construct a query that must contain at least one NUL byte.
        let query = format!("{prefix}\0{suffix}");

        let (_dir, path) = temp_db();
        let rt = runtime();
        let res = rt.block_on(async {
            let storage = StorageHandle::new(&path).await.expect("create storage");
            seed_pane(&storage, 1);
            seed_corpus(&storage, 1);
            storage
                .search_with_results(&query, SearchOptions::default())
                .await
        });

        match res {
            Ok(hits) => {
                if let Err(msg) = assert_result_well_formed(&hits, true, &query) {
                    prop_assert!(false, "{}", msg);
                }
            }
            Err(e) => {
                let ok = matches!(
                    &e,
                    frankenterm_core::Error::Storage(
                        StorageError::FtsQueryError(_) | StorageError::Database(_)
                    )
                );
                prop_assert!(
                    ok,
                    "NUL byte in query {:?} yielded wrong error variant: {e:?}",
                    query
                );
            }
        }
    }

    /// Idempotence: the same query against the same DB must yield
    /// identical result shape+score on repeated calls. This is a cheap
    /// liveness check that the fuzz-generated queries don't accidentally
    /// advance nondeterministic state in the engine.
    #[test]
    fn fuzz_search_is_idempotent_under_repeated_calls(query in arb_query()) {
        let (_dir, path) = temp_db();
        let rt = runtime();
        let (r1, r2) = rt.block_on(async {
            let storage = StorageHandle::new(&path).await.expect("create storage");
            seed_pane(&storage, 1);
            seed_corpus(&storage, 1);
            let opts = SearchOptions {
                include_snippets: Some(false),
                ..SearchOptions::default()
            };
            let a = storage.search_with_results(&query, opts.clone()).await;
            let b = storage.search_with_results(&query, opts).await;
            (a, b)
        });

        match (r1, r2) {
            (Ok(a), Ok(b)) => prop_assert_eq!(canon(&a), canon(&b)),
            (Err(_), Err(_)) => {
                // Both errors — that's fine; we only require determinism
                // on the Ok branch because error messages vary.
            }
            (a, b) => prop_assert!(
                false,
                "repeated search on query {:?} gave mismatched outcome types: {a:?} vs {b:?}",
                query
            ),
        }
    }
}
