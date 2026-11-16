#!/usr/bin/env bash
# Pure Agent Mail failover classifier for red-mail Beads-only coordination.
#
# This file intentionally performs no service calls and no filesystem cleanup.
# It maps the observed Agent Mail startup outcome to the stable fields consumed
# by scripts/swarm-tick.sh and the retained fixture contract.

agent_mail_failover_classify_json() {
  local raw_class="${1:-${FT_AGENT_MAIL_FAILURE_CLASS:-api_unreachable}}"
  local failure_class status attempt_count registered inbox_checked reason_codes error_summary

  case "${raw_class}" in
    available|healthy|success|none)
      failure_class="none"
      status="available"
      attempt_count=1
      registered=true
      inbox_checked=true
      reason_codes='["agent_mail.available"]'
      error_summary=""
      ;;
    database_recovery|database_recovery_notice|recovery)
      failure_class="database_recovery_notice"
      status="degraded"
      attempt_count=2
      registered=false
      inbox_checked=false
      reason_codes='["agent_mail.database_recovery_retry_exhausted","agent_mail.unavailable_after_retry","fallback.beads_only"]'
      error_summary="Agent Mail reported database recovery after the single retry; registration/inbox coordination is skipped and Beads-only fallback is active for this session."
      ;;
    database_error)
      failure_class="database_error"
      status="unavailable"
      attempt_count=2
      registered=false
      inbox_checked=false
      reason_codes='["agent_mail.database_error_after_retry","agent_mail.unavailable_after_retry","fallback.beads_only"]'
      error_summary="Agent Mail database access failed after the single retry; registration/inbox coordination is skipped and Beads-only fallback is active for this session."
      ;;
    api_unreachable|unreachable|unavailable|connection_refused)
      failure_class="api_unreachable"
      status="unavailable"
      attempt_count=2
      registered=false
      inbox_checked=false
      reason_codes='["agent_mail.unavailable_after_retry","fallback.beads_only"]'
      error_summary="Agent Mail API stayed unreachable after the single retry; registration/inbox coordination is skipped and Beads-only fallback is active for this session."
      ;;
    timeout|hang|hung)
      failure_class="timeout"
      status="unavailable"
      attempt_count=2
      registered=false
      inbox_checked=false
      reason_codes='["agent_mail.timeout_after_retry","agent_mail.unavailable_after_retry","fallback.beads_only"]'
      error_summary="Agent Mail timed out after the single retry; registration/inbox coordination is skipped and Beads-only fallback is active for this session."
      ;;
    registration_failed|registration_failure)
      failure_class="registration_failed"
      status="unavailable"
      attempt_count=2
      registered=false
      inbox_checked=false
      reason_codes='["agent_mail.registration_failed_after_retry","agent_mail.unavailable_after_retry","fallback.beads_only"]'
      error_summary="Agent Mail registration failed after the single retry; registration/inbox coordination is skipped and Beads-only fallback is active for this session."
      ;;
    contact_permission_failed|contact_failed|contact_failure)
      failure_class="contact_permission_failed"
      status="unavailable"
      attempt_count=2
      registered=false
      inbox_checked=false
      reason_codes='["agent_mail.contact_permission_failed_after_retry","agent_mail.unavailable_after_retry","fallback.beads_only"]'
      error_summary="Agent Mail contact or inbox setup failed after the single retry; registration/inbox coordination is skipped and Beads-only fallback is active for this session."
      ;;
    unknown|*)
      failure_class="unknown"
      status="unavailable"
      attempt_count=2
      registered=false
      inbox_checked=false
      reason_codes='["agent_mail.unknown_after_retry","agent_mail.unavailable_after_retry","fallback.beads_only"]'
      error_summary="Agent Mail returned an unclassified startup result after the single retry; registration/inbox coordination is skipped and Beads-only fallback is active for this session."
      ;;
  esac

  jq -cn \
    --arg status "${status}" \
    --argjson attempt_count "${attempt_count}" \
    --arg failure_class "${failure_class}" \
    --argjson registered "${registered}" \
    --argjson inbox_checked "${inbox_checked}" \
    --argjson reason_codes "${reason_codes}" \
    --arg error_summary "${error_summary}" \
    '{
      status: $status,
      attempt_count: $attempt_count,
      retry_limit: 1,
      registered: $registered,
      inbox_checked: $inbox_checked,
      failure_class: (if $failure_class == "none" then null else $failure_class end),
      reason_codes: $reason_codes,
      forbidden_actions: [
        "am_service_restart",
        "am_service_stop",
        "am_doctor_fix",
        "am_doctor_repair",
        "am_doctor_reconstruct",
        "kill_agent_mail_process",
        "destructive_git",
        "delete_files",
        "mutate_rch_worker",
        "cancel_build",
        "run_local_cargo_as_proof",
        "reopen_dirty_overlap_without_owner_clear"
      ],
      error_summary: (if $error_summary == "" then null else $error_summary end)
    }'
}

if [[ "${BASH_SOURCE[0]}" == "$0" ]]; then
  set -euo pipefail
  agent_mail_failover_classify_json "${1:-${FT_AGENT_MAIL_FAILURE_CLASS:-api_unreachable}}"
fi
