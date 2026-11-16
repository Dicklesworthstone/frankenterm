#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
ft_bin="${FT_BIN:-ft}"
trace="$repo_root/fixtures/scale-lab/resource-what-if-trace.v1.json"
candidate="$repo_root/fixtures/scale-lab/resource-what-if-candidate.v1.toml"

json_output="$("$ft_bin" resource what-if --trace "$trace" --override-package "$candidate" --format json)"
printf '%s\n' "$json_output" | jq -e '
  .schema_version == "ft.resource_what_if.v1" and
  .dry_run == true and
  .mutation_surface == [] and
  .trace_hash == "resource-what-if-trace-v1" and
  (.override_hash | type == "string") and
  (.decision_deltas.changed_steps >= 1) and
  (.risk_score | type == "number") and
  (.confidence_score | type == "number") and
  (.proof_status == "PASSED") and
  (.next_proof_steps | type == "array")
' >/dev/null

robot_output="$("$ft_bin" robot --format json resource what-if --trace "$trace" --override-package "$candidate")"
printf '%s\n' "$robot_output" | jq -e '
  .ok == true and
  .data.schema_version == "ft.resource_what_if.v1" and
  .data.dry_run == true and
  .data.mutation_surface == [] and
  .data.trace_hash == "resource-what-if-trace-v1"
' >/dev/null

plain_output="$("$ft_bin" resource what-if --trace "$trace" --override-package "$candidate")"
printf '%s\n' "$plain_output" | grep -q '^Resource what-if:'
printf '%s\n' "$plain_output" | grep -q '^Dry-run: no live panes'

toon_output="$("$ft_bin" robot --format toon resource what-if --trace "$trace" --override-package "$candidate")"
printf '%s\n' "$toon_output" | grep -q 'schema_version'

proof_trace="$repo_root/fixtures/scale-lab/resource-what-if-proof/cpu-queue-saturated-trace.v1.json"
proof_candidate="$repo_root/fixtures/scale-lab/resource-what-if-proof/queue-relief-candidate.v1.toml"
proof_command="ft resource what-if --trace fixtures/scale-lab/resource-what-if-proof/cpu-queue-saturated-trace.v1.json --override-package fixtures/scale-lab/resource-what-if-proof/queue-relief-candidate.v1.toml --format json"
proof_json="$("$ft_bin" resource what-if --trace "$proof_trace" --override-package "$proof_candidate" --format json)"
printf '%s\n' "$proof_json" | jq -e '
  .schema_version == "ft.resource_what_if.v1" and
  .dry_run == true and
  .mutation_surface == [] and
  .trace_hash == "resource-what-if-proof-cpu-queue-saturated-v1" and
  .proof_status == "SKIPPED_NOT_PROVEN" and
  .proof_evidence_source == "replay_backed" and
  .hardware_predicate == "skipped_not_proven" and
  .high_scale_claim_allowed == false and
  (.next_proof_steps | length >= 1)
' >/dev/null

proof_status="$(printf '%s\n' "$proof_json" | jq -r '.proof_status')"
hardware_predicate="$(printf '%s\n' "$proof_json" | jq -r '.hardware_predicate')"
high_scale_claim_allowed="$(printf '%s\n' "$proof_json" | jq -r '.high_scale_claim_allowed')"
jq -n \
  --arg schema_version "ft.resource_what_if.e2e_summary.v1" \
  --arg trace "fixtures/scale-lab/resource-what-if-proof/cpu-queue-saturated-trace.v1.json" \
  --arg override_package "fixtures/scale-lab/resource-what-if-proof/queue-relief-candidate.v1.toml" \
  --arg command "$proof_command" \
  --arg proof_status "$proof_status" \
  --arg hardware_predicate "$hardware_predicate" \
  --argjson high_scale_claim_allowed "$high_scale_claim_allowed" \
  '{
    schema_version: $schema_version,
    ok: true,
    trace: $trace,
    override_package: $override_package,
    command: $command,
    proof_status: $proof_status,
    hardware_predicate: $hardware_predicate,
    high_scale_claim_allowed: $high_scale_claim_allowed
  }'
