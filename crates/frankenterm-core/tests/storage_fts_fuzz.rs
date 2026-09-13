//! Structure-aware fuzz harness for the FTS5 lexical search path.
//!
//! `StorageHandle::search_with_results` accepts arbitrary caller-supplied
//! strings and threads them straight into FTS5's `MATCH` operator. SQLite's
//! FTS5 has its own micro-grammar (double-quoted phrases, `AND`/`OR`/`NOT`,
//! prefix `word*`, column filters `col:term`, `NEAR`, `+/-`), and malformed
//! input on any of those edges must come back inside the structured storage
//! error envelope rather than a panic or an aborted runtime. This harness
//! accepts `FtsQueryError` and `Database`, as the search path exposes both.
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
//!       `Ok(Vec<SearchResult>)` or a structured FTS/database storage error;
//!   (2) on success, every returned `SearchResult` has non-NaN finite
//!       score, non-empty content, and snippet/highlight presence
//!       matches the `include_snippets` option;
//!   (3) NUL-containing queries obey the same success/structured-error
//!       contract; accepting a query containing NUL is not itself a failure;
//!   (4) repeated calls with the same query are idempotent (same
//!       `(segment_id, score)` list).

// Storage writes traverse the owner-Cx and writer-response future layers.
#![recursion_limit = "256"]

use frankenterm_core::StorageError;
use frankenterm_core::runtime_async::{CompatRuntime, Runtime, RuntimeBuilder};
use frankenterm_core::storage::{PaneRecord, SearchOptions, SearchResult, StorageHandle};
use proptest::prelude::*;
use proptest::test_runner::{TestCaseResult, TestRunner};
use tempfile::TempDir;

