#!/usr/bin/env python3
"""ft-3kv6e — RuntimeProof coverage audit for frankenterm-core.

Walks `crates/frankenterm-core/src/` and classifies every `pub async fn`
site into a fail-closed census. A site is *covered* only when one direct
call argument carries one of:

  * `&Cx` / `&mut Cx` parameter (Cx is sealed — see `runtime_proof.rs`)
  * `impl RuntimeProof`
  * `: RuntimeProof` (generic bound)

A site is *runtime-layer exempt* if it lives in one of the runtime-layer
files listed in `EXEMPT_FILES` — those modules ARE the seal and cannot
take a `RuntimeProof` bound on themselves.

Narrow ambient-Cx wrappers and fresh-context cleanup adapters have separate,
body-validated allowlists and separate census categories. Nested types such as
`Option<&Cx>` and `impl FnOnce(&Cx)` are not proof arguments.

The script ratchets a baseline at `tests/runtime_proof_coverage_baseline.json`.
A run fails (exit 1) on uncovered growth or an aggregate/category/per-file
census collapse. Update the baseline only alongside an audited source change.

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
from collections import Counter
from dataclasses import dataclass
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
# considered transitively covered only when its parsed body constructs an
# ambient `Cx` and awaits the exact covered sibling once.
#
# Entries here MUST be paired with a real covered sibling — i.e. the same
# file must contain `pub async fn <name>_with_cx(cx: &Cx, ...)` or
# `pub async fn <name>_cx(cx: &Cx, ...)`. The audit cross-checks this so
# the allowlist can't drift into a silent escape hatch.
WRAPPER_EXEMPTIONS: set[tuple[str, str]] = {
    # ft-4ku44: ipc.rs ergonomic wrappers. Each entry below is a
    # non-Cx public method whose body constructs a default `Cx`
    # (`Cx::current().unwrap_or_else(for_request)`) and delegates to
    # the `_with_cx` sibling. Platform stubs follow the same rule so a
    # covered implementation under one cfg cannot hide a non-Cx variant.
    ("ipc.rs", "bind"),
    ("ipc.rs", "connect"),
    ("ipc.rs", "accept"),
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
    # ft-dit9w: workflows cluster ergonomic wrappers. Each entry below
    # constructs an ambient Cx and awaits its exact `_with_cx`/`_cx` sibling.
    ("workflows/context.rs", "send_text"),
    ("workflows/context.rs", "send_ctrl_c"),
    ("workflows/context.rs", "send_ctrl_d"),
    ("workflows/context.rs", "send_ctrl_z"),
    ("workflows/context.rs", "send_verified"),
    # workflows/engine.rs WorkflowEngine methods: each delegates to its
    # `_cx` sibling.
    ("workflows/engine.rs", "start"),
    ("workflows/engine.rs", "start_with_id"),
    ("workflows/engine.rs", "resume"),
    ("workflows/engine.rs", "find_incomplete"),
    ("workflows/engine.rs", "update_status"),
    ("workflows/engine.rs", "log_step"),
    # workflows/runner.rs WorkflowRunner methods: each delegates to its
    # `_with_cx` sibling.
    ("workflows/runner.rs", "handle_detection"),
    ("workflows/runner.rs", "run_workflow"),
    ("workflows/runner.rs", "run"),
    ("workflows/runner.rs", "resume_incomplete"),
    ("workflows/runner.rs", "abort_execution"),
    # workflows/plan_helpers.rs: delegates to `check_step_idempotency_with_cx`.
    ("workflows/plan_helpers.rs", "check_step_idempotency"),
    # ft-7xbaz: wezterm.rs ergonomic wrappers. Every entry below constructs
    # an ambient Cx and awaits its exact `_with_cx` sibling.
    ("wezterm.rs", "list_panes"),
    ("wezterm.rs", "get_pane"),
    ("wezterm.rs", "get_text"),
    ("wezterm.rs", "get_semantic_zones"),
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
    # public method constructs an ambient Cx and awaits its exact
    # `_with_cx` sibling.
    ("vendored/mux_client.rs", "connect"),
    ("vendored/mux_client.rs", "list_panes"),
    ("vendored/mux_client.rs", "spawn_v2"),
    ("vendored/mux_client.rs", "split_pane"),
    ("vendored/mux_client.rs", "get_pane_render_changes"),
    ("vendored/mux_client.rs", "get_semantic_zones"),
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
    # ft-7xbaz: vendored/mux_pool.rs ergonomic wrappers. Each pool-level
    # method constructs an ambient Cx and awaits its exact `_with_cx` sibling.
    ("vendored/mux_pool.rs", "list_panes"),
    ("vendored/mux_pool.rs", "spawn_v2"),
    ("vendored/mux_pool.rs", "split_pane"),
    ("vendored/mux_pool.rs", "get_lines"),
    ("vendored/mux_pool.rs", "get_pane_render_changes"),
    ("vendored/mux_pool.rs", "get_semantic_zones"),
    ("vendored/mux_pool.rs", "get_pane_render_changes_batch"),
    ("vendored/mux_pool.rs", "write_to_pane"),
    ("vendored/mux_pool.rs", "send_paste"),
    ("vendored/mux_pool.rs", "health_check"),
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
    # br-ft-dngp2 / ft-43lpu.cont: agent_profiles async surface.
    # Each public method delegates to its `_with_cx` sibling
    # via the canonical Cx::current().unwrap_or_else(for_request)
    # bridge — same pattern as insert_pane_bookmark above.
    ("storage.rs", "insert_agent_profile"),
    ("storage.rs", "get_agent_profile"),
    ("storage.rs", "list_agent_profiles"),
    ("storage.rs", "delete_agent_profile"),
    ("storage.rs", "prune_segments_before"),
    ("storage.rs", "retention_cleanup"),
    ("storage.rs", "record_usage_metric"),
    ("storage.rs", "count_events"),
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
    # 2026-08-05 live-census reconciliation. These newer StorageHandle
    # entry points all construct the ambient project Cx and immediately
    # delegate to the exact `_with_cx` sibling. Keep the names explicit so
    # the sibling audit fails loudly if either half is renamed or removed.
    ("storage.rs", "append_segment_with_zone"),
    ("storage.rs", "record_event_outcome"),
    ("storage.rs", "reserve_event_delivery"),
    ("storage.rs", "finalize_event_delivery"),
    ("storage.rs", "release_event_delivery"),
    ("storage.rs", "finalize_event_delivery_leases_bulk"),
    ("storage.rs", "release_event_delivery_leases_bulk"),
    ("storage.rs", "get_event_annotations_bulk"),
    ("storage.rs", "update_audit_action_submit_receipt"),
    ("storage.rs", "purge_operational_maintenance_before"),
    ("storage.rs", "get_config_value"),
    ("storage.rs", "set_config_value"),
    ("storage.rs", "enforce_size_limit"),
    ("storage.rs", "count_events_by_retention_rule"),
    ("storage.rs", "count_operational_maintenance_before"),
    ("storage.rs", "delete_events_by_retention_rule"),
    ("storage.rs", "consume_approval_token_by_id"),
    ("storage.rs", "get_events_stream_page"),
    ("storage.rs", "get_event_retention_snapshot"),
    ("storage.rs", "check_event_retention"),
    ("storage.rs", "check_event_retention_in_epoch"),
    ("storage.rs", "count_unhandled_events_by_pane_bulk"),
    ("storage.rs", "list_approval_tokens"),
    ("storage.rs", "get_approval_token_by_id"),
    ("storage.rs", "pane_last_output_at"),
    ("storage.rs", "pane_last_output_at_bulk"),
    ("storage.rs", "upsert_limit_window"),
    ("storage.rs", "get_limit_window"),
    ("storage.rs", "list_active_limit_windows"),
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
    # retry.rs: each retry wrapper awaits its exact `_cx` sibling.
    ("retry.rs", "with_retry"),
    ("retry.rs", "with_retry_outcome"),
    ("retry.rs", "with_retry_and_circuit"),
    ("retry.rs", "with_smart_retry"),
    # pool.rs: legacy acquire-only ergonomic wrappers around the `_with_cx`
    # siblings. Fallible return/maintenance operations deliberately have no
    # ambient wrappers.
    ("pool.rs", "try_acquire"),
    ("pool.rs", "acquire"),
    # cancellation_safe_channel.rs: the non-cx reserve/recv have
    # `_with_cx` siblings that gate the underlying mpsc primitive.
    ("cancellation_safe_channel.rs", "reserve"),
    ("cancellation_safe_channel.rs", "recv"),
    # spsc_ring_buffer.rs: send/recv construct an ambient Cx and await
    # their exact `_with_cx` siblings, preserving one canonical queue path.
    ("spsc_ring_buffer.rs", "send"),
    ("spsc_ring_buffer.rs", "recv"),
    # ft-tau16: policy/security cluster. Each ambient wrapper awaits its
    # exact `_with_cx` sibling.
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
    ("cass.rs", "is_session_indexed"),
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
    ("snapshot_engine.rs", "capture_with_options"),
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
    # ft-039ky: misc 1–3-site long tail. Each ambient wrapper awaits its
    # exact `_with_cx`/`_cx` sibling.
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
    ("ingest.rs", "persist_captured_segment_with_zone"),
    ("memory_budget.rs", "run"),
    ("memory_pressure.rs", "run"),
    ("metrics.rs", "start"),
    ("metrics.rs", "wait"),
    ("native_events.rs", "bind"),
    ("native_events.rs", "connect"),
    ("native_events.rs", "accept"),
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
    ("workflows/builtin_workflows.rs", "record_limit_window_for_trigger"),
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
    # ft-tr5a0: former missing-sibling tail. Each ambient wrapper now
    # constructs a Cx and awaits the exact Cx-bearing sibling; the sibling
    # owns the implementation so delegation cannot recurse back.
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
    ("runtime.rs", "shutdown_with_timeout"),
    ("runtime.rs", "update_health_snapshot"),
    # ft-e87u6.13: vendored mux geometry wrappers delegate through their
    # explicit-Cx siblings, matching the rest of the DirectMuxClient
    # ergonomic surface.
    ("vendored/mux_client.rs", "resize"),
    ("vendored/mux_client.rs", "adjust_pane_size"),
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

# These ordinary wrappers are deliberately stricter than the historical
# ergonomic surface: they refuse to mint a fresh request context when no
# ambient capability is installed.  Their bodies must bind the result of
# `Cx::current().ok_or{,_else}(...) ?` and then await the exact Cx sibling.
# They remain part of the ordinary-wrapper census so tightening a wrapper does
# not make the ratchet appear to have gained or lost a covered public API.
REQUIRED_AMBIENT_CX_WRAPPERS: set[tuple[str, str]] = {
    ("native_events.rs", "bind"),
    ("native_events.rs", "connect"),
    ("native_events.rs", "accept"),
    ("native_events.rs", "run"),
}

# Public cleanup/snapshot adapters whose contract intentionally uses a fresh,
# independent request context instead of inheriting ambient caller
# cancellation. These are NOT ordinary ergonomic wrappers: each entry must be
# exactly `let cx = ...for_request(); sibling(&cx, ...).await.expect("...")`,
# with the exact expected panic message recorded here. Keeping this category
# separate makes independent cleanup semantics visible in the census and
# prevents it from becoming a general post-await escape hatch. The current
# tree has no intentional production escape: this empty map is retained as a
# fail-closed grammar and category ratchet for any future proposal.
INDEPENDENT_CONTEXT_ADAPTERS: dict[tuple[str, str], str] = {
}

RAW_STRING_RE = re.compile(r"(?:br|cr|r)(?P<hashes>#{0,255})\"")
IDENT_RE = re.compile(r"(?:r#)?[^\W\d]\w*", re.UNICODE)
TOKEN_RE = re.compile(
    r"r#[^\W\d]\w*|[^\W\d]\w*|::|->|=>|\.\.=|\.\.|"
    r"==|!=|<=|>=|&&|\|\||\+=|-=|\*=|/=|%=|&=|\|=|\^=|[^\s]",
    re.UNICODE,
)

# This checker deliberately recognizes a narrow source grammar. It does not
# claim that a Python lexer is a Rust semantic frontend. In particular,
# attribute/procedural macros can synthesize APIs that have no source-declared
# `pub async fn` token sequence. Macro token bodies are excluded below, and an
# obvious macro-contained public async declaration is a hard audit error.
NEGATIVE_EVIDENCE = [
    "source-declared APIs only; procedural/attribute macro expansions require compiler-side proof",
    "type and module path resolution is lexical; shadowing/aliases require compiler-side proof",
    "proof parameters must be direct values; nested callback/container Cx types are rejected",
    "wrapper proof accepts only canonical ambient-Cx binding plus sibling await and optional literal expect grammar",
    "required-ambient wrapper proof accepts only Cx::current fail-closed acquisition plus the exact Cx sibling await",
    "required-ambient error-helper resolution is lexical; direct for_request constructors are rejected, while aliased/helper-body semantics require compiler-side review",
    "independent cleanup proof accepts only fresh for_request plus an exact literal expect adapter",
]


@dataclass(frozen=True)
class Token:
    value: str
    start: int
    end: int


@dataclass(frozen=True)
class FunctionSite:
    name: str
    line: int
    start: int
    signature: str
    param_text: str
    body: str | None
    raw_body: str | None
    scope: tuple[int, ...]
    cfg_key: tuple[tuple[str, ...], ...]


@dataclass(frozen=True)
class WrapperCall:
    sibling_name: str
    cx_arg_index: int


def _normal_name(value: str) -> str:
    return value[2:] if value.startswith("r#") else value


def _blank(chars: list[str], start: int, end: int) -> None:
    for index in range(start, min(end, len(chars))):
        if chars[index] not in "\r\n":
            chars[index] = " "


def _char_literal_end(text: str, start: int) -> int | None:
    """Return one-past a valid-looking Rust character literal, else None."""
    cursor = start + 1
    if cursor >= len(text) or text[cursor] in "\r\n":
        return None
    if text[cursor] == "\\":
        cursor += 1
        if cursor >= len(text):
            return None
        if text[cursor] == "x":
            cursor += 3
        elif text[cursor] == "u" and cursor + 1 < len(text) and text[cursor + 1] == "{":
            closing = text.find("}", cursor + 2)
            if closing < 0:
                return None
            cursor = closing + 1
        else:
            cursor += 1
    else:
        cursor += 1
    return cursor + 1 if cursor < len(text) and text[cursor] == "'" else None


def sanitize_rust(source: str) -> tuple[str, list[str]]:
    """Blank comments/literals while preserving every offset and newline."""
    chars = list(source)
    errors: list[str] = []
    cursor = 0
    while cursor < len(source):
        if source.startswith("//", cursor):
            end = source.find("\n", cursor + 2)
            end = len(source) if end < 0 else end
            _blank(chars, cursor, end)
            cursor = end
            continue
        if source.startswith("/*", cursor):
            depth = 1
            end = cursor + 2
            while end < len(source) and depth:
                if source.startswith("/*", end):
                    depth += 1
                    end += 2
                elif source.startswith("*/", end):
                    depth -= 1
                    end += 2
                else:
                    end += 1
            if depth:
                errors.append(f"unterminated block comment at offset {cursor}")
                end = len(source)
            _blank(chars, cursor, end)
            cursor = end
            continue

        raw = None
        if source[cursor] in "bcr" and (
            cursor == 0 or not (source[cursor - 1].isalnum() or source[cursor - 1] in "_#")
        ):
            raw = RAW_STRING_RE.match(source, cursor)
        if raw:
            terminator = '"' + raw.group("hashes")
            closing = source.find(terminator, raw.end())
            if closing < 0:
                errors.append(f"unterminated raw string at offset {cursor}")
                end = len(source)
            else:
                end = closing + len(terminator)
            _blank(chars, cursor, end)
            cursor = end
            continue

        string_prefix = 1 if source.startswith(("b\"", "c\""), cursor) else 0
        if source[cursor + string_prefix : cursor + string_prefix + 1] == '"':
            end = cursor + string_prefix + 1
            closed = False
            while end < len(source):
                if source[end] == "\\":
                    end += 2
                elif source[end] == '"':
                    end += 1
                    closed = True
                    break
                else:
                    end += 1
            if not closed:
                errors.append(f"unterminated string literal at offset {cursor}")
            _blank(chars, cursor, min(end, len(source)))
            cursor = min(end, len(source))
            continue

        char_start = cursor + 1 if source.startswith("b'", cursor) else cursor
        if source[char_start : char_start + 1] == "'":
            char_end = _char_literal_end(source, char_start)
            if char_end is not None:
                _blank(chars, cursor, char_end)
                cursor = char_end
                continue
        cursor += 1
    return "".join(chars), errors


def tokenize(code: str) -> list[Token]:
    return [Token(match.group(0), match.start(), match.end()) for match in TOKEN_RE.finditer(code)]


def delimiter_pairs(tokens: list[Token]) -> tuple[dict[int, int], list[str]]:
    pairs: dict[int, int] = {}
    errors: list[str] = []
    stack: list[tuple[str, int]] = []
    closing_for = {"(": ")", "[": "]", "{": "}"}
    opening_for = {value: key for key, value in closing_for.items()}
    for index, token in enumerate(tokens):
        if token.value in closing_for:
            stack.append((token.value, index))
        elif token.value in opening_for:
            expected = opening_for[token.value]
            if not stack or stack[-1][0] != expected:
                errors.append(f"unmatched {token.value!r} at offset {token.start}")
                continue
            _, opening = stack.pop()
            pairs[opening] = index
            pairs[index] = opening
    for value, index in stack:
        errors.append(f"unclosed {value!r} at offset {tokens[index].start}")
    return pairs, errors


def _macro_mentions_pub_async_fn(tokens: list[Token], start: int, end: int) -> bool:
    values = [_normal_name(token.value) for token in tokens[start:end]]
    for index, value in enumerate(values):
        if value != "pub" and not (value == "$" and index + 1 < len(values) and values[index + 1] == "vis"):
            continue
        tail = values[index + 1 : index + 24]
        if "async" in tail:
            async_index = tail.index("async")
            if "fn" in tail[async_index + 1 :]:
                return True
    return False


def mask_macro_token_bodies(code: str) -> tuple[str, list[str]]:
    """Exclude macro token bodies from the source-declared function census."""
    tokens = tokenize(code)
    pairs, errors = delimiter_pairs(tokens)
    ranges: list[tuple[int, int]] = []
    openings = {"(", "[", "{"}
    for index, token in enumerate(tokens):
        if token.value != "!" or index == 0:
            continue
        previous = _normal_name(tokens[index - 1].value)
        if not IDENT_RE.fullmatch(tokens[index - 1].value):
            continue
        group_index = index + 1
        if previous == "macro_rules":
            group_index += 1  # Skip the macro name.
        if group_index >= len(tokens) or tokens[group_index].value not in openings:
            continue
        closing = pairs.get(group_index)
        if closing is None:
            errors.append(f"unparseable macro token body at offset {tokens[group_index].start}")
            continue
        if _macro_mentions_pub_async_fn(tokens, group_index + 1, closing):
            errors.append(
                f"macro token body at offset {tokens[group_index].start} contains an ambiguous "
                "public async function declaration"
            )
        ranges.append((tokens[group_index].start, tokens[closing].end))

    # Declarative macro 2.0 has no `!`; mask its template body too.
    for index, token in enumerate(tokens):
        if _normal_name(token.value) != "macro" or index + 2 >= len(tokens):
            continue
        if not IDENT_RE.fullmatch(tokens[index + 1].value):
            continue
        group_index = index + 2
        if tokens[group_index].value not in openings:
            continue
        first_close = pairs.get(group_index)
        if first_close is None:
            continue
        body_index = first_close + 1
        if body_index >= len(tokens) or tokens[body_index].value not in openings:
            continue
        body_close = pairs.get(body_index)
        if body_close is None:
            continue
        if _macro_mentions_pub_async_fn(tokens, body_index + 1, body_close):
            errors.append(
                f"macro definition at offset {token.start} contains an ambiguous public async "
                "function declaration"
            )
        ranges.append((tokens[body_index].start, tokens[body_close].end))

    chars = list(code)
    for start, end in ranges:
        _blank(chars, start, end)
    return "".join(chars), errors


def _cfg_key(tokens: list[Token], pairs: dict[int, int], pub_index: int) -> tuple[tuple[str, ...], ...]:
    attributes: list[tuple[str, ...]] = []
    cursor = pub_index - 1
    while cursor >= 0 and tokens[cursor].value == "]":
        opening = pairs.get(cursor)
        if opening is None or opening == 0 or tokens[opening - 1].value != "#":
            break
        content = tuple(_normal_name(token.value) for token in tokens[opening + 1 : cursor])
        if content and content[0] in {"cfg", "cfg_attr"}:
            attributes.append(content)
        cursor = opening - 2
    attributes.reverse()
    return tuple(attributes)


def discover_functions(source: str) -> tuple[list[FunctionSite], list[str]]:
    """Discover source-declared public async functions without scanning macro bodies."""
    clean, errors = sanitize_rust(source)
    scan_code, macro_errors = mask_macro_token_bodies(clean)
    errors.extend(macro_errors)
    tokens = tokenize(scan_code)
    pairs, pair_errors = delimiter_pairs(tokens)
    errors.extend(pair_errors)

    scopes: dict[int, tuple[int, ...]] = {}
    brace_stack: list[int] = []
    for index, token in enumerate(tokens):
        if token.value == "}":
            opening = pairs.get(index)
            if brace_stack and brace_stack[-1] == opening:
                brace_stack.pop()
        if _normal_name(token.value) == "pub":
            scopes[index] = tuple(tokens[opening].start for opening in brace_stack)
        if token.value == "{":
            brace_stack.append(index)

    sites: list[FunctionSite] = []
    qualifiers = {"default", "const", "async", "unsafe", "extern"}
    openings = {"(", "["}
    for pub_index, pub_token in enumerate(tokens):
        if _normal_name(pub_token.value) != "pub":
            continue
        cursor = pub_index + 1
        if cursor < len(tokens) and tokens[cursor].value == "(":
            closing = pairs.get(cursor)
            if closing is None:
                errors.append(f"unparseable visibility at offset {pub_token.start}")
                continue
            cursor = closing + 1

        saw_async = False
        while cursor < len(tokens) and _normal_name(tokens[cursor].value) in qualifiers:
            saw_async |= _normal_name(tokens[cursor].value) == "async"
            cursor += 1
        if cursor >= len(tokens) or _normal_name(tokens[cursor].value) != "fn":
            if saw_async:
                errors.append(f"ambiguous public async item at offset {pub_token.start}")
            continue
        if not saw_async:
            continue
        fn_index = cursor
        cursor += 1
        if cursor >= len(tokens) or not IDENT_RE.fullmatch(tokens[cursor].value):
            errors.append(f"public async fn at offset {pub_token.start} has no parseable name")
            continue
        name = _normal_name(tokens[cursor].value)
        cursor += 1

        angle_depth = 0
        param_open = None
        while cursor < len(tokens):
            value = tokens[cursor].value
            if value == "<":
                angle_depth += 1
            elif value == ">" and angle_depth:
                angle_depth -= 1
            elif value == "(" and angle_depth == 0:
                param_open = cursor
                break
            elif value in {"{", ";"} and angle_depth == 0:
                break
            cursor += 1
        if param_open is None or param_open not in pairs:
            errors.append(f"public async fn {name} at offset {pub_token.start} has no parameter list")
            continue
        param_close = pairs[param_open]

        cursor = param_close + 1
        angle_depth = 0
        body_open = body_close = terminator = None
        while cursor < len(tokens):
            value = tokens[cursor].value
            if value == "<":
                angle_depth += 1
                cursor += 1
                continue
            if value == ">" and angle_depth:
                angle_depth -= 1
                cursor += 1
                continue
            if value in openings:
                closing = pairs.get(cursor)
                if closing is None:
                    break
                cursor = closing + 1
                continue
            if value == "{" and angle_depth:
                closing = pairs.get(cursor)
                if closing is None:
                    break
                cursor = closing + 1
                continue
            if value == "{" and angle_depth == 0:
                body_open = cursor
                body_close = pairs.get(cursor)
                break
            if value == ";" and angle_depth == 0:
                terminator = cursor
                break
            cursor += 1
        if body_open is None and terminator is None:
            errors.append(f"public async fn {name} at offset {pub_token.start} has no body terminator")
            continue
        if body_open is not None and body_close is None:
            errors.append(f"public async fn {name} at offset {pub_token.start} has an unclosed body")
            continue

        signature_end = tokens[body_open if body_open is not None else terminator].start
        body = None
        raw_body = None
        if body_open is not None and body_close is not None:
            body = clean[tokens[body_open].end : tokens[body_close].start]
            raw_body = source[tokens[body_open].end : tokens[body_close].start]
        sites.append(
            FunctionSite(
                name=name,
                line=source.count("\n", 0, pub_token.start) + 1,
                start=pub_token.start,
                signature=clean[pub_token.start:signature_end],
                param_text=clean[tokens[param_open].end : tokens[param_close].start],
                body=body,
                raw_body=raw_body,
                scope=scopes.get(pub_index, ()),
                cfg_key=_cfg_key(tokens, pairs, pub_index),
            )
        )
    return sites, errors


def _split_top_level(tokens: list[Token], separator: str) -> list[list[Token]]:
    chunks: list[list[Token]] = [[]]
    stack: list[str] = []
    closing = {")": "(", "]": "[", "}": "{"}
    angle_depth = 0
    for token in tokens:
        value = token.value
        if value in {"(", "[", "{"}:
            stack.append(value)
        elif value in closing and stack and stack[-1] == closing[value]:
            stack.pop()
        elif value == "<":
            angle_depth += 1
        elif value == ">" and angle_depth:
            angle_depth -= 1
        if value == separator and not stack and angle_depth == 0:
            chunks.append([])
        else:
            chunks[-1].append(token)
    return chunks


def _matching_angle_close(tokens: list[Token], opening: int) -> int | None:
    """Return the matching `>` for one generic `<`, conservatively."""
    depth = 0
    stack: list[str] = []
    closing = {")": "(", "]": "[", "}": "{"}
    for index in range(opening, len(tokens)):
        value = tokens[index].value
        if value in {"(", "[", "{"}:
            stack.append(value)
            continue
        if value in closing:
            if stack and stack[-1] == closing[value]:
                stack.pop()
            continue
        if stack:
            continue
        if value == "<":
            depth += 1
        elif value == ">":
            depth -= 1
            if depth == 0:
                return index
    return None


def _has_direct_runtime_proof_bound(tokens: list[Token]) -> bool:
    """Recognize `RuntimeProof` as a top-level trait bound, not a nested type."""
    for bound in _split_top_level(tokens, "+"):
        segments = _path_segments([_normal_name(token.value) for token in bound])
        if segments and segments[-1] == "RuntimeProof":
            return True
    return False


def _bound_generic_name(tokens: list[Token]) -> str | None:
    """Return `P` from one exact `P: ... + RuntimeProof` predicate."""
    parts = _split_top_level(tokens, ":")
    if len(parts) != 2 or len(parts[0]) != 1:
        return None
    name_token = parts[0][0]
    if not IDENT_RE.fullmatch(name_token.value):
        return None
    bounds_and_default = _split_top_level(parts[1], "=")
    if not bounds_and_default or not _has_direct_runtime_proof_bound(bounds_and_default[0]):
        return None
    return _normal_name(name_token.value)


def _runtime_proof_generic_names(signature: str) -> set[str]:
    """Collect directly RuntimeProof-bounded type parameters from a signature."""
    tokens = tokenize(signature)
    names: set[str] = set()
    fn_index = next(
        (
            index
            for index, token in enumerate(tokens)
            if _normal_name(token.value) == "fn"
        ),
        None,
    )
    if fn_index is None or fn_index + 2 >= len(tokens):
        return names

    cursor = fn_index + 2
    if tokens[cursor].value == "<":
        generic_close = _matching_angle_close(tokens, cursor)
        if generic_close is None:
            return names
        for parameter in _split_top_level(tokens[cursor + 1 : generic_close], ","):
            name = _bound_generic_name(parameter)
            if name is not None:
                names.add(name)
        cursor = generic_close + 1

    pairs, _ = delimiter_pairs(tokens)
    while cursor < len(tokens) and tokens[cursor].value != "(":
        cursor += 1
    if cursor >= len(tokens):
        return names
    param_close = pairs.get(cursor)
    if param_close is None:
        return names
    where_index = next(
        (
            index
            for index in range(param_close + 1, len(tokens))
            if tokens[index].value == "where"
        ),
        None,
    )
    if where_index is None:
        return names
    for predicate in _split_top_level(tokens[where_index + 1 :], ","):
        name = _bound_generic_name(predicate)
        if name is not None:
            names.add(name)
    return names


def _strip_reference_prefix(tokens: list[Token]) -> list[Token]:
    """Strip one direct Rust reference prefix, including lifetime and `mut`."""
    if not tokens or tokens[0].value != "&":
        return tokens
    cursor = 1
    if (
        cursor + 1 < len(tokens)
        and tokens[cursor].value == "'"
        and IDENT_RE.fullmatch(tokens[cursor + 1].value)
    ):
        cursor += 2
    if cursor < len(tokens) and _normal_name(tokens[cursor].value) == "mut":
        cursor += 1
    return tokens[cursor:]


def _is_direct_proof_type(tokens: list[Token], generic_names: set[str]) -> bool:
    """Require the call argument itself to carry Cx/RuntimeProof evidence."""
    direct = _strip_reference_prefix(tokens)
    values = [_normal_name(token.value) for token in direct]
    segments = _path_segments(values)
    if segments and segments[-1] == "Cx":
        return True
    if len(values) == 1 and values[0] in generic_names:
        return True
    if values[:1] in (["impl"], ["dyn"]):
        return _has_direct_runtime_proof_bound(direct[1:])
    return False


def _is_receiver_parameter(tokens: list[Token]) -> bool:
    """Recognize Rust receiver syntax without treating `self::Type` as a receiver."""
    declaration = _split_top_level(tokens, ":")
    receiver = declaration[0] if len(declaration) == 2 else tokens
    values = [_normal_name(token.value) for token in receiver]
    if not values or values[-1] != "self":
        return False
    prefix = values[:-1]
    if prefix in ([], ["mut"], ["&"], ["&", "mut"]):
        return True
    return (
        len(prefix) in {3, 4}
        and prefix[:1] == ["&"]
        and prefix[1] == "'"
        and IDENT_RE.fullmatch(prefix[2]) is not None
        and (len(prefix) == 3 or prefix[3] == "mut")
    )


def proof_param_positions(site: FunctionSite) -> tuple[int, ...]:
    """Return call-argument positions that carry a concrete proof value."""
    param_tokens = tokenize(site.param_text)
    chunks = _split_top_level(param_tokens, ",")
    generic_bounds = _runtime_proof_generic_names(site.signature)
    positions: list[int] = []
    call_position = 0
    for chunk in chunks:
        if not chunk:
            continue
        if _is_receiver_parameter(chunk):
            continue
        declaration = _split_top_level(chunk, ":")
        if len(declaration) == 2 and _is_direct_proof_type(declaration[1], generic_bounds):
            positions.append(call_position)
        call_position += 1
    return tuple(positions)


def is_covered(site: FunctionSite) -> bool:
    return bool(proof_param_positions(site))


def _cfg_predicate(key: tuple[tuple[str, ...], ...]) -> tuple[str, ...] | None:
    if len(key) != 1:
        return None
    attribute = key[0]
    if len(attribute) < 4 or attribute[0] != "cfg" or attribute[1] != "(" or attribute[-1] != ")":
        return None
    return attribute[2:-1]


def _cfg_predicates_are_complements(
    left: tuple[tuple[str, ...], ...], right: tuple[tuple[str, ...], ...]
) -> bool:
    left_predicate = _cfg_predicate(left)
    right_predicate = _cfg_predicate(right)
    if left_predicate is None or right_predicate is None:
        return False

    def is_not_of(candidate: tuple[str, ...], base: tuple[str, ...]) -> bool:
        return (
            len(candidate) == len(base) + 3
            and candidate[:2] == ("not", "(")
            and candidate[-1:] == (")",)
            and candidate[2:-1] == base
        )

    return is_not_of(left_predicate, right_predicate) or is_not_of(
        right_predicate, left_predicate
    )


def _select_cfg_siblings(
    wrapper: FunctionSite, candidates: list[FunctionSite]
) -> list[FunctionSite]:
    """Select one sibling or an exact `cfg(P)`/`cfg(not(P))` partition."""
    exact = [site for site in candidates if site.cfg_key == wrapper.cfg_key]
    if len(exact) == 1:
        return exact
    if exact:
        return []
    unconditional = [site for site in candidates if not site.cfg_key]
    if wrapper.cfg_key and len(unconditional) == 1:
        return unconditional
    if wrapper.cfg_key or unconditional or len(candidates) != 2:
        return []
    if _cfg_predicates_are_complements(candidates[0].cfg_key, candidates[1].cfg_key):
        return candidates
    return []


def _path_segments(values: list[str]) -> list[str] | None:
    if not values or len(values) % 2 == 0:
        return None
    segments: list[str] = []
    for index, value in enumerate(values):
        if index % 2:
            if value != "::":
                return None
        else:
            if not IDENT_RE.fullmatch(value):
                return None
            segments.append(_normal_name(value))
    return segments


def _is_for_request_path(values: list[str]) -> bool:
    segments = _path_segments(values)
    return tuple(segments or ()) in {
        ("cx", "for_request"),
        ("Cx", "for_request"),
        ("crate", "cx", "for_request"),
        ("crate", "cx", "Cx", "for_request"),
    }


def _is_current_path(values: list[str]) -> bool:
    segments = _path_segments(values)
    return tuple(segments or ()) in {
        ("Cx", "current"),
        ("cx", "Cx", "current"),
        ("crate", "cx", "Cx", "current"),
    }


def _is_ambient_cx_expr(tokens: list[Token]) -> bool:
    values = [token.value for token in tokens]
    # Direct canonical constructor: Cx::for_request() or crate::cx::for_request().
    if len(values) >= 3 and values[-2:] == ["(", ")"]:
        if _is_for_request_path(values[:-2]):
            return True

    try:
        first_open = values.index("(")
    except ValueError:
        return False
    if not _is_current_path(values[:first_open]):
        return False
    if values[first_open : first_open + 4] != ["(", ")", ".", "unwrap_or_else"]:
        return False
    rest = values[first_open + 4 :]
    if len(rest) < 3 or rest[0] != "(" or rest[-1] != ")":
        return False
    fallback = rest[1:-1]
    if fallback[:1] == ["||"]:
        fallback = fallback[1:]
    elif fallback[:2] == ["|", "|"]:
        fallback = fallback[2:]
    if len(fallback) >= 2 and fallback[-2:] == ["(", ")"]:
        fallback = fallback[:-2]
    return _is_for_request_path(fallback)


def _is_required_ambient_cx_expr(tokens: list[Token]) -> bool:
    """Accept one fail-closed `Cx::current().ok_or{,_else}(...) ?` expression.

    The error expression is intentionally narrow: either one qualified enum
    variant/path for `ok_or`, or a zero-argument closure that calls one
    qualified helper for `ok_or_else`. String literals are blanked by the
    source sanitizer, so a helper invocation with one literal argument has the
    same token shape as an empty argument list here.
    """
    values = [token.value for token in tokens]
    try:
        first_open = values.index("(")
    except ValueError:
        return False
    if not _is_current_path(values[:first_open]):
        return False
    if values[first_open : first_open + 3] != ["(", ")", "."]:
        return False
    if len(values) <= first_open + 4:
        return False
    method = values[first_open + 3]
    rest = values[first_open + 4 :]
    if len(rest) < 4 or rest[0] != "(" or rest[-2:] != [")", "?"]:
        return False
    error_expr = rest[1:-2]
    if method == "ok_or":
        return _path_segments(error_expr) is not None
    if method != "ok_or_else":
        return False
    if error_expr[:1] == ["||"]:
        error_expr = error_expr[1:]
    elif error_expr[:2] == ["|", "|"]:
        error_expr = error_expr[2:]
    else:
        return False
    if error_expr[:1] == ["{"] and error_expr[-1:] == ["}"]:
        error_expr = error_expr[1:-1]
    if len(error_expr) < 3 or error_expr[-2:] != ["(", ")"]:
        return False
    helper_path = error_expr[:-2]
    helper_segments = _path_segments(helper_path)
    return bool(helper_segments) and helper_segments[-1] != "for_request"


def _is_fresh_request_cx_expr(tokens: list[Token]) -> bool:
    """Accept only a direct `for_request()` constructor, never ambient Cx."""
    values = [token.value for token in tokens]
    return (
        len(values) >= 3
        and values[-2:] == ["(", ")"]
        and _is_for_request_path(values[:-2])
    )


def _matching_call_open(tokens: list[Token]) -> int | None:
    if not tokens or tokens[-1].value != ")":
        return None
    depth = 0
    for index in range(len(tokens) - 1, -1, -1):
        if tokens[index].value == ")":
            depth += 1
        elif tokens[index].value == "(":
            depth -= 1
            if depth == 0:
                return index
    return None


def _parse_wrapper(
    site: FunctionSite,
    independent_expect: str | None,
    *,
    require_existing_ambient: bool = False,
) -> tuple[WrapperCall | None, str | None]:
    """Parse one ordinary wrapper or one exact independent-context adapter."""
    if site.body is None:
        return None, "wrapper has no parseable body"
    tokens = tokenize(site.body)
    if not tokens:
        return None, "wrapper body is empty"
    forbidden_values = {
        "!", "#", "if", "else", "match", "for", "while", "loop",
        "return", "break", "continue", "async", "fn", "impl", "trait", "mod", "struct",
        "enum", "union", "const", "static", "use", "macro_rules",
    }
    if not require_existing_ambient:
        forbidden_values.update({"?", "{", "}"})
    present_forbidden = sorted({token.value for token in tokens if token.value in forbidden_values})
    if present_forbidden:
        return None, f"wrapper uses forbidden/ambiguous syntax: {', '.join(present_forbidden)}"
    if sum(token.value == "await" for token in tokens) != 1:
        return None, "wrapper must contain exactly one await token"

    if tokens[-1].value == ";":
        tokens = tokens[:-1]
    statements = _split_top_level(tokens, ";")
    if len(statements) != 2 or not all(statements):
        return None, "wrapper must contain exactly one Cx binding and one tail-position sibling await"
    binding, tail = statements

    values = [token.value for token in binding]
    cursor = 0
    if values[:1] != ["let"]:
        return None, "first wrapper statement must be a let binding"
    cursor += 1
    mutable_binding = cursor < len(values) and values[cursor] == "mut"
    if mutable_binding:
        cursor += 1
    if cursor >= len(values) or not IDENT_RE.fullmatch(values[cursor]):
        return None, "ambient Cx binding must use one plain identifier"
    binding_name = _normal_name(values[cursor])
    if binding_name == "_":
        return None, "Cx binding must be a usable identifier, not `_`"
    if independent_expect is not None and mutable_binding:
        return None, "independent adapter Cx binding may not be mutable"
    cursor += 1
    if cursor >= len(values) or values[cursor] != "=":
        return None, "ambient Cx binding may not use a type annotation or pattern"
    cx_expr = binding[cursor + 1 :]
    if require_existing_ambient:
        if independent_expect is not None:
            return None, "required-ambient wrapper cannot be an independent-context adapter"
        if not _is_required_ambient_cx_expr(cx_expr):
            return None, "first wrapper statement is not a canonical fail-closed ambient Cx acquisition"
    elif independent_expect is None:
        if not _is_ambient_cx_expr(cx_expr):
            return None, "first wrapper statement is not a canonical ambient Cx constructor"
    elif not _is_fresh_request_cx_expr(cx_expr):
        return None, "independent adapter must construct a fresh for_request Cx directly"

    tail_values = [token.value for token in tail]
    if any(value in {"?", "{", "}"} for value in tail_values):
        return None, "sibling await contains forbidden control-flow syntax"
    has_literal_expect = (
        len(tail_values) >= 8
        and tail_values[-4:] == [".", "expect", "(", ")"]
    )
    if require_existing_ambient and has_literal_expect:
        return None, "required-ambient wrapper may not panic-adapt the sibling result"
    if independent_expect is not None and not has_literal_expect:
        return None, "independent adapter must end in one literal .expect(...)"
    if has_literal_expect:
        if site.raw_body is None:
            return None, "wrapper has no raw body for literal expect validation"
        raw_literal = site.raw_body[tail[-2].end : tail[-1].start].strip()
        try:
            decoded_literal = json.loads(raw_literal)
        except (json.JSONDecodeError, TypeError) as error:
            return None, f"wrapper expect argument is not one JSON-style string: {error}"
        if not isinstance(decoded_literal, str):
            return None, "wrapper expect argument must decode to a string"
        if independent_expect is not None and decoded_literal != independent_expect:
            return None, "independent adapter expect message differs from its exact allowlist entry"
        tail = tail[:-4]
        tail_values = [token.value for token in tail]
    if len(tail_values) < 4 or tail_values[-2:] != [".", "await"]:
        return None, "second wrapper statement must be a tail-position direct await"
    call_tokens = tail[:-2]
    call_open = _matching_call_open(call_tokens)
    if call_open is None:
        return None, "tail await does not apply directly to one function call"
    target = [token.value for token in call_tokens[:call_open]]
    expected = {f"{site.name}_with_cx", f"{site.name}_cx"}
    if len(target) == 1:
        sibling_name = _normal_name(target[0])
    elif len(target) == 3 and target[:2] == ["self", "."]:
        sibling_name = _normal_name(target[2])
    elif len(target) == 3 and target[:2] == ["Self", "::"]:
        sibling_name = _normal_name(target[2])
    else:
        return None, "tail call target must be bare, self.<sibling>, or Self::<sibling>"
    if sibling_name not in expected:
        return None, f"tail call targets {sibling_name}, not the exact Cx sibling"

    arguments = _split_top_level(call_tokens[call_open + 1 : -1], ",")
    cx_argument_indexes: list[int] = []
    binding_mentions = 0
    for index, argument in enumerate(arguments):
        arg_values = [_normal_name(token.value) for token in argument]
        binding_mentions += arg_values.count(binding_name)
        if independent_expect is not None:
            is_cx_argument = arg_values == ["&", binding_name]
        else:
            is_cx_argument = arg_values in (
                [binding_name],
                ["&", binding_name],
                ["&", "mut", binding_name],
            )
        if is_cx_argument:
            cx_argument_indexes.append(index)
    if binding_mentions != 1 or len(cx_argument_indexes) != 1:
        return None, "the exact bound Cx identifier must be one complete sibling argument"
    return WrapperCall(sibling_name, cx_argument_indexes[0]), None


def parse_canonical_wrapper(site: FunctionSite) -> tuple[WrapperCall | None, str | None]:
    """Accept ambient Cx + exact sibling await + optional literal expect."""
    return _parse_wrapper(site, None)


def parse_required_ambient_wrapper(
    site: FunctionSite,
) -> tuple[WrapperCall | None, str | None]:
    """Accept fail-closed ambient Cx acquisition plus exact sibling await."""
    return _parse_wrapper(site, None, require_existing_ambient=True)


def parse_independent_context_adapter(
    site: FunctionSite, expected_message: str
) -> tuple[WrapperCall | None, str | None]:
    """Accept one fresh request Cx, one sibling await, and one exact expect."""
    return _parse_wrapper(site, expected_message)


def run_self_tests() -> list[str]:
    """Small adversarial tests for the lexer and canonical wrapper grammar."""
    failures: list[str] = []
    source = """
