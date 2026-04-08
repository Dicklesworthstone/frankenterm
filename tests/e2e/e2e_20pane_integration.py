#!/usr/bin/env python3
from __future__ import annotations

import argparse
import json
import os
import shlex
import shutil
import signal
import subprocess
import sys
import tempfile
import time
from dataclasses import dataclass
from datetime import datetime, timezone
from pathlib import Path
from typing import Any


REPO_ROOT = Path(__file__).resolve().parents[2]
OUTPUT_DIR = REPO_ROOT / "tests" / "e2e" / "output"
FIXTURE_DIR = REPO_ROOT / "crates" / "frankenterm-core" / "tests" / "corpus"
SKIP_EXIT_CODE = 77
DEFAULT_TIMEOUT_SECS = 60.0
PANE_DENY_ID = 15


CONFIG_TEMPLATE = """\
[ingest]
poll_interval_ms = 100
batch_size = 50
max_segment_bytes = 65536

[storage]
retention_days = 1

[logging]
level = "debug"
format = "json"

[safety]
require_prompt_active = false
block_alt_screen = false

[safety.rules]
enabled = true

[[safety.rules.rules]]
id = "e2e.robot.deny_pane_15"
description = "Deny robot send-text on pane 15 without impacting workflow actors"
priority = 1
decision = "deny"
message = "Pane 15 is reserved for robot policy denial coverage"

[safety.rules.rules.match_on]
actions = ["send_text"]
actors = ["robot"]
surfaces = ["mux"]
pane_ids = [15]

[workflows]
enabled = ["handle_claude_code_limits", "handle_gemini_quota"]
"""


FAKE_WEZTERM_SCRIPT = """\
#!/usr/bin/env python3
from __future__ import annotations

import contextlib
import fcntl
import json
import os
import pathlib
import signal
import subprocess
import sys
from typing import Any


STATE_ROOT = pathlib.Path(os.environ["FT_FAKE_WEZTERM_STATE"]).resolve()
STATE_ROOT.mkdir(parents=True, exist_ok=True)
LOCK_PATH = STATE_ROOT / "lock"
STATE_PATH = STATE_ROOT / "state.json"
LOG_DIR = STATE_ROOT / "logs"
LOG_DIR.mkdir(parents=True, exist_ok=True)


def load_state() -> dict[str, Any]:
    if not STATE_PATH.exists():
        return {"next_pane_id": 1, "panes": {}, "active_pane_id": None}
    return json.loads(STATE_PATH.read_text(encoding="utf-8"))


def save_state(state: dict[str, Any]) -> None:
    tmp_path = STATE_PATH.with_suffix(".tmp")
    tmp_path.write_text(json.dumps(state, indent=2, sort_keys=True), encoding="utf-8")
    tmp_path.replace(STATE_PATH)


def process_alive(pid: int | None) -> bool:
    if pid is None:
        return False
    try:
        os.kill(pid, 0)
    except OSError:
        return False
    return True


def cleanup_dead_panes(state: dict[str, Any]) -> None:
    active = state.get("active_pane_id")
    for pane_id, pane in state.get("panes", {}).items():
        pane["alive"] = process_alive(pane.get("pid"))
        if not pane["alive"] and active == int(pane_id):
            active = None
    state["active_pane_id"] = active


def pane_json(pane_id: int, pane: dict[str, Any], active_pane_id: int | None) -> dict[str, Any]:
    return {
        "pane_id": pane_id,
        "tab_id": pane.get("tab_id", pane_id),
        "window_id": pane.get("window_id", 1),
        "domain_name": pane.get("domain_name", "local"),
        "workspace": pane.get("workspace", "default"),
        "title": pane.get("title"),
        "cwd": pane.get("cwd"),
        "is_active": active_pane_id == pane_id,
        "is_zoomed": False,
        "size": {"rows": 24, "cols": 80},
        "cursor_x": 0,
        "cursor_y": 0,
    }


def parse_flag_value(args: list[str], name: str) -> str | None:
    if name in args:
        idx = args.index(name)
        if idx + 1 < len(args):
            return args[idx + 1]
    return None


def split_command(args: list[str]) -> tuple[list[str], list[str]]:
    if "--" not in args:
        return args, []
    idx = args.index("--")
    return args[:idx], args[idx + 1 :]


def main() -> int:
    argv = sys.argv[1:]
    if argv == ["--version"]:
        print("wezterm 2026.04.07 ft-e2e-fake")
        return 0

    if not argv or argv[0] != "cli":
        return 0

    subcommand = argv[1] if len(argv) > 1 else ""
    args = argv[2:]

    with LOCK_PATH.open("a+", encoding="utf-8") as lock_file:
        fcntl.flock(lock_file.fileno(), fcntl.LOCK_EX)
        state = load_state()
        cleanup_dead_panes(state)

        if subcommand == "list":
            panes = []
            active_pane_id = state.get("active_pane_id")
            for pane_id_str in sorted(state.get("panes", {}), key=lambda item: int(item)):
                pane = state["panes"][pane_id_str]
                if pane.get("alive", True):
                    panes.append(pane_json(int(pane_id_str), pane, active_pane_id))
            save_state(state)
            print(json.dumps(panes))
            return 0

        if subcommand == "get-text":
            pane_id = parse_flag_value(args, "--pane-id")
            if pane_id is None:
                print("missing --pane-id", file=sys.stderr)
                return 1
            pane = state.get("panes", {}).get(str(int(pane_id)))
            if not pane:
                print(f"pane {pane_id} not found", file=sys.stderr)
                return 1
            save_state(state)
            log_path = pathlib.Path(pane["log_path"])
            if log_path.exists():
                sys.stdout.write(log_path.read_text(encoding="utf-8", errors="replace"))
            return 0

        if subcommand == "spawn":
            flag_args, command = split_command(args)
            cwd = parse_flag_value(flag_args, "--cwd") or os.getcwd()
            if not command:
                print("spawn requires command after --", file=sys.stderr)
                return 1

            pane_id = int(state.get("next_pane_id", 1))
            state["next_pane_id"] = pane_id + 1

            log_path = (LOG_DIR / f"pane_{pane_id}.log").resolve()
            log_handle = log_path.open("a", encoding="utf-8")
            process = subprocess.Popen(
                command,
                cwd=cwd,
                stdin=subprocess.DEVNULL,
                stdout=log_handle,
                stderr=subprocess.STDOUT,
                text=True,
                preexec_fn=os.setsid,
            )
            log_handle.close()

            script_name = pathlib.Path(command[1]).stem if len(command) > 1 else pathlib.Path(command[0]).stem
            pane_record = {
                "pane_id": pane_id,
                "pid": process.pid,
                "tab_id": pane_id,
                "window_id": 1,
                "domain_name": "local",
                "workspace": "ft-e2e",
                "title": script_name,
                "cwd": cwd,
                "log_path": str(log_path),
                "alive": True,
            }
            state.setdefault("panes", {})[str(pane_id)] = pane_record
            state["active_pane_id"] = pane_id
            save_state(state)
            print(pane_id)
            return 0

        if subcommand == "kill-pane":
            pane_id = parse_flag_value(args, "--pane-id")
            if pane_id is None:
                print("missing --pane-id", file=sys.stderr)
                return 1
            pane = state.get("panes", {}).get(str(int(pane_id)))
            if pane and process_alive(pane.get("pid")):
                with contextlib.suppress(ProcessLookupError):
                    os.killpg(pane["pid"], signal.SIGTERM)
            if pane:
                pane["alive"] = False
                if state.get("active_pane_id") == int(pane_id):
                    state["active_pane_id"] = None
            save_state(state)
            return 0

        if subcommand == "activate-pane":
            pane_id = parse_flag_value(args, "--pane-id")
            if pane_id is None:
                print("missing --pane-id", file=sys.stderr)
                return 1
            if str(int(pane_id)) not in state.get("panes", {}):
                print(f"pane {pane_id} not found", file=sys.stderr)
                return 1
            state["active_pane_id"] = int(pane_id)
            save_state(state)
            return 0

        if subcommand == "send-text":
            pane_id = parse_flag_value(args, "--pane-id")
            if pane_id is None:
                print("missing --pane-id", file=sys.stderr)
                return 1
            pane = state.get("panes", {}).get(str(int(pane_id)))
            if not pane:
                print(f"pane {pane_id} not found", file=sys.stderr)
                return 1
            _, command = split_command(args)
            text = command[0] if command else ""
            suffix = "" if "--no-newline" in args else "\\n"
            log_path = pathlib.Path(pane["log_path"])
            with log_path.open("a", encoding="utf-8") as handle:
                handle.write(f"[FAKE_SEND] {text}{suffix}")
            save_state(state)
            return 0

        save_state(state)
        return 0


if __name__ == "__main__":
    sys.exit(main())
"""


