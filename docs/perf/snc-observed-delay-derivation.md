# Observed-Delay Heavy-Tail Bound

This note documents the `ft-perf-gate::snc::compute_snc_bound` contract for
`ft-tf6g3.56`.

## Scope

`compute_snc_bound` consumes `EvidenceSample.metric_value` as direct observed
end-to-end delay measurements in the sample's own `metric_unit`. It does not
consume arrival-rate samples, service rates, service bursts, or a queueing
topology. The returned `SncBound::HeavyTailBound.delay_ms` field keeps the
legacy wire name, but its numeric unit is the input metric unit.

The function is therefore an observed-delay Pareto tail quantile gate. It is
not a full stochastic network calculus service-curve composition.

## Derivation

For positive finite samples, the upper `k` order statistics are sorted so that
`X_(n-k)` is the Hill threshold `xm`. The Hill estimator is:

```text
1 / alpha_hat = (1 / k) * sum_i ln(X_(n-i) / X_(n-k))
```

For a Pareto upper tail:

```text
P(X > x) ~= (xm / x)^alpha
```

Inverting the upper tail at confidence `p` gives:

```text
x_p = xm * (1 / (1 - p))^(1 / alpha_hat)
```

`compute_snc_bound` returns `x_p` when `alpha_hat` is finite and below
`SncConfig.heavy_tail_alpha_threshold`. When `alpha_hat` is at or above that
threshold, it returns `SncBound::LindleyDomain` so callers can use the
deterministic Lindley path instead.

## Limits

This bound is only as good as the assumption that the upper tail is well
approximated by a Pareto distribution. Hill estimates on Gaussian or other
light-tail finite samples can be biased, so callers that need strict
heavy-tail classification should pair this gate with a goodness-of-fit check.

No service-curve parameter can modulate the returned bound. Any release,
attestation, or operator-facing wording must describe this as an observed-delay
tail-quantile proof unless a future implementation actually composes arrival
and service envelopes.
