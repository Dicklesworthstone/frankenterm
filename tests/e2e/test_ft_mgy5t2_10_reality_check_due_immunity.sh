#!/usr/bin/env bash
# tests/e2e/test_ft_mgy5t2_10_reality_check_due_immunity.sh
# Companion adversarial fixture test suite for ft-mgy5t2.9 / ft-mgy5t2.10 (RC4.G98).
# Proves scripts/check-reality-check-due.sh is immune to mass-delete/mass-restore
# commits (1b6e74080/78869998f pattern), fails closed on invalid Baseline commit
# headers, and proves scripts/reality-check-status.sh buckets RC2/RC3/RC4 cleanly
# with zero RC4 beads in uncategorized.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd -P)"

python3 - "$ROOT" <<'PY'
import json
import os
import subprocess
import sys
import tempfile
from pathlib import Path

root = Path(sys.argv[1])
due_sh = root / "scripts/check-reality-check-due.sh"
status_sh = root / "scripts/reality-check-status.sh"

results = []


def git(cwd: Path, *args: str) -> str:
    env = dict(os.environ)
    env.update({
        "GIT_AUTHOR_NAME": "Fixture Test",
        "GIT_AUTHOR_EMAIL": "fixture@example.test",
        "GIT_COMMITTER_NAME": "Fixture Test",
        "GIT_COMMITTER_EMAIL": "fixture@example.test",
    })
    p = subprocess.run(["git", *args], cwd=cwd, env=env, capture_output=True, text=True, check=True)
    return p.stdout.strip()


def init_fixture(repo: Path) -> str:
    git(repo, "init", "-b", "main")
    (repo / "Cargo.toml").write_text('[workspace.package]\nversion = "0.15.21"\n')
    (repo / "README.md").write_text("# ft\n- 10 ms latency\n## Quickstart\n")
    (repo / "docs/design").mkdir(parents=True, exist_ok=True)
    (repo / "docs/design/product-journey-contract.md").write_text("# Contract\nline 1\n")
    git(repo, "add", ".")
    git(repo, "commit", "-m", "initial baseline")
    return git(repo, "rev-parse", "HEAD")