class SkipScenario(RuntimeError):
    pass


@dataclass
class CommandResult:
    argv: list[str]
    returncode: int
    stdout: str
    stderr: str
    duration_ms: int

    def as_dict(self) -> dict[str, Any]:
        return {
            "argv": self.argv,
            "cmd": " ".join(shlex.quote(arg) for arg in self.argv),
            "returncode": self.returncode,
            "stdout": self.stdout,
            "stderr": self.stderr,
            "duration_ms": self.duration_ms,
        }


@dataclass
class BinaryChoice:
    path: str | None
    source: str | None
    candidates: list[dict[str, Any]]


@dataclass
class PaneSpec:
    family: str
    name: str
    delay_secs: float
    repeat_fixture_after_secs: float | None
    fixture_path: Path | None
    lines: list[str]
    expect_workflow: bool


def utc_now() -> str:
    return datetime.now(timezone.utc).replace(microsecond=0).isoformat().replace("+00:00", "Z")


def repo_relative(path: Path) -> str:
    try:
        return str(path.relative_to(REPO_ROOT))
    except ValueError:
        return str(path)


def ensure_output_dir() -> Path:
    OUTPUT_DIR.mkdir(parents=True, exist_ok=True)
    return OUTPUT_DIR


def run(
    argv: list[str],
    *,
    cwd: Path | None = None,
    env: dict[str, str] | None = None,
    timeout: float | None = None,
    check: bool = False,
) -> CommandResult:
    started = time.monotonic()
    completed = subprocess.run(
        argv,
        cwd=str(cwd) if cwd else None,
        env=env,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
        timeout=timeout,
    )
    result = CommandResult(
        argv=argv,
        returncode=completed.returncode,
        stdout=completed.stdout,
        stderr=completed.stderr,
        duration_ms=int((time.monotonic() - started) * 1000),
    )
    if check and result.returncode != 0:
        raise RuntimeError(
            f"command failed ({result.returncode}): {' '.join(shlex.quote(arg) for arg in argv)}\n"
            f"stdout:\n{result.stdout}\n"
            f"stderr:\n{result.stderr}"
        )
    return result


