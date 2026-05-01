#!/usr/bin/env python3
"""ft-3kv6e — RuntimeProof coverage audit for frankenterm-core.

Walks `crates/frankenterm-core/src/` and classifies every `pub async fn`
site as **covered** or **uncovered**. A site is *covered* if its
signature contains any of:

  * `&Cx` / `&mut Cx` parameter (Cx is sealed — see `runtime_proof.rs`)
  * `impl RuntimeProof`
  * `: RuntimeProof` (generic bound)

A site is *exempt* (not counted) if it lives in one of the runtime-layer
files listed in `EXEMPT_FILES` — those modules ARE the seal and cannot
take a `RuntimeProof` bound on themselves.

The script ratchets a baseline at `tests/runtime_proof_coverage_baseline.json`.
A run fails (exit 1) if the live uncovered count exceeds the baseline;
each adoption commit is expected to lower the baseline. Update the
baseline in the same commit that lowers the count.

Usage:
  scripts/check_runtime_proof_coverage.py
  scripts/check_runtime_proof_coverage.py --update-baseline   # rewrite baseline to match current state
  scripts/check_runtime_proof_coverage.py --json              # emit machine-readable summary

Cross-references:
  ft-i2eni.1 — RuntimeProof sealed trait foundation (e990cec00)
  ft-3kv6e   — this adoption sweep
  docs/runtime/runtime-proof-trait.md — doctrine
"""
from __future__ import annotations

import argparse
import json
import re
import sys
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent
SRC_ROOT = REPO_ROOT / "crates" / "frankenterm-core" / "src"
BASELINE_PATH = (
    REPO_ROOT / "crates" / "frankenterm-core" / "tests" / "runtime_proof_coverage_baseline.json"
)

# Runtime-layer files that ARE the seal — they cannot take a
# RuntimeProof bound on themselves without circular dependency.
# Add sparingly; every entry is a permanent doctrine carve-out.
EXEMPT_FILES: set[str] = {
    "runtime_async.rs",       # the wrapper module — primitives are sealed elsewhere
    "runtime_proof.rs",       # defines the seal itself
    "cx.rs",                  # Cx is the canonical structured-async witness; sealed in runtime_proof.rs
    "cx_stub.rs",             # build-time stub of cx.rs (no-op shim)
}

