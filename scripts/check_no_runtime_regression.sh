#!/usr/bin/env bash
# ft-xbnl0.2.6 — Strict per-file no-runtime-regression gate.
#
# Differs from validate_asupersync_cutover_runtime_guards.sh:
#  * That script enforces a sliding ceiling on forbidden tokens.
#  * This script enforces a strict per-file allowlist with named owners and
#    removal beads. Any file with forbidden tokens that is NOT explicitly
#    quarantined fails the gate.
#
# This is the durable no-regression invariant for the async migration lane.
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
POLICY_PATH="${ROOT_DIR}/docs/ft-xbnl0-2-6-no-runtime-regression-gate.json"
OUT_PATH="${ROOT_DIR}/docs/ft-xbnl0-2-6-no-runtime-regression-validation.json"
SELF_TEST=0

usage() {
  cat <<'USAGE'
Usage: check_no_runtime_regression.sh [options]

Options:
  --root <path>          Override repository root
  --policy-path <path>   Override gate policy JSON path
  --output <path>        Output report JSON path
  --self-test            Run validator self-tests before validation
  -h, --help             Show this help

Exit codes:
  0  gate passed
  1  gate failed (regression detected or policy malformed)
  2  validator internal error
USAGE
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --root)         ROOT_DIR="$2"; shift 2 ;;
    --policy-path)  POLICY_PATH="$2"; shift 2 ;;
    --output)       OUT_PATH="$2"; shift 2 ;;
    --self-test)    SELF_TEST=1; shift ;;
    -h|--help)      usage; exit 0 ;;
    *) echo "Unknown option: $1" >&2; usage >&2; exit 2 ;;
  esac
done

mkdir -p "$(dirname "${OUT_PATH}")"

python3 - "${ROOT_DIR}" "${POLICY_PATH}" "${OUT_PATH}" "${SELF_TEST}" <<'PY'
from __future__ import annotations

import json
import re
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


def normalize(p: Path) -> str:
    return p.as_posix()


def load_toml(path: Path) -> dict[str, Any]:
    return tomllib.loads(path.read_text(encoding="utf-8"))


def workspace_member_manifests(root: Path) -> list[Path]:
    workspace_manifest = root / "Cargo.toml"
    require(workspace_manifest.exists(), "workspace_manifest_missing",
            f"workspace Cargo.toml missing at {workspace_manifest}")
    workspace = load_toml(workspace_manifest)
    members = workspace.get("workspace", {}).get("members", [])
    require(isinstance(members, list), "invalid_workspace_members",
            "workspace.members must be a list")
    manifests = [Path("Cargo.toml")]
    for m in members:
        require(isinstance(m, str) and m, "invalid_workspace_member",
                "workspace member path must be a non-empty string")
        manifests.append(Path(m) / "Cargo.toml")
    for m in manifests:
        require((root / m).exists(), "workspace_member_manifest_missing",
                f"workspace member manifest missing: {m}")
    return manifests


def dependency_tables(doc: dict[str, Any]) -> list[dict[str, Any]]:
    out: list[dict[str, Any]] = []
    for k in ("dependencies", "dev-dependencies", "build-dependencies"):
        t = doc.get(k)
        if isinstance(t, dict):
            out.append(t)
    ws = doc.get("workspace", {})
    if isinstance(ws, dict):
        wd = ws.get("dependencies")
        if isinstance(wd, dict):
            out.append(wd)
    target = doc.get("target")
    if isinstance(target, dict):
        for cfg in target.values():
            if not isinstance(cfg, dict):
                continue
            for k in ("dependencies", "dev-dependencies", "build-dependencies"):
                t = cfg.get(k)
                if isinstance(t, dict):
                    out.append(t)
    return out


def collect_dependency_hits(root: Path, manifests: list[Path],
                            deps: list[str]) -> dict[str, list[str]]:
    hits: dict[str, set[str]] = {d: set() for d in deps}
    for rel in manifests:
        doc = load_toml(root / rel)
        for table in dependency_tables(doc):
            for d in deps:
                if d in table:
                    hits[d].add(normalize(rel))
    return {d: sorted(p) for d, p in hits.items()}


