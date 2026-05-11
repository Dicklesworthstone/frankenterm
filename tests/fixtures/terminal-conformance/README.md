# Terminal Conformance Fixtures

This corpus is owned by `ft-hme39.3` and is consumed by
`frankenterm/escape-parser/tests/terminal_conformance_corpus.rs`.

Each scenario has:

- a manifest row in `manifest.json`;
- one ASCII hex transcript under `transcripts/`;
- one expected semantic artifact under `expected/`.

The transcript files are hex-encoded to keep control bytes reviewable in normal
diffs. The consumer test decodes the bytes, runs the real escape parser, and
checks the expected parser-visible actions. Scenario IDs are stable and must
appear in assertion failures.

Minimized failing transcripts live under `minimized/` and are listed by
`manifest.json` in `minimized_cases`. These are quarantine/provenance fixtures,
not passing terminal behavior scenarios. Each minimized metadata file records
the scenario id, original artifact path, reduced input artifact, preserved
failure signature, ordered minimization steps, quarantine reason, follow-up bead,
promotion condition, residual risk, and redaction status.

Use the manual reduction workflow from `docs/terminal-conformance-contract.md`:
remove unrelated bytes one step at a time, rerun the narrow assertion after each
change, and keep only reductions that preserve the same failure signature.
Quarantine is allowed only with an explicit reason and follow-up bead. To promote
a minimized case into the main corpus, add a passing expected artifact under
`expected/`, move the scenario into `manifest.scenarios`, and run the RCH proof
that consumes the promoted fixture.

Proof command:

```bash
RCH_REQUIRE_REMOTE=1 rch exec -- env CARGO_TARGET_DIR=/tmp/ft-hme39.3-terminal-conformance cargo test -p frankenterm-escape-parser --test terminal_conformance_corpus -- --nocapture
```
