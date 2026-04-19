#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
POLICY_PATH="${ROOT_DIR}/docs/ft-xbnl0-4-6-release-gates.json"
EVIDENCE_ROOT=""
OUT_PATH="${ROOT_DIR}/docs/ft-xbnl0-4-6-release-gates-validation.json"

usage() {
  cat <<'USAGE'
Usage: validate_runtime_release_gates.sh [options]

Options:
  --root <path>          Override repository root
  --policy-path <path>   Override release-gate policy JSON path
  --evidence-root <path> Override evidence root (defaults to policy.evidence_root)
  --output <path>        Output report JSON path
  -h, --help             Show this help
USAGE
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --root)
      ROOT_DIR="$2"
      shift 2
      ;;
    --policy-path)
      POLICY_PATH="$2"
      shift 2
      ;;
    --evidence-root)
      EVIDENCE_ROOT="$2"
      shift 2
      ;;
    --output)
      OUT_PATH="$2"
      shift 2
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      echo "Unknown option: $1" >&2
      usage >&2
      exit 2
      ;;
  esac
done

mkdir -p "$(dirname "${OUT_PATH}")"

python3 - "${ROOT_DIR}" "${POLICY_PATH}" "${EVIDENCE_ROOT}" "${OUT_PATH}" <<'PY'
from __future__ import annotations

import json
import sys
from dataclasses import dataclass
from datetime import datetime, timezone
from pathlib import Path
from typing import Any


@dataclass
class ValidationFailure(Exception):
    code: str
    message: str
    detail: dict[str, Any] | None = None


def now_iso() -> str:
    return datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")


def fail(code: str, message: str, detail: dict[str, Any] | None = None) -> None:
    raise ValidationFailure(code=code, message=message, detail=detail)


def require(condition: bool, code: str, message: str, detail: dict[str, Any] | None = None) -> None:
    if not condition:
        fail(code, message, detail)


def load_json(path: Path) -> dict[str, Any]:
    try:
        return json.loads(path.read_text(encoding="utf-8"))
    except FileNotFoundError:
        fail("json_missing", f"missing JSON file: {path}", {"path": str(path)})
    except json.JSONDecodeError as exc:
        fail(
            "json_invalid",
            f"invalid JSON in {path}: {exc}",
            {"path": str(path), "lineno": exc.lineno, "colno": exc.colno},
        )


def validate_policy_shape(policy: dict[str, Any]) -> None:
    require(
        policy.get("contract_id") == "ft.xbnl0.4.6.release_gates.v1",
        "invalid_contract_id",
        "unexpected release gate contract_id",
    )
    require(policy.get("version") == "1.0.0", "invalid_contract_version", "unexpected release gate version")
    require(policy.get("bead_id") == "ft-xbnl0.4.6", "invalid_bead_id", "unexpected release gate bead_id")
    gates = policy.get("gates")
    require(isinstance(gates, list) and gates, "invalid_gates", "policy.gates must be a non-empty array")
    for gate in gates:
        require(isinstance(gate, dict), "invalid_gate_entry", "each gate entry must be an object")
        for key in ("gate_id", "title", "category", "kind", "upstream_bead", "scenario_id", "action"):
            require(isinstance(gate.get(key), str) and gate.get(key), "missing_gate_field", f"gate missing required field: {key}", {"gate": gate.get("gate_id")})


def latest_run_dir(base: Path) -> Path:
    require(base.exists(), "evidence_dir_missing", f"evidence directory missing: {base}", {"path": str(base)})
    runs = sorted([path for path in base.iterdir() if path.is_dir()])
    require(runs, "evidence_run_missing", f"no run directories found under {base}", {"path": str(base)})
    return runs[-1]


def load_structured_steps(path: Path) -> dict[str, dict[str, Any]]:
    require(path.exists(), "structured_log_missing", f"structured log missing: {path}", {"path": str(path)})
    steps: dict[str, dict[str, Any]] = {}
    for line_no, line in enumerate(path.read_text(encoding="utf-8").splitlines(), start=1):
        if not line.strip():
            continue
        try:
            entry = json.loads(line)
        except json.JSONDecodeError as exc:
            fail(
                "structured_log_invalid_json",
                f"invalid structured log JSON at {path}:{line_no}",
                {"path": str(path), "lineno": line_no, "colno": exc.colno},
            )
        step = entry.get("step")
        if isinstance(step, str) and step:
            steps[step] = entry
    return steps


