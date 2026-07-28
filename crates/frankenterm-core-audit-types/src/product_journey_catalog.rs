//! Versioned product-journey catalog contract for FrankenTerm.
//!
//! The catalog is deliberately a product-truth contract rather than a runtime
//! result or performance attestation.  It records the user, fleet, topology,
//! target, workflow, support, evidence, and release dimensions independently
//! so that an attractive fixture or proxy result cannot silently become a
//! support claim.
//!
//! This module is leaf-clean: it performs no file I/O and depends only on
//! `serde` and `std`.  Repository-path and Beads existence checks belong in the
//! integration test that loads the checked-in catalog.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

/// The only contract identifier accepted by schema version 1.
pub const PRODUCT_JOURNEY_CONTRACT_ID: &str = "ft.product_journey_catalog.v1";

/// The only schema version accepted by this implementation.
pub const PRODUCT_JOURNEY_SCHEMA_VERSION: u32 = 1;

/// The Bead that owns version 1 of the product-journey contract.
pub const PRODUCT_JOURNEY_SOURCE_BEAD_ID: &str =
    "ft-interactive-swarm-product-convergence-7xqz4.1.1";

/// Number of required product-journey coverage cells.
pub const REQUIRED_COVERAGE_CELL_COUNT: usize = 32;

/// Number of exact field journeys governed by the catalog.
pub const REQUIRED_FIELD_JOURNEY_COUNT: usize = 14;

/// Whether the catalog is still a contract or is bound to qualifying evidence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CatalogClaimState {
    /// The catalog describes intended behavior but cannot mint support claims.
    ContractOnly,
    /// Qualifying evidence producers are bound to the declared claims.
    EvidenceBound,
}

/// A first-class product persona.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProductPersona {
    /// A person directly typing, navigating, resizing, and reading output.
    InteractiveHuman,
    /// A human or agent supervising and steering a fleet through a meta-agent.
    MetaAgentOperator,
    /// An unattended agent using Robot/MCP automation surfaces.
    AutomationAgent,
    /// An operator diagnosing, containing, and recovering an incident.
    IncidentResponder,
}

impl ProductPersona {
    /// Every persona required by the v1 contract.
    pub const ALL: [Self; 4] = [
        Self::InteractiveHuman,
        Self::MetaAgentOperator,
        Self::AutomationAgent,
        Self::IncidentResponder,
    ];

    /// Stable serialized label.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::InteractiveHuman => "interactive_human",
            Self::MetaAgentOperator => "meta_agent_operator",
            Self::AutomationAgent => "automation_agent",
            Self::IncidentResponder => "incident_responder",
        }
    }
}

/// Exact pane-count qualification point.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FleetPoint {
    /// Two panes.
    Q002,
    /// Twenty panes.
    Q020,
    /// Fifty panes.
    Q050,
    /// Two hundred panes.
    Q200,
}

impl FleetPoint {
    /// Every fleet point required by the v1 contract.
    pub const ALL: [Self; 4] = [Self::Q002, Self::Q020, Self::Q050, Self::Q200];

    /// Exact pane count represented by this qualification point.
    #[must_use]
    pub const fn pane_count(self) -> u16 {
        match self {
            Self::Q002 => 2,
            Self::Q020 => 20,
            Self::Q050 => 50,
            Self::Q200 => 200,
        }
    }

    /// Stable serialized label.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Q002 => "q002",
            Self::Q020 => "q020",
            Self::Q050 => "q050",
            Self::Q200 => "q200",
        }
    }
}

/// Connection topology covered by the product contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Topology {
    /// Controller and pane workloads run on the local target.
    LocalOnly,
    /// A Mac controller drives a session host over the local network.
    MacLanRemote,
}

impl Topology {
    /// Every topology required by the v1 contract.
    pub const ALL: [Self; 2] = [Self::LocalOnly, Self::MacLanRemote];

    /// Stable serialized label.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::LocalOnly => "local_only",
            Self::MacLanRemote => "mac_lan_remote",
        }
    }
}

/// Concrete mux transport exercised by a journey variant or producer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Transport {
    /// In-process or same-host mux transport.
    LocalMux,
    /// Networked mux transport between the Mac controller and session host.
    RemoteMux,
}

impl Transport {
    /// Transport required by a topology in schema version 1.
    #[must_use]
    pub const fn for_topology(topology: Topology) -> Self {
        match topology {
            Topology::LocalOnly => Self::LocalMux,
            Topology::MacLanRemote => Self::RemoteMux,
        }
    }

    /// Stable serialized label.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::LocalMux => "local_mux",
            Self::RemoteMux => "remote_mux",
        }
    }
}

/// Declared support state, independent of evidence and run verdict.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SupportState {
    /// Product behavior is supported under the declared target contract.
    Supported,
    /// Product behavior is conditional on explicit constraints.
    Conditional,
    /// Product behavior is deliberately unavailable.
    Unavailable,
}

/// Strength and availability of retained evidence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceState {
    /// Qualifying direct evidence is retained.
    Proven,
    /// Only proxy evidence is retained.
    ProxyOnly,
    /// Only synthetic or recorded-fixture evidence is retained.
    FixtureOnly,
    /// The proof lane was deliberately skipped and remains unproven.
    SkippedNotProven,
    /// A named infrastructure or authority boundary blocked the proof.
    Blocked,
    /// No evidence artifact exists.
    Missing,
}

/// Verdict of the most authoritative run represented by a variant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunVerdict {
    /// All declared acceptance gates passed.
    Pass,
    /// One or more declared acceptance gates failed.
    Fail,
    /// The run completed with an explicit degraded result.
    Degraded,
    /// No qualifying run has occurred.
    NotRun,
    /// The physical target was unavailable.
    TargetUnavailable,
}

/// Coverage supplied by evidence-producing gates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProducerCoverage {
    /// Direct producers cover the complete variant claim.
    Direct,
    /// Producers cover only part of the variant claim.
    Partial,
    /// No producer currently covers the variant claim.
    Gap,
}

/// Whether a catalog entry gates release.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReleaseRequirement {
    /// The entry must reach its declared acceptance state for release.
    Required,
    /// The entry is useful but does not gate the current release.
    Optional,
    /// The entry is deliberately outside the current release claim.
    Excluded,
}

/// Exact target families represented by the v1 product contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TargetMode {
    /// Native Mac16,11-class M4 Pro controller.
    M4ProNative,
    /// Native base-M5 target, independently qualified when available.
    M5Native,
    /// Transitional combined M5 Pro/Max planning row. Schema v1 permanently
    /// freezes every lane at unknown/missing/not-run/unknown; it is never
    /// support authority for either physical SKU.
    M5ProMaxNative,
    /// Native Threadripper PRO 5995WX session host.
    #[serde(rename = "threadripper_pro_5995wx_native")]
    ThreadripperPro5995wxNative,
}

impl TargetMode {
    /// Every target family required by the v1 contract.
    pub const ALL: [Self; 4] = [
        Self::M4ProNative,
        Self::M5Native,
        Self::M5ProMaxNative,
        Self::ThreadripperPro5995wxNative,
    ];

    /// Stable serialized label.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::M4ProNative => "m4_pro_native",
            Self::M5Native => "m5_native",
            Self::M5ProMaxNative => "m5_pro_max_native",
            Self::ThreadripperPro5995wxNative => "threadripper_pro_5995wx_native",
        }
    }
}

/// How a user or agent drives a journey.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActorMode {
    /// Direct foreground human interaction.
    HumanInteractive,
    /// Supervised orchestration through a meta-agent.
    MetaAgentSupervised,
    /// Unattended automation through machine-facing control surfaces.
    AutomationUnattended,
    /// Interactive incident diagnosis, containment, and recovery.
    IncidentResponse,
}

impl ActorMode {
    /// Every actor mode required by the v1 contract.
    pub const ALL: [Self; 4] = [
        Self::HumanInteractive,
        Self::MetaAgentSupervised,
        Self::AutomationUnattended,
        Self::IncidentResponse,
    ];

    /// Stable serialized label.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::HumanInteractive => "human_interactive",
            Self::MetaAgentSupervised => "meta_agent_supervised",
            Self::AutomationUnattended => "automation_unattended",
            Self::IncidentResponse => "incident_response",
        }
    }
}

/// Whether one exact controller/session-host qualification lane is available.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TargetAvailability {
    /// The exact controller/session-host lane is available to the campaign.
    Available,
    /// The exact controller/session-host lane is explicitly unavailable.
    Unavailable,
    /// Lane availability has not been established.
    Unknown,
}

/// Freshness of retained target-qualification evidence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FreshnessState {
    /// The evidence is current for the catalog revision.
    Current,
    /// Evidence exists but is too old for support promotion.
    Stale,
    /// Freshness has not been established.
    Unknown,
}

/// Authority held by a retained review record.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewAuthorityKind {
    /// Machine review that cannot approve product claims.
    AutomatedInformational,
    /// Human product-owner authority.
    HumanProductOwner,
    /// Human visual-quality authority.
    HumanVisual,
    /// Human accessibility-review authority.
    HumanAccessibility,
    /// Human privacy-review authority.
    HumanPrivacy,
}

impl ReviewAuthorityKind {
    /// Stable serialized label.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::AutomatedInformational => "automated_informational",
            Self::HumanProductOwner => "human_product_owner",
            Self::HumanVisual => "human_visual",
            Self::HumanAccessibility => "human_accessibility",
            Self::HumanPrivacy => "human_privacy",
        }
    }
}

/// Review outcome retained in catalog history.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewDisposition {
    /// The reviewer approved the contract at the recorded revision.
    Approved,
    /// The reviewer requested changes.
    ChangesRequested,
    /// The record is informational and does not assert approval.
    Informational,
}

/// Lifecycle state of a documented contradiction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContradictionStatus {
    /// The contradiction remains unresolved and blocks applicable claims.
    Open,
    /// The contradiction has retained resolution evidence.
    Resolved,
}

/// State-dependent support declaration.
///
/// The internally tagged representation prevents fields from one state from
/// leaking into another state.  In particular, an unavailable row cannot carry
/// supported-only acceptance or evidence fields.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum SupportDeclaration {
    /// Supported behavior authorized by a content-bound promotion receipt.
    Supported {
        /// Relative repository reference to the signed promotion receipt.
        promotion_receipt_ref: String,
        /// Lowercase SHA-256 of the detached immutable receipt's exact raw bytes.
        promotion_receipt_sha256: String,
    },
    /// Behavior available only under named constraints.
    Conditional {
        /// Human-readable reason the support claim is conditional.
        reason: String,
        /// Non-empty constraints that bound the conditional behavior.
        constraints: Vec<String>,
        /// User-visible fallback or safe alternative.
        fallback: String,
        /// Beads that can remove or narrow the conditions.
        tracking_bead_ids: Vec<String>,
    },
    /// Behavior that is deliberately unavailable.
    Unavailable {
        /// Human-readable reason the behavior is unavailable.
        reason: String,
        /// User-visible fallback or next action.
        fallback: String,
        /// Beads tracking remediation or future qualification.
        tracking_bead_ids: Vec<String>,
    },
}

impl SupportDeclaration {
    /// Return the state independently of the state-specific payload.
    #[must_use]
    pub const fn state(&self) -> SupportState {
        match self {
            Self::Supported { .. } => SupportState::Supported,
            Self::Conditional { .. } => SupportState::Conditional,
            Self::Unavailable { .. } => SupportState::Unavailable,
        }
    }
}

/// Persona metadata retained in the catalog.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PersonaDefinition {
    /// Stable persona identifier.
    pub persona: ProductPersona,
    /// Human-readable name.
    pub title: String,
    /// Goals that motivate this persona.
    pub goals: Vec<String>,
    /// Product-level outcomes that make the persona successful.
    pub success_outcomes: Vec<String>,
    /// Relative repository references supporting the definition.
    pub source_refs: Vec<String>,
}

/// Exact fleet-point metadata retained in the catalog.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FleetPointDefinition {
    /// Stable fleet qualification point.
    pub fleet_point: FleetPoint,
    /// Exact pane count; validated against [`FleetPoint::pane_count`].
    pub pane_count: u16,
    /// Human-readable name.
    pub title: String,
    /// Workload characteristics expected at this qualification point.
    pub workload_characteristics: Vec<String>,
    /// Relative repository references supporting the definition.
    pub source_refs: Vec<String>,
}

/// Target placement allowed by a topology.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TopologyTargetPosture {
    /// Target modes allowed to run the controller/UI side.
    pub controller_modes: Vec<TargetMode>,
    /// Target modes allowed to run pane/session workloads.
    pub session_host_modes: Vec<TargetMode>,
}

/// Connection-topology metadata retained in the catalog.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TopologyDefinition {
    /// Stable topology identifier.
    pub topology: Topology,
    /// Human-readable name.
    pub title: String,
    /// Controller and session-host target posture.
    pub target_posture: TopologyTargetPosture,
    /// Product-facing topology description.
    pub description: String,
    /// Relative repository references supporting the definition.
    pub source_refs: Vec<String>,
}

/// Physical target-class metadata.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TargetClassDefinition {
    /// Stable target-class identifier.
    pub target_class_id: String,
    /// Human-readable name.
    pub title: String,
    /// Closed hardware family.
    pub target_mode: TargetMode,
    /// Operating-system/platform identity.
    pub platform: String,
    /// SKU and topology identity, without extrapolation to other targets.
    pub hardware_identity: String,
    /// Relative repository references supporting the definition.
    pub source_refs: Vec<String>,
}

/// Actor-mode metadata.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActorModeDefinition {
    /// Stable actor mode.
    pub actor_mode: ActorMode,
    /// Human-readable name.
    pub title: String,
    /// Personas permitted to use this mode.
    pub personas: Vec<ProductPersona>,
    /// Product-facing mode description.
    pub description: String,
    /// Relative repository references supporting the definition.
    pub source_refs: Vec<String>,
}

/// Evidence or acceptance gate referenced by journeys and variants.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GateDefinition {
    /// Stable gate identifier.
    pub gate_id: String,
    /// Human-readable name.
    pub title: String,
    /// Whether the gate blocks release.
    pub release_requirement: ReleaseRequirement,
    /// Beads responsible for producing gate evidence.
    pub producer_bead_ids: Vec<String>,
    /// Relative repository references to retained evidence, when present.
    pub evidence_refs: Vec<String>,
    /// Relative repository references establishing the gate definition.
    pub source_refs: Vec<String>,
    /// Product-facing gate description.
    pub description: String,
}

/// A product journey spanning the complete lifecycle.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JourneyDefinition {
    /// Stable journey identifier used by tests and claims.
    pub journey_id: String,
    /// Human-readable name.
    pub title: String,
    /// Exact `.11.1` through `.11.14` field-journey Bead.
    pub field_bead_id: String,
    /// Whether the journey gates release.
    pub release_requirement: ReleaseRequirement,
    /// Personas for whom the journey is defined.
    pub personas: Vec<ProductPersona>,
    /// Ordered identity/preflight followed by clean setup steps. Entry zero is
    /// the pre-mutation identity/preflight boundary; later entries establish
    /// installation, launch, workspace, and actors.
    pub setup: Vec<String>,
    /// Normal steady-work behavior.
    pub steady_work: Vec<String>,
    /// Overload and failure behavior exercised by the journey.
    pub failure_overload: Vec<String>,
    /// Recovery and final-intent convergence behavior.
    pub recovery: Vec<String>,
    /// Orderly teardown and retained-evidence behavior.
    pub teardown: Vec<String>,
    /// User-visible outcomes required from the journey.
    pub user_outcomes: Vec<String>,
    /// Accessibility expectations exercised throughout the journey.
    pub accessibility_expectations: Vec<String>,
    /// Privacy and data-minimization expectations.
    pub privacy_expectations: Vec<String>,
    /// Evidence the journey must eventually retain.
    pub evidence_requirements: Vec<String>,
    /// Relative repository references supporting the journey.
    pub source_refs: Vec<String>,
    /// Acceptance/evidence gates that govern the journey.
    pub gate_ids: Vec<String>,
}

/// The exact composite key for a promised catalog coverage cell.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CoverageKey {
    /// Product persona.
    pub persona: ProductPersona,
    /// Exact fleet point.
    pub fleet_point: FleetPoint,
    /// Connection topology.
    pub topology: Topology,
}

impl CoverageKey {
    /// Construct a coverage key.
    #[must_use]
    pub const fn new(persona: ProductPersona, fleet_point: FleetPoint, topology: Topology) -> Self {
        Self {
            persona,
            fleet_point,
            topology,
        }
    }

    /// Stable label suitable for diagnostics.
    #[must_use]
    pub fn label(self) -> String {
        format!(
            "{}:{}:{}",
            self.persona.as_str(),
            self.fleet_point.as_str(),
            self.topology.as_str()
        )
    }
}

/// Exact evidence-producer binding for one complete coverage cell.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExactProducerBinding {
    /// Bead responsible for producing the exact evidence.
    pub producer_bead_id: String,
    /// Exact persona/fleet/topology cell covered by the producer.
    pub coverage: CoverageKey,
    /// Actor mode exercised by the producer.
    pub actor_mode: ActorMode,
    /// Mux transport exercised by the producer.
    pub transport: Transport,
    /// Controller target classes covered by the producer.
    pub controller_target_class_ids: Vec<String>,
    /// Session-host target classes covered by the producer.
    pub session_host_target_class_ids: Vec<String>,
    /// Relative repository references establishing the binding.
    pub source_refs: Vec<String>,
}

/// One exact controller/session-host target qualification.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TargetQualification {
    /// Stable qualification identifier.
    pub qualification_id: String,
    /// Controller target class exercised by the qualification.
    pub controller_target_class_id: String,
    /// Session-host target class exercised by the qualification.
    pub session_host_target_class_id: String,
    /// Mux transport exercised by the qualification.
    pub transport: Transport,
    /// Relative repository reference to retained route identity, when present.
    pub route_identity_ref: Option<String>,
    /// Relative repository reference to retained candidate identity, when present.
    pub candidate_identity_ref: Option<String>,
    /// Availability of this exact controller/session-host qualification lane.
    pub availability: TargetAvailability,
    /// Strength and authority of the retained qualification evidence.
    pub evidence_state: EvidenceState,
    /// Verdict of the retained qualification run.
    pub run_verdict: RunVerdict,
    /// Freshness of the retained qualification evidence.
    pub freshness_state: FreshnessState,
    /// Relative repository references to retained evidence.
    pub evidence_refs: Vec<String>,
    /// Repository or Bead references explaining a skipped or blocked lane.
    pub blocker_refs: Vec<String>,
}

