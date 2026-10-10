//! Live, read-only audit-log sources for the attention router, shared by
//! `ft attention` / `ft robot attention` and MCP `wa.attention`
//! (ft-7h5da.7.11, ft-7h5da.7.7, ft-7h5da.3.9).
//!
//! The workspace database is opened read-only: no migration, no writer, and
//! nothing created, so attention stays read-only. Only the columns the router
//! needs are selected; action input, the policy's free-text reason and the
//! decision context never leave the database.

use std::path::Path;

use crate::attention_router::{
    AttentionRouterNotificationMute, AttentionRouterSourceAdapterInput, POLICY_GATE_AUDIT_MAX_ROWS,
    POLICY_GATE_AUDIT_WINDOW_MS, PolicyGateAuditRow, VERIFIED_SUBMIT_AUDIT_MAX_ROWS,
    approval_required_observation_from_audit, policy_denied_observation_from_audit,
    verified_submit_observation_from_receipts,
};
use crate::robot_types::SubmitReceipt;
use crate::storage::EventMuteRecord;

/// Live audit-log inputs for the attention router. `None` marks a read
/// failure for that input.
#[derive(Debug, Clone, Default)]
pub struct LiveAttentionAudit {
    pub denied: Option<Vec<PolicyGateAuditRow>>,
    pub approval_required: Option<Vec<PolicyGateAuditRow>>,
    pub receipts: Option<Vec<(i64, SubmitReceipt)>>,
    /// Active `ft mute` records; empty when unreadable, so a read failure
    /// shows items rather than hiding them.
    pub mutes: Vec<EventMuteRecord>,
}

impl LiveAttentionAudit {
    /// Every audit-backed input unreadable (no workspace layout, or the
    /// database could not be opened).
    #[must_use]
    pub const fn unreadable() -> Self {
        Self {
            denied: None,
            approval_required: None,
            receipts: None,
            mutes: Vec::new(),
        }
    }

    /// A workspace without a database: no audit history, which is empty
    /// rather than unreadable.
    #[must_use]
    pub const fn empty() -> Self {
        Self {
            denied: Some(Vec::new()),
            approval_required: Some(Vec::new()),
            receipts: Some(Vec::new()),
            mutes: Vec::new(),
        }
    }
}

/// An `ft mute` record as an attention-router mute: global scope stays
/// global; anything else applies to this workspace, whose database holds it.
#[must_use]
pub fn attention_mute_from_record(record: &EventMuteRecord) -> AttentionRouterNotificationMute {
    let mute = if record.scope == "global" {
        AttentionRouterNotificationMute::global(record.identity_key.clone())
    } else {
        AttentionRouterNotificationMute::workspace_current(record.identity_key.clone())
    };
    match record.reason.as_deref() {
        Some(reason) => mute.with_reason(reason),
        None => mute,
    }
}

/// Add the audit-backed live sources (policy denials, unresolved approval
/// holds, verified-submit drift) and the active `ft mute` records to `input`.
pub fn apply_live_attention_audit(
    input: &mut AttentionRouterSourceAdapterInput,
    audit: &LiveAttentionAudit,
) {
    let now_ms = input.generated_at_ms;
    input
        .observations
        .push(policy_denied_observation_from_audit(
            audit.denied.as_deref(),
            now_ms,
        ));
    input
        .observations
        .push(approval_required_observation_from_audit(
            audit.approval_required.as_deref(),
            now_ms,
        ));
    input
        .observations
        .push(verified_submit_observation_from_receipts(
            audit.receipts.as_deref(),
            now_ms,
        ));
    // Operator mutes (`ft mute add <key>`) suppress matching live facts.
    input
        .notification_mutes
        .extend(audit.mutes.iter().map(attention_mute_from_record));
}

/// Selects the [`PolicyGateAuditRow`] fields (pane = `target_pane_id`, as the
/// storage audit reader maps it). Input, reason and context columns are never
/// selected.
const LIVE_POLICY_GATE_COLUMNS: &str =
    "a.ts, a.actor_kind, a.actor_id, a.target_pane_id, a.action_kind, a.rule_id";

fn policy_gate_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<PolicyGateAuditRow> {
    Ok(PolicyGateAuditRow {
        ts: row.get(0)?,
        actor_kind: row.get(1)?,
        actor_id: row.get(2)?,
        pane_id: row
            .get::<_, Option<i64>>(3)?
            .and_then(|pane_id| u64::try_from(pane_id).ok()),
        action_kind: row.get(4)?,
        rule_id: row.get(5)?,
    })
}

