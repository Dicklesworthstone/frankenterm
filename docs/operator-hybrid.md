# Operator Hybrid Model

Status: pilot-ready for `ft-v5lz3.2.5`.

`scripts/swarm-tick.sh` is still the operator's compact four-minute snapshot, but it now includes a `.coordinator` section populated from read-only NTM robot surfaces. The installed NTM binary on this host does not expose `ntm coordinator status|digest|conflicts|enable`; the live equivalent commands are:

| Operator decision | NTM equivalent used now | Can be automated? | Notes |
| --- | --- | --- | --- |
| Session health/status | `ntm --robot-health=<session> --json` | Yes, read-only | Feeds `.coordinator.status` with healthy/degraded/unhealthy/rate-limited counts. |
| Attention digest | `ntm --robot-alerts --alerts-session <session> --json` | Yes, read-only | Feeds `.coordinator.digest` with active and critical/error alert counts. |
| File conflict scan | `ntm conflicts <session> --since 6h --limit 10 --json` | Yes, read-only | Feeds `.coordinator.conflicts.count`. |
| Auto-assignment candidates | `ntm --robot-assign=<session> --strategy=balanced --json` | Partially | Feeds `.coordinator.auto_assign`; the tick does not mutate beads or dispatch work. |
| Routine auto-assign daemon | `ntm coordinator enable auto-assign` | Blocked | The current NTM binary returns `unknown command "coordinator"`. |
| Periodic digest daemon | `ntm coordinator enable digest --interval=30m` | Blocked | Same missing command surface. |
| Conflict resolution | `ntm conflicts` plus human judgment | No | The script can flag conflict count; an operator chooses whether to pause, reassign, or ask agents to coordinate. |
| Stuck-pane recovery | `ntm --robot-health`, `ntm --robot-alerts`, `ntm --robot-send` | Partially | Detection can be summarized; cooperative nudges remain operator-controlled per AGENTS.md SO rules. |
| Backlog strategy | `br ready --json`, `bv --robot-triage`, `ntm --robot-assign` | No | Choosing broad strategy or new exploratory work still needs operator judgment. |
| Convergence call | `swarm-tick.sh` counters plus CI/bead state | No | The script surfaces evidence; the operator decides if the session is actually converged. |

## Current Tick Contract

The `.coordinator` object is intentionally summary-shaped:

```json
{
  "mode": "ntm_robot_equivalents",
  "native_coordinator_available": false,
  "status": {
    "total_agents": 6,
    "healthy": 5,
    "degraded": 1,
    "unhealthy": 0,
    "rate_limited": 0
  },
  "digest": {
    "active_alerts": 1,
    "critical_or_error_alerts": 1
  },
  "conflicts": { "count": 0 },
  "auto_assign": {
    "idle_agents": 1,
    "recommendations": 0,
    "blocked_beads": 0
  }
}
```

The operator loop can consume these numbers without parsing large NTM payloads. Any command failure or missing NTM surface falls back to zero counts while preserving valid JSON.

## Recommendation

Use hybrid mode for evidence collection and routine triage prompts, not autonomous dispatch yet. Promote auto-assignment or digest generation to daemon mode only after the deployed NTM binary exposes the `ntm coordinator` command family and a pilot run proves it does not conflict with Beads ownership or AGENTS.md dispatch rules.
