#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
CONTRACT_PATH="${ROOT_DIR}/docs/asupersync-runtime-doctrine-v1.json"
DOCTRINE_DOC_PATH="${ROOT_DIR}/docs/asupersync-architecture-doctrine.md"
OUT_PATH="${ROOT_DIR}/docs/asupersync-runtime-doctrine-validation.json"
SELF_TEST=0

usage() {
  cat <<'USAGE'
Usage: validate_asupersync_doctrine_pack.sh [options]

Options:
  --contract-path <path>   Override doctrine contract JSON path
  --doctrine-doc <path>    Override doctrine markdown path
  --output <path>          Output report path (JSON)
  --self-test              Run validator self-tests before validation
  -h, --help               Show this help
USAGE
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --contract-path)
      CONTRACT_PATH="$2"
      shift 2
      ;;
    --doctrine-doc)
      DOCTRINE_DOC_PATH="$2"
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

python3 - "${ROOT_DIR}" "${CONTRACT_PATH}" "${DOCTRINE_DOC_PATH}" "${OUT_PATH}" "${SELF_TEST}" <<'PY'
from __future__ import annotations

import json
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


def has_required_semantic_rows(rows: list[dict]) -> bool:
    keys = {row.get("semantic_key") for row in rows if isinstance(row, dict)}
    return {"spawn", "select", "timeout", "channel", "shutdown"}.issubset(keys)


def check_required_doc_sections(doc_text: str) -> list[str]:
    required_sections = [
        "## 1. Doctrine Principles",
        "### 1.2 Cx Everywhere",
        "### 1.7 Anti-Patterns (Reject in Review)",
        "### 1.8 User-Facing Guarantees",
        "### 2.4 Legacy-to-Doctrine Semantic Mapping",
    ]
    missing = [section for section in required_sections if section not in doc_text]
    return missing


def run_self_tests() -> None:
    good_rows = [
        {"semantic_key": "spawn"},
        {"semantic_key": "select"},
        {"semantic_key": "timeout"},
        {"semantic_key": "channel"},
        {"semantic_key": "shutdown"},
    ]
    bad_rows = [{"semantic_key": "spawn"}, {"semantic_key": "timeout"}]
    assert has_required_semantic_rows(good_rows)
    assert not has_required_semantic_rows(bad_rows)

    doc_blob = "\n".join(
        [
            "## 1. Doctrine Principles",
            "### 1.2 Cx Everywhere",
            "### 1.7 Anti-Patterns (Reject in Review)",
            "### 1.8 User-Facing Guarantees",
            "### 2.4 Legacy-to-Doctrine Semantic Mapping",
        ]
    )
    assert check_required_doc_sections(doc_blob) == []
    assert "### 1.2 Cx Everywhere" in doc_blob


def write_report(out_path: Path, payload: dict) -> None:
    out_path.write_text(json.dumps(payload, indent=2, sort_keys=True) + "\n", encoding="utf-8")


root = Path(sys.argv[1]).resolve()
contract_path = Path(sys.argv[2]).resolve()
doctrine_doc_path = Path(sys.argv[3]).resolve()
out_path = Path(sys.argv[4]).resolve()
self_test = bool(int(sys.argv[5]))

