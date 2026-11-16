# Antigravity Session Resume E2E

`scripts/e2e_antigravity_session_resume.sh` is the retained e2e harness for the
Antigravity (`agy`) and legacy Gemini CLI (`gmi`) session-resume contract.

Run it directly:

```bash
bash scripts/e2e_antigravity_session_resume.sh
```

The direct harness builds a minimal public `ft` binary with
`--no-default-features --features session-resume`, then exercises:

```bash
ft robot --format json session-resume list --provider agy --home <fixture-home>
ft robot --format json session-resume list --provider gmi --home <fixture-home>
ft robot --format json session-resume resume <uuid> --provider agy --dry-run --home <fixture-home>
ft robot --format json session-resume resume <legacy-id> --provider gmi --dry-run --home <fixture-home>
```

Run the RCH-friendly public robot wrapper:

```bash
cargo test -p frankenterm --no-default-features --features session-resume \
  --test antigravity_session_resume_robot_e2e -- --nocapture
```

That wrapper drives the compiled `ft` binary through the same public
`ft robot --format json session-resume list|resume` commands as the shell
harness. It appends retained JSONL records and stdout/stderr artifacts under
`target/e2e-logs/antigravity-session-resume/robot-wrapper-*`.

Run the RCH-friendly core bridge wrapper:

```bash
cargo test -p frankenterm-core --features session-resume \
  --test e2e_antigravity_session_resume_script -- --nocapture
```

Run the RCH-friendly formatting wrapper for Rust files owned by this lane:

```bash
cargo test -p frankenterm-core --test antigravity_format_proof -- --nocapture
```

That wrapper exists because `rch` rejects direct `cargo fmt --check` as a
non-compilation command. It invokes `rustfmt --edition 2024 --check` from inside
an integration test over the Antigravity-owned Rust files, which gives the final
proof lane a remote-admissible formatting signal without applying broad
workspace formatting to unrelated dirty files.

The script creates isolated fixture homes under `target/e2e-logs/` and never
reads or mutates the real `~/.gemini`. Each run writes retained artifacts:

- `manifest.json`: scenario roots, fake binaries, and expected argv.
- `antigravity-session-resume.jsonl`: per-step retained log records.
- `public-surface/stdout/*.json`: public robot command JSON envelopes.
- `public-surface/stderr/*.log`: public robot command stderr/cargo diagnostics.
- `rust-validation-summary.json`: cargo-wrapper validation summary.
- fake `agy`/`casr` stdout, stderr, and argv logs per scenario.

Scenario coverage:

- `agy-only`: discovers one `~/.gemini/antigravity-cli/conversations/<uuid>.db`.
- `legacy-gmi-only`: preserves legacy `~/.gemini/tmp/<hash>/chats/session-*.json`.
- `mixed`: proves the agy and gmi roots do not cross-list each other.
- `malformed-irrelevant`: ignores non-`.db`, directory, and non-UUID `.db` entries.
- `missing-agy-binary`: still returns the exact model-pinned dry-run plan without
  claiming that a provider process was executed.
- every non-dry native resume fails closed with `robot.feature_not_available`
  until the owned mux-PTY execution path is implemented; the harness never
  mistakes captured subprocess output for a usable interactive session.
- `optional-real-smoke`: opt in with `FT_AGY_E2E_REAL_HOME=/path/to/home` for read-only discovery.

The public robot surface is the user-level contract. The core cargo wrapper is
kept as an additional bridge-level regression check, not as a substitute for the
CLI/Robot path.
