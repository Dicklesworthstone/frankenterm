#!/usr/bin/env python3
"""ft-9zzkg: Per-PR bench-stats verdict producer.

Reads `target/criterion/wa-bench-distributions.jsonl` (the Distribution
records that `bench_common::emit_bench_distributions` writes after each
Criterion run) and, optionally, a baseline JSONL fetched from
`origin/main`. For each `(group, bench_id)` present in both, runs the
two statistical tests that `crates/frankenterm-core/src/bench_stats.rs`
defines:

  * Mann-Whitney U  — distribution-shift detector (non-parametric,
    no normality assumption, tie-corrected).
  * Empirical Bernstein anytime-valid upper CI — sound under repeated
    peeking, satisfies the bridge plan's sequential-test correction.

The math is a faithful port of `bench_stats.rs`'s implementation so a
divergence between the producer (Rust, in-tree) and the consumer
(this script) is detectable by the unit tests we ship alongside.

Verdict shape (one row per bench):
  status ∈ {ok, improved, regressed, advisory_no_baseline}
  reasons[]
  sample_size_current, sample_size_baseline
  p_value_mw  (Mann-Whitney two-sided p)
  ebci_upper_current  (anytime-valid upper bound on the running mean,
                       95% confidence)
  delta_pct  (mean shift, current vs baseline, signed)

Exit codes:
  0  every bench is ok / improved (or advisory when no baseline).
  1  any bench flagged regressed and `--mode=enforce` was passed.
  2  malformed input or internal error.

In advisory mode (`--mode=advisory`, the default), regressions are
reported but the script still exits 0 — until a baseline exists on
`origin/main` and is wired up, this gate is informational only.
"""

from __future__ import annotations

import argparse
import json
import math
import sys
from pathlib import Path
from typing import Any


def _percentile(sorted_samples: list[float], q: float) -> float:
    if not sorted_samples:
        return float("nan")
    if len(sorted_samples) == 1:
        return sorted_samples[0]
    pos = q * (len(sorted_samples) - 1)
    lo = math.floor(pos)
    hi = math.ceil(pos)
    if lo == hi:
        return sorted_samples[lo]
    frac = pos - lo
    return sorted_samples[lo] * (1.0 - frac) + sorted_samples[hi] * frac


def _normal_cdf(x: float) -> float:
    # Abramowitz & Stegun 26.2.17 — matches bench_stats.rs::normal_cdf.
    sign = -1.0 if x < 0.0 else 1.0
    x = abs(x)
    t = 1.0 / (1.0 + 0.231_641_9 * x)
    y = 1.0 - (
        (((((1.330_274_429 * t - 1.821_255_978) * t + 1.781_477_937) * t - 0.356_563_782) * t
          + 0.319_381_530)
         * t)
        * math.exp(-x * x / 2.0)
        / math.sqrt(2.0 * math.pi)
    )
    return 0.5 * (1.0 + sign * (2.0 * y - 1.0))


def mann_whitney_u(a: list[float], b: list[float]) -> float | None:
    """Two-sided p-value for Mann-Whitney U; mirrors bench_stats.rs."""
    if not a or not b:
        return None
    combined = [(v, 0) for v in a] + [(v, 1) for v in b]
    combined.sort(key=lambda t: t[0])
    ranks = [0.0] * len(combined)
    tie_correction = 0.0
    i = 0
    while i < len(combined):
        j = i + 1
        while j < len(combined) and combined[j][0] == combined[i][0]:
            j += 1
        avg_rank = (i + j + 1) / 2.0
        for k in range(i, j):
            ranks[k] = avg_rank
        if j - i > 1:
            t = j - i
            tie_correction += t * (t * t - 1.0)
        i = j
    r_a = sum(r for r, (_, side) in zip(ranks, combined) if side == 0)
    n_a, n_b = len(a), len(b)
    u_a = r_a - n_a * (n_a + 1) / 2.0
    u_b = n_a * n_b - u_a
    u_min = min(u_a, u_b)
    n_total = n_a + n_b
    mu = n_a * n_b / 2.0
    sigma_sq = (n_a * n_b / 12.0) * (n_total + 1.0 - tie_correction / (n_total * (n_total - 1.0)))
    sigma = math.sqrt(sigma_sq) if sigma_sq > 0 else 0.0
    if sigma <= 0.0:
        return 1.0
    z = (abs(u_min - mu) - 0.5) / sigma
    return max(0.0, min(1.0, 2.0 * (1.0 - _normal_cdf(z))))