def base_gate_result(gate: dict[str, Any], evidence_path: Path) -> dict[str, Any]:
    return {
        "gate_id": gate["gate_id"],
        "title": gate["title"],
        "category": gate["category"],
        "upstream_bead": gate["upstream_bead"],
        "scenario_id": gate["scenario_id"],
        "evidence_path": str(evidence_path),
        "action": gate["action"],
        "checks": [],
    }


def add_check(result: dict[str, Any], name: str, passed: bool, reason_code: str, detail: dict[str, Any]) -> None:
    result["checks"].append(
        {
            "name": name,
            "status": "passed" if passed else "failed",
            "reason_code": reason_code,
            "detail": detail,
        }
    )


def finalize_gate(result: dict[str, Any]) -> dict[str, Any]:
    failed = [check for check in result["checks"] if check["status"] != "passed"]
    if failed:
        result["status"] = "failed"
        result["reason_code"] = failed[0]["reason_code"]
    else:
        result["status"] = "passed"
        result["reason_code"] = "gate_passed"
    return result


def evaluate_structured_harness(evidence_root: Path, gate: dict[str, Any]) -> dict[str, Any]:
    run_dir = latest_run_dir(evidence_root / gate["upstream_bead"] / gate["scenario_id"])
    result = base_gate_result(gate, run_dir)
    summary_path = run_dir / "summary.json"
    structured_path = run_dir / "structured.log"

    summary = None
    if summary_path.exists():
        summary = load_json(summary_path)
        expected_status = gate.get("required_summary_status", "passed")
        passed = summary.get("status") == expected_status
        add_check(
            result,
            "summary_status",
            passed,
            "summary_status_mismatch",
            {
                "expected_status": expected_status,
                "observed_status": summary.get("status"),
                "summary_path": str(summary_path),
            },
        )
    else:
        add_check(
            result,
            "summary_status",
            False,
            "summary_missing",
            {"summary_path": str(summary_path)},
        )

    steps = load_structured_steps(structured_path)
    required_steps = gate.get("required_steps", [])
    require(isinstance(required_steps, list), "invalid_required_steps", "required_steps must be a list", {"gate": gate["gate_id"]})
    for step_name in required_steps:
        entry = steps.get(step_name)
        if entry is None:
            add_check(
                result,
                f"step::{step_name}",
                False,
                "required_step_missing",
                {"required_step": step_name, "structured_log": str(structured_path)},
            )
            continue
        add_check(
            result,
            f"step::{step_name}",
            entry.get("status") == "passed",
            "required_step_failed",
            {
                "required_step": step_name,
                "observed_status": entry.get("status"),
                "message": entry.get("message"),
            },
        )

    if summary is None and gate.get("allow_structured_log_fallback", False):
        result["checks"] = [check for check in result["checks"] if check["name"] != "summary_status"]
    return finalize_gate(result)


def require_exact_list(name: str, observed: Any, expected: list[Any], result: dict[str, Any], reason_code: str) -> None:
    add_check(
        result,
        name,
        observed == expected,
        reason_code,
        {"expected": expected, "observed": observed},
    )


def tier_rank(tier: str | None) -> int:
    order = {"Green": 0, "Yellow": 1, "Red": 2, "Black": 3}
    return order.get(tier or "", 99)


def load_cycle_summary(path_str: str) -> dict[str, Any]:
    return load_json(Path(path_str))


def evaluate_swarm_matrix(evidence_root: Path, gate: dict[str, Any]) -> tuple[Path, dict[str, Any], list[dict[str, Any]]]:
    run_dir = latest_run_dir(evidence_root / gate["upstream_bead"] / gate["scenario_id"])
    summary = load_json(run_dir / "summary.json")
    smoke_path = summary.get("profiles", {}).get("smoke", {}).get("summary")
    release_paths = summary.get("profiles", {}).get("release", {}).get("summaries", [])
    smoke_summary = load_cycle_summary(smoke_path)
    release_summaries = [load_cycle_summary(path) for path in release_paths]
    return run_dir, summary, [smoke_summary, *release_summaries]


