//! Lexical search quality harness with golden queries, relevance assertions,
//! and latency budgets.
//!
//! Bead: wa-oegrb.4.6
//!
//! This module provides a structured evaluation framework for the Tantivy
//! lexical search index. It enables:
//!
//! - **Golden query suites**: Predefined queries with expected results derived
//!   from realistic terminal workflows.
//! - **Relevance assertions**: Must-hit document checks and ordering tolerances
//!   for search result quality.
//! - **Latency budgets**: Per-query-class timing constraints to detect
//!   performance regressions.
//! - **CI integration**: Machine-readable pass/fail reports suitable for
//!   automated quality gates.
//!
//! The harness is backend-agnostic: it runs against any [`LexicalSearchService`]
//! implementation, including the in-memory reference service for unit testing.

use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::tantivy_ingest::IndexDocumentFields;
use crate::tantivy_query::{LexicalSearchService, SearchFilter, SearchQuery, SearchResults};

// ---------------------------------------------------------------------------
// Golden query definition
// ---------------------------------------------------------------------------

/// A golden query with expected relevance assertions.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GoldenQuery {
    /// Human-readable name for this test case.
    pub name: String,
    /// The query class (for grouping and latency budget selection).
    pub class: QueryClass,
    /// The search query to execute.
    pub query: SearchQuery,
    /// Relevance assertions that must hold for the results.
    pub assertions: Vec<RelevanceAssertion>,
    /// Optional description of what this query tests.
    #[serde(default)]
    pub description: String,
}

/// Classification of query types for latency budgets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum QueryClass {
    /// Simple single-term lookup (e.g., "error").
    SimpleTerm,
    /// Multi-term boolean query (e.g., "connection refused timeout").
    MultiTerm,
    /// Filtered query with time range or pane filters.
    Filtered,
    /// Forensic query combining text + multiple filters.
    Forensic,
    /// High-cardinality query scanning many documents.
    HighCardinality,
}

/// A relevance assertion on search results.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum RelevanceAssertion {
    /// The result set must contain a document with this event_id.
    MustHit { event_id: String },
    /// The result set must NOT contain a document with this event_id.
    MustNotHit { event_id: String },
    /// The total hit count must be at least this value.
    MinTotalHits(u64),
    /// The total hit count must be at most this value.
    MaxTotalHits(u64),
    /// The total hit count must be exactly this value.
    ExactTotalHits(u64),
    /// A specific event_id must appear within the top N results.
    InTopN { event_id: String, n: usize },
    /// Event A must rank higher (earlier index) than event B in results.
    RankedBefore { higher: String, lower: String },
    /// The first result must have this event_id.
    FirstResult { event_id: String },
    /// All results must pass the given filter.
    AllMatchFilter(SearchFilter),
}

// ---------------------------------------------------------------------------
// Latency budgets
// ---------------------------------------------------------------------------

/// Latency budget per query class.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LatencyBudget {
    /// Maximum allowed duration for this query class.
    pub max_duration: Duration,
    /// Query class this budget applies to.
    pub class: QueryClass,
}

/// Default latency budgets by query class.
pub fn default_latency_budgets() -> Vec<LatencyBudget> {
    vec![
        LatencyBudget {
            class: QueryClass::SimpleTerm,
            max_duration: Duration::from_millis(50),
        },
        LatencyBudget {
            class: QueryClass::MultiTerm,
            max_duration: Duration::from_millis(100),
        },
        LatencyBudget {
            class: QueryClass::Filtered,
            max_duration: Duration::from_millis(100),
        },
        LatencyBudget {
            class: QueryClass::Forensic,
            max_duration: Duration::from_millis(200),
        },
        LatencyBudget {
            class: QueryClass::HighCardinality,
            max_duration: Duration::from_millis(500),
        },
    ]
}

// ---------------------------------------------------------------------------
// Harness runner
// ---------------------------------------------------------------------------

/// Result of running a single golden query.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueryTestResult {
    /// Name of the golden query.
    pub name: String,
    /// Whether all assertions passed.
    pub passed: bool,
    /// Individual assertion results.
    pub assertion_results: Vec<AssertionResult>,
    /// Whether the query met its latency budget.
    pub latency_ok: bool,
    /// Actual query duration.
    pub duration_us: u64,
    /// Latency budget (if applicable).
    pub budget_us: Option<u64>,
    /// Number of hits returned.
    pub hits_returned: u64,
    /// Total hits reported by the service.
    pub total_hits: u64,
    /// Error message if the query itself failed.
    pub error: Option<String>,
}

