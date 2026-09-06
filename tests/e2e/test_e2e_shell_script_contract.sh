#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
BEAD_ID="ft-gv7y2.4"
SCENARIO_ID="e2e_shell_script_contract"
RUN_ID="${FT_GV7Y2_4_RUN_ID:-$(date -u +"%Y%m%dT%H%M%SZ")}"
ARTIFACT_DIR="${FT_GV7Y2_4_ARTIFACT_DIR:-${ROOT_DIR}/tests/e2e/artifacts/static-proof/${BEAD_ID}/${SCENARIO_ID}/${RUN_ID}}"
STRUCTURED_LOG="${ARTIFACT_DIR}/static-attestation.jsonl"
SUMMARY_FILE="${ARTIFACT_DIR}/summary.json"
COMMANDS_FILE="${ARTIFACT_DIR}/commands.txt"

mkdir -p "${ARTIFACT_DIR}"
: >"${STRUCTURED_LOG}"
: >"${COMMANDS_FILE}"

record_command() {
    printf '%s\n' "$*" >>"${COMMANDS_FILE}"
}

record_command "bash -n tests/e2e/test_e2e_shell_script_contract.sh"
bash -n "${BASH_SOURCE[0]}"

record_command "python3 isolated retention and smoke identity contracts (extracted production shell; no real actors)"
python3 - "${ROOT_DIR}" "${ARTIFACT_DIR}" <<'PY'
import hashlib
import itertools
import json
import os
from pathlib import Path
import re
import shlex
import subprocess
import sys
import tempfile

root, artifacts = map(Path, sys.argv[1:])
work = Path(tempfile.mkdtemp(prefix="retention-", dir=artifacts))
checks = []
paths = ["scripts/e2e_test.sh", "scripts/lib/e2e_artifacts.sh",
         "scripts/test_e2e_artifacts.sh", "tests/e2e/test_e2e_shell_script_contract.sh",
         "docs/e2e-harness-spec.md", "scripts/smoke/headless-mux-observe.sh"]
sources = {path: (root / path).read_text() for path in paths}
baseline_ref = os.environ.get("FT_E2E_RETENTION_BASELINE_REF", "")
if baseline_ref:
    for path in paths[:3]:
        sources[path] = subprocess.run(["git", "show", f"{baseline_ref}:{path}"], cwd=root,
                                       check=True, capture_output=True, text=True).stdout
(work / "source-manifest.json").write_text(json.dumps({
    "baseline_ref": baseline_ref or None,
    "source_sha256": {path: hashlib.sha256(text.encode()).hexdigest() for path, text in sources.items()},
}, indent=2) + "\n")
harness = sources[paths[0]]
packer = sources[paths[1]]

def check(ok, name):
    checks.append({"check": name, "passed": bool(ok)})
    with (work / "checks.jsonl").open("a") as stream:
        stream.write(json.dumps(checks[-1]) + "\n")
    if not ok:
        raise AssertionError(name)

def function(text, name, indent=""):
    start = text.index(f"{indent}{name}() {{\n")
    end = text.index(f"\n{indent}}}", start) + len(indent) + 2
    return text[start:end] + "\n"

def no_deletion(text):
    return not re.search(r"(?m)^\s*(?:command\s+)?(?:rm|rmdir|unlink)\s", text)

for path in paths[:3]:
    check(no_deletion(sources[path]), f"no_deletion:{path}")
check("e2e_github_summary" not in packer and "GITHUB_STEP_SUMMARY" not in packer,
      "obsolete_actions_summary_removed")
check(not no_deletion('cleanup() {\n    rm -rf "$fixture"\n}\n'),
      "planted_deletion_is_rejected_without_execution")

prelude = r'''
set -euo pipefail
record() { printf '%s' "$1" >> "$CALLS"; shift; printf '\t%s' "$@" >> "$CALLS"; printf '\n' >> "$CALLS"; }
kill() { record kill "$@"; }
wait() { record wait "$@"; }
wezterm() { record wezterm "$@"; }
timeout() { record timeout "$@"; }
vim() { record vim "$@"; }
tmux() { record tmux "$@"; [[ "${TMUX_CREATE_FAIL:-0}" != 1 || " $* " != *' new-session '* ]]; }
rm() { record forbidden_rm "$@"; return 97; }
rmdir() { record forbidden_rmdir "$@"; return 97; }
unlink() { record forbidden_unlink "$@"; return 97; }
log_info() { printf '%s\n' "$*"; }
log_verbose() { :; }
log_warn() { printf '%s\n' "$*"; }
log_pass() { :; }
log_fail() { :; }
info() { printf '%s\n' "$*"; }
check_pass() { printf '%s\n' "$*"; }
check_fail() { printf '%s\n' "$*"; }
'''

