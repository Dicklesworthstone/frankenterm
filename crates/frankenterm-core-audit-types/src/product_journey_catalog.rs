//! Versioned product-journey catalog contract for FrankenTerm.
//!
//! The catalog is deliberately a product-truth contract rather than a runtime
//! result or performance attestation.  It records the user, fleet, topology,
//! target, workflow, support, evidence, and release dimensions independently
//! so that an attractive fixture or proxy result cannot silently become a
//! support claim.
//!
//! This module is leaf-clean: it performs no file I/O and depends only on
//! serialization, SHA-256, Ed25519 verification, and `std`. Repository-path,
//! retained-snapshot resolution, and Beads existence checks belong in the
//! integration test or offline-verifier caller.

use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fmt;
use std::fmt::Write as _;

use ed25519_dalek::{Signature, VerifyingKey};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// The only contract identifier accepted by schema version 1.
pub const PRODUCT_JOURNEY_CONTRACT_ID: &str = "ft.product_journey_catalog.v1";

/// The only schema version accepted by this implementation.
pub const PRODUCT_JOURNEY_SCHEMA_VERSION: u32 = 1;

/// The Bead that owns version 1 of the product-journey contract.
pub const PRODUCT_JOURNEY_SOURCE_BEAD_ID: &str =
    "ft-interactive-swarm-product-convergence-7xqz4.1.1";

/// Content-addressed lineage envelope accepted by the offline verifier.
pub const PRODUCT_JOURNEY_LINEAGE_CONTRACT_ID: &str = "ft.product_journey_lineage.v1";

/// Version of the content-addressed lineage envelope.
pub const PRODUCT_JOURNEY_LINEAGE_SCHEMA_VERSION: u32 = 1;

/// Versioned projection used before hashing a lineage record.
pub const PRODUCT_JOURNEY_LINEAGE_PROJECTION: &str =
    "ft.product_journey_lineage.record_projection.v1";

/// Domain separator for canonical lineage-record SHA-256 digests.
pub const PRODUCT_JOURNEY_LINEAGE_DIGEST_DOMAIN: &str =
    "frankenterm.product-journey.lineage-record.sha256.v1";

/// Domain separator for detached Ed25519 signatures over record digests.
pub const PRODUCT_JOURNEY_LINEAGE_SIGNATURE_DOMAIN: &str =
    "frankenterm.product-journey.lineage-record.ed25519.v1";

/// Identifier for the verifier-pinned lineage trust policy.
pub const PRODUCT_JOURNEY_LINEAGE_TRUST_POLICY_ID: &str = "ft.product_journey_lineage.trust.v1";

/// Verifier-pinned repository lineage root key identifier.
pub const PRODUCT_JOURNEY_LINEAGE_ROOT_KEY_ID: &str =
    "ft.product-journey-lineage.repository-root.2026-08";

/// Verifier-pinned raw Ed25519 public key for lineage trust policy v1.
pub const PRODUCT_JOURNEY_LINEAGE_ROOT_PUBLIC_KEY_HEX: &str =
    "bccd8e321f07a395b30d9f80e54e40e72d8b4a221e6bc01d520341dab0ffd977";

/// Revision 1 never had retained bytes and must never acquire an identity.
pub const PRODUCT_JOURNEY_UNRETAINED_REVISION: &str = "2026-07-27.1";

/// Revision 2 is the first retained product-catalog artifact.
pub const PRODUCT_JOURNEY_GENESIS_REVISION: &str = "2026-07-27.2";

/// Exact Git commit containing the revision-2 genesis catalog.
pub const PRODUCT_JOURNEY_GENESIS_COMMIT: &str = "32d72991856a9b00d55086ca07384dc082b8a3fc";

/// Exact Git tree containing the revision-2 genesis catalog.
pub const PRODUCT_JOURNEY_GENESIS_TREE: &str = "9e02a16588e6a3e63f2655bc1fa6f6a5bce70b01";

/// Exact parent of the revision-2 genesis commit.
pub const PRODUCT_JOURNEY_GENESIS_PARENT: &str = "524d1e76e44167bd39440ab56e8d0d3556f451e3";

/// Exact Git blob containing the revision-2 genesis catalog bytes.
pub const PRODUCT_JOURNEY_GENESIS_BLOB: &str = "0605d08fed53cb3c0f45b277bb7d71021ffcf6f3";

/// SHA-256 of the exact raw revision-2 catalog bytes.
pub const PRODUCT_JOURNEY_GENESIS_RAW_SHA256: &str =
    "ee8c6b9c64d3530c428a6230a5661e21682b49ee1d1599f29043d43871241262";

/// Exact head revision pinned by lineage verifier v1.
pub const PRODUCT_JOURNEY_LINEAGE_CURRENT_REVISION: &str = "2026-07-27.3";

/// Canonical record digest of the exact v1 head, preventing tail truncation.
pub const PRODUCT_JOURNEY_LINEAGE_CURRENT_RECORD_SHA256: &str =
    "381cfaa037e3b3e85a71eee0b3273eabbca8da3d1d345a2ef41c78afe5a89b9c";

/// Revision 1's only truthful status explanation.
pub const PRODUCT_JOURNEY_UNRETAINED_REASON: &str = "No revision-1 catalog bytes, Git object, content digest, or signature were retained; this row records an uncommitted draft label only.";

/// Maximum raw JSON document accepted by the bounded decoder.
pub const MAX_PRODUCT_JOURNEY_CATALOG_BYTES: usize = 2 * 1024 * 1024;

/// Maximum raw lineage manifest accepted by the bounded decoder.
pub const MAX_PRODUCT_JOURNEY_LINEAGE_BYTES: usize = 256 * 1024;

/// Number of required product-journey coverage cells.
pub const REQUIRED_COVERAGE_CELL_COUNT: usize = 32;

/// Number of exact field journeys governed by the catalog.
pub const REQUIRED_FIELD_JOURNEY_COUNT: usize = 14;

/// Breaking successor contract for typed six-phase journey lifecycles.
pub const PRODUCT_JOURNEY_CONTRACT_ID_V2: &str = "ft.product_journey_catalog.v2";

/// Schema version for the typed six-phase lifecycle migration contract.
pub const PRODUCT_JOURNEY_SCHEMA_VERSION_V2: u32 = 2;

/// Bead that owns the breaking lifecycle migration.
pub const PRODUCT_JOURNEY_LIFECYCLE_SOURCE_BEAD_ID_V2: &str =
    "ft-interactive-swarm-product-convergence-7xqz4.1.1.3";

/// Exact signed v1 head whose positional lifecycle semantics remain unproved.
pub const PRODUCT_JOURNEY_V1_PREDECESSOR_RAW_SHA256: &str =
    "dd3b8a2bdd73c369152b291775205bbfc0bc1a0c9d41bf1bd091b53807408a54";

/// Number of typed lifecycle roles required for every v2 journey.
pub const REQUIRED_LIFECYCLE_PHASE_COUNT_V2: usize = 6;

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

/// Review outcome declared in catalog metadata.
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

/// Explicit statement about the evidentiary value of v1 positional lifecycle fields.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LegacyPositionalLifecycleSemanticsV2 {
    /// V1 string positions cannot be promoted into typed lifecycle proof.
    Unproved,
}

/// Closed lifecycle roles used by the breaking v2 contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JourneyLifecycleRoleV2 {
    /// Bind candidate, target, route, topology, and workload identities before mutation.
    IdentityPreflight,
    /// Establish a clean baseline only after identity preflight succeeds.
    CleanSetup,
    /// Exercise the intended user or machine workflow.
    SteadyWork,
    /// Introduce the declared failure, pressure, or overload condition.
    FailureOverload,
    /// Recover and converge authoritative user intent after the observed failure.
    RecoveryConvergence,
    /// Release resources and retain a final outcome.
    TeardownOutcome,
}

impl JourneyLifecycleRoleV2 {
    /// Every role, in lifecycle order.
    pub const ALL: [Self; REQUIRED_LIFECYCLE_PHASE_COUNT_V2] = [
        Self::IdentityPreflight,
        Self::CleanSetup,
        Self::SteadyWork,
        Self::FailureOverload,
        Self::RecoveryConvergence,
        Self::TeardownOutcome,
    ];

    /// Stable serialized field label.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::IdentityPreflight => "identity_preflight",
            Self::CleanSetup => "clean_setup",
            Self::SteadyWork => "steady_work",
            Self::FailureOverload => "failure_overload",
            Self::RecoveryConvergence => "recovery_convergence",
            Self::TeardownOutcome => "teardown_outcome",
        }
    }
}

/// Identity domains that a lifecycle phase must bind explicitly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JourneyIdentityRequirementV2 {
    /// Exact packaged candidate or component build identity.
    ExactCandidate,
    /// Exact controller target and operating-system identity.
    ControllerTarget,
    /// Exact session-host target and operating-system identity.
    SessionHostTarget,
    /// Exact topology and transport identity.
    TopologyTransport,
    /// Exact route or local endpoint identity.
    Route,
    /// Exact mux server, domain, and session generation.
    MuxSession,
    /// Exact pane inventory and fleet qualification point.
    PaneFleet,
    /// Exact renderer and display identity when presentation is in scope.
    RendererDisplay,
    /// Exact configuration, policy, and authority posture.
    ConfigurationPolicy,
    /// Exact workload and actor versions or deterministic fixture identities.
    WorkloadActors,
}

/// Preconditions that order the six lifecycle phases.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JourneyPhasePreconditionV2 {
    /// No product or workload mutation has occurred yet.
    UnmodifiedStartingState,
    /// Identity preflight completed successfully.
    IdentityPreflightPassed,
    /// Clean setup completed successfully.
    CleanSetupComplete,
    /// Steady work has begun and the system is observable.
    SteadyWorkStarted,
    /// The declared failure or overload was actually observed.
    FailureOverloadObserved,
    /// Recovery was attempted and its terminal state is known.
    RecoveryAttempted,
}

/// Mutation classes permitted within one lifecycle role.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JourneyMutationClassV2 {
    /// Establish the declared clean baseline.
    CleanSetup,
    /// Perform intended user or machine work.
    UserWork,
    /// Inject or induce the declared failure/pressure condition.
    FaultOrOverloadInjection,
    /// Apply a recovery or convergence action.
    RecoveryAction,
    /// Release resources and close authority.
    ResourceRelease,
}

