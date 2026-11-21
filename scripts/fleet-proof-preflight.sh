#!/usr/bin/env bash
# scripts/fleet-proof-preflight.sh — strictly read-only DSR + RCH fleet
# readiness preflight (ft-mgy5t2.11 / RC4.G100).
#
# Synthesizes `rch doctor`, `rch status`, and `dsr health all` per target
# platform (darwin/arm64, linux/amd64, linux/arm64, windows/amd64, and
# target-class:trj). Strictly read-only: never mutates remote hosts, never
# restarts Agent Mail (`am`), never invokes GitHub Actions (Rule 0.1), and
# never authorizes local Cargo fallback.
#
# Usage:
#   scripts/fleet-proof-preflight.sh [--json] [--write-artifact PATH]
#     [--min-rch-success-pct N] [--min-disk-gb N]
#     [--rch-doctor-input PATH] [--rch-status-input PATH] [--dsr-health-input PATH]
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd -P)"
JSON_OUTPUT=0
WRITE_ARTIFACT=""
MIN_RCH_SUCCESS_PCT=85
MIN_DISK_GB=10
RCH_DOCTOR_INPUT=""
RCH_STATUS_INPUT=""
DSR_HEALTH_INPUT=""

while [[ $# -gt 0 ]]; do
  case "$1" in
    --json) JSON_OUTPUT=1; shift ;;
    --write-artifact) WRITE_ARTIFACT="$2"; shift 2 ;;
    --min-rch-success-pct) MIN_RCH_SUCCESS_PCT="$2"; shift 2 ;;
    --min-disk-gb) MIN_DISK_GB="$2"; shift 2 ;;
    --rch-doctor-input) RCH_DOCTOR_INPUT="$2"; shift 2 ;;
    --rch-status-input) RCH_STATUS_INPUT="$2"; shift 2 ;;
    --dsr-health-input) DSR_HEALTH_INPUT="$2"; shift 2 ;;
    -h|--help) sed -n '2,15p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) echo "unknown option: $1" >&2; exit 2 ;;
  esac
done

python3 - "$ROOT" "$JSON_OUTPUT" "$WRITE_ARTIFACT" "$MIN_RCH_SUCCESS_PCT" "$MIN_DISK_GB" \
  "$RCH_DOCTOR_INPUT" "$RCH_STATUS_INPUT" "$DSR_HEALTH_INPUT" <<'PY'
import datetime
import json
import re
import subprocess
import sys
from pathlib import Path

root = Path(sys.argv[1])
json_output = sys.argv[2] == "1"
write_artifact = sys.argv[3]
min_rch_pct = int(sys.argv[4])
min_disk_gb = float(sys.argv[5])
rch_doctor_in = sys.argv[6]
rch_status_in = sys.argv[7]
dsr_health_in = sys.argv[8]


def read_or_run(input_path: str, cmd: list[str], stdout_only: bool = False) -> tuple[str, int]:
    if input_path:
        p = Path(input_path)
        if not p.is_file():
            sys.stderr.write(f"error: input fixture not found: {input_path}\n")
            sys.exit(2)
        return p.read_text(encoding="utf-8", errors="replace"), 0
    proc = subprocess.run(cmd, capture_output=True, text=True)
    text = proc.stdout if stdout_only else (proc.stdout + "\n" + proc.stderr)
    return text, proc.returncode


rch_doctor_txt, _ = read_or_run(rch_doctor_in, ["rch", "doctor"])
rch_status_txt, _ = read_or_run(rch_status_in, ["rch", "status", "--workers"])
dsr_health_txt, _ = read_or_run(dsr_health_in, ["dsr", "health", "all", "--json"], stdout_only=True)

# Parse DSR host health (supports JSON output with optional non-JSON prefix lines or text)
dsr_hosts = []
dsr_json_candidate = dsr_health_txt.strip()
json_start = dsr_json_candidate.find("{")
if json_start > 0:
    dsr_json_candidate = dsr_json_candidate[json_start:]
