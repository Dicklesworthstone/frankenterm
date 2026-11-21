#!/usr/bin/env bash
# tests/e2e/test_ft_mgy5t2_12_fleet_preflight.sh
# Companion adversarial test suite for ft-mgy5t2.11 / ft-mgy5t2.12 (RC4.G100).
# Validates scripts/fleet-proof-preflight.sh schema, 4 planted unhealthy-fleet
# negatives, and static read-only / Rule-0.1 / Agent-Mail safety guards.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd -P)"

python3 - "$ROOT" <<'PY'
import json
import re
import subprocess
import sys
import tempfile
from pathlib import Path

root = Path(sys.argv[1])
preflight_sh = root / "scripts/fleet-proof-preflight.sh"
results = []

HEALTHY_DSR = {
    "hosts": [
        {"host": "studio", "healthy": True, "platforms": ["darwin/arm64"]},
        {"host": "hz3", "healthy": True, "platforms": ["linux/amd64"]},
        {"host": "mold", "healthy": True, "platforms": ["linux/arm64"]},
        {"host": "wsurf", "healthy": True, "platforms": ["windows/amd64"]},
        {"host": "trj", "healthy": True, "platforms": ["linux/amd64"]},
    ]
}
HEALTHY_RCH_DOCTOR = "All 58 checks passed\n"
HEALTHY_RCH_STATUS = (
    "  Builds : 100 total, 92% command success rate\n"
    "  Worker hz3 80.0 GiB free\n"
    "  Worker trj 418.4 GiB free\n"
)


def run_case(case_id: str, dsr_obj: dict, rch_doc: str, rch_st: str, expected_rc: int, check_fn):
    with tempfile.TemporaryDirectory(prefix="ft-rc4-g100-test-") as td:
        p = Path(td)
        dsr_f = p / "dsr.json"
        doc_f = p / "rch_doc.txt"
        st_f = p / "rch_st.txt"
        dsr_f.write_text(json.dumps(dsr_obj))
        doc_f.write_text(rch_doc)
        st_f.write_text(rch_st)
        proc = subprocess.run(
            ["bash", str(preflight_sh), "--json",
             "--dsr-health-input", str(dsr_f),
             "--rch-doctor-input", str(doc_f),
             "--rch-status-input", str(st_f)],
            capture_output=True, text=True,
        )
        out = json.loads(proc.stdout)
        ok = (proc.returncode == expected_rc) and check_fn(out)
        results.append({
            "case_id": case_id,
            "release_ready": out.get("release_ready"),
            "target_class_ready": out.get("target_class_ready"),
            "missing_platforms": out.get("missing_platforms"),
            "expected_exit": expected_rc,
            "actual_exit": proc.returncode,
            "verdict": "pass" if ok else "fail",
        })


# Positive: all 4 platforms healthy, trj clean, RCH 92%
run_case(
    "pos_healthy_fleet",
    HEALTHY_DSR, HEALTHY_RCH_DOCTOR, HEALTHY_RCH_STATUS, 0,
    lambda o: o["release_ready"] is True and o["target_class_ready"] is True and o["missing_platforms"] == [],
)

# Negative 1: both Windows hosts unhealthy
dsr_no_win = {"hosts": [h for h in HEALTHY_DSR["hosts"] if h["host"] != "wsurf"] + [
    {"host": "wlap", "healthy": False, "platforms": ["windows/amd64"]},
    {"host": "wsurf", "healthy": False, "platforms": ["windows/amd64"]},
]}
run_case(
    "neg_missing_windows_platform",
    dsr_no_win, HEALTHY_RCH_DOCTOR, HEALTHY_RCH_STATUS, 1,
    lambda o: o["release_ready"] is False and o["missing_platforms"] == ["windows/amd64"],
)

# Negative 2: trj RCH_SOURCE_TOPOLOGY_INCOMPATIBLE
run_case(
    "neg_trj_topology_mismatch",
    HEALTHY_DSR,
    "Worker trj excluded: RCH_SOURCE_TOPOLOGY_INCOMPATIBLE conflicting_alias_base\n",
    HEALTHY_RCH_STATUS, 1,
    lambda o: o["target_class_ready"] is False and o["trj_topology_error"] is True,
)

# Negative 3: critical low disk on hz1 / mminiold
run_case(
    "neg_critical_disk_alert",
    HEALTHY_DSR, HEALTHY_RCH_DOCTOR,
    HEALTHY_RCH_STATUS + "  Worker hz1 5.9 GiB free\n  Worker mminiold 1.9 GiB free\n", 1,
    lambda o: o["release_ready"] is False and len(o["disk_alerts"]) >= 2,
)

# Negative 4: RCH 100-build success rate 34% (< 85% floor)
run_case(
    "neg_low_rch_success_rate",
    HEALTHY_DSR, HEALTHY_RCH_DOCTOR,
    HEALTHY_RCH_STATUS.replace("92%", "34%"), 1,
    lambda o: o["release_ready"] is False and o["rch_healthy"] is False and o["rch_success_pct"] == 34,
)

# Static read-only safety audit: no ssh write, no am service, no gh run
src = preflight_sh.read_text(encoding="utf-8")
# Strip comments before checking for forbidden command invocations
code_lines = "\n".join(l for l in src.splitlines() if not l.lstrip().startswith("#"))
forbidden = [
    ("ssh_exec", re.search(r"\bssh\s+", code_lines)),
    ("am_service", re.search(r"\bam\s+service\b", code_lines)),
    ("gh_run", re.search(r"\bgh\s+run\b", code_lines)),
]
bad_cmds = [name for name, hit in forbidden if hit]
results.append({
    "case_id": "pos_static_readonly_and_rule01_audit",
    "forbidden_hits": bad_cmds,
    "verdict": "pass" if not bad_cmds else "fail",
})

all_ok = all(r["verdict"] == "pass" for r in results)
print(json.dumps({
    "schema_version": "frankenterm.test_receipt.v1",
    "bead_id": "ft-mgy5t2.12",
    "parent_bead": "ft-mgy5t2.11",
    "ok": all_ok,
    "passed": sum(1 for r in results if r["verdict"] == "pass"),
    "failed": sum(1 for r in results if r["verdict"] != "pass"),
    "cases": results,
}, indent=2))
sys.exit(0 if all_ok else 1)
PY
