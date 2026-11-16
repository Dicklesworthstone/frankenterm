# Blocker Radar Merge Spec Mapping

Spec: `blocker-radar-merge.tla`

## Rust Correspondence

| TLA+ symbol | Rust target | Notes |
|---|---|---|
| `ClaimabilityVerdict` | `crates/frankenterm-core/src/blocker_radar.rs:1214` | Closed set of final and source verdicts. |
| `ClaimabilityInput` | `crates/frankenterm-core/src/blocker_radar.rs:1227` | Read-only source bundle before normalization. |
| `ClaimabilityReport` | `crates/frankenterm-core/src/blocker_radar.rs:1269` | Output shape containing source verdicts, final verdict, next action, and forbidden actions. |
| `ClaimabilityPartial` / `ClaimabilityParts` | `crates/frankenterm-core/src/blocker_radar.rs:1287` | Internal normalized source verdicts consumed by the merge function. |
| Claimability contract | `docs/blocker-radar-contract.md:133` | Fail-closed precedence and claimable-only-when-all-predicates-pass doctrine. |

## Action Mapping

| TLA+ action | Rust target | Notes |
|---|---|---|
| `Evaluate` | `crates/frankenterm-core/src/blocker_radar.rs:1320` | `build_claimability_report` refreshes and normalizes all source verdicts before merging. |
| Ready source | `crates/frankenterm-core/src/blocker_radar.rs:1438` | `claimability_ready_queue` maps BR ready output to `claimable` or `no_ready`. |
| Dependency source | `crates/frankenterm-core/src/blocker_radar.rs:1470` | `claimability_dependency` maps blocked dependency evidence. |
| Owner source | `crates/frankenterm-core/src/blocker_radar.rs:1491` | `claimability_owner` handles assignee and fresh-comment ownership blockers. |
| Dirty source | `crates/frankenterm-core/src/blocker_radar.rs:1522` | `claimability_dirty_paths` fails closed for dirty overlap. |
| External source | `crates/frankenterm-core/src/blocker_radar.rs:1542` | `claimability_external_wait` fails closed for queued or pending external substrates. |
| Mail source | `crates/frankenterm-core/src/blocker_radar.rs:1567` | `claimability_mail` records Agent Mail degradation without repairing or restarting it. |
| Tracker source | `crates/frankenterm-core/src/blocker_radar.rs:1587` | `claimability_tracker_consistency` fails closed on missing or contradictory tracker evidence. |
| Parse-failure source | `crates/frankenterm-core/src/blocker_radar.rs:1385` | Invalid JSON input maps directly to `tracker_inconsistent`. |

## Invariant Mapping

| TLA+ invariant | Rust target | Notes |
|---|---|---|
| `FailClosedPrecedence` | `crates/frankenterm-core/src/blocker_radar.rs:1635` | `claimability_final_verdict` encodes the precedence chain. |
| `ClaimableRequiresAllSafetyPredicates` | `crates/frankenterm-core/src/blocker_radar.rs:1663` | The only `Claimable` branch requires ready, dependency, owner, dirty, external, and mail predicates to pass. |
| `AllSafetyPredicatesYieldClaimable` | `crates/frankenterm-core/src/blocker_radar.rs:1663` | The positive branch returns `Claimable` when all safety predicates agree. |
| `TrackerInconsistencyDominates` | `crates/frankenterm-core/src/blocker_radar.rs:1639` | Tracker inconsistency is the first fail-closed guard. |
| `BlockingSourcesNeverClaim` | `crates/frankenterm-core/src/blocker_radar.rs:1642` | Dirty, dependency, external, and owner blockers all precede the claimable branch. |
| `MailDegradedNeverClaimsWithoutReadyEvidence` | `crates/frankenterm-core/src/blocker_radar.rs:1654` | Beads/git fallback can surface `mail_degraded`, but it cannot become `claimable` without ready evidence. |
| `HistoryRowsMatchMergeFunction` | `crates/frankenterm-core/src/blocker_radar.rs:1341` | Every report row is derived from the same merge function used for the root final verdict. |
| Safe next actions | `crates/frankenterm-core/src/blocker_radar.rs:1784` | Non-claimable verdicts route to read-only or coordination actions. |
| Forbidden actions | `crates/frankenterm-core/src/blocker_radar.rs:1807` | The output forbids dangerous follow-ups for non-claimable states. |

## TLC Configuration

Config: `blocker-radar-merge.cfg`

The deterministic smoke model uses two candidate classes: a normal bead and
the special `coordination-snapshot` fallback row. The source domains enumerate
every normalized verdict family consumed by `claimability_final_verdict`, so the
state space covers all fail-closed precedence combinations. The release-bundle
proof slot is `proofs/blocker-radar-merge.json`.
