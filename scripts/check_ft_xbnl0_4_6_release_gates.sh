#!/usr/bin/env bash
# ft-xbnl0.4.6 — release-readiness gate evaluator.
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
POLICY_PATH="${ROOT_DIR}/docs/ft-xbnl0-4-6-release-gates.json"
OUT_PATH="${ROOT_DIR}/docs/ft-xbnl0-4-6-release-gates-validation.json"
SELF_TEST=0

usage() {
  cat <<'USAGE'
Usage: check_ft_xbnl0_4_6_release_gates.sh [options]

Options:
  --root <path>          Override repository root
  --policy-path <path>   Override gate policy JSON path
  --output <path>        Output report JSON path
  --self-test            Run built-in evaluator self-tests
  -h, --help             Show this help

Exit codes:
  0  release gates passed
  1  release gates blocked or policy malformed
  2  evaluator internal error
USAGE
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --root) ROOT_DIR="$2"; shift 2 ;;
    --policy-path) POLICY_PATH="$2"; shift 2 ;;
    --output) OUT_PATH="$2"; shift 2 ;;
    --self-test) SELF_TEST=1; shift ;;
    -h|--help) usage; exit 0 ;;
    *) echo "Unknown option: $1" >&2; usage >&2; exit 2 ;;
  esac
done

mkdir -p "$(dirname "${OUT_PATH}")"

python3 - "${ROOT_DIR}" "${POLICY_PATH}" "${OUT_PATH}" "${SELF_TEST}" <<'PY'
from __future__ import annotations

import copy
import json
import sys
import tempfile
from dataclasses import dataclass
from datetime import datetime, timezone
from pathlib import Path
from typing import Any


@dataclass
class GateFailure(Exception):
    code: str
    message: str
    detail: dict[str, Any] | None = None


def now_iso() -> str:
    return datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")


def fail(code: str, message: str, detail: dict[str, Any] | None = None) -> None:
    raise GateFailure(code=code, message=message, detail=detail)


def require(cond: bool, code: str, message: str, detail: dict[str, Any] | None = None) -> None:
    if not cond:
        fail(code, message, detail)


def load_json(path: Path) -> dict[str, Any]:
    return json.loads(path.read_text(encoding="utf-8"))


def newest_summary(root: Path) -> Path | None:
    if not root.exists():
        return None
    candidates = sorted(root.glob("*/summary.json"))
    return candidates[-1] if candidates else None


