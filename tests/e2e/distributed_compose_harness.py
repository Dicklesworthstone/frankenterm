#!/usr/bin/env python3

import argparse
import json
import os
import shutil
import socket
import sqlite3
import subprocess
import sys
import time
from pathlib import Path


PROTOCOL_VERSION = 1
DEFAULT_TIMEOUT_SECS = 30.0


def now_ms() -> int:
    return int(time.time() * 1000)


def ensure_parent(path: Path) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)


def write_json(path: Path, payload: dict) -> None:
    ensure_parent(path)
    path.write_text(json.dumps(payload, indent=2, sort_keys=True) + "\n", encoding="utf-8")


def send_json_line(writer, payload: dict) -> None:
    writer.write((json.dumps(payload) + "\n").encode("utf-8"))
    writer.flush()


def read_json_line(reader, timeout_secs: float) -> dict:
    deadline = time.monotonic() + timeout_secs
    while time.monotonic() < deadline:
        line = reader.readline()
        if line:
            return json.loads(line.decode("utf-8").strip())
        time.sleep(0.05)
    raise TimeoutError("timed out waiting for JSON line")


def make_envelope(seq: int, sender: str, payload: dict, sent_at_ms: int | None = None) -> dict:
    return {
        "version": PROTOCOL_VERSION,
        "seq": seq,
        "sender": sender,
        "sent_at_ms": sent_at_ms if sent_at_ms is not None else now_ms(),
        "payload": payload,
    }


def connect_reader_writer(host: str, port: int) -> tuple[socket.socket, object, object]:
    sock = socket.create_connection((host, port), timeout=10.0)
    reader = sock.makefile("rb")
    writer = sock.makefile("wb")
    return sock, reader, writer


def run_valid_agent(args: argparse.Namespace) -> int:
    artifact_path = Path(args.artifact)
    sock, reader, writer = connect_reader_writer(args.host, args.port)
    session_id = args.session_id or f"{args.agent_id}-compose-session"
    try:
        handshake = {
            "protocol_version": PROTOCOL_VERSION,
            "token": args.token,
            "agent_id": args.agent_id,
            "session_id": session_id,
        }
        send_json_line(writer, handshake)
        ack = read_json_line(reader, DEFAULT_TIMEOUT_SECS)
        if not ack.get("ok"):
            raise RuntimeError(f"unexpected handshake failure: {ack}")

        base = now_ms()
        pane_id = args.pane_id
        marker = args.marker
        messages = [
            make_envelope(
                1,
                args.agent_id,
                {
                    "type": "pane_meta",
                    "pane_id": pane_id,
                    "pane_uuid": "compose-pane-uuid",
                    "domain": "compose",
                    "title": "compose-remote-pane",
                    "cwd": "/compose",
                    "rows": 24,
                    "cols": 120,
                    "observed": True,
                    "timestamp_ms": base,
                },
                base,
            ),
            make_envelope(
                2,
                args.agent_id,
                {
                    "type": "pane_delta",
                    "pane_id": pane_id,
                    "seq": 0,
                    "content": f"{marker} alpha",
                    "content_len": len(f"{marker} alpha"),
                    "captured_at_ms": base + 1,
                },
                base + 1,
            ),
            make_envelope(
                3,
                args.agent_id,
                {
                    "type": "gap",
                    "pane_id": pane_id,
                    "seq_before": 0,
                    "seq_after": 2,
                    "reason": "compose_gap",
                    "detected_at_ms": base + 2,
                },
                base + 2,
            ),
            make_envelope(
                4,
                args.agent_id,
                {
                    "type": "pane_delta",
                    "pane_id": pane_id,
                    "seq": 2,
                    "content": f"{marker} beta",
                    "content_len": len(f"{marker} beta"),
                    "captured_at_ms": base + 3,
                },
                base + 3,
            ),
            make_envelope(
                5,
                args.agent_id,
                {
                    "type": "detection",
                    "rule_id": "dist.compose.detected",
                    "agent_type": "codex",
                    "event_type": "compose_smoke",
                    "severity": "info",
                    "confidence": 1.0,
                    "extracted": {"case": "compose_smoke", "marker": marker},
                    "matched_text": marker,
                    "pane_id": pane_id,
                    "pane_uuid": "compose-pane-uuid",
                    "detected_at_ms": base + 4,
                },
                base + 4,
            ),
            make_envelope(
                6,
                args.agent_id,
                {
                    "type": "pane_delta",
                    "pane_id": pane_id,
                    "seq": 1,
                    "content": f"{marker} stale",
                    "content_len": len(f"{marker} stale"),
                    "captured_at_ms": base + 5,
                },
                base + 5,
            ),
        ]

        for message in messages:
            send_json_line(writer, message)

        time.sleep(args.settle_secs)
        write_json(
            artifact_path,
            {
                "sender": args.agent_id,
                "session_id": session_id,
                "pane_id": pane_id,
                "messages_sent": len(messages),
                "marker": marker,
                "handshake_ack": ack,
                "envelope_seqs": [message["seq"] for message in messages],
            },
        )
        return 0
    finally:
        try:
            writer.close()
        except Exception:
            pass
        try:
            reader.close()
        except Exception:
            pass
        sock.close()


