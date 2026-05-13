#!/usr/bin/env python3
"""Deterministic evidence-stream fixture regenerator.

G54 (ft-tf6g3.42) deliverable. Synthesizes per-claim JSONL streams
conforming to docs/perf/evidence-stream-schema.json (G47).

Five fixture flavors per claim:
    baseline-30d     — realistic 30-day-equivalent baseline (no anomaly)
    regression-injected — 15% step regression at a labeled timestamp
    regime-shift     — distribution shift (mean + variance) at a labeled timestamp
    heavy-tail       — Pareto alpha=1.5 arrivals for stochastic-NC tests
    sparse           — fewer than min-sample-count rows for degradation tests

Determinism is enforced by hashing (claim_id, flavor) into the PRNG
seed; rerunning the script produces bit-identical JSONL output.

Usage:
    scripts/regenerate-evidence-corpus.py --claim robot.p95
    scripts/regenerate-evidence-corpus.py --all
    scripts/regenerate-evidence-corpus.py --check     # verify existing files match
"""
import argparse
import dataclasses
import hashlib
import json
import math
import pathlib
import random
import sys
import typing

SCHEMA_VERSION = "ft.perf.evidence-sample.v1"
ROOT = pathlib.Path(__file__).resolve().parent.parent
CORPUS_ROOT = ROOT / "tests" / "fixtures" / "evidence-corpus"
MANIFEST_PATH = CORPUS_ROOT / "manifest.json"

# Five exemplar claims; G54 spec asks each of the 5 unproven headline claims
# from G19 to get its own per-claim tree. The script accepts --claim to do
# one at a time or --all to regenerate every supported claim.
CLAIMS: dict[str, dict] = {
    "robot.p95": {
        "metric_unit": "ms",
        "baseline_mean": 4.2,
        "baseline_stddev": 0.5,
        "sample_size_per_row": 1024,
        "workload_class": "robot-mode-mixed-fixtures",
        "description": "Robot Mode response latency p95 (G19.2 ft-tf6g3.4.2 headline claim).",
    },
    "fts5.query_p99": {
        "metric_unit": "ms",
        "baseline_mean": 7.5,
        "baseline_stddev": 1.2,
        "sample_size_per_row": 10000,
        "workload_class": "fts5-fixture-1k-corpus",
        "description": "FTS5 lexical-search query p99 (G19.1 ft-tf6g3.4.1 headline claim).",
    },
    "memory.hot_bytes_per_line": {
        "metric_unit": "bytes_per_line",
        "baseline_mean": 198.0,
        "baseline_stddev": 6.0,
        "sample_size_per_row": 200,
        "workload_class": "idle-swarm-200-panes-target-class",
        "description": "Memory per pane hot tier (G19.3 ft-tf6g3.4.3 headline claim).",
    },
    "memory.warm_bytes_per_line": {
        "metric_unit": "bytes_per_line",
        "baseline_mean": 39.5,
        "baseline_stddev": 2.0,
        "sample_size_per_row": 200,
        "workload_class": "warm-tier-200-panes-target-class",
        "description": "Memory per pane warm tier at 5:1 zstd (G19.4 ft-tf6g3.4.4 headline claim).",
    },
    "bloom.speedup_ratio": {
        "metric_unit": "ratio",
        "baseline_mean": 25.0,
        "baseline_stddev": 5.0,
        "sample_size_per_row": 500,
        "workload_class": "bloom-large-corpus-infrequent-query",
        "description": "Bloom filter 10-100x speedup (G19.5 ft-tf6g3.4.5 headline claim).",
    },
}

FLAVORS = [
    "baseline-30d",
    "regression-injected",
    "regime-shift",
    "heavy-tail",
    "sparse",
]


@dataclasses.dataclass(frozen=True)
class FixtureSpec:
    claim_id: str
    flavor: str
    label: str
    row_count: int
    ground_truth: dict


def stable_seed(claim_id: str, flavor: str) -> int:
    h = hashlib.sha256(f"{claim_id}|{flavor}|{SCHEMA_VERSION}".encode()).digest()
    return int.from_bytes(h[:8], "big")