# Functions that are ergonomic wrappers around a `_with_cx` / `_cx` sibling.
# Each entry is a (relative_file, fn_name) pair. The wrapper itself is
# considered transitively covered because it constructs a default `Cx`
# internally and immediately delegates to the covered sibling.
#
# Entries here MUST be paired with a real covered sibling — i.e. the same
# file must contain `pub async fn <name>_with_cx(cx: &Cx, ...)` or
# `pub async fn <name>_cx(cx: &Cx, ...)`. The audit cross-checks this so
# the allowlist can't drift into a silent escape hatch.
WRAPPER_EXEMPTIONS: set[tuple[str, str]] = {
    # ft-4ku44: ipc.rs ergonomic wrappers. Each entry below is a
    # non-Cx public method whose body either constructs a default
    # `Cx` (`Cx::current().unwrap_or_else(for_request)`) and delegates
    # to the `_with_cx` sibling, or — for the cfg(not(unix)) Windows
    # stubs in the second `impl IpcServer` / `impl IpcClient` blocks —
    # is a tracing::warn no-op that doesn't touch any runtime primitive
    # directly. Either way the seal is preserved: every concrete
    # async-await against a runtime primitive lives in the
    # `_with_cx` covered sibling.
    ("ipc.rs", "bind"),
    ("ipc.rs", "bind_with_permissions"),
    ("ipc.rs", "run"),
    ("ipc.rs", "run_with_registry"),
    ("ipc.rs", "run_with_auth"),
    ("ipc.rs", "run_with_registry_and_auth"),
    ("ipc.rs", "run_with_registry_auth_and_rpc"),
    ("ipc.rs", "run_with_registry_auth_rpc_and_search_config"),
    ("ipc.rs", "send_user_var"),
    ("ipc.rs", "ping"),
    ("ipc.rs", "status"),
    ("ipc.rs", "pane_state"),
    ("ipc.rs", "set_pane_priority"),
    ("ipc.rs", "clear_pane_priority"),
    ("ipc.rs", "call_rpc"),
    # ft-dit9w: workflows cluster ergonomic wrappers. Each entry below is
    # a non-Cx public method whose body either constructs a default `Cx`
    # and delegates to its `_with_cx`/`_cx` sibling, or transitively
    # delegates through another wrapper-exempt sibling that does.
    # workflows/context.rs send_* methods construct a default Cx
    # internally and pass it directly into the underlying CxPolicyInjector
    # primitive call — no parallel non-Cx primitive path exists.
    ("workflows/context.rs", "send_text"),
    ("workflows/context.rs", "send_ctrl_c"),
    ("workflows/context.rs", "send_ctrl_d"),
    ("workflows/context.rs", "send_ctrl_z"),
    # workflows/engine.rs WorkflowEngine methods: each delegates to its
    # `_cx` sibling.
    ("workflows/engine.rs", "start"),
    ("workflows/engine.rs", "start_with_id"),
    ("workflows/engine.rs", "resume"),
    ("workflows/engine.rs", "find_incomplete"),
    ("workflows/engine.rs", "update_status"),
    ("workflows/engine.rs", "log_step"),
    # workflows/engine.rs free-function audit-action helpers: each
    # delegates to its `_with_cx` sibling.
    ("workflows/engine.rs", "record_workflow_start_action"),
    ("workflows/engine.rs", "fetch_workflow_start_action_id"),
    ("workflows/engine.rs", "record_workflow_step_action"),
    ("workflows/engine.rs", "record_workflow_terminal_action"),
    # workflows/runner.rs WorkflowRunner methods: each delegates to its
    # `_with_cx` sibling.
    ("workflows/runner.rs", "handle_detection"),
    ("workflows/runner.rs", "run_workflow"),
    ("workflows/runner.rs", "run"),
    ("workflows/runner.rs", "resume_incomplete"),
    ("workflows/runner.rs", "abort_execution"),
    # workflows/plan_helpers.rs: delegates to `check_step_idempotency_with_cx`.
    ("workflows/plan_helpers.rs", "check_step_idempotency"),
    # ft-7xbaz: wezterm.rs ergonomic wrappers. Every entry below either
    # constructs a default Cx and calls a `_with_cx` sibling, OR
    # delegates to a private helper (`send_text_impl`, `run_cli_*`,
    # `query_panes`, etc.) whose body already constructs a default Cx
    # and routes the primitive-use path through `pool.*_with_cx`.
    # Either way the seal is preserved: every concrete async-await
    # against a runtime primitive lives behind a Cx-bearing call.
    ("wezterm.rs", "list_panes"),
    ("wezterm.rs", "get_pane"),
    ("wezterm.rs", "get_text"),
    ("wezterm.rs", "pane_tiered_scrollback_summary"),
    ("wezterm.rs", "send_text"),
    ("wezterm.rs", "send_text_no_paste"),
    ("wezterm.rs", "send_text_with_options"),
    ("wezterm.rs", "send_control"),
    ("wezterm.rs", "send_ctrl_c"),
    ("wezterm.rs", "send_ctrl_d"),
    ("wezterm.rs", "spawn"),
    ("wezterm.rs", "spawn_targeted"),
    ("wezterm.rs", "split_pane"),
    ("wezterm.rs", "activate_pane"),
    ("wezterm.rs", "get_pane_direction"),
    ("wezterm.rs", "kill_pane"),
    ("wezterm.rs", "zoom_pane"),
    ("wezterm.rs", "wait_for"),
    ("wezterm.rs", "wait_for_codex_session_summary"),
    # MockWezterm test-fixture methods (under #[cfg(test)] blocks); the
    # mock's `*_with_cx` siblings exist in the same file and exercise
    # the same simulated primitive surface.
    ("wezterm.rs", "add_pane"),
    ("wezterm.rs", "add_default_pane"),
    ("wezterm.rs", "inject"),
    ("wezterm.rs", "inject_output"),
    ("wezterm.rs", "pane_state"),
    ("wezterm.rs", "pane_count"),
    ("wezterm.rs", "set_watchdog_warnings"),
    ("wezterm.rs", "set_watchdog_warning_error"),
    # ft-7xbaz: vendored/mux_client.rs ergonomic wrappers. Each non-Cx
    # public method either delegates to its `_with_cx` sibling or
    # constructs a default Cx and calls the same primitive-using path.
    # connect/list_panes/etc. construct a default cx; the floating-pane
    # / layout / stack helpers each delegate via shared inner machinery
    # whose `_with_cx` variant carries the seal.
    ("vendored/mux_client.rs", "connect"),
    ("vendored/mux_client.rs", "list_panes"),
    ("vendored/mux_client.rs", "get_pane_render_changes"),
    ("vendored/mux_client.rs", "get_lines"),
    ("vendored/mux_client.rs", "write_to_pane"),
    ("vendored/mux_client.rs", "send_paste"),
    ("vendored/mux_client.rs", "create_floating_pane"),
    ("vendored/mux_client.rs", "move_floating_pane"),
    ("vendored/mux_client.rs", "set_floating_pane_z"),
    ("vendored/mux_client.rs", "toggle_floating_pane"),
    ("vendored/mux_client.rs", "remove_floating_pane"),
    ("vendored/mux_client.rs", "swap_to_layout"),
    ("vendored/mux_client.rs", "set_layout_cycle"),
    ("vendored/mux_client.rs", "cycle_stack"),
    ("vendored/mux_client.rs", "select_stack_pane"),
    ("vendored/mux_client.rs", "update_pane_constraints"),
    ("vendored/mux_client.rs", "get_pane_render_changes_batch"),
    ("vendored/mux_client.rs", "batch"),
    ("vendored/mux_client.rs", "next"),
    ("vendored/mux_client.rs", "shutdown"),
    # ft-7xbaz: vendored/mux_pool.rs ergonomic wrappers — pool-level
    # methods that delegate through to the underlying mux_client's
    # `_with_cx` chain.
    ("vendored/mux_pool.rs", "list_panes"),
    ("vendored/mux_pool.rs", "get_lines"),
    ("vendored/mux_pool.rs", "get_pane_render_changes"),
    ("vendored/mux_pool.rs", "get_pane_render_changes_batch"),
    ("vendored/mux_pool.rs", "write_to_pane"),
    ("vendored/mux_pool.rs", "send_paste"),
    ("vendored/mux_pool.rs", "health_check"),
    ("vendored/mux_pool.rs", "evict_idle"),
    ("vendored/mux_pool.rs", "clear"),
    ("vendored/mux_pool.rs", "stats"),
    # ft-m2xpx: storage.rs cleanup-engine section (lines 3763–4679).
    # Bulk retention helpers (count_X_before, delete_X_before), DBA ops
    # (vacuum, checkpoint, sync_fts, database_page_stats), approval-token
    # CRUD, and a handful of mutation helpers (upsert_pane / upsert_workflow
    # / upsert_action_plan / insert_step_log / insert_prepared_plan /
    # consume_prepared_plan). Each wraps its `_with_cx` sibling — the
    # primitive-using path runs through the cx-bearing variant, so the
    # seal is preserved at the SQL boundary.
    ("storage.rs", "count_segments_before"),
    ("storage.rs", "count_events_before"),
    ("storage.rs", "count_events_by_tier"),
    ("storage.rs", "count_audit_actions_before"),
    ("storage.rs", "count_usage_metrics_before"),
    ("storage.rs", "count_notification_history_before"),
    ("storage.rs", "delete_events_before"),
    ("storage.rs", "delete_events_by_tier"),
    ("storage.rs", "query_notification_history"),
    ("storage.rs", "get_notification"),
    ("storage.rs", "vacuum"),
    ("storage.rs", "checkpoint"),
    ("storage.rs", "database_page_stats"),
    ("storage.rs", "get_pane_indexing_stats"),
    ("storage.rs", "get_indexing_health"),
    ("storage.rs", "sync_fts"),
    ("storage.rs", "rebuild_fts"),
    ("storage.rs", "get_fts_index_state"),
    ("storage.rs", "insert_approval_token"),
    ("storage.rs", "consume_approval_token"),
    ("storage.rs", "get_approval_token_by_code"),
    ("storage.rs", "consume_approval_token_by_code"),
    ("storage.rs", "upsert_pane"),
    ("storage.rs", "upsert_workflow"),
    ("storage.rs", "upsert_action_plan"),
    ("storage.rs", "insert_prepared_plan"),
    ("storage.rs", "consume_prepared_plan"),
    ("storage.rs", "insert_step_log"),
    # ft-juz4v: storage.rs embeddings + read-side query section
    # (lines 5088–6092). The section header is "Embedding storage
    # (semantic search)" but the band actually contains the broader
    # read-side surface that lives there in source order: embedding
    # CRUD (store_embedding / get_embedding / embedding_stats),
    # vector search (semantic_search / hybrid_search_with_results),
    # event queries (get_events / get_events_stream / get_timeline /
    # count_unhandled_events_by_pane), audit + approval reads
    # (get_audit_actions{,_stream} / get_action_history /
    # count_active_approvals / get_approval_token), pane and segment
    # reads (get_pane{,s} / get_segments / scan_segments /
    # get_max_seq), workflow reads (get_workflow / get_step_logs /
    # get_latest_step_log / get_action_plan / get_prepared_plan /
    # find_incomplete_workflows / get_unhandled_events), plus
    # latest_secret_scan_report / get_last_activity_by_pane /
    # is_writable. Each delegates to its `_with_cx` sibling — the
    # primitive-using SQL path runs through the cx-bearing variant.
    ("storage.rs", "store_embedding"),
    ("storage.rs", "get_unembedded_segments"),
    ("storage.rs", "get_embedding"),
    ("storage.rs", "embedding_stats"),
    ("storage.rs", "store_embedding_f32"),
    ("storage.rs", "semantic_search"),
    ("storage.rs", "hybrid_search_with_results"),
    ("storage.rs", "get_unhandled_events"),
    ("storage.rs", "get_events"),
    ("storage.rs", "get_events_stream"),
    ("storage.rs", "get_timeline"),
    ("storage.rs", "count_unhandled_events_by_pane"),
    ("storage.rs", "get_last_activity_by_pane"),
    ("storage.rs", "get_audit_actions"),
    ("storage.rs", "get_audit_actions_stream"),
    ("storage.rs", "get_action_history"),
    ("storage.rs", "count_active_approvals"),
    ("storage.rs", "get_approval_token"),
    ("storage.rs", "get_max_seq"),
    ("storage.rs", "get_panes"),
    ("storage.rs", "get_pane"),
    ("storage.rs", "get_segments"),
    ("storage.rs", "scan_segments"),
    ("storage.rs", "latest_secret_scan_report"),
    ("storage.rs", "get_workflow"),
    ("storage.rs", "get_step_logs"),
    ("storage.rs", "get_latest_step_log"),
    ("storage.rs", "get_action_plan"),
    ("storage.rs", "get_prepared_plan"),
    ("storage.rs", "find_incomplete_workflows"),
    ("storage.rs", "is_writable"),
    # ft-z5x09: storage.rs sessions / accounts / reservations / exports
    # tail (lines 4679–5088 sessions, 6092–6289 accounts, 6289–6446
    # reservations, 6446+ exports). All 32 entries delegate to existing
    # `_with_cx` siblings.
    # Sessions section (Session Checkpoint Methods):
    ("storage.rs", "insert_mux_session"),
    ("storage.rs", "insert_session_checkpoint"),
    ("storage.rs", "prune_session_checkpoints"),
    ("storage.rs", "mark_session_shutdown_clean"),
    ("storage.rs", "get_latest_checkpoint_hash"),
    ("storage.rs", "upsert_agent_session"),
    ("storage.rs", "get_agent_session"),
    ("storage.rs", "get_active_sessions"),
    ("storage.rs", "get_sessions_for_pane"),
    ("storage.rs", "search"),
    ("storage.rs", "search_with_options"),
    ("storage.rs", "search_with_results"),
    # Accounts section:
    ("storage.rs", "upsert_account"),
    ("storage.rs", "update_account_last_used"),
    ("storage.rs", "delete_account"),
    ("storage.rs", "get_accounts_by_service"),
    ("storage.rs", "get_account"),
    ("storage.rs", "select_account"),
    # Pane Reservation Operations:
    ("storage.rs", "create_reservation"),
    ("storage.rs", "release_reservation"),
    ("storage.rs", "get_active_reservation"),
    ("storage.rs", "list_active_reservations"),
    # Export Query Operations + tail (shutdown / expire_stale_reservations):
    ("storage.rs", "export_segments"),
    ("storage.rs", "export_gaps"),
    ("storage.rs", "get_gaps"),
    ("storage.rs", "get_retention_cleanup_count"),
    ("storage.rs", "get_segment_time_range"),
    ("storage.rs", "export_workflows"),
    ("storage.rs", "export_sessions"),
    ("storage.rs", "export_reservations"),
    ("storage.rs", "expire_stale_reservations"),
    ("storage.rs", "shutdown"),
    # ft-b7kuk: storage.rs core section (lines 2128–3763 within
    # impl StorageHandle). The largest remaining storage subset:
    # constructors, segment/gap append, event lifecycle (record /
    # mark / triage / note / label / annotations), audit-action
    # write path (record / undo / redact / purge / mark_undone),
    # maintenance + secret-scan reporting, saved-search CRUD,
    # pane-bookmark CRUD, retention helpers (prune_segments_before
    # / retention_cleanup), usage-metric write/query/aggregate, and
    # notification write/ack/retry/purge. Every entry has an
    # existing `_with_cx` sibling — audit cross-check validates each.
    ("storage.rs", "new"),
    ("storage.rs", "with_config"),
    ("storage.rs", "append_segment"),
    ("storage.rs", "record_gap"),
    ("storage.rs", "record_event"),
    ("storage.rs", "mark_event_handled"),
    ("storage.rs", "set_event_triage_state"),
    ("storage.rs", "set_event_note"),
    ("storage.rs", "add_event_label"),
    ("storage.rs", "remove_event_label"),
    ("storage.rs", "get_event_annotations"),
    ("storage.rs", "get_event_identity_key"),
    ("storage.rs", "record_audit_action"),
    ("storage.rs", "record_audit_action_redacted"),
    ("storage.rs", "record_policy_denial_audit"),
    ("storage.rs", "upsert_action_undo"),
    ("storage.rs", "upsert_action_undo_redacted"),
    ("storage.rs", "get_action_undo"),
    ("storage.rs", "mark_action_undone"),
    ("storage.rs", "purge_audit_actions_before"),
    ("storage.rs", "record_maintenance"),
    ("storage.rs", "record_secret_scan_report"),
    ("storage.rs", "insert_saved_search"),
    ("storage.rs", "update_saved_search_run"),
    ("storage.rs", "update_saved_search_schedule"),
    ("storage.rs", "delete_saved_search"),
    ("storage.rs", "get_saved_search_by_name"),
    ("storage.rs", "list_saved_searches"),
    ("storage.rs", "insert_pane_bookmark"),
    ("storage.rs", "delete_pane_bookmark"),
    ("storage.rs", "get_pane_bookmark_by_alias"),
    ("storage.rs", "list_pane_bookmarks"),
    ("storage.rs", "list_pane_bookmarks_by_tag"),
    ("storage.rs", "prune_segments_before"),
    ("storage.rs", "retention_cleanup"),
    ("storage.rs", "record_usage_metric"),
    ("storage.rs", "record_usage_metrics_batch"),
    ("storage.rs", "purge_usage_metrics"),
    ("storage.rs", "query_usage_metrics"),
    ("storage.rs", "aggregate_daily_metrics"),
    ("storage.rs", "aggregate_by_agent"),
    ("storage.rs", "record_notification"),
    ("storage.rs", "update_notification_status"),
    ("storage.rs", "acknowledge_notification"),
    ("storage.rs", "increment_notification_retry"),
    ("storage.rs", "purge_notification_history"),
    # ft-wb02g: runtime/concurrency cluster long-tail (32 sites).
    # simulation.rs: deterministic LabRuntime simulation harness; each
    # method delegates to its `_with_cx` sibling for cancel propagation
    # through the simulated event timeline.
    ("simulation.rs", "setup"),
    ("simulation.rs", "execute_until"),
    ("simulation.rs", "execute_all"),
    ("simulation.rs", "execute_until_with_resize_timeline"),
    ("simulation.rs", "execute_all_with_resize_timeline"),
    ("simulation.rs", "new"),
    ("simulation.rs", "with_scenario"),
    ("simulation.rs", "trigger_exercise_events"),
    ("simulation.rs", "check_expectation"),
    ("simulation.rs", "check_all_expectations"),
    # wait.rs: each non-cx wait fn constructs a default Cx and
    # delegates to the `_cx` sibling (see wait_for at line 146).
    ("wait.rs", "wait_for"),
    ("wait.rs", "wait_for_value"),
    ("wait.rs", "wait_for_quiescence"),
    ("wait.rs", "wait_for_quiescence_with_backoff"),
    ("wait.rs", "wait_for_condition"),
    ("wait.rs", "wait_for_condition_with_backoff"),
    # retry.rs: with_retry chain delegates through `_outcome`
    # variants whose `_cx` siblings carry the seal.
    ("retry.rs", "with_retry"),
    ("retry.rs", "with_retry_outcome"),
    ("retry.rs", "with_retry_and_circuit"),
    ("retry.rs", "with_smart_retry"),
    # pool.rs: connection-pool ergonomic wrappers around the
    # `_with_cx` siblings that thread cancellation into the
    # underlying acquire/release primitive ops.
    ("pool.rs", "try_acquire"),
    ("pool.rs", "acquire"),
    ("pool.rs", "put"),
    ("pool.rs", "evict_idle"),
    ("pool.rs", "stats"),
    ("pool.rs", "clear"),
    # cancellation_safe_channel.rs: the non-cx reserve/recv have
    # `_with_cx` siblings that gate the underlying mpsc primitive.
    ("cancellation_safe_channel.rs", "reserve"),
    ("cancellation_safe_channel.rs", "recv"),
    # spsc_ring_buffer.rs: send/recv directly use sealed Notify +
    # lock-free queue primitives. Their bodies are deliberate
    # fast paths (no Cx checkpoint cost) that nevertheless preserve
    # the structural seal — Notify is sealed in runtime_proof.rs, so
    # tokio types could not substitute. The `_with_cx` siblings
    # exist for callers who want responsive mid-flight cancel.
    ("spsc_ring_buffer.rs", "send"),
    ("spsc_ring_buffer.rs", "recv"),
    # ft-tau16: policy/security cluster (24 sites). Each delegates to
    # its `_with_cx` sibling (or to a chain that bottoms out at one).
    # mcp_helpers.rs's 5 sites are deferred to ft-039ky misc_tail since
    # they have no existing sibling and need siblings authored — outside
    # this subset's scope.
    # approval.rs: token-issuance + consume helpers.
    ("approval.rs", "issue"),
    ("approval.rs", "issue_for_plan"),
    ("approval.rs", "consume_for_plan"),
    ("approval.rs", "consume_for_plan_with_context"),
    ("approval.rs", "attach_to_decision"),
    ("approval.rs", "consume"),
    ("approval.rs", "consume_with_context"),
    # policy.rs: send_text / send_ctrl_* / send_control — the policy
    # injector entry points. Each constructs a default Cx and routes
    # through the gated `_with_cx` sibling.
    ("policy.rs", "send_text"),
    ("policy.rs", "send_ctrl_c"),
    ("policy.rs", "send_ctrl_d"),
    ("policy.rs", "send_ctrl_z"),
    ("policy.rs", "send_control"),
    # storage/handle/event_mutes.rs: per-pane event-mute CRUD.
    ("storage/handle/event_mutes.rs", "add_event_mute"),
    ("storage/handle/event_mutes.rs", "remove_event_mute"),
    ("storage/handle/event_mutes.rs", "is_event_muted"),
    ("storage/handle/event_mutes.rs", "list_active_mutes"),
    # caut.rs: account usage / refresh ergonomic wrappers.
    ("caut.rs", "usage"),
    ("caut.rs", "refresh"),
    # secrets.rs: storage scan for sensitive data.
    ("secrets.rs", "scan_storage"),
    ("secrets.rs", "scan_storage_incremental"),
    # watchdog.rs: lifecycle methods on the supervised task.
    ("watchdog.rs", "join"),
    ("watchdog.rs", "check"),
    # robot_sdk_contracts.rs: RPC dispatch entry points.
    ("robot_sdk_contracts.rs", "call"),
    ("robot_sdk_contracts.rs", "call_value"),
    # ft-ow8np: search/recorder cluster (35 sites). Files at 0
    # uncovered after this batch: cass.rs, recorder_migration.rs,
    # snapshot_engine.rs, recording.rs, sharding.rs, search_bridge.rs,
    # session_correlation.rs, session_restore.rs, session_retention.rs,
    # search_explain.rs.
    # cass.rs: CASS search + session-store query surface.
    ("cass.rs", "export_sessions"),
    ("cass.rs", "export_content"),
    ("cass.rs", "search"),
    ("cass.rs", "search_sessions"),
    ("cass.rs", "query_session"),
    ("cass.rs", "query"),
    ("cass.rs", "status"),
    ("cass.rs", "trigger_index"),
    # recorder_migration.rs: legacy-recorder M0..M5 phase helpers.
    ("recorder_migration.rs", "m0_preflight"),
    ("recorder_migration.rs", "m2_import"),
    ("recorder_migration.rs", "m3_checkpoint_sync"),
    ("recorder_migration.rs", "m5_cutover"),
    ("recorder_migration.rs", "run_m0_m2"),
    # snapshot_engine.rs: snapshot capture + retention/checkpoint.
    ("snapshot_engine.rs", "capture"),
    ("snapshot_engine.rs", "cleanup"),
    ("snapshot_engine.rs", "run_periodic"),
    ("snapshot_engine.rs", "shutdown_checkpoint"),
    ("snapshot_engine.rs", "mark_shutdown"),
    # recording.rs: pane-recording lifecycle.
    ("recording.rs", "start_recording"),
    ("recording.rs", "stop_recording"),
    ("recording.rs", "record_segment"),
    ("recording.rs", "record_event"),
    # sharding.rs: cross-shard pane fan-out helpers.
    ("sharding.rs", "spawn_with_hints"),
    ("sharding.rs", "list_all_panes"),
    ("sharding.rs", "shard_health_report"),
    ("sharding.rs", "shard_watchdog_warnings"),
    # search_bridge.rs: hybrid-search bridge entry points.
    ("search_bridge.rs", "cancelled"),
    ("search_bridge.rs", "search"),
    # session_correlation.rs: cass↔session join helpers.
    ("session_correlation.rs", "correlate_with_cass"),
    ("session_correlation.rs", "correlate_and_persist_for_pane"),
    ("session_correlation.rs", "refresh_cass_summary_for_session"),
    # session_restore.rs: layout/session restore on startup.
    ("session_restore.rs", "restore"),
    ("session_restore.rs", "detect_and_restore"),
    # session_retention.rs / search_explain.rs: single-fn files.
    ("session_retention.rs", "cleanup_sessions_async"),
    ("search_explain.rs", "build_explain_context"),
    # ft-039ky: misc 1–3-site long tail (40 entries). Each delegates
    # to its `_with_cx` sibling. The remaining 20-ish sites in the
    # tail (runtime.rs, mcp_helpers.rs, tailer.rs, vendored.rs, plus
    # the workflows/* stragglers tracked under ft-k42zv) need new
    # `_with_cx` siblings authored — outside this subset's scope and
    # filed as a follow-up bead.
    ("alerts.rs", "check_alerts"),
    ("cleanup.rs", "cleanup_apply"),
    ("cleanup.rs", "cleanup_preview"),
    ("cpu_pressure.rs", "run"),
    ("diagnostic.rs", "generate_bundle"),
    ("environment.rs", "detect"),
    ("event_stream.rs", "next"),
    ("event_stream.rs", "wait"),
    ("events.rs", "recv"),
    ("export.rs", "export_jsonl"),
    ("ingest.rs", "persist_captured_segment"),
    ("memory_budget.rs", "run"),
    ("memory_pressure.rs", "run"),
    ("metrics.rs", "start"),
    ("metrics.rs", "wait"),
    ("native_events.rs", "bind"),
    ("native_events.rs", "run"),
    ("notifications.rs", "handle_detection"),
    ("orphan_reaper.rs", "reap_orphans"),
    ("orphan_reaper.rs", "run_orphan_reaper"),
    ("protocol_recovery.rs", "execute"),
    ("replay.rs", "play"),
    ("replay.rs", "play_simple"),
    ("reports.rs", "generate_session_report"),
    ("restore_layout.rs", "restore"),
    ("restore_process.rs", "execute"),
    ("restore_scrollback.rs", "inject"),
    ("storage_telemetry.rs", "append_batch_instrumented"),
    ("survival.rs", "run"),
    ("telemetry.rs", "run"),
    ("ui_query.rs", "list_pane_bookmarks"),
    ("ui_query.rs", "list_saved_searches"),
    ("undo.rs", "execute"),
    ("web.rs", "shutdown"),
    ("web/server.rs", "run_web_server"),
    ("web/server.rs", "start_web_server"),
    ("web_framework.rs", "finish"),
    ("web_framework.rs", "start"),
    ("webhook.rs", "dispatch"),
    ("webhook.rs", "dispatch_payload"),
    # ft-tr5a0: missing-sibling tail — final 20 sites where a new
    # `_with_cx` sibling was authored alongside the original. Each
    # new sibling adds a pre-flight `cx.checkpoint()` and delegates
    # to the legacy body, preserving existing semantics while
    # giving callers a cancel-aware entry point.
    # vendored.rs (cfg(not(unix)) DirectMuxClient stub):
    ("vendored.rs", "connect"),
    # tailer.rs:
    ("tailer.rs", "join_next"),
    ("tailer.rs", "shutdown"),
    # runtime.rs (ObservationRuntime lifecycle):
    ("runtime.rs", "start"),
    ("runtime.rs", "write_queue_depth"),
    ("runtime.rs", "join"),
    ("runtime.rs", "shutdown"),
    ("runtime.rs", "shutdown_with_summary"),
    ("runtime.rs", "update_health_snapshot"),
    # mcp_helpers.rs:
    ("mcp_helpers.rs", "derive_osc_state_from_storage"),
    ("mcp_helpers.rs", "fetch_pane_state_from_ipc"),
    ("mcp_helpers.rs", "resolve_pane_capabilities"),
    ("mcp_helpers.rs", "record_mcp_audit"),
    # workflows stragglers (also referenced under ft-k42zv):
    ("workflows/account_steps.rs", "refresh_and_select_account"),
    ("workflows/account_steps.rs", "persist_caut_refresh_accounts"),
    ("workflows/account_steps.rs", "mark_account_used"),
    ("workflows/codex_exit.rs", "codex_exit_and_wait_for_summary"),
    ("workflows/codex_exit.rs", "persist_codex_session_summary"),
    ("workflows/wait_execution.rs", "execute"),
}

