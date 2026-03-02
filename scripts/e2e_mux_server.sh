#!/usr/bin/env bash
set -euo pipefail

# End-to-end smoke harness for a headless frankenterm-mux-server instance.
# This script focuses on deterministic pass/fail timing output and can be run
# locally or in CI environments.

SERVER_BIN="${SERVER_BIN:-frankenterm-mux-server}"
FT_BIN="${FT_BIN:-ft}"
SOCKET_PATH="${SOCKET_PATH:-}"
LOG_PATH="${LOG_PATH:-/tmp/frankenterm-mux-server.e2e.log}"
TIMEOUT_SECS="${TIMEOUT_SECS:-20}"
DRY_RUN=0

if [[ "${1:-}" == "--dry-run" ]]; then
  DRY_RUN=1
fi

step_index=0
pass_count=0
fail_count=0
server_pid=""
pane_id=""

now_ms() {
  python3 - <<'PY'
import time
print(int(time.time() * 1000))
PY
}

log() {
  printf '%s\n' "$*"
}

run_cmd() {
  if [[ "$DRY_RUN" -eq 1 ]]; then
    log "[DRY-RUN] $*"
    return 0
  fi
  "$@"
}

run_step() {
  local name="$1"
  shift
  step_index=$((step_index + 1))
  local start_ms end_ms duration_ms
  start_ms="$(now_ms)"

  if "$@"; then
    end_ms="$(now_ms)"
    duration_ms=$((end_ms - start_ms))
    pass_count=$((pass_count + 1))
    log "[PASS] ${step_index}. ${name} (${duration_ms}ms)"
    return 0
  else
    end_ms="$(now_ms)"
    duration_ms=$((end_ms - start_ms))
    fail_count=$((fail_count + 1))
    log "[FAIL] ${step_index}. ${name} (${duration_ms}ms)"
    return 1
  fi
}

cleanup() {
  if [[ -n "$server_pid" ]] && kill -0 "$server_pid" 2>/dev/null; then
    kill -TERM "$server_pid" 2>/dev/null || true
    wait "$server_pid" 2>/dev/null || true
  fi
}
trap cleanup EXIT

require_bin() {
  command -v "$1" >/dev/null 2>&1
}

start_server() {
  if [[ "$DRY_RUN" -eq 1 ]]; then
    log "[DRY-RUN] $SERVER_BIN --daemonize=false >$LOG_PATH 2>&1 &"
    return 0
  fi

  if ! require_bin "$SERVER_BIN"; then
    log "missing binary: $SERVER_BIN"
    return 1
  fi

  : >"$LOG_PATH"
  "$SERVER_BIN" --daemonize=false >"$LOG_PATH" 2>&1 &
  server_pid="$!"

  for _ in $(seq 1 "$TIMEOUT_SECS"); do
    if ! kill -0 "$server_pid" 2>/dev/null; then
      log "server exited early; log follows"
      sed -n '1,200p' "$LOG_PATH" || true
      return 1
    fi
    sleep 1
    # Give server a chance to initialize before follow-up steps.
    if grep -qiE "listening|mux-startup|local" "$LOG_PATH" 2>/dev/null; then
      return 0
    fi
  done

  # If server is still alive after timeout, treat startup as successful.
  kill -0 "$server_pid" 2>/dev/null
}

check_socket() {
  if [[ -z "$SOCKET_PATH" ]]; then
    log "SOCKET_PATH not set; skipping strict socket existence check"
    return 0
  fi

  if [[ "$DRY_RUN" -eq 1 ]]; then
    log "[DRY-RUN] test -S $SOCKET_PATH"
    return 0
  fi

  for _ in $(seq 1 "$TIMEOUT_SECS"); do
    if [[ -S "$SOCKET_PATH" ]]; then
      return 0
    fi
    sleep 1
  done
  return 1
}

robot_state() {
  if [[ "$DRY_RUN" -eq 1 ]]; then
    log "[DRY-RUN] $FT_BIN robot --format json state"
    pane_id="0"
    return 0
  fi

  if ! require_bin "$FT_BIN"; then
    log "missing binary: $FT_BIN"
    return 1
  fi

  local out
  if [[ -n "$SOCKET_PATH" ]]; then
    out="$(WEZTERM_UNIX_SOCKET="$SOCKET_PATH" "$FT_BIN" robot --format json state)"
  else
    out="$("$FT_BIN" robot --format json state)"
  fi

  if command -v jq >/dev/null 2>&1; then
    pane_id="$(printf '%s' "$out" | jq -r '.data.panes[0].pane_id // empty')"
  else
    pane_id="$(printf '%s' "$out" | sed -n 's/.*"pane_id"[[:space:]]*:[[:space:]]*\([0-9][0-9]*\).*/\1/p' | head -n 1)"
  fi

  [[ -n "$pane_id" ]]
}

robot_send_and_search() {
  if [[ "$DRY_RUN" -eq 1 ]]; then
    log "[DRY-RUN] $FT_BIN robot send <pane_id> 'echo e2e-mux-server-ok'"
    log "[DRY-RUN] $FT_BIN robot search 'e2e-mux-server-ok'"
    return 0
  fi

  [[ -n "$pane_id" ]]

  if [[ -n "$SOCKET_PATH" ]]; then
    WEZTERM_UNIX_SOCKET="$SOCKET_PATH" "$FT_BIN" robot send "$pane_id" "echo e2e-mux-server-ok" >/dev/null
    sleep 1
    WEZTERM_UNIX_SOCKET="$SOCKET_PATH" "$FT_BIN" robot search "e2e-mux-server-ok" >/dev/null
  else
    "$FT_BIN" robot send "$pane_id" "echo e2e-mux-server-ok" >/dev/null
    sleep 1
    "$FT_BIN" robot search "e2e-mux-server-ok" >/dev/null
  fi
}

robot_events() {
  if [[ "$DRY_RUN" -eq 1 ]]; then
    log "[DRY-RUN] $FT_BIN robot events --limit 5"
    return 0
  fi

  if [[ -n "$SOCKET_PATH" ]]; then
    WEZTERM_UNIX_SOCKET="$SOCKET_PATH" "$FT_BIN" robot events --limit 5 >/dev/null
  else
    "$FT_BIN" robot events --limit 5 >/dev/null
  fi
}

graceful_shutdown() {
  if [[ "$DRY_RUN" -eq 1 ]]; then
    log "[DRY-RUN] kill -TERM <server_pid> && wait"
    return 0
  fi

  [[ -n "$server_pid" ]]
  kill -TERM "$server_pid"

  for _ in $(seq 1 "$TIMEOUT_SECS"); do
    if ! kill -0 "$server_pid" 2>/dev/null; then
      return 0
    fi
    sleep 1
  done
  return 1
}

main() {
  log "e2e_mux_server: starting"
  run_step "start frankenterm-mux-server" start_server || true
  run_step "verify socket path" check_socket || true
  run_step "ft robot state" robot_state || true
  run_step "ft robot send + search" robot_send_and_search || true
  run_step "ft robot events" robot_events || true
  run_step "SIGTERM graceful shutdown" graceful_shutdown || true

  log ""
  log "Summary: pass=${pass_count} fail=${fail_count} total=${step_index}"
  if [[ "$fail_count" -gt 0 ]]; then
    return 1
  fi
}

main "$@"
