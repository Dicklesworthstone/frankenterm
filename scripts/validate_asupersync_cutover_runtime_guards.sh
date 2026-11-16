#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
POLICY_PATH="${ROOT_DIR}/docs/asupersync-cutover-runtime-guardrails.json"
OUT_PATH="${ROOT_DIR}/docs/asupersync-cutover-runtime-guard-validation.json"
SELF_TEST=0

usage() {
  cat <<'USAGE'
Usage: validate_asupersync_cutover_runtime_guards.sh [options]

Options:
  --root <path>          Override repository root
  --policy-path <path>   Override guardrail policy JSON path
  --output <path>        Output report JSON path
  --self-test            Run validator self-tests before validation
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

python3 - "${ROOT_DIR}" "${POLICY_PATH}" "${OUT_PATH}" "${SELF_TEST}" <<'PY'
from __future__ import annotations

import json
import sys
import tempfile
from dataclasses import dataclass
from datetime import datetime, timezone
from pathlib import Path
from typing import Any

try:
    import tomllib
except ModuleNotFoundError:  # pragma: no cover
    import tomli as tomllib  # type: ignore[no-redef]


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


def normalize_path(path: Path) -> str:
    return path.as_posix()


def load_toml(path: Path) -> dict[str, Any]:
    return tomllib.loads(path.read_text(encoding="utf-8"))


def workspace_manifest_paths(root: Path) -> list[Path]:
    workspace_manifest = root / "Cargo.toml"
    require(workspace_manifest.exists(), "workspace_manifest_missing", f"workspace Cargo.toml missing at {workspace_manifest}")
    workspace = load_toml(workspace_manifest)
    members = workspace.get("workspace", {}).get("members", [])
    require(isinstance(members, list), "invalid_workspace_members", "workspace.members must be a list")

    manifests = [Path("Cargo.toml")]
    for member in members:
        require(isinstance(member, str) and member, "invalid_workspace_member", "workspace member path must be a non-empty string")
        manifests.append(Path(member) / "Cargo.toml")

    for manifest in manifests:
        require((root / manifest).exists(), "workspace_member_manifest_missing", f"workspace member manifest missing: {manifest}")
    return manifests


def dependency_tables(doc: dict[str, Any]) -> list[dict[str, Any]]:
    tables: list[dict[str, Any]] = []
    for key in ("dependencies", "dev-dependencies", "build-dependencies"):
        table = doc.get(key)
        if isinstance(table, dict):
            tables.append(table)
    workspace_table = doc.get("workspace", {})
    if isinstance(workspace_table, dict):
        ws_deps = workspace_table.get("dependencies")
        if isinstance(ws_deps, dict):
            tables.append(ws_deps)
    target_table = doc.get("target")
    if isinstance(target_table, dict):
        for target_cfg in target_table.values():
            if not isinstance(target_cfg, dict):
                continue
            for key in ("dependencies", "dev-dependencies", "build-dependencies"):
                table = target_cfg.get(key)
                if isinstance(table, dict):
                    tables.append(table)
    return tables


def collect_dependency_hits(root: Path, manifests: list[Path], deps: list[str]) -> dict[str, list[str]]:
    hits: dict[str, set[str]] = {dep: set() for dep in deps}
    for rel_manifest in manifests:
        doc = load_toml(root / rel_manifest)
        rel_manifest_str = normalize_path(rel_manifest)
        for table in dependency_tables(doc):
            for dep in deps:
                if dep in table:
                    hits[dep].add(rel_manifest_str)
    return {dep: sorted(paths) for dep, paths in hits.items()}


def collect_token_counts(root: Path, source_roots: list[str], tokens: list[str]) -> tuple[dict[str, int], dict[str, list[str]]]:
    counts = {token: 0 for token in tokens}
    files_by_token: dict[str, set[str]] = {token: set() for token in tokens}

    for rel_root in source_roots:
        source_root = root / rel_root
        require(source_root.exists() and source_root.is_dir(), "invalid_source_root", f"source root does not exist: {rel_root}")
        for rs_path in source_root.rglob("*.rs"):
            text = rs_path.read_text(encoding="utf-8", errors="ignore")
            rel_rs = normalize_path(rs_path.relative_to(root))
            for token in tokens:
                occurrences = text.count(token)
                if occurrences > 0:
                    counts[token] += occurrences
                    files_by_token[token].add(rel_rs)

    return counts, {token: sorted(paths) for token, paths in files_by_token.items()}


def validate_policy_shape(policy: dict[str, Any]) -> None:
    require(policy.get("contract_id") == "ft.asupersync.cutover_runtime_guards.v1", "invalid_contract_id", "unexpected contract_id")
    require(policy.get("version") == "1.0.0", "invalid_contract_version", "unexpected version")
    require(policy.get("bead_id") == "ft-e34d9.10.8.2", "invalid_bead_id", "unexpected bead_id")

    for key in (
        "source_roots",
        "forbidden_dependencies",
        "dependency_allowlist",
        "forbidden_token_ceilings",
        "strict_zero_modules",
        "strict_zero_forbidden_tokens",
    ):
        require(key in policy, "policy_key_missing", f"policy missing required key: {key}")

    require(isinstance(policy["source_roots"], list) and policy["source_roots"], "invalid_source_roots", "source_roots must be a non-empty list")
    require(isinstance(policy["forbidden_dependencies"], list) and policy["forbidden_dependencies"], "invalid_forbidden_dependencies", "forbidden_dependencies must be a non-empty list")
    require(isinstance(policy["dependency_allowlist"], dict), "invalid_dependency_allowlist", "dependency_allowlist must be an object")
    require(isinstance(policy["forbidden_token_ceilings"], dict) and policy["forbidden_token_ceilings"], "invalid_forbidden_token_ceilings", "forbidden_token_ceilings must be a non-empty object")
    require(isinstance(policy["strict_zero_modules"], list), "invalid_strict_zero_modules", "strict_zero_modules must be a list")
    require(isinstance(policy["strict_zero_forbidden_tokens"], list), "invalid_strict_zero_tokens", "strict_zero_forbidden_tokens must be a list")


def evaluate_policy(root: Path, policy: dict[str, Any]) -> dict[str, Any]:
    validate_policy_shape(policy)
    checks: list[dict[str, Any]] = []

    dependencies = [str(dep) for dep in policy["forbidden_dependencies"]]
    dependency_allowlist = policy["dependency_allowlist"]
    token_ceilings = {str(k): int(v) for k, v in policy["forbidden_token_ceilings"].items()}
    source_roots = [str(entry) for entry in policy["source_roots"]]
    strict_modules = [str(entry) for entry in policy["strict_zero_modules"]]
    strict_tokens = [str(entry) for entry in policy["strict_zero_forbidden_tokens"]]

    checks.append({"name": "contract_identity", "status": "passed"})

    manifests = workspace_manifest_paths(root)
    dependency_hits = collect_dependency_hits(root, manifests, dependencies)

    for dependency in dependencies:
        allowlisted_paths = dependency_allowlist.get(dependency, [])
        require(isinstance(allowlisted_paths, list), "invalid_dependency_allowlist_entry", f"allowlist for {dependency} must be a list")
        allowlisted = {str(path) for path in allowlisted_paths}
        observed = set(dependency_hits.get(dependency, []))
        unexpected = sorted(observed - allowlisted)
        require(
            not unexpected,
            "unexpected_dependency_manifest",
            f"dependency '{dependency}' appears in non-allowlisted manifests",
            {"dependency": dependency, "unexpected_manifests": unexpected, "allowlist": sorted(allowlisted)},
        )

    checks.append(
        {
            "name": "dependency_manifest_allowlist",
            "status": "passed",
            "observed": dependency_hits,
        }
    )

    token_counts, token_files = collect_token_counts(root, source_roots, list(token_ceilings.keys()))
    ceiling_violations: list[dict[str, Any]] = []
    for token, ceiling in token_ceilings.items():
        actual = token_counts.get(token, 0)
        if actual > ceiling:
            ceiling_violations.append(
                {
                    "token": token,
                    "actual": actual,
                    "ceiling": ceiling,
                    "files": token_files.get(token, []),
                }
            )

    require(
        not ceiling_violations,
        "token_ceiling_exceeded",
        "forbidden runtime token count exceeded configured ceiling",
        {"violations": ceiling_violations},
    )

    checks.append(
        {
            "name": "forbidden_token_ceiling",
            "status": "passed",
            "counts": token_counts,
            "files": token_files,
        }
    )

    strict_violations: list[dict[str, Any]] = []
    for rel_module in strict_modules:
        module_path = root / rel_module
        require(module_path.exists(), "strict_module_missing", f"strict module missing: {rel_module}")
        text = module_path.read_text(encoding="utf-8", errors="ignore")
        token_hits = {token: text.count(token) for token in strict_tokens if token in text}
        if token_hits:
            strict_violations.append({"path": rel_module, "token_hits": token_hits})

    require(
        not strict_violations,
        "strict_module_forbidden_token",
        "strict zero modules contain forbidden runtime tokens",
        {"violations": strict_violations},
    )
    checks.append({"name": "strict_zero_modules", "status": "passed", "module_count": len(strict_modules)})

    return {
        "status": "passed",
        "checked_at": now_iso(),
        "policy_path": str(policy_path.relative_to(root)) if policy_path.is_relative_to(root) else str(policy_path),
        "repo_root": str(root),
        "checks": checks,
        "summary": {
            "dependency_manifest_hits": dependency_hits,
            "forbidden_token_counts": token_counts,
        },
    }


def run_self_tests() -> None:
    with tempfile.TemporaryDirectory(prefix="asupersync-cutover-guard-selftest-") as temp_dir:
        temp_root = Path(temp_dir)
        (temp_root / "crates" / "sample" / "src").mkdir(parents=True)
        (temp_root / "frankenterm").mkdir(parents=True)

        (temp_root / "Cargo.toml").write_text(
            """[workspace]
members = ["crates/sample"]

[workspace.dependencies]
tokio = "1"
""",
            encoding="utf-8",
        )
        (temp_root / "crates" / "sample" / "Cargo.toml").write_text(
            """[package]
name = "sample"
version = "0.1.0"
edition = "2024"
""",
            encoding="utf-8",
        )
        (temp_root / "crates" / "sample" / "src" / "lib.rs").write_text(
            """pub fn sample() {
    let _ = tokio::spawn(async {});
}
""",
            encoding="utf-8",
        )

        base_policy = {
            "contract_id": "ft.asupersync.cutover_runtime_guards.v1",
            "version": "1.0.0",
            "bead_id": "ft-e34d9.10.8.2",
            "source_roots": ["crates", "frankenterm"],
            "forbidden_dependencies": ["tokio"],
            "dependency_allowlist": {"tokio": ["Cargo.toml"]},
            "forbidden_token_ceilings": {"tokio::": 1},
            "strict_zero_modules": [],
            "strict_zero_forbidden_tokens": ["tokio::"],
            "determinism_contract": {"count_policy": "actual_must_not_exceed_ceiling"},
        }

        result = evaluate_policy(temp_root, base_policy)
        assert result["status"] == "passed"

        bad_allowlist_policy = dict(base_policy)
        bad_allowlist_policy["dependency_allowlist"] = {"tokio": []}
        try:
            evaluate_policy(temp_root, bad_allowlist_policy)
            raise AssertionError("expected allowlist validation failure")
        except ValidationFailure as exc:
            assert exc.code == "unexpected_dependency_manifest"

        bad_ceiling_policy = dict(base_policy)
        bad_ceiling_policy["forbidden_token_ceilings"] = {"tokio::": 0}
        try:
            evaluate_policy(temp_root, bad_ceiling_policy)
            raise AssertionError("expected token ceiling validation failure")
        except ValidationFailure as exc:
            assert exc.code == "token_ceiling_exceeded"

        strict_path = "crates/sample/src/lib.rs"
        bad_strict_policy = dict(base_policy)
        bad_strict_policy["strict_zero_modules"] = [strict_path]
        try:
            evaluate_policy(temp_root, bad_strict_policy)
            raise AssertionError("expected strict module validation failure")
        except ValidationFailure as exc:
            assert exc.code == "strict_module_forbidden_token"


root = Path(sys.argv[1]).resolve()
policy_path = Path(sys.argv[2]).resolve()
out_path = Path(sys.argv[3]).resolve()
self_test = bool(int(sys.argv[4]))

try:
    if self_test:
        run_self_tests()

    require(policy_path.exists(), "policy_missing", f"policy file missing: {policy_path}")
    policy = json.loads(policy_path.read_text(encoding="utf-8"))
    report = evaluate_policy(root, policy)
    out_path.write_text(json.dumps(report, indent=2, sort_keys=True) + "\n", encoding="utf-8")
except ValidationFailure as exc:
    failure = {
        "status": "failed",
        "checked_at": now_iso(),
        "error_code": exc.code,
        "error_message": exc.message,
        "detail": exc.detail or {},
        "policy_path": str(policy_path),
        "repo_root": str(root),
    }
    out_path.write_text(json.dumps(failure, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    print(f"[cutover-runtime-guard-validator] {exc.code}: {exc.message}", file=sys.stderr)
    sys.exit(1)
except Exception as exc:  # pragma: no cover
    failure = {
        "status": "failed",
        "checked_at": now_iso(),
        "error_code": "validator_internal_error",
        "error_message": str(exc),
        "policy_path": str(policy_path),
        "repo_root": str(root),
    }
    out_path.write_text(json.dumps(failure, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    print(f"[cutover-runtime-guard-validator] validator_internal_error: {exc}", file=sys.stderr)
    sys.exit(1)
PY
