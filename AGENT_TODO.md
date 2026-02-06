# Agent TODO (VioletStream)

## 0) Session Bootstrap & Safety
- [x] Read `AGENTS.md` fully
- [x] Read `README.md` fully
- [x] Start Agent Mail session (register identity)
- [x] Verify Agent Mail inbox is empty / respond + ack if needed
- [x] Record active agents list + note missing names (QuietDeer/SilverPine)
- [x] Introduce self to other agents (targeted list)
- [ ] Create/update this TODO file after each major step

## 1) Codebase Archaeology (Architecture Understanding)
- [x] Orientation: list repo structure + manifests (Cargo.toml, crate manifests)
- [x] Identify entry points (`crates/wa/src/main.rs`)
- [x] Summarize CLI command tree + key handlers
- [x] Trace data flow: wezterm CLI → ingest/tailer → storage → patterns → event bus → workflows
- [x] Identify 3–5 key types (StorageHandle, ObservationRuntime, PatternEngine, PolicyEngine, WorkflowRunner, etc.)
- [x] Note integration points (wezterm CLI, sqlite, IPC, config)
- [x] Review configuration system (config.rs + CLI overrides)
- [x] Review tests layout (crates/wa-core/tests, benches, fuzz)
- [x] Write concise architecture summary for user

## 2) Agent Mail Coordination
- [x] Register as `VioletStream`
- [x] Fetch inbox
- [x] Send intro to key agents (CopperDesert, CoralCanyon, GreenHarbor, QuietCave, QuietGlen)
- [x] Note that QuietDeer/SilverPine not registered; ask user or wait
- [ ] Post progress updates on wa-y6g thread (after changes)
- [ ] Acknowledge any new messages promptly

## 3) Beads / BV Triage
- [x] Run `bv --robot-next`
- [ ] Run `bv --robot-triage` if more context needed
- [x] Run `br ready --json` and locate wa-y6g/wa-iqf
- [x] Confirm wa-y6g ownership / in-progress status
- [ ] If switching tasks, update bead status + notify agents

## 4) Dependency Updates (library-updater)
### 4.0 Discovery & Setup
- [x] Confirm manifests: root + crates/wa + crates/wa-core + fuzz
- [x] Verify `cargo outdated` availability
- [x] Verify `cargo audit` availability
- [x] Ensure `UPGRADE_LOG.md` exists
- [x] Ensure `UPGRADE_TODO.md` exists
- [x] Update `claude-upgrade-progress.json` with actual completed/pending
- [ ] Capture current dependency list + versions (workspace + crate-specific)

### 4.1 Per-dependency Loop (one at a time)
**Already updated (tests blocked by cargo locks; rerun later):**
- [x] clap 4.5 → 4.5.54
- [x] serde 1.0 → 1.0.228
- [x] serde_json 1.0 → 1.0.149
- [x] tokio 1.43 → 1.49.0
- [x] anyhow 1.0 → 1.0.100
- [x] tracing 0.1 → 0.1.44
- [x] tracing-subscriber 0.3 → 0.3.22
- [x] toml 0.8 → 0.8.23
- [x] toml_edit 0.22 → 0.24.0
- [x] toon_rust git → latest master
- [x] dirs 5.0 → 6.0.0
- [x] assert_cmd 2.0 → 2.1.2
- [x] predicates 3.1 → 3.1.3
- [x] fancy-regex already latest (skip)

**Pending research + update + test:**
- [x] thiserror
- [x] aho-corasick
- [x] memchr
- [x] regex
- [x] rand
- [x] sha2
- [x] rusqlite
- [x] fs2
- [x] base64
- [x] ratatui
- [x] crossterm
- [x] proptest
- [x] tempfile
- [x] criterion
- [x] libfuzzer-sys

For each dependency (completed; tests need rerun once locks clear):
- [x] Research breaking changes (software-research + web sources)
- [x] Update manifest/lock
- [ ] Run `cargo test` (blocked by lock; rerun pending)
- [x] Log results in `UPGRADE_LOG.md`
- [x] Update `claude-upgrade-progress.json`

### 4.2 Finalization
- [ ] Clear cargo lock contention (coordinate if needed)
- [ ] Run full test suite `cargo test`
- [x] Run `cargo fmt --check`
- [ ] Run `cargo check --all-targets`
- [ ] Run `cargo clippy --all-targets -- -D warnings`
- [x] Run `cargo audit`
- [x] Update `UPGRADE_LOG.md` summary counts + commands section

