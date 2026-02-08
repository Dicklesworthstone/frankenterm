# E2E Integration Checklist: Coverage vs Registry

## Purpose
Ensure every major feature has end-to-end (E2E) coverage with:
- deterministic synchronization (avoid fixed sleeps when possible)
- detailed, structured logging
- artifact bundles that make failures diagnosable

This checklist is human-friendly. The enforceable source of truth for what runs
under `--all` is the scenario registry in `scripts/e2e_test.sh`.

## How to use
For each checklist item:
- ensure there is a corresponding E2E case in the registry
- ensure the case runs via the standard runner (`scripts/e2e_test.sh`)
- ensure the case follows the harness contract (`docs/e2e-harness-spec.md`)

If an item is intentionally not E2E-tested (e.g., requires human credentials),
document the reason and provide at least one of:
- fixture-based integration tests, and/or
- a manual smoke-test command with stable output + artifact capture

## Scenario Registry (source of truth)
Registry lives in `scripts/e2e_test.sh` (SCENARIO_REGISTRY). Current entries:
- capture_search
- natural_language
- compaction_workflow
- unhandled_event_lifecycle
- workflow_lifecycle
- events_unhandled_alias
- events_annotations_triage
- usage_limit_safe_pause
- notification_webhook
- watch_notify_only
- policy_denial
- prepare_commit_approvals
- quickfix_suggestions
- triage_multi_issue
- rules_explain_trace
- stress_scale
- graceful_shutdown
- watcher_crash_bundle
- pane_exclude_filter
- workspace_isolation
- setup_idempotency
- uservar_forwarding
- alt_screen_detection
- no_lua_status_hook
- workflow_resume
- accounts_refresh

## Checklist by Feature Area

### Phase 1: Core features

#### Ingest (wa-4vx.4)
- [x] E2E: basic text capture from a real WezTerm dummy pane. Scenario(s): capture_search
- [ ] E2E: delta extraction produces correct segments (no duplication). Scenario(s): capture_search (partial; lacks explicit delta assertions)
- [x] E2E: FTS indexing enables search. Scenario(s): capture_search
- [ ] Artifacts: segment stats (bytes/lines), ingest lag, FTS timing. Scenario(s): none

#### Pattern detection (wa-4vx.5)
- [x] E2E: known patterns trigger events (at least 1 per critical workflow trigger). Scenario(s): compaction_workflow
- [ ] E2E: false positives are rejected (near-miss negative fixture). Scenario(s): none
- [ ] E2E: extraction captures expected structured facts. Scenario(s): none
- [ ] Artifacts: rule_id matched, extracted facts JSON, match timing. Scenario(s): none
- [x] E2E: explain-match trace surfaces (rules list/test + lint artifacts). Scenario(s): rules_explain_trace

#### Daemon/runtime (wa-4vx.6)
- [x] E2E: `wa watch` starts, runs, and exits cleanly. Scenario(s): graceful_shutdown
- [x] E2E: intentional watcher crash writes crash bundle and is surfaced by triage/doctor/reproduce. Scenario(s): watcher_crash_bundle (wa-upg.13.6)
- [ ] E2E: graceful shutdown flushes storage queue and releases lock. Scenario(s): graceful_shutdown (partial; lacks explicit lock/queue assertions)
- [ ] E2E: restart recovers cleanly (no corrupt state). Scenario(s): workflow_resume (partial; restart focused on workflow, not general state)
- [ ] Artifacts: health snapshot (queues/lag), watcher logs, lock state. Scenario(s): none

#### Robot mode (wa-4vx.7)
- [x] E2E: core robot commands produce valid JSON envelopes. Scenario(s): capture_search (robot state only; partial)
- [ ] E2E: stable error codes for common failures (pane missing, policy denied). Scenario(s): none
- [ ] E2E: `wa robot send` verification (`--wait-for`) works (or PaneWaiter equivalent). Scenario(s): none
- [ ] Artifacts: raw JSON outputs for each command + schema validation results. Scenario(s): none

#### Safety/policy (wa-4vx.8)
- [x] E2E: policy denies unsafe sends (alt-screen, recent gap, prompt not active). Scenario(s): policy_denial
- [x] E2E: approval allow-once flow works and is audited. Scenario(s): prepare_commit_approvals
- [x] E2E: audit trail captures allow/deny decisions with redaction. Scenario(s): prepare_commit_approvals (partial)
- [ ] Artifacts: audit export slice, redaction proof scans (no secrets). Scenario(s): none

