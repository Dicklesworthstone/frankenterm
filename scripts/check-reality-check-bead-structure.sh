#!/usr/bin/env bash
# Validate the ft-tf6g3 reality-check bead structure contract.
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BEADS_PATH="${ROOT_DIR}/.beads/issues.jsonl"
TAXONOMY_PATH="${ROOT_DIR}/docs/proof-taxonomy.json"
EPIC_ID="ft-tf6g3"
STRICT_CREATED_AFTER="2026-05-12T18:37:00Z"
JSON_OUTPUT=0
STRICT_ALL=0
REPORT_PATH=""

usage() {
  cat <<EOF
Usage: $0 [options]

Options:
  --beads PATH                 Beads JSONL path (default: .beads/issues.jsonl)
  --taxonomy PATH              Proof taxonomy JSON path (default: docs/proof-taxonomy.json)
  --epic-id ID                 Epic id prefix to audit (default: ft-tf6g3)
  --strict-created-after TS    Treat matching beads created at/after TS as hard errors
  --strict-all                 Treat all matching child beads as hard errors
  --write-report PATH          Write a markdown violation report
  --json                       Emit machine-readable JSON
  -h, --help                   Show this help
EOF
}

require_arg() {
  local flag="$1"
  local value="${2-}"
  if [[ -z "$value" || "$value" == --* ]]; then
    echo "error: $flag requires a value" >&2
    usage >&2
    exit 2
  fi
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --beads) require_arg "$1" "${2-}"; BEADS_PATH="$2"; shift 2 ;;
    --taxonomy) require_arg "$1" "${2-}"; TAXONOMY_PATH="$2"; shift 2 ;;
    --epic-id) require_arg "$1" "${2-}"; EPIC_ID="$2"; shift 2 ;;
    --strict-created-after) require_arg "$1" "${2-}"; STRICT_CREATED_AFTER="$2"; shift 2 ;;
    --strict-all) STRICT_ALL=1; shift ;;
    --write-report) require_arg "$1" "${2-}"; REPORT_PATH="$2"; shift 2 ;;
    --json) JSON_OUTPUT=1; shift ;;
    -h|--help) usage; exit 0 ;;
    *) echo "unknown arg: $1" >&2; usage >&2; exit 2 ;;
  esac
done

[[ -f "$BEADS_PATH" ]] || { echo "error: beads JSONL not found: $BEADS_PATH" >&2; exit 1; }
[[ -f "$TAXONOMY_PATH" ]] || { echo "error: taxonomy JSON not found: $TAXONOMY_PATH" >&2; exit 1; }
command -v python3 >/dev/null 2>&1 || { echo "error: python3 is required" >&2; exit 1; }

python3 - "$BEADS_PATH" "$TAXONOMY_PATH" "$EPIC_ID" "$STRICT_CREATED_AFTER" "$STRICT_ALL" "$JSON_OUTPUT" "$REPORT_PATH" <<'PY'
import json
import re
import sys
from datetime import datetime, timezone
from pathlib import Path

beads_path = Path(sys.argv[1])
taxonomy_path = Path(sys.argv[2])
epic_id = sys.argv[3]
strict_created_after_raw = sys.argv[4]
strict_all = sys.argv[5] == "1"
json_output = sys.argv[6] == "1"
report_path = Path(sys.argv[7]) if sys.argv[7] else None