## 5) wa-y6g (Schema Migration Framework)
- [x] Extend migration model (up/down, plan, status) in `crates/wa-core/src/storage.rs`
- [x] Wire CLI: `wa db migrate` with `--status`, `--run`, `--to <version>`
- [x] Add output formatting for migration status/plan
- [x] Add tests: upgrade path + rollback path
- [ ] Run required checks after code changes (fmt/check/clippy/test)
- [ ] Update bead status + notify Agent Mail thread

## 6) Communication & Reporting
- [x] Summarize architecture for user
- [x] Report dependency update progress + remaining items
- [x] Report bead status + next actions
- [x] Keep TODO updated as tasks complete

---

# Agent TODO (BoldRiver)

## 0) Session Bootstrap & Safety
- [x] Read `AGENTS.md` fully
- [x] Read `README.md` fully
- [x] Start Agent Mail session (`macro_start_session`)
- [x] Check inbox (`resource://inbox/BoldRiver`)
- [x] List active agents (`resource://agents/data-projects-wezterm-automata`)
- [x] Send intro to active agents (WildBrook, MagentaCove, RedFalcon, TurquoiseCave, RubyFox)

## 1) Beads / BV Triage
- [x] Run `bv --robot-triage`
- [x] Run `br ready --json` to find actionable tasks
- [x] Select bead `wa-4vx.10.13` (E2E unhandled→handled lifecycle)
- [x] Mark `wa-4vx.10.13` as `in_progress`
- [x] Announce start in Agent Mail thread `wa-4vx.10.13`

## 2) File Reservations (Agent Mail)
- [x] Reserve `scripts/e2e_test.sh`
- [x] Reserve `fixtures/e2e/dummy_agent.sh`
- [x] Reserve `docs/e2e-integration-checklist.md`

## 3) Implement wa-4vx.10.13 (E2E unhandled→handled lifecycle)
### 3.1 Scenario Definition (scripts/e2e_test.sh)
- [x] Add new scenario function `run_scenario_unhandled_event_lifecycle`
- [x] Add scenario to `SCENARIO_REGISTRY` with description
- [x] Add scenario dispatch case in `run_scenario`
- [x] Ensure scenario uses baseline config (`fixtures/e2e/config_baseline.toml`)
- [x] Emit two compaction markers (dedupe/cooldown assertion)
- [x] Use `wa events -f json --unhandled` to assert exactly 1 relevant event
- [x] Use `wa robot events --unhandled --would-handle --dry-run` to fetch recommended workflow
- [x] Confirm auto-handle workflow clears unhandled event
- [x] Capture artifacts: events pre/post JSON, audit slice, workflow logs, pane text

### 3.2 Dummy Agent Fixture (fixtures/e2e/dummy_agent.sh)
- [x] Add optional args for repeat compaction markers (count + interval)
- [x] Preserve default behavior for existing scenarios

### 3.3 Checklist Update (docs/e2e-integration-checklist.md)
- [x] Add new scenario reference for unhandled→handled lifecycle
- [x] Update dedupe/cooldown row to reference new scenario (remove “partial” note if appropriate)

## 4) Local Verification (required after substantive changes)
- [ ] Run `cargo fmt --check` (failed; formatting diffs in wa-core files not touched)
- [x] Run `cargo check --all-targets`
- [ ] Run `cargo clippy --all-targets -- -D warnings` (failed: clippy::needless_raw_string_hashes in wa-core/desktop_notify.rs)
- [ ] Run targeted E2E (optional if too heavy): `./scripts/e2e_test.sh --case unhandled_event_lifecycle`

## 5) Wrap-up & Coordination
- [ ] Post progress update to Agent Mail thread `wa-4vx.10.13` with files touched
- [ ] Release file reservations
- [ ] Mark bead `wa-4vx.10.13` closed with reason + tests run

---

# Agent TODO (CalmLynx)

## 0) Session Bootstrap
- [x] Read `AGENTS.md` and `README.md` fully
- [x] Deep codebase exploration (architecture, patterns, workflows, storage)
- [x] Register with Agent Mail as CalmLynx (opus-4.6)
- [x] Send intro to TopazStone + CC to GoldHarbor, CopperWolf, CyanForge
- [x] Check inbox and reply to TopazStone's progress update

