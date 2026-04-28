#!/usr/bin/env bats
# Cross-script concurrency tests for operator maintenance helpers.
#
# Bead: ft-v5lz3.2.8
#
# Run:
#   bats tests/operator_lock_tests.bats

setup() {
    if [[ "$(uname)" != "Darwin" ]]; then
        skip "macOS-only (clean-stale-targets.sh uses BSD stat -f %m)"
    fi

    TESTS_DIR="$(cd "$(dirname "${BATS_TEST_FILENAME}")" && pwd)"
    REPO_ROOT="$(cd "${TESTS_DIR}/.." && pwd)"
    SWARM_TICK="${REPO_ROOT}/scripts/swarm-tick.sh"
    CLEAN_STALE="${REPO_ROOT}/scripts/clean-stale-targets.sh"

    TMP_DIR="$(mktemp -d /tmp/operator-lock-test.XXXXXX)"
    BIN_DIR="${TMP_DIR}/bin"
    mkdir -p "$BIN_DIR"
    export FT_OPERATOR_LOCK_DIR="${TMP_DIR}/operator.lock"
    export TARGET_GLOB="${TMP_DIR}/ft-*-target"

    write_stub git '#!/usr/bin/env bash
case "$*" in
  *"1 hour ago"*) printf "a\nb\n" ;;
  *"4 minutes ago"*) printf "a\n" ;;
esac
'
    write_stub br '#!/usr/bin/env bash
if [[ "$1" == "ready" ]]; then
  printf "[{}]\n"
else
  printf "{\"issues\":[{},{}]}\n"
fi
'
    write_stub df '#!/usr/bin/env bash
printf "Filesystem Size Used Avail Capacity Mounted on\n"
printf "/dev/mock 100G 50G 50G 50%% /mock\n"
'
    write_stub find '#!/usr/bin/env bash
exit 0
'
    write_stub ls '#!/usr/bin/env bash
exit 1
'
    write_stub du '#!/usr/bin/env bash
exit 0
'
    write_stub ntm '#!/usr/bin/env bash
printf "%s\n" "{\"sessions\":[{\"name\":\"frankenterm\",\"panes\":1,\"agents\":[{\"pane_idx\":0,\"type\":\"codex\",\"pane\":\"pane-0\"}]}]}"
'
}

teardown() {
    if [[ -n "${TMP_DIR:-}" && -d "$TMP_DIR" ]]; then
        rm -rf "$TMP_DIR"
    fi
}

write_stub() {
    local name="$1"
    local body="$2"
    printf '%s' "$body" > "${BIN_DIR}/${name}"
    chmod +x "${BIN_DIR}/${name}"
}

@test "shared operator lock serializes swarm-tick behind clean-stale holder" {
    holder="${TMP_DIR}/hold-clean-lock.sh"
    cat > "$holder" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
mkdir "$FT_OPERATOR_LOCK_DIR"
printf '%s\n' "$$" > "$FT_OPERATOR_LOCK_DIR/pid"
printf '%s\n' "clean-stale-targets.sh" > "$FT_OPERATOR_LOCK_DIR/name"
printf 'ready\n' > "$HOLDER_READY"
trap 'rm -f "$FT_OPERATOR_LOCK_DIR/pid" "$FT_OPERATOR_LOCK_DIR/name"; rmdir "$FT_OPERATOR_LOCK_DIR" 2>/dev/null || true' EXIT
sleep 1
EOF
    chmod +x "$holder"

    HOLDER_READY="${TMP_DIR}/holder.ready" \
        FT_OPERATOR_LOCK_DIR="$FT_OPERATOR_LOCK_DIR" \
        "$holder" &
    holder_pid=$!

    for _ in {1..50}; do
        [[ -f "${TMP_DIR}/holder.ready" ]] && break
        sleep 0.02
    done
    [[ -f "${TMP_DIR}/holder.ready" ]]

    start=$(date +%s)
    PATH="${BIN_DIR}:$PATH" \
        REPO_ROOT="$REPO_ROOT" \
        FT_OPERATOR_LOCK_DIR="$FT_OPERATOR_LOCK_DIR" \
        bash "$SWARM_TICK" frankenterm > "${TMP_DIR}/swarm.json"
    elapsed=$(( $(date +%s) - start ))
    wait "$holder_pid"

    jq . "${TMP_DIR}/swarm.json" > /dev/null
    [[ "$elapsed" -ge 1 ]]
    [ ! -d "$FT_OPERATOR_LOCK_DIR" ]
}

@test "clean-stale recovers a stale shared operator lock before removing targets" {
    mkdir -p "${TMP_DIR}/ft-stale-target"
    touch -t "$(date -v-1500M +%Y%m%d%H%M.%S)" "${TMP_DIR}/ft-stale-target"
    mkdir "$FT_OPERATOR_LOCK_DIR"
    printf '999999\n' > "$FT_OPERATOR_LOCK_DIR/pid"
    printf 'dead-holder\n' > "$FT_OPERATOR_LOCK_DIR/name"

    run env FT_OPERATOR_LOCK_DIR="$FT_OPERATOR_LOCK_DIR" TARGET_GLOB="$TARGET_GLOB" bash "$CLEAN_STALE" 12

    [ "$status" -eq 0 ]
    [[ "$output" == *"cleaned 1 dirs"* ]]
    [ ! -d "$FT_OPERATOR_LOCK_DIR" ]
}
