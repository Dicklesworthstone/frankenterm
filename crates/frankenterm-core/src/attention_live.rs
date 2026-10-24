//! Live, read-only sources for the attention router, shared by
//! `ft attention` / `ft robot attention` and MCP `wa.attention`
//! (ft-7h5da.7.11, ft-7h5da.7.7, ft-7h5da.3.9): the workspace database's
//! audit log, events, reservations and mutes, plus (CLI only) the crash
//! bundle directory, the workspace's `git status` and its ready beads
//! (`br ready`), each under a time and output bound.
//!
//! The workspace database is opened read-only: no migration, no writer, and
//! nothing created, so attention stays read-only. Only the columns the router
//! needs are selected; action input, the policy's free-text reason and the
//! decision context never leave the database.

use std::path::Path;

use crate::attention_router::{
    ActiveTxRow, AttentionRouterNotificationMute, AttentionRouterSourceAdapterInput,
    CrashBundleRow, LiveGitStatus, LiveReadyBeads, MANUAL_RESERVATIONS_MAX_ROWS,
    ManualReservationRow, POLICY_GATE_AUDIT_MAX_ROWS, POLICY_GATE_AUDIT_WINDOW_MS,
    PolicyGateAuditRow, RECENT_CRASH_BUNDLES_MAX, RECENT_CRASH_BUNDLES_WINDOW_MS,
    UNHANDLED_EVENTS_MAX_ROWS, UNHANDLED_EVENTS_WINDOW_MS, UnhandledEventRow,
    VERIFIED_SUBMIT_AUDIT_MAX_ROWS, approval_required_observation_from_audit,
    manual_reservations_observation, parse_br_ready_json, parse_git_status_porcelain_v2,
    policy_denied_observation_from_audit, unhandled_events_observation,
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
    /// Unhandled warning and critical detection events.
    pub events: Option<Vec<UnhandledEventRow>>,
    /// Active, unexpired manual pane reservations (operator holds).
    pub reservations: Option<Vec<ManualReservationRow>>,
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
            events: None,
            reservations: None,
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
            events: Some(Vec::new()),
            reservations: Some(Vec::new()),
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

/// Add the database-backed live sources (policy denials, unresolved approval
/// holds, verified-submit drift, unhandled events, manual pane holds) and the
/// active `ft mute` records to `input`.
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
    input.observations.push(unhandled_events_observation(
        audit.events.as_deref(),
        now_ms,
    ));
    input.observations.push(manual_reservations_observation(
        audit.reservations.as_deref(),
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
        events: query_unhandled_events(
            &conn,
            i64::try_from(now_ms.saturating_sub(UNHANDLED_EVENTS_WINDOW_MS)).unwrap_or(0),
            i64::try_from(UNHANDLED_EVENTS_MAX_ROWS).unwrap_or(i64::MAX),
        )
        .ok(),
        reservations: query_active_manual_reservations(
            &conn,
            now,
            i64::try_from(MANUAL_RESERVATIONS_MAX_ROWS).unwrap_or(i64::MAX),
        )
        .ok(),
        mutes: query_active_event_mutes(&conn, now).unwrap_or_default(),
    }
}

/// The watcher's crash bundles written within the attention window, newest
/// first, for the live `incident_bundles` source, and whether the crash
/// module's bounded discovery could prove its list complete (an unreadable
/// bundle or a directory changing mid-scan makes it incomplete, which the
/// source reports instead of claiming "no crash"). Each bundle's crash time
/// comes from its report, falling back to the directory's modification time;
/// its message and backtrace never leave this function. A missing directory
/// means no crash; `None` means the path is not a readable directory.
#[must_use]
pub fn read_recent_crash_bundles(
    crash_dir: &Path,
    now_ms: u64,
) -> Option<(Vec<CrashBundleRow>, bool)> {
    if !crash_dir.exists() {
        return Some((Vec::new(), true));
    }
    if !crash_dir.is_dir() {
        return None;
    }
    let since = now_ms.saturating_sub(RECENT_CRASH_BUNDLES_WINDOW_MS);
    let discovery = crate::crash::discover_crash_bundles(crash_dir, RECENT_CRASH_BUNDLES_MAX);
    let complete = discovery.completeness.is_complete();
    let bundles = discovery
        .bundles
        .into_iter()
        .filter_map(|bundle| {
            let name = bundle.path.file_name()?.to_string_lossy().into_owned();
            let crashed_at_ms = match bundle.report.as_ref() {
                Some(report) => report.timestamp.saturating_mul(1000),
                None => std::fs::metadata(&bundle.path)
                    .and_then(|metadata| metadata.modified())
                    .ok()?
                    .duration_since(std::time::UNIX_EPOCH)
                    .ok()
                    .and_then(|age| u64::try_from(age.as_millis()).ok())?,
            };
            (crashed_at_ms >= since).then_some(CrashBundleRow {
                name,
                crashed_at_ms,
            })
        })
        .collect();
    Some((bundles, complete))
}

/// The workspace's active mission transaction contract
/// (`<ft_dir>/mission/tx-active.json`, the `ft tx` default): only its id and
/// lifecycle are deserialized. `Some(None)` means there is none; `None` means
/// it exists but could not be read, is oversized, or does not parse.
#[must_use]
pub fn read_active_tx(ft_dir: &Path) -> Option<Option<ActiveTxRow>> {
    use std::io::Read as _;

    // The same bound `ft tx` loads contracts under.
    let max_bytes = u64::try_from(crate::tx_execution::TX_CONTRACT_MAX_BYTES).unwrap_or(u64::MAX);

    #[derive(serde::Deserialize)]
    struct ContractIntent {
        tx_id: String,
    }

    #[derive(serde::Deserialize)]
    struct ContractLifecycle {
        intent: ContractIntent,
        lifecycle_state: crate::plan::MissionTxState,
    }

    let path = ft_dir.join("mission").join("tx-active.json");
    let file = match std::fs::File::open(&path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Some(None),
        Err(_) => return None,
    };
    if !file.metadata().ok()?.is_file() {
        return None;
    }
    // Bound the read itself, so a file grown or swapped after the size check
    // is still never read past the cap.
    let mut text = String::new();
    file.take(max_bytes.saturating_add(1))
        .read_to_string(&mut text)
        .ok()?;
    if u64::try_from(text.len()).unwrap_or(u64::MAX) > max_bytes {
        return None;
    }
    let contract: ContractLifecycle = serde_json::from_str(&text).ok()?;
    Some(Some(ActiveTxRow {
        tx_id: contract.intent.tx_id,
        state: contract.lifecycle_state,
    }))
}

/// How long a live `git` or `br` read may take.
const LIVE_COMMAND_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
/// Output a live `git` or `br` read may produce.
const LIVE_COMMAND_STDOUT_LIMIT: usize = 8 * 1024 * 1024;

fn run_live_command(
    program: &str,
    args: &[&str],
    workspace_root: &Path,
) -> std::io::Result<std::process::Output> {
    let mut command = crate::runtime_async::process::Command::new(program);
    command
        .args(args)
        .current_dir(workspace_root)
        // Stable diagnostics to classify, and no index refresh: a plain
        // `git status` takes index.lock in a tree other agents commit to.
        .env("LC_ALL", "C")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .kill_on_drop(true)
        .stdout_limit(LIVE_COMMAND_STDOUT_LIMIT);
    command.output_blocking(LIVE_COMMAND_TIMEOUT)
}

/// The workspace's `git status`, for the live `git` source. Read-only, with
/// a time and output bound; paths stay in the facts as listed paths only.
#[must_use]
pub fn read_git_status(workspace_root: &Path) -> LiveGitStatus {
    let output = match run_live_command(
        "git",
        &[
            "status",
            "--porcelain=v2",
            "--branch",
            "-z",
            "--untracked-files=normal",
        ],
        workspace_root,
    ) {
        Ok(output) => output,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return LiveGitStatus::Unavailable("git.not_installed");
        }
        Err(_) => return LiveGitStatus::Unavailable("git.status_failed"),
    };
    if !output.status.success() {
        return if String::from_utf8_lossy(&output.stderr).contains("not a git repository") {
            LiveGitStatus::NotARepository
        } else {
            LiveGitStatus::Unavailable("git.status_failed")
        };
    }
    parse_git_status_porcelain_v2(&output.stdout).map_or(
        LiveGitStatus::Unavailable("git.status_unparseable"),
        LiveGitStatus::Read,
    )
}

