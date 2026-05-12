#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'EOF'
Usage: scripts/wezterm-render-differential.sh [options]

Gate a FrankenTerm-vs-upstream-WezTerm render comparison report against the
deliberate-divergence allowlist. This script does not synthesize upstream
frames; it consumes a frame-comparison report from the render adapter and fails
closed when any divergent frame is not allowlisted.

Options:
  --comparison-report <path>  JSON report with a comparisons[] array.
  --allowlist <path>          Markdown allowlist (default: docs/wezterm-divergence-allowlist.md).
  --output <path>             Output attestation JSON path.
  --run-id <id>               Stable run identifier.
  --upstream-ref <ref>        Upstream WezTerm ref or commit used by the adapter.
  --self-test                 Run the parser/gate self-test without Cargo or GPU work.
  -h, --help                  Show this help.

Comparison report contract:
  {
    "schema_version": "wezterm-render-comparison.v1",
    "comparisons": [
      {
        "input_id": "tc-resize-wrap-001",
        "frame_id": "frame-000",
        "status": "pass" | "diverged",
        "metrics": {
          "ssim": 1.0,
          "l_inf": 0,
          "changed_pixel_fraction": 0.0
        },
        "frankenterm_png": "target/.../frame.png",
        "wezterm_png": "target/.../frame.png"
      }
    ]
  }
EOF
}

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
RUN_ID="$(date -u +%Y%m%dT%H%M%SZ)-$$"
ALLOWLIST="$PROJECT_ROOT/docs/wezterm-divergence-allowlist.md"
OUTPUT="$PROJECT_ROOT/target/wezterm-differential/$RUN_ID/wezterm-divergence.json"
COMPARISON_REPORT=""
UPSTREAM_REF="${FT_WEZTERM_DIFF_UPSTREAM_REF:-unknown}"
SELF_TEST=0

while [[ $# -gt 0 ]]; do
  case "$1" in
    --comparison-report)
      COMPARISON_REPORT="${2:?--comparison-report requires a path}"
      shift 2
      ;;
    --allowlist)
      ALLOWLIST="${2:?--allowlist requires a path}"
      shift 2
      ;;
    --output)
      OUTPUT="${2:?--output requires a path}"
      shift 2
      ;;
    --run-id)
      RUN_ID="${2:?--run-id requires a value}"
      shift 2
      ;;
    --upstream-ref)
      UPSTREAM_REF="${2:?--upstream-ref requires a ref}"
      shift 2
      ;;
    --self-test)
      SELF_TEST=1
      shift
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      echo "[wezterm-diff] unknown argument: $1" >&2
      usage >&2
      exit 64
      ;;
  esac
done