def run(label, body, variables=None, expected=0):
    directory = Path(tempfile.mkdtemp(prefix=label + "-", dir=work))
    calls = directory / "calls.tsv"
    env = os.environ.copy()
    env.update({"CALLS": str(calls), "TMUX_CREATE_FAIL": "0"})
    env.update({k: str(v) for k, v in (variables or {}).items()})
    result = subprocess.run(["bash", "-c", prelude + body], env=env,
                            capture_output=True, text=True, timeout=15)
    (directory / "stdout.log").write_text(result.stdout)
    (directory / "stderr.log").write_text(result.stderr)
    check(result.returncode == expected, f"{label}:exit={expected}")
    recorded = [line.split("\t") for line in calls.read_text().splitlines()] if calls.exists() else []
    check(not any(row[0].startswith("forbidden_") for row in recorded), f"{label}:no_deletion_call")
    return result, recorded

retain = function(harness, "retain_workspace_artifacts")
cleanup = function(harness, "cleanup_artifacts")
seed = work / "root evidence with spaces"
seed.mkdir()
marker = seed / "seed.bin"
marker.write_bytes(b"retained\x00evidence\n")
for keep, failed, preserve in itertools.product(["false", "true"], [0, 1], [0, 1]):
    result, _ = run("root", cleanup + "cleanup_artifacts\n",
                    {"RUN_ARTIFACTS_DIR": seed, "KEEP_ARTIFACTS": keep,
                     "FAILED": failed, "FT_E2E_PRESERVE_TEMP": preserve})
    check(marker.read_bytes() == b"retained\x00evidence\n", "root:seed_survives")
    check(str(seed) in result.stdout, "root:retained_path_reported")

# This table is independent of shell extraction: exact owned actors are the
# behavioral oracle, including cleanup paths that should address no actors.
actors = {
    "capture_search": (["ft_pid"], ["pane_id"]),
    "search_linting_rebuild": ([], []),
    "natural_language": (["ft_pid"], ["pane_id"]),
    "compaction_workflow": (["ft_pid"], ["pane_id"]),
    "unhandled_event_lifecycle": (["ft_pid"], ["pane_id"]),
    "usage_limit_safe_pause": (["ft_pid", "ft_pid_restart"], ["pane_id"]),
    "notification_webhook": (["mock_pid", "ft_pid"], ["pane_id"]),
    "watch_notify_only": (["mock_pid", "ft_pid"], ["pane_usage", "pane_token", "pane_burst"]),
    "policy_denial": (["ft_pid"], ["pane_id"]),
    "audit_tail": (["tail_pid", "ft_pid"], ["pane_id"]),
    "ipc_rpc_roundtrip": (["ft_pid"], []),
    "prepare_commit_approvals": ([], ["pane_id"]),
    "quickfix_suggestions": (["ft_pid"], ["compaction_pane", "alt_pane"]),
    "triage_multi_issue": ([], []),
    "stress_scale": (["ft_pid"], ["pane_id", "pane_burst"]),
    "graceful_shutdown": (["ft_pid"], ["pane_id"]),
    "pane_exclude_filter": (["ft_pid"], ["observed_pane_id", "ignored_pane_id"]),
    "workspace_isolation": (["ft_pid"], ["pane_a_id", "pane_b_id"]),
    "setup_idempotency": ([], []),
    "uservar_forwarding": (["ft_pid", "wezterm_pid"], ["pane_id"]),
    "workflow_resume": (["ft_pid"], ["pane_id"]),
    "dry_run_mode": (["ft_pid"], ["pane_id"]),
    "workflow_lifecycle": ([], []),
    "events_unhandled_alias": ([], []),
    "events_annotations_triage": ([], []),
    "history_undo_workflow": ([], []),
    "accounts_refresh": ([], []),
    "alt_screen_detection": (["ft_pid", "wezterm_pid"], ["pane_id"]),
    "alt_screen_conformance": (["ft_pid", "wezterm_pid"], ["pane_id", "pane_burst"]),
    "watcher_crash_bundle": (["ft_pid"], []),
    "environment_detection": ([], []),
}
found = re.findall(r"(?m)^    cleanup_(\w+)\(\) \{", harness)
check(set(found) == set(actors), "all_31_cleanup_bodies_have_actor_oracles")
actor_names = sorted({name for processes, panes in actors.values() for name in processes + panes})
actor_values = {name: str(11000 + i) for i, name in enumerate(actor_names)}
cleanup_cases = 0
for name, (processes, panes) in actors.items():
    base = Path(tempfile.mkdtemp(prefix=name + "-", dir=work))
    variables = dict(actor_values)
    seed_files = []
    for variable in ["temp_workspace", "temp_workspace_fail", "temp_workspace_invalid",
                     "temp_home", "workspace_a", "workspace_b"]:
        directory = base / (variable + " with spaces")
        (directory / ".ft").mkdir(parents=True)
        variables[variable] = str(directory)
        for leaf in [".ft/state.db", ".hidden", "seed.bin", "ft.toml", "caut_invocations.log"]:
            path = directory / leaf
            path.write_bytes(b"retained\x00evidence\n")
            seed_files.append(path)
    for variable in ["config_file", "runner_script", "emit_script", "enter_seq_file",
                     "leave_seq_file", "fixture_dummy_script"]:
        variables[variable] = str(seed_files[2])
    variables.update({"old_path": os.environ["PATH"], "wezterm_class": "owned-class",
                      "wezterm_socket": str(base / "owned.socket"), "wezterm_bin": "wezterm"})
    for variable in ["old_ft_data_dir", "old_ft_workspace", "old_ft_config",
                     "old_caut_mode", "old_caut_log", "old_crash_flag"]:
        variables[variable] = ""
    body = retain + function(harness, "cleanup_" + name, "    ")
    body += 'pane_ids=("$pane_id" "$pane_burst"); spawned_panes=("$pane_id" "$pane_burst")\n'
    body += "cleanup_" + name + "\n"
    for keep, failed, preserve in itertools.product(["false", "true"], [0, 1], [0, 1]):
        scenario = base / f"scenario-{keep}-{failed}-{preserve}"
        scenario.mkdir()
        variables.update({"scenario_dir": str(scenario), "KEEP_ARTIFACTS": keep,
                          "FAILED": failed, "FT_E2E_PRESERVE_TEMP": preserve})
        result, calls = run(name, body, variables)
        cleanup_cases += 1
        check(all(path.read_bytes() == b"retained\x00evidence\n" for path in seed_files),
              f"{name}:all_seeded_bytes_survive")
        stopped = [row[-1] for row in calls if row[0] == "kill" and row[1] != "-0"]
        waited = [row[-1] for row in calls if row[0] == "wait"]
        closed = [row[-1] for row in calls if row[0] == "wezterm"]
        check(sorted(stopped) == sorted(actor_values[v] for v in processes), f"{name}:exact_owned_pids")
        check(sorted(waited) == sorted(stopped), f"{name}:owned_waits_preserved")
        check(sorted(closed) == sorted(actor_values[v] for v in panes), f"{name}:exact_owned_panes")