/// Ready beads, for the live `beads` source: `br ready --json` in the
/// workspace, without `br`'s automatic JSONL import or export, so reading
/// attention never rewrites the tracker. A workspace without `.beads` has no
/// tracker.
#[must_use]
pub fn read_ready_beads(workspace_root: &Path) -> LiveReadyBeads {
    if !workspace_root.join(".beads").is_dir() {
        return LiveReadyBeads::NotConfigured;
    }
    let output = match run_live_command(
        "br",
        &[
            "--no-auto-import",
            "--no-auto-flush",
            "ready",
            "--json",
            "--sort",
            "priority",
        ],
        workspace_root,
    ) {
        Ok(output) => output,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return LiveReadyBeads::Unavailable("beads.br_not_installed");
        }
        Err(_) => return LiveReadyBeads::Unavailable("beads.br_failed"),
    };
    if !output.status.success() {
        return LiveReadyBeads::Unavailable("beads.br_failed");
    }
    parse_br_ready_json(&output.stdout).map_or(
        LiveReadyBeads::Unavailable("beads.br_output_unparseable"),
        LiveReadyBeads::Read,
    )
}

/// Active, unexpired manual reservations, with the storage reader's
/// predicate. The free-text reason column is never selected.
fn query_active_manual_reservations(
    conn: &rusqlite::Connection,
    now_ms: i64,
    limit: i64,
) -> rusqlite::Result<Vec<ManualReservationRow>> {
    let mut statement = conn.prepare(
        "SELECT id, pane_id, owner_id, created_at, expires_at FROM pane_reservations \
         WHERE status = 'active' AND expires_at > ?1 AND owner_kind = 'manual' \
         ORDER BY created_at DESC, id DESC LIMIT ?2",
    )?;
    let rows = statement.query_map(rusqlite::params![now_ms, limit], |row| {
        Ok(ManualReservationRow {
            reservation_id: row.get(0)?,
            pane_id: u64::try_from(row.get::<_, i64>(1)?).unwrap_or(0),
            owner_id: row.get(2)?,
            created_at: row.get(3)?,
            expires_at: row.get(4)?,
        })
    })?;
    rows.collect()
}

