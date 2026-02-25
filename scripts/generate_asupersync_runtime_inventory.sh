#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SELF_TEST=0
if [[ "${1:-}" == "--self-test" ]]; then
  SELF_TEST=1
  shift
fi

OUT_PATH="${1:-${ROOT_DIR}/docs/asupersync-runtime-inventory.json}"
mkdir -p "$(dirname "${OUT_PATH}")"

log_json() {
  local level="$1"
  local event="$2"
  local message="$3"
  local now
  now="$(date -u +"%Y-%m-%dT%H:%M:%SZ")"
  printf '{"ts":"%s","level":"%s","event":"%s","message":"%s"}\n' \
    "${now}" "${level}" "${event}" "${message}" >&2
}

if ! command -v python3 >/dev/null 2>&1; then
  log_json "error" "missing_python3" "python3 is required to generate runtime inventory"
  exit 1
fi

TMP_PATH="$(mktemp)"
cleanup() {
  rm -f "${TMP_PATH}"
}
trap cleanup EXIT

log_json "info" "start" "Generating asupersync runtime inventory"
log_json "info" "context" "root=${ROOT_DIR} out=${OUT_PATH}"

FT_ASUPERSYNC_INVENTORY_SELF_TEST="${SELF_TEST}" \
python3 - "${ROOT_DIR}" "${TMP_PATH}" <<'PY'
from __future__ import annotations

import json
import re
import sys
from collections import defaultdict
from datetime import datetime, timezone
from pathlib import Path

try:
    import tomllib
except ModuleNotFoundError:
    import tomli as tomllib  # type: ignore

root = Path(sys.argv[1]).resolve()
out_path = Path(sys.argv[2]).resolve()
self_test = bool(int(__import__("os").environ.get("FT_ASUPERSYNC_INVENTORY_SELF_TEST", "0")))

patterns = {
    "asupersync": "asupersync::",
    "tokio": "tokio::",
    "runtime_compat": "runtime_compat::",
    "smol": "smol::",
    "async_std": "async_std::",
}

symbol_tokens = {
    "tokio_spawn": "tokio::spawn",
    "tokio_sleep": "tokio::time::sleep",
    "tokio_timeout": "tokio::time::timeout",
    "tokio_mpsc": "tokio::sync::mpsc",
    "runtime_compat_spawn": "runtime_compat::spawn",
    "runtime_compat_sleep": "runtime_compat::sleep",
    "runtime_compat_timeout": "runtime_compat::timeout",
    "smol_block_on": "smol::block_on",
    "smol_spawn": "smol::spawn",
    "asupersync_cx": "asupersync::Cx",
    "asupersync_executor": "asupersync::Executor",
}

manifest_symbols = {
    "tokio": "tokio",
    "asupersync": "asupersync",
    "smol": "smol",
    "async_std": "async-std",
}

runtime_lock_packages = {
    "asupersync",
    "tokio",
    "smol",
    "async-std",
    "async-io",
    "async-channel",
    "futures-lite",
    "polling",
}


def rel(path: Path) -> str:
    return path.resolve().relative_to(root).as_posix()


def crate_root(path: Path) -> str:
    parts = path.relative_to(root).parts
    if len(parts) >= 2 and parts[0] == "crates":
        return f"crates/{parts[1]}"
    if len(parts) >= 2 and parts[0] == "frankenterm":
        return f"frankenterm/{parts[1]}"
    return "."


def read_text(path: Path) -> str:
    return path.read_text(encoding="utf-8", errors="ignore")


def criticality_for_total(total: int) -> str:
    if total >= 120:
        return "high"
    if total >= 40:
        return "medium"
    return "low"


def difficulty_for_symbol_mix(symbol_mix: int) -> str:
    if symbol_mix >= 3:
        return "high"
    if symbol_mix == 2:
        return "medium"
    return "low"


def recommend_target(counts: dict[str, int], text: str) -> str:
    if "spawn" in text:
        return "cx::spawn_with_cx / scope-owned spawn"
    if "sleep" in text or "timeout" in text:
        return "runtime_compat::sleep/timeout"
    if "Mutex" in text or "RwLock" in text or "Semaphore" in text or "mpsc" in text:
        return "runtime_compat sync/channel adapters"
    if counts["smol"] > 0:
        return "asupersync-native adapter over smol surface"
    return "runtime_compat boundary adapter"


