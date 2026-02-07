#!/bin/bash
# E2E: Saved searches (create/list/run/scheduled alert/disable)
# Implements: bd-1yzi
#
# This script uses the e2e_artifacts helper to capture deterministic artifacts:
# - CLI JSON outputs + timestamps
# - Event payloads (including redacted snippets)
#
# Prereqs:
# - WezTerm running with `wezterm cli` available
# - `jq` available for JSON assertions
#
# Notes:
# - This script intentionally does NOT delete its temp workspace to avoid
#   destructive filesystem actions in automated agent contexts. It prints the
#   workspace path for manual cleanup if desired.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

source "$SCRIPT_DIR/lib/e2e_artifacts.sh"

require_cmd() {
  local cmd="$1"
  if ! command -v "$cmd" >/dev/null 2>&1; then
    echo "Missing prerequisite: $cmd" >&2
    return 1
  fi
}

find_wa_binary() {
  if [[ -n "${WA_BINARY:-}" ]] && [[ -x "${WA_BINARY:-}" ]]; then
    return 0
  fi
  if [[ -x "$PROJECT_ROOT/target/debug/wa" ]]; then
    WA_BINARY="$PROJECT_ROOT/target/debug/wa"
    return 0
  fi
  if [[ -x "$PROJECT_ROOT/target/release/wa" ]]; then
    WA_BINARY="$PROJECT_ROOT/target/release/wa"
    return 0
  fi

  # Best-effort build (kept quiet; artifacts capture stdout/stderr per scenario).
  cargo build -p wa >/dev/null
  WA_BINARY="$PROJECT_ROOT/target/debug/wa"
  if [[ ! -x "$WA_BINARY" ]]; then
    echo "Could not find wa binary at $WA_BINARY" >&2
    return 1
  fi
}

wait_for_json_condition() {
  local desc="$1"
  local timeout_secs="$2"
  local cmd="$3"

  local start
  start=$(date +%s)
  while true; do
    if bash -lc "$cmd" >/dev/null 2>&1; then
      echo "OK: $desc"
      return 0
    fi
    if (( $(date +%s) - start >= timeout_secs )); then
      echo "TIMEOUT: $desc" >&2
      return 1
    fi
    sleep 0.5
  done
}

TEMP_WORKSPACE=""
WA_PID=""
PANE_A=""
PANE_B=""
MARKER=""
SAVED_NAME="saved-search-e2e"

cleanup_best_effort() {
  set +e
  if [[ -n "$WA_PID" ]] && kill -0 "$WA_PID" 2>/dev/null; then
    kill "$WA_PID" 2>/dev/null || true
    wait "$WA_PID" 2>/dev/null || true
  fi
  WA_PID=""

  if [[ -n "$PANE_A" ]]; then
    wezterm cli kill-pane --pane-id "$PANE_A" 2>/dev/null || true
  fi
  if [[ -n "$PANE_B" ]]; then
    wezterm cli kill-pane --pane-id "$PANE_B" 2>/dev/null || true
  fi

  if [[ -n "${TEMP_WORKSPACE:-}" ]]; then
    echo "Temp workspace left on disk: $TEMP_WORKSPACE" >&2
  fi
  set -e
}
trap cleanup_best_effort EXIT

scenario_create_list_run() {
  echo "marker=$MARKER"
  "$WA_BINARY" search save "$SAVED_NAME" "$MARKER" --limit 50 -f json | tee /dev/stdout | jq -e '.ok == true' >/dev/null
  "$WA_BINARY" search saved list -f json | tee /dev/stdout | jq -e '.ok == true and (.saved_searches[]? | select(.name == "'"$SAVED_NAME"'" ))' >/dev/null
  "$WA_BINARY" search saved run "$SAVED_NAME" -f json | tee /dev/stdout | jq -e '.ok == true' >/dev/null
}

scenario_scheduled_alert_emits_event() {
  "$WA_BINARY" search saved schedule "$SAVED_NAME" 1000 -f json | tee /dev/stdout | jq -e '.ok == true' >/dev/null

  # Wait for a saved_search.alert event to appear.
  wait_for_json_condition \
    "saved_search.alert event recorded" \
    30 \
    "\"$WA_BINARY\" events -f json --limit 200 | jq -e '.ok == true and (.events[]? | select(.event_type == \"saved_search.alert\" and .rule_id == \"wezterm.saved_search.alert\"))'"

  # Capture the event payload and assert redaction of the injected sk- token.
  "$WA_BINARY" events -f json --limit 200 > /tmp/wa_saved_search_events.json
  e2e_add_file "events.json" "$(cat /tmp/wa_saved_search_events.json)"
  jq -e '
    .events[]
    | select(.event_type == "saved_search.alert" and .rule_id == "wezterm.saved_search.alert")
    | .extracted.snippet? // ""
    | test("\\\\[REDACTED\\\\]")
  ' /tmp/wa_saved_search_events.json >/dev/null
}

