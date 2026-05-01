# iTerm2 OSC 1337 Cluster — GUI Prompt + Multipart + SetColors

**Bead:** [BR-TERM-EMULATOR-UPLIFT-2.1.3.cont] / `ft-tzusd`
**Parent:** `ft-fy4ty` (term-layer security gate shipped at
`74c75dca9` — `Alert::SetProfileRequested` + 6 integration
tests).
**Status:** Foundation slice shipped. Contract layer +
multipart accumulator with caps + allowlist state machine +
rollout staging + audit doc + 29 tests all live; production
GUI modal + VHS-captured fixtures + parser audit + SetColors
dispatch are integration follow-ons.

## Why this matters

iTerm2's `OSC 1337` extension covers a cluster of in-band
protocols TUI apps can use to mutate frankenterm's UI state:

- `File=name=...; size=...; inline=1` — inline image / file
  display (the canonical `imgcat` invocation).
- `SetProfile=<name>` — request a profile switch.
- `SetColors=<slot>=<rgb>;...` — palette mutation.
- `MultipartFile` — chunked file transfer for large payloads.

The parent bead established that **`SetProfile` raises an
`Alert::SetProfileRequested`** so profile state never mutates
silently. This continuation extends the same security framing
to the rest of the cluster + ships the chunking accumulator
with bounded depth and total size.

## Multipart accumulator caps

`MultipartFileAccumulator::DEFAULT_DEPTH_CAP = 1024` chunks;
`MultipartFileAccumulator::DEFAULT_SIZE_CAP = 64 MiB`.

A runaway multipart could OOM the renderer; the cap forces
`MultipartDenialReason::SizeCapExceeded` instead. State is
unchanged on `Denied` (verified by 1024-trial random sweep
that asserts `acc == prior` after every Denied outcome).

Denial taxonomy:

| Reason | Trigger |
|---|---|
| `DepthCapExceeded { cap }` | More than `cap` chunks attempted |
| `SizeCapExceeded { cap, attempted }` | Cumulative bytes would exceed `cap` |
| `SizeMismatch { declared, received }` | `finalize()` called with declared size != received |
| `AlreadyFinalized` | `append_chunk` or second `finalize` after finalization |

## Allowlist state machine

`AllowlistDecision`:

- `Allow` — one-shot, doesn't persist.
- `Deny` — one-shot.
- `AlwaysAllow { app_id }` — persists in `ProfileAllowlist`
  for future requests.

`ProfileAllowlist::resolve(app_id)` returns
`Some(Allow)` if already allowlisted; otherwise `None` (caller
prompts). `apply()` returns `true` iff the allowlist mutated
(idempotent on duplicate AlwaysAllow for same `app_id`).

## Rollout staging

The bead specifies three phases:

| Phase | User prompted | Allowlist persists |
|---|---|---|
| `AlwaysDeny` | no | no |
| `PromptEachTime` | yes | no |
| `RememberPerName` | yes | yes |

Default starts at `AlwaysDeny`. The `Iterm2Osc1337Health`
snapshot's `is_safe()` predicate enforces that
**`set_profile_allows_total` MUST be 0 in `AlwaysDeny` phase**
— if it ever fires, the gate has a bug and the doctor warns.

## Conformance corpus

3 fixtures at `tests/golden/iterm2/<slug>/`:

| Slug | Coverage |
|---|---|
| `imgcat` | Recorded imgcat invocation (canonical inline image) |
| `setprofile` | Recorded profile-switch attempt; exercises the security gate |
| `setcolors` | Palette mutation sequence |

Each fixture's bytes get captured via VHS once a Linux GPU CI
runner is provisioned; the contract layer is the slot they
fill.

## SetColors privileged-slot rule

`SetColorsPaletteEntry::is_privileged_slot()` returns `true`
for: `fg`, `bg`, `curfg`, `curbg`, `selbg`, `selfg`. These
affect text readability or cursor visibility — the security
gate alerts on them just like `SetProfile`. ANSI / 256-color
indexed slots are unprivileged (changing color 5 doesn't
break rendering).

## Tests

| Test | Coverage |
|---|---|
| 29 lib tests | every transition + serde + cap behavior + allowlist + rollout + corpus + privileged slots |
| `multipart_random_schedule_sweep_no_invariant_violations` | 1024 trials × 16 ops = ~16k ops; asserts state unchanged on every Denial |

## Bead acceptance status

| Item | Status |
|---|---|
| GUI confirmation prompt UI | ⏳ integration follow-on (consumes `AllowlistDecision` + `ProfileAllowlist`) |
| File argument matrix completeness audit | ✓ `ItermFileArgument` typed envelope + `is_fully_audited()` predicate; parser audit per-fixture is integration follow-on |
| SetColors palette mutation | ✓ `SetColorsPaletteEntry` + `is_privileged_slot()` predicate; dispatch wiring is integration follow-on |
| MultipartFile chunking | ✓ `MultipartFileAccumulator` with 4-reason denial taxonomy + 1024-trial sweep |
| Conformance corpus | ✓ types + paths shipped; VHS-captured fixtures land on GPU runner |
| Feature-flag rollout staging | ✓ `RolloutPhase` enum (AlwaysDeny / PromptEachTime / RememberPerName) |
| Per-release attestation entry | ⏳ depends on `ft-syqcz.1` |

## Cross-references

- **Parent term-layer gate:** `ft-fy4ty` shipped at
  `74c75dca9` — `Alert::SetProfileRequested` variant.
- **Sibling DEC 2026 work:** `ft-u6jos` — same RolloutPhase
  + ft-mpc9b.9 substrate.
- **Sibling foundation fixtures** (same `*Health` /
  state-machine + caps pattern):
  `passive_watch_invariant`, `redactor_coverage_matrix`,
  `wire_dedup_model`, `tx_killswitch_model`,
  `dec_2026_presentation_hold`, `audit_erasure_spec`,
  `gpu_regression_fuzz_report`, plus the 5 robot-family
  state machines (checkpoint, work, fleet, context, profile).
- **Attestation cross-link:** `ft-syqcz.1`.
