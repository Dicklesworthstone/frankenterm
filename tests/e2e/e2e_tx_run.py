#!/usr/bin/env python3
"""E2E proof for ft tx run: transactional multi-pane operations with audit trail.

Bead: ft-d0ez0.4

Three scenarios:
  1. Happy path — 2-step transaction sends markers to 2 panes, verifies committed state.
  2. Partial failure — step 2 targets nonexistent pane, compensation fires on pane 1.
  3. Policy denial — safety rule blocks send_text on target pane, prepare phase denies.

Uses a fake wezterm backend (embedded Python script) so no live terminal is needed.
"""
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
from dataclasses import dataclass, field
from datetime import datetime, timezone
from pathlib import Path
from typing import Any


REPO_ROOT = Path(__file__).resolve().parents[2]
OUTPUT_DIR = REPO_ROOT / "tests" / "e2e" / "output"
SKIP_EXIT_CODE = 77
DEFAULT_TIMEOUT_SECS = 60.0


# ---------------------------------------------------------------------------
# Minimal ft.toml (scenarios 1 & 2 — no restrictive safety rules)
# ---------------------------------------------------------------------------
BASE_CONFIG = """\
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
"""

# ---------------------------------------------------------------------------
# ft.toml for scenario 3 — deny send_text on pane 1 for robot actor
# ---------------------------------------------------------------------------
DENY_CONFIG = """\
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
id = "e2e.deny_pane_1"
description = "Block all robot send_text on pane 1 for policy-denial coverage"
priority = 1
decision = "deny"
message = "Policy denial E2E coverage"

[safety.rules.rules.match_on]
actions = ["send_text"]
actors = ["robot"]
surfaces = ["mux"]
pane_ids = [1]
"""

# ---------------------------------------------------------------------------
# Fake wezterm CLI — embedded Python script
# Identical to the one used by e2e_20pane_integration.py.
# Implements: list, spawn, kill-pane, get-text, send-text, activate-pane.
# ---------------------------------------------------------------------------
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
    return args[:idx], args[idx + 1:]


def main() -> int:
    argv = sys.argv[1:]
    if argv == ["--version"]:
        print("wezterm 2026.04.10 ft-e2e-tx-run-fake")
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
                "workspace": "ft-e2e-tx",
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


# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------

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
class ScenarioResult:
    name: str
    passed: bool
    assertions: dict[str, bool]
    commands: dict[str, Any] = field(default_factory=dict)
    error: str | None = None

    def as_dict(self) -> dict[str, Any]:
        result: dict[str, Any] = {
            "scenario_name": self.name,
            "passed": self.passed,
            "assertions": self.assertions,
            "commands": self.commands,
        }
        if self.error:
            result["error"] = self.error
        return result


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
            f"failed to parse JSON from {' '.join(result.argv)}: {exc}\n"
            f"stdout:\n{result.stdout}\nstderr:\n{result.stderr}"
        ) from exc


def resolve_ft_binary() -> BinaryChoice:
    candidates: list[dict[str, Any]] = []

    def record(path: str | None, source: str, usable: bool, note: str | None = None) -> None:
        entry: dict[str, Any] = {"source": source, "path": path, "usable": usable}
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


# ---------------------------------------------------------------------------
# Workspace setup
# ---------------------------------------------------------------------------

def make_workspace(config_text: str, prefix: str = "ft-e2e-tx-") -> tuple[Path, Path]:
    workspace = Path(tempfile.mkdtemp(prefix=prefix))
    data_dir = workspace / ".ft"
    data_dir.mkdir(parents=True, exist_ok=True)
    config_path = workspace / "ft.toml"
    config_path.write_text(config_text, encoding="utf-8")
    return workspace, config_path


def write_fake_wezterm_cli(workspace: Path) -> tuple[Path, Path]:
    state_dir = workspace / ".fake-wezterm"
    state_dir.mkdir(parents=True, exist_ok=True)
    script_path = workspace / "fake_wezterm.py"
    script_path.write_text(FAKE_WEZTERM_SCRIPT, encoding="utf-8")
    script_path.chmod(0o755)
    return script_path, state_dir


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


