//! Deterministic context-horizon risk prediction.
//!
//! This module is intentionally read-only. It consumes already-redacted,
//! structured context evidence and produces the v1 horizon DTO shape without
//! reading pane text or storing prompt/transcript content.

use std::collections::BTreeSet;
use std::path::Path;

use rusqlite::OptionalExtension;
use serde::{Deserialize, Serialize};

pub const CONTEXT_HORIZON_CONTRACT_ID: &str = "ft.context_horizon.v1";
pub const CONTEXT_HORIZON_SCHEMA_VERSION: u16 = 1;
const DEFAULT_HORIZON_WINDOW_MS: u64 = 15 * 60 * 1000;

#[derive(Debug, thiserror::Error)]
pub enum ContextHorizonSqliteError {
    #[error("pane_id {0} exceeds SQLite INTEGER range")]
    PaneIdOutOfRange(u64),
    #[error("failed to open context database {path}: {source}")]
    Open {
        path: String,
        source: rusqlite::Error,
    },
    #[error("failed to query context horizon {operation}: {source}")]
    Query {
        operation: &'static str,
        source: rusqlite::Error,
    },
}

type SqliteResult<T> = std::result::Result<T, ContextHorizonSqliteError>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextHorizonEvidenceState {
    Measured,
    Inferred,
    Simulated,
    Stale,
    Unavailable,
    Mixed,
}

impl ContextHorizonEvidenceState {
    fn combine(states: impl IntoIterator<Item = Self>) -> Self {
        let states = states.into_iter().collect::<BTreeSet<_>>();
        match states.len() {
            0 => Self::Unavailable,
            1 => *states.iter().next().expect("state set has one entry"),
            _ => Self::Mixed,
        }
    }

