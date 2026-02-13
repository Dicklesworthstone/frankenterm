#!/usr/bin/env bash
set -euo pipefail

SCRIPT_NAME="$(basename "$0")"
SCENARIO_DIR=""
FT_BINARY=""
TIMEOUT_SECS=120
VERBOSE=false

WORK_DIR=""
USER_SSH_DIR=""
USER_SSH_CONFIG=""
SSH_CONFIG_BACKUP=""
SSH_CONFIG_EXISTED=false
IMAGE_TAG=""
GOOD_CID=""
GOOD_PORT=""
BAD_CID=""
BAD_PORT=""

usage() {
    cat <<EOF
Usage: $SCRIPT_NAME --scenario-dir DIR --ft-binary PATH [--timeout-secs N] [--verbose]

Runs dockerized E2E for:
  - ft setup remote dry-run
  - ft setup remote apply
  - idempotent second apply
  - failure-injection apply with rollback evidence
EOF
}

log() {
    local level="$1"
    shift
    printf '[setup-remote-e2e] [%s] %s\n' "$level" "$*"
}

debug() {
    if [[ "$VERBOSE" == "true" ]]; then
        log "DEBUG" "$*"
    fi
}

die() {
    log "ERROR" "$*"
    exit 1
}

parse_args() {
    while [[ $# -gt 0 ]]; do
        case "$1" in
            --scenario-dir)
                SCENARIO_DIR="$2"
                shift 2
                ;;
            --ft-binary)
                FT_BINARY="$2"
                shift 2
                ;;
            --timeout-secs)
                TIMEOUT_SECS="$2"
                shift 2
                ;;
            --verbose)
                VERBOSE=true
                shift
                ;;
            -h|--help)
                usage
                exit 0
                ;;
            *)
                die "Unknown option: $1"
                ;;
        esac
    done

    [[ -n "$SCENARIO_DIR" ]] || die "--scenario-dir is required"
    [[ -n "$FT_BINARY" ]] || die "--ft-binary is required"
    [[ -x "$FT_BINARY" ]] || die "ft binary is not executable: $FT_BINARY"
    [[ -d "$SCENARIO_DIR" ]] || die "scenario dir does not exist: $SCENARIO_DIR"
    [[ "$TIMEOUT_SECS" =~ ^[0-9]+$ ]] || die "--timeout-secs must be numeric"
}

require_cmds() {
    local missing=()
    for cmd in docker ssh ssh-keygen tar; do
        if ! command -v "$cmd" >/dev/null 2>&1; then
            missing+=("$cmd")
        fi
    done
    if [[ "${#missing[@]}" -gt 0 ]]; then
        die "Missing required commands: ${missing[*]}"
    fi
}

cleanup() {
    local status=$?
    set +e

    if [[ -n "$GOOD_CID" ]]; then
        docker logs "$GOOD_CID" > "$SCENARIO_DIR/good_container.log" 2>&1 || true
        docker inspect "$GOOD_CID" > "$SCENARIO_DIR/good_container_inspect.json" 2>/dev/null || true
        docker rm -f "$GOOD_CID" >/dev/null 2>&1 || true
    fi
    if [[ -n "$BAD_CID" ]]; then
        docker logs "$BAD_CID" > "$SCENARIO_DIR/failure_container.log" 2>&1 || true
        docker inspect "$BAD_CID" > "$SCENARIO_DIR/failure_container_inspect.json" 2>/dev/null || true
        docker rm -f "$BAD_CID" >/dev/null 2>&1 || true
    fi
    if [[ -n "$IMAGE_TAG" ]]; then
        docker image rm "$IMAGE_TAG" >/dev/null 2>&1 || true
    fi

    if [[ -n "$USER_SSH_CONFIG" ]]; then
        if [[ "$SSH_CONFIG_EXISTED" == "true" && -f "$SSH_CONFIG_BACKUP" ]]; then
            cp "$SSH_CONFIG_BACKUP" "$USER_SSH_CONFIG" >/dev/null 2>&1 || true
        elif [[ "$SSH_CONFIG_EXISTED" == "false" ]]; then
            rm -f "$USER_SSH_CONFIG" >/dev/null 2>&1 || true
        fi
    fi

    if [[ "${FT_E2E_PRESERVE_REMOTE_SETUP_TEMP:-0}" == "1" ]]; then
        log "WARN" "Preserving temp work dir: $WORK_DIR"
    else
        rm -rf "$WORK_DIR" >/dev/null 2>&1 || true
    fi

    exit "$status"
}