/// Required terminal outcomes for a lifecycle role.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JourneyPhaseOutcomeV2 {
    /// Every required identity was bound before mutation.
    IdentityBound,
    /// A declared clean baseline is ready.
    CleanBaselineReady,
    /// Intended steady work reached its declared terminal boundary.
    SteadyWorkCompleted,
    /// The declared failure or overload was observed rather than inferred.
    FailureObserved,
    /// Authoritative user intent and product state converged.
    AuthoritativeStateConverged,
    /// Resources and transient authority were released.
    ResourcesReleased,
    /// A final success, degraded, failed, cancelled, or indeterminate outcome was retained.
    FinalOutcomeRecorded,
}

/// Evidence classes retained by one lifecycle role.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JourneyPhaseEvidenceV2 {
    /// Candidate, target, topology, route, and workload identity receipt.
    IdentityReceipt,
    /// Clean-setup and precondition receipt.
    SetupReceipt,
    /// Intended-work and user-outcome receipt.
    WorkReceipt,
    /// Failure-injection and observed-failure receipt.
    FailureReceipt,
    /// Recovery and convergence receipt.
    ConvergenceReceipt,
    /// Cleanup, final-state, and terminal-outcome receipt.
    TeardownReceipt,
}

/// Cancellation semantics frozen for each lifecycle role.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JourneyCancellationSemanticsV2 {
    /// Stop before any product or workload mutation.
    AbortBeforeMutation,
    /// Roll back or retain an explicit resumable setup receipt.
    RollbackOrResumeSetup,
    /// Preserve acknowledged user intent and expose the cancellation boundary.
    PreserveUserIntent,
    /// Preserve already-observed failure evidence without inventing recovery.
    PreserveFailureEvidence,
    /// Retain a retryable recovery checkpoint and exact pending intent.
    CheckpointRetryableRecovery,
    /// Cleanup remains required and incomplete cleanup is reported explicitly.
    CleanupRemainsRequired,
}

/// Failure semantics frozen for each lifecycle role.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JourneyFailureSemanticsV2 {
    /// Identity uncertainty stops the journey before mutation.
    StopBeforeMutation,
    /// Setup failure is explicit and cannot masquerade as a clean baseline.
    SetupFailureIsExplicit,
    /// Steady-work failure is explicit and cannot be promoted as completion.
    WorkFailureIsExplicit,
    /// The injected failure is test input; inability to observe it fails the phase.
    FailureIsTestInput,
    /// Failure to converge is a terminal journey failure or typed degraded outcome.
    NonConvergenceIsFailure,
    /// Incomplete cleanup or missing final outcome fails teardown.
    IncompleteTeardownIsFailure,
}

/// Canonical semantic contract referenced by every journey phase of one role.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JourneyLifecyclePhaseContractV2 {
    /// Lifecycle role governed by this contract.
    pub role: JourneyLifecycleRoleV2,
    /// Identity domains that must be bound for this role.
    pub required_identities: Vec<JourneyIdentityRequirementV2>,
    /// Preconditions that must be established before this role begins.
    pub required_preconditions: Vec<JourneyPhasePreconditionV2>,
    /// Mutation classes permitted in this role; empty for identity preflight.
    pub allowed_mutations: Vec<JourneyMutationClassV2>,
    /// Outcomes that must be observed before the role completes.
    pub required_outcomes: Vec<JourneyPhaseOutcomeV2>,
    /// Evidence classes required from this role.
    pub required_evidence: Vec<JourneyPhaseEvidenceV2>,
    /// Cancellation behavior for this role.
    pub cancellation: JourneyCancellationSemanticsV2,
    /// Failure behavior for this role.
    pub failure_semantics: JourneyFailureSemanticsV2,
}

/// One explicitly typed journey phase bound to a canonical role contract.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JourneyLifecyclePhaseV2 {
    /// Role contract that governs these journey-specific steps.
    pub contract_role: JourneyLifecycleRoleV2,
    /// Non-empty journey-specific actions and assertions for this role.
    pub steps: Vec<String>,
}

/// Six explicit lifecycle fields for one v2 journey producer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JourneyLifecycleV2 {
    /// Pre-mutation identity and authority binding.
    pub identity_preflight: JourneyLifecyclePhaseV2,
    /// Clean baseline establishment.
    pub clean_setup: JourneyLifecyclePhaseV2,
    /// Intended workflow execution.
    pub steady_work: JourneyLifecyclePhaseV2,
    /// Failure and overload exercise.
    pub failure_overload: JourneyLifecyclePhaseV2,
    /// Recovery and authoritative convergence.
    pub recovery_convergence: JourneyLifecyclePhaseV2,
    /// Cleanup and retained final outcome.
    pub teardown_outcome: JourneyLifecyclePhaseV2,
}

/// Explicit migration record for one of the fourteen field-journey producers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JourneyLifecycleProducerV2 {
    /// Stable journey identifier retained from the signed v1 head.
    pub journey_id: String,
    /// Exact field-journey Bead that owns this producer.
    pub field_bead_id: String,
    /// Complete typed six-phase lifecycle.
    pub lifecycle: JourneyLifecycleV2,
}

/// Explicit v2 migration record for one of the 32 release-closure consumers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JourneyClosureConsumerV2 {
    /// Stable claim identifier retained from the signed v1 head.
    pub claim_id: String,
    /// Exact persona/fleet/topology cell consumed at release closure.
    pub coverage: CoverageKey,
    /// Explicit typed journey producers required by this cell.
    pub journey_ids: Vec<String>,
}

/// Breaking v2 migration catalog for typed journey lifecycles.
///
/// This contract deliberately does not deserialize or upgrade v1 positional
/// lifecycle arrays. It binds the signed v1 inventory by digest, marks its
/// positional meaning unproved, and requires all fourteen producers and all
/// thirty-two closure consumers to be restated explicitly.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProductJourneyCatalogV2 {
    /// Must equal [`PRODUCT_JOURNEY_CONTRACT_ID_V2`].
    pub contract_id: String,
    /// Must equal [`PRODUCT_JOURNEY_SCHEMA_VERSION_V2`].
    pub schema_version: u32,
    /// Content revision of this unsigned migration contract.
    pub catalog_revision: String,
    /// Bead that owns the breaking lifecycle migration.
    pub source_bead_id: String,
    /// V2 remains a contract and cannot mint product support.
    pub catalog_claim_state: CatalogClaimState,
    /// Historical v1 contract identifier; never treated as v2 data.
    pub predecessor_contract_id: String,
    /// Exact signed v1 head revision.
    pub predecessor_catalog_revision: String,
    /// Exact raw SHA-256 of the signed v1 head.
    pub predecessor_raw_sha256: String,
    /// Mandatory negative-evidence statement about v1 positions.
    pub v1_positional_lifecycle_semantics: LegacyPositionalLifecycleSemanticsV2,
    /// Canonical closed contracts for all six phase roles.
    pub phase_contracts: Vec<JourneyLifecyclePhaseContractV2>,
    /// Explicitly migrated fourteen journey producers.
    pub journey_producers: Vec<JourneyLifecycleProducerV2>,
    /// Explicitly migrated thirty-two release-closure consumers.
    pub closure_consumers: Vec<JourneyClosureConsumerV2>,
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

/// Declared catalog-revision metadata entry.
///
/// Schema v1 retains these rows in the current document, but it does not carry
/// content-addressed predecessor snapshots or signatures. Consumers must not
/// treat a row by itself as proof that the named prior revision was retained.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChangeRecord {
    /// Stable change identifier.
    pub change_id: String,
    /// Catalog content revision described by this row.
    pub catalog_revision: String,
    /// UTC timestamp as retained by the catalog artifact.
    pub changed_at_utc: String,
    /// Human-readable change summary.
    pub summary: String,
    /// Relative repository references motivating the change.
    pub source_refs: Vec<String>,
}

/// Retention role of one declared catalog revision.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CatalogRevisionRetention {
    /// A label survived, but no bytes or content identity did.
    UnretainedUncommittedDraft,
    /// The first retained artifact and trust-chain root.
    RetainedGenesis,
    /// A retained artifact bound to its exact predecessor.
    RetainedSuccessor,
}

/// Whether Git itself carried a verified signature for a retained commit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CatalogGitCommitSignature {
    /// The historical commit was not signed.
    Unsigned,
}

/// Immutable content and source-control identity for one catalog snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CatalogSnapshotReceipt {
    /// Catalog revision encoded by the referenced bytes.
    pub catalog_revision: String,
    /// Content-addressed resolver key for the exact raw bytes.
    pub snapshot_ref: String,
    /// Lowercase SHA-256 of the exact raw snapshot bytes.
    pub raw_sha256: String,
    /// Exact forty-hex Git commit retaining the bytes.
    pub git_commit: String,
    /// Exact forty-hex root tree of `git_commit`.
    pub git_tree: String,
    /// Exact forty-hex first parent of `git_commit`.
    pub git_parent: String,
    /// Exact forty-hex Git blob containing the raw snapshot bytes.
    pub catalog_blob: String,
    /// Historical Git commit-signature state, distinct from the lineage signature.
    pub git_commit_signature: CatalogGitCommitSignature,
}

/// Key authorized to sign a later retained lineage record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CatalogLineageDelegatedKey {
    /// Stable key identifier pinned by the verifier trust policy.
    pub key_id: String,
}

/// Explicit successor-signer policy authenticated by the current record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CatalogLineageDelegation {
    /// Non-empty exact set of keys allowed to sign the next retained record.
    pub authorized_successor_keys: Vec<CatalogLineageDelegatedKey>,
}

/// One catalog revision in the content-addressed lineage.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CatalogLineageRecord {
    /// Exact catalog revision governed by this record.
    pub catalog_revision: String,
    /// Whether this revision has bytes and where it sits in the chain.
    pub retention: CatalogRevisionRetention,
    /// Exact current snapshot for retained revisions; absent for revision 1.
    pub snapshot: Option<CatalogSnapshotReceipt>,
    /// Exact prior retained snapshot for successors only.
    pub predecessor_snapshot: Option<CatalogSnapshotReceipt>,
    /// Unretained predecessor label acknowledged by the genesis record.
    pub unretained_predecessor_revision: Option<String>,
    /// Truthful explanation for a revision with no retained bytes.
    pub no_data_reason: Option<String>,
    /// Key that signed this retained record.
    pub signer_key_id: Option<String>,
    /// Successor-signer policy authenticated by this retained record.
    pub delegation: Option<CatalogLineageDelegation>,
    /// SHA-256 of the versioned canonical projection, excluding this field and signature.
    pub canonical_record_sha256: Option<String>,
    /// Detached Ed25519 signature over the domain-separated canonical digest.
    pub signature_ed25519_hex: Option<String>,
}