/// A support/evidence declaration for one exact coverage cell.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JourneyVariant {
    /// Stable variant identifier.
    pub variant_id: String,
    /// Unique public claim identifier for this exact cell.
    pub claim_id: String,
    /// Exact persona/fleet/topology coverage key.
    pub coverage: CoverageKey,
    /// Product journeys exercised by this cell.
    pub journey_ids: Vec<String>,
    /// Mode used to drive the cell.
    pub actor_mode: ActorMode,
    /// Mux transport exercised by this cell.
    pub transport: Transport,
    /// Declared support state and its state-specific payload.
    pub support: SupportDeclaration,
    /// Completeness of evidence-producer coverage.
    pub producer_coverage: ProducerCoverage,
    /// Exact producers bound to the complete cell.
    pub exact_producer_bindings: Vec<ExactProducerBinding>,
    /// Producers known to cover only part of the cell.
    pub partial_producer_bead_ids: Vec<String>,
    /// Three canonical native-target qualifications for the topology.
    pub target_qualifications: Vec<TargetQualification>,
    /// Whether this cell gates release.
    pub release_requirement: ReleaseRequirement,
    /// Gates that govern this cell.
    pub gate_ids: Vec<String>,
    /// Relative repository references supporting the declaration.
    pub source_refs: Vec<String>,
}

/// Mapping from a legacy scenario identifier to canonical journeys.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LegacyMapping {
    /// Stable legacy identifier.
    pub legacy_id: String,
    /// Canonical journey identifiers replacing or refining the legacy term.
    pub journey_ids: Vec<String>,
    /// Relative repository references for the legacy scenario.
    pub source_refs: Vec<String>,
    /// Human-readable mapping rationale.
    pub notes: String,
}

/// Mapping from a README promise to canonical journeys.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReadmeMapping {
    /// Stable mapping identifier.
    pub mapping_id: String,
    /// Relative README reference, optionally including a fragment.
    pub readme_ref: String,
    /// Exact README claim text at the catalog revision.
    pub claim_text: String,
    /// Lowercase SHA-256 of the exact UTF-8 `claim_text` bytes, without normalization.
    pub claim_sha256: String,
    /// Canonical journey identifiers that own the promise.
    pub journey_ids: Vec<String>,
    /// Human-readable scope and non-claim notes.
    pub notes: String,
}

/// Human or machine review retained against a catalog revision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewRecord {
    /// Stable review identifier.
    pub review_id: String,
    /// UTC timestamp as retained by the catalog artifact.
    pub reviewed_at_utc: String,
    /// Exact catalog revision reviewed.
    pub reviewed_catalog_revision: String,
    /// Lowercase forty-hex Git commit reviewed, when authority was exercised.
    pub reviewed_commit: Option<String>,
    /// Reviewer identity or review lane.
    pub reviewer: String,
    /// Authority held by the reviewer.
    pub authority_kind: ReviewAuthorityKind,
    /// Review disposition.
    pub disposition: ReviewDisposition,
    /// Non-empty catalog scopes covered by the review.
    pub scope: Vec<String>,
    /// Relative repository reference to the authority receipt, when present.
    pub authority_receipt_ref: Option<String>,
    /// Lowercase SHA-256 of the detached immutable receipt's exact raw bytes.
    pub authority_receipt_sha256: Option<String>,
    /// Review notes and authority boundary.
    pub notes: String,
    /// Relative repository references used by the review.
    pub source_refs: Vec<String>,
}

/// A product or documentation contradiction tracked by the catalog.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContradictionRecord {
    /// Stable contradiction identifier.
    pub contradiction_id: String,
    /// Human-readable contradiction title.
    pub title: String,
    /// Whether the contradiction remains open.
    pub status: ContradictionStatus,
    /// True when the contradiction applies to every public claim.
    pub blocks_all_claims: bool,
    /// Exact claim identifiers affected when this is not a global blocker.
    pub affected_claim_ids: Vec<String>,
    /// Beads tracking diagnosis and resolution.
    pub tracking_bead_ids: Vec<String>,
    /// Relative repository references establishing the contradiction.
    pub source_refs: Vec<String>,
    /// Relative repository references proving resolution, when resolved.
    pub resolution_refs: Vec<String>,
    /// Human-readable scope and resolution notes.
    pub notes: String,
}

/// Catalog revision history entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChangeRecord {
    /// Stable change identifier.
    pub change_id: String,
    /// Exact catalog content revision introduced by this append-only row.
    pub catalog_revision: String,
    /// UTC timestamp as retained by the catalog artifact.
    pub changed_at_utc: String,
    /// Human-readable change summary.
    pub summary: String,
    /// Relative repository references motivating the change.
    pub source_refs: Vec<String>,
}

/// Complete versioned product-journey catalog.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProductJourneyCatalog {
    /// Contract identifier; must equal [`PRODUCT_JOURNEY_CONTRACT_ID`].
    pub contract_id: String,
    /// Schema version; must equal [`PRODUCT_JOURNEY_SCHEMA_VERSION`].
    pub schema_version: u32,
    /// Catalog content revision.
    pub catalog_revision: String,
    /// Bead that owns this catalog contract.
    pub source_bead_id: String,
    /// Whether the catalog can carry evidence-bound support claims.
    pub catalog_claim_state: CatalogClaimState,
    /// Required persona definitions.
    pub personas: Vec<PersonaDefinition>,
    /// Exact fleet-point definitions.
    pub fleet_points: Vec<FleetPointDefinition>,
    /// Required connection topologies.
    pub topologies: Vec<TopologyDefinition>,
    /// Identity/capability inventory for physical target classes.
    pub target_classes: Vec<TargetClassDefinition>,
    /// Actor modes and their allowed personas.
    pub actor_modes: Vec<ActorModeDefinition>,
    /// Evidence and release gates.
    pub gates: Vec<GateDefinition>,
    /// Fourteen complete field journeys.
    pub journey_definitions: Vec<JourneyDefinition>,
    /// Explicit promised persona/fleet/topology cells.
    pub required_coverage: Vec<CoverageKey>,
    /// One support/evidence declaration per required coverage cell.
    pub variants: Vec<JourneyVariant>,
    /// Legacy-scenario mappings.
    pub legacy_mappings: Vec<LegacyMapping>,
    /// README-promise mappings.
    pub readme_mappings: Vec<ReadmeMapping>,
    /// Explicit documentation and product contradictions.
    pub contradictions: Vec<ContradictionRecord>,
    /// Retained review history.
    pub review_history: Vec<ReviewRecord>,
    /// Retained revision history.
    pub change_history: Vec<ChangeRecord>,
}

impl ProductJourneyCatalog {
    /// Validate all semantic invariants that JSON Schema cannot express.
    #[must_use]
    pub fn validate(&self) -> CatalogValidationReport {
        validate_product_journey_catalog(self)
    }
}

/// Stable semantic validation category.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CatalogValidationCode {
    /// Contract identifier differs from the implemented contract.
    UnknownContract,
    /// Schema version differs from the implemented schema.
    UnknownSchemaVersion,
    /// A required scalar or list is empty.
    EmptyRequiredField,
    /// A stable identifier is duplicated.
    DuplicateId,
    /// A persona/fleet/topology composite key is duplicated.
    DuplicateCompositeKey,
    /// A public claim identifier is duplicated.
    DuplicateClaimId,
    /// The exact required coverage matrix is incomplete.
    MissingRequiredCoverage,
    /// A reference names an undefined catalog entity.
    DanglingReference,
    /// A required materialized lifecycle phase is empty.
    EmptyLifecyclePhase,
    /// Support, evidence, run verdict, or availability disagree.
    ContradictoryClaim,
    /// A contract-only catalog attempted to declare support.
    ContractOnlySupportedClaim,
    /// Schema version 1 attempted to use unsupported claim authority.
    UnsupportedClaimAuthority,
    /// Producer coverage disagrees with exact or partial binding fields.
    InvalidProducerCoverage,
    /// A native target qualification is malformed or self-contradictory.
    InvalidTargetQualification,
    /// A retained review overstates or omits its human authority.
    InvalidReviewAuthority,
    /// A contradiction record is malformed or blocks an asserted claim.
    InvalidContradiction,
    /// The `.11.1` through `.11.14` field-journey binding is incomplete.
    IncompleteFieldJourneyBinding,
    /// A repository or Bead reference is malformed.
    MalformedReference,
    /// A definition violates a closed-domain invariant.
    InvalidDefinition,
}

impl CatalogValidationCode {
    /// Stable machine-facing code.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::UnknownContract => "PJC-CONTRACT-001",
            Self::UnknownSchemaVersion => "PJC-SCHEMA-001",
            Self::EmptyRequiredField => "PJC-SCHEMA-002",
            Self::DuplicateId => "PJC-UNIQUE-001",
            Self::DuplicateCompositeKey => "PJC-UNIQUE-002",
            Self::DuplicateClaimId => "PJC-UNIQUE-003",
            Self::MissingRequiredCoverage => "PJC-COVERAGE-001",
            Self::DanglingReference => "PJC-REFERENCE-001",
            Self::EmptyLifecyclePhase => "PJC-LIFECYCLE-001",
            Self::ContradictoryClaim => "PJC-CLAIM-001",
            Self::ContractOnlySupportedClaim => "PJC-CLAIM-002",
            Self::UnsupportedClaimAuthority => "PJC-AUTHORITY-001",
            Self::InvalidProducerCoverage => "PJC-PRODUCER-001",
            Self::InvalidTargetQualification => "PJC-TARGET-001",
            Self::InvalidReviewAuthority => "PJC-REVIEW-001",
            Self::InvalidContradiction => "PJC-CONTRADICTION-001",
            Self::IncompleteFieldJourneyBinding => "PJC-JOURNEY-001",
            Self::MalformedReference => "PJC-REFERENCE-002",
            Self::InvalidDefinition => "PJC-DEFINITION-001",
        }
    }
}

/// One actionable semantic validation error.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CatalogValidationError {
    /// Stable error category.
    pub code: CatalogValidationCode,
    /// JSON-style location of the failing field.
    pub path: String,
    /// Human-readable diagnostic.
    pub detail: String,
}

/// Aggregate semantic validation result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CatalogValidationReport {
    /// True only when no errors were found.
    pub valid: bool,
    /// All detected errors, retained in deterministic validation order.
    pub errors: Vec<CatalogValidationError>,
}

impl CatalogValidationReport {
    /// Whether this report contains a particular stable error category.
    #[must_use]
    pub fn contains_code(&self, code: CatalogValidationCode) -> bool {
        self.errors.iter().any(|error| error.code == code)
    }
}

struct ValidatorState {
    errors: Vec<CatalogValidationError>,
}

impl ValidatorState {
    fn new() -> Self {
        Self { errors: Vec::new() }
    }

    fn error(
        &mut self,
        code: CatalogValidationCode,
        path: impl Into<String>,
        detail: impl Into<String>,
    ) {
        self.errors.push(CatalogValidationError {
            code,
            path: path.into(),
            detail: detail.into(),
        });
    }

    fn require_text(&mut self, path: &str, value: &str) {
        if value.trim().is_empty() {
            self.error(
                CatalogValidationCode::EmptyRequiredField,
                path,
                "required text must not be empty",
            );
        }
    }

    fn require_text_list(&mut self, path: &str, values: &[String]) {
        if values.is_empty() {
            self.error(
                CatalogValidationCode::EmptyRequiredField,
                path,
                "required list must not be empty",
            );
            return;
        }
        for (index, value) in values.iter().enumerate() {
            self.require_text(&format!("{path}[{index}]"), value);
        }
    }

    fn require_repo_refs(&mut self, path: &str, refs: &[String], allow_empty: bool) {
        if !allow_empty && refs.is_empty() {
            self.error(
                CatalogValidationCode::EmptyRequiredField,
                path,
                "at least one repository reference is required",
            );
        }
        for (index, reference) in refs.iter().enumerate() {
            if let Err(detail) = validate_repository_reference(reference) {
                self.error(
                    CatalogValidationCode::MalformedReference,
                    format!("{path}[{index}]"),
                    detail,
                );
            }
        }
    }

    fn require_optional_repo_ref(&mut self, path: &str, reference: Option<&str>) {
        if let Some(reference) = reference
            && let Err(detail) = validate_repository_reference(reference)
        {
            self.error(CatalogValidationCode::MalformedReference, path, detail);
        }
    }

    fn require_catalog_refs(&mut self, path: &str, refs: &[String], allow_empty: bool) {
        if !allow_empty && refs.is_empty() {
            self.error(
                CatalogValidationCode::EmptyRequiredField,
                path,
                "at least one repository or Bead reference is required",
            );
        }
        for (index, reference) in refs.iter().enumerate() {
            if !is_well_formed_bead_id(reference)
                && let Err(detail) = validate_repository_reference(reference)
            {
                self.error(
                    CatalogValidationCode::MalformedReference,
                    format!("{path}[{index}]"),
                    detail,
                );
            }
        }
    }

    fn require_hex_digest(
        &mut self,
        code: CatalogValidationCode,
        path: &str,
        value: &str,
        expected_len: usize,
        label: &str,
    ) {
        if value.len() != expected_len
            || !value
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            self.error(
                code,
                path,
                format!(
                    "{label} must contain exactly {expected_len} lowercase hexadecimal characters"
                ),
            );
        }
    }

    fn require_bead_refs(&mut self, path: &str, refs: &[String], allow_empty: bool) {
        if !allow_empty && refs.is_empty() {
            self.error(
                CatalogValidationCode::EmptyRequiredField,
                path,
                "at least one tracking Bead is required",
            );
        }
        for (index, bead_id) in refs.iter().enumerate() {
            if !is_well_formed_bead_id(bead_id) {
                self.error(
                    CatalogValidationCode::MalformedReference,
                    format!("{path}[{index}]"),
                    format!("malformed Bead reference `{bead_id}`"),
                );
            }
        }
    }
}

/// Validate a product-journey catalog without performing file I/O.
#[must_use]
pub fn validate_product_journey_catalog(
    catalog: &ProductJourneyCatalog,
) -> CatalogValidationReport {
    let mut validator = ValidatorState::new();

    validate_header(catalog, &mut validator);
    let definition_index = validate_definitions(catalog, &mut validator);
    validate_journeys(catalog, &definition_index, &mut validator);
    validate_coverage(catalog, &definition_index, &mut validator);
    validate_mappings_and_history(catalog, &definition_index, &mut validator);

    CatalogValidationReport {
        valid: validator.errors.is_empty(),
        errors: validator.errors,
    }
}

struct DefinitionIndex {
    personas: BTreeSet<ProductPersona>,
    fleet_points: BTreeSet<FleetPoint>,
    topologies: BTreeSet<Topology>,
    target_classes: BTreeMap<String, TargetClassIndex>,
    actor_modes: BTreeMap<ActorMode, BTreeSet<ProductPersona>>,
    gate_ids: BTreeSet<String>,
}

#[derive(Debug, Clone, Copy)]
struct TargetClassIndex {
    mode: TargetMode,
}

fn validate_header(catalog: &ProductJourneyCatalog, validator: &mut ValidatorState) {
    if catalog.contract_id != PRODUCT_JOURNEY_CONTRACT_ID {
        validator.error(
            CatalogValidationCode::UnknownContract,
            "contract_id",
            format!(
                "expected `{PRODUCT_JOURNEY_CONTRACT_ID}`, got `{}`",
                catalog.contract_id
            ),
        );
    }
    if catalog.schema_version != PRODUCT_JOURNEY_SCHEMA_VERSION {
        validator.error(
            CatalogValidationCode::UnknownSchemaVersion,
            "schema_version",
            format!(
                "expected {PRODUCT_JOURNEY_SCHEMA_VERSION}, got {}",
                catalog.schema_version
            ),
        );
    }
    validator.require_text("catalog_revision", &catalog.catalog_revision);
    if catalog.source_bead_id != PRODUCT_JOURNEY_SOURCE_BEAD_ID {
        validator.error(
            CatalogValidationCode::MalformedReference,
            "source_bead_id",
            format!(
                "expected source Bead `{PRODUCT_JOURNEY_SOURCE_BEAD_ID}`, got `{}`",
                catalog.source_bead_id
            ),
        );
    }
    if catalog.catalog_claim_state != CatalogClaimState::ContractOnly {
        validator.error(
            CatalogValidationCode::UnsupportedClaimAuthority,
            "catalog_claim_state",
            "schema version 1 accepts only `contract_only`; no signed promotion-receipt validator exists",
        );
    }
}

fn validate_definitions(
    catalog: &ProductJourneyCatalog,
    validator: &mut ValidatorState,
) -> DefinitionIndex {
    let personas = validate_personas(&catalog.personas, validator);
    let fleet_points = validate_fleet_points(&catalog.fleet_points, validator);
    let (topologies, posture_target_modes) = validate_topologies(&catalog.topologies, validator);
    let (target_classes, target_modes) =
        validate_target_classes(&catalog.target_classes, validator);
    let actor_modes = validate_actor_modes(&catalog.actor_modes, &personas, validator);
    let gate_ids = validate_gates(&catalog.gates, validator);

    for mode in posture_target_modes {
        if !target_modes.contains(&mode) {
            validator.error(
                CatalogValidationCode::DanglingReference,
                "topologies[].target_posture",
                format!(
                    "topology posture references undefined target mode `{}`",
                    mode.as_str()
                ),
            );
        }
    }

    DefinitionIndex {
        personas,
        fleet_points,
        topologies,
        target_classes,
        actor_modes,
        gate_ids,
    }
}

fn validate_personas(
    definitions: &[PersonaDefinition],
    validator: &mut ValidatorState,
) -> BTreeSet<ProductPersona> {
    let mut seen = BTreeSet::new();
    for (index, definition) in definitions.iter().enumerate() {
        let path = format!("personas[{index}]");
        if !seen.insert(definition.persona) {
            validator.error(
                CatalogValidationCode::DuplicateId,
                format!("{path}.persona"),
                format!("duplicate persona `{}`", definition.persona.as_str()),
            );
        }
        validator.require_text(&format!("{path}.title"), &definition.title);
        validator.require_text_list(&format!("{path}.goals"), &definition.goals);
        validator.require_text_list(
            &format!("{path}.success_outcomes"),
            &definition.success_outcomes,
        );
        validator.require_repo_refs(
            &format!("{path}.source_refs"),
            &definition.source_refs,
            false,
        );
    }
    require_exact_enum_set(
        "personas",
        &seen,
        ProductPersona::ALL,
        ProductPersona::as_str,
        validator,
    );
    seen
}

