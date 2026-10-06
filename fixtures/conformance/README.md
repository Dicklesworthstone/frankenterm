# Terminal conformance fixtures (ft-yccm0.1.9)

The headless conformance runner, `frankenterm/term/tests/conformance.rs`,
checks frankenterm-term against these expectations.

## Layout

- `vttest/*.vtrec`: real vttest 2.7 (20251205) sessions. `vttest/capture_vttest.py`
  drove vttest through a pty and recorded what it wrote after each key. The
  runner replays them screen by screen.
- `vttest/goldens/<session>.screens.txt` and `<session>.attrs.json`: the
  expected screen after every recorded screen. The text file holds the
  cursor, DECSCNM and the rows; the JSON holds attribute runs. The format is
  documented in `frankenterm/term/tests/conformance/dump.rs`.
- `xfail.txt`: cases the legacy engine is known to fail, each with its
  reason.
- `baseline/legacy.txt`: the legacy engine's status for every case.

The esctest-style cases are Rust code in
`frankenterm/term/tests/conformance/cases.rs`. Their expectations come from
the xterm control-sequence documentation, the DEC VT510 manual and Unicode
section 3.9, never from an engine's output.

## Recordings

The driver ran `vttest 24x80.80` (frankenterm-term has no 132-column mode)
with `TERM=vt100` and `LC_ALL=C`. It answered vttest's DA1, DA2 and DECSCL
queries with the exact bytes frankenterm-term sends. Recapture only for a
new vttest version or a new session, and give the reason in the commit body.
Sessions that are left out, and why, are listed in the driver.

## How the goldens were made

The legacy engine's dumps were reviewed screen by screen against what each
vttest screen says it should show. They were also cross-checked with the
bytes vttest sent.

Where legacy is wrong, the golden was corrected by hand and the screen is
listed in `xfail.txt`. The corrected screens are named at the top of each
`.screens.txt`. So a golden states correct behavior, not just current
behavior.

## Running

Each run prints a PASS / FAIL / XFAIL / XPASS line per case, with reasons,
and writes the report to `$FT_CONFORMANCE_OUT` (default: the test temp dir).

- **Legacy engine.** Must match `baseline/legacy.txt` exactly:
  `cargo test -p frankenterm-term --test conformance -- --nocapture`
- **A parser candidate** registered in
  `tests/differential/engine.rs::candidates`:
  `FT_CONFORMANCE_ENGINE=mux_two_stage cargo test -p frankenterm-term --test conformance`
- **The page grid**, once ft-yccm0.3.3.4 reads it in `Terminal::new`:
  `FT_GRID_ENGINE=page cargo test -p frankenterm-term --test conformance`

A candidate run fails only on regressions, meaning a case that passes in the
baseline now fails. Cases a candidate newly passes are reported as
improvements.

`FT_CONFORMANCE_PRINT=1` also prints the engine's dumps in golden-file form,
plus its baseline, for review.

## Changing expectations

A golden, baseline or xfail change needs a recorded reason in the commit
body. Never regenerate goldens to make a run pass.

When an engine fix makes an xfail pass, the legacy run reports XPASS. Remove
the entry, flip its baseline line to PASS, and say what was fixed.