def collect_source_hits(root: Path, source_roots: list[str],
                        tokens: list[str]) -> dict[str, dict[str, int]]:
    """Return {file_path: {token: count}} for any source file that contains a forbidden token."""
    hits: dict[str, dict[str, int]] = {}
    for rel_root in source_roots:
        sr = root / rel_root
        require(sr.exists() and sr.is_dir(), "invalid_source_root",
                f"source root does not exist: {rel_root}")
        for rs in sr.rglob("*.rs"):
            text = rs.read_text(encoding="utf-8", errors="ignore")
            file_hits: dict[str, int] = {}
            for tok in tokens:
                c = text.count(tok)
                if c > 0:
                    file_hits[tok] = c
            if file_hits:
                hits[normalize(rs.relative_to(root))] = file_hits
    return hits


def collect_doc_hits(root: Path, doc_roots: list[str],
                     tokens: list[str], exempt_dirs: list[str],
                     exempt_extensions: list[str]) -> dict[str, dict[str, int]]:
    hits: dict[str, dict[str, int]] = {}
    exempt_dir_paths = [Path(d) for d in exempt_dirs]
    for rel_root in doc_roots:
        target = root / rel_root
        if not target.exists():
            continue
        if target.is_file():
            paths = [target]
        else:
            paths = [
                p for p in target.rglob("*")
                if p.is_file()
                and p.suffix not in exempt_extensions
                and p.suffix in (".md", ".json", ".txt", ".rst", ".toml")
            ]
        for p in paths:
            rel = p.relative_to(root)
            if any(str(rel).startswith(str(ed) + "/") or str(rel) == str(ed) for ed in exempt_dir_paths):
                continue
            text = p.read_text(encoding="utf-8", errors="ignore")
            file_hits: dict[str, int] = {}
            for tok in tokens:
                c = text.count(tok)
                if c > 0:
                    file_hits[tok] = c
            if file_hits:
                hits[normalize(rel)] = file_hits
    return hits


def matches_quarantine_pattern(rel_path: str, q: dict[str, Any]) -> bool:
    pat = q.get("pattern", "")
    dirs = q.get("applies_to_dirs", [])
    if not any(rel_path.startswith(d + "/") for d in dirs):
        return False
    return re.search(pat, rel_path) is not None


