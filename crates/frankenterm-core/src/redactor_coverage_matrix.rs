//! Recall/precision matrix for the secret redactor
//! ([BR-RC-SAFETY-PROOFS.G10] / `ft-x0666.2`).
//!
//! redactor.rs ships 32 regex patterns covering OpenAI,
//! Anthropic, GitHub, Google, Slack, Stripe, AWS, generic
//! key/token/secret, etc. Coverage is *claimed* but, until
//! this module shipped, not measured. The bead's headline
//! requirement:
//!
//! > Industry standard is to publish recall+precision against
//! > a public test corpus so users can calibrate trust.
//! > ≥99% recall on gitleaks corpus; fail CI on dip.
//!
//! ## What this module ships
//!
//! - [`RedactorTestVector`] — one labeled corpus row: input
//!   bytes + the expected pattern matches (or empty for a
//!   negative).
//! - [`MatchOutcome`] — TP / FN / FP / partial classification of
//!   evaluating one vector.
//! - [`evaluate_vector`] — traces actual production replacements and
//!   requires complete coverage of every expected secret byte.
//! - [`MatrixSnapshot`] — per-provider TP/FP/FN/TN counters +
//!   recall + precision + per-vector results.
//! - [`RedactorCoverageHealth`] — `ft doctor` counter snapshot
//!   matching this session's `*Health` shape.
//! - [`synthesized_corpus`] — in-tree corpus covering each of
//!   the live redactor patterns with at least 3 positives plus
//!   targeted lookalike negatives.
//!   Values are synthetic — random byte sequences shaped like
//!   the format. None are real credentials.
//! - JSONL render/parse helpers for the per-release coverage
//!   report.
//!
//! ## What this module is NOT
//!
//! - Vendored gitleaks / trufflehog corpora. Those have
//!   licensing implications that need operator sign-off; this
//!   module establishes the contract slot they fit into. When
//!   vendored, additional `RedactorTestVector` rows append to
//!   the corpus and the harness re-runs unchanged.
//! - The Fano's-inequality sample-size derivation. That's
//!   methodology written up in
//!   `docs/security/redactor-recall-derivation.md`. The
//!   harness publishes the derived sample floor in the
//!   per-release report.
//! - The CI gate. The bead's "≥99% recall floor; fail CI on
//!   dip" is a property test in
//!   `tests/redactor_coverage_matrix.rs`.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::redactor::Redactor;

// ============================================================================
// Test vector
// ============================================================================

/// Expected match within a test vector input. Use `start` /
/// `end` byte indices into the input; `pattern_name` MUST be
/// one of the names in `SECRET_PATTERNS` (e.g.,
/// `"openai_key"`). Span correctness is checked structurally —
/// the harness tolerates any superset match (i.e., production
/// regex matching a wider range than `start..end` is still a
/// True Positive as long as `[start, end)` is contained).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ExpectedMatch {
    pub pattern_name: String,
    pub start: u32,
    pub end: u32,
}

/// One labeled corpus row.
///
/// - **Positive vector:** non-empty `expected_matches`. Every
///   span MUST be detected by the redactor; missing spans are
///   False Negatives.
/// - **Negative vector:** empty `expected_matches`. Any
///   detection is a False Positive.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RedactorTestVector {
    /// Stable name for the row (e.g., `"openai_key_basic"`).
    pub name: String,
    /// Input bytes — what the redactor sees.
    pub input: String,
    /// Expected detections; empty = negative vector.
    pub expected_matches: Vec<ExpectedMatch>,
    /// Provider category for per-provider breakdown
    /// (e.g., `"openai"`, `"anthropic"`, `"generic"`).
    pub provider: String,
    /// Why this vector exists — what bug class / corpus it
    /// targets.
    pub rationale: String,
}

// ============================================================================
// Outcome classification
// ============================================================================

/// Per-detection classification when evaluating a vector.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MatchOutcome {
    /// Production matched an expected span.
    TruePositive,
    /// Expected span the production failed to match. Counts
    /// against recall.
    FalseNegative,
    /// Production matched a span the test vector says is
    /// clean. Counts against precision.
    FalsePositive,
    /// Replacement removes only part of an expected secret. The expected
    /// span remains a false negative until every byte is covered.
    PartialCoverage,
}

/// Per-vector evaluation summary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VectorEvaluation {
    pub vector_name: String,
    pub provider: String,
    pub true_positives: u32,
    pub false_negatives: u32,
    pub false_positives: u32,
    pub per_detection: Vec<DetectionRecord>,
    /// Invalid fixture or replacement metadata. Such a vector never passes
    /// a coverage gate, even if its other spans were fully redacted.
    pub validation_errors: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DetectionRecord {
    pub pattern_name: String,
    pub start: u32,
    pub end: u32,
    pub outcome: MatchOutcome,
}

/// Run the redactor against `vector` and classify each
/// detection. Coverage is computed at the **span level**
/// (what matters for redaction is that the secret bytes get
/// covered, regardless of which pattern caught them — a
/// `generic_secret` regex catching an `openai_key` still
/// redacts it):
///
/// - For each expected match: TP only if the union of actual replacement
///   source intervals covers every byte; any surviving byte is an FN.
/// - Replacement intervals overlapping no expected match are FP. Partial
///   coverage is annotated without crediting a TP.
/// - Empty, out-of-bounds, reversed, or non-UTF-8-boundary intervals are
///   invalid evidence and fail the gate explicitly.
#[must_use]
pub fn evaluate_vector(vector: &RedactorTestVector) -> VectorEvaluation {
    let trace = Redactor::new().redact_with_replacement_spans(&vector.input);
    classify_replacement_spans(vector, &trace.replacements)
}

fn classify_replacement_spans(
    vector: &RedactorTestVector,
    replacements: &[(&str, usize, usize)],
) -> VectorEvaluation {
    let mut validation_errors = Vec::new();
    if u32::try_from(vector.input.len()).is_err() {
        validation_errors.push("input length exceeds the corpus offset range".to_string());
    }
    let valid_span = |start: usize, end: usize| {
        start < end
            && end <= vector.input.len()
            && vector.input.is_char_boundary(start)
            && vector.input.is_char_boundary(end)
    };
    for (index, expected) in vector.expected_matches.iter().enumerate() {
        if !valid_span(expected.start as usize, expected.end as usize) {
            validation_errors.push(format!(
                "expected_matches[{index}] has an invalid byte interval"
            ));
        }
    }
    for (index, (_, start, end)) in replacements.iter().enumerate() {
        if !valid_span(*start, *end) {
            validation_errors.push(format!(
                "replacements[{index}] has an invalid byte interval"
            ));
        }
    }
    if !validation_errors.is_empty() {
        return VectorEvaluation {
            vector_name: vector.name.clone(),
            provider: vector.provider.clone(),
            true_positives: 0,
            false_negatives: u32::try_from(vector.expected_matches.len()).unwrap_or(u32::MAX),
            false_positives: 0,
            per_detection: Vec::new(),
            validation_errors,
        };
    }

    let mut intervals = replacements
        .iter()
        .map(|(_, start, end)| (*start as u32, *end as u32))
        .collect::<Vec<_>>();
    intervals.sort_unstable();
    let covered = vector
        .expected_matches
        .iter()
        .map(|expected| span_is_fully_covered(expected.start, expected.end, &intervals))
        .collect::<Vec<_>>();
    let mut per_detection = Vec::new();
    let tp = u32::try_from(covered.iter().filter(|covered| **covered).count()).unwrap_or(u32::MAX);
    let mut fp = 0u32;
    for (name, start, end) in replacements {
        let start_u32 = *start as u32;
        let end_u32 = *end as u32;
        let mut hits_expected = false;
        let mut hits_covered = false;
        for (idx, exp) in vector.expected_matches.iter().enumerate() {
            if spans_overlap(exp.start, exp.end, start_u32, end_u32) {
                hits_expected = true;
                hits_covered |= covered[idx];
            }
        }

        let outcome = if hits_covered {
            MatchOutcome::TruePositive
        } else if hits_expected {
            MatchOutcome::PartialCoverage
        } else {
            fp += 1;
            MatchOutcome::FalsePositive
        };

        per_detection.push(DetectionRecord {
            pattern_name: (*name).to_string(),
            start: start_u32,
            end: end_u32,
            outcome,
        });
    }

    let mut fns = 0u32;
    for (idx, exp) in vector.expected_matches.iter().enumerate() {
        if !covered[idx] {
            fns += 1;
            per_detection.push(DetectionRecord {
                pattern_name: exp.pattern_name.clone(),
                start: exp.start,
                end: exp.end,
                outcome: MatchOutcome::FalseNegative,
            });
        }
    }

    VectorEvaluation {
        vector_name: vector.name.clone(),
        provider: vector.provider.clone(),
        true_positives: tp,
        false_negatives: fns,
        false_positives: fp,
        per_detection,
        validation_errors,
    }
}

/// `intervals` must be sorted by start. Adjacent and overlapping spans may
/// jointly cover the expected interval; a one-byte hole is still a miss.
fn span_is_fully_covered(start: u32, end: u32, intervals: &[(u32, u32)]) -> bool {
    let mut covered_until = start;
    for &(replacement_start, replacement_end) in intervals {
        if replacement_start > covered_until {
            break;
        }
        covered_until = covered_until.max(replacement_end);
        if covered_until >= end {
            return true;
        }
    }
    false
}

fn spans_overlap(a_start: u32, a_end: u32, b_start: u32, b_end: u32) -> bool {
    a_start < b_end && b_start < a_end
}

// ============================================================================
// Per-provider snapshot
// ============================================================================

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderCounters {
    pub true_positives: u32,
    pub false_negatives: u32,
    pub false_positives: u32,
    pub vectors_evaluated: u32,
}

impl ProviderCounters {
    /// Recall = TP / (TP + FN). Returns 1.0 when both are 0
    /// (vacuously perfect — no positives expected).
    #[must_use]
    pub fn recall(&self) -> f64 {
        let denom = self.true_positives + self.false_negatives;
        if denom == 0 {
            return 1.0;
        }
        self.true_positives as f64 / denom as f64
    }

    /// Precision = TP / (TP + FP). Returns 1.0 when both are 0
    /// (vacuously perfect — no detections expected).
    #[must_use]
    pub fn precision(&self) -> f64 {
        let denom = self.true_positives + self.false_positives;
        if denom == 0 {
            return 1.0;
        }
        self.true_positives as f64 / denom as f64
    }
}