def write_pane_script(workspace: Path, name: str) -> Path:
    """Write a minimal shell script that sleeps forever (pane process)."""
    script_path = workspace / f"{name}.sh"
    script_path.write_text(
        "#!/bin/bash\nset -euo pipefail\n"
        f"printf '%s\\n' '[e2e-tx] {name} ready'\n"
        "while true; do sleep 1; done\n",
        encoding="utf-8",
    )
    script_path.chmod(0o755)
    return script_path


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


def read_pane_log(fake_state_dir: Path, pane_id: int) -> str:
    log_path = fake_state_dir / "logs" / f"pane_{pane_id}.log"
    if log_path.exists():
        return log_path.read_text(encoding="utf-8", errors="replace")
    return ""


# ---------------------------------------------------------------------------
# Contract builders
# ---------------------------------------------------------------------------

def now_ms() -> int:
    return int(time.time() * 1000)


def build_happy_contract(pane_a: int, pane_b: int) -> dict[str, Any]:
    """2-step happy-path contract: send markers to two panes."""
    return {
        "tx_version": 1,
        "intent": {
            "tx_id": "e2e-tx-happy-1",
            "requested_by": "operator",
            "summary": "E2E happy-path: send markers to 2 panes",
            "correlation_id": "e2e-happy-001",
            "created_at_ms": now_ms(),
        },
        "plan": {
            "plan_id": "plan-happy-1",
            "tx_id": "e2e-tx-happy-1",
            "steps": [
                {
                    "step_id": "step-1",
                    "ordinal": 0,
                    "action": {
                        "type": "send_text",
                        "pane_id": pane_a,
                        "text": "echo step1-marker",
                    },
                    "description": "Send step1-marker to pane A",
                },
                {
                    "step_id": "step-2",
                    "ordinal": 1,
                    "action": {
                        "type": "send_text",
                        "pane_id": pane_b,
                        "text": "echo step2-marker",
                    },
                    "description": "Send step2-marker to pane B",
                },
            ],
            "preconditions": [],
            "compensations": [],
        },
        "lifecycle_state": "draft",
        "outcome": "pending",
        "receipts": [],
    }


def build_fail_contract(pane_real: int, pane_missing: int) -> dict[str, Any]:
    """Step 1 succeeds, step 2 targets a nonexistent pane. Compensation for step 1."""
    return {
        "tx_version": 1,
        "intent": {
            "tx_id": "e2e-tx-fail-1",
            "requested_by": "operator",
            "summary": "E2E partial-failure: step 2 targets missing pane",
            "correlation_id": "e2e-fail-001",
            "created_at_ms": now_ms(),
        },
        "plan": {
            "plan_id": "plan-fail-1",
            "tx_id": "e2e-tx-fail-1",
            "steps": [
                {
                    "step_id": "step-1",
                    "ordinal": 0,
                    "action": {
                        "type": "send_text",
                        "pane_id": pane_real,
                        "text": "echo step1-will-be-compensated",
                    },
                    "description": "Send text to existing pane (will be compensated)",
                },
                {
                    "step_id": "step-2",
                    "ordinal": 1,
                    "action": {
                        "type": "send_text",
                        "pane_id": pane_missing,
                        "text": "echo this-should-fail",
                    },
                    "description": "Send text to nonexistent pane (triggers failure)",
                },
            ],
            "preconditions": [],
            "compensations": [
                {
                    "for_step_id": "step-1",
                    "action": {
                        "type": "send_text",
                        "pane_id": pane_real,
                        "text": "echo COMPENSATED",
                    },
                },
            ],
        },
        "lifecycle_state": "draft",
        "outcome": "pending",
        "receipts": [],
    }


def build_deny_contract(pane_denied: int) -> dict[str, Any]:
    """Single-step contract targeting a pane that is policy-denied."""
    return {
        "tx_version": 1,
        "intent": {
            "tx_id": "e2e-tx-deny-1",
            "requested_by": "operator",
            "summary": "E2E policy-denial: send_text blocked by safety rule",
            "correlation_id": "e2e-deny-001",
            "created_at_ms": now_ms(),
        },
        "plan": {
            "plan_id": "plan-deny-1",
            "tx_id": "e2e-tx-deny-1",
            "steps": [
                {
                    "step_id": "step-1",
                    "ordinal": 0,
                    "action": {
                        "type": "send_text",
                        "pane_id": pane_denied,
                        "text": "echo SHOULD-NOT-APPEAR",
                    },
                    "description": "This step should be blocked by policy",
                },
            ],
            "preconditions": [],
            "compensations": [],
        },
        "lifecycle_state": "draft",
        "outcome": "pending",
        "receipts": [],
    }


