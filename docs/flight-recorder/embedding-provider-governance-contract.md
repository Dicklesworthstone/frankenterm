# Embedding Provider Interface + Model Governance Contract (`ft-oegrb.5.1`)

Date: 2026-02-12  
Status: Approved design contract for semantic track bring-up  
Owners: `BoldBarn` + semantic track implementers

## Why this exists
Semantic retrieval quality and stability are dominated by provider/model policy, not only ranking code.
This contract defines the pluggable embedding interface and deterministic governance rules needed before implementing `ft-oegrb.5.2` and `ft-oegrb.5.3`.

## Inputs
- `docs/flight-recorder/adr-0001-flight-recorder-architecture.md`
- `docs/flight-recorder/cass-two-tier-architecture-dossier.md`
- `docs/flight-recorder/xf-hybrid-retrieval-dossier.md`
- `docs/flight-recorder/cross-project-extraction-matrix.md`

## Scope
In scope:
- Provider abstraction API (local/remote swappable).
- Model selection rubric and deployment profiles.
- Embedding normalization and versioning compatibility rules.
- Deterministic fallback behavior when semantic embedding is unavailable.

Out of scope:
- Full provider implementation.
- ANN index design details (`ft-oegrb.5.3+`).
- Fine-tuned relevance evaluation corpus (`ft-oegrb.7.*`).

## Terminology
- `provider`: embedding service implementation (local runtime or remote API).
- `model_spec`: fully qualified model identity including revision/version.
- `embedding_profile`: immutable compatibility fingerprint for vector production.
- `semantic_available`: runtime state indicating semantic path is usable.

## Provider Interface Contract

### Required trait shape (normative)
```rust
pub trait EmbeddingProvider: Send + Sync {
    fn provider_id(&self) -> &'static str;
    fn deployment_mode(&self) -> DeploymentMode;
    fn model_spec(&self) -> ModelSpec;
    fn health(&self) -> ProviderHealth;

    async fn embed_query(&self, text: &str, opts: EmbedOpts) -> Result<EmbeddingVector, EmbedError>;
    async fn embed_batch(
        &self,
        inputs: &[String],
        opts: EmbedOpts,
    ) -> Result<Vec<EmbeddingVector>, EmbedError>;
}
```

### Required supporting types
- `DeploymentMode = Local | Remote | Hybrid`.
- `ModelSpec`:
  - `name`
  - `revision`
  - `dimension`
  - `token_limit`
  - `license_class`
- `EmbeddingVector`:
  - `values: Vec<f32>`
  - `profile_id: String`
  - `normalized: bool` (must be `true` for persisted vectors)
- `EmbedError` variants:
  - `RateLimited`
  - `Unavailable`
  - `AuthFailed`
  - `Misconfigured`
  - `Transient`
  - `Permanent`

### Registry contract
The query/index pipeline must depend on a provider registry, not a concrete provider:
```rust
pub trait EmbeddingProviderRegistry {
    fn resolve(&self, policy: &SemanticPolicy) -> Result<Arc<dyn EmbeddingProvider>, ResolveError>;
}
```
No query-layer API may import provider-specific SDK types.

## Model Governance Strategy

### Approved model catalog policy
Only models explicitly listed in config are allowed for production indexing/querying:
- `semantic.allowed_models[]` is the source of truth.
- `semantic.default_model` must be one of the allowed models.
- Unknown model IDs are hard-fail at startup.

### Selection rubric (required dimensions)
Each candidate model is scored on:
- Retrieval quality (offline eval on recorder corpus).
- Latency p95 (query and batch indexing separately).
- Cost (USD / 1M tokens or equivalent infra cost).
- Privacy posture (local-only, self-hosted, third-party).
- Operational resilience (offline capability, outage behavior, rate-limit tolerance).

### Reference selection matrix (baseline)
| Profile | Quality | Latency | Cost | Privacy | Offline |
|---|---|---|---|---|---|
| Local compact model | Medium | Low | Low | High | Yes |
| Local high-quality model | High | Medium-High | Medium | High | Yes |
| Remote managed model | High | Medium | High/variable | Medium-Low | No |

Policy rule:
- Default production profile should maximize privacy and deterministic availability unless quality regression exceeds accepted threshold.

## Normalization and Versioning Contract

### Mandatory normalization
- Persisted embeddings must be L2-normalized.
- All similarity computation in v1 assumes cosine-equivalent dot product on normalized vectors.
- Non-normalized vectors are rejected at writer boundary.

### Embedding profile compatibility ID
Every persisted vector must carry:
`profile_id = "{provider}:{model}:{revision}:dim={d}:norm=l2:chunker={chunker_ver}:preproc={preproc_ver}"`

Compatibility rules:
- Different `profile_id` values must never be mixed in the same active semantic index generation.
- Profile change requires a new generation and controlled rebuild.
- Hybrid query path must assert profile match before semantic scoring.

## Deployment Modes

### Local mode
- Provider executes inside FrankenTerm process or local worker.
- Preferred for privacy-sensitive environments.
- Must function without external network.

### Remote mode
- Provider uses external API endpoint.
- Must enforce explicit timeout, retry budget, and rate-limit backoff.
- Must expose health and quota status for policy decisions.

### Hybrid mode
- Local primary + remote fallback OR remote primary + local fallback.
- Ordering must be deterministic and configured (no dynamic best-effort switching without audit).

## Deterministic Outage and Fallback Behavior

### Query-time behavior
When semantic provider is unavailable:
- Return lexical results only.
- Include machine-readable reason: `semantic_unavailable`.
- Do not fail the entire query unless caller explicitly requested `semantic_only`.

### Index-time behavior
When embeddings fail during indexing:
- Mark chunks as `embedding_pending` with retry metadata.
- Apply bounded retry budget.
- After budget exhaustion, emit explicit degraded-state marker and continue lexical indexing.

### Hard safety rules
- Never silently emit random/default vectors.
- Never block lexical query path on semantic provider health.
- Every fallback decision must be observable via structured status/audit fields.

## Required Configuration Surface
```toml
[semantic]
enabled = true
deployment_mode = "local" # local|remote|hybrid
default_model = "text-embed-local-v1"
allowed_models = ["text-embed-local-v1", "text-embed-remote-v2"]
query_timeout_ms = 1500
batch_timeout_ms = 10000
retry_budget = 3
fallback_mode = "lexical_only" # lexical_only|fail_semantic_only
```

## Acceptance Mapping for `ft-oegrb.5.1`
- Embedding abstraction API with pluggable providers:
  - Defined by `EmbeddingProvider` + `EmbeddingProviderRegistry` contracts.
- Model selection matrix:
  - Defined in governance rubric and baseline matrix above.
- Normalization/versioning strategy:
  - Defined by mandatory L2 normalization + immutable `profile_id`.
- Outage/fallback policy:
  - Defined by deterministic query/index fallback rules and hard safety constraints.

## Handoff to downstream beads
- `ft-oegrb.5.2`: consume `profile_id`, `chunker_ver`, and fallback constraints for chunk metadata design.
- `ft-oegrb.5.3`: implement vector storage lifecycle keyed by `profile_id` generation boundaries.
- `ft-oegrb.5.4`: enforce lexical-safe hybrid behavior based on `semantic_unavailable` reason codes.
