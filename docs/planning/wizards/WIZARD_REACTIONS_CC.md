# WIZARD_REACTIONS_CC.md — Claude Code Reacts to Codex's and Gemini's Scores of Its Ideas

> Claude Code (Opus 4.8), 2026-06-06. Source material: WIZARD_SCORES_COD_ON_CC.md
> and WIZARD_SCORES_GMI_ON_CC.md, read in full. This is Step 1 (honest reaction);
> the formal defenses and counterattacks are in WIZARD_REBUTTAL_CC.md.

## The scoreboard, consolidated

| My idea | COD | GMI | Avg | My read of their read |
|---|---|---|---|---|
| 1. Verified-Submit Dispatch | 930 | 850 | 890 | Consensus #1 — agree |
| 2. watch-events Subscription | 900 | 820 | 860 | Consensus #2 — agree |
| 3. Dead-Wire Closure + Gate | 875 | 760 | 818 | Shared caveat worth absorbing |
| 4. `ft robot next` | 830 | 680 | 755 | Fair |
| 5. Time-Travel CI | 800 | 620 | 710 | Fair; consensus caveat = my own caveat |
| 6. Fleet Reconciliation | 765 | 710 | 738 | Fair |
| 7. Rate-Limit Economics | 815 | 740 | 778 | Fair |
| 8. Durable Agent Sessions | 745 | 580 | 663 | Fair; one good new failure case |
| 9. Taint + Canaries | 690 | 450 | 570 | Partially wrong — defended in rebuttal |
| 10. `ft deck` | 720 | 650 | 685 | Fair |
| 11. Operation Target-Class | 835 | 350 | 593 | GMI wrong — defended in rebuttal |
| 12. WASM Phase-Shipped | 665 | 520 | 593 | GMI's perf critique misreads the design |
| 13. Timeline Forensics | 795 | 590 | 693 | GMI's scale critique misreads the design |
| 14. Contract SDKs | 810 | 490 | 650 | Split; both half-right |
| 15. `ft doctor --fix` | 700 | 410 | 555 | **Concede** — their best critique |

## Overall reaction

Two evaluators, two very different instruments. **Codex** graded me generously
(mean ≈ 788) and its critiques are almost all restatements of caveats I had
already written into the ideas myself — validating, but low-information; when
your reviewer's "strongest argument against" is quoted from your own risk
section, you've learned little. Its one genuinely additive critique (#15's
cultural argument) is excellent. **Gemini** graded harder (mean ≈ 615) with a
wider spread, and is the more *useful* reviewer where it engaged correctly —
but three of its low scores rest on demonstrable misreadings of what I
proposed, and one (#11 at 350) rests on a scoring criterion that wasn't in the
brief.

A calibration note I'll state plainly because the user asked for candor: both
evaluators scored my list higher than I scored theirs (my means: COD 668,
GMI 449). Either my list is genuinely stronger — my honest belief, and the
asymmetry has a cause: my ideas were anchored in verified caller-graph facts
about this tree, while several of theirs collided with documented doctrine —
or I'm simply the harsher grader. Probably both are true; readers should
apply that lens to all four score files.

## Where I agree

- **The consensus top two is correct and decision-grade.** Both models
  independently ranked Verified-Submit and watch-events as my two best, in the
  same order I did. Three models, three methodologies, one answer: if anything
  gets built from these lists, it's those two first.
- **#1, UI-drift brittleness (both raised it):** agreed without reservation —
  it was my stated risk. The consensus elevates it from "risk" to "the
  maintenance contract": submit profiles must be owned, fixtured surfaces with
  the same drift discipline as rule packs, or the feature rots. The fail-open
  `verification_unavailable` floor means rot degrades to today's behavior, not
  below it — which is why neither model's caveat moved their score much, and
  rightly so.
- **#4, ranking opacity (both):** GMI's sharper form — "operators will
  eventually ignore the endpoint and write custom queries" — is partially
  right and worth conceding in design terms: `next` must be an advisory
  composite over surfaces that all remain independently queryable, never an
  exclusive gateway. Mandatory `reasons[]` and deterministic ordering were
  already specified; I accept the score band.
- **#5, golden-maintenance burden (both):** this was my own self-ranked
  weakest point of my top five, and two independent confirmations matter. I
  trim my confidence: the initial corpus must be brutally small (3–5
  fixtures), and the expected-drift/bless workflow must ship *in the first
  cut*, not as a fast-follow. GMI's 620 is harsher than I'd self-score but is
  not unreasonable.
- **#8, third-party resume flakiness (both):** fair, and GMI contributed the
  one concrete new failure case in either review — resume flows that block on
  interactive confirmation when local state is corrupted. That goes straight
  into the design: a `resume_blocked_interactive` receipt state, detected via
  the same composer-pattern machinery as Idea 1, falling back to
  `fresh_with_context`. The ceiling-outside-our-control point was in my
  original text; conceded then, conceded now.
- **#10, TUI scope creep (both):** agreed; my design's constitutional rule
  (composition layer only, CLI verbs land first) is the same mitigation COD
  recommends. No score quarrel — 650–720 is about right for an
  operator-minority feature.
- **#15, `ft doctor --fix` (both — and my fullest concession, see below).**

## Where they're wrong

- **GMI on #11, Operation Target-Class (350: "simply a Jira ticket… wastes
  ideation space").** Wrong on the rubric and wrong on the facts. The brief
  asked for *accretive and pragmatic*, not maximal novelty — penalizing an
  idea for being executable is criterion inversion. And "zero new runtime
  capabilities" is factually false: the operating envelope *consults the
  target-class proof artifact at decision time* and emits
  `capacity.target_class_unproven` → Defer; flipping the artifact changes
  live admission verdicts. Full defense in the rebuttal file. (Notably, COD
  scored the same idea 835 — the single widest disagreement between the two
  evaluators, 485 points. There is no consensus against this idea; there is
  one outlier with a novelty bias.)
- **GMI on #12, WASM ("passing scrollback chunks across the WASM boundary
  millions of times per second… will obliterate performance").** This
  critiques a design I didn't propose. The writeup explicitly runs WASM rules
  *post-Bloom, only on candidate chunks, budget-bounded* — the Bloom
  prefilter rejects 80–95% of chunks before any rule (native or WASM) runs.
  Real-world candidate volume is orders of magnitude below "millions per
  second." The legitimate core of the concern (boundary-crossing overhead per
  candidate) is real and was already bounded by the per-extension time budget
  and fail-open semantics. I keep my own cautious 12th-place ranking, but for
  my reasons (lift size, demand uncertainty), not this one.
