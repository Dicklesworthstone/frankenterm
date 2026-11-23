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

# ELF inputs require a successful, bounded readelf inspection. Text fixtures
# accept readelf --version-info or objdump -T records, never arbitrary strings.
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd -P)"
# The ${arr[@]+...} form keeps an empty array legal under `set -u` in the
# macOS system bash 3.2, which otherwise aborts as "unbound variable".
python3 - "$SCRIPT_DIR/../check-linux-release-abi.py" \
  "$MAX_GLIBC" "$MAX_GLIBCXX" "$MAX_CXXABI" "$JSON_OUTPUT" \
  "${#SYMBOLS_FILES[@]}" ${SYMBOLS_FILES[@]+"${SYMBOLS_FILES[@]}"} \
  ${TARGETS[@]+"${TARGETS[@]}"} <<'PY'
import json
import re
import runpy
import shutil
import subprocess
import sys
import tarfile
import tempfile
import zlib
from pathlib import Path

# A corrupt compressed stream raises the codec's own error while member data is
# read, which is neither OSError nor TarError. Without lzma, .tar.xz fails to
# open with tarfile's CompressionError instead.
try:
    import lzma

    ARCHIVE_DECODE_ERRORS = (zlib.error, lzma.LZMAError)
except ImportError:
    ARCHIVE_DECODE_ERRORS = (zlib.error,)

# Share the strict, count-checked verneed parser and bounded readelf invocation
# with the architecture-specific release verifier. Its target/provider policy is
# intentionally not applied here: this command checks the requested ceilings.
parser = runpy.run_path(sys.argv[1])
readelf = parser["readelf"]
version_needs = parser["version_needs"]
elf_field = parser["field"]
supported_name = parser["supported_name"]
Refusal = parser["Refusal"]

max_glibc = sys.argv[2]
max_glibcxx = sys.argv[3]
max_cxxabi = sys.argv[4]
json_output = sys.argv[5] == "1"
num_sym_files = int(sys.argv[6])
sym_files = sys.argv[7 : 7 + num_sym_files]
targets = sys.argv[7 + num_sym_files :]

FLOORS = {
    "GLIBC": max_glibc,
    "GLIBCXX": max_glibcxx,
    "CXXABI": max_cxxabi,
}
SYM_RE = re.compile(r"(GLIBC|GLIBCXX|CXXABI)_([0-9]+(?:\.[0-9]+)*)")
MAX_BINARY_BYTES = 1024 * 1024 * 1024
MAX_TRANSCRIPT_BYTES = 16 * 1024 * 1024
MAX_ARCHIVE_ENTRIES = 100_000
MAX_ARCHIVE_BYTES = 4 * MAX_BINARY_BYTES
MAX_ARCHIVE_NAME_BYTES = 8 * 1024 * 1024


def parse_ver(raw: str) -> tuple[int, ...]:
    if len(raw) > 128 or not re.fullmatch(r"[0-9]+(?:\.[0-9]+)*", raw):
        raise ValueError(f"invalid version string: {raw!r}")
    return tuple(int(part) for part in raw.split("."))


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


def required_versions(needs) -> list[str]:
    result = []
    for _, _, versions in needs:
        for name, weak in versions:
            if not supported_name(name):
                raise Refusal(f"unsupported ABI requirement: {name}")
            # Weak verneed entries are optional, not a mandatory ABI floor.
            if not weak:
                result.append(name)
    return result


def versions_from_transcript(text: str) -> list[str]:
    if re.search(r"^Version needs section ", text, re.M):
        return required_versions(version_needs(text))
    # objdump -T excerpts are accepted for fixtures. Only undefined symbols
    # impose requirements; exported definitions and incidental strings do not.
    requirements = []
    imports = 0
    for line in text.splitlines():
        if "*UND*" not in line:
            continue
        match = re.fullmatch(
            r"\s*[0-9a-fA-F]+\s+[a-zA-Z! \t]*\*UND\*\s+[0-9a-fA-F]+\s+"
            r"(?:\(([^()\s]+)\)|([^()\s]+))\s+\S+\s*", line
        )
        if match is None:
            raise Refusal("unparsed undefined-symbol record")
        imports += 1
        name = match[1] or match[2]
        if name == "Base":
            continue
        if not supported_name(name):
            raise Refusal(f"unsupported ABI requirement: {name}")
        requirements.append(name)
    if imports == 0:
        raise Refusal("transcript contains no version-needs section or undefined symbols")
    return requirements