try:
    dsr_data = json.loads(dsr_json_candidate)
    raw_hosts = dsr_data.get("hosts") or dsr_data.get("results") or (dsr_data if isinstance(dsr_data, list) else [])
    if isinstance(raw_hosts, dict):
        for k, v in raw_hosts.items():
            if isinstance(v, dict):
                v = dict(v)
                v.setdefault("host", k)
                dsr_hosts.append(v)
    elif isinstance(raw_hosts, list):
        for item in raw_hosts:
            if isinstance(item, dict):
                dsr_hosts.append(item)
except ValueError:
    # Text fallback: parse lines like "host_name ... healthy|unhealthy ... platforms"
    for line in dsr_health_txt.splitlines():
        m = re.match(r"^\s*([a-zA-Z0-9_-]+)\s+.*\b(healthy|unhealthy|ok|fail|unreachable)\b", line, re.IGNORECASE)
        if m:
            dsr_hosts.append({
                "host": m.group(1),
                "status": m.group(2).lower(),
                "raw": line.strip(),
            })

# Map known hosts/platform tags
REQUIRED_PLATFORMS = ["darwin/arm64", "linux/amd64", "linux/arm64", "windows/amd64"]
HOST_PLATFORM_HINTS = {
    "mmini": ["darwin/arm64"],
    "mminiold": ["darwin/arm64"],
    "studio": ["darwin/arm64"],
    "local": ["darwin/arm64"],
    "mac": ["darwin/arm64"],
    "wlap": ["windows/amd64"],
    "wsurf": ["windows/amd64"],
    "trj": ["linux/amd64"],
    "ts1": ["linux/amd64"],
    "hz1": ["linux/amd64"],
    "hz2": ["linux/amd64"],
    "hz3": ["linux/amd64"],
    "hz4": ["linux/amd64"],
    "csd": ["linux/amd64"],
    "css": ["linux/amd64"],
    "fmd": ["linux/amd64"],
    "mold": ["linux/arm64"],
    "arm": ["linux/arm64"],
    "ovh-a": ["linux/amd64"],
    "ovh-b": ["linux/amd64"],
}

platform_healthy: dict[str, list[str]] = {p: [] for p in REQUIRED_PLATFORMS}
unhealthy_hosts = []

for h in dsr_hosts:
    name = str(h.get("hostname") or h.get("host") or h.get("name") or h.get("id") or "unknown")
    status_raw = str(h.get("status") or ("healthy" if h.get("healthy") is True else "unhealthy")).lower()
    is_healthy = (
        h.get("healthy") is True
        or status_raw in ("healthy", "ok", "ready", "pass")
    )
    plats = h.get("platforms") or h.get("targets") or []
    if isinstance(plats, str):
        plats = [plats]
    if not plats and isinstance(h.get("platform"), str) and h["platform"]:
        plats = [h["platform"]]
    if not plats:
        low = name.lower()
        for hint, hp in HOST_PLATFORM_HINTS.items():
            if low == hint or hint in low:
                plats = hp
                break
    if is_healthy:
        for p in plats:
            if p in platform_healthy:
                platform_healthy[p].append(name)
    else:
        unhealthy_hosts.append({"host": name, "status": status_raw, "platforms": plats})

missing_platforms = [p for p in REQUIRED_PLATFORMS if not platform_healthy[p]]

# Parse RCH rolling success rate
rch_success_pct = None
m_pct = re.search(r"Builds\s*:\s*\d+\s*total,\s*(\d+)%\s*command success rate", rch_status_txt)
if m_pct:
    rch_success_pct = int(m_pct.group(1))
rch_healthy = (rch_success_pct is not None and rch_success_pct >= min_rch_pct)

# Parse trj topology mismatch & critical disk alerts
trj_topology_error = bool(re.search(r"trj.*RCH_SOURCE_TOPOLOGY_INCOMPATIBLE|RCH_SOURCE_TOPOLOGY_INCOMPATIBLE.*trj", rch_doctor_txt + "\n" + rch_status_txt, re.DOTALL))
disk_alerts = []
seen_disk_workers = set()
for m in re.finditer(r"\bWorker\s+'?([a-zA-Z0-9_-]+)'?[^\n]*?(\d+(?:\.\d+)?)\s*(?:GB|GiB)\s*free", rch_status_txt + "\n" + rch_doctor_txt):
    wname, gb_str = m.group(1), m.group(2)
    gb = float(gb_str)
    if gb < min_disk_gb and wname not in seen_disk_workers:
        seen_disk_workers.add(wname)
        disk_alerts.append({"worker": wname, "free_gb": gb, "min_disk_gb": min_disk_gb})
