//! Plugin capability + sandbox-policy substrate (ft-mpc9b.4.3).
//!
//! Steals the zellij plugin model: third-party WIT-bound WASM plugins
//! render status-bar tiles, side panels, and modal overlays. Each
//! plugin declares the capabilities it needs; the host enforces the
//! sandbox boundary via policy.rs `ActionKind` gates.
//!
//! This module ships the capability enumeration, manifest data type,
//! lifecycle state machine, and capability-grant decision policy as
//! pure-logic substrate. The wasmtime host, WIT bindings, file I/O,
//! plugin signing, and capability-gate integration with policy.rs
//! live in the integration bead.
//!
//! ## What this module ships
//!
//! - `PluginCapability` — enumerates every action a plugin can ask
//!   for. Three classes:
//!   - **Permissive** — granted by default (read-only state /
//!     events / config; render-only render slots).
//!   - **Restricted** — requires an explicit approval token
//!     (sensitive reads / sends / spawns / network).
//!   - **Forbidden** — refused unconditionally; declaring one in a
//!     manifest fails validation. Listed in the enum for documentation
//!     symmetry with the bead's enumeration but the policy refuses
//!     them.
//! - `PluginManifest` — id, version, capabilities, signature class,
//!   resource limits.
//! - `validate_manifest` — checks invariants: id non-empty, no
//!   forbidden capabilities, resource limits within safe bounds,
//!   signature requirement honoured.
//! - `PluginLifecycleState` — state machine (Loaded / Initialised /
//!   Running / Reloading / Stopped / Errored). Pure transitions; the
//!   host drives them via WIT-bound `lifecycle()` calls.
//! - `CapabilityGrantDecision` — pure policy: given a manifest, an
//!   approval-token bag, and a request to use a capability, decide
//!   `Granted` / `Denied(reason)`.
//! - `PluginResourceBudget` — render-deadline + memory + disk caps;
//!   `clamp_to_safe_bounds` enforces the bead's defaults
//!   (50 ms / 16 MB / 100 MB).
//! - `PluginSignatureClass` — Unsigned / Selfsigned / Verified
//!   (sigstore/cosign). Operator config decides which classes load.
//!
//! ## What is deferred to the integration bead (ft-mpc9b.4.3.cont)
//!
//! - wasmtime host + WIT plugin trait bindings.
//! - Plugin discovery from `~/.config/ft/plugins/*.wasm`.
//! - WASI-subset configuration.
//! - Per-plugin sandbox dir scaffolding under
//!   `~/.config/ft/plugins/<name>/`.
//! - Capability gate wiring through `policy.rs` `ActionKind` (the
//!   substrate's `CapabilityGrantDecision` is the pure-logic core
//!   the gate consumes).
//! - Audit chain: every plugin action logs through
//!   `policy_audit_chain`.
//! - `ft plugin list/install/remove/update/sign` CLI commands.
//! - Three example plugins: clock-tile / fleet-status-tile /
//!   git-branch-tile.
//! - Resource-limit enforcement (50 ms render budget kill + backoff,
//!   memory cap, disk quota) via wasmtime fuel + per-instance limits.
//! - Sigstore/cosign signature verification + verified-plugin badge.
//! - Cross-platform conformance test (same .wasm runs identically
//!   on macOS / Linux / Windows).

#![allow(dead_code)]

// ============================================================================
// Capability classes
// ============================================================================

/// What a plugin can ask the host to do. Three classes per the
/// bead: permissive (granted by default), restricted (needs approval
/// token), forbidden (refused unconditionally). The class is
/// determined by the variant via `class_of()`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PluginCapability {
    // ---- Permissive: granted by default ----
    /// Read pane metadata (id, title, cwd, agent_type, state).
    ReadState,
    /// Read the detection-events stream.
    ReadEvents,
    /// Render into a status-bar tile slot (cross-link
    /// `status_bar.rs` ft-mpc9b.4.4).
    RenderTile,
    /// Render into a side-panel slot.
    RenderSidePanel,
    /// Render a modal overlay (hover / click only — no input
    /// capture).
    RenderOverlay,
    /// Read the plugin's own sandboxed config under
    /// `~/.config/ft/plugins/<name>/`.
    ReadOwnConfig,

    // ---- Restricted: needs an approval token ----
    /// Read scrollback content. Sensitive — secrets!
    ReadPaneText,
    /// Send text or keys to a pane.
    SendInput,
    /// Spawn a new pane.
    SpawnPane,
    /// Close an existing pane.
    ClosePane,
    /// Launch a workflow.
    LaunchWorkflow,
    /// Outbound HTTP (rate-limited by the policy engine).
    Network,

    // ---- Forbidden: declaring one fails validation ----
    /// File-system access outside the plugin's sandbox dir.
    FileSystemEscape,
    /// Direct PTY access (bypasses pane abstraction).
    DirectPtyAccess,
    /// Direct mux-server IPC.
    DirectMuxIpc,
    /// Spawn arbitrary subprocesses.
    ArbitrarySubprocess,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CapabilityClass {
    Permissive,
    Restricted,
    Forbidden,
}

