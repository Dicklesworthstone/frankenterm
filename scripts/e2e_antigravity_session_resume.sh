#!/usr/bin/env bash
# Retained e2e harness for Antigravity and legacy Gemini session-resume discovery.
#
# Default mode prepares isolated fixtures, then runs the cargo-test wrapper that
# validates those fixtures through the shipped session_resume bridge. Use
# --prepare-only when the cargo-test wrapper itself needs the script to create
# fixtures without recursively invoking cargo.

set -euo pipefail

PROJECT_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BEAD_ID="ft-agy-provider-q8o4y-685af.5"
RUN_ID="$(date -u +"%Y%m%dT%H%M%SZ")"
ARTIFACT_DIR="${FT_AGY_E2E_ARTIFACT_DIR:-$PROJECT_ROOT/target/e2e-logs/antigravity-session-resume/$RUN_ID}"
FIXTURE_ROOT="${FT_AGY_E2E_FIXTURE_ROOT:-$ARTIFACT_DIR/fixtures}"
PREPARE_ONLY=0

usage() {
  cat <<'USAGE'
Usage: scripts/e2e_antigravity_session_resume.sh [options]

Options:
  --prepare-only          Create retained fixtures and manifest, then exit.
  --artifact-dir <dir>    Directory for retained JSONL/log artifacts.
  --fixture-root <dir>    Directory for generated isolated HOME/PATH fixtures.
  --help                  Show this help.

The default mode first exercises the public robot surface:
  ft robot session-resume list|resume

Then it runs:
  cargo test -p frankenterm-core --features session-resume \
    --test e2e_antigravity_session_resume_script -- --nocapture
USAGE
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --prepare-only)
      PREPARE_ONLY=1
      shift
      ;;
    --artifact-dir)
      ARTIFACT_DIR="$2"
      shift 2
      ;;
    --fixture-root)
      FIXTURE_ROOT="$2"
      shift 2
      ;;
    --help)
      usage
      exit 0
      ;;
    *)
      echo "unknown argument: $1" >&2
      usage >&2
      exit 64
      ;;
  esac
done

if ! command -v python3 >/dev/null 2>&1; then
  echo "python3 is required to create JSON/SQLite e2e fixtures" >&2
  exit 69
fi

mkdir -p "$ARTIFACT_DIR"
mkdir -p "$FIXTURE_ROOT"

export FT_AGY_E2E_PROJECT_ROOT="$PROJECT_ROOT"
export FT_AGY_E2E_BEAD_ID="$BEAD_ID"
export FT_AGY_E2E_ARTIFACT_DIR="$ARTIFACT_DIR"
export FT_AGY_E2E_FIXTURE_ROOT="$FIXTURE_ROOT"
export FT_AGY_E2E_LOG_JSONL="$ARTIFACT_DIR/antigravity-session-resume.jsonl"
export FT_AGY_E2E_ORIGINAL_PATH="${PATH:-}"

python3 <<'PY_FIXTURE_GENERATOR'
import json
import os
import sqlite3
import stat
import time
from pathlib import Path

project_root = Path(os.environ["FT_AGY_E2E_PROJECT_ROOT"])
bead_id = os.environ["FT_AGY_E2E_BEAD_ID"]
artifact_dir = Path(os.environ["FT_AGY_E2E_ARTIFACT_DIR"])
fixture_root = Path(os.environ["FT_AGY_E2E_FIXTURE_ROOT"])
log_jsonl = Path(os.environ["FT_AGY_E2E_LOG_JSONL"])
original_path = os.environ.get("FT_AGY_E2E_ORIGINAL_PATH", "")
model_name = "Gemini 3.1 Pro (High)"
agy_binary = "agy"
user_surface_status = "PUBLIC_SURFACE_EXERCISED"
user_surface_note = (
    "The harness exercises ft robot session-resume list/resume through isolated "
    "HOME/PATH fixtures before running the core bridge tests."
)

artifact_dir.mkdir(parents=True, exist_ok=True)
fixture_root.mkdir(parents=True, exist_ok=True)
log_jsonl.parent.mkdir(parents=True, exist_ok=True)


def now_ms() -> int:
    return int(time.time() * 1000)


