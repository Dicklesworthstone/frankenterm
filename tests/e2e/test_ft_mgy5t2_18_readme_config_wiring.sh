#!/usr/bin/env bash
# tests/e2e/test_ft_mgy5t2_18_readme_config_wiring.sh
# Companion adversarial test suite for ft-mgy5t2.17 / ft-mgy5t2.18 (RC4.G102).
# Proves 100% of TOML keys in README.md's sample ft.toml blocks are wired
# (zero parsed_but_unconsumed exposure) and proves planted unconsumed keys fail closed.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd -P)"

python3 - "$ROOT" <<'PY'
import json
import subprocess
import sys
import tempfile
from pathlib import Path

root = Path(sys.argv[1])
gate_sh = root / "scripts/check-readme-config-wiring.sh"
readme_src = (root / "README.md").read_text(encoding="utf-8")
results = []

# Positive 1: live README.md passes with 0 violations (now that safety.redaction is Consumed!)
p = subprocess.run(["bash", str(gate_sh), "--json"], capture_output=True, text=True)
out = json.loads(p.stdout)
results.append({
    "case_id": "pos_live_readme_ft_toml_clean",
    "readme_keys_checked": out.get("readme_keys_checked"),
    "expected_exit": 0,
    "actual_exit": p.returncode,
    "verdict": "pass" if (p.returncode == 0 and out["ok"] is True and out["readme_keys_checked"] > 20) else "fail",
})

with tempfile.TemporaryDirectory(prefix="ft-rc4-g102-test-") as td:
    td_path = Path(td)

    # Planted negative 1: advertise unconsumed [safety.capabilities] in fixture README
    r_bad1 = td_path / "README_bad_cap.md"
    r_bad1.write_text(readme_src + "\n```toml\n[safety.capabilities]\nallow_arbitrary_text = true\n```\n")
    p1 = subprocess.run(["bash", str(gate_sh), "--json", "--readme", str(r_bad1)], capture_output=True, text=True)
    out1 = json.loads(p1.stdout)
    hit1 = any(v["unconsumed_inventory_key"] == "safety.capabilities" for v in out1.get("violations", []))
    results.append({
        "case_id": "neg_planted_unconsumed_safety_capabilities",
        "expected_exit": 1,
        "actual_exit": p1.returncode,
        "detected": hit1,
        "verdict": "pass" if (p1.returncode == 1 and hit1) else "fail",
    })

    # Planted negative 2: advertise unconsumed search.reranker_enabled in fixture README
    r_bad2 = td_path / "README_bad_rerank.md"
    r_bad2.write_text(readme_src + "\n```toml\n[search]\nreranker_enabled = true\n```\n")
    p2 = subprocess.run(["bash", str(gate_sh), "--json", "--readme", str(r_bad2)], capture_output=True, text=True)
    out2 = json.loads(p2.stdout)
    hit2 = any(v["unconsumed_inventory_key"] == "search.reranker_enabled" for v in out2.get("violations", []))
    results.append({
        "case_id": "neg_planted_unconsumed_search_reranker",
        "expected_exit": 1,
        "actual_exit": p2.returncode,
        "detected": hit2,
        "verdict": "pass" if (p2.returncode == 1 and hit2) else "fail",
    })

# Positive 2: scripts/release-gates.sh --list includes "readme config wiring"
p_list = subprocess.run(["bash", str(root / "scripts/release-gates.sh"), "--list"], capture_output=True, text=True, check=True)
results.append({
    "case_id": "pos_release_gates_list_includes_readme_config_wiring",
    "expected_exit": 0,
    "actual_exit": 0 if "readme config wiring" in p_list.stdout else 1,
    "verdict": "pass" if "readme config wiring" in p_list.stdout else "fail",
})

all_ok = all(r["verdict"] == "pass" for r in results)
print(json.dumps({
    "schema_version": "frankenterm.test_receipt.v1",
    "bead_id": "ft-mgy5t2.18",
    "parent_bead": "ft-mgy5t2.17",
    "ok": all_ok,
    "passed": sum(1 for r in results if r["verdict"] == "pass"),
    "failed": sum(1 for r in results if r["verdict"] != "pass"),
    "cases": results,
}, indent=2))
sys.exit(0 if all_ok else 1)
PY