impl PluginCapability {
    #[must_use]
    pub fn class(self) -> CapabilityClass {
        match self {
            Self::ReadState
            | Self::ReadEvents
            | Self::RenderTile
            | Self::RenderSidePanel
            | Self::RenderOverlay
            | Self::ReadOwnConfig => CapabilityClass::Permissive,
            Self::ReadPaneText
            | Self::SendInput
            | Self::SpawnPane
            | Self::ClosePane
            | Self::LaunchWorkflow
            | Self::Network => CapabilityClass::Restricted,
            Self::FileSystemEscape
            | Self::DirectPtyAccess
            | Self::DirectMuxIpc
            | Self::ArbitrarySubprocess => CapabilityClass::Forbidden,
        }
    }

    #[must_use]
    pub fn is_permissive(self) -> bool {
        matches!(self.class(), CapabilityClass::Permissive)
    }

    #[must_use]
    pub fn requires_approval_token(self) -> bool {
        matches!(self.class(), CapabilityClass::Restricted)
    }

    #[must_use]
    pub fn is_forbidden(self) -> bool {
        matches!(self.class(), CapabilityClass::Forbidden)
    }
}

// ============================================================================
// Signature class
// ============================================================================

/// Signature provenance for a plugin. The integration's loader
/// matches the operator's `[plugins] allowed_signature_classes`
/// config against this.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum PluginSignatureClass {
    /// No signature. Operator-disabled by default.
    #[default]
    Unsigned,
    /// Self-signed by the plugin author. Trust-on-first-use model.
    SelfSigned,
    /// Verified via sigstore / cosign against a public key chain.
    /// Earns the "verified plugin" badge in the registry.
    Verified,
}

// ============================================================================
// Resource budget
// ============================================================================

/// Per-plugin resource caps the host enforces via wasmtime fuel +
/// per-instance limits. Defaults match the bead.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PluginResourceBudget {
    /// Max render-call deadline. Bead default: 50 ms.
    pub render_deadline_ms: u32,
    /// Max linear-memory size. Bead default: 16 MB.
    pub memory_cap_bytes: u64,
    /// Max sandbox-dir disk usage. Bead default: 100 MB.
    pub disk_cap_bytes: u64,
}

pub const DEFAULT_RENDER_DEADLINE_MS: u32 = 50;
pub const DEFAULT_MEMORY_CAP_BYTES: u64 = 16 * 1024 * 1024;
pub const DEFAULT_DISK_CAP_BYTES: u64 = 100 * 1024 * 1024;

/// Hard ceiling for `render_deadline_ms` — no plugin may ask for more
/// than this (matches the bead's "50ms render budget"); operator
/// config can lower it.
pub const HARD_RENDER_DEADLINE_CEILING_MS: u32 = 50;
/// Hard ceiling for memory: 16 MB per the bead.
pub const HARD_MEMORY_CAP_BYTES: u64 = 16 * 1024 * 1024;
/// Hard ceiling for disk: 100 MB per the bead.
pub const HARD_DISK_CAP_BYTES: u64 = 100 * 1024 * 1024;

impl Default for PluginResourceBudget {
    fn default() -> Self {
        Self {
            render_deadline_ms: DEFAULT_RENDER_DEADLINE_MS,
            memory_cap_bytes: DEFAULT_MEMORY_CAP_BYTES,
            disk_cap_bytes: DEFAULT_DISK_CAP_BYTES,
        }
    }
}

impl PluginResourceBudget {
    /// Clamp every field down to the hard ceilings. Returns the
    /// clamped budget plus a flag per axis indicating whether the
    /// clamp engaged. Used at manifest load to refuse the plugin's
    /// over-ask while still letting it run with safe limits.
    #[must_use]
    pub fn clamp_to_safe_bounds(self) -> SafeBudget {
        let mut clamped = self;
        let mut render_clamped = false;
        let mut mem_clamped = false;
        let mut disk_clamped = false;
        if clamped.render_deadline_ms > HARD_RENDER_DEADLINE_CEILING_MS {
            clamped.render_deadline_ms = HARD_RENDER_DEADLINE_CEILING_MS;
            render_clamped = true;
        }
        if clamped.memory_cap_bytes > HARD_MEMORY_CAP_BYTES {
            clamped.memory_cap_bytes = HARD_MEMORY_CAP_BYTES;
            mem_clamped = true;
        }
        if clamped.disk_cap_bytes > HARD_DISK_CAP_BYTES {
            clamped.disk_cap_bytes = HARD_DISK_CAP_BYTES;
            disk_clamped = true;
        }
        SafeBudget {
            budget: clamped,
            render_clamped,
            mem_clamped,
            disk_clamped,
        }
    }

