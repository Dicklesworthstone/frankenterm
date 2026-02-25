# Pattern Corpus Fixtures

This folder contains golden corpus fixtures for the pattern engine.

Each fixture is a pair of files:
- <name>.txt: input text
- <name>.expect.json: expected detections (JSON array)

How to add a fixture
1) Capture real output (redacted if needed).
2) Save it as tests/corpus/<agent>/<name>.txt.
3) Run the corpus test to see the diff.
4) Update the rule/pack or expected JSON until green.

Guidelines
- Keep inputs small and focused.
- Prefer one scenario per file unless a combined scenario is clearer.
- Avoid secrets in fixtures.

Dogfood metadata
- Dogfood fixtures (filenames containing `_dogfood_`) must include a companion `<name>.meta.json`.
- Required metadata keys: `scenario`, `source`, `captured_at`, `platform`, `cross_platform`, `sanitized`.
- `platform` must be `macos` or `linux`.
- `cross_platform`:
  - `pending`: only one platform fixture exists for that scenario.
  - `complete`: both `macos` and `linux` fixtures exist for that scenario.
- `sanitized` must be `true` before committing.

Live capture workflow (`ft-nu4.3.9.5`)
1) Run the bead E2E gate:
   - `tests/e2e/test_ft_nu4_3_9_5.sh`
2) The gate enforces:
   - `rch` worker availability before any cargo invocation.
   - `rch`-only execution for `cargo test -p frankenterm-core --test pattern_corpus`.
   - live capture prerequisites (`ft`, `wezterm`, reachable mux, and an active pane).
3) If prerequisites are missing, the script fails fast and writes a structured JSONL reason code in `tests/e2e/logs/`.
4) If capture succeeds, promote the captured text into a sanitized dogfood fixture and add/update:
   - `<name>.txt`
   - `<name>.expect.json`
   - `<name>.meta.json`
5) Update `cross_platform` state:
   - keep `pending` until both `macos` and `linux` captures exist for the same `scenario`.
   - switch to `complete` only after both platform fixtures are present and validated by `pattern_corpus`.

Example metadata:
```json
{
  "scenario": "codex_usage_reached",
  "source": "ft_robot_capture",
  "captured_at": "2026-02-14T20:00:00Z",
  "platform": "macos",
  "cross_platform": "pending",
  "sanitized": true
}
```