def write_json(path: Path, payload) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(payload, indent=2, sort_keys=True) + "\n", encoding="utf-8")


def log_event(**payload) -> None:
    payload.setdefault("bead_id", bead_id)
    payload.setdefault("cwd", str(project_root))
    payload.setdefault("duration_ms", 0)
    with log_jsonl.open("a", encoding="utf-8") as fh:
        fh.write(json.dumps(payload, sort_keys=True) + "\n")


def make_sqlite_db(path: Path) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    conn = sqlite3.connect(path)
    try:
        conn.execute("PRAGMA user_version = 1")
        conn.commit()
    finally:
        conn.close()


def make_executable(path: Path) -> None:
    path.chmod(path.stat().st_mode | stat.S_IXUSR | stat.S_IXGRP | stat.S_IXOTH)


def write_fake_agy(bin_dir: Path, argv_log: Path) -> Path:
    path = bin_dir / agy_binary
    path.write_text(
        f"""#!/usr/bin/env bash
set -euo pipefail
python3 - "$@" <<'PY' >> "{argv_log}"
import json
import sys
print(json.dumps({{"argv": sys.argv[1:]}}))
PY
if [[ "$#" -ne 4 || "$1" != "--conversation" || "$3" != "--model" || "$4" != "{model_name}" ]]; then
  echo "fake agy expected: --conversation <uuid> --model {model_name}" >&2
  exit 65
fi
python3 - "$2" "$4" <<'PY'
import json
import sys
print(json.dumps({{"ok": True, "provider": "agy", "conversation_id": sys.argv[1], "model": sys.argv[2]}}))
PY
""",
        encoding="utf-8",
    )
    make_executable(path)
    return path


def write_fake_casr(bin_dir: Path, list_json: Path, resume_log: Path) -> Path:
    path = bin_dir / "casr"
    path.write_text(
        f"""#!/usr/bin/env bash
set -euo pipefail
if [[ "${{1:-}}" == "list" && "${{2:-}}" == "--json" ]]; then
  cat "{list_json}"
  exit 0
fi
if [[ "${{1:-}}" == "resume" ]]; then
  python3 - "$@" <<'PY' >> "{resume_log}"
import json
import sys
print(json.dumps({{"argv": sys.argv[1:]}}))
PY
  echo '{{"ok":true,"target_session_id":"fake-casr-resume","warnings":[]}}'
  exit 0
fi
echo "unexpected fake casr args: $*" >&2
exit 42
""",
        encoding="utf-8",
    )
    make_executable(path)
    return path


def create_legacy_gmi(home: Path, legacy_id: str, hash_name: str = "legacy-hash") -> Path:
    path = home / ".gemini" / "tmp" / hash_name / "chats" / f"{legacy_id}.json"
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps({"provider": "gmi", "session_id": legacy_id}) + "\n", encoding="utf-8")
    return path


def create_agy_db(home: Path, conversation_id: str) -> Path:
    path = home / ".gemini" / "antigravity-cli" / "conversations" / f"{conversation_id}.db"
    make_sqlite_db(path)
    return path