/// Aggregate result of running the matrix.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MatrixSnapshot {
    pub vectors_total: u32,
    pub overall: ProviderCounters,
    pub by_provider: BTreeMap<String, ProviderCounters>,
    pub vectors: Vec<VectorEvaluation>,
}

impl MatrixSnapshot {
    /// Run the full corpus through the redactor.
    #[must_use]
    pub fn evaluate(corpus: &[RedactorTestVector]) -> Self {
        let mut snap = Self {
            vectors_total: 0,
            overall: ProviderCounters::default(),
            by_provider: BTreeMap::new(),
            vectors: Vec::with_capacity(corpus.len()),
        };

        for vec in corpus {
            let eval = evaluate_vector(vec);
            snap.vectors_total += 1;
            snap.overall.vectors_evaluated += 1;
            snap.overall.true_positives += eval.true_positives;
            snap.overall.false_negatives += eval.false_negatives;
            snap.overall.false_positives += eval.false_positives;

            let prov = snap.by_provider.entry(eval.provider.clone()).or_default();
            prov.vectors_evaluated += 1;
            prov.true_positives += eval.true_positives;
            prov.false_negatives += eval.false_negatives;
            prov.false_positives += eval.false_positives;

            snap.vectors.push(eval);
        }

        snap
    }

    /// Whether every provider clears the recall floor.
    #[must_use]
    pub fn meets_recall_floor(&self, floor: f64) -> bool {
        self.vectors_total > 0
            && floor.is_finite()
            && (0.0..=1.0).contains(&floor)
            && self
                .vectors
                .iter()
                .all(|vector| vector.validation_errors.is_empty())
            && self.by_provider.values().all(|p| p.recall() >= floor)
    }

    /// Lowest per-provider recall — useful for CI failure
    /// messages.
    #[must_use]
    pub fn min_provider_recall(&self) -> Option<(String, f64)> {
        self.by_provider
            .iter()
            .map(|(name, c)| (name.clone(), c.recall()))
            .min_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal))
    }
}

// ============================================================================
// Health snapshot
// ============================================================================

/// `ft doctor` counter snapshot for the redactor coverage
/// matrix. Mirrors the `*Health` shape used across this session
/// (a11y_tree, color_management, atlas_stability, triple_buffer,
/// live_resize, render_quality, snap_back_fuzz,
/// wayland_frame_pacing, bidi_correctness, tx_killswitch_model,
/// passive_watch_invariant, wire_dedup_model).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RedactorCoverageHealth {
    pub vectors_evaluated_total: u64,
    pub true_positives_total: u64,
    pub false_negatives_total: u64,
    pub false_positives_total: u64,
    pub providers_below_recall_floor: u32,
}

impl RedactorCoverageHealth {
    #[must_use]
    pub const fn baseline() -> Self {
        Self {
            vectors_evaluated_total: 0,
            true_positives_total: 0,
            false_negatives_total: 0,
            false_positives_total: 0,
            providers_below_recall_floor: 0,
        }
    }

    /// True iff redactor coverage has been measured AND every
    /// provider clears the recall floor.
    ///
    /// Per ft-cy273 fix: the prior implementation reported
    /// `is_safe == true` on cold start (before any vector had
    /// been evaluated) because `providers_below_recall_floor`
    /// is zero by construction in `baseline()`. Doctor would
    /// then surface the redactor as green even when the coverage
    /// probe had never been wired or had silently failed to run.
    /// We now require at least one evaluated vector before the
    /// safe verdict is granted.
    #[must_use]
    pub const fn is_safe(&self) -> bool {
        self.vectors_evaluated_total > 0 && self.providers_below_recall_floor == 0
    }

    #[must_use]
    pub fn overall_recall(&self) -> f64 {
        let denom = self.true_positives_total + self.false_negatives_total;
        if denom == 0 {
            return 1.0;
        }
        self.true_positives_total as f64 / denom as f64
    }

    #[must_use]
    pub fn overall_precision(&self) -> f64 {
        let denom = self.true_positives_total + self.false_positives_total;
        if denom == 0 {
            return 1.0;
        }
        self.true_positives_total as f64 / denom as f64
    }
}

/// Fold a `MatrixSnapshot` into a health rollup.
pub fn fold_snapshot(health: &mut RedactorCoverageHealth, snap: &MatrixSnapshot, floor: f64) {
    health.vectors_evaluated_total += snap.vectors_total as u64;
    health.true_positives_total += snap.overall.true_positives as u64;
    health.false_negatives_total += snap.overall.false_negatives as u64;
    health.false_positives_total += snap.overall.false_positives as u64;
    let below = snap
        .by_provider
        .iter()
        .filter(|(provider, counters)| {
            counters.recall() < floor
                || !floor.is_finite()
                || !(0.0..=1.0).contains(&floor)
                || snap.vectors.iter().any(|vector| {
                    vector.provider.as_str() == provider.as_str()
                        && !vector.validation_errors.is_empty()
                })
        })
        .count() as u32;
    health.providers_below_recall_floor = below;
}

// ============================================================================
// JSONL render
// ============================================================================

#[must_use]
pub fn render_evaluations_jsonl(evals: &[VectorEvaluation]) -> String {
    let mut out = String::new();
    for eval in evals {
        let line = serde_json::to_string(eval).expect("VectorEvaluation always serializes");
        out.push_str(&line);
        out.push('\n');
    }
    out
}

pub fn parse_evaluations_jsonl(jsonl: &str) -> Result<Vec<VectorEvaluation>, serde_json::Error> {
    let mut out = Vec::new();
    for line in jsonl.lines() {
        if line.trim().is_empty() {
            continue;
        }
        out.push(serde_json::from_str(line)?);
    }
    Ok(out)
}

// ============================================================================
// In-tree synthesized corpus
// ============================================================================

/// Build a positive vector: one expected match spanning the
/// substring `secret` inside `input`. Panics if `secret` is
/// not in `input`.
fn pos(
    name: &str,
    provider: &str,
    rationale: &str,
    input: &str,
    secret: &str,
    pattern_name: &str,
) -> RedactorTestVector {
    let start = input
        .find(secret)
        .expect("positive redactor fixture secret must be embedded in input");
    let end = start + secret.len();
    RedactorTestVector {
        name: name.to_string(),
        input: input.to_string(),
        expected_matches: vec![ExpectedMatch {
            pattern_name: pattern_name.to_string(),
            start: start as u32,
            end: end as u32,
        }],
        provider: provider.to_string(),
        rationale: rationale.to_string(),
    }
}

/// Build a negative vector — input must be flagged as clean.
fn neg(name: &str, provider: &str, rationale: &str, input: &str) -> RedactorTestVector {
    RedactorTestVector {
        name: name.to_string(),
        input: input.to_string(),
        expected_matches: vec![],
        provider: provider.to_string(),
        rationale: rationale.to_string(),
    }
}

