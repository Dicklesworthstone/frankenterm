#!/usr/bin/env bash
# Append to a Beads notes field without losing prior rounds.
set -euo pipefail

MAX_BYTES="${SAFE_BR_NOTES_MAX_BYTES:-1048576}"
DATE_STAMP="${SAFE_BR_UPDATE_NOTES_DATE:-$(date -u +%F)}"
DRY_RUN=0
BODY_FILE=""
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"

usage() {
  cat <<'EOF'
Usage:
  scripts/safe-br-update-notes.sh [--dry-run] [--body-file PATH] <bead> <new-section-h2-header> [new-section-body]
  scripts/safe-br-update-notes.sh --check-raw-replacement <existing-notes-file> <candidate-notes-file>
  scripts/safe-br-update-notes.sh --audit-notes-regressions [--json] [--epic-id ID]

The normal mode reads the current notes with `br show`, appends a dated H2
section, then writes the full concatenated notes with one `br update --notes`
call. If the exact section already exists, the command is a no-op. If the same
header exists with different body text, the new header gets a numeric suffix.

Environment:
  SAFE_BR_NOTES_MAX_BYTES       Maximum final notes size, default 1048576.
  SAFE_BR_UPDATE_NOTES_DATE     Override the UTC date prefix for tests.
EOF
}

die() {
  echo "safe-br-update-notes: error: $*" >&2
  exit 2
}

h2_count() {
  awk 'BEGIN { count = 0 } /^##[[:space:]]/ { count++ } END { print count + 0 }'
}

normalize_header() {
  local raw="$1"
  if [[ "$raw" =~ ^##[[:space:]] ]]; then
    printf '%s\n' "$raw"
  else
    printf '## %s - %s\n' "$DATE_STAMP" "$raw"
  fi
}

notes_bytes() {
  wc -c | tr -d '[:space:]'
}

contains_line() {
  local needle="$1"
  grep -Fqx -- "$needle"
}

check_raw_replacement() {
  local existing_path="$1"
  local candidate_path="$2"
  [[ -e "$existing_path" ]] || die "existing notes file not found: $existing_path"
  [[ -e "$candidate_path" ]] || die "candidate notes file not found: $candidate_path"

  local existing candidate existing_h2 candidate_h2 warned=0
  existing="$(cat "$existing_path")"
  candidate="$(cat "$candidate_path")"
  existing_h2="$(printf '%s\n' "$existing" | h2_count)"
  candidate_h2="$(printf '%s\n' "$candidate" | h2_count)"

  if (( existing_h2 > 1 && candidate_h2 < existing_h2 )); then
    echo "warning: raw br update --notes would replace ${existing_h2} H2 sections with ${candidate_h2}; use safe-br-update-notes instead" >&2
    warned=1
  fi

  for required in "Test companion" "Operator surface" "Degradation behavior" "Proof category"; do
    if grep -Fqi -- "$required" <<<"$existing" && ! grep -Fqi -- "$required" <<<"$candidate"; then
      echo "warning: raw br update --notes candidate drops required section: $required" >&2
      warned=1
    fi
  done

  if (( warned )); then
    return 4
  fi
}

audit_notes_regressions() {
  local json_output=0
  local epic_id="ft-tf6g3"
  local beads_path="${REPO_ROOT}/.beads/issues.jsonl"
  local bridge_plan="${REPO_ROOT}/docs/reality-check-bridge-plan-2026-05-12.md"

  while [[ $# -gt 0 ]]; do
    case "$1" in
      --json) json_output=1; shift ;;
      --epic-id) epic_id="${2:?--epic-id requires an id}"; shift 2 ;;
      --beads) beads_path="${2:?--beads requires a path}"; shift 2 ;;
      --bridge-plan) bridge_plan="${2:?--bridge-plan requires a path}"; shift 2 ;;
      *) die "unknown audit option: $1" ;;
    esac
  done

  [[ -f "$beads_path" ]] || die "beads JSONL not found: $beads_path"
  command -v python3 >/dev/null 2>&1 || die "python3 is required for audit mode"

  python3 - "$beads_path" "$bridge_plan" "$epic_id" "$json_output" <<'PY'
import json
import sys
from pathlib import Path

beads_path = Path(sys.argv[1])
bridge_plan = Path(sys.argv[2])
epic_id = sys.argv[3]
json_output = sys.argv[4] == "1"

required = {
    "test_companion": ("test companion",),
    "operator_surface": ("operator surface", "operator-surface"),
    "degradation_behavior": ("degradation behavior", "failure mode"),
    "proof_category": ("proof category", "proof_category:", "proof-artifact category"),
}

def has_section(text, names):
    lowered = text.lower()
    return any(name in lowered for name in names)

bridge_text = bridge_plan.read_text() if bridge_plan.exists() else ""
bridge_declares = all(has_section(bridge_text, names) for names in required.values())

