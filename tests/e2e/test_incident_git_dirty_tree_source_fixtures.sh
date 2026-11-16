#!/usr/bin/env bash
# Static verifier for incident-bundle git_dirty_tree source fixtures.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "${ROOT}"

FIXTURE="fixtures/incident-bundles/git-dirty-tree-source/cases.v1.json"

fail() {
  printf 'incident git_dirty_tree fixture corpus: %s\n' "$*" >&2
  exit 1
}

[[ -f "${FIXTURE}" ]] || fail "missing fixture: ${FIXTURE}"

jq empty "${FIXTURE}"

jq -e '
  .schema_version == 1
  and .contract_id == "ft.incident_bundle.git_dirty_tree_source.fixtures.v1"
  and .producing_bead == "ft-9srtq"
  and .related_bead == "ft-ip124"
  and (.purpose | type == "string" and length > 0)
  and .golden_confidence.artifact_kind == "structural_json_golden"
  and .golden_confidence.deterministic == true
  and .golden_confidence.platform_dependent == false
  and (.golden_confidence.volatility | type == "number" and . >= 1 and . <= 5)
  and (.source_policy | keys | sort) == [
    "checkout_allowed",
    "clean_allowed",
    "delete_files_allowed",
    "full_diff_payload_allowed",
    "local_cargo_proof_allowed",
    "mutates_git_state_allowed",
    "rch_service_mutation_allowed",
    "reset_allowed",
    "staging_allowed"
  ]
  and all(.source_policy[]; . == false)
  and (.required_cases | sort) == [
    "clean-tree",
    "dirty-tracked-overlap",
    "git-metadata-unavailable",
    "untracked-review-required"
  ]
  and ([.cases[].case_id] | sort) == (.required_cases | sort)
' "${FIXTURE}" >/dev/null || fail "top-level fixture contract is incomplete"

jq -e '
  def source_status_ok: IN("collected", "unavailable");
  def evidence_state_ok: IN("measured", "unavailable");
  def risk_ok: IN("clean", "medium", "high", "unavailable");
  def severity_ok: IN("medium", "high");
  def safe_command:
    . == "git status --short --branch" or . == "git diff --stat --";
  def retained_repo_path_ok:
    type == "string"
    and length > 0
    and . != "."
    and . != ".."
    and (startswith("/") | not)
    and (startswith("./") | not)
    and (startswith("../") | not)
    and (contains("/../") | not)
    and (contains("/./") | not)
    and (endswith("/..") | not)
    and (endswith("/.") | not)
    and . != ".git"
    and (startswith(".git/") | not)
    and (contains("/.git/") | not);
  def forbidden_set_ok:
    sort == [
      "delete_files",
      "git add <path>",
      "git checkout -- <path>",
      "git clean -fd",
      "git reset --hard"
    ];

  all(.cases[];
    .source_name == "git_dirty_tree"
    and .source_surface == "git status --short --branch"
    and (.title | type == "string" and length > 0)
    and (.allowed_source_commands | type == "array" and length >= 1 and all(.[]; safe_command))
    and (.bundle_entry.status | source_status_ok)
    and (.bundle_entry.evidence_state | evidence_state_ok)
    and .bundle_entry.mutates_state == false
    and (.bundle_entry.redaction | IN("partial", "not_applicable"))
    and .bundle_entry.privacy_tier == "metadata_only"
    and (.bundle_entry.size_bytes | type == "number" and . >= 0)
    and (.bundle_entry.warning_ids | type == "array")
    and (.summary.risk_level | risk_ok)
    and .summary.truncated == false
    and .summary.full_diff_stored == false
    and (.path_summaries | type == "array")
    and all(.path_summaries[];
      (.status | IN(" M", "??"))
      and (.path | retained_repo_path_ok)
      and (.category | IN("tracked_overlap_risk", "untracked_review_required"))
      and (.severity | severity_ok)
    )
    and (.warnings | type == "array")
    and all(.warnings[];
      (.id | test("^git-dirty-tree\\."))
      and (.severity | severity_ok)
      and (.reason_code | test("^git\\."))
      and (.message | type == "string" and length > 0)
    )
    and (.forbidden_actions | forbidden_set_ok)
    and (.safe_next_action | test("^record_"))
  )
' "${FIXTURE}" >/dev/null || fail "one or more fixture cases are incomplete"

jq -e '
  .cases[]
  | select(.case_id == "clean-tree")
  | .bundle_entry.status == "collected"
    and .bundle_entry.evidence_state == "measured"
    and .summary.risk_level == "clean"
    and .summary.tracked_dirty_count == 0
    and .summary.untracked_count == 0
    and .summary.high_risk_count == 0
    and (.path_summaries | length == 0)
    and (.warnings | length == 0)
' "${FIXTURE}" >/dev/null || fail "clean-tree case drifted"

jq -e '
  .cases[]
  | select(.case_id == "dirty-tracked-overlap")
  | .bundle_entry.status == "collected"
    and .summary.risk_level == "high"
    and .summary.tracked_dirty_count > 0
    and .summary.high_risk_count == .summary.tracked_dirty_count
    and all(.path_summaries[]; .category == "tracked_overlap_risk" and .severity == "high")
    and ([.warnings[].reason_code] | index("git.tracked_overlap_risk") != null)
' "${FIXTURE}" >/dev/null || fail "dirty-tracked-overlap case drifted"

jq -e '
  .cases[]
  | select(.case_id == "untracked-review-required")
  | .bundle_entry.status == "collected"
    and .summary.risk_level == "medium"
    and .summary.tracked_dirty_count == 0
    and .summary.untracked_count > 0
    and all(.path_summaries[]; .category == "untracked_review_required" and .severity == "medium")
    and ([.warnings[].reason_code] | index("git.untracked_review_required") != null)
' "${FIXTURE}" >/dev/null || fail "untracked-review-required case drifted"

jq -e '
  .cases[]
  | select(.case_id == "git-metadata-unavailable")
  | .bundle_entry.status == "unavailable"
    and .bundle_entry.evidence_state == "unavailable"
    and .branch.name == null
    and .summary.risk_level == "unavailable"
    and .summary.tracked_dirty_count == null
    and ([.warnings[].reason_code] | index("git.metadata_unavailable") != null)
    and .safe_next_action == "record_unavailable_git_dirty_tree_source"
' "${FIXTURE}" >/dev/null || fail "git-metadata-unavailable case drifted"

# Content-leak guard (ft-ip124 / ft-1nqye class): a dirty-tree snapshot must
# carry only path metadata, never file contents or diffs. The per-case loop
# above validates the four allowed path_summary fields but does not forbid
# EXTRA keys, so a smuggled diff/content field would slip through. Assert every
# path_summary has exactly the metadata key set and that the bundle stores no
# full diff, then prove the guard bites against a smuggled content field.
jq -e '
  def metadata_only($c):
    $c.summary.full_diff_stored == false
    and $c.summary.truncated == false
    and all($c.path_summaries[]; (keys - ["category", "path", "severity", "status"]) == []);
  (all(.cases[]; metadata_only(.)))
  and (.cases[] | select((.path_summaries | length) > 0) | [.] ) as $withpaths
  | ($withpaths[0] as $p
    | (metadata_only($p | .summary.full_diff_stored = true) | not)
    and (metadata_only($p | .path_summaries[0].diff = "leaked file contents") | not))
' "${FIXTURE}" >/dev/null || fail "git dirty-tree content-leak guard does not bite"

case_count="$(jq -r '.cases | length' "${FIXTURE}")"
printf 'incident git_dirty_tree fixture corpus: static verifier passed (%s cases)\n' "${case_count}"
