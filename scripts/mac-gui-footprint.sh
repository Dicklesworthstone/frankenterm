#!/usr/bin/env bash
# Memory and GPU footprint bundles for a FrankenTerm GUI process on macOS
# (ft-yccm0.1.7, the M.6 footprint gate).
#
#   capture --pid PID [--out DIR] [--label NAME] [--runtime-dir DIR] [--no-unmapped]
#       Writes DIR/<label>-<pid>-<UTC stamp>/ with:
#         vmmap-summary.txt  raw `vmmap --summary PID`
#         footprint.txt      raw `footprint PID` text; adds --unmapped (the
#                            "owned unmapped" GPU memory search) only when run
#                            as root, because footprint refuses it otherwise.
#                            bundle.json records unmapped_searched either way.
#         footprint.json     raw `footprint -j` JSON
#         resources.json     the GUI's published resource ledger, when present
#                            (<runtime-dir>/frankenterm-resources-<pid>.json)
#         bundle.json        parsed, timestamped summary (schema below)
#         bundle.txt         the same summary for humans
#       The bundle directory is printed on stdout.
#
#   diff BUNDLE_A BUNDLE_B
#       Prints the growth of every footprint category, vmmap region, GPU
#       ledger counter, cache gauge and allocator statistic, largest first.
#
#   growth BUNDLE_DIR [--gpu-budget-mib N]
#       Slope test over the last half of the bundles in BUNDLE_DIR (ordered by
#       capture time). Fails (exit 1) when a tracked metric keeps growing: its
#       least-squares slope projects to more than max(64 MiB, 10% of its
#       median) over 24 h with R^2 >= 0.5, or when GPU-owned memory in any
#       bundle exceeds the budget. Prints one JSON verdict line.
#
#   m2 --after-t1 BUNDLE --soak BUNDLE_DIR [--rss-budget-mib N] [--gpu-budget-mib N]
#       Computes the M2 scoreboard row: resident size after the T1 drain
#       <= the RSS budget (default 1024 MiB), GPU-owned memory bounded by the
#       GPU budget, and no growth over the soak (the `growth` test).
#
#   --self-test
#       Captures and diffs two bundles of a throwaway `sleep` process and runs
#       the growth/m2 math on synthetic series. Needs no GUI.
#
# GPU-owned memory is the sum of dirty+swapped bytes of footprint categories
# whose name contains IOAccelerator, IOSurface, "owned unmapped" or GPU.
# The default GPU budget (1024 MiB) covers two 8192^2 RGBA atlases (512 MiB,
# the transient pair during a rebuild), four drawables of a 6K display at
# 8 bytes/texel (~470 MiB) and headroom; the 0.15.2 incident held ~4.2 GB.
#
# All tools here are read-only inspectors (vmmap, footprint, ps, sysctl).
# Point them only at processes you started; never at the operator's GUI.
set -euo pipefail

DEFAULT_GPU_BUDGET_MIB=1024
DEFAULT_RSS_BUDGET_MIB=1024

die() {
  echo "mac-gui-footprint: $*" >&2
  exit 2
}

usage() {
  sed -n '2,/^set -euo/p' "$0" | sed -e '/^set -euo/d' -e 's/^# \{0,1\}//'
}