fn runtime() -> frankenterm_core::runtime_async::Runtime {
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

async fn seed_pane(storage: &StorageHandle, pane_id: u64) -> frankenterm_core::Result<()> {
    let ts = now_ms();
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
}

async fn seed_corpus(storage: &StorageHandle, pane_id: u64) -> frankenterm_core::Result<()> {
    seed_pane(storage, pane_id).await?;
    for content in [
        "alpha beta gamma",
        "foo bar baz",
        "apple banana cherry",
        "The quick brown fox jumps over the lazy dog",
        "Unicode smoke test: café naïve résumé 漢字 Ω",
        "digits 123 456 789 and punctuation ,.;:!?",
    ] {
        storage.append_segment(pane_id, content, None).await?;
    }
    Ok(())
}

/// Queries only read this corpus. Each property owns its database/runtime;
/// generated cases and shrinking reuse them without sharing across test threads.
fn run_search_property<S: Strategy>(
    name: &'static str,
    strategy: S,
    test: impl Fn(&Runtime, &StorageHandle, S::Value) -> TestCaseResult,
) {
    let (_dir, path) = temp_db();
    let rt = runtime();
    let storage = rt
        .block_on(StorageHandle::new(&path))
        .expect("create storage");
    let seeded = rt.block_on(seed_corpus(&storage, 1));
    if let Err(error) = seeded {
        rt.block_on(storage.shutdown())
            .expect("shutdown failed fixture");
        panic!("seed corpus: {error}");
    }
    let mut runner = TestRunner::new(ProptestConfig {
        cases: 5_000,
        source_file: Some(file!()),
        test_name: Some(name),
        ..ProptestConfig::default()
    });
    // A known query covers every seeded row, catching an empty/broken index
    // that could otherwise make successful generated queries vacuous.
    let corpus_query = "alpha OR foo OR apple OR quick OR Unicode OR digits";
    let before = rt.block_on(storage.search_with_results(corpus_query, SearchOptions::default()));
    // Retain the result until the real writer has settled, including when
    // proptest reports a failing/shrunk case. The directory outlives shutdown.
    let result = runner.run(&strategy, |value| test(&rt, &storage, value));
    let after = rt.block_on(storage.search_with_results(corpus_query, SearchOptions::default()));
    rt.block_on(storage.shutdown())
        .expect("shutdown fuzz storage");
    drop(storage);
    drop(rt);
    let before = before.expect("seeded corpus must be searchable");
    let after = after.expect("fuzz queries must leave the corpus searchable");
    assert_eq!(before.len(), 6, "known query must find every seeded row");
    assert_eq!(
        canon(&before),
        canon(&after),
        "read-only fuzzing must preserve the corpus"
    );
    result.unwrap_or_else(|error| panic!("{name}: {error}"));
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
        Just("\x07".to_string()),    // bell
        Just("\x1b[0m".to_string()), // ANSI SGR
        // NUL byte inside — exercise tokenizer/binding boundary behavior
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
    (arb_operand(), "AND|OR|NOT", arb_operand()).prop_map(|(a, op, b)| format!("{a} {op} {b}"))
}

/// A deliberately-malformed fragment: unbalanced quotes, trailing ops,
/// nested nonsense parens, etc. These exist to force the query onto the
/// `FtsQueryError` path and confirm the error is structured, not a panic.
fn arb_malformed() -> impl Strategy<Value = String> {
    prop_oneof![
        Just("\"unterminated phrase".to_string()),
        Just("AND".to_string()),      // bare operator
        Just("word AND".to_string()), // trailing operator
        Just("AND word".to_string()), // leading operator
        Just("(((".to_string()),      // unbalanced parens
        Just("word OR OR word".to_string()),
        Just(String::new()),                // empty query
        Just(" ".to_string()),              // whitespace only
        Just("*".to_string()),              // prefix with nothing
        Just("word\x00middle".to_string()), // NUL byte smuggled in
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
            return Err(format!(
                "non-positive segment id {} on query {:?}",
                r.segment.id, query
            ));
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

/// Every caller-supplied query terminates in well-formed results or a
/// structured storage error, without panicking or escaping the error envelope.
#[test]
fn fuzz_search_never_panics_and_always_yields_structured_error() {
    run_search_property(
        concat!(
            module_path!(),
            "::fuzz_search_never_panics_and_always_yields_structured_error"
        ),
        arb_query(),
        |rt, storage, query| {
            let opts = SearchOptions {
                include_snippets: Some(true),
                ..SearchOptions::default()
            };
            let results = rt.block_on(storage.search_with_results(&query, opts));
            match results {
                Ok(hits) => {
                    if let Err(msg) = assert_result_well_formed(&hits, true, &query) {
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
            Ok(())
        },
    );
}

/// NUL-containing queries must not crash or escape the structured error
/// contract. SQLite accepts length-prefixed text bindings containing NUL;
/// acceptance by the FTS query parser depends on the surrounding syntax.
#[test]
fn fuzz_nul_byte_in_query_does_not_crash_engine() {
    run_search_property(
        concat!(
            module_path!(),
            "::fuzz_nul_byte_in_query_does_not_crash_engine"
        ),
        ("[a-z]{0,4}", "[a-z]{0,4}"),
        |rt, storage, (prefix, suffix)| {
            let query = format!("{prefix}\0{suffix}");
            let res = rt.block_on(storage.search_with_results(&query, SearchOptions::default()));
            match res {
                Ok(hits) => {
                    if let Err(msg) = assert_result_well_formed(&hits, true, &query) {
                        prop_assert!(false, "{}", msg);
                    }
                }
                Err(e) => {
                    prop_assert!(
                        matches!(
                            &e,
                            frankenterm_core::Error::Storage(
                                StorageError::FtsQueryError(_) | StorageError::Database(_)
                            )
                        ),
                        "NUL byte in query {:?} yielded wrong error variant: {e:?}",
                        query
                    );
                }
            }
            Ok(())
        },
    );
}

/// The same query against the same DB must yield identical result IDs/scores
/// on repeated calls; generated queries must not advance nondeterministic state.
#[test]
fn fuzz_search_is_idempotent_under_repeated_calls() {
    run_search_property(
        concat!(
            module_path!(),
            "::fuzz_search_is_idempotent_under_repeated_calls"
        ),
        arb_query(),
        |rt, storage, query| {
            let (r1, r2) = rt.block_on(async {
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
                    // Error messages may vary; successful results must agree.
                }
                (a, b) => prop_assert!(
                    false,
                    "repeated search on query {:?} gave mismatched outcome types: {a:?} vs {b:?}",
                    query
                ),
            }
            Ok(())
        },
    );
}