def infer_workflows(path: str, text: str) -> list[str]:
    workflows: list[str] = []
    path_l = path.lower()
    text_l = text.lower()

    if path_l.endswith("src/main.rs"):
        workflows.append("cli-command-execution")
    if "ipc" in path_l or "robot" in text_l or "mcp" in text_l:
        workflows.append("agent-orchestration-control-plane")
    if "search" in path_l:
        workflows.append("search-and-retrieval")
    if "storage" in path_l or "recorder" in path_l or "replay" in path_l:
        workflows.append("capture-persistence-and-replay")
    if "runtime_compat" in path_l or "tailer" in path_l or "pool" in path_l:
        workflows.append("runtime-abstraction-and-scheduling")
    if "/tests/" in f"/{path_l}" or path_l.startswith("tests/"):
        workflows.append("validation-and-test-infrastructure")

    if not workflows:
        workflows.append("internal-runtime-maintenance")
    return sorted(set(workflows))


def collect_symbol_occurrences(text: str) -> dict[str, int]:
    counts: dict[str, int] = {}
    for symbol_name, token in symbol_tokens.items():
        match_count = len(re.findall(re.escape(token), text))
        if match_count > 0:
            counts[symbol_name] = match_count
    return counts


def run_self_tests() -> None:
    assert criticality_for_total(10) == "low"
    assert criticality_for_total(40) == "medium"
    assert criticality_for_total(120) == "high"

    assert difficulty_for_symbol_mix(1) == "low"
    assert difficulty_for_symbol_mix(2) == "medium"
    assert difficulty_for_symbol_mix(3) == "high"

    base_counts = {k: 0 for k in patterns}
    assert recommend_target(base_counts, "tokio::spawn(async move {})") == "cx::spawn_with_cx / scope-owned spawn"
    assert recommend_target(base_counts, "tokio::time::sleep(Duration::from_millis(1))") == "runtime_compat::sleep/timeout"
    assert recommend_target(base_counts, "let _ = Mutex::new(1);") == "runtime_compat sync/channel adapters"

    smol_counts = dict(base_counts)
    smol_counts["smol"] = 1
    assert recommend_target(smol_counts, "smol::Task::spawn(async {})") == "cx::spawn_with_cx / scope-owned spawn"
    assert recommend_target(smol_counts, "smol::block_on(async {})") == "asupersync-native adapter over smol surface"
    assert infer_workflows("crates/frankenterm/src/main.rs", "tokio::spawn(async {})") == [
        "cli-command-execution"
    ]
    assert infer_workflows("crates/frankenterm-core/src/ipc.rs", "robot mcp control plane") == [
        "agent-orchestration-control-plane"
    ]

    occurrences = collect_symbol_occurrences(
        "tokio::spawn(async {}); tokio::spawn(async {}); runtime_compat::sleep(x);"
    )
    assert occurrences["tokio_spawn"] == 2
    assert occurrences["runtime_compat_sleep"] == 1


if self_test:
    run_self_tests()


rs_files = sorted((root / "crates").rglob("*.rs")) + sorted((root / "frankenterm").rglob("*.rs"))

manifest_files = [root / "Cargo.toml"]
manifest_files += sorted((root / "crates").rglob("Cargo.toml"))
manifest_files += sorted((root / "frankenterm").rglob("Cargo.toml"))
manifest_files = [p for p in manifest_files if p.exists()]

lock_file = root / "Cargo.lock"

pattern_reference_counts = {k: 0 for k in patterns}
pattern_file_counts = {k: 0 for k in patterns}
symbol_reference_counts = {k: 0 for k in symbol_tokens}
usage_by_crate = defaultdict(lambda: {k: 0 for k in patterns})
file_totals: list[tuple[str, int]] = []
classification_rows = []
symbol_occurrence_rows = []

for path in rs_files:
    text = read_text(path)
    counts = {k: text.count(v) for k, v in patterns.items()}
    total = sum(counts.values())
    if total == 0:
        continue
    for key, value in counts.items():
        pattern_reference_counts[key] += value
        if value > 0:
            pattern_file_counts[key] += 1
            usage_by_crate[crate_root(path)][key] += value
    rel_path = rel(path)
    owner = crate_root(path)
    file_totals.append((rel_path, total))

    symbol_mix = sum(1 for value in counts.values() if value > 0)
    criticality = criticality_for_total(total)
    difficulty = difficulty_for_symbol_mix(symbol_mix)
    recommended = recommend_target(counts, text)
    affected_workflows = infer_workflows(rel_path, text)
    symbol_occurrences = collect_symbol_occurrences(text)

    for symbol_name, value in symbol_occurrences.items():
        symbol_reference_counts[symbol_name] += value

    classification_rows.append(
        {
            "path": rel_path,
            "owner_module": owner,
            "criticality": criticality,
            "migration_difficulty": difficulty,
            "recommended_target_primitive": recommended,
            "reference_count": total,
            "affected_user_workflows": affected_workflows,
        }
    )
    if symbol_occurrences:
        symbol_occurrence_rows.append(
            {
                "path": rel_path,
                "owner_module": owner,
                "total_symbol_references": sum(symbol_occurrences.values()),
                "symbol_occurrences": symbol_occurrences,
            }
        )