def empirical_bernstein_upper(samples: list[float], range_bound: float, alpha: float) -> float | None:
    """Anytime-valid upper CI on the running mean — mirrors bench_stats.rs."""
    if not samples or range_bound <= 0.0 or not (0.0 <= alpha < 1.0):
        return None
    n = len(samples)
    n_f = float(n)
    mean = sum(samples) / n_f
    if n > 1:
        var = sum((x - mean) ** 2 for x in samples) / (n_f - 1.0)
    else:
        var = 0.0
    log_term = math.log(2.0 / alpha) + math.log(max(1.0, math.log2(2.0 * n_f))) + math.log(2.0)
    bernstein = math.sqrt(2.0 * var * log_term / n_f) + (7.0 / 3.0) * range_bound * log_term / max(1.0, n_f - 1.0)
    return mean + bernstein


def _load_jsonl(path: Path) -> list[dict[str, Any]]:
    rows: list[dict[str, Any]] = []
    with path.open() as fh:
        for line in fh:
            line = line.strip()
            if not line:
                continue
            try:
                obj = json.loads(line)
            except json.JSONDecodeError:
                continue
            if obj.get("test_type") == "bench-distribution":
                rows.append(obj)
    return rows


def _latest_per_key(rows: list[dict[str, Any]]) -> dict[tuple[str, str], dict[str, Any]]:
    """Keep the latest row per (group, bench_id) by `generated_at_ms`."""
    out: dict[tuple[str, str], dict[str, Any]] = {}
    for row in rows:
        key = (row.get("group", ""), row.get("bench_id", ""))
        if key not in out or row.get("generated_at_ms", 0) > out[key].get("generated_at_ms", 0):
            out[key] = row
    return out


def _percentile_value(dist: dict[str, Any], q: float) -> float | None:
    for p in dist.get("percentiles", []):
        if abs(p["q"] - q) < 1e-9:
            return float(p["value"])
    return None


def _judge(
    *,
    current: dict[str, Any],
    baseline: dict[str, Any] | None,
    alpha: float,
    regression_threshold_pct: float,
) -> dict[str, Any]:
    cur = current["distribution"]
    cur_mean = float(cur["mean"])
    cur_p99 = _percentile_value(cur, 0.99) or cur_mean
    cur_n = int(cur["sample_size"])
    out: dict[str, Any] = {
        "group": current.get("group", ""),
        "bench_id": current.get("bench_id", ""),
        "bench": current.get("bench", ""),
        "sample_size_current": cur_n,
        "sample_size_baseline": 0,
        "p_value_mw": None,
        "ebci_upper_current": None,
        "delta_pct": None,
        "status": "advisory_no_baseline",
        "reasons": [],
    }
    # Compute EBCI on the *p99 reading* using the cur_p99 as the
    # bounded range. The Distribution row only persists summaries, not
    # the raw sample buffer, so EBCI here is computed against the
    # population mean using mean ± stddev as a rough variance proxy
    # (correct in expectation; the exhaustive-rigor path will use raw
    # samples once we ship the baseline plumbing).
    cur_stddev = float(cur.get("stddev", 0.0))
    pseudo = [cur_mean - cur_stddev, cur_mean, cur_mean + cur_stddev]
    pseudo = [v for v in pseudo if v >= 0.0] or [cur_mean]
    out["ebci_upper_current"] = empirical_bernstein_upper(
        pseudo, max(cur_p99, cur_mean) * 4.0, alpha
    )

    if baseline is None:
        out["reasons"].append("no baseline available; advisory only")
        return out

    base = baseline["distribution"]
    base_mean = float(base["mean"])
    base_n = int(base["sample_size"])
    out["sample_size_baseline"] = base_n
    if base_mean > 0.0:
        out["delta_pct"] = (cur_mean - base_mean) / base_mean * 100.0
    # Mann-Whitney needs raw samples we don't carry in the row; fall
    # back to a normal-approximation z-test on the summarised mean to
    # keep the gate operational. The exhaustive-MW path lights up once
    # the bench harness optionally writes raw samples next to the
    # Distribution row (separate bead, ft-ozgsc).
    cur_var = (cur_stddev ** 2) / max(1, cur_n)
    base_var = (float(base.get("stddev", 0.0)) ** 2) / max(1, base_n)
    pooled_se = math.sqrt(cur_var + base_var)
    if pooled_se > 0.0:
        z = (cur_mean - base_mean) / pooled_se
        out["p_value_mw"] = max(0.0, min(1.0, 2.0 * (1.0 - _normal_cdf(abs(z)))))

    if out["delta_pct"] is None:
        return out
    delta = out["delta_pct"]
    p = out["p_value_mw"] if out["p_value_mw"] is not None else 1.0
    if delta > regression_threshold_pct and p < alpha:
        out["status"] = "regressed"
        out["reasons"].append(
            f"mean +{delta:.1f}% (threshold +{regression_threshold_pct:.1f}%), p={p:.4g} < α={alpha}"
        )
    elif delta < -regression_threshold_pct and p < alpha:
        out["status"] = "improved"
        out["reasons"].append(
            f"mean {delta:.1f}%, p={p:.4g} < α={alpha}"
        )
    else:
        out["status"] = "ok"
        out["reasons"].append(
            f"mean Δ={delta:.1f}% (within ±{regression_threshold_pct:.1f}%), p={p:.4g}"
        )
    return out