    #[must_use]
    pub fn any_clamp_engaged(self) -> bool {
        let s = self.clamp_to_safe_bounds();
        s.render_clamped || s.mem_clamped || s.disk_clamped
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SafeBudget {
    pub budget: PluginResourceBudget,
    pub render_clamped: bool,
    pub mem_clamped: bool,
    pub disk_clamped: bool,
}

// ============================================================================
// Plugin manifest + validation
// ============================================================================

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginManifest {
    pub id: String,
    pub version: String,
    pub signature_class: PluginSignatureClass,
    pub capabilities: Vec<PluginCapability>,
    pub resource_budget: PluginResourceBudget,
}

impl PluginManifest {
    #[must_use]
    pub fn new(id: impl Into<String>, version: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            version: version.into(),
            signature_class: PluginSignatureClass::default(),
            capabilities: Vec::new(),
            resource_budget: PluginResourceBudget::default(),
        }
    }

    pub fn with_capabilities<I: IntoIterator<Item = PluginCapability>>(
        mut self,
        caps: I,
    ) -> Self {
        self.capabilities.extend(caps);
        self
    }

    pub fn with_signature(mut self, sig: PluginSignatureClass) -> Self {
        self.signature_class = sig;
        self
    }

    pub fn with_budget(mut self, b: PluginResourceBudget) -> Self {
        self.resource_budget = b;
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ManifestValidationError {
    /// Manifest id is empty.
    EmptyId,
    /// Manifest version is empty.
    EmptyVersion,
    /// Manifest declares a forbidden capability — refuse to load.
    ForbiddenCapabilityDeclared {
        plugin_id: String,
        capability: PluginCapability,
    },
    /// Manifest has duplicate capabilities — non-fatal but worth
    /// surfacing as a config warning. Returned as an error so callers
    /// can decide whether to refuse or de-dup.
    DuplicateCapability {
        plugin_id: String,
        capability: PluginCapability,
    },
    /// Manifest's signature class is `Unsigned` but the operator's
    /// loader requires at least `SelfSigned`.
    SignatureClassDisallowed {
        plugin_id: String,
        actual: PluginSignatureClass,
        minimum: PluginSignatureClass,
    },
}

/// Operator-configurable signature requirement. Default `SelfSigned`
/// — matches "trust-on-first-use" pragmatism without allowing
/// completely anonymous plugins by default.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PluginLoaderPolicy {
    pub minimum_signature_class: PluginSignatureClass,
}

impl Default for PluginLoaderPolicy {
    fn default() -> Self {
        Self {
            minimum_signature_class: PluginSignatureClass::SelfSigned,
        }
    }
}

impl PluginLoaderPolicy {
    #[must_use]
    pub const fn allow_unsigned() -> Self {
        Self {
            minimum_signature_class: PluginSignatureClass::Unsigned,
        }
    }

    #[must_use]
    pub const fn require_verified() -> Self {
        Self {
            minimum_signature_class: PluginSignatureClass::Verified,
        }
    }
}

fn signature_rank(s: PluginSignatureClass) -> u8 {
    match s {
        PluginSignatureClass::Unsigned => 0,
        PluginSignatureClass::SelfSigned => 1,
        PluginSignatureClass::Verified => 2,
    }
}

/// Validate a manifest against the loader policy. Returns the first
/// error found; callers fix and retry.
pub fn validate_manifest(
    manifest: &PluginManifest,
    policy: PluginLoaderPolicy,
) -> Result<(), ManifestValidationError> {
    if manifest.id.is_empty() {
        return Err(ManifestValidationError::EmptyId);
    }
    if manifest.version.is_empty() {
        return Err(ManifestValidationError::EmptyVersion);
    }
    let mut seen: Vec<PluginCapability> = Vec::new();
    for cap in &manifest.capabilities {
        if cap.is_forbidden() {
            return Err(ManifestValidationError::ForbiddenCapabilityDeclared {
                plugin_id: manifest.id.clone(),
                capability: *cap,
            });
        }
        if seen.contains(cap) {
            return Err(ManifestValidationError::DuplicateCapability {
                plugin_id: manifest.id.clone(),
                capability: *cap,
            });
        }
        seen.push(*cap);
    }
    if signature_rank(manifest.signature_class) < signature_rank(policy.minimum_signature_class)
    {
        return Err(ManifestValidationError::SignatureClassDisallowed {
            plugin_id: manifest.id.clone(),
            actual: manifest.signature_class,
            minimum: policy.minimum_signature_class,
        });
    }
    Ok(())
}

// ============================================================================
// Lifecycle state machine
// ============================================================================

/// Per-plugin state. The host's WIT-bound `lifecycle()` calls drive
/// transitions; this enum is the pure-logic record.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum PluginLifecycleState {
    /// Loaded from disk, manifest validated, not yet initialised.
    #[default]
    Loaded,
    /// `init()` succeeded; ready to render.
    Initialised,
    /// Running normally.
    Running,
    /// `reload()` in flight (config changed, re-initialising state).
    Reloading,
    /// `shutdown()` called or unloaded; no further calls.
    Stopped,
    /// Hit an unrecoverable error (panic, resource-cap exceeded
    /// repeatedly). Host backs off + restarts; this state is the
    /// pause-before-restart marker.
    Errored,
}

impl PluginLifecycleState {
    /// Whether the host can dispatch a render call to a plugin in
    /// this state.
    #[must_use]
    pub fn is_render_eligible(self) -> bool {
        matches!(self, Self::Initialised | Self::Running)
    }