fn open_read_only(db_path: &Path) -> rusqlite::Result<rusqlite::Connection> {
    let conn = rusqlite::Connection::open_with_flags(
        db_path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    conn.busy_timeout(std::time::Duration::from_secs(5))?;
    Ok(conn)
}

/// Recent denied and unresolved approval-required audit rows, verified-send
/// receipts, and active `ft mute` records, within the attention router's
/// window and row caps.
#[must_use]
pub fn read_live_attention_audit(db_path: &Path, now_ms: u64) -> LiveAttentionAudit {
    if !db_path.exists() {
        return LiveAttentionAudit::empty();
    }
    let Ok(conn) = open_read_only(db_path) else {
        return LiveAttentionAudit::unreadable();
    };
    let since = i64::try_from(now_ms.saturating_sub(POLICY_GATE_AUDIT_WINDOW_MS)).unwrap_or(0);
    let now = i64::try_from(now_ms).unwrap_or(i64::MAX);
    let gate_limit = i64::try_from(POLICY_GATE_AUDIT_MAX_ROWS).unwrap_or(i64::MAX);
    let receipt_limit = i64::try_from(VERIFIED_SUBMIT_AUDIT_MAX_ROWS).unwrap_or(i64::MAX);
    LiveAttentionAudit {
        denied: query_policy_gate_rows(
            &conn,
            &format!(
                "SELECT {LIVE_POLICY_GATE_COLUMNS} FROM audit_actions a \
                 WHERE a.policy_decision = 'deny' AND a.ts >= ?1 \
                 ORDER BY a.ts DESC, a.id DESC LIMIT ?2"
            ),
            since,
            gate_limit,
        )
        .ok(),
        // An approval-required action followed by an allowed action from the
        // same actor, action kind and pane was approved and ran.
        approval_required: query_policy_gate_rows(
            &conn,
            &format!(
                "SELECT {LIVE_POLICY_GATE_COLUMNS} FROM audit_actions a \
                 WHERE a.policy_decision = 'require_approval' AND a.ts >= ?1 \
                 AND NOT EXISTS (SELECT 1 FROM audit_actions b \
                     WHERE b.target_pane_id IS a.target_pane_id AND b.ts > a.ts \
                     AND b.policy_decision = 'allow' AND b.action_kind = a.action_kind \
                     AND b.actor_kind = a.actor_kind AND b.actor_id IS a.actor_id) \
                 ORDER BY a.ts DESC, a.id DESC LIMIT ?2"
            ),
            since,
            gate_limit,
        )
        .ok(),
        receipts: query_verified_send_receipts(&conn, since, receipt_limit).ok(),
        mutes: query_active_event_mutes(&conn, now).unwrap_or_default(),
    }
}

fn query_policy_gate_rows(
    conn: &rusqlite::Connection,
    sql: &str,
    since: i64,
    limit: i64,
) -> rusqlite::Result<Vec<PolicyGateAuditRow>> {
    let mut statement = conn.prepare(sql)?;
    let rows = statement.query_map(rusqlite::params![since, limit], policy_gate_row)?;
    rows.collect()
}

/// Verified sends attach their SubmitReceipt to the allowed send_text audit
/// row; summaries that are not receipts are skipped.
fn query_verified_send_receipts(
    conn: &rusqlite::Connection,
    since: i64,
    limit: i64,
) -> rusqlite::Result<Vec<(i64, SubmitReceipt)>> {
    let mut statement = conn.prepare(
        "SELECT ts, verification_summary FROM audit_actions \
         WHERE action_kind = 'send_text' AND policy_decision = 'allow' AND ts >= ?1 \
         AND verification_summary IS NOT NULL \
         ORDER BY ts DESC, id DESC LIMIT ?2",
    )?;
    let rows = statement.query_map(rusqlite::params![since, limit], |row| {
        Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
    })?;
    let rows = rows.collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows
        .into_iter()
        .filter_map(|(ts, summary)| {
            serde_json::from_str::<SubmitReceipt>(&summary)
                .ok()
                .map(|receipt| (ts, receipt))
        })
        .collect())
}

/// Active `ft mute` records, with the storage mute reader's predicate.
fn query_active_event_mutes(
    conn: &rusqlite::Connection,
    now_ms: i64,
) -> rusqlite::Result<Vec<EventMuteRecord>> {
    let mut statement = conn.prepare(
        "SELECT identity_key, scope, created_at, expires_at, created_by, reason \
         FROM event_mutes WHERE expires_at IS NULL OR expires_at > ?1",
    )?;
    let rows = statement.query_map(rusqlite::params![now_ms], |row| {
        Ok(EventMuteRecord {
            identity_key: row.get(0)?,
            scope: row.get(1)?,
            created_at: row.get(2)?,
            expires_at: row.get(3)?,
            created_by: row.get(4)?,
            reason: row.get(5)?,
        })
    })?;
    rows.collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_workspace_without_a_database_is_empty_and_never_created() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("ft.db");
        let audit = read_live_attention_audit(&db_path, 1_000);
        assert_eq!(audit.denied, Some(Vec::new()));
        assert_eq!(audit.approval_required, Some(Vec::new()));
        assert_eq!(audit.receipts.as_ref().map(Vec::len), Some(0));
        assert!(!db_path.exists(), "a read never creates the database");
    }

    #[test]
    fn an_unopenable_database_reads_as_unreadable_and_adds_unavailable_sources() {
        let dir = tempfile::tempdir().expect("tempdir");
        // A directory where the database file should be cannot be opened.
        let db_path = dir.path().join("ft.db");
        std::fs::create_dir(&db_path).expect("directory in place of the db");
        let audit = read_live_attention_audit(&db_path, 1_000);
        assert!(audit.denied.is_none() && audit.receipts.is_none());

        let mut input = AttentionRouterSourceAdapterInput::new(1_000, "workspace");
        apply_live_attention_audit(&mut input, &audit);
        assert_eq!(input.observations.len(), 3, "denied, approvals, receipts");
        assert!(input.notification_mutes.is_empty());
    }
}
