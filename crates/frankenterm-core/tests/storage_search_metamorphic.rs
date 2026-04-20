//! Metamorphic tests for `StorageHandle::search_with_results` (FTS5 lexical lane).
//!
//! Metamorphic testing lets us validate the search engine without knowing the
//! "correct" score of any individual query — we only need input/output
//! relations that *must* hold for a sound FTS5 backend. Violations surface
//! tokenizer regressions, BM25 parameter drift, and ordering non-determinism.
//!
//! Relations covered:
//!
//!   MR1 (case insensitivity): under the default FTS5 tokenizer (ascii
//!       lowercase), `search("APPLE")`, `search("apple")`, and `search("Apple")`
//!       over the same corpus must produce identical `SearchResult` lists —
//!       same matching segment ids in the same order, and the same BM25 score
//!       per hit.
//!
//!   MR2 (whitespace normalization): `search("foo bar")` and
//!       `search("foo  bar")` (extra inner space) must return identical
//!       `SearchResult` lists. Excess whitespace is a tokenizer-level
//!       invariant; scores must not drift on spacing alone.
//!
//!   MR3 (idempotence / add-then-remove no-op on lexical score): calling
//!       `search(Q)` twice back-to-back, with no intervening write, must
//!       return `SearchResult` lists with identical segment ids and identical
//!       scores. This is the "add-then-remove = noop" shape restricted to
//!       what the storage surface exposes — any FTS5 state change between
//!       reads would violate idempotence.
//!
//!   MR4 (no-op prune is identity): `prune_segments_before(T)` with `T`
//!       older than every inserted segment must not change search results.
//!       A prune that touches zero rows must leave the FTS5 index — and
//!       therefore every search score — bit-identical.

use frankenterm_core::runtime_compat::{CompatRuntime, RuntimeBuilder};
use frankenterm_core::storage::{
    PaneRecord, SearchOptions, SearchResult, StorageHandle,
};
use tempfile::TempDir;

fn runtime() -> frankenterm_core::runtime_compat::Runtime {
    RuntimeBuilder::current_thread()
        .build()
        .expect("build runtime")
}

