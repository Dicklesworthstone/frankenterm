#!/usr/bin/env bash
# ft-g6lfc / ft-xbnl0.3.6 — supported-path stub marker guard.
#
# This is the cargo-free CI guard for the supported-path truth sweep:
# production Rust paths must not grow active todo!/unimplemented!()
# markers or panic-based "not implemented" stubs. Test-only fake panes
# and the explicitly unsupported non-Rust SDK template markers remain
# classified exclusions.
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
OUT_PATH="${ROOT_DIR}/docs/ft-xbnl0-3-6-supported-path-stub-markers-validation.json"

usage() {
  cat <<'USAGE'
Usage: check_supported_path_stub_markers.sh [options]

Options:
  --output <path>    Output validation JSON path
  -h, --help         Show this help
USAGE
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --output) OUT_PATH="$2"; shift 2 ;;
    -h|--help) usage; exit 0 ;;
    *) echo "Unknown option: $1" >&2; usage >&2; exit 2 ;;
  esac
done

if ! command -v python3 >/dev/null 2>&1; then
  echo "error: python3 is required for supported-path stub marker guard" >&2
  exit 2
fi

mkdir -p "$(dirname "${OUT_PATH}")"

python3 - "${ROOT_DIR}" "${OUT_PATH}" <<'PY'
from __future__ import annotations

import datetime as dt
import json
import re
import sys
from pathlib import Path

root = Path(sys.argv[1])
out_path = Path(sys.argv[2])

scan_roots = [root / "crates", root / "frankenterm"]
macro_re = re.compile(r"\b(?:todo|unimplemented)!\s*\(")
panic_stub_re = re.compile(
    r"\bpanic!\s*\([^;\n]*(?:not\s+implemented|unimplemented|placeholder|todo)",
    re.IGNORECASE,
)
cfg_test_re = re.compile(r"#\s*\[\s*cfg\s*\((?:test|all\s*\([^)]*test[^)]*\)|any\s*\([^)]*test[^)]*\))\)\s*\]")
scope_start_re = re.compile(r"\b(?:mod|fn)\s+[A-Za-z_][A-Za-z0-9_]*\b")


def strip_line_code(line: str) -> str:
    """Return line text with strings/chars removed and // comments truncated."""
    out: list[str] = []
    i = 0
    in_string = False
    in_char = False
    escaped = False
    while i < len(line):
        ch = line[i]
        nxt = line[i + 1] if i + 1 < len(line) else ""
        if in_string:
            if escaped:
                escaped = False
            elif ch == "\\":
                escaped = True
            elif ch == '"':
                in_string = False
            out.append(" ")
            i += 1
            continue
        if in_char:
            if escaped:
                escaped = False
            elif ch == "\\":
                escaped = True
            elif ch == "'":
                in_char = False
            out.append(" ")
            i += 1
            continue
        if ch == "/" and nxt == "/":
            break
        if ch == '"':
            in_string = True
            out.append(" ")
            i += 1
            continue
        if ch == "'":
            in_char = True
            out.append(" ")
            i += 1
            continue
        out.append(ch)
        i += 1
    return "".join(out)


def rust_files() -> list[Path]:
    paths: list[Path] = []
    for scan_root in scan_roots:
        if not scan_root.exists():
            continue
        for path in scan_root.rglob("*.rs"):
            parts = set(path.relative_to(root).parts)
            if "target" in parts or ".git" in parts:
                continue
            if "tests" in parts or "benches" in parts or "examples" in parts:
                continue
            paths.append(path)
    return sorted(paths)


def classify_file(path: Path) -> tuple[list[dict[str, object]], list[dict[str, object]]]:
    rel = path.relative_to(root).as_posix()
    try:
        lines = path.read_text(encoding="utf-8").splitlines()
    except UnicodeDecodeError:
        lines = path.read_text(encoding="utf-8", errors="replace").splitlines()

    depth = 0
    pending_test_attr = False
    test_scope_depths: list[int] = []
    allowed: list[dict[str, object]] = []
    unexpected: list[dict[str, object]] = []

    for idx, line in enumerate(lines, start=1):
        code = strip_line_code(line)
        inside_test_scope = any(depth >= scope_depth for scope_depth in test_scope_depths)

        markers: list[str] = []
        if macro_re.search(code):
            markers.append("macro_stub_marker")
        if panic_stub_re.search(line):
            markers.append("panic_stub_marker")
        if "transport not wired" in line:
            markers.append("transport_not_wired_marker")

        for marker in markers:
            entry = {
                "path": rel,
                "line": idx,
                "marker": marker,
                "source": line.strip(),
            }
            if marker in {"macro_stub_marker", "panic_stub_marker"} and inside_test_scope:
                entry["classification"] = "test_only_cfg_scope"
                allowed.append(entry)
            elif (
                marker == "transport_not_wired_marker"
                and rel == "crates/frankenterm-core/src/robot_sdk_contracts.rs"
            ):
                entry["classification"] = "non_rust_sdk_template_or_assertion_guard"
                allowed.append(entry)
            else:
                entry["classification"] = "unexpected_supported_path_stub_marker"
                unexpected.append(entry)

        if cfg_test_re.search(code):
            pending_test_attr = True

        opens = code.count("{")
        closes = code.count("}")
        if pending_test_attr and opens > 0 and scope_start_re.search(code):
            test_scope_depths.append(depth + opens)
            pending_test_attr = False
        elif pending_test_attr:
            stripped = code.strip()
            if stripped and not stripped.startswith("#["):
                pending_test_attr = False

        depth += opens - closes
        test_scope_depths = [scope_depth for scope_depth in test_scope_depths if depth >= scope_depth]

    return allowed, unexpected


all_allowed: list[dict[str, object]] = []
all_unexpected: list[dict[str, object]] = []
files = rust_files()
for path in files:
    allowed, unexpected = classify_file(path)
    all_allowed.extend(allowed)
    all_unexpected.extend(unexpected)

status = "passed" if not all_unexpected else "failed"
report = {
    "contract_id": "ft.xbnl0.3.6.supported_path_stub_markers.v1",
    "bead_id": "ft-g6lfc",
    "upstream_contracts": ["ft-xbnl0.3.6", "ft-xbnl0.5.2"],
    "checked_at": dt.datetime.now(dt.UTC).replace(microsecond=0).isoformat().replace("+00:00", "Z"),
    "status": status,
    "scan_roots": [path.relative_to(root).as_posix() for path in scan_roots],
    "totals": {
        "rust_files_scanned": len(files),
        "allowed_markers": len(all_allowed),
        "unexpected_markers": len(all_unexpected),
    },
    "allowed_markers": all_allowed,
    "unexpected_markers": all_unexpected,
}

out_path.write_text(json.dumps(report, indent=2, sort_keys=True) + "\n", encoding="utf-8")

if all_unexpected:
    print("supported-path stub marker guard FAILED", file=sys.stderr)
    for item in all_unexpected:
        print(
            f"  {item['path']}:{item['line']}: {item['marker']}: {item['source']}",
            file=sys.stderr,
        )
    sys.exit(1)

print(
    "supported-path stub marker guard passed: "
    f"{len(files)} Rust files scanned, {len(all_allowed)} classified marker(s), 0 unexpected"
)
PY