def evaluate_policy(root: Path, policy: dict[str, Any]) -> dict[str, Any]:
    require(policy.get("contract_id") == "ft.xbnl0.2.6.no_runtime_regression_gate.v1",
            "invalid_contract_id", "unexpected contract_id")
    for key in ("forbidden_tokens", "forbidden_dependencies",
                "manifest_dependency_allowlist", "source_quarantine",
                "test_quarantine_pattern", "bench_quarantine_pattern",
                "test_quarantine_explicit",
                "doc_narrative_allowlist", "doc_narrative_rule",
                "source_roots", "doc_roots"):
        require(key in policy, "policy_key_missing",
                f"policy missing required key: {key}")

    tokens = list(policy["forbidden_tokens"])
    deps = list(policy["forbidden_dependencies"])
    source_roots = list(policy["source_roots"])
    doc_roots = list(policy["doc_roots"])
    manifest_allow = policy["manifest_dependency_allowlist"]
    src_quarantine = policy["source_quarantine"]
    test_explicit = policy["test_quarantine_explicit"]
    test_pattern = policy["test_quarantine_pattern"]
    bench_pattern = policy["bench_quarantine_pattern"]
    doc_allow = set(policy["doc_narrative_allowlist"])
    doc_rule = policy["doc_narrative_rule"]

    checks: list[dict[str, Any]] = []
    checks.append({"name": "contract_identity", "status": "passed"})

    # ---- Manifest dependency check ----
    manifests = workspace_member_manifests(root)
    dep_hits = collect_dependency_hits(root, manifests, deps)
    manifest_violations: list[dict[str, Any]] = []
    for d in deps:
        allowed_paths = set((manifest_allow.get(d) or {}).keys())
        observed = set(dep_hits.get(d, []))
        unexpected = sorted(observed - allowed_paths)
        if unexpected:
            manifest_violations.append({
                "dependency": d,
                "unexpected_manifests": unexpected,
                "allowlisted": sorted(allowed_paths),
            })
        # Each allowlisted entry must declare owner + reason + removal_bead.
        for path, meta in (manifest_allow.get(d) or {}).items():
            for required in ("owner", "reason", "removal_bead"):
                if required not in meta:
                    manifest_violations.append({
                        "dependency": d,
                        "incomplete_allowlist_entry": path,
                        "missing_field": required,
                    })
    require(not manifest_violations, "manifest_dependency_regression",
            "forbidden dependency declared in non-allowlisted manifest or allowlist entry incomplete",
            {"violations": manifest_violations})
    checks.append({"name": "manifest_dependency_allowlist",
                   "status": "passed", "observed": dep_hits})

    # ---- Source/test/bench scan ----
    src_hits = collect_source_hits(root, source_roots, tokens)
    src_violations: list[dict[str, Any]] = []
    quarantine_owner_map: dict[str, str] = {}
    for path, file_tokens in sorted(src_hits.items()):
        # Determine which quarantine, if any, covers the file.
        q_meta: dict[str, Any] | None = None
        if path in src_quarantine:
            q_meta = src_quarantine[path]
        elif path in test_explicit:
            q_meta = test_explicit[path]
        elif matches_quarantine_pattern(path, test_pattern):
            q_meta = test_pattern
        elif matches_quarantine_pattern(path, bench_pattern):
            q_meta = bench_pattern
        if q_meta is None:
            src_violations.append({
                "file": path,
                "tokens": file_tokens,
                "diagnosis": "file is not in any quarantine allowlist",
                "fix_hint": "either remove the forbidden runtime usage or add an explicit quarantine entry with owner+reason+removal_bead",
            })
            continue
        # Validate quarantine metadata.
        for field in ("owner", "reason", "removal_bead", "allowed_tokens"):
            if field not in q_meta:
                src_violations.append({
                    "file": path,
                    "diagnosis": f"quarantine metadata missing {field}",
                })
                break
        else:
            allowed = set(q_meta["allowed_tokens"])
            observed_tokens = set(file_tokens.keys())
            stray = sorted(observed_tokens - allowed)
            if stray:
                src_violations.append({
                    "file": path,
                    "tokens": file_tokens,
                    "diagnosis": f"file uses tokens not in its quarantine allowlist: {stray}",
                    "owner": q_meta.get("owner"),
                    "removal_bead": q_meta.get("removal_bead"),
                })
            else:
                quarantine_owner_map[path] = q_meta["owner"]
    require(not src_violations, "source_runtime_regression",
            "forbidden runtime token observed outside the allowlisted quarantine surface",
            {"violations": src_violations})
    checks.append({"name": "source_quarantine_enforcement",
                   "status": "passed",
                   "quarantined_files": len(quarantine_owner_map)})

    # ---- Doc narrative scan ----
    doc_hits = collect_doc_hits(
        root,
        doc_roots,
        tokens,
        list(doc_rule.get("exempt_dirs", [])),
        list(doc_rule.get("exempt_extensions", [])),
    )
    doc_violations: list[dict[str, Any]] = []
    for path, file_tokens in sorted(doc_hits.items()):
        if path not in doc_allow:
            doc_violations.append({
                "file": path,
                "tokens": file_tokens,
                "diagnosis": "doc references forbidden runtime tokens but is not on doc_narrative_allowlist; rewrite the narrative to match the native asupersync architecture or add an explicit allowlist entry",
            })
    require(not doc_violations, "doc_narrative_regression",
            "documentation references forbidden runtime tokens outside the narrative allowlist",
            {"violations": doc_violations})
    checks.append({"name": "doc_narrative_enforcement",
                   "status": "passed", "doc_hits": len(doc_hits)})

    # ---- Check for stale allowlist entries ----
    stale: list[dict[str, Any]] = []
    for path in src_quarantine:
        if path not in src_hits:
            stale.append({"file": path, "kind": "source_quarantine"})
    for path in test_explicit:
        if path not in src_hits:
            stale.append({"file": path, "kind": "test_quarantine_explicit"})
    require(not stale, "stale_quarantine_entry",
            "quarantine allowlist references a file that no longer contains any forbidden tokens; remove the entry to keep the allowlist honest",
            {"stale_entries": stale})
    checks.append({"name": "stale_allowlist_check", "status": "passed"})

    return {
        "status": "passed",
        "checked_at": now_iso(),
        "policy_path": str(policy_path.relative_to(root)) if policy_path.is_relative_to(root) else str(policy_path),
        "repo_root": str(root),
        "checks": checks,
        "summary": {
            "manifest_dependency_hits": dep_hits,
            "source_files_quarantined": len(quarantine_owner_map),
            "doc_files_with_tokens": len(doc_hits),
            "owner_inventory": sorted({owner for owner in quarantine_owner_map.values()}),
        },
    }


