# Resize Baseline Scenario Suite

Bead: `wa-1u90p.1.1`

This document defines the canonical deterministic scenario pack for worst-case resize/font-change reproduction across pane, tab, and scrollback scales.

## Location

- `fixtures/simulations/resize_baseline/resize_single_pane_scrollback.yaml`
- `fixtures/simulations/resize_baseline/resize_multi_tab_storm.yaml`
- `fixtures/simulations/resize_baseline/font_churn_multi_pane.yaml`
- `fixtures/simulations/resize_baseline/mixed_scale_soak.yaml`

## Metadata Contract

Each scenario includes `metadata` keys used for reproducibility and longitudinal comparison:

- `suite`: fixed to `resize_baseline`
- `suite_version`: scenario-pack revision
- `seed`: deterministic generation seed
- `scale_profile`: scenario family
- `pane_count`, `tab_count`, `scrollback_lines`, `font_steps`: declared workload axes

`ft simulate run --json` and `ft simulate validate --json` now emit `metadata` and `reproducibility_key`.

## Event Contract

Additional simulation actions used by this suite:

- `set_font_size`: records deterministic font-size transition markers
- `generate_scrollback`: synthesizes deterministic scrollback (`LINES` or `LINESxWIDTH`)

The mock simulation runtime encodes these as append markers/content so expectations and timeline replay remain deterministic.

## Resize Timeline Instrumentation

`Scenario` now exposes stage-level resize timeline probes for baseline attribution:

- `execute_all_with_resize_timeline`
- `execute_until_with_resize_timeline`

Each resize-class event (`resize`, `set_font_size`, `generate_scrollback`) emits ordered stage samples:

1. `input_intent`
2. `scheduler_queueing` (includes queue depth before/after)
3. `logical_reflow`
4. `render_prep`
5. `presentation`

The timeline artifact includes nanosecond stage durations, per-event structured records, stage summaries (`p95`, `max`, `avg`), and flamegraph-ready rows via `flame_samples()`.

## How To Run

```bash
ft simulate list

ft simulate validate fixtures/simulations/resize_baseline/resize_single_pane_scrollback.yaml
ft simulate run fixtures/simulations/resize_baseline/resize_single_pane_scrollback.yaml --json
```

## Coverage

Automated integration coverage lives in:

- `crates/frankenterm-core/tests/simulation_resize_suite.rs`

That test loads every suite file, validates metadata/reproducibility keys, executes all events in `MockWezterm`, and asserts all `contains` expectations.
