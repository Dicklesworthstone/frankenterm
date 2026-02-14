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
