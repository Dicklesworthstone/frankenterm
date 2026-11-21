#!/usr/bin/env bash
# scripts/check-readme-consistency.sh — fail-closed documentation truth &
# workspace-crate inventory guard (ft-mgy5t2.7).
#
# Checks:
#   1. Every workspace crate directory under `crates/*` (with Cargo.toml) is
#      referenced in AGENTS.md.
#   2. README.md does not claim distributed mode is "off by default" when
#      Cargo.toml enables `distributed` in default features.
#   3. README.md does not claim semantic search "requires ML embeddings" while
#      documenting zero-dependency hash embeddings.
#   4. README.md does not label RC2 (`ft-tf6g3`) as the "current round".
#   5. docs/perf/headline-claims.json does not contradict README.md's 1 MiB
#      delta overlap window with legacy `4KB overlap`.
#
# Usage:
#   scripts/check-readme-consistency.sh [--json] [--readme PATH] [--agents PATH]
#                                       [--claims PATH] [--crates-dir DIR]
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd -P)"
README_PATH="$ROOT/README.md"
AGENTS_PATH="$ROOT/AGENTS.md"
CLAIMS_PATH="$ROOT/docs/perf/headline-claims.json"
CRATES_DIR="$ROOT/crates"
JSON_OUTPUT=0

while [[ $# -gt 0 ]]; do
  case "$1" in
    --readme) README_PATH="$2"; shift 2 ;;
    --agents) AGENTS_PATH="$2"; shift 2 ;;
    --claims) CLAIMS_PATH="$2"; shift 2 ;;
    --crates-dir) CRATES_DIR="$2"; shift 2 ;;
    --json) JSON_OUTPUT=1; shift ;;
    -h|--help) sed -n '2,18p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) echo "unknown option: $1" >&2; exit 2 ;;
  esac
done

python3 - "$README_PATH" "$AGENTS_PATH" "$CLAIMS_PATH" "$CRATES_DIR" "$JSON_OUTPUT" <<'PY'
import json
import re
import sys
from pathlib import Path

readme_path = Path(sys.argv[1])
agents_path = Path(sys.argv[2])
claims_path = Path(sys.argv[3])
crates_dir = Path(sys.argv[4])
json_output = sys.argv[5] == "1"

for label, p in [("README", readme_path), ("AGENTS", agents_path), ("headline-claims", claims_path)]:
    if not p.is_file():
        sys.stderr.write(f"error: {label} file not found: {p}\n")
        sys.exit(2)
if not crates_dir.is_dir():
    sys.stderr.write(f"error: crates directory not found: {crates_dir}\n")
    sys.exit(2)

readme = readme_path.read_text(encoding="utf-8")
agents = agents_path.read_text(encoding="utf-8")
claims_raw = claims_path.read_text(encoding="utf-8")
claims = json.loads(claims_raw)

violations = []

# 1. Every crates/* workspace crate directory must be documented in AGENTS.md
crate_names = sorted(
    p.name for p in crates_dir.iterdir()
    if p.is_dir() and (p / "Cargo.toml").is_file()
)
missing_crates = [c for c in crate_names if c not in agents]
for c in missing_crates:
    violations.append({
        "rule": "agents_missing_workspace_crate",
        "file": str(agents_path),
        "detail": f"workspace crate 'crates/{c}' is not documented in AGENTS.md",
    })

# 2. No "off by default" distributed-mode contradiction in README.md
for idx, line in enumerate(readme.splitlines(), start=1):
    if re.search(r"distributed\s+mode.*off\s+by\s+default", line, re.IGNORECASE):
        violations.append({
            "rule": "readme_distributed_default_contradiction",
            "file": f"{readme_path}:{idx}",
            "detail": "README claims distributed mode is 'off by default' despite default Cargo feature",
        })

# 3. No "requires ML embeddings" contradiction in README.md
for idx, line in enumerate(readme.splitlines(), start=1):
    if re.search(r"requires\s+ML\s+embeddings", line, re.IGNORECASE):
        violations.append({
            "rule": "readme_semantic_ml_contradiction",
            "file": f"{readme_path}:{idx}",
            "detail": "README claims semantic search 'requires ML embeddings' contradicting hash-embedding production lane",
        })

# 4. No stale RC2 (ft-tf6g3) described as "current round"
for idx, line in enumerate(readme.splitlines(), start=1):
    if re.search(r"current\s+round\s*\(\s*ft-tf6g3", line, re.IGNORECASE):
        violations.append({
            "rule": "readme_stale_rc_current_round",
            "file": f"{readme_path}:{idx}",
            "detail": "README still describes RC2 (ft-tf6g3) as the current reality-check round",
        })

# 5. headline-claims.json capture_latency_p99 overlap must match README's 1 MiB (not stale 4KB)
for claim in claims.get("claims", []):
    if claim.get("id") == "capture_latency_p99":
        sentence = str(claim.get("readme_sentence", ""))
        wdesc = str(claim.get("workload", {}).get("description", ""))
        if re.search(r"\b4\s*KB\b", sentence, re.IGNORECASE) or re.search(r"\b4\s*KB\b", wdesc, re.IGNORECASE):
            violations.append({
                "rule": "headline_claims_stale_4kb_overlap",
                "file": str(claims_path),
                "detail": "headline-claims.json capture_latency_p99 still cites stale '4KB overlap' instead of bounded 1 MiB",
            })

ok = len(violations) == 0
payload = {
    "schema_version": "frankenterm.readme_consistency.v1",
    "ok": ok,
    "crates_checked": len(crate_names),
    "invariants_checked": [
        "agents_missing_workspace_crate",
        "readme_distributed_default_contradiction",
        "readme_semantic_ml_contradiction",
        "readme_stale_rc_current_round",
        "headline_claims_stale_4kb_overlap",
    ],
    "violation_count": len(violations),
    "violations": violations,
}

if json_output:
    print(json.dumps(payload, indent=2, sort_keys=True))
else:
    if ok:
        print(f"readme consistency passed: crates_checked={len(crate_names)} violations=0")
    else:
        print(f"readme consistency FAILED: {len(violations)} violation(s)")
        for v in violations:
            print(f"  FAIL [{v['rule']}] {v['file']}: {v['detail']}")

sys.exit(0 if ok else 1)
PY
