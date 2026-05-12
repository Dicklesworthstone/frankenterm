#!/usr/bin/env bash
# Validate proof_category lines for the ft-tf6g3 reality-check epic.
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TAXONOMY_PATH="${ROOT_DIR}/docs/proof-taxonomy.json"
BEADS_PATH="${ROOT_DIR}/.beads/issues.jsonl"
JSON_OUTPUT=0

usage() {
  cat <<EOF
Usage: $0 [--json] [--taxonomy <path>] [--beads <path>]

Checks every ft-tf6g3 parent/child bead for a proof_category line. Numeric
references must resolve to docs/proof-taxonomy.json category IDs. Non-proof
work may instead use one of the taxonomy's non_proof_classifications.
EOF
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --json) JSON_OUTPUT=1; shift ;;
    --taxonomy) TAXONOMY_PATH="${2:?--taxonomy requires a path}"; shift 2 ;;
    --beads) BEADS_PATH="${2:?--beads requires a path}"; shift 2 ;;
    -h|--help) usage; exit 0 ;;
    *) echo "unknown arg: $1" >&2; usage >&2; exit 2 ;;
  esac
done

[[ -f "$TAXONOMY_PATH" ]] || { echo "error: taxonomy not found: $TAXONOMY_PATH" >&2; exit 1; }
[[ -f "$BEADS_PATH" ]] || { echo "error: beads JSONL not found: $BEADS_PATH" >&2; exit 1; }
command -v python3 >/dev/null 2>&1 || { echo "error: python3 is required" >&2; exit 1; }

python3 - "$TAXONOMY_PATH" "$BEADS_PATH" "$JSON_OUTPUT" <<'PY'
import json
import re
import sys
from collections import Counter
from pathlib import Path

taxonomy_path = Path(sys.argv[1])
beads_path = Path(sys.argv[2])
json_output = sys.argv[3] == "1"

taxonomy = json.loads(taxonomy_path.read_text())
categories = taxonomy.get("categories", [])
category_by_id = {int(item["id"]): item for item in categories}
non_proof = {item["slug"].lower(): item for item in taxonomy.get("non_proof_classifications", [])}

issue_re = re.compile(r"^ft-tf6g3(?:\.(\d+))?$")
line_re = re.compile(r"^proof_category:\s*(.+)$", re.IGNORECASE | re.MULTILINE)
number_re = re.compile(r"(?<![\w.])(\d+)(?![\w.])")

issues = []
for lineno, line in enumerate(beads_path.read_text().splitlines(), start=1):
    if not line.strip():
        continue
    try:
        issue = json.loads(line)
    except json.JSONDecodeError as exc:
        raise SystemExit(f"{beads_path}:{lineno}: invalid JSON: {exc}") from exc
    issue_id = issue.get("id", "")
    if issue_re.match(issue_id):
        issues.append(issue)

violations = []
category_counts = Counter()
non_proof_counts = Counter()
checked = 0

for issue in sorted(issues, key=lambda item: [int(part) if part.isdigit() else part for part in item["id"].replace("ft-tf6g3.", "ft-tf6g3.0.").split(".")]):
    checked += 1
    issue_id = issue["id"]
    description = issue.get("description") or ""
    match = line_re.search(description)
    if not match:
        violations.append({
            "id": issue_id,
            "title": issue.get("title", ""),
            "kind": "missing",
            "message": "missing proof_category line"
        })
        continue

    value = match.group(1).strip()
    numbers = [int(item) for item in number_re.findall(value)]
    unknown = [item for item in numbers if item not in category_by_id]
    for item in numbers:
        if item in category_by_id:
            category_counts[str(item)] += 1

    lower_value = value.lower()
    matched_non_proof = [slug for slug in non_proof if re.search(rf"(?<![\w-]){re.escape(slug)}(?![\w-])", lower_value)]
    for slug in matched_non_proof:
        non_proof_counts[slug] += 1

    if unknown:
        violations.append({
            "id": issue_id,
            "title": issue.get("title", ""),
            "kind": "unknown_category",
            "message": f"unknown taxonomy id(s): {', '.join(str(item) for item in unknown)}",
            "proof_category": value
        })
        continue

    if not numbers and not matched_non_proof:
        violations.append({
            "id": issue_id,
            "title": issue.get("title", ""),
            "kind": "unclassified",
            "message": "proof_category line contains no taxonomy id or non-proof classification",
            "proof_category": value
        })

core_count = sum(1 for item in categories if item.get("bridge_plan_core") is True)
summary = {
    "schema_version": "1.0.0",
    "taxonomy_path": str(taxonomy_path),
    "beads_path": str(beads_path),
    "checked_issue_count": checked,
    "taxonomy_category_count": len(categories),
    "core_category_count": core_count,
    "extension_category_count": len(categories) - core_count,
    "non_proof_classification_count": len(non_proof),
    "category_counts": dict(sorted(category_counts.items(), key=lambda item: int(item[0]))),
    "non_proof_counts": dict(sorted(non_proof_counts.items())),
    "violation_count": len(violations),
    "violations": violations,
}

if json_output:
    print(json.dumps({"ok": not violations, "summary": summary}, indent=2, sort_keys=True))
else:
    if violations:
        print(f"FAIL: {len(violations)} proof taxonomy violation(s)")
        for violation in violations:
            print(f"  - {violation['id']}: {violation['message']}")
    else:
        print(f"OK: {checked} ft-tf6g3 bead(s) have valid proof_category lines")
        print(f"  taxonomy categories: {len(categories)} ({core_count} core, {len(categories) - core_count} extension)")
        print(f"  non-proof classifications: {len(non_proof)}")

sys.exit(1 if violations else 0)
PY