# Run only the writable-directory production block, never its native checks.
probe = harness.split("    # Check 4: Artifacts directory writable\n", 1)[1].split("    # Check 5:", 1)[0]
probe_body = "probe() { local all_passed=true\n" + probe + '\n[[ "$all_passed" == true ]]; }\nprobe\n'
legacy_probe = seed / ".write-test"
legacy_probe.write_bytes(b"existing probe must survive")
for _ in range(2):
    run("write-probe", probe_body, {"ARTIFACTS_DIR": seed})
check(len(list(seed.glob(".write-test.*"))) == 2, "write_probe:unique_and_retained")
check(legacy_probe.read_bytes() == b"existing probe must survive", "write_probe:preexisting_bytes_survive")
run("write-probe-failure", probe_body, {"ARTIFACTS_DIR": marker}, expected=1)

runner = harness.split('    cat > "$runner_script" <<\'EOS\'\n', 1)[1].split("\nEOS", 1)[0]
runner_workspace = work / "runner workspace"
runner_workspace.mkdir()
for failed in [0, 1]:
    result, calls = run("tmux-profile", 'set -- tmux 2\n' + runner,
                        {"FT_WORKSPACE": runner_workspace, "TMUX_CREATE_FAIL": failed}, expected=failed)
    tmux_calls = [row for row in calls if row[0] == "tmux"]
    check(len(tmux_calls) == (1 if failed else 2), "tmux:creation_gates_teardown")
    socket = Path(tmux_calls[0][2])
    check(tmux_calls[0][1] == "-S" and socket.parent.parent == runner_workspace
          and socket.parent.is_dir(), "tmux:fresh_private_socket")
    check(tmux_calls[0][3:8] == ["-f", "/dev/null", "new-session", "-d", "-s"],
          "tmux:never_attach_existing_session_or_load_operator_config")
    check(all(row[1:3] == ["-S", str(socket)] for row in tmux_calls), "tmux:teardown_uses_owned_socket")
    timed = [row for row in calls if row[0] == "timeout"]
    check(len(timed) == (0 if failed else 1), "tmux:failed_creation_never_attaches")
    check(str(socket.parent) in result.stdout, "tmux:workspace_retained_and_reported")
run("vim-profile", 'set -- vim 2\n' + runner, {"FT_WORKSPACE": runner_workspace})
vim_files = list(runner_workspace.glob("ft-alt-vim-*"))
check(len(vim_files) == 1 and vim_files[0].read_text() == "line 1\nline 2\nline 3\n",
      "vim:fixture_retained_in_workspace")