/// Content-addressed, signed lineage envelope for the product catalog.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProductJourneyLineageManifest {
    /// Contract identifier; must equal [`PRODUCT_JOURNEY_LINEAGE_CONTRACT_ID`].
    pub contract_id: String,
    /// Schema version; must equal [`PRODUCT_JOURNEY_LINEAGE_SCHEMA_VERSION`].
    pub schema_version: u32,
    /// Canonical record projection identifier.
    pub canonical_projection: String,
    /// Canonical record digest domain.
    pub digest_domain: String,
    /// Detached signature domain.
    pub signature_domain: String,
    /// Verifier-pinned key/delegation policy identifier.
    pub trust_policy_id: String,
    /// Current catalog revision; must equal the last retained record.
    pub current_catalog_revision: String,
    /// Ordered complete history, including the explicit revision-1 NO-DATA row.
    pub records: Vec<CatalogLineageRecord>,
}

/// One verifier-pinned Ed25519 public key.
#[derive(Debug, Clone, PartialEq, Eq)]
struct CatalogLineageTrustedKey {
    /// Stable identifier referenced by records and delegations.
    key_id: String,
    /// Lowercase hex encoding of the exact 32-byte Ed25519 public key.
    public_key_hex: String,
    /// Whether this key may sign the genesis record.
    genesis_authority: bool,
}

/// Trust roots compiled into the offline lineage verifier.
#[derive(Debug, Clone, PartialEq, Eq)]
struct CatalogLineageTrustPolicy {
    /// Identifier bound by the manifest.
    policy_id: String,
    /// Closed set of trusted lineage-signing keys.
    keys: Vec<CatalogLineageTrustedKey>,
}

impl CatalogLineageTrustPolicy {
    /// Exact trust roots compiled into the v1 offline verifier.
    #[must_use]
    fn pinned_v1() -> Self {
        Self {
            policy_id: PRODUCT_JOURNEY_LINEAGE_TRUST_POLICY_ID.to_string(),
            keys: vec![CatalogLineageTrustedKey {
                key_id: PRODUCT_JOURNEY_LINEAGE_ROOT_KEY_ID.to_string(),
                public_key_hex: PRODUCT_JOURNEY_LINEAGE_ROOT_PUBLIC_KEY_HEX.to_string(),
                genesis_authority: true,
            }],
        }
    }
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
    /// Declared review metadata.
    pub review_history: Vec<ReviewRecord>,
    /// Declared revision-lineage metadata.
    pub change_history: Vec<ChangeRecord>,
}

impl ProductJourneyCatalog {
    /// Decode one bounded JSON document and reject unknown or trailing data.
    pub fn decode_json_bounded(raw: &[u8]) -> Result<Self, ProductJourneyDecodeError> {
        decode_product_journey_catalog(raw)
    }

    /// Validate all semantic invariants that JSON Schema cannot express.
    #[must_use]
    pub fn validate(&self) -> CatalogValidationReport {
        validate_product_journey_catalog(self)
    }
}

impl ProductJourneyCatalogV2 {
    /// Decode one bounded v2 migration catalog without accepting v1 data.
    pub fn decode_json_bounded(raw: &[u8]) -> Result<Self, ProductJourneyDecodeError> {
        decode_product_journey_catalog_v2(raw)
    }

    /// Validate the breaking lifecycle migration without file I/O.
    #[must_use]
    pub fn validate(&self) -> CatalogValidationReport {
        validate_product_journey_catalog_v2(self)
    }
}

/// Stable bounded-decoder failure category.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProductJourneyDecodeCode {
    /// Raw bytes exceed the catalog limit.
    PayloadTooLarge,
    /// The first JSON value is malformed or has the wrong closed shape.
    InvalidJson,
    /// Non-whitespace data follows the first valid catalog value.
    TrailingData,
}

impl ProductJourneyDecodeCode {
    /// Stable machine-facing code.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::PayloadTooLarge => "PJC-DECODE-001",
            Self::InvalidJson => "PJC-DECODE-002",
            Self::TrailingData => "PJC-DECODE-003",
        }
    }
}

/// Failure returned by [`decode_product_journey_catalog`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ProductJourneyDecodeError {
    /// Raw input exceeds the pre-deserialization limit.
    PayloadTooLarge {
        /// Observed byte count.
        actual_bytes: usize,
        /// Configured maximum byte count.
        max_bytes: usize,
    },
    /// Serde rejected malformed JSON, an unknown field, or a wrong type/value.
    InvalidJson {
        /// Parser diagnostic; callers should route on [`Self::code`].
        detail: String,
    },
    /// A valid first value was followed by non-whitespace data.
    TrailingData {
        /// Parser diagnostic; callers should route on [`Self::code`].
        detail: String,
    },
}

impl ProductJourneyDecodeError {
    /// Stable decoder category.
    #[must_use]
    pub const fn code(&self) -> ProductJourneyDecodeCode {
        match self {
            Self::PayloadTooLarge { .. } => ProductJourneyDecodeCode::PayloadTooLarge,
            Self::InvalidJson { .. } => ProductJourneyDecodeCode::InvalidJson,
            Self::TrailingData { .. } => ProductJourneyDecodeCode::TrailingData,
        }
    }
}

impl fmt::Display for ProductJourneyDecodeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::PayloadTooLarge {
                actual_bytes,
                max_bytes,
            } => write!(
                formatter,
                "{}: product journey catalog is {actual_bytes} bytes (maximum {max_bytes})",
                self.code().as_str()
            ),
            Self::InvalidJson { detail } | Self::TrailingData { detail } => {
                write!(formatter, "{}: {detail}", self.code().as_str())
            }
        }
    }
}

impl Error for ProductJourneyDecodeError {}

/// Decode one bounded, closed-shape JSON catalog.
///
/// The byte limit is checked before Serde allocates catalog vectors. Serde's
/// `deny_unknown_fields` annotations reject schema drift and duplicate struct
/// or tagged-enum fields, while `end()` rejects any second value or other
/// trailing non-whitespace input. Semantic validation remains a separate,
/// explicit step so decoding cannot be mistaken for product-proof authority.
pub fn decode_product_journey_catalog(
    raw: &[u8],
) -> Result<ProductJourneyCatalog, ProductJourneyDecodeError> {
    if raw.len() > MAX_PRODUCT_JOURNEY_CATALOG_BYTES {
        return Err(ProductJourneyDecodeError::PayloadTooLarge {
            actual_bytes: raw.len(),
            max_bytes: MAX_PRODUCT_JOURNEY_CATALOG_BYTES,
        });
    }

    let mut decoder = serde_json::Deserializer::from_slice(raw);
    let catalog = ProductJourneyCatalog::deserialize(&mut decoder).map_err(|error| {
        ProductJourneyDecodeError::InvalidJson {
            detail: error.to_string(),
        }
    })?;
    decoder
        .end()
        .map_err(|error| ProductJourneyDecodeError::TrailingData {
            detail: error.to_string(),
        })?;
    Ok(catalog)
}

/// Decode one bounded, closed-shape v2 lifecycle-migration catalog.
///
/// This entry point accepts only [`ProductJourneyCatalogV2`]. In particular,
/// there is no fallback decoder or conversion from v1 positional arrays.
pub fn decode_product_journey_catalog_v2(
    raw: &[u8],
) -> Result<ProductJourneyCatalogV2, ProductJourneyDecodeError> {
    if raw.len() > MAX_PRODUCT_JOURNEY_CATALOG_BYTES {
        return Err(ProductJourneyDecodeError::PayloadTooLarge {
            actual_bytes: raw.len(),
            max_bytes: MAX_PRODUCT_JOURNEY_CATALOG_BYTES,
        });
    }

    let mut decoder = serde_json::Deserializer::from_slice(raw);
    let catalog = ProductJourneyCatalogV2::deserialize(&mut decoder).map_err(|error| {
        ProductJourneyDecodeError::InvalidJson {
            detail: error.to_string(),
        }
    })?;
    decoder
        .end()
        .map_err(|error| ProductJourneyDecodeError::TrailingData {
            detail: error.to_string(),
        })?;
    Ok(catalog)
}

/// Stable bounded-decoder failure for a lineage manifest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProductJourneyLineageDecodeError {
    /// Raw bytes exceed the lineage-manifest limit.
    PayloadTooLarge {
        /// Observed byte count.
        actual_bytes: usize,
        /// Configured maximum byte count.
        max_bytes: usize,
    },
    /// Serde rejected malformed JSON, a duplicate, an unknown field, or a wrong type.
    InvalidJson {
        /// Parser diagnostic.
        detail: String,
    },
    /// A valid first value was followed by non-whitespace data.
    TrailingData {
        /// Parser diagnostic.
        detail: String,
    },
}

impl fmt::Display for ProductJourneyLineageDecodeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::PayloadTooLarge {
                actual_bytes,
                max_bytes,
            } => write!(
                formatter,
                "product journey lineage is {actual_bytes} bytes (maximum {max_bytes})"
            ),
            Self::InvalidJson { detail } | Self::TrailingData { detail } => {
                formatter.write_str(detail)
            }
        }
    }
}

impl Error for ProductJourneyLineageDecodeError {}

impl ProductJourneyLineageManifest {
    /// Decode one bounded, closed-shape lineage manifest.
    pub fn decode_json_bounded(raw: &[u8]) -> Result<Self, ProductJourneyLineageDecodeError> {
        if raw.len() > MAX_PRODUCT_JOURNEY_LINEAGE_BYTES {
            return Err(ProductJourneyLineageDecodeError::PayloadTooLarge {
                actual_bytes: raw.len(),
                max_bytes: MAX_PRODUCT_JOURNEY_LINEAGE_BYTES,
            });
        }
        let mut decoder = serde_json::Deserializer::from_slice(raw);
        let manifest = Self::deserialize(&mut decoder).map_err(|error| {
            ProductJourneyLineageDecodeError::InvalidJson {
                detail: error.to_string(),
            }
        })?;
        decoder
            .end()
            .map_err(|error| ProductJourneyLineageDecodeError::TrailingData {
                detail: error.to_string(),
            })?;
        Ok(manifest)
    }
}