def write_contract(workspace: Path, name: str, contract: dict[str, Any]) -> Path:
    contract_path = workspace / f"{name}.json"
    contract_path.write_text(json.dumps(contract, indent=2) + "\n", encoding="utf-8")
    return contract_path


# ---------------------------------------------------------------------------
# Scenario runners
# ---------------------------------------------------------------------------

def run_scenario_happy(
    ft_binary: str,
    workspace: Path,
    config_path: Path,
    fake_wezterm: Path,
    fake_state_dir: Path,
    env: dict[str, str],
) -> ScenarioResult:
    """Scenario 1: Happy path — 2-step transaction commits successfully."""
    commands: dict[str, Any] = {}
    assertions: dict[str, bool] = {}

    # Spawn 2 panes
    script_a = write_pane_script(workspace, "pane_a")
    script_b = write_pane_script(workspace, "pane_b")
    pane_a = spawn_pane(fake_wezterm, workspace, script_a, env)
    pane_b = spawn_pane(fake_wezterm, workspace, script_b, env)

    # Write contract
    contract = build_happy_contract(pane_a, pane_b)
    contract_path = write_contract(workspace, "happy_contract", contract)

    # Run ft tx run
    tx_run_result = run(
        ft_cmd(ft_binary, workspace, config_path, "tx", "run", "--contract-file", str(contract_path), "--format", "json"),
        cwd=REPO_ROOT,
        env=env,
        timeout=30.0,
    )
    commands["ft_tx_run"] = tx_run_result.as_dict()

    # Parse output — handle both success JSON and error exit codes
    try:
        tx_output = parse_json_output(tx_run_result)
    except RuntimeError:
        return ScenarioResult(
            name="happy_path",
            passed=False,
            assertions={},
            commands=commands,
            error=f"ft tx run failed (exit {tx_run_result.returncode}): {tx_run_result.stderr[:500]}",
        )

    # Extract result data (handle both envelope and flat formats)
    data = tx_output.get("data", tx_output)

    # Assertions
    final_state = data.get("final_state", "")
    prepare_outcome = ""
    prepare_report = data.get("prepare_report", {})
    if isinstance(prepare_report, dict):
        prepare_outcome = prepare_report.get("outcome", "")

    commit_report = data.get("commit_report")
    commit_outcome = ""
    committed_count = 0
    failed_count = 0
    if isinstance(commit_report, dict):
        commit_outcome = commit_report.get("outcome", "")
        committed_count = commit_report.get("committed_count", 0)
        failed_count = commit_report.get("failed_count", 0)

    assertions["ft_tx_run_exit_zero"] = tx_run_result.returncode == 0
    assertions["prepare_outcome_is_all_ready"] = prepare_outcome == "all_ready"
    assertions["final_state_is_committed"] = final_state == "committed"
    assertions["commit_outcome_is_fully_committed"] = commit_outcome == "fully_committed"
    assertions["committed_count_is_2"] = committed_count == 2
    assertions["failed_count_is_0"] = failed_count == 0

    # Verify pane logs contain sent text
    pane_a_log = read_pane_log(fake_state_dir, pane_a)
    pane_b_log = read_pane_log(fake_state_dir, pane_b)
    assertions["pane_a_contains_step1_marker"] = "step1-marker" in pane_a_log
    assertions["pane_b_contains_step2_marker"] = "step2-marker" in pane_b_log

    # Run ft tx show to verify contract state inspection
    tx_show_result = run(
        ft_cmd(
            ft_binary, workspace, config_path,
            "tx", "show", "--contract-file", str(contract_path), "--include-contract", "--format", "json",
        ),
        cwd=REPO_ROOT,
        env=env,
        timeout=15.0,
    )
    commands["ft_tx_show"] = tx_show_result.as_dict()

    passed = all(assertions.values())
    return ScenarioResult(name="happy_path", passed=passed, assertions=assertions, commands=commands)


