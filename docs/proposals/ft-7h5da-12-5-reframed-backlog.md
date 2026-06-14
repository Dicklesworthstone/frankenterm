# ft-7h5da.12.5 Reframed-But-Alive Backlog

Status: register artifact for `ft-7h5da.12.5`.

This register preserves ideas from the 2026-06-06 duel that remain worth doing
later, but only in the reframed scope below. The intent is to prevent later
agents from rebuilding the rejected version of an idea because the shorter bead
summary looked attractive in isolation.

## Promotion Rule

Do not implement an entry directly from this register. Promote it to a normal
bead first, with a concrete owner, acceptance criteria, proof plan, and explicit
links to the dependency work named here.

When promoting an entry, preserve the `Do not build` clause. Removing that
clause requires new evidence, not just renewed enthusiasm.

## Register

| Idea | Reframed scope | Do not build | Promotion trigger |
| --- | --- | --- | --- |
| Sludge compaction | Hash-identity service-time compaction for `ft robot get-text`: serve stable SHA-256/FNV-1a/XOR identity summaries while canonical storage keeps raw bytes. | Lossy storage compaction or BOCPD-based semantic deletion of captured pane text. | A measured read-path token/latency problem where raw bytes remain available and redaction semantics are preserved. |
| Semantic breakpoints | Policy-gated mission pause, input quarantine, or approval handoff at the control-plane boundary. | `SIGSTOP` or PTY-child suspension of a live model/API stream. | W6 attention/intervention surfaces need a stronger "pause before action" primitive. |
| Adversarial consensus | Optional structured-reviewer approval strategy where reviewers speak Robot Mode contracts. | Scraping arbitrary pane text and treating it as policy input. | A workflow needs multi-reviewer approval and can express reviewer outputs as typed Robot/MCP envelopes. |
| Mission rehearsal | Structured findings, blocking reasons, and next actions emitted from rehearsal artifacts. | A single numeric score that rubber-stamps scenario quality. | Rehearsal scenarios already produce artifacts but operators still need comparable findings. |
| Durable agent sessions | Native resume with an honest fallback ladder: `resumed`, `fresh_with_context`, or `fresh_blind`. | Claiming continuity when filesystem, process, network, or model state was not actually restored. | Agent correlator/session IDs are sufficient to drive a user-visible resume attempt. |
| Timeline forensics | Offline read-path composition with explicit confidence per inferred relation. | Causal claims from coincidental timestamps. | Incident autopsy needs cross-surface timeline explanation and can label every edge with evidence strength. |
| Fleet reconciliation | `ft fleet apply` after attention routing and verified-submit mature. | Bulk fleet mutation before ownership, submission proof, and policy gates are live. | W6 attention plus W2 verified-send can prove ownership and delivery for each mutation. |
| WASM phased rollout | Fuel-budgeted detection rules first, after Bloom/replay compatibility is stable. | General-purpose extension execution in the hot path before deterministic replay and fuel accounting exist. | Rule packs need sandboxed logic that cannot be expressed safely in existing matchers. |
| Contract SDKs | Generated-thin SDKs from serde types only. | Hand-authored compatibility layers or bespoke client semantics. | Robot/MCP contract doctor and error-code taxonomy are stable enough to generate clients. |
| Doctor fix mode | No unattended `--yes` in v1; only independent double-probes before any lock fix. Destructive classes remain non-executable. | Automatic destructive repair or one-probe lock cleanup. | A recurring operator issue has two independent, non-destructive probes and an auditable remediation receipt. |

## Dependencies

- `ft-7h5da.3.*` for verified-submit before fleet mutation.
- `ft-7h5da.7.*` for attention, intervention, and ownership surfaces.
- `ft-7h5da.13.3` for stable error-code taxonomy before generated SDKs.
- Existing redaction/read-path policy before any evidence or rehearsal surface
  promotes captured text into reusable artifacts.