def versions_from_reports(header: str, program: str, dynamic: str, versions: str) -> list[str]:
    if elf_field(header, "Class") not in ("ELF32", "ELF64"):
        raise Refusal("invalid ELF class")
    if elf_field(header, "Data") not in (
        "2's complement, little endian", "2's complement, big endian"
    ):
        raise Refusal("invalid ELF byte order")
    if elf_field(header, "Type").split(" ", 1)[0] not in ("EXEC", "DYN"):
        raise Refusal("ELF is not an executable or shared object")
    if not elf_field(header, "Machine") or int(elf_field(header, "Number of program headers")) <= 0:
        raise Refusal("ELF has no executable program headers")
    if not re.search(r"^\s*LOAD\s", program, re.M):
        raise Refusal("ELF has no loadable segment")

    # GNU readelf reports a missing dynamic section and missing version data in
    # words; llvm-readelf prints nothing for either. Both are credible only for
    # a binary with no PT_DYNAMIC segment, the table the loader actually reads.
    if dynamic.strip() in ("", "There is no dynamic section in this file."):
        if re.search(r"^\s*DYNAMIC\s", program, re.M):
            raise Refusal("dynamic segment exists but no dynamic section was reported")
        if re.search(r"^\s*INTERP\s", program, re.M) or "[Requesting program interpreter:" in program:
            raise Refusal("ELF interpreter exists without a dynamic section")
        if versions.strip() not in ("", "No version information found in this file."):
            raise Refusal("static ELF has unexpected version metadata")
        return []

    sections = re.findall(
        r"^Dynamic section .* contains (\d+) entries:\n(.*)\Z", dynamic, re.M | re.S
    )
    if len(sections) != 1:
        raise Refusal("missing or duplicate dynamic section")
    count, body = sections[0]
    entries = [
        line for line in body.splitlines()
        if line.strip() and not line.strip().startswith("Tag ")
    ]
    if len(entries) != int(count) or not all(
        re.fullmatch(r"\s*0x[0-9a-fA-F]+\s+\([^)]+\)\s+.+", line) for line in entries
    ):
        raise Refusal("unparsed or truncated dynamic section")
    if not re.search(r"\(NULL\)\s+0x0\s*$", dynamic, re.M):
        raise Refusal("unterminated dynamic section")
    needed = re.findall(r"\(NEEDED\)\s+Shared library: \[([^\]]+)\]", dynamic)
    if len(needed) != len(re.findall(r"\(NEEDED\)", dynamic)) or len(needed) != len(set(needed)):
        raise Refusal("unparsed or duplicate dynamic dependency")

    if not re.search(r"^Version needs section ", versions, re.M):
        if re.search(r"\(VERNEED(?:NUM)?\)", dynamic):
            raise Refusal("dynamic version-needs metadata has no matching section")
        if versions.strip() != "No version information found in this file." and not re.search(
            r"^Version (?:definition|symbols) section ", versions, re.M
        ):
            raise Refusal("unrecognized version information")
        # A fully inspected static PIE or unversioned binary has no versioned
        # imports. Definitions alone do not impose a libc requirement.
        return []

    needs = version_needs(versions)
    if re.findall(r"\(VERNEEDNUM\)\s+(\d+)\s*$", dynamic, re.M) != [str(len(needs))]:
        raise Refusal("dynamic version provider count mismatch")
    if len(re.findall(r"\(VERNEED\)", dynamic)) != 1:
        raise Refusal("missing or duplicate version-needs address")
    if any(provider not in needed for provider, _, _ in needs):
        raise Refusal("version provider is absent from dynamic dependencies")
    return required_versions(needs)


def extract_elf_versions(path: Path) -> list[str]:
    size = path.stat().st_size
    if size < 4 or size > MAX_BINARY_BYTES:
        raise Refusal("ELF input is empty, truncated, or exceeds the 1 GiB bound")
    with path.open("rb") as source:
        if source.read(4) != b"\x7fELF":
            raise Refusal(f"not a Linux ELF binary: {path}")
    # Fall back to llvm-readelf only when readelf could not be started. Once a
    # tool has run, its failure, warning, timeout, or unparseable report is the
    # verdict: retrying with another tool would let the most lenient one pass a
    # binary the other flagged as malformed.
    failures = []
    for tool in ("readelf", "llvm-readelf"):
        exe = shutil.which(tool)
        if not exe:
            continue
        try:
            reports = [
                readelf(exe, option, path) for option in
                ("--file-header", "--program-headers", "--dynamic", "--version-info")
            ]
            return versions_from_reports(*reports)
        except OSError as error:
            failures.append(f"{tool} could not run: {error}")
        except (Refusal, ValueError, subprocess.SubprocessError) as error:
            raise Refusal(f"ELF inspection failed: {tool}: {error}") from None
    if failures:
        raise Refusal("ELF inspection failed: " + "; ".join(failures))
    raise Refusal("readelf or llvm-readelf is required; raw-byte scanning is not ABI evidence")