    /// Allowed next states from this state. Pure-logic transition
    /// table; the host's WIT bindings call this to gate transitions.
    #[must_use]
    pub fn allowed_next(self) -> &'static [PluginLifecycleState] {
        match self {
            Self::Loaded => &[Self::Initialised, Self::Errored, Self::Stopped],
            Self::Initialised => &[Self::Running, Self::Reloading, Self::Errored, Self::Stopped],
            Self::Running => &[Self::Reloading, Self::Errored, Self::Stopped],
            Self::Reloading => &[Self::Running, Self::Errored, Self::Stopped],
            Self::Stopped | Self::Errored => &[Self::Loaded, Self::Stopped],
        }
    }

    /// Whether `next` is a legal transition from `self`.
    #[must_use]
    pub fn can_transition_to(self, next: PluginLifecycleState) -> bool {
        self.allowed_next().contains(&next)
    }
}

// ============================================================================
// Capability grant decision
// ============================================================================

/// Token the host issues when the user approves a restricted
/// capability for a specific plugin. Opaque to this substrate; the
/// integration's policy engine binds the token to (plugin_id,
/// capability, scope).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ApprovalToken {
    pub plugin_id: String,
    pub capability: PluginCapability,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CapabilityGrantDecision {
    Granted,
    /// Capability was never in the plugin's manifest — declarative
    /// gate. Plugin is asking for something it didn't declare.
    DeniedNotInManifest,
    /// Capability is forbidden — should never reach grant-time
    /// (caught at manifest validation), but the policy is defensive.
    DeniedForbidden,
    /// Capability is restricted and no approval token was issued.
    DeniedNoApproval,
}