def evaluate_swarm_soak_confidence(evidence_root: Path, gate: dict[str, Any]) -> dict[str, Any]:
    run_dir, summary, cycle_summaries = evaluate_swarm_matrix(evidence_root, gate)
    result = base_gate_result(gate, run_dir)

    add_check(
        result,
        "suite_status",
        summary.get("status") == "passed",
        "suite_status_failed",
        {"observed_status": summary.get("status"), "summary_path": str(run_dir / "summary.json")},
    )
    required_cycles = gate.get("required_cycles", {})
    add_check(
        result,
        "smoke_cycle_count",
        summary.get("profiles", {}).get("smoke", {}).get("cycles") == required_cycles.get("smoke"),
        "smoke_cycle_count_mismatch",
        {
            "expected": required_cycles.get("smoke"),
            "observed": summary.get("profiles", {}).get("smoke", {}).get("cycles"),
        },
    )
    add_check(
        result,
        "release_cycle_count",
        summary.get("profiles", {}).get("release", {}).get("cycles") == required_cycles.get("release"),
        "release_cycle_count_mismatch",
        {
            "expected": required_cycles.get("release"),
            "observed": summary.get("profiles", {}).get("release", {}).get("cycles"),
        },
    )

    expected_tests_run = gate.get("required_tests_run")
    expected_pane_scales = gate.get("required_pane_scales", [])
    expected_metric_names = gate.get("required_metric_names", [])

    for index, cycle_summary in enumerate(cycle_summaries):
        cycle_label = cycle_summary.get("profile", f"cycle_{index}")
        add_check(
            result,
            f"{cycle_label}::tests_run",
            cycle_summary.get("tests_run") == expected_tests_run,
            "tests_run_mismatch",
            {"expected": expected_tests_run, "observed": cycle_summary.get("tests_run")},
        )
        require_exact_list(
            f"{cycle_label}::pane_scales",
            cycle_summary.get("pane_scales"),
            expected_pane_scales,
            result,
            "pane_scales_mismatch",
        )
        require_exact_list(
            f"{cycle_label}::metric_names",
            cycle_summary.get("metric_names"),
            expected_metric_names,
            result,
            "metric_names_mismatch",
        )

    release_signatures = [
        {
            "tests_run": cycle.get("tests_run"),
            "pane_scales": cycle.get("pane_scales"),
            "metric_names": cycle.get("metric_names"),
        }
        for cycle in cycle_summaries[1:]
    ]
    add_check(
        result,
        "release_cycle_consistency",
        len({json.dumps(signature, sort_keys=True) for signature in release_signatures}) == 1,
        "release_cycle_drift",
        {"release_signatures": release_signatures},
    )

    return finalize_gate(result)


