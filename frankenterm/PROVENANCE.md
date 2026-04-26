# FrankenTerm Provenance

Source: <https://github.com/wezterm/wezterm>
Reference commit: `05343b387085842b434d267f91b6b0ec157e4331`
Date imported: 2026-02-10
License: MIT (see `../LICENSE`)

## Scope

The `frankenterm/<crate>/` subtree vendors a subset of the wezterm
workspace plus a handful of home-grown FrankenTerm-specific crates.
This is the load-bearing portability surface for the project — the
`MuxInterface` trait + concrete client (`crates/frankenterm-core/src/wezterm.rs`,
`mux_client.rs`) sit *on top of* this subtree.

Per `docs/proposals/ft-zoxxq-mux-boundary-truth.md` stance (b) — committed
to the wezterm-fork identity — these vendored crates are first-class
workspace members and are NOT tracking upstream wezterm for sync.
Modifications are owned by the FrankenTerm project; upstream
compatibility is explicitly not maintained.

## Per-crate classification (ft-zoxxq.5)

Columns:

- **dir** — directory name under `frankenterm/`
- **package** — `name = ` field in `Cargo.toml`
- **origin** — `wezterm` (vendored from upstream wezterm), `own` (home-grown
  in this repo), or `wezterm?` (generic-name utility likely vendored but
  not directly attributed in the crate's own Cargo.toml)
- **commits** — count of commits in this repo touching files under the
  crate dir (proxy for modification depth; `git log --oneline -- frankenterm/<dir>`)
- **classification** — derived from origin + commits:
  - `vendored-as-is` — origin=wezterm, ≤5 commits
  - `forked-light` — origin=wezterm, 6–20 commits
  - `forked-modified` — origin=wezterm, 21–50 commits
  - `forked-heavy` — origin=wezterm, >50 commits
  - `home-grown` — origin=own
  - `unknown-vendored` — origin=wezterm?

| dir                  | package                          | origin    | commits | classification     |
| -------------------- | -------------------------------- | --------- | ------- | ------------------ |
| async_ossl           | async_ossl                       | wezterm   | 7       | forked-light       |
| base91               | base91                           | wezterm?  | 10      | unknown-vendored   |
| bidi                 | frankenterm-bidi                 | wezterm   | 9       | forked-light       |
| bintree              | bintree                          | wezterm   | 10      | forked-light       |
| blob-leases          | frankenterm-blob-leases          | wezterm   | 14      | forked-light       |
| cell                 | frankenterm-cell                 | wezterm   | 11      | forked-light       |
| char-props           | frankenterm-char-props           | wezterm   | 12      | forked-light       |
| client               | wezterm-client                   | wezterm   | 25      | forked-modified    |
| codec                | codec                            | wezterm   | 82      | forked-heavy       |
| color-types          | frankenterm-color-types          | wezterm   | 12      | forked-light       |
| config               | config                           | wezterm   | 64      | forked-heavy       |
| deps-fontconfig      | fontconfig                       | wezterm   | 1       | vendored-as-is     |
| deps-freetype        | freetype                         | wezterm   | 4       | vendored-as-is     |
| deps-harfbuzz        | harfbuzz                         | wezterm   | 6       | forked-light       |
| dynamic              | frankenterm-dynamic              | wezterm   | 7       | forked-light       |
| env-bootstrap        | env-bootstrap                    | wezterm   | 2       | vendored-as-is     |
| escape-parser        | frankenterm-escape-parser        | wezterm   | 35      | forked-modified    |
| filedescriptor       | filedescriptor                   | wezterm   | 10      | forked-light       |
| font                 | wezterm-font                     | wezterm   | 9       | forked-light       |
| frecency             | frecency                         | wezterm?  | 3       | unknown-vendored   |
| gui-subcommands      | frankenterm-gui-subcommands      | own       | 2       | home-grown         |
| input-types          | frankenterm-input-types          | wezterm   | 12      | forked-light       |
| lfucache             | lfucache                         | wezterm?  | 5       | unknown-vendored   |
| lua-api-crates       | (workspace member)               | wezterm   | 16      | forked-light       |
| luahelper            | luahelper                        | wezterm   | 11      | forked-light       |
| mux                  | mux                              | wezterm   | 112     | forked-heavy       |
| open-url             | wezterm-open-url                 | wezterm   | 6       | forked-light       |
| procinfo             | procinfo                         | wezterm   | 10      | forked-light       |
| promise              | promise                          | wezterm   | 15      | forked-light       |
| pty                  | portable-pty                     | wezterm   | 19      | forked-light       |
| rangeset             | rangeset                         | wezterm   | 11      | forked-light       |
| ratelim              | ratelim                          | wezterm?  | 3       | unknown-vendored   |
| scripting            | frankenterm-scripting            | own       | 17      | home-grown         |
| ssh                  | frankenterm-ssh                  | wezterm   | 44      | forked-modified    |
| surface              | frankenterm-surface              | wezterm   | 36      | forked-modified    |
| tabout               | tabout                           | wezterm   | 4       | vendored-as-is     |
| term                 | frankenterm-term                 | wezterm   | 78      | forked-heavy       |
| termwiz              | termwiz                          | wezterm   | 29      | forked-modified    |
| toast-notification   | wezterm-toast-notification       | wezterm   | 8       | forked-light       |
| uds                  | frankenterm-uds                  | wezterm   | 15      | forked-light       |
| umask                | umask                            | wezterm   | 11      | forked-light       |
| vtparse              | vtparse                          | wezterm   | 11      | forked-light       |
| window               | window                           | wezterm   | 25      | forked-modified    |

42 crates total (excluding the `assets/` directory, which is not a crate).

## Notes on classification

- **`wezterm?` rows** — `frecency`, `lfucache`, `ratelim`, `base91`. These
  have generic package names with no `repository` or `homepage` field in
  their `Cargo.toml`. The directory layout matches the upstream wezterm
  workspace and the historical record places them alongside the original
  29-crate import; we believe they are vendored from wezterm but cannot
  confirm without diffing against an upstream checkout. Classified as
  `unknown-vendored` until that diff lands as a follow-up bead.
- **`mux` (112 commits)** — the deepest-forked crate. Most
  FrankenTerm-specific functionality (cancel-correctness, Cx threading,
  asupersync integration, durable-state hooks, recorder integration) is
  wired in here. Treat this as a FrankenTerm-owned crate for all intents.
- **`codec` (82 commits)** — second deepest. The varbincode
  positional-format guard (ft-e1emx, see `codec/src/lib.rs:1204-1228`)
  and async asupersync wiring originate here.
- **`term` (78 commits)** — the virtual-terminal-emulator core; heavily
  modified for FrankenTerm's recorder and replay paths.
- **`config` (64 commits)** — heavily modified primarily because the
  FrankenTerm config schema has diverged from upstream wezterm's
  Lua-based config model. Lua removal lives in this crate and
  `lua-api-crates` / `luahelper`.
- **`lua-api-crates`** — workspace member; the inner crates are
  per-Lua-API-surface stubs. The package field is omitted from the
  top-level `Cargo.toml`. Classified `forked-light` because the
  directory structure is preserved but most Lua APIs are
  stubbed/disabled.
- **Home-grown crates** — `gui-subcommands` and `scripting` are
  FrankenTerm-original. They are not derived from wezterm sources.

## How to refresh this table

```bash
cd /path/to/frankenterm/frankenterm
for d in */; do
  d="${d%/}"; [[ "$d" == "assets" ]] && continue
  pkg="$(awk -F'"' '/^name *=/{print $2; exit}' "$d/Cargo.toml" 2>/dev/null)"
  count="$(cd .. && git log --oneline -- "frankenterm/$d" 2>/dev/null | wc -l | tr -d ' ')"
  printf "%-22s %-32s %s\n" "$d" "${pkg:-?}" "$count"
done
```

The `commits` column is a snapshot at the time of the last edit; it
grows over time. Re-run when the audit needs to be refreshed (e.g.,
before a release, or when upstream wezterm needs a re-sync evaluation).

## Ownership posture

This code is owned by the FrankenTerm project. Per ft-zoxxq stance (b),
upstream wezterm compatibility is **explicitly not maintained**.
Modifications listed under any non-`vendored-as-is` row are intentional
and the crate is not expected to merge cleanly with upstream. The four
`vendored-as-is` rows (`deps-fontconfig`, `deps-freetype`, `tabout`,
`env-bootstrap`) are candidates for future re-sync if upstream ships a
relevant fix; the others have diverged enough that re-sync is a manual
patch operation, not a merge.
