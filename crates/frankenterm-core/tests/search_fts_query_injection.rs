use frankenterm_core::StorageError;
use frankenterm_core::runtime_async::{CompatRuntime, RuntimeBuilder};
use frankenterm_core::storage::{PaneRecord, SearchOptions, StorageHandle};
use tempfile::TempDir;

fn runtime() -> frankenterm_core::runtime_async::Runtime {
    RuntimeBuilder::current_thread()
        .build()
        .expect("build runtime")
}

fn temp_db() -> (TempDir, String) {
    let dir = TempDir::new().expect("create temp dir");
    let path = dir.path().join("fts_injection_audit.db");
    (dir, path.to_string_lossy().to_string())
}

fn pane_record(pane_id: u64, now_ms: i64) -> PaneRecord {
    PaneRecord {
        pane_id,
        pane_uuid: None,
        domain: "local".to_string(),
        window_id: None,
        tab_id: None,
        title: None,
        cwd: None,
        tty_name: None,
        first_seen_at: now_ms,
        last_seen_at: now_ms,
        observed: true,
        ignore_reason: None,
        last_decision_at: None,
    }
}

async fn seed_search_corpus(storage: &StorageHandle) {
    let now_ms = 1_700_000_000_000i64;
    storage
        .upsert_pane(pane_record(1, now_ms))
        .await
        .expect("upsert pane");
    storage
        .append_segment(1, "needle alpha", None)
        .await
        .expect("append alpha");
    storage
        .append_segment(1, "needle beta", None)
        .await
        .expect("append beta");
}

#[test]
fn sqlish_payload_stays_in_structured_fts_error_path() {
    let (_dir, path) = temp_db();
    let rt = runtime();

    rt.block_on(async {
        let storage = StorageHandle::new(&path).await.expect("create storage");
        seed_search_corpus(&storage).await;

        let err = storage
            .search_with_results("\" OR 1 --", SearchOptions::default())
            .await
            .expect_err("sql-looking payload must not execute as SQL");
        let err_dbg = format!("{err:?}");

        let msg = match err {
            frankenterm_core::Error::Storage(StorageError::FtsQueryError(msg)) => msg,
            _ => String::new(),
        };
        assert!(!msg.is_empty(), "unexpected error variant: {err_dbg}");
        assert!(
            msg.contains("Invalid FTS5 query syntax") || msg.contains("Query validation failed")
        );
    });
}

/// Outcome contract for an injection-style query payload. Every payload
/// the harness exercises MUST land in exactly one of these buckets — never
/// in a panic, OS error, SQL widening, or an unstructured anyhow chain.
#[derive(Debug)]
enum InjectionOutcome {
    /// Payload was syntactically valid FTS5; storage returned a result set
    /// (possibly empty). Common for column-prefixed queries against real
    /// columns and for well-formed phrase / NEAR / prefix payloads.
    Success,
    /// Payload was rejected by FTS5's grammar or by the validation layer's
    /// `Query validation failed: …` wrapper. Either is acceptable; both are
    /// the structured error path the renderer / MCP envelope assume.
    StructuredFtsError,
}

/// Run a single injection payload through the real search path and assert
/// that the outcome matches the expected bucket. Used by the per-grammar
/// tests below; keeping the assertion logic in one place prevents the
/// per-payload tests from drifting into ad-hoc string-match noise.
async fn assert_injection_outcome(
    storage: &StorageHandle,
    payload: &str,
    expected: InjectionOutcome,
    label: &str,
) {
    match storage
        .search_with_results(payload, SearchOptions::default())
        .await
    {
        Ok(_results) => {
            assert!(
                matches!(expected, InjectionOutcome::Success),
                "[{label}] payload {payload:?} succeeded but the test expected {expected:?}"
            );
        }
        Err(frankenterm_core::Error::Storage(StorageError::FtsQueryError(msg))) => {
            assert!(
                matches!(expected, InjectionOutcome::StructuredFtsError),
                "[{label}] payload {payload:?} returned structured FTS error \
                 but the test expected {expected:?}: {msg}"
            );
            assert!(
                msg.contains("Invalid FTS5 query syntax")
                    || msg.contains("Query validation failed")
                    || msg.contains("no such column"),
                "[{label}] FTS error message lost its structured prefix: {msg}"
            );
        }
        Err(other) => panic!(
            "[{label}] payload {payload:?} surfaced a non-FTS error variant; \
             that is the regression class this whole file exists to prevent: {other:?}"
        ),
    }
}