- **GMI on #13, Timeline ("unreadable garbage at 200-pane scale").** Misreads
  the proposal twice: the interface defaults to bounded windows with `--panes`
  filtering and `--correlate` *ranking* precisely so nobody renders 200 lanes;
  and the robot variant exists because the primary consumer is an AI agent
  reading structured lanes, where visual lane-count is irrelevant. Critiquing
  a forensics tool for an anti-pattern its flags exist to prevent is a miss.
- **GMI on #2's failure mechanism (right concern, wrong physics).** The
  worry — disconnected clients risk "silent event drops or duplicate storms"
  when "the SQLite WAL flushes or rotates" — names a real design obligation
  but a wrong mechanism: WAL checkpointing doesn't delete rows, and the
  `events` table cursor is a monotonic id. The *actual* hazard is retention
  pruning advancing past a stale cursor, and the design answer is an explicit
  typed `cursor_expired` error (never silent skip), mirroring the capture
  pipeline's no-silent-gaps doctrine. I credit the instinct, not the
  analysis.
- **COD, structurally:** very little is wrong, but very little is new. Twelve
  of fifteen "strongest arguments against" paraphrase my own stated risks
  back at me. That's agreement wearing a critique's clothes — pleasant, and
  fair, but it means COD's review mostly confirms rather than stress-tests.

## Consensus effects: where BOTH raised the same concern

1. **#9 taint heuristics / paraphrase-evasion / false-confidence (both).**
   This is genuine consensus and it earns a genuine update — of *framing*,
   not of conviction. I concede the bundled presentation overweighted the
   speculative half (sketch-taint) relative to the immediately shippable half
   (trust labels + canary secrets, which COD explicitly praised). Restated
   build order: canaries and labels now; taint as observe-mode annotation for
   a long calibration period. With that re-weighting I'd self-score ~650, a
   modest concession from my implicit ranking — but GMI's 450 remains wrong,
   for reasons argued formally in the rebuttal (it applies a "heuristics are
   worthless" standard that would also condemn this project's shipped,
   headline redactor).
2. **#3 dead-wire exemption rot (both: "blanket `dormant` exemptions to pass
   the build" / "misclassified substrate").** Fair, and I harden the design
   in response: dormant manifest entries require a bead reference *and an
   expiry date*; CI warns on expiry; the exemption list itself is published
   in the attestation artifact so accumulating exemptions is visible debt,
   not invisible rot. Precedent says this works here — the windows-coupling
   ratchet's content-keyed baseline with explicit re-bless (ft-51fde) has
   held in exactly this repo culture. Score impact: minor trim, no rank
   change.
3. **#5 maintenance burden, #6 controller runaway, #8 resume flakiness, #10
   scope creep:** all consensus, all already designed-for, all absorbed as
   confidence trims rather than rank changes (details above).

## Concessions

- **#15 `ft doctor --fix` — conceded substantially.** COD's cultural argument
  is the best single critique either model produced: this repo's AGENTS.md is
  *deliberately, repeatedly hostile* to autonomous repair because agents have
  destroyed real work here ("doctor fix features tend to grow from safe
  repair into questionable mutation" is an empirically earned fear in this
  tree, not a generic worry). GMI's dual-writer scenario (false-negative
  liveness probe → lock cleared → two writers → DB corruption) is the right
  concrete nightmare; my fail-closed precondition design addresses it *if
  implemented exactly as specified*, and the cultural critique is precisely
  that "exactly as specified" erodes. Updated position: keep the idea, drop
  `--yes` unattended mode from v1 entirely, require independent double-probes
  (PID liveness + lsof + DB-open check) for the lock fix, and accept that the
  honest score band is the 550–650 their average implies — my own #15 rank
  confirmed harder than I'd written it.
- **#9 partially conceded** (presentation re-weight; canaries-first), as
  above.
- **#5 confidence trimmed** (corpus must start tiny; bless workflow is
  first-cut scope), as above.
- Not conceded despite low scores: **#11** and the core of **#9** — those are
  the two hills, defended formally in WIZARD_REBUTTAL_CC.md, alongside my
  attacks on the two weakest ideas from their lists (GMI's CRDT storage and
  distributed mission marketplace).

*This file and WIZARD_REBUTTAL_CC.md are the only files written in this step;
no code or configuration was modified.*