def run_self_tests() -> None:
    """Self-tests run against synthetic mini-repos to prove gate enforcement."""

    base_policy = {
        "contract_id": "ft.xbnl0.2.6.no_runtime_regression_gate.v1",
        "version": "1.0.0",
        "bead_id": "ft-xbnl0.2.6",
        "source_roots": ["crates"],
        "doc_roots": ["docs"],
        "manifest_roots": ["Cargo.toml"],
        "forbidden_tokens": ["tokio::"],
        "forbidden_dependencies": ["tokio"],
        "manifest_dependency_allowlist": {
            "tokio": {
                "Cargo.toml": {
                    "owner": "ft-test", "reason": "self-test",
                    "removal_bead": "ft-test",
                }
            }
        },
        "source_quarantine": {
            "crates/sample/src/quarantine.rs": {
                "owner": "ft-test", "reason": "self-test fixture",
                "removal_bead": "ft-test", "allowed_tokens": ["tokio::"],
            }
        },
        "test_quarantine_explicit": {},
        "test_quarantine_pattern": {
            "pattern": "_labruntime\\.rs$",
            "applies_to_dirs": ["crates/sample/tests"],
            "owner": "ft-test", "reason": "self-test pattern",
            "removal_bead": "ft-test", "allowed_tokens": ["tokio::"],
        },
        "bench_quarantine_pattern": {
            "pattern": ".*\\.rs$",
            "applies_to_dirs": ["crates/sample/benches"],
            "owner": "ft-test", "reason": "self-test bench pattern",
            "removal_bead": "ft-test", "allowed_tokens": ["tokio::"],
        },
        "doc_narrative_allowlist": ["docs/migration.md"],
        "doc_narrative_rule": {"description": "x", "exempt_dirs": [], "exempt_extensions": [".log"]},
    }

    def make_repo(td: Path, *, has_unallowed_src=False, has_unallowed_doc=False,
                  unallowed_dep=False, stray_token=False):
        (td / "crates" / "sample" / "src").mkdir(parents=True)
        (td / "crates" / "sample" / "tests").mkdir(parents=True)
        (td / "crates" / "sample" / "benches").mkdir(parents=True)
        (td / "docs").mkdir(parents=True)
        (td / "Cargo.toml").write_text(
            ('[workspace]\nmembers = ["crates/sample"]\n'
             '[workspace.dependencies]\ntokio = "1"\n'),
            encoding="utf-8",
        )
        (td / "crates" / "sample" / "Cargo.toml").write_text(
            "[package]\nname=\"sample\"\nversion=\"0.1.0\"\nedition=\"2024\"\n",
            encoding="utf-8",
        )
        (td / "crates" / "sample" / "src" / "quarantine.rs").write_text(
            "fn x() { let _ = tokio::spawn(async {}); }\n", encoding="utf-8",
        )
        (td / "crates" / "sample" / "tests" / "foo_labruntime.rs").write_text(
            "fn t() { let _ = tokio::time::sleep; }\n", encoding="utf-8",
        )
        (td / "crates" / "sample" / "benches" / "b.rs").write_text(
            "fn b() { let _ = tokio::runtime::Runtime::new(); }\n", encoding="utf-8",
        )
        (td / "docs" / "migration.md").write_text("tokio:: is allowed here\n", encoding="utf-8")
        if has_unallowed_src:
            (td / "crates" / "sample" / "src" / "bad.rs").write_text(
                "fn b() { tokio::spawn(async {}); }\n", encoding="utf-8",
            )
        if has_unallowed_doc:
            (td / "docs" / "rogue.md").write_text("uses tokio:: in narrative\n", encoding="utf-8")
        if unallowed_dep:
            (td / "crates" / "sample" / "Cargo.toml").write_text(
                ('[package]\nname="sample"\nversion="0.1.0"\nedition="2024"\n'
                 '[dependencies]\ntokio = "1"\n'),
                encoding="utf-8",
            )
        if stray_token:
            (td / "crates" / "sample" / "src" / "quarantine.rs").write_text(
                "fn x() { let _ = tokio::spawn(async {}); let _ = smol::block_on; }\n",
                encoding="utf-8",
            )

    # Happy path passes.
    with tempfile.TemporaryDirectory(prefix="ft-no-regress-st-") as td:
        repo = Path(td)
        make_repo(repo)
        report = evaluate_policy(repo, base_policy)
        assert report["status"] == "passed", report

    # Unallowed src file fails.
    with tempfile.TemporaryDirectory(prefix="ft-no-regress-st-") as td:
        repo = Path(td)
        make_repo(repo, has_unallowed_src=True)
        try:
            evaluate_policy(repo, base_policy)
        except GateFailure as e:
            assert e.code == "source_runtime_regression", e
        else:
            raise AssertionError("expected source_runtime_regression failure")

    # Unallowed doc fails.
    with tempfile.TemporaryDirectory(prefix="ft-no-regress-st-") as td:
        repo = Path(td)
        make_repo(repo, has_unallowed_doc=True)
        try:
            evaluate_policy(repo, base_policy)
        except GateFailure as e:
            assert e.code == "doc_narrative_regression", e
        else:
            raise AssertionError("expected doc_narrative_regression failure")

    # Unallowed dependency manifest fails.
    with tempfile.TemporaryDirectory(prefix="ft-no-regress-st-") as td:
        repo = Path(td)
        make_repo(repo, unallowed_dep=True)
        try:
            evaluate_policy(repo, base_policy)
        except GateFailure as e:
            assert e.code == "manifest_dependency_regression", e
        else:
            raise AssertionError("expected manifest_dependency_regression failure")

    # Stale allowlist entry fails (file has no forbidden tokens but allowlist still references it).
    with tempfile.TemporaryDirectory(prefix="ft-no-regress-st-") as td:
        repo = Path(td)
        make_repo(repo)
        # Replace quarantined file with no-tokio content.
        (repo / "crates" / "sample" / "src" / "quarantine.rs").write_text(
            "fn x() {}\n", encoding="utf-8",
        )
        try:
            evaluate_policy(repo, base_policy)
        except GateFailure as e:
            assert e.code == "stale_quarantine_entry", e
        else:
            raise AssertionError("expected stale_quarantine_entry failure")

    # Stray token (token not in file's allowed_tokens) fails.
    with tempfile.TemporaryDirectory(prefix="ft-no-regress-st-") as td:
        repo = Path(td)
        make_repo(repo, stray_token=True)
        # Add smol:: as forbidden token but not allowed in this file's quarantine.
        stray_policy = json.loads(json.dumps(base_policy))
        stray_policy["forbidden_tokens"] = ["tokio::", "smol::"]
        try:
            evaluate_policy(repo, stray_policy)
        except GateFailure as e:
            assert e.code == "source_runtime_regression", e
        else:
            raise AssertionError("expected source_runtime_regression failure for stray token")


