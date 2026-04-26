# Proposal: stop pretending `crate::wezterm` is a backend abstraction

**Bead:** [ft-zoxxq](../../.beads/issues.jsonl) — `crate::wezterm pseudo-boundary`
**Status:** draft
**Related:** ft-y0loj (monolith split), ft-7iof6 (runtime_compat reframe)

## Stance

**Stance (b): commit to the wezterm-fork identity. Drop the abstraction theatre.**

The `WeztermInterface` trait + `WeztermHandle = Arc<dyn WeztermInterface>`
type alias exist, but the importer audit (numbers below) shows that
consumers reach for the concrete `WeztermClient` and concrete data types
(`PaneInfo`, `SpawnTarget`, `BackendKind`, etc.) almost everywhere. Stance
(a) — extracting a real backend trait with type erasure across 31 core
importers — is a multi-month rewrite for negligible benefit because we
have no second mux backend planned and no existing caller is written to
the trait surface.

The honest move is to rename the abstraction to reflect its actual job
(the in-process mux session interface), keep the concrete types as
crate-local public types, and update the README + AGENTS.md framing so
agents and reviewers stop treating the boundary as load-bearing.

## Importer audit

| metric                                                                      | count    |
|-----------------------------------------------------------------------------|----------|
| `wezterm.rs` LOC                                                            | 7 803    |
| `wezterm_native.rs` LOC                                                     | 38       |
| `crates/frankenterm-core/src/*.rs` files importing `crate::wezterm`         | 31       |
| references to concrete types (`WeztermClient` / `PaneInfo` / `SpawnTarget`) | 192      |
| direct `WeztermClient` construction call-sites                              | 58       |
| files using `dyn WeztermInterface` outside `wezterm.rs` itself              | 0        |
| files mentioning the `WeztermInterface` trait at all (any context)         | 9        |
| vendored `frankenterm/<crate>/` directories                                 | 48       |

Three observations from those numbers:

1. **The trait does no work.** Outside `wezterm.rs`, no file in core
   takes a `&dyn WeztermInterface` parameter. The trait is exercised
   only by `Arc<dyn WeztermInterface>` *inside* `wezterm.rs` for one
   forwarding `impl` (so `Arc::clone` propagates through). Consumers
   either own a concrete `WeztermClient` or call functions that take
   one by reference.
2. **The concrete-type fan-out is enormous.** 192 references to
   concrete public types across 31 files is what stance (a) would have
   to migrate to `Box<dyn MuxBackend>` or similar. Each call-site
   touches a published type that in turn names other concrete types
   (e.g., `PaneInfo` references `WaitMatcher` which references
   `BackendKind`). A real trait extraction would have to convert that
   transitive surface into either method-on-trait calls or
   trait-object-bound generics — at the cost of every importer.
3. **The 48 vendored subcrates are not optional in any meaningful
   sense.** They are workspace members under `frankenterm/` (codec,
   mux, term, termwiz, config, window, …). Several are forked from
   upstream wezterm; several are home-grown (`escape-parser`,
   `frecency`, `lfucache`); all are imported by the GUI binary. The
   "wezterm is optional" framing in AGENTS.md does not survive contact
   with the workspace `Cargo.toml`.

## Migration cost (sketched)

For stance (a) — extracting a true `MuxBackend` trait with object-safe
type erasure — the work would have to:

1. Define a `MuxBackend` trait with ~120 methods (every public method
   currently on `WeztermClient`). Several of those methods today take
   or return concrete types from vendored crates (`mux::pane::PaneId`,
   `config::keyassignment::SpawnTabDomain`, …). Object safety forces
   us either to keep those concrete or to introduce parallel
   trait-friendly types. *Estimate: ~3 weeks of design + breaking
   changes.*
2. Migrate the 192 concrete-type references to either `Box<dyn
   MuxBackend>` or a sealed enum of backend variants. Each call-site
   has to be re-typechecked. *Estimate: ~2 weeks of mechanical edits +
   compile loop.*