write_stub_scripts() {
    local stubs_dir="$1/stubs"
    mkdir -p "$stubs_dir"

    cat > "$stubs_dir/wezterm" <<'EOF'
#!/usr/bin/env bash
if [[ "${1:-}" == "--version" ]]; then
    echo "wezterm 20260213-e2e-stub"
    exit 0
fi
echo "wezterm stub"
EOF

    cat > "$stubs_dir/wezterm-mux-server" <<'EOF'
#!/usr/bin/env bash
echo "wezterm-mux-server stub"
EOF

    cat > "$stubs_dir/sudo" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
exec "$@"
EOF

    cat > "$stubs_dir/systemctl" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail

state_dir="/home/e2e/.ft-e2e-state"
mkdir -p "$state_dir"

if [[ "${1:-}" == "--user" ]]; then
    shift
fi

cmd="${1:-}"
case "$cmd" in
    daemon-reload)
        exit 0
        ;;
    enable)
        if [[ "${2:-}" == "--now" && "${3:-}" == "wezterm-mux-server" ]]; then
            if [[ -f "$state_dir/fail-enable" ]]; then
                echo "simulated systemctl enable failure" >&2
                echo "rollback: systemctl --user disable --now wezterm-mux-server" >&2
                exit 42
            fi
            touch "$state_dir/service-enabled"
            exit 0
        fi
        ;;
    disable)
        if [[ "${2:-}" == "--now" && "${3:-}" == "wezterm-mux-server" ]]; then
            rm -f "$state_dir/service-enabled"
            exit 0
        fi
        ;;
    is-active)
        if [[ "${2:-}" == "wezterm-mux-server" ]]; then
            if [[ -f "$state_dir/service-enabled" ]]; then
                echo "active"
                exit 0
            fi
            echo "inactive"
            exit 3
        fi
        ;;
    status)
        if [[ "${2:-}" == "wezterm-mux-server" ]]; then
            if [[ -f "$state_dir/service-enabled" ]]; then
                echo "wezterm-mux-server.service - active (stub)"
                exit 0
            fi
            echo "wezterm-mux-server.service - inactive (stub)"
            exit 3
        fi
        ;;
esac

echo "unsupported systemctl invocation: $*" >&2
exit 1
EOF

    cat > "$stubs_dir/loginctl" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail

state_dir="/home/e2e/.ft-e2e-state"
mkdir -p "$state_dir"

case "${1:-}" in
    show-user)
        echo "Linger=$([[ -f "$state_dir/linger-enabled" ]] && echo yes || echo no)"
        exit 0
        ;;
    enable-linger)
        touch "$state_dir/linger-enabled"
        exit 0
        ;;
    disable-linger)
        rm -f "$state_dir/linger-enabled"
        exit 0
        ;;
esac

