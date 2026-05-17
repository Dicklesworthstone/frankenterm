#!/usr/bin/env bash
# Static verifier for the massive-swarm evidence index fixture.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "${ROOT}"

INDEX="fixtures/scale-lab/massive-swarm-evidence-index.v1.json"
REQUIRED_SCENARIOS=(
  "synthetic_10k_policy_audit"
  "synthetic_1k_churn"
  "synthetic_5k_event_storm"
)
TARGET_CPU_COUNT=64
TARGET_MEMORY_BYTES=274877906944

fail() {
  printf 'massive-swarm evidence index: %s\n' "$*" >&2
  exit 1
}

require_command() {
  command -v "$1" >/dev/null 2>&1 || fail "missing command: $1"
}

require_file() {
  local path="$1"
  [[ -f "${path}" ]] || fail "missing file: ${path}"
}

require_command jq
require_file "${INDEX}"

jq empty "${INDEX}"

jq -e --argjson required "$(printf '%s\n' "${REQUIRED_SCENARIOS[@]}" | jq -R . | jq -s .)" '
  (.manifest.scenarios | type == "array" and length == 3)
  and (.proofs | type == "array" and length == 3)
  and ([.manifest.scenarios[].id] | sort) == ($required | sort)
  and ([.proofs[].scenario_id] | sort) == ($required | sort)
  and all(.manifest.scenarios[];
    (.id | type == "string" and length > 0)
    and (.label | type == "string" and length > 0)
    and .class == "synthetic"
    and (.deterministic_seed | type == "number" and . > 0)
    and (.dimensions | type == "array" and length >= 2)
    and (.dimensions | index("correctness") != null)
    and (.counters | type == "object")
    and (.counters.logical_panes | type == "number" and . > 0)
    and (.counters.logical_agents == .counters.logical_panes)
    and (.counters.churn_events | type == "number" and . > 0)
    and (.counters.alt_screen_flips | type == "number" and . > 0)
    and (.counters.event_storms | type == "number" and . > 0)
    and (.counters.output_burst_bytes | type == "number" and . > 0)
    and (.counters.storage_writes | type == "number" and . > 0)
    and (.counters.policy_denials | type == "number" and . > 0)
  )
  and all(.proofs[];
    (.scenario_id | type == "string" and length > 0)
    and (.class | IN("synthetic", "live_hardware"))
    and (.dimensions | type == "array" and length >= 1)
    and (.status | IN("PASSED", "SKIPPED_NOT_PROVEN"))
    and (.evidence_source | IN("synthetic", "rch_remote", "live_hardware"))
    and (.note | type == "string" and length > 0)
  )
' "${INDEX}" >/dev/null || fail "manifest/proof shape drifted"

jq -e '
  def by_id($id): .manifest.scenarios[] | select(.id == $id);

  (by_id("synthetic_1k_churn").counters.logical_panes == 1024)
  and (by_id("synthetic_5k_event_storm").counters.logical_panes == 5120)
  and (by_id("synthetic_10k_policy_audit").counters.logical_panes == 10240)
  and (by_id("synthetic_1k_churn").counters.logical_panes < by_id("synthetic_5k_event_storm").counters.logical_panes)
  and (by_id("synthetic_5k_event_storm").counters.logical_panes < by_id("synthetic_10k_policy_audit").counters.logical_panes)
  and (by_id("synthetic_1k_churn").counters.output_burst_bytes < by_id("synthetic_5k_event_storm").counters.output_burst_bytes)
  and (by_id("synthetic_5k_event_storm").counters.output_burst_bytes < by_id("synthetic_10k_policy_audit").counters.output_burst_bytes)
  and (by_id("synthetic_1k_churn").counters.storage_writes < by_id("synthetic_5k_event_storm").counters.storage_writes)
  and (by_id("synthetic_5k_event_storm").counters.storage_writes < by_id("synthetic_10k_policy_audit").counters.storage_writes)
  and (by_id("synthetic_10k_policy_audit").dimensions | index("memory") != null)
' "${INDEX}" >/dev/null || fail "synthetic scenario counter scale drifted"

jq -e \
  --argjson target_cpu "${TARGET_CPU_COUNT}" \
  --argjson target_memory "${TARGET_MEMORY_BYTES}" '
  all(.proofs[] | select(.status == "PASSED");
    .class == "synthetic"
    and (.evidence | type == "object")
    and (.evidence.cpu_count | type == "number" and . < $target_cpu)
    and (.evidence.memory_bytes | type == "number" and . < $target_memory)
    and (.evidence.command | type == "string" and length > 0)
    and (.evidence.elapsed_ms | type == "number" and . > 0)
    and (.evidence.git_commit | type == "string" and length > 0)
  )
  and any(.proofs[];
    .scenario_id == "synthetic_1k_churn"
    and .status == "PASSED"
    and .evidence_source == "synthetic"
    and (.evidence.command | startswith("cargo test "))
    and ((.evidence.command | startswith("rch exec")) | not)
  )
  and any(.proofs[];
    .scenario_id == "synthetic_5k_event_storm"
    and .status == "PASSED"
    and .evidence_source == "rch_remote"
    and (.evidence.command | startswith("rch exec --"))
  )
' "${INDEX}" >/dev/null || fail "reduced synthetic/RCH proof rows are incomplete or overclaim target hardware"

jq -e '
  any(.proofs[];
    .scenario_id == "synthetic_10k_policy_audit"
    and .class == "live_hardware"
    and .status == "SKIPPED_NOT_PROVEN"
    and .evidence_source == "live_hardware"
    and (.dimensions | index("hardware") != null)
    and (has("evidence") | not)
    and (.note | contains("SKIPPED_NOT_PROVEN"))
    and (.note | contains("64-core / 256 GiB"))
  )
  and ([.proofs[] | select(.class == "live_hardware" and .status == "PASSED")] | length) == 0
' "${INDEX}" >/dev/null || fail "live-hardware proof row no longer fails closed"

jq -e \
  --argjson target_cpu "${TARGET_CPU_COUNT}" \
  --argjson target_memory "${TARGET_MEMORY_BYTES}" '
  all(.proofs[];
    if .status == "PASSED" then
      .class == "synthetic"
      and .evidence.cpu_count < $target_cpu
      and .evidence.memory_bytes < $target_memory
    else
      .status == "SKIPPED_NOT_PROVEN"
    end
  )
' "${INDEX}" >/dev/null || fail "fixture can be misread as target-class hardware proof"

printf 'massive-swarm evidence index: static verifier passed (%d scenarios, %d proof rows)\n' \
  "$(jq '.manifest.scenarios | length' "${INDEX}")" \
  "$(jq '.proofs | length' "${INDEX}")"