3. Add a second backend impl to make the abstraction non-vacuous.
   No second backend is planned (and the native mux server reuses the
   wezterm protocol via the codec crate, so it isn't a "second
   backend" in the trait sense — it's the same backend, server side).
   *Estimate: ∞ until a real second backend appears.*

For stance (b) — commit to the wezterm-fork identity — the work is:

1. Rename `WeztermInterface` → `MuxInterface` (+ alias for one release
   so external imports don't break).
2. Relocate `WeztermClient` and the concrete data types into a
   `mux_client` module (or `mux/`) so the file:purpose mapping reads
   cleanly. `wezterm.rs` shrinks to a thin re-export shim that
   future-grep'ers can find.
3. Update AGENTS.md and the workspace README to drop the "implementation
   boundary" framing. Replace it with: *"frankenterm is a fork of
   wezterm-the-mux. The vendored `frankenterm/*` subcrates are
   first-class workspace members. There is no plan to support a
   second mux backend."*
4. Add a one-line CI check that fails if any new file outside
   `wezterm.rs`/`mux_client.rs` tries to introduce a `dyn
   MuxInterface` import — codifying that stance (b) is the active
   contract.

Total cost for (b): a renaming sweep, a documentation pass, and one
clippy-style guard. Days, not months.

## Why stance (b) is also philosophically correct here

The "implementation boundary" framing is what AGENTS.md aspires to. It
is also what we'd want if we were planning to swap mux engines. We're
not. The native mux server, the GUI, the recorder, the workflow runner
— all of them are coupled to the wezterm pane/tab/window model because
that is the model. Pretending otherwise is what made the trait surface
get added; the absence of any consumer that uses the trait is the
project's revealed preference.

ADR 0002 says "one writer terminal ownership" — that contract is
expressed in concrete `WeztermClient` semantics (Cx delegation,
runtime guard sequencing, etc.), not in trait methods. Stance (b)
makes the contract enforcement match the contract description.

## Open questions

1. **Does committing to the fork identity block future portability?**
   Probably yes, but only for "second mux backend" futures. It does not
   block running on additional OSes (already work), additional GPU
   stacks (the GUI crate handles that), or additional asynchronous
   runtimes (handled by `runtime_compat`, ft-7iof6). The 48 vendored
   subcrates are the load-bearing portability surface, not the trait.
2. **What does the rename buy us, concretely?** The honest answer is
   discoverability: a new contributor reading `wezterm.rs` today thinks
   "this is the wezterm backend impl"; they should be reading "this is
   the mux session API". The rename is paying for a mental-model
   correction, not a technical capability.
3. **Should the 48 vendored subcrates also be renamed?** No. Their
   names (`mux`, `term`, `termwiz`, `codec`, `config`, `window`)
   already describe what they do. The fork-vs-upstream relationship is
   a `PROVENANCE.md` concern, not a name-of-crate concern.

## Acceptance criteria for this proposal

- [ ] Stance (b) is recorded; AGENTS.md is updated to match.
- [ ] Child beads filed under ft-zoxxq for the renaming sweep, the
      relocation, the framing update, and the CI guard.
- [ ] `WeztermInterface` trait gains a deprecation note pointing at
      `MuxInterface` once the rename lands.

## Child beads to file

The next commit on this thread should be the bead-creation pass.
Proposed beads:

1. `ft-zoxxq.1` — rename `WeztermInterface` → `MuxInterface` with a
   one-release type alias for backward compat.
2. `ft-zoxxq.2` — relocate `WeztermClient` and its concrete data types
   out of `wezterm.rs` into `mux_client.rs`; leave `wezterm.rs` as a
   re-export shim.
3. `ft-zoxxq.3` — update AGENTS.md + workspace README to drop the
   "implementation boundary" framing; add a one-paragraph "frankenterm
   is a wezterm fork" identity statement.
4. `ft-zoxxq.4` — add a CI guard (clippy lint or grep-based check)
   that fails if a new file outside the mux module introduces
   `dyn MuxInterface` — codifies the stance.
5. `ft-zoxxq.5` — audit the 48 vendored `frankenterm/<crate>/` dirs:
   produce a `PROVENANCE.md` table listing each as
   `forked-from-upstream` / `home-grown` / `experimental`, with the
   upstream commit hash where applicable.