def run_scenario_fail_compensate(
    ft_binary: str,
    workspace: Path,
    config_path: Path,
    fake_wezterm: Path,
    fake_state_dir: Path,
    env: dict[str, str],
) -> ScenarioResult:
    """Scenario 2: Partial failure — step 2 fails, compensation fires for step 1."""
    commands: dict[str, Any] = {}
    assertions: dict[str, bool] = {}

    # Spawn 1 real pane
    script = write_pane_script(workspace, "pane_comp")
    pane_real = spawn_pane(fake_wezterm, workspace, script, env)
    pane_missing = 99  # Does not exist

    # Write contract
    contract = build_fail_contract(pane_real, pane_missing)
    contract_path = write_contract(workspace, "fail_contract", contract)

    # Run ft tx run
    tx_run_result = run(
        ft_cmd(ft_binary, workspace, config_path, "tx", "run", "--contract-file", str(contract_path), "--format", "json"),
        cwd=REPO_ROOT,
        env=env,
        timeout=30.0,
    )
    commands["ft_tx_run"] = tx_run_result.as_dict()

    try:
        tx_output = parse_json_output(tx_run_result)
    except RuntimeError:
        return ScenarioResult(
            name="fail_compensate",
            passed=False,
            assertions={},
            commands=commands,
            error=f"ft tx run failed to produce JSON (exit {tx_run_result.returncode}): {tx_run_result.stderr[:500]}",
        )

    data = tx_output.get("data", tx_output)

    # The prepare phase should pass (gates check policy, not pane existence)
    prepare_report = data.get("prepare_report", {})
    prepare_outcome = prepare_report.get("outcome", "") if isinstance(prepare_report, dict) else ""

    # The commit phase should fail because step 2 targets a nonexistent pane
    commit_report = data.get("commit_report")
    commit_outcome = ""
    commit_failed_count = 0
    if isinstance(commit_report, dict):
        commit_outcome = commit_report.get("outcome", "")
        commit_failed_count = commit_report.get("failed_count", 0)

    # Compensation should run
    compensation_report = data.get("compensation_report")
    compensation_outcome = ""
    compensated_count = 0
    if isinstance(compensation_report, dict):
        compensation_outcome = compensation_report.get("outcome", "")
        compensated_count = compensation_report.get("compensated_count", 0)

    final_state = data.get("final_state", "")

    assertions["prepare_passed"] = prepare_outcome == "all_ready"
    assertions["commit_has_failure"] = commit_failed_count > 0
    assertions["commit_outcome_is_partial_failure"] = commit_outcome == "partial_failure"
    assertions["compensation_ran"] = compensation_report is not None
    assertions["compensation_outcome_is_fully_rolled_back"] = compensation_outcome == "fully_rolled_back"
    assertions["compensated_count_gte_1"] = compensated_count >= 1
    assertions["final_state_is_compensated"] = final_state in ("compensated", "rolled_back")

    # Verify pane log shows compensation text
    pane_log = read_pane_log(fake_state_dir, pane_real)
    assertions["pane_log_contains_compensated_marker"] = "COMPENSATED" in pane_log

    # Run ft tx show
    tx_show_result = run(
        ft_cmd(
            ft_binary, workspace, config_path,
            "tx", "show", "--contract-file", str(contract_path), "--format", "json",
        ),
        cwd=REPO_ROOT,
        env=env,
        timeout=15.0,
    )
    commands["ft_tx_show"] = tx_show_result.as_dict()

    passed = all(assertions.values())
    return ScenarioResult(name="fail_compensate", passed=passed, assertions=assertions, commands=commands)