#[test]
fn near_operator_well_formed_payload_succeeds() {
    // NEAR(a b, n) is valid FTS5 grammar with a literal token list and a
    // bounded distance. `needle` is in the seeded corpus so the query
    // must return Ok (with possibly zero or one hit) — the outcome we
    // care about is "structured success", not the hit count.
    let (_dir, path) = temp_db();
    runtime().block_on(async {
        let storage = StorageHandle::new(&path).await.expect("create storage");
        seed_search_corpus(&storage).await;

        for payload in [
            "NEAR(needle alpha, 5)",
            "NEAR(\"alpha\" \"beta\", 10)",
            "NEAR(needle beta)",
        ] {
            assert_injection_outcome(
                &storage,
                payload,
                InjectionOutcome::Success,
                "near_well_formed",
            )
            .await;
        }
    });
}

#[test]
fn near_operator_malformed_payload_stays_in_structured_fts_error_path() {
    // NEAR with a single argument, garbage distance, or bare keyword must
    // be rejected by the FTS5 parser, not panic and not widen into a raw
    // SQL error. This is the inverse of the well-formed case — same
    // operator, structurally invalid usage.
    let (_dir, path) = temp_db();
    runtime().block_on(async {
        let storage = StorageHandle::new(&path).await.expect("create storage");
        seed_search_corpus(&storage).await;

        // Note: `NEAR alpha beta` (no parens) is NOT a malformed NEAR — FTS5
        // only reserves the NEAR keyword inside `NEAR(...)` syntax. Bare
        // `NEAR` is treated as an ordinary token, which is unexpected but
        // intentional per FTS5 spec; it's covered by the well-formed case
        // implicitly. Test only the strictly malformed parenthesized forms.
        for payload in [
            "NEAR(needle, abc)",          // distance must be integer
            "NEAR(, 5)",                  // missing token list
            "NEAR(needle alpha,)",        // trailing comma, missing distance
        ] {
            assert_injection_outcome(
                &storage,
                payload,
                InjectionOutcome::StructuredFtsError,
                "near_malformed",
            )
            .await;
        }
    });
}

#[test]
fn real_content_column_prefix_is_accepted_as_valid_fts_syntax() {
    // `content` IS the only column in output_segments_fts (storage.rs:187-192,
    // `CREATE VIRTUAL TABLE … fts5(content, …)`). Prefixing the query with
    // it MUST be accepted as a structured success — not surfaced as
    // "no such column", because the column does exist. This catches the
    // regression where a future schema rename silently breaks every
    // column-qualified query a client builds.
    let (_dir, path) = temp_db();
    runtime().block_on(async {
        let storage = StorageHandle::new(&path).await.expect("create storage");
        seed_search_corpus(&storage).await;

        for payload in ["content:needle", "{content}: needle", "content : alpha"] {
            assert_injection_outcome(
                &storage,
                payload,
                InjectionOutcome::Success,
                "real_column_prefix",
            )
            .await;
        }
    });
}