def parse_json_output(result: CommandResult) -> Any:
    payload = result.stdout.strip()
    if not payload:
        raise RuntimeError(f"empty stdout from {' '.join(result.argv)}")
    try:
        return json.loads(payload)
    except json.JSONDecodeError as exc:
        raise RuntimeError(
            f"failed to parse JSON from {' '.join(result.argv)}: {exc}\nstdout:\n{result.stdout}\nstderr:\n{result.stderr}"
        ) from exc


def raw_response(result: CommandResult, limit: int = 500) -> str:
    payload = result.stdout.strip() or result.stderr.strip()
    return payload[:limit]


def resolve_ft_binary() -> BinaryChoice:
    candidates: list[dict[str, Any]] = []

    def record(path: str | None, source: str, usable: bool, note: str | None = None) -> None:
        entry = {"source": source, "path": path, "usable": usable}
        if note:
            entry["note"] = note
        candidates.append(entry)

    env_binary = os.environ.get("FT_BINARY")
    if env_binary:
        env_path = Path(env_binary).expanduser()
        if env_path.is_file() and os.access(env_path, os.X_OK):
            record(str(env_path), "env:FT_BINARY", True)
            return BinaryChoice(str(env_path), "env:FT_BINARY", candidates)
        record(str(env_path), "env:FT_BINARY", False, "not executable")

    cargo_roots: list[tuple[Path, str]] = []
    cargo_target_dir = os.environ.get("CARGO_TARGET_DIR")
    if cargo_target_dir:
        cargo_roots.append((Path(cargo_target_dir).expanduser(), "cargo_target_dir"))
    cargo_roots.append((REPO_ROOT / "target", "repo_target"))

    seen: set[Path] = set()
    for root, source in cargo_roots:
        for profile in ("release", "debug"):
            candidate = (root / profile / "ft").resolve()
            if candidate in seen:
                continue
            seen.add(candidate)
            if candidate.is_file() and os.access(candidate, os.X_OK):
                record(str(candidate), f"{source}:{profile}", True)
                return BinaryChoice(str(candidate), f"{source}:{profile}", candidates)
            record(str(candidate), f"{source}:{profile}", False, "not executable")

    path_binary = shutil.which("ft")
    if path_binary:
        record(path_binary, "path:which_ft", True)
        return BinaryChoice(path_binary, "path:which_ft", candidates)
    record(None, "path:which_ft", False, "not found in PATH")
    return BinaryChoice(None, None, candidates)


def make_workspace() -> tuple[Path, Path]:
    workspace = Path(tempfile.mkdtemp(prefix="ft-e2e-20pane-"))
    data_dir = workspace / ".ft"
    data_dir.mkdir(parents=True, exist_ok=True)
    config_path = workspace / "ft.toml"
    config_path.write_text(CONFIG_TEMPLATE, encoding="utf-8")
    return workspace, config_path


def write_fake_wezterm_cli(workspace: Path) -> tuple[Path, Path]:
    state_dir = workspace / ".fake-wezterm"
    state_dir.mkdir(parents=True, exist_ok=True)
    script_path = workspace / "fake_wezterm.py"
    script_path.write_text(FAKE_WEZTERM_SCRIPT, encoding="utf-8")
    script_path.chmod(0o755)
    return script_path, state_dir


def write_pane_script(
    workspace: Path,
    spec: PaneSpec,
    fixture_text: str | None,
    trigger_path: Path,
) -> Path:
    script_path = workspace / f"{spec.name}.sh"
    body = [
        "#!/bin/bash",
        "set -euo pipefail",
        f"while [ ! -f {shlex.quote(str(trigger_path))} ]; do",
        "  sleep 0.1",
        "done",
        f"sleep {spec.delay_secs:.1f}",
    ]
    for line in spec.lines:
        body.append(f"printf '%s\\n' {shlex.quote(line)}")
    if fixture_text:
        body.append("cat <<'EOF_FIXTURE'")
        body.append(fixture_text.rstrip("\n"))
        body.append("EOF_FIXTURE")
    body.append(f"printf '%s\\n' {shlex.quote(f'[{spec.family}] ready {spec.name}')}")
    if fixture_text and spec.repeat_fixture_after_secs is not None:
        # Re-emit the matching payload after the pane has been live long enough
        # for the watcher to discover it; this avoids one-shot startup races.
        body.append(f"sleep {spec.repeat_fixture_after_secs:.1f}")
        body.append("cat <<'EOF_FIXTURE_REPEAT'")
        body.append(fixture_text.rstrip("\n"))
        body.append("EOF_FIXTURE_REPEAT")
    body.extend(
        [
            "while true; do",
            "  sleep 1",
            "done",
        ]
    )
    script_path.write_text("\n".join(body) + "\n", encoding="utf-8")
    script_path.chmod(0o755)
    return script_path