/// Result of evaluating a single assertion.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AssertionResult {
    /// Human-readable description of the assertion.
    pub description: String,
    /// Whether this assertion passed.
    pub passed: bool,
    /// Diagnostic message on failure.
    pub message: Option<String>,
}

/// Aggregate report from running a full quality suite.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QualityReport {
    /// Individual query results.
    pub results: Vec<QueryTestResult>,
    /// Total queries run.
    pub total_queries: usize,
    /// Queries that passed all assertions.
    pub passed: usize,
    /// Queries that failed at least one assertion.
    pub failed: usize,
    /// Queries that exceeded their latency budget.
    pub latency_violations: usize,
    /// Queries that produced errors.
    pub errors: usize,
    /// Overall pass/fail.
    pub all_passed: bool,
}

/// The quality harness runner.
pub struct QualityHarness {
    /// Golden queries to execute.
    queries: Vec<GoldenQuery>,
    /// Latency budgets by query class.
    budgets: Vec<LatencyBudget>,
}

impl QualityHarness {
    /// Create a harness with the given golden queries and default latency budgets.
    pub fn new(queries: Vec<GoldenQuery>) -> Self {
        Self {
            queries,
            budgets: default_latency_budgets(),
        }
    }

    /// Create a harness with custom latency budgets.
    pub fn with_budgets(queries: Vec<GoldenQuery>, budgets: Vec<LatencyBudget>) -> Self {
        Self { queries, budgets }
    }

    /// Run all golden queries against the given search service.
    pub fn run<S: LexicalSearchService>(&self, service: &S) -> QualityReport {
        let mut results = Vec::with_capacity(self.queries.len());

        for gq in &self.queries {
            let result = self.run_single(service, gq);
            results.push(result);
        }

        let total_queries = results.len();
        let passed = results.iter().filter(|r| r.passed && r.latency_ok).count();
        let failed = results
            .iter()
            .filter(|r| !r.passed && r.error.is_none())
            .count();
        let latency_violations = results.iter().filter(|r| !r.latency_ok).count();
        let errors = results.iter().filter(|r| r.error.is_some()).count();
        let all_passed = passed == total_queries;

        QualityReport {
            results,
            total_queries,
            passed,
            failed,
            latency_violations,
            errors,
            all_passed,
        }
    }

    /// Run a single golden query.
    fn run_single<S: LexicalSearchService>(
        &self,
        service: &S,
        gq: &GoldenQuery,
    ) -> QueryTestResult {
        let budget = self
            .budgets
            .iter()
            .find(|b| b.class == gq.class)
            .map(|b| b.max_duration);

        let start = Instant::now();
        let search_result = service.search(&gq.query);
        let duration = start.elapsed();

        match search_result {
            Ok(results) => {
                let assertion_results: Vec<_> = gq
                    .assertions
                    .iter()
                    .map(|a| evaluate_assertion(a, &results))
                    .collect();

                let all_assertions_pass = assertion_results.iter().all(|r| r.passed);
                let latency_ok = budget.map_or(true, |b| duration <= b);

                QueryTestResult {
                    name: gq.name.clone(),
                    passed: all_assertions_pass,
                    assertion_results,
                    latency_ok,
                    duration_us: duration.as_micros() as u64,
                    budget_us: budget.map(|b| b.as_micros() as u64),
                    hits_returned: results.hits.len() as u64,
                    total_hits: results.total_hits,
                    error: None,
                }
            }
            Err(e) => QueryTestResult {
                name: gq.name.clone(),
                passed: false,
                assertion_results: Vec::new(),
                latency_ok: true,
                duration_us: duration.as_micros() as u64,
                budget_us: budget.map(|b| b.as_micros() as u64),
                hits_returned: 0,
                total_hits: 0,
                error: Some(format!("{e}")),
            },
        }
    }
}