fn validate_fleet_points(
    definitions: &[FleetPointDefinition],
    validator: &mut ValidatorState,
) -> BTreeSet<FleetPoint> {
    let mut seen = BTreeSet::new();
    for (index, definition) in definitions.iter().enumerate() {
        let path = format!("fleet_points[{index}]");
        if !seen.insert(definition.fleet_point) {
            validator.error(
                CatalogValidationCode::DuplicateId,
                format!("{path}.fleet_point"),
                format!(
                    "duplicate fleet point `{}`",
                    definition.fleet_point.as_str()
                ),
            );
        }
        if definition.pane_count != definition.fleet_point.pane_count() {
            validator.error(
                CatalogValidationCode::InvalidDefinition,
                format!("{path}.pane_count"),
                format!(
                    "`{}` requires pane_count {}, got {}",
                    definition.fleet_point.as_str(),
                    definition.fleet_point.pane_count(),
                    definition.pane_count
                ),
            );
        }
        validator.require_text(&format!("{path}.title"), &definition.title);
        validator.require_text_list(
            &format!("{path}.workload_characteristics"),
            &definition.workload_characteristics,
        );
        validator.require_repo_refs(
            &format!("{path}.source_refs"),
            &definition.source_refs,
            false,
        );
    }
    require_exact_enum_set(
        "fleet_points",
        &seen,
        FleetPoint::ALL,
        FleetPoint::as_str,
        validator,
    );
    seen
}

fn validate_topologies(
    definitions: &[TopologyDefinition],
    validator: &mut ValidatorState,
) -> (BTreeSet<Topology>, BTreeSet<TargetMode>) {
    let mut seen = BTreeSet::new();
    let mut posture_modes = BTreeSet::new();
    for (index, definition) in definitions.iter().enumerate() {
        let path = format!("topologies[{index}]");
        if !seen.insert(definition.topology) {
            validator.error(
                CatalogValidationCode::DuplicateId,
                format!("{path}.topology"),
                format!("duplicate topology `{}`", definition.topology.as_str()),
            );
        }
        validator.require_text(&format!("{path}.title"), &definition.title);
        validator.require_text(&format!("{path}.description"), &definition.description);
        if definition.target_posture.controller_modes.is_empty() {
            validator.error(
                CatalogValidationCode::EmptyRequiredField,
                format!("{path}.target_posture.controller_modes"),
                "controller target posture must not be empty",
            );
        }
        if definition.target_posture.session_host_modes.is_empty() {
            validator.error(
                CatalogValidationCode::EmptyRequiredField,
                format!("{path}.target_posture.session_host_modes"),
                "session-host target posture must not be empty",
            );
        }
        let controller_modes = definition
            .target_posture
            .controller_modes
            .iter()
            .copied()
            .collect::<BTreeSet<_>>();
        let session_host_modes = definition
            .target_posture
            .session_host_modes
            .iter()
            .copied()
            .collect::<BTreeSet<_>>();
        if controller_modes.len() != definition.target_posture.controller_modes.len() {
            validator.error(
                CatalogValidationCode::DuplicateId,
                format!("{path}.target_posture.controller_modes"),
                "controller target posture contains a duplicate mode",
            );
        }
        if session_host_modes.len() != definition.target_posture.session_host_modes.len() {
            validator.error(
                CatalogValidationCode::DuplicateId,
                format!("{path}.target_posture.session_host_modes"),
                "session-host target posture contains a duplicate mode",
            );
        }
        let expected_controllers = canonical_mac_modes().into_iter().collect::<BTreeSet<_>>();
        let expected_session_hosts = match definition.topology {
            Topology::LocalOnly => expected_controllers.clone(),
            Topology::MacLanRemote => [TargetMode::ThreadripperPro5995wxNative]
                .into_iter()
                .collect(),
        };
        if controller_modes != expected_controllers {
            validator.error(
                CatalogValidationCode::InvalidDefinition,
                format!("{path}.target_posture.controller_modes"),
                "v1 topology posture requires exactly M4 Pro, M5, and M5 Pro/Max controllers",
            );
        }
        if session_host_modes != expected_session_hosts {
            validator.error(
                CatalogValidationCode::InvalidDefinition,
                format!("{path}.target_posture.session_host_modes"),
                match definition.topology {
                    Topology::LocalOnly => {
                        "local_only requires exactly the three Mac modes as session hosts"
                    }
                    Topology::MacLanRemote => {
                        "mac_lan_remote requires only Threadripper PRO 5995WX as session host"
                    }
                },
            );
        }
        posture_modes.extend(controller_modes);
        posture_modes.extend(session_host_modes);
        validator.require_repo_refs(
            &format!("{path}.source_refs"),
            &definition.source_refs,
            false,
        );
    }
    require_exact_enum_set(
        "topologies",
        &seen,
        Topology::ALL,
        Topology::as_str,
        validator,
    );
    (seen, posture_modes)
}

fn validate_target_classes(
    definitions: &[TargetClassDefinition],
    validator: &mut ValidatorState,
) -> (BTreeMap<String, TargetClassIndex>, BTreeSet<TargetMode>) {
    let mut classes = BTreeMap::new();
    let mut modes = BTreeSet::new();
    for (index, definition) in definitions.iter().enumerate() {
        let path = format!("target_classes[{index}]");
        validator.require_text(
            &format!("{path}.target_class_id"),
            &definition.target_class_id,
        );
        if classes
            .insert(
                definition.target_class_id.clone(),
                TargetClassIndex {
                    mode: definition.target_mode,
                },
            )
            .is_some()
        {
            validator.error(
                CatalogValidationCode::DuplicateId,
                format!("{path}.target_class_id"),
                format!("duplicate target class id `{}`", definition.target_class_id),
            );
        }
        validator.require_text(&format!("{path}.title"), &definition.title);
        validator.require_text(&format!("{path}.platform"), &definition.platform);
        validator.require_text(
            &format!("{path}.hardware_identity"),
            &definition.hardware_identity,
        );
        match canonical_target_class_mode(&definition.target_class_id) {
            Some(expected_mode) if expected_mode != definition.target_mode => {
                validator.error(
                    CatalogValidationCode::InvalidDefinition,
                    format!("{path}.target_mode"),
                    format!(
                        "target class `{}` requires mode `{}`, got `{}`",
                        definition.target_class_id,
                        expected_mode.as_str(),
                        definition.target_mode.as_str()
                    ),
                );
            }
            None => {
                validator.error(
                    CatalogValidationCode::InvalidDefinition,
                    format!("{path}.target_class_id"),
                    format!(
                        "schema version 1 does not define target class `{}`",
                        definition.target_class_id
                    ),
                );
            }
            Some(_) => {}
        }
        if !modes.insert(definition.target_mode) {
            validator.error(
                CatalogValidationCode::DuplicateId,
                format!("{path}.target_mode"),
                format!(
                    "target mode `{}` is bound more than once",
                    definition.target_mode.as_str()
                ),
            );
        }
        validator.require_repo_refs(
            &format!("{path}.source_refs"),
            &definition.source_refs,
            false,
        );
    }
    require_exact_enum_set(
        "target_classes[].target_mode",
        &modes,
        TargetMode::ALL,
        TargetMode::as_str,
        validator,
    );
    for (target_class_id, _) in canonical_target_classes() {
        if !classes.contains_key(target_class_id) {
            validator.error(
                CatalogValidationCode::InvalidDefinition,
                "target_classes",
                format!("missing canonical target class `{target_class_id}`"),
            );
        }
    }
    if definitions.len() != TargetMode::ALL.len() {
        validator.error(
            CatalogValidationCode::InvalidDefinition,
            "target_classes",
            format!(
                "schema version 1 requires exactly {} target classes, found {}",
                TargetMode::ALL.len(),
                definitions.len()
            ),
        );
    }
    (classes, modes)
}

fn validate_actor_modes(
    definitions: &[ActorModeDefinition],
    personas: &BTreeSet<ProductPersona>,
    validator: &mut ValidatorState,
) -> BTreeMap<ActorMode, BTreeSet<ProductPersona>> {
    let mut modes = BTreeMap::new();
    for (index, definition) in definitions.iter().enumerate() {
        let path = format!("actor_modes[{index}]");
        if modes.contains_key(&definition.actor_mode) {
            validator.error(
                CatalogValidationCode::DuplicateId,
                format!("{path}.actor_mode"),
                format!("duplicate actor mode `{}`", definition.actor_mode.as_str()),
            );
        }
        validator.require_text(&format!("{path}.title"), &definition.title);
        validator.require_text(&format!("{path}.description"), &definition.description);
        if definition.personas.is_empty() {
            validator.error(
                CatalogValidationCode::EmptyRequiredField,
                format!("{path}.personas"),
                "actor mode must name at least one persona",
            );
        }
        let mut allowed = BTreeSet::new();
        for persona in &definition.personas {
            if !allowed.insert(*persona) {
                validator.error(
                    CatalogValidationCode::DuplicateId,
                    format!("{path}.personas"),
                    format!("duplicate persona `{}`", persona.as_str()),
                );
            }
            if !personas.contains(persona) {
                validator.error(
                    CatalogValidationCode::DanglingReference,
                    format!("{path}.personas"),
                    format!("undefined persona `{}`", persona.as_str()),
                );
            }
        }
        let expected_persona = canonical_persona_for_actor_mode(definition.actor_mode);
        if allowed != [expected_persona].into_iter().collect() {
            validator.error(
                CatalogValidationCode::InvalidDefinition,
                format!("{path}.personas"),
                format!(
                    "actor mode `{}` must map one-to-one to persona `{}`",
                    definition.actor_mode.as_str(),
                    expected_persona.as_str()
                ),
            );
        }
        modes.insert(definition.actor_mode, allowed);
        validator.require_repo_refs(
            &format!("{path}.source_refs"),
            &definition.source_refs,
            false,
        );
    }
    let seen = modes.keys().copied().collect::<BTreeSet<_>>();
    require_exact_enum_set(
        "actor_modes",
        &seen,
        ActorMode::ALL,
        ActorMode::as_str,
        validator,
    );
    modes
}

fn validate_gates(
    definitions: &[GateDefinition],
    validator: &mut ValidatorState,
) -> BTreeSet<String> {
    let mut ids = BTreeSet::new();
    for (index, definition) in definitions.iter().enumerate() {
        let path = format!("gates[{index}]");
        validator.require_text(&format!("{path}.gate_id"), &definition.gate_id);
        if !ids.insert(definition.gate_id.clone()) {
            validator.error(
                CatalogValidationCode::DuplicateId,
                format!("{path}.gate_id"),
                format!("duplicate gate id `{}`", definition.gate_id),
            );
        }
        validator.require_text(&format!("{path}.title"), &definition.title);
        validator.require_text(&format!("{path}.description"), &definition.description);
        if definition.release_requirement != ReleaseRequirement::Required {
            validator.error(
                CatalogValidationCode::InvalidDefinition,
                format!("{path}.release_requirement"),
                "every schema-v1 gate must be release-required",
            );
        }
        validator.require_bead_refs(
            &format!("{path}.producer_bead_ids"),
            &definition.producer_bead_ids,
            false,
        );
        if definition
            .producer_bead_ids
            .iter()
            .collect::<BTreeSet<_>>()
            .len()
            != definition.producer_bead_ids.len()
        {
            validator.error(
                CatalogValidationCode::DuplicateId,
                format!("{path}.producer_bead_ids"),
                "gate producer Bead identifiers must be unique",
            );
        }
        match canonical_gate_producer_beads(&definition.gate_id) {
            Some(expected) => {
                let actual = definition
                    .producer_bead_ids
                    .iter()
                    .map(String::as_str)
                    .collect::<BTreeSet<_>>();
                let expected = expected.iter().copied().collect::<BTreeSet<_>>();
                if actual != expected || actual.len() != definition.producer_bead_ids.len() {
                    validator.error(
                        CatalogValidationCode::InvalidDefinition,
                        format!("{path}.producer_bead_ids"),
                        "gate producers must equal the canonical schema-v1 Bead assignment without duplicates",
                    );
                }
            }
            None => validator.error(
                CatalogValidationCode::InvalidDefinition,
                format!("{path}.gate_id"),
                format!("unexpected schema-v1 gate `{}`", definition.gate_id),
            ),
        }
        validator.require_repo_refs(
            &format!("{path}.evidence_refs"),
            &definition.evidence_refs,
            true,
        );
        validator.require_repo_refs(
            &format!("{path}.source_refs"),
            &definition.source_refs,
            false,
        );
    }
    if definitions.is_empty() {
        validator.error(
            CatalogValidationCode::EmptyRequiredField,
            "gates",
            "catalog must define at least one gate",
        );
    }
    let expected = canonical_gate_ids().into_iter().collect::<BTreeSet<_>>();
    let actual = ids.iter().map(String::as_str).collect::<BTreeSet<_>>();
    if actual != expected {
        validator.error(
            CatalogValidationCode::InvalidDefinition,
            "gates",
            "schema-v1 gate identifiers must equal the canonical closed gate domain",
        );
    }
    ids
}

fn validate_journeys(
    catalog: &ProductJourneyCatalog,
    index: &DefinitionIndex,
    validator: &mut ValidatorState,
) {
    let mut journey_ids = BTreeSet::new();
    let mut field_bead_counts = BTreeMap::<String, usize>::new();

    for (journey_index, journey) in catalog.journey_definitions.iter().enumerate() {
        let path = format!("journey_definitions[{journey_index}]");
        validator.require_text(&format!("{path}.journey_id"), &journey.journey_id);
        if !journey_ids.insert(journey.journey_id.clone()) {
            validator.error(
                CatalogValidationCode::DuplicateId,
                format!("{path}.journey_id"),
                format!("duplicate journey id `{}`", journey.journey_id),
            );
        }
        validator.require_text(&format!("{path}.title"), &journey.title);
        validator.require_bead_refs(
            &format!("{path}.field_bead_id"),
            std::slice::from_ref(&journey.field_bead_id),
            false,
        );
        match canonical_journey_id_for_field_bead(&journey.field_bead_id) {
            Some(expected_journey_id) if journey.journey_id != expected_journey_id => {
                validator.error(
                    CatalogValidationCode::IncompleteFieldJourneyBinding,
                    format!("{path}.journey_id"),
                    format!(
                        "field Bead `{}` must map to canonical journey `{expected_journey_id}`",
                        journey.field_bead_id
                    ),
                );
            }
            None => {}
            Some(_) => {}
        }
        if journey.release_requirement != ReleaseRequirement::Required {
            validator.error(
                CatalogValidationCode::InvalidDefinition,
                format!("{path}.release_requirement"),
                "every schema-v1 journey must be release-required",
            );
        }
        *field_bead_counts
            .entry(journey.field_bead_id.clone())
            .or_default() += 1;

        if journey.personas.is_empty() {
            validator.error(
                CatalogValidationCode::EmptyRequiredField,
                format!("{path}.personas"),
                "journey must name at least one persona",
            );
        }
        for persona in &journey.personas {
            if !index.personas.contains(persona) {
                validator.error(
                    CatalogValidationCode::DanglingReference,
                    format!("{path}.personas"),
                    format!("undefined persona `{}`", persona.as_str()),
                );
            }
        }
        if let Some(expected_personas) = canonical_journey_personas(&journey.journey_id) {
            let actual = journey.personas.iter().copied().collect::<BTreeSet<_>>();
            let expected = expected_personas.iter().copied().collect::<BTreeSet<_>>();
            if actual != expected || actual.len() != journey.personas.len() {
                validator.error(
                    CatalogValidationCode::InvalidDefinition,
                    format!("{path}.personas"),
                    "journey personas must equal the canonical schema-v1 mapping without duplicates",
                );
            }
        }

        for (field, values) in [
            ("setup", &journey.setup),
            ("steady_work", &journey.steady_work),
            ("failure_overload", &journey.failure_overload),
            ("recovery", &journey.recovery),
            ("teardown", &journey.teardown),
        ] {
            if values.is_empty() || values.iter().any(|value| value.trim().is_empty()) {
                validator.error(
                    CatalogValidationCode::EmptyLifecyclePhase,
                    format!("{path}.{field}"),
                    format!("journey lifecycle phase `{field}` must contain non-empty steps"),
                );
            }
        }
        if journey.setup.len() < 2 {
            validator.error(
                CatalogValidationCode::EmptyLifecyclePhase,
                format!("{path}.setup"),
                "setup must retain identity/preflight at index zero and at least one later clean-setup step",
            );
        }
        validator.require_text_list(&format!("{path}.user_outcomes"), &journey.user_outcomes);
        validator.require_text_list(
            &format!("{path}.accessibility_expectations"),
            &journey.accessibility_expectations,
        );
        validator.require_text_list(
            &format!("{path}.privacy_expectations"),
            &journey.privacy_expectations,
        );
        validator.require_text_list(
            &format!("{path}.evidence_requirements"),
            &journey.evidence_requirements,
        );
        validator.require_repo_refs(&format!("{path}.source_refs"), &journey.source_refs, false);
        if journey.gate_ids.is_empty() {
            validator.error(
                CatalogValidationCode::EmptyRequiredField,
                format!("{path}.gate_ids"),
                "journey must reference at least one gate",
            );
        }
        for gate_id in &journey.gate_ids {
            if !index.gate_ids.contains(gate_id) {
                validator.error(
                    CatalogValidationCode::DanglingReference,
                    format!("{path}.gate_ids"),
                    format!("undefined gate `{gate_id}`"),
                );
            }
        }
        if let Some(expected_gates) = canonical_journey_gates(&journey.journey_id) {
            let actual = journey
                .gate_ids
                .iter()
                .map(String::as_str)
                .collect::<BTreeSet<_>>();
            let expected = expected_gates.iter().copied().collect::<BTreeSet<_>>();
            if actual != expected || actual.len() != journey.gate_ids.len() {
                validator.error(
                    CatalogValidationCode::InvalidDefinition,
                    format!("{path}.gate_ids"),
                    "journey gates must equal the canonical schema-v1 mapping without duplicates",
                );
            }
        }
    }

    for field_bead_id in required_field_bead_ids() {
        match field_bead_counts.get(&field_bead_id).copied().unwrap_or(0) {
            1 => {}
            count => validator.error(
                CatalogValidationCode::IncompleteFieldJourneyBinding,
                "journey_definitions[].field_bead_id",
                format!(
                    "required field Bead `{field_bead_id}` must be bound exactly once, found {count}"
                ),
            ),
        }
    }
    for (field_bead_id, count) in field_bead_counts {
        if !is_required_field_bead_id(&field_bead_id) {
            validator.error(
                CatalogValidationCode::IncompleteFieldJourneyBinding,
                "journey_definitions[].field_bead_id",
                format!("unexpected field-journey Bead `{field_bead_id}` occurs {count} time(s)"),
            );
        }
    }
    if catalog.journey_definitions.len() != REQUIRED_FIELD_JOURNEY_COUNT {
        validator.error(
            CatalogValidationCode::IncompleteFieldJourneyBinding,
            "journey_definitions",
            format!(
                "expected {REQUIRED_FIELD_JOURNEY_COUNT} journey definitions, found {}",
                catalog.journey_definitions.len()
            ),
        );
    }
}

