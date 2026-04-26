# Proposal: rename `runtime_compat` to `runtime_async` and drop the migration-seam framing

**Bead:** [ft-7iof6](../../.beads/issues.jsonl) — `runtime_compat is architecturally load-bearing`
**Status:** draft
**Related:** ft-y0loj (monolith split), ft-zoxxq (wezterm pseudo-boundary)

## Stance

**Stance (a): rename to `runtime_async`, acknowledge it as the canonical
async surface, retire the `SurfaceDisposition::{Keep,Replace,Retire}`
ledger.** Do NOT flatten the shim.

The module self-describes as a migration seam left over from the
post-Tokio transition, but the importer audit (numbers below) shows it
is the project's canonical async API and has been for at least a release.
It also exports a public `SurfaceDisposition` enum + a 7 615-line
`SURFACE_CONTRACT_V1` catalog whose entire purpose is to track which
APIs are still "transitional" — but the head of the module already
states that "Asupersync is now the sole async runtime." The
transitional framing is dead language; the import graph contradicts it.

Stance (b) — flatten the shim, replace 741 `runtime_compat::*` references
with direct `asupersync::*` imports — gives up a defensible abstraction
(stable internal surface independent of asupersync API churn,
project-curated ergonomic helpers like `sleep_with_cx` /
`timeout_with_cx`) for one indirection's worth of LOC. It is also a
multi-week mechanical-edit pass with no behavioural payoff. The honest
move is the rename, not the rip-out.

## Importer audit

| metric                                                                | count   |
|-----------------------------------------------------------------------|---------|
| `runtime_compat.rs` LOC                                               | 6 729   |
| `runtime_compat_surface_guard.rs` LOC                                 |   886   |
| total compat-surface LOC                                              | 7 615   |
| files in `crates/frankenterm-core/src/`                               |   488   |
| `.rs` files referencing `runtime_compat::` (anywhere)                 |    87   |
| individual `runtime_compat::` references                              |   741   |
| files using `use … runtime_compat` import paths                       |    69   |
| `pub use` / `pub fn` / `pub struct` / `pub trait` / `pub type` exports| ~115    |
| `SurfaceDisposition` consumers in unrelated modules                   | 3 files |
| asupersync workspace pin                                              | `0f04de1c…787e807` |
| `asupersync-runtime` feature gate                                     | no-op (Cargo.toml:677) |

Three observations:

1. **The "compat" framing is already retracted in the source.** Line 1
   of `runtime_compat.rs` reads "Asupersync runtime surface — wrappers
   and ergonomic helpers." Line 11 reads "The dual-runtime Tokio
   fallback was removed in ft-xbnl0.2.5. Asupersync is now the sole
   async runtime." The module's own header documentation has dropped
   the migration framing; the *name* and the `SurfaceDisposition`
   ledger have not caught up. Stance (a) finishes that catch-up.
2. **`SurfaceDisposition` is exercised but no longer load-bearing.**
   Three modules (`dependency_eradication.rs`,
   `manifest_dep_eradication.rs`, `vendored_async_contracts.rs`)
   match on `Keep | Replace | Retire`, but every entry in
   `SURFACE_CONTRACT_V1` whose disposition would matter (i.e.,
   `Replace` or `Retire`) was either replaced or kept; nothing is
   actively retiring. The ledger has become a static tribute to a
   migration that is over. Removing it costs three small refactors
   in those consumer files.
3. **Asupersync coupling already exists 87 files deep.** The pin is
   workspace-wide (`asupersync = "0.3.1"` + git override at
   commit `0f04de1c…787e807`). If asupersync breaks at that pin,
   the 87 files importing `runtime_compat::*` don't break in a way
   that the wrapper can absorb — they break because the wrappers
   themselves stop compiling. The wrapper insulates against
   `asupersync::*` *path* churn, not against ABI / semantic churn.
   That's still real value, but it is not the "we could swap runtimes
   if asupersync goes away" insulation the seam framing implies.

