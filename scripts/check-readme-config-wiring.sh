#!/usr/bin/env bash
# scripts/check-readme-config-wiring.sh — fail-closed gate verifying every TOML
# key advertised in README.md's sample ft.toml blocks is NOT listed as
# `parsed_but_unconsumed` in docs/attestations/doctrine/config-key-wiring-status.json
# (ft-mgy5t2.17 / RC4.G102).
#
# Usage:
#   scripts/check-readme-config-wiring.sh [--json] [--readme PATH] [--inventory PATH]
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd -P)"
README_PATH="$ROOT/README.md"
INVENTORY_PATH="$ROOT/docs/attestations/doctrine/config-key-wiring-status.json"
JSON_OUTPUT=0

while [[ $# -gt 0 ]]; do
  case "$1" in
    --readme) README_PATH="$2"; shift 2 ;;
    --inventory) INVENTORY_PATH="$2"; shift 2 ;;
    --json) JSON_OUTPUT=1; shift ;;
    -h|--help) sed -n '2,9p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) echo "unknown option: $1" >&2; exit 2 ;;
  esac
done

python3 - "$README_PATH" "$INVENTORY_PATH" "$JSON_OUTPUT" <<'PY'
import json
import re
import sys
from pathlib import Path

readme_path = Path(sys.argv[1])
inv_path = Path(sys.argv[2])
json_output = sys.argv[3] == "1"

for label, p in [("README", readme_path), ("inventory", inv_path)]:
    if not p.is_file():
        sys.stderr.write(f"error: {label} file not found: {p}\n")
        sys.exit(2)

inv = json.loads(inv_path.read_text(encoding="utf-8"))
unconsumed = {
    rec["key"]: rec
    for rec in inv.get("records", [])
    if rec.get("status") == "parsed_but_unconsumed"
}
consumed = {
    rec["key"]: rec
    for rec in inv.get("records", [])
    if rec.get("status") == "consumed"
}

# Extract all ```toml fenced blocks from README.md and track table + full dotted key paths
lines = readme_path.read_text(encoding="utf-8").splitlines()
in_toml = False
current_table = ""
documented_keys = []  # list of (lineno, dotted_key)

for lineno, raw in enumerate(lines, start=1):
    stripped = raw.strip()
    if stripped.startswith("```toml"):
        in_toml = True
        current_table = ""
        continue
    if in_toml and stripped.startswith("```"):
        in_toml = False
        current_table = ""
        continue
    if not in_toml or not stripped or stripped.startswith("#"):
        continue
    m_tbl = re.match(r"^\[+([A-Za-z0-9_.-]+)\]+", stripped)
    if m_tbl:
        current_table = m_tbl.group(1)
        documented_keys.append((lineno, current_table))
        continue
    m_kv = re.match(r"^([A-Za-z0-9_-]+)\s*=", stripped)
    if m_kv:
        key = m_kv.group(1)
        full = f"{current_table}.{key}" if current_table else key
        documented_keys.append((lineno, full))

violations = []
for lineno, full_key in documented_keys:
    for bad_key, rec in unconsumed.items():
        # Exact match (e.g. [safety.capabilities] or search.reranker_enabled)
        # or sub-key under an unconsumed table (e.g. safety.capabilities.allow_*).
        # Do NOT flag valid parent tables like [safety] or [ingest] unless the
        # documented key itself matches or falls under an unconsumed entry.
        if full_key == bad_key or full_key.startswith(bad_key + "."):
            violations.append({
                "readme_line": lineno,
                "readme_key": full_key,
                "unconsumed_inventory_key": bad_key,
                "tracking_bead": rec.get("tracking_bead"),
                "detail": rec.get("evidence"),
            })

ok = len(violations) == 0
payload = {
    "schema_version": "frankenterm.readme_config_wiring.v1",
    "ok": ok,
    "readme_keys_checked": len(documented_keys),
    "inventory_consumed_count": len(consumed),
    "inventory_unconsumed_count": len(unconsumed),
    "violation_count": len(violations),
    "violations": violations,
}

if json_output:
    print(json.dumps(payload, indent=2, sort_keys=True))
else:
    if ok:
        print(
            f"readme config wiring passed: readme_keys_checked={len(documented_keys)} "
            f"consumed_inventory={len(consumed)} unconsumed_inventory={len(unconsumed)} violations=0"
        )
    else:
        print(f"readme config wiring FAILED: {len(violations)} violation(s)")
        for v in violations:
            print(
                f"  FAIL {readme_path}:{v['readme_line']} key '{v['readme_key']}' "
                f"hits parsed_but_unconsumed '{v['unconsumed_inventory_key']}' ({v['tracking_bead']})"
            )

sys.exit(0 if ok else 1)
PY