def run_scenario_policy_denial(
    ft_binary: str,
    workspace: Path,
    config_path: Path,
    fake_wezterm: Path,
    fake_state_dir: Path,
    env: dict[str, str],
) -> ScenarioResult:
    """Scenario 3: Policy denial — safety rule blocks send_text."""
    commands: dict[str, Any] = {}
    assertions: dict[str, bool] = {}

    # Spawn pane (ID will be 1 in a fresh workspace)
    script = write_pane_script(workspace, "pane_denied")
    pane_denied = spawn_pane(fake_wezterm, workspace, script, env)

    # Write contract targeting the denied pane
    contract = build_deny_contract(pane_denied)
    contract_path = write_contract(workspace, "deny_contract", contract)

    # Run ft tx run
    tx_run_result = run(
        ft_cmd(ft_binary, workspace, config_path, "tx", "run", "--contract-file", str(contract_path), "--format", "json"),
        cwd=REPO_ROOT,
        env=env,
        timeout=30.0,
    )
    commands["ft_tx_run"] = tx_run_result.as_dict()

    try:
        tx_output = parse_json_output(tx_run_result)
    except RuntimeError:
        return ScenarioResult(
            name="policy_denial",
            passed=False,
            assertions={},
            commands=commands,
            error=f"ft tx run failed to produce JSON (exit {tx_run_result.returncode}): {tx_run_result.stderr[:500]}",
        )

    data = tx_output.get("data", tx_output)

    # The prepare phase should deny
    prepare_report = data.get("prepare_report", {})
    prepare_outcome = prepare_report.get("outcome", "") if isinstance(prepare_report, dict) else ""

    # No commit should happen
    commit_report = data.get("commit_report")
    final_state = data.get("final_state", "")

    assertions["prepare_outcome_is_denied"] = prepare_outcome == "denied"
    assertions["commit_report_is_none"] = commit_report is None
    assertions["final_state_is_not_committed"] = final_state != "committed"

    # Verify pane log does NOT contain the denied text
    pane_log = read_pane_log(fake_state_dir, pane_denied)
    assertions["pane_log_does_not_contain_denied_text"] = "SHOULD-NOT-APPEAR" not in pane_log

    # Query audit records for denial evidence
    audit_result = run(
        ft_cmd(ft_binary, workspace, config_path, "audit", "-f", "json", "-l", "50"),
        cwd=REPO_ROOT,
        env=env,
        timeout=15.0,
    )
    commands["ft_audit"] = audit_result.as_dict()

    passed = all(assertions.values())
    return ScenarioResult(name="policy_denial", passed=passed, assertions=assertions, commands=commands)


# ---------------------------------------------------------------------------
# Main
# ---------------------------------------------------------------------------