/// Stable fail-closed lineage-verification category.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum CatalogLineageValidationCode {
    /// Manifest contract identifier is not implemented.
    UnknownContract,
    /// Manifest schema version is not implemented.
    UnknownSchemaVersion,
    /// Canonical projection or digest/signature domain drifted.
    UnknownCanonicalDomain,
    /// The supplied trust policy is not the policy bound by the manifest.
    UnknownTrustPolicy,
    /// The history is missing, duplicated, or out of order.
    InvalidHistoryOrder,
    /// Revision 1 attempted to acquire invented bytes or authority.
    InventedUnretainedHistory,
    /// Revision 2 is absent or differs from the exact genesis identity.
    InvalidGenesis,
    /// A retained snapshot receipt is malformed or copied.
    InvalidSnapshotIdentity,
    /// Snapshot bytes required by a receipt were not supplied.
    MissingSnapshot,
    /// Snapshot raw bytes disagree with their signed SHA-256.
    SnapshotDigestMismatch,
    /// Snapshot bytes do not decode to the receipt's catalog revision.
    SnapshotCatalogMismatch,
    /// A successor does not embed the exact prior retained snapshot.
    PredecessorMismatch,
    /// The stored canonical digest disagrees with the versioned projection.
    CanonicalDigestMismatch,
    /// A retained record has no signer or detached signature.
    MissingSignature,
    /// A signer or delegated successor key is absent from the trust policy.
    UnknownSigner,
    /// Strict Ed25519 verification failed.
    InvalidSignature,
    /// A signer was not authorized by genesis policy or its predecessor.
    InvalidDelegation,
}

impl CatalogLineageValidationCode {
    /// Stable machine-facing code.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::UnknownContract => "PJC-LINEAGE-CONTRACT-001",
            Self::UnknownSchemaVersion => "PJC-LINEAGE-SCHEMA-001",
            Self::UnknownCanonicalDomain => "PJC-LINEAGE-DOMAIN-001",
            Self::UnknownTrustPolicy => "PJC-LINEAGE-TRUST-001",
            Self::InvalidHistoryOrder => "PJC-LINEAGE-HISTORY-001",
            Self::InventedUnretainedHistory => "PJC-LINEAGE-HISTORY-002",
            Self::InvalidGenesis => "PJC-LINEAGE-GENESIS-001",
            Self::InvalidSnapshotIdentity => "PJC-LINEAGE-SNAPSHOT-001",
            Self::MissingSnapshot => "PJC-LINEAGE-SNAPSHOT-002",
            Self::SnapshotDigestMismatch => "PJC-LINEAGE-SNAPSHOT-003",
            Self::SnapshotCatalogMismatch => "PJC-LINEAGE-SNAPSHOT-004",
            Self::PredecessorMismatch => "PJC-LINEAGE-PREDECESSOR-001",
            Self::CanonicalDigestMismatch => "PJC-LINEAGE-DIGEST-001",
            Self::MissingSignature => "PJC-LINEAGE-SIGNATURE-001",
            Self::UnknownSigner => "PJC-LINEAGE-SIGNATURE-002",
            Self::InvalidSignature => "PJC-LINEAGE-SIGNATURE-003",
            Self::InvalidDelegation => "PJC-LINEAGE-DELEGATION-001",
        }
    }
}

/// One deterministic lineage-verification failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogLineageValidationError {
    /// Stable error category.
    pub code: CatalogLineageValidationCode,
    /// JSON-style location of the failing field.
    pub path: String,
    /// Human-readable diagnostic.
    pub detail: String,
}

/// Aggregate offline verification report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogLineageValidationReport {
    /// True only when every structural, content, signature, and delegation check passed.
    pub valid: bool,
    /// Deterministically ordered verification errors.
    pub errors: Vec<CatalogLineageValidationError>,
}

impl CatalogLineageValidationReport {
    /// Whether this report contains a particular stable category.
    #[must_use]
    pub fn contains_code(&self, code: CatalogLineageValidationCode) -> bool {
        self.errors.iter().any(|error| error.code == code)
    }
}

#[derive(Serialize)]
struct CatalogLineageRecordProjection<'a> {
    projection: &'static str,
    catalog_revision: &'a str,
    retention: CatalogRevisionRetention,
    snapshot: &'a Option<CatalogSnapshotReceipt>,
    predecessor_snapshot: &'a Option<CatalogSnapshotReceipt>,
    unretained_predecessor_revision: &'a Option<String>,
    no_data_reason: &'a Option<String>,
    signer_key_id: &'a Option<String>,
    delegation: &'a Option<CatalogLineageDelegation>,
}

/// Compute the canonical, domain-separated SHA-256 for one lineage record.
///
/// The projection deliberately omits `canonical_record_sha256` and
/// `signature_ed25519_hex`, so neither digest nor signature is self-referential.
pub fn product_journey_lineage_record_digest(
    record: &CatalogLineageRecord,
) -> Result<[u8; 32], serde_json::Error> {
    let projection = CatalogLineageRecordProjection {
        projection: PRODUCT_JOURNEY_LINEAGE_PROJECTION,
        catalog_revision: &record.catalog_revision,
        retention: record.retention,
        snapshot: &record.snapshot,
        predecessor_snapshot: &record.predecessor_snapshot,
        unretained_predecessor_revision: &record.unretained_predecessor_revision,
        no_data_reason: &record.no_data_reason,
        signer_key_id: &record.signer_key_id,
        delegation: &record.delegation,
    };
    let canonical = serde_json::to_vec(&projection)?;
    let mut hasher = Sha256::new();
    hasher.update(PRODUCT_JOURNEY_LINEAGE_DIGEST_DOMAIN.as_bytes());
    hasher.update([0]);
    hasher.update(canonical);
    Ok(hasher.finalize().into())
}

/// Construct the exact domain-separated bytes covered by Ed25519.
pub fn product_journey_lineage_signature_message(
    record: &CatalogLineageRecord,
) -> Result<Vec<u8>, serde_json::Error> {
    let digest = product_journey_lineage_record_digest(record)?;
    let mut message = Vec::with_capacity(PRODUCT_JOURNEY_LINEAGE_SIGNATURE_DOMAIN.len() + 33);
    message.extend_from_slice(PRODUCT_JOURNEY_LINEAGE_SIGNATURE_DOMAIN.as_bytes());
    message.push(0);
    message.extend_from_slice(&digest);
    Ok(message)
}

fn bytes_to_lower_hex(bytes: &[u8]) -> String {
    let mut encoded = String::with_capacity(bytes.len().saturating_mul(2));
    for byte in bytes {
        write!(&mut encoded, "{byte:02x}").expect("writing to String cannot fail");
    }
    encoded
}

fn decode_lower_hex_array<const N: usize>(encoded: &str) -> Option<[u8; N]> {
    if encoded.len() != N.saturating_mul(2)
        || !encoded
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return None;
    }
    let mut decoded = [0_u8; N];
    for (index, output) in decoded.iter_mut().enumerate() {
        let start = index.saturating_mul(2);
        *output = u8::from_str_radix(&encoded[start..start + 2], 16).ok()?;
    }
    Some(decoded)
}

fn is_lower_hex(encoded: &str, expected_len: usize) -> bool {
    encoded.len() == expected_len
        && encoded
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn catalog_revision_sort_key(revision: &str) -> Option<(u32, u32, u32, u64)> {
    let (date, sequence) = revision.rsplit_once('.')?;
    if sequence.starts_with('0') || sequence.is_empty() {
        return None;
    }
    let mut date_parts = date.split('-');
    let year_text = date_parts.next()?;
    let month_text = date_parts.next()?;
    let day_text = date_parts.next()?;
    if date_parts.next().is_some()
        || year_text.len() != 4
        || month_text.len() != 2
        || day_text.len() != 2
        || !year_text.bytes().all(|byte| byte.is_ascii_digit())
        || !month_text.bytes().all(|byte| byte.is_ascii_digit())
        || !day_text.bytes().all(|byte| byte.is_ascii_digit())
        || !sequence.bytes().all(|byte| byte.is_ascii_digit())
    {
        return None;
    }
    let year = year_text.parse().ok()?;
    let month = month_text.parse().ok()?;
    let day = day_text.parse().ok()?;
    let sequence = sequence.parse().ok()?;
    if year == 0
        || !(1..=12).contains(&month)
        || !(1..=days_in_gregorian_month(year, month)).contains(&day)
        || sequence == 0
    {
        return None;
    }
    Some((year, month, day, sequence))
}

const fn days_in_gregorian_month(year: u32, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if is_gregorian_leap_year(year) => 29,
        2 => 28,
        _ => 0,
    }
}

const fn is_gregorian_leap_year(year: u32) -> bool {
    year.is_multiple_of(4) && (!year.is_multiple_of(100) || year.is_multiple_of(400))
}

fn is_direct_catalog_revision_successor(
    previous: (u32, u32, u32, u64),
    current: (u32, u32, u32, u64),
) -> bool {
    let previous_date = (previous.0, previous.1, previous.2);
    let current_date = (current.0, current.1, current.2);
    if current_date == previous_date {
        previous
            .3
            .checked_add(1)
            .is_some_and(|expected| current.3 == expected)
    } else {
        current_date > previous_date && current.3 == 1
    }
}

struct CatalogLineageValidator {
    errors: Vec<CatalogLineageValidationError>,
}

impl CatalogLineageValidator {
    fn error(
        &mut self,
        code: CatalogLineageValidationCode,
        path: impl Into<String>,
        detail: impl Into<String>,
    ) {
        self.errors.push(CatalogLineageValidationError {
            code,
            path: path.into(),
            detail: detail.into(),
        });
    }
}