## Migration cost (sketched)

For stance (a) — rename + retire `SurfaceDisposition`:

1. Rename `runtime_compat` → `runtime_async` at the module declaration
   in `lib.rs` plus 87 importer files. Mechanical `sed` pass; 0
   semantic change.
2. Add a one-release deprecated re-export shim
   (`pub mod runtime_compat { pub use super::runtime_async::*; … }`)
   so external consumers (test crates, scripts) don't break on the
   day of the rename.
3. Delete `SurfaceDisposition`, `SurfaceContractEntry`,
   `SURFACE_CONTRACT_V1`, and `runtime_compat_surface_guard.rs`. Update
   the three consumers (`dependency_eradication.rs`,
   `manifest_dep_eradication.rs`, `vendored_async_contracts.rs`) to
   stop pattern-matching on a ledger that no longer exists. Each
   consumer's match arms collapse to a single arm.
4. Update AGENTS.md "runtime_compat: Audited runtime/channel/time/
   blocking boundary for asupersync-native code" to read
   "runtime_async: canonical async API surface — asupersync wrappers +
   ergonomic helpers (`sleep_with_cx`, `timeout_with_cx`, etc.)."
5. Delete the no-op `asupersync-runtime` feature flag (Cargo.toml:677).
   Either remove from the dependency lists referenced by tests or
   leave a one-line comment; the comment in Cargo.toml already calls
   it out as a no-op.

Total cost for (a): one renaming sweep, one ledger deletion, one
documentation pass. **Days, not weeks.** No production behaviour
changes.

For stance (b) — flatten the shim:

1. Replace each of the 741 `runtime_compat::X` references with the
   appropriate `asupersync::X` (or the direct dependency). Some are
   1:1; many are not — `runtime_compat::sleep_with_cx` has no exact
   asupersync analog, so each call site needs the helper inlined or
   pulled out into a new module. *Estimate: ~2 weeks of mechanical
   edits + compile loop.*
2. Re-evaluate every ergonomic helper in `runtime_compat.rs` for
   whether to (i) inline at call site, (ii) move to a new
   `frankenterm_core::async_helpers` module, or (iii) propose
   upstream into asupersync. *Estimate: ~1 week of design + RFCs to
   asupersync upstream.*
3. Drop the surface_guard tests and surface contract; rebuild whatever
   coverage they were giving us in a tighter form. *Estimate:
   ~3 days.*
4. Re-test under the `asupersync-runtime` no-op flag and every other
   feature flag combination. *Estimate: ~2 days.*

Total cost for (b): ~3 weeks of pure churn for the same observable
behaviour we have today.

## Why stance (a) is the right call

1. **The header already says we're done migrating.** The rename is
   making the file's name match its own opening paragraph.
2. **The wrapper is non-vacuous.** Even without dual-runtime support,
   the wrapper provides a stable internal API (`Mutex`, `RwLock`,
   `Semaphore`, `mpsc`, `watch`, `broadcast`, `oneshot`,
   `RuntimeBuilder`, `sleep_with_cx`, `timeout_with_cx`) that we
   control. Asupersync's API is not yet 1.0; insulating 87 files from
   churn there is real value.
3. **Stance (b) is a bigger version of stance (a)'s work, with worse
   payoff.** The 741 reference migration is unavoidable in either
   stance — stance (a) renames in-place; stance (b) replaces with a
   different module path. Both touch the same lines. Stance (a)
   preserves the abstraction.
4. **`SurfaceDisposition` is debt, not insurance.** The three consumer
   files that pattern-match on it would be cleaner with the enum
   gone; nobody is actively reading the ledger to decide what to
   migrate next.

## Why NOT stance (b)

1. **Fan-out.** 741 references times their context (signatures,
   bounds, type aliases) is a multi-week edit pass.