run_gate() {
  local comparison_report="$1"
  local allowlist="$2"
  local output="$3"
  local run_id="$4"
  local upstream_ref="$5"

  python3 - "$comparison_report" "$allowlist" "$output" "$run_id" "$upstream_ref" <<'PY'
import fnmatch
import json
import re
import sys
from pathlib import Path

comparison_path = Path(sys.argv[1])
allowlist_path = Path(sys.argv[2])
output_path = Path(sys.argv[3])
run_id = sys.argv[4]
upstream_ref = sys.argv[5]


def load_json(path: Path):
    try:
        return json.loads(path.read_text(encoding="utf-8"))
    except FileNotFoundError:
        raise SystemExit(f"[wezterm-diff] missing file: {path}") from None
    except json.JSONDecodeError as exc:
        raise SystemExit(f"[wezterm-diff] invalid JSON in {path}: {exc}") from None


def load_allowlist(path: Path):
    text = path.read_text(encoding="utf-8")
    marker = "wezterm-divergence-allowlist:json"
    if marker not in text:
        raise SystemExit(f"[wezterm-diff] allowlist marker not found in {path}")
    match = re.search(
        r"<!--\s*wezterm-divergence-allowlist:json\s*-->\s*```json\s*(.*?)\s*```",
        text,
        re.DOTALL,
    )
    if not match:
        raise SystemExit(f"[wezterm-diff] allowlist JSON block not found in {path}")
    try:
        data = json.loads(match.group(1))
    except json.JSONDecodeError as exc:
        raise SystemExit(f"[wezterm-diff] invalid allowlist JSON in {path}: {exc}") from None
    entries = data.get("entries")
    if not isinstance(entries, list):
        raise SystemExit(f"[wezterm-diff] allowlist entries must be an array in {path}")
    for index, entry in enumerate(entries):
        for field in ("input_pattern", "frame_pattern", "rationale", "bead_id"):
            if not entry.get(field):
                raise SystemExit(
                    f"[wezterm-diff] allowlist entry {index} missing required field {field}"
                )
    return data


def comparison_status(row):
    status = row.get("status")
    if status in {"pass", "diverged"}:
        return status
    metrics = row.get("metrics") or {}
    ssim = metrics.get("ssim")
    l_inf = metrics.get("l_inf")
    changed = metrics.get("changed_pixel_fraction")
    if ssim is None or l_inf is None or changed is None:
        return "diverged"
    thresholds = row.get("thresholds") or {}
    min_ssim = thresholds.get("min_ssim", 0.99)
    max_l_inf = thresholds.get("max_l_inf", 8)
    max_changed = thresholds.get("max_changed_pixel_fraction", 0.001)
    if ssim >= min_ssim and l_inf <= max_l_inf and changed <= max_changed:
        return "pass"
    return "diverged"


def matches(entry, row):
    input_id = row.get("input_id", "")
    frame_id = row.get("frame_id", "")
    if not fnmatch.fnmatchcase(input_id, entry["input_pattern"]):
        return False
    if not fnmatch.fnmatchcase(frame_id, entry["frame_pattern"]):
        return False
    max_changed = entry.get("max_changed_pixel_fraction")
    max_l_inf = entry.get("max_l_inf")
    min_ssim = entry.get("min_ssim")
    metrics = row.get("metrics") or {}
    if max_changed is not None and metrics.get("changed_pixel_fraction", 1.0) > max_changed:
        return False
    if max_l_inf is not None and metrics.get("l_inf", 10**9) > max_l_inf:
        return False
    if min_ssim is not None and metrics.get("ssim", 0.0) < min_ssim:
        return False
    return True


comparison = load_json(comparison_path)
if comparison.get("schema_version") != "wezterm-render-comparison.v1":
    raise SystemExit(
        "[wezterm-diff] comparison report schema_version must be "
        "wezterm-render-comparison.v1"
    )
comparisons = comparison.get("comparisons")
if not isinstance(comparisons, list):
    raise SystemExit("[wezterm-diff] comparison report comparisons must be an array")

allowlist = load_allowlist(allowlist_path)
entries = allowlist.get("entries", [])

divergences = []
allowlisted = []
novel = []
for row in comparisons:
    if comparison_status(row) != "diverged":
        continue
    normalized = {
        "input_id": row.get("input_id"),
        "frame_id": row.get("frame_id"),
        "metrics": row.get("metrics", {}),
        "thresholds": row.get("thresholds", {}),
        "frankenterm_png": row.get("frankenterm_png"),
        "wezterm_png": row.get("wezterm_png"),
        "reason": row.get("reason", "metric_threshold_exceeded"),
    }
    divergence_match = next((entry for entry in entries if matches(entry, row)), None)
    divergences.append(normalized)
    if divergence_match:
        normalized["allowlist_bead_id"] = divergence_match["bead_id"]
        normalized["allowlist_rationale"] = divergence_match["rationale"]
        allowlisted.append(normalized)
    else:
        novel.append(normalized)

artifact = {
    "schema_version": "wezterm-divergence.v1",
    "category": "tui/render-parity",
    "kind": "frankenterm-vs-wezterm-render-differential",
    "produced_by_bead": "ft-tf6g3.21",
    "run_id": run_id,
    "upstream_wezterm_ref": upstream_ref,
    "comparison_report": str(comparison_path),
    "allowlist": {
        "path": str(allowlist_path),
        "schema_version": allowlist.get("schema_version"),
        "entry_count": len(entries),
    },
    "counts": {
        "frames_compared_total": len(comparisons),
        "divergence_count": len(divergences),
        "allowlisted_count": len(allowlisted),
        "novel_divergence_count": len(novel),
    },
    "pass_condition": "novel_divergence_count == 0",
    "divergences": divergences,
    "allowlisted_divergences": allowlisted,
    "novel_divergences": novel,
    "ci_contract": {
        "workflow": ".github/workflows/wezterm-render-differential.yml",
        "fixed_corpus_seed": "tests/fixtures/terminal-conformance/manifest.json",
        "ssim_infra": "crates/frankenterm-gui/src/gpu_regression.rs::compare_images",
        "new_divergence_policy": "fail",
    },
}

output_path.parent.mkdir(parents=True, exist_ok=True)
output_path.write_text(json.dumps(artifact, indent=2, sort_keys=True) + "\n", encoding="utf-8")

print(
    "[wezterm-diff] frames={frames} divergences={divergences} "
    "allowlisted={allowlisted} novel={novel} output={output}".format(
        frames=len(comparisons),
        divergences=len(divergences),
        allowlisted=len(allowlisted),
        novel=len(novel),
        output=output_path,
    )
)

if novel:
    for row in novel:
        print(
            "[wezterm-diff] novel divergence: {input_id} {frame_id}".format(
                input_id=row.get("input_id"),
                frame_id=row.get("frame_id"),
            ),
            file=sys.stderr,
        )
    raise SystemExit(1)
PY
}

