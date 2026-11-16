# scratch/

Throwaway Rust reproductions and audit probes that don't belong in a
crate's `tests/` tree. These files are standalone — they are NOT
compiled as part of the workspace. Run them directly, e.g.:

```bash
rustc scratch/test_bug.rs -L target/debug/deps --edition 2024 -o /tmp/tb && /tmp/tb
```

Or adapt the snippet into a proper `#[test]` in the relevant crate when
the reproduction matures into a regression fence.

## What lives here

| File | Purpose |
|------|---------|
| `test_bug.rs` | Spot-check for `frankenterm_core::policy::is_command_candidate` against `terraform destroy` / `helm uninstall` strings |
| `test_is_cmd.rs` | Regex behaviour probe — env-assignment-then-command detection (`FOO=bar; rm -rf /`) |
| `test_safe.rs` | Spot-check for `frankenterm_core::command_guard::evaluate_stateless` on compound rm invocations |

## Policy

- Anything in `scratch/*.rs` is considered throwaway. Do not import from
  here in production code; do not add bench/test harnesses against it.
- `scratch/` itself is tracked so audit reproductions survive across
  branches, but ad-hoc byproducts at the repo root (`test_*.rs`,
  `ubs_*.txt`, `storage.sqlite3*`, etc.) are `.gitignore`d — move them
  here explicitly if they're worth keeping.
- When a scratch file's concern lands as a proper test, delete the
  scratch version in the same commit so the row above stays accurate.

Tracked for `ft-j3ayu` — see `AGENTS.md` "Workspace Structure" for how
this fits with the rest of the repo.
