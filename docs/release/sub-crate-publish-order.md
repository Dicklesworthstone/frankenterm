# Sub-crate publish order (ft-fytns)

The ft-y0loj monolith-split work has shipped 10 sub-crates carved out
of `frankenterm-core`. They publish in a strict topological order:
**leaves first, then `frankenterm-core`, then mid-tier sub-crates that
depend back on it.** Skipping the order causes `cargo publish` to fail
with "no matching package named X found" because the sub-crate's
versioned dep on a not-yet-published crate cannot resolve from
crates.io.

The release script `scripts/publish-sub-crates.sh` emits the commands
in the right order. This doc is the *why*.

## Dependency graph

Computed from each `Cargo.toml`'s `[dependencies]` (regular only;
`[dev-dependencies]` are stripped at publish time and don't constrain
ordering).

```
Level 0 — true leaves (no frankenterm-* deps):
  frankenterm-core-config-types
  frankenterm-core-error-types
  frankenterm-core-policy-types
  frankenterm-core-replay-types
  frankenterm-core-resource-types
  frankenterm-core-telemetry-types

Level 1 — frankenterm-core (depends on all 6 leaves):
  frankenterm-core

Level 2 — mid-tier (depend on frankenterm-core ± leaves):
  frankenterm-core-ars       → frankenterm-core
  frankenterm-core-fleet     → frankenterm-core + frankenterm-core-resource-types
  frankenterm-core-replay    → frankenterm-core + frankenterm-core-replay-types
  frankenterm-core-tantivy   → frankenterm-core
```

`frankenterm-core` itself dev-depends on the 3 of 4 mid-tier crates
(ars, replay, tantivy) for its integration tests. Cargo allows dev-dep
cycles in the workspace; at publish time the `[dev-dependencies]`
section is omitted, so the cycle does not block publishing.

## Topological order

```
1. frankenterm-core-config-types
2. frankenterm-core-error-types
3. frankenterm-core-policy-types
4. frankenterm-core-replay-types
5. frankenterm-core-resource-types
6. frankenterm-core-telemetry-types
7. frankenterm-core
8. frankenterm-core-ars
9. frankenterm-core-fleet
10. frankenterm-core-replay
11. frankenterm-core-tantivy
```

Levels 0 and 2 are internally parallel-safe — items within the same
level have no inter-dependencies — but for predictable failure
reporting the script publishes serially.

## Path-deps will block publishing

Every sub-crate's `Cargo.toml` declares its inter-workspace deps as
`{ path = "../frankenterm-core-X" }`. Cargo accepts this for local
development but **rejects** it during `cargo publish` unless paired
with a `version = "x.y.z"` field. The release sequence either:

- adds explicit `version = "..."` fields next to every path dep
  before publishing, **or**
- relies on `cargo publish`'s implicit version-from-workspace
  inheritance (works only when the dep crate is already on
  crates.io at the requested version).

Practically: bump every workspace-member version in lockstep, push
them together, and run the publish script in order. Don't try to
publish only one sub-crate at a time without first ensuring its
dependencies are already on crates.io at the matching version.

## Running the publish script

```sh
# Dry-run (prints the cargo commands but does not execute):
scripts/publish-sub-crates.sh --dry-run

# Actually publish (requires CARGO_REGISTRY_TOKEN):
scripts/publish-sub-crates.sh
```

The script aborts on the first failure so you can re-run from the
breakpoint after fixing whatever blocked the publish (typically a
missing version field on a path dep, or a dirty working tree).

## Adding a new sub-crate

When ft-y0loj.x lands a new sub-crate:

1. Add it to `scripts/publish-sub-crates.sh` in its correct
   topological slot (see the levels above).
2. Update the dependency graph in this doc.
3. Verify `cargo publish --dry-run -p <new-crate>` succeeds in a
   fresh checkout before claiming the bead complete.

## Verification

The script and this doc were reconciled against the workspace at
ft-fytns close. Re-run this sanity check after any new extraction:

```sh
for c in crates/frankenterm-core-*; do
  name=$(basename "$c")
  deps=$(grep -E "^frankenterm-(core|alloc)" "$c/Cargo.toml" \
    | sed -E 's/ ?=.*//' | sort -u | grep -v "^$name$" \
    | tr '\n' ',' | sed 's/,$//')
  printf "%-38s deps: %s\n" "$name" "${deps:-<none>}"
done
```

The output should match the dependency graph above. If a dep edge
appears that crosses a level (e.g. a leaf gaining a `frankenterm-core`
dep), the topological order in the script needs revisiting before the
next release.
