# Codec versions

Single source of truth for `CODEC_VERSION` history.

The CI guard `scripts/check_codec_version_release_notes.sh` (track A
of ft-kuxho, ft-8smkj) reads the current `CODEC_VERSION` constant from
`frankenterm/codec/src/lib.rs:650` and **fails CI** if no row in this
file documents that version. Bumping `CODEC_VERSION` without adding a
row here is a silent protocol change and is rejected at CI time.

When you bump `CODEC_VERSION` in `frankenterm/codec/src/lib.rs`, add a
row at the top of the table below in the same commit. Each row records:

- **version** — the new `CODEC_VERSION` value (matches the constant exactly)
- **date** — `YYYY-MM-DD` of the commit that bumped the version
- **kind** — `additive` (rolling upgrade safe per ft-kuxho/B) or `breaking`
  (atomic redeploy required; bumps `CODEC_VERSION_MIN_SUPPORTED` once
  ft-kuxho.B.1 lands)
- **change** — short summary; reference the PDU id(s) and the bead/commit

Future-proofing: when `CODEC_VERSION_MIN_SUPPORTED` lands (ft-kuxho.B.1),
the same guard will be extended to require a row whenever `MIN`
advances. Until then, every `CODEC_VERSION` bump implicitly raises the
minimum to itself (atomic-redeploy semantics).

## History

| version | date       | kind     | change |
| ------- | ---------- | -------- | ------ |
| 46      | 2026-02-10 | initial  | starting value at fork import from wezterm @ `05343b387085842b434d267f91b6b0ec157e4331`. See `frankenterm/PROVENANCE.md`. |
