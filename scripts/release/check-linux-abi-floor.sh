#!/usr/bin/env bash
# scripts/release/check-linux-abi-floor.sh — fail-closed Linux ELF ABI ceiling gate (ft-mgy5t2.32 / ft-gjsx6).
#
# Refuses any Linux binary importing versioned symbols above the supported
# portable floor (default: GLIBC <= 2.35, GLIBCXX <= 3.4.30, CXXABI <= 1.3.13)
# before DSR release upload.
#
# Usage:
#   scripts/release/check-linux-abi-floor.sh [options] <binary-or-archive...>
#   scripts/release/check-linux-abi-floor.sh [options] --symbols-file <path>
#
# Options:
#   --max-glibc VER         Max allowed GLIBC version (default: 2.35)
#   --max-glibcxx VER       Max allowed GLIBCXX version (default: 3.4.30)
#   --max-cxxabi VER        Max allowed CXXABI version (default: 1.3.13)
#   --symbols-file PATH     Inspect pre-extracted symbol/readelf text (offline testing)
#   --json                  Emit machine-readable JSON verdict
#   -h, --help              Show this help
set -euo pipefail

MAX_GLIBC="2.35"
MAX_GLIBCXX="3.4.30"
MAX_CXXABI="1.3.13"
JSON_OUTPUT=0
SYMBOLS_FILES=()
TARGETS=()

usage() {
  sed -n '2,20p' "$0" | sed 's/^# \{0,1\}//'
}

require_arg() {
  local flag="$1"
  local value="${2-}"
  if [[ -z "$value" || "$value" == --* ]]; then
    echo "error: $flag requires a value" >&2
    exit 2
  fi
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --max-glibc) require_arg "$1" "${2-}"; MAX_GLIBC="$2"; shift 2 ;;
    --max-glibcxx) require_arg "$1" "${2-}"; MAX_GLIBCXX="$2"; shift 2 ;;
    --max-cxxabi) require_arg "$1" "${2-}"; MAX_CXXABI="$2"; shift 2 ;;
    --symbols-file) require_arg "$1" "${2-}"; SYMBOLS_FILES+=("$2"); shift 2 ;;
    --json) JSON_OUTPUT=1; shift ;;
    -h|--help) usage; exit 0 ;;
    -*) echo "error: unknown option: $1" >&2; usage >&2; exit 2 ;;
    *) TARGETS+=("$1"); shift ;;
  esac
done