scenario_disable_prevents_future_alerts() {
  local before
  before=$("$WA_BINARY" events -f json --limit 500 | jq '[.events[]? | select(.event_type == "saved_search.alert" and .rule_id == "wezterm.saved_search.alert")] | length')
  echo "alerts_before=$before"

  "$WA_BINARY" search saved disable "$SAVED_NAME" -f json | tee /dev/stdout | jq -e '.ok == true' >/dev/null

  # Generate more matching output in a second pane; search is unscoped, so new data would normally trigger.
  local secret="sk-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
  local spawn_output
  spawn_output=$(wezterm cli spawn --cwd "$TEMP_WORKSPACE" -- bash -lc "for i in \$(seq 1 30); do echo \"E2E saved search disable test: $MARKER $secret line=\$i\"; sleep 0.02; done; sleep 5" 2>&1)
  PANE_B=$(echo "$spawn_output" | grep -oE '^[0-9]+$' | head -1 || true)
  echo "pane_b=$PANE_B"

  # Wait for pane B output to be ingested (replaces bare sleep 3)
  if [[ -n "$PANE_B" ]]; then
    wait_for_json_condition \
      "pane $PANE_B observed by watcher" 10 \
      "$WA_BINARY status -f json 2>/dev/null | jq -e '.observed_panes[]? | select(.pane_id == $PANE_B)' >/dev/null 2>&1"
  fi

  local after
  after=$("$WA_BINARY" events -f json --limit 500 | jq '[.events[]? | select(.event_type == "saved_search.alert" and .rule_id == "wezterm.saved_search.alert")] | length')
  echo "alerts_after=$after"

  if [[ "$after" != "$before" ]]; then
    echo "Expected no new alerts after disabling; before=$before after=$after" >&2
    return 1
  fi
}

main() {
  require_cmd wezterm
  require_cmd jq
  require_cmd sqlite3 || true
  find_wa_binary

  MARKER="E2E_SAVED_SEARCH_$(date +%s%N)"
  TEMP_WORKSPACE="$(mktemp -d /tmp/wa-e2e-saved-searches.XXXXXX)"

  export WA_DATA_DIR="$TEMP_WORKSPACE/.wa"
  export WA_WORKSPACE="$TEMP_WORKSPACE"
  mkdir -p "$WA_DATA_DIR"

  e2e_init_artifacts "saved-searches" >/dev/null
  e2e_add_file "workspace.txt" "$TEMP_WORKSPACE"
  e2e_add_file "marker.txt" "$MARKER"

  # Start a pane that emits marker + secret content.
  local secret="sk-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
  local spawn_output
  spawn_output=$(wezterm cli spawn --cwd "$TEMP_WORKSPACE" -- bash -lc "for i in \$(seq 1 60); do echo \"E2E saved search: $MARKER $secret line=\$i\"; sleep 0.02; done; sleep 10" 2>&1)
  PANE_A=$(echo "$spawn_output" | grep -oE '^[0-9]+$' | head -1 || true)
  if [[ -z "$PANE_A" ]]; then
    echo "Failed to spawn pane A. Output: $spawn_output" >&2
    return 1
  fi
  e2e_add_file "pane_a.txt" "$PANE_A"

  # Start watcher (scheduler runs inside watcher).
  "$WA_BINARY" watch --foreground >"$E2E_RUN_DIR/wa_watch.log" 2>&1 &
  WA_PID=$!

  # Wait for pane observation.
  wait_for_json_condition \
    "pane observed by watcher" \
    30 \
    "\"$WA_BINARY\" robot state -f json | jq -e '.data[]? | select(.pane_id == $PANE_A)'"

  # Run scenarios in deterministic order.
  e2e_capture_scenario "create_list_run" scenario_create_list_run
  e2e_capture_scenario "scheduled_alert_emits_event" scenario_scheduled_alert_emits_event
  e2e_capture_scenario "disable_prevents_future_alerts" scenario_disable_prevents_future_alerts

  e2e_finalize 0
}

main "$@"