def build_specs() -> list[PaneSpec]:
    specs: list[PaneSpec] = []
    for index in range(1, 8):
        specs.append(
            PaneSpec(
                family="codex",
                name=f"codex_usage_{index:02d}",
                delay_secs=(index - 1) * 0.4,
                repeat_fixture_after_secs=2.0,
                fixture_path=FIXTURE_DIR / "codex" / "usage_reached.txt",
                lines=[f"[codex] lane {index} boot", f"[codex] tracking swarm pane {index}"],
                expect_workflow=False,
            )
        )
    for index in range(8, 15):
        specs.append(
            PaneSpec(
                family="claude_code",
                name=f"claude_limit_{index:02d}",
                delay_secs=(index - 1) * 0.4,
                repeat_fixture_after_secs=2.0,
                fixture_path=FIXTURE_DIR / "claude_code" / "usage_reached_v2.txt",
                lines=[f"[claude_code] lane {index} boot", f"[claude_code] monitoring rate limit path {index}"],
                expect_workflow=True,
            )
        )
    for index in range(15, 18):
        specs.append(
            PaneSpec(
                family="gemini",
                name=f"gemini_quota_{index:02d}",
                delay_secs=(index - 1) * 0.4,
                repeat_fixture_after_secs=2.0,
                fixture_path=FIXTURE_DIR / "gemini" / "usage_reached.txt",
                lines=[f"[gemini] lane {index} boot", f"[gemini] quota lane {index} warm"],
                expect_workflow=True,
            )
        )
    for index in range(18, 21):
        specs.append(
            PaneSpec(
                family="shell",
                name=f"shell_plain_{index:02d}",
                delay_secs=(index - 1) * 0.4,
                repeat_fixture_after_secs=None,
                fixture_path=None,
                lines=[
                    f"[shell] lane {index} boot",
                    f"[shell] heartbeat stable {index}",
                    f"[shell] no-ai-patterns-here {index}",
                ],
                expect_workflow=False,
            )
        )
    return specs


def ft_env(workspace: Path, fake_wezterm: Path, fake_state_dir: Path) -> dict[str, str]:
    env = os.environ.copy()
    env["FT_WORKSPACE"] = str(workspace)
    env["FT_DATA_DIR"] = str(workspace / ".ft")
    env["FT_OUTPUT_FORMAT"] = "json"
    env["FT_WEZTERM_CLI"] = str(fake_wezterm)
    env["FT_FAKE_WEZTERM_STATE"] = str(fake_state_dir)
    return env


def ft_cmd(ft_binary: str, workspace: Path, config_path: Path, *args: str) -> list[str]:
    return [ft_binary, "--workspace", str(workspace), "--config", str(config_path), *args]


def spawn_pane(fake_wezterm: Path, workspace: Path, script_path: Path, env: dict[str, str]) -> int:
    result = run(
        [str(fake_wezterm), "cli", "spawn", "--cwd", str(workspace), "--", "bash", str(script_path)],
        cwd=REPO_ROOT,
        env=env,
        check=False,
    )
    if result.returncode != 0:
        raise RuntimeError(f"failed to spawn pane for {script_path.name}: {result.stderr or result.stdout}")
    for token in result.stdout.split():
        if token.isdigit():
            return int(token)
    raise RuntimeError(f"fake wezterm spawn did not return a pane id for {script_path.name}: {result.stdout}")


def close_pane(fake_wezterm: Path, pane_id: int, env: dict[str, str]) -> None:
    run([str(fake_wezterm), "cli", "kill-pane", "--pane-id", str(pane_id)], cwd=REPO_ROOT, env=env)


def stop_process(proc: subprocess.Popen[str] | None) -> None:
    if proc is None or proc.poll() is not None:
        return
    proc.send_signal(signal.SIGTERM)
    try:
        proc.wait(timeout=10)
    except subprocess.TimeoutExpired:
        proc.kill()
        proc.wait(timeout=5)


def wait_for(description: str, timeout_secs: float, condition: Any) -> Any:
    deadline = time.monotonic() + timeout_secs
    last_value: Any = None
    last_error: Exception | None = None
    while time.monotonic() < deadline:
        try:
            last_value = condition()
            last_error = None
        except Exception as exc:  # noqa: BLE001
            last_error = exc
            last_value = None
        if last_value:
            return last_value
        time.sleep(0.5)
    if last_error is not None:
        raise RuntimeError(f"timed out waiting for {description}; last error: {last_error}") from last_error
    raise RuntimeError(f"timed out waiting for {description}")


def extract_panes(payload: Any) -> list[dict[str, Any]]:
    data = payload.get("data") if isinstance(payload, dict) else None
    if isinstance(data, list):
        return data
    if isinstance(data, dict) and isinstance(data.get("panes"), list):
        return data["panes"]
    return []


def extract_events(payload: Any) -> list[dict[str, Any]]:
    data = payload.get("data") if isinstance(payload, dict) else None
    if isinstance(data, dict) and isinstance(data.get("events"), list):
        return data["events"]
    if isinstance(payload, list):
        return payload
    return []


def extract_search_results(payload: Any) -> list[dict[str, Any]]:
    data = payload.get("data") if isinstance(payload, dict) else None
    if isinstance(data, dict) and isinstance(data.get("results"), list):
        return data["results"]
    return []


def extract_batch_text_results(payload: Any) -> dict[str, Any]:
    data = payload.get("data") if isinstance(payload, dict) else None
    if isinstance(data, dict) and isinstance(data.get("results"), dict):
        return data["results"]
    return {}