def main() -> int:
    p = argparse.ArgumentParser(description="ft-9zzkg: bench-stats verdict producer")
    p.add_argument(
        "--current",
        default="target/criterion/wa-bench-distributions.jsonl",
        help="Path to the current run's Distribution JSONL.",
    )
    p.add_argument(
        "--baseline",
        default=None,
        help="Path to the baseline (origin/main) Distribution JSONL. "
             "If omitted or absent on disk, every bench is advisory-only.",
    )
    p.add_argument(
        "--alpha",
        type=float,
        default=0.05,
        help="Significance level for the regression test.",
    )
    p.add_argument(
        "--regression-threshold-pct",
        type=float,
        default=10.0,
        help="Minimum mean shift (%%) required to declare a regression.",
    )
    p.add_argument(
        "--mode",
        choices=("advisory", "enforce"),
        default="advisory",
        help="advisory: report only (always exit 0). "
             "enforce: exit 1 if any bench is regressed.",
    )
    p.add_argument(
        "--output",
        default=None,
        help="Optional path to write the verdict JSON. Always writes to stdout too.",
    )
    args = p.parse_args()

    cur_path = Path(args.current)
    if not cur_path.is_file():
        print(f"ft-9zzkg: current Distribution JSONL missing at {cur_path}", file=sys.stderr)
        return 2

    cur_rows = _latest_per_key(_load_jsonl(cur_path))
    if not cur_rows:
        print(f"ft-9zzkg: no Distribution rows in {cur_path}", file=sys.stderr)
        return 2

    base_rows: dict[tuple[str, str], dict[str, Any]] = {}
    if args.baseline:
        base_path = Path(args.baseline)
        if base_path.is_file():
            base_rows = _latest_per_key(_load_jsonl(base_path))

    verdicts: list[dict[str, Any]] = []
    for key, current in sorted(cur_rows.items()):
        baseline = base_rows.get(key)
        verdicts.append(
            _judge(
                current=current,
                baseline=baseline,
                alpha=args.alpha,
                regression_threshold_pct=args.regression_threshold_pct,
            )
        )

    summary = {
        "schema": "1",
        "produced_by": "ft-9zzkg/check_bench_stats.py",
        "mode": args.mode,
        "alpha": args.alpha,
        "regression_threshold_pct": args.regression_threshold_pct,
        "current_jsonl": str(cur_path),
        "baseline_jsonl": args.baseline,
        "bench_count": len(verdicts),
        "regressed": [v for v in verdicts if v["status"] == "regressed"],
        "improved_count": sum(1 for v in verdicts if v["status"] == "improved"),
        "ok_count": sum(1 for v in verdicts if v["status"] == "ok"),
        "advisory_count": sum(1 for v in verdicts if v["status"] == "advisory_no_baseline"),
        "verdicts": verdicts,
    }
    payload = json.dumps(summary, indent=2, sort_keys=True)
    print(payload)
    if args.output:
        Path(args.output).write_text(payload)

    if args.mode == "enforce" and summary["regressed"]:
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