smoke_tail = sources[paths[2]].split("    # Test evidence is retained", 1)[1].split('    echo ""', 1)[0]
smoke_tail = smoke_tail.split("\n", 1)[1]
for flag in [0, 1]:
    result, _ = run("artifact-smoke-retention", smoke_tail,
                    {"E2E_ARTIFACTS_BASE": seed, "E2E_ARTIFACTS_CLEANUP": flag})
    check(marker.read_bytes() == b"retained\x00evidence\n" and str(seed) in result.stdout,
          "artifact_smoke:retention_unconditional")

# Sourcing this library defines functions/configuration only. None of its
# environment collectors, visual detectors, or complete harnesses is invoked.
library = "source " + shlex.quote(str(root / paths[1])) + "\n"
plain = work / "plain.log"
plain.write_bytes(b"plain\x00bytes\n\n")
run("no-match-redaction", library + 'mktemp() { return 96; }; e2e_redact_secrets "$INPUT"\n', {"INPUT": plain})
check(plain.read_bytes() == b"plain\x00bytes\n\n", "redaction:no_match_preserves_bytes_without_allocation")
secret = "A/" * 20
text_file = work / "secret.log"
text_file.write_text("aws_secret_access_key=" + secret + "\npassword=synthetic-secret\n")
run("actual-redaction", library + 'e2e_redact_secrets "$INPUT"\n', {"INPUT": text_file})
check(secret not in text_file.read_text() and "synthetic-secret" not in text_file.read_text()
      and "[REDACTED]" in text_file.read_text(), "redaction:slash_credential_and_password_removed")

failed_file = work / "failed.log"
failed_file.write_text("password=synthetic-secret\n")
failure_body = library + 'perl() { echo PRIVATE_PATTERN_SENTINEL >&2; return 42; }; e2e_redact_secrets "$INPUT" || exit $?\n'
result, _ = run("redactor-failure", failure_body, {"INPUT": failed_file}, expected=1)
check("PRIVATE_PATTERN_SENTINEL" not in result.stderr and "synthetic-secret" not in result.stderr,
      "redaction:engine_errors_are_reason_only")
check(failed_file.read_text() == "password=synthetic-secret\n", "redaction:failed_engine_retains_input")
for mode in ["invalid", "read-error"]:
    injected = ('E2E_REDACT_PATTERNS="[PRIVATE_PATTERN_SENTINEL"\n' if mode == "invalid"
                else 'grep() { echo PRIVATE_PATTERN_SENTINEL >&2; return 2; }\n')
    result, _ = run("pattern-" + mode, library + injected + 'e2e_redact_secrets "$INPUT" || exit $?\n',
                    {"INPUT": plain}, expected=1)
    check("PRIVATE_PATTERN_SENTINEL" not in result.stderr, f"redaction:{mode}_privacy")
result, _ = run("pattern-registration", library + 'E2E_DEBUG=true; e2e_add_redact_pattern PRIVATE_PATTERN_SENTINEL\n')
check("PRIVATE_PATTERN_SENTINEL" not in result.stderr, "redaction:registration_does_not_log_pattern")

packed = work / "packed"
packed.mkdir()
pack_env = {"E2E_RUN_DIR": packed, "E2E_CURRENT_SCENARIO": "", "E2E_REDACT_SECRETS": "true",
            "E2E_MAX_FILE_SIZE": "5000", "INPUT": failed_file}
for producer in ['e2e_add_file result.txt "password=synthetic-secret"',
                 'e2e_add_json result.json \'{"value":"password=synthetic-secret"}\'',
                 'e2e_copy_file "$INPUT" result-copy.txt']:
    run("producer-failure", library + 'e2e_redact_secrets() { return 23; }; ' + producer + ' || exit $?\n',
        pack_env, expected=23)
capture_body = library + r'''
E2E_SCENARIOS_DIR="$E2E_RUN_DIR/scenarios"
mkdir -p "$E2E_SCENARIOS_DIR"
e2e_redact_secrets() { return 23; }
e2e_limit_size() { return 0; }
e2e_capture_scenario redaction_failure printf '' || exit $?
'''
run("capture-failure", capture_body, pack_env, expected=1)
metadata = json.loads((packed / "scenarios/redaction_failure/metadata.json").read_text())
check(metadata["exit_code"] == 1 and metadata["redaction_status"] == "failed"
      and (packed / "scenarios/redaction_failure/FAIL").exists(), "capture:redaction_failure_is_failed_evidence")

run("json-redaction", library + 'e2e_add_json valid.json \'{"value":"password=synthetic-secret","n":2}\'\n', pack_env)
payload = json.loads((packed / "valid.json").read_text())
check(payload == {"value": "[REDACTED]", "n": 2}, "json:redaction_preserves_valid_structure")
escaped = work / "escaped.json"
escaped.write_text('{"value":"pass\\u0077ord=synthetic-secret"}')
run("json-escaped-redaction", library + 'e2e_redact_secrets "$INPUT"\n', {"INPUT": escaped})
check(json.loads(escaped.read_text()) == {"value": "[REDACTED]"}, "json:decoded_credentials_are_matched")
keyed = work / "keyed.json"
keyed_bytes = b'{"password=synthetic-secret":1,"[REDACTED]":2}'
keyed.write_bytes(keyed_bytes)
result, _ = run("json-key-refusal", library + 'e2e_redact_secrets "$INPUT" || exit $?\n',
                {"INPUT": keyed}, expected=1)
