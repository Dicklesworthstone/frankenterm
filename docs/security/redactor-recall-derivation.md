# Redactor Recall Lower-Bound Derivation

**Bead:** `ft-tf6g3.9`
**Artifact:** `docs/security/redactor-coverage.json`
**Status:** Derived floor published; external corpus expansion tracked by
`ft-tf6g3.35`.

## Scope

The production catalog in
`crates/frankenterm-core/src/redactor.rs::SECRET_PATTERNS` currently
contains 32 secret-pattern classes. For the recall derivation, add one
clean/negative class for non-secret inputs:

```text
K = 32 secret classes
M = K + 1 = 33 total classes
```

The coverage harness now reads the live pattern names through
`secret_pattern_names()` and fails if a pattern class lacks synthetic
positive rows. That guards the per-release report against catalog drift.

## What Fano Bounds

Fano's inequality is a class-discrimination bound, not a binomial
confidence interval. For a uniformly distributed true class `X` and a
redactor decision `Y`, the probability of classification error `Pe`
satisfies:

```text
H(X | Y) <= H2(Pe) + Pe log2(M - 1)
I(X;Y) = H(X) - H(X | Y)
```

Therefore, to make class error at most `alpha = 0.01`
information-theoretically plausible across `M = 33` classes, the
observation must retain at least:

```text
H2(0.01) = 0.080793136 bits
log2(33) = 5.044394119 bits
0.01 * log2(32) = 0.050000000 bits

I_min = log2(33) - H2(0.01) - 0.01 log2(32)
      = 4.913600983 bits
```

This value is published in `sample_size_floor` as
`fano_min_mutual_information_bits`.

## What Gives The Sample-Size Floor

The honest finite-sample recall floor comes from the one-sided
zero-miss binomial calculation per secret class. To claim recall
`r >= 0.99` with confidence `1 - alpha = 0.99` after observing zero
misses in `N` positive examples:

```text
P(zero misses | true recall = r0) = r0^N
r0^N <= alpha
N >= log(alpha) / log(r0)
N >= log(0.01) / log(0.99)
N >= 458.210576553
N* = 459 positive examples per secret class
```

The always-on synthesized corpus has 3 positive examples for each live
secret class, so it is a regression net, not a statistically sufficient
99% recall claim. `docs/security/redactor-coverage.json` marks every
pattern class as `under_sampled` until the external corpus work reaches
the 459-positive floor or records a class-specific unavailable-corpus
exception.

## External Corpus Status

The external corpus is intentionally not vendored in this slice. The
approved follow-up is `ft-tf6g3.35`.

Primary-source license check:

- Gitleaks is MIT-licensed at
  `https://raw.githubusercontent.com/gitleaks/gitleaks/master/LICENSE`.
- TruffleHog is AGPL-3.0-licensed at
  `https://raw.githubusercontent.com/trufflesecurity/trufflehog/main/LICENSE`.

That mixed licensing is why the coverage artifact reports
`external_corpus_status: "not_vendored_license_signoff_required"` and
why `ft-tf6g3.35` requires operator/license sign-off before vendoring
or differential cross-classifier fixtures.
