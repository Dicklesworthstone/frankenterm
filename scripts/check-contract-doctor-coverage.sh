#!/usr/bin/env bash
# Contract-Doctor completeness oracle (W11.3b / ft-7h5da.13.6).
#
# Spec: docs/robot-contracts/contract-doctor-matrix.md (ft-7h5da.13.5).
# Fails closed if the Robot/MCP contract coverage matrix drifts from the typed
# registries, if a surfaced GAP loses its tracking bead, or if the known
# registry-completeness gap (G4) regresses.
#
# No Cargo: static inventory analysis over source, docs, and the MCP manifest.
# Exit zero proves inventory consistency only. --strict additionally rejects
# tracked partial dimensions; the DSR release gate must also execute the
# corresponding runtime tests under its retained source identity.
#
# Exit codes: 0 = all invariants hold; 1 = a contract-coverage invariant failed;
# 2 = a required input file is missing (treated as failure).
set -uo pipefail
STRICT=0
case "${1:-}" in
  "") ;;
  --strict) STRICT=1; shift ;;
  *) echo "unsupported Contract Doctor argument: $1" >&2; exit 2 ;;
esac
[[ $# -eq 0 ]] || { echo "unexpected Contract Doctor arguments" >&2; exit 2; }

# The script lives in <repo>/scripts/, so its parent dir is the repo root.
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT" || { echo "FATAL: cannot cd to repo root"; exit 2; }

REGISTRY="crates/frankenterm-core/src/robot_api_contracts.rs"
MATRIX="docs/robot-contracts/contract-doctor-matrix.md"
LEDGER="docs/robot-contracts/api-surface-coverage.md"
MCP_DISPATCH="crates/frankenterm-core/src/mcp.rs"

CATS='pane|search|events|workflow|rules|agent|accounts|reservations|mission|tx|replay|diagnostics|meta'

fail=0
note() { printf '  %s\n' "$1"; }
err()  { printf 'FAIL: %s\n' "$1"; fail=1; }

for f in "$REGISTRY" "$MATRIX" "$LEDGER" "$MCP_DISPATCH"; do
  [[ -f "$f" ]] || { echo "FATAL: missing required input: $f"; exit 2; }
done

echo "Contract-Doctor completeness oracle (ft-7h5da.13.6)"
echo "=================================================="

# --- Extract the typed registry: count entries in `const ALL: &[ApiSurface]` ---
all_start="$(grep -nE 'pub const ALL: ' "$REGISTRY" | head -1 | cut -d: -f1)"
all_end="$(awk -v s="$all_start" 'NR>s && /\];/{print NR; exit}' "$REGISTRY")"
REG_COUNT="$(awk -v s="$all_start" -v e="$all_end" 'NR>s && NR<e && /Self::/{c++} END{print c+0}' "$REGISTRY")"

# --- Extract surface command-names from the doctor matrix and the ledger ---
# Doctor matrix data rows:  | `surface` | <cat> | ...   (bare category)
matrix_surfaces="$(awk -F'|' -v cats="$CATS" '
  $0 ~ /^\| `[a-z0-9-]+` \|/ {
    c=$3; gsub(/[ `]/,"",c);
    if (c ~ ("^(" cats ")$")) { s=$2; gsub(/[ `]/,"",s); print s }
  }' "$MATRIX" | sort -u)"
# Ledger data rows:         | `surface` | `<cat>` | ...   (backticked category)
ledger_surfaces="$(awk -F'|' -v cats="$CATS" '
  $0 ~ /^\| `[a-z0-9-]+` \| `/ {
    c=$3; gsub(/[ `]/,"",c);
    if (c ~ ("^(" cats ")$")) { s=$2; gsub(/[ `]/,"",s); print s }
  }' "$LEDGER" | sort -u)"

MATRIX_COUNT="$(printf '%s\n' "$matrix_surfaces" | grep -c .)"
LEDGER_COUNT="$(printf '%s\n' "$ledger_surfaces" | grep -c .)"

# --- Check 1: registry count == matrix row count (a new ApiSurface needs a row) ---
echo "[1] registry tie: ApiSurface::ALL=$REG_COUNT vs matrix rows=$MATRIX_COUNT"
if [[ "$REG_COUNT" -ne "$MATRIX_COUNT" ]]; then
  err "ApiSurface::ALL has $REG_COUNT entries but the doctor matrix has $MATRIX_COUNT surface rows."
  note "A surface was added to/removed from the registry without updating $MATRIX."
fi

# --- Check 2: matrix surface set == ledger surface set (no drift either way) ---
echo "[2] completeness: doctor-matrix surfaces == registry-synced ledger surfaces"
missing_in_matrix="$(comm -23 <(printf '%s\n' "$ledger_surfaces") <(printf '%s\n' "$matrix_surfaces"))"
extra_in_matrix="$(comm -13 <(printf '%s\n' "$ledger_surfaces") <(printf '%s\n' "$matrix_surfaces"))"
if [[ -n "$missing_in_matrix" ]]; then
  err "surfaces in $LEDGER but MISSING from the doctor matrix:"; printf '%s\n' "$missing_in_matrix" | sed 's/^/      - /'
fi
if [[ -n "$extra_in_matrix" ]]; then
  err "surfaces in the doctor matrix not present in the registry ledger:"; printf '%s\n' "$extra_in_matrix" | sed 's/^/      - /'
fi
[[ -z "$missing_in_matrix$extra_in_matrix" ]] && note "ledger=$LEDGER_COUNT surfaces, all present in the doctor matrix."

# --- Check 3 (G4): policy-gated MCP mutation tools outside ApiSurface::ALL stay flagged ---
echo "[3] G4 registry gap: MCP mutation tools absent from ApiSurface::ALL are flagged"
for tool in wa.mission_pause wa.mission_resume wa.mission_abort; do
  if grep -q "\"$tool\"" "$MCP_DISPATCH"; then
    # The tool IS a real dispatched MCP tool; it must be acknowledged in the matrix G4 section.
    if ! grep -qF "$tool" "$MATRIX" && ! grep -qE 'mission_(pause|resume|abort)' "$MATRIX"; then
      err "$tool is a dispatched MCP mutation tool but is not flagged in $MATRIX (G4 regressed)."
    fi
  fi
done
note "G4 tracking checked; exact manifest-to-matrix dispatch union is verified below."

# --- Check 4: every surfaced GAP keeps a tracking reference ---
echo "[4] gap tracking: surfaced gaps still cite their tracking beads"
for bead in ft-6mmyp ft-5puf0; do
  grep -qF "$bead" "$MATRIX" || err "GAP SUMMARY no longer references tracking bead $bead."
done
note "G1 -> ft-6mmyp, G3 -> ft-5puf0 referenced."

echo "[5] six-dimension cells and registered MCP twins"
if ! python3 - "$MATRIX" crates/frankenterm-core/tests/fixtures/mcp_manifest.json "$STRICT" <<'PY'
import json
import re
import sys

matrix_path, manifest_path, strict = sys.argv[1:]
text = open(matrix_path, encoding="utf-8").read()
tools = {tool["name"] for tool in json.load(open(manifest_path, encoding="utf-8"))["tools"]}
dimensions = ("ENV", "PAR", "POL", "RED", "TOON", "ERR")
partials = {}
rows = []
for line in text.splitlines():
    if re.match(r"^\| `[a-z0-9-]+` \|", line):
        rows.append([cell.strip().strip("`") for cell in line.split("|")[1:-1]])
mcp_inventory = re.findall(r"^\| `(wa\.[a-z_]+)` \|", text, re.MULTILINE)

def errors_for(candidate_rows, supplemental=mcp_inventory):
    errors = []
    seen = set()
    mapped_tools = []
    for row in candidate_rows:
        if len(row) != 10:
            errors.append("dimension_count")
            continue
        surface, _, _, twin, *cells = row
        if surface in seen:
            errors.append("duplicate_surface")
        seen.add(surface)
        if twin != "none":
            twins = twin.split("/")
            prefix = twins[0].rsplit("_", 1)[0] + "_"
            names = [name if name.startswith("wa.") else prefix + name for name in twins]
            mapped_tools.extend(names)
            if any(name not in tools for name in names):
                errors.append(f"unregistered_mcp_twin:{surface}")
        for dimension, cell in zip(dimensions, cells):
            if cell not in ("✓", "~", "n/a"):
                errors.append(f"uncovered_cell:{surface}:{dimension}")
            if cell == "~" and (
                (surface, dimension) not in partials or partials[(surface, dimension)] not in text
            ):
                errors.append(f"untracked_partial:{surface}:{dimension}")
    inventory = mapped_tools + supplemental
    if len(inventory) != len(set(inventory)):
        errors.append("duplicate_mcp_registry_tool")
    if set(inventory) - tools:
        errors.append("unregistered_mcp_registry_tool")
    if tools - set(inventory):
        errors.append("missing_mcp_registry_tool:" + ",".join(sorted(tools - set(inventory))))
    return errors

errors = errors_for(rows)
if not rows:
    errors.append("empty_matrix")
if errors:
    raise SystemExit("FAIL: " + ", ".join(errors))
# Causal negatives exercise the exact live validator, without rewriting files.
for column, mutation, expected in (
    (4, "GAP", "uncovered_cell"),
    (4, "~", "untracked_partial"),
    (3, "wa.nonexistent_contract_doctor_tool", "unregistered_mcp_twin"),
):
    mutated = [row.copy() for row in rows]
    mutated[0][column] = mutation
    if not any(error.startswith(expected) for error in errors_for(mutated)):
        raise SystemExit("FAIL: matrix negative control did not reject " + expected)
if not mcp_inventory or not any(error.startswith("missing_mcp_registry_tool")
                              for error in errors_for(rows, mcp_inventory[1:])):
    raise SystemExit("FAIL: supplemental MCP omission negative control did not reject missing tool")
if "duplicate_mcp_registry_tool" not in errors_for(rows, mcp_inventory + mcp_inventory[:1]):
    raise SystemExit("FAIL: supplemental MCP duplicate negative control did not reject duplicate tool")
print(json.dumps({"surfaces": len(rows), "dimensions": list(dimensions),
                  "registered_mcp_tools": len(tools), "supplemental_mcp_tools": len(mcp_inventory),
                  "cells": len(rows) * len(dimensions),
                  "tracked_partial_cells": sum(row[4:].count("~") for row in rows),
                  "runtime_tests_executed": False}, sort_keys=True))
if strict == "1":
    unresolved = [f"{row[0]}:{dimension}" for row in rows
                  for dimension, cell in zip(dimensions, row[4:]) if cell == "~"]
    if unresolved:
        raise SystemExit("FAIL: stable Contract Doctor has incomplete dimensions: " + ", ".join(unresolved))
PY
then
  err "six-dimension coverage or MCP support declaration drifted"
fi

echo "=================================================="
if [[ "$fail" -eq 0 ]]; then
  echo "OK: Robot/MCP coverage inventory is consistent ($REG_COUNT surfaces); this static result is not a runtime or stable-release verdict."
  exit 0
fi
echo "DRIFT DETECTED: fix $MATRIX (or file/track the new gap) and re-run."
exit 1
