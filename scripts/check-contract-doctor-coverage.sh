#!/usr/bin/env bash
# Contract-Doctor completeness oracle (W11.3b / ft-7h5da.13.6).
#
# Spec: docs/robot-contracts/contract-doctor-matrix.md (ft-7h5da.13.5).
# Fails closed if the Robot/MCP contract coverage matrix drifts from the typed
# registries, if a surfaced GAP loses its tracking bead, or if the known
# registry-completeness gap (G4) regresses.
#
# ZERO RCH / ZERO cargo: pure static analysis (grep/awk over source + docs).
# Run it directly; a clean exit 0 IS the proof. Intended to be wired into CI by
# ft-7h5da.13.7 (the unified Contract-Doctor verdict).
#
# Exit codes: 0 = all invariants hold; 1 = a contract-coverage invariant failed;
# 2 = a required input file is missing (treated as failure).
set -uo pipefail

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
note "mission_pause/resume/abort dispatched in $MCP_DISPATCH and flagged as G4 in the matrix."

# --- Check 4: every surfaced GAP keeps a tracking reference ---
echo "[4] gap tracking: surfaced gaps still cite their tracking beads"
for bead in ft-6mmyp ft-5puf0; do
  grep -qF "$bead" "$MATRIX" || err "GAP SUMMARY no longer references tracking bead $bead."
done
note "G1 -> ft-6mmyp, G3 -> ft-5puf0 referenced."

echo "=================================================="
if [[ "$fail" -eq 0 ]]; then
  echo "OK: Robot/MCP contract coverage is complete and gaps are tracked ($REG_COUNT surfaces)."
  exit 0
fi
echo "DRIFT DETECTED: fix $MATRIX (or file/track the new gap) and re-run."
exit 1