echo "unsupported loginctl invocation: $*" >&2
exit 1
EOF

    chmod +x "$stubs_dir"/*
}

prepare_docker_context() {
    local build_dir="$1"
    mkdir -p "$build_dir"

    write_stub_scripts "$build_dir"
    cp "$WORK_DIR/id_ed25519.pub" "$build_dir/authorized_keys"

    cat > "$build_dir/Dockerfile" <<'EOF'
FROM debian:bookworm-slim

RUN apt-get update && \
    DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
      openssh-server bash ca-certificates && \
    rm -rf /var/lib/apt/lists/*

RUN useradd -m -s /bin/bash e2e && \
    mkdir -p /home/e2e/.ssh /var/run/sshd /home/e2e/.config/systemd/user /home/e2e/.ft-e2e-state && \
    chown -R e2e:e2e /home/e2e && \
    chmod 700 /home/e2e/.ssh

COPY authorized_keys /home/e2e/.ssh/authorized_keys
COPY stubs/ /usr/local/bin/

RUN chown e2e:e2e /home/e2e/.ssh/authorized_keys && \
    chmod 600 /home/e2e/.ssh/authorized_keys && \
    chmod +x /usr/local/bin/* && \
    ssh-keygen -A && \
    printf 'PermitRootLogin no\nPasswordAuthentication no\nPubkeyAuthentication yes\nAllowUsers e2e\n' >> /etc/ssh/sshd_config && \
    printf 'FT_E2E_CONTAINER_OK\n' > /etc/ft-e2e-container-sentinel

EXPOSE 22
CMD ["/usr/sbin/sshd", "-D", "-e"]
EOF
}

start_container() {
    local name="$1"
    local fail_enable="$2"
    local cid
    cid=$(docker run -d --rm \
        --name "$name" \
        -p 127.0.0.1::22 \
        "$IMAGE_TAG")
    if [[ "$fail_enable" == "1" ]]; then
        docker exec "$cid" sh -lc "mkdir -p /home/e2e/.ft-e2e-state && touch /home/e2e/.ft-e2e-state/fail-enable" >/dev/null
    fi
    echo "$cid"
}

container_port() {
    local cid="$1"
    docker port "$cid" 22/tcp \
        | head -n1 \
        | sed -E 's/.*:([0-9]+)$/\1/'
}

register_host_alias() {
    local alias="$1"
    local port="$2"
    cat >> "$USER_SSH_CONFIG" <<EOF
Host $alias
  HostName 127.0.0.1
  Port $port
  User e2e
  IdentityFile $WORK_DIR/id_ed25519
  IdentitiesOnly yes
  StrictHostKeyChecking no
  UserKnownHostsFile /dev/null
  LogLevel ERROR

EOF
}

assert_alias_localhost() {
    local alias="$1"
    local expected_port="$2"
    local host_name=""
    local host_port=""

    host_name=$(awk -v h="$alias" '
        $1=="Host" && $2==h { in_block=1; next }
        $1=="Host" && in_block { exit }
        in_block && $1=="HostName" { print $2; exit }
    ' "$USER_SSH_CONFIG")
    host_port=$(awk -v h="$alias" '
        $1=="Host" && $2==h { in_block=1; next }
        $1=="Host" && in_block { exit }
        in_block && $1=="Port" { print $2; exit }
    ' "$USER_SSH_CONFIG")

    [[ "$host_name" == "127.0.0.1" ]] || die "Safety guard failed: alias $alias is not localhost"
    [[ "$host_port" == "$expected_port" ]] || die "Safety guard failed: alias $alias port mismatch"
}

wait_for_ssh() {
    local alias="$1"
    local timeout="$2"
    local started
    started=$(date +%s)

    while true; do
        if ssh "$alias" "echo ready" >/dev/null 2>&1; then
            return 0
        fi
        local now
        now=$(date +%s)
        if (( now - started >= timeout )); then
            return 1
        fi
        sleep 1
    done
}

assert_container_sentinel() {
    local alias="$1"
    local marker
    marker=$(ssh "$alias" "cat /etc/ft-e2e-container-sentinel" 2>/dev/null || true)
    [[ "$marker" == "FT_E2E_CONTAINER_OK" ]] || die "Safety guard failed: host $alias missing container sentinel"
}

run_ft_setup() {
    local mode="$1"
    local host_alias="$2"
    local log_file="$3"

    local -a cmd=("$FT_BINARY")
    if [[ "$VERBOSE" == "true" ]]; then
        cmd+=("-v")
    fi
    cmd+=("setup")
    if [[ "$mode" == "dry-run" ]]; then
        cmd+=("--dry-run")
    elif [[ "$mode" == "apply" ]]; then
        cmd+=("--apply")
    else
        die "unknown mode: $mode"
    fi
    cmd+=("remote" "$host_alias" "--timeout-secs" "$TIMEOUT_SECS" "--yes")

    "${cmd[@]}" >"$log_file" 2>&1
}

capture_remote_service() {
    local alias="$1"
    local out_file="$2"
    ssh "$alias" "cat ~/.config/systemd/user/wezterm-mux-server.service" > "$out_file"
}

assert_remote_state() {
    local alias="$1"
    ssh "$alias" "test -f ~/.ft-e2e-state/service-enabled && test -f ~/.ft-e2e-state/linger-enabled"
}

capture_remote_snapshot() {
    local alias="$1"
    local out_tar="$2"
    ssh "$alias" \
        "cd ~ && tar -cf - .config/systemd/user .ft-e2e-state 2>/dev/null || true" > "$out_tar"
}

main() {
    parse_args "$@"
    require_cmds

    WORK_DIR="$(mktemp -d /tmp/ft-e2e-setup-remote-XXXXXX)"
    USER_SSH_DIR="$HOME/.ssh"
    USER_SSH_CONFIG="$USER_SSH_DIR/config"
    SSH_CONFIG_BACKUP="$WORK_DIR/original_ssh_config"
    mkdir -p "$USER_SSH_DIR" "$WORK_DIR/build"
    chmod 700 "$USER_SSH_DIR"
    trap cleanup EXIT

    if [[ -f "$USER_SSH_CONFIG" ]]; then
        cp "$USER_SSH_CONFIG" "$SSH_CONFIG_BACKUP"
        SSH_CONFIG_EXISTED=true
    else
        SSH_CONFIG_EXISTED=false
        : > "$USER_SSH_CONFIG"
    fi
    chmod 600 "$USER_SSH_CONFIG"

    log "INFO" "Generating ephemeral SSH key"
    ssh-keygen -q -t ed25519 -N "" -f "$WORK_DIR/id_ed25519" >/dev/null

    log "INFO" "Preparing dockerized sshd context"
    prepare_docker_context "$WORK_DIR/build"

    IMAGE_TAG="ft-e2e-setup-remote:$(date +%s)-$$"
    log "INFO" "Building image: $IMAGE_TAG"
    docker build -t "$IMAGE_TAG" "$WORK_DIR/build" > "$SCENARIO_DIR/docker_build.log" 2>&1

    local suffix
    suffix="$(date +%s)-$$"
    local good_alias="ft-e2e-remote-good-$suffix"
    local bad_alias="ft-e2e-remote-fail-$suffix"

    log "INFO" "Starting good container"
    GOOD_CID="$(start_container "ft-e2e-good-$suffix" "0")"
    GOOD_PORT="$(container_port "$GOOD_CID")"
    [[ -n "$GOOD_PORT" ]] || die "Failed to detect docker mapped port for good container"
    register_host_alias "$good_alias" "$GOOD_PORT"

    log "INFO" "Starting failure-injection container"
    BAD_CID="$(start_container "ft-e2e-fail-$suffix" "1")"
    BAD_PORT="$(container_port "$BAD_CID")"
    [[ -n "$BAD_PORT" ]] || die "Failed to detect docker mapped port for failure container"
    register_host_alias "$bad_alias" "$BAD_PORT"
    chmod 600 "$USER_SSH_CONFIG"

    assert_alias_localhost "$good_alias" "$GOOD_PORT"
    assert_alias_localhost "$bad_alias" "$BAD_PORT"

    log "INFO" "Waiting for SSH readiness"
    wait_for_ssh "$good_alias" 30 || die "Timed out waiting for SSH on good container"
    wait_for_ssh "$bad_alias" 30 || die "Timed out waiting for SSH on failure container"
    assert_container_sentinel "$good_alias"
    assert_container_sentinel "$bad_alias"

    log "INFO" "Run 1/4: dry-run"
    run_ft_setup "dry-run" "$good_alias" "$SCENARIO_DIR/setup_remote_dry_run.log"
    grep -q "(dry run)" "$SCENARIO_DIR/setup_remote_dry_run.log" \
        || die "Dry-run output missing '(dry run)' marker"

    log "INFO" "Run 2/4: apply"
    run_ft_setup "apply" "$good_alias" "$SCENARIO_DIR/setup_remote_apply.log"
    capture_remote_service "$good_alias" "$SCENARIO_DIR/service_unit_after_apply_1.service"
    assert_remote_state "$good_alias" || die "Expected remote service+linger state after apply"

    log "INFO" "Run 3/4: apply again (idempotency)"
    run_ft_setup "apply" "$good_alias" "$SCENARIO_DIR/setup_remote_apply_2.log"
    capture_remote_service "$good_alias" "$SCENARIO_DIR/service_unit_after_apply_2.service"
    if diff -u \
        "$SCENARIO_DIR/service_unit_after_apply_1.service" \
        "$SCENARIO_DIR/service_unit_after_apply_2.service" \
        > "$SCENARIO_DIR/service_unit_idempotency.diff"; then
        debug "Service unit unchanged on second apply"
    else
        die "Service unit changed on second apply"
    fi

    capture_remote_snapshot "$good_alias" "$SCENARIO_DIR/remote_filesystem_snapshot.tar"

    log "INFO" "Run 4/4: failure injection (expect non-zero + rollback hint)"
    if run_ft_setup "apply" "$bad_alias" "$SCENARIO_DIR/setup_remote_failure_injected.log"; then
        die "Failure injection run unexpectedly succeeded"
    fi
    grep -qi "rollback" "$SCENARIO_DIR/setup_remote_failure_injected.log" \
        || die "Failure-injection output missing rollback guidance"

    cat > "$SCENARIO_DIR/setup_remote_docker_summary.json" <<EOF
{
  "scenario": "setup_remote_docker",
  "good_host_alias": "$good_alias",
  "good_port": $GOOD_PORT,
  "failure_host_alias": "$bad_alias",
  "failure_port": $BAD_PORT,
  "dry_run_log": "setup_remote_dry_run.log",
  "apply_log": "setup_remote_apply.log",
  "idempotent_log": "setup_remote_apply_2.log",
  "failure_injection_log": "setup_remote_failure_injected.log",
  "service_unit_files": [
    "service_unit_after_apply_1.service",
    "service_unit_after_apply_2.service"
  ],
  "remote_snapshot": "remote_filesystem_snapshot.tar"
}
EOF

    log "INFO" "Remote setup docker E2E completed"
}

main "$@"