def synthesize(spec: FixtureSpec, cfg: dict) -> list[dict]:
    rng = random.Random(stable_seed(spec.claim_id, spec.flavor))
    rows: list[dict] = []
    base_ts = 1_715_500_000_000
    mean = cfg["baseline_mean"]
    stddev = cfg["baseline_stddev"]

    if spec.flavor == "baseline-30d":
        for i in range(spec.row_count):
            value = rng.gauss(mean, stddev)
            rows.append(_row(spec.claim_id, base_ts + i * 60_000, value, cfg, i))
    elif spec.flavor == "regression-injected":
        cut = spec.row_count // 2
        for i in range(spec.row_count):
            v_mean = mean if i < cut else mean * 1.15  # 15% regression step
            value = rng.gauss(v_mean, stddev)
            rows.append(_row(spec.claim_id, base_ts + i * 60_000, value, cfg, i))
    elif spec.flavor == "regime-shift":
        cut = spec.row_count // 2
        for i in range(spec.row_count):
            v_mean = mean if i < cut else mean * 1.30  # 30% mean shift
            v_std = stddev if i < cut else stddev * 1.8  # variance shift too
            value = rng.gauss(v_mean, v_std)
            rows.append(_row(spec.claim_id, base_ts + i * 60_000, value, cfg, i))
    elif spec.flavor == "heavy-tail":
        # Pareto alpha=1.5 — finite mean, INFINITE variance — exactly the regime
        # classical Lindley breaks down on and stochastic-NC (G38) is built for.
        alpha = 1.5
        xm = mean * (alpha - 1) / alpha  # scale to keep mean at baseline
        for i in range(spec.row_count):
            u = rng.random()
            value = xm / (u ** (1.0 / alpha))
            rows.append(_row(spec.claim_id, base_ts + i * 60_000, value, cfg, i))
    elif spec.flavor == "sparse":
        # Deliberately below the SPRT min_samples=2 threshold (1 row) so that
        # downstream degradation behavior is exercisable.
        value = rng.gauss(mean, stddev)
        rows.append(_row(spec.claim_id, base_ts, value, cfg, 0))
    else:
        raise ValueError(f"unknown flavor: {spec.flavor}")
    return rows


def _row(claim_id: str, ts: int, value: float, cfg: dict, seq: int) -> dict:
    return {
        "schema_version": SCHEMA_VERSION,
        "ts_ms": ts,
        "claim_id": claim_id,
        "metric_value": round(value, 4),
        "metric_unit": cfg["metric_unit"],
        "sample_size": cfg["sample_size_per_row"],
        "commit_sha": "fd89ed378",  # G47 schema-shipping commit
        "workload_class": cfg["workload_class"],
        "tags": {
            "frankenterm_version": "0.0.0-dev",
            "fixture_seq": str(seq),
        },
    }


def row_count_for(flavor: str) -> int:
    if flavor == "baseline-30d":
        return 720  # 12 per hour * 24 * 30/12 = abbreviated 30-day at 1-min cadence
    if flavor in ("regression-injected", "regime-shift", "heavy-tail"):
        return 200
    if flavor == "sparse":
        return 1
    raise ValueError(f"unknown flavor: {flavor}")


def label_for(flavor: str, claim_id: str) -> str:
    cuts = {"regression-injected": "at sample 100", "regime-shift": "at sample 100"}
    if flavor == "baseline-30d":
        return f"stationary baseline for {claim_id}"
    if flavor == "regression-injected":
        return f"15% step regression {cuts[flavor]} in {claim_id}"
    if flavor == "regime-shift":
        return f"30% mean + 1.8x stddev shift {cuts[flavor]} in {claim_id}"
    if flavor == "heavy-tail":
        return f"Pareto alpha=1.5 distribution for {claim_id} (finite mean, infinite variance)"
    if flavor == "sparse":
        return f"single-row stream for {claim_id} (sub-min-samples degradation)"
    raise ValueError(f"unknown flavor: {flavor}")


def ground_truth_for(flavor: str) -> dict:
    if flavor == "baseline-30d":
        return {"shift": False, "regression": False, "tail_class": "light"}
    if flavor == "regression-injected":
        return {"shift": False, "regression": True, "change_point_index": 100, "magnitude_pct": 15}
    if flavor == "regime-shift":
        return {"shift": True, "regression": False, "change_point_index": 100, "shift_kind": "mean+variance"}
    if flavor == "heavy-tail":
        return {"shift": False, "regression": False, "tail_class": "pareto-alpha-1.5"}
    if flavor == "sparse":
        return {"shift": False, "regression": False, "below_min_samples": True}
    raise ValueError(f"unknown flavor: {flavor}")


