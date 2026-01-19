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
