#!/usr/bin/env bash
# scripts/beads-closeout-audit.sh — strictly read-only Beads closeout & WIP/P0
# health auditor (ft-mgy5t2.13 / RC4.G99; schema: beads.closeout_audit.v1).
#
# Classifies every unfinished `in_progress` bead into:
#   - proof_complete_unclosed : comment/notes cite commit + qualifying receipt/pass
#   - blocked_on_infra        : comment/notes cite unhealthy RCH/DSR worker or local fallback
#   - stale_abandoned         : idle > stale_days (default 14) without recent commit linkage
#   - active                  : updated within stale_days without completion/infra blocker
#
# Usage:
#   scripts/beads-closeout-audit.sh [--beads PATH] [--as-of YYYY-MM-DD]
#     [--stale-days N] [--p0-budget N] [--enforce-budget] [--json]
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd -P)"
BEADS_PATH="$ROOT/.beads/issues.jsonl"
AS_OF="$(date -u +%Y-%m-%d)"
STALE_DAYS=14
P0_BUDGET=25
ENFORCE_BUDGET=0
JSON_OUTPUT=0

while [[ $# -gt 0 ]]; do
  case "$1" in
    --beads) BEADS_PATH="$2"; shift 2 ;;
    --as-of) AS_OF="$2"; shift 2 ;;
    --stale-days) STALE_DAYS="$2"; shift 2 ;;
    --p0-budget) P0_BUDGET="$2"; shift 2 ;;
    --enforce-budget) ENFORCE_BUDGET=1; shift ;;
    --json) JSON_OUTPUT=1; shift ;;
    -h|--help) sed -n '2,15p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) echo "unknown option: $1" >&2; exit 2 ;;
  esac
done

[[ -f "$BEADS_PATH" ]] || { echo "error: beads JSONL not found: $BEADS_PATH" >&2; exit 2; }

python3 - "$BEADS_PATH" "$AS_OF" "$STALE_DAYS" "$P0_BUDGET" "$ENFORCE_BUDGET" "$JSON_OUTPUT" <<'PY'
import datetime
import json
import re
import sys
from pathlib import Path

beads_path = Path(sys.argv[1])
as_of_raw = sys.argv[2]
stale_days = int(sys.argv[3])
p0_budget = int(sys.argv[4])
enforce_budget = sys.argv[5] == "1"
json_output = sys.argv[6] == "1"

as_of = datetime.datetime.strptime(as_of_raw, "%Y-%m-%d").replace(tzinfo=datetime.timezone.utc) + datetime.timedelta(days=1)

PROOF_COMPLETE_RE = re.compile(
    r"(all\s+\d+\s+tests?\s+pass|passed\s+\d+/\d+|verified\b.*receipt|receipt\.json|READY_FOR_DISTRIBUTION|"
    r"commit(?:ted)?\s+(?:in\s+)?[0-9a-f]{7,40}.*(?:pass|verified|receipt)|remote\s+RCH.*pass)",
    re.IGNORECASE,
)
INFRA_BLOCKED_RE = re.compile(
    r"(RCH-E\d+|RCH_SOURCE_TOPOLOGY_INCOMPATIBLE|no admissible workers|local fallback|unhealthy\s+(?:DSR|RCH|worker)|"
    r"disk_free_below_critical|remote_build_disk_budget_exceeded|worker.*offline|pending successful matching DSR)",
    re.IGNORECASE,
)


def parse_ts(raw: str | None):
    if not raw:
        return None
    try:
        dt = datetime.datetime.fromisoformat(raw.replace("Z", "+00:00"))
        return dt if dt.tzinfo else dt.replace(tzinfo=datetime.timezone.utc)
    except ValueError:
        return None


issues = []
for line in beads_path.read_text(encoding="utf-8").splitlines():
    if not line.strip():
        continue
    issues.append(json.loads(line))