if [[ ${#TARGETS[@]} -eq 0 && ${#SYMBOLS_FILES[@]} -eq 0 ]]; then
  echo "error: provide at least one ELF binary/tar.xz archive or --symbols-file" >&2
  usage >&2
  exit 2
fi

command -v python3 >/dev/null 2>&1 || { echo "error: python3 is required" >&2; exit 2; }

python3 - "$MAX_GLIBC" "$MAX_GLIBCXX" "$MAX_CXXABI" "$JSON_OUTPUT" \
  "${#SYMBOLS_FILES[@]}" "${SYMBOLS_FILES[@]}" "${TARGETS[@]}" <<'PY'
import json
import os
import re
import shutil
import subprocess
import sys
import tarfile
import tempfile
from pathlib import Path

max_glibc = sys.argv[1]
max_glibcxx = sys.argv[2]
max_cxxabi = sys.argv[3]
json_output = sys.argv[4] == "1"
num_sym_files = int(sys.argv[5])
sym_files = sys.argv[6 : 6 + num_sym_files]
targets = sys.argv[6 + num_sym_files :]

FLOORS = {
    "GLIBC": max_glibc,
    "GLIBCXX": max_glibcxx,
    "CXXABI": max_cxxabi,
}

SYM_RE = re.compile(r"\b(GLIBC|GLIBCXX|CXXABI)_([0-9]+(?:\.[0-9]+)*)\b")


def parse_ver(raw: str) -> tuple[int, ...]:
    parts = raw.split(".")
    if not parts or any(not p.isdigit() for p in parts):
        raise ValueError(f"invalid version string: {raw!r}")
    return tuple(int(p) for p in parts)


def ver_gt(a: str, b: str) -> bool:
    ta, tb = parse_ver(a), parse_ver(b)
    n = max(len(ta), len(tb))
    return ta + (0,) * (n - len(ta)) > tb + (0,) * (n - len(tb))


for fam, raw in FLOORS.items():
    try:
        parse_ver(raw)
    except ValueError as exc:
        sys.stderr.write(f"error: invalid --max-{fam.lower()} floor: {exc}\n")
        sys.exit(2)


def extract_elf_symbols(path: Path) -> str:
    data = path.read_bytes()
    if not data.startswith(b"\x7fELF"):
        raise RuntimeError(f"not a Linux ELF binary: {path}")
    for tool in ("readelf", "llvm-readelf", "objdump", "llvm-objdump"):
        exe = shutil.which(tool)
        if not exe:
            continue
        args = [exe, "-V", str(path)] if "readelf" in tool else [exe, "-T", str(path)]
        res = subprocess.run(args, capture_output=True, text=True)
        if res.returncode == 0:
            return res.stdout + "\n" + res.stderr
    # Fallback: scan dynamic version-needed ASCII strings inside ELF binary
    # (verneed/dynstr store exact GLIBC_X.Y / GLIBCXX_X.Y.Z / CXXABI_X.Y.Z tokens).
    return data.decode("latin-1")


checked = []
violations = []
max_observed: dict[str, str | None] = {k: None for k in FLOORS}


def audit_text(label: str, text: str) -> None:
    seen: dict[str, set[str]] = {k: set() for k in FLOORS}
    for fam, ver in SYM_RE.findall(text):
        seen[fam].add(ver)
        cur = max_observed[fam]
        if cur is None or ver_gt(ver, cur):
            max_observed[fam] = ver
        if ver_gt(ver, FLOORS[fam]):
            violations.append(
                {
                    "target": label,
                    "family": fam,
                    "symbol_version": f"{fam}_{ver}",
                    "floor": f"{fam}_{FLOORS[fam]}",
                }
            )
    checked.append(
        {
            "target": label,
            "max_by_family": {
                fam: (max(vers, key=parse_ver) if vers else None)
                for fam, vers in seen.items()
            },
        }
    )


for sf in sym_files:
    p = Path(sf)
    if not p.is_file():
        sys.stderr.write(f"error: symbols file not found: {sf}\n")
        sys.exit(2)
    audit_text(f"symbols-file:{sf}", p.read_text(encoding="utf-8", errors="replace"))

for raw_target in targets:
    p = Path(raw_target)
    if not p.is_file():
        sys.stderr.write(f"error: target not found: {raw_target}\n")
        sys.exit(2)
    if p.name.endswith((".tar.xz", ".tar.gz", ".tar")):
        with tempfile.TemporaryDirectory(prefix="ft-abi-floor-") as td:
            with tarfile.open(p, mode="r:*") as tf:
                elf_count = 0
                for member in tf.getmembers():
                    if not member.isfile():
                        continue
                    extracted = tf.extractfile(member)
                    if extracted is None:
                        continue
                    head = extracted.read(4)
                    if head != b"\x7fELF":
                        continue
                    elf_count += 1
                    out = Path(td) / os.path.basename(member.name)
                    out.write_bytes(head + extracted.read())
                    audit_text(f"{raw_target}::{member.name}", extract_elf_symbols(out))
                if elf_count == 0:
                    violations.append(
                        {
                            "target": raw_target,
                            "family": "ELF",
                            "symbol_version": "none",
                            "floor": "at_least_one_elf_binary",
                        }
                    )
    else:
        try:
            audit_text(raw_target, extract_elf_symbols(p))
        except RuntimeError as exc:
            sys.stderr.write(f"error: {exc}\n")
            sys.exit(2)

# Deduplicate violations deterministically
uniq_violations = []
seen_keys = set()
for v in violations:
    key = (v["target"], v["family"], v["symbol_version"], v["floor"])
    if key not in seen_keys:
        seen_keys.add(key)
        uniq_violations.append(v)

ok = len(uniq_violations) == 0
payload = {
    "schema_version": "frankenterm.linux_abi_floor.v1",
    "ok": ok,
    "floor": {k: f"{k}_{v}" for k, v in FLOORS.items()},
    "max_observed": {
        k: (f"{k}_{v}" if v is not None else None) for k, v in max_observed.items()
    },
    "checked_count": len(checked),
    "checked": checked,
    "violation_count": len(uniq_violations),
    "violations": uniq_violations,
}

if json_output:
    print(json.dumps(payload, indent=2, sort_keys=True))
else:
    if ok:
        print(
            f"linux ABI floor check passed: checked={len(checked)} "
            f"max_glibc={payload['max_observed']['GLIBC'] or 'none'} "
            f"max_glibcxx={payload['max_observed']['GLIBCXX'] or 'none'} "
            f"max_cxxabi={payload['max_observed']['CXXABI'] or 'none'}"
        )
    else:
        print(f"linux ABI floor check FAILED: {len(uniq_violations)} violation(s)")
        for v in uniq_violations:
            print(f"  FAIL {v['target']}: requires {v['symbol_version']} > floor {v['floor']}")

sys.exit(0 if ok else 1)
PY
