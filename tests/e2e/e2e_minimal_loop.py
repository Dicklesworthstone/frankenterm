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
DEFAULT_TIMEOUT_SECS = 45.0
MINIMAL_CONFIG = """\
[ingest]
poll_interval_ms = 100
batch_size = 50
max_segment_bytes = 65536

[storage]
retention_days = 1

[logging]
level = "debug"
format = "json"
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


def resolve_wezterm_binary() -> tuple[str | None, str, str | None]:
    env_binary = os.environ.get("FT_WEZTERM_CLI")
    if env_binary:
        env_path = Path(env_binary).expanduser()
        if env_path.is_file() and os.access(env_path, os.X_OK):
            return str(env_path), "env:FT_WEZTERM_CLI", None
        return str(env_path), "env:FT_WEZTERM_CLI", "FT_WEZTERM_CLI is not executable"

    path_binary = shutil.which("wezterm")
    if path_binary:
        return path_binary, "path:which_wezterm", None
    return None, "path:which_wezterm", "wezterm is not available in PATH"


def wezterm_binary() -> str:
    path, _, error = resolve_wezterm_binary()
    if not path:
        raise RuntimeError(error or "wezterm backend is unavailable")
    return path


def resolve_wezterm() -> dict[str, Any]:
    wezterm, source, error = resolve_wezterm_binary()
    info: dict[str, Any] = {"path": wezterm, "source": source, "available": False}
    if error:
        info["error"] = error
        return info

    list_result = run([wezterm, "cli", "list"], cwd=REPO_ROOT)
    info["cli_list"] = list_result.as_dict()
    if list_result.returncode != 0:
        info["error"] = "wezterm cli list failed; active backend is unavailable"
        return info
    info["available"] = True
    return info


def make_workspace() -> tuple[Path, Path]:
    workspace = Path(tempfile.mkdtemp(prefix="ft-e2e-minimal-"))
    data_dir = workspace / ".ft"
    data_dir.mkdir(parents=True, exist_ok=True)
    config_path = workspace / "ft.toml"
    config_path.write_text(MINIMAL_CONFIG, encoding="utf-8")
    return workspace, config_path


def write_pane_script(workspace: Path, name: str, lines: list[str], fixture_text: str | None = None) -> Path:
    script_path = workspace / f"{name}.sh"
    body = ["#!/bin/bash", "set -euo pipefail"]
    for line in lines:
        body.append(f"printf '%s\\n' {shlex.quote(line)}")
    if fixture_text:
        body.append("cat <<'EOF_FIXTURE'")
        body.append(fixture_text.rstrip("\n"))
        body.append("EOF_FIXTURE")
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


def ft_env(workspace: Path) -> dict[str, str]:
    env = os.environ.copy()
    env["FT_WORKSPACE"] = str(workspace)
    env["FT_DATA_DIR"] = str(workspace / ".ft")
    env["FT_OUTPUT_FORMAT"] = "json"
    return env


def ft_cmd(ft_binary: str, workspace: Path, config_path: Path, *args: str) -> list[str]:
    return [ft_binary, "--workspace", str(workspace), "--config", str(config_path), *args]


def spawn_pane(workspace: Path, script_path: Path) -> int:
    result = run(
        [wezterm_binary(), "cli", "spawn", "--cwd", str(workspace), "--", "bash", str(script_path)],
        cwd=REPO_ROOT,
        check=False,
    )
    if result.returncode != 0:
        raise RuntimeError(f"failed to spawn pane for {script_path.name}: {result.stderr or result.stdout}")
    for token in result.stdout.split():
        if token.isdigit():
            return int(token)
    raise RuntimeError(f"wezterm spawn did not return a pane id for {script_path.name}: {result.stdout}")


def close_pane(pane_id: int) -> None:
    run([wezterm_binary(), "cli", "kill-pane", "--pane-id", str(pane_id)], cwd=REPO_ROOT)


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


def extract_text(payload: Any) -> str:
    data = payload.get("data") if isinstance(payload, dict) else None
    if isinstance(data, dict) and isinstance(data.get("text"), str):
        return data["text"]
    return ""


def main() -> int:
    parser = argparse.ArgumentParser(description="Minimal ft watch + robot loop proof")
    parser.add_argument("--timeout-secs", type=float, default=DEFAULT_TIMEOUT_SECS)
    parser.add_argument("--output", default="", help="Override report path")
    parser.add_argument("--keep-workspace", action="store_true", help="Preserve the temp workspace for debugging")
    args = parser.parse_args()

    timestamp = datetime.now(timezone.utc).strftime("%Y%m%dT%H%M%SZ")
    output_dir = ensure_output_dir()
    report_path = Path(args.output) if args.output else output_dir / f"e2e_minimal_loop_{timestamp}.json"
    watch_log_path = output_dir / f"e2e_minimal_loop_{timestamp}.watch.log"

    report: dict[str, Any] = {
        "schema_version": "ft.e2e_minimal_loop.v1",
        "generated_at_utc": utc_now(),
        "bead_id": "ft-d0ez0.1",
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
        "assertions": {},
        "notes": [
            "Binary discovery order: FT_BINARY, cargo build output, then PATH which ft.",
            "Use --keep-workspace to preserve the temp workspace for debugging.",
        ],
    }

    workspace: Path | None = None
    config_path: Path | None = None
    watch_proc: subprocess.Popen[str] | None = None
    spawned_panes: list[int] = []

    try:
        codex_fixture_path = FIXTURE_DIR / "codex" / "usage_reached.txt"
        claude_fixture_path = FIXTURE_DIR / "claude_code" / "usage_reached.txt"
        report["fixtures"] = {
            "codex_usage_reached": repo_relative(codex_fixture_path),
            "claude_code_usage_reached": repo_relative(claude_fixture_path),
        }

        binary = resolve_ft_binary()
        report["binary_resolution"] = {
            "selected_path": binary.path,
            "selected_source": binary.source,
            "candidates": binary.candidates,
        }

        backend = resolve_wezterm()
        report["backend"] = backend

        missing_reasons: list[str] = []
        if not binary.path:
            missing_reasons.append("ft binary not found via FT_BINARY, cargo build output, or PATH")
        if not backend.get("available", False):
            missing_reasons.append(str(backend.get("error", "active backend is unavailable")))
        if missing_reasons:
            raise SkipScenario("; ".join(missing_reasons))

        workspace, config_path = make_workspace()
        report["workspace"] = {
            "root": str(workspace),
            "data_dir": str(workspace / ".ft"),
            "config_path": str(config_path),
        }

        codex_fixture = codex_fixture_path.read_text(encoding="utf-8").strip()
        claude_fixture = claude_fixture_path.read_text(encoding="utf-8").strip()

        codex_script = write_pane_script(
            workspace,
            "pane_codex_usage",
            ["[codex] fixture begin"],
            codex_fixture,
        )
        claude_script = write_pane_script(
            workspace,
            "pane_claude_usage",
            ["[claude_code] fixture begin"],
            claude_fixture,
        )
        control_script = write_pane_script(
            workspace,
            "pane_control",
            ["[control] swarm heartbeat stable", "[control] minimal loop ready"],
        )

        env = ft_env(workspace)
        watch_argv = ft_cmd(binary.path, workspace, config_path, "watch", "--foreground")
        with watch_log_path.open("w", encoding="utf-8") as watch_log:
            watch_proc = subprocess.Popen(
                watch_argv,
                cwd=str(REPO_ROOT),
                env=env,
                stdout=watch_log,
                stderr=subprocess.STDOUT,
                text=True,
            )

        time.sleep(1.0)
        if watch_proc.poll() is not None:
            raise RuntimeError(f"ft watch exited immediately with code {watch_proc.returncode}")

        for script_path in (codex_script, claude_script, control_script):
            pane_id = spawn_pane(workspace, script_path)
            spawned_panes.append(pane_id)
            report["spawned_panes"].append({"pane_id": pane_id, "script": script_path.name})

        def state_ready() -> bool:
            result = run(ft_cmd(binary.path, workspace, config_path, "robot", "state"), cwd=REPO_ROOT, env=env)
            payload = parse_json_output(result)
            report["commands"]["robot_state_wait"] = result.as_dict()
            panes = extract_panes(payload)
            observed_ids = {int(pane["pane_id"]) for pane in panes if "pane_id" in pane}
            return all(pane_id in observed_ids for pane_id in spawned_panes)

        wait_for("robot state to observe all spawned panes", args.timeout_secs, state_ready)

        def events_ready() -> bool:
            result = run(
                ft_cmd(binary.path, workspace, config_path, "robot", "events", "--limit", "20"),
                cwd=REPO_ROOT,
                env=env,
            )
            payload = parse_json_output(result)
            report["commands"]["robot_events_wait"] = result.as_dict()
            rule_ids = {event.get("rule_id") for event in extract_events(payload)}
            return "codex.usage.reached" in rule_ids and "claude_code.usage.reached" in rule_ids

        wait_for("usage-limit events from codex and claude_code", args.timeout_secs, events_ready)

        def search_ready() -> bool:
            result = run(
                ft_cmd(binary.path, workspace, config_path, "robot", "search", "usage limit"),
                cwd=REPO_ROOT,
                env=env,
            )
            payload = parse_json_output(result)
            report["commands"]["robot_search_wait"] = result.as_dict()
            return any(int(hit.get("pane_id", -1)) == spawned_panes[0] for hit in extract_search_results(payload))

        wait_for("robot search to find the codex usage-limit pane", args.timeout_secs, search_ready)

        final_state = run(ft_cmd(binary.path, workspace, config_path, "robot", "state"), cwd=REPO_ROOT, env=env, check=True)
        final_events = run(
            ft_cmd(binary.path, workspace, config_path, "robot", "events", "--limit", "20"),
            cwd=REPO_ROOT,
            env=env,
            check=True,
        )
        final_search = run(
            ft_cmd(binary.path, workspace, config_path, "robot", "search", "usage limit"),
            cwd=REPO_ROOT,
            env=env,
            check=True,
        )
        final_get_text = run(
            ft_cmd(binary.path, workspace, config_path, "robot", "get-text", str(spawned_panes[0]), "--tail", "5"),
            cwd=REPO_ROOT,
            env=env,
            check=True,
        )

        report["commands"]["robot_state"] = final_state.as_dict()
        report["commands"]["robot_events"] = final_events.as_dict()
        report["commands"]["robot_search"] = final_search.as_dict()
        report["commands"]["robot_get_text"] = final_get_text.as_dict()

        state_payload = parse_json_output(final_state)
        events_payload = parse_json_output(final_events)
        search_payload = parse_json_output(final_search)
        get_text_payload = parse_json_output(final_get_text)

        observed_ids = {int(pane["pane_id"]) for pane in extract_panes(state_payload)}
        event_rule_ids = {event.get("rule_id") for event in extract_events(events_payload)}
        search_pane_ids = [int(hit.get("pane_id", -1)) for hit in extract_search_results(search_payload)]
        pane_text = extract_text(get_text_payload)

        assertions = {
            "robot_state_observed_all_spawned_panes": all(pane_id in observed_ids for pane_id in spawned_panes),
            "robot_events_include_codex_usage_reached": "codex.usage.reached" in event_rule_ids,
            "robot_events_include_claude_code_usage_reached": "claude_code.usage.reached" in event_rule_ids,
            "robot_search_usage_limit_hits_codex_pane": spawned_panes[0] in search_pane_ids,
            "robot_get_text_tail_contains_codex_fixture": codex_fixture in pane_text,
        }
        report["assertions"] = assertions

        if not all(assertions.values()):
            raise RuntimeError(f"one or more assertions failed: {assertions}")

        report["status"] = "passed"
        return 0
    except SkipScenario as exc:
        report["status"] = "skipped"
        report["skip_reason"] = str(exc)
        return SKIP_EXIT_CODE
    except Exception as exc:  # noqa: BLE001
        report["status"] = "failed"
        report["error"] = str(exc)
        return 1
    finally:
        for pane_id in reversed(spawned_panes):
            close_pane(pane_id)
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