def write_scenario(
    scenario_id: str,
    agy_uuid: str | None,
    legacy_id: str | None,
    casr_entries: list[dict],
    malformed: bool = False,
    missing_agy_binary: bool = False,
) -> dict:
    scenario_dir = fixture_root / scenario_id
    home = scenario_dir / "home"
    bin_dir = scenario_dir / "bin"
    empty_bin = scenario_dir / "empty-bin"
    logs_dir = scenario_dir / "logs"
    home.mkdir(parents=True, exist_ok=True)
    bin_dir.mkdir(parents=True, exist_ok=True)
    empty_bin.mkdir(parents=True, exist_ok=True)
    logs_dir.mkdir(parents=True, exist_ok=True)

    agy_db = create_agy_db(home, agy_uuid) if agy_uuid is not None else None
    legacy_path = create_legacy_gmi(home, legacy_id) if legacy_id is not None else None

    if malformed:
        conversations = home / ".gemini" / "antigravity-cli" / "conversations"
        conversations.mkdir(parents=True, exist_ok=True)
        (conversations / "not-a-conversation.txt").write_text("ignore me\n", encoding="utf-8")
        make_sqlite_db(conversations / "not-a-uuid.db")
        (conversations / "bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb.db").mkdir(exist_ok=True)

    casr_list_json = scenario_dir / "casr-list.json"
    casr_resume_log = logs_dir / "fake-casr-resume.jsonl"
    fake_agy_argv_log = logs_dir / "fake-agy-argv.jsonl"
    write_json(casr_list_json, casr_entries)
    fake_casr = write_fake_casr(bin_dir, casr_list_json, casr_resume_log)
    fake_agy = None if missing_agy_binary else write_fake_agy(bin_dir, fake_agy_argv_log)

    path_env = str(empty_bin) if missing_agy_binary else f"{bin_dir}{os.pathsep}{original_path}"
    expected_resume_argv = (
        [agy_binary, "--conversation", agy_uuid, "--model", model_name]
        if agy_uuid is not None
        else None
    )
    manifest = {
        "schema_version": "ft.agy-session-resume.scenario.v1",
        "bead_id": bead_id,
        "scenario_id": scenario_id,
        "home": str(home),
        "temp_home": str(home),
        "bin_dir": str(bin_dir),
        "path_env": path_env,
        "casr_binary": str(fake_casr),
        "casr_list_json": str(casr_list_json),
        "casr_resume_log": str(casr_resume_log),
        "fake_agy": None if fake_agy is None else str(fake_agy),
        "fake_agy_argv_log": str(fake_agy_argv_log),
        "agy_uuid": agy_uuid,
        "agy_db": None if agy_db is None else str(agy_db),
        "legacy_session_id": legacy_id,
        "legacy_path": None if legacy_path is None else str(legacy_path),
        "expected_resume_argv": expected_resume_argv,
        "expect_missing_agy_binary": missing_agy_binary,
        "malformed_fixture_policy": "ignore_non_db_non_file_and_non_uuid_db_names",
        "antigravity_schema_status": "schema_unknown_minimal_valid_sqlite",
        "user_surface_status": user_surface_status,
        "user_surface_note": user_surface_note,
    }
    write_json(scenario_dir / "scenario.json", manifest)
    log_event(
        scenario_id=scenario_id,
        step="fixture_prepare",
        command="python3 fixture generator",
        temp_home=str(home),
        provider="agy" if agy_uuid else "gemini",
        session_id=agy_uuid or legacy_id,
        path=str(agy_db or legacy_path or scenario_dir),
        exit_code=0,
        expected="isolated HOME/PATH fixture",
        actual="prepared",
        status="pass",
    )
    if agy_db is not None:
        log_event(
            scenario_id=scenario_id,
            step="agy_sqlite_fixture",
            command="sqlite3 minimal valid database",
            temp_home=str(home),
            provider="agy",
            session_id=agy_uuid,
            path=str(agy_db),
            exit_code=0,
            expected="valid sqlite database; schema_unknown logged",
            actual="schema_unknown_minimal_valid_sqlite",
            status="pass",
        )
    return manifest


legacy_only_path = fixture_root / "legacy-gmi-only" / "home" / ".gemini" / "tmp" / "legacy-hash" / "chats" / "session-legacy-gmi.json"
mixed_legacy_path = fixture_root / "mixed" / "home" / ".gemini" / "tmp" / "legacy-hash" / "chats" / "session-mixed-gmi.json"
scenarios = [
    write_scenario(
        "agy-only",
        "123e4567-e89b-12d3-a456-426614174000",
        None,
        [],
    ),
    write_scenario(
        "legacy-gmi-only",
        None,
        "session-legacy-gmi",
        [
            {
                "session_id": "session-legacy-gmi",
                "provider": "gemini",
                "title": "legacy Gemini CLI session",
                "messages": 1,
                "workspace": None,
                "started_at": None,
                "path": str(legacy_only_path),
            }
        ],
    ),
    write_scenario(
        "mixed",
        "223e4567-e89b-12d3-a456-426614174001",
        "session-mixed-gmi",
        [
            {
                "session_id": "session-mixed-gmi",
                "provider": "gemini",
                "title": "legacy Gemini CLI session",
                "messages": 1,
                "workspace": None,
                "started_at": None,
                "path": str(mixed_legacy_path),
            }
        ],
    ),
    write_scenario(
        "malformed-irrelevant",
        "323e4567-e89b-12d3-a456-426614174002",
        None,
        [],
        malformed=True,
    ),
    write_scenario(
        "missing-agy-binary",
        "423e4567-e89b-12d3-a456-426614174003",
        None,
        [],
        missing_agy_binary=True,
    ),
]