usage_by_crate_root = []
for crate in sorted(usage_by_crate.keys()):
    row = {"crate_root": crate}
    row.update(usage_by_crate[crate])
    usage_by_crate_root.append(row)

top_runtime_reference_files = [
    {"path": path, "reference_count": count}
    for path, count in sorted(file_totals, key=lambda item: (-item[1], item[0]))[:25]
]

symbol_occurrence_top_files = sorted(
    symbol_occurrence_rows,
    key=lambda item: (-item["total_symbol_references"], item["path"]),
)[:25]

runtime_manifests = []
for path in manifest_files:
    text = read_text(path)
    counts = {k: text.count(v) for k, v in manifest_symbols.items()}
    if any(counts.values()):
        row = {"manifest": rel(path)}
        row.update(counts)
        runtime_manifests.append(row)

transitive_runtime_packages = []
if lock_file.exists():
    lock_data = tomllib.loads(read_text(lock_file))
    for pkg in lock_data.get("package", []):
        name = pkg.get("name")
        if name in runtime_lock_packages:
            transitive_runtime_packages.append(
                {
                    "name": name,
                    "version": pkg.get("version", ""),
                    "source": pkg.get("source", "workspace"),
                }
            )
    transitive_runtime_packages.sort(key=lambda item: item["name"])

observations = []
main_rs = next((item for item in top_runtime_reference_files if item["path"] == "crates/frankenterm/src/main.rs"), None)
if main_rs and main_rs["reference_count"] >= 100:
    observations.append(
        "crates/frankenterm/src/main.rs currently has high direct runtime reference density and should stay on a dedicated migration lane."
    )

runtime_compat_rs = next(
    (item for item in top_runtime_reference_files if item["path"] == "crates/frankenterm-core/src/runtime_compat.rs"),
    None,
)
if runtime_compat_rs:
    observations.append(
        "crates/frankenterm-core/src/runtime_compat.rs is the largest concentration point and should remain the primary boundary for runtime API normalization."
    )

if any(row.get("smol", 0) > 0 for row in usage_by_crate_root):
    observations.append(
        "Vendored frankenterm crates still expose smol-centric features, representing major harmonization risk for deep migration tracks."
    )

payload = {
    "inventory_version": 3,
    "generated_at": datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ"),
    "workspace": str(root),
    "inventory_scope": {
        "source_globs": [
            "crates/**/*.rs",
            "frankenterm/**/*.rs",
            "**/Cargo.toml",
            "Cargo.lock",
        ],
        "symbol_patterns": [patterns[key] for key in patterns],
        "symbol_token_probes": [symbol_tokens[key] for key in symbol_tokens],
        "caveats": [
            "Text-pattern inventory; does not prove runtime execution paths.",
            "Feature-gated code may be present in source counts even when inactive.",
            "Cargo manifest presence marks potential direct dependency exposure, not guaranteed linkage in every feature set.",
        ],
    },
    "pattern_reference_counts": pattern_reference_counts,
    "pattern_file_counts": pattern_file_counts,
    "symbol_reference_counts": symbol_reference_counts,
    "usage_by_crate_root": usage_by_crate_root,
    "top_runtime_reference_files": top_runtime_reference_files,
    "symbol_occurrence_top_files": symbol_occurrence_top_files,
    "migration_classification": sorted(
        classification_rows, key=lambda item: (-item["reference_count"], item["path"])
    )[:100],
    "runtime_manifests": runtime_manifests,
    "transitive_runtime_packages": transitive_runtime_packages,
    "observations": observations,
}

out_path.write_text(json.dumps(payload, indent=2) + "\n", encoding="utf-8")
PY

mv "${TMP_PATH}" "${OUT_PATH}"
log_json "info" "success" "Runtime inventory written to ${OUT_PATH}"