root = Path(sys.argv[1]).resolve()
policy_path = Path(sys.argv[2]).resolve()
out_path = Path(sys.argv[3]).resolve()
self_test = bool(int(sys.argv[4]))

try:
    if self_test:
        run_self_tests()

    require(policy_path.exists(), "policy_missing",
            f"policy file missing: {policy_path}")
    policy = json.loads(policy_path.read_text(encoding="utf-8"))
    report = evaluate_policy(root, policy)
    out_path.write_text(json.dumps(report, indent=2, sort_keys=True) + "\n",
                        encoding="utf-8")
except GateFailure as exc:
    failure = {
        "status": "failed",
        "checked_at": now_iso(),
        "error_code": exc.code,
        "error_message": exc.message,
        "detail": exc.detail or {},
        "policy_path": str(policy_path),
        "repo_root": str(root),
    }
    out_path.write_text(json.dumps(failure, indent=2, sort_keys=True) + "\n",
                        encoding="utf-8")
    print(f"[ft-xbnl0.2.6 no-regression gate] {exc.code}: {exc.message}",
          file=sys.stderr)
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
    out_path.write_text(json.dumps(failure, indent=2, sort_keys=True) + "\n",
                        encoding="utf-8")
    print(f"[ft-xbnl0.2.6 no-regression gate] validator_internal_error: {exc}",
          file=sys.stderr)
    sys.exit(2)
PY