/// Synthesized in-tree test corpus. Each pattern in
/// `SECRET_PATTERNS` gets at least 3 positives; provider-
/// specific and cross-cutting negatives cover common lookalikes.
/// All "secret" values are random byte sequences shaped like
/// the format — none are real credentials.
#[must_use]
pub fn synthesized_corpus() -> Vec<RedactorTestVector> {
    vec![
        // -----------------------------------------------------
        // Anthropic
        // -----------------------------------------------------
        pos(
            "anthropic_basic",
            "anthropic",
            "canonical sk-ant- prefix; ensures the Anthropic regex runs before the OpenAI sk- alternation",
            "API_KEY=sk-ant-api03-XXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXX1234567890",
            "sk-ant-api03-XXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXX1234567890",
            "anthropic_key",
        ),
        pos(
            "anthropic_admin",
            "anthropic",
            "admin variant — sk-ant-admin01-",
            "secret: sk-ant-admin01-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "sk-ant-admin01-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "anthropic_key",
        ),
        pos(
            "anthropic_in_log",
            "anthropic",
            "embedded in a log line; ensures regex doesn't require boundary",
            "[2026-05-01T07:00:00Z] auth=sk-ant-api03-FGHIJKLMNOPQRSTUVWXYZ1234567890ABCDEFGH status=ok",
            "sk-ant-api03-FGHIJKLMNOPQRSTUVWXYZ1234567890ABCDEFGH",
            "anthropic_key",
        ),
        neg(
            "anthropic_too_short",
            "anthropic",
            "below the {40,} post-sk-ant- threshold; must NOT match",
            "broken: sk-ant-shortie",
        ),
        // -----------------------------------------------------
        // OpenAI (and DeepSeek/Together that share sk- prefix)
        // -----------------------------------------------------
        pos(
            "openai_classic",
            "openai",
            "sk-<20+ alnum> form — pre-project keys",
            "OPENAI_API_KEY=sk-1234567890abcdefghij1234567890ABCD",
            "sk-1234567890abcdefghij1234567890ABCD",
            "openai_key",
        ),
        pos(
            "openai_proj",
            "openai",
            "sk-proj- prefix; project-scoped keys",
            "key: sk-proj-AAAAAAAAAAAAAAAAAAAA12345678",
            "sk-proj-AAAAAAAAAAAAAAAAAAAA12345678",
            "openai_key",
        ),
        pos(
            "openai_svcacct",
            "openai",
            "sk-svcacct- prefix; service-account keys",
            "auth=sk-svcacct-BBBBBBBBBBBBBBBBBBBB12345678 trailing",
            "sk-svcacct-BBBBBBBBBBBBBBBBBBBB12345678",
            "openai_key",
        ),
        neg(
            "openai_too_short",
            "openai",
            "sk- prefix but body below {20,} — clean",
            "ok: sk-shortprefix",
        ),
        // -----------------------------------------------------
        // GitHub classic
        // -----------------------------------------------------
        pos(
            "github_classic_pat",
            "github",
            "ghp_<36+ alnum> personal access token",
            "GITHUB_TOKEN=ghp_AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
            "ghp_AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
            "github_token",
        ),
        pos(
            "github_oauth",
            "github",
            "gho_ OAuth token",
            "auth: gho_BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB ok",
            "gho_BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB",
            "github_token",
        ),
        pos(
            "github_server",
            "github",
            "ghs_ server-to-server token",
            "ghs_CCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCC",
            "ghs_CCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCC",
            "github_token",
        ),
        neg(
            "github_too_short",
            "github",
            "ghp_ prefix below {36,} threshold",
            "broken: ghp_shortie123",
        ),
        // -----------------------------------------------------
        // GitHub fine-grained PAT
        // -----------------------------------------------------
        pos(
            "github_fg_pat_basic",
            "github",
            "github_pat_<50+ body chars>",
            "TOK=github_pat_11ABCDEFG_AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAZZZZZZ",
            "github_pat_11ABCDEFG_AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAZZZZZZ",
            "github_fine_grained_pat",
        ),
        pos(
            "github_fg_pat_in_url",
            "github",
            "embedded in a longer line",
            "url=https://x-access-token:github_pat_11AAAAAA_BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBYYYYYY@github.com/x.git",
            "github_pat_11AAAAAA_BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBYYYYYY",
            "github_fine_grained_pat",
        ),
        pos(
            "github_fg_pat_long",
            "github",
            "longer body to confirm {40,} matches arbitrary length",
            "github_pat_11ZZZZZZZZ_CCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCDDDDDDDDDD",
            "github_pat_11ZZZZZZZZ_CCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCDDDDDDDDDD",
            "github_fine_grained_pat",
        ),
        neg(
            "github_fg_pat_too_short",
            "github",
            "github_pat_ prefix below body threshold",
            "broken: github_pat_short",
        ),
        // -----------------------------------------------------
        // GitLab
        // -----------------------------------------------------
        pos(
            "gitlab_pat_basic",
            "gitlab",
            "glpat-<20+> personal access token",
            "GITLAB_TOKEN=glpat-AAAAAAAAAAAAAAAAAAAA",
            "glpat-AAAAAAAAAAAAAAAAAAAA",
            "gitlab_token",
        ),
        pos(
            "gitlab_pat_in_url",
            "gitlab",
            "embedded in clone URL",
            "url=https://oauth2:glpat-BBBBBBBBBBBBBBBBBBBBB@gitlab.example.com/group/repo.git",
            "glpat-BBBBBBBBBBBBBBBBBBBBB",
            "gitlab_token",
        ),
        pos(
            "gitlab_pat_long",
            "gitlab",
            "longer GitLab token body",
            "auth=glpat-CCCCCCCCCCCCCCCCCCCCDDDDDDDDDD",
            "glpat-CCCCCCCCCCCCCCCCCCCCDDDDDDDDDD",
            "gitlab_token",
        ),
        neg(
            "gitlab_pat_too_short",
            "gitlab",
            "glpat- prefix below {20,} threshold",
            "broken: glpat-short",
        ),
        // -----------------------------------------------------
        // xAI
        // -----------------------------------------------------
        pos(
            "xai_basic",
            "xai",
            "xai-<40+ alnum>",
            "XAI_KEY=xai-AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
            "xai-AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
            "xai_key",
        ),
        pos(
            "xai_in_log",
            "xai",
            "log line context",
            "request_id=42 token=xai-BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB end",
            "xai-BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB",
            "xai_key",
        ),
        pos(
            "xai_long",
            "xai",
            "longer body",
            "xai-CCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCC",
            "xai-CCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCC",
            "xai_key",
        ),
        // -----------------------------------------------------
        // Groq
        // -----------------------------------------------------
        pos(
            "groq_basic",
            "groq",
            "gsk_<40+ alnum>",
            "GROQ=gsk_AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
            "gsk_AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
            "groq_key",
        ),
        pos(
            "groq_in_config",
            "groq",
            "config line",
            "groq_api_key=gsk_BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB",
            "gsk_BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB",
            "groq_key",
        ),
        pos(
            "groq_long",
            "groq",
            "longer body",
            "gsk_CCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCDDDDDDDDDD",
            "gsk_CCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCDDDDDDDDDD",
            "groq_key",
        ),
        // -----------------------------------------------------
        // Google API key
        // -----------------------------------------------------
        pos(
            "google_api_basic",
            "google",
            "AIza<35> exact-39-total format",
            "GOOGLE_API_KEY=AIzaA1B2C3D4E5F6G7H8I9J0K1L2M3N4O5P6Q7-",
            "AIzaA1B2C3D4E5F6G7H8I9J0K1L2M3N4O5P6Q7-",
            "google_api_key",
        ),
        pos(
            "google_api_underscore",
            "google",
            "underscore in body",
            "k=AIzaA_B_C_D_E_F_G_H_I_J_K_L_M_N_O_P_Q3R",
            "AIzaA_B_C_D_E_F_G_H_I_J_K_L_M_N_O_P_Q3R",
            "google_api_key",
        ),
        pos(
            "google_api_in_url",
            "google",
            "embedded in URL",
            "https://maps.googleapis.com/maps/api/?key=AIzaA1B2C3D4E5F6G7H8I9J0K1L2M3N4O5P6Q12",
            "AIzaA1B2C3D4E5F6G7H8I9J0K1L2M3N4O5P6Q12",
            "google_api_key",
        ),
        neg(
            "google_api_short",
            "google",
            "AIza prefix but body too short",
            "broken: AIzaShortValueOnly",
        ),
        // -----------------------------------------------------
        // Google OAuth
        // -----------------------------------------------------
        pos(
            "google_oauth_basic",
            "google",
            "ya29.<base64-ish> access token",
            "Authorization: Bearer ya29.AAAAAAAAAAAAAAAAAAAA1234567890",
            "ya29.AAAAAAAAAAAAAAAAAAAA1234567890",
            "google_oauth_token",
        ),
        pos(
            "google_oauth_long",
            "google",
            "longer body",
            "ya29.BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBCC",
            "ya29.BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBCC",
            "google_oauth_token",
        ),
        pos(
            "google_oauth_in_log",
            "google",
            "log line",
            "request_token=ya29.CCCCCCCCCCCCCCCCCCCC123456 endline",
            "ya29.CCCCCCCCCCCCCCCCCCCC123456",
            "google_oauth_token",
        ),
        // -----------------------------------------------------
        // Hugging Face
        // -----------------------------------------------------
        pos(
            "hf_basic",
            "huggingface",
            "hf_<30+ alnum>",
            "HF_TOKEN=hf_AAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
            "hf_AAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
            "huggingface_token",
        ),
        pos(
            "hf_in_url",
            "huggingface",
            "embedded in HF download URL",
            "curl -H 'Authorization: Bearer hf_BBBBBBBBBBBBBBBBBBBBBBBBBBBBBB' https://huggingface.co/...",
            "hf_BBBBBBBBBBBBBBBBBBBBBBBBBBBBBB",
            "huggingface_token",
        ),
        pos(
            "hf_long",
            "huggingface",
            "longer body",
            "hf_CCCCCCCCCCCCCCCCCCCCCCCCCCCCCCDDDDDDDDDD",
            "hf_CCCCCCCCCCCCCCCCCCCCCCCCCCCCCCDDDDDDDDDD",
            "huggingface_token",
        ),
        // -----------------------------------------------------
        // Replicate
        // -----------------------------------------------------
        pos(
            "replicate_basic",
            "replicate",
            "r8_<30+ alnum>",
            "REPLICATE_API_TOKEN=r8_AAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
            "r8_AAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
            "replicate_token",
        ),
        pos(
            "replicate_long",
            "replicate",
            "longer body",
            "r8_BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBCCCCCCCCCC",
            "r8_BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBCCCCCCCCCC",
            "replicate_token",
        ),
        pos(
            "replicate_in_log",
            "replicate",
            "log line context",
            "auth=r8_DDDDDDDDDDDDDDDDDDDDDDDDDDDDDD ok",
            "r8_DDDDDDDDDDDDDDDDDDDDDDDDDDDDDD",
            "replicate_token",
        ),
        // -----------------------------------------------------
        // Anyscale
        // -----------------------------------------------------
        pos(
            "anyscale_basic",
            "anyscale",
            "esecret_<30+ alnum>",
            "ANYSCALE_API_KEY=esecret_AAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
            "esecret_AAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
            "anyscale_key",
        ),
        pos(
            "anyscale_long",
            "anyscale",
            "longer body",
            "key: esecret_BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBCCCCCCCCCC trailing",
            "esecret_BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBCCCCCCCCCC",
            "anyscale_key",
        ),
        pos(
            "anyscale_in_config",
            "anyscale",
            "config-style assignment",
            "anyscale.api_key=esecret_DDDDDDDDDDDDDDDDDDDDDDDDDDDDDD",
            "esecret_DDDDDDDDDDDDDDDDDDDDDDDDDDDDDD",
            "anyscale_key",
        ),
        // -----------------------------------------------------
        // Perplexity
        // -----------------------------------------------------
        pos(
            "perplexity_basic",
            "perplexity",
            "pplx-<40+ alnum>",
            "PPLX_API_KEY=pplx-AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
            "pplx-AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
            "perplexity_key",
        ),
        pos(
            "perplexity_long",
            "perplexity",
            "longer body",
            "pplx-BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBCCCCCCCCCC",
            "pplx-BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBCCCCCCCCCC",
            "perplexity_key",
        ),
        pos(
            "perplexity_in_log",
            "perplexity",
            "log line",
            "request key=pplx-DDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDD next",
            "pplx-DDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDD",
            "perplexity_key",
        ),
        // -----------------------------------------------------
        // AI provider keyed value (cohere/mistral/together/...)
        // -----------------------------------------------------
        pos(
            "cohere_keyed",
            "ai_provider_keyed",
            "cohere_api_key=...",
            "cohere_api_key=AAAAAAAAAAAAAAAA1234567890",
            "AAAAAAAAAAAAAAAA1234567890",
            "ai_provider_keyed_value",
        ),
        pos(
            "mistral_keyed",
            "ai_provider_keyed",
            "MISTRAL_API_KEY uppercase config form",
            "MISTRAL_API_KEY: 'BBBBBBBBBBBBBBBBCCCCCCCCCC'",
            "BBBBBBBBBBBBBBBBCCCCCCCCCC",
            "ai_provider_keyed_value",
        ),
        pos(
            "together_keyed",
            "ai_provider_keyed",
            "together_ai_api_key=...",
            "together_ai_api_key=DDDDDDDDDDDDDDDDEEEEEEEEEE",
            "DDDDDDDDDDDDDDDDEEEEEEEEEE",
            "ai_provider_keyed_value",
        ),
        // ft-sydcu: real `databricks_token=dapi...` form. Before the regex fix
        // (`databricks[_-]?token` -> `databricks`) this was a FALSE NEGATIVE —
        // the corpus had zero databricks vectors so the recall floor was
        // vacuously green while the dedicated provider branch was dead. This
        // vector now exercises that branch so the gate fails if it regresses.
        pos(
            "databricks_keyed",
            "ai_provider_keyed",
            "databricks_token= form (ft-sydcu: provider name must end at the shared key/token/secret suffix)",
            "databricks_token=dapideadbeefcafef00d0123456789ab",
            "dapideadbeefcafef00d0123456789ab",
            "ai_provider_keyed_value",
        ),
        // ft-sydcu: NVIDIA_API_KEY= form. The `nvidia[_-]?api` branch already
        // fired correctly, but the corpus never exercised it; this positive
        // vector puts it under the recall gate alongside databricks.
        pos(
            "nvidia_keyed",
            "ai_provider_keyed",
            "NVIDIA_API_KEY uppercase env form (ft-sydcu: exercise the nvidia[_-]?api branch in the recall gate)",
            "NVIDIA_API_KEY=nvapi-0123456789abcdefABCDEFGH",
            "nvapi-0123456789abcdefABCDEFGH",
            "ai_provider_keyed_value",
        ),
        // -----------------------------------------------------
        // AWS Access Key ID
        // -----------------------------------------------------
        pos(
            "aws_access_key_basic",
            "aws",
            "AKIA + 16 uppercase alnum",
            "AWS_ACCESS_KEY_ID=AKIAIOSFODNN7EXAMPLE",
            "AKIAIOSFODNN7EXAMPLE",
            "aws_access_key_id",
        ),
        pos(
            "aws_access_key_in_log",
            "aws",
            "log line context",
            "[2026] auth using AKIAEXAMPLE12345ABCD ok",
            "AKIAEXAMPLE12345ABCD",
            "aws_access_key_id",
        ),
        pos(
            "aws_access_key_in_env",
            "aws",
            "env-export form",
            "export AWS_ACCESS_KEY_ID=AKIAQQQQWWWWEEEERRRR",
            "AKIAQQQQWWWWEEEERRRR",
            "aws_access_key_id",
        ),
        neg(
            "aws_access_key_too_short",
            "aws",
            "AKIA prefix but body below 16 chars",
            "broken: AKIASHORT",
        ),
        // -----------------------------------------------------
        // AWS Secret Key
        // -----------------------------------------------------
        pos(
            "aws_secret_basic",
            "aws",
            "aws_secret_access_key=...",
            "aws_secret_access_key=wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY",
            "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY",
            "aws_secret_key",
        ),
        pos(
            "aws_secret_quoted",
            "aws",
            "quoted form",
            r#"aws_secret_access_key: "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA""#,
            "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
            "aws_secret_key",
        ),
        pos(
            "aws_secret_in_config",
            "aws",
            "TOML/INI-style config",
            "aws_secret_access_key=BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB",
            "BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB",
            "aws_secret_key",
        ),
        // -----------------------------------------------------
        // Bearer token
        // -----------------------------------------------------
        pos(
            "bearer_basic",
            "bearer",
            "Authorization header bearer",
            "Authorization: Bearer abcdefghijklmnopqrstuvwxyz0123456789",
            "Bearer abcdefghijklmnopqrstuvwxyz0123456789",
            "bearer_token",
        ),
        pos(
            "bearer_lowercase",
            "bearer",
            "case-insensitive",
            "authorization: bearer ABCDEFGHIJ1234567890ABCDEFG",
            "bearer ABCDEFGHIJ1234567890ABCDEFG",
            "bearer_token",
        ),
        pos(
            "bearer_inline",
            "bearer",
            "inline 'bearer ...' without auth header",
            "saw bearer abcdefghij1234567890abcdefghij in log",
            "bearer abcdefghij1234567890abcdefghij",
            "bearer_token",
        ),
        // Base64 bearer values: before the charset carried `+`, `/` and `=`,
        // this input could not satisfy `{20,}` at all (only `AbC` precedes
        // the first `+`), so the whole credential was emitted in cleartext.
        pos(
            "bearer_base64_value",
            "bearer",
            "base64 body with + / = padding",
            "Authorization: Bearer AbC+dEfGhIjKlMnOpQr/StUvWxYz0123456789==",
            "Bearer AbC+dEfGhIjKlMnOpQr/StUvWxYz0123456789==",
            "bearer_token",
        ),
        // -----------------------------------------------------
        // HTTP Basic
        // -----------------------------------------------------
        pos(
            "basic_auth_header",
            "basic",
            "Authorization: Basic <base64> from curl -v output",
            "> Authorization: Basic dXNlcjpzdXBlcnNlY3JldA==",
            "Authorization: Basic dXNlcjpzdXBlcnNlY3JldA==",
            "http_basic_auth",
        ),
        pos(
            "basic_auth_lowercase",
            "basic",
            "case-insensitive header and scheme",
            "authorization: basic YWRtaW46aHVudGVyMg==",
            "authorization: basic YWRtaW46aHVudGVyMg==",
            "http_basic_auth",
        ),
        pos(
            "basic_auth_quoted_config",
            "basic",
            "quoted header value in a config/JSON body",
            r#"{"Authorization": "Basic Zm9vOmJhcmJhemJhcXV1eA=="}"#,
            r#"Authorization": "Basic Zm9vOmJhcmJhemJhcXV1eA=="#,
            "http_basic_auth",
        ),
        neg(
            "basic_auth_prose",
            "negative",
            "'basic' as an ordinary English word must not redact prose",
            "See the docs for basic troubleshooting of authorization failures.",
        ),
        // -----------------------------------------------------
        // Slack
        // -----------------------------------------------------
        pos(
            "slack_bot",
            "slack",
            "xoxb- bot token",
            "SLACK_BOT_TOKEN=xoxb-1234567890-AbCdEfGhIjKlMnOpQrStUv",
            "xoxb-1234567890-AbCdEfGhIjKlMnOpQrStUv",
            "slack_token",
        ),
        pos(
            "slack_personal",
            "slack",
            "xoxp- personal token",
            "auth: xoxp-AAAAAAAAAA-BBBBBBBBBB-CCCCCCCCCC-DDDDDDDDDD",
            "xoxp-AAAAAAAAAA-BBBBBBBBBB-CCCCCCCCCC-DDDDDDDDDD",
            "slack_token",
        ),
        pos(
            "slack_app",
            "slack",
            "xoxa- app token",
            "xoxa-1234567890-EEEEEEEEEE-FFFFFFFFFF",
            "xoxa-1234567890-EEEEEEEEEE-FFFFFFFFFF",
            "slack_token",
        ),
        // -----------------------------------------------------
        // Stripe
        // -----------------------------------------------------
        pos(
            "stripe_secret_live",
            "stripe",
            "sk_live_<20+>",
            "STRIPE_SECRET_KEY=sk_live_ABCDEFGHIJKLMNOPQRSTUV1234",
            "sk_live_ABCDEFGHIJKLMNOPQRSTUV1234",
            "stripe_key",
        ),
        pos(
            "stripe_publishable_test",
            "stripe",
            "pk_test_<20+>",
            "pub: pk_test_QWERTYUIOPASDFGHJKLZ12345",
            "pk_test_QWERTYUIOPASDFGHJKLZ12345",
            "stripe_key",
        ),
        pos(
            "stripe_secret_test",
            "stripe",
            "sk_test_<20+>",
            "sk_test_AAAAAAAAAAAAAAAAAAAAAAAAAAAA",
            "sk_test_AAAAAAAAAAAAAAAAAAAAAAAAAAAA",
            "stripe_key",
        ),
        // -----------------------------------------------------
        // Twilio Account SID
        // -----------------------------------------------------
        pos(
            "twilio_sid_basic",
            "twilio",
            "AC + 32 hex chars",
            "TWILIO_ACCOUNT_SID=AC0123456789abcdef0123456789ABCDEF",
            "AC0123456789abcdef0123456789ABCDEF",
            "twilio_account_sid",
        ),
        pos(
            "twilio_sid_lower_hex",
            "twilio",
            "lowercase hex body",
            "account_sid: ACabcdefabcdefabcdefabcdefabcdefab",
            "ACabcdefabcdefabcdefabcdefabcdefab",
            "twilio_account_sid",
        ),
        pos(
            "twilio_sid_in_log",
            "twilio",
            "SID embedded in a log line",
            "[twilio] sid=AC11112222333344445555666677778888 ok",
            "AC11112222333344445555666677778888",
            "twilio_account_sid",
        ),
        neg(
            "twilio_sid_too_short",
            "twilio",
            "AC prefix below 32 hex chars",
            "broken: AC0123456789abcdef",
        ),
        // -----------------------------------------------------
        // SendGrid
        // -----------------------------------------------------
        pos(
            "sendgrid_key_basic",
            "sendgrid",
            "SG.<20+>.<40+> API key",
            "SENDGRID_API_KEY=SG.ABCDEFGHIJKLMNOPQRST.ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmn",
            "SG.ABCDEFGHIJKLMNOPQRST.ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmn",
            "sendgrid_key",
        ),
        pos(
            "sendgrid_key_underscore",
            "sendgrid",
            "SendGrid token body with URL-safe underscore",
            "api_key=SG.ABCDEFGHIJKLMNOPQRST.ABCDEFGHIJKLMNO_PQRSTUVWXYZabcdefghijklmn",
            "SG.ABCDEFGHIJKLMNOPQRST.ABCDEFGHIJKLMNO_PQRSTUVWXYZabcdefghijklmn",
            "sendgrid_key",
        ),
        pos(
            "sendgrid_key_dash",
            "sendgrid",
            "SendGrid token body with URL-safe dash",
            "sendgrid: SG.abcdefghijklmnopqrst.abcdefghijklmnopqrstuvwxyzABCD-EFGHIJKLMN",
            "SG.abcdefghijklmnopqrst.abcdefghijklmnopqrstuvwxyzABCD-EFGHIJKLMN",
            "sendgrid_key",
        ),
        neg(
            "sendgrid_key_too_short",
            "sendgrid",
            "SG prefix with undersized third segment",
            "broken: SG.short.too_short",
        ),
        // -----------------------------------------------------
        // Datadog
        // -----------------------------------------------------
        pos(
            "datadog_dd_api_key",
            "datadog",
            "DD_API_KEY keyed 32-hex value",
            "DD_API_KEY=0123456789abcdef0123456789ABCDEF",
            "0123456789abcdef0123456789ABCDEF",
            "datadog_api_key",
        ),
        pos(
            "datadog_full_api_key",
            "datadog",
            "DATADOG_API_KEY keyed value",
            "DATADOG_API_KEY: abcdefabcdefabcdefabcdefabcdef12",
            "abcdefabcdefabcdefabcdefabcdef12",
            "datadog_api_key",
        ),
        pos(
            "datadog_quoted_api_key",
            "datadog",
            "quoted Datadog API key value",
            r#"DD_API_KEY="ABCDEFABCDEFABCDEFABCDEFABCDEF12""#,
            "ABCDEFABCDEFABCDEFABCDEFABCDEF12",
            "datadog_api_key",
        ),
        neg(
            "datadog_api_key_non_hex",
            "datadog",
            "DD_API_KEY value with non-hex chars",
            "DD_API_KEY=nothexnothex",
        ),
        // -----------------------------------------------------
        // Database URL
        // -----------------------------------------------------
        pos(
            "postgres_url_with_password",
            "database",
            "postgres://user:password@host/db",
            "DATABASE_URL=postgres://admin:s3cretP4ss@db.example.com:5432/app",
            "s3cretP4ss",
            "database_url",
        ),
        pos(
            "mysql_url_with_password",
            "database",
            "mysql://...",
            "url=mysql://root:hunter2pass@localhost/test",
            "hunter2pass",
            "database_url",
        ),
        pos(
            "mongo_url_with_password",
            "database",
            "mongodb://...",
            "uri=mongodb://user:secret123@cluster0.example.mongodb.net/db",
            "secret123",
            "database_url",
        ),
        // -----------------------------------------------------
        // SSH / PEM private key blocks
        // -----------------------------------------------------
        pos(
            "ssh_rsa_private_key_block",
            "ssh_private_key",
            "RSA private key PEM envelope",
            "paste:\n-----BEGIN RSA PRIVATE KEY-----\nMIIEpAIBAAKCAQEAaBcDeFgHiJkLmNoPqRsTuVwXyZ1234567890\nEXAMPLE_BODY_NOT_A_REAL_KEY_aBcDeFgHiJkLmNoPqRsTuVwX\n-----END RSA PRIVATE KEY-----",
            "-----BEGIN RSA PRIVATE KEY-----\nMIIEpAIBAAKCAQEAaBcDeFgHiJkLmNoPqRsTuVwXyZ1234567890\nEXAMPLE_BODY_NOT_A_REAL_KEY_aBcDeFgHiJkLmNoPqRsTuVwX\n-----END RSA PRIVATE KEY-----",
            "ssh_private_key",
        ),
        pos(
            "ssh_openssh_private_key_block",
            "ssh_private_key",
            "OpenSSH private key PEM envelope",
            "-----BEGIN OPENSSH PRIVATE KEY-----\nb3BlbnNzaC1rZXktdjEAAAAABG5vbmUAAAAEXAMPLE_SYNTHETIC_BODY\n-----END OPENSSH PRIVATE KEY-----",
            "-----BEGIN OPENSSH PRIVATE KEY-----\nb3BlbnNzaC1rZXktdjEAAAAABG5vbmUAAAAEXAMPLE_SYNTHETIC_BODY\n-----END OPENSSH PRIVATE KEY-----",
            "ssh_private_key",
        ),
        pos(
            "ssh_pkcs8_encrypted_private_key_block",
            "ssh_private_key",
            "encrypted PKCS#8 private key envelope",
            "secret block -----BEGIN ENCRYPTED PRIVATE KEY-----\nMIIFHDBOBgkqhkiG9w0BBQ0wQTApBgkqhkiG9w0BBQwwHAQIEXAMPLEBODY123456\n-----END ENCRYPTED PRIVATE KEY----- trailing",
            "-----BEGIN ENCRYPTED PRIVATE KEY-----\nMIIFHDBOBgkqhkiG9w0BBQ0wQTApBgkqhkiG9w0BBQwwHAQIEXAMPLEBODY123456\n-----END ENCRYPTED PRIVATE KEY-----",
            "ssh_private_key",
        ),
        neg(
            "ssh_private_key_incomplete_block",
            "ssh_private_key",
            "BEGIN marker without matching END marker",
            "-----BEGIN RSA PRIVATE KEY-----\nMIIEpAIBAAKCAQEAaBcDeFg",
        ),
        // -----------------------------------------------------
        // PGP / OpenPGP armored blocks
        // -----------------------------------------------------
        pos(
            "pgp_private_key_block",
            "pgp_block",
            "PGP private key armored envelope",
            "-----BEGIN PGP PRIVATE KEY BLOCK-----\nVersion: Synthetic\n\nlQOYBGUAAAAABCDEF1234567890SYNTHETIC\n-----END PGP PRIVATE KEY BLOCK-----",
            "-----BEGIN PGP PRIVATE KEY BLOCK-----\nVersion: Synthetic\n\nlQOYBGUAAAAABCDEF1234567890SYNTHETIC\n-----END PGP PRIVATE KEY BLOCK-----",
            "pgp_block",
        ),
        pos(
            "pgp_message_block",
            "pgp_block",
            "PGP message armored envelope",
            "payload=-----BEGIN PGP MESSAGE-----\n\nwcBMA0EXAMPLESYNTHETICBODY1234567890\n-----END PGP MESSAGE-----",
            "-----BEGIN PGP MESSAGE-----\n\nwcBMA0EXAMPLESYNTHETICBODY1234567890\n-----END PGP MESSAGE-----",
            "pgp_block",
        ),
        pos(
            "pgp_signed_message_block",
            "pgp_block",
            "PGP signed-message envelope closes with END PGP SIGNATURE",
            "-----BEGIN PGP SIGNED MESSAGE-----\nHash: SHA256\n\nhello\n-----BEGIN PGP SIGNATURE-----\nSYNTHETICSIGNATUREBODY\n-----END PGP SIGNATURE-----",
            "-----BEGIN PGP SIGNED MESSAGE-----\nHash: SHA256\n\nhello\n-----BEGIN PGP SIGNATURE-----\nSYNTHETICSIGNATUREBODY\n-----END PGP SIGNATURE-----",
            "pgp_block",
        ),
        neg(
            "pgp_block_incomplete",
            "pgp_block",
            "PGP opener without a closing armored marker",
            "-----BEGIN PGP MESSAGE-----\nmissing trailer",
        ),
        // -----------------------------------------------------
        // JWT
        // -----------------------------------------------------
        pos(
            "jwt_bare_token",
            "jwt",
            "bare JWT starts with eyJ header and payload",
            "jwt=eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjMifQ.signature0123456789",
            "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjMifQ.signature0123456789",
            "jwt_token",
        ),
        pos(
            "jwt_in_log_line",
            "jwt",
            "JWT embedded in log output",
            "DEBUG bearerless token eyJ0eXAiOiJKV1QifQ.eyJpc3MiOiJmdCJ9.syntheticSig end",
            "eyJ0eXAiOiJKV1QifQ.eyJpc3MiOiJmdCJ9.syntheticSig",
            "jwt_token",
        ),
        pos(
            "jwt_long_signature",
            "jwt",
            "JWT with long base64url signature",
            "eyJraWQiOiJleGFtcGxlIn0.eyJhdWQiOiJmcmFua2VudGVybSJ9.ABCDEFGHIJKLMNOPQRSTUVWXYZ_abcdef0123456789",
            "eyJraWQiOiJleGFtcGxlIn0.eyJhdWQiOiJmcmFua2VudGVybSJ9.ABCDEFGHIJKLMNOPQRSTUVWXYZ_abcdef0123456789",
            "jwt_token",
        ),
        neg(
            "jwt_two_segments_only",
            "jwt",
            "JWT-looking value without signature segment",
            "not-a-jwt eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjMifQ",
        ),
        // -----------------------------------------------------
        // Device code (OAuth device flow)
        // -----------------------------------------------------
        pos(
            "device_code_basic",
            "device_code",
            "device_code=ABCDEF",
            "device_code=ABCDEF12-3456",
            "ABCDEF12-3456",
            "device_code",
        ),
        pos(
            "user_code_basic",
            "device_code",
            "user_code=...",
            "user_code: 'ABCD-EFGH'",
            "ABCD-EFGH",
            "device_code",
        ),
        pos(
            "device_code_quoted",
            "device_code",
            "quoted",
            r#"device-code="WXYZ123456""#,
            "WXYZ123456",
            "device_code",
        ),
        // -----------------------------------------------------
        // OAuth URL with token in query
        // -----------------------------------------------------
        pos(
            "oauth_url_access_token",
            "oauth_url",
            "access_token=... query param",
            "redirect=https://example.com/cb?access_token=abc123def456&state=xyz",
            "https://example.com/cb?access_token=abc123def456",
            "oauth_url",
        ),
        pos(
            "oauth_url_code",
            "oauth_url",
            "code=... query param",
            "https://app.example.com/oauth/cb?code=ZZZZZZZZZZ",
            "https://app.example.com/oauth/cb?code=ZZZZZZZZZZ",
            "oauth_url",
        ),
        pos(
            "oauth_url_token",
            "oauth_url",
            "token=... query param",
            "url: http://localhost:8080/return?token=abc123def456ghi789",
            "http://localhost:8080/return?token=abc123def456ghi789",
            "oauth_url",
        ),
        // -----------------------------------------------------
        // Generic API key
        // -----------------------------------------------------
        pos(
            "generic_api_key_basic",
            "generic",
            "api_key=...",
            "api_key=ABCDEFGHIJKLMNOP1234567890",
            "ABCDEFGHIJKLMNOP1234567890",
            "generic_api_key",
        ),
        pos(
            "generic_apikey_lower",
            "generic",
            "apikey: ...",
            "apikey: 'AAAABBBBCCCCDDDD12345678'",
            "AAAABBBBCCCCDDDD12345678",
            "generic_api_key",
        ),
        pos(
            "generic_api_key_base64",
            "generic",
            "ft-5o6u5: base64-shaped value with /+= characters",
            "api_key=AAAA/BBBB+CCCC=DDDD12345678",
            "AAAA/BBBB+CCCC=DDDD12345678",
            "generic_api_key",
        ),
        // -----------------------------------------------------
        // Generic token
        // -----------------------------------------------------
        pos(
            "generic_token_basic",
            "generic",
            "token=... assignment",
            "token=ABCDEFGHIJKL1234567890",
            "ABCDEFGHIJKL1234567890",
            "generic_token",
        ),
        pos(
            "generic_token_quoted",
            "generic",
            "quoted token",
            "auth_token: \"AAAABBBBCCCCDDDDEEEEFFFF\"",
            "AAAABBBBCCCCDDDDEEEEFFFF",
            "generic_token",
        ),
        pos(
            "generic_token_with_dot",
            "generic",
            "token with `.` separator (JWT-shaped body)",
            "token=eyJhbGciOiJIUzI1NiJ9.something1234567",
            "eyJhbGciOiJIUzI1NiJ9.something1234567",
            "generic_token",
        ),
        // -----------------------------------------------------
        // Generic password
        // -----------------------------------------------------
        pos(
            "generic_password_basic",
            "generic",
            "password=...",
            "password=mySecret1234",
            "mySecret1234",
            "generic_password",
        ),
        pos(
            "generic_password_quoted",
            "generic",
            "quoted password",
            r#"password: "MyP@ssw0rd!2024""#,
            "MyP@ssw0rd!2024",
            "generic_password",
        ),
        pos(
            "generic_password_single_quote",
            "generic",
            "single-quoted password",
            "password='ssh-trustno1'",
            "ssh-trustno1",
            "generic_password",
        ),
        // -----------------------------------------------------
        // Generic secret
        // -----------------------------------------------------
        pos(
            "generic_secret_basic",
            "generic",
            "secret=... assignment",
            "secret=AAAA1234BBBB",
            "AAAA1234BBBB",
            "generic_secret",
        ),
        pos(
            "generic_secret_quoted",
            "generic",
            "quoted secret",
            "client_secret: 'CCCC5678DDDD'",
            "CCCC5678DDDD",
            "generic_secret",
        ),
        pos(
            "generic_secret_base64",
            "generic",
            "base64 body",
            "client_secret=AAAA/BBBB+CCCC=DDDD12",
            "AAAA/BBBB+CCCC=DDDD12",
            "generic_secret",
        ),
        // camelCase + JSON-quoted key shapes. Both were byte-identical
        // passthroughs before: the keyword guard rejected a preceding letter
        // (`clientSecret`), and the key side did not tolerate a closing quote
        // before the delimiter (`"secret": …`). snake_case with a bare `=`
        // always worked, which is why the corpus never caught it.
        pos(
            "generic_secret_camel_case_json",
            "generic",
            "camelCase JSON key — clientSecret",
            r#"{"clientSecret":"abcd1234EFGH5678"}"#,
            "abcd1234EFGH5678",
            "generic_secret",
        ),
        pos(
            "generic_token_camel_case_json",
            "generic",
            "camelCase JSON key — accessToken",
            r#"{"accessToken": "0123456789abcdefghij"}"#,
            "0123456789abcdefghij",
            "generic_token",
        ),
        pos(
            "generic_api_key_json_quoted",
            "generic",
            "JSON-quoted api_key with a closing quote before the colon",
            r#"{"api_key": "AAAABBBBCCCCDDDD1234"}"#,
            "AAAABBBBCCCCDDDD1234",
            "generic_api_key",
        ),
        // -----------------------------------------------------
        // Cross-cutting negatives — lookalikes that must NOT
        // match.
        // -----------------------------------------------------
        neg(
            "lookalike_var_name_only",
            "negative",
            "mentions 'api_key' in prose without an assignment; clean",
            "Documentation: refer to your provider's api_key management page.",
        ),
        neg(
            "lookalike_short_secret",
            "negative",
            "secret=tooShort — below {8,}",
            "secret=short",
        ),
        neg(
            "lookalike_uuid",
            "negative",
            "UUID is not a secret",
            "request_id: 550e8400-e29b-41d4-a716-446655440000",
        ),
    ]
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::redactor::secret_pattern_names;
    use sha2::{Digest, Sha256};

    // Independent oracle: mark individual ORIGINAL bytes, without sorting,
    // merging intervals or calling the production coverage classifier.
    fn original_byte_mask(input: &str, ranges: &[(usize, usize)]) -> Result<Vec<bool>, usize> {
        let mut mask = vec![false; input.len()];
        for (index, &(start, end)) in ranges.iter().enumerate() {
            let Some(span) = input.get(start..end).filter(|span| !span.is_empty()) else {
                return Err(index);
            };
            for covered in &mut mask[start..start + span.len()] {
                *covered = true;
            }
        }
        Ok(mask)
    }

    struct ByteOracleFixture {
        vector: RedactorTestVector,
        removed: Vec<bool>,
        output: String,
    }

    fn byte_oracle_fixture(
        name: &str,
        parts: &[(&str, bool)],
        expect_whole: bool,
    ) -> ByteOracleFixture {
        // These fixture annotations specify the expected output and source
        // mask BEFORE invoking any production redactor or report code.
        let mut input = String::new();
        let mut output = String::new();
        let mut removed = Vec::new();
        let mut expected_matches = Vec::new();
        for &(text, redact) in parts {
            let start = input.len();
            input.push_str(text);
            removed.extend(std::iter::repeat_n(redact, text.len()));
            output.push_str(if redact { "[REDACTED]" } else { text });
            if redact {
                expected_matches.push(ExpectedMatch {
                    pattern_name: "generic_secret".to_string(),
                    start: u32::try_from(start).unwrap(),
                    end: u32::try_from(input.len()).unwrap(),
                });
            }
        }
        if expect_whole {
            expected_matches = vec![ExpectedMatch {
                pattern_name: "generic_secret".to_string(),
                start: 0,
                end: u32::try_from(input.len()).unwrap(),
            }];
        }
        ByteOracleFixture {
            vector: RedactorTestVector {
                name: name.to_string(),
                input,
                expected_matches,
                provider: "byte_oracle".to_string(),
                rationale: "Owned synthetic original-byte coverage control".to_string(),
            },
            removed,
            output,
        }
    }

    #[test]
    fn independent_byte_mask_agrees_with_real_pipeline_and_serialized_report() {
        // Twelve bounded fixtures, each < 256 UTF-8 bytes. No file writes,
        // providers, private data, golden blessing or production-leak claim.
        let key = "ghp_AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
        let anthropic = "sk-ant-api03-1234567890123456789012345678901234567890";
        let pem = concat!(
            "-----BEGIN PRIVATE KEY-----\n",
            "ghp_AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA\n",
            "-----END PRIVATE KEY-----"
        );
        let mut fixtures = vec![
            byte_oracle_fixture("clean", &[("π ordinary text λ", false)], false),
            byte_oracle_fixture("empty_clean", &[("", false)], false),
            byte_oracle_fixture(
                "unicode_surroundings",
                &[("πleft ", false), (key, true), (" rightλ", false)],
                false,
            ),
            // The specific Anthropic pass runs first. Two same-prefix
            // variable-length GitHub tokens without a delimiter would instead
            // be ambiguous under the token regex's {36,} body grammar.
            byte_oracle_fixture("adjacent_union", &[(key, true), (anthropic, true)], true),
            byte_oracle_fixture(
                "prefix_only",
                &[(key, true), ("|private_suffix", false)],
                true,
            ),
            byte_oracle_fixture(
                "suffix_only",
                &[("private_prefix|", false), (key, true)],
                true,
            ),
            byte_oracle_fixture(
                "one_byte_hole",
                &[(key, true), ("|", false), (key, true)],
                true,
            ),
            byte_oracle_fixture(
                "unicode_hole",
                &[(key, true), ("秘密", false), (key, true)],
                true,
            ),
            byte_oracle_fixture("nested_pem", &[(pem, true)], true),
            byte_oracle_fixture("missed_span", &[("synthetic_missed_span", false)], true),
        ];
        let mut one_byte = byte_oracle_fixture(
            "one_byte_overlap",
            &[("private_prefix|", false), (key, true)],
            true,
        );
        one_byte.vector.expected_matches[0].end = "private_prefix|".len() as u32 + 1;
        fixtures.push(one_byte);
        let mut duplicate = byte_oracle_fixture("duplicate_expectations", &[(key, true)], false);
        duplicate
            .vector
            .expected_matches
            .push(duplicate.vector.expected_matches[0].clone());
        duplicate.vector.expected_matches.push(ExpectedMatch {
            pattern_name: "generic_secret".to_string(),
            start: 1,
            end: 5,
        });
        fixtures.push(duplicate);

        let redactor = Redactor::new();
        let mut passing = 0;
        let mut rejected = 0;
        for fixture in &fixtures {
            let vector = &fixture.vector;
            assert!(vector.input.len() < 256);
            let trace = redactor.redact_with_replacement_spans(&vector.input);
            let actual_ranges: Vec<_> = trace
                .replacements
                .iter()
                .map(|(_, s, e)| (*s, *e))
                .collect();
            let actual_mask = original_byte_mask(&vector.input, &actual_ranges).unwrap();
            assert_eq!(actual_mask, fixture.removed, "fixture={}", vector.name);
            assert_eq!(trace.redacted, fixture.output, "fixture={}", vector.name);
            assert_eq!(redactor.redact(&vector.input), fixture.output);
            let bytes_result = redactor.redact_bytes_with_evidence(vector.input.as_bytes());
            assert_eq!(bytes_result.bytes, fixture.output.as_bytes());
            assert_eq!(
                bytes_result.evidence.secret_input_bytes_replaced,
                fixture.removed.iter().filter(|removed| **removed).count() as u64
            );
            let expected_ranges: Vec<_> = vector
                .expected_matches
                .iter()
                .map(|expected| (expected.start as usize, expected.end as usize))
                .collect();
            let expected_mask = original_byte_mask(&vector.input, &expected_ranges).unwrap();
            let tp = expected_ranges
                .iter()
                .filter(|(start, end)| actual_mask[*start..*end].iter().all(|removed| *removed))
                .count() as u32;
            let fn_count = vector.expected_matches.len() as u32 - tp;
            let fp = actual_ranges
                .iter()
                .filter(|(start, end)| {
                    !expected_mask[*start..*end].iter().any(|expected| *expected)
                })
                .count() as u32;
            let covered = expected_mask
                .iter()
                .zip(&actual_mask)
                .filter(|(expected, removed)| **expected && **removed)
                .count();
            let uncovered = expected_mask
                .iter()
                .zip(&actual_mask)
                .filter(|(expected, removed)| **expected && !**removed)
                .count();
            let snapshot = MatrixSnapshot::evaluate(std::slice::from_ref(vector));
            let evaluation = &snapshot.vectors[0];
            assert!(evaluation.validation_errors.is_empty());
            assert_eq!(
                (
                    evaluation.true_positives,
                    evaluation.false_negatives,
                    evaluation.false_positives
                ),
                (tp, fn_count, fp)
            );
            let gate = snapshot.meets_recall_floor(1.0);
            assert_eq!(gate, uncovered == 0, "fixture={}", vector.name);
            let report = serde_json::to_value(&snapshot).unwrap();
            assert_eq!(report["overall"]["true_positives"], tp);
            assert_eq!(report["overall"]["false_negatives"], fn_count);
            assert_eq!(report["by_provider"]["byte_oracle"]["false_positives"], fp);
            let jsonl = render_evaluations_jsonl(&snapshot.vectors);
            let published = parse_evaluations_jsonl(&jsonl).unwrap();
            assert_eq!(published[0].true_positives, tp);
            assert_eq!(published[0].false_negatives, fn_count);
            let detections = redactor.detect(&vector.input);
            if vector.name == "nested_pem" {
                let detection_ranges: Vec<_> =
                    detections.iter().map(|(_, s, e)| (*s, *e)).collect();
                assert_ne!(
                    original_byte_mask(&vector.input, &detection_ranges).unwrap(),
                    actual_mask
                );
                assert_eq!(trace.replacement_count, 2);
            }
            if gate {
                passing += 1;
            } else {
                rejected += 1;
                // Even with the complete original string gone, the known
                // output and byte mask prove that a fragment remains.
                if covered > 0 {
                    assert!(!fixture.output.contains(&vector.input));
                }
                assert!(uncovered > 0);
            }
            eprintln!(
                "REDACTOR_BYTE_ORACLE {}",
                serde_json::json!({
                "fixture": vector.name,
                "input_sha256": format!("{:x}", Sha256::digest(vector.input.as_bytes())),
                "output_sha256": format!("{:x}", Sha256::digest(&bytes_result.bytes)),
                "expected_spans": expected_ranges.len(), "replacement_spans": actual_ranges.len(),
                "replacement_operations": trace.replacement_count, "detector_spans": detections.len(),
                "covered_original_bytes": covered, "uncovered_original_bytes": uncovered,
                "metadata_valid": evaluation.validation_errors.is_empty(),
                "true_positives": tp, "false_negatives": fn_count, "false_positives": fp,
                "coverage_gate": gate
                })
            );
        }
        assert_eq!(fixtures.len(), 12);
        assert_eq!((passing, rejected), (6, 6));
    }

    #[test]
    fn independent_byte_mask_checks_all_small_interval_pairs() {
        let vector = RedactorTestVector {
            name: "all_small_interval_pairs".to_string(),
            input: "abcdefgh".to_string(),
            expected_matches: vec![ExpectedMatch {
                pattern_name: "generic_secret".to_string(),
                start: 2,
                end: 6,
            }],
            provider: "byte_oracle".to_string(),
            rationale: "Adjacent, overlapping, duplicate, reordered and gapped interval unions"
                .to_string(),
        };
        let ranges: Vec<_> = (0..8)
            .flat_map(|start| (start + 1..=8).map(move |end| (start, end)))
            .collect();
        let mut fully_covered = 0;
        let mut partial = 0;
        for &(a, b) in &ranges {
            for &(c, d) in &ranges {
                let mask = original_byte_mask(&vector.input, &[(a, b), (c, d)]).unwrap();
                let complete = mask[2..6].iter().all(|covered| *covered);
                let evaluation =
                    classify_replacement_spans(&vector, &[("first", a, b), ("second", c, d)]);
                assert!(evaluation.validation_errors.is_empty());
                assert_eq!(
                    evaluation.true_positives,
                    u32::from(complete),
                    "ranges={a}..{b},{c}..{d}"
                );
                assert_eq!(evaluation.false_negatives, u32::from(!complete));
                if complete {
                    fully_covered += 1;
                } else {
                    partial += 1;
                }
            }
        }
        assert_eq!(fully_covered + partial, 1296);
        assert!(fully_covered > 0 && partial > 0);
        eprintln!(
            "REDACTOR_INTERVAL_ORACLE cases=1296 complete={fully_covered} incomplete={partial}"
        );
    }

    #[test]
    fn independent_byte_mask_rejects_invalid_unicode_and_empty_metadata() {
        let mut fixture = byte_oracle_fixture("invalid_metadata", &[("é秘密abcd", false)], true);
        for (start, end) in [(0, 0), (6, 5), (0, 13), (1, 2), (0, 1)] {
            assert_eq!(
                original_byte_mask(&fixture.vector.input, &[(start, end)]),
                Err(0)
            );
            fixture.vector.expected_matches[0].start = start as u32;
            fixture.vector.expected_matches[0].end = end as u32;
            let snapshot = MatrixSnapshot::evaluate(std::slice::from_ref(&fixture.vector));
            assert!(!snapshot.vectors[0].validation_errors.is_empty());
            assert!(!snapshot.meets_recall_floor(0.0));
            assert_eq!(snapshot.vectors[0].true_positives, 0);
            let report = serde_json::to_value(&snapshot).unwrap();
            assert!(
                !report["vectors"][0]["validation_errors"]
                    .as_array()
                    .unwrap()
                    .is_empty()
            );
            fixture.vector.expected_matches[0].start = 0;
            fixture.vector.expected_matches[0].end = 12;
            let replacement =
                classify_replacement_spans(&fixture.vector, &[("invalid", start, end)]);
            assert!(!replacement.validation_errors.is_empty());
            assert_eq!(replacement.true_positives, 0);
        }
        let empty = byte_oracle_fixture("empty_positive", &[("", false)], true);
        assert_eq!(original_byte_mask("", &[(0, 0)]), Err(0));
        let snapshot = MatrixSnapshot::evaluate(&[empty.vector]);
        assert!(!snapshot.vectors[0].validation_errors.is_empty());
        assert!(!snapshot.meets_recall_floor(0.0));
        eprintln!(
            "REDACTOR_METADATA_ORACLE invalid_expected=6 invalid_replacements=5 coverage_gate=false"
        );
    }

    #[test]
    fn corpus_has_minimum_size() {
        let corpus = synthesized_corpus();
        let expected_min = secret_pattern_names().count() * 3;
        assert!(
            corpus.len() >= expected_min,
            "corpus too small: {} (expected ≥{expected_min})",
            corpus.len(),
        );
    }

    #[test]
    fn corpus_covers_every_live_secret_pattern() {
        let covered: std::collections::BTreeSet<String> = synthesized_corpus()
            .into_iter()
            .flat_map(|v| v.expected_matches.into_iter().map(|m| m.pattern_name))
            .collect();
        let missing = secret_pattern_names()
            .filter(|name| !covered.contains(*name))
            .collect::<Vec<_>>();
        assert!(
            missing.is_empty(),
            "coverage corpus is missing live redactor pattern classes: {missing:?}"
        );
    }

    #[test]
    fn every_corpus_provider_is_lowercase_alpha() {
        for v in synthesized_corpus() {
            for c in v.provider.chars() {
                assert!(
                    c.is_ascii_lowercase() || c == '_',
                    "provider {:?} on {:?} has invalid char {c:?}",
                    v.provider,
                    v.name,
                );
            }
        }
    }

    #[test]
    fn corpus_vector_names_are_unique() {
        use std::collections::HashSet;
        let corpus = synthesized_corpus();
        let mut seen = HashSet::new();
        for v in &corpus {
            assert!(
                seen.insert(v.name.clone()),
                "duplicate vector name: {}",
                v.name
            );
        }
    }

    #[test]
    fn corpus_runs_clean_on_redactor() {
        let corpus = synthesized_corpus();
        let snap = MatrixSnapshot::evaluate(&corpus);
        // Smoke check — coverage meets a 99% recall floor on
        // the synthesized corpus. The integration harness
        // (`tests/redactor_coverage_matrix.rs`) re-runs this
        // against the floor and exposes the bless flow.
        assert!(
            snap.meets_recall_floor(0.99),
            "synthesized corpus failed 0.99 recall floor; min provider: {:?}; snap: {:?}",
            snap.min_provider_recall(),
            snap.overall,
        );
    }

    #[test]
    fn corpus_expected_pattern_names_are_detected_by_name() {
        let redactor = Redactor::new();
        for vector in synthesized_corpus() {
            let trace = redactor.redact_with_replacement_spans(&vector.input);
            for expected in &vector.expected_matches {
                let mut intervals = trace
                    .replacements
                    .iter()
                    .filter(|(name, _, _)| *name == expected.pattern_name)
                    .map(|(_, start, end)| (*start as u32, *end as u32))
                    .collect::<Vec<_>>();
                intervals.sort_unstable();
                let matched_by_name =
                    span_is_fully_covered(expected.start, expected.end, &intervals);
                assert!(
                    matched_by_name,
                    "vector {} expected pattern {} to replace all of span {}..{}; replacements: {:?}",
                    vector.name,
                    expected.pattern_name,
                    expected.start,
                    expected.end,
                    trace.replacements,
                );
            }
        }
    }

    #[test]
    fn coverage_rejects_partial_prefix_suffix_and_single_byte_matches() {
        let vector = RedactorTestVector {
            name: "partial_coverage".to_string(),
            input: "abcdefgh".to_string(),
            expected_matches: vec![ExpectedMatch {
                pattern_name: "generic_secret".to_string(),
                start: 2,
                end: 6,
            }],
            provider: "test".to_string(),
            rationale: "Any surviving expected byte is a false negative".to_string(),
        };
        for ranges in [
            vec![("prefix", 2, 4)],
            vec![("suffix", 4, 6)],
            vec![("one_byte", 1, 3)],
            vec![("left", 2, 4), ("right", 5, 6)],
            vec![("adjacent_before", 0, 2), ("adjacent_after", 6, 8)],
        ] {
            let evaluation = classify_replacement_spans(&vector, &ranges);
            assert!(evaluation.validation_errors.is_empty());
            assert_eq!(evaluation.true_positives, 0, "ranges: {ranges:?}");
            assert_eq!(evaluation.false_negatives, 1, "ranges: {ranges:?}");
        }
        for ranges in [
            vec![("exact", 2, 6)],
            vec![("superset", 0, 8)],
            vec![("right", 4, 6), ("left", 2, 4)],
            vec![("overlap", 3, 6), ("left", 2, 5), ("duplicate", 2, 5)],
        ] {
            let evaluation = classify_replacement_spans(&vector, &ranges);
            assert!(evaluation.validation_errors.is_empty());
            assert_eq!(evaluation.true_positives, 1, "ranges: {ranges:?}");
            assert_eq!(evaluation.false_negatives, 0, "ranges: {ranges:?}");
        }
    }

    #[test]
    fn coverage_checks_actual_output_for_surviving_secret_fragments() {
        let key = "ghp_AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
        for input in [
            format!("private_prefix|{key}"),
            format!("{key}|private_suffix"),
        ] {
            let vector = pos(
                "partial_real_redaction",
                "test",
                "planted missing bytes",
                &input,
                &input,
                "github_token",
            );
            let evaluation = evaluate_vector(&vector);
            let output = Redactor::new().redact(&input);
            assert!(
                !output.contains(&input),
                "whole-token disappearance alone is insufficient"
            );
            assert!(output.contains("private_prefix") || output.contains("private_suffix"));
            assert_eq!(evaluation.true_positives, 0);
            assert_eq!(evaluation.false_negatives, 1);
            assert!(
                evaluation
                    .per_detection
                    .iter()
                    .any(|record| record.outcome == MatchOutcome::PartialCoverage)
            );
        }
    }

    #[test]
    fn coverage_unions_unicode_source_bytes_and_rejects_invalid_metadata() {
        let mut vector = RedactorTestVector {
            name: "unicode_coverage".to_string(),
            input: "é秘密abcd".to_string(),
            expected_matches: vec![ExpectedMatch {
                pattern_name: "generic_secret".to_string(),
                start: 0,
                end: 12,
            }],
            provider: "test".to_string(),
            rationale: "Offsets are UTF-8 byte boundaries".to_string(),
        };
        let full = [("latin", 0, 2), ("cjk", 2, 8), ("ascii", 8, 12)];
        assert_eq!(classify_replacement_spans(&vector, &full).true_positives, 1);
        for (start, end) in [(0, 0), (6, 5), (0, 13), (1, 2), (0, 1)] {
            vector.expected_matches[0].start = start;
            vector.expected_matches[0].end = end;
            let evaluation = classify_replacement_spans(&vector, &full);
            assert!(
                !evaluation.validation_errors.is_empty(),
                "interval: {start}..{end}"
            );
            assert_eq!(evaluation.true_positives, 0);
            let snapshot = MatrixSnapshot::evaluate(&[vector.clone()]);
            assert!(!snapshot.meets_recall_floor(0.0));
            let mut health = RedactorCoverageHealth::baseline();
            fold_snapshot(&mut health, &snapshot, 0.0);
            assert!(!health.is_safe());
        }
        vector.expected_matches[0].start = 0;
        vector.expected_matches[0].end = 12;
        for (start, end) in [(0, 0), (6, 5), (0, 13), (1, 2), (0, 1)] {
            let evaluation = classify_replacement_spans(&vector, &[("invalid", start, end)]);
            assert!(!evaluation.validation_errors.is_empty());
            assert_eq!(evaluation.true_positives, 0);
        }
    }

    #[test]
    fn evaluate_vector_records_true_positive() {
        let v = pos(
            "smoke_anthropic",
            "anthropic",
            "smoke",
            "key=sk-ant-api03-1234567890123456789012345678901234567890",
            "sk-ant-api03-1234567890123456789012345678901234567890",
            "anthropic_key",
        );
        let eval = evaluate_vector(&v);
        assert_eq!(eval.true_positives, 1);
        assert_eq!(eval.false_negatives, 0);
        assert_eq!(eval.false_positives, 0);
    }

    #[test]
    fn evaluate_vector_records_false_negative_for_unmatched_expectation() {
        // A valid expected span with no production replacement is an FN.
        let v = RedactorTestVector {
            name: "unreachable_expectation".to_string(),
            input: "no secrets here".to_string(),
            expected_matches: vec![ExpectedMatch {
                pattern_name: "openai_key".to_string(),
                start: 0,
                end: 14,
            }],
            provider: "test".to_string(),
            rationale: "synth FN".to_string(),
        };
        let eval = evaluate_vector(&v);
        assert_eq!(eval.true_positives, 0);
        assert_eq!(eval.false_negatives, 1);
        assert_eq!(eval.false_positives, 0);
    }

    #[test]
    fn evaluate_vector_records_false_positive_on_unexpected_match() {
        let v = RedactorTestVector {
            name: "unexpected_match".to_string(),
            input: "TOK=ghp_AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA".to_string(),
            expected_matches: vec![],
            provider: "test".to_string(),
            rationale: "synth FP — the redactor catches the GitHub PAT we said was clean"
                .to_string(),
        };
        let eval = evaluate_vector(&v);
        assert_eq!(eval.true_positives, 0);
        assert_eq!(eval.false_negatives, 0);
        assert_eq!(eval.false_positives, 1);
    }

    #[test]
    fn matrix_snapshot_aggregates_per_provider() {
        let v_pos = pos(
            "p1",
            "alpha",
            "smoke",
            "X=AKIAIOSFODNN7EXAMPLE",
            "AKIAIOSFODNN7EXAMPLE",
            "aws_access_key_id",
        );
        let v_neg = neg("n1", "alpha", "negative", "no secrets here");
        let snap = MatrixSnapshot::evaluate(&[v_pos, v_neg]);
        let alpha = snap.by_provider.get("alpha").unwrap();
        assert_eq!(alpha.vectors_evaluated, 2);
        assert_eq!(alpha.true_positives, 1);
        assert_eq!(alpha.false_negatives, 0);
        assert_eq!(alpha.false_positives, 0);
        assert!((alpha.recall() - 1.0).abs() < 1e-9);
        assert!((alpha.precision() - 1.0).abs() < 1e-9);
    }

    #[test]
    fn provider_counters_handle_zero_division() {
        let p = ProviderCounters::default();
        assert!((p.recall() - 1.0).abs() <= f64::EPSILON);
        assert!((p.precision() - 1.0).abs() <= f64::EPSILON);
    }

    #[test]
    fn baseline_health_is_unsafe_until_measured() {
        // Per ft-cy273 fix: cold baseline (no vectors evaluated)
        // must NOT report safe. Previously the function returned
        // true because providers_below_recall_floor is zero by
        // construction in baseline(), which made the doctor
        // surface report green before any coverage probe ran.
        let h = RedactorCoverageHealth::baseline();
        assert!(
            !h.is_safe(),
            "cold baseline must be unsafe (no measurement yet)",
        );
        // Recall/precision stay at 1.0 in the absence of FN/FP
        // because the denominators are zero — this is intentional
        // for the rate accessors.
        assert!((h.overall_recall() - 1.0).abs() <= f64::EPSILON);
        assert!((h.overall_precision() - 1.0).abs() <= f64::EPSILON);
    }

    #[test]
    fn fold_snapshot_with_clean_data_marks_safe() {
        // Per ft-cy273 fix: once vectors have been evaluated AND
        // every provider clears the floor, is_safe == true.
        let mut snap = MatrixSnapshot {
            vectors_total: 5,
            overall: ProviderCounters {
                true_positives: 5,
                false_negatives: 0,
                false_positives: 0,
                vectors_evaluated: 5,
            },
            by_provider: BTreeMap::new(),
            vectors: vec![],
        };
        snap.by_provider.insert(
            "good".to_string(),
            ProviderCounters {
                true_positives: 5,
                false_negatives: 0,
                false_positives: 0,
                vectors_evaluated: 5,
            },
        );
        let mut h = RedactorCoverageHealth::baseline();
        assert!(!h.is_safe(), "baseline must be unsafe");
        fold_snapshot(&mut h, &snap, 0.99);
        assert!(
            h.is_safe(),
            "post-fold with clean data must be safe; vectors={}, below_floor={}",
            h.vectors_evaluated_total,
            h.providers_below_recall_floor,
        );
    }

    #[test]
    fn fold_snapshot_marks_unsafe_below_floor() {
        // Synthesize a snapshot with an under-floor provider.
        let mut snap = MatrixSnapshot {
            vectors_total: 1,
            overall: ProviderCounters {
                true_positives: 1,
                false_negatives: 1,
                false_positives: 0,
                vectors_evaluated: 1,
            },
            by_provider: BTreeMap::new(),
            vectors: vec![],
        };
        snap.by_provider.insert(
            "weak".to_string(),
            ProviderCounters {
                true_positives: 1,
                false_negatives: 1,
                false_positives: 0,
                vectors_evaluated: 1,
            },
        );
        let mut h = RedactorCoverageHealth::baseline();
        fold_snapshot(&mut h, &snap, 0.99);
        assert!(!h.is_safe());
        assert_eq!(h.providers_below_recall_floor, 1);
    }

    #[test]
    fn jsonl_evaluations_roundtrip() {
        let v = pos(
            "smoke",
            "test",
            "smoke",
            "AKIAIOSFODNN7EXAMPLE in env",
            "AKIAIOSFODNN7EXAMPLE",
            "aws_access_key_id",
        );
        let snap = MatrixSnapshot::evaluate(&[v]);
        let jsonl = render_evaluations_jsonl(&snap.vectors);
        let parsed = parse_evaluations_jsonl(&jsonl).unwrap();
        assert_eq!(parsed, snap.vectors);
    }

    #[test]
    fn min_provider_recall_returns_lowest() {
        let v_high = pos(
            "h",
            "alpha",
            "high",
            "AKIAIOSFODNN7EXAMPLE here",
            "AKIAIOSFODNN7EXAMPLE",
            "aws_access_key_id",
        );
        let v_unmatched = RedactorTestVector {
            name: "u".to_string(),
            input: "no secret".to_string(),
            expected_matches: vec![ExpectedMatch {
                pattern_name: "openai_key".to_string(),
                start: 0,
                end: 5,
            }],
            provider: "beta".to_string(),
            rationale: "synth FN".to_string(),
        };
        let snap = MatrixSnapshot::evaluate(&[v_high, v_unmatched]);
        let (name, _) = snap.min_provider_recall().unwrap();
        assert_eq!(name, "beta");
    }

    #[test]
    fn spans_overlap_predicate() {
        assert!(super::spans_overlap(0, 5, 3, 7)); // partial
        assert!(super::spans_overlap(0, 10, 3, 5)); // contained
        assert!(super::spans_overlap(3, 5, 0, 10)); // contains
        assert!(!super::spans_overlap(0, 5, 5, 10)); // adjacent
        assert!(!super::spans_overlap(5, 10, 0, 5));
        assert!(!super::spans_overlap(0, 5, 6, 10));
    }
}
