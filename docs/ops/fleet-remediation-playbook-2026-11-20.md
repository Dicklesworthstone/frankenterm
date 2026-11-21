# DSR & RCH Fleet Remediation Playbook — 2026-11-20 (`ft-mgy5t2.11`)

> **Rules:** Strictly DSR-only (zero GitHub Actions, Rule 0.1). Never restart/stop/kill `am` / Agent Mail. Never weaken RCH fail-closed rules or substitute local Cargo output as remote proof. Never edit `workers.toml` or `rch workers disable` for transient worker hiccups.

## 1. Observed Fleet Failures (2026-11-20)

| Host / Surface | Observed Failure | Blocking Impact |
|---|---|---|
| `trj` (64-core target-class host) | `RCH_SOURCE_TOPOLOGY_INCOMPATIBLE`: `canonical="/Users/jemanuel/projects"` vs `alias="/dp"` (`alias_physical="/data/projects"`); `/scratch` 98% full | Blocks 64-core `resource-cockpit-target-class.json` (`ft-7h5da.10.4.3`, `ft-mgy5t2.29`) and 8 RCH build slots |
| `wlap`, `wsurf` (Windows DSR hosts) | `wlap` unreachable; `wsurf` <40 GiB disk free | Blocks `windows/amd64` archive in signed cross-platform stable DSR release (`ft-mgy5t2.1`) |
| `hz1`, `mminiold`, `ts1`, `hz2` | `hz1` (5.9 GiB) & `mminiold` (1.9 GiB) disk critical; `ts1` 97% full; `hz2` memory/disk pressure | Drives RCH rolling success rate down to 28–34% (`RCH-E330` disk-budget refusals) |
| `csdw1030`, `hz3w1030` | DSR health checks failing | Reduces cross-platform Linux build headroom |

## 2. Non-Destructive Operator Recovery Procedures

### A. Recover `trj` Source Topology & `/scratch` Disk
1. Confirm no active build owns `/Users/jemanuel/projects` or `/data/projects` on `trj`:
   ```bash
   rch queue --json
   ```
2. On `trj`, inspect `/dp`, `/data/projects`, `/Users/jemanuel/projects`, and `/scratch` usage:
   ```bash
   ssh ubuntu@100.91.120.17 'ls -ld /dp /data/projects /Users/jemanuel/projects; df -h / /tmp /scratch'
   ```
3. Reconcile the conflicting alias base so `canonical_physical` and `alias_physical` resolve consistently without destroying source repositories, clear expired build caches under `/scratch` (target `< 80%` used, `>= 40 GiB` free), then refresh RCH capabilities:
   ```bash
   rch workers capabilities --refresh --json
   rch doctor --reliability --scope topology,pressure
   ```

### B. Reclaim Worker Disk Headroom (`hz1`, `mminiold`, `ts1`, `hz2`)
1. Preview safe RCH garbage collection (strictly `--dry-run` first):
   ```bash
   rch gc --dry-run --workers hz1
   rch gc --dry-run --workers mminiold
   rch gc --dry-run --workers ts1
   ```
2. Apply RCH cache reclamation / `sbh` ballast recovery on confirmed idle workers until each worker has `>= 40 GiB` free, then verify:
   ```bash
   rch workers list
   ```

### C. Restore Windows DSR Capacity (`wsurf` / `wlap`)
1. Check `wsurf` disk headroom (`>= 40 GiB` free required by DSR) and `wlap` Tailscale/SSH reachability:
   ```bash
   dsr health all
   ```
2. Reclaim old target directories on `wsurf` (or reconnect `wlap`) until `dsr health all` reports `>= 1` healthy `windows/amd64` host.

## 3. Verification Gate

After operator recovery, run the strictly read-only preflight gate:
```bash
bash scripts/fleet-proof-preflight.sh --json \
  --write-artifact docs/attestations/infrastructure/fleet-preflight.json
```
Exit `0` requires `release_ready == true`, `target_class_ready == true`, `missing_platforms == []`, `disk_alerts == []`, and `rch_success_pct >= 85`.