PUB_ASYNC_RE = re.compile(r"^\s*pub(?:\([^)]*\))? async fn\b")
# Cx in the signature (ref or owned) / impl RuntimeProof / : RuntimeProof.
# The `Cx` patterns admit:
#   &Cx, &mut Cx, &crate::cx::Cx, &self::cx::Cx                — borrowed
#   cx: Cx, : crate::cx::Cx                                    — owned
# The optional `mut` and intermediate `[a-z_]+::` segments cover the common
# fully-qualified forms threaded through frankenterm-core today. Matching
# happens on the *raw* signature text so whitespace inside the param list
# (line wraps, doc comments) is tolerated by the `\s*` runs.
#
# Owned `Cx` is just as sealed as `&Cx` (`impl RuntimeProof for Cx` lives
# in runtime_proof.rs). Either form satisfies the bead's acceptance.
COVERED_PATTERNS = [
    re.compile(r"&\s*(?:mut\s+)?(?:[a-z_][a-z0-9_]*\s*::\s*)*Cx\b"),
    re.compile(r":\s*(?:[a-z_][a-z0-9_]*\s*::\s*)*Cx\s*[,)<\s]"),
    re.compile(r"\bimpl\s+RuntimeProof\b"),
    re.compile(r":\s*RuntimeProof\b"),
]
FN_NAME_RE = re.compile(r"pub(?:\([^)]*\))? async fn\s+([A-Za-z_][A-Za-z0-9_]*)")