def evaluate_runtime_performance_budget(evidence_root: Path, gate: dict[str, Any]) -> dict[str, Any]:
    run_dir, summary, cycle_summaries = evaluate_swarm_matrix(evidence_root, gate)
    result = base_gate_result(gate, run_dir)

    max_peak_rss_mb = float(gate.get("max_peak_rss_mb"))
    max_duration_s = float(gate.get("max_duration_s"))
    max_release_cycle_peak_rss_mb = float(gate.get("max_release_cycle_peak_rss_mb"))
    max_release_cycle_duration_s = float(gate.get("max_release_cycle_duration_s"))

    smoke_summary = cycle_summaries[0]
    release_summaries = cycle_summaries[1:]

    add_check(
        result,
        "smoke_peak_rss_budget",
        float(smoke_summary.get("peak_rss_mb", 0.0)) <= max_peak_rss_mb,
        "peak_rss_budget_exceeded",
        {
            "threshold": max_peak_rss_mb,
            "observed": smoke_summary.get("peak_rss_mb"),
            "summary_path": summary.get("profiles", {}).get("smoke", {}).get("summary"),
        },
    )
    add_check(
        result,
        "smoke_duration_budget",
        float(smoke_summary.get("max_duration_s", 0.0)) <= max_duration_s,
        "duration_budget_exceeded",
        {
            "threshold": max_duration_s,
            "observed": smoke_summary.get("max_duration_s"),
            "summary_path": summary.get("profiles", {}).get("smoke", {}).get("summary"),
        },
    )

    for index, release_summary in enumerate(release_summaries, start=1):
        path = summary.get("profiles", {}).get("release", {}).get("summaries", [None] * len(release_summaries))[index - 1]
        add_check(
            result,
            f"release_{index:02d}::peak_rss_budget",
            float(release_summary.get("peak_rss_mb", 0.0)) <= max_release_cycle_peak_rss_mb,
            "peak_rss_budget_exceeded",
            {
                "threshold": max_release_cycle_peak_rss_mb,
                "observed": release_summary.get("peak_rss_mb"),
                "summary_path": path,
            },
        )
        add_check(
            result,
            f"release_{index:02d}::duration_budget",
            float(release_summary.get("max_duration_s", 0.0)) <= max_release_cycle_duration_s,
            "duration_budget_exceeded",
            {
                "threshold": max_release_cycle_duration_s,
                "observed": release_summary.get("max_duration_s"),
                "summary_path": path,
            },
        )
        add_check(
            result,
            f"release_{index:02d}::backpressure_tier_known",
            tier_rank(release_summary.get("highest_backpressure_tier")) <= tier_rank("Black"),
            "unknown_backpressure_tier",
            {"observed": release_summary.get("highest_backpressure_tier"), "summary_path": path},
        )

    return finalize_gate(result)


def main() -> int:
    root = Path(sys.argv[1]).resolve()
    policy_path = Path(sys.argv[2]).resolve()
    evidence_root_override = sys.argv[3]
    out_path = Path(sys.argv[4]).resolve()

    try:
        policy = load_json(policy_path)
        validate_policy_shape(policy)
        evidence_root = Path(evidence_root_override).resolve() if evidence_root_override else (root / policy["evidence_root"]).resolve()
        results = []
        for gate in policy["gates"]:
            kind = gate["kind"]
            if kind == "structured_harness":
                results.append(evaluate_structured_harness(evidence_root, gate))
            elif kind == "swarm_soak_matrix":
                if gate["gate_id"] == "runtime_soak_confidence":
                    results.append(evaluate_swarm_soak_confidence(evidence_root, gate))
                elif gate["gate_id"] == "runtime_performance_budget":
                    results.append(evaluate_runtime_performance_budget(evidence_root, gate))
                else:
                    fail("unknown_swarm_gate", f"unsupported swarm_soak_matrix gate_id: {gate['gate_id']}")
            else:
                fail("unsupported_gate_kind", f"unsupported gate kind: {kind}", {"gate": gate["gate_id"]})

        report = {
            "contract_id": policy["contract_id"],
            "bead_id": policy["bead_id"],
            "checked_at": now_iso(),
            "policy_path": str(policy_path),
            "evidence_root": str(evidence_root),
            "status": "passed" if all(result["status"] == "passed" for result in results) else "failed",
            "gates": results,
        }
        out_path.parent.mkdir(parents=True, exist_ok=True)
        out_path.write_text(json.dumps(report, indent=2, sort_keys=True) + "\n", encoding="utf-8")
        return 0 if report["status"] == "passed" else 1
    except ValidationFailure as exc:
        report = {
            "contract_id": "ft.xbnl0.4.6.release_gates.v1",
            "bead_id": "ft-xbnl0.4.6",
            "checked_at": now_iso(),
            "policy_path": str(policy_path),
            "evidence_root": str(Path(evidence_root_override).resolve() if evidence_root_override else ""),
            "status": "failed",
            "error_code": exc.code,
            "message": exc.message,
            "detail": exc.detail,
            "gates": [],
        }
        out_path.parent.mkdir(parents=True, exist_ok=True)
        out_path.write_text(json.dumps(report, indent=2, sort_keys=True) + "\n", encoding="utf-8")
        return 2


raise SystemExit(main())
PY