/// Evaluate a single relevance assertion against search results.
fn evaluate_assertion(assertion: &RelevanceAssertion, results: &SearchResults) -> AssertionResult {
    match assertion {
        RelevanceAssertion::MustHit { event_id } => {
            let found = results.hits.iter().any(|h| h.doc.event_id == *event_id);
            AssertionResult {
                description: format!("must contain event_id={event_id}"),
                passed: found,
                message: if found {
                    None
                } else {
                    Some(format!("event_id '{event_id}' not found in results"))
                },
            }
        }
        RelevanceAssertion::MustNotHit { event_id } => {
            let found = results.hits.iter().any(|h| h.doc.event_id == *event_id);
            AssertionResult {
                description: format!("must not contain event_id={event_id}"),
                passed: !found,
                message: if !found {
                    None
                } else {
                    Some(format!(
                        "event_id '{event_id}' unexpectedly found in results"
                    ))
                },
            }
        }
        RelevanceAssertion::MinTotalHits(min) => {
            let ok = results.total_hits >= *min;
            AssertionResult {
                description: format!("total_hits >= {min}"),
                passed: ok,
                message: if ok {
                    None
                } else {
                    Some(format!(
                        "total_hits {} < expected min {}",
                        results.total_hits, min
                    ))
                },
            }
        }
        RelevanceAssertion::MaxTotalHits(max) => {
            let ok = results.total_hits <= *max;
            AssertionResult {
                description: format!("total_hits <= {max}"),
                passed: ok,
                message: if ok {
                    None
                } else {
                    Some(format!(
                        "total_hits {} > expected max {}",
                        results.total_hits, max
                    ))
                },
            }
        }
        RelevanceAssertion::ExactTotalHits(expected) => {
            let ok = results.total_hits == *expected;
            AssertionResult {
                description: format!("total_hits == {expected}"),
                passed: ok,
                message: if ok {
                    None
                } else {
                    Some(format!(
                        "total_hits {} != expected {}",
                        results.total_hits, expected
                    ))
                },
            }
        }
        RelevanceAssertion::InTopN { event_id, n } => {
            let pos = results
                .hits
                .iter()
                .position(|h| h.doc.event_id == *event_id);
            let ok = pos.map_or(false, |p| p < *n);
            AssertionResult {
                description: format!("event_id={event_id} in top {n}"),
                passed: ok,
                message: if ok {
                    None
                } else {
                    match pos {
                        Some(p) => Some(format!(
                            "event_id '{event_id}' at position {} (expected < {n})",
                            p + 1
                        )),
                        None => Some(format!("event_id '{event_id}' not found in results")),
                    }
                },
            }
        }
        RelevanceAssertion::RankedBefore { higher, lower } => {
            let pos_h = results.hits.iter().position(|h| h.doc.event_id == *higher);
            let pos_l = results.hits.iter().position(|h| h.doc.event_id == *lower);

            match (pos_h, pos_l) {
                (Some(h), Some(l)) => {
                    let ok = h < l;
                    AssertionResult {
                        description: format!("{higher} ranked before {lower}"),
                        passed: ok,
                        message: if ok {
                            None
                        } else {
                            Some(format!(
                                "'{higher}' at position {} but '{lower}' at position {}",
                                h + 1,
                                l + 1
                            ))
                        },
                    }
                }
                (None, _) => AssertionResult {
                    description: format!("{higher} ranked before {lower}"),
                    passed: false,
                    message: Some(format!("'{higher}' not found in results")),
                },
                (_, None) => AssertionResult {
                    description: format!("{higher} ranked before {lower}"),
                    passed: false,
                    message: Some(format!("'{lower}' not found in results")),
                },
            }
        }
        RelevanceAssertion::FirstResult { event_id } => {
            let first = results.hits.first().map(|h| &h.doc.event_id);
            let ok = first == Some(event_id);
            AssertionResult {
                description: format!("first result is event_id={event_id}"),
                passed: ok,
                message: if ok {
                    None
                } else {
                    match first {
                        Some(actual) => {
                            Some(format!("first result is '{actual}', expected '{event_id}'"))
                        }
                        None => Some("no results returned".to_string()),
                    }
                },
            }
        }
        RelevanceAssertion::AllMatchFilter(filter) => {
            let failures: Vec<_> = results
                .hits
                .iter()
                .filter(|h| !filter.matches(&h.doc))
                .map(|h| h.doc.event_id.clone())
                .collect();
            let ok = failures.is_empty();
            AssertionResult {
                description: format!("all results match filter {:?}", filter),
                passed: ok,
                message: if ok {
                    None
                } else {
                    Some(format!(
                        "{} results don't match filter: {:?}",
                        failures.len(),
                        &failures[..failures.len().min(5)]
                    ))
                },
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Built-in golden query suites
// ---------------------------------------------------------------------------

/// Terminal forensic scenario: operator searching for error patterns.
///
/// Queries use terms that appear in the `build_forensic_corpus()` output.
pub fn forensic_golden_queries() -> Vec<GoldenQuery> {
    vec![
        GoldenQuery {
            name: "error_keyword_search".to_string(),
            class: QueryClass::SimpleTerm,
            query: SearchQuery::simple("error"),
            assertions: vec![
                // "out-error-2" has "error: connection timed out"
                // "out-build-error" has "error[E0308]"
                RelevanceAssertion::MinTotalHits(2),
                RelevanceAssertion::MustHit {
                    event_id: "out-build-error".to_string(),
                },
            ],
            description: "Basic error keyword search finds compiler and runtime errors".to_string(),
        },
        GoldenQuery {
            name: "connection_refused_multi_term".to_string(),
            class: QueryClass::MultiTerm,
            query: SearchQuery::simple("connection refused"),
            assertions: vec![
                RelevanceAssertion::MinTotalHits(1),
                RelevanceAssertion::MustHit {
                    event_id: "out-error".to_string(),
                },
            ],
            description: "Multi-term search for connection errors".to_string(),
        },
        GoldenQuery {
            name: "pane_filtered_search".to_string(),
            class: QueryClass::Filtered,
            query: SearchQuery::simple("Compiling")
                .with_filter(SearchFilter::PaneId { values: vec![42] }),
            assertions: vec![
                RelevanceAssertion::MinTotalHits(1),
                RelevanceAssertion::AllMatchFilter(SearchFilter::PaneId { values: vec![42] }),
            ],
            description: "Filtered search scoped to a specific pane".to_string(),
        },
        GoldenQuery {
            name: "egress_event_type_filter".to_string(),
            class: QueryClass::Filtered,
            query: SearchQuery::simple("Compiling").with_filter(SearchFilter::EventType {
                values: vec!["egress_output".to_string()],
            }),
            assertions: vec![
                RelevanceAssertion::MinTotalHits(1),
                RelevanceAssertion::AllMatchFilter(SearchFilter::EventType {
                    values: vec!["egress_output".to_string()],
                }),
            ],
            description: "Filter by event type ensures only egress results".to_string(),
        },
    ]
}

/// Agent workflow scenario: searching for command outputs and agent actions.
pub fn agent_workflow_golden_queries() -> Vec<GoldenQuery> {
    vec![
        GoldenQuery {
            name: "cargo_test_command".to_string(),
            class: QueryClass::SimpleTerm,
            query: SearchQuery::simple("cargo test"),
            assertions: vec![
                RelevanceAssertion::MinTotalHits(1),
                RelevanceAssertion::MustHit {
                    event_id: "cmd-cargo-test".to_string(),
                },
            ],
            description: "Search for cargo test invocations".to_string(),
        },
        GoldenQuery {
            name: "git_commit_ingress".to_string(),
            class: QueryClass::Filtered,
            query: SearchQuery::simple("git commit").with_filter(SearchFilter::EventType {
                values: vec!["ingress_text".to_string()],
            }),
            assertions: vec![
                RelevanceAssertion::MinTotalHits(1),
                RelevanceAssertion::MustHit {
                    event_id: "cmd-git-commit".to_string(),
                },
                RelevanceAssertion::AllMatchFilter(SearchFilter::EventType {
                    values: vec!["ingress_text".to_string()],
                }),
            ],
            description: "Search for git commit commands (ingress only)".to_string(),
        },
    ]
}

/// Corpus builder: creates a realistic document set for golden query evaluation.
///
/// Returns a vector of `IndexDocumentFields` representing a terminal session
/// with mixed ingress/egress, errors, and agent commands.
pub fn build_forensic_corpus() -> Vec<IndexDocumentFields> {
    let mut docs = Vec::new();
    let mut seq = 0u64;

    let mut make_doc = |event_id: &str,
                        pane_id: u64,
                        event_type: &str,
                        text: &str,
                        source: &str|
     -> IndexDocumentFields {
        let s = seq;
        seq += 1;
        IndexDocumentFields {
            schema_version: "ft.recorder.event.v1".to_string(),
            lexical_schema_version: "ft.recorder.lexical.v1".to_string(),
            event_id: event_id.to_string(),
            pane_id,
            session_id: Some("sess-forensic".to_string()),
            workflow_id: None,
            correlation_id: None,
            parent_event_id: None,
            trigger_event_id: None,
            root_event_id: None,
            source: source.to_string(),
            event_type: event_type.to_string(),
            ingress_kind: if event_type == "ingress_text" {
                Some("send_text".to_string())
            } else {
                None
            },
            segment_kind: if event_type == "egress_output" {
                Some("delta".to_string())
            } else {
                None
            },
            control_marker_type: None,
            lifecycle_phase: None,
            is_gap: false,
            redaction: Some("none".to_string()),
            occurred_at_ms: 1_700_000_000_000i64 + s as i64 * 100,
            recorded_at_ms: 1_700_000_000_001i64 + s as i64 * 100,
            sequence: s,
            log_offset: s,
            text: text.to_string(),
            text_symbols: text.to_string(),
            details_json: "{}".to_string(),
        }
    };

    // Pane 42: Agent running cargo test
    docs.push(make_doc(
        "cmd-cargo-test",
        42,
        "ingress_text",
        "cargo test -p frankenterm-core",
        "robot_mode",
    ));
    docs.push(make_doc(
        "out-compiling",
        42,
        "egress_output",
        "   Compiling frankenterm-core v0.1.0\n",
        "wezterm_mux",
    ));
    docs.push(make_doc(
        "out-test-ok",
        42,
        "egress_output",
        "test result: ok. 47 passed; 0 failed\n",
        "wezterm_mux",
    ));

    // Pane 42: git operations
    docs.push(make_doc(
        "cmd-git-commit",
        42,
        "ingress_text",
        "git commit -m \"feat: add new module\"",
        "robot_mode",
    ));
    docs.push(make_doc(
        "out-git-commit",
        42,
        "egress_output",
        "[main abc1234] feat: add new module\n 2 files changed, 100 insertions(+)\n",
        "wezterm_mux",
    ));

    // Pane 10: Error scenario
    docs.push(make_doc(
        "cmd-curl",
        10,
        "ingress_text",
        "curl http://localhost:8080/api/health",
        "operator_action",
    ));
    docs.push(make_doc(
        "out-error",
        10,
        "egress_output",
        "curl: (7) Failed to connect to localhost port 8080: Connection refused\n",
        "wezterm_mux",
    ));
    docs.push(make_doc(
        "out-error-2",
        10,
        "egress_output",
        "error: connection timed out after 30s\n",
        "wezterm_mux",
    ));

    // Pane 10: Another error with different text
    docs.push(make_doc(
        "cmd-npm",
        10,
        "ingress_text",
        "npm install",
        "operator_action",
    ));
    docs.push(make_doc(
        "out-npm-error",
        10,
        "egress_output",
        "npm ERR! code ENOENT\nnpm ERR! syscall open\n",
        "wezterm_mux",
    ));

    // Pane 42: More agent output
    docs.push(make_doc(
        "cmd-cargo-build",
        42,
        "ingress_text",
        "cargo build --release",
        "robot_mode",
    ));
    docs.push(make_doc(
        "out-build-warn",
        42,
        "egress_output",
        "warning: unused variable: `x`\n  --> src/main.rs:10:9\n",
        "wezterm_mux",
    ));
    docs.push(make_doc(
        "out-build-error",
        42,
        "egress_output",
        "error[E0308]: mismatched types\n  --> src/lib.rs:42:5\n",
        "wezterm_mux",
    ));

    docs
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tantivy_query::InMemorySearchService;

    fn corpus_service() -> InMemorySearchService {
        InMemorySearchService::from_docs(build_forensic_corpus())
    }

    // =========================================================================
    // Assertion evaluation tests
    // =========================================================================

    #[test]
    fn must_hit_passes_when_present() {
        let svc = corpus_service();
        // "out-build-error" has text "error[E0308]: mismatched types"
        let results = svc.search(&SearchQuery::simple("error")).unwrap();
        let r = evaluate_assertion(
            &RelevanceAssertion::MustHit {
                event_id: "out-build-error".to_string(),
            },
            &results,
        );
        assert!(r.passed);
    }

    #[test]
    fn must_hit_fails_when_missing() {
        let svc = corpus_service();
        let results = svc.search(&SearchQuery::simple("error")).unwrap();
        let r = evaluate_assertion(
            &RelevanceAssertion::MustHit {
                event_id: "nonexistent".to_string(),
            },
            &results,
        );
        assert!(!r.passed);
        assert!(r.message.is_some());
    }

    #[test]
    fn must_not_hit_passes_when_absent() {
        let svc = corpus_service();
        // "cmd-cargo-test" text is "cargo test -p frankenterm-core" — no "error"
        let results = svc.search(&SearchQuery::simple("error")).unwrap();
        let r = evaluate_assertion(
            &RelevanceAssertion::MustNotHit {
                event_id: "cmd-cargo-test".to_string(),
            },
            &results,
        );
        assert!(r.passed);
    }

    #[test]
    fn must_not_hit_fails_when_present() {
        let svc = corpus_service();
        // "out-build-error" has "error[E0308]" — it will be in results
        let results = svc.search(&SearchQuery::simple("error")).unwrap();
        let r = evaluate_assertion(
            &RelevanceAssertion::MustNotHit {
                event_id: "out-build-error".to_string(),
            },
            &results,
        );
        assert!(!r.passed);
    }

    #[test]
    fn min_total_hits_check() {
        let svc = corpus_service();
        let results = svc.search(&SearchQuery::simple("error")).unwrap();
        let r = evaluate_assertion(&RelevanceAssertion::MinTotalHits(1), &results);
        assert!(r.passed);

        let r2 = evaluate_assertion(&RelevanceAssertion::MinTotalHits(999), &results);
        assert!(!r2.passed);
    }

    #[test]
    fn max_total_hits_check() {
        let svc = corpus_service();
        let results = svc.search(&SearchQuery::simple("error")).unwrap();
        let r = evaluate_assertion(&RelevanceAssertion::MaxTotalHits(999), &results);
        assert!(r.passed);

        let r2 = evaluate_assertion(&RelevanceAssertion::MaxTotalHits(0), &results);
        assert!(!r2.passed);
    }

    #[test]
    fn exact_total_hits_check() {
        let svc = corpus_service();
        let results = svc
            .search(
                &SearchQuery::simple("cargo test")
                    .with_filter(SearchFilter::PaneId { values: vec![42] }),
            )
            .unwrap();

        // Count how many hits we got
        let n = results.total_hits;
        let r = evaluate_assertion(&RelevanceAssertion::ExactTotalHits(n), &results);
        assert!(r.passed);

        let r2 = evaluate_assertion(&RelevanceAssertion::ExactTotalHits(n + 1), &results);
        assert!(!r2.passed);
    }

    #[test]
    fn in_top_n_check() {
        let svc = corpus_service();
        let results = svc.search(&SearchQuery::simple("error")).unwrap();
        assert!(!results.hits.is_empty());

        let first_id = results.hits[0].doc.event_id.clone();
        let r = evaluate_assertion(
            &RelevanceAssertion::InTopN {
                event_id: first_id.clone(),
                n: 1,
            },
            &results,
        );
        assert!(r.passed);

        // Nonexistent event not in top N
        let r2 = evaluate_assertion(
            &RelevanceAssertion::InTopN {
                event_id: "nonexistent".to_string(),
                n: 10,
            },
            &results,
        );
        assert!(!r2.passed);
    }

    #[test]
    fn ranked_before_check() {
        let svc = corpus_service();
        let results = svc.search(&SearchQuery::simple("error")).unwrap();
        assert!(results.hits.len() >= 2);

        let first = results.hits[0].doc.event_id.clone();
        let second = results.hits[1].doc.event_id.clone();

        let r = evaluate_assertion(
            &RelevanceAssertion::RankedBefore {
                higher: first.clone(),
                lower: second.clone(),
            },
            &results,
        );
        assert!(r.passed);

        // Reversed should fail
        let r2 = evaluate_assertion(
            &RelevanceAssertion::RankedBefore {
                higher: second,
                lower: first,
            },
            &results,
        );
        assert!(!r2.passed);
    }

    #[test]
    fn first_result_check() {
        let svc = corpus_service();
        let results = svc.search(&SearchQuery::simple("error")).unwrap();
        let first_id = results.hits[0].doc.event_id.clone();

        let r = evaluate_assertion(
            &RelevanceAssertion::FirstResult { event_id: first_id },
            &results,
        );
        assert!(r.passed);

        let r2 = evaluate_assertion(
            &RelevanceAssertion::FirstResult {
                event_id: "wrong".to_string(),
            },
            &results,
        );
        assert!(!r2.passed);
    }

    #[test]
    fn all_match_filter_check() {
        let svc = corpus_service();
        // "Compiling" appears in pane 42's egress output
        let results = svc
            .search(
                &SearchQuery::simple("Compiling")
                    .with_filter(SearchFilter::PaneId { values: vec![42] }),
            )
            .unwrap();

        let r = evaluate_assertion(
            &RelevanceAssertion::AllMatchFilter(SearchFilter::PaneId { values: vec![42] }),
            &results,
        );
        assert!(r.passed);

        // Wrong pane should fail
        let r2 = evaluate_assertion(
            &RelevanceAssertion::AllMatchFilter(SearchFilter::PaneId { values: vec![999] }),
            &results,
        );
        assert!(!r2.passed);
    }

    // =========================================================================
    // Harness runner tests
    // =========================================================================

    #[test]
    fn harness_runs_forensic_suite() {
        let svc = corpus_service();
        let queries = forensic_golden_queries();
        let harness = QualityHarness::new(queries);
        let report = harness.run(&svc);

        assert!(report.all_passed, "forensic suite failed: {:#?}", report);
        assert_eq!(report.errors, 0);
        assert_eq!(report.latency_violations, 0);
    }

    #[test]
    fn harness_runs_agent_workflow_suite() {
        let svc = corpus_service();
        let queries = agent_workflow_golden_queries();
        let harness = QualityHarness::new(queries);
        let report = harness.run(&svc);

        assert!(
            report.all_passed,
            "agent workflow suite failed: {:#?}",
            report
        );
    }

    #[test]
    fn harness_detects_missing_hits() {
        let svc = InMemorySearchService::new(); // empty index
        let queries = vec![GoldenQuery {
            name: "should_fail".to_string(),
            class: QueryClass::SimpleTerm,
            query: SearchQuery::simple("nonexistent")
                .with_filter(SearchFilter::PaneId { values: vec![1] }), // needs at least a filter to avoid empty query error
            assertions: vec![RelevanceAssertion::MustHit {
                event_id: "missing".to_string(),
            }],
            description: "This should fail".to_string(),
        }];

        let harness = QualityHarness::new(queries);
        let report = harness.run(&svc);

        assert!(!report.all_passed);
        assert_eq!(report.failed, 1);
    }

    #[test]
    fn harness_detects_latency_violation() {
        let svc = corpus_service();
        // Set an impossibly low latency budget
        let budgets = vec![LatencyBudget {
            class: QueryClass::SimpleTerm,
            max_duration: Duration::from_nanos(1), // 1 nanosecond — impossible
        }];
        let queries = vec![GoldenQuery {
            name: "latency_test".to_string(),
            class: QueryClass::SimpleTerm,
            query: SearchQuery::simple("error"),
            assertions: vec![RelevanceAssertion::MinTotalHits(1)],
            description: "This should exceed latency budget".to_string(),
        }];

        let harness = QualityHarness::with_budgets(queries, budgets);
        let report = harness.run(&svc);

        // Assertions pass but latency fails
        assert!(!report.all_passed);
        assert_eq!(report.latency_violations, 1);
        assert!(report.results[0].passed); // assertions still pass
        assert!(!report.results[0].latency_ok);
    }

    #[test]
    fn harness_handles_search_errors() {
        let svc = InMemorySearchService::new();
        let queries = vec![GoldenQuery {
            name: "error_query".to_string(),
            class: QueryClass::SimpleTerm,
            query: SearchQuery::simple(""), // empty query + no filters = error
            assertions: vec![],
            description: "Empty query should error".to_string(),
        }];

        let harness = QualityHarness::new(queries);
        let report = harness.run(&svc);

        assert!(!report.all_passed);
        assert_eq!(report.errors, 1);
        assert!(report.results[0].error.is_some());
    }

    // =========================================================================
    // Quality report tests
    // =========================================================================

    #[test]
    fn quality_report_serialization_roundtrip() {
        let report = QualityReport {
            results: vec![QueryTestResult {
                name: "test".to_string(),
                passed: true,
                assertion_results: vec![AssertionResult {
                    description: "min hits".to_string(),
                    passed: true,
                    message: None,
                }],
                latency_ok: true,
                duration_us: 500,
                budget_us: Some(50000),
                hits_returned: 5,
                total_hits: 5,
                error: None,
            }],
            total_queries: 1,
            passed: 1,
            failed: 0,
            latency_violations: 0,
            errors: 0,
            all_passed: true,
        };

        let json = serde_json::to_string(&report).unwrap();
        let deser: QualityReport = serde_json::from_str(&json).unwrap();
        assert_eq!(deser.all_passed, report.all_passed);
        assert_eq!(deser.total_queries, report.total_queries);
    }

    #[test]
    fn golden_query_serialization_roundtrip() {
        let gq = GoldenQuery {
            name: "test".to_string(),
            class: QueryClass::Forensic,
            query: SearchQuery::simple("error"),
            assertions: vec![
                RelevanceAssertion::MustHit {
                    event_id: "e1".to_string(),
                },
                RelevanceAssertion::MinTotalHits(3),
                RelevanceAssertion::InTopN {
                    event_id: "e1".to_string(),
                    n: 5,
                },
            ],
            description: "test query".to_string(),
        };

        let json = serde_json::to_string(&gq).unwrap();
        let deser: GoldenQuery = serde_json::from_str(&json).unwrap();
        assert_eq!(deser.name, gq.name);
        assert_eq!(deser.class, gq.class);
    }

    // =========================================================================
    // Default latency budgets
    // =========================================================================

    #[test]
    fn default_budgets_cover_all_classes() {
        let budgets = default_latency_budgets();
        let classes = [
            QueryClass::SimpleTerm,
            QueryClass::MultiTerm,
            QueryClass::Filtered,
            QueryClass::Forensic,
            QueryClass::HighCardinality,
        ];
        for class in &classes {
            assert!(
                budgets.iter().any(|b| b.class == *class),
                "missing budget for {:?}",
                class
            );
        }
    }

    #[test]
    fn budgets_are_monotonically_increasing() {
        let budgets = default_latency_budgets();
        // Simple < MultiTerm <= Filtered <= Forensic <= HighCardinality
        let simple = budgets
            .iter()
            .find(|b| b.class == QueryClass::SimpleTerm)
            .unwrap();
        let high_card = budgets
            .iter()
            .find(|b| b.class == QueryClass::HighCardinality)
            .unwrap();
        assert!(simple.max_duration <= high_card.max_duration);
    }

    // =========================================================================
    // Corpus builder
    // =========================================================================

    #[test]
    fn forensic_corpus_is_non_empty() {
        let corpus = build_forensic_corpus();
        assert!(!corpus.is_empty());
        assert!(corpus.len() >= 10);
    }

    #[test]
    fn forensic_corpus_has_unique_event_ids() {
        let corpus = build_forensic_corpus();
        let mut ids: Vec<&str> = corpus.iter().map(|d| d.event_id.as_str()).collect();
        let len = ids.len();
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), len, "duplicate event_ids in corpus");
    }

    #[test]
    fn forensic_corpus_has_mixed_types() {
        let corpus = build_forensic_corpus();
        let has_ingress = corpus.iter().any(|d| d.event_type == "ingress_text");
        let has_egress = corpus.iter().any(|d| d.event_type == "egress_output");
        assert!(has_ingress);
        assert!(has_egress);
    }

    #[test]
    fn forensic_corpus_has_multiple_panes() {
        let corpus = build_forensic_corpus();
        let pane_ids: std::collections::HashSet<u64> = corpus.iter().map(|d| d.pane_id).collect();
        assert!(pane_ids.len() >= 2);
    }

    #[test]
    fn forensic_corpus_has_errors() {
        let corpus = build_forensic_corpus();
        let has_error = corpus
            .iter()
            .any(|d| d.text.to_lowercase().contains("error"));
        assert!(has_error);
    }

    // =========================================================================
    // Full suite integration
    // =========================================================================

    #[test]
    fn full_forensic_suite_passes_on_corpus() {
        let svc = corpus_service();
        let mut all_queries = forensic_golden_queries();
        all_queries.extend(agent_workflow_golden_queries());

        let harness = QualityHarness::new(all_queries);
        let report = harness.run(&svc);

        for r in &report.results {
            if !r.passed {
                for a in &r.assertion_results {
                    if !a.passed {
                        panic!(
                            "Query '{}' failed assertion '{}': {}",
                            r.name,
                            a.description,
                            a.message.as_deref().unwrap_or("no message")
                        );
                    }
                }
            }
        }

        assert!(report.all_passed);
        assert_eq!(report.errors, 0);
    }
}
