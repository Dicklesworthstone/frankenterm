# Semantic Search PAC-Bayes Recall Bound

**Bead:** `ft-tf6g3.26`
**Proof category:** `7` (`information-theoretic`)
**Machine artifact:** `docs/attestations/proofs/semantic-search-pac-bayes.json`

This note defines the release-bundle contract for a PAC-Bayes lower
bound on semantic or hybrid search recall. It does not claim a
non-vacuous production bound yet. The current repository evidence has
deterministic semantic-quality fixtures, but it does not yet have a
production-realistic held-out query distribution, calibration/test
split, or frozen posterior over embedder and retriever hyperparameters.

## Claim Shape

For a frozen retriever sampled from posterior `Q`, a prior `P`, a
held-out query distribution `D`, and top-k loss

```text
loss(q, h) = 1 when at least one relevant segment for query q is
             absent from h(q)'s top-k results, else 0,
```

the true risk is `R(Q) = E_{h~Q, q~D}[loss(q, h)]`. The corresponding
recall lower bound is:

```text
recall_lower_bound = 1 - risk_upper_bound
```

The release artifact must publish a lower bound with confidence
`1 - delta = 0.99`. Until the prerequisites below exist, the only
truthful lower bound is `0.0`.

## Bound

The operational contract uses the PAC-Bayes-kl family associated with
McAllester and Catoni-style posterior analysis:

```text
kl(r_hat || r) <= (KL(Q || P) + ln(2 * sqrt(n) / delta)) / n
```

where:

- `r_hat` is empirical top-k miss rate on the held-out test split.
- `r` is true top-k miss risk.
- `n` is the number of independent test queries.
- `KL(Q || P)` is measured in nats.
- `delta = 0.01` for a 99% confidence statement.

For a release gate, compute the smallest `r` in `[r_hat, 1]` whose
Bernoulli KL divergence from `r_hat` satisfies the inequality. The
published recall lower bound is `max(0, 1 - r)`.

The checked artifact also records the conservative square-root
relaxation already used by existing FrankenTerm PAC-Bayes code:

```text
r <= r_hat + sqrt((KL(Q || P) + ln(2 * sqrt(n) / delta)) / (2n))
```

The inverse-kl value is the preferred published number. The
square-root value is retained for review because it is monotone, easy
to audit by hand, and never more optimistic than the exact release
gate if the implementation treats it as a fallback ceiling.

## Prerequisites

A non-vacuous bound requires all of the following:

1. A production-realistic held-out query set sampled from operator
   search behavior, not from synthetic fixture construction.
2. A calibration split used only to freeze query normalization,
   top-k, RRF weight, embedder selection, and threshold policy.
3. A test split used exactly once for the published bound.
4. A prior `P` over embedder and retriever settings that is fixed
   before the test split is evaluated.
5. A posterior `Q` that records the final distribution over:
   `embedder_id`, `embedder_tier`, embedding dimension, normalization,
   reranker policy, RRF `k`, and top-k cutoff.
6. Per-query relevance judgments with reviewer provenance.

The built-in harness in `crates/frankenterm-core/src/semantic_quality.rs`
currently contains four deterministic representative queries. Those
fixtures are useful regression tests, but they are not admitted as
production-realistic held-out data for this bound.

## Current Published Bound

Because the admitted production held-out sample count is zero:

```text
n = 0
delta = 0.01
KL(Q || P) = not measured
recall_lower_bound_99 = 0.0
status = blocked_prerequisites_not_met
```

This is intentionally vacuous. It prevents README or release copy from
turning semantic-search fixture recall into a claim about future user
queries.

## Sample-Size Floor

For planning, assume an ideal zero-miss test split (`r_hat = 0`) and
use the conservative square-root relaxation. The minimum `n` needed to
make the risk ceiling at most `epsilon` is the first integer satisfying:

```text
sqrt((KL(Q || P) + ln(2 * sqrt(n) / 0.01)) / (2n)) <= epsilon
```

| Target recall lower bound | Epsilon risk | KL=0 | KL=1 | KL=5 | KL=10 | KL=25 |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| 0.90 | 0.10 | 416 | 469 | 678 | 936 | 1,701 |
| 0.95 | 0.05 | 1,810 | 2,021 | 2,856 | 3,887 | 6,945 |
| 0.99 | 0.01 | 53,721 | 58,953 | 79,707 | 105,406 | 181,768 |

The one-point tightening requirement in the bead is therefore
operationalized as: for a zero-miss split at 99% confidence, moving a
published lower bound from 0.98 to 0.99 requires enough additional test
queries to move from the `epsilon = 0.02` row to the `epsilon = 0.01`
row after plugging in the measured `KL(Q || P)`. The artifact records
the 0.99 row now because the posterior is not frozen yet.

## Sensitivity

The bound is intentionally sensitive to posterior complexity. At
99% confidence and zero observed misses, the sample requirement for a
0.99 recall lower bound is:

- 53,721 queries when `KL(Q || P) = 0`.
- 79,707 queries when `KL(Q || P) = 5`.
- 181,768 queries when `KL(Q || P) = 25`.

That sensitivity is useful: if a future retriever has many tunable
degrees of freedom, it must either freeze a tighter prior/posterior or
earn the claim with a larger held-out set.

## Sources

- Olivier Catoni, *Pac-Bayesian Supervised Classification: The
  Thermodynamics of Statistical Learning*, arXiv:0712.0248,
  https://arxiv.org/abs/0712.0248.
- David A. McAllester, *PAC-Bayesian Model Averaging*, COLT 1999,
  https://doi.org/10.1145/307400.307435.
- Gintare Karolina Dziugaite and Daniel M. Roy, *Computing
  Nonvacuous Generalization Bounds for Deep (Stochastic) Neural
  Networks with Many More Parameters than Training Data*, arXiv:1703.11008,
  https://arxiv.org/abs/1703.11008.
