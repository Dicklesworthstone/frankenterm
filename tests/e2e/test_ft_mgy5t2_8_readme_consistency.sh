#!/usr/bin/env bash
# tests/e2e/test_ft_mgy5t2_8_readme_consistency.sh
# Companion adversarial test suite for ft-mgy5t2.7 / ft-mgy5t2.8 (RC4.G97).
# Validates scripts/check-readme-consistency.sh on the live repo AND against
# all 5 planted documentation-drift negative fixtures.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd -P)"

python3 - "$ROOT" <<'PY'
import json
import subprocess
import sys
import tempfile
from pathlib import Path

root = Path(sys.argv[1])
check_sh = root / "scripts/check-readme-consistency.sh"
readme_src = (root / "README.md").read_text(encoding="utf-8")
agents_src = (root / "AGENTS.md").read_text(encoding="utf-8")
claims_src = (root / "docs/perf/headline-claims.json").read_text(encoding="utf-8")

results = []


def run_check(case_id: str, expected_rc: int, expected_rule: str | None, args: list[str]):
    p = subprocess.run(["bash", str(check_sh), "--json", *args], capture_output=True, text=True)
    out = json.loads(p.stdout)
    rules = [v["rule"] for v in out.get("violations", [])]
    ok = (p.returncode == expected_rc) and (
        expected_rule is None if expected_rc == 0 else expected_rule in rules
    )
    results.append({
        "case_id": case_id,
        "expected_exit": expected_rc,
        "actual_exit": p.returncode,
        "detected_rules": rules,
        "verdict": "pass" if ok else "fail",
    })


# Positive: live repository passes with 0 violations
run_check("pos_live_repo_clean", 0, None, [])

with tempfile.TemporaryDirectory(prefix="ft-rc4-g97-test-") as td:
    td_path = Path(td)
    r_ok = td_path / "README.md"
    a_ok = td_path / "AGENTS.md"
    c_ok = td_path / "claims.json"
    r_ok.write_text(readme_src, encoding="utf-8")
    a_ok.write_text(agents_src, encoding="utf-8")
    c_ok.write_text(claims_src, encoding="utf-8")

    # Negative 1: remove one crates/* entry from fixture AGENTS.md
    a_bad = td_path / "AGENTS_missing_crate.md"
    a_bad.write_text(agents_src.replace("frankenterm-renderer-metal", "removed-crate-name"), encoding="utf-8")
    run_check(
        "neg_missing_workspace_crate", 1, "agents_missing_workspace_crate",
        ["--readme", str(r_ok), "--agents", str(a_bad), "--claims", str(c_ok)],
    )

    # Negative 2: re-insert distributed "off by default" contradiction
    r_dist = td_path / "README_dist.md"
    r_dist.write_text(readme_src + "\nSecure distributed mode is available and is off by default.\n", encoding="utf-8")
    run_check(
        "neg_distributed_off_by_default", 1, "readme_distributed_default_contradiction",
        ["--readme", str(r_dist), "--agents", str(a_ok), "--claims", str(c_ok)],
    )

    # Negative 3: re-insert "requires ML embeddings" contradiction
    r_ml = td_path / "README_ml.md"
    r_ml.write_text(readme_src + "\n| Semantic search | Feature-gated (requires ML embeddings) |\n", encoding="utf-8")
    run_check(
        "neg_requires_ml_embeddings", 1, "readme_semantic_ml_contradiction",
        ["--readme", str(r_ml), "--agents", str(a_ok), "--claims", str(c_ok)],
    )

    # Negative 4: re-insert stale RC2 "current round (ft-tf6g3)"
    r_rc = td_path / "README_rc2.md"
    r_rc.write_text(readme_src + "\nThe current round (ft-tf6g3, opened 2026-05-12) is active.\n", encoding="utf-8")
    run_check(
        "neg_stale_rc2_current_round", 1, "readme_stale_rc_current_round",
        ["--readme", str(r_rc), "--agents", str(a_ok), "--claims", str(c_ok)],
    )

    # Negative 5: stale 4KB overlap in headline-claims.json
    c_bad = td_path / "claims_4kb.json"
    c_bad.write_text(claims_src.replace("bounded 1 MiB overlap matching", "4KB overlap matching"), encoding="utf-8")
    run_check(
        "neg_stale_4kb_overlap", 1, "headline_claims_stale_4kb_overlap",
        ["--readme", str(r_ok), "--agents", str(a_ok), "--claims", str(c_bad)],
    )

# Also verify scripts/release-gates.sh --list includes "readme consistency"
p_list = subprocess.run(["bash", str(root / "scripts/release-gates.sh"), "--list"], capture_output=True, text=True, check=True)
ok_wired = "readme consistency" in p_list.stdout
results.append({
    "case_id": "pos_release_gates_list_includes_readme_consistency",
    "expected_exit": 0,
    "actual_exit": 0 if ok_wired else 1,
    "verdict": "pass" if ok_wired else "fail",
})

all_ok = all(r["verdict"] == "pass" for r in results)
print(json.dumps({
    "schema_version": "frankenterm.test_receipt.v1",
    "bead_id": "ft-mgy5t2.8",
    "parent_bead": "ft-mgy5t2.7",
    "ok": all_ok,
    "passed": sum(1 for r in results if r["verdict"] == "pass"),
    "failed": sum(1 for r in results if r["verdict"] != "pass"),
    "cases": results,
}, indent=2))
sys.exit(0 if all_ok else 1)
PY