def signature_blocks(path: Path):
    """Yield (start_line_num, fn_name, signature_text) for each pub async fn."""
    text = path.read_text(encoding="utf-8", errors="replace")
    lines = text.splitlines()
    i = 0
    while i < len(lines):
        if PUB_ASYNC_RE.match(lines[i]):
            start = i
            buf = []
            ended = False
            while i < len(lines):
                line = lines[i]
                buf.append(line)
                if "{" in line or line.rstrip().endswith(";") or re.search(r"\bwhere\b", line):
                    if re.search(r"\bwhere\b", line):
                        # Consume up to the next `{` or `;`
                        j = i + 1
                        while j < len(lines) and "{" not in lines[j] and ";" not in lines[j]:
                            buf.append(lines[j])
                            j += 1
                        if j < len(lines):
                            buf.append(lines[j])
                            i = j
                    ended = True
                    break
                i += 1
            if not ended:
                return
            sig = "\n".join(buf)
            m = FN_NAME_RE.search(sig)
            name = m.group(1) if m else "<unknown>"
            yield start + 1, name, sig
        i += 1


def is_covered(sig: str) -> bool:
    return any(pat.search(sig) for pat in COVERED_PATTERNS)


def audit() -> dict:
    files = sorted(SRC_ROOT.rglob("*.rs"))
    results = {
        "total_sites": 0,
        "exempt_files_sites": 0,
        "wrapper_exempt_sites": 0,
        "covered_sites": 0,
        "uncovered_sites": 0,
        "by_file": {},
        "uncovered_examples": [],  # First N uncovered for debug
        "wrapper_audit_errors": [],
    }
    file_data: dict[str, dict] = {}
    for path in files:
        rel = path.relative_to(SRC_ROOT).as_posix()
        is_exempt_file = path.name in EXEMPT_FILES
        # Index function names per file so we can sanity-check the
        # WRAPPER_EXEMPTIONS allowlist.
        fn_names: set[str] = set()
        local_total = local_covered = local_uncovered = local_wrapper = 0
        local_uncovered_lines = []
        for line_no, name, sig in signature_blocks(path):
            results["total_sites"] += 1
            local_total += 1
            fn_names.add(name)
            if is_exempt_file:
                results["exempt_files_sites"] += 1
                continue
            if is_covered(sig):
                results["covered_sites"] += 1
                local_covered += 1
                continue
            if (rel, name) in WRAPPER_EXEMPTIONS:
                results["wrapper_exempt_sites"] += 1
                local_wrapper += 1
                continue
            results["uncovered_sites"] += 1
            local_uncovered += 1
            local_uncovered_lines.append((line_no, name))
            if len(results["uncovered_examples"]) < 25:
                results["uncovered_examples"].append({
                    "file": rel,
                    "line": line_no,
                    "fn": name,
                })
        # Cross-check wrapper allowlist: each (file, name) must point
        # at a real fn in this file whose name has a covered sibling.
        if not is_exempt_file:
            for ef, ename in WRAPPER_EXEMPTIONS:
                if ef != rel:
                    continue
                if ename not in fn_names:
                    results["wrapper_audit_errors"].append(
                        f"{rel}::{ename} listed in WRAPPER_EXEMPTIONS but no such pub async fn"
                    )
                    continue
                expected_siblings = [f"{ename}_with_cx", f"{ename}_cx"]
                if not any(s in fn_names for s in expected_siblings):
                    results["wrapper_audit_errors"].append(
                        f"{rel}::{ename} wrapper-exempt but no _with_cx/_cx sibling found"
                    )
        file_data[rel] = {
            "exempt_file": is_exempt_file,
            "total": local_total,
            "covered": local_covered,
            "uncovered": local_uncovered,
            "wrapper_exempt": local_wrapper,
            "uncovered_lines": local_uncovered_lines,
        }
    results["by_file"] = file_data
    return results