2. **Helper dispersion.** `sleep_with_cx`, `timeout_with_cx`, the Cx-
   first patterns generally — these exist in `runtime_compat`
   precisely because they're helpers we wrote, not asupersync APIs.
   Removing the wrapper module forces a new home for them, which is
   itself a wrapper module by another name.
3. **Asupersync version lock.** Pinning every consumer to
   `asupersync::*` paths makes the project's response to an
   asupersync API rename a 741-site mechanical edit instead of a
   wrapper-internal change.
4. **No revealed preference for it.** No agent / contributor has
   asked for the flattening. The only artifact arguing for it is the
   misleading "compat" name itself.

## Open questions

1. **Should `runtime_compat_surface_guard.rs` survive the rename?**
   No. It is 886 lines of test scaffolding whose purpose was to
   catch silent surface changes during the Tokio→asupersync migration.
   That migration is over. The right replacement is one or two
   targeted unit tests in `runtime_async.rs` (e.g. "every public
   export is documented"; "surface size doesn't grow without a
   release-note row"). File a child bead for that.
2. **Should the `asupersync-runtime` feature flag be deleted or kept
   as a comment?** Delete it. It's a no-op per the Cargo.toml comment
   itself. Anything still referencing it should error at compile time
   so the residue is forced out.
3. **Should the `SurfaceContractEntry` exports be made `#[deprecated]`
   for one release before deletion?** Yes — same one-release
   deprecation cycle as the `runtime_compat` module path. The three
   consumer files can be updated in the same commit that lands the
   `#[deprecated]` warnings; the deletion happens in a follow-up
   commit one release later.
4. **Does stance (a) block any future runtime swap?** No. The wrapper
   layer is exactly what makes a future runtime swap tractable. The
   rename does not affect that capability — it just stops pretending
   the swap is in progress.

## Acceptance criteria for this proposal

- [ ] Stance (a) is recorded; this file lands in `docs/proposals/`.
- [ ] Five child beads filed under ft-7iof6 covering the rename, the
      ledger deletion, the surface-guard replacement, the AGENTS.md
      update, and the feature-flag cleanup.
- [ ] Each child bead has a one-line "blast radius" estimate (files
      touched / mechanical-vs-design split) so the swarm can pick
      them up independently.

## Child beads to file

The next step is the bead-creation pass. Proposed beads:

1. `ft-7iof6.1` — rename `runtime_compat` module path to
   `runtime_async`; sed across 87 importer files; add a one-release
   deprecated re-export at `runtime_compat` so external consumers
   don't break on the day of the rename.
2. `ft-7iof6.2` — delete `SurfaceDisposition`, `SurfaceContractEntry`,
   `SURFACE_CONTRACT_V1`; collapse the three pattern-matching consumers
   (`dependency_eradication.rs`, `manifest_dep_eradication.rs`,
   `vendored_async_contracts.rs`) to a single-arm form.
3. `ft-7iof6.3` — replace `runtime_compat_surface_guard.rs` (886 LOC of
   migration-era scaffolding) with a tighter "every public export is
   documented + surface size has a release-note row when it grows"
   guard. Net code delta should be negative.
4. `ft-7iof6.4` — update AGENTS.md "Current Module Map" entry for
   runtime_compat to read the canonical-surface description, and
   update the related skill note in `cm` / `bv` skills if they cite
   the old name.
5. `ft-7iof6.5` — delete the no-op `asupersync-runtime` feature flag
   from `crates/frankenterm-core/Cargo.toml:677`; remove every
   `required-features = ["asupersync-runtime"]` from `[[bench]]` and
   `[[test]]` declarations that still carry it; verify CI matrix
   doesn't try to enable it explicitly.
6. `ft-7iof6.6` — file a follow-up *only if* an asupersync API rename
   actually arrives during the deprecation window; until then the
   ledger-rename is enough. Placeholder bead to track the trigger.