### Phase 2: Workflows

#### Workflow engine (wa-nu4.1.1)
- [x] E2E: workflow triggered by an event under `--auto-handle`. Scenario(s): compaction_workflow
- [ ] E2E: workflow step logging is complete and ordered. Scenario(s): none
- [x] E2E: workflow resumes after restart (idempotent; no duplicate sends). Scenario(s): workflow_resume
- [ ] Artifacts: workflow execution logs + step log export. Scenario(s): none

#### Usage limits (wa-nu4.1.3)
- [x] E2E: end-to-end usage-limit workflow fixture-first (no real auth). Scenario(s): usage_limit_safe_pause
- [ ] E2E: key failure modes are safe/actionable (MFA required, cannot pick account). Scenario(s): usage_limit_safe_pause (partial: cannot pick account)
- [ ] Artifacts: parsed resume/session info (redacted), next-step plan output. Scenario(s): usage_limit_safe_pause (partial)

#### Compaction (wa-nu4.1.2)
- [x] E2E: compaction detected and handled exactly once per event (dedupe/cooldown). Scenario(s): unhandled_event_lifecycle
- [ ] E2E: injected context is verified (echo/marker). Scenario(s): compaction_workflow (partial; verify marker explicitly)
- [ ] Artifacts: detection evidence, injection payload preview (redacted), verification tail hash. Scenario(s): none

### Phase 3-4: Polish / integration

#### Diagnostics/health/metrics (wa-nu4.3.4)
- [ ] E2E: `wa doctor` healthy vs broken output is actionable. Scenario(s): none
- [ ] E2E: metrics endpoint responds when enabled. Scenario(s): none
- [ ] E2E: watcher health snapshot visible via CLI. Scenario(s): none
- [ ] Artifacts: diag bundle layout + redaction proofs. Scenario(s): none

#### Triage (wa-upg.9)
- [x] E2E: triage ordering and suggested actions with multiple issues. Scenario(s): triage_multi_issue

#### Event annotations + triage state (bd-2sua)
- [x] E2E: event annotate/label/triage lifecycle with redaction and audit evidence. Scenario(s): events_annotations_triage

#### Notifications (wa-psm)
- [x] E2E: events trigger notifications (webhook mock server). Scenario(s): notification_webhook
- [x] E2E: throttling prevents spam. Scenario(s): notification_webhook
- [x] E2E: failure recovery works (network down, endpoint 500). Scenario(s): notification_webhook
- [x] Artifacts: delivery attempts, retry/backoff, persisted notification history. Scenario(s): notification_webhook
- [x] E2E: notify-only mode delivers notifications without auto-handling; filters + throttling apply. Scenario(s): watch_notify_only

#### CLI polish (wa-rnf)
- [ ] E2E: shell completion generation works (smoke). Scenario(s): none
- [ ] E2E: alias expansion is correct. Scenario(s): none
- [ ] E2E: help text stays accurate (docs-smoke style). Scenario(s): none

#### Timeline/correlation (wa-6sk)
- [ ] E2E: events appear in timeline. Scenario(s): none
- [ ] E2E: correlations are deterministic on fixtures. Scenario(s): none
- [ ] E2E: query performance acceptable under seeded dataset. Scenario(s): stress_scale

#### Quick-fix suggestions (wa-bnm)
- [ ] E2E: suggestions appear on common errors/events. Scenario(s): quickfix_suggestions
- [ ] E2E: suggestions are copy-pasteable and safe. Scenario(s): quickfix_suggestions
- [ ] Artifacts: suggestion IDs fired + dismissal persistence (if applicable). Scenario(s): quickfix_suggestions

## E2E Case Requirements (non-negotiable)
- No fixed `sleep N` synchronization; use wait-for conditions with timeouts.
- All cases write an artifacts directory and print its path on failure.
- Every case prints a PASS/FAIL summary with elapsed time and key assertions.

## Registry Cross-Check
This checklist intentionally references bead IDs (e.g., wa-4vx.4). A validator
should fail if any referenced bead IDs are missing or if scenario names referenced
here are not present in SCENARIO_REGISTRY.

## Notes / Gaps
- Many checklist items currently lack matching scenarios. Use this document to
  drive new scenario beads or add cases to the registry.
- Several existing scenarios likely need stronger assertions and explicit
  artifact capture to fully meet requirements (e.g., delta extraction, dedupe).