required_description_sections = {
    "background": re.compile(r"\bbackground\b", re.IGNORECASE),
    "why_this_matters": re.compile(r"\bwhy\s+this\s+matters\b", re.IGNORECASE),
    "acceptance_criteria": re.compile(r"\bacceptance\s+criteria\b", re.IGNORECASE),
    "references": re.compile(r"\breferences?\b", re.IGNORECASE),
}
required_notes_sections = {
    "test_companion": re.compile(r"\btest\s+companion\b", re.IGNORECASE),
    "operator_surface": re.compile(r"\boperator\s+surface\b", re.IGNORECASE),
    "degradation_behavior": re.compile(r"\b(degradation\s+behavior|failure\s+mode)\b", re.IGNORECASE),
    "proof_category_section": re.compile(r"\bproof\s+category\b", re.IGNORECASE),
}
proof_category_re = re.compile(r"^proof_category:\s*(.+)$", re.IGNORECASE | re.MULTILINE)
number_re = re.compile(r"(?<![\w.])(\d+)(?![\w.])")
evidence_comment_re = re.compile(
    r"(G55 affected-bead audit|artifact-present|artifact path|verified|verification|"
    r"proof:|validated|rch|ci|command|scripts/|docs/|crates/|tests/)",
    re.IGNORECASE,
)
zero_width_re = re.compile("[\u200b\u200c\u200d\ufeff]")
word_re = re.compile(r"\b[\w'-]+\b", re.UNICODE)
foreign_language_re = re.compile(
    r"\b(contexte|pourquoi|crit[eè]res?|références?|preuve|op[eé]rateur)\b",
    re.IGNORECASE,
)
section_header_patterns = {
    "background": re.compile(r"(?im)^\s*(?:#{1,6}\s*)?background\b"),
    "why_this_matters": re.compile(r"(?im)^\s*(?:#{1,6}\s*)?why\s+this\s+matters\b"),
    "acceptance_criteria": re.compile(r"(?im)^\s*(?:#{1,6}\s*)?acceptance\s+criteria\b"),
    "references": re.compile(r"(?im)^\s*(?:#{1,6}\s*)?references?\b"),
    "test_companion": re.compile(r"(?im)^\s*(?:#{1,6}\s*)?test\s+companion\b"),
    "operator_surface": re.compile(r"(?im)^\s*(?:#{1,6}\s*)?operator\s+surface\b"),
    "degradation_behavior": re.compile(r"(?im)^\s*(?:#{1,6}\s*)?(?:degradation\s+behavior|failure\s+mode)\b"),
    "proof_category_section": re.compile(r"(?im)^\s*(?:#{1,6}\s*)?proof\s+category\b"),
}


def parse_ts(raw):
    if not raw:
        return None
    value = raw.replace("Z", "+00:00")
    try:
        parsed = datetime.fromisoformat(value)
    except ValueError:
        return None
    if parsed.tzinfo is None:
        parsed = parsed.replace(tzinfo=timezone.utc)
    return parsed.astimezone(timezone.utc)


strict_created_after = parse_ts(strict_created_after_raw)

taxonomy = json.loads(taxonomy_path.read_text())
category_ids = {int(item["id"]) for item in taxonomy.get("categories", [])}
non_proof = {
    item["slug"].lower()
    for item in taxonomy.get("non_proof_classifications", [])
}

issues = []
for lineno, line in enumerate(beads_path.read_text().splitlines(), start=1):
    if not line.strip():
        continue
    try:
        issue = json.loads(line)
    except json.JSONDecodeError as exc:
        raise SystemExit(f"{beads_path}:{lineno}: invalid JSON: {exc}") from exc
    issue_id = issue.get("id", "")
    if issue_id == epic_id or issue_id.startswith(f"{epic_id}."):
        issues.append(issue)


def sort_key(issue):
    parts = re.split(r"([0-9]+)", issue.get("id", ""))
    return [int(part) if part.isdigit() else part for part in parts]


def is_child(issue):
    return issue.get("id", "") != epic_id


def strict_structure(issue):
    if not is_child(issue):
        return False
    if strict_all:
        return True
    created_at = parse_ts(issue.get("created_at", ""))
    return bool(strict_created_after and created_at and created_at >= strict_created_after)


def normalized_text(value):
    return zero_width_re.sub("", value or "")


def likely_foreign_language(value):
    return bool(foreign_language_re.search(value or ""))


violations = []
checked = 0
strict_checked = 0
closed_checked = 0