def run_invalid_agent(args: argparse.Namespace) -> int:
    artifact_path = Path(args.artifact)
    sock, reader, writer = connect_reader_writer(args.host, args.port)
    session_id = args.session_id or f"{args.agent_id}-compose-invalid-session"
    try:
        handshake = {
            "protocol_version": PROTOCOL_VERSION,
            "token": args.token,
            "agent_id": args.agent_id,
            "session_id": session_id,
        }
        send_json_line(writer, handshake)
        response = read_json_line(reader, DEFAULT_TIMEOUT_SECS)
        error_code = response.get("error", {}).get("code")
        if response.get("ok") is not False or error_code != "dist.auth_failed":
            raise RuntimeError(f"expected dist.auth_failed handshake response, got {response}")

        write_json(
            artifact_path,
            {
                "auth_mode": "token",
                "invalid_token_error_code": error_code,
                "redacted": True,
                "response": response,
            },
        )
        return 0
    finally:
        try:
            writer.close()
        except Exception:
            pass
        try:
            reader.close()
        except Exception:
            pass
        sock.close()


def sqlite_counts(db_path: Path, marker: str) -> dict:
    with sqlite3.connect(db_path) as conn:
        conn.row_factory = sqlite3.Row
        panes = conn.execute(
            "SELECT COUNT(*) AS count FROM panes WHERE domain LIKE 'distributed:%'"
        ).fetchone()["count"]
        segments = conn.execute(
            "SELECT COUNT(*) AS count FROM output_segments WHERE content LIKE ?",
            (f"%{marker}%",),
        ).fetchone()["count"]
        gaps = conn.execute("SELECT COUNT(*) AS count FROM output_gaps").fetchone()["count"]
        events = conn.execute(
            "SELECT COUNT(*) AS count FROM events WHERE rule_id = ?",
            ("dist.compose.detected",),
        ).fetchone()["count"]
        return {
            "pane_count": int(panes),
            "segment_count": int(segments),
            "gap_count": int(gaps),
            "event_count": int(events),
        }


def run_command(command: list[str], cwd: Path | None = None) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        command,
        cwd=str(cwd) if cwd is not None else None,
        text=True,
        capture_output=True,
        check=False,
    )


def wait_for_counts(db_path: Path, marker: str, timeout_secs: float) -> dict:
    deadline = time.monotonic() + timeout_secs
    last_counts: dict | None = None
    while time.monotonic() < deadline:
        if db_path.exists():
            try:
                counts = sqlite_counts(db_path, marker)
                last_counts = counts
                if (
                    counts["pane_count"] >= 1
                    and counts["segment_count"] >= 2
                    and counts["gap_count"] >= 1
                    and counts["event_count"] >= 1
                ):
                    return counts
            except sqlite3.Error:
                pass
        time.sleep(0.5)
    raise TimeoutError(f"timed out waiting for distributed data; last_counts={last_counts}")