fn validate_coverage(
    catalog: &ProductJourneyCatalog,
    index: &DefinitionIndex,
    validator: &mut ValidatorState,
) {
    let expected = expected_coverage_keys();
    let mut required_counts = BTreeMap::<CoverageKey, usize>::new();
    for coverage in &catalog.required_coverage {
        *required_counts.entry(*coverage).or_default() += 1;
        validate_coverage_dimensions(*coverage, index, "required_coverage", validator);
    }
    for (coverage, count) in &required_counts {
        if *count > 1 {
            validator.error(
                CatalogValidationCode::DuplicateCompositeKey,
                "required_coverage",
                format!("coverage key `{}` occurs {count} times", coverage.label()),
            );
        }
    }
    report_coverage_difference(
        "required_coverage",
        &expected,
        &required_counts.keys().copied().collect(),
        validator,
    );

    let journeys_by_id = catalog
        .journey_definitions
        .iter()
        .map(|journey| (journey.journey_id.clone(), journey))
        .collect::<BTreeMap<_, _>>();
    let journey_ids = journeys_by_id.keys().cloned().collect::<BTreeSet<_>>();
    let journey_field_beads = catalog
        .journey_definitions
        .iter()
        .map(|journey| (journey.journey_id.clone(), journey.field_bead_id.clone()))
        .collect::<BTreeMap<_, _>>();
    let mut referenced_journeys = BTreeSet::new();
    let mut variant_ids = BTreeSet::new();
    let mut claim_ids = BTreeSet::new();
    let mut qualification_ids = BTreeSet::new();
    let mut variant_counts = BTreeMap::<CoverageKey, usize>::new();

    for (variant_index, variant) in catalog.variants.iter().enumerate() {
        let path = format!("variants[{variant_index}]");
        validator.require_text(&format!("{path}.variant_id"), &variant.variant_id);
        if !variant_ids.insert(variant.variant_id.clone()) {
            validator.error(
                CatalogValidationCode::DuplicateId,
                format!("{path}.variant_id"),
                format!("duplicate variant id `{}`", variant.variant_id),
            );
        }
        validator.require_text(&format!("{path}.claim_id"), &variant.claim_id);
        if !claim_ids.insert(variant.claim_id.clone()) {
            validator.error(
                CatalogValidationCode::DuplicateClaimId,
                format!("{path}.claim_id"),
                format!("duplicate claim id `{}`", variant.claim_id),
            );
        }
        let expected_variant_id = canonical_variant_id(variant.coverage);
        if variant.variant_id != expected_variant_id {
            validator.error(
                CatalogValidationCode::InvalidDefinition,
                format!("{path}.variant_id"),
                format!("expected canonical variant id `{expected_variant_id}`"),
            );
        }
        let expected_claim_id = canonical_claim_id(variant.coverage);
        if variant.claim_id != expected_claim_id {
            validator.error(
                CatalogValidationCode::InvalidDefinition,
                format!("{path}.claim_id"),
                format!("expected canonical claim id `{expected_claim_id}`"),
            );
        }

        *variant_counts.entry(variant.coverage).or_default() += 1;
        validate_coverage_dimensions(
            variant.coverage,
            index,
            &format!("{path}.coverage"),
            validator,
        );
        if !required_counts.contains_key(&variant.coverage) {
            validator.error(
                CatalogValidationCode::DanglingReference,
                format!("{path}.coverage"),
                format!(
                    "variant coverage `{}` is not declared in required_coverage",
                    variant.coverage.label()
                ),
            );
        }

        if variant.journey_ids.is_empty() {
            validator.error(
                CatalogValidationCode::EmptyRequiredField,
                format!("{path}.journey_ids"),
                "variant must reference at least one journey",
            );
        }
        for journey_id in &variant.journey_ids {
            if !journey_ids.contains(journey_id) {
                validator.error(
                    CatalogValidationCode::DanglingReference,
                    format!("{path}.journey_ids"),
                    format!("undefined journey `{journey_id}`"),
                );
                continue;
            }
            referenced_journeys.insert(journey_id.clone());
            if let Some(journey) = journeys_by_id.get(journey_id)
                && !journey.personas.contains(&variant.coverage.persona)
            {
                validator.error(
                    CatalogValidationCode::ContradictoryClaim,
                    format!("{path}.journey_ids"),
                    format!(
                        "journey `{journey_id}` does not declare persona `{}`",
                        variant.coverage.persona.as_str()
                    ),
                );
            }
        }
        let actual_journeys = variant
            .journey_ids
            .iter()
            .map(String::as_str)
            .collect::<BTreeSet<_>>();
        let expected_journeys = canonical_variant_journeys(variant.coverage);
        if actual_journeys != expected_journeys
            || actual_journeys.len() != variant.journey_ids.len()
        {
            validator.error(
                CatalogValidationCode::InvalidDefinition,
                format!("{path}.journey_ids"),
                "variant journeys must equal the canonical persona/fleet/topology projection without duplicates",
            );
        }

        let expected_actor_mode = canonical_actor_mode(variant.coverage.persona);
        if variant.actor_mode != expected_actor_mode {
            validator.error(
                CatalogValidationCode::InvalidDefinition,
                format!("{path}.actor_mode"),
                format!(
                    "persona `{}` requires actor mode `{}`",
                    variant.coverage.persona.as_str(),
                    expected_actor_mode.as_str()
                ),
            );
        }
        match index.actor_modes.get(&variant.actor_mode) {
            None => validator.error(
                CatalogValidationCode::DanglingReference,
                format!("{path}.actor_mode"),
                format!("undefined actor mode `{}`", variant.actor_mode.as_str()),
            ),
            Some(personas) if !personas.contains(&variant.coverage.persona) => validator.error(
                CatalogValidationCode::ContradictoryClaim,
                format!("{path}.actor_mode"),
                format!(
                    "actor mode `{}` does not admit persona `{}`",
                    variant.actor_mode.as_str(),
                    variant.coverage.persona.as_str()
                ),
            ),
            Some(_) => {}
        }
        let expected_transport = Transport::for_topology(variant.coverage.topology);
        if variant.transport != expected_transport {
            validator.error(
                CatalogValidationCode::InvalidDefinition,
                format!("{path}.transport"),
                format!(
                    "topology `{}` requires transport `{}`",
                    variant.coverage.topology.as_str(),
                    expected_transport.as_str()
                ),
            );
        }
        if variant.release_requirement != ReleaseRequirement::Required {
            validator.error(
                CatalogValidationCode::InvalidDefinition,
                format!("{path}.release_requirement"),
                "every schema-v1 variant must be release-required",
            );
        }
        if variant.gate_ids.is_empty() {
            validator.error(
                CatalogValidationCode::EmptyRequiredField,
                format!("{path}.gate_ids"),
                "variant must reference at least one acceptance or evidence gate",
            );
        }
        let actual_gate_ids = variant.gate_ids.iter().cloned().collect::<BTreeSet<_>>();
        if actual_gate_ids.len() != variant.gate_ids.len() {
            validator.error(
                CatalogValidationCode::DuplicateId,
                format!("{path}.gate_ids"),
                "variant gate identifiers must be unique",
            );
        }
        let mut expected_gate_ids = BTreeSet::from(["gate.target_qualification".to_string()]);
        for journey_id in &variant.journey_ids {
            if let Some(journey) = journeys_by_id.get(journey_id) {
                expected_gate_ids.extend(journey.gate_ids.iter().cloned());
            }
        }
        if actual_gate_ids != expected_gate_ids {
            validator.error(
                CatalogValidationCode::InvalidDefinition,
                format!("{path}.gate_ids"),
                "variant gates must equal the union of its journey gates plus `gate.target_qualification`",
            );
        }
        for gate_id in &variant.gate_ids {
            if !index.gate_ids.contains(gate_id) {
                validator.error(
                    CatalogValidationCode::DanglingReference,
                    format!("{path}.gate_ids"),
                    format!("undefined gate `{gate_id}`"),
                );
            }
        }
        validator.require_repo_refs(&format!("{path}.source_refs"), &variant.source_refs, false);
        validate_producer_bindings(variant, index, &journey_field_beads, &path, validator);
        validate_target_qualifications(variant, index, &path, &mut qualification_ids, validator);
        validate_support_and_proof(catalog, variant, &path, validator);
    }

    for (coverage, count) in &variant_counts {
        if *count > 1 {
            validator.error(
                CatalogValidationCode::DuplicateCompositeKey,
                "variants[].coverage",
                format!(
                    "variant composite key `{}` occurs {count} times",
                    coverage.label()
                ),
            );
        }
    }
    report_coverage_difference(
        "variants[].coverage",
        &expected,
        &variant_counts.keys().copied().collect(),
        validator,
    );
    if catalog.variants.len() != REQUIRED_COVERAGE_CELL_COUNT {
        validator.error(
            CatalogValidationCode::MissingRequiredCoverage,
            "variants",
            format!(
                "expected {REQUIRED_COVERAGE_CELL_COUNT} variants, found {}",
                catalog.variants.len()
            ),
        );
    }

    for journey_id in journey_ids.difference(&referenced_journeys) {
        validator.error(
            CatalogValidationCode::DanglingReference,
            "journey_definitions",
            format!("journey `{journey_id}` is not exercised by any variant"),
        );
    }
}

fn validate_producer_bindings(
    variant: &JourneyVariant,
    index: &DefinitionIndex,
    journey_field_beads: &BTreeMap<String, String>,
    path: &str,
    validator: &mut ValidatorState,
) {
    let expected_field_beads = variant
        .journey_ids
        .iter()
        .filter_map(|journey_id| journey_field_beads.get(journey_id))
        .cloned()
        .collect::<BTreeSet<_>>();
    let mut exact_producer_ids = BTreeSet::new();
    for (binding_index, binding) in variant.exact_producer_bindings.iter().enumerate() {
        let binding_path = format!("{path}.exact_producer_bindings[{binding_index}]");
        validator.require_bead_refs(
            &format!("{binding_path}.producer_bead_id"),
            std::slice::from_ref(&binding.producer_bead_id),
            false,
        );
        if !exact_producer_ids.insert(binding.producer_bead_id.clone()) {
            validator.error(
                CatalogValidationCode::DuplicateId,
                format!("{binding_path}.producer_bead_id"),
                format!(
                    "duplicate exact producer Bead `{}`",
                    binding.producer_bead_id
                ),
            );
        }
        if !expected_field_beads.contains(&binding.producer_bead_id) {
            validator.error(
                CatalogValidationCode::InvalidProducerCoverage,
                format!("{binding_path}.producer_bead_id"),
                "an exact producer must be one of the owning variant's mapped field-journey Beads",
            );
        }
        validator.require_repo_refs(
            &format!("{binding_path}.source_refs"),
            &binding.source_refs,
            false,
        );
        if binding.coverage != variant.coverage {
            validator.error(
                CatalogValidationCode::InvalidProducerCoverage,
                format!("{binding_path}.coverage"),
                "exact producer coverage must equal the owning variant coverage",
            );
        }
        if binding.actor_mode != variant.actor_mode {
            validator.error(
                CatalogValidationCode::InvalidProducerCoverage,
                format!("{binding_path}.actor_mode"),
                "exact producer actor mode must equal the owning variant actor mode",
            );
        }
        if binding.transport != variant.transport {
            validator.error(
                CatalogValidationCode::InvalidProducerCoverage,
                format!("{binding_path}.transport"),
                "exact producer transport must equal the owning variant transport",
            );
        }
        validate_producer_target_roles(
            &binding.controller_target_class_ids,
            TargetRole::Controller,
            variant.coverage.topology,
            index,
            &format!("{binding_path}.controller_target_class_ids"),
            validator,
        );
        validate_producer_target_roles(
            &binding.session_host_target_class_ids,
            TargetRole::SessionHost,
            variant.coverage.topology,
            index,
            &format!("{binding_path}.session_host_target_class_ids"),
            validator,
        );
        validate_exact_producer_target_sets(
            binding,
            variant.coverage.topology,
            &binding_path,
            validator,
        );
    }

    validator.require_bead_refs(
        &format!("{path}.partial_producer_bead_ids"),
        &variant.partial_producer_bead_ids,
        true,
    );
    let partial_count = variant
        .partial_producer_bead_ids
        .iter()
        .collect::<BTreeSet<_>>()
        .len();
    if partial_count != variant.partial_producer_bead_ids.len() {
        validator.error(
            CatalogValidationCode::DuplicateId,
            format!("{path}.partial_producer_bead_ids"),
            "partial producer Bead identifiers must be unique",
        );
    }
    let partial_producer_ids = variant
        .partial_producer_bead_ids
        .iter()
        .cloned()
        .collect::<BTreeSet<_>>();
    let canonical_coverage = canonical_producer_coverage(variant.coverage);
    if variant.producer_coverage != canonical_coverage {
        validator.error(
            CatalogValidationCode::InvalidProducerCoverage,
            format!("{path}.producer_coverage"),
            format!(
                "schema-v1 coverage `{}` requires producer coverage `{canonical_coverage:?}`",
                variant.coverage.label()
            ),
        );
    }

    match variant.producer_coverage {
        ProducerCoverage::Direct => {
            let expected = canonical_direct_producer_bead(variant.coverage)
                .into_iter()
                .collect::<BTreeSet<_>>();
            let actual = exact_producer_ids
                .iter()
                .map(String::as_str)
                .collect::<BTreeSet<_>>();
            if actual != expected
                || actual.len() != variant.exact_producer_bindings.len()
                || !variant.partial_producer_bead_ids.is_empty()
            {
                validator.error(
                    CatalogValidationCode::InvalidProducerCoverage,
                    format!("{path}.producer_coverage"),
                    "direct coverage requires exactly the canonical complete-cell producer binding and no partial producers",
                );
            }
        }
        ProducerCoverage::Partial
            if !variant.exact_producer_bindings.is_empty()
                || variant.partial_producer_bead_ids.is_empty()
                || partial_producer_ids != expected_field_beads =>
        {
            validator.error(
                CatalogValidationCode::InvalidProducerCoverage,
                format!("{path}.producer_coverage"),
                "partial coverage requires no exact bindings and exactly the field-journey Beads mapped by the owning variant",
            );
        }
        ProducerCoverage::Gap
            if !variant.exact_producer_bindings.is_empty()
                || !variant.partial_producer_bead_ids.is_empty() =>
        {
            validator.error(
                CatalogValidationCode::InvalidProducerCoverage,
                format!("{path}.producer_coverage"),
                "gap coverage requires both exact and partial producer bindings to be empty",
            );
        }
        ProducerCoverage::Partial | ProducerCoverage::Gap => {}
    }
}

#[derive(Debug, Clone, Copy)]
enum TargetRole {
    Controller,
    SessionHost,
}

fn validate_producer_target_roles(
    target_class_ids: &[String],
    role: TargetRole,
    topology: Topology,
    index: &DefinitionIndex,
    path: &str,
    validator: &mut ValidatorState,
) {
    if target_class_ids.is_empty() {
        validator.error(
            CatalogValidationCode::EmptyRequiredField,
            path,
            "exact producer target-role binding must not be empty",
        );
        return;
    }
    let mut seen = BTreeSet::new();
    for target_class_id in target_class_ids {
        if !seen.insert(target_class_id) {
            validator.error(
                CatalogValidationCode::DuplicateId,
                path,
                format!("duplicate target class `{target_class_id}`"),
            );
        }
        let Some(target) = index.target_classes.get(target_class_id) else {
            validator.error(
                CatalogValidationCode::DanglingReference,
                path,
                format!("undefined target class `{target_class_id}`"),
            );
            continue;
        };
        if !target_mode_allowed_for_role(target.mode, topology, role) {
            validator.error(
                CatalogValidationCode::InvalidProducerCoverage,
                path,
                format!(
                    "target class `{target_class_id}` is incompatible with the {} role for topology `{}`",
                    match role {
                        TargetRole::Controller => "controller",
                        TargetRole::SessionHost => "session-host",
                    },
                    topology.as_str()
                ),
            );
        }
    }
}

fn validate_exact_producer_target_sets(
    binding: &ExactProducerBinding,
    topology: Topology,
    path: &str,
    validator: &mut ValidatorState,
) {
    let expected_controllers = ["mac16_11_m4_pro", "m5_native", "m5_pro_max_native"]
        .into_iter()
        .collect::<BTreeSet<_>>();
    let expected_session_hosts = match topology {
        Topology::LocalOnly => ["mac16_11_m4_pro", "m5_native", "m5_pro_max_native"]
            .into_iter()
            .collect::<BTreeSet<_>>(),
        Topology::MacLanRemote => ["trj_5995wx"].into_iter().collect::<BTreeSet<_>>(),
    };
    let controllers = binding
        .controller_target_class_ids
        .iter()
        .map(String::as_str)
        .collect::<BTreeSet<_>>();
    let session_hosts = binding
        .session_host_target_class_ids
        .iter()
        .map(String::as_str)
        .collect::<BTreeSet<_>>();
    if controllers != expected_controllers || session_hosts != expected_session_hosts {
        validator.error(
            CatalogValidationCode::InvalidProducerCoverage,
            format!("{path}.controller_target_class_ids"),
            "an exact producer must bind the complete canonical target-role sets for its topology",
        );
    }
}