check(keyed.read_bytes() == keyed_bytes, "json:matching_key_refused_without_collision_or_data_loss")
check("synthetic-secret" not in result.stderr, "json:key_refusal_logs_no_secret")
run("json-size-refusal", library + 'E2E_MAX_FILE_SIZE=2; e2e_add_json oversized.json \'{"n":2}\' || exit $?\n',
    pack_env, expected=1)
check(json.loads((packed / "oversized.json").read_text()) == {"n": 2}, "json:oversize_input_retained_valid")
run("stat-failure", library + 'stat() { return 42; }; e2e_limit_size "$INPUT" 5000 || exit $?\n',
    {"INPUT": plain}, expected=1)
check(plain.read_bytes() == b"plain\x00bytes\n\n", "stat_failure:input_retained")
run("json-engine-failure", library + 'jq() { echo PRIVATE_PATTERN_SENTINEL >&2; return 42; }; e2e_redact_secrets "$INPUT" || exit $?\n',
    {"INPUT": escaped}, expected=1)
tree = work / "source-tree"
(tree / ".hidden").mkdir(parents=True)
(tree / ".hidden/credentials.txt").write_text("password=synthetic-secret\n")
(tree / "data.json").write_text('{"value":"password=synthetic-secret"}')
(tree / "large.txt").write_text("x" * 10000)
run("directory-copy", library + 'e2e_copy_file "$TREE" safe-tree\n', {**pack_env, "TREE": tree})
check("synthetic-secret" not in (packed / "safe-tree/.hidden/credentials.txt").read_text(), "directory:hidden_file_sanitized")
check(json.loads((packed / "safe-tree/data.json").read_text()) == {"value": "[REDACTED]"}, "directory:json_valid_and_sanitized")
check((packed / "safe-tree/large.txt").stat().st_size <= 5000, "directory:size_limit_applied")
unsafe = work / "linked-tree"
unsafe.mkdir()
(unsafe / "external").symlink_to(plain)
run("directory-symlink-refusal", library + 'e2e_copy_file "$TREE" refused-tree || exit $?\n',
    {**pack_env, "TREE": unsafe}, expected=1)
check(not (packed / "refused-tree").exists() and plain.read_bytes() == b"plain\x00bytes\n\n",
      "directory:external_link_not_followed_or_copied")
special = work / "special-tree"
special.mkdir()
os.mkfifo(special / "pipe")
run("directory-special-refusal", library + 'e2e_copy_file "$TREE" refused-special || exit $?\n',
    {**pack_env, "TREE": special}, expected=1)
check(not (packed / "refused-special").exists(), "directory:special_file_refused_before_copy")
run("directory-redaction-failure", library + 'e2e_redact_secrets() { return 23; }; e2e_copy_file "$TREE" failed-tree || exit $?\n',
    {**pack_env, "TREE": tree}, expected=23)
check((tree / ".hidden/credentials.txt").read_text() == "password=synthetic-secret\n",
      "directory:source_unchanged_after_failed_sanitization")

# Exercise the actual common smoke finish and SHA reader against owned byte
# fixtures. Only version output and step logging are fixture adapters; this
# proves receipt identity rejection, never a real mux/CLI smoke result.
smoke_source = sources["scripts/smoke/headless-mux-observe.sh"]
def smoke_function(name):
    start = smoke_source.index(f"{name}() {{")
    end = smoke_source.index("\n}", start) + 2
    return smoke_source[start:end] + "\n"

