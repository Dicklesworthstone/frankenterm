# wa-1u90p.3.11: Monospace Knuth-Plass Cost Model and Bounds

## Goal
Define the scoring contract for resize-time monospace Knuth-Plass (KP) wrapping before integrating the bounded DP engine (`wa-1u90p.3.12`).

This spec is intentionally constrained for terminal workloads:
- deterministic output for identical inputs,
- bounded runtime under resize storms,
- graceful fallback to existing greedy behavior when budgets are exceeded.

## User-Visible Readability Goals

| Model term | User-visible behavior |
| --- | --- |
| Cubic slack badness | Strongly discourages very ragged non-last lines; small slack differences stay low-impact. |
| Last-line zero badness | Avoids over-optimizing paragraph tail; prevents unnatural re-breaks late in a line. |
| Forced-break penalty | Avoids splitting long tokens unless necessary; preserves code/log token integrity when possible. |
| Max-line-badness tie-break | Prefers more even visual raggedness over a single very bad line. |
| Deterministic lexical break-offset tie-break | Stable wraps across runs; no flicker from nondeterministic equal-cost choices. |

## Cost Model (Monospace)

Let:
- `target_width`: terminal columns for wrapping,
- `line_width`: measured columns for a candidate line,
- `slack = target_width - line_width`.

For non-last lines:

`badness = (slack^3 * badness_scale) / target_width^3`

with defaults:
- `badness_scale = 10_000`
- `forced_break_penalty = 5_000`
- `overflow => KP_BADNESS_INF`
- `last line => 0` (TeX-style convention)

### Monotonicity (proof sketch)
For fixed positive `target_width`, the function `f(slack) = slack^3` is monotone non-decreasing for `slack >= 0`. Multiplying by a positive constant and dividing by a positive constant preserves monotonicity. Therefore line badness is monotone non-decreasing in slack for feasible non-last lines.

## Deterministic Tie-Break Order

When comparing candidate break plans, sort by:
1. lower `total_cost`
2. fewer `forced_breaks`
3. lower `max_line_badness`
4. fewer `line_count`
5. lexicographically smaller `break_offsets` (final deterministic tie-break)

This guarantees strict deterministic ordering even when high-level metrics tie.

## Bounded Complexity Contract

### DP transition bound
Given token count `n` and lookahead cap `L`:

`states_estimate = n * min(n, L)`

Defaults:
- `lookahead_limit = 64`
- `max_dp_states = 8_192`

### Fallback trigger thresholds
Use deterministic greedy fallback when:
- `states_estimate > max_dp_states`, or
- implementation-specific runtime budget is exceeded in `wa-1u90p.3.12` (to be instrumented).

Fallback must preserve current `Line::wrap` semantics and wrapped-cell metadata behavior.

## Code Anchors

- Cost model contract types/functions:
  - `frankenterm/surface/src/line/line.rs`
  - `MonospaceKpCostModel`
  - `compare_monospace_break_candidates`
- Unit evidence:
  - `kp_cost_model_badness_is_monotonic_for_non_last_lines`
  - `kp_cost_model_enforces_bounded_state_budget`
  - `kp_candidate_tiebreak_is_deterministic_on_fixed_corpora`

## Notes for wa-1u90p.3.12

- Reuse this comparator exactly to avoid ranking drift between spec and implementation.
- Emit per-line mode (`dp` vs `fallback`) and timing counters for SLO evidence collection.
- Keep fallback deterministic and side-effect-equivalent to current greedy wrapping.