fn verify_snapshot_receipt(
    snapshot: &CatalogSnapshotReceipt,
    path: &str,
    supplied_snapshots: &BTreeMap<String, Vec<u8>>,
    seen_snapshot_refs: &mut BTreeSet<String>,
    seen_snapshot_digests: &mut BTreeSet<String>,
    validator: &mut CatalogLineageValidator,
) {
    let expected_ref = format!(
        "docs/design/product-journey-catalog.snapshots/{}.json#sha256={}",
        snapshot.catalog_revision, snapshot.raw_sha256
    );
    if snapshot.snapshot_ref != expected_ref
        || !is_lower_hex(&snapshot.raw_sha256, 64)
        || !is_lower_hex(&snapshot.git_commit, 40)
        || !is_lower_hex(&snapshot.git_tree, 40)
        || !is_lower_hex(&snapshot.git_parent, 40)
        || !is_lower_hex(&snapshot.catalog_blob, 40)
    {
        validator.error(
            CatalogLineageValidationCode::InvalidSnapshotIdentity,
            path,
            "snapshot reference must bind the retained repository path and raw SHA-256, and every digest must be canonical lowercase hex",
        );
    }
    if !seen_snapshot_refs.insert(snapshot.snapshot_ref.clone())
        || !seen_snapshot_digests.insert(snapshot.raw_sha256.clone())
    {
        validator.error(
            CatalogLineageValidationCode::InvalidSnapshotIdentity,
            path,
            "retained revisions must not copy a snapshot reference or raw digest",
        );
    }
    let Some(raw) = supplied_snapshots.get(&snapshot.snapshot_ref) else {
        validator.error(
            CatalogLineageValidationCode::MissingSnapshot,
            format!("{path}.snapshot_ref"),
            format!(
                "snapshot bytes for `{}` were not supplied",
                snapshot.snapshot_ref
            ),
        );
        return;
    };
    if raw.len() > MAX_PRODUCT_JOURNEY_CATALOG_BYTES {
        validator.error(
            CatalogLineageValidationCode::SnapshotCatalogMismatch,
            format!("{path}.snapshot_ref"),
            format!(
                "snapshot is {} bytes, exceeding the bounded catalog limit of {} bytes",
                raw.len(),
                MAX_PRODUCT_JOURNEY_CATALOG_BYTES
            ),
        );
        return;
    }
    let actual_digest = bytes_to_lower_hex(&Sha256::digest(raw));
    if actual_digest != snapshot.raw_sha256 {
        validator.error(
            CatalogLineageValidationCode::SnapshotDigestMismatch,
            format!("{path}.raw_sha256"),
            format!(
                "snapshot bytes hash to `{actual_digest}`, not `{}`",
                snapshot.raw_sha256
            ),
        );
    }
    match decode_product_journey_catalog(raw) {
        Ok(catalog) if catalog.catalog_revision == snapshot.catalog_revision => {}
        Ok(catalog) => validator.error(
            CatalogLineageValidationCode::SnapshotCatalogMismatch,
            format!("{path}.catalog_revision"),
            format!(
                "snapshot encodes catalog revision `{}`, not `{}`",
                catalog.catalog_revision, snapshot.catalog_revision
            ),
        ),
        Err(error) => validator.error(
            CatalogLineageValidationCode::SnapshotCatalogMismatch,
            format!("{path}.snapshot_ref"),
            format!("snapshot is not a bounded product catalog: {error}"),
        ),
    }
}

fn validate_unretained_record(
    record: &CatalogLineageRecord,
    path: &str,
    validator: &mut CatalogLineageValidator,
) {
    if record.catalog_revision != PRODUCT_JOURNEY_UNRETAINED_REVISION
        || record.retention != CatalogRevisionRetention::UnretainedUncommittedDraft
        || record.snapshot.is_some()
        || record.predecessor_snapshot.is_some()
        || record.unretained_predecessor_revision.is_some()
        || record.no_data_reason.as_deref() != Some(PRODUCT_JOURNEY_UNRETAINED_REASON)
        || record.signer_key_id.is_some()
        || record.delegation.is_some()
        || record.canonical_record_sha256.is_some()
        || record.signature_ed25519_hex.is_some()
    {
        validator.error(
            CatalogLineageValidationCode::InventedUnretainedHistory,
            path,
            "revision 1 must remain an unsigned unretained_uncommitted_draft with NO-DATA identities",
        );
    }
}

fn validate_genesis_snapshot(
    snapshot: &CatalogSnapshotReceipt,
    record: &CatalogLineageRecord,
    path: &str,
    validator: &mut CatalogLineageValidator,
) {
    if record.catalog_revision != PRODUCT_JOURNEY_GENESIS_REVISION
        || record.retention != CatalogRevisionRetention::RetainedGenesis
        || record.predecessor_snapshot.is_some()
        || record.unretained_predecessor_revision.as_deref()
            != Some(PRODUCT_JOURNEY_UNRETAINED_REVISION)
        || record.no_data_reason.is_some()
        || snapshot.catalog_revision != PRODUCT_JOURNEY_GENESIS_REVISION
        || snapshot.git_commit != PRODUCT_JOURNEY_GENESIS_COMMIT
        || snapshot.git_tree != PRODUCT_JOURNEY_GENESIS_TREE
        || snapshot.git_parent != PRODUCT_JOURNEY_GENESIS_PARENT
        || snapshot.catalog_blob != PRODUCT_JOURNEY_GENESIS_BLOB
        || snapshot.raw_sha256 != PRODUCT_JOURNEY_GENESIS_RAW_SHA256
        || snapshot.git_commit_signature != CatalogGitCommitSignature::Unsigned
    {
        validator.error(
            CatalogLineageValidationCode::InvalidGenesis,
            path,
            "revision 2 must be the sole genesis and match its exact commit, tree, parent, blob, raw SHA-256, and unsigned Git-commit state",
        );
    }
}

