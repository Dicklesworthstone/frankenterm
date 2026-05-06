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