## 1) Build Fixes (pre-existing compilation errors)
- [x] Fix fastapi package rename: add `package = "fastapi-rust"` to workspace Cargo.toml
- [x] Fix storage.rs: `rows.next().transpose()` → `rows.next()` (rusqlite API)
- [x] Fix storage.rs: u64 ToSql/FromSql for SavedSearchRecord.pane_id (cast via i64)
- [x] Fix storage.rs: return type mismatch in query_saved_search_by_name
- [x] Fix storage.rs: rusqlite::Error → error::Error conversion
- [x] Fix missing `dedupe_key` field in StoredEvent test initializers
- [x] Fix missing `priority_override` in PaneEntry::with_uuid
- [x] Fix missing `pane_priority_overrides` in HealthSnapshot test initializers
- [x] Fix PaneNotFound struct→tuple variant in ingest.rs
- [x] Fix should_notify() 3-arg signature in main.rs
- [x] Fix OutputFormat::Auto not covered in match
- [x] Fix clippy doc_lazy_continuation lint in events.rs
- [x] Fix SQL `limit` reserved word: quote as `"limit"` in DDL and `\"limit\"` in queries
- [x] Fix `saturating_shl` → `checked_shl` for i64
- [x] Fix `Duration::from_millis(1_000)` → `Duration::from_secs(1)` (clippy)
- [x] Remove broken `run_saved_search_scheduler` reference (function not yet defined)
- [x] All checks pass: cargo fmt, cargo check, cargo clippy, cargo test (1929 tests)

## 2) Implement wa-1pe.3: `wa workflow run --dry-run`
- [x] Replace name-based action type inference with structured StepPlan-based approach
- [x] Add `step_action_to_dry_run_type()`: maps StepAction → ActionType with Custom fallback
- [x] Add `step_plan_metadata()`: extracts step_id, idempotent, timeouts, preconditions from StepPlan
- [x] Add `infer_action_type_from_name()`: fallback for Custom steps from `steps_to_plans()`
- [x] Update `build_workflow_dry_run_report()` to use `wf.steps_to_plans(pane)`
- [x] Add JSON output support: detect_format() + emit_json for `--dry-run`
- [x] Add 6 new tests (step metadata, lock/release, JSON roundtrip, triggers, usage_limits, human format)
- [x] All 8 workflow dry-run tests pass
- [x] Full test suite: 1929 tests, 0 failures

## 3) Implement wa-1pe.5: Dry-run testing suite
- [x] Created `crates/wa-core/tests/dry_run_integration.rs` with 33 tests across 7 categories
- [x] Fixed PolicyDecision::allow() call signature (no args)
- [x] Fixed serde roundtrip (skip_serializing_if on warnings field)
- [x] All 33 tests pass, full suite green

