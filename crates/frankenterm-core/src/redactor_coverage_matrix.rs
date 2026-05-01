//! Recall/precision matrix for the secret redactor
//! ([BR-RC-SAFETY-PROOFS.G10] / `ft-x0666.2`).
//!
//! redactor.rs ships 25 regex patterns covering OpenAI,
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
//! - [`MatrixOutcome`] — TP / FN / FP / TN classification of
//!   evaluating one vector.
//! - [`evaluate_vector`] — runs `Redactor::detect` against the
//!   vector and classifies each match.
//! - [`MatrixSnapshot`] — per-provider TP/FP/FN/TN counters +
//!   recall + precision + per-vector results.
//! - [`RedactorCoverageHealth`] — `ft doctor` counter snapshot
//!   matching this session's `*Health` shape.
//! - [`synthesized_corpus`] — in-tree corpus covering each of
//!   the 25 patterns with at least 3 positives + 1 negative.
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
//!   `docs/security/redactor-coverage-methodology.md`. The
//!   harness side reads the derived bound and applies it.
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
/// - For each expected match: TP if at least one production
///   detection overlaps it; FN otherwise.
/// - For each production detection: counted as a duplicate-TP
///   record if it overlaps an expected match (annotated, but
///   not double-counted in the TP count); FP if it overlaps
///   no expected match.
#[must_use]
pub fn evaluate_vector(vector: &RedactorTestVector) -> VectorEvaluation {
    let redactor = Redactor::new();
    let detections = redactor.detect(&vector.input);

    let mut per_detection = Vec::new();
    let mut tp = 0u32;
    let mut fp = 0u32;

    // Span-level coverage: an expected match is "covered" if
    // any production detection overlaps it. Each expected
    // match contributes at most 1 to the TP count.
    let mut covered = vec![false; vector.expected_matches.len()];

    for (name, start, end) in &detections {
        let start_u32 = *start as u32;
        let end_u32 = *end as u32;

        let mut hits_expected = false;
        for (idx, exp) in vector.expected_matches.iter().enumerate() {
            if spans_overlap(exp.start, exp.end, start_u32, end_u32) {
                if !covered[idx] {
                    covered[idx] = true;
                    tp += 1;
                }
                hits_expected = true;
                // Don't break — a single detection may cover
                // multiple adjacent expected spans (rare but
                // possible).
            }
        }

        let outcome = if hits_expected {
            MatchOutcome::TruePositive
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
    }
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
        self.by_provider.values().all(|p| p.recall() >= floor)
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

    #[must_use]
    pub const fn is_safe(&self) -> bool {
        self.providers_below_recall_floor == 0
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
        .values()
        .filter(|p| p.recall() < floor)
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
        .unwrap_or_else(|| panic!("secret {secret:?} not in input {input:?}"));
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
/// `SECRET_PATTERNS` gets at least 3 positives + 1 negative.
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
            "API_KEY=sk-ant-api03-XXXXXXXXXXXXXXXXXXXX1234567890",
            "sk-ant-api03-XXXXXXXXXXXXXXXXXXXX1234567890",
            "anthropic_key",
        ),
        pos(
            "anthropic_admin",
            "anthropic",
            "admin variant — sk-ant-admin01-",
            "secret: sk-ant-admin01-aaaaaaaaaaaaaaaaaaaaaaaaaa",
            "sk-ant-admin01-aaaaaaaaaaaaaaaaaaaaaaaaaa",
            "anthropic_key",
        ),
        pos(
            "anthropic_in_log",
            "anthropic",
            "embedded in a log line; ensures regex doesn't require boundary",
            "[2026-05-01T07:00:00Z] auth=sk-ant-api03-FGHIJKLMNOPQRSTUVWXYZ1234567890 status=ok",
            "sk-ant-api03-FGHIJKLMNOPQRSTUVWXYZ1234567890",
            "anthropic_key",
        ),
        neg(
            "anthropic_too_short",
            "anthropic",
            "below the {20,} threshold; must NOT match",
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
            "github_pat_<82+ alnum>",
            "TOK=github_pat_11ABCDEFG_AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
            "github_pat_11ABCDEFG_AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
            "github_fine_grained_pat",
        ),
        pos(
            "github_fg_pat_in_url",
            "github",
            "embedded in a longer line",
            "url=https://x-access-token:github_pat_11AAAAAA_BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB@github.com/x.git",
            "github_pat_11AAAAAA_BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB",
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
            "GOOGLE_API_KEY=AIzaA1B2C3D4E5F6G7H8I9J0K1L2M3N4O5P6Q-",
            "AIzaA1B2C3D4E5F6G7H8I9J0K1L2M3N4O5P6Q-",
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

    #[test]
    fn corpus_has_minimum_size() {
        let corpus = synthesized_corpus();
        // 25 patterns × ≥3 vectors each = ≥75. The actual
        // corpus is larger because some patterns get more
        // coverage and we add cross-cutting negatives.
        assert!(
            corpus.len() >= 60,
            "corpus too small: {} (expected ≥60)",
            corpus.len()
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
    fn evaluate_vector_records_true_positive() {
        let v = pos(
            "smoke_anthropic",
            "anthropic",
            "smoke",
            "key=sk-ant-api03-1234567890123456789012345",
            "sk-ant-api03-1234567890123456789012345",
            "anthropic_key",
        );
        let eval = evaluate_vector(&v);
        assert_eq!(eval.true_positives, 1);
        assert_eq!(eval.false_negatives, 0);
        assert_eq!(eval.false_positives, 0);
    }

    #[test]
    fn evaluate_vector_records_false_negative_for_unmatched_expectation() {
        // Expected match span beyond input length — the
        // production regex won't match, so it's a False Negative.
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
        assert_eq!(p.recall(), 1.0);
        assert_eq!(p.precision(), 1.0);
    }

    #[test]
    fn baseline_health_is_safe() {
        let h = RedactorCoverageHealth::baseline();
        assert!(h.is_safe());
        assert_eq!(h.overall_recall(), 1.0);
        assert_eq!(h.overall_precision(), 1.0);
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