def evaluate_policy(root: Path, policy_path: Path, policy: dict[str, Any]) -> dict[str, Any]:
    require(
        policy.get("contract_id") == "ft.xbnl0.4.6.release_gates.v1",
        "invalid_contract_id",
        "unexpected contract_id",
    )
    thresholds = policy.get("thresholds", {})
    required_pane_scales = thresholds.get("required_pane_scales")
    require(isinstance(required_pane_scales, list) and required_pane_scales,
            "missing_required_pane_scales",
            "thresholds.required_pane_scales must be a non-empty list")
    required_metric_count = thresholds.get("required_metric_count")
    min_smoke_cycles = thresholds.get("min_smoke_cycles")
    min_release_cycles = thresholds.get("min_release_cycles")
    max_peak_rss_mb = thresholds.get("max_peak_rss_mb")
    max_duration_s = thresholds.get("max_duration_s")
    required_backpressure_tiers = thresholds.get("required_backpressure_tiers", [])

    artifact_roots = policy.get("artifact_roots", {})
    leak_root = root / artifact_roots["leak_oracle_root"]
    soak_root = root / artifact_roots["soak_matrix_root"]
    guard_report_path = root / artifact_roots["guard_report"]

    checks: list[dict[str, Any]] = []

    leak_summary_path = newest_summary(leak_root)
    leak_detail = {
        "artifact_root": leak_root.as_posix(),
        "summary_path": None if leak_summary_path is None else leak_summary_path.as_posix(),
    }
    if leak_summary_path is None:
        checks.append({
            "gate_id": "REL-01-leak-oracle",
            "name": "Leak behavior",
            "status": "failed",
            "reason_code": "leak_gate_missing_summary",
            "blocking": True,
            "detail": leak_detail | {
                "action": "Run the ft-xbnl0.4.4 leak-oracle harness and capture a passing summary.json bundle."
            },
        })
    else:
        leak_summary = load_json(leak_summary_path)
        leak_passed = leak_summary.get("status") == "passed"
        checks.append({
            "gate_id": "REL-01-leak-oracle",
            "name": "Leak behavior",
            "status": "passed" if leak_passed else "failed",
            "reason_code": "leak_gate_passed" if leak_passed else "leak_gate_failed_summary",
            "blocking": True,
            "detail": leak_detail | {
                "summary_status": leak_summary.get("status"),
                "action": "Fix the failing deterministic leak regression or rerun the lane until status=passed.",
            },
        })

    if not guard_report_path.exists():
        checks.append({
            "gate_id": "REL-02-guard-surface",
            "name": "Permanent guard surface",
            "status": "failed",
            "reason_code": "guard_report_missing",
            "blocking": True,
            "detail": {
                "guard_report": guard_report_path.as_posix(),
                "action": "Generate docs/ft-xbnl0-5-2-finish-line-guards-validation.json before trusting release-readiness."
            },
        })
    else:
        guard_report = load_json(guard_report_path)
        guard_passed = guard_report.get("status") == "passed"
        checks.append({
            "gate_id": "REL-02-guard-surface",
            "name": "Permanent guard surface",
            "status": "passed" if guard_passed else "failed",
            "reason_code": "guard_surface_passed" if guard_passed else "guard_surface_failed",
            "blocking": True,
            "detail": {
                "guard_report": guard_report_path.as_posix(),
                "guard_status": guard_report.get("status"),
                "action": "Repair the permanent finish-line guards before using the release decision."
            },
        })

    soak_wrapper_path = newest_summary(soak_root)
    if soak_wrapper_path is None:
        checks.append({
            "gate_id": "REL-03-soak-confidence",
            "name": "Soak confidence",
            "status": "failed",
            "reason_code": "soak_wrapper_missing",
            "blocking": True,
            "detail": {
                "artifact_root": soak_root.as_posix(),
                "action": "Run the ft-xbnl0.4.5 soak wrapper and save a wrapper summary.json."
            },
        })
        checks.append({
            "gate_id": "REL-04-performance-budget",
            "name": "Performance budget",
            "status": "failed",
            "reason_code": "performance_source_missing",
            "blocking": True,
            "detail": {
                "artifact_root": soak_root.as_posix(),
                "action": "Run the ft-xbnl0.4.5 soak matrix so explicit RSS/duration budgets can be checked."
            },
        })
    else:
        wrapper = load_json(soak_wrapper_path)
        smoke_cycles = int(wrapper.get("profiles", {}).get("smoke", {}).get("cycles", 0))
        release_summaries = wrapper.get("profiles", {}).get("release", {}).get("summaries", [])
        release_cycles = len(release_summaries)
        smoke_summary_path = Path(wrapper.get("profiles", {}).get("smoke", {}).get("summary", ""))
        all_summary_paths = [smoke_summary_path] + [Path(path) for path in release_summaries]

        summaries: list[dict[str, Any]] = []
        missing_nested = []
        for summary_path in all_summary_paths:
            if not summary_path or not summary_path.exists():
                missing_nested.append(summary_path.as_posix())
                continue
            summaries.append(load_json(summary_path))

        pane_scales = sorted({
            int(scale)
            for summary in summaries
            for scale in summary.get("pane_scales", [])
        })
        metric_counts = sorted({int(summary.get("tests_run", 0)) for summary in summaries})
        release_shapes = sorted({
            json.dumps({
                "tests_run": summary.get("tests_run"),
                "pane_scales": summary.get("pane_scales"),
                "metric_names": summary.get("metric_names"),
            }, sort_keys=True)
            for summary in summaries[1:]
        })
        release_consistent = len(release_shapes) == 1 if summaries[1:] else False
        peak_rss_mb = max((float(summary.get("peak_rss_mb", 0.0)) for summary in summaries), default=0.0)
        max_duration = max((float(summary.get("max_duration_s", 0.0)) for summary in summaries), default=0.0)
        highest_tiers = sorted({
            summary.get("highest_backpressure_tier")
            for summary in summaries
            if summary.get("highest_backpressure_tier") is not None
        })
        backpressure_ok = any(tier in required_backpressure_tiers for tier in highest_tiers)

        soak_passed = (
            wrapper.get("status") == "passed"
            and smoke_cycles >= min_smoke_cycles
            and release_cycles >= min_release_cycles
            and not missing_nested
            and pane_scales == required_pane_scales
            and release_consistent
        )
        checks.append({
            "gate_id": "REL-03-soak-confidence",
            "name": "Soak confidence",
            "status": "passed" if soak_passed else "failed",
            "reason_code": "soak_confidence_passed" if soak_passed else "soak_confidence_failed",
            "blocking": True,
            "detail": {
                "wrapper_summary": soak_wrapper_path.as_posix(),
                "wrapper_status": wrapper.get("status"),
                "smoke_cycles": smoke_cycles,
                "release_cycles": release_cycles,
                "missing_nested_summaries": missing_nested,
                "pane_scales": pane_scales,
                "release_consistent": release_consistent,
                "action": "Regenerate the smoke/release soak matrix until the wrapper summary is passed and the nested summaries agree.",
            },
        })

        performance_passed = (
            not missing_nested
            and wrapper.get("status") == "passed"
            and metric_counts == [required_metric_count]
            and peak_rss_mb <= max_peak_rss_mb
            and max_duration <= max_duration_s
            and backpressure_ok
        )
        checks.append({
            "gate_id": "REL-04-performance-budget",
            "name": "Performance budget",
            "status": "passed" if performance_passed else "failed",
            "reason_code": "performance_budget_passed" if performance_passed else "performance_budget_failed",
            "blocking": True,
            "detail": {
                "wrapper_summary": soak_wrapper_path.as_posix(),
                "metric_counts": metric_counts,
                "peak_rss_mb": peak_rss_mb,
                "max_duration_s": max_duration,
                "highest_backpressure_tiers": highest_tiers,
                "required_metric_count": required_metric_count,
                "max_peak_rss_mb": max_peak_rss_mb,
                "max_duration_s_budget": max_duration_s,
                "required_backpressure_tiers": required_backpressure_tiers,
                "action": "Reduce cost or tune the runtime/workload until RSS, duration, and backpressure expectations hold.",
            },
        })

    overall_status = "passed"
    if any(check["blocking"] and check["status"] != "passed" for check in checks):
        overall_status = "failed"

    return {
        "checked_at": now_iso(),
        "policy_path": policy_path.as_posix(),
        "repo_root": root.as_posix(),
        "status": overall_status,
        "checks": checks,
        "summary": {
            "blocking_failed": sum(1 for check in checks if check["blocking"] and check["status"] != "passed"),
            "latest_leak_summary": None if leak_summary_path is None else leak_summary_path.as_posix(),
            "latest_soak_wrapper_summary": None if soak_wrapper_path is None else soak_wrapper_path.as_posix(),
            "guard_report": guard_report_path.as_posix(),
        },
    }