# Compute unblocking fan-out (how many unfinished issues depend on each issue)
unblocks_count: dict[str, int] = {}
unfinished = [i for i in issues if i.get("status") not in ("closed", "tombstone")]
for i in unfinished:
    for dep in i.get("dependencies") or []:
        if dep.get("type") == "parent-child":
            continue
        tgt = dep.get("depends_on_id")
        if tgt:
            unblocks_count[tgt] = unblocks_count.get(tgt, 0) + 1

open_p0 = [i["id"] for i in unfinished if i.get("priority") == 0]
buckets: dict[str, list[dict]] = {
    "proof_complete_unclosed": [],
    "blocked_on_infra": [],
    "stale_abandoned": [],
    "active": [],
}

for i in issues:
    if i.get("status") != "in_progress":
        continue
    iid = i["id"]
    comments_txt = "\n".join(str(c.get("text") or "") for c in (i.get("comments") or []))
    notes_txt = str(i.get("notes") or "")
    combined = f"{notes_txt}\n{comments_txt}"
    ts_candidates = [parse_ts(i.get("updated_at")), parse_ts(i.get("created_at"))]
    for c in i.get("comments") or []:
        ts_candidates.append(parse_ts(c.get("created_at")))
    last_ts = max((t for t in ts_candidates if t is not None), default=None)
    idle_days = max(0, int((as_of - last_ts).total_seconds() // 86400)) if last_ts else 9999

    if INFRA_BLOCKED_RE.search(combined):
        cls = "blocked_on_infra"
    elif PROOF_COMPLETE_RE.search(combined):
        cls = "proof_complete_unclosed"
    elif idle_days > stale_days:
        cls = "stale_abandoned"
    else:
        cls = "active"

    buckets[cls].append({
        "id": iid,
        "priority": i.get("priority"),
        "idle_days": idle_days,
        "unblocks": unblocks_count.get(iid, 0),
        "title": i.get("title", ""),
    })

for k in buckets:
    buckets[k].sort(key=lambda r: (-r["unblocks"], r["priority"] if r["priority"] is not None else 9, -r["idle_days"], r["id"]))

violations = []
if len(open_p0) > p0_budget:
    violations.append(f"p0_budget_exceeded: {len(open_p0)} > {p0_budget}")
if len(buckets["stale_abandoned"]) > 0:
    violations.append(f"stale_in_progress_gt_{stale_days}d: {len(buckets['stale_abandoned'])} > 0")

ok = (len(violations) == 0) if enforce_budget else True
payload = {
    "schema_version": "beads.closeout_audit.v1",
    "as_of": as_of_raw,
    "read_only": True,
    "total_issues": len(issues),
    "unfinished_count": len(unfinished),
    "in_progress_count": sum(len(v) for v in buckets.values()),
    "open_p0_count": len(open_p0),
    "p0_budget": p0_budget,
    "stale_days_threshold": stale_days,
    "counts": {k: len(v) for k, v in buckets.items()},
    "budget_violations": violations,
    "ok": ok,
    "top_proof_complete_unclosed": buckets["proof_complete_unclosed"][:25],
    "top_blocked_on_infra": buckets["blocked_on_infra"][:15],
    "top_stale_abandoned": buckets["stale_abandoned"][:25],
}

if json_output:
    print(json.dumps(payload, indent=2, sort_keys=True))
else:
    print(
        f"beads closeout audit (as_of={as_of_raw}): unfinished={len(unfinished)} "
        f"in_progress={payload['in_progress_count']} open_p0={len(open_p0)}/{p0_budget} "
        f"proof_complete_unclosed={len(buckets['proof_complete_unclosed'])} "
        f"blocked_on_infra={len(buckets['blocked_on_infra'])} "
        f"stale_abandoned={len(buckets['stale_abandoned'])} active={len(buckets['active'])}"
    )
    for v in violations:
        print(f"  VIOLATION: {v}")

sys.exit(0 if ok else 1)
PY