fn validate_target_qualifications(
    variant: &JourneyVariant,
    index: &DefinitionIndex,
    path: &str,
    qualification_ids: &mut BTreeSet<String>,
    validator: &mut ValidatorState,
) {
    if variant.target_qualifications.len() != 3 {
        validator.error(
            CatalogValidationCode::InvalidTargetQualification,
            format!("{path}.target_qualifications"),
            format!(
                "every schema-v1 variant requires exactly three target qualifications, found {}",
                variant.target_qualifications.len()
            ),
        );
    }

    let expected_pairs = canonical_target_pairs(variant.coverage.topology);
    let mut actual_pairs = BTreeSet::new();
    for (qualification_index, qualification) in variant.target_qualifications.iter().enumerate() {
        let qualification_path = format!("{path}.target_qualifications[{qualification_index}]");
        validator.require_text(
            &format!("{qualification_path}.qualification_id"),
            &qualification.qualification_id,
        );
        let expected_qualification_id = canonical_qualification_id(
            variant.coverage,
            &qualification.controller_target_class_id,
            &qualification.session_host_target_class_id,
        );
        if qualification.qualification_id != expected_qualification_id {
            validator.error(
                CatalogValidationCode::InvalidTargetQualification,
                format!("{qualification_path}.qualification_id"),
                format!("expected canonical qualification id `{expected_qualification_id}`"),
            );
        }
        if !qualification_ids.insert(qualification.qualification_id.clone()) {
            validator.error(
                CatalogValidationCode::DuplicateId,
                format!("{qualification_path}.qualification_id"),
                format!(
                    "duplicate qualification id `{}`",
                    qualification.qualification_id
                ),
            );
        }
        if !actual_pairs.insert((
            qualification.controller_target_class_id.as_str(),
            qualification.session_host_target_class_id.as_str(),
        )) {
            validator.error(
                CatalogValidationCode::InvalidTargetQualification,
                qualification_path.clone(),
                "duplicate controller/session-host qualification pair",
            );
        }
        if qualification.transport != variant.transport {
            validator.error(
                CatalogValidationCode::InvalidTargetQualification,
                format!("{qualification_path}.transport"),
                "qualification transport must equal the owning variant transport",
            );
        }
        validator.require_optional_repo_ref(
            &format!("{qualification_path}.route_identity_ref"),
            qualification.route_identity_ref.as_deref(),
        );
        validator.require_optional_repo_ref(
            &format!("{qualification_path}.candidate_identity_ref"),
            qualification.candidate_identity_ref.as_deref(),
        );
        validator.require_repo_refs(
            &format!("{qualification_path}.evidence_refs"),
            &qualification.evidence_refs,
            true,
        );
        validator.require_catalog_refs(
            &format!("{qualification_path}.blocker_refs"),
            &qualification.blocker_refs,
            true,
        );

        let controller = index
            .target_classes
            .get(&qualification.controller_target_class_id);
        let session_host = index
            .target_classes
            .get(&qualification.session_host_target_class_id);
        if controller.is_none() {
            validator.error(
                CatalogValidationCode::DanglingReference,
                format!("{qualification_path}.controller_target_class_id"),
                format!(
                    "undefined target class `{}`",
                    qualification.controller_target_class_id
                ),
            );
        }
        if session_host.is_none() {
            validator.error(
                CatalogValidationCode::DanglingReference,
                format!("{qualification_path}.session_host_target_class_id"),
                format!(
                    "undefined target class `{}`",
                    qualification.session_host_target_class_id
                ),
            );
        }
        if let Some(controller) = controller
            && !target_mode_allowed_for_role(
                controller.mode,
                variant.coverage.topology,
                TargetRole::Controller,
            )
        {
            validator.error(
                CatalogValidationCode::InvalidTargetQualification,
                format!("{qualification_path}.controller_target_class_id"),
                "controller target class violates the canonical topology posture",
            );
        }
        if let Some(session_host) = session_host
            && !target_mode_allowed_for_role(
                session_host.mode,
                variant.coverage.topology,
                TargetRole::SessionHost,
            )
        {
            validator.error(
                CatalogValidationCode::InvalidTargetQualification,
                format!("{qualification_path}.session_host_target_class_id"),
                "session-host target class violates the canonical topology posture",
            );
        }
        let uses_transitional_m5_pro_max = controller
            .is_some_and(|definition| definition.mode == TargetMode::M5ProMaxNative)
            || session_host
                .is_some_and(|definition| definition.mode == TargetMode::M5ProMaxNative);
        if uses_transitional_m5_pro_max
            && (qualification.availability != TargetAvailability::Unknown
                || qualification.evidence_state != EvidenceState::Missing
                || qualification.run_verdict != RunVerdict::NotRun
                || qualification.freshness_state != FreshnessState::Unknown
                || qualification.route_identity_ref.is_some()
                || qualification.candidate_identity_ref.is_some()
                || !qualification.evidence_refs.is_empty()
                || !qualification.blocker_refs.is_empty())
        {
            validator.error(
                CatalogValidationCode::InvalidTargetQualification,
                qualification_path.clone(),
                "schema-v1 m5_pro_max_native is a transitional planning lane permanently frozen at unknown/missing/not_run/unknown with null identities and empty evidence/blocker references; separate M5 Pro and M5 Max lanes are required for authority",
            );
        }
        validate_qualification_evidence(qualification, &qualification_path, validator);
    }

    if actual_pairs != expected_pairs {
        validator.error(
            CatalogValidationCode::InvalidTargetQualification,
            format!("{path}.target_qualifications"),
            match variant.coverage.topology {
                Topology::LocalOnly => {
                    "local_only requires the three same-ID Mac controller/session-host pairs"
                }
                Topology::MacLanRemote => {
                    "mac_lan_remote requires each Mac controller paired with trj_5995wx"
                }
            },
        );
    }
}

fn validate_qualification_evidence(
    qualification: &TargetQualification,
    path: &str,
    validator: &mut ValidatorState,
) {
    let executed_run = matches!(
        qualification.run_verdict,
        RunVerdict::Pass | RunVerdict::Fail | RunVerdict::Degraded
    );
    let identity_bound_evidence = executed_run
        || !qualification.evidence_refs.is_empty()
        || matches!(
            qualification.evidence_state,
            EvidenceState::Proven | EvidenceState::ProxyOnly | EvidenceState::FixtureOnly
        )
        || matches!(
            qualification.freshness_state,
            FreshnessState::Current | FreshnessState::Stale
        );
    if identity_bound_evidence
        && (qualification.route_identity_ref.is_none()
            || qualification.candidate_identity_ref.is_none())
    {
        validator.error(
            CatalogValidationCode::InvalidTargetQualification,
            path,
            "executed, retained, current, stale, proven, proxy, or fixture evidence requires both candidate and route identity references",
        );
    }
    if (qualification.route_identity_ref.is_none()
        || qualification.candidate_identity_ref.is_none())
        && qualification.freshness_state != FreshnessState::Unknown
    {
        validator.error(
            CatalogValidationCode::InvalidTargetQualification,
            format!("{path}.freshness_state"),
            "a null candidate or route identity forces unknown freshness",
        );
    }
    if qualification.run_verdict == RunVerdict::TargetUnavailable
        && qualification.availability != TargetAvailability::Unavailable
    {
        validator.error(
            CatalogValidationCode::InvalidTargetQualification,
            format!("{path}.run_verdict"),
            "target_unavailable is valid only when this exact qualification lane is unavailable",
        );
    }
    if executed_run && qualification.availability != TargetAvailability::Available {
        validator.error(
            CatalogValidationCode::InvalidTargetQualification,
            format!("{path}.run_verdict"),
            "pass, fail, or degraded requires this exact qualification lane to be available",
        );
    }
    if matches!(
        qualification.evidence_state,
        EvidenceState::Proven | EvidenceState::ProxyOnly | EvidenceState::FixtureOnly
    ) && qualification.availability != TargetAvailability::Available
    {
        validator.error(
            CatalogValidationCode::InvalidTargetQualification,
            format!("{path}.evidence_state"),
            "proven, proxy, or fixture evidence requires this exact qualification lane to be available",
        );
    }
    if matches!(
        qualification.freshness_state,
        FreshnessState::Current | FreshnessState::Stale
    ) && qualification.evidence_refs.is_empty()
    {
        validator.error(
            CatalogValidationCode::InvalidTargetQualification,
            format!("{path}.freshness_state"),
            "current or stale freshness requires retained evidence references",
        );
    }

    match qualification.evidence_state {
        EvidenceState::Proven => {
            if qualification.evidence_refs.is_empty()
                || !qualification.blocker_refs.is_empty()
                || !executed_run
                || qualification.freshness_state == FreshnessState::Unknown
            {
                validator.error(
                    CatalogValidationCode::InvalidTargetQualification,
                    format!("{path}.evidence_state"),
                    "proven requires retained evidence, an executed run, known freshness, and no blocker references",
                );
            }
        }
        EvidenceState::ProxyOnly | EvidenceState::FixtureOnly => {
            if qualification.evidence_refs.is_empty()
                || !qualification.blocker_refs.is_empty()
                || !executed_run
                || qualification.freshness_state == FreshnessState::Unknown
            {
                validator.error(
                    CatalogValidationCode::InvalidTargetQualification,
                    format!("{path}.evidence_state"),
                    "proxy or fixture evidence requires retained evidence, an executed run, known freshness, and no blocker references",
                );
            }
        }
        EvidenceState::SkippedNotProven => {
            if !qualification.evidence_refs.is_empty()
                || qualification.blocker_refs.is_empty()
                || executed_run
                || qualification.freshness_state != FreshnessState::Unknown
            {
                validator.error(
                    CatalogValidationCode::InvalidTargetQualification,
                    format!("{path}.evidence_state"),
                    "skipped qualification uses blocker references, no evidence references, no executed verdict, and unknown freshness",
                );
            }
        }
        EvidenceState::Blocked => {
            if !qualification.evidence_refs.is_empty()
                || qualification.blocker_refs.is_empty()
                || qualification.run_verdict == RunVerdict::Pass
                || qualification.freshness_state != FreshnessState::Unknown
            {
                validator.error(
                    CatalogValidationCode::InvalidTargetQualification,
                    format!("{path}.evidence_state"),
                    "blocked qualification uses blocker references, no evidence references, no passing verdict, and unknown freshness",
                );
            }
        }
        EvidenceState::Missing => {
            if !qualification.evidence_refs.is_empty()
                || !matches!(
                    qualification.run_verdict,
                    RunVerdict::NotRun | RunVerdict::TargetUnavailable
                )
                || qualification.freshness_state != FreshnessState::Unknown
                || (qualification.availability == TargetAvailability::Available
                    && !qualification.blocker_refs.is_empty())
            {
                validator.error(
                    CatalogValidationCode::InvalidTargetQualification,
                    format!("{path}.evidence_state"),
                    "missing evidence requires no evidence, not_run, unknown freshness, and no blocker on an available lane",
                );
            }
        }
    }
}

fn validate_support_and_proof(
    catalog: &ProductJourneyCatalog,
    variant: &JourneyVariant,
    path: &str,
    validator: &mut ValidatorState,
) {
    match &variant.support {
        SupportDeclaration::Supported {
            promotion_receipt_ref,
            promotion_receipt_sha256,
        } => {
            validator.require_optional_repo_ref(
                &format!("{path}.support.promotion_receipt_ref"),
                Some(promotion_receipt_ref),
            );
            validator.require_hex_digest(
                CatalogValidationCode::UnsupportedClaimAuthority,
                &format!("{path}.support.promotion_receipt_sha256"),
                promotion_receipt_sha256,
                64,
                "promotion receipt SHA-256",
            );
            if catalog.catalog_claim_state == CatalogClaimState::ContractOnly {
                validator.error(
                    CatalogValidationCode::ContractOnlySupportedClaim,
                    format!("{path}.support"),
                    "a contract-only catalog cannot declare a supported variant",
                );
            }
            validator.error(
                CatalogValidationCode::UnsupportedClaimAuthority,
                format!("{path}.support"),
                "schema version 1 has no signed promotion-receipt validator and rejects every supported variant",
            );
            if variant.producer_coverage != ProducerCoverage::Direct {
                validator.error(
                    CatalogValidationCode::InvalidProducerCoverage,
                    format!("{path}.producer_coverage"),
                    "supported requires direct producer coverage",
                );
            }
            if variant.target_qualifications.len() != 3
                || variant.target_qualifications.iter().any(|qualification| {
                    qualification.availability != TargetAvailability::Available
                        || qualification.freshness_state != FreshnessState::Current
                        || qualification.evidence_state != EvidenceState::Proven
                        || qualification.run_verdict != RunVerdict::Pass
                        || qualification.candidate_identity_ref.is_none()
                        || qualification.route_identity_ref.is_none()
                })
            {
                validator.error(
                    CatalogValidationCode::ContradictoryClaim,
                    format!("{path}.target_qualifications"),
                    "supported requires all three canonical qualifications to be available, current, proven, passing, and bound to candidate and route identity",
                );
            }
            for contradiction in &catalog.contradictions {
                let applies = contradiction.blocks_all_claims
                    || contradiction
                        .affected_claim_ids
                        .iter()
                        .any(|claim_id| claim_id == &variant.claim_id);
                if contradiction.status == ContradictionStatus::Open && applies {
                    validator.error(
                        CatalogValidationCode::InvalidContradiction,
                        format!("{path}.support"),
                        format!(
                            "open contradiction `{}` blocks claim `{}`",
                            contradiction.contradiction_id, variant.claim_id
                        ),
                    );
                }
            }
        }
        SupportDeclaration::Conditional {
            reason,
            constraints,
            fallback,
            tracking_bead_ids,
        } => {
            validator.require_text(&format!("{path}.support.reason"), reason);
            validator.require_text_list(&format!("{path}.support.constraints"), constraints);
            validator.require_text(&format!("{path}.support.fallback"), fallback);
            validator.require_bead_refs(
                &format!("{path}.support.tracking_bead_ids"),
                tracking_bead_ids,
                false,
            );
        }
        SupportDeclaration::Unavailable {
            reason,
            fallback,
            tracking_bead_ids,
        } => {
            validator.require_text(&format!("{path}.support.reason"), reason);
            validator.require_text(&format!("{path}.support.fallback"), fallback);
            validator.require_bead_refs(
                &format!("{path}.support.tracking_bead_ids"),
                tracking_bead_ids,
                false,
            );
        }
    }
}