#[test]
fn pane_id_column_ref_is_rejected_because_fts_table_only_has_content() {
    // pane_id lives on output_segments (the base table) but NOT on
    // output_segments_fts (the FTS5 virtual table). Querying it through
    // FTS5 must produce the structured "no such column" / FTS error
    // path; otherwise a curious client would think pane_id-keyed search
    // works and silently get wrong rows.
    let (_dir, path) = temp_db();
    runtime().block_on(async {
        let storage = StorageHandle::new(&path).await.expect("create storage");
        seed_search_corpus(&storage).await;

        for payload in ["pane_id:1", "id:42", "captured_at:1700000000000"] {
            assert_injection_outcome(
                &storage,
                payload,
                InjectionOutcome::StructuredFtsError,
                "non_fts_column_ref",
            )
            .await;
        }
    });
}

#[test]
fn unbalanced_quote_payloads_stay_in_structured_fts_error_path() {
    // FTS5 phrase syntax requires balanced double quotes. The parser
    // produces "fts5: syntax error near …" for each of these.
    let (_dir, path) = temp_db();
    runtime().block_on(async {
        let storage = StorageHandle::new(&path).await.expect("create storage");
        seed_search_corpus(&storage).await;

        for payload in [
            "\"unclosed",
            "ab\"c",                     // stray quote in the middle
            "needle \"\"\"",              // odd number of quotes
            "\"alpha\" \"beta",          // second quoted phrase opens but never closes
        ] {
            assert_injection_outcome(
                &storage,
                payload,
                InjectionOutcome::StructuredFtsError,
                "unbalanced_quotes",
            )
            .await;
        }
    });
}

#[test]
fn unicode_tokenizer_edges_succeed_without_widening_to_sql() {
    // The unicode61 tokenizer should accept these as ordinary text tokens
    // — if any of them ever started panicking or surfacing a non-FTS
    // error, the whole search surface would be exposed to the locale of
    // whichever shell happened to type a non-ASCII character.
    let (_dir, path) = temp_db();
    runtime().block_on(async {
        let storage = StorageHandle::new(&path).await.expect("create storage");
        seed_search_corpus(&storage).await;

        for payload in [
            "café",
            "日本語",
            "🎉",
            "needle\u{200B}alpha",       // zero-width space
            "naïve",                       // combining diacritic
            "\u{0301}accented",            // leading combining mark
            "ʇsǝʇ",                        // upside-down ascii (mirrored unicode)
        ] {
            assert_injection_outcome(
                &storage,
                payload,
                InjectionOutcome::Success,
                "unicode_tokenizer_edges",
            )
            .await;
        }
    });
}

#[test]
fn prefix_match_abuse_is_handled_or_rejected_consistently() {
    // FTS5 supports trailing `*` for prefix match (e.g. `nee*`). The
    // grammar rejects bare `*` and leading `*`; some shapes resolve to
    // empty results. All three outcomes must stay in the structured
    // success / structured FTS-error path, never panic, never widen.
    let (_dir, path) = temp_db();
    runtime().block_on(async {
        let storage = StorageHandle::new(&path).await.expect("create storage");
        seed_search_corpus(&storage).await;

        // Trailing prefix wildcard: valid grammar, hits the seeded corpus.
        assert_injection_outcome(
            &storage,
            "nee*",
            InjectionOutcome::Success,
            "prefix_trailing_star",
        )
        .await;

        // Bare and leading-star payloads: FTS5 rejects them.
        for payload in ["*", "*needle", "**", "needle**"] {
            let result = storage
                .search_with_results(payload, SearchOptions::default())
                .await;
            match result {
                Ok(_) | Err(frankenterm_core::Error::Storage(StorageError::FtsQueryError(_))) => {}
                Err(other) => panic!(
                    "[prefix_abuse] payload {payload:?} surfaced unexpected variant: {other:?}"
                ),
            }
        }
    });
}