python_tool() {
  # Shared parsing/analysis. Args: subcommand, then subcommand args.
  python3 -I - "$@" <<'PY'
import json, math, os, re, statistics, sys, time

MIB = 1024 * 1024
GPU_PATTERNS = ("ioaccelerator", "iosurface", "owned unmapped", "gpu")
SIZE_RE = re.compile(r"^([0-9.]+)([KMGT]?)$")
UNIT = {"": 1, "K": 1024, "M": 1024**2, "G": 1024**3, "T": 1024**4}


def parse_size(token):
    match = SIZE_RE.match(token)
    if not match:
        raise ValueError(f"not a vmmap size: {token!r}")
    return int(round(float(match.group(1)) * UNIT[match.group(2)]))


def parse_vmmap(text):
    """REGION TYPE table of `vmmap --summary`: name -> sizes in bytes."""
    regions, in_table, totals = {}, False, None
    row = re.compile(
        r"^(?P<name>\S.*?)\s+(?P<virtual>[0-9.]+[KMGT]?)\s+(?P<resident>[0-9.]+[KMGT]?)"
        r"\s+(?P<dirty>[0-9.]+[KMGT]?)\s+(?P<swapped>[0-9.]+[KMGT]?)\s+(?P<volatile>[0-9.]+[KMGT]?)"
        r"\s+(?P<nonvol>[0-9.]+[KMGT]?)\s+(?P<empty>[0-9.]+[KMGT]?)\s+(?P<count>\d+)\b"
    )
    footprint = peak = None
    for line in text.splitlines():
        if line.startswith("Physical footprint:"):
            footprint = parse_size(line.split()[-1])
        elif line.startswith("Physical footprint (peak):"):
            peak = parse_size(line.split()[-1])
        if line.startswith("REGION TYPE"):
            in_table = True
            continue
        if in_table and line.startswith("MALLOC ZONE"):
            break
        if not in_table or line.startswith("==="):
            continue
        match = row.match(line)
        if not match:
            continue
        sizes = {key: parse_size(match.group(key)) for key in
                 ("virtual", "resident", "dirty", "swapped")}
        sizes["count"] = int(match.group("count"))
        name = match.group("name").strip()
        if name == "TOTAL":
            totals = sizes
        else:
            regions[name] = sizes
    if totals is None:
        raise ValueError("vmmap summary has no TOTAL row")
    return {"physical_footprint": footprint, "physical_footprint_peak": peak,
            "regions": regions, "total": totals}


def parse_footprint(raw, pid):
    for process in raw.get("processes", []):
        if process.get("pid") == pid:
            categories = {}
            for name, values in process.get("categories", {}).items():
                categories[name] = {key: int(values.get(key, 0)) for key in
                                    ("dirty", "swapped", "clean", "reclaimable", "wired", "regions")}
            return {"footprint": int(process.get("footprint", 0)), "categories": categories,
                    "auxiliary": process.get("auxiliary", {})}
    raise ValueError(f"footprint JSON has no process {pid}")


def gpu_owned_bytes(footprint):
    total = 0
    for name, values in footprint["categories"].items():
        if any(pattern in name.lower() for pattern in GPU_PATTERNS):
            total += values["dirty"] + values["swapped"]
    return total


def build_bundle(args):
    (bundle_dir, pid, label, rss_kib, loadavg, comm, resources_path, unmapped) = args
    pid = int(pid)
    with open(os.path.join(bundle_dir, "vmmap-summary.txt")) as handle:
        vmmap = parse_vmmap(handle.read())
    with open(os.path.join(bundle_dir, "footprint.json")) as handle:
        footprint = parse_footprint(json.load(handle), pid)
    resources = None
    if resources_path and os.path.exists(resources_path):
        with open(resources_path) as handle:
            resources = json.load(handle)
        if resources.get("pid") != pid:
            resources = None
    bundle = {
        "schema": "frankenterm.footprint_bundle.v1",
        "pid": pid,
        "label": label,
        "process": comm,
        "captured_unix_ms": int(time.time() * 1000),
        "host_load_average": [float(x) for x in loadavg.strip("{} \n").split()],
        "rss_bytes": int(rss_kib) * 1024,
        "unmapped_searched": unmapped == "1",
        "phys_footprint": footprint["footprint"],
        "gpu_owned_bytes": gpu_owned_bytes(footprint),
        "footprint": footprint,
        "vmmap": vmmap,
        "resources": resources,
    }
    with open(os.path.join(bundle_dir, "bundle.json"), "w") as handle:
        json.dump(bundle, handle, indent=2, sort_keys=True)
    with open(os.path.join(bundle_dir, "bundle.txt"), "w") as handle:
        handle.write(render_text(bundle))
    print(render_text(bundle), file=sys.stderr, end="")


def render_text(bundle):
    lines = [
        f"{bundle['label']} pid {bundle['pid']} ({bundle['process']}) at "
        f"{time.strftime('%Y-%m-%dT%H:%M:%SZ', time.gmtime(bundle['captured_unix_ms'] / 1000))}",
        f"  load average {bundle['host_load_average']}",
        f"  rss {bundle['rss_bytes'] / MIB:.1f} MiB, phys_footprint {bundle['phys_footprint'] / MIB:.1f} MiB,"
        f" GPU-owned {bundle['gpu_owned_bytes'] / MIB:.1f} MiB"
        f"{'' if bundle['unmapped_searched'] else ' (owned-unmapped not searched)'}",
    ]
    top = sorted(bundle["footprint"]["categories"].items(),
                 key=lambda item: item[1]["dirty"] + item[1]["swapped"], reverse=True)[:8]
    for name, values in top:
        lines.append(f"    {name}: dirty {values['dirty'] / MIB:.1f} MiB, swapped {values['swapped'] / MIB:.1f} MiB")
    resources = bundle.get("resources")
    if resources:
        gpu = resources["gpu"]
        lines.append(f"  ledger: textures {gpu['texture_total']['live_count']} / "
                     f"{gpu['texture_total']['live_bytes'] / MIB:.1f} MiB, buffers "
                     f"{gpu['buffer_total']['live_count']} / {gpu['buffer_total']['live_bytes'] / MIB:.1f} MiB, "
                     f"atlas generations {gpu['atlas_generations']}")
    else:
        lines.append("  ledger: no published resource snapshot for this pid")
    return "\n".join(lines) + "\n"


def flat_metrics(bundle):
    """Every comparable number in a bundle, keyed by a stable name."""
    out = {"rss_bytes": bundle["rss_bytes"], "phys_footprint": bundle["phys_footprint"],
           "gpu_owned_bytes": bundle["gpu_owned_bytes"]}
    for name, values in bundle["footprint"]["categories"].items():
        out[f"footprint/{name}"] = values["dirty"] + values["swapped"]
    for name, values in bundle["vmmap"]["regions"].items():
        out[f"vmmap/{name}"] = values["dirty"] + values["swapped"]
    resources = bundle.get("resources")
    if resources:
        gpu = resources["gpu"]
        for purpose, counter in gpu["textures"].items():
            out[f"ledger/texture/{purpose}"] = counter["live_bytes"]
        for purpose, counter in gpu["buffers"].items():
            out[f"ledger/buffer/{purpose}"] = counter["live_bytes"]
        for name, value in resources.get("caches", {}).items():
            out[f"cache/{name}"] = value
        stats = (resources.get("allocator") or {}).get("stats")
        if stats:
            for name, value in stats.items():
                out[f"allocator/{name}"] = value
        panes = resources.get("panes", [])
        out["panes/warm_resident_bytes"] = sum(p["warm_resident_bytes"] for p in panes)
        out["panes/hot_rows"] = sum(p["hot_rows"] for p in panes)
    return out


def load_bundle(path):
    if os.path.isdir(path):
        path = os.path.join(path, "bundle.json")
    with open(path) as handle:
        return json.load(handle)


def diff(a_path, b_path):
    a, b = load_bundle(a_path), load_bundle(b_path)
    ma, mb = flat_metrics(a), flat_metrics(b)
    rows = []
    for key in sorted(set(ma) | set(mb)):
        before, after = ma.get(key, 0), mb.get(key, 0)
        if before or after:
            rows.append((after - before, key, before, after))
    rows.sort(key=lambda row: abs(row[0]), reverse=True)
    elapsed = (b["captured_unix_ms"] - a["captured_unix_ms"]) / 1000
    print(f"growth from pid {a['pid']} {a['label']} to pid {b['pid']} {b['label']} over {elapsed:.0f} s")
    print(f"{'delta':>14} {'before':>14} {'after':>14}  metric")
    for delta, key, before, after in rows:
        scale = 1 if key.startswith(("cache/", "panes/hot_rows")) and not key.endswith("bytes") else MIB
        unit = "" if scale == 1 else " MiB"
        print(f"{delta / scale:>+14.2f} {before / scale:>14.2f} {after / scale:>14.2f}  {key}{unit}")


def regression(points):
    n = len(points)
    mean_x = sum(x for x, _ in points) / n
    mean_y = sum(y for _, y in points) / n
    sxx = sum((x - mean_x) ** 2 for x, _ in points)
    if sxx == 0:
        return 0.0, 0.0
    sxy = sum((x - mean_x) * (y - mean_y) for x, y in points)
    slope = sxy / sxx
    syy = sum((y - mean_y) ** 2 for _, y in points)
    r2 = (sxy * sxy) / (sxx * syy) if syy else 0.0
    return slope, r2


GROWTH_METRICS = ("rss_bytes", "phys_footprint", "gpu_owned_bytes")
GROWTH_PREFIXES = ("ledger/", "cache/", "allocator/", "panes/warm_resident_bytes")


def growth_verdict(bundles, gpu_budget_bytes):
    bundles = sorted(bundles, key=lambda bundle: bundle["captured_unix_ms"])
    failures, metrics = [], {}
    if len(bundles) < 4:
        return {"pass": False, "failures": [f"need >= 4 bundles, have {len(bundles)}"], "metrics": {}}
    for bundle in bundles:
        if bundle["gpu_owned_bytes"] > gpu_budget_bytes:
            failures.append(f"GPU-owned {bundle['gpu_owned_bytes'] / MIB:.1f} MiB exceeds the "
                            f"{gpu_budget_bytes / MIB:.0f} MiB budget at {bundle['captured_unix_ms']}")
    tail = bundles[len(bundles) // 2:]
    t0 = tail[0]["captured_unix_ms"]
    series = {}
    for bundle in tail:
        hours = (bundle["captured_unix_ms"] - t0) / 3_600_000
        for key, value in flat_metrics(bundle).items():
            if key in GROWTH_METRICS or key.startswith(GROWTH_PREFIXES):
                if key.startswith("cache/") and not key.endswith("bytes"):
                    continue
                series.setdefault(key, []).append((hours, value))
    for key, points in sorted(series.items()):
        if len(points) < 3:
            continue
        slope, r2 = regression(points)
        median = statistics.median(y for _, y in points)
        projected = slope * 24
        tolerance = max(64 * MIB, 0.10 * median)
        grows = projected > tolerance and r2 >= 0.5
        metrics[key] = {"slope_bytes_per_hour": round(slope), "r2": round(r2, 3),
                        "projected_24h_bytes": round(projected), "tolerance_bytes": round(tolerance),
                        "grows": grows}
        if grows:
            failures.append(f"{key} grows {projected / MIB:.1f} MiB/24h (R^2 {r2:.2f}) > "
                            f"tolerance {tolerance / MIB:.1f} MiB")
    span_h = (bundles[-1]["captured_unix_ms"] - bundles[0]["captured_unix_ms"]) / 3_600_000
    return {"pass": not failures, "failures": failures, "bundles": len(bundles),
            "span_hours": round(span_h, 4), "tail_bundles": len(tail), "metrics": metrics}


def load_dir(directory):
    bundles = []
    for entry in sorted(os.listdir(directory)):
        candidate = os.path.join(directory, entry, "bundle.json")
        if os.path.exists(candidate):
            with open(candidate) as handle:
                bundles.append(json.load(handle))
    return bundles


def growth(directory, gpu_budget_mib):
    verdict = growth_verdict(load_dir(directory), gpu_budget_mib * MIB)
    print(json.dumps(verdict, sort_keys=True))
    return 0 if verdict["pass"] else 1


def m2(after_t1, soak_dir, rss_budget_mib, gpu_budget_mib):
    bundle = load_bundle(after_t1)
    soak = growth_verdict(load_dir(soak_dir), gpu_budget_mib * MIB)
    row = {
        "row": "M2",
        "rss_after_t1_bytes": bundle["rss_bytes"],
        "rss_budget_bytes": rss_budget_mib * MIB,
        "rss_ok": bundle["rss_bytes"] <= rss_budget_mib * MIB,
        "gpu_owned_after_t1_bytes": bundle["gpu_owned_bytes"],
        "gpu_budget_bytes": gpu_budget_mib * MIB,
        "gpu_ok": bundle["gpu_owned_bytes"] <= gpu_budget_mib * MIB and not any(
            failure.startswith("GPU-owned") for failure in soak["failures"]),
        "no_growth_ok": soak["pass"],
        "soak_span_hours": soak.get("span_hours"),
        "soak_failures": soak["failures"],
        "host_load_average_after_t1": bundle["host_load_average"],
    }
    row["pass"] = row["rss_ok"] and row["gpu_ok"] and row["no_growth_ok"]
    print(json.dumps(row, sort_keys=True))
    return 0 if row["pass"] else 1


def selftest_math():
    hour = 3_600_000
    def synthetic(slope_mib_per_hour, noise=0):
        series = []
        for i in range(12):
            value = 500 * MIB + int(slope_mib_per_hour * MIB * i) + (noise if i % 2 else -noise)
            series.append({"captured_unix_ms": i * hour, "rss_bytes": value, "phys_footprint": value,
                           "gpu_owned_bytes": 100 * MIB, "footprint": {"categories": {}},
                           "vmmap": {"regions": {}}, "resources": None})
        return series
    flat = growth_verdict(synthetic(0, noise=8 * MIB), 1024 * MIB)
    assert flat["pass"], flat
    leak = growth_verdict(synthetic(16), 1024 * MIB)
    assert not leak["pass"] and any("rss_bytes" in f for f in leak["failures"]), leak
    over = synthetic(0)
    over[3]["gpu_owned_bytes"] = 2048 * MIB
    assert not growth_verdict(over, 1024 * MIB)["pass"]
    assert not growth_verdict(synthetic(0)[:3], 1024 * MIB)["pass"], "too few bundles must fail"
    sample = ("REGION TYPE                        SIZE     SIZE     SIZE     SIZE     SIZE     SIZE     SIZE    COUNT (non-coalesced)\n"
              "===========                     ======= ========    =====  ======= ========   ======    =====  =======\n"
              "IOAccelerator                     1.5G     800M     800M       0K       0K       0K       0K       12\n"
              "MALLOC_SMALL                      4096K      32K      32K      16K       0K       0K       0K        1         see MALLOC ZONE table below\n"
              "dyld private memory                160K       24       24       0K       0K       0K       0K        4\n"
              "TOTAL                            817.9M    65.0M     896K       0K       0K       0K       0K      281\n")
    parsed = parse_vmmap("Physical footprint:         880K\n" + sample)
    assert parsed["physical_footprint"] == 880 * 1024
    assert parsed["regions"]["IOAccelerator"]["dirty"] == 800 * MIB
    assert parsed["regions"]["MALLOC_SMALL"]["swapped"] == 16 * 1024
    assert parsed["regions"]["dyld private memory"]["resident"] == 24
    assert parsed["total"]["count"] == 281
    print("math self-test ok")


def main(argv):
    command, args = argv[0], argv[1:]
    if command == "build":
        build_bundle(args)
        return 0
    if command == "diff":
        diff(*args)
        return 0
    if command == "growth":
        return growth(args[0], int(args[1]))
    if command == "m2":
        return m2(args[0], args[1], int(args[2]), int(args[3]))
    if command == "selftest-math":
        selftest_math()
        return 0
    raise SystemExit(f"unknown python_tool command {command}")


sys.exit(main(sys.argv[1:]))
PY
}

cmd_capture() {
  local pid="" out="${PWD}/footprint-bundles" label="gui" runtime_dir="${HOME}/.local/share/frankenterm"
  local unmapped=0
  if [[ "$(id -u)" == 0 ]]; then
    unmapped=1
  fi
  while [[ $# -gt 0 ]]; do
    case "$1" in
      --pid) pid="$2"; shift 2 ;;
      --out) out="$2"; shift 2 ;;
      --label) label="$2"; shift 2 ;;
      --runtime-dir) runtime_dir="$2"; shift 2 ;;
      --no-unmapped) unmapped=0; shift ;;
      *) die "capture: unknown argument $1" ;;
    esac
  done
  [[ "$pid" =~ ^[0-9]+$ ]] || die "capture: --pid must be a process id"
  local owner
  owner="$(ps -o user= -p "$pid" 2>/dev/null | tr -d ' ')" || true
  [[ -n "$owner" ]] || die "capture: no process $pid"
  [[ "$owner" == "$(id -un)" ]] || die "capture: process $pid belongs to $owner"

  local stamp dir
  stamp="$(date -u +%Y%m%dT%H%M%SZ)"
  dir="${out}/${label}-${pid}-${stamp}"
  mkdir -p "$dir"
  vmmap --summary "$pid" >"$dir/vmmap-summary.txt" 2>&1 || die "capture: vmmap failed (see $dir/vmmap-summary.txt)"
  local footprint_args=(-j "$dir/footprint.json" --swapped --wired)
  if [[ "$unmapped" == 1 ]]; then
    footprint_args+=(--unmapped)
  fi
  footprint "${footprint_args[@]}" "$pid" >"$dir/footprint.txt" 2>&1 \
    || die "capture: footprint failed (see $dir/footprint.txt)"
  local resources="${runtime_dir}/frankenterm-resources-${pid}.json"
  if [[ -f "$resources" ]]; then
    cp "$resources" "$dir/resources.json"
  fi
  local rss comm loadavg
  rss="$(ps -o rss= -p "$pid" | tr -d ' ')"
  comm="$(ps -o comm= -p "$pid")"
  loadavg="$(sysctl -n vm.loadavg)"
  python_tool build "$dir" "$pid" "$label" "$rss" "$loadavg" "$comm" "$dir/resources.json" "$unmapped"
  echo "$dir"
}