checked = []
violations = []
inspection_errors = []
max_observed: dict[str, str | None] = {k: None for k in FLOORS}


def audit_versions(label: str, versions: list[str], inspection: str) -> None:
    seen: dict[str, set[str]] = {k: set() for k in FLOORS}
    for name in versions:
        match = SYM_RE.fullmatch(name)
        if match is None:
            # required_versions already checked the shared explicit namespace
            # allowlist (including GCC/ZLIB and the named CXXABI exceptions).
            continue
        fam, ver = match.groups()
        seen[fam].add(ver)
        cur = max_observed[fam]
        if cur is None or ver_gt(ver, cur):
            max_observed[fam] = ver
        if ver_gt(ver, FLOORS[fam]):
            violations.append(
                {
                    "target": label,
                    "family": fam,
                    "symbol_version": name,
                    "floor": f"{fam}_{FLOORS[fam]}",
                }
            )
    checked.append(
        {
            "target": label,
            "inspection": inspection,
            "max_by_family": {
                fam: (max(vers, key=parse_ver) if vers else None)
                for fam, vers in seen.items()
            },
        }
    )


for sf in sym_files:
    try:
        p = Path(sf)
        if not p.is_file() or not 0 < p.stat().st_size <= MAX_TRANSCRIPT_BYTES:
            raise Refusal("symbols transcript is missing, empty, or exceeds the 16 MiB bound")
        audit_versions(
            f"symbols-file:{sf}",
            versions_from_transcript(p.read_text(encoding="utf-8")),
            "symbol_transcript",
        )
    except (Refusal, OSError, ValueError) as error:
        inspection_errors.append({"target": sf, "reason": str(error)})

for raw_target in targets:
    try:
        p = Path(raw_target)
        if not p.is_file():
            raise Refusal("target is not a regular file")
        if p.name.endswith((".tar.xz", ".tar.gz", ".tar")):
            if p.stat().st_size > MAX_BINARY_BYTES:
                raise Refusal("archive exceeds the 1 GiB input bound")
            with tempfile.TemporaryDirectory(prefix="ft-abi-floor-") as td:
                with tarfile.open(p, mode="r:*") as tf:
                    elf_count = 0
                    payload_bytes = 0
                    name_bytes = 0
                    for index, member in enumerate(tf):
                        name_bytes += len(member.name.encode("utf-8", errors="surrogateescape"))
                        payload_bytes += member.size
                        if (index >= MAX_ARCHIVE_ENTRIES or
                                name_bytes > MAX_ARCHIVE_NAME_BYTES or
                                member.size < 0 or member.size > MAX_BINARY_BYTES or
                                payload_bytes > MAX_ARCHIVE_BYTES):
                            raise Refusal("archive exceeds its member, name, or expanded-size bound")
                        if not member.isfile():
                            continue
                        extracted = tf.extractfile(member)
                        if extracted is None:
                            raise Refusal("regular archive member could not be read")
                        with extracted:
                            head = extracted.read(4)
                            if head != b"\x7fELF":
                                continue
                            elf_count += 1
                            # Never use archive member paths as filesystem paths,
                            # including duplicate basenames or traversal names.
                            out = Path(td) / f"member-{index}.elf"
                            with out.open("xb") as destination:
                                destination.write(head)
                                shutil.copyfileobj(extracted, destination, 1024 * 1024)
                            audit_versions(
                                f"{raw_target}::{member.name}",
                                extract_elf_versions(out),
                                "elf_version_needs",
                            )
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
            audit_versions(raw_target, extract_elf_versions(p), "elf_version_needs")
    except (
        Refusal, OSError, ValueError, tarfile.TarError, EOFError, *ARCHIVE_DECODE_ERRORS
    ) as error:
        inspection_errors.append({"target": raw_target, "reason": str(error)})

# Deduplicate violations deterministically
uniq_violations = []
seen_keys = set()
for v in violations:
    key = (v["target"], v["family"], v["symbol_version"], v["floor"])
    if key not in seen_keys:
        seen_keys.add(key)
        uniq_violations.append(v)

ok = not uniq_violations and not inspection_errors
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
    "inspection_error_count": len(inspection_errors),
    "inspection_errors": inspection_errors,
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
        print(
            f"linux ABI floor check FAILED: {len(uniq_violations)} violation(s), "
            f"{len(inspection_errors)} inspection error(s)"
        )
        for v in uniq_violations:
            print(f"  FAIL {v['target']}: requires {v['symbol_version']} > floor {v['floor']}")
        for error in inspection_errors:
            print(f"  ERROR {error['target']}: {error['reason']}")

sys.exit(2 if inspection_errors else (0 if ok else 1))
PY