for issue in sorted(issues, key=sort_key):
    checked += 1
    issue_id = issue["id"]
    title = issue.get("title", "")
    description = issue.get("description") or ""
    notes = issue.get("notes") or ""
    normalized_description = normalized_text(description)
    normalized_notes = normalized_text(notes)
    body = f"{normalized_description}\n{normalized_notes}"
    hard_structure = strict_structure(issue)
    if hard_structure:
        strict_checked += 1

    description_missing = [
        name
        for name, pattern in required_description_sections.items()
        if not pattern.search(normalized_description)
    ]
    notes_missing = [
        name
        for name, pattern in required_notes_sections.items()
        if not pattern.search(body)
    ]
    missing_sections = description_missing + notes_missing
    for section in missing_sections:
        parse_warning = section in description_missing and likely_foreign_language(normalized_description)
        violations.append({
            "id": issue_id,
            "title": title,
            "severity": "warning" if parse_warning or not hard_structure else "error",
            "kind": "parse_warning" if parse_warning else "missing_section",
            "section": section,
            "message": (
                f"foreign-language or unusual formatting needs reviewer parse check: {section}"
                if parse_warning else
                f"missing required reality-check section: {section}"
            ),
        })

    if hard_structure and is_child(issue):
        description_words = len(word_re.findall(normalized_description))
        if description_words < 8:
            violations.append({
                "id": issue_id,
                "title": title,
                "severity": "error",
                "kind": "degenerate_description",
                "message": f"description is too short for reviewer-grade reality-check context ({description_words} words)",
            })
        if not normalized_notes.strip() and notes_missing:
            violations.append({
                "id": issue_id,
                "title": title,
                "severity": "error",
                "kind": "missing_notes",
                "message": "notes field is null or empty",
            })

    for section, pattern in section_header_patterns.items():
        hits = pattern.findall(body)
        if len(hits) > 1:
            violations.append({
                "id": issue_id,
                "title": title,
                "severity": "warning",
                "kind": "duplicate_section_header",
                "section": section,
                "message": f"duplicate reality-check section header: {section}",
            })

    proof_match = proof_category_re.search(body)
    if not proof_match:
        violations.append({
            "id": issue_id,
            "title": title,
            "severity": "error",
            "kind": "missing_proof_category",
            "message": "missing machine-readable proof_category line",
        })
    else:
        value = proof_match.group(1).strip()
        numbers = [int(item) for item in number_re.findall(value)]
        unknown = [item for item in numbers if item not in category_ids]
        lower_value = value.lower()
        matched_non_proof = [
            slug for slug in non_proof
            if re.search(rf"(?<![\w-]){re.escape(slug)}(?![\w-])", lower_value)
        ]
        if unknown:
            violations.append({
                "id": issue_id,
                "title": title,
                "severity": "error",
                "kind": "unknown_proof_category",
                "proof_category": value,
                "message": "unknown proof taxonomy id(s): " + ", ".join(str(item) for item in unknown),
            })
        elif not numbers and not matched_non_proof:
            violations.append({
                "id": issue_id,
                "title": title,
                "severity": "error",
                "kind": "unclassified_proof_category",
                "proof_category": value,
                "message": "proof_category contains no taxonomy id or non-proof classification",
            })

    if issue.get("status") == "closed" and is_child(issue):
        closed_checked += 1
        comments = issue.get("comments") or []
        if not any(evidence_comment_re.search(str(comment.get("text") or "")) for comment in comments):
            violations.append({
                "id": issue_id,
                "title": title,
                "severity": "error",
                "kind": "missing_closeout_evidence_comment",
                "message": "closed reality-check bead lacks evidence-bearing closeout/audit comment",
            })

error_count = sum(1 for item in violations if item["severity"] == "error")
warning_count = sum(1 for item in violations if item["severity"] == "warning")
summary = {
    "schema_version": "reality_check.bead_structure.summary.v1",
    "epic_id": epic_id,
    "beads_path": str(beads_path),
    "taxonomy_path": str(taxonomy_path),
    "checked_issue_count": checked,
    "strict_structure_checked_count": strict_checked,
    "closed_checked_count": closed_checked,
    "error_count": error_count,
    "warning_count": warning_count,
    "violation_count": len(violations),
}
payload = {
    "ok": error_count == 0,
    "summary": summary,
    "violations": violations,
}

if report_path:
    report_path.parent.mkdir(parents=True, exist_ok=True)
    lines = [
        "# Reality-check bead structure violations",
        "",
        f"- Epic: `{epic_id}`",
        f"- Checked issues: {checked}",
        f"- Strict-structure checked: {strict_checked}",
        f"- Closed checked: {closed_checked}",
        f"- Errors: {error_count}",
        f"- Warnings: {warning_count}",
        "",
    ]
    if violations:
        lines.extend(["| Severity | Bead | Kind | Detail |", "|---|---|---|---|"])
        for item in violations:
            detail = item.get("message", "")
            if "section" in item:
                detail = f"{detail} ({item['section']})"
            lines.append(
                f"| {item['severity']} | `{item['id']}` | {item['kind']} | {detail.replace('|', '/') } |"
            )
    else:
        lines.append("No structure violations found.")
    report_path.write_text("\n".join(lines) + "\n")

if json_output:
    print(json.dumps(payload, indent=2, sort_keys=True))
else:
    status = "passed" if error_count == 0 else "failed"
    print(
        f"reality-check bead structure {status}: "
        f"errors={error_count} warnings={warning_count} checked={checked} strict={strict_checked}"
    )
    if report_path:
        print(f"report: {report_path}")

sys.exit(0 if error_count == 0 else 1)
PY