/// Verify a lineage using only supplied immutable snapshot bytes and pinned keys.
///
/// No ambient `HEAD`, working-tree file, network service, or wall clock is
/// consulted. Callers must resolve every `snapshot_ref` to exact retained bytes.
#[must_use]
fn verify_product_journey_lineage_with_trust_policy(
    manifest: &ProductJourneyLineageManifest,
    supplied_snapshots: &BTreeMap<String, Vec<u8>>,
    trust_policy: &CatalogLineageTrustPolicy,
) -> CatalogLineageValidationReport {
    let mut validator = CatalogLineageValidator { errors: Vec::new() };
    if manifest.contract_id != PRODUCT_JOURNEY_LINEAGE_CONTRACT_ID {
        validator.error(
            CatalogLineageValidationCode::UnknownContract,
            "contract_id",
            format!("expected `{PRODUCT_JOURNEY_LINEAGE_CONTRACT_ID}`"),
        );
    }
    if manifest.schema_version != PRODUCT_JOURNEY_LINEAGE_SCHEMA_VERSION {
        validator.error(
            CatalogLineageValidationCode::UnknownSchemaVersion,
            "schema_version",
            format!("expected {PRODUCT_JOURNEY_LINEAGE_SCHEMA_VERSION}"),
        );
    }
    if manifest.canonical_projection != PRODUCT_JOURNEY_LINEAGE_PROJECTION
        || manifest.digest_domain != PRODUCT_JOURNEY_LINEAGE_DIGEST_DOMAIN
        || manifest.signature_domain != PRODUCT_JOURNEY_LINEAGE_SIGNATURE_DOMAIN
    {
        validator.error(
            CatalogLineageValidationCode::UnknownCanonicalDomain,
            "canonical_projection",
            "canonical projection and digest/signature domains must match the implemented v1 contract",
        );
    }
    if manifest.trust_policy_id != PRODUCT_JOURNEY_LINEAGE_TRUST_POLICY_ID
        || trust_policy.policy_id != PRODUCT_JOURNEY_LINEAGE_TRUST_POLICY_ID
        || trust_policy.policy_id != manifest.trust_policy_id
    {
        validator.error(
            CatalogLineageValidationCode::UnknownTrustPolicy,
            "trust_policy_id",
            "manifest and verifier must bind the exact implemented trust policy",
        );
    }

    let mut trusted_keys = BTreeMap::new();
    let mut genesis_keys = BTreeSet::new();
    for (index, key) in trust_policy.keys.iter().enumerate() {
        let Some(raw_key) = decode_lower_hex_array::<32>(&key.public_key_hex) else {
            validator.error(
                CatalogLineageValidationCode::UnknownSigner,
                format!("trust_policy.keys[{index}].public_key_hex"),
                "trusted Ed25519 public key must be exactly 32 lowercase-hex bytes",
            );
            continue;
        };
        let Ok(verifying_key) = VerifyingKey::from_bytes(&raw_key) else {
            validator.error(
                CatalogLineageValidationCode::UnknownSigner,
                format!("trust_policy.keys[{index}].public_key_hex"),
                "trusted Ed25519 public key is not a valid compressed point",
            );
            continue;
        };
        if trusted_keys
            .insert(key.key_id.clone(), verifying_key)
            .is_some()
        {
            validator.error(
                CatalogLineageValidationCode::UnknownSigner,
                format!("trust_policy.keys[{index}].key_id"),
                format!("duplicate trusted key id `{}`", key.key_id),
            );
        }
        if key.genesis_authority {
            genesis_keys.insert(key.key_id.clone());
        }
    }

    if manifest.records.len() != 3 {
        validator.error(
            CatalogLineageValidationCode::InvalidHistoryOrder,
            "records",
            "trust-policy v1 requires exactly revision 1 NO-DATA, revision 2 genesis, and revision 3 signed successor",
        );
    }
    let mut seen_revisions = BTreeSet::new();
    let mut seen_snapshot_refs = BTreeSet::new();
    let mut seen_snapshot_digests = BTreeSet::new();
    let mut previous_revision = None;
    let mut previous_snapshot: Option<&CatalogSnapshotReceipt> = None;
    let mut authorized_signers = BTreeSet::new();

    for (index, record) in manifest.records.iter().take(3).enumerate() {
        let path = format!("records[{index}]");
        let revision_key = catalog_revision_sort_key(&record.catalog_revision);
        if !seen_revisions.insert(record.catalog_revision.clone())
            || revision_key.is_none()
            || previous_revision.is_some_and(|previous| {
                revision_key
                    .is_none_or(|current| !is_direct_catalog_revision_successor(previous, current))
            })
        {
            validator.error(
                CatalogLineageValidationCode::InvalidHistoryOrder,
                format!("{path}.catalog_revision"),
                "catalog revisions must be unique, canonical, and contiguous",
            );
        }
        previous_revision = revision_key;

        if index == 0 {
            validate_unretained_record(record, &path, &mut validator);
            continue;
        }

        let Some(snapshot) = record.snapshot.as_ref() else {
            validator.error(
                CatalogLineageValidationCode::InvalidSnapshotIdentity,
                format!("{path}.snapshot"),
                "every retained revision requires an exact current snapshot",
            );
            continue;
        };
        if snapshot.catalog_revision != record.catalog_revision {
            validator.error(
                CatalogLineageValidationCode::InvalidSnapshotIdentity,
                format!("{path}.snapshot.catalog_revision"),
                "record and snapshot catalog revisions must match",
            );
        }
        verify_snapshot_receipt(
            snapshot,
            &format!("{path}.snapshot"),
            supplied_snapshots,
            &mut seen_snapshot_refs,
            &mut seen_snapshot_digests,
            &mut validator,
        );

        if index == 1 {
            validate_genesis_snapshot(snapshot, record, &path, &mut validator);
        } else {
            if record.retention != CatalogRevisionRetention::RetainedSuccessor
                || record.unretained_predecessor_revision.is_some()
                || record.no_data_reason.is_some()
            {
                validator.error(
                    CatalogLineageValidationCode::InvalidHistoryOrder,
                    &path,
                    "every post-genesis record must be a retained_successor",
                );
            }
            if record.predecessor_snapshot.as_ref() != previous_snapshot {
                validator.error(
                    CatalogLineageValidationCode::PredecessorMismatch,
                    format!("{path}.predecessor_snapshot"),
                    "successor must embed the exact immediately preceding retained snapshot receipt",
                );
            }
        }

        let signer_id = record.signer_key_id.as_deref();
        let digest_hex = record.canonical_record_sha256.as_deref();
        let signature_hex = record.signature_ed25519_hex.as_deref();
        let Some((signer_id, digest_hex, signature_hex)) = signer_id
            .zip(digest_hex)
            .zip(signature_hex)
            .map(|((signer_id, digest_hex), signature_hex)| (signer_id, digest_hex, signature_hex))
        else {
            validator.error(
                CatalogLineageValidationCode::MissingSignature,
                &path,
                "every retained record requires signer, canonical digest, and detached Ed25519 signature",
            );
            previous_snapshot = Some(snapshot);
            continue;
        };

        if (index == 1 && !genesis_keys.contains(signer_id))
            || (index > 1 && !authorized_signers.contains(signer_id))
        {
            validator.error(
                CatalogLineageValidationCode::InvalidDelegation,
                format!("{path}.signer_key_id"),
                format!("signer `{signer_id}` is not authorized for this chain position"),
            );
        }

        let computed_digest = product_journey_lineage_record_digest(record);
        match computed_digest {
            Ok(computed) => {
                let computed_hex = bytes_to_lower_hex(&computed);
                if digest_hex != computed_hex {
                    validator.error(
                        CatalogLineageValidationCode::CanonicalDigestMismatch,
                        format!("{path}.canonical_record_sha256"),
                        format!("canonical digest is `{computed_hex}`, not `{digest_hex}`"),
                    );
                }
                match (
                    trusted_keys.get(signer_id),
                    decode_lower_hex_array::<64>(signature_hex),
                ) {
                    (Some(verifying_key), Some(signature_bytes)) => {
                        let mut message =
                            Vec::with_capacity(PRODUCT_JOURNEY_LINEAGE_SIGNATURE_DOMAIN.len() + 33);
                        message
                            .extend_from_slice(PRODUCT_JOURNEY_LINEAGE_SIGNATURE_DOMAIN.as_bytes());
                        message.push(0);
                        message.extend_from_slice(&computed);
                        let signature = Signature::from_bytes(&signature_bytes);
                        if verifying_key.verify_strict(&message, &signature).is_err() {
                            validator.error(
                                CatalogLineageValidationCode::InvalidSignature,
                                format!("{path}.signature_ed25519_hex"),
                                "strict Ed25519 verification failed",
                            );
                        }
                    }
                    (None, _) => validator.error(
                        CatalogLineageValidationCode::UnknownSigner,
                        format!("{path}.signer_key_id"),
                        format!("signer `{signer_id}` is absent from the trust policy"),
                    ),
                    (Some(_), None) => validator.error(
                        CatalogLineageValidationCode::InvalidSignature,
                        format!("{path}.signature_ed25519_hex"),
                        "signature must be exactly 64 lowercase-hex bytes",
                    ),
                }
            }
            Err(error) => validator.error(
                CatalogLineageValidationCode::CanonicalDigestMismatch,
                format!("{path}.canonical_record_sha256"),
                format!("canonical projection could not be encoded: {error}"),
            ),
        }

        let Some(delegation) = record.delegation.as_ref() else {
            validator.error(
                CatalogLineageValidationCode::InvalidDelegation,
                format!("{path}.delegation"),
                "every retained record must authenticate a non-empty successor-key policy",
            );
            previous_snapshot = Some(snapshot);
            continue;
        };
        authorized_signers.clear();
        for (key_index, key) in delegation.authorized_successor_keys.iter().enumerate() {
            if !trusted_keys.contains_key(&key.key_id) {
                validator.error(
                    CatalogLineageValidationCode::UnknownSigner,
                    format!("{path}.delegation.authorized_successor_keys[{key_index}].key_id"),
                    format!(
                        "delegated key `{}` is absent from the trust policy",
                        key.key_id
                    ),
                );
            }
            if !authorized_signers.insert(key.key_id.clone()) {
                validator.error(
                    CatalogLineageValidationCode::InvalidDelegation,
                    format!("{path}.delegation.authorized_successor_keys[{key_index}].key_id"),
                    format!("delegated key `{}` appears more than once", key.key_id),
                );
            }
        }
        if authorized_signers.is_empty() {
            validator.error(
                CatalogLineageValidationCode::InvalidDelegation,
                format!("{path}.delegation.authorized_successor_keys"),
                "at least one trusted successor key is required",
            );
        }
        previous_snapshot = Some(snapshot);
    }

    if manifest.current_catalog_revision != PRODUCT_JOURNEY_LINEAGE_CURRENT_REVISION
        || manifest.records.last().is_none_or(|record| {
            record.catalog_revision != manifest.current_catalog_revision
                || record.canonical_record_sha256.as_deref()
                    != Some(PRODUCT_JOURNEY_LINEAGE_CURRENT_RECORD_SHA256)
        })
    {
        validator.error(
            CatalogLineageValidationCode::InvalidHistoryOrder,
            "current_catalog_revision",
            "current revision and canonical head digest must equal the verifier-pinned v1 chain head",
        );
    }

    CatalogLineageValidationReport {
        valid: validator.errors.is_empty(),
        errors: validator.errors,
    }
}