with tempfile.TemporaryDirectory(prefix="ft-rc4-g98-test-") as td:
    td_path = Path(td)

    # Case 1: Plan with explicit **Baseline commit:** survives mass delete + mass restore
    r1 = td_path / "repo_header"
    r1.mkdir()
    c1 = init_fixture(r1)
    plan1 = r1 / "docs/reality-check-bridge-plan-2026-11-20.md"
    plan1.write_text(f"# Plan\n\n**Baseline commit:** `{c1}`\n")
    git(r1, "add", ".")
    git(r1, "commit", "-m", "add plan with baseline header")

    # Add 65 contract lines (must trigger contract_doc_churn >= 50)
    contract = r1 / "docs/design/product-journey-contract.md"
    contract.write_text("# Contract\n" + "".join(f"contract clause {i}\n" for i in range(65)))
    git(r1, "add", ".")
    git(r1, "commit", "-m", "expand contract by 65 lines")

    # Simulate mass delete + mass restore touching the plan file
    git(r1, "rm", "docs/reality-check-bridge-plan-2026-11-20.md")
    git(r1, "commit", "-m", "accidental mass delete")
    plan1.write_text(f"# Plan\n\n**Baseline commit:** `{c1}`\n")
    git(r1, "add", ".")
    git(r1, "commit", "-m", "mass restore")
    c_restore = git(r1, "rev-parse", "HEAD")

    p = subprocess.run(
        ["bash", str(due_sh), "--repo-root", str(r1), "--as-of", "2026-11-20", "--json"],
        capture_output=True, text=True, check=True,
    )
    out = json.loads(p.stdout)
    ok1 = (
        out["baseline_source"] == "plan_header"
        and out["latest_tracked_plan_commit"] == c1
        and out["latest_tracked_plan_commit"] != c_restore
        and out["signals"]["contract_doc_churn"]["changed_lines"] >= 60
        and out["signals"]["contract_doc_churn"]["triggered"] is True
    )
    results.append({
        "case_id": "pos_plan_header_survives_mass_restore",
        "baseline_source": out["baseline_source"],
        "expected_sha": c1,
        "actual_sha": out["latest_tracked_plan_commit"],
        "contract_lines": out["signals"]["contract_doc_churn"]["changed_lines"],
        "verdict": "pass" if ok1 else "fail",
    })

    # Case 2: Legacy plan WITHOUT header falls back to first_add_commit (NOT restore commit!)
    r2 = td_path / "repo_legacy"
    r2.mkdir()
    init_fixture(r2)
    plan2 = r2 / "docs/reality-check-bridge-plan-2026-09-01.md"
    plan2.write_text("# Legacy Plan without Baseline header\n")
    git(r2, "add", ".")
    git(r2, "commit", "-m", "add legacy plan")
    c_add = git(r2, "rev-parse", "HEAD")

    (r2 / "docs/design/product-journey-contract.md").write_text(
        "# Contract\n" + "".join(f"legacy clause {i}\n" for i in range(60))
    )
    git(r2, "add", ".")
    git(r2, "commit", "-m", "contract churn after legacy plan")

    git(r2, "rm", "docs/reality-check-bridge-plan-2026-09-01.md")
    git(r2, "commit", "-m", "accidental mass delete")
    plan2.write_text("# Legacy Plan without Baseline header\n")
    git(r2, "add", ".")
    git(r2, "commit", "-m", "mass restore")
    c_restore2 = git(r2, "rev-parse", "HEAD")

    p = subprocess.run(
        ["bash", str(due_sh), "--repo-root", str(r2), "--as-of", "2026-11-20", "--json"],
        capture_output=True, text=True, check=True,
    )
    out2 = json.loads(p.stdout)
    ok2 = (
        out2["baseline_source"] == "first_add_commit"
        and out2["latest_tracked_plan_commit"] == c_add
        and out2["latest_tracked_plan_commit"] != c_restore2
        and out2["signals"]["contract_doc_churn"]["changed_lines"] >= 55
    )
    results.append({
        "case_id": "pos_legacy_first_add_survives_mass_restore",
        "baseline_source": out2["baseline_source"],
        "expected_sha": c_add,
        "actual_sha": out2["latest_tracked_plan_commit"],
        "contract_lines": out2["signals"]["contract_doc_churn"]["changed_lines"],
        "verdict": "pass" if ok2 else "fail",
    })

    # Case 3: Planted negative — invalid Baseline commit fails closed (exit 2)
    r3 = td_path / "repo_bad_sha"
    r3.mkdir()
    init_fixture(r3)
    (r3 / "docs/reality-check-bridge-plan-2026-11-20.md").write_text(
        "# Bad Plan\n\n**Baseline commit:** `deadbeefdeadbeefdeadbeefdeadbeefdeadbeef`\n"
    )
    git(r3, "add", ".")
    git(r3, "commit", "-m", "add plan with bogus sha")
    p = subprocess.run(
        ["bash", str(due_sh), "--repo-root", str(r3), "--as-of", "2026-11-20", "--json"],
        capture_output=True, text=True,
    )
    ok3 = (p.returncode == 2 and "invalid_baseline_commit" in p.stderr)
    results.append({
        "case_id": "neg_invalid_baseline_commit_fails_closed",
        "expected_exit": 2,
        "actual_exit": p.returncode,
        "stderr": p.stderr.strip(),
        "verdict": "pass" if ok3 else "fail",
    })

    # Case 4: Live repo check-reality-check-due.sh anchors on 93d33742e... via plan_header
    p = subprocess.run(["bash", str(due_sh), "--as-of", "2026-11-20", "--json"], capture_output=True, text=True, check=True)
    out4 = json.loads(p.stdout)
    ok4 = (
        out4["baseline_source"] == "plan_header"
        and out4["latest_tracked_plan_commit"] == "93d33742e75b063115a7460da9904e5f0d360303"
        and out4["latest_plan_file"] == "docs/reality-check-bridge-plan-2026-11-20.md"
    )
    results.append({
        "case_id": "pos_live_repo_due_check_anchored",
        "baseline_source": out4["baseline_source"],
        "actual_sha": out4["latest_tracked_plan_commit"],
        "verdict": "pass" if ok4 else "fail",
    })

    # Case 5: Live repo reality-check-status.sh buckets RC2, RC3, RC4 cleanly (0 RC4 in uncategorized)
    p = subprocess.run(["bash", str(status_sh), "--json"], capture_output=True, text=True, check=True)
    st = json.loads(p.stdout)
    epics = set(st.get("epics", []))
    uncat_rc4 = []
    for grp in st.get("issues_by_epic", []):
        if grp["epic"] == "uncategorized":
            for iss in grp.get("issues", []):
                if iss["id"] == "ft-mgy5t2" or iss["id"].startswith("ft-mgy5t2."):
                    uncat_rc4.append(iss["id"])
    ok5 = (
        {"RC2-ft-tf6g3", "RC3-ft-xxfwy", "RC4-ft-mgy5t2"}.issubset(epics)
        and len(uncat_rc4) == 0
    )
    results.append({
        "case_id": "pos_live_status_multi_epic_zero_rc4_uncat",
        "epics_found": sorted(epics),
        "uncategorized_rc4": len(uncat_rc4),
        "verdict": "pass" if ok5 else "fail",
    })

all_ok = all(r["verdict"] == "pass" for r in results)
print(json.dumps({
    "schema_version": "frankenterm.test_receipt.v1",
    "bead_id": "ft-mgy5t2.10",
    "parent_bead": "ft-mgy5t2.9",
    "ok": all_ok,
    "passed": sum(1 for r in results if r["verdict"] == "pass"),
    "failed": sum(1 for r in results if r["verdict"] != "pass"),
    "cases": results,
}, indent=2))
sys.exit(0 if all_ok else 1)
PY