smoke_body = smoke_function("file_sha") + smoke_function("finish") + r'''
run_bounded() { printf '%s\n' 'isolated version fixture'; }
log() { printf '%s\n' "$*"; }
step() {
    jq -cn --arg name "$1" --arg status "$2" --arg detail "$3" \
        '{name:$name,status:$status,detail:$detail}' >> "$STEPS"
}
finish pass
'''
for mode, change in itertools.product(["0", "1"], ["unchanged", "cli", "mux", "unreadable"]):
    fixture = Path(tempfile.mkdtemp(prefix="smoke-identity-", dir=work))
    cli, mux = fixture / "ft", fixture / "frankenterm-mux-server"
    cli.write_bytes(b"owned original CLI fixture\n")
    mux.write_bytes(b"owned original mux fixture\n")
    cli_sha = hashlib.sha256(cli.read_bytes()).hexdigest()
    mux_sha = hashlib.sha256(mux.read_bytes()).hexdigest()
    if change == "cli":
        cli.write_bytes(b"replaced CLI bytes\n")
    elif change == "mux":
        mux.write_bytes(b"replaced mux bytes\n")
    elif change == "unreadable":
        # An absent path rejects the real SHA reader even for root; do not
        # delete the original fixture or depend on permission-bit behavior.
        cli = fixture / "missing-cli"
    steps, receipt = fixture / "steps.jsonl", fixture / "receipt.json"
    steps.write_text("")
    run("smoke-identity-" + change, smoke_body, {
        "D": fixture, "FT": cli, "MUX": mux, "PYTHON": sys.executable,
        "CLI_SHA": cli_sha, "MUX_SHA": mux_sha, "STEPS": steps,
        "RECEIPT": receipt, "KILL_SWITCH_SMOKE": mode,
        "RELEASE_COMMIT": "0" * 40, "SOURCE_AUTHORITY": "isolated-shell-fixture",
        "SOCK": fixture / "unused.socket", "CODEC_VERSION": "1", "BIN_DIR": fixture,
    }, expected=0 if change == "unchanged" else 1)
    recorded = json.loads(receipt.read_text())
    expected = "pass" if change == "unchanged" else "fail"
    check(recorded["status"] == expected, f"smoke_identity:{mode}:{change}:receipt_status")
    check(recorded["cli_sha256"] == cli_sha and recorded["mux_sha256"] == mux_sha,
          f"smoke_identity:{mode}:{change}:original_hashes_retained")
    check(len(recorded["steps"]) == 1 and recorded["steps"][0]["name"] == "source_identity"
          and recorded["steps"][0]["status"] == expected,
          f"smoke_identity:{mode}:{change}:explicit_identity_step")

check(all((root / path).read_text() == text for path, text in sources.items()),
      "source_files_unchanged_during_proof")
summary = {"bead_id": "ft-xxfwy.63.1", "status": "passed", "checks": len(checks),
           "cleanup_bodies": len(actors), "cleanup_cases": cleanup_cases,
           "proof_kind": "isolated-shell-boundary", "artifacts": str(work),
           "source_sha256": {path: hashlib.sha256(text.encode()).hexdigest() for path, text in sources.items()}}
(work / "checks.json").write_text(json.dumps(checks, indent=2) + "\n")
(artifacts / "retention-summary.json").write_text(json.dumps(summary, indent=2) + "\n")
print(json.dumps(summary, sort_keys=True))
PY

if [[ "${1:-}" == "--retention-only" ]]; then
    exit 0
fi

# shellcheck source=tests/scripts/static_attestation_helpers.sh
source "${ROOT_DIR}/tests/scripts/static_attestation_helpers.sh"

record_command "ruby static E2E shell script contract verifier"
static_attestation_run_ruby - "${STRUCTURED_LOG}" "${SUMMARY_FILE}" "${ROOT_DIR}" <<'RUBY'
require "open3"

structured_log_path = ARGV.fetch(0)
summary_path = ARGV.fetch(1)
root = ARGV.fetch(2)

log_io = File.open(structured_log_path, "a")
StaticAttestation.configure(log_io: log_io, log_enabled: true)