fn validate_mappings_and_history(
    catalog: &ProductJourneyCatalog,
    _index: &DefinitionIndex,
    validator: &mut ValidatorState,
) {
    let journey_ids = catalog
        .journey_definitions
        .iter()
        .map(|journey| journey.journey_id.clone())
        .collect::<BTreeSet<_>>();

    let mut legacy_ids = BTreeSet::new();
    for (index, mapping) in catalog.legacy_mappings.iter().enumerate() {
        let path = format!("legacy_mappings[{index}]");
        validator.require_text(&format!("{path}.legacy_id"), &mapping.legacy_id);
        if !legacy_ids.insert(mapping.legacy_id.clone()) {
            validator.error(
                CatalogValidationCode::DuplicateId,
                format!("{path}.legacy_id"),
                format!("duplicate legacy mapping id `{}`", mapping.legacy_id),
            );
        }
        validate_mapping_journeys(&path, &mapping.journey_ids, &journey_ids, validator);
        match canonical_legacy_mapping_journeys(&mapping.legacy_id) {
            Some(expected) => {
                validate_canonical_mapping(&path, &mapping.journey_ids, expected, validator);
            }
            None => validator.error(
                CatalogValidationCode::InvalidDefinition,
                format!("{path}.legacy_id"),
                format!(
                    "unexpected schema-v1 legacy mapping `{}`",
                    mapping.legacy_id
                ),
            ),
        }
        validator.require_repo_refs(&format!("{path}.source_refs"), &mapping.source_refs, false);
        if let Some(expected_refs) = canonical_legacy_mapping_source_refs(&mapping.legacy_id) {
            let actual = mapping
                .source_refs
                .iter()
                .map(String::as_str)
                .collect::<BTreeSet<_>>();
            let expected = expected_refs.iter().copied().collect::<BTreeSet<_>>();
            if actual != expected || actual.len() != mapping.source_refs.len() {
                validator.error(
                    CatalogValidationCode::InvalidDefinition,
                    format!("{path}.source_refs"),
                    "legacy mapping sources must equal the canonical schema-v1 references without duplicates",
                );
            }
        }
        validator.require_text(&format!("{path}.notes"), &mapping.notes);
    }
    if catalog.legacy_mappings.is_empty() {
        validator.error(
            CatalogValidationCode::EmptyRequiredField,
            "legacy_mappings",
            "catalog must map existing legacy scenarios",
        );
    }
    let expected_legacy_ids = canonical_legacy_mapping_ids()
        .into_iter()
        .collect::<BTreeSet<_>>();
    let actual_legacy_ids = legacy_ids
        .iter()
        .map(String::as_str)
        .collect::<BTreeSet<_>>();
    if actual_legacy_ids != expected_legacy_ids {
        validator.error(
            CatalogValidationCode::InvalidDefinition,
            "legacy_mappings",
            "schema-v1 legacy mapping identifiers must equal the canonical closed domain",
        );
    }

    let mut readme_ids = BTreeSet::new();
    for (index, mapping) in catalog.readme_mappings.iter().enumerate() {
        let path = format!("readme_mappings[{index}]");
        validator.require_text(&format!("{path}.mapping_id"), &mapping.mapping_id);
        if !readme_ids.insert(mapping.mapping_id.clone()) {
            validator.error(
                CatalogValidationCode::DuplicateId,
                format!("{path}.mapping_id"),
                format!("duplicate README mapping id `{}`", mapping.mapping_id),
            );
        }
        if let Err(detail) = validate_repository_reference(&mapping.readme_ref) {
            validator.error(
                CatalogValidationCode::MalformedReference,
                format!("{path}.readme_ref"),
                detail,
            );
        } else if !mapping.readme_ref.starts_with("README.md") {
            validator.error(
                CatalogValidationCode::InvalidDefinition,
                format!("{path}.readme_ref"),
                "README mapping must reference README.md",
            );
        }
        validator.require_text(&format!("{path}.claim_text"), &mapping.claim_text);
        validator.require_hex_digest(
            CatalogValidationCode::InvalidDefinition,
            &format!("{path}.claim_sha256"),
            &mapping.claim_sha256,
            64,
            "README claim SHA-256",
        );
        validate_mapping_journeys(&path, &mapping.journey_ids, &journey_ids, validator);
        match canonical_readme_mapping(&mapping.mapping_id) {
            Some((expected_ref, expected_journeys)) => {
                if mapping.readme_ref != expected_ref {
                    validator.error(
                        CatalogValidationCode::InvalidDefinition,
                        format!("{path}.readme_ref"),
                        format!("expected canonical README reference `{expected_ref}`"),
                    );
                }
                validate_canonical_mapping(
                    &path,
                    &mapping.journey_ids,
                    expected_journeys,
                    validator,
                );
            }
            None => validator.error(
                CatalogValidationCode::InvalidDefinition,
                format!("{path}.mapping_id"),
                format!(
                    "unexpected schema-v1 README mapping `{}`",
                    mapping.mapping_id
                ),
            ),
        }
        validator.require_text(&format!("{path}.notes"), &mapping.notes);
    }
    if catalog.readme_mappings.is_empty() {
        validator.error(
            CatalogValidationCode::EmptyRequiredField,
            "readme_mappings",
            "catalog must map README product promises",
        );
    }
    let expected_readme_ids = canonical_readme_mapping_ids()
        .into_iter()
        .collect::<BTreeSet<_>>();
    let actual_readme_ids = readme_ids
        .iter()
        .map(String::as_str)
        .collect::<BTreeSet<_>>();
    if actual_readme_ids != expected_readme_ids {
        validator.error(
            CatalogValidationCode::InvalidDefinition,
            "readme_mappings",
            "schema-v1 README mapping identifiers must equal the canonical closed domain",
        );
    }

    let claim_ids = catalog
        .variants
        .iter()
        .map(|variant| variant.claim_id.as_str())
        .collect::<BTreeSet<_>>();
    let mut contradiction_ids = BTreeSet::new();
    for (index, record) in catalog.contradictions.iter().enumerate() {
        let path = format!("contradictions[{index}]");
        validator.require_text(
            &format!("{path}.contradiction_id"),
            &record.contradiction_id,
        );
        if !contradiction_ids.insert(record.contradiction_id.as_str()) {
            validator.error(
                CatalogValidationCode::DuplicateId,
                format!("{path}.contradiction_id"),
                format!("duplicate contradiction id `{}`", record.contradiction_id),
            );
        }
        validator.require_text(&format!("{path}.title"), &record.title);
        validator.require_text(&format!("{path}.notes"), &record.notes);
        validator.require_bead_refs(
            &format!("{path}.tracking_bead_ids"),
            &record.tracking_bead_ids,
            false,
        );
        validator.require_repo_refs(&format!("{path}.source_refs"), &record.source_refs, false);
        validator.require_repo_refs(
            &format!("{path}.resolution_refs"),
            &record.resolution_refs,
            true,
        );
        let mut affected = BTreeSet::new();
        for claim_id in &record.affected_claim_ids {
            validator.require_text(&format!("{path}.affected_claim_ids"), claim_id);
            if !affected.insert(claim_id.clone()) {
                validator.error(
                    CatalogValidationCode::DuplicateClaimId,
                    format!("{path}.affected_claim_ids"),
                    format!("duplicate affected claim id `{claim_id}`"),
                );
            }
            if !claim_ids.contains(claim_id.as_str()) {
                validator.error(
                    CatalogValidationCode::DanglingReference,
                    format!("{path}.affected_claim_ids"),
                    format!("undefined claim `{claim_id}`"),
                );
            }
        }
        if record.blocks_all_claims != record.affected_claim_ids.is_empty() {
            validator.error(
                CatalogValidationCode::InvalidContradiction,
                format!("{path}.blocks_all_claims"),
                "blocks_all_claims must be true if and only if affected_claim_ids is empty",
            );
        }
        if let Some((expected_global, expected_affected)) =
            canonical_contradiction_scope(&record.contradiction_id, catalog)
            && (record.blocks_all_claims != expected_global || affected != expected_affected)
        {
            validator.error(
                CatalogValidationCode::InvalidContradiction,
                format!("{path}.affected_claim_ids"),
                "contradiction scope must equal the canonical schema-v1 claim projection",
            );
        }
        match record.status {
            ContradictionStatus::Open if !record.resolution_refs.is_empty() => {
                validator.error(
                    CatalogValidationCode::InvalidContradiction,
                    format!("{path}.resolution_refs"),
                    "an open contradiction cannot carry resolution references",
                );
            }
            ContradictionStatus::Resolved => {
                validator.error(
                    CatalogValidationCode::InvalidContradiction,
                    format!("{path}.status"),
                    "schema version 1 has no signed contradiction-resolution receipt validator and rejects every resolved record",
                );
            }
            ContradictionStatus::Open => {}
        }
    }
    let expected_contradiction_ids = canonical_contradiction_ids()
        .into_iter()
        .collect::<BTreeSet<_>>();
    for missing in expected_contradiction_ids.difference(&contradiction_ids) {
        validator.error(
            CatalogValidationCode::InvalidDefinition,
            "contradictions",
            format!("missing canonical contradiction `{missing}`"),
        );
    }
    for unexpected in contradiction_ids.difference(&expected_contradiction_ids) {
        validator.error(
            CatalogValidationCode::InvalidDefinition,
            "contradictions",
            format!("unexpected schema-v1 contradiction `{unexpected}`"),
        );
    }
    if catalog.contradictions.is_empty() {
        validator.error(
            CatalogValidationCode::EmptyRequiredField,
            "contradictions",
            "catalog must retain its known contradiction ledger",
        );
    }

    let declared_change_revisions = catalog
        .change_history
        .iter()
        .map(|record| record.catalog_revision.as_str())
        .collect::<BTreeSet<_>>();
    let mut review_ids = BTreeSet::new();
    let mut current_revision_reviewed = false;
    for (index, record) in catalog.review_history.iter().enumerate() {
        let path = format!("review_history[{index}]");
        validator.require_text(&format!("{path}.review_id"), &record.review_id);
        if !review_ids.insert(record.review_id.clone()) {
            validator.error(
                CatalogValidationCode::DuplicateId,
                format!("{path}.review_id"),
                format!("duplicate review id `{}`", record.review_id),
            );
        }
        validator.require_text(&format!("{path}.reviewed_at_utc"), &record.reviewed_at_utc);
        if !is_canonical_utc_timestamp(&record.reviewed_at_utc) {
            validator.error(
                CatalogValidationCode::InvalidReviewAuthority,
                format!("{path}.reviewed_at_utc"),
                "review timestamp must use canonical UTC `YYYY-MM-DDTHH:MM:SSZ` form",
            );
        }
        validator.require_text(
            &format!("{path}.reviewed_catalog_revision"),
            &record.reviewed_catalog_revision,
        );
        if !declared_change_revisions.contains(record.reviewed_catalog_revision.as_str()) {
            validator.error(
                CatalogValidationCode::InvalidReviewAuthority,
                format!("{path}.reviewed_catalog_revision"),
                format!(
                    "reviewed revision `{}` has no retained append-only change-history row",
                    record.reviewed_catalog_revision
                ),
            );
        }
        current_revision_reviewed |= record.reviewed_catalog_revision == catalog.catalog_revision;
        if let Some(reviewed_commit) = &record.reviewed_commit {
            validator.require_hex_digest(
                CatalogValidationCode::InvalidReviewAuthority,
                &format!("{path}.reviewed_commit"),
                reviewed_commit,
                40,
                "reviewed Git commit",
            );
        }
        validator.require_text(&format!("{path}.reviewer"), &record.reviewer);
        validator.require_text_list(&format!("{path}.scope"), &record.scope);
        validator.require_optional_repo_ref(
            &format!("{path}.authority_receipt_ref"),
            record.authority_receipt_ref.as_deref(),
        );
        if let Some(receipt_sha256) = &record.authority_receipt_sha256 {
            validator.require_hex_digest(
                CatalogValidationCode::InvalidReviewAuthority,
                &format!("{path}.authority_receipt_sha256"),
                receipt_sha256,
                64,
                "authority receipt SHA-256",
            );
        }
        if record.authority_receipt_ref.is_some() != record.authority_receipt_sha256.is_some() {
            validator.error(
                CatalogValidationCode::InvalidReviewAuthority,
                format!("{path}.authority_receipt_ref"),
                "authority receipt reference and SHA-256 must either both be present or both be absent",
            );
        }
        match record.disposition {
            ReviewDisposition::Approved | ReviewDisposition::ChangesRequested => {
                validator.error(
                    CatalogValidationCode::InvalidReviewAuthority,
                    path.clone(),
                    "schema version 1 has no trusted signer registry or signed authority-receipt verifier and rejects every approved or changes_requested review",
                );
                if record.authority_kind == ReviewAuthorityKind::AutomatedInformational {
                    validator.error(
                        CatalogValidationCode::InvalidReviewAuthority,
                        format!("{path}.authority_kind"),
                        "approved or changes_requested review requires human authority",
                    );
                }
                if record.reviewed_commit.is_none()
                    || record.authority_receipt_ref.is_none()
                    || record.authority_receipt_sha256.is_none()
                {
                    validator.error(
                        CatalogValidationCode::InvalidReviewAuthority,
                        path.clone(),
                        "approved or changes_requested review requires a 40-hex commit and content-bound authority receipt",
                    );
                }
                let expected_scope = canonical_review_scope(record.authority_kind);
                let actual_scope = record
                    .scope
                    .iter()
                    .map(String::as_str)
                    .collect::<BTreeSet<_>>();
                if actual_scope != BTreeSet::from([expected_scope])
                    || actual_scope.len() != record.scope.len()
                {
                    validator.error(
                        CatalogValidationCode::InvalidReviewAuthority,
                        format!("{path}.scope"),
                        format!(
                            "authority `{}` requires the exact scope `{expected_scope}`",
                            record.authority_kind.as_str()
                        ),
                    );
                }
            }
            ReviewDisposition::Informational => {}
        }
        if record.authority_kind == ReviewAuthorityKind::AutomatedInformational
            && (record.disposition != ReviewDisposition::Informational
                || record.authority_receipt_ref.is_some()
                || record.authority_receipt_sha256.is_some())
        {
            validator.error(
                CatalogValidationCode::InvalidReviewAuthority,
                format!("{path}.authority_kind"),
                "automated_informational review must be informational and cannot carry an authority receipt",
            );
        }
        validator.require_text(&format!("{path}.notes"), &record.notes);
        validator.require_repo_refs(&format!("{path}.source_refs"), &record.source_refs, false);
        if record.review_id == INITIAL_INFORMATIONAL_REVIEW_ID {
            if index != 0
                || record.reviewed_at_utc != INITIAL_INFORMATIONAL_REVIEWED_AT_UTC
                || record.reviewed_catalog_revision != INITIAL_CATALOG_REVISION
                || record.reviewed_commit.is_some()
                || record.reviewer != INITIAL_INFORMATIONAL_REVIEWER
                || record.authority_kind != ReviewAuthorityKind::AutomatedInformational
                || record.disposition != ReviewDisposition::Informational
                || record.scope.len() != 1
                || record.scope[0] != INITIAL_INFORMATIONAL_REVIEW_SCOPE
                || record.authority_receipt_ref.is_some()
                || record.authority_receipt_sha256.is_some()
                || record.notes != INITIAL_INFORMATIONAL_REVIEW_NOTES
            {
                validator.error(
                    CatalogValidationCode::InvalidReviewAuthority,
                    path.clone(),
                    "the immutable initial informational review fields must equal the canonical schema-v1 record",
                );
            }
            let actual = record
                .source_refs
                .iter()
                .map(String::as_str)
                .collect::<BTreeSet<_>>();
            let expected = canonical_initial_review_source_refs()
                .iter()
                .copied()
                .collect::<BTreeSet<_>>();
            if actual != expected || actual.len() != record.source_refs.len() {
                validator.error(
                    CatalogValidationCode::InvalidReviewAuthority,
                    format!("{path}.source_refs"),
                    "the initial informational review must retain the exact canonical provenance set without duplicates",
                );
            }
        }
    }
    if catalog.review_history.is_empty() {
        validator.error(
            CatalogValidationCode::EmptyRequiredField,
            "review_history",
            "catalog must retain at least one review record",
        );
    }
    if !review_ids.contains(INITIAL_INFORMATIONAL_REVIEW_ID) {
        validator.error(
            CatalogValidationCode::InvalidReviewAuthority,
            "review_history",
            format!("schema-v1 review history must retain `{INITIAL_INFORMATIONAL_REVIEW_ID}`"),
        );
    }
    if !current_revision_reviewed {
        validator.error(
            CatalogValidationCode::InvalidReviewAuthority,
            "review_history",
            format!(
                "catalog revision `{}` requires its own retained review record; historical reviews cannot be rewritten",
                catalog.catalog_revision
            ),
        );
    }

    let mut change_ids = BTreeSet::new();
    let mut change_revisions = BTreeSet::new();
    for (index, record) in catalog.change_history.iter().enumerate() {
        let path = format!("change_history[{index}]");
        validator.require_text(&format!("{path}.change_id"), &record.change_id);
        if !change_ids.insert(record.change_id.clone()) {
            validator.error(
                CatalogValidationCode::DuplicateId,
                format!("{path}.change_id"),
                format!("duplicate change id `{}`", record.change_id),
            );
        }
        validator.require_text(
            &format!("{path}.catalog_revision"),
            &record.catalog_revision,
        );
        if !change_revisions.insert(record.catalog_revision.clone()) {
            validator.error(
                CatalogValidationCode::DuplicateId,
                format!("{path}.catalog_revision"),
                format!(
                    "duplicate catalog revision `{}` in append-only history",
                    record.catalog_revision
                ),
            );
        }
        let expected_change_id = canonical_change_id(&record.catalog_revision);
        if record.change_id != expected_change_id {
            validator.error(
                CatalogValidationCode::InvalidDefinition,
                format!("{path}.change_id"),
                format!("expected canonical change id `{expected_change_id}`"),
            );
        }
        validator.require_text(&format!("{path}.changed_at_utc"), &record.changed_at_utc);
        if !is_canonical_utc_timestamp(&record.changed_at_utc) {
            validator.error(
                CatalogValidationCode::InvalidDefinition,
                format!("{path}.changed_at_utc"),
                "change timestamp must use canonical UTC `YYYY-MM-DDTHH:MM:SSZ` form",
            );
        }
        validator.require_text(&format!("{path}.summary"), &record.summary);
        validator.require_repo_refs(&format!("{path}.source_refs"), &record.source_refs, false);
        if record.change_id == INITIAL_CHANGE_ID {
            let actual_sources = record
                .source_refs
                .iter()
                .map(String::as_str)
                .collect::<BTreeSet<_>>();
            let expected_sources = canonical_initial_change_source_refs()
                .iter()
                .copied()
                .collect::<BTreeSet<_>>();
            if index != 0
                || record.catalog_revision != INITIAL_CATALOG_REVISION
                || record.changed_at_utc != INITIAL_INFORMATIONAL_REVIEWED_AT_UTC
                || record.summary != INITIAL_CHANGE_SUMMARY
                || actual_sources != expected_sources
                || actual_sources.len() != record.source_refs.len()
            {
                validator.error(
                    CatalogValidationCode::InvalidDefinition,
                    path.clone(),
                    "the immutable initial change-history row must equal the canonical schema-v1 record",
                );
            }
        }
    }
    if catalog.change_history.is_empty() {
        validator.error(
            CatalogValidationCode::EmptyRequiredField,
            "change_history",
            "catalog must retain at least one change record",
        );
    }
    if !change_ids.contains(INITIAL_CHANGE_ID) {
        validator.error(
            CatalogValidationCode::InvalidDefinition,
            "change_history",
            format!("schema-v1 history must retain immutable `{INITIAL_CHANGE_ID}`"),
        );
    }
    if !change_revisions.contains(&catalog.catalog_revision) {
        validator.error(
            CatalogValidationCode::InvalidDefinition,
            "change_history",
            format!(
                "current catalog revision `{}` has no append-only change-history row",
                catalog.catalog_revision
            ),
        );
    }
    if catalog
        .change_history
        .last()
        .is_some_and(|record| record.catalog_revision != catalog.catalog_revision)
    {
        validator.error(
            CatalogValidationCode::InvalidDefinition,
            "change_history",
            "the final append-only change-history row must introduce the current catalog revision",
        );
    }
}

fn validate_mapping_journeys(
    path: &str,
    mapped: &[String],
    journey_ids: &BTreeSet<String>,
    validator: &mut ValidatorState,
) {
    if mapped.is_empty() {
        validator.error(
            CatalogValidationCode::EmptyRequiredField,
            format!("{path}.journey_ids"),
            "mapping must reference at least one journey",
        );
    }
    for journey_id in mapped {
        if !journey_ids.contains(journey_id) {
            validator.error(
                CatalogValidationCode::DanglingReference,
                format!("{path}.journey_ids"),
                format!("undefined journey `{journey_id}`"),
            );
        }
    }
}

fn validate_canonical_mapping(
    path: &str,
    actual: &[String],
    expected: &[&str],
    validator: &mut ValidatorState,
) {
    let actual_set = actual.iter().map(String::as_str).collect::<BTreeSet<_>>();
    let expected_set = expected.iter().copied().collect::<BTreeSet<_>>();
    if actual_set != expected_set || actual_set.len() != actual.len() {
        validator.error(
            CatalogValidationCode::InvalidDefinition,
            format!("{path}.journey_ids"),
            "mapping journeys must equal the canonical schema-v1 set without duplicates",
        );
    }
}

fn validate_coverage_dimensions(
    coverage: CoverageKey,
    index: &DefinitionIndex,
    path: &str,
    validator: &mut ValidatorState,
) {
    if !index.personas.contains(&coverage.persona) {
        validator.error(
            CatalogValidationCode::DanglingReference,
            path,
            format!("undefined persona `{}`", coverage.persona.as_str()),
        );
    }
    if !index.fleet_points.contains(&coverage.fleet_point) {
        validator.error(
            CatalogValidationCode::DanglingReference,
            path,
            format!("undefined fleet point `{}`", coverage.fleet_point.as_str()),
        );
    }
    if !index.topologies.contains(&coverage.topology) {
        validator.error(
            CatalogValidationCode::DanglingReference,
            path,
            format!("undefined topology `{}`", coverage.topology.as_str()),
        );
    }
}