if [[ "$SELF_TEST" -eq 1 ]]; then
  tmp_dir="${TMPDIR:-/tmp}/wezterm-render-differential-self-test-$RUN_ID"
  mkdir -p "$tmp_dir"
  python3 - "$tmp_dir" <<'PY'
import json
import sys
from pathlib import Path

tmp = Path(sys.argv[1])
(tmp / "comparison.json").write_text(
    json.dumps(
        {
            "schema_version": "wezterm-render-comparison.v1",
            "comparisons": [
                {
                    "input_id": "tc-resize-wrap-001",
                    "frame_id": "frame-000",
                    "status": "pass",
                    "metrics": {"ssim": 1.0, "l_inf": 0, "changed_pixel_fraction": 0.0},
                },
                {
                    "input_id": "allowlisted-cursor-shape",
                    "frame_id": "frame-000",
                    "status": "diverged",
                    "metrics": {"ssim": 0.995, "l_inf": 2, "changed_pixel_fraction": 0.0002},
                },
            ],
        },
        indent=2,
    )
    + "\n",
    encoding="utf-8",
)
(tmp / "allowlist.md").write_text(
    """# Self-test allowlist

<!-- wezterm-divergence-allowlist:json -->
```json
{
  "schema_version": "wezterm-divergence-allowlist.v1",
  "entries": [
    {
      "input_pattern": "allowlisted-*",
      "frame_pattern": "frame-*",
      "rationale": "Self-test synthetic divergence.",
      "bead_id": "ft-tf6g3.21",
      "min_ssim": 0.99,
      "max_l_inf": 8,
      "max_changed_pixel_fraction": 0.001
    }
  ]
}
```
""",
    encoding="utf-8",
)
PY
  run_gate "$tmp_dir/comparison.json" "$tmp_dir/allowlist.md" "$tmp_dir/output.json" "$RUN_ID-self-test" "self-test"
  echo "[wezterm-diff] self-test PASS"
  exit 0
fi

if [[ -z "$COMPARISON_REPORT" ]]; then
  echo "[wezterm-diff] --comparison-report is required unless --self-test is used" >&2
  exit 64
fi

run_gate "$COMPARISON_REPORT" "$ALLOWLIST" "$OUTPUT" "$RUN_ID" "$UPSTREAM_REF"
