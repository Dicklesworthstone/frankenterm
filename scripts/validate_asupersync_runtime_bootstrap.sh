#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
MAIN_PATH="${ROOT_DIR}/crates/frankenterm/src/main.rs"
OUT_PATH="${ROOT_DIR}/docs/asupersync-runtime-bootstrap-validation.json"
SELF_TEST=0

usage() {
  cat <<'USAGE'
Usage: validate_asupersync_runtime_bootstrap.sh [options]

Options:
  --main-path <path>    Override main.rs path
  --output <path>       Output report JSON path
  --self-test           Run validator self-tests before validation
  -h, --help            Show this help
USAGE
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --main-path)
      MAIN_PATH="$2"
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

python3 - "${MAIN_PATH}" "${OUT_PATH}" "${SELF_TEST}" <<'PY'
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
    enum RuntimeProcessRole { Cli, Watch, Web, Robot }
    fn runtime_bootstrap_spec_for_role(role: RuntimeProcessRole) -> RuntimeBootstrapSpec { role }
    fn build_process_runtime(spec: RuntimeBootstrapSpec, worker_threads: Option<usize>) {}
    fn parse_runtime_worker_threads(value: Option<&str>) -> Result<Option<usize>, String> { Ok(None) }
    fn sniff_runtime_process_role_from_args() -> RuntimeProcessRole { RuntimeProcessRole::Cli }
    fn sniff_primary_subcommand_from_iter<I, S>(args: I) -> Option<String> where I: IntoIterator<Item=S>, S: AsRef<str> { None }
    fn emit_runtime_bootstrap_lifecycle(spec: RuntimeBootstrapSpec, phase: &'static str, outcome: &'static str, error_code: Option<&str>) {}
    fn main() {
      let _ = RuntimeProcessRole::Watch;
      let _ = RuntimeProcessRole::Web;
      let _ = RuntimeProcessRole::Robot;
      let runtime_role = sniff_runtime_process_role_from_args();
      let runtime_spec = runtime_bootstrap_spec_for_role(runtime_role);
      let runtime_worker_threads = parse_runtime_worker_threads(None).unwrap();
      let rt = match build_process_runtime(runtime_spec, runtime_worker_threads) { _ => return };
      emit_runtime_bootstrap_lifecycle(runtime_spec, "startup", "runtime_initialized", None);
      emit_runtime_bootstrap_lifecycle(runtime_spec, "shutdown", "run_completed", None);
    }
    """
    required = [
        "enum RuntimeProcessRole",
        "RuntimeProcessRole::Cli",
        "RuntimeProcessRole::Watch",
        "RuntimeProcessRole::Web",
        "RuntimeProcessRole::Robot",
        "fn runtime_bootstrap_spec_for_role(",
        "fn build_process_runtime(",
        "fn parse_runtime_worker_threads(",
        "fn sniff_runtime_process_role_from_args(",
        "fn sniff_primary_subcommand_from_iter<",
        "fn emit_runtime_bootstrap_lifecycle(",
    ]
    for token in required:
        if token not in sample:
            raise AssertionError(f"missing self-test token: {token}")


def write_report(out_path: Path, payload: dict) -> None:
    out_path.write_text(json.dumps(payload, indent=2, sort_keys=True) + "\n", encoding="utf-8")


main_path = Path(sys.argv[1]).resolve()
out_path = Path(sys.argv[2]).resolve()
self_test = bool(int(sys.argv[3]))

try:
    if self_test:
        run_self_tests()

    require(main_path.exists(), "main_missing", f"main file not found: {main_path}")
    text = main_path.read_text(encoding="utf-8", errors="ignore")

    checks: list[dict] = []

    required_tokens = [
        "enum RuntimeProcessRole",
        "RuntimeProcessRole::Cli",
        "RuntimeProcessRole::Watch",
        "RuntimeProcessRole::Web",
        "RuntimeProcessRole::Robot",
        "fn runtime_bootstrap_spec_for_role(",
        "fn build_process_runtime(",
        "fn parse_runtime_worker_threads(",
        "fn sniff_runtime_process_role_from_args(",
        "fn sniff_primary_subcommand_from_iter<",
        "fn emit_runtime_bootstrap_lifecycle(",
        "let runtime_role = sniff_runtime_process_role_from_args();",
        "let runtime_spec = runtime_bootstrap_spec_for_role(runtime_role);",
        "let rt = match build_process_runtime(runtime_spec, runtime_worker_threads)",
        "emit_runtime_bootstrap_lifecycle(runtime_spec, \"startup\", \"runtime_initialized\", None);",
        "emit_runtime_bootstrap_lifecycle(runtime_spec, \"shutdown\", \"run_completed\", None);",
    ]
    for token in required_tokens:
        require(token in text, "missing_required_token", f"Required token missing: {token}")
    checks.append({"name": "required_tokens", "status": "passed", "count": len(required_tokens)})

    # Ensure unified runtime bootstrap goes through helper in main(), not direct builder construction.
    if "fn main()" in text and "async fn run(" in text:
        main_segment = text.split("fn main()", 1)[1].split("async fn run(", 1)[0]
    else:
        fail("main_segment_missing", "Unable to isolate fn main() segment")

    require(
        "build_process_runtime(runtime_spec, runtime_worker_threads)" in main_segment,
        "main_not_using_unified_builder",
        "fn main() must instantiate runtime through build_process_runtime()",
    )
    require(
        "RuntimeBuilder::multi_thread()" not in main_segment,
        "ambient_runtime_builder_in_main",
        "fn main() must not call RuntimeBuilder::multi_thread() directly",
    )
    checks.append({"name": "main_runtime_bootstrap_contract", "status": "passed"})

    # Validate lifecycle reason-code mapping is present for all process roles.
    reason_code_tokens = [
        "runtime.bootstrap.cli.startup",
        "runtime.bootstrap.cli.shutdown",
        "runtime.bootstrap.watch.startup",
        "runtime.bootstrap.watch.shutdown",
        "runtime.bootstrap.web.startup",
        "runtime.bootstrap.web.shutdown",
        "runtime.bootstrap.robot.startup",
        "runtime.bootstrap.robot.shutdown",
    ]
    for token in reason_code_tokens:
        require(token in text, "missing_reason_code", f"Missing lifecycle reason code: {token}")
    checks.append({"name": "lifecycle_reason_codes", "status": "passed", "count": len(reason_code_tokens)})

    # Validate unit tests cover role detection, parsing, and mapping contract.
    test_tokens = [
        "fn runtime_bootstrap_subcommand_detection_covers_modes()",
        "fn runtime_bootstrap_worker_thread_parser_handles_errors()",
        "fn runtime_bootstrap_spec_maps_modes_to_thread_names()",
    ]
    for token in test_tokens:
        require(token in text, "missing_runtime_bootstrap_test", f"Missing runtime bootstrap unit test: {token}")
    checks.append({"name": "unit_test_contract", "status": "passed", "count": len(test_tokens)})

    report = {
        "status": "passed",
        "checked_at": now_iso(),
        "main_path": str(main_path),
        "checks": checks,
    }
    write_report(out_path, report)
except ValidationFailure as exc:
    failure = {
        "status": "failed",
        "checked_at": now_iso(),
        "error_code": exc.code,
        "error_message": exc.message,
        "main_path": str(main_path),
    }
    write_report(out_path, failure)
    print(f"[runtime-bootstrap-validator] {exc.code}: {exc.message}", file=sys.stderr)
    sys.exit(1)
except Exception as exc:  # pragma: no cover
    failure = {
        "status": "failed",
        "checked_at": now_iso(),
        "error_code": "validator_internal_error",
        "error_message": str(exc),
        "main_path": str(main_path),
    }
    write_report(out_path, failure)
    print(f"[runtime-bootstrap-validator] validator_internal_error: {exc}", file=sys.stderr)
    sys.exit(1)
PY