def extract_workflow_executions(payload: Any) -> list[dict[str, Any]]:
    data = payload.get("data") if isinstance(payload, dict) else None
    if isinstance(data, dict) and isinstance(data.get("executions"), list):
        return data["executions"]
    if isinstance(data, dict) and data.get("execution_id"):
        return [data]
    return []


def append_check(
    report: dict[str, Any],
    *,
    name: str,
    expected: str,
    actual: Any,
    passed: bool,
    duration_ms: int,
    raw: str,
) -> None:
    report.setdefault("checks", []).append(
        {
            "check_name": name,
            "expected": expected,
            "actual": actual,
            "passed": passed,
            "duration_ms": duration_ms,
            "raw_response": raw[:500],
        }
    )


def capture_diagnostics(
    report: dict[str, Any],
    *,
    ft_binary: str,
    workspace: Path,
    config_path: Path,
    env: dict[str, str],
    workflow_probe_panes: list[int],
) -> None:
    diagnostics: dict[str, Any] = {}
    commands = {
        "doctor": ft_cmd(ft_binary, workspace, config_path, "doctor", "--json"),
        "robot_state": ft_cmd(ft_binary, workspace, config_path, "robot", "state"),
        "robot_events": ft_cmd(ft_binary, workspace, config_path, "robot", "events", "--limit", "200"),
        "robot_search": ft_cmd(ft_binary, workspace, config_path, "robot", "search", "rate limit"),
        "audit_deny": ft_cmd(
            ft_binary,
            workspace,
            config_path,
            "audit",
            "--decision",
            "deny",
            "--format",
            "json",
            "--limit",
            "200",
        ),
    }
    for key, argv in commands.items():
        diagnostics[key] = run(argv, cwd=REPO_ROOT, env=env, check=False).as_dict()
    workflow_snapshots: dict[str, Any] = {}
    for pane_id in workflow_probe_panes:
        result = run(
            ft_cmd(ft_binary, workspace, config_path, "robot", "workflow", "status", "--pane", str(pane_id)),
            cwd=REPO_ROOT,
            env=env,
            check=False,
        )
        workflow_snapshots[str(pane_id)] = result.as_dict()
    diagnostics["robot_workflow_status_by_pane"] = workflow_snapshots
    report["diagnostics"] = diagnostics