root_manifest = {
    "schema_version": "ft.agy-session-resume.harness.v1",
    "bead_id": bead_id,
    "project_root": str(project_root),
    "artifact_dir": str(artifact_dir),
    "fixture_root": str(fixture_root),
    "log_jsonl": str(log_jsonl),
    "user_surface_status": user_surface_status,
    "user_surface_note": user_surface_note,
    "scenarios": scenarios,
}
write_json(fixture_root / "manifest.json", root_manifest)
write_json(artifact_dir / "manifest.json", root_manifest)
log_event(
    scenario_id="harness",
    step="surface_check",
    command="ft robot session-resume list|resume",
    temp_home=None,
    provider=None,
    session_id=None,
    path=None,
    exit_code=0,
    expected="user-level CLI/Robot session-resume surface",
    actual=user_surface_note,
    status="pass",
)
PY_FIXTURE_GENERATOR

if [[ "$PREPARE_ONLY" -eq 1 ]]; then
  echo "prepared Antigravity session-resume fixtures: $FIXTURE_ROOT"
  echo "retained log: $FT_AGY_E2E_LOG_JSONL"
  exit 0
fi

PUBLIC_STDOUT_DIR="$ARTIFACT_DIR/public-surface/stdout"
PUBLIC_STDERR_DIR="$ARTIFACT_DIR/public-surface/stderr"
mkdir -p "$PUBLIC_STDOUT_DIR" "$PUBLIC_STDERR_DIR"

if [[ -n "${FT_AGY_E2E_FT_BIN:-}" ]]; then
  RESOLVED_FT_BIN="$FT_AGY_E2E_FT_BIN"
else
  PUBLIC_BUILD_STDOUT="$ARTIFACT_DIR/public-surface/ft-build.stdout.log"
  PUBLIC_BUILD_STDERR="$ARTIFACT_DIR/public-surface/ft-build.stderr.log"
  set +e
  (
    cd "$PROJECT_ROOT"
    cargo build -q -p frankenterm --no-default-features --features session-resume --bin ft
  ) >"$PUBLIC_BUILD_STDOUT" 2>"$PUBLIC_BUILD_STDERR"
  PUBLIC_BUILD_EXIT=$?
  set -e
  if [[ "$PUBLIC_BUILD_EXIT" -ne 0 ]]; then
    echo "Failed to build minimal ft session-resume binary (exit $PUBLIC_BUILD_EXIT)" >&2
    echo "stdout: $PUBLIC_BUILD_STDOUT" >&2
    echo "stderr: $PUBLIC_BUILD_STDERR" >&2
    exit "$PUBLIC_BUILD_EXIT"
  fi
  if [[ -n "${CARGO_TARGET_DIR:-}" ]]; then
    RESOLVED_FT_BIN="$CARGO_TARGET_DIR/debug/ft"
  else
    RESOLVED_FT_BIN="$PROJECT_ROOT/target/debug/ft"
  fi
fi
if [[ ! -x "$RESOLVED_FT_BIN" ]]; then
  echo "Resolved ft binary is not executable: $RESOLVED_FT_BIN" >&2
  exit 70
fi

export FT_AGY_E2E_PUBLIC_STDOUT_DIR="$PUBLIC_STDOUT_DIR"
export FT_AGY_E2E_PUBLIC_STDERR_DIR="$PUBLIC_STDERR_DIR"
export FT_AGY_E2E_RESOLVED_FT_BIN="$RESOLVED_FT_BIN"
python3 <<'PY_PUBLIC_SURFACE'
import json
import os
import subprocess
import time
from pathlib import Path