def load_baseline() -> dict | None:
    if not BASELINE_PATH.is_file():
        return None
    return json.loads(BASELINE_PATH.read_text())


def save_baseline(audit_data: dict) -> None:
    payload = {
        "comment": "ft-3kv6e ratchet baseline. Lower with each adoption commit. "
                   "Generated by scripts/check_runtime_proof_coverage.py.",
        "uncovered_sites": audit_data["uncovered_sites"],
        "covered_sites": audit_data["covered_sites"],
        "exempt_files_sites": audit_data["exempt_files_sites"],
        "wrapper_exempt_sites": audit_data["wrapper_exempt_sites"],
        "total_sites": audit_data["total_sites"],
        "by_file_uncovered": {
            f: data["uncovered"]
            for f, data in sorted(audit_data["by_file"].items())
            if data["uncovered"] > 0
        },
    }
    BASELINE_PATH.write_text(json.dumps(payload, indent=2, sort_keys=True) + "\n")


def main() -> int:
    p = argparse.ArgumentParser(description="ft-3kv6e RuntimeProof coverage audit")
    p.add_argument("--update-baseline", action="store_true",
                   help="Rewrite the baseline JSON to match current state.")
    p.add_argument("--json", action="store_true",
                   help="Emit a machine-readable JSON summary instead of human text.")
    p.add_argument("--summary", action="store_true",
                   help="Print only the headline numbers (no per-file detail).")
    args = p.parse_args()

    data = audit()

    if data["wrapper_audit_errors"]:
        print("ft-3kv6e: WRAPPER_EXEMPTIONS allowlist is inconsistent:", file=sys.stderr)
        for err in data["wrapper_audit_errors"]:
            print(f"  - {err}", file=sys.stderr)
        return 2

    if args.update_baseline:
        save_baseline(data)
        print(f"Baseline updated: uncovered={data['uncovered_sites']} "
              f"covered={data['covered_sites']} exempt={data['exempt_files_sites']}")
        return 0

    if args.json:
        print(json.dumps(data, indent=2, sort_keys=True))
        return 0

    print(f"ft-3kv6e RuntimeProof coverage audit")
    print(f"  total pub async fn      : {data['total_sites']}")
    print(f"  in exempt runtime files : {data['exempt_files_sites']}")
    print(f"  covered (Cx/RuntimeProof): {data['covered_sites']}")
    print(f"  wrapper-exempt          : {data['wrapper_exempt_sites']}")
    print(f"  uncovered               : {data['uncovered_sites']}")

    baseline = load_baseline()
    if baseline is None:
        print()
        print("WARNING: no baseline file at", BASELINE_PATH)
        print("Run with --update-baseline to seed it.")
        return 0

    baseline_uncovered = baseline.get("uncovered_sites", 0)
    print()
    print(f"Baseline (from {BASELINE_PATH.name}): uncovered={baseline_uncovered}")
    if data["uncovered_sites"] > baseline_uncovered:
        delta = data["uncovered_sites"] - baseline_uncovered
        print(f"FAIL: uncovered count grew by {delta} since baseline.", file=sys.stderr)
        if not args.summary:
            print("Newly-introduced sites likely include:", file=sys.stderr)
            for ex in data["uncovered_examples"][:15]:
                print(f"  {ex['file']}:{ex['line']} :: {ex['fn']}", file=sys.stderr)
        return 1
    if data["uncovered_sites"] < baseline_uncovered:
        delta = baseline_uncovered - data["uncovered_sites"]
        print(f"PROGRESS: uncovered count dropped by {delta}. "
              f"Re-run with --update-baseline in this commit.")
    else:
        print("Uncovered count matches baseline (no regression).")
    return 0


if __name__ == "__main__":
    sys.exit(main())