def main() -> int:
    parser = argparse.ArgumentParser(description="20-pane ft integration proof")
    parser.add_argument("--timeout-secs", type=float, default=DEFAULT_TIMEOUT_SECS)
    parser.add_argument("--output", default="", help="Override report path")
    parser.add_argument("--keep-workspace", action="store_true", help="Preserve the temp workspace for debugging")
    args = parser.parse_args()

    timestamp = datetime.now(timezone.utc).strftime("%Y%m%dT%H%M%SZ")
    output_dir = ensure_output_dir()
    report_path = Path(args.output) if args.output else output_dir / f"e2e_20pane_{timestamp}.json"
    watch_log_path = output_dir / f"e2e_20pane_{timestamp}.watch.log"
    scenario_started = time.monotonic()

    report: dict[str, Any] = {
        "schema_version": "ft.e2e_20pane_integration.v1",
        "generated_at_utc": utc_now(),
        "bead_id": "ft-d0ez0.2",
        "status": "failed",
        "skip_exit_code": SKIP_EXIT_CODE,
        "report_path": repo_relative(report_path),
        "watch_log_path": repo_relative(watch_log_path),
        "binary_resolution": {},
        "backend": {},
        "workspace": {},
        "fixtures": {},
        "spawned_panes": [],
        "commands": {},
        "checks": [],
        "notes": [
            "This harness uses FT_WEZTERM_CLI to provide a deterministic fake WezTerm CLI surface.",
            "Pane 15 denial is asserted against the current implementation contract: ok=true with data.injection.status=denied.",
            "Human workflow verification uses `ft workflow status <execution_id>` after discovering a completed execution via robot list mode.",
        ],
    }

    workspace: Path | None = None
    config_path: Path | None = None
    watch_proc: subprocess.Popen[str] | None = None
    fake_wezterm: Path | None = None
    fake_state_dir: Path | None = None
    env: dict[str, str] | None = None
    spawned_panes: list[int] = []
    binary: BinaryChoice | None = None
    pattern_trigger: Path | None = None

    try:
        specs = build_specs()
        report["fixtures"] = {
            "codex_usage_reached": repo_relative(FIXTURE_DIR / "codex" / "usage_reached.txt"),
            "claude_usage_reached_v2": repo_relative(FIXTURE_DIR / "claude_code" / "usage_reached_v2.txt"),
            "gemini_usage_reached": repo_relative(FIXTURE_DIR / "gemini" / "usage_reached.txt"),
        }

        binary = resolve_ft_binary()
        report["binary_resolution"] = {
            "selected_path": binary.path,
            "selected_source": binary.source,
            "candidates": binary.candidates,
        }
        if not binary.path:
            raise SkipScenario("ft binary not found via FT_BINARY, cargo build output, or PATH")

        workspace, config_path = make_workspace()
        fake_wezterm, fake_state_dir = write_fake_wezterm_cli(workspace)
        env = ft_env(workspace, fake_wezterm, fake_state_dir)
        pattern_trigger = workspace / "emit_patterns.trigger"

        backend_probe = run([str(fake_wezterm), "cli", "list"], cwd=REPO_ROOT, env=env, check=True)
        report["backend"] = {
            "type": "fake_wezterm_cli",
            "path": str(fake_wezterm),
            "state_dir": str(fake_state_dir),
            "probe": backend_probe.as_dict(),
        }
        report["workspace"] = {
            "root": str(workspace),
            "data_dir": str(workspace / ".ft"),
            "config_path": str(config_path),
        }

        watch_argv = ft_cmd(binary.path, workspace, config_path, "watch", "--foreground", "--auto-handle")
        with watch_log_path.open("w", encoding="utf-8") as watch_log:
            watch_proc = subprocess.Popen(
                watch_argv,
                cwd=str(REPO_ROOT),
                env=env,
                stdout=watch_log,
                stderr=subprocess.STDOUT,
                text=True,
            )

        def watch_ready() -> bool:
            if watch_proc.poll() is not None:
                raise RuntimeError(f"ft watch exited immediately with code {watch_proc.returncode}")
            return (workspace / ".ft" / "ft.db").exists() or (workspace / ".ft" / "ft.lock").exists()

        wait_for("ft watch to initialize", min(args.timeout_secs, 15.0), watch_ready)

        pane_id_to_spec: dict[int, PaneSpec] = {}
        for spec in specs:
            fixture_text = spec.fixture_path.read_text(encoding="utf-8").strip() if spec.fixture_path else None
            script_path = write_pane_script(workspace, spec, fixture_text, pattern_trigger)
            pane_id = spawn_pane(fake_wezterm, workspace, script_path, env)
            spawned_panes.append(pane_id)
            pane_id_to_spec[pane_id] = spec
            report["spawned_panes"].append(
                {
                    "pane_id": pane_id,
                    "family": spec.family,
                    "title": spec.name,
                    "script": script_path.name,
                    "fixture": repo_relative(spec.fixture_path) if spec.fixture_path else None,
                    "delay_secs": spec.delay_secs,
                }
            )

        agent_panes = {pane_id for pane_id, spec in pane_id_to_spec.items() if spec.family != "shell"}
        shell_panes = {pane_id for pane_id, spec in pane_id_to_spec.items() if spec.family == "shell"}
        workflow_probe_panes = [pane_id for pane_id, spec in pane_id_to_spec.items() if spec.expect_workflow]

        def state_ready() -> bool:
            result = run(ft_cmd(binary.path, workspace, config_path, "robot", "state"), cwd=REPO_ROOT, env=env)
            payload = parse_json_output(result)
            report["commands"]["robot_state_wait"] = result.as_dict()
            observed_ids = {int(pane["pane_id"]) for pane in extract_panes(payload) if "pane_id" in pane}
            return all(pane_id in observed_ids for pane_id in spawned_panes)

        wait_for("robot state to observe all 20 panes", args.timeout_secs, state_ready)
        pattern_trigger.touch()

        def events_ready() -> bool:
            result = run(
                ft_cmd(binary.path, workspace, config_path, "robot", "events", "--limit", "50"),
                cwd=REPO_ROOT,
                env=env,
            )
            payload = parse_json_output(result)
            report["commands"]["robot_events_wait"] = result.as_dict()
            event_panes = {int(event.get("pane_id", -1)) for event in extract_events(payload)}
            return agent_panes.issubset(event_panes)

        wait_for("pattern events for all 17 agent panes", args.timeout_secs, events_ready)

        completed_execution_id: str | None = None

        def workflow_ready() -> str | None:
            nonlocal completed_execution_id
            for pane_id in workflow_probe_panes:
                result = run(
                    ft_cmd(binary.path, workspace, config_path, "robot", "workflow", "status", "--pane", str(pane_id)),
                    cwd=REPO_ROOT,
                    env=env,
                )
                report["commands"][f"robot_workflow_status_wait_{pane_id}"] = result.as_dict()
                payload = parse_json_output(result)
                for execution in extract_workflow_executions(payload):
                    if execution.get("status") == "completed":
                        completed_execution_id = str(execution["execution_id"])
                        return completed_execution_id
            return None

        wait_for("completed workflow execution on a safe workflow pane", args.timeout_secs, workflow_ready)

        def search_ready() -> bool:
            result = run(
                ft_cmd(binary.path, workspace, config_path, "robot", "search", "rate limit"),
                cwd=REPO_ROOT,
                env=env,
            )
            payload = parse_json_output(result)
            report["commands"]["robot_search_wait"] = result.as_dict()
            pane_ids = {int(hit.get("pane_id", -1)) for hit in extract_search_results(payload)}
            return len(pane_ids) >= 2

        wait_for("search hits spanning multiple panes", args.timeout_secs, search_ready)

        final_get_text_all = run(
            ft_cmd(binary.path, workspace, config_path, "robot", "get-text", "--all", "--tail", "1"),
            cwd=REPO_ROOT,
            env=env,
            check=True,
        )
        final_events = run(
            ft_cmd(binary.path, workspace, config_path, "robot", "events", "--limit", "50"),
            cwd=REPO_ROOT,
            env=env,
            check=True,
        )
        final_search = run(
            ft_cmd(binary.path, workspace, config_path, "robot", "search", "rate limit"),
            cwd=REPO_ROOT,
            env=env,
            check=True,
        )
        final_state = run(
            ft_cmd(binary.path, workspace, config_path, "robot", "state"),
            cwd=REPO_ROOT,
            env=env,
            check=True,
        )
        final_send_deny = run(
            ft_cmd(binary.path, workspace, config_path, "robot", "send", str(PANE_DENY_ID), "test"),
            cwd=REPO_ROOT,
            env=env,
            check=True,
        )

        report["commands"]["robot_get_text_all"] = final_get_text_all.as_dict()
        report["commands"]["robot_events"] = final_events.as_dict()
        report["commands"]["robot_search"] = final_search.as_dict()
        report["commands"]["robot_state"] = final_state.as_dict()
        report["commands"]["robot_send_deny"] = final_send_deny.as_dict()

        get_text_payload = parse_json_output(final_get_text_all)
        events_payload = parse_json_output(final_events)
        search_payload = parse_json_output(final_search)
        state_payload = parse_json_output(final_state)
        send_payload = parse_json_output(final_send_deny)

        batch_text_results = extract_batch_text_results(get_text_payload)
        event_items = extract_events(events_payload)
        search_hits = extract_search_results(search_payload)
        state_items = extract_panes(state_payload)

        check_start = time.monotonic()
        get_text_actual = {
            "pane_count": len(batch_text_results),
            "statuses": {
                key: value.get("status")
                for key, value in batch_text_results.items()
            },
        }
        get_text_passed = len(batch_text_results) == 20 and all(
            value.get("status") == "ok" for value in batch_text_results.values()
        )
        append_check(
            report,
            name="delta_extraction_get_text_all_tail_1",
            expected="20 pane text results with status=ok",
            actual=get_text_actual,
            passed=get_text_passed,
            duration_ms=int((time.monotonic() - check_start) * 1000),
            raw=raw_response(final_get_text_all),
        )

        check_start = time.monotonic()
        event_pane_ids = {int(event.get("pane_id", -1)) for event in event_items}
        shell_event_ids = sorted(pane_id for pane_id in shell_panes if pane_id in event_pane_ids)
        agent_event_ids = sorted(pane_id for pane_id in agent_panes if pane_id in event_pane_ids)
        events_passed = len(agent_event_ids) == 17
        append_check(
            report,
            name="pattern_detection_events_cover_17_agent_panes",
            expected="robot events include detections for all 17 non-shell panes",
            actual={"agent_event_pane_ids": agent_event_ids, "event_count": len(event_items)},
            passed=events_passed,
            duration_ms=int((time.monotonic() - check_start) * 1000),
            raw=raw_response(final_events),
        )

        check_start = time.monotonic()
        false_positive_passed = not shell_event_ids
        append_check(
            report,
            name="false_positive_check_shell_panes_zero_events",
            expected="shell panes 18, 19, 20 produce zero events",
            actual={"shell_event_pane_ids": shell_event_ids},
            passed=false_positive_passed,
            duration_ms=int((time.monotonic() - check_start) * 1000),
            raw=raw_response(final_events),
        )

        if not completed_execution_id:
            raise RuntimeError("workflow wait finished without recording a completed execution id")

        final_robot_workflow = run(
            ft_cmd(
                binary.path,
                workspace,
                config_path,
                "robot",
                "workflow",
                "status",
                completed_execution_id,
            ),
            cwd=REPO_ROOT,
            env=env,
            check=True,
        )
        final_human_workflow = run(
            ft_cmd(binary.path, workspace, config_path, "workflow", "status", completed_execution_id),
            cwd=REPO_ROOT,
            env=env,
            check=True,
        )
        report["commands"]["robot_workflow_status"] = final_robot_workflow.as_dict()
        report["commands"]["human_workflow_status"] = final_human_workflow.as_dict()

        robot_workflow_payload = parse_json_output(final_robot_workflow)
        robot_workflow_items = extract_workflow_executions(robot_workflow_payload)
        robot_workflow_item = robot_workflow_items[0] if robot_workflow_items else {}
        check_start = time.monotonic()
        workflow_passed = (
            robot_workflow_item.get("status") == "completed"
            and "Status: completed" in final_human_workflow.stdout
        )
        append_check(
            report,
            name="workflow_status_shows_completed_execution",
            expected="completed workflow is visible via robot and human workflow status",
            actual={
                "execution_id": completed_execution_id,
                "robot_status": robot_workflow_item.get("status"),
                "human_status_line_present": "Status: completed" in final_human_workflow.stdout,
            },
            passed=workflow_passed,
            duration_ms=int((time.monotonic() - check_start) * 1000),
            raw=raw_response(final_human_workflow),
        )

        check_start = time.monotonic()
        send_data = send_payload.get("data", {}) if isinstance(send_payload, dict) else {}
        injection = send_data.get("injection", {}) if isinstance(send_data, dict) else {}
        send_passed = (
            bool(send_payload.get("ok"))
            and injection.get("status") == "denied"
            and int(send_data.get("pane_id", -1)) == PANE_DENY_ID
            and injection.get("action") == "send_text"
            and isinstance(injection.get("decision"), dict)
            and injection["decision"].get("decision") == "deny"
        )
        append_check(
            report,
            name="policy_gate_robot_send_denied_for_pane_15",
            expected="robot send to pane 15 is denied with injection.status=denied",
            actual={
                "ok": send_payload.get("ok"),
                "pane_id": send_data.get("pane_id"),
                "injection_status": injection.get("status"),
                "decision": injection.get("decision"),
            },
            passed=send_passed,
            duration_ms=int((time.monotonic() - check_start) * 1000),
            raw=raw_response(final_send_deny),
        )

        def audit_ready() -> bool:
            result = run(
                ft_cmd(
                    binary.path,
                    workspace,
                    config_path,
                    "audit",
                    "--decision",
                    "deny",
                    "--format",
                    "json",
                    "--limit",
                    "50",
                ),
                cwd=REPO_ROOT,
                env=env,
            )
            report["commands"]["audit_deny_wait"] = result.as_dict()
            payload = parse_json_output(result)
            if not isinstance(payload, list):
                return False
            return any(
                int(item.get("pane_id", -1)) == PANE_DENY_ID
                and item.get("policy_decision") == "deny"
                and item.get("action_kind") == "send_text"
                for item in payload
            )

        wait_for("audit record for pane 15 denial", min(args.timeout_secs, 15.0), audit_ready)

        audit_result = run(
            ft_cmd(
                binary.path,
                workspace,
                config_path,
                "audit",
                "--decision",
                "deny",
                "--format",
                "json",
                "--limit",
                "50",
            ),
            cwd=REPO_ROOT,
            env=env,
            check=True,
        )
        report["commands"]["audit_deny"] = audit_result.as_dict()
        audit_payload = parse_json_output(audit_result)
        if not isinstance(audit_payload, list):
            raise RuntimeError(f"expected audit JSON array, got: {type(audit_payload)!r}")

        check_start = time.monotonic()
        audit_matches = [
            item
            for item in audit_payload
            if int(item.get("pane_id", -1)) == PANE_DENY_ID
            and item.get("policy_decision") == "deny"
            and item.get("action_kind") == "send_text"
        ]
        audit_passed = bool(audit_matches)
        append_check(
            report,
            name="audit_trail_includes_pane_15_denial",
            expected="audit deny export contains pane 15 send_text denial",
            actual={
                "matching_record_ids": [item.get("id") for item in audit_matches],
                "matching_count": len(audit_matches),
            },
            passed=audit_passed,
            duration_ms=int((time.monotonic() - check_start) * 1000),
            raw=raw_response(audit_result),
        )

        check_start = time.monotonic()
        unique_search_panes = sorted({int(hit.get("pane_id", -1)) for hit in search_hits})
        search_passed = len(unique_search_panes) >= 2
        append_check(
            report,
            name="fts_search_spans_multiple_panes",
            expected="robot search 'rate limit' returns hits across at least two panes",
            actual={"pane_ids": unique_search_panes, "hit_count": len(search_hits)},
            passed=search_passed,
            duration_ms=int((time.monotonic() - check_start) * 1000),
            raw=raw_response(final_search),
        )

        check_start = time.monotonic()
        state_count = len(state_items)
        domain_mismatches = sorted(
            int(item["pane_id"])
            for item in state_items
            if item.get("domain") != "local"
        )
        title_mismatches = sorted(
            int(item["pane_id"])
            for item in state_items
            if pane_id_to_spec[int(item["pane_id"])].name != item.get("title")
        )
        state_passed = state_count == 20 and not domain_mismatches and not title_mismatches
        append_check(
            report,
            name="robot_state_reports_all_20_panes_with_expected_metadata",
            expected="20 panes, domain=local, and title matches generated pane name",
            actual={
                "pane_count": state_count,
                "domain_mismatches": domain_mismatches,
                "title_mismatches": title_mismatches,
            },
            passed=state_passed,
            duration_ms=int((time.monotonic() - check_start) * 1000),
            raw=raw_response(final_state),
        )

        all_passed = all(check["passed"] for check in report["checks"])
        report["summary"] = {
            "total_checks": len(report["checks"]),
            "passed": sum(1 for check in report["checks"] if check["passed"]),
            "failed": sum(1 for check in report["checks"] if not check["passed"]),
            "elapsed_total_ms": int((time.monotonic() - scenario_started) * 1000),
        }
        if not all_passed:
            raise RuntimeError(f"one or more checks failed: {report['summary']}")

        report["status"] = "passed"
        return 0
    except SkipScenario as exc:
        report["status"] = "skipped"
        report["skip_reason"] = str(exc)
        return SKIP_EXIT_CODE
    except Exception as exc:  # noqa: BLE001
        report["status"] = "failed"
        report["error"] = str(exc)
        if binary and binary.path and workspace and config_path and env:
            workflow_probe_panes = [
                entry["pane_id"]
                for entry in report.get("spawned_panes", [])
                if entry.get("family") in {"claude_code", "gemini"}
            ]
            capture_diagnostics(
                report,
                ft_binary=binary.path,
                workspace=workspace,
                config_path=config_path,
                env=env,
                workflow_probe_panes=workflow_probe_panes,
            )
        return 1
    finally:
        if fake_wezterm and env:
            for pane_id in reversed(spawned_panes):
                close_pane(fake_wezterm, pane_id, env)
        stop_process(watch_proc)
        if workspace is not None:
            report.setdefault("workspace", {})
            report["workspace"]["kept"] = bool(args.keep_workspace)
            if not args.keep_workspace:
                shutil.rmtree(workspace, ignore_errors=True)
        report["generated_at_utc"] = utc_now()
        report_path.parent.mkdir(parents=True, exist_ok=True)
        report_path.write_text(json.dumps(report, indent=2, sort_keys=True) + "\n", encoding="utf-8")
        print(repo_relative(report_path))


if __name__ == "__main__":
    sys.exit(main())