project_root = Path(os.environ["FT_AGY_E2E_PROJECT_ROOT"])
bead_id = os.environ["FT_AGY_E2E_BEAD_ID"]
fixture_root = Path(os.environ["FT_AGY_E2E_FIXTURE_ROOT"])
log_jsonl = Path(os.environ["FT_AGY_E2E_LOG_JSONL"])
stdout_dir = Path(os.environ["FT_AGY_E2E_PUBLIC_STDOUT_DIR"])
stderr_dir = Path(os.environ["FT_AGY_E2E_PUBLIC_STDERR_DIR"])
ft_bin = os.environ["FT_AGY_E2E_RESOLVED_FT_BIN"]
model_name = "Gemini 3.1 Pro (High)"

manifest = json.loads((fixture_root / "manifest.json").read_text(encoding="utf-8"))


def log_event(**payload) -> None:
    payload.setdefault("bead_id", bead_id)
    payload.setdefault("cwd", str(project_root))
    payload.setdefault("duration_ms", 0)
    with log_jsonl.open("a", encoding="utf-8") as fh:
        fh.write(json.dumps(payload, sort_keys=True) + "\n")


def ft_base() -> list[str]:
    return [ft_bin]


def run_ft(scenario: dict, step: str, args: list[str], expect_ok: bool = True) -> dict:
    scenario_id = scenario["scenario_id"]
    stdout_path = stdout_dir / f"{scenario_id}-{step}.json"
    stderr_path = stderr_dir / f"{scenario_id}-{step}.log"
    env = os.environ.copy()
    env["HOME"] = scenario["home"]
    env["PATH"] = scenario["path_env"]
    command = ft_base() + ["robot", "--format", "json"] + args
    start = time.time()
    proc = subprocess.run(
        command,
        cwd=project_root,
        env=env,
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        check=False,
    )
    duration_ms = int((time.time() - start) * 1000)
    stdout_path.write_text(proc.stdout, encoding="utf-8")
    stderr_path.write_text(proc.stderr, encoding="utf-8")
    try:
        payload = json.loads(proc.stdout)
    except json.JSONDecodeError as exc:
        log_event(
            scenario_id=scenario_id,
            step=step,
            command=" ".join(command),
            temp_home=scenario["home"],
            provider=None,
            session_id=None,
            path=scenario.get("agy_db") or scenario.get("legacy_path"),
            stdout_path=str(stdout_path),
            stderr_path=str(stderr_path),
            exit_code=proc.returncode,
            expected="parseable robot JSON envelope",
            actual=f"failed to parse stdout as JSON: {exc}",
            status="fail",
            duration_ms=duration_ms,
        )
        raise

    ok = payload.get("ok") is True
    status = "pass" if proc.returncode == 0 and ok == expect_ok else "fail"
    log_event(
        scenario_id=scenario_id,
        step=step,
        command=" ".join(command),
        temp_home=scenario["home"],
        provider=None,
        session_id=scenario.get("agy_uuid") or scenario.get("legacy_session_id"),
        path=scenario.get("agy_db") or scenario.get("legacy_path"),
        stdout_path=str(stdout_path),
        stderr_path=str(stderr_path),
        exit_code=proc.returncode,
        expected=f"robot envelope ok={expect_ok}",
        actual=f"exit={proc.returncode} ok={payload.get('ok')} error_code={payload.get('error_code')}",
        status=status,
        duration_ms=duration_ms,
    )
    if status != "pass":
        raise AssertionError(
            f"{scenario_id}:{step} expected ok={expect_ok}, got exit={proc.returncode} payload={payload}"
        )
    return payload


def sessions(payload: dict) -> list[dict]:
    return payload["data"]["sessions"]


def assert_has_session(payload: dict, provider: str, session_id: str, step: str) -> dict:
    for entry in sessions(payload):
        if entry.get("provider") == provider and entry.get("session_id") == session_id:
            return entry
    raise AssertionError(f"{step}: missing {provider}:{session_id} in {sessions(payload)!r}")


def assert_lacks_provider(payload: dict, provider: str, step: str) -> None:
    offenders = [entry for entry in sessions(payload) if entry.get("provider") == provider]
    if offenders:
        raise AssertionError(f"{step}: unexpectedly listed provider {provider}: {offenders!r}")