def main() -> int:
    parser = argparse.ArgumentParser(description="E2E proof for ft tx run")
    parser.add_argument("--timeout-secs", type=float, default=DEFAULT_TIMEOUT_SECS)
    parser.add_argument("--output", default="", help="Override report path")
    parser.add_argument("--keep-workspace", action="store_true", help="Preserve temp workspaces")
    args = parser.parse_args()

    timestamp = datetime.now(timezone.utc).strftime("%Y%m%dT%H%M%SZ")
    output_dir = ensure_output_dir()
    report_path = Path(args.output) if args.output else output_dir / f"e2e_tx_run_{timestamp}.json"

    report: dict[str, Any] = {
        "schema_version": "ft.e2e_tx_run.v1",
        "generated_at_utc": utc_now(),
        "bead_id": "ft-d0ez0.4",
        "status": "failed",
        "skip_exit_code": SKIP_EXIT_CODE,
        "report_path": repo_relative(report_path),
        "binary_resolution": {},
        "scenarios": [],
        "summary": {},
        "notes": [
            "Uses fake wezterm backend — no live terminal required.",
            "Each scenario gets its own isolated workspace.",
            "Binary discovery order: FT_BINARY, cargo build output, then PATH which ft.",
        ],
    }

    workspaces: list[Path] = []
    spawned_processes: list[tuple[Path, int, dict[str, str]]] = []  # (fake_wezterm, pane_id, env)

    try:
        # Resolve ft binary
        binary = resolve_ft_binary()
        report["binary_resolution"] = {
            "selected_path": binary.path,
            "selected_source": binary.source,
            "candidates": binary.candidates,
        }
        if not binary.path:
            raise SkipScenario("ft binary not found via FT_BINARY, cargo build output, or PATH")

        ft_binary = binary.path
        scenario_results: list[ScenarioResult] = []

        # -----------------------------------------------------------------
        # Scenario 1: Happy path
        # -----------------------------------------------------------------
        workspace_1, config_1 = make_workspace(BASE_CONFIG, prefix="ft-e2e-tx-happy-")
        workspaces.append(workspace_1)
        fake_wezterm_1, fake_state_1 = write_fake_wezterm_cli(workspace_1)
        env_1 = ft_env(workspace_1, fake_wezterm_1, fake_state_1)

        result_1 = run_scenario_happy(ft_binary, workspace_1, config_1, fake_wezterm_1, fake_state_1, env_1)
        scenario_results.append(result_1)

        # -----------------------------------------------------------------
        # Scenario 2: Partial failure with compensation
        # -----------------------------------------------------------------
        workspace_2, config_2 = make_workspace(BASE_CONFIG, prefix="ft-e2e-tx-fail-")
        workspaces.append(workspace_2)
        fake_wezterm_2, fake_state_2 = write_fake_wezterm_cli(workspace_2)
        env_2 = ft_env(workspace_2, fake_wezterm_2, fake_state_2)

        result_2 = run_scenario_fail_compensate(ft_binary, workspace_2, config_2, fake_wezterm_2, fake_state_2, env_2)
        scenario_results.append(result_2)

        # -----------------------------------------------------------------
        # Scenario 3: Policy denial
        # -----------------------------------------------------------------
        workspace_3, config_3 = make_workspace(DENY_CONFIG, prefix="ft-e2e-tx-deny-")
        workspaces.append(workspace_3)
        fake_wezterm_3, fake_state_3 = write_fake_wezterm_cli(workspace_3)
        env_3 = ft_env(workspace_3, fake_wezterm_3, fake_state_3)

        result_3 = run_scenario_policy_denial(ft_binary, workspace_3, config_3, fake_wezterm_3, fake_state_3, env_3)
        scenario_results.append(result_3)

        # -----------------------------------------------------------------
        # Aggregate results
        # -----------------------------------------------------------------
        report["scenarios"] = [r.as_dict() for r in scenario_results]

        total = len(scenario_results)
        passed_count = sum(1 for r in scenario_results if r.passed)
        failed_names = [r.name for r in scenario_results if not r.passed]

        report["summary"] = {
            "total_scenarios": total,
            "passed": passed_count,
            "failed": total - passed_count,
            "failed_scenarios": failed_names,
            "all_passed": passed_count == total,
        }

        if passed_count == total:
            report["status"] = "passed"
            return 0
        else:
            report["status"] = "failed"
            report["error"] = f"{total - passed_count}/{total} scenarios failed: {', '.join(failed_names)}"
            return 1

    except SkipScenario as exc:
        report["status"] = "skipped"
        report["skip_reason"] = str(exc)
        return SKIP_EXIT_CODE
    except Exception as exc:  # noqa: BLE001
        report["status"] = "failed"
        report["error"] = str(exc)
        return 1
    finally:
        # Cleanup: kill spawned pane processes via fake wezterm state
        for workspace in workspaces:
            fake_state = workspace / ".fake-wezterm"
            state_path = fake_state / "state.json"
            if state_path.exists():
                try:
                    state = json.loads(state_path.read_text(encoding="utf-8"))
                    for pane in state.get("panes", {}).values():
                        pid = pane.get("pid")
                        if pid:
                            try:
                                os.killpg(pid, signal.SIGTERM)
                            except (ProcessLookupError, PermissionError, OSError):
                                pass
                except (json.JSONDecodeError, OSError):
                    pass

        if not args.keep_workspace:
            for workspace in workspaces:
                shutil.rmtree(workspace, ignore_errors=True)
        else:
            report["workspaces_kept"] = [str(w) for w in workspaces]

        report["generated_at_utc"] = utc_now()
        report_path.parent.mkdir(parents=True, exist_ok=True)
        report_path.write_text(json.dumps(report, indent=2, sort_keys=True) + "\n", encoding="utf-8")
        print(repo_relative(report_path))


if __name__ == "__main__":
    sys.exit(main())