checked = 0
issues = []
prefix = f"{epic_id}."
for lineno, line in enumerate(beads_path.read_text().splitlines(), start=1):
    if not line.strip():
        continue
    issue = json.loads(line)
    issue_id = issue.get("id", "")
    if issue_id != epic_id and not issue_id.startswith(prefix):
        continue
    checked += 1
    notes = issue.get("notes") or ""
    notes_lower = notes.lower()
    h2_count = sum(1 for row in notes.splitlines() if row.startswith("## "))
    restored = "restored" in notes_lower
    overwrite = "overwrite" in notes_lower
    if h2_count <= 1 and not restored and not overwrite:
        continue
    body = f"{issue.get('description') or ''}\n{notes}"
    missing = [
        name for name, aliases in required.items()
        if not has_section(body, aliases)
    ]
    issues.append({
        "id": issue_id,
        "title": issue.get("title", ""),
        "status": issue.get("status", ""),
        "h2_section_count": h2_count,
        "restored_marker_present": restored,
        "overwrite_marker_present": overwrite,
        "missing_sections": missing,
        "regression": bool(missing),
    })

regressions = sum(1 for issue in issues if issue["regression"])
payload = {
    "schema_version": "safe_br_update_notes.audit_notes_regressions.v1",
    "ok": regressions == 0,
    "epic_id": epic_id,
    "beads_path": str(beads_path),
    "bridge_plan": str(bridge_plan),
    "bridge_plan_present": bridge_plan.exists(),
    "bridge_plan_declares_required_sections": bridge_declares,
    "checked_issue_count": checked,
    "multi_round_issue_count": len(issues),
    "regression_count": regressions,
    "issues": issues,
}

if json_output:
    print(json.dumps(payload, indent=2, sort_keys=True))
else:
    status = "passed" if regressions == 0 else "failed"
    print(
        f"reality-check notes regression audit {status}: "
        f"regressions={regressions} multi_round={len(issues)} checked={checked}"
    )
    print(f"bridge plan: {bridge_plan}")
    for issue in issues:
        if issue["regression"]:
            print(f"- {issue['id']} regression: missing {', '.join(issue['missing_sections'])}")
        else:
            print(
                f"- {issue['id']} ok: h2_sections={issue['h2_section_count']} "
                f"restored_marker={str(issue['restored_marker_present']).lower()} "
                f"overwrite_marker={str(issue['overwrite_marker_present']).lower()}"
            )

sys.exit(0 if regressions == 0 else 1)
PY
}

append_notes() {
  local bead="$1"
  local raw_header="$2"
  local body="$3"

  command -v br >/dev/null 2>&1 || die "br is required"
  command -v jq >/dev/null 2>&1 || die "jq is required"

  [[ -n "$bead" ]] || die "bead id is required"
  [[ -n "$raw_header" ]] || die "new section H2 header is required"
  [[ -n "$body" ]] || die "new section body is required"

  local existing_json existing_notes header section base_header suffix new_notes byte_count
  existing_json="$(br show "$bead" --json)"
  existing_notes="$(jq -r 'if type == "array" then .[0].notes // "" else .notes // "" end' <<<"$existing_json")"

  header="$(normalize_header "$raw_header")"
  section="${header}"$'\n\n'"${body}"

  if [[ "$existing_notes" == *"$section"* ]]; then
    echo "safe-br-update-notes: unchanged; section already present for $bead"
    return 0
  fi

  base_header="$header"
  suffix=2
  while printf '%s\n' "$existing_notes" | contains_line "$header"; do
    header="${base_header} (${suffix})"
    section="${header}"$'\n\n'"${body}"
    suffix=$((suffix + 1))
  done

  if [[ -n "$existing_notes" ]]; then
    new_notes="${existing_notes%$'\n'}"$'\n\n'"${section}"$'\n'
  else
    new_notes="${section}"$'\n'
  fi

  byte_count="$(printf '%s' "$new_notes" | notes_bytes)"
  if (( byte_count > MAX_BYTES )); then
    echo "notes-overflow: final notes would be ${byte_count} bytes, limit is ${MAX_BYTES}; split the new material into Beads comments instead" >&2
    return 3
  fi

  if (( DRY_RUN )); then
    printf '%s' "$new_notes"
    return 0
  fi

  br update "$bead" --notes "$new_notes"
  echo "safe-br-update-notes: appended notes section to $bead (${byte_count} bytes)"
}

if [[ $# -eq 0 ]]; then
  usage >&2
  exit 2
fi

if [[ "${1:-}" == "--check-raw-replacement" ]]; then
  [[ $# -eq 3 ]] || die "--check-raw-replacement requires existing and candidate notes files"
  check_raw_replacement "$2" "$3"
  exit $?
fi

if [[ "${1:-}" == "--audit-notes-regressions" ]]; then
  shift
  audit_notes_regressions "$@"
  exit $?
fi

while [[ $# -gt 0 ]]; do
  case "$1" in
    --dry-run)
      DRY_RUN=1
      shift
      ;;
    --body-file)
      BODY_FILE="${2:?--body-file requires a path}"
      shift 2
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    --)
      shift
      break
      ;;
    -*)
      die "unknown option: $1"
      ;;
    *)
      break
      ;;
  esac
done

[[ $# -ge 2 ]] || die "expected <bead> and <new-section-h2-header>"
BEAD_ID="$1"
HEADER_ARG="$2"
shift 2

if [[ -n "$BODY_FILE" ]]; then
  [[ $# -eq 0 ]] || die "do not pass body text when --body-file is used"
  [[ -f "$BODY_FILE" ]] || die "body file not found: $BODY_FILE"
  BODY_TEXT="$(cat "$BODY_FILE")"
else
  [[ $# -eq 1 ]] || die "expected exactly one new-section-body argument, or use --body-file"
  BODY_TEXT="$1"
fi

append_notes "$BEAD_ID" "$HEADER_ARG" "$BODY_TEXT"
