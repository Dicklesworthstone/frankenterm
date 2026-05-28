//! Read-only attention-router source snapshot adapters.
//!
//! This module intentionally does not execute subprocesses, inspect live panes,
//! call coordination services, run proof commands, or mutate project state. It
//! normalizes bounded, already-redacted caller observations into the source
//! snapshot substrate for the `ft.attention_router.v1` contract.

use serde::{Deserialize, Serialize};

pub const ATTENTION_ROUTER_CONTRACT_ID: &str = "ft.attention_router.v1";
pub const ATTENTION_ROUTER_SOURCE_SCHEMA_VERSION: u16 = 1;
pub const ATTENTION_ROUTER_SUMMARY_MAX_CHARS: usize = 512;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttentionRouterSourceKind {
    Beads,
    AgentMail,
    Git,
    Rch,
    PaneState,
    OperatingEnvelope,
    Manual,
    Fixture,
}

impl AttentionRouterSourceKind {
    fn slug(self) -> &'static str {
        match self {
            Self::Beads => "beads",
            Self::AgentMail => "agent_mail",
            Self::Git => "git",
            Self::Rch => "rch",
            Self::PaneState => "pane_state",
            Self::OperatingEnvelope => "operating_envelope",
            Self::Manual => "manual",
            Self::Fixture => "fixture",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttentionRouterSourceHealth {
    Available,
    Degraded,
    Unavailable,
    NotConfigured,
}

impl AttentionRouterSourceHealth {
    fn reason_code(self, source_kind: AttentionRouterSourceKind) -> String {
        let state = match self {
            Self::Available => "available",
            Self::Degraded => "degraded",
            Self::Unavailable => "unavailable",
            Self::NotConfigured => "not_configured",
        };
        format!("source.{}.{}", source_kind.slug(), state)
    }

    fn is_attention_issue(self) -> bool {
        self != Self::Available
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttentionRouterRedactionPosture {
    Redacted,
    SummaryOnly,
    NoSensitiveContent,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttentionRouterSourceFactKind {
    BeadsReady,
    BeadsBlocked,
    BeadsInProgress,
    BeadsPriority,
    BeadsAssignee,
    BeadsDependencies,
    BeadsAge,
    BeadsRecentComments,
    BvRecommendationConflict,
    AgentMailRegisteredAgents,
    AgentMailRecentMessages,
    AgentMailAckRequired,
    AgentMailFileReservations,
    AgentMailFallbackState,
    GitBranchDivergence,
    GitDirtyPaths,
    GitStagedPaths,
    GitRecentCommits,
    GitClaimOverlap,
    RchInstalledStatus,
    RchQueueState,
    RchWorkerPressure,
    RchRemoteDryRun,
    RchProofStarvation,
    PaneAgentLiveness,
    PaneIdleSignal,
    PaneStuckSignal,
    PaneCodexPlaceholderCaveat,
    OperatingEnvelopeCapacity,
    OperatingEnvelopeSideEffectPolicy,
    OperatingEnvelopeProofPosture,
    SourceUnavailable,
    SourceNotConfigured,
    Manual,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttentionRouterSourceFact {
    pub fact: AttentionRouterSourceFactKind,
    pub summary: String,
    pub count: Option<u64>,
    pub bead_ids: Vec<String>,
    pub agent_names: Vec<String>,
    pub affected_paths: Vec<String>,
    pub reason_codes: Vec<String>,
}

impl AttentionRouterSourceFact {
    #[must_use]
    pub fn new(fact: AttentionRouterSourceFactKind, summary: impl Into<String>) -> Self {
        Self {
            fact,
            summary: bounded_string(summary, "source fact unavailable"),
            count: None,
            bead_ids: Vec::new(),
            agent_names: Vec::new(),
            affected_paths: Vec::new(),
            reason_codes: Vec::new(),
        }
    }

    #[must_use]
    pub fn count(mut self, count: u64) -> Self {
        self.count = Some(count);
        self
    }

    #[must_use]
    pub fn with_bead_id(mut self, bead_id: impl Into<String>) -> Self {
        push_unique(&mut self.bead_ids, bead_id);
        self
    }

    #[must_use]
    pub fn with_agent_name(mut self, agent_name: impl Into<String>) -> Self {
        push_unique(&mut self.agent_names, agent_name);
        self
    }

    #[must_use]
    pub fn with_affected_path(mut self, affected_path: impl Into<String>) -> Self {
        push_unique(&mut self.affected_paths, affected_path);
        self
    }

    #[must_use]
    pub fn with_reason_code(mut self, reason_code: impl Into<String>) -> Self {
        push_unique(&mut self.reason_codes, reason_code);
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttentionRouterSourceObservation {
    pub source_id: String,
    pub source_kind: AttentionRouterSourceKind,
    pub health: AttentionRouterSourceHealth,
    pub collected_at_ms: Option<u64>,
    pub freshness_ms: Option<u64>,
    pub command_or_api: String,
    pub live: bool,
    pub redaction_posture: AttentionRouterRedactionPosture,
    pub source_summary: String,
    pub reason_codes: Vec<String>,
    pub facts: Vec<AttentionRouterSourceFact>,
    pub items_seen: Option<u64>,
}

impl AttentionRouterSourceObservation {
    #[must_use]
    pub fn new(
        source_id: impl Into<String>,
        source_kind: AttentionRouterSourceKind,
        health: AttentionRouterSourceHealth,
        command_or_api: impl Into<String>,
        source_summary: impl Into<String>,
    ) -> Self {
        let mut reason_codes = Vec::new();
        push_unique(&mut reason_codes, health.reason_code(source_kind));
        Self {
            source_id: bounded_string(source_id, "source.unknown"),
            source_kind,
            health,
            collected_at_ms: None,
            freshness_ms: None,
            command_or_api: bounded_string(command_or_api, "collector.unavailable"),
            live: false,
            redaction_posture: AttentionRouterRedactionPosture::Redacted,
            source_summary: bounded_string(source_summary, "source summary unavailable"),
            reason_codes,
            facts: Vec::new(),
            items_seen: None,
        }
    }

    #[must_use]
    pub fn live(mut self, collected_at_ms: u64, freshness_ms: u64) -> Self {
        self.live = true;
        self.collected_at_ms = Some(collected_at_ms);
        self.freshness_ms = Some(freshness_ms);
        self
    }

    #[must_use]
    pub fn redaction_posture(mut self, posture: AttentionRouterRedactionPosture) -> Self {
        self.redaction_posture = posture;
        self
    }

    #[must_use]
    pub fn with_reason_code(mut self, reason_code: impl Into<String>) -> Self {
        push_unique(&mut self.reason_codes, reason_code);
        self
    }

    #[must_use]
    pub fn with_fact(mut self, fact: AttentionRouterSourceFact) -> Self {
        self.facts.push(fact);
        self
    }

    #[must_use]
    pub fn items_seen(mut self, items_seen: u64) -> Self {
        self.items_seen = Some(items_seen);
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttentionRouterSourceAdapterInput {
    pub generated_at_ms: u64,
    pub workspace: String,
    pub observations: Vec<AttentionRouterSourceObservation>,
}

impl AttentionRouterSourceAdapterInput {
    #[must_use]
    pub fn new(generated_at_ms: u64, workspace: impl Into<String>) -> Self {
        Self {
            generated_at_ms,
            workspace: bounded_string(workspace, "."),
            observations: Vec::new(),
        }
    }

    #[must_use]
    pub fn with_observation(mut self, observation: AttentionRouterSourceObservation) -> Self {
        self.observations.push(observation);
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttentionRouterSourceSnapshot {
    pub source_id: String,
    pub source_kind: AttentionRouterSourceKind,
    pub health: AttentionRouterSourceHealth,
    pub collected_at_ms: Option<u64>,
    pub freshness_ms: Option<u64>,
    pub command_or_api: String,
    pub live: bool,
    pub redaction_posture: AttentionRouterRedactionPosture,
    pub source_summary: String,
    pub redacted: bool,
    pub reason_codes: Vec<String>,
    pub facts: Vec<AttentionRouterSourceFact>,
    pub items_seen: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttentionRouterSourceHealthRecord {
    pub source_kind: AttentionRouterSourceKind,
    pub health: AttentionRouterSourceHealth,
    pub source_id: String,
    pub reason_codes: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttentionRouterSourceBundle {
    pub schema_version: u16,
    pub contract_id: String,
    pub generated_at_ms: u64,
    pub workspace: String,
    pub sources: Vec<AttentionRouterSourceSnapshot>,
    pub source_health: Vec<AttentionRouterSourceHealthRecord>,
    pub warnings: Vec<String>,
    pub raw_pane_content_stored: bool,
    pub raw_message_bodies_stored: bool,
    pub side_effects_executed: bool,
}

#[must_use]
pub fn build_attention_router_source_bundle(
    input: &AttentionRouterSourceAdapterInput,
) -> AttentionRouterSourceBundle {
    let mut observations = input.observations.clone();
    for source_kind in required_source_kinds() {
        if !observations
            .iter()
            .any(|observation| observation.source_kind == source_kind)
        {
            observations.push(missing_source_observation(
                source_kind,
                input.generated_at_ms,
            ));
        }
    }

    observations.sort_by_key(|observation| {
        (
            observation.source_kind,
            observation.source_id.clone(),
            observation.health,
        )
    });

    let sources = observations.iter().map(source_snapshot).collect::<Vec<_>>();
    let source_health = sources
        .iter()
        .filter(|source| source.health.is_attention_issue())
        .map(|source| AttentionRouterSourceHealthRecord {
            source_kind: source.source_kind,
            health: source.health,
            source_id: source.source_id.clone(),
            reason_codes: source.reason_codes.clone(),
        })
        .collect::<Vec<_>>();
    let warnings = source_health
        .iter()
        .map(|record| {
            format!(
                "{} source health is {:?}",
                record.source_kind.slug(),
                record.health
            )
            .to_ascii_lowercase()
        })
        .collect::<Vec<_>>();

    AttentionRouterSourceBundle {
        schema_version: ATTENTION_ROUTER_SOURCE_SCHEMA_VERSION,
        contract_id: ATTENTION_ROUTER_CONTRACT_ID.to_string(),
        generated_at_ms: input.generated_at_ms,
        workspace: input.workspace.clone(),
        sources,
        source_health,
        warnings,
        raw_pane_content_stored: false,
        raw_message_bodies_stored: false,
        side_effects_executed: false,
    }
}

fn source_snapshot(
    observation: &AttentionRouterSourceObservation,
) -> AttentionRouterSourceSnapshot {
    let mut reason_codes = observation.reason_codes.clone();
    push_unique(
        &mut reason_codes,
        observation.health.reason_code(observation.source_kind),
    );
    for fact in &observation.facts {
        for reason_code in &fact.reason_codes {
            push_unique(&mut reason_codes, reason_code.clone());
        }
    }

    AttentionRouterSourceSnapshot {
        source_id: observation.source_id.clone(),
        source_kind: observation.source_kind,
        health: observation.health,
        collected_at_ms: observation.collected_at_ms,
        freshness_ms: observation.freshness_ms,
        command_or_api: observation.command_or_api.clone(),
        live: observation.live,
        redaction_posture: observation.redaction_posture,
        source_summary: observation.source_summary.clone(),
        redacted: true,
        reason_codes,
        facts: observation.facts.clone(),
        items_seen: observation
            .items_seen
            .unwrap_or_else(|| fact_count(&observation.facts)),
    }
}

fn fact_count(facts: &[AttentionRouterSourceFact]) -> u64 {
    facts
        .iter()
        .map(|fact| fact.count.unwrap_or(1))
        .sum::<u64>()
}

fn required_source_kinds() -> [AttentionRouterSourceKind; 6] {
    [
        AttentionRouterSourceKind::Beads,
        AttentionRouterSourceKind::AgentMail,
        AttentionRouterSourceKind::Git,
        AttentionRouterSourceKind::Rch,
        AttentionRouterSourceKind::PaneState,
        AttentionRouterSourceKind::OperatingEnvelope,
    ]
}

fn missing_source_observation(
    source_kind: AttentionRouterSourceKind,
    generated_at_ms: u64,
) -> AttentionRouterSourceObservation {
    match source_kind {
        AttentionRouterSourceKind::PaneState => AttentionRouterSourceObservation::new(
            "pane_state.not_configured",
            source_kind,
            AttentionRouterSourceHealth::NotConfigured,
            "collector.optional",
            "pane state source was not configured by the caller",
        )
        .live(generated_at_ms, 0)
        .with_fact(
            AttentionRouterSourceFact::new(
                AttentionRouterSourceFactKind::SourceNotConfigured,
                "pane state is optional and was not configured",
            )
            .with_reason_code("pane_state.optional_not_configured"),
        ),
        _ => {
            let slug = source_kind.slug();
            AttentionRouterSourceObservation::new(
                format!("{slug}.unavailable"),
                source_kind,
                AttentionRouterSourceHealth::Unavailable,
                "collector.unavailable",
                format!("{slug} source was not collected by the caller"),
            )
            .live(generated_at_ms, 0)
            .with_fact(
                AttentionRouterSourceFact::new(
                    AttentionRouterSourceFactKind::SourceUnavailable,
                    format!("{slug} source was missing from adapter input"),
                )
                .with_reason_code(format!("source.{slug}.missing")),
            )
        }
    }
}

fn bounded_string(value: impl Into<String>, fallback: &str) -> String {
    let value = value.into();
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return fallback.to_string();
    }

    let mut output = String::new();
    for ch in trimmed.chars().take(ATTENTION_ROUTER_SUMMARY_MAX_CHARS) {
        output.push(ch);
    }
    if trimmed.chars().count() > ATTENTION_ROUTER_SUMMARY_MAX_CHARS {
        let keep = ATTENTION_ROUTER_SUMMARY_MAX_CHARS.saturating_sub(3);
        output = trimmed.chars().take(keep).collect::<String>();
        output.push_str("...");
    }
    output
}

fn push_unique(values: &mut Vec<String>, value: impl Into<String>) {
    let value = bounded_string(value, "");
    if !value.is_empty() && !values.contains(&value) {
        values.push(value);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn source<'a>(
        bundle: &'a AttentionRouterSourceBundle,
        kind: AttentionRouterSourceKind,
    ) -> &'a AttentionRouterSourceSnapshot {
        bundle
            .sources
            .iter()
            .find(|source| source.source_kind == kind)
            .unwrap_or_else(|| panic!("missing source {kind:?}"))
    }

    #[test]
    fn missing_required_sources_are_explicitly_unhealthy() {
        let bundle = build_attention_router_source_bundle(
            &AttentionRouterSourceAdapterInput::new(1_770_000_000_100, "/repo").with_observation(
                AttentionRouterSourceObservation::new(
                    "beads.ready",
                    AttentionRouterSourceKind::Beads,
                    AttentionRouterSourceHealth::Available,
                    "br ready --json",
                    "ready beads were collected",
                )
                .items_seen(2)
                .with_fact(
                    AttentionRouterSourceFact::new(
                        AttentionRouterSourceFactKind::BeadsReady,
                        "two ready beads",
                    )
                    .count(2)
                    .with_bead_id("ft-ready")
                    .with_reason_code("beads.ready_available"),
                ),
            ),
        );

        assert_eq!(bundle.contract_id, ATTENTION_ROUTER_CONTRACT_ID);
        assert!(!bundle.raw_pane_content_stored);
        assert!(!bundle.raw_message_bodies_stored);
        assert!(!bundle.side_effects_executed);
        assert_eq!(
            source(&bundle, AttentionRouterSourceKind::Beads).health,
            AttentionRouterSourceHealth::Available
        );
        assert_eq!(
            source(&bundle, AttentionRouterSourceKind::AgentMail).health,
            AttentionRouterSourceHealth::Unavailable
        );
        assert_eq!(
            source(&bundle, AttentionRouterSourceKind::PaneState).health,
            AttentionRouterSourceHealth::NotConfigured
        );
        assert!(bundle.source_health.iter().any(|record| {
            record.source_kind == AttentionRouterSourceKind::AgentMail
                && record.health == AttentionRouterSourceHealth::Unavailable
        }));
        assert!(bundle.source_health.iter().any(|record| {
            record.source_kind == AttentionRouterSourceKind::PaneState
                && record.health == AttentionRouterSourceHealth::NotConfigured
        }));
    }

    #[test]
    fn source_facts_are_bounded_redacted_and_deduplicated() {
        let long_summary = "pane output ".repeat(80);
        let bundle = build_attention_router_source_bundle(
            &AttentionRouterSourceAdapterInput::new(1, "/repo").with_observation(
                AttentionRouterSourceObservation::new(
                    "pane_state.live",
                    AttentionRouterSourceKind::PaneState,
                    AttentionRouterSourceHealth::Degraded,
                    "ft robot state --format toon",
                    long_summary,
                )
                .redaction_posture(AttentionRouterRedactionPosture::SummaryOnly)
                .with_reason_code("pane_state.summary_only")
                .with_reason_code("pane_state.summary_only")
                .with_fact(
                    AttentionRouterSourceFact::new(
                        AttentionRouterSourceFactKind::PaneCodexPlaceholderCaveat,
                        "Codex placeholder text is caveat evidence, not stuck evidence",
                    )
                    .with_agent_name("IvoryCreek")
                    .with_reason_code("pane_state.codex_placeholder_caveat"),
                ),
            ),
        );
        let pane = source(&bundle, AttentionRouterSourceKind::PaneState);

        assert!(pane.redacted);
        assert_eq!(
            pane.redaction_posture,
            AttentionRouterRedactionPosture::SummaryOnly
        );
        assert!(
            pane.reason_codes
                .contains(&"pane_state.summary_only".to_string())
        );
        assert_eq!(
            pane.reason_codes
                .iter()
                .filter(|reason| reason.as_str() == "pane_state.summary_only")
                .count(),
            1
        );
        assert!(pane.source_id.len() <= ATTENTION_ROUTER_SUMMARY_MAX_CHARS);
        assert!(
            pane.facts
                .iter()
                .all(|fact| { fact.summary.chars().count() <= ATTENTION_ROUTER_SUMMARY_MAX_CHARS })
        );
    }

    #[test]
    fn adapters_preserve_agent_mail_git_and_rch_signals() {
        let bundle = build_attention_router_source_bundle(
            &AttentionRouterSourceAdapterInput::new(2, "/repo")
                .with_observation(
                    AttentionRouterSourceObservation::new(
                        "agent_mail.inbox",
                        AttentionRouterSourceKind::AgentMail,
                        AttentionRouterSourceHealth::Available,
                        "mcp.agent_mail.fetch_inbox",
                        "recent inbox metadata collected",
                    )
                    .with_fact(
                        AttentionRouterSourceFact::new(
                            AttentionRouterSourceFactKind::AgentMailAckRequired,
                            "ack-required messages need response",
                        )
                        .count(2)
                        .with_agent_name("SapphireCardinal")
                        .with_reason_code("agent_mail.ack_required"),
                    )
                    .with_fact(
                        AttentionRouterSourceFact::new(
                            AttentionRouterSourceFactKind::AgentMailFileReservations,
                            "one active reservation overlaps a planned path",
                        )
                        .count(1)
                        .with_affected_path("crates/frankenterm-core/src/attention_router.rs")
                        .with_reason_code("agent_mail.file_reservation_overlap"),
                    ),
                )
                .with_observation(
                    AttentionRouterSourceObservation::new(
                        "git.status",
                        AttentionRouterSourceKind::Git,
                        AttentionRouterSourceHealth::Degraded,
                        "git status --short --branch",
                        "dirty tree requires ownership firewall",
                    )
                    .with_fact(
                        AttentionRouterSourceFact::new(
                            AttentionRouterSourceFactKind::GitDirtyPaths,
                            "tracked file is dirty",
                        )
                        .with_affected_path("docs/robot-contracts/attention-router.md")
                        .with_reason_code("git.tracked_dirty_paths"),
                    ),
                )
                .with_observation(
                    AttentionRouterSourceObservation::new(
                        "rch.dry_run",
                        AttentionRouterSourceKind::Rch,
                        AttentionRouterSourceHealth::Degraded,
                        "rch remote-required dry-run",
                        "RCH refused remote proof before Cargo",
                    )
                    .with_fact(
                        AttentionRouterSourceFact::new(
                            AttentionRouterSourceFactKind::RchProofStarvation,
                            "remote-required proof is starved",
                        )
                        .with_reason_code("rch.proof_starved"),
                    ),
                ),
        );

        let mail = source(&bundle, AttentionRouterSourceKind::AgentMail);
        assert_eq!(mail.items_seen, 3);
        assert!(mail.facts.iter().any(|fact| {
            fact.fact == AttentionRouterSourceFactKind::AgentMailAckRequired
                && fact.count == Some(2)
        }));
        let git = source(&bundle, AttentionRouterSourceKind::Git);
        assert_eq!(git.health, AttentionRouterSourceHealth::Degraded);
        assert!(git.facts.iter().any(|fact| {
            fact.affected_paths
                .contains(&"docs/robot-contracts/attention-router.md".to_string())
        }));
        let rch = source(&bundle, AttentionRouterSourceKind::Rch);
        assert!(rch.reason_codes.contains(&"rch.proof_starved".to_string()));
        assert!(
            bundle
                .warnings
                .iter()
                .any(|warning| warning.contains("rch"))
        );
    }

    #[test]
    fn bundle_serializes_stable_contract_values() {
        let bundle = build_attention_router_source_bundle(
            &AttentionRouterSourceAdapterInput::new(3, "/repo").with_observation(
                AttentionRouterSourceObservation::new(
                    "operating_envelope.proof_posture",
                    AttentionRouterSourceKind::OperatingEnvelope,
                    AttentionRouterSourceHealth::Available,
                    "ft operating-envelope snapshot",
                    "target hardware proof posture collected",
                )
                .with_fact(
                    AttentionRouterSourceFact::new(
                        AttentionRouterSourceFactKind::OperatingEnvelopeProofPosture,
                        "target class proof remains skipped",
                    )
                    .with_reason_code("operating_envelope.target_class_skipped"),
                ),
            ),
        );

        let value = serde_json::to_value(bundle).expect("attention source bundle serializes");
        assert_eq!(
            value["contract_id"].as_str(),
            Some(ATTENTION_ROUTER_CONTRACT_ID)
        );
        assert_eq!(
            value["schema_version"].as_u64(),
            Some(u64::from(ATTENTION_ROUTER_SOURCE_SCHEMA_VERSION))
        );
        assert_eq!(value["side_effects_executed"].as_bool(), Some(false));
        assert_eq!(value["raw_message_bodies_stored"].as_bool(), Some(false));
        assert!(value["sources"].as_array().is_some_and(|sources| {
            sources.iter().any(|source| {
                source["source_kind"].as_str() == Some("operating_envelope")
                    && source["health"].as_str() == Some("available")
            })
        }));
    }
}