try:
    if self_test:
        run_self_tests()

    require(contract_path.exists(), "contract_missing", f"Contract file not found: {contract_path}")
    require(doctrine_doc_path.exists(), "doctrine_doc_missing", f"Doctrine markdown not found: {doctrine_doc_path}")

    contract = json.loads(contract_path.read_text(encoding="utf-8"))
    doctrine_doc_text = doctrine_doc_path.read_text(encoding="utf-8")

    checks: list[dict] = []

    contract_id = contract.get("contract_id")
    contract_version = contract.get("version")
    require(contract_id == "ft.runtime.doctrine.v1", "invalid_contract_id", "Expected contract_id ft.runtime.doctrine.v1")
    require(contract_version == "1.0.0", "invalid_contract_version", "Expected contract version 1.0.0")
    checks.append({"name": "contract_identity", "status": "passed"})

    invariants = contract.get("invariants", [])
    anti_patterns = contract.get("anti_patterns", [])
    mappings = contract.get("legacy_to_target_mapping", [])
    guarantees = contract.get("user_facing_guarantees", [])
    modules = contract.get("representative_modules", [])

    require(isinstance(invariants, list) and len(invariants) >= 5, "missing_invariants", "Expected at least 5 invariants")
    require(all(isinstance(item, dict) and item.get("id") and item.get("title") and item.get("invariant") for item in invariants),
            "invalid_invariant_shape", "Invariant rows must include id/title/invariant")
    checks.append({"name": "invariant_pack", "status": "passed", "count": len(invariants)})

    require(isinstance(anti_patterns, list) and len(anti_patterns) >= 5, "missing_anti_patterns", "Expected at least 5 anti-patterns")
    checks.append({"name": "anti_pattern_pack", "status": "passed", "count": len(anti_patterns)})

    require(isinstance(mappings, list) and has_required_semantic_rows(mappings),
            "missing_semantic_mapping_rows",
            "Expected semantic mapping rows for spawn/select/timeout/channel/shutdown")
    checks.append({"name": "semantic_mapping_pack", "status": "passed", "count": len(mappings)})

    guarantee_ids = {item.get("id") for item in guarantees if isinstance(item, dict)}
    required_guarantees = {
        "no_silent_command_event_loss",
        "deterministic_shutdown_messaging",
        "actionable_error_contract",
    }
    missing_guarantees = sorted(required_guarantees - guarantee_ids)
    require(not missing_guarantees, "missing_user_guarantee", f"Missing user guarantees: {', '.join(missing_guarantees)}")
    checks.append({"name": "user_guarantees", "status": "passed", "count": len(guarantees)})

    missing_sections = check_required_doc_sections(doctrine_doc_text)
    require(not missing_sections, "missing_required_section", f"Doctrine markdown missing sections: {', '.join(missing_sections)}")
    require(contract_id in doctrine_doc_text, "contract_id_not_referenced", "Doctrine markdown must reference contract_id")
    checks.append({"name": "doctrine_doc_sections", "status": "passed"})

    module_results = []
    for module in modules:
        module_path = root / module["path"]
        require(module_path.exists(), "module_missing", f"Representative module missing: {module_path}")
        text = module_path.read_text(encoding="utf-8", errors="ignore")

        must_contain_any = module.get("must_contain_any", [])
        must_not_contain = module.get("must_not_contain", [])

        if must_contain_any:
            require(any(token in text for token in must_contain_any),
                    "module_required_token_missing",
                    f"None of required tokens found in {module['path']}: {must_contain_any}")
        for token in must_not_contain:
            require(token not in text, "module_forbidden_token_found", f"Forbidden token '{token}' found in {module['path']}")

        module_results.append({"path": module["path"], "status": "passed"})

    checks.append({"name": "representative_module_integration", "status": "passed", "count": len(module_results)})

    report = {
        "status": "passed",
        "checked_at": now_iso(),
        "contract_path": contract_path.relative_to(root).as_posix() if contract_path.is_relative_to(root) else str(contract_path),
        "doctrine_doc_path": doctrine_doc_path.relative_to(root).as_posix() if doctrine_doc_path.is_relative_to(root) else str(doctrine_doc_path),
        "checks": checks,
        "integration_summary": {
            "module_checks_passed": len(module_results),
            "module_results": module_results,
        },
    }
    write_report(out_path, report)
except ValidationFailure as exc:
    failure_report = {
        "status": "failed",
        "checked_at": now_iso(),
        "error_code": exc.code,
        "error_message": exc.message,
        "contract_path": str(contract_path),
        "doctrine_doc_path": str(doctrine_doc_path),
    }
    write_report(out_path, failure_report)
    print(f"[doctrine-validator] {exc.code}: {exc.message}", file=sys.stderr)
    sys.exit(1)
except Exception as exc:  # pragma: no cover - defensive fallback
    failure_report = {
        "status": "failed",
        "checked_at": now_iso(),
        "error_code": "validator_internal_error",
        "error_message": str(exc),
        "contract_path": str(contract_path),
        "doctrine_doc_path": str(doctrine_doc_path),
    }
    write_report(out_path, failure_report)
    print(f"[doctrine-validator] validator_internal_error: {exc}", file=sys.stderr)
    sys.exit(1)
PY