begin
  git_ls_files = IO.popen(["git", "ls-files", "tests/e2e"], &:read)
  StaticAttestation.assert!(
    $?.success?,
    "failed to enumerate tracked E2E scripts",
    check: "e2e_script_contract.git_ls_files",
    input_path: "tests/e2e",
    expected: "success",
    actual: $?.exitstatus,
  )
  scripts = git_ls_files.lines.map(&:chomp).select { |path| path.end_with?(".sh") }.sort
  readme = StaticAttestation.read_text!("README.md", check: "e2e_script_contract.readme")

  exceptions = {
    "tests/e2e/lib_rch_guards.sh" => {
      "direct_exec" => false,
      "strict_mode" => false,
      "reason" => "source-only RCH guard library; sourced by executable harnesses",
    },
    "tests/e2e/test_frankensearch_integration.sh" => {
      "strict_mode" => false,
      "reason" => "legacy aggregate-failure harness keeps set -u plus pipefail while tallying assertion and infrastructure failures",
    },
    "tests/e2e/test_search_load.sh" => {
      "strict_mode" => false,
      "reason" => "legacy load harness keeps set -u plus pipefail while accumulating per-query failure metrics",
    },
    "tests/e2e/test_search_regression.sh" => {
      "strict_mode" => false,
      "reason" => "legacy regression harness keeps set -u plus pipefail while aggregating query assertions",
    },
  }

  StaticAttestation.assert!(
    readme.include?("<!--count:e2e_scripts-->#{scripts.length}<!--/count-->"),
    "README stamped E2E script count is stale",
    check: "e2e_script_contract.readme_marker_count",
    input_path: "README.md",
    expected: scripts.length,
    actual: readme[/<!--count:e2e_scripts-->(\d+)<!--\/count-->/, 1],
  )
  StaticAttestation.assert!(
    readme.include?("# #{scripts.length} shell E2E scripts"),
    "README tree E2E script count is stale",
    check: "e2e_script_contract.readme_tree_count",
    input_path: "README.md",
    expected: "#{scripts.length} shell E2E scripts",
    actual: readme.include?("# #{scripts.length} shell E2E scripts") ? "present" : "missing",
  )

  counts_snapshot_path = "docs/attestations/doctrine/agents-md-counts.json"
  counts_snapshot = StaticAttestation.read_json!(
    counts_snapshot_path,
    check: "e2e_script_contract.agents_md_counts.snapshot_json",
  )
  StaticAttestation.assert!(
    counts_snapshot.dig("source", "count_source") == "head",
    "agents-md-counts snapshot must be generated from committed HEAD",
    check: "e2e_script_contract.agents_md_counts.source_mode",
    input_path: counts_snapshot_path,
    expected: "head",
    actual: counts_snapshot.dig("source", "count_source"),
  )

  e2e_count_entry = counts_snapshot.fetch("counts", []).find { |entry| entry["name"] == "e2e_scripts" }
  StaticAttestation.assert!(
    !e2e_count_entry.nil?,
    "agents-md-counts snapshot missing e2e_scripts entry",
    check: "e2e_script_contract.agents_md_counts.e2e_entry_present",
    input_path: counts_snapshot_path,
    expected: "e2e_scripts",
    actual: counts_snapshot.fetch("counts", []).map { |entry| entry["name"] },
  )
  readme_e2e_doc = e2e_count_entry.fetch("documents", []).find do |document|
    document["path"] == "README.md" && document["placeholder_present"] == true
  end
  StaticAttestation.assert!(
    !readme_e2e_doc.nil?,
    "agents-md-counts snapshot missing README e2e_scripts document entry",
    check: "e2e_script_contract.agents_md_counts.e2e_readme_entry_present",
    input_path: counts_snapshot_path,
    expected: "README.md",
    actual: e2e_count_entry.fetch("documents", []).map { |document| document["path"] },
  )
  StaticAttestation.assert!(
    e2e_count_entry.fetch("live_value") == scripts.length,
    "agents-md-counts E2E script value is stale",
    check: "e2e_script_contract.agents_md_counts.e2e_value",
    input_path: counts_snapshot_path,
    expected: scripts.length,
    actual: e2e_count_entry.fetch("live_value"),
  )
  StaticAttestation.assert!(
    readme_e2e_doc.fetch("documented_value") == scripts.length,
    "agents-md-counts E2E documented value is stale",
    check: "e2e_script_contract.agents_md_counts.e2e_documented_value",
    input_path: counts_snapshot_path,
    expected: scripts.length,
    actual: readme_e2e_doc.fetch("documented_value"),
  )
  StaticAttestation.assert!(
    readme_e2e_doc.fetch("live_value") == scripts.length,
    "agents-md-counts README E2E live value is stale",
    check: "e2e_script_contract.agents_md_counts.e2e_readme_live_value",
    input_path: counts_snapshot_path,
    expected: scripts.length,
    actual: readme_e2e_doc.fetch("live_value"),
  )
  StaticAttestation.assert!(
    e2e_count_entry.fetch("command").include?("git ls-tree -r --name-only HEAD tests/e2e"),
    "agents-md-counts E2E command must read committed HEAD",
    check: "e2e_script_contract.agents_md_counts.e2e_head_command",
    input_path: counts_snapshot_path,
    expected: "git ls-tree -r --name-only HEAD tests/e2e",
    actual: e2e_count_entry.fetch("command"),
  )

  generated_stdout, generated_stderr, generated_status = Open3.capture3(
    "bash",
    "scripts/stamp-readme-counts.sh",
    "--source=head",
    "--json",
    chdir: root,
  )
  StaticAttestation.assert!(
    generated_status.success?,
    "failed to generate head-sourced agents-md-counts snapshot",
    check: "e2e_script_contract.agents_md_counts.generate_head_snapshot",
    input_path: "scripts/stamp-readme-counts.sh",
    expected: "success",
    actual: {
      "exitstatus" => generated_status.exitstatus,
      "stderr" => generated_stderr,
    },
  )
  begin
    generated_snapshot = JSON.parse(generated_stdout)
  rescue JSON::ParserError => error
    StaticAttestation.fail!(
      "generated head-sourced agents-md-counts snapshot is not valid JSON",
      check: "e2e_script_contract.agents_md_counts.generated_json",
      input_path: "scripts/stamp-readme-counts.sh",
      expected: "valid_json",
      actual: error.message,
    )
  end
  normalize_counts_snapshot = lambda do |payload|
    normalized = JSON.parse(JSON.generate(payload))
    normalized.delete("generated_at")
    normalized
  end
  normalized_counts_snapshot = normalize_counts_snapshot.call(counts_snapshot)
  normalized_generated_snapshot = normalize_counts_snapshot.call(generated_snapshot)
  StaticAttestation.assert!(
    normalized_counts_snapshot == normalized_generated_snapshot,
    "checked-in agents-md-counts snapshot drifted from head-sourced generator output",
    check: "e2e_script_contract.agents_md_counts.normalized_snapshot",
    input_path: counts_snapshot_path,
    expected: Digest::SHA256.hexdigest(JSON.generate(normalized_generated_snapshot)),
    actual: Digest::SHA256.hexdigest(JSON.generate(normalized_counts_snapshot)),
  )

  unknown_exceptions = exceptions.keys - scripts
  StaticAttestation.assert!(
    unknown_exceptions.empty?,
    "E2E script contract exception list contains unknown paths",
    check: "e2e_script_contract.exceptions_known",
    input_path: "tests/e2e",
    expected: [],
    actual: unknown_exceptions,
  )

  direct_exec_required = 0
  direct_exec_exceptions = 0
  strict_required = 0
  strict_exceptions = 0

  scripts.each do |path|
    exception = exceptions.fetch(path, {})
    text = StaticAttestation.read_text!(path, check: "e2e_script_contract.read_script")
    candidate = StaticAttestation.repo_path(path)
    first_line = text.lines.first.to_s.strip
    executable = File.executable?(candidate)
    strict = text.include?("set -euo pipefail")

    StaticAttestation.assert!(
      first_line.start_with?("#!"),
      "#{path} missing shebang",
      check: "e2e_script_contract.shebang",
      input_path: path,
      expected: "#!",
      actual: first_line.empty? ? "empty" : first_line,
    )

    if exception.fetch("direct_exec", true)
      direct_exec_required += 1
      StaticAttestation.assert!(
        executable,
        "#{path} is not directly executable",
        check: "e2e_script_contract.direct_exec",
        input_path: path,
        expected: "executable",
        actual: executable ? "executable" : "not_executable",
      )
    else
      direct_exec_exceptions += 1
      StaticAttestation.log_check(
        "e2e_script_contract.direct_exec_exception",
        input_path: path,
        expected: "not_required",
        actual: executable ? "executable" : "not_executable",
        status: "pass",
      )
    end

    if exception.fetch("strict_mode", true)
      strict_required += 1
      StaticAttestation.assert!(
        strict,
        "#{path} missing set -euo pipefail",
        check: "e2e_script_contract.strict_mode",
        input_path: path,
        expected: "set -euo pipefail",
        actual: strict ? "present" : "missing",
      )
    else
      strict_exceptions += 1
      StaticAttestation.assert!(
        !strict,
        "#{path} strict-mode exception should be removed once strict mode is present",
        check: "e2e_script_contract.strict_mode_exception",
        input_path: path,
        expected: "known_exception_without_set_e",
        actual: strict ? "present" : "missing",
      )
    end

    unless exception.empty?
      StaticAttestation.log_check(
        "e2e_script_contract.known_exception",
        input_path: path,
        expected: exception.fetch("reason"),
        actual: {
          "direct_exec" => executable ? "executable" : "not_executable",
          "strict_mode" => strict ? "present" : "missing",
        },
        status: "pass",
      )
    end
  end

  summary = {
    "bead_id" => "ft-gv7y2.4",
    "scenario_id" => "e2e_shell_script_contract",
    "status" => "passed",
    "script_count" => scripts.length,
    "agents_md_counts_snapshot_checked" => true,
    "agents_md_counts_source" => counts_snapshot.dig("source", "count_source"),
    "shebang_checked_count" => scripts.length,
    "direct_exec_required_count" => direct_exec_required,
    "direct_exec_exception_count" => direct_exec_exceptions,
    "strict_mode_required_count" => strict_required,
    "strict_mode_exception_count" => strict_exceptions,
    "known_exceptions" => exceptions.map do |path, detail|
      {
        "path" => path,
        "direct_exec" => detail.fetch("direct_exec", true),
        "strict_mode" => detail.fetch("strict_mode", true),
        "reason" => detail.fetch("reason"),
      }
    end,
    "artifacts" => {
      "structured_log" => structured_log_path.delete_prefix("#{root}/"),
      "summary" => summary_path.delete_prefix("#{root}/"),
    },
  }

  File.write(summary_path, JSON.pretty_generate(summary) + "\n")
  StaticAttestation.log_check(
    "e2e_script_contract.summary",
    input_path: "tests/e2e",
    expected: "passed",
    actual: summary,
    status: "pass",
  )
  puts "E2E shell script contract: passed (#{scripts.length} scripts, #{direct_exec_exceptions} direct-exec exceptions, #{strict_exceptions} strict-mode exceptions)"
ensure
  log_io.close
end
RUBY
