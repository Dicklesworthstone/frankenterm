#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
WORKFLOWS_PATH="${ROOT_DIR}/crates/frankenterm-core/src/workflows.rs"
OUT_PATH="${ROOT_DIR}/docs/asupersync-cx-outcome-validation.json"
SELF_TEST=0

usage() {
  cat <<'USAGE'
Usage: validate_asupersync_cx_outcome_contract.sh [options]

Options:
  --workflows-path <path>   Override workflows.rs path
  --output <path>           Output report JSON path
  --self-test               Run validator self-tests before validation
  -h, --help                Show this help
USAGE
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --workflows-path)
      WORKFLOWS_PATH="$2"
      shift 2
      ;;
    --output)
      OUT_PATH="$2"
      shift 2
      ;;
    --self-test)
      SELF_TEST=1
      shift
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

python3 - "${WORKFLOWS_PATH}" "${OUT_PATH}" "${SELF_TEST}" <<'PY'
from __future__ import annotations

import json
import re
import sys
from dataclasses import dataclass
from datetime import datetime, timezone
from pathlib import Path


@dataclass
class ValidationFailure(Exception):
    code: str
    message: str


def now_iso() -> str:
    return datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")


def fail(code: str, message: str) -> None:
    raise ValidationFailure(code=code, message=message)


def require(condition: bool, code: str, message: str) -> None:
    if not condition:
        fail(code, message)


def run_self_tests() -> None:
    sample = """
    pub struct WorkflowContext { cx: crate::cx::Cx, execution_id: String }
    impl WorkflowContext {
      pub fn with_cx(mut self, cx: crate::cx::Cx) -> Self { self.cx = cx; self }
      pub fn cx(&self) -> &crate::cx::Cx { &self.cx }
    }
    async fn execute_wait_condition_with_cx_outcome(
      cx: &crate::cx::Cx, condition: &WaitCondition, timeout: Duration
    ) -> crate::outcome::FtOutcome<()> { crate::outcome::result_to_outcome(Ok(())) }
    fn wait_failure_reason_code(condition: &WaitCondition, failure_kind: &str) -> String { "x".to_string() }
    fn wait_failure_to_abort_reason(condition: &WaitCondition, outcome: crate::outcome::FtOutcome<()>) -> Option<String> { None }
    fn run() {
      let _ = execute_wait_condition_with_cx_outcome(ctx.cx(), &condition, timeout).await;
      record_workflow_terminal_action(&storage, "wf", "exec", 1, "workflow_wait_failed", "error", None, Some(0), None, None);
      record_workflow_terminal_action(&storage, "wf", "exec", 1, "workflow_wait_failed_after_send", "error", None, Some(0), None, None);
    }
    """
    required = [
        "cx: crate::cx::Cx",
        "pub fn with_cx(mut self, cx: crate::cx::Cx) -> Self",
        "pub fn cx(&self) -> &crate::cx::Cx",
        "execute_wait_condition_with_cx_outcome(",
        "wait_failure_reason_code(",
        "wait_failure_to_abort_reason(",
        "workflow_wait_failed",
        "workflow_wait_failed_after_send",
    ]
    for token in required:
        if token not in sample:
            raise AssertionError(f"missing self-test token: {token}")


def write_report(out_path: Path, payload: dict) -> None:
    out_path.write_text(json.dumps(payload, indent=2, sort_keys=True) + "\n", encoding="utf-8")


workflows_path = Path(sys.argv[1]).resolve()
out_path = Path(sys.argv[2]).resolve()
self_test = bool(int(sys.argv[3]))

try:
    if self_test:
        run_self_tests()

    require(
        workflows_path.exists(),
        "workflows_missing",
        f"workflows file not found: {workflows_path}",
    )
    text = workflows_path.read_text(encoding="utf-8", errors="ignore")

    checks: list[dict] = []

    required_tokens = [
        "cx: crate::cx::Cx,",
        "pub fn with_cx(mut self, cx: crate::cx::Cx) -> Self",
        "pub fn cx(&self) -> &crate::cx::Cx",
        "async fn execute_wait_condition_with_cx_outcome(",
        "fn wait_failure_reason_code(",
        "fn wait_failure_to_abort_reason(",
        "execute_wait_condition_with_cx_outcome(ctx.cx(), &condition, timeout).await;",
        "wait_failure_to_abort_reason(&condition, wait_outcome)",
        "\"workflow_wait_failed\"",
        "\"workflow_wait_failed_after_send\"",
    ]
    for token in required_tokens:
        require(
            token in text,
            "missing_required_token",
            f"Required token missing: {token}",
        )
    checks.append(
        {
            "name": "required_tokens",
            "status": "passed",
            "count": len(required_tokens),
        }
    )

    wait_adapter_occurrences = len(
        re.findall(r"\bexecute_wait_condition_with_cx_outcome\s*\(", text)
    )
    require(
        wait_adapter_occurrences >= 3,
        "wait_adapter_not_propagated",
        "Expected execute_wait_condition_with_cx_outcome definition + at least 2 call sites",
    )
    checks.append(
        {
            "name": "wait_adapter_propagation",
            "status": "passed",
            "occurrences": wait_adapter_occurrences,
        }
    )

    # Ensure legacy inline wait branches are no longer used in run_workflow path.
    run_workflow_match = re.search(
        r"pub async fn run_workflow\([^\)]*\)\s*->\s*WorkflowExecutionResult\s*\{(?P<body>.*?)\n\s*\}\n\n    /// Run the event loop",
        text,
        flags=re.DOTALL,
    )
    require(
        run_workflow_match is not None,
        "run_workflow_not_found",
        "Unable to isolate run_workflow body",
    )
    run_body = run_workflow_match.group("body")
    require(
        run_body.count("execute_wait_condition_with_cx_outcome(") >= 2,
        "run_workflow_wait_adapter_missing",
        "run_workflow must route WaitFor and SendText wait paths through execute_wait_condition_with_cx_outcome()",
    )
    checks.append(
        {
            "name": "run_workflow_wait_adapter_usage",
            "status": "passed",
        }
    )

    test_tokens = [
        "fn wait_condition_kind_covers_all_variants()",
        "fn wait_failure_reason_code_formats_stably()",
        "fn wait_failure_to_abort_reason_maps_err()",
        "fn wait_failure_to_abort_reason_maps_panicked()",
        "async fn execute_wait_condition_with_cx_outcome_sleep_path_ok()",
    ]
    for token in test_tokens:
        require(
            token in text,
            "missing_unit_test",
            f"Missing unit test token: {token}",
        )
    checks.append(
        {
            "name": "unit_test_contract",
            "status": "passed",
            "count": len(test_tokens),
        }
    )

    report = {
        "status": "passed",
        "checked_at": now_iso(),
        "workflows_path": str(workflows_path),
        "checks": checks,
    }
    write_report(out_path, report)
except ValidationFailure as exc:
    failure = {
        "status": "failed",
        "checked_at": now_iso(),
        "error_code": exc.code,
        "error_message": exc.message,
        "workflows_path": str(workflows_path),
    }
    write_report(out_path, failure)
    print(f"[cx-outcome-validator] {exc.code}: {exc.message}", file=sys.stderr)
    sys.exit(1)
except Exception as exc:  # pragma: no cover
    failure = {
        "status": "failed",
        "checked_at": now_iso(),
        "error_code": "validator_internal_error",
        "error_message": str(exc),
        "workflows_path": str(workflows_path),
    }
    write_report(out_path, failure)
    print(f"[cx-outcome-validator] validator_internal_error: {exc}", file=sys.stderr)
    sys.exit(1)
PY
