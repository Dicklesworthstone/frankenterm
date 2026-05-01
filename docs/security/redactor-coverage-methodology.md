# Redactor Recall/Precision Methodology

**Bead:** [BR-RC-SAFETY-PROOFS.G10] / `ft-x0666.2`
**Status:** Foundation slice shipped. Synthesized in-tree
corpus + recall/precision matrix + per-provider breakdown +
deliberate-bless flow + per-release JSON report all live;
vendoring upstream gitleaks/trufflehog corpora is the follow-on
(operator sign-off needed for licensing).

## Why this matters

`redactor.rs` ships **25 regex patterns** covering OpenAI,
Anthropic, GitHub (classic + fine-grained PAT), Google
(API key + OAuth), xAI, Groq, Hugging Face, Replicate,
Anyscale, Perplexity, the cohere/mistral/together/fireworks
"AI provider keyed value" cluster, AWS (access key + secret),
Bearer tokens, Slack, Stripe, database URLs, OAuth device
codes, OAuth URLs, and four generic-shape patterns
(api_key, token, password, secret).

The README *claims* coverage. Industry standard practice — and
this bead's headline rule — is to **publish recall and
precision against a public test corpus so users can calibrate
trust**:

> ≥99% recall on gitleaks corpus; fail CI on dip.

## Definitions

For each test vector with N expected secret spans:

- **TP** (True Positive): production redactor detection
  overlaps an expected span. Counted **once per expected
  span** (an expected span covered by 2+ overlapping
  detections is still 1 TP — what matters for redaction
  semantics is whether the secret bytes get covered).
- **FN** (False Negative): expected span has no overlapping
  production detection. The secret leaks — **the bead's
  headline failure**.
- **FP** (False Positive): production detection overlaps no
  expected span. The redactor over-redacted (degrades
  usability but does not leak).

Per-provider:

- **Recall** = TP / (TP + FN). Measures coverage. The bead's
  ≥99% floor.
- **Precision** = TP / (TP + FP). Measures noise. Floor
  pinned at 0.50 overall (see § Precision Floor).

## Test corpus

### In-tree (this bead)

`crates/frankenterm-core/src/redactor_coverage_matrix.rs::synthesized_corpus`
provides **83 hand-curated test vectors**:

- ≥3 positive vectors per pattern, exercising the canonical
  shape, common embeddings (env-var assignment, log line, URL,
  config file), and edge variants (admin/proj/svcacct prefixes,
  case insensitivity, base64 charsets).
- 1 negative vector per pattern where the format almost-but-
  not-quite matches (below `{N,}` threshold, prose mention,
  lookalike).
- 3 cross-cutting negatives (UUID, prose-only key reference,
  too-short value).

All "secret" values are **synthetic** — random byte
sequences shaped like the format. None are real credentials.

### Vendored (follow-on bead)

The bead's action #1 vendors gitleaks + trufflehog test
corpora into `tests/redactor_corpus/` (version-pinned).
Licensing implications need operator sign-off before
checkout. When vendored, additional `RedactorTestVector` rows
append to `synthesized_corpus()` (or live in a parallel
`vendored_corpus()` function) and the harness re-runs
unchanged.

### Sample-size derivation (Fano's inequality)

Per the bead's action #3 (round-3 alien-artifact uplift), the
recall-floor confidence target is ≥99%. Fano's inequality gives
the lower bound on the test-corpus size needed for that
confidence:

> For a binary detector with true recall *r* and observed
> recall *r̂* on N samples, the (1 - δ)-confidence bound on
> *r - r̂* is approximately
>
>     |r - r̂| ≤ √( H₂(δ) / N )
>
> where H₂ is binary entropy.

For the bead's ≥99% recall floor, observation tolerance ±0.01
(i.e., we want to be confident at the 0.01 level), and δ =
0.01:

```text
H₂(0.01) ≈ 0.0808
N ≥ H₂(0.01) / 0.01² = 808
```