for m in re.finditer(r"Worker '([^']+)' in critical pressure state \(disk_free_below_critical_gb\)", rch_status_txt):
    wname = m.group(1)
    if wname not in seen_disk_workers:
        seen_disk_workers.add(wname)
        disk_alerts.append({"worker": wname, "free_gb": None, "min_disk_gb": min_disk_gb, "reason": "disk_free_below_critical_gb"})

trj_dsr_unhealthy = any(h["host"].lower() == "trj" for h in unhealthy_hosts)
target_class_ready = (not trj_topology_error) and (not trj_dsr_unhealthy) and (not any(a["worker"] == "trj" for a in disk_alerts))
release_ready = (len(missing_platforms) == 0) and rch_healthy and (len(disk_alerts) == 0)

remediations = []
if trj_topology_error:
    remediations.append({
        "target": "trj",
        "code": "RCH_SOURCE_TOPOLOGY_INCOMPATIBLE",
        "action": "Reconcile /dp vs /data/projects vs /Users/jemanuel/projects physical mapping on trj after active owners settle, then run `rch workers capabilities --refresh` (see docs/ops/fleet-remediation-playbook-2026-11-20.md).",
    })
if missing_platforms:
    remediations.append({
        "target": ",".join(missing_platforms),
        "code": "MISSING_HEALTHY_DSR_PLATFORM",
        "action": f"Restore >=1 healthy DSR build host for: {', '.join(missing_platforms)} and verify via `dsr health all`.",
    })
if not rch_healthy:
    remediations.append({
        "target": "rch-fleet",
        "code": "RCH_SUCCESS_RATE_BELOW_FLOOR",
        "action": f"Investigate RCH failures (observed={rch_success_pct}%, floor={min_rch_pct}%) via `rch status --remediation` and `rch doctor`; never substitute local Cargo builds.",
    })
for a in disk_alerts:
    remediations.append({
        "target": a["worker"],
        "code": "WORKER_DISK_CRITICAL",
        "action": f"Reclaim build caches/ballast on {a['worker']} via `rch gc --dry-run --workers {a['worker']}` / `sbh` to restore >= {min_disk_gb:.0f} GiB free.",
    })

payload = {
    "schema_version": "frankenterm.fleet_preflight.v1",
    "generated_at": datetime.datetime.now(datetime.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ"),
    "read_only": True,
    "release_ready": release_ready,
    "target_class_ready": target_class_ready,
    "rch_healthy": rch_healthy,
    "rch_success_pct": rch_success_pct,
    "min_rch_success_pct": min_rch_pct,
    "missing_platforms": missing_platforms,
    "platform_healthy_hosts": platform_healthy,
    "unhealthy_dsr_hosts": unhealthy_hosts,
    "trj_topology_error": trj_topology_error,
    "disk_alerts": disk_alerts,
    "remediations": remediations,
    "playbook": "docs/ops/fleet-remediation-playbook-2026-11-20.md",
}

if write_artifact:
    out_p = Path(write_artifact)
    out_p.parent.mkdir(parents=True, exist_ok=True)
    out_p.write_text(json.dumps(payload, indent=2, sort_keys=True) + "\n", encoding="utf-8")

if json_output:
    print(json.dumps(payload, indent=2, sort_keys=True))
else:
    print(
        f"fleet preflight: release_ready={release_ready} target_class_ready={target_class_ready} "
        f"rch_healthy={rch_healthy} (rate={rch_success_pct}%/{min_rch_pct}%) "
        f"missing_platforms={missing_platforms or 'none'} disk_alerts={len(disk_alerts)}"
    )
    for r in remediations:
        print(f"  [{r['code']}] {r['target']}: {r['action']}")

sys.exit(0 if (release_ready and target_class_ready) else 1)
PY
