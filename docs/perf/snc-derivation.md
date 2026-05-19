# Observed-delay heavy-tail derivation

`ft_perf_gate::snc` is the G38 heavy-tail proof-gate substrate for already
observed latency samples. Despite the historical `SNC` name, the current
implementation is not a Jiang-style queueing composition over an arrival
envelope and service curve. It computes a Pareto upper-tail quantile from
direct delay observations carried by `EvidenceSample.metric_value`.

This document is the retained derivation for `ft-tf6g3.56`.

## Input contract

Each sample is an observed end-to-end delay for one claim stream, in the unit
recorded by `EvidenceSample.metric_unit`. The implementation filters to
positive finite `metric_value` rows and uses `SncConfig`:

- `confidence`: target probability `p`, required to be in `[0.5, 1)`.
- `hill_k`: number of upper-order statistics for the Hill estimator, clamped to
  at least `2`.
- `heavy_tail_alpha_threshold`: alpha threshold below which the sample is routed
  to the observed-delay heavy-tail bound. The default is `2.0`.

`compute_snc_bound(samples, cfg)` deliberately has no service-curve parameter.
Changing `MgfServiceCurve.rate`, `MgfServiceCurve.burst`, or a stability margin
cannot affect this path because those types are not part of the direct-delay
API.

## Hill tail estimator

Let `X_1..X_n` be the positive finite sample values sorted ascending. Let
`k = max(cfg.hill_k, 2)` and `x_m = X_(n-k)`, the threshold immediately below
the largest `k` observations. The Hill estimator is:

```text
1 / alpha_hat = (1 / k) * sum_{i=1..k} ln(X_(n-i+1) / x_m)
```

If there are fewer than `k + 1` positive finite samples, if `x_m <= 0`, or if
the estimate is non-finite, the gate returns `SncBound::OutOfDomain`.

## Quantile bound

For a Pareto upper tail,

```text
P(X > x) ~= (x_m / x)^alpha
```

Inverting at tail probability `1 - p` gives:

```text
x_p = x_m * (1 / (1 - p))^(1 / alpha_hat)
```

The returned `SncBound::HeavyTailBound.delay_ms` is `x_p`. The field name keeps
the historical wire-format suffix, but the value is in the same unit as the
input evidence stream. If the input unit is microseconds or bytes per line, the
numeric bound is in that same unit.

If `alpha_hat >= cfg.heavy_tail_alpha_threshold`, the gate returns
`SncBound::LindleyDomain` so callers can use the deterministic Lindley/min-plus
path instead.

## Limits

This is an observed-delay tail gate, not a full stochastic network calculus
proof. It does not estimate arrivals, does not model service, does not compose
multi-stage queues, and does not prove that a rate-latency service curve absorbs
a heavy-tail arrival envelope. Release or attestation wording may cite this path
only as an empirical observed-delay Pareto quantile bound unless a future bead
adds a real arrival/service composition and retained tests showing service-curve
parameters change the bound.

Current proof hooks:

- `crates/ft-perf-gate/src/snc.rs` contains the Hill estimator, quantile
  calculation, and a regression test pinning the direct-delay function
  signature.
- `crates/ft-perf-gate/tests/fixture_corpus_integration.rs` exercises the
  Pareto `heavy-tail` and stationary baseline fixture streams.
- `docs/perf/fixture-coverage.md` records the heavy-tail fixture as an
  observed-delay tail-gate consumer, not a queueing-envelope consumer.