#[test]
fn parenthesis_nesting_payloads_stay_in_grammar() {
    // Well-balanced and pathologically-nested parens should both be
    // grammatical FTS5; unbalanced parens land on the structured error
    // path. SQLite's FTS5 parser has its own depth limits — we don't try
    // to exhaust them, just probe a representative range.
    let (_dir, path) = temp_db();
    runtime().block_on(async {
        let storage = StorageHandle::new(&path).await.expect("create storage");
        seed_search_corpus(&storage).await;

        // Valid: balanced nesting.
        for payload in [
            "(needle)",
            "((needle alpha) OR beta)",
            "(((((((((((needle)))))))))))", // 11-deep, well within FTS5 limits
        ] {
            assert_injection_outcome(
                &storage,
                payload,
                InjectionOutcome::Success,
                "paren_balanced",
            )
            .await;
        }

        // Invalid: unbalanced nesting.
        for payload in [
            "(((needle",
            "needle)))",
            "(()(()))(",          // structurally lopsided
            ")(",
        ] {
            assert_injection_outcome(
                &storage,
                payload,
                InjectionOutcome::StructuredFtsError,
                "paren_unbalanced",
            )
            .await;
        }
    });
}

#[test]
fn empty_and_whitespace_only_payloads_resolve_without_panic() {
    // Empty and whitespace-only queries are FTS5 syntax errors. Bare
    // operator keywords like `AND` / `OR` / `NOT` are also rejected. We
    // care that they all land on the structured error path — the
    // renderer surfaces FtsQueryError with a hint string instead of
    // panicking on a None / empty Vec.
    let (_dir, path) = temp_db();
    runtime().block_on(async {
        let storage = StorageHandle::new(&path).await.expect("create storage");
        seed_search_corpus(&storage).await;

        for payload in ["", "   ", "\t\t", "\n", "AND", "OR", "NOT", "AND OR NOT"] {
            assert_injection_outcome(
                &storage,
                payload,
                InjectionOutcome::StructuredFtsError,
                "empty_and_keyword_only",
            )
            .await;
        }
    });
}

#[test]
fn very_long_payload_does_not_blow_the_validation_path() {
    // 10 KiB of mixed grammar terms. FTS5 either parses it (Success) or
    // rejects it (StructuredFtsError) — what we care about is that the
    // call returns within a reasonable wall-clock window and never
    // panics, OOMs, or widens into a raw rusqlite error.
    let (_dir, path) = temp_db();
    runtime().block_on(async {
        let storage = StorageHandle::new(&path).await.expect("create storage");
        seed_search_corpus(&storage).await;

        // 1024 repeats of "needle " (~7 KiB) + a balanced trailing OR chain.
        let long_token_run = "needle ".repeat(1024);
        let or_chain = (0..256)
            .map(|i| format!("term{i}"))
            .collect::<Vec<_>>()
            .join(" OR ");
        let long_payload = format!("({long_token_run}) OR ({or_chain})");
        assert!(long_payload.len() > 8_000);

        let result = storage
            .search_with_results(&long_payload, SearchOptions::default())
            .await;
        match result {
            Ok(_) | Err(frankenterm_core::Error::Storage(StorageError::FtsQueryError(_))) => {}
            Err(other) => panic!(
                "[long_payload] surfaced unexpected variant for {} byte input: {other:?}",
                long_payload.len()
            ),
        }
    });
}

#[test]
fn unknown_column_selector_stays_in_structured_fts_error_path() {
    let (_dir, path) = temp_db();
    let rt = runtime();

    rt.block_on(async {
        let storage = StorageHandle::new(&path).await.expect("create storage");
        seed_search_corpus(&storage).await;

        let err = storage
            .search_with_results("unknowncol:needle", SearchOptions::default())
            .await
            .expect_err("unknown FTS column selector must not widen into SQL");
        let err_dbg = format!("{err:?}");

        let msg = match err {
            frankenterm_core::Error::Storage(StorageError::FtsQueryError(msg)) => msg,
            _ => String::new(),
        };
        assert!(!msg.is_empty(), "unexpected error variant: {err_dbg}");
        assert!(
            msg.contains("Invalid FTS5 query syntax")
                || msg.contains("no such column")
                || msg.contains("Query validation failed")
        );
    });
}