/// Unhandled warning and critical events, grouped in SQL by pane, rule, type
/// and extracted values (so a detection storm costs one row, and the cap
/// counts groups, not repeats), newest group first, each with the identity
/// key `ft mute` uses (computed as storage computes it). An event whose
/// handling workflow failed is still unhandled. Events triaged as resolved
/// or dismissed, and the watcher's own drift events (already the
/// `verified_submit` item), are left out. Extracted values feed only the
/// redacted identity hash; matched text is never selected.
fn query_unhandled_events(
    conn: &rusqlite::Connection,
    since: i64,
    limit: i64,
) -> rusqlite::Result<Vec<UnhandledEventRow>> {
    let mut statement = conn.prepare(
        "SELECT e.pane_id, e.rule_id, e.event_type, e.extracted, p.pane_uuid, COUNT(*), \
         MAX(e.detected_at), MAX(e.severity = 'critical'), MAX(e.handled_at IS NOT NULL) \
         FROM events e LEFT JOIN panes p ON p.pane_id = e.pane_id \
         WHERE (e.handled_at IS NULL OR e.handled_status IN ('failed', 'error')) \
         AND e.severity IN ('warning', 'critical') AND e.detected_at >= ?1 \
         AND (e.triage_state IS NULL OR lower(e.triage_state) NOT IN ('resolved', 'dismissed')) \
         AND e.rule_id <> ?3 \
         GROUP BY e.pane_id, e.rule_id, e.event_type, e.extracted, p.pane_uuid \
         ORDER BY MAX(e.detected_at) DESC LIMIT ?2",
    )?;
    let rows = statement.query_map(
        rusqlite::params![
            since,
            limit,
            crate::attention_router::VERIFIED_SUBMIT_DRIFT_RULE_ID
        ],
        |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, Option<String>>(4)?,
                row.get::<_, i64>(5)?,
                row.get::<_, i64>(6)?,
                row.get::<_, i64>(7)? != 0,
                row.get::<_, i64>(8)? != 0,
            ))
        },
    )?;
    let rows = rows.collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows
        .into_iter()
        .map(
            |(
                pane_id,
                rule_id,
                event_type,
                extracted,
                pane_uuid,
                occurrences,
                newest_at,
                critical,
                handling_failed,
            )| {
                let pane_id = u64::try_from(pane_id).unwrap_or(0);
                let detection = crate::patterns::Detection {
                    rule_id: rule_id.clone(),
                    agent_type: crate::patterns::AgentType::Unknown,
                    event_type,
                    severity: crate::patterns::Severity::Info,
                    confidence: 0.0,
                    extracted: extracted
                        .as_deref()
                        .and_then(|text| serde_json::from_str(text).ok())
                        .unwrap_or(serde_json::Value::Null),
                    matched_text: String::new(),
                    span: (0, 0),
                };
                UnhandledEventRow {
                    pane_id,
                    identity_key: crate::events::event_identity_key(
                        &detection,
                        pane_id,
                        pane_uuid.as_deref(),
                    ),
                    rule_id,
                    occurrences: u64::try_from(occurrences).unwrap_or(0),
                    newest_at,
                    critical,
                    handling_failed,
                }
            },
        )
        .collect())
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
        assert_eq!(audit.events, Some(Vec::new()));
        assert_eq!(audit.reservations, Some(Vec::new()));
        assert!(!db_path.exists(), "a read never creates the database");
    }

    #[test]
    fn active_tx_reads_only_id_and_lifecycle_and_fails_closed() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert_eq!(read_active_tx(dir.path()), Some(None), "no contract, no tx");
        let mission = dir.path().join("mission");
        std::fs::create_dir(&mission).expect("mission dir");
        let contract = mission.join("tx-active.json");
        std::fs::write(
            &contract,
            r#"{"tx_version":1,"intent":{"tx_id":"tx:deploy-42","objective":"SECRET OBJECTIVE"},
               "lifecycle_state":"failed","receipts":[]}"#,
        )
        .expect("contract");
        assert_eq!(
            read_active_tx(dir.path()),
            Some(Some(ActiveTxRow {
                tx_id: "tx:deploy-42".to_string(),
                state: crate::plan::MissionTxState::Failed,
            }))
        );
        std::fs::write(&contract, "not json").expect("garbage");
        assert_eq!(
            read_active_tx(dir.path()),
            None,
            "unparseable is unreadable"
        );
    }

    #[test]
    fn recent_crash_bundles_surface_only_their_name_and_age() {
        const DAY_MS: u64 = 24 * 60 * 60 * 1000;
        let dir = tempfile::tempdir().expect("tempdir");
        let crash_dir = dir.path().join("crash");
        assert_eq!(
            read_recent_crash_bundles(&crash_dir, 1_000),
            Some((Vec::new(), true)),
            "no crash directory, no crash"
        );
        std::fs::create_dir(&crash_dir).expect("crash dir");
        // The crash time comes from the report, not the directory mtime.
        let crashed_at_s: u64 = 1_770_000_000;
        let report = crate::crash::CrashReport {
            message: "SECRET PANIC".to_string(),
            location: Some("src/secret.rs:1:1".to_string()),
            backtrace: None,
            timestamp: crashed_at_s,
            pid: 1,
            thread_name: None,
        };
        let bundle = crate::crash::write_crash_bundle(&crash_dir, &report, None, None)
            .expect("write crash bundle");
        let now_ms = crashed_at_s * 1000 + 60_000;
        let (recent, complete) = read_recent_crash_bundles(&crash_dir, now_ms).expect("readable");
        assert!(complete);
        assert_eq!(recent.len(), 1);
        assert_eq!(
            recent[0].name,
            bundle.file_name().expect("name").to_string_lossy()
        );
        assert_eq!(recent[0].crashed_at_ms, crashed_at_s * 1000);
        assert_eq!(
            read_recent_crash_bundles(&crash_dir, now_ms + 2 * DAY_MS).map(|(rows, _)| rows.len()),
            Some(0),
            "a bundle older than the window is not an attention item"
        );
        let observation =
            crate::attention_router::crash_bundles_observation(Some(&recent), complete, now_ms);
        let rendered = serde_json::to_string(&observation).expect("serialize");
        assert!(
            !rendered.contains("SECRET PANIC") && !rendered.contains("secret.rs"),
            "{rendered}"
        );
        assert!(
            rendered.contains("ft reproduce export --kind crash"),
            "{rendered}"
        );
    }

    #[test]
    fn manual_reservations_are_active_unexpired_operator_holds_without_their_reason() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("ft.db");
        let conn = rusqlite::Connection::open(&db_path).expect("create db");
        conn.execute_batch(
            "CREATE TABLE pane_reservations (id INTEGER PRIMARY KEY, pane_id INTEGER NOT NULL,
                 owner_kind TEXT NOT NULL, owner_id TEXT NOT NULL, reason TEXT,
                 created_at INTEGER NOT NULL, expires_at INTEGER NOT NULL,
                 released_at INTEGER, status TEXT NOT NULL DEFAULT 'active');
             INSERT INTO pane_reservations VALUES
                 (1, 7, 'manual', 'operator', 'SECRET REASON', 1000, 900000, NULL, 'active'),
                 (2, 8, 'manual', 'operator', NULL, 1000, 2000, NULL, 'active'),
                 (3, 9, 'manual', 'operator', NULL, 1000, 900000, 5000, 'released'),
                 (4, 10, 'workflow', 'wf-1', NULL, 1000, 900000, NULL, 'active');",
        )
        .expect("schema and rows");
        drop(conn);

        let audit = read_live_attention_audit(&db_path, 300_000);
        let reservations = audit.reservations.as_ref().expect("reservations readable");
        assert_eq!(
            reservations.as_slice(),
            [ManualReservationRow {
                reservation_id: 1,
                pane_id: 7,
                owner_id: "operator".to_string(),
                created_at: 1000,
                expires_at: 900_000,
            }],
            "expired, released and workflow holds are left out"
        );
        let mut input = AttentionRouterSourceAdapterInput::new(300_000, "workspace");
        apply_live_attention_audit(&mut input, &audit);
        let rendered = serde_json::to_string(&input).expect("serialize");
        assert!(!rendered.contains("SECRET REASON"), "{rendered}");
        assert!(rendered.contains("expires in 600s"), "{rendered}");
    }

    #[test]
    fn unhandled_events_are_recent_untriaged_warnings_or_criticals_keyed_like_ft_mute() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("ft.db");
        let now_ms: u64 = 200_000_000;
        let recent: i64 = 199_940_000;
        let older: i64 = 199_900_000;
        let conn = rusqlite::Connection::open(&db_path).expect("create db");
        conn.execute_batch(&format!(
            "CREATE TABLE panes (pane_id INTEGER PRIMARY KEY, pane_uuid TEXT);
             CREATE TABLE events (id INTEGER PRIMARY KEY, pane_id INTEGER NOT NULL,
                 rule_id TEXT NOT NULL, event_type TEXT NOT NULL, severity TEXT NOT NULL,
                 extracted TEXT, matched_text TEXT, detected_at INTEGER NOT NULL,
                 handled_at INTEGER, handled_status TEXT, triage_state TEXT);
             INSERT INTO panes VALUES (3, 'uuid-3');
             INSERT INTO events VALUES (1, 3, 'codex.usage.reached', 'usage.reached',
                 'critical', '{{\"reset\":\"5pm\"}}', 'SECRET TEXT', {recent}, NULL, NULL, NULL);
             INSERT INTO events VALUES (6, 3, 'codex.usage.reached', 'usage.reached',
                 'warning', '{{\"reset\":\"5pm\"}}', NULL, {older}, NULL, NULL, NULL);
             INSERT INTO events VALUES (7, 3, 'codex.usage.reached', 'usage.reached',
                 'warning', '{{\"reset\":\"5pm\"}}', NULL, {older}, {recent}, 'error', NULL);
             INSERT INTO events VALUES (2, 3, 'codex.usage.reached', 'usage.reached',
                 'info', NULL, NULL, {recent}, NULL, NULL, NULL);
             INSERT INTO events VALUES (3, 3, 'codex.usage.reached', 'usage.reached',
                 'warning', NULL, NULL, {recent}, {recent}, 'completed', NULL);
             INSERT INTO events VALUES (4, 3, 'codex.usage.reached', 'usage.reached',
                 'warning', NULL, NULL, {recent}, NULL, NULL, 'Resolved');
             INSERT INTO events VALUES (8, 3, 'core.verified_submit:profile_drift',
                 'submit_profile.drift', 'warning', NULL, NULL, {recent}, NULL, NULL, NULL);
             INSERT INTO events VALUES (5, 4, 'claude.compaction', 'session.compaction',
                 'warning', NULL, NULL, 1000, NULL, NULL, NULL);"
        ))
        .expect("schema and rows");
        drop(conn);

        let audit = read_live_attention_audit(&db_path, now_ms);
        let events = audit.events.as_ref().expect("events readable");
        assert_eq!(
            events.len(),
            1,
            "info, completed, resolved (any case), drift and stale events are left out; \
             the three identical ones are one group: {events:?}"
        );
        assert_eq!(events[0].occurrences, 3);
        assert_eq!(events[0].newest_at, recent);
        assert!(events[0].critical);
        assert!(
            events[0].handling_failed,
            "the 'error' handling still counts"
        );
        let detection = crate::patterns::Detection {
            rule_id: "codex.usage.reached".to_string(),
            agent_type: crate::patterns::AgentType::Unknown,
            event_type: "usage.reached".to_string(),
            severity: crate::patterns::Severity::Info,
            confidence: 0.0,
            extracted: serde_json::json!({"reset": "5pm"}),
            matched_text: String::new(),
            span: (0, 0),
        };
        assert_eq!(
            events[0].identity_key,
            crate::events::event_identity_key(&detection, 3, Some("uuid-3"))
        );
        assert_eq!(events[0].pane_id, 3);
        // No audit table here: those inputs fail on their own, events still read.
        assert!(audit.denied.is_none() && audit.receipts.is_none());

        let mut input = AttentionRouterSourceAdapterInput::new(now_ms, "workspace");
        apply_live_attention_audit(&mut input, &audit);
        let rendered = serde_json::to_string(&input).expect("serialize");
        assert!(
            !rendered.contains("SECRET TEXT") && !rendered.contains("5pm"),
            "{rendered}"
        );
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
        assert_eq!(
            input.observations.len(),
            5,
            "denied, approvals, receipts, events, reservations"
        );
        assert!(input.notification_mutes.is_empty());
    }

    #[test]
    fn live_git_and_beads_sources_read_the_workspace() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert_eq!(read_ready_beads(dir.path()), LiveReadyBeads::NotConfigured);
        let Ok(init) = std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(dir.path())
            .status()
        else {
            eprintln!("git is not installed; skipping the live git read");
            return;
        };
        assert!(init.success());
        std::fs::write(dir.path().join("new file.txt"), "x").expect("write");
        match read_git_status(dir.path()) {
            LiveGitStatus::Read(summary) => {
                assert_eq!(summary.uncommitted_paths, ["new file.txt"]);
                assert_eq!(summary.untracked, 1);
                assert!(summary.staged_paths.is_empty());
            }
            other => panic!("expected a readable work tree: {other:?}"),
        }
    }
}