cmd_self_test() {
  local work
  work="$(mktemp -d "${TMPDIR:-/tmp}/ft-footprint-selftest.XXXXXX")"
  python_tool selftest-math
  sleep 60 &
  local victim=$!
  local first second
  first="$(cmd_capture --pid "$victim" --out "$work" --label selftest-a --no-unmapped 2>/dev/null)"
  second="$(cmd_capture --pid "$victim" --out "$work" --label selftest-b --no-unmapped 2>/dev/null)"
  kill "$victim" 2>/dev/null || true
  wait "$victim" 2>/dev/null || true
  for bundle in "$first" "$second"; do
    for file in bundle.json bundle.txt vmmap-summary.txt footprint.json footprint.txt; do
      [[ -s "$bundle/$file" ]] || die "self-test: $bundle/$file missing"
    done
  done
  python_tool diff "$first" "$second" | head -5
  echo "mac-gui-footprint self-test ok ($work)"
}

main() {
  [[ $# -gt 0 ]] || { usage; exit 2; }
  local cmd="$1"
  shift
  case "$cmd" in
    capture) cmd_capture "$@" ;;
    diff)
      [[ $# -eq 2 ]] || die "diff needs two bundles"
      python_tool diff "$1" "$2"
      ;;
    growth)
      [[ $# -ge 1 ]] || die "growth needs a bundle directory"
      local dir="$1" budget="$DEFAULT_GPU_BUDGET_MIB"
      shift
      if [[ "${1:-}" == "--gpu-budget-mib" ]]; then budget="$2"; fi
      python_tool growth "$dir" "$budget"
      ;;
    m2)
      local after="" soak="" rss="$DEFAULT_RSS_BUDGET_MIB" gpu="$DEFAULT_GPU_BUDGET_MIB"
      while [[ $# -gt 0 ]]; do
        case "$1" in
          --after-t1) after="$2"; shift 2 ;;
          --soak) soak="$2"; shift 2 ;;
          --rss-budget-mib) rss="$2"; shift 2 ;;
          --gpu-budget-mib) gpu="$2"; shift 2 ;;
          *) die "m2: unknown argument $1" ;;
        esac
      done
      [[ -n "$after" && -n "$soak" ]] || die "m2 needs --after-t1 and --soak"
      python_tool m2 "$after" "$soak" "$rss" "$gpu"
      ;;
    --self-test) cmd_self_test ;;
    -h|--help) usage ;;
    *) die "unknown command $cmd" ;;
  esac
}

main "$@"