    fn penalty(self) -> f64 {
        match self {
            Self::Measured => 0.0,
            Self::Inferred => 0.1,
            Self::Simulated => 0.25,
            Self::Stale => 0.35,
            Self::Unavailable => 0.5,
            Self::Mixed => 0.2,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextHorizonRiskTier {
    Unknown,
    Green,
    Yellow,
    Red,
    Black,
}

impl ContextHorizonRiskTier {
    fn from_pressure_tier(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "unknown" => Some(Self::Unknown),
            "green" => Some(Self::Green),
            "yellow" => Some(Self::Yellow),
            "red" => Some(Self::Red),
            "black" => Some(Self::Black),
            _ => None,
        }
    }

    fn from_utilization(utilization: f64) -> Self {
        if utilization >= 0.98 {
            Self::Black
        } else if utilization >= 0.90 {
            Self::Red
        } else if utilization >= 0.75 {
            Self::Yellow
        } else {
            Self::Green
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextHorizonFailureClass {
    SourceRegression,
    PrivacyViolation,
    EnvironmentBlocked,
    UnavailableEvidence,
    TargetHardwareSkipped,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextHorizonActionKind {
    RotateContext,
    PrepareHandoff,
    ReduceFanout,
    PauseAssignment,
    InspectPrompt,
    CollectIncidentBundle,
    None,
}

impl ContextHorizonActionKind {
    fn id_fragment(self) -> &'static str {
        match self {
            Self::RotateContext => "rotate_context",
            Self::PrepareHandoff => "prepare_handoff",
            Self::ReduceFanout => "reduce_fanout",
            Self::PauseAssignment => "pause_assignment",
            Self::InspectPrompt => "inspect_prompt",
            Self::CollectIncidentBundle => "collect_incident_bundle",
            Self::None => "none",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextHorizonScope {
    Pane,
    Fleet,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextHorizonPolicyState {
    AllowedDryRun,
    RequiresApproval,
    Blocked,
    Unavailable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextHorizonHandoffReadiness {
    NotNeeded,
    Prepare,
    Ready,
    Blocked,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextHorizonUnavailableDomain {
    pub domain: String,
    pub evidence_state: ContextHorizonEvidenceState,
    pub reason_codes: Vec<String>,
    pub failure_class: ContextHorizonFailureClass,
}

impl ContextHorizonUnavailableDomain {
    #[must_use]
    pub fn unavailable(domain: impl Into<String>, reason_code: impl Into<String>) -> Self {
        Self {
            domain: domain.into(),
            evidence_state: ContextHorizonEvidenceState::Unavailable,
            reason_codes: vec![reason_code.into()],
            failure_class: ContextHorizonFailureClass::UnavailableEvidence,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextHorizonCitation {
    pub citation_id: String,
    pub source: String,
    pub evidence_state: ContextHorizonEvidenceState,
    pub redacted: bool,
    pub summary: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ContextHorizonPaneEvidence {
    pub pane_id: u64,
    pub active_context_present: bool,
    pub token_budget: Option<i64>,
    pub tokens_consumed: Option<i64>,
    pub pressure_tier: Option<String>,
    pub compaction_count: Option<i64>,
    pub last_rotated_at_ms: Option<i64>,
    pub last_activity_at_ms: Option<i64>,
    pub previous_utilization: Option<f64>,
    pub recent_rate_limit_events: u32,
    pub recent_compaction_events: u32,
    pub evidence_state: ContextHorizonEvidenceState,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ContextHorizonInput {
    pub generated_at_ms: u64,
    pub horizon_window_ms: u64,
    pub panes: Vec<ContextHorizonPaneEvidence>,
    pub unavailable_domains: Vec<ContextHorizonUnavailableDomain>,
    pub artifact_paths: Vec<String>,
}

impl ContextHorizonInput {
    #[must_use]
    pub fn new(generated_at_ms: u64) -> Self {
        Self {
            generated_at_ms,
            horizon_window_ms: DEFAULT_HORIZON_WINDOW_MS,
            panes: Vec::new(),
            unavailable_domains: Vec::new(),
            artifact_paths: Vec::new(),
        }
    }
}

#[derive(Debug, Clone)]
struct SqliteContextRow {
    depth: i64,
    token_budget: i64,
    tokens_consumed: i64,
    pressure_tier: String,
    created_at_ms: i64,
}

#[derive(Debug, Clone, Copy)]
struct SqliteEventEvidence {
    recent_rate_limit_events: u32,
    last_activity_at_ms: Option<i64>,
    events_available: bool,
}

#[must_use]
pub fn context_horizon_unavailable_report(
    generated_at_ms: u64,
    horizon_window_ms: u64,
    source: &str,
    domain: impl Into<String>,
    reason_code: impl Into<String>,
    failure_class: ContextHorizonFailureClass,
) -> ContextHorizonReport {
    let mut input = ContextHorizonInput::new(generated_at_ms);
    input.horizon_window_ms = horizon_window_ms.max(1);
    input
        .unavailable_domains
        .push(ContextHorizonUnavailableDomain {
            domain: domain.into(),
            evidence_state: ContextHorizonEvidenceState::Unavailable,
            reason_codes: vec![reason_code.into()],
            failure_class,
        });
    let mut report = predict_context_horizon(&input);
    report.source = source.to_string();
    report
}

pub fn predict_context_horizon_from_sqlite(
    db_path: &Path,
    pane_id: Option<u64>,
    generated_at_ms: u64,
    horizon_window_ms: u64,
    source: &str,
) -> SqliteResult<ContextHorizonReport> {
    let horizon_window_ms = horizon_window_ms.max(1);
    if !db_path.exists() {
        return Ok(context_horizon_unavailable_report(
            generated_at_ms,
            horizon_window_ms,
            source,
            "native_context_registry",
            "evidence.context_database_missing",
            ContextHorizonFailureClass::EnvironmentBlocked,
        ));
    }

    let conn =
        rusqlite::Connection::open_with_flags(db_path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .map_err(|source| ContextHorizonSqliteError::Open {
                path: db_path.display().to_string(),
                source,
            })?;
    conn.busy_timeout(std::time::Duration::from_secs(2))
        .map_err(|source| ContextHorizonSqliteError::Query {
            operation: "busy_timeout",
            source,
        })?;

    let mut input = ContextHorizonInput::new(generated_at_ms);
    input.horizon_window_ms = horizon_window_ms;

    if !sqlite_table_exists(&conn, "pane_contexts")?
        || !sqlite_table_exists(&conn, "context_rotations")?
    {
        input
            .unavailable_domains
            .push(ContextHorizonUnavailableDomain {
                domain: "native_context_registry".to_string(),
                evidence_state: ContextHorizonEvidenceState::Unavailable,
                reason_codes: vec!["evidence.context_registry_tables_missing".to_string()],
                failure_class: ContextHorizonFailureClass::UnavailableEvidence,
            });
        let mut report = predict_context_horizon(&input);
        report.source = source.to_string();
        return Ok(report);
    }

    let events_available = sqlite_table_exists(&conn, "events")?;
    if !events_available {
        input
            .unavailable_domains
            .push(ContextHorizonUnavailableDomain {
                domain: "runtime_events".to_string(),
                evidence_state: ContextHorizonEvidenceState::Unavailable,
                reason_codes: vec!["evidence.runtime_events_table_missing".to_string()],
                failure_class: ContextHorizonFailureClass::UnavailableEvidence,
            });
    }

    let pane_ids = if let Some(pane_id) = pane_id {
        vec![sqlite_pane_id(pane_id)?]
    } else {
        sqlite_context_pane_ids(&conn)?
    };

    for pane_id in pane_ids {
        input.panes.push(sqlite_pane_evidence(
            &conn,
            pane_id,
            generated_at_ms,
            horizon_window_ms,
            events_available,
        )?);
    }

    let mut report = predict_context_horizon(&input);
    report.source = source.to_string();
    Ok(report)
}

fn sqlite_query_error(
    operation: &'static str,
) -> impl FnOnce(rusqlite::Error) -> ContextHorizonSqliteError {
    move |source| ContextHorizonSqliteError::Query { operation, source }
}

fn sqlite_pane_id(pane_id: u64) -> SqliteResult<i64> {
    i64::try_from(pane_id).map_err(|_| ContextHorizonSqliteError::PaneIdOutOfRange(pane_id))
}

fn sqlite_table_exists(conn: &rusqlite::Connection, table_name: &str) -> SqliteResult<bool> {
    conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1)",
        [table_name],
        |row| row.get::<_, i64>(0),
    )
    .map(|exists| exists != 0)
    .map_err(sqlite_query_error("table existence"))
}

fn sqlite_active_context_row(
    conn: &rusqlite::Connection,
    pane_id: i64,
) -> SqliteResult<Option<SqliteContextRow>> {
    conn.query_row(
        r"
        SELECT depth, token_budget, tokens_consumed, pressure_tier, created_at_ms
        FROM pane_contexts
        WHERE pane_id = ?1 AND state = 'active'
        ",
        [pane_id],
        |row| {
            Ok(SqliteContextRow {
                depth: row.get(0)?,
                token_budget: row.get(1)?,
                tokens_consumed: row.get(2)?,
                pressure_tier: row.get(3)?,
                created_at_ms: row.get(4)?,
            })
        },
    )
    .optional()
    .map_err(sqlite_query_error("active pane context"))
}

fn sqlite_context_pane_ids(conn: &rusqlite::Connection) -> SqliteResult<Vec<i64>> {
    let mut stmt = conn
        .prepare(
            r"
            SELECT pane_id FROM (
                SELECT pane_id FROM pane_contexts
                UNION
                SELECT pane_id FROM context_rotations
            )
            ORDER BY pane_id
            ",
        )
        .map_err(sqlite_query_error("context pane id prepare"))?;
    let rows = stmt
        .query_map([], |row| row.get::<_, i64>(0))
        .map_err(sqlite_query_error("context pane id query"))?;
    let mut pane_ids = Vec::new();
    for row in rows {
        pane_ids.push(row.map_err(sqlite_query_error("context pane id row"))?);
    }
    Ok(pane_ids)
}

fn sqlite_rotation_count(conn: &rusqlite::Connection, pane_id: i64) -> SqliteResult<i64> {
    conn.query_row(
        "SELECT COUNT(*) FROM context_rotations WHERE pane_id = ?1",
        [pane_id],
        |row| row.get(0),
    )
    .map_err(sqlite_query_error("context rotation count"))
}

fn sqlite_recent_compaction_count(
    conn: &rusqlite::Connection,
    pane_id: i64,
    cutoff_ms: i64,
) -> SqliteResult<u32> {
    let count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM context_rotations WHERE pane_id = ?1 AND rotated_at_ms >= ?2",
            rusqlite::params![pane_id, cutoff_ms],
            |row| row.get(0),
        )
        .map_err(sqlite_query_error("recent context rotations"))?;
    Ok(count.max(0).min(i64::from(u32::MAX)) as u32)
}

fn sqlite_last_rotation_at(conn: &rusqlite::Connection, pane_id: i64) -> SqliteResult<Option<i64>> {
    conn.query_row(
        r"
        SELECT rotated_at_ms FROM context_rotations
        WHERE pane_id = ?1
        ORDER BY rotated_at_ms DESC
        LIMIT 1
        ",
        [pane_id],
        |row| row.get(0),
    )
    .optional()
    .map_err(sqlite_query_error("last context rotation"))
}

fn sqlite_event_evidence(
    conn: &rusqlite::Connection,
    pane_id: i64,
    cutoff_ms: i64,
    events_available: bool,
) -> SqliteResult<SqliteEventEvidence> {
    if !events_available {
        return Ok(SqliteEventEvidence {
            recent_rate_limit_events: 0,
            last_activity_at_ms: None,
            events_available,
        });
    }

    let recent_rate_limit_events: i64 = conn
        .query_row(
            r"
            SELECT COUNT(*) FROM events
            WHERE pane_id = ?1
              AND detected_at >= ?2
              AND (
                lower(rule_id) LIKE '%rate%limit%'
                OR lower(rule_id) LIKE '%usage%limit%'
                OR lower(event_type) LIKE '%rate%limit%'
                OR lower(event_type) LIKE '%usage%limit%'
              )
            ",
            rusqlite::params![pane_id, cutoff_ms],
            |row| row.get(0),
        )
        .map_err(sqlite_query_error("recent rate-limit events"))?;
    let last_activity_at_ms = conn
        .query_row(
            "SELECT MAX(detected_at) FROM events WHERE pane_id = ?1",
            [pane_id],
            |row| row.get(0),
        )
        .optional()
        .map_err(sqlite_query_error("last event activity"))?
        .flatten();

    Ok(SqliteEventEvidence {
        recent_rate_limit_events: recent_rate_limit_events.max(0).min(i64::from(u32::MAX)) as u32,
        last_activity_at_ms,
        events_available,
    })
}

fn sqlite_pane_evidence(
    conn: &rusqlite::Connection,
    pane_id: i64,
    generated_at_ms: u64,
    horizon_window_ms: u64,
    events_available: bool,
) -> SqliteResult<ContextHorizonPaneEvidence> {
    let cutoff_ms =
        i64::try_from(generated_at_ms.saturating_sub(horizon_window_ms)).unwrap_or(i64::MAX);
    let active = sqlite_active_context_row(conn, pane_id)?;
    let rotation_count = sqlite_rotation_count(conn, pane_id)?;
    let recent_compaction_events = sqlite_recent_compaction_count(conn, pane_id, cutoff_ms)?;
    let last_rotated_at_ms = sqlite_last_rotation_at(conn, pane_id)?;
    let event_evidence = sqlite_event_evidence(conn, pane_id, cutoff_ms, events_available)?;

    let (
        active_context_present,
        token_budget,
        tokens_consumed,
        pressure_tier,
        active_depth,
        created_at_ms,
    ) = active.map_or_else(
        || (false, None, None, Some("unknown".to_string()), 0, None),
        |row| {
            (
                true,
                Some(row.token_budget),
                Some(row.tokens_consumed),
                Some(row.pressure_tier),
                row.depth,
                Some(row.created_at_ms),
            )
        },
    );
    let last_activity_at_ms = event_evidence
        .last_activity_at_ms
        .or(created_at_ms)
        .or(last_rotated_at_ms);
    let evidence_state = if active_context_present {
        if event_evidence.events_available {
            ContextHorizonEvidenceState::Measured
        } else {
            ContextHorizonEvidenceState::Inferred
        }
    } else {
        ContextHorizonEvidenceState::Unavailable
    };

    Ok(ContextHorizonPaneEvidence {
        pane_id: u64::try_from(pane_id.max(0)).unwrap_or(0),
        active_context_present,
        token_budget,
        tokens_consumed,
        pressure_tier,
        compaction_count: Some(rotation_count.max(active_depth)),
        last_rotated_at_ms,
        last_activity_at_ms,
        previous_utilization: None,
        recent_rate_limit_events: event_evidence.recent_rate_limit_events,
        recent_compaction_events,
        evidence_state,
    })
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ContextHorizonPaneRisk {
    pub pane_id: u64,
    pub risk_tier: ContextHorizonRiskTier,
    pub evidence_state: ContextHorizonEvidenceState,
    pub context_utilization: Option<f64>,
    pub tokens_consumed: Option<u64>,
    pub token_budget: Option<u64>,
    pub rotation_depth: u32,
    pub ms_since_last_rotation: Option<u64>,
    pub compaction_pressure: ContextHorizonRiskTier,
    pub rate_limit_risk: ContextHorizonRiskTier,
    pub handoff_readiness: ContextHorizonHandoffReadiness,
    pub reason_codes: Vec<String>,
    pub citation_ids: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextHorizonFleetSummary {
    pub total_panes: usize,
    pub highest_risk_tier: ContextHorizonRiskTier,
    pub panes_at_red_or_black: usize,
    pub top_operator_move: String,
    pub evidence_state: ContextHorizonEvidenceState,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextHorizonRecommendation {
    pub recommendation_id: String,
    pub scope: ContextHorizonScope,
    pub pane_id: Option<u64>,
    pub action_kind: ContextHorizonActionKind,
    pub mutation_allowed: bool,
    pub policy_state: ContextHorizonPolicyState,
    pub operator_summary: String,
    pub suggested_command: Option<String>,
    pub reason_codes: Vec<String>,
    pub citation_ids: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ContextHorizonAdvisorRecord {
    pub recommendation_id: String,
    pub scope: ContextHorizonScope,
    pub pane_id: Option<u64>,
    pub action_kind: ContextHorizonActionKind,
    pub reason_codes: Vec<String>,
    pub evidence_ids: Vec<String>,
    pub policy_state: ContextHorizonPolicyState,
    pub mutation_allowed: bool,
    pub confidence: f64,
    pub expected_operator_effect: String,
    pub suggested_command: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextHorizonRedactionPolicy {
    pub raw_transcript_allowed: bool,
    pub raw_prompt_allowed: bool,
    pub bounded_citations_only: bool,
    pub secret_redaction_required: bool,
}

impl Default for ContextHorizonRedactionPolicy {
    fn default() -> Self {
        Self {
            raw_transcript_allowed: false,
            raw_prompt_allowed: false,
            bounded_citations_only: true,
            secret_redaction_required: true,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ContextHorizonReport {
    pub schema_version: u16,
    pub contract_id: String,
    pub generated_at_ms: u64,
    pub source: String,
    pub evidence_state: ContextHorizonEvidenceState,
    pub horizon_window_ms: u64,
    pub fleet_summary: ContextHorizonFleetSummary,
    pub pane_risks: Vec<ContextHorizonPaneRisk>,
    pub recommendations: Vec<ContextHorizonRecommendation>,
    pub citations: Vec<ContextHorizonCitation>,
    pub unavailable_domains: Vec<ContextHorizonUnavailableDomain>,
    pub redaction_policy: ContextHorizonRedactionPolicy,
    pub artifact_paths: Vec<String>,
    pub raw_context_content_stored: bool,
}

#[must_use]
pub fn predict_context_horizon(input: &ContextHorizonInput) -> ContextHorizonReport {
    let mut unavailable_domains = input.unavailable_domains.clone();
    if input.panes.is_empty() && unavailable_domains.is_empty() {
        unavailable_domains.push(ContextHorizonUnavailableDomain::unavailable(
            "pane_contexts",
            "evidence.pane_contexts_unavailable",
        ));
    }

    let pane_risks = input
        .panes
        .iter()
        .map(|pane| score_pane(input.generated_at_ms, input.horizon_window_ms, pane))
        .collect::<Vec<_>>();
    let evidence_state = ContextHorizonEvidenceState::combine(
        pane_risks.iter().map(|risk| risk.evidence_state).chain(
            unavailable_domains
                .iter()
                .map(|domain| domain.evidence_state),
        ),
    );
    let fleet_summary = summarize_fleet(&pane_risks, evidence_state);
    let mut citations = pane_risks
        .iter()
        .map(|pane| ContextHorizonCitation {
            citation_id: pane_citation_id(pane.pane_id),
            source: "robot.context.status".to_string(),
            evidence_state: pane.evidence_state,
            redacted: true,
            summary: format!(
                "redacted context counters for pane {} with {:?} risk",
                pane.pane_id, pane.risk_tier
            ),
        })
        .collect::<Vec<_>>();
    citations.extend(
        unavailable_domains
            .iter()
            .map(|domain| ContextHorizonCitation {
                citation_id: domain_citation_id(&domain.domain),
                source: domain.domain.clone(),
                evidence_state: domain.evidence_state,
                redacted: true,
                summary: nonempty_reason_codes(&domain.reason_codes, "evidence.unavailable")
                    .join(", "),
            }),
    );

    let recommendations = build_recommendations(&pane_risks, &unavailable_domains);

    ContextHorizonReport {
        schema_version: CONTEXT_HORIZON_SCHEMA_VERSION,
        contract_id: CONTEXT_HORIZON_CONTRACT_ID.to_string(),
        generated_at_ms: input.generated_at_ms,
        source: "native_context_horizon_predictor".to_string(),
        evidence_state,
        horizon_window_ms: input.horizon_window_ms,
        fleet_summary,
        pane_risks,
        recommendations,
        citations,
        unavailable_domains,
        redaction_policy: ContextHorizonRedactionPolicy::default(),
        artifact_paths: input.artifact_paths.clone(),
        raw_context_content_stored: false,
    }
}

fn score_pane(
    generated_at_ms: u64,
    horizon_window_ms: u64,
    pane: &ContextHorizonPaneEvidence,
) -> ContextHorizonPaneRisk {
    let mut reason_codes = Vec::new();
    let mut states = vec![pane.evidence_state];

    let token_budget = sanitize_positive_u64(pane.token_budget, "token_budget", &mut reason_codes);
    let tokens_consumed =
        sanitize_nonnegative_u64(pane.tokens_consumed, "tokens_consumed", &mut reason_codes);
    if token_budget.is_none() || tokens_consumed.is_none() {
        states.push(ContextHorizonEvidenceState::Unavailable);
    }

    let context_utilization = match (tokens_consumed, token_budget) {
        (Some(consumed), Some(budget)) if budget > 0 => {
            let raw = consumed as f64 / budget as f64;
            if raw > 1.0 {
                reason_codes.push("context.utilization_clamped".to_string());
            }
            Some(raw.clamp(0.0, 1.0))
        }
        (Some(consumed), None) if consumed > 0 => {
            reason_codes.push("evidence.token_budget_missing_for_consumed_tokens".to_string());
            None
        }
        _ => None,
    };

    let mut risk_tier = context_utilization
        .map(ContextHorizonRiskTier::from_utilization)
        .unwrap_or(ContextHorizonRiskTier::Green);
    if token_budget.is_none() || tokens_consumed.is_none() {
        risk_tier = risk_tier.max(ContextHorizonRiskTier::Yellow);
    }
    let mut compaction_pressure = ContextHorizonRiskTier::Green;
    if let Some(pressure_tier) = pane
        .pressure_tier
        .as_deref()
        .and_then(ContextHorizonRiskTier::from_pressure_tier)
    {
        compaction_pressure = pressure_tier;
        if pressure_tier == ContextHorizonRiskTier::Unknown {
            reason_codes.push("evidence.pressure_tier_unavailable".to_string());
            states.push(ContextHorizonEvidenceState::Unavailable);
            risk_tier = risk_tier.max(ContextHorizonRiskTier::Yellow);
        } else {
            risk_tier = risk_tier.max(pressure_tier);
        }
    } else if pane.pressure_tier.is_some() {
        reason_codes.push("evidence.pressure_tier_unrecognized".to_string());
        states.push(ContextHorizonEvidenceState::Unavailable);
    }

    if !pane.active_context_present {
        reason_codes.push("context.active_context_missing".to_string());
        risk_tier = risk_tier.max(ContextHorizonRiskTier::Yellow);
        states.push(ContextHorizonEvidenceState::Inferred);
    }
    let rate_limit_risk = match pane.recent_rate_limit_events {
        0 => ContextHorizonRiskTier::Green,
        1 => ContextHorizonRiskTier::Yellow,
        _ => ContextHorizonRiskTier::Red,
    };
    if rate_limit_risk > ContextHorizonRiskTier::Green {
        reason_codes.push("provider.rate_limit_recent".to_string());
        risk_tier = risk_tier.max(rate_limit_risk);
    }

    let rotation_depth = sanitize_compaction_count(pane.compaction_count, &mut reason_codes);
    if pane.recent_compaction_events > 1 {
        reason_codes.push("context.compaction_repeated_recent".to_string());
        compaction_pressure = compaction_pressure.max(ContextHorizonRiskTier::Red);
    } else if pane.recent_compaction_events == 1 {
        reason_codes.push("context.compaction_recent".to_string());
        compaction_pressure = compaction_pressure.max(ContextHorizonRiskTier::Yellow);
    }
    risk_tier = risk_tier.max(compaction_pressure);

    let ms_since_last_rotation = age_ms(
        generated_at_ms,
        pane.last_rotated_at_ms,
        "last_rotated_at_ms",
        &mut reason_codes,
    );
    let last_activity_age_ms = age_ms(
        generated_at_ms,
        pane.last_activity_at_ms,
        "last_activity_at_ms",
        &mut reason_codes,
    );
    if last_activity_age_ms.is_some_and(|age| age > horizon_window_ms) {
        reason_codes.push("evidence.last_activity_stale".to_string());
        states.push(ContextHorizonEvidenceState::Stale);
        risk_tier = risk_tier.max(ContextHorizonRiskTier::Yellow);
    }
    if ms_since_last_rotation.is_some_and(|age| age > horizon_window_ms.saturating_mul(4)) {
        reason_codes.push("evidence.last_rotation_stale".to_string());
        states.push(ContextHorizonEvidenceState::Stale);
        risk_tier = risk_tier.max(ContextHorizonRiskTier::Yellow);
    }

    let previous_utilization = pane
        .previous_utilization
        .unwrap_or_else(|| context_utilization.unwrap_or(0.0))
        .clamp(0.0, 1.0);
    let utilization_trend = context_utilization
        .map(|utilization| (utilization - previous_utilization).clamp(-1.0, 1.0))
        .unwrap_or(0.0);
    if utilization_trend >= 0.15 {
        reason_codes.push("context.utilization_rising".to_string());
        risk_tier = risk_tier.max(ContextHorizonRiskTier::Yellow);
    }

    let evidence_state = ContextHorizonEvidenceState::combine(states);
    let handoff_readiness = if row_has_unavailable_or_stale_evidence(&reason_codes, evidence_state)
    {
        ContextHorizonHandoffReadiness::Blocked
    } else if risk_tier == ContextHorizonRiskTier::Black {
        ContextHorizonHandoffReadiness::Ready
    } else if risk_tier == ContextHorizonRiskTier::Red
        || compaction_pressure >= ContextHorizonRiskTier::Red
    {
        ContextHorizonHandoffReadiness::Prepare
    } else {
        ContextHorizonHandoffReadiness::NotNeeded
    };
    if reason_codes.is_empty() {
        reason_codes.push("risk.normal".to_string());
    }

    ContextHorizonPaneRisk {
        pane_id: pane.pane_id,
        risk_tier,
        evidence_state,
        context_utilization,
        tokens_consumed,
        token_budget,
        rotation_depth,
        ms_since_last_rotation,
        compaction_pressure,
        rate_limit_risk,
        handoff_readiness,
        reason_codes,
        citation_ids: vec![pane_citation_id(pane.pane_id)],
    }
}

fn sanitize_positive_u64(
    value: Option<i64>,
    field: &str,
    reasons: &mut Vec<String>,
) -> Option<u64> {
    match value {
        Some(value) if value > 0 => u64::try_from(value).ok(),
        Some(_) => {
            reasons.push(format!("evidence.{field}_invalid"));
            None
        }
        None => {
            reasons.push(format!("evidence.{field}_unavailable"));
            None
        }
    }
}

fn sanitize_nonnegative_u64(
    value: Option<i64>,
    field: &str,
    reasons: &mut Vec<String>,
) -> Option<u64> {
    match value {
        Some(value) if value >= 0 => u64::try_from(value).ok(),
        Some(_) => {
            reasons.push(format!("evidence.{field}_invalid"));
            None
        }
        None => {
            reasons.push(format!("evidence.{field}_unavailable"));
            None
        }
    }
}

fn sanitize_compaction_count(value: Option<i64>, reasons: &mut Vec<String>) -> u32 {
    match value {
        Some(value) if value >= 0 => u32::try_from(value).unwrap_or(u32::MAX),
        Some(_) => {
            reasons.push("evidence.compaction_count_invalid".to_string());
            0
        }
        None => {
            reasons.push("evidence.compaction_count_unavailable".to_string());
            0
        }
    }
}

fn age_ms(
    generated_at_ms: u64,
    timestamp_ms: Option<i64>,
    field: &str,
    reasons: &mut Vec<String>,
) -> Option<u64> {
    match timestamp_ms {
        Some(value) if value >= 0 => u64::try_from(value)
            .ok()
            .map(|timestamp| generated_at_ms.saturating_sub(timestamp)),
        Some(_) => {
            reasons.push(format!("evidence.{field}_invalid"));
            None
        }
        None => None,
    }
}

fn reason_penalty(reason_count: usize) -> f64 {
    (reason_count.min(6) as f64) * 0.05
}

fn summarize_fleet(
    pane_risks: &[ContextHorizonPaneRisk],
    evidence_state: ContextHorizonEvidenceState,
) -> ContextHorizonFleetSummary {
    let highest_risk_tier = pane_risks
        .iter()
        .map(|risk| risk.risk_tier)
        .max()
        .unwrap_or(ContextHorizonRiskTier::Unknown);
    let panes_at_red_or_black = pane_risks
        .iter()
        .filter(|risk| risk.risk_tier >= ContextHorizonRiskTier::Red)
        .count();

    ContextHorizonFleetSummary {
        total_panes: pane_risks.len(),
        highest_risk_tier,
        panes_at_red_or_black,
        top_operator_move: top_operator_move(pane_risks).to_string(),
        evidence_state,
    }
}

fn build_recommendations(
    pane_risks: &[ContextHorizonPaneRisk],
    unavailable_domains: &[ContextHorizonUnavailableDomain],
) -> Vec<ContextHorizonRecommendation> {
    build_advisor_records(pane_risks, unavailable_domains)
        .into_iter()
        .map(|record| ContextHorizonRecommendation {
            recommendation_id: record.recommendation_id,
            scope: record.scope,
            pane_id: record.pane_id,
            action_kind: record.action_kind,
            mutation_allowed: record.mutation_allowed,
            policy_state: record.policy_state,
            operator_summary: record.expected_operator_effect,
            suggested_command: record.suggested_command,
            reason_codes: record.reason_codes,
            citation_ids: record.evidence_ids,
        })
        .collect()
}

#[must_use]
pub fn advise_context_horizon(report: &ContextHorizonReport) -> Vec<ContextHorizonAdvisorRecord> {
    build_advisor_records(&report.pane_risks, &report.unavailable_domains)
}

fn build_advisor_records(
    pane_risks: &[ContextHorizonPaneRisk],
    unavailable_domains: &[ContextHorizonUnavailableDomain],
) -> Vec<ContextHorizonAdvisorRecord> {
    let mut records = Vec::new();

    for domain in unavailable_domains {
        let reason_codes = nonempty_reason_codes(&domain.reason_codes, "evidence.unavailable");
        records.push(ContextHorizonAdvisorRecord {
            recommendation_id: format!("rec:fleet:{}:none", sanitize_identifier(&domain.domain)),
            scope: ContextHorizonScope::Fleet,
            pane_id: None,
            action_kind: ContextHorizonActionKind::None,
            reason_codes,
            evidence_ids: vec![domain_citation_id(&domain.domain)],
            policy_state: ContextHorizonPolicyState::Unavailable,
            mutation_allowed: false,
            confidence: 0.05,
            expected_operator_effect: "surface unavailable evidence before taking fleet action"
                .to_string(),
            suggested_command: None,
        });
    }

    for risk in pane_risks {
        if risk.risk_tier == ContextHorizonRiskTier::Green
            && risk.evidence_state == ContextHorizonEvidenceState::Measured
            && risk.rate_limit_risk == ContextHorizonRiskTier::Green
            && risk.handoff_readiness == ContextHorizonHandoffReadiness::NotNeeded
        {
            continue;
        }

        let action_kind = advisor_action_kind(risk);
        let policy_state = advisor_policy_state(risk, action_kind);
        let suggested_command = advisor_suggested_command(risk, action_kind, policy_state);
        let mut reason_codes = risk.reason_codes.clone();
        reason_codes.push(format!("advisor.{}", action_kind.id_fragment()));
        reason_codes.sort();
        reason_codes.dedup();

        records.push(ContextHorizonAdvisorRecord {
            recommendation_id: format!("rec:pane:{}:{}", risk.pane_id, action_kind.id_fragment()),
            scope: ContextHorizonScope::Pane,
            pane_id: Some(risk.pane_id),
            action_kind,
            reason_codes,
            evidence_ids: risk.citation_ids.clone(),
            policy_state,
            mutation_allowed: false,
            confidence: advisor_confidence(risk),
            expected_operator_effect: expected_operator_effect(action_kind).to_string(),
            suggested_command,
        });
    }

    if pane_risks
        .iter()
        .any(|risk| risk.risk_tier == ContextHorizonRiskTier::Black)
    {
        let evidence_ids = pane_risks
            .iter()
            .filter(|risk| risk.risk_tier == ContextHorizonRiskTier::Black)
            .flat_map(|risk| risk.citation_ids.iter().cloned())
            .collect::<Vec<_>>();
        records.push(ContextHorizonAdvisorRecord {
            recommendation_id: "rec:fleet:collect_incident_bundle".to_string(),
            scope: ContextHorizonScope::Fleet,
            pane_id: None,
            action_kind: ContextHorizonActionKind::CollectIncidentBundle,
            reason_codes: vec![
                "advisor.collect_incident_bundle".to_string(),
                "risk.black_pane_present".to_string(),
            ],
            evidence_ids: nonempty_reason_codes(&evidence_ids, "fleet:black-risk"),
            policy_state: ContextHorizonPolicyState::AllowedDryRun,
            mutation_allowed: false,
            confidence: 0.75,
            expected_operator_effect:
                "prepare incident evidence planning without collecting or mutating artifacts"
                    .to_string(),
            suggested_command: None,
        });
    }

    records
}

fn top_operator_move(pane_risks: &[ContextHorizonPaneRisk]) -> &'static str {
    if pane_risks
        .iter()
        .any(|risk| risk.risk_tier == ContextHorizonRiskTier::Black)
    {
        ContextHorizonActionKind::PauseAssignment.id_fragment()
    } else if pane_risks
        .iter()
        .any(|risk| risk.rate_limit_risk > ContextHorizonRiskTier::Green)
    {
        ContextHorizonActionKind::ReduceFanout.id_fragment()
    } else if pane_risks
        .iter()
        .any(|risk| risk.risk_tier == ContextHorizonRiskTier::Red)
    {
        ContextHorizonActionKind::PrepareHandoff.id_fragment()
    } else if pane_risks
        .iter()
        .any(|risk| row_has_unavailable_or_stale_evidence(&risk.reason_codes, risk.evidence_state))
    {
        ContextHorizonActionKind::InspectPrompt.id_fragment()
    } else if pane_risks
        .iter()
        .any(|risk| risk.risk_tier == ContextHorizonRiskTier::Yellow)
    {
        ContextHorizonActionKind::RotateContext.id_fragment()
    } else {
        ContextHorizonActionKind::None.id_fragment()
    }
}

fn advisor_action_kind(risk: &ContextHorizonPaneRisk) -> ContextHorizonActionKind {
    if risk.risk_tier == ContextHorizonRiskTier::Black {
        ContextHorizonActionKind::PauseAssignment
    } else if risk.rate_limit_risk > ContextHorizonRiskTier::Green {
        ContextHorizonActionKind::ReduceFanout
    } else if risk.risk_tier == ContextHorizonRiskTier::Red
        || matches!(
            risk.handoff_readiness,
            ContextHorizonHandoffReadiness::Prepare | ContextHorizonHandoffReadiness::Ready
        )
    {
        ContextHorizonActionKind::PrepareHandoff
    } else if row_has_unavailable_or_stale_evidence(&risk.reason_codes, risk.evidence_state) {
        ContextHorizonActionKind::InspectPrompt
    } else if risk.risk_tier == ContextHorizonRiskTier::Yellow {
        ContextHorizonActionKind::RotateContext
    } else {
        ContextHorizonActionKind::None
    }
}

fn advisor_policy_state(
    risk: &ContextHorizonPaneRisk,
    action_kind: ContextHorizonActionKind,
) -> ContextHorizonPolicyState {
    if row_has_unavailable_or_stale_evidence(&risk.reason_codes, risk.evidence_state) {
        return ContextHorizonPolicyState::Unavailable;
    }
    match action_kind {
        ContextHorizonActionKind::RotateContext => ContextHorizonPolicyState::RequiresApproval,
        ContextHorizonActionKind::None => ContextHorizonPolicyState::Blocked,
        _ => ContextHorizonPolicyState::AllowedDryRun,
    }
}

fn advisor_suggested_command(
    risk: &ContextHorizonPaneRisk,
    action_kind: ContextHorizonActionKind,
    policy_state: ContextHorizonPolicyState,
) -> Option<String> {
    if policy_state != ContextHorizonPolicyState::AllowedDryRun {
        return None;
    }
    let command = match action_kind {
        ContextHorizonActionKind::PauseAssignment | ContextHorizonActionKind::PrepareHandoff => {
            format!(
                "ft robot --format json events --pane {} --limit 20",
                risk.pane_id
            )
        }
        ContextHorizonActionKind::ReduceFanout => format!(
            "ft robot --format json events --pane {} --rule-id rate_limit --limit 20",
            risk.pane_id
        ),
        _ => return None,
    };
    is_safe_suggested_command(&command).then_some(command)
}

fn advisor_confidence(risk: &ContextHorizonPaneRisk) -> f64 {
    (1.0 - risk.evidence_state.penalty() - reason_penalty(risk.reason_codes.len())).clamp(0.05, 1.0)
}

fn expected_operator_effect(action_kind: ContextHorizonActionKind) -> &'static str {
    match action_kind {
        ContextHorizonActionKind::RotateContext => {
            "prepare an approval-gated context rotation without executing it"
        }
        ContextHorizonActionKind::PrepareHandoff => {
            "prepare handoff material before the pane crosses the context horizon"
        }
        ContextHorizonActionKind::ReduceFanout => {
            "reduce new work pressure while rate-limit evidence is reviewed"
        }
        ContextHorizonActionKind::PauseAssignment => {
            "pause new assignment planning for the pane until pressure drops"
        }
        ContextHorizonActionKind::InspectPrompt => {
            "inspect evidence freshness before trusting prompt or context state"
        }
        ContextHorizonActionKind::CollectIncidentBundle => {
            "prepare incident-bundle planning without collecting artifacts"
        }
        ContextHorizonActionKind::None => "take no mutating action",
    }
}

fn row_has_unavailable_or_stale_evidence(
    reason_codes: &[String],
    evidence_state: ContextHorizonEvidenceState,
) -> bool {
    matches!(
        evidence_state,
        ContextHorizonEvidenceState::Stale | ContextHorizonEvidenceState::Unavailable
    ) || reason_codes.iter().any(|reason| {
        reason.contains("unavailable")
            || reason.contains("stale")
            || reason.contains("invalid")
            || reason.contains("missing")
    })
}

fn is_safe_suggested_command(command: &str) -> bool {
    let lower = command.to_ascii_lowercase();
    let forbidden = [
        "am service",
        "am doctor",
        "mcp-agent-mail",
        "ft robot send",
        "robot send",
        "context rotate",
        "rotate context",
        "git reset",
        "git clean",
        "worktree",
        "rm ",
        "kill",
        "pkill",
    ];
    if forbidden.iter().any(|needle| lower.contains(needle)) {
        return false;
    }
    lower.starts_with("ft robot --format json events")
        || lower.starts_with("ft robot --format toon events")
        || lower.starts_with("ft robot --format json state")
        || lower.starts_with("ft robot --format toon state")
}

fn pane_citation_id(pane_id: u64) -> String {
    format!("pane:{pane_id}:context_status")
}

fn domain_citation_id(domain: &str) -> String {
    format!("domain:{}:availability", sanitize_identifier(domain))
}

fn sanitize_identifier(value: &str) -> String {
    let sanitized = value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-' | '.') {
                ch
            } else {
                '_'
            }
        })
        .collect::<String>();
    if sanitized.is_empty() {
        "unknown".to_string()
    } else {
        sanitized
    }
}

fn nonempty_reason_codes(values: &[String], fallback: &str) -> Vec<String> {
    if values.is_empty() {
        vec![fallback.to_string()]
    } else {
        values.to_vec()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pane(pane_id: u64, consumed: i64, budget: i64) -> ContextHorizonPaneEvidence {
        ContextHorizonPaneEvidence {
            pane_id,
            active_context_present: true,
            token_budget: Some(budget),
            tokens_consumed: Some(consumed),
            pressure_tier: Some("green".to_string()),
            compaction_count: Some(0),
            last_rotated_at_ms: Some(1_000),
            last_activity_at_ms: Some(9_000),
            previous_utilization: None,
            recent_rate_limit_events: 0,
            recent_compaction_events: 0,
            evidence_state: ContextHorizonEvidenceState::Measured,
        }
    }

    #[test]
    fn context_horizon_predictor_classifies_thresholds_deterministically() {
        let input = ContextHorizonInput {
            generated_at_ms: 10_000,
            horizon_window_ms: 10_000,
            panes: vec![
                pane(1, 100, 1_000),
                pane(2, 750, 1_000),
                pane(3, 900, 1_000),
                pane(4, 990, 1_000),
            ],
            unavailable_domains: Vec::new(),
            artifact_paths: Vec::new(),
        };

        let report = predict_context_horizon(&input);
        let tiers = report
            .pane_risks
            .iter()
            .map(|risk| risk.risk_tier)
            .collect::<Vec<_>>();

        assert_eq!(
            tiers,
            vec![
                ContextHorizonRiskTier::Green,
                ContextHorizonRiskTier::Yellow,
                ContextHorizonRiskTier::Red,
                ContextHorizonRiskTier::Black,
            ]
        );
        assert_eq!(report.fleet_summary.panes_at_red_or_black, 2);
        assert_eq!(
            report.fleet_summary.highest_risk_tier,
            ContextHorizonRiskTier::Black
        );
        assert_eq!(report.fleet_summary.top_operator_move, "pause_assignment");
        assert_eq!(report.evidence_state, ContextHorizonEvidenceState::Measured);
        assert!(!report.raw_context_content_stored);
    }

    #[test]
    fn context_horizon_empty_storage_fails_closed_as_unavailable() {
        let report = predict_context_horizon(&ContextHorizonInput::new(42));

        assert_eq!(
            report.evidence_state,
            ContextHorizonEvidenceState::Unavailable
        );
        assert_eq!(report.fleet_summary.total_panes, 0);
        assert_eq!(
            report.fleet_summary.highest_risk_tier,
            ContextHorizonRiskTier::Unknown
        );
        assert_eq!(report.unavailable_domains.len(), 1);
        assert_eq!(report.unavailable_domains[0].domain, "pane_contexts");
        assert_eq!(
            report.unavailable_domains[0].reason_codes,
            vec!["evidence.pane_contexts_unavailable".to_string()]
        );
        let recommendation = report
            .recommendations
            .first()
            .expect("unavailable evidence produces a fail-closed recommendation");
        assert_eq!(recommendation.scope, ContextHorizonScope::Fleet);
        assert_eq!(recommendation.action_kind, ContextHorizonActionKind::None);
        assert_eq!(
            recommendation.policy_state,
            ContextHorizonPolicyState::Unavailable
        );
        assert!(!recommendation.mutation_allowed);
        assert_eq!(recommendation.suggested_command, None);
    }

    #[test]
    fn context_horizon_malformed_counters_do_not_panic() {
        let input = ContextHorizonInput {
            generated_at_ms: 20_000,
            horizon_window_ms: 1_000,
            panes: vec![ContextHorizonPaneEvidence {
                pane_id: 7,
                active_context_present: false,
                token_budget: Some(-1),
                tokens_consumed: Some(-5),
                pressure_tier: Some("not-a-tier".to_string()),
                compaction_count: Some(-2),
                last_rotated_at_ms: Some(-1),
                last_activity_at_ms: None,
                previous_utilization: Some(2.0),
                recent_rate_limit_events: 1,
                recent_compaction_events: 0,
                evidence_state: ContextHorizonEvidenceState::Measured,
            }],
            unavailable_domains: Vec::new(),
            artifact_paths: Vec::new(),
        };

        let report = predict_context_horizon(&input);
        let risk = &report.pane_risks[0];

        assert_eq!(risk.evidence_state, ContextHorizonEvidenceState::Mixed);
        assert!(risk.risk_tier >= ContextHorizonRiskTier::Yellow);
        assert_eq!(risk.token_budget, None);
        assert_eq!(risk.tokens_consumed, None);
        assert_eq!(risk.rotation_depth, 0);
        assert!(
            risk.reason_codes
                .iter()
                .any(|reason| reason.contains("token_budget"))
        );
        assert_eq!(
            risk.handoff_readiness,
            ContextHorizonHandoffReadiness::Blocked
        );
    }

    #[test]
    fn context_horizon_unknown_pressure_tier_is_not_green() {
        let mut unknown_pressure = pane(8, 100, 1_000);
        unknown_pressure.pressure_tier = Some("unknown".to_string());
        let report = predict_context_horizon(&ContextHorizonInput {
            generated_at_ms: 10_000,
            horizon_window_ms: 10_000,
            panes: vec![unknown_pressure],
            unavailable_domains: Vec::new(),
            artifact_paths: Vec::new(),
        });
        let risk = &report.pane_risks[0];

        assert_eq!(risk.compaction_pressure, ContextHorizonRiskTier::Unknown);
        assert!(risk.risk_tier >= ContextHorizonRiskTier::Yellow);
        assert_eq!(
            risk.handoff_readiness,
            ContextHorizonHandoffReadiness::Blocked
        );
        assert_eq!(
            advise_context_horizon(&report)[0].policy_state,
            ContextHorizonPolicyState::Unavailable
        );
    }

    #[test]
    fn context_horizon_stale_activity_marks_stale_evidence() {
        let mut stale = pane(9, 300, 1_000);
        stale.last_activity_at_ms = Some(1_000);
        let input = ContextHorizonInput {
            generated_at_ms: 60_000,
            horizon_window_ms: 10_000,
            panes: vec![stale],
            unavailable_domains: Vec::new(),
            artifact_paths: Vec::new(),
        };

        let report = predict_context_horizon(&input);
        let risk = &report.pane_risks[0];

        assert_eq!(risk.evidence_state, ContextHorizonEvidenceState::Mixed);
        assert_eq!(report.evidence_state, ContextHorizonEvidenceState::Mixed);
        assert!(
            risk.reason_codes
                .iter()
                .any(|reason| reason.contains("stale"))
        );
    }

    #[test]
    fn context_horizon_advisor_noops_healthy_fleet() {
        let report = predict_context_horizon(&ContextHorizonInput {
            generated_at_ms: 10_000,
            horizon_window_ms: 10_000,
            panes: vec![pane(1, 100, 1_000)],
            unavailable_domains: Vec::new(),
            artifact_paths: Vec::new(),
        });

        assert!(report.recommendations.is_empty());
        assert!(advise_context_horizon(&report).is_empty());
        assert_eq!(report.fleet_summary.top_operator_move, "none");
    }

    #[test]
    fn context_horizon_advisor_pauses_black_pressure_without_mutation() {
        let report = predict_context_horizon(&ContextHorizonInput {
            generated_at_ms: 10_000,
            horizon_window_ms: 10_000,
            panes: vec![pane(44, 990, 1_000)],
            unavailable_domains: Vec::new(),
            artifact_paths: Vec::new(),
        });

        let records = advise_context_horizon(&report);
        let pause = records
            .iter()
            .find(|record| {
                record.pane_id == Some(44)
                    && record.action_kind == ContextHorizonActionKind::PauseAssignment
            })
            .expect("black context pressure produces pause-assignment advice");

        assert_eq!(pause.scope, ContextHorizonScope::Pane);
        assert_eq!(pause.policy_state, ContextHorizonPolicyState::AllowedDryRun);
        assert!(!pause.mutation_allowed);
        assert!(
            pause
                .reason_codes
                .iter()
                .any(|code| code == "advisor.pause_assignment")
        );
        assert_eq!(
            pause.evidence_ids,
            vec!["pane:44:context_status".to_string()]
        );
        assert!(
            pause
                .suggested_command
                .as_deref()
                .is_some_and(is_safe_suggested_command)
        );
        assert!(records.iter().any(|record| {
            record.scope == ContextHorizonScope::Fleet
                && record.action_kind == ContextHorizonActionKind::CollectIncidentBundle
                && !record.mutation_allowed
                && record.suggested_command.is_none()
        }));
    }

    #[test]
    fn context_horizon_advisor_blocks_stale_prompt_inspection_without_command() {
        let mut stale = pane(9, 300, 1_000);
        stale.last_activity_at_ms = Some(1_000);
        let report = predict_context_horizon(&ContextHorizonInput {
            generated_at_ms: 60_000,
            horizon_window_ms: 10_000,
            panes: vec![stale],
            unavailable_domains: Vec::new(),
            artifact_paths: Vec::new(),
        });

        let inspect = advise_context_horizon(&report)
            .into_iter()
            .find(|record| record.action_kind == ContextHorizonActionKind::InspectPrompt)
            .expect("stale context evidence produces prompt-inspection advice");

        assert_eq!(inspect.policy_state, ContextHorizonPolicyState::Unavailable);
        assert!(!inspect.mutation_allowed);
        assert_eq!(inspect.suggested_command, None);
        assert!(
            inspect
                .reason_codes
                .iter()
                .any(|reason| reason == "evidence.last_activity_stale")
        );
    }

    #[test]
    fn context_horizon_advisor_reduces_fanout_on_rate_limit_risk() {
        let mut rate_limited = pane(12, 100, 1_000);
        rate_limited.recent_rate_limit_events = 1;
        let report = predict_context_horizon(&ContextHorizonInput {
            generated_at_ms: 10_000,
            horizon_window_ms: 10_000,
            panes: vec![rate_limited],
            unavailable_domains: Vec::new(),
            artifact_paths: Vec::new(),
        });

        let fanout = advise_context_horizon(&report)
            .into_iter()
            .find(|record| record.action_kind == ContextHorizonActionKind::ReduceFanout)
            .expect("rate-limit evidence produces fanout advice");

        assert_eq!(fanout.pane_id, Some(12));
        assert_eq!(
            fanout.policy_state,
            ContextHorizonPolicyState::AllowedDryRun
        );
        assert!(!fanout.mutation_allowed);
        assert!(
            fanout
                .reason_codes
                .iter()
                .any(|reason| reason == "provider.rate_limit_recent")
        );
        assert!(
            fanout
                .suggested_command
                .as_deref()
                .is_some_and(is_safe_suggested_command)
        );
    }

    #[test]
    fn context_horizon_advisor_prepares_handoff_for_red_pressure() {
        let report = predict_context_horizon(&ContextHorizonInput {
            generated_at_ms: 10_000,
            horizon_window_ms: 10_000,
            panes: vec![pane(31, 900, 1_000)],
            unavailable_domains: Vec::new(),
            artifact_paths: Vec::new(),
        });

        let handoff = advise_context_horizon(&report)
            .into_iter()
            .find(|record| record.action_kind == ContextHorizonActionKind::PrepareHandoff)
            .expect("red context pressure produces handoff advice");

        assert_eq!(handoff.pane_id, Some(31));
        assert_eq!(
            handoff.policy_state,
            ContextHorizonPolicyState::AllowedDryRun
        );
        assert!(handoff.confidence > 0.0);
        assert!(!handoff.expected_operator_effect.is_empty());
    }

    #[test]
    fn context_horizon_advisor_never_emits_mutating_or_agent_mail_repair_commands() {
        let mut black = pane(77, 990, 1_000);
        black.recent_rate_limit_events = 2;
        let report = predict_context_horizon(&ContextHorizonInput {
            generated_at_ms: 10_000,
            horizon_window_ms: 10_000,
            panes: vec![black],
            unavailable_domains: vec![ContextHorizonUnavailableDomain::unavailable(
                "agent_mail",
                "evidence.agent_mail_unavailable",
            )],
            artifact_paths: Vec::new(),
        });

        for recommendation in &report.recommendations {
            assert!(!recommendation.mutation_allowed);
            if let Some(command) = &recommendation.suggested_command {
                assert!(is_safe_suggested_command(command), "{command}");
                assert!(!command.contains("am service"));
                assert!(!command.contains("am doctor"));
                assert!(!command.contains("ft robot send"));
                assert!(!command.contains("kill"));
            }
        }
        for record in advise_context_horizon(&report) {
            assert!(!record.mutation_allowed);
            if let Some(command) = &record.suggested_command {
                assert!(is_safe_suggested_command(command), "{command}");
            }
        }
    }

    #[test]
    fn context_horizon_serialization_contains_no_raw_prompt_or_transcript_fields() {
        let report = predict_context_horizon(&ContextHorizonInput {
            generated_at_ms: 10_000,
            horizon_window_ms: 10_000,
            panes: vec![pane(1, 200, 1_000)],
            unavailable_domains: Vec::new(),
            artifact_paths: Vec::new(),
        });
        let value = serde_json::to_value(&report).expect("report serializes");
        let mut disallowed = Vec::new();
        collect_disallowed_keys(&value, &mut disallowed);

        assert!(disallowed.is_empty(), "{disallowed:?}");
        assert_eq!(
            value["raw_context_content_stored"],
            serde_json::json!(false)
        );
        assert_eq!(
            value["redaction_policy"]["raw_prompt_allowed"],
            serde_json::json!(false)
        );
        assert_eq!(
            value["redaction_policy"]["raw_transcript_allowed"],
            serde_json::json!(false)
        );
    }

    #[test]
    fn context_horizon_sqlite_missing_db_returns_contract_unavailable() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("missing.sqlite3");

        let report = predict_context_horizon_from_sqlite(
            &db_path,
            None,
            1_700_000_000_000,
            60_000,
            "test.context_horizon",
        )
        .expect("missing db should produce unavailable report");

        assert_eq!(report.contract_id, CONTEXT_HORIZON_CONTRACT_ID);
        assert_eq!(report.source, "test.context_horizon");
        assert_eq!(
            report.evidence_state,
            ContextHorizonEvidenceState::Unavailable
        );
        assert_eq!(report.raw_context_content_stored, false);
        assert_eq!(report.unavailable_domains.len(), 1);
        assert_eq!(
            report.unavailable_domains[0].reason_codes,
            vec!["evidence.context_database_missing"]
        );
    }

    #[test]
    fn context_horizon_sqlite_reads_context_registry_and_rate_limit_events() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("context.sqlite3");
        let conn = rusqlite::Connection::open(&db_path).expect("open sqlite");
        conn.execute_batch(
            r"
            CREATE TABLE pane_contexts (
                context_id TEXT PRIMARY KEY NOT NULL,
                pane_id INTEGER NOT NULL,
                state TEXT NOT NULL,
                depth INTEGER NOT NULL,
                created_at_ms INTEGER NOT NULL,
                archived_at_ms INTEGER,
                token_budget INTEGER NOT NULL,
                tokens_consumed INTEGER NOT NULL,
                pressure_tier TEXT NOT NULL,
                source TEXT NOT NULL
            );
            CREATE TABLE context_rotations (
                rotation_id TEXT PRIMARY KEY NOT NULL,
                pane_id INTEGER NOT NULL,
                previous_context_id TEXT,
                new_context_id TEXT NOT NULL,
                strategy TEXT NOT NULL,
                reason TEXT,
                caller_idempotency_key TEXT,
                rotated_at_ms INTEGER NOT NULL,
                tokens_before INTEGER NOT NULL,
                tokens_after INTEGER NOT NULL,
                tokens_freed INTEGER NOT NULL,
                created_at_ms INTEGER NOT NULL
            );
            CREATE TABLE events (
                id INTEGER PRIMARY KEY,
                pane_id INTEGER NOT NULL,
                rule_id TEXT NOT NULL,
                event_type TEXT NOT NULL,
                detected_at INTEGER NOT NULL
            );
            INSERT INTO pane_contexts
                (context_id, pane_id, state, depth, created_at_ms, token_budget,
                 tokens_consumed, pressure_tier, source)
            VALUES
                ('ctx-1', 7, 'active', 2, 1699999990000, 1000, 910, 'yellow',
                 'test');
            INSERT INTO context_rotations
                (rotation_id, pane_id, previous_context_id, new_context_id, strategy,
                 rotated_at_ms, tokens_before, tokens_after, tokens_freed, created_at_ms)
            VALUES
                ('rot-1', 7, NULL, 'ctx-1', 'agent_default', 1699999995000, 900,
                 100, 800, 1699999995000);
            INSERT INTO events
                (pane_id, rule_id, event_type, detected_at)
            VALUES
                (7, 'codex.usage.limit', 'rate_limit', 1699999999000);
            ",
        )
        .expect("seed context horizon sqlite");
        drop(conn);

        let report = predict_context_horizon_from_sqlite(
            &db_path,
            Some(7),
            1_700_000_000_000,
            60_000,
            "robot.context.horizon",
        )
        .expect("sqlite horizon report");

        assert_eq!(report.source, "robot.context.horizon");
        assert_eq!(report.pane_risks.len(), 1);
        let risk = &report.pane_risks[0];
        assert_eq!(risk.pane_id, 7);
        assert!(risk.risk_tier >= ContextHorizonRiskTier::Red);
        assert_eq!(risk.rate_limit_risk, ContextHorizonRiskTier::Yellow);
        assert_eq!(
            risk.handoff_readiness,
            ContextHorizonHandoffReadiness::Prepare
        );
        assert!(
            risk.reason_codes
                .iter()
                .any(|reason| reason == "provider.rate_limit_recent")
        );
        assert_eq!(report.raw_context_content_stored, false);
        assert!(
            report
                .recommendations
                .iter()
                .all(|recommendation| !recommendation.mutation_allowed)
        );
    }

    fn collect_disallowed_keys(value: &serde_json::Value, out: &mut Vec<String>) {
        match value {
            serde_json::Value::Object(map) => {
                for (key, value) in map {
                    if matches!(
                        key.as_str(),
                        "raw_prompt"
                            | "raw_transcript"
                            | "prompt_body"
                            | "pane_text"
                            | "raw_text"
                            | "transcript"
                    ) {
                        out.push(key.clone());
                    }
                    collect_disallowed_keys(value, out);
                }
            }
            serde_json::Value::Array(values) => {
                for value in values {
                    collect_disallowed_keys(value, out);
                }
            }
            _ => {}
        }
    }
}