fn temp_db() -> (TempDir, String) {
    let dir = TempDir::new().expect("create temp dir");
    let path = dir.path().join("mr.db").to_string_lossy().to_string();
    (dir, path)
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

fn pane(pane_id: u64, ts: i64) -> PaneRecord {
    PaneRecord {
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
    }
}

async fn seed_corpus(storage: &StorageHandle, pane_id: u64) {
    // The corpus deliberately mixes case and repeated tokens so BM25 has
    // non-trivial scoring to compare across queries.
    let ts = now_ms();
    storage.upsert_pane(pane(pane_id, ts)).await.expect("upsert pane");
    for content in [
        "Apple Banana Cherry Apple",
        "banana cherry date",
        "APPLE APPLE ELDERBERRY",
        "fig grape honeydew",
        "cherry cherry cherry",
    ] {
        storage
            .append_segment(pane_id, content, None)
            .await
            .expect("append segment");
    }
}

/// Canonicalize a SearchResult list for comparison across metamorphic pairs.
/// We compare the tuple `(segment.id, score-rounded-to-1e-6)` in the order
/// the search engine returned them, which preserves both which docs matched
/// and their relative ranking.
fn canon(results: &[SearchResult]) -> Vec<(i64, i64)> {
    results
        .iter()
        .map(|r| (r.segment.id, (r.score * 1_000_000.0).round() as i64))
        .collect()
}

fn opts_no_snippets() -> SearchOptions {
    // Snippet rendering depends on the query text itself (the highlighted
    // terms differ between "apple" and "APPLE"), so disable snippets so
    // only the BM25 score + segment identity drive the comparison. This
    // keeps the MR pure: same corpus + tokenizer-equivalent query ⇒
    // identical `SearchResult` projection.
    SearchOptions {
        include_snippets: Some(false),
        ..SearchOptions::default()
    }
}

// ─────────────────────────────────────────────────────────────────────────
// MR1: case insensitivity
// ─────────────────────────────────────────────────────────────────────────

#[test]
fn mr_case_insensitive_search_yields_identical_results() {
    let rt = runtime();
    rt.block_on(async {
        let (_dir, path) = temp_db();
        let storage = StorageHandle::new(&path).await.expect("create storage");
        seed_corpus(&storage, 1).await;

        let opts = opts_no_snippets();
        let lower = storage
            .search_with_results("apple", opts.clone())
            .await
            .expect("search lower");
        let upper = storage
            .search_with_results("APPLE", opts.clone())
            .await
            .expect("search upper");
        let title = storage
            .search_with_results("Apple", opts)
            .await
            .expect("search title");

        assert!(!lower.is_empty(), "corpus must contain 'apple' matches");
        assert_eq!(canon(&lower), canon(&upper), "case should not affect search");
        assert_eq!(canon(&upper), canon(&title), "case should not affect search");
    });
}

// ─────────────────────────────────────────────────────────────────────────
// MR2: whitespace normalization
// ─────────────────────────────────────────────────────────────────────────

#[test]
fn mr_whitespace_normalization_yields_identical_results() {
    let rt = runtime();
    rt.block_on(async {
        let (_dir, path) = temp_db();
        let storage = StorageHandle::new(&path).await.expect("create storage");
        seed_corpus(&storage, 1).await;

        let opts = opts_no_snippets();
        let tight = storage
            .search_with_results("apple cherry", opts.clone())
            .await
            .expect("search tight");
        let spaced = storage
            .search_with_results("apple  cherry", opts.clone())
            .await
            .expect("search spaced");
        let tabbed = storage
            .search_with_results("apple\tcherry", opts)
            .await
            .expect("search tabbed");

        assert!(!tight.is_empty(), "corpus must match 'apple cherry'");
        assert_eq!(canon(&tight), canon(&spaced), "extra space must be absorbed");
        assert_eq!(canon(&tight), canon(&tabbed), "tab must be absorbed");
    });
}

// ─────────────────────────────────────────────────────────────────────────
// MR3: idempotence (== add-then-remove noop for the lexical score)
// ─────────────────────────────────────────────────────────────────────────

#[test]
fn mr_repeated_search_is_idempotent() {
    let rt = runtime();
    rt.block_on(async {
        let (_dir, path) = temp_db();
        let storage = StorageHandle::new(&path).await.expect("create storage");
        seed_corpus(&storage, 1).await;

        let opts = opts_no_snippets();
        let first = storage
            .search_with_results("cherry", opts.clone())
            .await
            .expect("search 1");
        let second = storage
            .search_with_results("cherry", opts.clone())
            .await
            .expect("search 2");
        let third = storage
            .search_with_results("cherry", opts)
            .await
            .expect("search 3");

        assert!(!first.is_empty(), "corpus must match 'cherry'");
        assert_eq!(canon(&first), canon(&second), "search must be idempotent");
        assert_eq!(canon(&second), canon(&third), "search must be idempotent");
    });
}

// ─────────────────────────────────────────────────────────────────────────
// MR4: no-op prune is identity
// ─────────────────────────────────────────────────────────────────────────

#[test]
fn mr_noop_prune_preserves_lexical_scores() {
    let rt = runtime();
    rt.block_on(async {
        let (_dir, path) = temp_db();
        let storage = StorageHandle::new(&path).await.expect("create storage");
        seed_corpus(&storage, 1).await;

        let opts = opts_no_snippets();
        let before = storage
            .search_with_results("banana", opts.clone())
            .await
            .expect("search before prune");
        assert!(!before.is_empty(), "corpus must match 'banana'");

        // Pruning everything strictly older than timestamp 0 deletes nothing:
        // every inserted segment's captured_at is > 0. If the FTS5 index is
        // mutated as a side effect of running an empty prune pass (e.g. a
        // spurious delete+reindex), the post-prune scores will drift.
        let removed = storage
            .prune_segments_before(0)
            .await
            .expect("noop prune");
        assert_eq!(removed, 0, "prune_segments_before(0) must be a no-op");

        let after = storage
            .search_with_results("banana", opts)
            .await
            .expect("search after prune");
        assert_eq!(
            canon(&before),
            canon(&after),
            "no-op prune must leave lexical scores untouched"
        );
    });
}