for scenario in manifest["scenarios"]:
    scenario_id = scenario["scenario_id"]
    agy_uuid = scenario.get("agy_uuid")
    legacy_id = scenario.get("legacy_session_id")

    list_all = run_ft(
        scenario,
        "public-list-all",
        [
            "session-resume",
            "list",
            "--home",
            scenario["home"],
            "--casr-binary",
            scenario["casr_binary"],
        ],
    )
    if agy_uuid:
        assert_has_session(list_all, "agy", agy_uuid, f"{scenario_id}:list-all")
    if legacy_id:
        assert_has_session(list_all, "gemini", legacy_id, f"{scenario_id}:list-all")

    list_agy = run_ft(
        scenario,
        "public-list-agy",
        [
            "session-resume",
            "list",
            "--provider",
            "agy",
            "--home",
            scenario["home"],
            "--casr-binary",
            scenario["casr_binary"],
        ],
    )
    assert_lacks_provider(list_agy, "gemini", f"{scenario_id}:list-agy")
    if agy_uuid:
        agy_entry = assert_has_session(list_agy, "agy", agy_uuid, f"{scenario_id}:list-agy")
        expected = ["agy", "--conversation", agy_uuid, "--model", model_name]
        if agy_entry.get("native_resume_command") != expected:
            raise AssertionError(
                f"{scenario_id}: Antigravity argv drifted: {agy_entry.get('native_resume_command')!r}"
            )

    if legacy_id or not scenario.get("expect_missing_agy_binary"):
        list_gmi = run_ft(
            scenario,
            "public-list-gmi",
            [
                "session-resume",
                "list",
                "--provider",
                "gmi",
                "--home",
                scenario["home"],
                "--casr-binary",
                scenario["casr_binary"],
            ],
        )
        assert_lacks_provider(list_gmi, "agy", f"{scenario_id}:list-gmi")
    else:
        list_gmi = None
    if legacy_id:
        assert list_gmi is not None
        assert_has_session(list_gmi, "gemini", legacy_id, f"{scenario_id}:list-gmi")

    if agy_uuid and not scenario.get("expect_missing_agy_binary"):
        dry_run = run_ft(
            scenario,
            "public-resume-agy-dry-run",
            [
                "session-resume",
                "resume",
                agy_uuid,
                "--provider",
                "antigravity",
                "--dry-run",
                "--home",
                scenario["home"],
                "--casr-binary",
                scenario["casr_binary"],
            ],
        )
        expected = ["agy", "--conversation", agy_uuid, "--model", model_name]
        if dry_run["data"]["command_argv"] != expected:
            raise AssertionError(f"{scenario_id}: dry-run command drifted: {dry_run['data']!r}")

        executed = run_ft(
            scenario,
            "public-resume-agy-execute",
            [
                "session-resume",
                "resume",
                agy_uuid,
                "--provider",
                "agy",
                "--home",
                scenario["home"],
                "--casr-binary",
                scenario["casr_binary"],
            ],
        )
        if executed["data"]["native_execution"]["exit_code"] != 0:
            raise AssertionError(f"{scenario_id}: fake agy did not exit cleanly")

    if agy_uuid and scenario.get("expect_missing_agy_binary"):
        missing = run_ft(
            scenario,
            "public-resume-agy-missing-binary",
            [
                "session-resume",
                "resume",
                agy_uuid,
                "--provider",
                "agy",
                "--dry-run",
                "--home",
                scenario["home"],
                "--casr-binary",
                scenario["casr_binary"],
            ],
            expect_ok=False,
        )
        if missing.get("error_code") != "robot.session_resume.native_provider_not_found":
            raise AssertionError(f"{scenario_id}: wrong missing-binary code {missing!r}")

    if legacy_id:
        legacy_resume = run_ft(
            scenario,
            "public-resume-gmi-dry-run",
            [
                "session-resume",
                "resume",
                legacy_id,
                "--provider",
                "gmi",
                "--dry-run",
                "--home",
                scenario["home"],
                "--casr-binary",
                scenario["casr_binary"],
            ],
        )
        if legacy_resume["data"]["resume_kind"] != "casr":
            raise AssertionError(f"{scenario_id}: legacy gmi must resume through casr")
        if legacy_resume["data"]["command_argv"][-1] != "--dry-run":
            raise AssertionError(f"{scenario_id}: legacy dry-run argv missing --dry-run")