def run_self_tests() -> None:
    with tempfile.TemporaryDirectory() as td:
        root = Path(td)
        policy = {
            "contract_id": "ft.xbnl0.4.6.release_gates.v1",
            "artifact_roots": {
                "leak_oracle_root": "leak",
                "soak_matrix_root": "soak",
                "guard_report": "guards.json",
            },
            "thresholds": {
                "required_pane_scales": [1, 50, 100, 200],
                "required_metric_count": 8,
                "min_smoke_cycles": 1,
                "min_release_cycles": 3,
                "max_peak_rss_mb": 32.0,
                "max_duration_s": 3.0,
                "required_backpressure_tiers": ["Black"],
            },
        }
        (root / "leak" / "run-1").mkdir(parents=True)
        (root / "soak" / "run-1").mkdir(parents=True)
        (root / "guards.json").write_text(json.dumps({"status": "passed"}), encoding="utf-8")
        (root / "leak" / "run-1" / "summary.json").write_text(
            json.dumps({"status": "passed"}), encoding="utf-8"
        )
        smoke_summary = root / "soak" / "run-1" / "smoke.json"
        release_paths = [root / "soak" / "run-1" / f"release-{idx}.json" for idx in range(1, 4)]
        sample_summary = {
            "tests_run": 8,
            "peak_rss_mb": 16.6,
            "max_duration_s": 2.2,
            "highest_backpressure_tier": "Black",
            "pane_scales": [1, 50, 100, 200],
            "metric_names": [f"metric-{idx}" for idx in range(8)],
        }
        smoke_summary.write_text(json.dumps(sample_summary), encoding="utf-8")
        for path in release_paths:
            path.write_text(json.dumps(sample_summary), encoding="utf-8")
        (root / "soak" / "run-1" / "summary.json").write_text(
            json.dumps({
                "status": "passed",
                "profiles": {
                    "smoke": {"cycles": 1, "summary": smoke_summary.as_posix()},
                    "release": {
                        "cycles": 3,
                        "summaries": [path.as_posix() for path in release_paths],
                    },
                },
            }),
            encoding="utf-8",
        )
        passing = evaluate_policy(root, root / "policy.json", copy.deepcopy(policy))
        require(passing["status"] == "passed", "self_test_pass_case_failed", "expected passing case")

        (root / "leak" / "run-1" / "summary.json").unlink()
        failing = evaluate_policy(root, root / "policy.json", copy.deepcopy(policy))
        require(failing["status"] == "failed", "self_test_fail_case_failed", "expected missing leak summary to fail")
        leak_check = next(check for check in failing["checks"] if check["gate_id"] == "REL-01-leak-oracle")
        require(
            leak_check["reason_code"] == "leak_gate_missing_summary",
            "self_test_wrong_reason",
            "unexpected leak gate failure reason",
            leak_check,
        )