fn report_coverage_difference(
    path: &str,
    expected: &BTreeSet<CoverageKey>,
    actual: &BTreeSet<CoverageKey>,
    validator: &mut ValidatorState,
) {
    for missing in expected.difference(actual) {
        validator.error(
            CatalogValidationCode::MissingRequiredCoverage,
            path,
            format!("missing required coverage `{}`", missing.label()),
        );
    }
    for unexpected in actual.difference(expected) {
        validator.error(
            CatalogValidationCode::MissingRequiredCoverage,
            path,
            format!("unexpected coverage `{}`", unexpected.label()),
        );
    }
}

fn require_exact_enum_set<T, const N: usize>(
    path: &str,
    seen: &BTreeSet<T>,
    required: [T; N],
    label: impl Fn(T) -> &'static str,
    validator: &mut ValidatorState,
) where
    T: Copy + Ord,
{
    for expected in required {
        if !seen.contains(&expected) {
            validator.error(
                CatalogValidationCode::InvalidDefinition,
                path,
                format!("missing required definition `{}`", label(expected)),
            );
        }
    }
}

const fn canonical_mac_modes() -> [TargetMode; 3] {
    [
        TargetMode::M4ProNative,
        TargetMode::M5Native,
        TargetMode::M5ProMaxNative,
    ]
}

const fn canonical_gate_ids() -> [&'static str; 13] {
    [
        "gate.catalog_binding",
        "gate.candidate_identity",
        "gate.transport_identity",
        "gate.native_interaction",
        "gate.presentation_quality",
        "gate.accessibility",
        "gate.fleet_fairness",
        "gate.long_session_resources",
        "gate.automation_policy",
        "gate.failure_recovery",
        "gate.privacy_diagnostics",
        "gate.target_qualification",
        "gate.release_promotion",
    ]
}

fn canonical_gate_producer_beads(gate_id: &str) -> Option<&'static [&'static str]> {
    match gate_id {
        "gate.catalog_binding" => Some(&[
            "ft-interactive-swarm-product-convergence-7xqz4.1.1",
            "ft-interactive-swarm-product-convergence-7xqz4.11.15",
        ]),
        "gate.candidate_identity" => Some(&[
            "ft-interactive-swarm-product-convergence-7xqz4.2.1",
            "ft-interactive-swarm-product-convergence-7xqz4.2.2",
            "ft-interactive-swarm-product-convergence-7xqz4.2.3",
            "ft-interactive-swarm-product-convergence-7xqz4.2.9",
        ]),
        "gate.native_interaction" => Some(&[
            "ft-interactive-systems-performance-4tenz.2",
            "ft-interactive-swarm-product-convergence-7xqz4.4.2",
            "ft-interactive-swarm-product-convergence-7xqz4.4.3",
            "ft-interactive-swarm-product-convergence-7xqz4.4.4",
            "ft-interactive-swarm-product-convergence-7xqz4.4.5",
            "ft-interactive-swarm-product-convergence-7xqz4.4.6",
        ]),
        "gate.presentation_quality" => Some(&[
            "ft-interactive-systems-performance-4tenz.3",
            "ft-interactive-swarm-product-convergence-7xqz4.9",
        ]),
        "gate.transport_identity" => Some(&[
            "ft-interactive-swarm-product-convergence-7xqz4.11.8",
            "ft-interactive-swarm-product-convergence-7xqz4.11.9",
        ]),
        "gate.fleet_fairness" => Some(&[
            "ft-interactive-systems-performance-4tenz.5",
            "ft-interactive-systems-performance-4tenz.6",
            "ft-interactive-swarm-product-convergence-7xqz4.3",
            "ft-interactive-swarm-product-convergence-7xqz4.11.4",
            "ft-interactive-swarm-product-convergence-7xqz4.11.5",
        ]),
        "gate.long_session_resources" => Some(&[
            "ft-interactive-systems-performance-4tenz.4",
            "ft-interactive-swarm-product-convergence-7xqz4.10",
            "ft-interactive-swarm-product-convergence-7xqz4.11.2",
            "ft-interactive-swarm-product-convergence-7xqz4.11.5",
        ]),
        "gate.automation_policy" => Some(&[
            "ft-0elb9",
            "ft-interactive-swarm-product-convergence-7xqz4.4.7",
            "ft-interactive-swarm-product-convergence-7xqz4.11.6",
            "ft-interactive-swarm-product-convergence-7xqz4.11.14",
        ]),
        "gate.failure_recovery" => Some(&[
            "ft-interactive-swarm-product-convergence-7xqz4.8",
            "ft-interactive-swarm-product-convergence-7xqz4.11.8",
            "ft-interactive-swarm-product-convergence-7xqz4.11.9",
            "ft-interactive-swarm-product-convergence-7xqz4.11.10",
            "ft-interactive-swarm-product-convergence-7xqz4.11.11",
        ]),
        "gate.accessibility" => Some(&[
            "ft-interactive-swarm-product-convergence-7xqz4.9",
            "ft-interactive-swarm-product-convergence-7xqz4.11.12",
        ]),
        "gate.privacy_diagnostics" => Some(&[
            "ft-interactive-swarm-product-convergence-7xqz4.1.6",
            "ft-interactive-swarm-product-convergence-7xqz4.7",
            "ft-interactive-swarm-product-convergence-7xqz4.11.13",
            "ft-x8e67",
        ]),
        "gate.target_qualification" => Some(&[
            "ft-interactive-systems-performance-4tenz.9.3",
            "ft-interactive-systems-performance-4tenz.9.4",
            "ft-interactive-systems-performance-4tenz.9.5",
            "ft-interactive-systems-performance-4tenz.9.6",
            "ft-interactive-systems-performance-4tenz.9.7",
            "ft-interactive-swarm-product-convergence-7xqz4.10.6",
            "ft-interactive-swarm-product-convergence-7xqz4.10.9",
            "ft-interactive-swarm-product-convergence-7xqz4.12.3.1",
            "ft-tf6g3.14",
        ]),
        "gate.release_promotion" => Some(&[
            "ft-interactive-swarm-product-convergence-7xqz4.11.15",
            "ft-interactive-swarm-product-convergence-7xqz4.12.1",
            "ft-interactive-swarm-product-convergence-7xqz4.12.3",
            "ft-interactive-swarm-product-convergence-7xqz4.12.7",
            "ft-interactive-swarm-product-convergence-7xqz4.12.9",
        ]),
        _ => None,
    }
}

fn canonical_journey_personas(journey_id: &str) -> Option<&'static [ProductPersona]> {
    const ALL: &[ProductPersona] = &[
        ProductPersona::InteractiveHuman,
        ProductPersona::MetaAgentOperator,
        ProductPersona::AutomationAgent,
        ProductPersona::IncidentResponder,
    ];
    const NON_INCIDENT: &[ProductPersona] = &[
        ProductPersona::InteractiveHuman,
        ProductPersona::MetaAgentOperator,
        ProductPersona::AutomationAgent,
    ];
    const INTERACTIVE_ONLY: &[ProductPersona] = &[ProductPersona::InteractiveHuman];

    match journey_id {
        "journey.clean_mac_first_hour"
        | "journey.two_agent_everyday"
        | "journey.version_pinned_agent_dogfood" => Some(NON_INCIDENT),
        "journey.accessible_operator_day" => Some(INTERACTIVE_ONLY),
        "journey.twenty_pane_daily"
        | "journey.fifty_pane_mac_trj"
        | "journey.two_hundred_pane_mission"
        | "journey.attention_and_verified_submit"
        | "journey.concurrent_maintenance"
        | "journey.remote_unavailable_recovery"
        | "journey.route_roam_sleep_wake"
        | "journey.live_update_rollback"
        | "journey.component_crash_recovery"
        | "journey.field_lag_diagnosis" => Some(ALL),
        _ => None,
    }
}

fn canonical_journey_gates(journey_id: &str) -> Option<&'static [&'static str]> {
    match journey_id {
        "journey.clean_mac_first_hour" => Some(&[
            "gate.catalog_binding",
            "gate.candidate_identity",
            "gate.native_interaction",
            "gate.presentation_quality",
            "gate.accessibility",
            "gate.privacy_diagnostics",
            "gate.release_promotion",
        ]),
        "journey.two_agent_everyday" => Some(&[
            "gate.catalog_binding",
            "gate.candidate_identity",
            "gate.transport_identity",
            "gate.native_interaction",
            "gate.presentation_quality",
            "gate.accessibility",
            "gate.fleet_fairness",
            "gate.long_session_resources",
            "gate.privacy_diagnostics",
            "gate.release_promotion",
        ]),
        "journey.twenty_pane_daily" => Some(&[
            "gate.catalog_binding",
            "gate.candidate_identity",
            "gate.transport_identity",
            "gate.native_interaction",
            "gate.presentation_quality",
            "gate.accessibility",
            "gate.fleet_fairness",
            "gate.long_session_resources",
            "gate.automation_policy",
            "gate.failure_recovery",
            "gate.privacy_diagnostics",
            "gate.release_promotion",
        ]),
        "journey.fifty_pane_mac_trj" | "journey.two_hundred_pane_mission" => Some(&[
            "gate.catalog_binding",
            "gate.candidate_identity",
            "gate.transport_identity",
            "gate.native_interaction",
            "gate.presentation_quality",
            "gate.accessibility",
            "gate.fleet_fairness",
            "gate.long_session_resources",
            "gate.automation_policy",
            "gate.failure_recovery",
            "gate.privacy_diagnostics",
            "gate.target_qualification",
            "gate.release_promotion",
        ]),
        "journey.attention_and_verified_submit" => Some(&[
            "gate.catalog_binding",
            "gate.candidate_identity",
            "gate.accessibility",
            "gate.fleet_fairness",
            "gate.automation_policy",
            "gate.failure_recovery",
            "gate.privacy_diagnostics",
            "gate.release_promotion",
        ]),
        "journey.concurrent_maintenance" => Some(&[
            "gate.catalog_binding",
            "gate.candidate_identity",
            "gate.accessibility",
            "gate.fleet_fairness",
            "gate.long_session_resources",
            "gate.automation_policy",
            "gate.failure_recovery",
            "gate.privacy_diagnostics",
            "gate.release_promotion",
        ]),
        "journey.remote_unavailable_recovery" => Some(&[
            "gate.catalog_binding",
            "gate.candidate_identity",
            "gate.transport_identity",
            "gate.accessibility",
            "gate.automation_policy",
            "gate.failure_recovery",
            "gate.privacy_diagnostics",
            "gate.release_promotion",
        ]),
        "journey.route_roam_sleep_wake" => Some(&[
            "gate.catalog_binding",
            "gate.candidate_identity",
            "gate.transport_identity",
            "gate.native_interaction",
            "gate.presentation_quality",
            "gate.accessibility",
            "gate.failure_recovery",
            "gate.privacy_diagnostics",
            "gate.target_qualification",
            "gate.release_promotion",
        ]),
        "journey.live_update_rollback" => Some(&[
            "gate.catalog_binding",
            "gate.candidate_identity",
            "gate.accessibility",
            "gate.automation_policy",
            "gate.failure_recovery",
            "gate.privacy_diagnostics",
            "gate.release_promotion",
        ]),
        "journey.component_crash_recovery" => Some(&[
            "gate.catalog_binding",
            "gate.candidate_identity",
            "gate.presentation_quality",
            "gate.accessibility",
            "gate.long_session_resources",
            "gate.automation_policy",
            "gate.failure_recovery",
            "gate.privacy_diagnostics",
            "gate.release_promotion",
        ]),
        "journey.accessible_operator_day" => Some(&[
            "gate.catalog_binding",
            "gate.candidate_identity",
            "gate.native_interaction",
            "gate.presentation_quality",
            "gate.accessibility",
            "gate.failure_recovery",
            "gate.privacy_diagnostics",
            "gate.release_promotion",
        ]),
        "journey.field_lag_diagnosis" => Some(&[
            "gate.catalog_binding",
            "gate.candidate_identity",
            "gate.transport_identity",
            "gate.native_interaction",
            "gate.presentation_quality",
            "gate.accessibility",
            "gate.fleet_fairness",
            "gate.long_session_resources",
            "gate.failure_recovery",
            "gate.privacy_diagnostics",
            "gate.target_qualification",
            "gate.release_promotion",
        ]),
        "journey.version_pinned_agent_dogfood" => Some(&[
            "gate.catalog_binding",
            "gate.candidate_identity",
            "gate.native_interaction",
            "gate.presentation_quality",
            "gate.accessibility",
            "gate.fleet_fairness",
            "gate.automation_policy",
            "gate.failure_recovery",
            "gate.privacy_diagnostics",
            "gate.release_promotion",
        ]),
        _ => None,
    }
}

const fn canonical_legacy_mapping_ids() -> [&'static str; 4] {
    [
        "legacy.operator_acceptance",
        "legacy.demo_scenarios",
        "legacy.performance_campaign",
        "legacy.high_scale_rehearsals",
    ]
}

fn canonical_legacy_mapping_journeys(legacy_id: &str) -> Option<&'static [&'static str]> {
    match legacy_id {
        "legacy.operator_acceptance" => Some(&[
            "journey.clean_mac_first_hour",
            "journey.two_agent_everyday",
            "journey.twenty_pane_daily",
            "journey.attention_and_verified_submit",
            "journey.live_update_rollback",
            "journey.component_crash_recovery",
            "journey.accessible_operator_day",
        ]),
        "legacy.demo_scenarios" => Some(&[
            "journey.clean_mac_first_hour",
            "journey.two_agent_everyday",
            "journey.twenty_pane_daily",
            "journey.attention_and_verified_submit",
            "journey.version_pinned_agent_dogfood",
        ]),
        "legacy.performance_campaign" => Some(&[
            "journey.two_agent_everyday",
            "journey.twenty_pane_daily",
            "journey.fifty_pane_mac_trj",
            "journey.two_hundred_pane_mission",
            "journey.attention_and_verified_submit",
            "journey.concurrent_maintenance",
            "journey.remote_unavailable_recovery",
            "journey.route_roam_sleep_wake",
            "journey.live_update_rollback",
            "journey.component_crash_recovery",
            "journey.accessible_operator_day",
            "journey.field_lag_diagnosis",
        ]),
        "legacy.high_scale_rehearsals" => Some(&[
            "journey.fifty_pane_mac_trj",
            "journey.two_hundred_pane_mission",
            "journey.concurrent_maintenance",
            "journey.component_crash_recovery",
            "journey.field_lag_diagnosis",
        ]),
        _ => None,
    }
}

fn canonical_legacy_mapping_source_refs(legacy_id: &str) -> Option<&'static [&'static str]> {
    match legacy_id {
        "legacy.operator_acceptance" => {
            Some(&["docs/ft-xbnl0-5-4-operator-acceptance-scenarios.json"])
        }
        "legacy.demo_scenarios" => Some(&["docs/demo-scenarios.md"]),
        "legacy.performance_campaign" => Some(&[
            "docs/perf/mux-long-session-performance-campaign.md",
            "docs/perf-ledger/interactive-systems-negative-results.md",
        ]),
        "legacy.high_scale_rehearsals" => Some(&[
            "docs/high-scale-operator-rehearsals.md",
            "docs/swarm-capacity-simulation-corpus.md",
        ]),
        _ => None,
    }
}

const fn canonical_readme_mapping_ids() -> [&'static str; 7] {
    [
        "readme.fleet_200_plus",
        "readme.capture_every_byte",
        "readme.overlapping_capacity_bands",
        "readme.atomic_first_hour",
        "readme.product_persona_namespace",
        "readme.remote_topology_identity",
        "readme.feature_vs_journey_support",
    ]
}

fn canonical_readme_mapping(mapping_id: &str) -> Option<(&'static str, &'static [&'static str])> {
    match mapping_id {
        "readme.fleet_200_plus" => Some((
            "README.md#whats-here",
            &["journey.two_hundred_pane_mission"],
        )),
        "readme.capture_every_byte" => Some((
            "README.md#tldr",
            &[
                "journey.concurrent_maintenance",
                "journey.component_crash_recovery",
                "journey.field_lag_diagnosis",
            ],
        )),
        "readme.overlapping_capacity_bands" => Some((
            "README.md#deep-dive-capacity-planning",
            &[
                "journey.fifty_pane_mac_trj",
                "journey.two_hundred_pane_mission",
            ],
        )),
        "readme.atomic_first_hour" => Some((
            "README.md#10-minute-tour",
            &["journey.clean_mac_first_hour"],
        )),
        "readme.product_persona_namespace" => Some((
            "README.md#deep-dive-agent-profiles-personas-and-fleet-templates",
            &[
                "journey.attention_and_verified_submit",
                "journey.version_pinned_agent_dogfood",
            ],
        )),
        "readme.remote_topology_identity" => Some((
            "README.md#what-ft-doesnt-do-yet",
            &[
                "journey.fifty_pane_mac_trj",
                "journey.remote_unavailable_recovery",
                "journey.route_roam_sleep_wake",
            ],
        )),
        "readme.feature_vs_journey_support" => Some((
            "README.md#tldr",
            &[
                "journey.clean_mac_first_hour",
                "journey.two_agent_everyday",
                "journey.twenty_pane_daily",
                "journey.fifty_pane_mac_trj",
                "journey.two_hundred_pane_mission",
                "journey.accessible_operator_day",
            ],
        )),
        _ => None,
    }
}

const INITIAL_INFORMATIONAL_REVIEW_ID: &str = "ft.product-journey-review.2026-07-27.initial";
const INITIAL_CATALOG_REVISION: &str = "2026-07-27.1";
const INITIAL_INFORMATIONAL_REVIEWED_AT_UTC: &str = "2026-07-27T23:04:36Z";
const INITIAL_INFORMATIONAL_REVIEWER: &str = "Codex systems architecture review";
const INITIAL_INFORMATIONAL_REVIEW_SCOPE: &str = "Personas, exact fleet points, mux topologies, target posture, 32-cell coverage, 14 field journeys, lifecycle, visual, accessibility, privacy, dependencies, and known claim drift.";
const INITIAL_INFORMATIONAL_REVIEW_NOTES: &str = "AI-authored initial informational review. Commit df4414f5587cccface7edebdee6028ae758f82f8 is only the shared source baseline and does not contain these catalog bytes. Human product-owner approval and later human-locked visual and accessibility reviews remain pending; no cell is supported.";
const INITIAL_CHANGE_ID: &str = "change.2026_07_27.1";
const INITIAL_CHANGE_SUMMARY: &str = "Initial contract-only catalog with four product personas, four exact fleet points, two mux topologies, four neutral target identities, four actor modes, thirteen fail-closed gates, fourteen field journeys, thirty-two required cells, exact and partial producer bindings, three target qualifications per cell, content-bound README mappings, and eight open contradictions.";

const fn canonical_initial_review_source_refs() -> [&'static str; 12] {
    [
        "AGENTS.md",
        "README.md",
        ".beads/issues.jsonl",
        "docs/design/product-journey-contract.md",
        "docs/json-schema/ft-product-journey-catalog.json",
        "docs/perf/mux-long-session-performance-campaign.md",
        "docs/ft-xbnl0-5-4-operator-acceptance-scenarios.json",
        "docs/demo-scenarios.md",
        "docs/high-scale-operator-rehearsals.md",
        "docs/a11y/scenario-corpus.md",
        "docs/perf/target-class-hardware.md",
        "docs/ft-xbnl0-5-5-closure-metadata.json",
    ]
}

const fn canonical_initial_change_source_refs() -> [&'static str; 2] {
    [
        "docs/design/product-journey-contract.md",
        "docs/json-schema/ft-product-journey-catalog.json",
    ]
}

fn canonical_change_id(catalog_revision: &str) -> String {
    format!("change.{}", catalog_revision.replace('-', "_"))
}

const fn canonical_review_scope(authority_kind: ReviewAuthorityKind) -> &'static str {
    match authority_kind {
        ReviewAuthorityKind::AutomatedInformational => "informational_only",
        ReviewAuthorityKind::HumanProductOwner => "catalog_contract",
        ReviewAuthorityKind::HumanVisual => "visual_quality",
        ReviewAuthorityKind::HumanAccessibility => "accessibility",
        ReviewAuthorityKind::HumanPrivacy => "privacy",
    }
}