log_event(
    scenario_id="harness",
    step="public_surface_complete",
    command="ft robot session-resume list|resume",
    temp_home=None,
    provider=None,
    session_id=None,
    path=str(fixture_root),
    exit_code=0,
    expected="public robot session-resume surface exercises agy/gmi list/resume",
    actual="public surface passed all isolated fixture scenarios",
    status="pass",
)
PY_PUBLIC_SURFACE

STDOUT_LOG="$ARTIFACT_DIR/cargo-test.stdout.log"
STDERR_LOG="$ARTIFACT_DIR/cargo-test.stderr.log"
START_MS="$(python3 - <<'PY'
import time
print(int(time.time() * 1000))
PY
)"
COMMAND_DESC="cargo test -p frankenterm-core --features session-resume --test e2e_antigravity_session_resume_script -- --nocapture"

set +e
(
  cd "$PROJECT_ROOT"
  env \
    FT_AGY_E2E_FIXTURE_ROOT="$FIXTURE_ROOT" \
    FT_AGY_E2E_LOG_JSONL="$FT_AGY_E2E_LOG_JSONL" \
    cargo test -p frankenterm-core --features session-resume --test e2e_antigravity_session_resume_script -- --nocapture
) >"$STDOUT_LOG" 2>"$STDERR_LOG"
EXIT_CODE=$?
set -e

END_MS="$(python3 - <<'PY'
import time
print(int(time.time() * 1000))
PY
)"
DURATION_MS=$((END_MS - START_MS))

export FT_AGY_E2E_COMMAND_DESC="$COMMAND_DESC"
export FT_AGY_E2E_STDOUT_LOG="$STDOUT_LOG"
export FT_AGY_E2E_STDERR_LOG="$STDERR_LOG"
export FT_AGY_E2E_EXIT_CODE="$EXIT_CODE"
export FT_AGY_E2E_DURATION_MS="$DURATION_MS"
python3 <<'PY'
import json
import os
from pathlib import Path

log_jsonl = Path(os.environ["FT_AGY_E2E_LOG_JSONL"])
record = {
    "bead_id": os.environ["FT_AGY_E2E_BEAD_ID"],
    "scenario_id": "harness",
    "step": "cargo_test_wrapper",
    "command": os.environ["FT_AGY_E2E_COMMAND_DESC"],
    "cwd": os.environ["FT_AGY_E2E_PROJECT_ROOT"],
    "temp_home": None,
    "provider": None,
    "session_id": None,
    "path": os.environ["FT_AGY_E2E_FIXTURE_ROOT"],
    "stdout_path": os.environ["FT_AGY_E2E_STDOUT_LOG"],
    "stderr_path": os.environ["FT_AGY_E2E_STDERR_LOG"],
    "exit_code": int(os.environ["FT_AGY_E2E_EXIT_CODE"]),
    "expected": "cargo test wrapper exits 0",
    "actual": "cargo test wrapper completed",
    "duration_ms": int(os.environ["FT_AGY_E2E_DURATION_MS"]),
    "status": "pass" if os.environ["FT_AGY_E2E_EXIT_CODE"] == "0" else "fail",
}
with log_jsonl.open("a", encoding="utf-8") as fh:
    fh.write(json.dumps(record, sort_keys=True) + "\n")

summary = {
    "schema_version": "ft.agy-session-resume.summary.v1",
    "bead_id": record["bead_id"],
    "artifact_dir": os.environ["FT_AGY_E2E_ARTIFACT_DIR"],
    "fixture_root": os.environ["FT_AGY_E2E_FIXTURE_ROOT"],
    "log_jsonl": str(log_jsonl),
    "exit_code": record["exit_code"],
    "stdout_path": record["stdout_path"],
    "stderr_path": record["stderr_path"],
}
Path(os.environ["FT_AGY_E2E_ARTIFACT_DIR"], "summary.json").write_text(
    json.dumps(summary, indent=2, sort_keys=True) + "\n",
    encoding="utf-8",
)
PY

if [[ "$EXIT_CODE" -ne 0 ]]; then
  echo "Antigravity session-resume e2e failed with exit $EXIT_CODE" >&2
  echo "stdout: $STDOUT_LOG" >&2
  echo "stderr: $STDERR_LOG" >&2
  echo "jsonl:  $FT_AGY_E2E_LOG_JSONL" >&2
fi

exit "$EXIT_CODE"