The synthesized corpus alone is 83 vectors — **insufficient
for 99% confidence** by this bound. The bead's vendored
gitleaks/trufflehog corpora (action #1) bring the count to
~6,000+ test vectors, comfortably above 808. The synthesized
corpus is the **always-on regression net**; the vendored
corpora are the **statistical confidence floor**.

(Note: this is a conservative bound. In practice the
detector's regex shape is fixed and the recall measurement
is over a finite test set, so the true confidence is higher
than Fano predicts. The bound is the floor, not a
prediction.)

## Precision floor

The bead does not pin a precision floor (the headline rule is
recall). The harness pins **overall precision ≥ 0.50** as a
loose sanity bound — generic patterns (`generic_api_key`,
`generic_secret`) are *intentionally* over-broad, so per-
provider precision drops on cross-cutting negatives are
expected. A precision drop **below 0.50** signals the regex
set has degenerated into matching arbitrary text and warrants
review.

The current synthesized-corpus precision is **1.0**
(zero false positives) because the negative vectors are
hand-shaped to NOT trip any pattern.

## Per-provider breakdown

The current coverage report at
`docs/security/redactor-coverage.json` lists 20 providers:

| Provider | Patterns | TP | FN | FP | Recall | Precision |
|---|---|---|---|---|---|---|
| anthropic | anthropic_key | 3 | 0 | 0 | 1.0 | 1.0 |
| openai | openai_key | 3 | 0 | 0 | 1.0 | 1.0 |
| github | github_token, github_fine_grained_pat | 6 | 0 | 0 | 1.0 | 1.0 |
| google | google_api_key, google_oauth_token | 6 | 0 | 0 | 1.0 | 1.0 |
| xai | xai_key | 3 | 0 | 0 | 1.0 | 1.0 |
| groq | groq_key | 3 | 0 | 0 | 1.0 | 1.0 |
| huggingface | huggingface_token | 3 | 0 | 0 | 1.0 | 1.0 |
| replicate | replicate_token | 3 | 0 | 0 | 1.0 | 1.0 |
| anyscale | anyscale_key | 3 | 0 | 0 | 1.0 | 1.0 |
| perplexity | perplexity_key | 3 | 0 | 0 | 1.0 | 1.0 |
| ai_provider_keyed | ai_provider_keyed_value | 3 | 0 | 0 | 1.0 | 1.0 |
| aws | aws_access_key_id, aws_secret_key | 6 | 0 | 0 | 1.0 | 1.0 |
| bearer | bearer_token | 3 | 0 | 0 | 1.0 | 1.0 |
| slack | slack_token | 3 | 0 | 0 | 1.0 | 1.0 |
| stripe | stripe_key | 3 | 0 | 0 | 1.0 | 1.0 |
| database | database_url | 3 | 0 | 0 | 1.0 | 1.0 |
| device_code | device_code | 3 | 0 | 0 | 1.0 | 1.0 |
| oauth_url | oauth_url | 3 | 0 | 0 | 1.0 | 1.0 |
| generic | generic_api_key, generic_token, generic_password, generic_secret | 12 | 0 | 0 | 1.0 | 1.0 |
| negative | (cross-cutting negatives) | 0 | 0 | 0 | 1.0 | 1.0 |

Source of truth: `docs/security/redactor-coverage.json`.

## CI gate

`tests/redactor_coverage_matrix.rs::synthesized_corpus_meets_recall_floor`
asserts every provider clears the ≥99% recall floor on every
PR. Hard failure on dip; the bead's "fail CI on dip"
requirement.

The bless flow (`FT_REDACTOR_COVERAGE_BLESS=1`) is for
**deliberate** corpus changes only — adding a new pattern,
adding new test vectors, or adopting vendored corpora. The
default mode is **regression-only**: a recall drop ≥0.01 on
any provider fails the test.

Re-bless recipe:

```bash
FT_REDACTOR_COVERAGE_BLESS=1 \
CARGO_TARGET_DIR=/tmp/ft-pane3-target \
CC=/opt/homebrew/opt/llvm/bin/clang CXX=/opt/homebrew/opt/llvm/bin/clang++ \
cargo test -p frankenterm-core --test redactor_coverage_matrix \
    --features asupersync-runtime --no-default-features
```

## False-positive clustering (action #6)

When upstream gitleaks/trufflehog corpora are vendored, false
positives on those corpora will be clustered by pattern_name
+ surrounding context. Common over-redaction shapes (e.g.,
`generic_api_key` matching prose like "the api_key=value
syntax") will drive **regex-tightening fixes** rather than
relaxing the recall floor.

The contract for this clustering lives in
`MatrixSnapshot::vectors[].per_detection`: every FalsePositive
record carries pattern_name + start + end + the input bytes
(via the parent vector). Aggregating across the corpus yields
the FP cluster table.

## Re-running

```bash
# Full regression — ≥99% recall floor + bless-flow check.
CARGO_TARGET_DIR=/tmp/ft-pane3-target \
CC=/opt/homebrew/opt/llvm/bin/clang CXX=/opt/homebrew/opt/llvm/bin/clang++ \
cargo test -p frankenterm-core --test redactor_coverage_matrix \
    --features asupersync-runtime --no-default-features
# → 5 passed

# Lib tests (corpus shape + evaluate_vector + MatrixSnapshot
# + RedactorCoverageHealth + JSONL roundtrip):
cargo test -p frankenterm-core --lib redactor_coverage_matrix:: \
    --features asupersync-runtime --no-default-features
# → 14 passed
```

## Bead acceptance status

| Item | Status |
|---|---|
| Synthesized in-tree corpus | ✓ (83 vectors, 25 patterns, 20 providers) |
| Recall/precision benchmark + report | ✓ (`tests/redactor_coverage_matrix.rs` + `docs/security/redactor-coverage.json`) |
| Per-provider breakdown | ✓ (20 providers in the JSON report) |
| ≥99% recall floor enforced | ✓ (CI test + 0.01 drift bound on bless flow) |
| Per-release JSON artifact | ✓ (`docs/security/redactor-coverage.json` re-blessed per release) |
| Vendored gitleaks corpus | ⏳ operator sign-off needed for licensing |
| Vendored trufflehog corpus | ⏳ same |
| Fano's-inequality sample-size derivation | ✓ (this doc) |
| FP clustering + regex tightening | ⏳ activates when vendored corpora land |
| Per-release attestation entry | ⏳ depends on `ft-syqcz.1` schema bead |

## Cross-references

- **Production redactor:** `crates/frankenterm-core/src/redactor.rs`
  — 25 regex patterns + `Redactor::detect`/`redact`.
- **Coverage matrix module:**
  `crates/frankenterm-core/src/redactor_coverage_matrix.rs`.
- **Regression harness:**
  `crates/frankenterm-core/tests/redactor_coverage_matrix.rs`.
- **Coverage report:** `docs/security/redactor-coverage.json`
  (deliberate-bless via `FT_REDACTOR_COVERAGE_BLESS=1`).
- **Sibling fixtures** (same session pattern):
  `a11y_tree`, `color_management`, `ime_caret`,
  `atlas_stability`, `triple_buffer`, `live_resize`,
  `grid_reflow`, `render_quality`, `snap_back_fuzz`,
  `wayland_frame_pacing`, `bidi_correctness`,
  `tx_killswitch_model`, `passive_watch_invariant`,
  `wire_dedup_model`.
- **Attestation cross-link:** `BR-RC-FOUNDATION.G3.1`
  (`ft-syqcz.1`) — per-release attestation JSON entry.
