#!/usr/bin/env bash
set -euo pipefail

CHECKLIST_FILE="${1:-docs/e2e-integration-checklist.md}"
REGISTRY_FILE="${2:-scripts/e2e_test.sh}"

if [[ ! -f "$CHECKLIST_FILE" ]]; then
  echo "checklist missing: $CHECKLIST_FILE" >&2
  exit 1
fi
if [[ ! -f "$REGISTRY_FILE" ]]; then
  echo "registry script missing: $REGISTRY_FILE" >&2
  exit 1
fi

python3 - <<'PY' "$CHECKLIST_FILE" "$REGISTRY_FILE"
import re
import subprocess
import sys
from pathlib import Path

checklist_path = Path(sys.argv[1])
registry_path = Path(sys.argv[2])

checklist = checklist_path.read_text(encoding="utf-8")
registry = registry_path.read_text(encoding="utf-8")

# Extract scenario names referenced in checklist lines.
# Expected format: "Scenario(s): name[, name2] (notes...)"
scenario_refs = set()
for match in re.finditer(r"Scenario\(s\):([^\n]+)", checklist):
    tail = match.group(1)
    # Drop parenthetical notes.
    tail = re.sub(r"\([^)]*\)", "", tail)
    for part in tail.split(","):
        name = part.strip()
        if not name:
            continue
        # Only take the first token (scenario name).
        name = name.split()[0].strip().lower()
        if name == "none":
            continue
        scenario_refs.add(name)

registry_block_match = re.search(r"SCENARIO_REGISTRY=\(\n(.*?)\n\)", registry, re.S)
if not registry_block_match:
    print("SCENARIO_REGISTRY block not found in registry script")
    sys.exit(4)

registry_block = registry_block_match.group(1)
registry_entries = re.findall(r'"([^"]+)"', registry_block)

registry_names = set()
registry_errors = []
for entry in registry_entries:
    parts = entry.split("|")
    if len(parts) != 5:
        registry_errors.append(f"Invalid registry entry (expected 5 fields): {entry}")
        continue
    name, desc, default_flag, prereqs, why = [p.strip() for p in parts]
    if not name or not desc or not why:
        registry_errors.append(f"Registry entry missing required fields: {entry}")
    if default_flag not in ("true", "false"):
        registry_errors.append(f"Registry entry has invalid default flag: {entry}")
    if name in registry_names:
        registry_errors.append(f"Duplicate registry entry for scenario: {name}")
    registry_names.add(name)

if registry_errors:
    print("Registry format errors:")
    for err in registry_errors:
        print(f"  - {err}")
    sys.exit(2)

missing = sorted(scenario_refs - registry_names)
if missing:
    print("Checklist references scenarios not in SCENARIO_REGISTRY:")
    for name in missing:
        print(f"  - {name}")
    sys.exit(3)

run_funcs = set(re.findall(r"^\s*run_scenario_([a-z0-9_]+)\(\)", registry, re.M))
missing_impl = sorted(registry_names - run_funcs)
if missing_impl:
    print("Registry references scenarios without run_scenario_ implementation:")
    for name in missing_impl:
        print(f"  - {name}")
    sys.exit(5)

missing_from_registry = sorted(run_funcs - registry_names)
if missing_from_registry:
    print("run_scenario_ functions missing from SCENARIO_REGISTRY:")
    for name in missing_from_registry:
        print(f"  - {name}")
    sys.exit(6)

missing_case = []
for name in sorted(registry_names):
    if not re.search(rf"\n\s*{re.escape(name)}\)", registry):
        missing_case.append(name)
if missing_case:
    print("Registry scenarios missing from run_scenario case dispatch:")
    for name in missing_case:
        print(f"  - {name}")
    sys.exit(7)

# Optional: check bead IDs referenced in checklist exist in br.
bead_ids = sorted(set(re.findall(r"wa-[a-z0-9][a-z0-9.-]*", checklist)))

if bead_ids:
    if subprocess.call(["bash", "-lc", "command -v br >/dev/null 2>&1"]) == 0:
        missing_beads = []
        for bead_id in bead_ids:
            result = subprocess.call(["br", "show", bead_id, "--json"], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
            if result != 0:
                missing_beads.append(bead_id)
        if missing_beads:
            print("Checklist references bead IDs not found by br:")
            for bead_id in missing_beads:
                print(f"  - {bead_id}")
            sys.exit(8)
    else:
        print("br not available; skipping bead ID validation", file=sys.stderr)

print("OK: checklist and registry align; registry entries are complete")
PY