pub    async unsafe fn spaced(cx: &'static crate::cx::Cx) {}
pub async fn commented() /* &Cx */ {}
pub async fn first(cx: &Cx) {} pub async fn second(cx: &Cx) {}
pub async fn const_generic(_: Foo<{ 1 }>, cx: &Cx) {}
"""
    sites, errors = discover_functions(source)
    names = [site.name for site in sites]
    if errors or names != ["spaced", "commented", "first", "second", "const_generic"]:
        failures.append(f"function discovery regression: names={names!r}, errors={errors!r}")
    by_name = {site.name: site for site in sites}
    if "commented" in by_name and is_covered(by_name["commented"]):
        failures.append("signature comments falsely satisfy RuntimeProof coverage")
    if "const_generic" in by_name and by_name["const_generic"].body is None:
        failures.append("const-generic braces were mistaken for the function body")

    proof_shape_source = """
pub async fn direct(cx: &crate::cx::Cx) {}
pub async fn self_path(cx: &self::Cx) {}
pub async fn owned(cx: crate::cx::Cx) {}
pub async fn generic<P: crate::runtime_proof::RuntimeProof>(proof: &P) {}
pub async fn where_bound<P>(proof: P) where P: RuntimeProof {}
pub async fn optional(cx: Option<&crate::cx::Cx>) {}
pub async fn callback(callback: impl FnOnce(&crate::cx::Cx)) {}
pub async fn nested_bound<P: FnOnce(&dyn RuntimeProof)>(callback: P) {}
"""
    proof_sites, proof_errors = discover_functions(proof_shape_source)
    proof_coverage = {site.name: is_covered(site) for site in proof_sites}
    expected_proof_coverage = {
        "direct": True,
        "self_path": True,
        "owned": True,
        "generic": True,
        "where_bound": True,
        "optional": False,
        "callback": False,
        "nested_bound": False,
    }
    if proof_errors or proof_coverage != expected_proof_coverage:
        failures.append(
            "direct-proof parameter classification regression: "
            f"coverage={proof_coverage!r}, errors={proof_errors!r}"
        )

    wrapper_source = """
pub async fn run(&self) {
    let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
    self.run_with_cx(&cx).await
}
pub async fn run_with_cx(&self, cx: &crate::cx::Cx) {}
"""
    wrapper_sites, wrapper_errors = discover_functions(wrapper_source)
    if wrapper_errors or len(wrapper_sites) != 2:
        failures.append(f"canonical wrapper fixture did not parse: {wrapper_errors!r}")
    else:
        call, error = parse_canonical_wrapper(wrapper_sites[0])
        if error or call != WrapperCall("run_with_cx", 0):
            failures.append(f"canonical wrapper was rejected: call={call!r}, error={error!r}")

    wrapper_expect_source = """
pub async fn run(&self) {
    let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
    self.run_with_cx(&cx)
        .await
        .expect("ambient wrapper failed");
}
pub async fn run_with_cx(&self, cx: &crate::cx::Cx) {}
"""
    wrapper_expect_sites, wrapper_expect_errors = discover_functions(wrapper_expect_source)
    if wrapper_expect_errors or len(wrapper_expect_sites) != 2:
        failures.append(
            f"literal-expect wrapper fixture did not parse: {wrapper_expect_errors!r}"
        )
    else:
        call, error = parse_canonical_wrapper(wrapper_expect_sites[0])
        if error or call != WrapperCall("run_with_cx", 0):
            failures.append(
                f"literal-expect wrapper was rejected: call={call!r}, error={error!r}"
            )

        dynamic_expect_sites, _ = discover_functions(
            wrapper_expect_source.replace('"ambient wrapper failed"', "failure_message")
        )
        if dynamic_expect_sites:
            _, error = parse_canonical_wrapper(dynamic_expect_sites[0])
            if error is None:
                failures.append("ordinary wrapper accepted a dynamic expect message")

        ordinary_unwrap_sites, _ = discover_functions(
            wrapper_expect_source.replace('.expect("ambient wrapper failed")', ".unwrap()")
        )
        if ordinary_unwrap_sites:
            _, error = parse_canonical_wrapper(ordinary_unwrap_sites[0])
            if error is None:
                failures.append("ordinary wrapper accepted unwrap instead of literal expect")

    independent_source = """
pub async fn clear(&self) {
    let cleanup_cx = crate::cx::for_request();
    self.clear_with_cx(&cleanup_cx)
        .await
        .expect("independent cleanup failed");
}
pub async fn clear_with_cx(&self, cx: &crate::cx::Cx) {}
"""
    independent_sites, independent_errors = discover_functions(independent_source)
    if independent_errors or len(independent_sites) != 2:
        failures.append(
            f"independent adapter fixture did not parse: {independent_errors!r}"
        )
    else:
        call, error = parse_independent_context_adapter(
            independent_sites[0], "independent cleanup failed"
        )
        if error or call != WrapperCall("clear_with_cx", 0):
            failures.append(
                f"independent adapter was rejected: call={call!r}, error={error!r}"
            )
        _, error = parse_independent_context_adapter(
            independent_sites[0], "different message"
        )
        if error is None:
            failures.append("independent adapter accepted the wrong expect message")

        unqualified_sites, _ = discover_functions(
            independent_source.replace("crate::cx::for_request()", "for_request()")
        )
        if unqualified_sites:
            _, error = parse_independent_context_adapter(
                unqualified_sites[0], "independent cleanup failed"
            )
            if error is None:
                failures.append("independent adapter accepted an unqualified for_request symbol")

        mutable_sites, _ = discover_functions(
            independent_source
            .replace("let cleanup_cx", "let mut cleanup_cx")
            .replace("&cleanup_cx", "&mut cleanup_cx")
        )
        if mutable_sites:
            _, error = parse_independent_context_adapter(
                mutable_sites[0], "independent cleanup failed"
            )
            if error is None:
                failures.append("independent adapter accepted a mutable fresh Cx contract")

        unwrap_sites, _ = discover_functions(
            independent_source.replace(
                '.expect("independent cleanup failed")', ".unwrap()"
            )
        )
        if unwrap_sites:
            _, error = parse_independent_context_adapter(
                unwrap_sites[0], "independent cleanup failed"
            )
            if error is None:
                failures.append("independent adapter accepted unwrap instead of exact expect")

    escaped_expect_source = """
pub async fn clear(&self) {
    /* offset-preserving noise: ") .expect(fake)" */
    let cleanup_cx = crate::cx::for_request();
    self.clear_with_cx(&cleanup_cx)
        .await
        .expect("independent cleanup\\nfailed");
}
"""
    escaped_sites, escaped_errors = discover_functions(escaped_expect_source)
    if escaped_errors or not escaped_sites:
        failures.append(f"escaped expect fixture did not parse: {escaped_errors!r}")
    else:
        call, error = parse_independent_context_adapter(
            escaped_sites[0], "independent cleanup\nfailed"
        )
        if error or call != WrapperCall("clear_with_cx", 0):
            failures.append(
                "raw/sanitized expect offsets diverged: "
                f"call={call!r}, error={error!r}"
            )

    ambient_adapter_source = """
pub async fn clear(&self) {
    let cleanup_cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
    self.clear_with_cx(&cleanup_cx)
        .await
        .expect("independent cleanup failed");
}
"""
    ambient_adapter_sites, _ = discover_functions(ambient_adapter_source)
    if ambient_adapter_sites:
        _, error = parse_independent_context_adapter(
            ambient_adapter_sites[0], "independent cleanup failed"
        )
        if error is None:
            failures.append("independent adapter accepted an ambient/current Cx")

    required_ambient_source = """
pub async fn run(&self) -> Result<(), Error> {
    let cx = crate::cx::Cx::current().ok_or(Error::ContextUnavailable)?;
    self.run_with_cx(&cx).await
}
pub async fn run_with_cx(&self, cx: &crate::cx::Cx) {}
"""
    required_sites, required_errors = discover_functions(required_ambient_source)
    if required_errors or len(required_sites) != 2:
        failures.append(
            f"required-ambient wrapper fixture did not parse: {required_errors!r}"
        )
    else:
        call, error = parse_required_ambient_wrapper(required_sites[0])
        if error or call != WrapperCall("run_with_cx", 0):
            failures.append(
                f"required-ambient wrapper was rejected: call={call!r}, error={error!r}"
            )

        fallback_sites, _ = discover_functions(
            required_ambient_source.replace(
                "crate::cx::Cx::current().ok_or(Error::ContextUnavailable)?",
                "crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request)",
            )
        )
        if fallback_sites:
            _, error = parse_required_ambient_wrapper(fallback_sites[0])
            if error is None:
                failures.append("required-ambient wrapper accepted a fresh-context fallback")

        fresh_error_sites, _ = discover_functions(
            required_ambient_source.replace(
                "crate::cx::Cx::current().ok_or(Error::ContextUnavailable)?",
                "crate::cx::Cx::current().ok_or_else(|| crate::cx::for_request())?",
            )
        )
        if fresh_error_sites:
            _, error = parse_required_ambient_wrapper(fresh_error_sites[0])
            if error is None:
                failures.append(
                    "required-ambient wrapper minted a fresh Cx as its missing-context error"
                )

        tail_try_sites, _ = discover_functions(
            required_ambient_source.replace(
                "self.run_with_cx(&cx).await",
                "self.run_with_cx(&cx).await?",
            )
        )
        if tail_try_sites:
            _, error = parse_required_ambient_wrapper(tail_try_sites[0])
            if error is None:
                failures.append("required-ambient wrapper accepted a fallible/ambiguous tail await")

        discarded_result_sites, _ = discover_functions(
            required_ambient_source.replace(
                "self.run_with_cx(&cx).await",
                "self.run_with_cx(&cx).await;\n    Ok(())",
            )
        )
        if discarded_result_sites:
            _, error = parse_required_ambient_wrapper(discarded_result_sites[0])
            if error is None:
                failures.append(
                    "required-ambient wrapper discarded its sibling result behind Ok(())"
                )

        panic_adapter_sites, _ = discover_functions(
            required_ambient_source.replace(
                "self.run_with_cx(&cx).await",
                'self.run_with_cx(&cx).await.expect("listener succeeds")',
            )
        )
        if panic_adapter_sites:
            _, error = parse_required_ambient_wrapper(panic_adapter_sites[0])
            if error is None:
                failures.append(
                    "required-ambient wrapper panic-adapted its sibling result"
                )

    required_closure_source = """
pub async fn bind(path: Path) -> Result<Listener, Error> {
    let cx = crate::cx::Cx::current().ok_or_else(|| {
        context_error("bind_context_unavailable")
    })?;
    bind_with_cx(&cx, path).await
}
pub async fn bind_with_cx(cx: &crate::cx::Cx, path: Path) -> Result<Listener, Error> {}
"""
    required_closure_sites, required_closure_errors = discover_functions(
        required_closure_source
    )
    if required_closure_errors or len(required_closure_sites) != 2:
        failures.append(
            "required-ambient closure fixture did not parse: "
            f"{required_closure_errors!r}"
        )
    else:
        call, error = parse_required_ambient_wrapper(required_closure_sites[0])
        if error or call != WrapperCall("bind_with_cx", 0):
            failures.append(
                "required-ambient closure wrapper was rejected: "
                f"call={call!r}, error={error!r}"
            )

    cfg_duplicate_source = """
#[cfg(unix)]
pub async fn run() -> Result<(), Error> {
    let cx = crate::cx::Cx::current().ok_or(Error::ContextUnavailable)?;
    run_with_cx(&cx).await
}
#[cfg(unix)]
pub async fn run_with_cx(cx: &crate::cx::Cx) -> Result<(), Error> {}
#[cfg(not(unix))]
pub async fn run() -> Result<(), Error> {
    let cx = crate::cx::Cx::current().ok_or(Error::ContextUnavailable)?;
    run_with_cx(&cx).await
}
#[cfg(not(unix))]
pub async fn run_with_cx(cx: &crate::cx::Cx) -> Result<(), Error> {}
"""
    cfg_duplicate_sites, cfg_duplicate_errors = discover_functions(
        cfg_duplicate_source
    )
    cfg_wrappers = [site for site in cfg_duplicate_sites if site.name == "run"]
    cfg_siblings = [
        site for site in cfg_duplicate_sites if site.name == "run_with_cx"
    ]
    if cfg_duplicate_errors or len(cfg_wrappers) != 2 or len(cfg_siblings) != 2:
        failures.append(
            "complementary-cfg duplicate wrapper fixture did not parse: "
            f"{cfg_duplicate_errors!r}"
        )
    else:
        for wrapper in cfg_wrappers:
            call, error = parse_required_ambient_wrapper(wrapper)
            candidates = [
                sibling
                for sibling in cfg_siblings
                if call is not None
                and call.cx_arg_index in proof_param_positions(sibling)
            ]
            selected = _select_cfg_siblings(wrapper, candidates)
            if error or len(selected) != 1 or selected[0].cfg_key != wrapper.cfg_key:
                failures.append(
                    "complementary-cfg duplicate wrapper did not select its exact sibling: "
                    f"call={call!r}, error={error!r}, selected={selected!r}"
                )

    overlapping_cfg_source = """
pub async fn run() -> Result<(), Error> {
    let cx = crate::cx::Cx::current().ok_or(Error::ContextUnavailable)?;
    run_with_cx(&cx).await
}
#[cfg(unix)]
pub async fn run_with_cx(cx: &crate::cx::Cx) -> Result<(), Error> {}
#[cfg(windows)]
pub async fn run_with_cx(cx: &crate::cx::Cx) -> Result<(), Error> {}
"""
    overlapping_sites, overlapping_errors = discover_functions(
        overlapping_cfg_source
    )
    if overlapping_errors or len(overlapping_sites) != 3:
        failures.append(
            f"overlapping-cfg sibling fixture did not parse: {overlapping_errors!r}"
        )
    else:
        overlapping_wrapper = overlapping_sites[0]
        overlapping_candidates = [
            site
            for site in overlapping_sites[1:]
            if 0 in proof_param_positions(site)
        ]
        if _select_cfg_siblings(overlapping_wrapper, overlapping_candidates):
            failures.append(
                "ordinary wrapper accepted overlapping cfg siblings as a complete partition"
            )

    stringify_source = """
pub async fn run(&self) {
    let _ = stringify!(Cx::current(); self.run_with_cx(&cx).await);
}
"""
    stringify_sites, _ = discover_functions(stringify_source)
    if stringify_sites:
        _, error = parse_canonical_wrapper(stringify_sites[0])
        if error is None:
            failures.append("stringify macro token body defeated the wrapper grammar")

    baseline_fixture = {
        "schema_version": 3,
        "total_sites": 1,
        "covered_sites": 0,
        "exempt_files_sites": 0,
        "wrapper_exempt_sites": 1,
        "required_ambient_wrapper_sites": 1,
        "independent_context_adapter_sites": 0,
        "uncovered_sites": 0,
        "by_file_counts": {
            "fixture.rs": {
                "total": 1,
                "covered": 0,
                "exempt": 0,
                "wrapper_exempt": 1,
                "required_ambient_wrapper": 1,
                "independent_adapter": 0,
                "uncovered": 0,
            }
        },
    }
    live_category_swap = {
        "total_sites": 1,
        "covered_sites": 0,
        "exempt_files_sites": 0,
        "wrapper_exempt_sites": 0,
        "required_ambient_wrapper_sites": 0,
        "independent_context_adapter_sites": 1,
        "uncovered_sites": 0,
        "by_file": {
            "fixture.rs": {
                "total": 1,
                "covered": 0,
                "exempt": 0,
                "wrapper_exempt": 0,
                "required_ambient_wrapper": 0,
                "independent_adapter": 1,
                "uncovered": 0,
            }
        },
    }
    category_swap_errors = validate_baseline(live_category_swap, baseline_fixture)
    if not any("ordinary-wrapper census collapsed" in error for error in category_swap_errors):
        failures.append(
            "baseline category ratchet allowed ordinary wrappers to become independent adapters"
        )
    live_strictness_downgrade = {
        **live_category_swap,
        "wrapper_exempt_sites": 1,
        "independent_context_adapter_sites": 0,
        "by_file": {
            "fixture.rs": {
                **live_category_swap["by_file"]["fixture.rs"],
                "wrapper_exempt": 1,
                "independent_adapter": 0,
            }
        },
    }
    strictness_errors = validate_baseline(live_strictness_downgrade, baseline_fixture)
    if not any("required-ambient wrapper census collapsed" in error for error in strictness_errors):
        failures.append(
            "baseline ratchet allowed a required-ambient wrapper to become an ordinary wrapper"
        )
    malformed_subset_baseline = {
        **baseline_fixture,
        "required_ambient_wrapper_sites": 2,
        "by_file_counts": {
            "fixture.rs": {
                **baseline_fixture["by_file_counts"]["fixture.rs"],
                "required_ambient_wrapper": 2,
            }
        },
    }
    malformed_subset_errors = validate_baseline(
        live_category_swap,
        malformed_subset_baseline,
    )
    if not any("required-ambient wrapper count exceeds" in error for error in malformed_subset_errors):
        failures.append(
            "baseline validator accepted required-ambient wrappers outside the wrapper subset"
        )
    return failures


def audit() -> dict:
    results = {
        "total_sites": 0,
        "exempt_files_sites": 0,
        "wrapper_exempt_sites": 0,
        "required_ambient_wrapper_sites": 0,
        "independent_context_adapter_sites": 0,
        "covered_sites": 0,
        "uncovered_sites": 0,
        "by_file": {},
        "uncovered_examples": [],
        "wrapper_audit_errors": [],
        "negative_evidence": NEGATIVE_EVIDENCE,
    }
    if not SRC_ROOT.is_dir():
        results["wrapper_audit_errors"].append(f"source root does not exist: {SRC_ROOT}")
        return results
    files = sorted(SRC_ROOT.rglob("*.rs"))
    if not files:
        results["wrapper_audit_errors"].append(f"source root contains no Rust files: {SRC_ROOT}")
        return results
    relative_files = {path.relative_to(SRC_ROOT).as_posix() for path in files}
    for exempt_file in sorted(EXEMPT_FILES):
        if exempt_file not in relative_files:
            results["wrapper_audit_errors"].append(
                f"{exempt_file} runtime-layer exemption names a nonexistent file"
            )
    for exempt_file, exempt_name in sorted(WRAPPER_EXEMPTIONS):
        if exempt_file not in relative_files:
            results["wrapper_audit_errors"].append(
                f"{exempt_file}::{exempt_name} allowlist entry names a nonexistent file"
            )
    for required_wrapper in sorted(REQUIRED_AMBIENT_CX_WRAPPERS):
        if required_wrapper not in WRAPPER_EXEMPTIONS:
            results["wrapper_audit_errors"].append(
                f"{required_wrapper[0]}::{required_wrapper[1]} required-ambient entry "
                "is not in WRAPPER_EXEMPTIONS"
            )
    for (adapter_file, adapter_name), adapter_message in sorted(
        INDEPENDENT_CONTEXT_ADAPTERS.items()
    ):
        if adapter_file not in relative_files:
            results["wrapper_audit_errors"].append(
                f"{adapter_file}::{adapter_name} independent-adapter entry names a nonexistent file"
            )
        if (adapter_file, adapter_name) in WRAPPER_EXEMPTIONS:
            results["wrapper_audit_errors"].append(
                f"{adapter_file}::{adapter_name} appears in both wrapper categories"
            )
        if (
            not isinstance(adapter_message, str)
            or not adapter_message
            or adapter_message.strip() != adapter_message
        ):
            results["wrapper_audit_errors"].append(
                f"{adapter_file}::{adapter_name} independent-adapter expect message must be "
                "a non-empty string without surrounding whitespace"
            )

    file_data: dict[str, dict] = {}
    for path in files:
        rel = path.relative_to(SRC_ROOT).as_posix()
        is_exempt_file = rel in EXEMPT_FILES
        source = path.read_text(encoding="utf-8")
        sites, parse_errors = discover_functions(source)
        results["wrapper_audit_errors"].extend(f"{rel}: {error}" for error in parse_errors)

        fn_names: Counter[str] = Counter(site.name for site in sites)
        ordinary_wrapper_fn_names: Counter[str] = Counter()
        independent_adapter_fn_names: Counter[str] = Counter()
        covered_positions: dict[int, tuple[int, ...]] = {}
        wrappers: list[tuple[FunctionSite, WrapperCall]] = []
        local_total = local_covered = local_uncovered = local_wrapper = 0
        local_required_ambient = 0
        local_independent = local_exempt = 0
        local_uncovered_lines: list[tuple[int, str]] = []
        for site in sites:
            results["total_sites"] += 1
            local_total += 1
            if is_exempt_file:
                results["exempt_files_sites"] += 1
                local_exempt += 1
                continue
            positions = proof_param_positions(site)
            if positions:
                results["covered_sites"] += 1
                local_covered += 1
                covered_positions[site.start] = positions
                continue
            if (rel, site.name) in WRAPPER_EXEMPTIONS:
                results["wrapper_exempt_sites"] += 1
                local_wrapper += 1
                ordinary_wrapper_fn_names[site.name] += 1
                if (rel, site.name) in REQUIRED_AMBIENT_CX_WRAPPERS:
                    results["required_ambient_wrapper_sites"] += 1
                    local_required_ambient += 1
                    call, error = parse_required_ambient_wrapper(site)
                else:
                    call, error = parse_canonical_wrapper(site)
                if error:
                    results["wrapper_audit_errors"].append(
                        f"{rel}:{site.line}::{site.name} {error}"
                    )
                elif call is not None:
                    wrappers.append((site, call))
                continue
            adapter_key = (rel, site.name)
            if adapter_key in INDEPENDENT_CONTEXT_ADAPTERS:
                adapter_message = INDEPENDENT_CONTEXT_ADAPTERS[adapter_key]
                results["independent_context_adapter_sites"] += 1
                local_independent += 1
                independent_adapter_fn_names[site.name] += 1
                call, error = parse_independent_context_adapter(site, adapter_message)
                if error:
                    results["wrapper_audit_errors"].append(
                        f"{rel}:{site.line}::{site.name} {error}"
                    )
                elif call is not None:
                    wrappers.append((site, call))
                continue
            results["uncovered_sites"] += 1
            local_uncovered += 1
            local_uncovered_lines.append((site.line, site.name))
            if len(results["uncovered_examples"]) < 25:
                results["uncovered_examples"].append(
                    {"file": rel, "line": site.line, "fn": site.name}
                )

        for exempt_file, exempt_name in WRAPPER_EXEMPTIONS:
            if exempt_file != rel:
                continue
            if fn_names[exempt_name] == 0:
                results["wrapper_audit_errors"].append(
                    f"{rel}::{exempt_name} listed in WRAPPER_EXEMPTIONS but no such pub async fn"
                )
            elif ordinary_wrapper_fn_names[exempt_name] == 0:
                results["wrapper_audit_errors"].append(
                    f"{rel}::{exempt_name} allowlist entry is unused; every occurrence is "
                    "directly covered or exempt"
                )
        for adapter_file, adapter_name in INDEPENDENT_CONTEXT_ADAPTERS:
            if adapter_file != rel:
                continue
            if fn_names[adapter_name] == 0:
                results["wrapper_audit_errors"].append(
                    f"{rel}::{adapter_name} independent-adapter entry has no pub async fn"
                )
            elif independent_adapter_fn_names[adapter_name] == 0:
                results["wrapper_audit_errors"].append(
                    f"{rel}::{adapter_name} independent-adapter entry is unused; every "
                    "occurrence is directly covered or exempt"
                )

        used_siblings: set[int] = set()
        for wrapper, call in wrappers:
            candidates = [
                site
                for site in sites
                if site.name == call.sibling_name
                and site.scope == wrapper.scope
                and call.cx_arg_index in covered_positions.get(site.start, ())
            ]
            selected_siblings = _select_cfg_siblings(wrapper, candidates)
            if not selected_siblings:
                results["wrapper_audit_errors"].append(
                    f"{rel}:{wrapper.line}::{wrapper.name} tail call {call.sibling_name} has "
                    f"no unique same-scope covered sibling or exact complementary cfg partition "
                    f"among {len(candidates)} candidate occurrences"
                )
                continue
            for sibling in selected_siblings:
                if sibling.start in used_siblings:
                    results["wrapper_audit_errors"].append(
                        f"{rel}:{wrapper.line}::{wrapper.name} reuses covered sibling occurrence "
                        f"{call.sibling_name} at line {sibling.line}"
                    )
                used_siblings.add(sibling.start)

        file_data[rel] = {
            "exempt_file": is_exempt_file,
            "total": local_total,
            "exempt": local_exempt,
            "covered": local_covered,
            "uncovered": local_uncovered,
            "wrapper_exempt": local_wrapper,
            "required_ambient_wrapper": local_required_ambient,
            "independent_adapter": local_independent,
            "uncovered_lines": local_uncovered_lines,
        }
        local_classified = (
            local_exempt
            + local_covered
            + local_wrapper
            + local_independent
            + local_uncovered
        )
        if local_classified != local_total:
            results["wrapper_audit_errors"].append(
                f"{rel}: classification accounting mismatch: total={local_total} "
                f"classified={local_classified}"
            )
    results["by_file"] = file_data
    if results["total_sites"] == 0:
        results["wrapper_audit_errors"].append("function census returned zero sites")
    classified = (
        results["exempt_files_sites"]
        + results["covered_sites"]
        + results["wrapper_exempt_sites"]
        + results["independent_context_adapter_sites"]
        + results["uncovered_sites"]
    )
    if classified != results["total_sites"]:
        results["wrapper_audit_errors"].append(
            f"classification accounting mismatch: total={results['total_sites']} "
            f"classified={classified}"
        )
    return results


def load_baseline() -> dict | None:
    if not BASELINE_PATH.is_file():
        return None
    return json.loads(BASELINE_PATH.read_text())


def save_baseline(audit_data: dict) -> None:
    payload = {
        "schema_version": 3,
        "comment": "ft-3kv6e fail-closed census ratchet. Update only with an audited source "
                   "change. Generated by scripts/check_runtime_proof_coverage.py.",
        "uncovered_sites": audit_data["uncovered_sites"],
        "covered_sites": audit_data["covered_sites"],
        "exempt_files_sites": audit_data["exempt_files_sites"],
        "wrapper_exempt_sites": audit_data["wrapper_exempt_sites"],
        "required_ambient_wrapper_sites": audit_data[
            "required_ambient_wrapper_sites"
        ],
        "independent_context_adapter_sites": audit_data[
            "independent_context_adapter_sites"
        ],
        "total_sites": audit_data["total_sites"],
        "by_file_uncovered": {
            f: data["uncovered"]
            for f, data in sorted(audit_data["by_file"].items())
            if data["uncovered"] > 0
        },
        "by_file_counts": {
            f: {
                "total": data["total"],
                "exempt": data["exempt"],
                "covered": data["covered"],
                "wrapper_exempt": data["wrapper_exempt"],
                "required_ambient_wrapper": data["required_ambient_wrapper"],
                "independent_adapter": data["independent_adapter"],
                "uncovered": data["uncovered"],
            }
            for f, data in sorted(audit_data["by_file"].items())
            if data["total"] > 0
        },
    }
    BASELINE_PATH.write_text(json.dumps(payload, indent=2, sort_keys=True) + "\n")


def _is_non_negative_int(value: object) -> bool:
    return isinstance(value, int) and not isinstance(value, bool) and value >= 0


def validate_baseline(data: dict, baseline: dict | None) -> list[str]:
    """Reject uncovered growth and any aggregate/category/per-file census collapse."""
    if baseline is None:
        return [f"required baseline is missing: {BASELINE_PATH}"]
    if not isinstance(baseline, dict):
        return ["baseline root must be a JSON object"]
    if baseline.get("schema_version") != 3:
        return [
            f"{BASELINE_PATH.name} does not use census schema 3; run --update-baseline "
            "only after reviewing the live schema-v3 census"
        ]
    required = {
        "total_sites", "covered_sites", "exempt_files_sites", "wrapper_exempt_sites",
        "required_ambient_wrapper_sites",
        "independent_context_adapter_sites", "uncovered_sites", "by_file_counts",
    }
    missing = sorted(required - baseline.keys())
    if missing:
        return [f"baseline is missing required fields: {', '.join(missing)}"]
    errors: list[str] = []
    numeric_fields = required - {"by_file_counts"}
    for field in sorted(numeric_fields):
        if not _is_non_negative_int(baseline.get(field)):
            errors.append(f"baseline field {field} must be a non-negative integer")
    if errors:
        return errors
    baseline_classified = (
        baseline["covered_sites"]
        + baseline["exempt_files_sites"]
        + baseline["wrapper_exempt_sites"]
        + baseline["independent_context_adapter_sites"]
        + baseline["uncovered_sites"]
    )
    if baseline_classified != baseline["total_sites"]:
        errors.append(
            "baseline aggregate classification does not add up: "
            f"total={baseline['total_sites']} classified={baseline_classified}"
        )
    if baseline["required_ambient_wrapper_sites"] > baseline["wrapper_exempt_sites"]:
        errors.append(
            "baseline aggregate required-ambient wrapper count exceeds "
            "the ordinary-wrapper subset"
        )
    if data["required_ambient_wrapper_sites"] > data["wrapper_exempt_sites"]:
        errors.append(
            "live aggregate required-ambient wrapper count exceeds "
            "the ordinary-wrapper subset"
        )

    if data["uncovered_sites"] > baseline["uncovered_sites"]:
        errors.append(
            f"uncovered count grew from {baseline['uncovered_sites']} to "
            f"{data['uncovered_sites']}"
        )
    for field, label in (
        ("total_sites", "total site census"),
        ("covered_sites", "directly covered census"),
        ("exempt_files_sites", "runtime-file exempt census"),
        ("wrapper_exempt_sites", "ordinary-wrapper census"),
        ("required_ambient_wrapper_sites", "required-ambient wrapper census"),
        ("independent_context_adapter_sites", "independent-context adapter census"),
    ):
        if data[field] < baseline[field]:
            errors.append(f"{label} collapsed from {baseline[field]} to {data[field]}")
    live_accepted = (
        data["covered_sites"]
        + data["wrapper_exempt_sites"]
        + data["independent_context_adapter_sites"]
        + data["exempt_files_sites"]
    )
    baseline_accepted = (
        baseline["covered_sites"]
        + baseline["wrapper_exempt_sites"]
        + baseline["independent_context_adapter_sites"]
        + baseline["exempt_files_sites"]
    )
    if live_accepted < baseline_accepted:
        errors.append(
            f"accepted-site census collapsed from {baseline_accepted} to {live_accepted}"
        )

    by_file_counts = baseline["by_file_counts"]
    if not isinstance(by_file_counts, dict) or not by_file_counts:
        errors.append("baseline by_file_counts must be a non-empty object")
        return errors
    per_file_fields = {
        "total",
        "covered",
        "exempt",
        "wrapper_exempt",
        "required_ambient_wrapper",
        "independent_adapter",
        "uncovered",
    }
    baseline_file_sums = {field: 0 for field in per_file_fields}
    all_file_counts_valid = True
    for rel, expected in sorted(by_file_counts.items(), key=lambda item: repr(item[0])):
        if not isinstance(rel, str) or not rel:
            errors.append("baseline by_file_counts keys must be non-empty strings")
            all_file_counts_valid = False
            continue
        if not isinstance(expected, dict):
            errors.append(f"baseline by_file_counts[{rel!r}] must be an object")
            all_file_counts_valid = False
            continue
        missing_fields = sorted(per_file_fields - expected.keys())
        if missing_fields:
            errors.append(
                f"baseline {rel} is missing count fields: {', '.join(missing_fields)}"
            )
            all_file_counts_valid = False
            continue
        invalid_fields = sorted(
            field
            for field in per_file_fields
            if not _is_non_negative_int(expected.get(field))
        )
        if invalid_fields:
            errors.append(
                f"baseline {rel} fields must be non-negative integers: "
                f"{', '.join(invalid_fields)}"
            )
            all_file_counts_valid = False
            continue
        expected_classified = sum(
            expected[field]
            for field in (
                "covered",
                "exempt",
                "wrapper_exempt",
                "independent_adapter",
                "uncovered",
            )
        )
        if expected_classified != expected["total"]:
            errors.append(
                f"baseline {rel} classification does not add up: "
                f"total={expected['total']} classified={expected_classified}"
            )
            all_file_counts_valid = False
            continue
        if expected["required_ambient_wrapper"] > expected["wrapper_exempt"]:
            errors.append(
                f"baseline {rel} required-ambient wrapper count exceeds "
                "the ordinary-wrapper subset"
            )
            all_file_counts_valid = False
            continue
        for field in per_file_fields:
            baseline_file_sums[field] += expected[field]
        live = data["by_file"].get(rel)
        if live is None:
            errors.append(f"per-file census disappeared entirely: {rel}")
            continue
        if live["required_ambient_wrapper"] > live["wrapper_exempt"]:
            errors.append(
                f"live {rel} required-ambient wrapper count exceeds "
                "the ordinary-wrapper subset"
            )
        for field in (
            "total",
            "covered",
            "exempt",
            "wrapper_exempt",
            "required_ambient_wrapper",
            "independent_adapter",
        ):
            floor = expected[field]
            if live[field] < floor:
                errors.append(f"per-file {rel}:{field} collapsed from {floor} to {live[field]}")
        expected_accepted = sum(
            expected[field]
            for field in ("covered", "wrapper_exempt", "independent_adapter", "exempt")
        )
        live_accepted_for_file = sum(
            live[field]
            for field in ("covered", "wrapper_exempt", "independent_adapter", "exempt")
        )
        if live_accepted_for_file < expected_accepted:
            errors.append(
                f"per-file {rel}:accepted collapsed from {expected_accepted} "
                f"to {live_accepted_for_file}"
            )
        expected_uncovered = expected["uncovered"]
        if live["uncovered"] > expected_uncovered:
            errors.append(
                f"per-file {rel}:uncovered grew from {expected_uncovered} "
                f"to {live['uncovered']}"
            )
    if all_file_counts_valid:
        aggregate_by_file_fields = {
            "total": "total_sites",
            "covered": "covered_sites",
            "exempt": "exempt_files_sites",
            "wrapper_exempt": "wrapper_exempt_sites",
            "required_ambient_wrapper": "required_ambient_wrapper_sites",
            "independent_adapter": "independent_context_adapter_sites",
            "uncovered": "uncovered_sites",
        }
        for file_field, aggregate_field in aggregate_by_file_fields.items():
            if baseline_file_sums[file_field] != baseline[aggregate_field]:
                errors.append(
                    f"baseline per-file {file_field} sum {baseline_file_sums[file_field]} "
                    f"does not match aggregate {aggregate_field}={baseline[aggregate_field]}"
                )
    for rel, live in sorted(data["by_file"].items()):
        if rel not in by_file_counts and live["uncovered"]:
            errors.append(
                f"new census file {rel} introduces {live['uncovered']} uncovered sites"
            )
    return errors


def main() -> int:
    p = argparse.ArgumentParser(description="ft-3kv6e RuntimeProof coverage audit")
    p.add_argument("--update-baseline", action="store_true",
                   help="Rewrite the baseline JSON to match current state.")
    p.add_argument("--json", action="store_true",
                   help="Emit a machine-readable JSON summary instead of human text.")
    p.add_argument("--summary", action="store_true",
                   help="Print only the headline numbers (no per-file detail).")
    args = p.parse_args()

    self_test_errors = run_self_tests()
    data = audit()
    data["self_test_errors"] = self_test_errors

    if args.update_baseline:
        if self_test_errors or data["wrapper_audit_errors"]:
            print("ft-3kv6e: refusing to update a failed/ambiguous census:", file=sys.stderr)
            for err in self_test_errors + data["wrapper_audit_errors"]:
                print(f"  - {err}", file=sys.stderr)
            return 2
        save_baseline(data)
        print(f"Baseline updated: uncovered={data['uncovered_sites']} "
              f"covered={data['covered_sites']} "
              f"independent={data['independent_context_adapter_sites']} "
              f"exempt={data['exempt_files_sites']}")
        return 0

    try:
        baseline = load_baseline()
        baseline_errors = validate_baseline(data, baseline)
    except (OSError, UnicodeError, json.JSONDecodeError, TypeError, ValueError) as error:
        baseline = None
        baseline_errors = [f"baseline could not be read or validated: {error}"]
    data["baseline_errors"] = baseline_errors

    if args.json:
        print(json.dumps(data, indent=2, sort_keys=True))
        if self_test_errors or data["wrapper_audit_errors"]:
            return 2
        return 1 if baseline_errors else 0

    if self_test_errors:
        print("ft-3kv6e: INTERNAL SELF-TEST FAILURE:", file=sys.stderr)
        for err in self_test_errors:
            print(f"  - {err}", file=sys.stderr)
        return 2
    if data["wrapper_audit_errors"]:
        print("ft-3kv6e: parser/wrapper audit is inconsistent:", file=sys.stderr)
        for err in data["wrapper_audit_errors"]:
            print(f"  - {err}", file=sys.stderr)
        return 2

    print(f"ft-3kv6e RuntimeProof coverage audit")
    print(f"  total pub async fn      : {data['total_sites']}")
    print(f"  in exempt runtime files : {data['exempt_files_sites']}")
    print(f"  covered (Cx/RuntimeProof): {data['covered_sites']}")
    print(f"  wrapper-exempt          : {data['wrapper_exempt_sites']}")
    print(f"    required ambient      : {data['required_ambient_wrapper_sites']}")
    print(f"  independent-context     : {data['independent_context_adapter_sites']}")
    print(f"  uncovered               : {data['uncovered_sites']}")
    print()
    if baseline_errors:
        print("FAIL: census baseline validation failed:", file=sys.stderr)
        for error in baseline_errors:
            print(f"  - {error}", file=sys.stderr)
        if not args.summary and data["uncovered_examples"]:
            print("Live uncovered sites include:", file=sys.stderr)
            for example in data["uncovered_examples"][:15]:
                print(
                    f"  {example['file']}:{example['line']} :: {example['fn']}",
                    file=sys.stderr,
                )
        return 1
    print(f"Baseline ({BASELINE_PATH.name}) passed schema-v3 aggregate/category/per-file ratchets.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