const fn canonical_target_classes() -> [(&'static str, TargetMode); 4] {
    [
        ("mac16_11_m4_pro", TargetMode::M4ProNative),
        ("m5_native", TargetMode::M5Native),
        ("m5_pro_max_native", TargetMode::M5ProMaxNative),
        ("trj_5995wx", TargetMode::ThreadripperPro5995wxNative),
    ]
}

fn canonical_target_class_mode(target_class_id: &str) -> Option<TargetMode> {
    canonical_target_classes()
        .into_iter()
        .find_map(|(canonical_id, mode)| (canonical_id == target_class_id).then_some(mode))
}

const fn canonical_actor_mode(persona: ProductPersona) -> ActorMode {
    match persona {
        ProductPersona::InteractiveHuman => ActorMode::HumanInteractive,
        ProductPersona::MetaAgentOperator => ActorMode::MetaAgentSupervised,
        ProductPersona::AutomationAgent => ActorMode::AutomationUnattended,
        ProductPersona::IncidentResponder => ActorMode::IncidentResponse,
    }
}

fn canonical_producer_coverage(coverage: CoverageKey) -> ProducerCoverage {
    match coverage.persona {
        ProductPersona::InteractiveHuman => match (coverage.topology, coverage.fleet_point) {
            (Topology::LocalOnly, FleetPoint::Q002 | FleetPoint::Q020)
            | (
                Topology::MacLanRemote,
                FleetPoint::Q002 | FleetPoint::Q020 | FleetPoint::Q050 | FleetPoint::Q200,
            ) => ProducerCoverage::Direct,
            (Topology::LocalOnly, FleetPoint::Q050 | FleetPoint::Q200) => ProducerCoverage::Gap,
        },
        ProductPersona::MetaAgentOperator | ProductPersona::AutomationAgent => {
            match coverage.fleet_point {
                FleetPoint::Q020 | FleetPoint::Q050 => ProducerCoverage::Partial,
                FleetPoint::Q002 | FleetPoint::Q200 => ProducerCoverage::Gap,
            }
        }
        ProductPersona::IncidentResponder => ProducerCoverage::Partial,
    }
}

fn canonical_direct_producer_bead(coverage: CoverageKey) -> Option<&'static str> {
    if coverage.persona != ProductPersona::InteractiveHuman {
        return None;
    }
    match (coverage.topology, coverage.fleet_point) {
        (Topology::LocalOnly | Topology::MacLanRemote, FleetPoint::Q002) => {
            Some("ft-interactive-swarm-product-convergence-7xqz4.11.2")
        }
        (Topology::LocalOnly | Topology::MacLanRemote, FleetPoint::Q020) => {
            Some("ft-interactive-swarm-product-convergence-7xqz4.11.3")
        }
        (Topology::MacLanRemote, FleetPoint::Q050) => {
            Some("ft-interactive-swarm-product-convergence-7xqz4.11.4")
        }
        (Topology::MacLanRemote, FleetPoint::Q200) => {
            Some("ft-interactive-swarm-product-convergence-7xqz4.11.5")
        }
        (Topology::LocalOnly, FleetPoint::Q050 | FleetPoint::Q200) => None,
    }
}

const fn canonical_persona_for_actor_mode(actor_mode: ActorMode) -> ProductPersona {
    match actor_mode {
        ActorMode::HumanInteractive => ProductPersona::InteractiveHuman,
        ActorMode::MetaAgentSupervised => ProductPersona::MetaAgentOperator,
        ActorMode::AutomationUnattended => ProductPersona::AutomationAgent,
        ActorMode::IncidentResponse => ProductPersona::IncidentResponder,
    }
}

fn canonical_variant_journeys(coverage: CoverageKey) -> BTreeSet<&'static str> {
    let mut journeys = BTreeSet::new();
    match coverage.persona {
        ProductPersona::InteractiveHuman => {
            journeys.insert("journey.accessible_operator_day");
            journeys.insert("journey.version_pinned_agent_dogfood");
            if coverage.topology == Topology::MacLanRemote {
                journeys.insert("journey.component_crash_recovery");
                journeys.insert("journey.live_update_rollback");
            }
            match coverage.fleet_point {
                FleetPoint::Q002 if coverage.topology == Topology::LocalOnly => {
                    journeys.insert("journey.clean_mac_first_hour");
                }
                FleetPoint::Q020 | FleetPoint::Q050 => {
                    journeys.insert("journey.attention_and_verified_submit");
                    journeys.insert("journey.concurrent_maintenance");
                }
                FleetPoint::Q200 => {
                    journeys.insert("journey.concurrent_maintenance");
                    journeys.insert("journey.component_crash_recovery");
                    if coverage.topology == Topology::MacLanRemote {
                        journeys.insert("journey.attention_and_verified_submit");
                    }
                }
                FleetPoint::Q002 => {}
            }
        }
        ProductPersona::MetaAgentOperator | ProductPersona::AutomationAgent => {
            journeys.insert("journey.attention_and_verified_submit");
            journeys.insert("journey.component_crash_recovery");
            journeys.insert("journey.live_update_rollback");
            journeys.insert("journey.version_pinned_agent_dogfood");
            if coverage.fleet_point != FleetPoint::Q002 {
                journeys.insert("journey.concurrent_maintenance");
            } else if coverage.topology == Topology::LocalOnly {
                journeys.insert("journey.clean_mac_first_hour");
            }
        }
        ProductPersona::IncidentResponder => {
            journeys.insert("journey.concurrent_maintenance");
            journeys.insert("journey.live_update_rollback");
            journeys.insert("journey.component_crash_recovery");
            journeys.insert("journey.field_lag_diagnosis");
        }
    }

    match coverage.fleet_point {
        FleetPoint::Q002 if coverage.persona != ProductPersona::IncidentResponder => {
            journeys.insert("journey.two_agent_everyday");
        }
        FleetPoint::Q020 => {
            journeys.insert("journey.twenty_pane_daily");
        }
        FleetPoint::Q050 if coverage.topology == Topology::MacLanRemote => {
            journeys.insert("journey.fifty_pane_mac_trj");
        }
        FleetPoint::Q200 => {
            journeys.insert("journey.two_hundred_pane_mission");
        }
        FleetPoint::Q002 | FleetPoint::Q050 => {}
    }

    if coverage.topology == Topology::MacLanRemote {
        journeys.insert("journey.remote_unavailable_recovery");
        journeys.insert("journey.route_roam_sleep_wake");
        journeys.insert("journey.field_lag_diagnosis");
    }
    journeys
}

fn canonical_variant_id(coverage: CoverageKey) -> String {
    format!(
        "variant.{}.{}.{}",
        coverage.persona.as_str(),
        coverage.topology.as_str(),
        coverage.fleet_point.as_str()
    )
}

fn canonical_claim_id(coverage: CoverageKey) -> String {
    format!(
        "claim.{}.{}.{}",
        coverage.persona.as_str(),
        coverage.topology.as_str(),
        coverage.fleet_point.as_str()
    )
}

fn canonical_qualification_id(
    coverage: CoverageKey,
    controller_target_class_id: &str,
    session_host_target_class_id: &str,
) -> String {
    format!(
        "qual.{}.{}.{}.{}.{}",
        coverage.persona.as_str(),
        coverage.topology.as_str(),
        coverage.fleet_point.as_str(),
        controller_target_class_id,
        session_host_target_class_id,
    )
}

fn target_mode_allowed_for_role(mode: TargetMode, topology: Topology, role: TargetRole) -> bool {
    match role {
        TargetRole::Controller => canonical_mac_modes().contains(&mode),
        TargetRole::SessionHost => match topology {
            Topology::LocalOnly => canonical_mac_modes().contains(&mode),
            Topology::MacLanRemote => mode == TargetMode::ThreadripperPro5995wxNative,
        },
    }
}

fn canonical_target_pairs(topology: Topology) -> BTreeSet<(&'static str, &'static str)> {
    let session_host = match topology {
        Topology::LocalOnly => None,
        Topology::MacLanRemote => Some("trj_5995wx"),
    };
    ["mac16_11_m4_pro", "m5_native", "m5_pro_max_native"]
        .into_iter()
        .map(|controller| (controller, session_host.unwrap_or(controller)))
        .collect()
}

const fn canonical_field_journey_bindings() -> [(&'static str, &'static str); 14] {
    [
        (
            "ft-interactive-swarm-product-convergence-7xqz4.11.1",
            "journey.clean_mac_first_hour",
        ),
        (
            "ft-interactive-swarm-product-convergence-7xqz4.11.2",
            "journey.two_agent_everyday",
        ),
        (
            "ft-interactive-swarm-product-convergence-7xqz4.11.3",
            "journey.twenty_pane_daily",
        ),
        (
            "ft-interactive-swarm-product-convergence-7xqz4.11.4",
            "journey.fifty_pane_mac_trj",
        ),
        (
            "ft-interactive-swarm-product-convergence-7xqz4.11.5",
            "journey.two_hundred_pane_mission",
        ),
        (
            "ft-interactive-swarm-product-convergence-7xqz4.11.6",
            "journey.attention_and_verified_submit",
        ),
        (
            "ft-interactive-swarm-product-convergence-7xqz4.11.7",
            "journey.concurrent_maintenance",
        ),
        (
            "ft-interactive-swarm-product-convergence-7xqz4.11.8",
            "journey.remote_unavailable_recovery",
        ),
        (
            "ft-interactive-swarm-product-convergence-7xqz4.11.9",
            "journey.route_roam_sleep_wake",
        ),
        (
            "ft-interactive-swarm-product-convergence-7xqz4.11.10",
            "journey.live_update_rollback",
        ),
        (
            "ft-interactive-swarm-product-convergence-7xqz4.11.11",
            "journey.component_crash_recovery",
        ),
        (
            "ft-interactive-swarm-product-convergence-7xqz4.11.12",
            "journey.accessible_operator_day",
        ),
        (
            "ft-interactive-swarm-product-convergence-7xqz4.11.13",
            "journey.field_lag_diagnosis",
        ),
        (
            "ft-interactive-swarm-product-convergence-7xqz4.11.14",
            "journey.version_pinned_agent_dogfood",
        ),
    ]
}

const fn canonical_contradiction_ids() -> [&'static str; 8] {
    [
        "contradiction.readme_200_plus_scope",
        "contradiction.readme_lossless_capture",
        "contradiction.readme_capacity_overlap",
        "contradiction.clean_first_hour_gap",
        "contradiction.legacy_rch_local_fallback",
        "contradiction.performance_q001_product_q002",
        "contradiction.persona_namespace_collision",
        "contradiction.remote_path_conflation",
    ]
}

fn canonical_contradiction_scope(
    contradiction_id: &str,
    catalog: &ProductJourneyCatalog,
) -> Option<(bool, BTreeSet<String>)> {
    let globally_blocking = matches!(
        contradiction_id,
        "contradiction.readme_lossless_capture"
            | "contradiction.legacy_rch_local_fallback"
            | "contradiction.persona_namespace_collision"
    );
    if globally_blocking {
        return Some((true, BTreeSet::new()));
    }

    let affected = match contradiction_id {
        "contradiction.readme_200_plus_scope" | "contradiction.readme_capacity_overlap" => catalog
            .variants
            .iter()
            .filter(|variant| variant.coverage.fleet_point == FleetPoint::Q200)
            .map(|variant| variant.claim_id.clone())
            .collect(),
        "contradiction.clean_first_hour_gap" => catalog
            .variants
            .iter()
            .filter(|variant| {
                variant
                    .journey_ids
                    .iter()
                    .any(|journey_id| journey_id == "journey.clean_mac_first_hour")
            })
            .map(|variant| variant.claim_id.clone())
            .collect(),
        "contradiction.performance_q001_product_q002" => catalog
            .variants
            .iter()
            .filter(|variant| variant.coverage.fleet_point == FleetPoint::Q002)
            .map(|variant| variant.claim_id.clone())
            .collect(),
        "contradiction.remote_path_conflation" => catalog
            .variants
            .iter()
            .filter(|variant| variant.coverage.topology == Topology::MacLanRemote)
            .map(|variant| variant.claim_id.clone())
            .collect(),
        "contradiction.readme_lossless_capture"
        | "contradiction.legacy_rch_local_fallback"
        | "contradiction.persona_namespace_collision" => BTreeSet::new(),
        _ => return None,
    };
    Some((false, affected))
}

fn canonical_journey_id_for_field_bead(field_bead_id: &str) -> Option<&'static str> {
    canonical_field_journey_bindings()
        .into_iter()
        .find_map(|(canonical_bead_id, journey_id)| {
            (canonical_bead_id == field_bead_id).then_some(journey_id)
        })
}

fn expected_coverage_keys() -> BTreeSet<CoverageKey> {
    let mut keys = BTreeSet::new();
    for persona in ProductPersona::ALL {
        for fleet_point in FleetPoint::ALL {
            for topology in Topology::ALL {
                keys.insert(CoverageKey::new(persona, fleet_point, topology));
            }
        }
    }
    keys
}

fn is_canonical_utc_timestamp(value: &str) -> bool {
    let bytes = value.as_bytes();
    if bytes.len() != 20
        || bytes[4] != b'-'
        || bytes[7] != b'-'
        || bytes[10] != b'T'
        || bytes[13] != b':'
        || bytes[16] != b':'
        || bytes[19] != b'Z'
    {
        return false;
    }
    let Some(year) = parse_decimal(&bytes[0..4]) else {
        return false;
    };
    let Some(month) = parse_decimal(&bytes[5..7]) else {
        return false;
    };
    let Some(day) = parse_decimal(&bytes[8..10]) else {
        return false;
    };
    let Some(hour) = parse_decimal(&bytes[11..13]) else {
        return false;
    };
    let Some(minute) = parse_decimal(&bytes[14..16]) else {
        return false;
    };
    let Some(second) = parse_decimal(&bytes[17..19]) else {
        return false;
    };
    let leap_year = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    let days_in_month = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if leap_year => 29,
        2 => 28,
        _ => return false,
    };
    (1..=days_in_month).contains(&day) && hour < 24 && minute < 60 && second < 60
}

fn parse_decimal(bytes: &[u8]) -> Option<u32> {
    bytes.iter().try_fold(0_u32, |value, byte| {
        if byte.is_ascii_digit() {
            Some(value * 10 + u32::from(*byte - b'0'))
        } else {
            None
        }
    })
}

fn required_field_bead_ids() -> Vec<String> {
    canonical_field_journey_bindings()
        .into_iter()
        .map(|(field_bead_id, _)| field_bead_id.to_string())
        .collect()
}

fn is_required_field_bead_id(value: &str) -> bool {
    required_field_bead_ids()
        .iter()
        .any(|required| required == value)
}

fn is_well_formed_bead_id(value: &str) -> bool {
    let suffix = match value.strip_prefix("ft-") {
        Some(suffix) => suffix,
        None => return false,
    };
    let mut characters = suffix.chars();
    let Some(first) = characters.next() else {
        return false;
    };
    (first.is_ascii_lowercase() || first.is_ascii_digit())
        && characters.all(|character| {
            character.is_ascii_lowercase()
                || character.is_ascii_digit()
                || matches!(character, '-' | '.')
        })
}

fn validate_repository_reference(reference: &str) -> Result<(), String> {
    let trimmed = reference.trim();
    if trimmed.is_empty() {
        return Err("repository reference must not be empty".to_string());
    }
    if trimmed != reference {
        return Err(format!(
            "repository reference must not contain surrounding whitespace: `{reference}`"
        ));
    }
    if reference.contains('\\') {
        return Err(format!(
            "repository reference must use forward slashes: `{reference}`"
        ));
    }
    if reference.contains("://") || reference.starts_with('/') || reference.starts_with("./") {
        return Err(format!(
            "repository reference must be relative to the repository root: `{reference}`"
        ));
    }

    let (path, fragment) = match reference.split_once('#') {
        Some((path, fragment)) => (path, Some(fragment)),
        None => (reference, None),
    };
    if path.is_empty() {
        return Err(format!(
            "repository reference path must not be empty: `{reference}`"
        ));
    }
    if path
        .split('/')
        .any(|component| component.is_empty() || matches!(component, "." | ".."))
    {
        return Err(format!(
            "repository reference contains an empty or traversal component: `{reference}`"
        ));
    }
    if fragment.is_some_and(str::is_empty) {
        return Err(format!(
            "repository reference fragment must not be empty: `{reference}`"
        ));
    }
    Ok(())
}