## 4) Build fixes (pre-existing from other agents)
- [x] Fix FK constraint in `saved_search_scheduler_emits_alert_and_redacts_snippet` (register pane before append_segment)
- [x] Fix `manual_assert` clippy lint in `wait_for_saved_search_error`
- [x] Fix formatting in saved search CLI code (TopazStone's code)

## 5) Implement wa-upg.8.5: Noise control tests (dedupe/cooldown/mute)
- [x] Created `crates/wa-core/tests/noise_control_tests.rs` with 34 tests
- [x] Mute storage CRUD: add/query, nonexistent, remove, expiry past/future/boundary (7 tests)
- [x] Mute determinism: upsert overwrites, idempotent, multiple keys (3 tests)
- [x] Identity key + mute integration: round-trip, UUID determinism (2 tests)
- [x] Dedup edge cases: zero window, capacity-1, suppressed count, expired get, defaults (5 tests)
- [x] Cooldown edge cases: zero period, capacity-1, accumulation, expired count, get entry (5 tests)
- [x] NotificationGate composite: filter, severity, agent type, sequence, include/exclude (6 tests)
- [x] EventFilter standalone: allow_all, permissive, glob, exact match (6 tests)
- [x] All checks pass: fmt, clippy, 1999 tests, 0 failures

## 6) Implement wa-upg.8.3: Mute/unmute CLI commands
- [x] Added `Mute` variant to `Commands` enum with after_help examples
- [x] Added `MuteCommands` enum: `Add`, `Remove`, `List` subcommands
- [x] Added `list_active_mutes` async method + `list_active_mutes_sync` in storage.rs
- [x] Added `parse_duration_to_ms` helper (supports s/m/h/d/w suffixes)
- [x] Added `Some(Commands::Mute { command })` handler dispatch in main match block
- [x] JSON and human-readable output for all three subcommands
- [x] Duration parsing: `--for 1h`, `--for 30m`, `--for 7d`, permanent if omitted
- [x] 10 unit tests (parse_duration) + 3 storage round-trip tests (add/list/remove, permanent, expired)
- [x] All checks pass: fmt, clippy, 2012 tests, 0 failures

## 7) Implement wa-upg.8.4: Integrate noise control into status/triage outputs
- [x] Added `render_with_noise_info()` to EventListRenderer (renderers.rs)
  - Shows "muted" status for events whose dedupe_key is in active mute set
  - JSON output includes `"muted": true/false` per event
  - Footer shows active mute count with "wa mute list" hint
- [x] Updated `wa events` handler to fetch mutes and use `render_with_noise_info`
- [x] Updated `wa triage` handler with noise control integration:
  - Unhandled events show "(muted)" prefix when identity key is muted
  - Added "Mute for 1 hour" action suggestion for unmuted events with dedupe_key
  - Added "Noise Control" section showing active mute count + details
- [x] 6 renderer tests for `render_with_noise_info` (muted status, JSON muted field, footer, edge cases)
- [x] All checks pass: fmt, clippy, 2018 tests, 0 failures

## 8) Implement wa-upg.1.5: Incident bundle + replay tests
- [x] Created `crates/wa-core/tests/incident_bundle_tests.rs` with 26 integration tests
- [x] Multi-pattern crash bundle redaction: Anthropic, OpenAI, GitHub, Bearer, DB URL, Stripe, AWS keys
- [x] Multiple secrets in single message/backtrace redaction
- [x] Incident bundle redaction: config secrets, crash report+health snapshot secrets across all files
- [x] Privacy budget enforcement: large backtrace, huge health snapshot, oversized config truncation
- [x] Edge cases: Unicode messages, empty fields, priority overrides round-trip
- [x] Incident manifest validation: JSON structure, file list vs disk consistency
- [x] Bundle uniqueness: multiple same-timestamp bundles get unique names
- [x] Listing/filtering: ignores non-bundle files, latest returns None when empty
- [x] Standalone Redactor tests: all major patterns, bearer/DB URLs, multiline, empty input, non-secret preservation
- [x] All checks pass: fmt, clippy, 2044 tests, 0 failures

## 9) Implement wa-upg.1.2: Enhanced incident bundle collector
- [x] Added `RedactionReport`, `FileRedactionEntry`, `DbMetadata`, `IncidentBundleOptions` structs to crash.rs
- [x] Added `collect_incident_bundle()`: enhanced bundle with DB metadata, recent events, redaction report
- [x] Added `collect_db_metadata()`: reads schema_version, journal_mode, event/segment counts from SQLite
- [x] Added `collect_recent_events_summary()`: queries recent events with matched_text preview
- [x] Added `write_redacted_file()` helper: tracks redactions per-file and accumulates size/file list
- [x] Fixed borrow checker: closure → standalone function for write_redacted_file
- [x] Fixed serde_json::json! macro: extracted row.get() calls before macro invocation
- [x] 11 new integration tests: manifest, DB metadata, recent events, max_events limit, zero events skip, config redaction, crash kind, clean report, secret redaction in events, nonexistent DB, files list vs disk
- [x] All checks pass: fmt, clippy, 2055 tests, 0 failures

## 10) Implement wa-upg.1.4: `wa reproduce replay` (deterministic incident replayer)
- [x] Converted `wa reproduce` from flat command to subcommand structure: `export` + `replay`
- [x] Added `ReproduceCommands` enum with `Export` (backward-compatible) and `Replay` subcommands
- [x] Updated `wa reproduce export` to use `collect_incident_bundle` (enhanced collector with DB metadata)
- [x] Added `ReplayMode` enum (Policy, Rules) with Display and Serialize/Deserialize
- [x] Added `ReplayCheck` struct (name, passed, detail) and `ReplayResult` struct (mode, status, checks, warnings)
- [x] Implemented `replay_incident_bundle()` with checks:
  - Manifest validation (exists, valid JSON, parseable as IncidentBundleResult)
  - Redaction report validation
  - Secret leak detection across all bundle .json/.toml files
  - Policy mode: crash_report structure, db_metadata schema validation
  - Rules mode: event structure validation, matched_text_preview bounds check
  - File completeness (manifest files vs disk)
- [x] Updated all `wa reproduce --kind crash` references to `wa reproduce export --kind crash`
- [x] 8 new replay tests: nonexistent bundle, clean policy pass, DB metadata, rules mode, secret leak detection, empty bundle, JSON roundtrip, crash report validation
- [x] All checks pass: fmt, clippy, 2063 tests, 0 failures

## 11) Implement wa-upg.5.4: Storage + FTS p95 regression guard benchmarks
- [x] Created `crates/wa-core/benches/storage_regression.rs` with 5 benchmark groups:
  - `storage_append_single`: single append_segment latency at 1K/10K/100K DB sizes (p95 < 2ms budget)
  - `storage_append_batch`: batch throughput on empty and 10K pre-populated DBs (> 500 seg/sec budget)
  - `storage_fts_p95`: FTS query regression guard at 100K segments (p95 < 15ms budget, 6 query types)
  - `storage_upsert_pane`: metadata write latency (p95 < 1ms budget, update + insert)
  - `storage_append_scaling`: latency scaling at 100/1K/10K/50K existing segments (regression detection)
- [x] Added `[[bench]] name = "storage_regression" harness = false` to wa-core/Cargo.toml
- [x] Budget declarations via bench_common: append p95 < 2ms, batch > 500/sec, FTS p95 < 15ms, upsert p95 < 1ms
- [x] All checks pass: fmt, clippy, tests green; bench test mode passes all groups

## 12) Implement wa-985.1: Analytics data model (usage_metrics table)
- [x] Incremented SCHEMA_VERSION from 15 to 16
- [x] Added `usage_metrics` table to SCHEMA_SQL with 4 indexes (timestamp, type+ts, agent+ts, account+ts)
- [x] Added Migration v16 with up_sql (CREATE TABLE + indexes) and down_sql (DROP TABLE)
- [x] Added `MetricType` enum (TokenUsage, ApiCost, ApiCall, RateLimitHit, WorkflowCost, SessionDuration) with `as_str()`, `FromStr`, `Display` impls
- [x] Added `UsageMetricRecord`, `DailyMetricSummary`, `AgentMetricBreakdown`, `MetricQuery` structs
- [x] Added `WriteCommand::RecordUsageMetric` and `WriteCommand::PurgeUsageMetrics` variants
- [x] Added sync functions: `record_usage_metric_sync`, `purge_usage_metrics_sync`, `query_usage_metrics_sync`, `aggregate_daily_sync`, `aggregate_by_agent_sync`
- [x] Added 5 public async methods on StorageHandle: `record_usage_metric`, `purge_usage_metrics`, `query_usage_metrics`, `aggregate_daily_metrics`, `aggregate_by_agent`
- [x] Created `crates/wa-core/tests/usage_metrics_tests.rs` with 15 tests covering type roundtrips, CRUD, filtering, aggregation, retention, schema migration
- [x] All checks pass: fmt, clippy, 2074 tests, 0 failures

## 13) Implement wa-5ap: Notification history (persistent log + queries + retention)
- [x] Incremented SCHEMA_VERSION from 16 to 17
- [x] Added `notification_history` table to SCHEMA_SQL with 4 indexes (timestamp, status, event_id, channel+ts)
- [x] Added Migration v17 with up_sql (CREATE TABLE + indexes) and down_sql (DROP TABLE)
- [x] Added `NotificationStatus` enum (Pending, Sent, Failed, Throttled) with `as_str()`, `FromStr`, `Display`, serde impls
- [x] Added `NotificationHistoryRecord` and `NotificationHistoryQuery` structs
- [x] Added 5 WriteCommand variants: RecordNotification, UpdateNotificationStatus, AcknowledgeNotification, IncrementNotificationRetry, PurgeNotificationHistory
- [x] Added 7 sync functions: record, update_status, acknowledge, increment_retry, purge, query, get_by_id
- [x] Added 8 public async methods on StorageHandle: record_notification, update_notification_status, acknowledge_notification, increment_notification_retry, purge_notification_history, query_notification_history, get_notification
- [x] Created `crates/wa-core/tests/notification_history_tests.rs` with 24 tests covering status roundtrips, CRUD, filtering (channel/status/event_id/time_range/combined), purge, retry, acknowledgement, schema migration
- [x] All checks pass: fmt, clippy, 2102 tests, 0 failures

## 14) Implement wa-985.4: Proactive alerts (threshold monitoring + notification triggers)
- [x] Created `crates/wa-core/src/alerts.rs` module (~500 lines)
- [x] Added `AlertPeriod` enum (Day, Week, Month) with `duration_ms()`, `as_str()`, `FromStr`, `Display`
- [x] Added `AlertLevel` enum (Info, Warning, Critical, Exceeded) with `from_percent()` thresholds (50%/75%/90%/100%)
- [x] Added `AlertMetric` enum (Cost, TokenUsage, RateLimitFrequency, AccountBalance)
- [x] Added `AlertRule` struct with constructors: `cost()`, `token_usage()`, `rate_limit()`, `account_balance()`
- [x] Implemented `AlertRule::check()` for threshold evaluation against current values
- [x] Added `TriggeredAlert` struct with `summary()` method for human-readable output
- [x] Added `AlertMonitor` struct with `new()`, `rules()`, `add_rule()`, `remove_rule()`, `check_alerts()` (async, queries storage)
- [x] Implemented `get_current_value()` querying MetricType-specific usage_metrics or accounts by service
- [x] Added `pub mod alerts;` to lib.rs
- [x] 15 unit tests in alerts.rs: AlertPeriod/AlertLevel/AlertMetric roundtrips, threshold evaluation, rule CRUD, percentage-based alert levels
- [x] Created `crates/wa-core/tests/alert_monitor_tests.rs` with 11 integration tests: cost alerts (threshold/below/exceeded), token alerts, rate limit alerts, multiple rules, disabled rules, empty DB, old metrics outside window, summary formatting, account balance with no service
- [x] All checks pass: fmt, clippy, all alert tests pass

## 15) Implement bd-1yk8: Event annotations + triage state data model
- [x] Verified data model already in place: SCHEMA_VERSION=18, migration v18 (ALTER TABLE events + event_labels + event_notes tables)
- [x] Verified existing types: `EventAnnotations` struct with triage_state, note, labels fields
- [x] Verified existing sync functions: `set_event_triage_state_sync`, `set_event_note_sync` (with redaction), `add_event_label_sync`, `remove_event_label_sync`, `query_event_annotations_sync`
- [x] Verified WriteCommand variants and writer dispatch entries for all annotation operations
- [x] Verified async methods: `set_event_triage_state`, `set_event_note`, `add_event_label`, `remove_event_label`, `get_event_annotations`
- [x] Verified EventQuery supports `triage_state` and `label` filter fields
- [x] Created `crates/wa-core/tests/event_annotation_tests.rs` with 21 integration tests:
  - Triage: set/get, clear, nonexistent event, state transitions
  - Notes: set/get, overwrite, clear
  - Labels: add/get (sorted), duplicate idempotent, remove, remove nonexistent, scoped to event
  - Combined: full roundtrip, fresh event empty, nonexistent event returns None
  - Query: filter by triage_state, filter by label, combined triage+label filters
  - Serde: EventAnnotations roundtrip, default is empty
  - Schema: migration v18 tables exist and work
- [x] All checks pass: fmt, clippy, 2150 tests, 0 failures

## 16) Implement wa-985.3: Analytics CLI commands (wa analytics)
- [x] Added `AnalyticsCommands` enum (Daily, ByAgent, Export) and `Analytics` variant to `Commands` enum in main.rs
- [x] Added CLI args: `--period` (default 7d), `--format` (auto/plain/json), export `--format` (csv/json) + `--output`
- [x] Added analytics renderers in `crates/wa-core/src/output/renderers.rs`:
  - `AnalyticsSummaryData` struct (serde) + `AnalyticsSummaryRenderer` (tokens, cost, rate limits, workflows)
  - `AnalyticsDailyRenderer` (daily metrics table with date, tokens, cost, events)
  - `AnalyticsAgentRenderer` (per-agent breakdown with % of total)
  - `AnalyticsExportRenderer` (CSV and JSON export)
  - Helper functions: `format_number()` (comma separators), `format_epoch_date()` (epoch-ms to YYYY-MM-DD)
- [x] Updated `crates/wa-core/src/output/mod.rs` with new public exports
- [x] Added handler dispatch in main.rs: summary (default), daily, by-agent, export (csv/json + file output)
- [x] Added helper functions: `now_epoch_ms()`, `parse_period_ms()`, `format_period_label()`
- [x] Fixed pre-existing `Commands::Events` pattern match (missing `command` field → `..`)
- [x] Created `crates/wa-core/tests/analytics_cli_tests.rs` with 15 integration tests:
  - SummaryData serde roundtrip, plain/json rendering, zeros
  - DailyRenderer: plain/json/empty
  - AgentRenderer: plain/json/empty/single-100%
  - ExportRenderer: CSV format/empty, JSON format/empty
- [x] Added 7 CLI parsing tests in main.rs: default, daily, by-agent, export+csv, period flag, parse_period_ms, format_period_label
- [x] All checks pass: fmt, clippy, 2172 tests, 0 failures