def write_fixture(spec: FixtureSpec, cfg: dict) -> pathlib.Path:
    rows = synthesize(spec, cfg)
    claim_dir = CORPUS_ROOT / "per-claim" / spec.claim_id
    claim_dir.mkdir(parents=True, exist_ok=True)
    path = claim_dir / f"{spec.flavor}.jsonl"
    with path.open("w") as f:
        for r in rows:
            f.write(json.dumps(r, sort_keys=True, separators=(",", ":")) + "\n")
    return path


def content_hash(path: pathlib.Path) -> str:
    return hashlib.sha256(path.read_bytes()).hexdigest()


def regenerate_claim(claim_id: str) -> list[dict]:
    cfg = CLAIMS[claim_id]
    manifest_entries: list[dict] = []
    for flavor in FLAVORS:
        spec = FixtureSpec(
            claim_id=claim_id,
            flavor=flavor,
            label=label_for(flavor, claim_id),
            row_count=row_count_for(flavor),
            ground_truth=ground_truth_for(flavor),
        )
        path = write_fixture(spec, cfg)
        manifest_entries.append({
            "claim_id": claim_id,
            "flavor": flavor,
            "path": str(path.relative_to(ROOT)),
            "row_count": spec.row_count,
            "ground_truth": spec.ground_truth,
            "label": spec.label,
            "content_hash": content_hash(path),
            "seed": stable_seed(claim_id, flavor),
        })
    return manifest_entries


def write_manifest(entries: list[dict]) -> None:
    manifest = {
        "schema_version": "ft.perf.evidence-corpus.v1",
        "schema_ref": "docs/perf/evidence-stream-schema.json",
        "generated_by": "scripts/regenerate-evidence-corpus.py",
        "bead": "ft-tf6g3.42",
        "claims_supported": sorted(CLAIMS.keys()),
        "flavors": FLAVORS,
        "fixtures": entries,
    }
    MANIFEST_PATH.parent.mkdir(parents=True, exist_ok=True)
    with MANIFEST_PATH.open("w") as f:
        json.dump(manifest, f, indent=2, sort_keys=True)
        f.write("\n")


def check_mode() -> int:
    if not MANIFEST_PATH.exists():
        print("manifest.json missing; run without --check to generate", file=sys.stderr)
        return 2
    manifest = json.loads(MANIFEST_PATH.read_text())
    failures = 0
    for entry in manifest["fixtures"]:
        path = ROOT / entry["path"]
        if not path.exists():
            print(f"MISSING: {path}", file=sys.stderr)
            failures += 1
            continue
        actual = content_hash(path)
        if actual != entry["content_hash"]:
            print(f"HASH-DRIFT: {path}\n  expected {entry['content_hash']}\n  actual   {actual}", file=sys.stderr)
            failures += 1
    if failures:
        print(f"\n{failures} fixture(s) drifted; regenerate via --all", file=sys.stderr)
        return 1
    print(f"OK: {len(manifest['fixtures'])} fixtures match manifest")
    return 0


def main() -> int:
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("--claim", choices=sorted(CLAIMS.keys()), help="regenerate one claim's 5 flavors")
    p.add_argument("--all", action="store_true", help="regenerate every supported claim")
    p.add_argument("--check", action="store_true", help="verify existing fixtures match manifest hashes")
    args = p.parse_args()

    if args.check:
        return check_mode()

    if not (args.claim or args.all):
        p.error("must pass --claim CLAIM, --all, or --check")

    targets = sorted(CLAIMS.keys()) if args.all else [args.claim]
    entries: list[dict] = []
    if MANIFEST_PATH.exists() and not args.all:
        prior = json.loads(MANIFEST_PATH.read_text()).get("fixtures", [])
        entries.extend(e for e in prior if e["claim_id"] not in targets)
    for claim_id in targets:
        entries.extend(regenerate_claim(claim_id))
    entries.sort(key=lambda e: (e["claim_id"], e["flavor"]))
    write_manifest(entries)
    print(f"regenerated {len(entries)} fixtures across {len(targets)} claim(s); manifest: {MANIFEST_PATH.relative_to(ROOT)}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