def run_test_runner(args: argparse.Namespace) -> int:
    artifacts_dir = Path(args.artifacts)
    artifacts_dir.mkdir(parents=True, exist_ok=True)
    db_path = Path(args.db_path)
    aggregator_log_path = artifacts_dir / "aggregator.log"
    agent_log_path = artifacts_dir / "agent_log.json"
    security_log_path = artifacts_dir / "security_log.json"
    db_snapshot_path = artifacts_dir / "db_snapshot.sqlite"
    db_snapshot_meta_path = artifacts_dir / "db_snapshot.json"
    query_visibility_path = artifacts_dir / "query_visibility.json"
    aggregator_log_meta_path = artifacts_dir / "aggregator_log.json"
    cli_output_path = artifacts_dir / "cli_search_output.json"
    robot_output_path = artifacts_dir / "robot_search_output.json"

    if not agent_log_path.exists():
        raise FileNotFoundError(f"missing agent artifact: {agent_log_path}")
    if not security_log_path.exists():
        raise FileNotFoundError(f"missing security artifact: {security_log_path}")

    counts = wait_for_counts(db_path, args.marker, args.timeout_secs)
    shutil.copy2(db_path, db_snapshot_path)

    common_prefix = [
        args.binary,
        "--workspace",
        args.workspace,
        "--config",
        args.config,
    ]

    cli_result = run_command(
        common_prefix + ["search", args.marker, "-f", "json", "--limit", "10"]
    )
    if cli_result.returncode != 0 or args.marker not in cli_result.stdout:
        raise RuntimeError(
            f"cli search failed rc={cli_result.returncode} stdout={cli_result.stdout!r} stderr={cli_result.stderr!r}"
        )
    cli_output_path.write_text(cli_result.stdout, encoding="utf-8")

    robot_result = run_command(
        common_prefix
        + ["robot", "--format", "json", "search", args.marker, "--limit", "10"]
    )
    if robot_result.returncode != 0:
        raise RuntimeError(
            f"robot search failed rc={robot_result.returncode} stdout={robot_result.stdout!r} stderr={robot_result.stderr!r}"
        )
    robot_output_path.write_text(robot_result.stdout, encoding="utf-8")
    robot_payload = json.loads(robot_result.stdout)
    if not robot_payload.get("ok"):
        raise RuntimeError(f"robot search returned error payload: {robot_payload}")
    robot_hits = robot_payload.get("data", {}).get("results", [])
    if not any(
        args.marker in (hit.get("content") or "") or args.marker in (hit.get("snippet") or "")
        for hit in robot_hits
    ):
        raise RuntimeError(f"robot search did not surface marker {args.marker!r}")

    security_payload = json.loads(security_log_path.read_text(encoding="utf-8"))
    invalid_code = security_payload.get("invalid_token_error_code")
    if invalid_code != "dist.auth_failed":
        raise RuntimeError(f"expected dist.auth_failed, got {invalid_code!r}")

    write_json(
        aggregator_log_meta_path,
        {
            "path": str(aggregator_log_path),
            "exists": aggregator_log_path.exists(),
            "listener_ready": aggregator_log_path.exists()
            and "Distributed listener started"
            in aggregator_log_path.read_text(encoding="utf-8", errors="replace"),
        },
    )
    write_json(
        db_snapshot_meta_path,
        {
            "path": str(db_snapshot_path),
            "size_bytes": db_snapshot_path.stat().st_size,
            "pane_count": counts["pane_count"],
            "segment_count": counts["segment_count"],
            "event_count": counts["event_count"],
            "gap_count": counts["gap_count"],
        },
    )
    write_json(
        query_visibility_path,
        {
            "cli_search_marker_found": True,
            "robot_equivalent": {
                "total_hits": len(robot_hits),
                "result_pane_ids": [hit.get("pane_id") for hit in robot_hits],
            },
        },
    )

    summary = {
        "marker": args.marker,
        "counts": counts,
        "cli_search_rc": cli_result.returncode,
        "robot_search_hits": len(robot_hits),
        "db_path": str(db_path),
    }
    write_json(artifacts_dir / "compose_summary.json", summary)
    return 0


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description="Distributed compose E2E harness")
    subparsers = parser.add_subparsers(dest="command", required=True)

    valid = subparsers.add_parser("fake-agent", help="Send valid distributed payloads")
    valid.add_argument("--host", required=True)
    valid.add_argument("--port", required=True, type=int)
    valid.add_argument("--token", required=True)
    valid.add_argument("--agent-id", required=True)
    valid.add_argument("--pane-id", type=int, default=77)
    valid.add_argument("--marker", required=True)
    valid.add_argument("--artifact", required=True)
    valid.add_argument("--session-id")
    valid.add_argument("--settle-secs", type=float, default=1.0)

    invalid = subparsers.add_parser("invalid-agent", help="Assert invalid token rejection")
    invalid.add_argument("--host", required=True)
    invalid.add_argument("--port", required=True, type=int)
    invalid.add_argument("--token", required=True)
    invalid.add_argument("--agent-id", required=True)
    invalid.add_argument("--artifact", required=True)
    invalid.add_argument("--session-id")

    runner = subparsers.add_parser("test-runner", help="Assert compose smoke artifacts")
    runner.add_argument("--binary", required=True)
    runner.add_argument("--workspace", required=True)
    runner.add_argument("--config", required=True)
    runner.add_argument("--db-path", required=True)
    runner.add_argument("--artifacts", required=True)
    runner.add_argument("--marker", required=True)
    runner.add_argument("--timeout-secs", type=float, default=30.0)

    return parser


def main() -> int:
    args = build_parser().parse_args()
    if args.command == "fake-agent":
        return run_valid_agent(args)
    if args.command == "invalid-agent":
        return run_invalid_agent(args)
    if args.command == "test-runner":
        return run_test_runner(args)
    raise AssertionError(f"unsupported command {args.command}")


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except Exception as exc:
        print(f"[distributed_compose_harness] {exc}", file=sys.stderr)
        raise
