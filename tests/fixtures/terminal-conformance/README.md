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

Proof command:

```bash
RCH_REQUIRE_REMOTE=1 rch exec -- env CARGO_TARGET_DIR=/tmp/ft-hme39.3-terminal-conformance cargo test -p frankenterm-escape-parser --test terminal_conformance_corpus -- --nocapture
```
