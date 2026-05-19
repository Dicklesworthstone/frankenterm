# Deferred Proof Ownership Gate Contract

`ft.deferred_proof_ownership_gate.v1` decides whether a queued deferred proof
receipt can be replayed in the current checkout without proving or staging
another agent's work. It is a static contract for `ft-zbnz4.3`; no runtime CLI
surface ships with this document.

The schema is `docs/json-schema/ft-deferred-proof-ownership-gate.json`; the
fixture corpus lives under `fixtures/deferred-proof-replay/ownership-gate/`.

## Purpose

Deferred proof receipts are useful only if replay is fail-closed. A receipt that
was valid yesterday can be unsafe today when:

- current dirty paths overlap the receipt ownership slice;
- an active non-stale in-progress bead owns the same paths;
- Agent Mail is unavailable and no fallback coordination snapshot exists;
- prerequisite beads are still open, blocked, or in progress;
- the receipt freshness window has expired; or
- RCH admission is still blocked for material Cargo proof.

The gate converts those facts into a small decision vocabulary that Robot/MCP
surfaces and later replay runners can explain without reading historical Beads
comments by hand.

## Decision States

| State | Meaning |
| --- | --- |
| `allow` | Ownership is clean and the receipt may be replayed. |
| `wait` | Ownership is clean, but RCH or shared tracker state says to wait. |
| `stale` | The receipt is outside its freshness window and must be regenerated. |
| `dirty_overlap` | Current or captured dirty paths overlap the receipt-owned paths. |
| `owner_handoff_required` | A non-stale in-progress owner overlaps the receipt-owned paths. |
| `prerequisite_blocked` | At least one prerequisite bead is not closed. |
| `mail_state_unknown` | Neither Agent Mail state nor the documented fallback snapshot is available. |

Only `allow` may set `replay_allowed: true`.

## Fail-Closed Rules

- Non-overlap dirty files are reportable context, not a reason to mutate them.
- `.beads/issues.jsonl` dirty while Agent Mail is unavailable produces `wait`
  because tracker state cannot be safely bundled with proof replay.
- Active overlapping owners produce `owner_handoff_required`; stale detection
  requires explicit Beads evidence and must not be guessed from the filesystem.
- Dirty overlap produces `dirty_overlap` even if RCH is otherwise admissible.
- Missing Agent Mail and missing fallback snapshot produces `mail_state_unknown`.
- Material Cargo proof blocked by RCH admission produces `wait`; local Cargo is
  never proof.

The gate must never recommend deletion, stash, reset, broad formatting, local
Cargo proof, RCH service repair, RCH worker mutation, Agent Mail repair, build
cancellation, or proving unowned dirty work.

The machine-readable `forbidden_actions` vocabulary is:

- `delete_files`
- `destructive_git`
- `stash_or_reset_worktree`
- `broad_formatting`
- `local_cargo_proof`
- `rch_service_repair`
- `rch_worker_mutation`
- `agent_mail_repair`
- `build_cancellation`
- `prove_unowned_dirty_work`

## Golden Corpus

The static verifier freezes these scenarios:

- `allow-clean-static`
- `wait-rch-critical-pressure`
- `dirty-overlap-current`
- `owner-handoff-active`
- `prerequisite-blocked`
- `mail-state-unknown`
- `stale-receipt`
- `wait-shared-tracker-dirty`

Run:

```bash
bash tests/e2e/test_deferred_proof_ownership_gate.sh
```

This is static contract work. Any later implementation that compiles Rust,
executes `ft`, or reaches Cargo must use remote-required RCH.
