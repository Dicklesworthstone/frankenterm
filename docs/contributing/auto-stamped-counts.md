# Auto-Stamped Counts in README and AGENTS

**Bead:** `ft-tf6g3.2` (G17, extending `ft-i2eni.5` / BR-RC-DOCTRINE.G6) · **Stamper:** `scripts/stamp-readme-counts.sh` · **CI:** drift check on every PR.

## Why this exists

Hand-edited workspace counts in `README.md` and `AGENTS.md` drift fast.
ft-d3awp and ft-hdvvo were both filed because a count claim that was
correct when written had become wrong by the time anyone re-read the
sentence around it. The doctrine epic (`ft-i2eni`) takes the position
that **public credibility commitments must not silently lie**, so we
moved the drift-prone counts behind a placeholder convention that a
build-time stamper keeps current.

## Placeholder convention

A tracked count lives between an HTML-comment open/close pair:

```markdown
… <!--count:NAME-->VALUE<!--/count--> …
```

- **HTML comments** keep the marker invisible in rendered Markdown.
- **`NAME`** is one of the manifest keys defined in
  `scripts/stamp-readme-counts.sh` (e.g. `workspace_members`,
  `core_top_level_modules`, `test_count`, `criterion_bench_files`).
- **`VALUE`** is whatever the stamper most-recently wrote. It is the
  value a reader sees in the rendered doc, so prose can wrap it
  ("…with `<!--count:workspace_members-->68<!--/count-->` workspace
  crates…").
- **Multiple placeholders** with the same `NAME` in the same file are
  allowed. Rewrite, check, strict-check, and JSON modes inspect every
  occurrence so a stale repeated footer cannot hide behind an up-to-date
  first match.

## Authoring a new tracked count

1. Add a row to the `MANIFEST` array near the top of
   `scripts/stamp-readme-counts.sh`:
   ```
   "my_count_name|<shell command that prints one integer>"
   ```
   The command must produce an integer to stdout. Whitespace is trimmed.
2. Insert the placeholder block in `README.md` and/or `AGENTS.md`:
   ```markdown
   …<!--count:my_count_name-->0<!--/count-->…
   ```
3. Run the stamper to bake in the live value:
   ```bash
   bash scripts/stamp-readme-counts.sh
   ```
4. Commit the manifest entry, the placeholder, and the stamped value
   together so a reviewer can audit the full surface in one diff.

## Running the stamper

```bash
# Rewrite mode: update placeholder values to live counts (default).
bash scripts/stamp-readme-counts.sh

# Advisory check: fail if any placeholder drifts > 5% from live.
bash scripts/stamp-readme-counts.sh --check

# Strict check: fail on ANY discrepancy (no drift tolerance).
bash scripts/stamp-readme-counts.sh --check --strict

# Custom drift threshold.
bash scripts/stamp-readme-counts.sh --check --threshold=10

# Machine-readable attestation snapshot.
bash scripts/stamp-readme-counts.sh --json > docs/attestations/doctrine/agents-md-counts.json
```

CI runs `--check` (5% threshold) on every PR. Failure prints
the offending placeholder occurrence with its documented vs. live
values and the calculated drift percentage. Re-run the stamper without
`--check` to fix the doc, then commit alongside the workspace change
that moved the count.

## Tracked counts (current manifest)

| Name | Command |
| --- | --- |
| `workspace_members` | total workspace members in `Cargo.toml` |
| `vendored_members` | workspace members under `frankenterm/` |
| `vendored_top_level` | `find frankenterm -maxdepth 2 -name Cargo.toml` count |
| `core_subcrates` | `crates/frankenterm-core-*` directory count |
| `core_top_level_modules` | `*.rs` files in `crates/frankenterm-core/src/` (depth 1) |
| `core_loc` | summed line count of every `.rs` under `crates/frankenterm-core/src/` |
| `test_count` | `#[test]` / `#[tokio::test]` / `#[asupersync_test::test]` annotations across `crates/` |
| `core_rust_test_files` | Rust test files under `crates/frankenterm-core/tests/` |
| `criterion_bench_files` | Criterion bench files under `crates/frankenterm-core/benches/` |
| `fuzz_targets` | cargo-fuzz target files under `fuzz/**/fuzz_targets/` |
| `doc_markdown_files` | Markdown documentation files under `docs/` |
| `e2e_scripts` | tracked shell E2E scripts under `tests/e2e/` |

The list is intentionally short. Counts that are stable on the order
of years (e.g. major version numbers) don't belong here; counts that
move with every workspace change do.

## Why advisory, not strict

The drift threshold is 5% by design — workspace counts move by 1–3
between many PRs, and a strict gate would force every minor refactor
to also bump the doc. The advisory mode catches the *gross* drift
that the ft-d3awp / ft-hdvvo incidents flagged (10 %+ off, occasionally
30 %+) without making the doc-rewrite churny. A future bead can
graduate to `--strict` once the placeholder corpus is comprehensive
enough that the per-PR doc churn would be 0 in the steady state.
