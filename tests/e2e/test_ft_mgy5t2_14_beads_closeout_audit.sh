#!/usr/bin/env bash
# tests/e2e/test_ft_mgy5t2_14_beads_closeout_audit.sh
# Companion adversarial test suite for ft-mgy5t2.13 / ft-mgy5t2.14 (RC4.G99).
# Proves 4-bucket classification accuracy, P0<=25 & stale-WIP budget enforcement,
# and byte-identical read-only safety of scripts/beads-closeout-audit.sh.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd -P)"

python3 - "$ROOT" <<'PY'
import hashlib
import json
import subprocess
import sys
import tempfile
from pathlib import Path

root = Path(sys.argv[1])
audit_sh = root / "scripts/beads-closeout-audit.sh"
results = []

with tempfile.TemporaryDirectory(prefix="ft-rc4-g99-test-") as td:
    td_path = Path(td)

    # Fixture 1: all 4 classification buckets + read-only byte-identity check
    f1 = td_path / "four_buckets.jsonl"
    records = [
        {
            "id": "ft-case-proof", "status": "in_progress", "priority": 1,
            "title": "Proof complete unclosed",
            "updated_at": "2026-11-18T00:00:00Z",
            "comments": [{"text": "Committed in abcdef123456; all 11 tests passed and verified receipt.json"}],
        },
        {
            "id": "ft-case-infra", "status": "in_progress", "priority": 1,
            "title": "Blocked on RCH infra",
            "updated_at": "2026-11-18T00:00:00Z",
            "comments": [{"text": "Remote build hit RCH-E330 remote_build_disk_budget_exceeded on worker"}],
        },
        {
            "id": "ft-case-stale", "status": "in_progress", "priority": 2,
            "title": "Abandoned stale bead",
            "updated_at": "2026-10-25T00:00:00Z",
            "comments": [],
        },
        {
            "id": "ft-case-active", "status": "in_progress", "priority": 1,
            "title": "Actively worked bead",
            "updated_at": "2026-11-19T00:00:00Z",
            "comments": [{"text": "Working on parser refactor"}],
        },
    ]
    f1.write_text("\n".join(json.dumps(r) for r in records) + "\n")
    sha_before = hashlib.sha256(f1.read_bytes()).hexdigest()

    p = subprocess.run(
        ["bash", str(audit_sh), "--beads", str(f1), "--as-of", "2026-11-20", "--json"],
        capture_output=True, text=True, check=True,
    )
    sha_after = hashlib.sha256(f1.read_bytes()).hexdigest()
    out = json.loads(p.stdout)
    ok_cls = (
        out["counts"] == {
            "active": 1,
            "blocked_on_infra": 1,
            "proof_complete_unclosed": 1,
            "stale_abandoned": 1,
        }
        and sha_before == sha_after
    )
    results.append({
        "case_id": "pos_four_buckets_and_readonly_sha",
        "counts": out["counts"],
        "readonly_preserved": sha_before == sha_after,
        "verdict": "pass" if ok_cls else "fail",
    })

    # Negative 1: 26 open P0s under --enforce-budget -> exit 1 naming p0_budget_exceeded: 26 > 25
    f2 = td_path / "p0_overflow.jsonl"
    f2.write_text(
        "\n".join(
            json.dumps({"id": f"ft-p0-{i}", "status": "open", "priority": 0, "title": f"P0 {i}", "updated_at": "2026-11-19T00:00:00Z"})
            for i in range(26)
        ) + "\n"
    )
    p2 = subprocess.run(
        ["bash", str(audit_sh), "--beads", str(f2), "--as-of", "2026-11-20", "--enforce-budget", "--json"],
        capture_output=True, text=True,
    )
    out2 = json.loads(p2.stdout)
    ok2 = (p2.returncode == 1) and any("p0_budget_exceeded: 26 > 25" in v for v in out2["budget_violations"])
    results.append({
        "case_id": "neg_p0_budget_26_exceeds_25",
        "actual_exit": p2.returncode,
        "violations": out2["budget_violations"],
        "verdict": "pass" if ok2 else "fail",
    })

    # Negative 2: 1 stale_abandoned bead under --enforce-budget -> exit 1 naming stale_in_progress_gt_14d: 1 > 0
    p3 = subprocess.run(
        ["bash", str(audit_sh), "--beads", str(f1), "--as-of", "2026-11-20", "--enforce-budget", "--json"],
        capture_output=True, text=True,
    )
    out3 = json.loads(p3.stdout)
    ok3 = (p3.returncode == 1) and any("stale_in_progress_gt_14d: 1 > 0" in v for v in out3["budget_violations"])
    results.append({
        "case_id": "neg_stale_wip_fails_budget",
        "actual_exit": p3.returncode,
        "violations": out3["budget_violations"],
        "verdict": "pass" if ok3 else "fail",
    })

    # Positive: clean budget (<=25 P0, 0 stale_abandoned) passes --enforce-budget (exit 0)
    f4 = td_path / "clean_budget.jsonl"
    f4.write_text(
        json.dumps({"id": "ft-ok-1", "status": "in_progress", "priority": 0, "title": "Active P0", "updated_at": "2026-11-19T00:00:00Z"}) + "\n"
    )
    p4 = subprocess.run(
        ["bash", str(audit_sh), "--beads", str(f4), "--as-of", "2026-11-20", "--enforce-budget", "--json"],
        capture_output=True, text=True,
    )
    out4 = json.loads(p4.stdout)
    ok4 = (p4.returncode == 0) and out4["ok"] is True
    results.append({
        "case_id": "pos_clean_budget_passes",
        "actual_exit": p4.returncode,
        "verdict": "pass" if ok4 else "fail",
    })

all_ok = all(r["verdict"] == "pass" for r in results)
print(json.dumps({
    "schema_version": "frankenterm.test_receipt.v1",
    "bead_id": "ft-mgy5t2.14",
    "parent_bead": "ft-mgy5t2.13",
    "ok": all_ok,
    "passed": sum(1 for r in results if r["verdict"] == "pass"),
    "failed": sum(1 for r in results if r["verdict"] != "pass"),
    "cases": results,
}, indent=2))
sys.exit(0 if all_ok else 1)
PY