repo_root = Path(sys.argv[1])
policy_path = Path(sys.argv[2])
out_path = Path(sys.argv[3])
self_test = sys.argv[4] == "1"

try:
    require(policy_path.exists(), "policy_missing", f"policy missing at {policy_path}")
    policy = load_json(policy_path)
    if self_test:
        run_self_tests()
        out_path.write_text(
            json.dumps(
                {
                    "checked_at": now_iso(),
                    "status": "passed",
                    "mode": "self-test",
                    "policy_path": policy_path.as_posix(),
                    "repo_root": repo_root.as_posix(),
                },
                indent=2,
                sort_keys=True,
            )
            + "\n",
            encoding="utf-8",
        )
        sys.exit(0)
    report = evaluate_policy(repo_root, policy_path, policy)
    out_path.write_text(json.dumps(report, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    sys.exit(0 if report["status"] == "passed" else 1)
except GateFailure as exc:
    report = {
        "checked_at": now_iso(),
        "status": "failed",
        "error_code": exc.code,
        "message": exc.message,
        "detail": exc.detail,
        "policy_path": policy_path.as_posix(),
        "repo_root": repo_root.as_posix(),
    }
    out_path.write_text(json.dumps(report, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    sys.exit(1)
except Exception as exc:  # pragma: no cover
    report = {
        "checked_at": now_iso(),
        "status": "failed",
        "error_code": "internal_error",
        "message": str(exc),
        "policy_path": policy_path.as_posix(),
        "repo_root": repo_root.as_posix(),
    }
    out_path.write_text(json.dumps(report, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    sys.exit(2)
PY