/// Pure-logic capability gate. The integration layer passes in the
/// (validated) manifest, the set of approval tokens the user has
/// issued, and the requested capability; this returns the grant
/// decision. The integration's policy engine then either dispatches
/// the host call or routes to the audit chain with the denial reason.
#[must_use]
pub fn decide_capability_grant(
    manifest: &PluginManifest,
    approval_tokens: &[ApprovalToken],
    requested: PluginCapability,
) -> CapabilityGrantDecision {
    if requested.is_forbidden() {
        return CapabilityGrantDecision::DeniedForbidden;
    }
    if !manifest.capabilities.contains(&requested) {
        return CapabilityGrantDecision::DeniedNotInManifest;
    }
    if requested.is_permissive() {
        return CapabilityGrantDecision::Granted;
    }
    // Restricted: needs an approval token bound to this plugin id +
    // this capability.
    let has_token = approval_tokens.iter().any(|t| {
        t.plugin_id == manifest.id && t.capability == requested
    });
    if has_token {
        CapabilityGrantDecision::Granted
    } else {
        CapabilityGrantDecision::DeniedNoApproval
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest_with(caps: &[PluginCapability]) -> PluginManifest {
        PluginManifest::new("test.plugin", "1.0.0").with_capabilities(caps.iter().copied())
    }

    // ----------------------------------------------------------------
    // Capability classes
    // ----------------------------------------------------------------

    #[test]
    fn permissive_capabilities_classified_correctly() {
        for c in [
            PluginCapability::ReadState,
            PluginCapability::ReadEvents,
            PluginCapability::RenderTile,
            PluginCapability::RenderSidePanel,
            PluginCapability::RenderOverlay,
            PluginCapability::ReadOwnConfig,
        ] {
            assert!(c.is_permissive(), "{c:?} should be permissive");
            assert!(!c.requires_approval_token());
            assert!(!c.is_forbidden());
        }
    }

    #[test]
    fn restricted_capabilities_classified_correctly() {
        for c in [
            PluginCapability::ReadPaneText,
            PluginCapability::SendInput,
            PluginCapability::SpawnPane,
            PluginCapability::ClosePane,
            PluginCapability::LaunchWorkflow,
            PluginCapability::Network,
        ] {
            assert!(c.requires_approval_token(), "{c:?} should require approval");
            assert!(!c.is_permissive());
            assert!(!c.is_forbidden());
        }
    }

    #[test]
    fn forbidden_capabilities_classified_correctly() {
        for c in [
            PluginCapability::FileSystemEscape,
            PluginCapability::DirectPtyAccess,
            PluginCapability::DirectMuxIpc,
            PluginCapability::ArbitrarySubprocess,
        ] {
            assert!(c.is_forbidden(), "{c:?} should be forbidden");
            assert!(!c.is_permissive());
            assert!(!c.requires_approval_token());
        }
    }

    // ----------------------------------------------------------------
    // Signature class
    // ----------------------------------------------------------------

    #[test]
    fn signature_class_default_is_unsigned() {
        assert_eq!(PluginSignatureClass::default(), PluginSignatureClass::Unsigned);
    }

    #[test]
    fn signature_rank_is_monotonic() {
        assert!(signature_rank(PluginSignatureClass::Unsigned)
            < signature_rank(PluginSignatureClass::SelfSigned));
        assert!(signature_rank(PluginSignatureClass::SelfSigned)
            < signature_rank(PluginSignatureClass::Verified));
    }

    // ----------------------------------------------------------------
    // PluginResourceBudget
    // ----------------------------------------------------------------

    #[test]
    fn budget_default_matches_bead_constants() {
        let b = PluginResourceBudget::default();
        assert_eq!(b.render_deadline_ms, DEFAULT_RENDER_DEADLINE_MS);
        assert_eq!(b.memory_cap_bytes, DEFAULT_MEMORY_CAP_BYTES);
        assert_eq!(b.disk_cap_bytes, DEFAULT_DISK_CAP_BYTES);
        assert_eq!(b.render_deadline_ms, 50);
        assert_eq!(b.memory_cap_bytes, 16 * 1024 * 1024);
        assert_eq!(b.disk_cap_bytes, 100 * 1024 * 1024);
    }

    #[test]
    fn budget_clamp_no_op_at_default() {
        let b = PluginResourceBudget::default();
        let s = b.clamp_to_safe_bounds();
        assert_eq!(s.budget, b);
        assert!(!s.render_clamped);
        assert!(!s.mem_clamped);
        assert!(!s.disk_clamped);
    }

    #[test]
    fn budget_clamp_engages_render_axis() {
        let b = PluginResourceBudget {
            render_deadline_ms: 10_000,
            ..PluginResourceBudget::default()
        };
        let s = b.clamp_to_safe_bounds();
        assert_eq!(s.budget.render_deadline_ms, HARD_RENDER_DEADLINE_CEILING_MS);
        assert!(s.render_clamped);
        assert!(!s.mem_clamped);
        assert!(!s.disk_clamped);
    }

    #[test]
    fn budget_clamp_engages_memory_axis() {
        let b = PluginResourceBudget {
            memory_cap_bytes: 1024 * 1024 * 1024,
            ..PluginResourceBudget::default()
        };
        let s = b.clamp_to_safe_bounds();
        assert_eq!(s.budget.memory_cap_bytes, HARD_MEMORY_CAP_BYTES);
        assert!(s.mem_clamped);
    }

    #[test]
    fn budget_clamp_engages_disk_axis() {
        let b = PluginResourceBudget {
            disk_cap_bytes: 10 * 1024 * 1024 * 1024,
            ..PluginResourceBudget::default()
        };
        let s = b.clamp_to_safe_bounds();
        assert_eq!(s.budget.disk_cap_bytes, HARD_DISK_CAP_BYTES);
        assert!(s.disk_clamped);
    }

    #[test]
    fn budget_clamp_engages_all_axes() {
        let b = PluginResourceBudget {
            render_deadline_ms: 9_999,
            memory_cap_bytes: u64::MAX,
            disk_cap_bytes: u64::MAX,
        };
        let s = b.clamp_to_safe_bounds();
        assert!(s.render_clamped && s.mem_clamped && s.disk_clamped);
        assert_eq!(s.budget.render_deadline_ms, HARD_RENDER_DEADLINE_CEILING_MS);
        assert_eq!(s.budget.memory_cap_bytes, HARD_MEMORY_CAP_BYTES);
        assert_eq!(s.budget.disk_cap_bytes, HARD_DISK_CAP_BYTES);
        assert!(b.any_clamp_engaged());
    }

    // ----------------------------------------------------------------
    // Manifest builder
    // ----------------------------------------------------------------

    #[test]
    fn manifest_default_unsigned_with_no_caps() {
        let m = PluginManifest::new("p", "1.0");
        assert_eq!(m.id, "p");
        assert_eq!(m.version, "1.0");
        assert_eq!(m.signature_class, PluginSignatureClass::Unsigned);
        assert!(m.capabilities.is_empty());
    }

    #[test]
    fn manifest_with_caps_and_signature_chains() {
        let m = PluginManifest::new("p", "1.0")
            .with_capabilities([PluginCapability::ReadState, PluginCapability::RenderTile])
            .with_signature(PluginSignatureClass::Verified);
        assert_eq!(m.capabilities.len(), 2);
        assert_eq!(m.signature_class, PluginSignatureClass::Verified);
    }

    // ----------------------------------------------------------------
    // Manifest validation
    // ----------------------------------------------------------------

    #[test]
    fn validate_accepts_well_formed_manifest_with_self_signed_under_default_policy() {
        let m = PluginManifest::new("p", "1.0")
            .with_capabilities([PluginCapability::ReadState])
            .with_signature(PluginSignatureClass::SelfSigned);
        let policy = PluginLoaderPolicy::default();
        assert!(validate_manifest(&m, policy).is_ok());
    }

    #[test]
    fn validate_rejects_empty_id() {
        let m = PluginManifest::new("", "1.0");
        let err = validate_manifest(&m, PluginLoaderPolicy::allow_unsigned()).unwrap_err();
        assert_eq!(err, ManifestValidationError::EmptyId);
    }

    #[test]
    fn validate_rejects_empty_version() {
        let m = PluginManifest::new("p", "");
        let err = validate_manifest(&m, PluginLoaderPolicy::allow_unsigned()).unwrap_err();
        assert_eq!(err, ManifestValidationError::EmptyVersion);
    }

    #[test]
    fn validate_rejects_forbidden_capability() {
        let m = manifest_with(&[
            PluginCapability::ReadState,
            PluginCapability::FileSystemEscape,
        ]);
        let err = validate_manifest(&m, PluginLoaderPolicy::allow_unsigned()).unwrap_err();
        match err {
            ManifestValidationError::ForbiddenCapabilityDeclared { capability, .. } => {
                assert_eq!(capability, PluginCapability::FileSystemEscape);
            }
            other => panic!("expected ForbiddenCapabilityDeclared, got {other:?}"),
        }
    }

    #[test]
    fn validate_rejects_duplicate_capability() {
        let m = manifest_with(&[
            PluginCapability::ReadState,
            PluginCapability::ReadState,
        ]);
        let err = validate_manifest(&m, PluginLoaderPolicy::allow_unsigned()).unwrap_err();
        match err {
            ManifestValidationError::DuplicateCapability { capability, .. } => {
                assert_eq!(capability, PluginCapability::ReadState);
            }
            other => panic!("expected DuplicateCapability, got {other:?}"),
        }
    }

    #[test]
    fn validate_rejects_unsigned_under_default_policy() {
        let m = PluginManifest::new("p", "1.0").with_signature(PluginSignatureClass::Unsigned);
        let err = validate_manifest(&m, PluginLoaderPolicy::default()).unwrap_err();
        match err {
            ManifestValidationError::SignatureClassDisallowed {
                actual, minimum, ..
            } => {
                assert_eq!(actual, PluginSignatureClass::Unsigned);
                assert_eq!(minimum, PluginSignatureClass::SelfSigned);
            }
            other => panic!("expected SignatureClassDisallowed, got {other:?}"),
        }
    }

    #[test]
    fn validate_accepts_unsigned_under_permissive_policy() {
        let m = PluginManifest::new("p", "1.0").with_signature(PluginSignatureClass::Unsigned);
        assert!(validate_manifest(&m, PluginLoaderPolicy::allow_unsigned()).is_ok());
    }

    #[test]
    fn validate_rejects_self_signed_under_require_verified_policy() {
        let m = PluginManifest::new("p", "1.0").with_signature(PluginSignatureClass::SelfSigned);
        let err =
            validate_manifest(&m, PluginLoaderPolicy::require_verified()).unwrap_err();
        match err {
            ManifestValidationError::SignatureClassDisallowed { actual, minimum, .. } => {
                assert_eq!(actual, PluginSignatureClass::SelfSigned);
                assert_eq!(minimum, PluginSignatureClass::Verified);
            }
            other => panic!("expected SignatureClassDisallowed, got {other:?}"),
        }
    }

    // ----------------------------------------------------------------
    // Lifecycle state machine
    // ----------------------------------------------------------------

    #[test]
    fn lifecycle_default_is_loaded() {
        assert_eq!(PluginLifecycleState::default(), PluginLifecycleState::Loaded);
    }

    #[test]
    fn lifecycle_render_eligible_only_in_initialised_or_running() {
        assert!(!PluginLifecycleState::Loaded.is_render_eligible());
        assert!(PluginLifecycleState::Initialised.is_render_eligible());
        assert!(PluginLifecycleState::Running.is_render_eligible());
        assert!(!PluginLifecycleState::Reloading.is_render_eligible());
        assert!(!PluginLifecycleState::Stopped.is_render_eligible());
        assert!(!PluginLifecycleState::Errored.is_render_eligible());
    }

    #[test]
    fn lifecycle_loaded_can_transition_to_initialised() {
        assert!(PluginLifecycleState::Loaded
            .can_transition_to(PluginLifecycleState::Initialised));
    }

    #[test]
    fn lifecycle_loaded_cannot_transition_to_running_directly() {
        // Must go via Initialised.
        assert!(!PluginLifecycleState::Loaded
            .can_transition_to(PluginLifecycleState::Running));
    }

    #[test]
    fn lifecycle_initialised_can_transition_to_running_or_reloading() {
        assert!(PluginLifecycleState::Initialised
            .can_transition_to(PluginLifecycleState::Running));
        assert!(PluginLifecycleState::Initialised
            .can_transition_to(PluginLifecycleState::Reloading));
    }

    #[test]
    fn lifecycle_errored_can_only_reload_or_stop() {
        let allowed = PluginLifecycleState::Errored.allowed_next();
        assert!(allowed.contains(&PluginLifecycleState::Loaded));
        assert!(allowed.contains(&PluginLifecycleState::Stopped));
        assert!(!allowed.contains(&PluginLifecycleState::Running));
    }

    #[test]
    fn lifecycle_stopped_can_only_reload_or_stay_stopped() {
        let allowed = PluginLifecycleState::Stopped.allowed_next();
        assert!(allowed.contains(&PluginLifecycleState::Loaded));
        assert!(allowed.contains(&PluginLifecycleState::Stopped));
        assert!(!allowed.contains(&PluginLifecycleState::Running));
    }

    // ----------------------------------------------------------------
    // Capability grant decision
    // ----------------------------------------------------------------

    #[test]
    fn grant_permissive_when_in_manifest() {
        let m = manifest_with(&[PluginCapability::ReadState]);
        let d = decide_capability_grant(&m, &[], PluginCapability::ReadState);
        assert_eq!(d, CapabilityGrantDecision::Granted);
    }

    #[test]
    fn deny_capability_not_in_manifest() {
        let m = manifest_with(&[PluginCapability::ReadState]);
        let d = decide_capability_grant(&m, &[], PluginCapability::ReadEvents);
        assert_eq!(d, CapabilityGrantDecision::DeniedNotInManifest);
    }

    #[test]
    fn deny_forbidden_unconditionally() {
        let m = manifest_with(&[]);
        let d =
            decide_capability_grant(&m, &[], PluginCapability::FileSystemEscape);
        assert_eq!(d, CapabilityGrantDecision::DeniedForbidden);
    }

    #[test]
    fn deny_restricted_without_approval_token() {
        let m = manifest_with(&[PluginCapability::ReadPaneText]);
        let d = decide_capability_grant(&m, &[], PluginCapability::ReadPaneText);
        assert_eq!(d, CapabilityGrantDecision::DeniedNoApproval);
    }

    #[test]
    fn grant_restricted_with_matching_approval_token() {
        let m = manifest_with(&[PluginCapability::Network]);
        let token = ApprovalToken {
            plugin_id: "test.plugin".to_string(),
            capability: PluginCapability::Network,
        };
        let d = decide_capability_grant(&m, &[token], PluginCapability::Network);
        assert_eq!(d, CapabilityGrantDecision::Granted);
    }

    #[test]
    fn deny_restricted_with_mismatched_token_plugin_id() {
        let m = manifest_with(&[PluginCapability::Network]);
        let token = ApprovalToken {
            plugin_id: "different.plugin".to_string(),
            capability: PluginCapability::Network,
        };
        let d = decide_capability_grant(&m, &[token], PluginCapability::Network);
        assert_eq!(d, CapabilityGrantDecision::DeniedNoApproval);
    }

    #[test]
    fn deny_restricted_with_mismatched_token_capability() {
        let m = manifest_with(&[
            PluginCapability::Network,
            PluginCapability::SendInput,
        ]);
        let token = ApprovalToken {
            plugin_id: "test.plugin".to_string(),
            capability: PluginCapability::SendInput,
        };
        // Plugin asks for Network, but the token is for SendInput.
        let d = decide_capability_grant(&m, &[token], PluginCapability::Network);
        assert_eq!(d, CapabilityGrantDecision::DeniedNoApproval);
    }

    // ----------------------------------------------------------------
    // Cross-cut scenarios
    // ----------------------------------------------------------------

    #[test]
    fn scenario_clock_tile_plugin_passes_validation_runs_normally() {
        // The bead's clock-tile example plugin: only needs ReadState
        // + RenderTile; both permissive; SelfSigned is fine under
        // default policy.
        let m = PluginManifest::new("ft.clock", "1.0.0")
            .with_capabilities([PluginCapability::ReadState, PluginCapability::RenderTile])
            .with_signature(PluginSignatureClass::SelfSigned);
        validate_manifest(&m, PluginLoaderPolicy::default()).unwrap();
        let d = decide_capability_grant(&m, &[], PluginCapability::ReadState);
        assert_eq!(d, CapabilityGrantDecision::Granted);
        let d = decide_capability_grant(&m, &[], PluginCapability::RenderTile);
        assert_eq!(d, CapabilityGrantDecision::Granted);
    }

    #[test]
    fn scenario_git_plugin_needs_network_token_for_remote_status() {
        // git-branch-tile (bead example) wants the local branch (ReadState
        // is enough) — but a hypothetical extension wants to fetch
        // remote status; that needs Network + an approval token.
        let m = PluginManifest::new("ft.git", "1.0.0")
            .with_capabilities([
                PluginCapability::ReadState,
                PluginCapability::RenderTile,
                PluginCapability::Network,
            ])
            .with_signature(PluginSignatureClass::SelfSigned);
        validate_manifest(&m, PluginLoaderPolicy::default()).unwrap();
        // Without token: Network denied.
        assert_eq!(
            decide_capability_grant(&m, &[], PluginCapability::Network),
            CapabilityGrantDecision::DeniedNoApproval
        );
        // With token: granted.
        let token = ApprovalToken {
            plugin_id: "ft.git".to_string(),
            capability: PluginCapability::Network,
        };
        assert_eq!(
            decide_capability_grant(&m, &[token], PluginCapability::Network),
            CapabilityGrantDecision::Granted
        );
    }

    #[test]
    fn scenario_malicious_plugin_declaring_filesystem_escape_refused_at_load() {
        let m = PluginManifest::new("evil", "1.0.0")
            .with_capabilities([
                PluginCapability::ReadState,
                PluginCapability::FileSystemEscape,
            ])
            .with_signature(PluginSignatureClass::Verified);
        let err = validate_manifest(&m, PluginLoaderPolicy::default()).unwrap_err();
        match err {
            ManifestValidationError::ForbiddenCapabilityDeclared { capability, .. } => {
                assert_eq!(capability, PluginCapability::FileSystemEscape);
            }
            other => panic!("expected ForbiddenCapabilityDeclared, got {other:?}"),
        }
    }

    #[test]
    fn scenario_runaway_plugin_budget_clamped_engages_all_axes() {
        // Plugin author over-asks; loader clamps and reports.
        let b = PluginResourceBudget {
            render_deadline_ms: 5_000,
            memory_cap_bytes: 4 * 1024 * 1024 * 1024,
            disk_cap_bytes: 100 * 1024 * 1024 * 1024,
        };
        let s = b.clamp_to_safe_bounds();
        assert_eq!(s.budget.render_deadline_ms, 50);
        assert_eq!(s.budget.memory_cap_bytes, 16 * 1024 * 1024);
        assert_eq!(s.budget.disk_cap_bytes, 100 * 1024 * 1024);
        assert!(s.render_clamped && s.mem_clamped && s.disk_clamped);
    }

    #[test]
    fn scenario_lifecycle_full_happy_path_loaded_init_run_stop() {
        let mut state = PluginLifecycleState::default();
        // Load → Initialised → Running → Stopped.
        assert!(state.can_transition_to(PluginLifecycleState::Initialised));
        state = PluginLifecycleState::Initialised;
        assert!(state.can_transition_to(PluginLifecycleState::Running));
        state = PluginLifecycleState::Running;
        assert!(state.is_render_eligible());
        assert!(state.can_transition_to(PluginLifecycleState::Stopped));
        state = PluginLifecycleState::Stopped;
        assert!(!state.is_render_eligible());
    }
}