/// Verify a lineage against the exact public keys compiled into this verifier.
#[must_use]
pub fn verify_product_journey_lineage(
    manifest: &ProductJourneyLineageManifest,
    supplied_snapshots: &BTreeMap<String, Vec<u8>>,
) -> CatalogLineageValidationReport {
    verify_product_journey_lineage_with_trust_policy(
        manifest,
        supplied_snapshots,
        &CatalogLineageTrustPolicy::pinned_v1(),
    )
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
    /// A v2 phase contract differs from its closed role semantics.
    InvalidLifecycleContract,
    /// A journey phase is bound to the wrong explicit role.
    SwappedLifecyclePhase,
    /// Lifecycle steps or roles were duplicated or collapsed.
    DuplicateLifecyclePhase,
    /// Identity preflight permits or follows a product mutation.
    PostMutationPreflight,
    /// Recovery is not causally preceded by an observed failure/overload.
    RecoveryWithoutFailure,
    /// Teardown does not retain resource-release and final-outcome authority.
    TeardownWithoutOutcome,
    /// The v2 producer/consumer migration is incomplete or changes v1 identity.
    InvalidLifecycleMigration,
    /// Support, evidence, run verdict, or availability disagree.
    ContradictoryClaim,
    /// A contract-only catalog attempted to declare support.
    ContractOnlySupportedClaim,
    /// Schema version 1 attempted to use unsupported claim authority.
    UnsupportedClaimAuthority,
    /// Schema version 1 attempted to mint positive run-evidence authority.
    UnsupportedEvidenceAuthority,
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
            Self::InvalidLifecycleContract => "PJC-LIFECYCLE-002",
            Self::SwappedLifecyclePhase => "PJC-LIFECYCLE-003",
            Self::DuplicateLifecyclePhase => "PJC-LIFECYCLE-004",
            Self::PostMutationPreflight => "PJC-LIFECYCLE-005",
            Self::RecoveryWithoutFailure => "PJC-LIFECYCLE-006",
            Self::TeardownWithoutOutcome => "PJC-LIFECYCLE-007",
            Self::InvalidLifecycleMigration => "PJC-LIFECYCLE-008",
            Self::ContradictoryClaim => "PJC-CLAIM-001",
            Self::ContractOnlySupportedClaim => "PJC-CLAIM-002",
            Self::UnsupportedClaimAuthority => "PJC-AUTHORITY-001",
            Self::UnsupportedEvidenceAuthority => "PJC-AUTHORITY-002",
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

/// Validate the breaking v2 typed-lifecycle migration without performing I/O.
#[must_use]
pub fn validate_product_journey_catalog_v2(
    catalog: &ProductJourneyCatalogV2,
) -> CatalogValidationReport {
    let mut validator = ValidatorState::new();

    if catalog.contract_id != PRODUCT_JOURNEY_CONTRACT_ID_V2 {
        validator.error(
            CatalogValidationCode::UnknownContract,
            "contract_id",
            format!(
                "expected `{PRODUCT_JOURNEY_CONTRACT_ID_V2}`, got `{}`",
                catalog.contract_id
            ),
        );
    }
    if catalog.schema_version != PRODUCT_JOURNEY_SCHEMA_VERSION_V2 {
        validator.error(
            CatalogValidationCode::UnknownSchemaVersion,
            "schema_version",
            format!(
                "expected schema version {PRODUCT_JOURNEY_SCHEMA_VERSION_V2}, got {}",
                catalog.schema_version
            ),
        );
    }
    validator.require_text("catalog_revision", &catalog.catalog_revision);
    if catalog_revision_sort_key(&catalog.catalog_revision).is_none() {
        validator.error(
            CatalogValidationCode::InvalidLifecycleMigration,
            "catalog_revision",
            "v2 catalog revision must be a canonical Gregorian YYYY-MM-DD.N successor label",
        );
    }
    if catalog.source_bead_id != PRODUCT_JOURNEY_LIFECYCLE_SOURCE_BEAD_ID_V2 {
        validator.error(
            CatalogValidationCode::MalformedReference,
            "source_bead_id",
            format!("v2 lifecycle source must be `{PRODUCT_JOURNEY_LIFECYCLE_SOURCE_BEAD_ID_V2}`"),
        );
    }
    if catalog.catalog_claim_state != CatalogClaimState::ContractOnly {
        validator.error(
            CatalogValidationCode::UnsupportedClaimAuthority,
            "catalog_claim_state",
            "v2 typed lifecycle metadata is contract-only and cannot mint product support",
        );
    }
    if catalog.predecessor_contract_id != PRODUCT_JOURNEY_CONTRACT_ID
        || catalog.predecessor_catalog_revision != PRODUCT_JOURNEY_LINEAGE_CURRENT_REVISION
        || catalog.predecessor_raw_sha256 != PRODUCT_JOURNEY_V1_PREDECESSOR_RAW_SHA256
        || catalog.v1_positional_lifecycle_semantics
            != LegacyPositionalLifecycleSemanticsV2::Unproved
    {
        validator.error(
            CatalogValidationCode::InvalidLifecycleMigration,
            "predecessor",
            "v2 must bind the exact signed v1 head and mark its positional lifecycle semantics unproved",
        );
    }
    validator.require_hex_digest(
        CatalogValidationCode::InvalidLifecycleMigration,
        "predecessor_raw_sha256",
        &catalog.predecessor_raw_sha256,
        64,
        "v1 predecessor raw SHA-256",
    );

    validate_phase_contracts_v2(catalog, &mut validator);
    let producer_ids = validate_journey_producers_v2(catalog, &mut validator);
    validate_closure_consumers_v2(catalog, &producer_ids, &mut validator);

    CatalogValidationReport {
        valid: validator.errors.is_empty(),
        errors: validator.errors,
    }
}

fn canonical_phase_contract_v2(role: JourneyLifecycleRoleV2) -> JourneyLifecyclePhaseContractV2 {
    use JourneyCancellationSemanticsV2 as Cancellation;
    use JourneyFailureSemanticsV2 as Failure;
    use JourneyIdentityRequirementV2 as Identity;
    use JourneyMutationClassV2 as Mutation;
    use JourneyPhaseEvidenceV2 as Evidence;
    use JourneyPhaseOutcomeV2 as Outcome;
    use JourneyPhasePreconditionV2 as Precondition;

    match role {
        JourneyLifecycleRoleV2::IdentityPreflight => JourneyLifecyclePhaseContractV2 {
            role,
            required_identities: vec![
                Identity::ExactCandidate,
                Identity::ControllerTarget,
                Identity::SessionHostTarget,
                Identity::TopologyTransport,
                Identity::Route,
                Identity::MuxSession,
                Identity::PaneFleet,
                Identity::RendererDisplay,
                Identity::ConfigurationPolicy,
                Identity::WorkloadActors,
            ],
            required_preconditions: vec![Precondition::UnmodifiedStartingState],
            allowed_mutations: Vec::new(),
            required_outcomes: vec![Outcome::IdentityBound],
            required_evidence: vec![Evidence::IdentityReceipt],
            cancellation: Cancellation::AbortBeforeMutation,
            failure_semantics: Failure::StopBeforeMutation,
        },
        JourneyLifecycleRoleV2::CleanSetup => JourneyLifecyclePhaseContractV2 {
            role,
            required_identities: vec![
                Identity::ExactCandidate,
                Identity::TopologyTransport,
                Identity::MuxSession,
                Identity::ConfigurationPolicy,
            ],
            required_preconditions: vec![Precondition::IdentityPreflightPassed],
            allowed_mutations: vec![Mutation::CleanSetup],
            required_outcomes: vec![Outcome::CleanBaselineReady],
            required_evidence: vec![Evidence::SetupReceipt],
            cancellation: Cancellation::RollbackOrResumeSetup,
            failure_semantics: Failure::SetupFailureIsExplicit,
        },
        JourneyLifecycleRoleV2::SteadyWork => JourneyLifecyclePhaseContractV2 {
            role,
            required_identities: vec![
                Identity::ExactCandidate,
                Identity::MuxSession,
                Identity::PaneFleet,
                Identity::WorkloadActors,
            ],
            required_preconditions: vec![Precondition::CleanSetupComplete],
            allowed_mutations: vec![Mutation::UserWork],
            required_outcomes: vec![Outcome::SteadyWorkCompleted],
            required_evidence: vec![Evidence::WorkReceipt],
            cancellation: Cancellation::PreserveUserIntent,
            failure_semantics: Failure::WorkFailureIsExplicit,
        },
        JourneyLifecycleRoleV2::FailureOverload => JourneyLifecyclePhaseContractV2 {
            role,
            required_identities: vec![
                Identity::ExactCandidate,
                Identity::Route,
                Identity::MuxSession,
                Identity::PaneFleet,
            ],
            required_preconditions: vec![Precondition::SteadyWorkStarted],
            allowed_mutations: vec![Mutation::FaultOrOverloadInjection],
            required_outcomes: vec![Outcome::FailureObserved],
            required_evidence: vec![Evidence::FailureReceipt],
            cancellation: Cancellation::PreserveFailureEvidence,
            failure_semantics: Failure::FailureIsTestInput,
        },
        JourneyLifecycleRoleV2::RecoveryConvergence => JourneyLifecyclePhaseContractV2 {
            role,
            required_identities: vec![
                Identity::ExactCandidate,
                Identity::Route,
                Identity::MuxSession,
                Identity::PaneFleet,
            ],
            required_preconditions: vec![Precondition::FailureOverloadObserved],
            allowed_mutations: vec![Mutation::RecoveryAction],
            required_outcomes: vec![Outcome::AuthoritativeStateConverged],
            required_evidence: vec![Evidence::ConvergenceReceipt],
            cancellation: Cancellation::CheckpointRetryableRecovery,
            failure_semantics: Failure::NonConvergenceIsFailure,
        },
        JourneyLifecycleRoleV2::TeardownOutcome => JourneyLifecyclePhaseContractV2 {
            role,
            required_identities: vec![
                Identity::ExactCandidate,
                Identity::MuxSession,
                Identity::PaneFleet,
            ],
            required_preconditions: vec![Precondition::RecoveryAttempted],
            allowed_mutations: vec![Mutation::ResourceRelease],
            required_outcomes: vec![Outcome::ResourcesReleased, Outcome::FinalOutcomeRecorded],
            required_evidence: vec![Evidence::TeardownReceipt],
            cancellation: Cancellation::CleanupRemainsRequired,
            failure_semantics: Failure::IncompleteTeardownIsFailure,
        },
    }
}

fn validate_unique_phase_contract_values<T: Ord>(
    validator: &mut ValidatorState,
    path: &str,
    values: &[T],
) {
    if values.len() != values.iter().collect::<BTreeSet<_>>().len() {
        validator.error(
            CatalogValidationCode::InvalidLifecycleContract,
            path,
            "phase contract collections must not contain duplicate semantic requirements",
        );
    }
}

fn validate_phase_contracts_v2(catalog: &ProductJourneyCatalogV2, validator: &mut ValidatorState) {
    if catalog.phase_contracts.len() != REQUIRED_LIFECYCLE_PHASE_COUNT_V2 {
        validator.error(
            CatalogValidationCode::InvalidLifecycleContract,
            "phase_contracts",
            format!("v2 requires exactly {REQUIRED_LIFECYCLE_PHASE_COUNT_V2} phase contracts"),
        );
    }

    let mut roles = BTreeSet::new();
    for (index, contract) in catalog.phase_contracts.iter().enumerate() {
        let path = format!("phase_contracts[{index}]");
        if !roles.insert(contract.role) {
            validator.error(
                CatalogValidationCode::DuplicateLifecyclePhase,
                format!("{path}.role"),
                format!("duplicate phase role `{}`", contract.role.as_str()),
            );
        }

        validate_unique_phase_contract_values(
            validator,
            &format!("{path}.required_identities"),
            &contract.required_identities,
        );
        validate_unique_phase_contract_values(
            validator,
            &format!("{path}.required_preconditions"),
            &contract.required_preconditions,
        );
        validate_unique_phase_contract_values(
            validator,
            &format!("{path}.allowed_mutations"),
            &contract.allowed_mutations,
        );
        validate_unique_phase_contract_values(
            validator,
            &format!("{path}.required_outcomes"),
            &contract.required_outcomes,
        );
        validate_unique_phase_contract_values(
            validator,
            &format!("{path}.required_evidence"),
            &contract.required_evidence,
        );

        let expected = canonical_phase_contract_v2(contract.role);
        if contract != &expected {
            validator.error(
                CatalogValidationCode::InvalidLifecycleContract,
                path.clone(),
                format!(
                    "phase contract `{}` differs from its closed v2 semantics",
                    contract.role.as_str()
                ),
            );
        }
        if contract.role == JourneyLifecycleRoleV2::IdentityPreflight
            && (!contract.allowed_mutations.is_empty()
                || contract.required_preconditions.as_slice()
                    != [JourneyPhasePreconditionV2::UnmodifiedStartingState].as_slice())
        {
            validator.error(
                CatalogValidationCode::PostMutationPreflight,
                path.clone(),
                "identity preflight must occur in an unmodified state and permit no mutation",
            );
        }
        if contract.role == JourneyLifecycleRoleV2::RecoveryConvergence
            && !contract
                .required_preconditions
                .contains(&JourneyPhasePreconditionV2::FailureOverloadObserved)
        {
            validator.error(
                CatalogValidationCode::RecoveryWithoutFailure,
                path.clone(),
                "recovery_convergence requires an observed failure_overload boundary",
            );
        }
        if contract.role == JourneyLifecycleRoleV2::TeardownOutcome
            && (!contract
                .required_outcomes
                .contains(&JourneyPhaseOutcomeV2::ResourcesReleased)
                || !contract
                    .required_outcomes
                    .contains(&JourneyPhaseOutcomeV2::FinalOutcomeRecorded))
        {
            validator.error(
                CatalogValidationCode::TeardownWithoutOutcome,
                path,
                "teardown_outcome must release resources and retain a final outcome",
            );
        }
    }

    let expected = JourneyLifecycleRoleV2::ALL
        .into_iter()
        .collect::<BTreeSet<_>>();
    if roles != expected {
        validator.error(
            CatalogValidationCode::InvalidLifecycleContract,
            "phase_contracts",
            "phase contracts must cover each of the six roles exactly once",
        );
    }
}

fn lifecycle_phases_v2(
    lifecycle: &JourneyLifecycleV2,
) -> [(
    &'static str,
    JourneyLifecycleRoleV2,
    &JourneyLifecyclePhaseV2,
); REQUIRED_LIFECYCLE_PHASE_COUNT_V2] {
    [
        (
            "identity_preflight",
            JourneyLifecycleRoleV2::IdentityPreflight,
            &lifecycle.identity_preflight,
        ),
        (
            "clean_setup",
            JourneyLifecycleRoleV2::CleanSetup,
            &lifecycle.clean_setup,
        ),
        (
            "steady_work",
            JourneyLifecycleRoleV2::SteadyWork,
            &lifecycle.steady_work,
        ),
        (
            "failure_overload",
            JourneyLifecycleRoleV2::FailureOverload,
            &lifecycle.failure_overload,
        ),
        (
            "recovery_convergence",
            JourneyLifecycleRoleV2::RecoveryConvergence,
            &lifecycle.recovery_convergence,
        ),
        (
            "teardown_outcome",
            JourneyLifecycleRoleV2::TeardownOutcome,
            &lifecycle.teardown_outcome,
        ),
    ]
}

fn validate_journey_producers_v2(
    catalog: &ProductJourneyCatalogV2,
    validator: &mut ValidatorState,
) -> BTreeSet<String> {
    if catalog.journey_producers.len() != REQUIRED_FIELD_JOURNEY_COUNT {
        validator.error(
            CatalogValidationCode::InvalidLifecycleMigration,
            "journey_producers",
            format!("v2 requires exactly {REQUIRED_FIELD_JOURNEY_COUNT} journey producers"),
        );
    }

    let mut journey_ids = BTreeSet::new();
    let mut field_bead_ids = BTreeSet::new();
    for (index, producer) in catalog.journey_producers.iter().enumerate() {
        let path = format!("journey_producers[{index}]");
        validator.require_text(&format!("{path}.journey_id"), &producer.journey_id);
        validator.require_bead_refs(
            &format!("{path}.field_bead_id"),
            std::slice::from_ref(&producer.field_bead_id),
            false,
        );
        if !journey_ids.insert(producer.journey_id.clone()) {
            validator.error(
                CatalogValidationCode::DuplicateId,
                format!("{path}.journey_id"),
                format!("duplicate migrated journey `{}`", producer.journey_id),
            );
        }
        if !field_bead_ids.insert(producer.field_bead_id.clone()) {
            validator.error(
                CatalogValidationCode::DuplicateId,
                format!("{path}.field_bead_id"),
                format!("duplicate migrated field Bead `{}`", producer.field_bead_id),
            );
        }
        if canonical_journey_id_for_field_bead(&producer.field_bead_id)
            != Some(producer.journey_id.as_str())
        {
            validator.error(
                CatalogValidationCode::InvalidLifecycleMigration,
                path.clone(),
                "v2 journey and field-Bead identities must equal the signed v1 inventory",
            );
        }

        let mut normalized_steps = BTreeSet::new();
        for (field, expected_role, phase) in lifecycle_phases_v2(&producer.lifecycle) {
            let phase_path = format!("{path}.lifecycle.{field}");
            if phase.contract_role != expected_role {
                validator.error(
                    CatalogValidationCode::SwappedLifecyclePhase,
                    format!("{phase_path}.contract_role"),
                    format!(
                        "field `{field}` must bind role `{}`, not `{}`",
                        expected_role.as_str(),
                        phase.contract_role.as_str()
                    ),
                );
            }
            validator.require_text_list(&format!("{phase_path}.steps"), &phase.steps);
            for (step_index, step) in phase.steps.iter().enumerate() {
                let normalized = step.split_whitespace().collect::<Vec<_>>().join(" ");
                if !normalized.is_empty() && !normalized_steps.insert(normalized) {
                    validator.error(
                        CatalogValidationCode::DuplicateLifecyclePhase,
                        format!("{phase_path}.steps[{step_index}]"),
                        "journey lifecycle phases must not duplicate or collapse the same step",
                    );
                }
            }
        }
    }

    let expected_bindings = canonical_field_journey_bindings();
    let expected_journeys = expected_bindings
        .iter()
        .map(|(_, journey_id)| (*journey_id).to_string())
        .collect::<BTreeSet<_>>();
    let expected_beads = expected_bindings
        .iter()
        .map(|(bead_id, _)| (*bead_id).to_string())
        .collect::<BTreeSet<_>>();
    if journey_ids != expected_journeys || field_bead_ids != expected_beads {
        validator.error(
            CatalogValidationCode::InvalidLifecycleMigration,
            "journey_producers",
            "v2 must migrate the exact fourteen signed-v1 journey/field-Bead identities",
        );
    }

    journey_ids
}

fn claim_id_for_coverage_v2(coverage: CoverageKey) -> String {
    format!(
        "claim.{}.{}.{}",
        coverage.persona.as_str(),
        coverage.topology.as_str(),
        coverage.fleet_point.as_str()
    )
}

fn validate_closure_consumers_v2(
    catalog: &ProductJourneyCatalogV2,
    producer_ids: &BTreeSet<String>,
    validator: &mut ValidatorState,
) {
    if catalog.closure_consumers.len() != REQUIRED_COVERAGE_CELL_COUNT {
        validator.error(
            CatalogValidationCode::InvalidLifecycleMigration,
            "closure_consumers",
            format!("v2 requires exactly {REQUIRED_COVERAGE_CELL_COUNT} closure consumers"),
        );
    }

    let mut claim_ids = BTreeSet::new();
    let mut coverage = BTreeSet::new();
    let mut consumed_journeys = BTreeSet::new();
    for (index, consumer) in catalog.closure_consumers.iter().enumerate() {
        let path = format!("closure_consumers[{index}]");
        validator.require_text(&format!("{path}.claim_id"), &consumer.claim_id);
        if !claim_ids.insert(consumer.claim_id.clone()) {
            validator.error(
                CatalogValidationCode::DuplicateClaimId,
                format!("{path}.claim_id"),
                format!("duplicate v2 closure consumer `{}`", consumer.claim_id),
            );
        }
        if consumer.claim_id != claim_id_for_coverage_v2(consumer.coverage) {
            validator.error(
                CatalogValidationCode::InvalidLifecycleMigration,
                format!("{path}.claim_id"),
                "closure claim identity must equal its explicit persona/topology/fleet coverage",
            );
        }
        if !coverage.insert(consumer.coverage) {
            validator.error(
                CatalogValidationCode::DuplicateCompositeKey,
                format!("{path}.coverage"),
                format!("duplicate v2 coverage `{}`", consumer.coverage.label()),
            );
        }
        if consumer.journey_ids.is_empty() {
            validator.error(
                CatalogValidationCode::EmptyRequiredField,
                format!("{path}.journey_ids"),
                "closure consumer must bind at least one typed journey producer",
            );
        }
        let mut local_ids = BTreeSet::new();
        for (journey_index, journey_id) in consumer.journey_ids.iter().enumerate() {
            if !local_ids.insert(journey_id.clone()) {
                validator.error(
                    CatalogValidationCode::DuplicateId,
                    format!("{path}.journey_ids[{journey_index}]"),
                    format!("duplicate journey reference `{journey_id}`"),
                );
            }
            if !producer_ids.contains(journey_id) {
                validator.error(
                    CatalogValidationCode::DanglingReference,
                    format!("{path}.journey_ids[{journey_index}]"),
                    format!("closure consumer references unmigrated journey `{journey_id}`"),
                );
            }
            consumed_journeys.insert(journey_id.clone());
        }
        let actual_journeys = consumer
            .journey_ids
            .iter()
            .map(String::as_str)
            .collect::<BTreeSet<_>>();
        let expected_journeys = canonical_variant_journeys(consumer.coverage);
        if actual_journeys != expected_journeys
            || actual_journeys.len() != consumer.journey_ids.len()
        {
            validator.error(
                CatalogValidationCode::InvalidLifecycleMigration,
                format!("{path}.journey_ids"),
                "v2 closure journeys must equal the exact signed-v1 persona/fleet/topology projection without duplicates",
            );
        }
    }

    if coverage != expected_coverage_keys() {
        validator.error(
            CatalogValidationCode::MissingRequiredCoverage,
            "closure_consumers",
            "v2 closure consumers must materialize the exact 32 persona/fleet/topology cells",
        );
    }
    if &consumed_journeys != producer_ids {
        validator.error(
            CatalogValidationCode::InvalidLifecycleMigration,
            "closure_consumers",
            "every migrated journey producer must be consumed by at least one explicit closure cell",
        );
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
            Topology::MacLanRemote => {
                std::iter::once(TargetMode::ThreadripperPro5995wxNative).collect()
            }
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
        if allowed != std::iter::once(expected_persona).collect() {
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
        Topology::MacLanRemote => std::iter::once("trj_5995wx").collect::<BTreeSet<_>>(),
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
            || session_host.is_some_and(|definition| definition.mode == TargetMode::M5ProMaxNative);
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
    if qualification.evidence_state == EvidenceState::Proven {
        validator.error(
            CatalogValidationCode::UnsupportedEvidenceAuthority,
            format!("{path}.evidence_state"),
            "schema version 1 has no signed evidence-receipt verifier and cannot mint `proven` evidence",
        );
    }
    if qualification.run_verdict == RunVerdict::Pass {
        validator.error(
            CatalogValidationCode::UnsupportedEvidenceAuthority,
            format!("{path}.run_verdict"),
            "schema version 1 has no signed run-receipt verifier and cannot mint a `pass` verdict",
        );
    }
    if qualification.freshness_state == FreshnessState::Current {
        validator.error(
            CatalogValidationCode::UnsupportedEvidenceAuthority,
            format!("{path}.freshness_state"),
            "schema version 1 has no signed candidate-and-route freshness verifier and cannot mint `current` evidence",
        );
    }

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
                    "reviewed revision `{}` has no declared change-history row in this catalog",
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
        if record.authority_kind != ReviewAuthorityKind::AutomatedInformational {
            validator.error(
                CatalogValidationCode::InvalidReviewAuthority,
                format!("{path}.authority_kind"),
                "schema version 1 accepts only automated_informational review metadata; human authority requires a later signed-receipt contract",
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
                || record.reviewed_commit.is_some()
                || record.authority_receipt_ref.is_some()
                || record.authority_receipt_sha256.is_some())
        {
            validator.error(
                CatalogValidationCode::InvalidReviewAuthority,
                format!("{path}.authority_kind"),
                "automated_informational review must be informational and cannot carry a reviewed commit or authority receipt",
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
                    "duplicate catalog revision `{}` in declared history",
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
                    "the canonical initial draft-metadata row must equal the schema-v1 record",
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
            format!("schema-v1 declared lineage must retain canonical `{INITIAL_CHANGE_ID}`"),
        );
    }
    if !change_revisions.contains(&catalog.catalog_revision) {
        validator.error(
            CatalogValidationCode::InvalidDefinition,
            "change_history",
            format!(
                "current catalog revision `{}` has no declared change-history row",
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
            "the final declared change-history row must describe the current catalog revision",
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
    let days_in_month = days_in_gregorian_month(year, month);
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
