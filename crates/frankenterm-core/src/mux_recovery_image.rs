//! Canonical bounded whole-mux recovery image types, validation, and converter.
//!
//! Bead: `ft-interactive-swarm-product-convergence-7xqz4.8.14.1.2`
//!
//! This module defines the canonical, versioned root and bounded immutable
//! logical-object graph representing a whole-mux snapshot. It preserves:
//! - Exact ordered domain, window, and tab identities.
//! - Authoritative user tab order from live `WindowOrderRevision`.
//! - Un-lossy split trees with physical first/second `TerminalSize` (matching `tab.rs:2714`).
//! - Floating panes, floating focus, and zoom state.
//! - Pane stacks (`pane_stacks`) preserving hidden stack panes sharing a layout position.
//! - Per-client active workspace bindings (mirroring `mux/lib.rs:11710`) without inventing
//!   global workspace truth.
//! - Cryptographic binding of terminal checkpoint object references to the exact
//!   mux incarnation and pane registration generation, preventing snapshot-swapping attacks.
//! - Explicit authority distinction between `ModelOnly` parser ground captures and
//!   `Guardian` protocol leases (never promoting a raw digest to authority).
//! - Feature-gated (`#[cfg(feature = "frankenterm-deps")]`) un-lossy production converter
//!   from live [`mux::MuxCapturedTopology`] and per-pane [`mux::ModelParserCheckpointAck`]
//!   into canonical [`MuxRecoveryImage`].
//!
//! # Invariants and Resource Limits
//! - Hard byte ceiling: inputs exceeding [`MAX_RECOVERY_IMAGE_BYTES`] (16 MiB) fail pre-parse.
//! - Split tree recursion depth is bounded to [`MAX_SPLIT_TREE_DEPTH`] (32).
//! - Window, tab, and pane counts are capped at 4,096.
//! - All string lengths are strictly bounded to prevent memory exhaustion.
//! - Duplicate IDs (incarnation numeric IDs or stable UUIDs) fail validation closed.
//! - Exact 1-to-1 global placement bijection: every pane in the catalog must be placed
//!   in exactly one split leaf, floating pane, or hidden pane stack member across all
//!   windows and tabs. Duplicate placements and orphan catalog panes are strictly rejected.
//! - Placed pane UUIDs in split leaves and floating panes must match the pane catalog UUID.
//! - Checkpoint bindings require non-zero registration generation, non-empty object ID,
//!   non-zero payload length, non-zero digest, and validated authority metadata.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::io::Write;

// =============================================================================
// Constants & Limits
// =============================================================================

/// Canonical 4-byte magic identifying a FrankenTerm whole-mux recovery image.
pub const MUX_RECOVERY_IMAGE_MAGIC: [u8; 4] = *b"FTMR";

/// Current schema version.
pub const MUX_RECOVERY_IMAGE_SCHEMA_VERSION: u32 = 7;

/// Maximum payload bytes accepted for a recovery image JSON input (16 MiB).
pub const MAX_RECOVERY_IMAGE_BYTES: usize = 16 * 1024 * 1024;

/// Maximum domains admitted per recovery image.
pub const MAX_RECOVERY_DOMAINS: usize = 256;

/// Maximum windows admitted per recovery image.
pub const MAX_RECOVERY_WINDOWS: usize = 4_096;

/// Maximum tabs admitted per window.
pub const MAX_RECOVERY_TABS_PER_WINDOW: usize = 4_096;

/// Maximum panes admitted across the whole recovery image.
pub const MAX_RECOVERY_PANES: usize = 4_096;

/// Maximum depth of nested split trees (prevents stack overflow).
pub const MAX_SPLIT_TREE_DEPTH: usize = 32;

/// Maximum floating panes admitted per tab.
pub const MAX_FLOATING_PANES_PER_TAB: usize = 256;

/// Maximum string byte length for titles, names, workspaces, and paths.
pub const MAX_STRING_BYTES: usize = 1_024;

/// Maximum string byte length for session or incarnation identifiers.
pub const MAX_ID_STRING_BYTES: usize = 256;

/// Domain separator for computing the canonical image digest.
const HASH_DOMAIN_IMAGE: &[u8] = b"frankenterm:mux_recovery_image:v1";

// =============================================================================
// Error Types
// =============================================================================

/// Validation, conversion, and decoding errors for whole-mux recovery images.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum MuxRecoveryImageError {
    #[error("recovery image input too large: {bytes} bytes (limit {limit})")]
    TooLarge { bytes: usize, limit: usize },

    #[error("split tree nesting too deep: depth {depth} (limit {limit})")]
    TooDeep { depth: usize, limit: usize },

    #[error("resource limit exceeded for {resource}: count {count} (limit {limit})")]
    ResourceLimit {
        resource: &'static str,
        count: usize,
        limit: usize,
    },

    #[error("string field '{field}' exceeds length limit: {len} bytes (limit {limit})")]
    StringTooLong {
        field: &'static str,
        len: usize,
        limit: usize,
    },

    #[error("invalid magic: expected {expected:?}, found {found:?}")]
    InvalidMagic { expected: [u8; 4], found: [u8; 4] },

    #[error("unsupported schema version: {0}")]
    UnsupportedSchemaVersion(u32),

    #[error("invalid or incomplete workspace recovery state: {0}")]
    InvalidWorkspaceState(&'static str),

    #[error("invalid recovered tab runtime state: {0}")]
    InvalidTabRuntimeState(&'static str),

    #[error("invalid header: {0}")]
    InvalidHeader(&'static str),

    #[error("{field} must be a canonical nonnil UUID")]
    InvalidDurableIdentity { field: &'static str },

    #[error("duplicate incarnation window id {0}")]
    DuplicateWindowId(usize),

    #[error("duplicate stable window id '{0}'")]
    DuplicateStableWindowId(String),

    #[error("duplicate incarnation tab id {0}")]
    DuplicateTabId(usize),

    #[error("duplicate stable tab id '{0}'")]
    DuplicateStableTabId(String),

    #[error("duplicate incarnation pane id {0}")]
    DuplicatePaneId(usize),

    #[error("duplicate stable pane uuid '{0}'")]
    DuplicatePaneUuid(String),

    #[error("duplicate pane placement in topology: pane id {0} placed multiple times")]
    DuplicatePanePlacement(usize),

    #[error(
        "orphan pane in catalog: pane id {0} is never placed in any tab split tree, stack, or floating list"
    )]
    OrphanCatalogPane(usize),

    #[error("duplicate domain name '{0}'")]
    DuplicateDomainName(String),

    #[error("duplicate incarnation domain id {0}")]
    DuplicateDomainId(usize),

    #[error("referenced domain '{0}' not found in image domain catalog")]
    MissingDomain(String),

    #[error(
        "pane id {0} referenced in tab split tree, stack, or floating list not found in pane catalog"
    )]
    MissingPane(usize),

    #[error(
        "placed pane {pane_id} uuid '{placed_uuid}' does not match catalog uuid '{catalog_uuid}'"
    )]
    PaneUuidMismatchCatalog {
        pane_id: usize,
        catalog_uuid: String,
        placed_uuid: String,
    },

    #[error("window {window_id} active tab index {index} is out of bounds (tab count {count})")]
    InvalidActiveTabIndex {
        window_id: usize,
        index: usize,
        count: usize,
    },

    #[error(
        "tab {tab_id} active pane id {pane_id} not found in split tree, stacks, or floating panes"
    )]
    InvalidActivePaneId { tab_id: usize, pane_id: usize },

    #[error("tab {tab_id} floating focus pane id {pane_id} not found in tab floating panes")]
    InvalidFloatingFocusId { tab_id: usize, pane_id: usize },

    #[error("tab {tab_id} zoomed pane id {pane_id} not found in tab panes")]
    InvalidZoomedPaneId { tab_id: usize, pane_id: usize },

    #[error(
        "tab {tab_id} underlying tiled active pane id {pane_id} not found in split tree leaves"
    )]
    InvalidUnderlyingTiledActivePaneId { tab_id: usize, pane_id: usize },

    #[error("tab {tab_id} has contradictory active pane state: {reason}")]
    ContradictoryActivePaneState { tab_id: usize, reason: &'static str },

    #[error("tab {tab_id} has duplicate pane stack slot {slot_index}")]
    DuplicateStackSlot { tab_id: usize, slot_index: usize },

    #[error(
        "tab {tab_id} pane stack slot {slot_index} is out of bounds for split tree (leaf count {leaf_count})"
    )]
    StackSlotOutOfBounds {
        tab_id: usize,
        slot_index: usize,
        leaf_count: usize,
    },

    #[error(
        "tab {tab_id} pane stack slot {slot_index} active member {found_pane_id} does not match split tree leaf {expected_pane_id}"
    )]
    StackActiveMemberMismatch {
        tab_id: usize,
        slot_index: usize,
        expected_pane_id: usize,
        found_pane_id: usize,
    },

    #[error("focused window id {0} not found in window catalog")]
    InvalidFocusedWindowId(usize),

    #[error(
        "checkpoint binding for pane {pane_id} has mismatched incarnation: expected '{expected}', found '{found}'"
    )]
    IncarnationMismatch {
        pane_id: usize,
        expected: String,
        found: String,
    },

    #[error(
        "checkpoint binding for pane {pane_id} has mismatched pane uuid: expected '{expected}', found '{found}'"
    )]
    PaneUuidMismatch {
        pane_id: usize,
        expected: String,
        found: String,
    },

    #[error("checkpoint ref for pane {pane_id} is invalid: {reason}")]
    InvalidCheckpointRef {
        pane_id: usize,
        reason: &'static str,
    },

    #[error("authority for pane {pane_id} is invalid: {reason}")]
    InvalidAuthority {
        pane_id: usize,
        reason: &'static str,
    },

    #[error("malformed split node: {0}")]
    MalformedSplit(&'static str),

    #[error("image digest mismatch: computed {computed}, recorded {recorded}")]
    DigestMismatch { computed: String, recorded: String },

    #[error("conversion error: pane {0} missing from model parser checkpoint acks")]
    MissingCheckpointAck(usize),

    #[error("conversion error: pane {0} missing from checkpoint object refs")]
    MissingCheckpointObjectRef(usize),

    #[error(
        "conversion error: extraneous checkpoint ack for pane {0} not in captured pane bindings"
    )]
    ExtraneousCheckpointAck(usize),

    #[error(
        "conversion error: extraneous checkpoint object ref for pane {0} not in captured pane bindings"
    )]
    ExtraneousCheckpointObjectRef(usize),

    #[error(
        "conversion error: pane {pane_id} registration wire identity mismatch: captured {captured:?}, ack {ack:?}"
    )]
    RegistrationWireIdentityMismatch {
        pane_id: usize,
        captured: [u8; 16],
        ack: [u8; 16],
    },

    #[error("conversion error: pane {0} has all-zero registration wire identity")]
    ZeroRegistrationWireIdentity(usize),

    #[error("conversion error: pane {0} has nil durable pane id")]
    NilDurablePaneId(usize),

    #[error(
        "conversion error: pane {pane_id} durable uuid mismatch: captured '{captured}', ack '{ack}'"
    )]
    DurablePaneUuidMismatch {
        pane_id: usize,
        captured: String,
        ack: String,
    },

    #[error(
        "pane {pane_id} ACK stream watermark {ack} differs from its terminal checkpoint watermark {checkpoint}"
    )]
    ParserStreamWatermarkMismatch {
        pane_id: usize,
        ack: u64,
        checkpoint: u64,
    },

    #[error("invalid captured topology: {0}")]
    InvalidCapturedTopology(&'static str),

    #[error("pane {pane_id} terminal checkpoint model is invalid: {reason}")]
    InvalidCheckpointModel {
        pane_id: usize,
        reason: &'static str,
    },

    #[error("serialization error: {0}")]
    Serialization(String),

    #[error("deserialization error: {0}")]
    Deserialization(String),
}

// =============================================================================
// Streaming & Bounded IO Adapters
// =============================================================================

/// An [`std::io::Write`] sink that feeds bytes directly to an underlying [`Sha256`] hasher without heap allocation.
pub struct Sha256Sink<'a> {
    hasher: &'a mut Sha256,
}

impl<'a> Sha256Sink<'a> {
    pub fn new(hasher: &'a mut Sha256) -> Self {
        Self { hasher }
    }
}

impl Write for Sha256Sink<'_> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.hasher.update(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// A writer wrapper that enforces an upper bound on total bytes written.
pub struct BoundedWriter<W> {
    inner: W,
    written: usize,
    limit: usize,
    exceeded: bool,
}

impl<W: Write> BoundedWriter<W> {
    pub fn new(inner: W, limit: usize) -> Self {
        Self {
            inner,
            written: 0,
            limit,
            exceeded: false,
        }
    }

    pub fn written(&self) -> usize {
        self.written
    }

    pub fn exceeded(&self) -> bool {
        self.exceeded
    }

    pub fn into_inner(self) -> W {
        self.inner
    }
}

impl<W: Write> Write for BoundedWriter<W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if self.written.saturating_add(buf.len()) > self.limit {
            self.exceeded = true;
            return Err(std::io::Error::other(format!(
                "output byte limit exceeded: attempted {} bytes with limit {}",
                self.written.saturating_add(buf.len()),
                self.limit
            )));
        }
        let n = self.inner.write(buf)?;
        self.written += n;
        Ok(n)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

// =============================================================================
// Core Data Types
// =============================================================================

/// Canonical physical terminal dimensions matching `frankenterm_term::TerminalSize`.
#[derive(Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerminalSize {
    pub rows: usize,
    pub cols: usize,
    pub pixel_width: usize,
    pub pixel_height: usize,
    pub dpi: u32,
}

impl fmt::Debug for TerminalSize {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}x{} ({}x{}px @ {}dpi)",
            self.cols, self.rows, self.pixel_width, self.pixel_height, self.dpi
        )
    }
}

/// Split orientation matching `frankenterm_mux::tab::SplitDirection`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum SplitDirection {
    Horizontal,
    Vertical,
}

/// Split geometry matching `frankenterm_mux::tab::SplitDirectionAndSize` (`tab.rs:2714`).
///
/// Preserves exact child terminal sizes and separator math without floating-point or
/// ratio loss.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SplitDirectionAndSize {
    pub direction: SplitDirection,
    pub first: TerminalSize,
    pub second: TerminalSize,
}

/// Binary split tree matching `bintree::Tree<Arc<dyn Pane>, SplitDirectionAndSize>`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum RecoverySplitNode {
    Leaf {
        pane_id: usize,
        pane_uuid: String,
    },
    Split {
        split: SplitDirectionAndSize,
        left: Box<RecoverySplitNode>,
        right: Box<RecoverySplitNode>,
    },
}

impl RecoverySplitNode {
    /// Collects all pane leaf entries `(pane_id, &pane_uuid)` in deterministic depth-first order.
    #[must_use]
    pub fn leaves(&self) -> Vec<(usize, &str)> {
        let mut out = Vec::new();
        self.collect_leaves(&mut out);
        out
    }

    fn collect_leaves<'a>(&'a self, out: &mut Vec<(usize, &'a str)>) {
        match self {
            Self::Leaf { pane_id, pane_uuid } => {
                out.push((*pane_id, pane_uuid.as_str()));
            }
            Self::Split { left, right, .. } => {
                left.collect_leaves(out);
                right.collect_leaves(out);
            }
        }
    }
}

/// Position and dimensions of a floating pane on the terminal grid.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FloatingPaneRect {
    pub left: usize,
    pub top: usize,
    pub width: usize,
    pub height: usize,
}

/// Exact state of an un-tiled floating pane within a tab.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecoveryFloatingPane {
    pub pane_id: usize,
    pub pane_uuid: String,
    pub rect: FloatingPaneRect,
    pub z_order: u32,
    pub visible: bool,
    pub pinned: bool,
    /// Bit-cast IEEE 754 float (`f32::to_bits()`) for deterministic serialization.
    pub opacity_bits: u32,
}

impl RecoveryFloatingPane {
    /// Helper to construct with standard float opacity.
    #[must_use]
    pub fn new(
        pane_id: usize,
        pane_uuid: String,
        rect: FloatingPaneRect,
        z_order: u32,
        visible: bool,
        pinned: bool,
        opacity: f32,
    ) -> Self {
        let opacity_bits = if opacity.is_finite() && (0.0..=1.0).contains(&opacity) {
            opacity.to_bits()
        } else {
            1.0f32.to_bits()
        };
        Self {
            pane_id,
            pane_uuid,
            rect,
            z_order,
            visible,
            pinned,
            opacity_bits,
        }
    }

    /// Retrieve opacity as float.
    #[must_use]
    pub fn opacity(&self) -> f32 {
        f32::from_bits(self.opacity_bits)
    }
}

/// Stack of panes sharing a single layout slot within a tab.
///
/// Only the pane at `active_index` is visible in the split tree;
/// remaining panes are hidden stack members.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecoveryPaneStack {
    /// Slot index or root pane ID within the tab layout.
    pub slot_index: usize,
    /// Ordered list of pane IDs in this stack.
    pub pane_ids: Vec<usize>,
    /// Index of currently visible/active pane in `pane_ids`.
    pub active_index: usize,
}

impl RecoveryPaneStack {
    /// Returns the active pane ID for this stack.
    #[must_use]
    pub fn active_pane_id(&self) -> Option<usize> {
        self.pane_ids.get(self.active_index).copied()
    }
}

/// Top-level whole-mux recovery image envelope.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MuxRecoveryImage {
    pub header: RecoveryImageHeader,
    pub topology: RecoveryTopology,
    pub panes: Vec<RecoveryPane>,
    /// SHA-256 digest over canonical serialized representation of header (sans digest),
    /// topology, and panes.
    pub image_digest: [u8; 32],
}

impl fmt::Debug for MuxRecoveryImage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MuxRecoveryImage")
            .field("header", &self.header)
            .field("windows_count", &self.topology.windows.len())
            .field("domains_count", &self.topology.domains.len())
            .field("panes_count", &self.panes.len())
            .field("image_digest_hex", &hex::encode(self.image_digest))
            .finish()
    }
}

/// Metadata header for whole-mux recovery images.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecoveryImageHeader {
    pub magic: [u8; 4],
    pub schema_version: u32,
    pub generation: u64,
    pub predecessor_digest: Option<[u8; 32]>,
    pub created_at_epoch_ms: u64,
    /// Stable or incarnation identifier for the live mux process.
    pub mux_incarnation_id: String,
    pub ft_version: String,
    pub session_id: String,
}

/// Topology container holding domains, windows, and optional client workspace bindings.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecoveryTopology {
    pub domains: Vec<RecoveryDomain>,
    pub windows: Vec<RecoveryWindow>,
    pub focused_window_id: Option<usize>,
    /// Active workspace is per-client (`mux/lib.rs:11710`), not global truth.
    pub client_workspace: Option<ClientWorkspaceBinding>,
    /// Required from schema 5 onward. Absent historical state stays absent and cannot
    /// authorize a complete live restore; omission preserves schema 3/4 hashes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_state: Option<RecoveryWorkspaceState>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub domain_state: Option<RecoveryDomainState>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecoveryDomainState {
    #[serde(deserialize_with = "deserialize_required_workspace_active_window")]
    pub default_domain_id: Option<usize>,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecoveryLocalDomainPolicy {
    #[serde(deserialize_with = "deserialize_required_domain_program")]
    pub default_prog: Option<Vec<String>>,
    #[serde(deserialize_with = "deserialize_required_domain_cwd")]
    pub default_cwd: Option<String>,
    pub environment: std::collections::BTreeMap<String, String>,
    pub term: String,
}

impl fmt::Debug for RecoveryLocalDomainPolicy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RecoveryLocalDomainPolicy")
            .field("argument_count", &self.default_prog.as_ref().map(Vec::len))
            .field("has_default_cwd", &self.default_cwd.is_some())
            .field("environment_count", &self.environment.len())
            .finish_non_exhaustive()
    }
}

fn deserialize_required_domain_program<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<Vec<String>>, D::Error> {
    Option::<Vec<String>>::deserialize(deserializer)
}

fn deserialize_required_domain_cwd<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<String>, D::Error> {
    Option::<String>::deserialize(deserializer)
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub enum RecoveryDomainPolicy {
    Local(RecoveryLocalDomainPolicy),
    GuardianLocal {
        commands: RecoveryLocalDomainPolicy,
        socket_path: String,
        token_path: String,
    },
}

impl RecoveryDomainPolicy {
    #[cfg(test)]
    pub(crate) fn local_test_fixture() -> Self {
        Self::Local(RecoveryLocalDomainPolicy {
            default_prog: None,
            default_cwd: None,
            environment: std::collections::BTreeMap::new(),
            term: "xterm-256color".into(),
        })
    }

    fn validate(&self) -> Result<usize, MuxRecoveryImageError> {
        let invalid =
            || MuxRecoveryImageError::InvalidCapturedTopology("invalid domain recovery policy");
        let (commands, endpoints) = match self {
            Self::Local(commands) => (commands, None),
            Self::GuardianLocal {
                commands,
                socket_path,
                token_path,
            } => (commands, Some([socket_path.as_str(), token_path.as_str()])),
        };
        if commands
            .default_prog
            .as_ref()
            .is_some_and(|args| args.is_empty() || args.len() > 4096)
            || commands.environment.len() > 4096
            || commands
                .environment
                .keys()
                .any(|key| key.is_empty() || key.contains('='))
        {
            return Err(invalid());
        }
        let mut bytes = 0usize;
        let values = commands
            .default_prog
            .iter()
            .flatten()
            .map(String::as_str)
            .chain(
                commands
                    .environment
                    .iter()
                    .flat_map(|(key, value)| [key.as_str(), value.as_str()]),
            )
            .chain(commands.default_cwd.as_deref())
            .chain(std::iter::once(commands.term.as_str()));
        for value in values {
            if value.len() > 65_536 || value.contains('\0') {
                return Err(invalid());
            }
            bytes = bytes.checked_add(value.len()).ok_or_else(invalid)?;
        }
        if bytes > 2 * 1024 * 1024 {
            return Err(invalid());
        }
        if let Some(endpoints) = endpoints {
            for path in endpoints {
                if path.len() > 65_536
                    || path.contains('\0')
                    || !std::path::Path::new(path).is_absolute()
                {
                    return Err(invalid());
                }
                bytes = bytes.checked_add(path.len()).ok_or_else(invalid)?;
            }
        }
        Ok(bytes)
    }

    #[cfg(feature = "frankenterm-deps")]
    fn from_mux(policy: &mux::domain::DomainRecoveryPolicy) -> Result<Self, MuxRecoveryImageError> {
        let invalid = || {
            MuxRecoveryImageError::InvalidCapturedTopology("invalid native domain recovery policy")
        };
        let commands = |policy: &mux::domain::LocalDomainRecoveryPolicy| -> Result<RecoveryLocalDomainPolicy, MuxRecoveryImageError> {
            policy.validate().map_err(|_| invalid())?;
            Ok(RecoveryLocalDomainPolicy {
                default_prog: policy.default_prog.clone(),
                default_cwd: policy.default_cwd.as_ref().map(|path| path.to_str().map(str::to_owned).ok_or_else(invalid)).transpose()?,
                environment: policy.environment.clone(), term: policy.term.clone(),
            })
        };
        let result = match policy {
            mux::domain::DomainRecoveryPolicy::Local(policy) => Self::Local(commands(policy)?),
            mux::domain::DomainRecoveryPolicy::GuardianLocal {
                commands: policy,
                socket_path,
                token_path,
            } => {
                for path in [socket_path, token_path] {
                    let text = path.to_str().ok_or_else(invalid)?;
                    if !path.is_absolute() || text.len() > 65_536 || text.contains('\0') {
                        return Err(invalid());
                    }
                }
                Self::GuardianLocal {
                    commands: commands(policy)?,
                    socket_path: socket_path.to_str().ok_or_else(invalid)?.to_owned(),
                    token_path: token_path.to_str().ok_or_else(invalid)?.to_owned(),
                }
            }
        };
        result.validate()?;
        Ok(result)
    }

    #[cfg(feature = "frankenterm-deps")]
    pub(crate) fn to_mux(&self) -> mux::domain::DomainRecoveryPolicy {
        let commands =
            |policy: &RecoveryLocalDomainPolicy| mux::domain::LocalDomainRecoveryPolicy {
                default_prog: policy.default_prog.clone(),
                default_cwd: policy.default_cwd.as_ref().map(std::path::PathBuf::from),
                environment: policy.environment.clone(),
                term: policy.term.clone(),
            };
        match self {
            Self::Local(policy) => mux::domain::DomainRecoveryPolicy::Local(commands(policy)),
            Self::GuardianLocal {
                commands: policy,
                socket_path,
                token_path,
            } => mux::domain::DomainRecoveryPolicy::GuardianLocal {
                commands: commands(policy),
                socket_path: socket_path.into(),
                token_path: token_path.into(),
            },
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecoveryWorkspaceState {
    pub default_workspace: String,
    pub workspaces: Vec<RecoveryWorkspace>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecoveryWorkspace {
    pub name: String,
    pub window_ids: Vec<usize>,
    #[serde(deserialize_with = "deserialize_required_workspace_active_window")]
    pub active_window_id: Option<usize>,
}

fn deserialize_required_workspace_active_window<'de, D>(
    deserializer: D,
) -> Result<Option<usize>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Option::<usize>::deserialize(deserializer)
}

/// Binds a specific client identity to its active workspace.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientWorkspaceBinding {
    pub client_id: String,
    pub active_workspace: String,
}

/// Representation of a live multiplexer domain.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecoveryDomain {
    pub incarnation_domain_id: usize,
    pub domain_name: String,
    pub is_attached: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recovery_policy: Option<RecoveryDomainPolicy>,
}

/// Exact configured window placement, without inventing desktop dimensions.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecoveryGuiPosition {
    pub x: RecoveryGuiDimension,
    pub y: RecoveryGuiDimension,
    pub origin: RecoveryGeometryOrigin,
}

/// Unit-tagged IEEE-754 f32 bits preserve the captured coordinate exactly.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum RecoveryGuiDimension {
    Points(u32),
    Pixels(u32),
    Percent(u32),
    Cells(u32),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum RecoveryGeometryOrigin {
    ScreenCoordinateSystem,
    MainScreen,
    ActiveScreen,
    Named(String),
}

#[cfg(feature = "frankenterm-deps")]
impl From<&config::GuiPosition> for RecoveryGuiPosition {
    fn from(position: &config::GuiPosition) -> Self {
        fn dimension(value: config::Dimension) -> RecoveryGuiDimension {
            match value {
                config::Dimension::Points(value) => RecoveryGuiDimension::Points(value.to_bits()),
                config::Dimension::Pixels(value) => RecoveryGuiDimension::Pixels(value.to_bits()),
                config::Dimension::Percent(value) => RecoveryGuiDimension::Percent(value.to_bits()),
                config::Dimension::Cells(value) => RecoveryGuiDimension::Cells(value.to_bits()),
            }
        }
        Self {
            x: dimension(position.x),
            y: dimension(position.y),
            origin: match &position.origin {
                config::GeometryOrigin::ScreenCoordinateSystem => {
                    RecoveryGeometryOrigin::ScreenCoordinateSystem
                }
                config::GeometryOrigin::MainScreen => RecoveryGeometryOrigin::MainScreen,
                config::GeometryOrigin::ActiveScreen => RecoveryGeometryOrigin::ActiveScreen,
                config::GeometryOrigin::Named(name) => RecoveryGeometryOrigin::Named(name.clone()),
            },
        }
    }
}

/// Ordered window representation preserving exact user tab order.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecoveryWindow {
    pub window_id: usize,
    pub stable_window_id: String,
    pub workspace: String,
    pub title: String,
    pub order_revision: u64,
    pub gui_position: Option<RecoveryGuiPosition>,
    pub tabs: Vec<RecoveryTab>,
    pub active_tab_index: usize,
    #[serde(deserialize_with = "deserialize_required_last_active_tab_id")]
    pub last_active_tab_id: Option<usize>,
    pub tab_stacks: Vec<RecoveryTabStack>,
}

// An absent field must not silently become None in a supposedly complete image.
fn deserialize_required_last_active_tab_id<'de, D>(
    deserializer: D,
) -> Result<Option<usize>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Option::<usize>::deserialize(deserializer)
}

/// Window-level tab grouping; member order and visible identity are semantic.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecoveryTabStack {
    pub stack_id: usize,
    pub tab_ids: Vec<usize>,
    pub visible_tab_id: usize,
}

/// Ordered tab representation preserving exact split hierarchy, sizes, floating panes, and stacks.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecoveryTab {
    pub tab_id: usize,
    pub stable_tab_id: String,
    /// Mux-owned tab display title from the captured tab, not a pane process-title fallback.
    pub title: String,
    /// Optional launch metadata, separate from checkpoint terminal directory state.
    /// Live conversion leaves this absent rather than claiming an OS-process CWD cut.
    pub working_dir: Option<String>,
    pub size: TerminalSize,
    pub size_before_zoom: TerminalSize,
    pub zoomed_pane_id: Option<usize>,
    pub root_split: Option<RecoverySplitNode>,
    pub floating_panes: Vec<RecoveryFloatingPane>,
    pub floating_focus: Option<usize>,
    pub active_pane_id: usize,
    /// Stacked panes sharing a layout position (first/active pane in split tree, others hidden).
    #[serde(default)]
    pub pane_stacks: Vec<RecoveryPaneStack>,
    /// Active pane of the underlying tiled split tree when floating focus is active.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub underlying_tiled_active_pane_id: Option<usize>,
    /// Schema 6 captures behavior that cannot be reconstructed from geometry.
    /// Historical absence is preserved in canonical bytes and is not a default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime_state: Option<RecoveryTabRuntime>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecoveryTabRuntime {
    pub recency_count: usize,
    pub recency_by_index: Vec<(usize, usize)>,
    #[serde(deserialize_with = "deserialize_required_layout_cycle")]
    pub layout_cycle: Option<RecoveryLayoutCycle>,
    pub constraint_overrides: Vec<(usize, RecoveryPaneConstraints)>,
    pub collapsed_panes: Vec<usize>,
}

fn deserialize_required_layout_cycle<'de, D>(
    deserializer: D,
) -> Result<Option<RecoveryLayoutCycle>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Option::<RecoveryLayoutCycle>::deserialize(deserializer)
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecoveryPaneConstraints {
    pub min_width: usize,
    pub min_height: usize,
    pub max_width: Option<usize>,
    pub max_height: Option<usize>,
    pub preferred_width: Option<usize>,
    pub preferred_height: Option<usize>,
    pub fixed: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecoveryLayoutCycle {
    pub layouts: Vec<RecoverySwapLayout>,
    pub current_index: usize,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecoverySwapLayout {
    pub name: String,
    pub description: Option<String>,
    pub arrangement: RecoveryLayoutArrangement,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub enum RecoveryLayoutArrangement {
    Split {
        direction: SplitDirection,
        /// Preserve the exact finite IEEE-754 ratio, including signed zero.
        ratio_bits: u64,
        first: Box<Self>,
        second: Box<Self>,
    },
    Slot {
        is_main: bool,
    },
}

impl RecoveryTabRuntime {
    #[cfg(feature = "frankenterm-deps")]
    fn from_mux(
        state: &mux::tab::MuxCapturedTabRuntime,
        pane_ids: &HashSet<usize>,
    ) -> Result<Self, MuxRecoveryImageError> {
        fn arrangement(node: &mux::layout::LayoutArrangement) -> RecoveryLayoutArrangement {
            match node {
                mux::layout::LayoutArrangement::Slot { is_main } => {
                    RecoveryLayoutArrangement::Slot { is_main: *is_main }
                }
                mux::layout::LayoutArrangement::Split {
                    direction,
                    ratio,
                    first,
                    second,
                } => RecoveryLayoutArrangement::Split {
                    direction: match direction {
                        mux::tab::SplitDirection::Horizontal => SplitDirection::Horizontal,
                        mux::tab::SplitDirection::Vertical => SplitDirection::Vertical,
                    },
                    ratio_bits: ratio.to_bits(),
                    first: Box::new(arrangement(first)),
                    second: Box::new(arrangement(second)),
                },
            }
        }
        state.validate(pane_ids).map_err(|_| {
            MuxRecoveryImageError::InvalidTabRuntimeState("invalid captured tab runtime state")
        })?;
        Ok(Self {
            recency_count: state.recency_count,
            recency_by_index: state.recency_by_index.clone(),
            layout_cycle: state
                .layout_cycle
                .as_ref()
                .map(|cycle| RecoveryLayoutCycle {
                    current_index: cycle.current_index,
                    layouts: cycle
                        .layouts
                        .iter()
                        .map(|layout| RecoverySwapLayout {
                            name: layout.name.clone(),
                            description: layout.description.clone(),
                            arrangement: arrangement(&layout.arrangement),
                        })
                        .collect(),
                }),
            constraint_overrides: state
                .constraint_overrides
                .iter()
                .map(|(id, c)| {
                    (
                        *id,
                        RecoveryPaneConstraints {
                            min_width: c.min_width,
                            min_height: c.min_height,
                            max_width: c.max_width,
                            max_height: c.max_height,
                            preferred_width: c.preferred_width,
                            preferred_height: c.preferred_height,
                            fixed: c.fixed,
                        },
                    )
                })
                .collect(),
            collapsed_panes: state.collapsed_panes.clone(),
        })
    }

    #[cfg(feature = "frankenterm-deps")]
    pub(crate) fn to_mux(&self) -> mux::tab::MuxCapturedTabRuntime {
        fn arrangement(node: &RecoveryLayoutArrangement) -> mux::layout::LayoutArrangement {
            match node {
                RecoveryLayoutArrangement::Slot { is_main } => {
                    mux::layout::LayoutArrangement::Slot { is_main: *is_main }
                }
                RecoveryLayoutArrangement::Split {
                    direction,
                    ratio_bits,
                    first,
                    second,
                } => mux::layout::LayoutArrangement::Split {
                    direction: match direction {
                        SplitDirection::Horizontal => mux::tab::SplitDirection::Horizontal,
                        SplitDirection::Vertical => mux::tab::SplitDirection::Vertical,
                    },
                    ratio: f64::from_bits(*ratio_bits),
                    first: Box::new(arrangement(first)),
                    second: Box::new(arrangement(second)),
                },
            }
        }
        mux::tab::MuxCapturedTabRuntime {
            recency_count: self.recency_count,
            recency_by_index: self.recency_by_index.clone(),
            layout_cycle: self.layout_cycle.as_ref().map(|cycle| {
                mux::tab::MuxCapturedLayoutCycle {
                    current_index: cycle.current_index,
                    layouts: cycle
                        .layouts
                        .iter()
                        .map(|layout| mux::layout::SwapLayout {
                            name: layout.name.clone(),
                            description: layout.description.clone(),
                            arrangement: arrangement(&layout.arrangement),
                        })
                        .collect(),
                }
            }),
            constraint_overrides: self
                .constraint_overrides
                .iter()
                .map(|(id, c)| {
                    (
                        *id,
                        mux::pane::PaneConstraints {
                            min_width: c.min_width,
                            min_height: c.min_height,
                            max_width: c.max_width,
                            max_height: c.max_height,
                            preferred_width: c.preferred_width,
                            preferred_height: c.preferred_height,
                            fixed: c.fixed,
                        },
                    )
                })
                .collect(),
            collapsed_panes: self.collapsed_panes.clone(),
        }
    }

    fn validate_bounds(&self) -> Result<(), MuxRecoveryImageError> {
        for (resource, count) in [
            ("tab_recency", self.recency_by_index.len()),
            ("tab_constraints", self.constraint_overrides.len()),
            ("tab_collapsed_panes", self.collapsed_panes.len()),
        ] {
            if count > MAX_RECOVERY_PANES {
                return Err(MuxRecoveryImageError::ResourceLimit {
                    resource,
                    count,
                    limit: MAX_RECOVERY_PANES,
                });
            }
        }
        if let Some(cycle) = &self.layout_cycle {
            if cycle.layouts.is_empty()
                || cycle.layouts.len() > 64
                || cycle.current_index >= cycle.layouts.len()
            {
                return Err(MuxRecoveryImageError::InvalidTabRuntimeState(
                    "invalid layout cycle",
                ));
            }
            let mut nodes = 0usize;
            let mut text_bytes = 0usize;
            for layout in &cycle.layouts {
                validate_string_len("layout.name", &layout.name, MAX_STRING_BYTES)?;
                text_bytes += layout.name.len();
                if let Some(description) = &layout.description {
                    validate_string_len("layout.description", description, MAX_STRING_BYTES)?;
                    text_bytes += description.len();
                }
                if text_bytes > 65_536 {
                    return Err(MuxRecoveryImageError::InvalidTabRuntimeState(
                        "layout text budget exceeded",
                    ));
                }
                let mut pending = vec![(&layout.arrangement, 1usize)];
                while let Some((node, depth)) = pending.pop() {
                    nodes += 1;
                    if nodes > 8_191 || depth > MAX_SPLIT_TREE_DEPTH {
                        return Err(MuxRecoveryImageError::InvalidTabRuntimeState(
                            "layout tree budget exceeded",
                        ));
                    }
                    if let RecoveryLayoutArrangement::Split {
                        ratio_bits,
                        first,
                        second,
                        ..
                    } = node
                    {
                        let ratio = f64::from_bits(*ratio_bits);
                        if !ratio.is_finite() || !(0.0..=1.0).contains(&ratio) {
                            return Err(MuxRecoveryImageError::InvalidTabRuntimeState(
                                "invalid layout ratio",
                            ));
                        }
                        pending.push((second, depth + 1));
                        pending.push((first, depth + 1));
                    }
                }
            }
        }
        Ok(())
    }

    fn validate_membership(&self, pane_ids: &[usize]) -> Result<(), MuxRecoveryImageError> {
        if !self
            .recency_by_index
            .windows(2)
            .all(|pair| pair[0].0 < pair[1].0)
            || self
                .recency_by_index
                .iter()
                .any(|(_, score)| *score > self.recency_count)
            || !self
                .constraint_overrides
                .windows(2)
                .all(|pair| pair[0].0 < pair[1].0)
            || !self
                .collapsed_panes
                .windows(2)
                .all(|pair| pair[0] < pair[1])
        {
            return Err(MuxRecoveryImageError::InvalidTabRuntimeState(
                "noncanonical runtime entries",
            ));
        }
        let members: HashSet<_> = pane_ids.iter().copied().collect();
        if self
            .constraint_overrides
            .iter()
            .any(|(id, _)| !members.contains(id))
            || self.collapsed_panes.iter().any(|id| !members.contains(id))
        {
            return Err(MuxRecoveryImageError::InvalidTabRuntimeState(
                "runtime pane outside tab",
            ));
        }
        for (_, constraints) in &self.constraint_overrides {
            for (min, max, preferred) in [
                (
                    constraints.min_width,
                    constraints.max_width,
                    constraints.preferred_width,
                ),
                (
                    constraints.min_height,
                    constraints.max_height,
                    constraints.preferred_height,
                ),
            ] {
                if min == 0
                    || max.is_some_and(|max| max < min)
                    || preferred
                        .is_some_and(|value| value < min || max.is_some_and(|max| value > max))
                {
                    return Err(MuxRecoveryImageError::InvalidTabRuntimeState(
                        "unnormalized pane constraints",
                    ));
                }
            }
        }
        Ok(())
    }
}

impl RecoveryTab {
    /// Returns all unique pane IDs contained in this tab (split tree leaves, hidden stack members, plus floating panes).
    #[must_use]
    pub fn all_pane_ids(&self) -> Vec<usize> {
        let mut panes = Vec::new();
        if let Some(ref root) = self.root_split {
            for (id, _) in root.leaves() {
                panes.push(id);
            }
        }
        for stack in &self.pane_stacks {
            for &id in &stack.pane_ids {
                if !panes.contains(&id) {
                    panes.push(id);
                }
            }
        }
        for fp in &self.floating_panes {
            panes.push(fp.pane_id);
        }
        panes
    }
}

/// Per-pane metadata and authenticated terminal checkpoint reference.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecoveryPane {
    pub pane_id: usize,
    pub pane_uuid: String,
    pub spawn_custody: RecoverySpawnCustody,
    pub domain_name: String,
    /// Terminal-model title from the validated checkpoint; no process-title fallback.
    pub title: String,
    /// Terminal-reported directory URL from the checkpoint, not sampled OS process CWD.
    pub cwd: Option<String>,
    pub size: TerminalSize,
    /// Terminal cursor as (column, row).
    pub cursor_position: (usize, usize),
    pub alt_screen_active: bool,
    pub checkpoint: PaneCheckpointBinding,
}

/// Nonsecret provenance, explicitly absent for legacy panes. Recorded values
/// locate private AEAD custody; they never substitute for its capability.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum RecoverySpawnCustody {
    Absent,
    Original(Box<RecoveryOriginalCustody>),
}

/// Boxed provenance keeps absent custody small without changing its wire shape.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecoveryOriginalCustody {
    pub broker_lineage: uuid::Uuid,
    pub guardian_incarnation: uuid::Uuid,
    pub original_mux_incarnation: uuid::Uuid,
    pub broker_build: [u8; 32],
    pub guardian_build: [u8; 32],
    pub original_mux_build: [u8; 32],
    pub pane_id: uuid::Uuid,
    pub spawn_effect_id: uuid::Uuid,
    pub current_mux_incarnation: uuid::Uuid,
    pub current_lease_generation: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub acknowledged_successor: Option<RecoverySuccessorCustody>,
}

/// Public identities selecting encrypted custody, never the capability secret.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecoveryCustodyOwner {
    pub guardian_incarnation: uuid::Uuid,
    pub connection_id: uuid::Uuid,
    pub mux_incarnation: uuid::Uuid,
    pub guardian_build: [u8; 32],
    pub mux_build: [u8; 32],
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecoverySuccessorCustody {
    pub broker_incarnation: uuid::Uuid,
    pub broker_lineage: uuid::Uuid,
    pub broker_build: [u8; 32],
    pub predecessor: RecoveryCustodyOwner,
    pub successor: RecoveryCustodyOwner,
    pub pane_id: uuid::Uuid,
    pub handoff_id: uuid::Uuid,
    pub ack_id: uuid::Uuid,
    pub lease_generation: u64,
    pub rebind_from_connection: uuid::Uuid,
}

#[cfg(feature = "frankenterm-deps")]
impl From<mux::guardian_checkpoint::GuardianSuccessorCustodyContextV1>
    for RecoverySuccessorCustody
{
    fn from(c: mux::guardian_checkpoint::GuardianSuccessorCustodyContextV1) -> Self {
        let owner =
            |o: mux::guardian_checkpoint::GuardianSuccessorCustodyOwnerV1| RecoveryCustodyOwner {
                guardian_incarnation: o.guardian_incarnation,
                connection_id: o.connection_id,
                mux_incarnation: o.mux_incarnation,
                guardian_build: o.guardian_build,
                mux_build: o.mux_build,
            };
        Self {
            broker_incarnation: c.broker_incarnation,
            broker_lineage: c.broker_lineage,
            broker_build: c.broker_build,
            predecessor: owner(c.predecessor),
            successor: owner(c.successor),
            pane_id: c.pane_id,
            handoff_id: c.handoff_id,
            ack_id: c.ack_id,
            lease_generation: c.lease_generation,
            rebind_from_connection: c.rebind_from_connection,
        }
    }
}

#[cfg(feature = "frankenterm-deps")]
impl From<RecoverySuccessorCustody>
    for mux::guardian_checkpoint::GuardianSuccessorCustodyContextV1
{
    fn from(c: RecoverySuccessorCustody) -> Self {
        let owner =
            |o: RecoveryCustodyOwner| mux::guardian_checkpoint::GuardianSuccessorCustodyOwnerV1 {
                guardian_incarnation: o.guardian_incarnation,
                connection_id: o.connection_id,
                mux_incarnation: o.mux_incarnation,
                guardian_build: o.guardian_build,
                mux_build: o.mux_build,
            };
        Self {
            broker_incarnation: c.broker_incarnation,
            broker_lineage: c.broker_lineage,
            broker_build: c.broker_build,
            predecessor: owner(c.predecessor),
            successor: owner(c.successor),
            pane_id: c.pane_id,
            handoff_id: c.handoff_id,
            ack_id: c.ack_id,
            lease_generation: c.lease_generation,
            rebind_from_connection: c.rebind_from_connection,
        }
    }
}

#[cfg(feature = "frankenterm-deps")]
impl From<Option<mux::guardian_checkpoint::GuardianSpawnCaptureProvenanceV1>>
    for RecoverySpawnCustody
{
    fn from(value: Option<mux::guardian_checkpoint::GuardianSpawnCaptureProvenanceV1>) -> Self {
        let Some(value) = value else {
            return Self::Absent;
        };
        let scope = value.original;
        Self::Original(Box::new(RecoveryOriginalCustody {
            broker_lineage: scope.broker_lineage,
            guardian_incarnation: scope.guardian_incarnation,
            original_mux_incarnation: scope.mux_incarnation,
            broker_build: scope.broker_build,
            guardian_build: scope.guardian_build,
            original_mux_build: scope.mux_build,
            pane_id: scope.pane_id,
            spawn_effect_id: scope.effect_id,
            current_mux_incarnation: value.current_mux_incarnation,
            current_lease_generation: value.current_lease_generation,
            acknowledged_successor: value.acknowledged_successor.map(Into::into),
        }))
    }
}

impl fmt::Debug for RecoveryPane {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RecoveryPane")
            .field("pane_id", &self.pane_id)
            .field("pane_uuid", &self.pane_uuid)
            .field("domain_name", &self.domain_name)
            .field("title", &self.title)
            .field("cwd", &self.cwd)
            .field("size", &self.size)
            .field("cursor", &self.cursor_position)
            .field("alt_screen", &self.alt_screen_active)
            .field(
                "checkpoint_obj_id",
                &self.checkpoint.checkpoint_ref.object_id,
            )
            .finish()
    }
}

/// Binds a decoupled terminal checkpoint object reference to the exact topology
/// incarnation, pane registration generation, and external parser capture boundary.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneCheckpointBinding {
    pub topology_incarnation_id: String,
    pub pane_uuid: String,
    pub registration_wire_identity: [u8; 16],
    pub parser_capture: ParserCaptureIdentity,
    pub checkpoint_ref: RecoveryObjectRef,
    pub authority: CheckpointAuthority,
}

/// Identifiers captured at the external parser barrier.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ParserCaptureIdentity {
    pub watermark_bytes: u64,
    /// Absent for a model-only capture that has no guardian journal receipt.
    pub segment_id: Option<u64>,
    /// A true parser sequence identity, when supplied by the capture authority.
    /// A byte watermark is never substituted for this value.
    pub parser_seqno: Option<u64>,
}

/// Reference to a stored, content-addressed, encrypted/RaptorQ-encoded checkpoint object.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecoveryObjectRef {
    pub object_id: String,
    pub byte_length: u64,
    pub payload_digest: [u8; 32],
    pub schema_version: u32,
}

/// Explicit authority declaration for a terminal checkpoint.
///
/// Distinguishes direct parser model snapshots from guardian protocol receipts,
/// preventing unauthenticated captures from masquerading as verified guardian checkpoints.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum CheckpointAuthority {
    /// Model-only capture executed directly at external parser ground.
    ModelOnly {
        captured_at_epoch_ms: u64,
        parser_seqno: Option<u64>,
    },
    /// Cryptographically signed or verified under guardian lease and catalog protocol.
    Guardian {
        guardian_generation: u64,
        publication: RecoveryGuardianPublication,
        catalog_generation: u64,
    },
}

/// Serialized identity of a publication receipt, not a standalone verifier.
/// Restore must authenticate it against live guardian custody/catalog state.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecoveryGuardianPublication {
    pub guardian_incarnation: uuid::Uuid,
    pub mux_incarnation: uuid::Uuid,
    pub effect_id: uuid::Uuid,
    pub checkpoint_identity: [u8; 32],
    pub output_boundary_identity: [u8; 32],
}

/// Thin terminal reference projection consumed directly by the restore planner (`session_restore.rs`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecoveryPaneTerminalRef {
    pub pane_id: u64,
    pub stable_pane_uuid: String,
    pub semantic_digest: [u8; 32],
    pub terminal_checkpoint_object_id: String,
    pub authority: CheckpointAuthority,
}

/// Generation metadata provided by the snapshot publisher or caller.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecoveryImageGenerationMeta {
    pub generation: u64,
    pub predecessor_digest: Option<[u8; 32]>,
    pub created_at_epoch_ms: u64,
    pub ft_version: String,
    pub session_id: String,
}

// =============================================================================
// Validation & Public Operations
// =============================================================================

impl MuxRecoveryImage {
    /// Total count of panes in this image.
    #[must_use]
    pub fn pane_count(&self) -> usize {
        self.panes.len()
    }

    /// Projects the pane checkpoint references into the shape required by `%576` (`session_restore.rs`).
    #[must_use]
    pub fn pane_terminal_refs(&self) -> Vec<RecoveryPaneTerminalRef> {
        self.panes
            .iter()
            .map(|p| RecoveryPaneTerminalRef {
                pane_id: p.pane_id as u64,
                stable_pane_uuid: p.pane_uuid.clone(),
                semantic_digest: p.checkpoint.checkpoint_ref.payload_digest,
                terminal_checkpoint_object_id: p.checkpoint.checkpoint_ref.object_id.clone(),
                authority: p.checkpoint.authority.clone(),
            })
            .collect()
    }

    /// Looks up a pane by its numeric incarnation ID.
    #[must_use]
    pub fn find_pane(&self, pane_id: usize) -> Option<&RecoveryPane> {
        self.panes.iter().find(|p| p.pane_id == pane_id)
    }

    /// Looks up a pane by its durable UUID.
    #[must_use]
    pub fn find_pane_by_uuid(&self, uuid: &str) -> Option<&RecoveryPane> {
        self.panes.iter().find(|p| p.pane_uuid == uuid)
    }

    /// Looks up a window by its numeric incarnation ID.
    #[must_use]
    pub fn find_window(&self, window_id: usize) -> Option<&RecoveryWindow> {
        self.topology
            .windows
            .iter()
            .find(|w| w.window_id == window_id)
    }

    /// Looks up a window by its stable window ID.
    #[must_use]
    pub fn find_window_by_stable_id(&self, stable_id: &str) -> Option<&RecoveryWindow> {
        self.topology
            .windows
            .iter()
            .find(|w| w.stable_window_id == stable_id)
    }

    /// Looks up a tab by its numeric incarnation ID across all windows.
    #[must_use]
    pub fn find_tab(&self, tab_id: usize) -> Option<&RecoveryTab> {
        self.topology
            .windows
            .iter()
            .flat_map(|w| w.tabs.iter())
            .find(|t| t.tab_id == tab_id)
    }

    /// Looks up a tab by its stable tab ID across all windows.
    #[must_use]
    pub fn find_tab_by_stable_id(&self, stable_id: &str) -> Option<&RecoveryTab> {
        self.topology
            .windows
            .iter()
            .flat_map(|w| w.tabs.iter())
            .find(|t| t.stable_tab_id == stable_id)
    }

    /// Performs pre-serialization bounds checks (counts, depths, string lengths).
    ///
    /// Used before streaming into hasher or JSON writer to avoid unbounded processing
    /// of deeply nested or oversized in-memory structures.
    pub fn validate_bounds(&self) -> Result<(), MuxRecoveryImageError> {
        // 1. Magic and schema version
        if self.header.magic != MUX_RECOVERY_IMAGE_MAGIC {
            return Err(MuxRecoveryImageError::InvalidMagic {
                expected: MUX_RECOVERY_IMAGE_MAGIC,
                found: self.header.magic,
            });
        }
        if !matches!(
            self.header.schema_version,
            3 | 4 | 5 | 6 | MUX_RECOVERY_IMAGE_SCHEMA_VERSION
        ) {
            return Err(MuxRecoveryImageError::UnsupportedSchemaVersion(
                self.header.schema_version,
            ));
        }

        // 2. Header string limits
        validate_string_len(
            "header.mux_incarnation_id",
            &self.header.mux_incarnation_id,
            MAX_ID_STRING_BYTES,
        )?;
        validate_string_len(
            "header.ft_version",
            &self.header.ft_version,
            MAX_ID_STRING_BYTES,
        )?;
        validate_string_len(
            "header.session_id",
            &self.header.session_id,
            MAX_ID_STRING_BYTES,
        )?;

        if let Some(ref cw) = self.topology.client_workspace {
            validate_string_len(
                "topology.client_workspace.client_id",
                &cw.client_id,
                MAX_ID_STRING_BYTES,
            )?;
            validate_string_len(
                "topology.client_workspace.active_workspace",
                &cw.active_workspace,
                MAX_STRING_BYTES,
            )?;
        }

        match (&self.topology.workspace_state, self.header.schema_version) {
            (None, 5..) => {
                return Err(MuxRecoveryImageError::InvalidWorkspaceState(
                    "schema 5 requires captured workspace state",
                ));
            }
            (Some(_), ..=4) => {
                return Err(MuxRecoveryImageError::InvalidWorkspaceState(
                    "historical schema cannot claim schema 5 workspace authority",
                ));
            }
            _ => {}
        }
        if let Some(state) = &self.topology.workspace_state {
            validate_string_len(
                "workspace.default",
                &state.default_workspace,
                MAX_STRING_BYTES,
            )?;
            if state.workspaces.len() > MAX_RECOVERY_WINDOWS {
                return Err(MuxRecoveryImageError::ResourceLimit {
                    resource: "workspaces",
                    count: state.workspaces.len(),
                    limit: MAX_RECOVERY_WINDOWS,
                });
            }
            let mut members = 0usize;
            for workspace in &state.workspaces {
                validate_string_len("workspace.name", &workspace.name, MAX_STRING_BYTES)?;
                members = members.checked_add(workspace.window_ids.len()).ok_or(
                    MuxRecoveryImageError::InvalidWorkspaceState("window membership overflow"),
                )?;
                if members > MAX_RECOVERY_WINDOWS {
                    return Err(MuxRecoveryImageError::ResourceLimit {
                        resource: "workspace_window_members",
                        count: members,
                        limit: MAX_RECOVERY_WINDOWS,
                    });
                }
            }
        }

        // 3. Resource count limits
        if self.topology.domains.len() > MAX_RECOVERY_DOMAINS {
            return Err(MuxRecoveryImageError::ResourceLimit {
                resource: "domains",
                count: self.topology.domains.len(),
                limit: MAX_RECOVERY_DOMAINS,
            });
        }
        if self.topology.windows.len() > MAX_RECOVERY_WINDOWS {
            return Err(MuxRecoveryImageError::ResourceLimit {
                resource: "windows",
                count: self.topology.windows.len(),
                limit: MAX_RECOVERY_WINDOWS,
            });
        }
        if self.panes.len() > MAX_RECOVERY_PANES {
            return Err(MuxRecoveryImageError::ResourceLimit {
                resource: "panes",
                count: self.panes.len(),
                limit: MAX_RECOVERY_PANES,
            });
        }

        match (&self.topology.domain_state, self.header.schema_version) {
            (None, 7..) | (Some(_), ..=6) => {
                return Err(MuxRecoveryImageError::InvalidCapturedTopology(
                    "domain authority does not match image schema",
                ));
            }
            _ => {}
        }
        let mut policy_bytes = 0usize;
        for domain in &self.topology.domains {
            validate_string_len("domain.domain_name", &domain.domain_name, MAX_STRING_BYTES)?;
            match (&domain.recovery_policy, self.header.schema_version) {
                (None, 7..) | (Some(_), ..=6) => {
                    return Err(MuxRecoveryImageError::InvalidCapturedTopology(
                        "domain policy does not match image schema",
                    ));
                }
                _ => {}
            }
            if let Some(policy) = &domain.recovery_policy {
                if domain.incarnation_domain_id == usize::MAX || domain.domain_name.contains('\0') {
                    return Err(MuxRecoveryImageError::InvalidCapturedTopology(
                        "invalid current domain identity",
                    ));
                }
                // Both supported native constructors are always attached.
                // Detached remote/plugin domains require their own policy and
                // cannot be reconstructed as a local attached domain.
                if !domain.is_attached {
                    return Err(MuxRecoveryImageError::InvalidCapturedTopology(
                        "supported domain policy requires attached state",
                    ));
                }
                policy_bytes = policy_bytes
                    .checked_add(policy.validate()?)
                    .and_then(|bytes| bytes.checked_add(domain.domain_name.len()))
                    .ok_or(MuxRecoveryImageError::InvalidCapturedTopology(
                        "domain policy budget overflow",
                    ))?;
                if policy_bytes > MAX_RECOVERY_IMAGE_BYTES {
                    return Err(MuxRecoveryImageError::InvalidCapturedTopology(
                        "aggregate domain policy budget exceeded",
                    ));
                }
            }
        }

        for pane in &self.panes {
            validate_string_len("pane.pane_uuid", &pane.pane_uuid, MAX_ID_STRING_BYTES)?;
            validate_string_len("pane.title", &pane.title, MAX_STRING_BYTES)?;
            if let Some(ref cwd) = pane.cwd {
                validate_string_len("pane.cwd", cwd, MAX_STRING_BYTES)?;
            }
            validate_string_len(
                "pane.checkpoint.checkpoint_ref.object_id",
                &pane.checkpoint.checkpoint_ref.object_id,
                MAX_ID_STRING_BYTES,
            )?;
        }

        for window in &self.topology.windows {
            validate_string_len(
                "window.stable_window_id",
                &window.stable_window_id,
                MAX_ID_STRING_BYTES,
            )?;
            validate_string_len("window.workspace", &window.workspace, MAX_STRING_BYTES)?;
            validate_string_len("window.title", &window.title, MAX_STRING_BYTES)?;

            if window.tab_stacks.len() > MAX_RECOVERY_TABS_PER_WINDOW {
                return Err(MuxRecoveryImageError::ResourceLimit {
                    resource: "window_tab_stacks",
                    count: window.tab_stacks.len(),
                    limit: MAX_RECOVERY_TABS_PER_WINDOW,
                });
            }
            let stack_members = window.tab_stacks.iter().try_fold(0usize, |count, stack| {
                count.checked_add(stack.tab_ids.len())
            });
            if stack_members.is_none_or(|count| count > MAX_RECOVERY_TABS_PER_WINDOW) {
                return Err(MuxRecoveryImageError::ResourceLimit {
                    resource: "window_tab_stack_members",
                    count: stack_members.unwrap_or(usize::MAX),
                    limit: MAX_RECOVERY_TABS_PER_WINDOW,
                });
            }

            if window.tabs.len() > MAX_RECOVERY_TABS_PER_WINDOW {
                return Err(MuxRecoveryImageError::ResourceLimit {
                    resource: "tabs_per_window",
                    count: window.tabs.len(),
                    limit: MAX_RECOVERY_TABS_PER_WINDOW,
                });
            }

            for tab in &window.tabs {
                match (&tab.runtime_state, self.header.schema_version) {
                    (None, 6..) => {
                        return Err(MuxRecoveryImageError::InvalidTabRuntimeState(
                            "schema 6 requires tab runtime state",
                        ));
                    }
                    (Some(_), ..=5) => {
                        return Err(MuxRecoveryImageError::InvalidTabRuntimeState(
                            "historical schema cannot claim schema 6 tab authority",
                        ));
                    }
                    _ => {}
                }
                if let Some(state) = &tab.runtime_state {
                    state.validate_bounds()?;
                }
                validate_string_len("tab.stable_tab_id", &tab.stable_tab_id, MAX_ID_STRING_BYTES)?;
                validate_string_len("tab.title", &tab.title, MAX_STRING_BYTES)?;
                if let Some(ref cwd) = tab.working_dir {
                    validate_string_len("tab.working_dir", cwd, MAX_STRING_BYTES)?;
                }
                if tab.floating_panes.len() > MAX_FLOATING_PANES_PER_TAB {
                    return Err(MuxRecoveryImageError::ResourceLimit {
                        resource: "floating_panes_per_tab",
                        count: tab.floating_panes.len(),
                        limit: MAX_FLOATING_PANES_PER_TAB,
                    });
                }
                for fp in &tab.floating_panes {
                    validate_string_len(
                        "tab.floating_pane.pane_uuid",
                        &fp.pane_uuid,
                        MAX_ID_STRING_BYTES,
                    )?;
                }
                for stack in &tab.pane_stacks {
                    if stack.pane_ids.len() > MAX_RECOVERY_PANES {
                        return Err(MuxRecoveryImageError::ResourceLimit {
                            resource: "pane_stack_members",
                            count: stack.pane_ids.len(),
                            limit: MAX_RECOVERY_PANES,
                        });
                    }
                }
                if let Some(ref root_split) = tab.root_split {
                    check_split_tree_bounds(root_split, 0)?;
                }
            }
        }

        Ok(())
    }

    /// Performs complete structural, geometric, cryptographic, and referential validation.
    ///
    /// Invariants enforced:
    /// - Strict bounds on counts, split depth (<= 32), and string lengths.
    /// - Non-empty identifiers and positive generations/timestamps.
    /// - Duplicate IDs (windows, tabs, panes, domains) fail closed.
    /// - All domains referenced by panes must exist in the domain catalog.
    /// - Checkpoint bindings must match image incarnation and pane UUID, with non-zero
    ///   registration generation and valid authority.
    /// - Exact 1-to-1 global placement bijection: each pane in `self.panes` must be placed
    ///   in exactly one split leaf, floating pane, or hidden pane stack member across all
    ///   windows and tabs. Duplicate placements and orphan catalog panes fail closed.
    /// - Placed pane UUIDs in leaves and floating panes must match the catalog UUID.
    /// - Image digest matches canonical computed SHA-256 digest.
    pub fn validate(&self) -> Result<(), MuxRecoveryImageError> {
        // 1. Initial bounds validation
        self.validate_bounds()?;

        // 2. Header semantic validation
        if self.header.generation == 0 {
            return Err(MuxRecoveryImageError::InvalidHeader(
                "generation must be greater than zero",
            ));
        }
        if self.header.created_at_epoch_ms == 0 {
            return Err(MuxRecoveryImageError::InvalidHeader(
                "created_at_epoch_ms must be greater than zero",
            ));
        }
        if self.header.mux_incarnation_id.is_empty() {
            return Err(MuxRecoveryImageError::InvalidHeader(
                "mux_incarnation_id must not be empty",
            ));
        }
        if self.header.session_id.is_empty() {
            return Err(MuxRecoveryImageError::InvalidHeader(
                "session_id must not be empty",
            ));
        }
        if let Some(pred) = self.header.predecessor_digest {
            if pred == [0u8; 32] {
                return Err(MuxRecoveryImageError::InvalidHeader(
                    "predecessor_digest if present must not be all zeros",
                ));
            }
        }

        if let Some(ref cw) = self.topology.client_workspace {
            if cw.client_id.is_empty() {
                return Err(MuxRecoveryImageError::StringTooLong {
                    field: "topology.client_workspace.client_id (empty)",
                    len: 0,
                    limit: MAX_ID_STRING_BYTES,
                });
            }
        }

        // 3. Domains uniqueness & catalog
        let mut known_domain_names = HashSet::new();
        let mut known_domain_ids = HashSet::new();
        for domain in &self.topology.domains {
            if domain.domain_name.is_empty() {
                return Err(MuxRecoveryImageError::StringTooLong {
                    field: "domain.domain_name (empty)",
                    len: 0,
                    limit: MAX_STRING_BYTES,
                });
            }
            if !known_domain_names.insert(&domain.domain_name) {
                return Err(MuxRecoveryImageError::DuplicateDomainName(
                    domain.domain_name.clone(),
                ));
            }
            if !known_domain_ids.insert(domain.incarnation_domain_id) {
                return Err(MuxRecoveryImageError::DuplicateDomainId(
                    domain.incarnation_domain_id,
                ));
            }
        }

        // 4. Panes catalog uniqueness, domain reference validity, and checkpoint bindings
        if let Some(state) = &self.topology.domain_state {
            if state
                .default_domain_id
                .is_some_and(|id| !known_domain_ids.contains(&id))
                || !self
                    .topology
                    .domains
                    .windows(2)
                    .all(|pair| pair[0].incarnation_domain_id < pair[1].incarnation_domain_id)
            {
                return Err(MuxRecoveryImageError::InvalidCapturedTopology(
                    "invalid domain census/default authority",
                ));
            }
        }
        let mut catalog_pane_ids = HashSet::new();
        let mut catalog_pane_uuids = HashSet::new();
        let mut catalog_pane_uuids_by_id = HashMap::new();

        for pane in &self.panes {
            if pane.pane_uuid.is_empty() {
                return Err(MuxRecoveryImageError::StringTooLong {
                    field: "pane.pane_uuid (empty)",
                    len: 0,
                    limit: MAX_ID_STRING_BYTES,
                });
            }
            if !known_domain_names.contains(&pane.domain_name) {
                return Err(MuxRecoveryImageError::MissingDomain(
                    pane.domain_name.clone(),
                ));
            }
            if !catalog_pane_ids.insert(pane.pane_id) {
                return Err(MuxRecoveryImageError::DuplicatePaneId(pane.pane_id));
            }
            if !catalog_pane_uuids.insert(&pane.pane_uuid) {
                return Err(MuxRecoveryImageError::DuplicatePaneUuid(
                    pane.pane_uuid.clone(),
                ));
            }
            catalog_pane_uuids_by_id.insert(pane.pane_id, pane.pane_uuid.as_str());

            // Checkpoint binding identity match
            if pane.checkpoint.topology_incarnation_id != self.header.mux_incarnation_id {
                return Err(MuxRecoveryImageError::IncarnationMismatch {
                    pane_id: pane.pane_id,
                    expected: self.header.mux_incarnation_id.clone(),
                    found: pane.checkpoint.topology_incarnation_id.clone(),
                });
            }
            if pane.checkpoint.pane_uuid != pane.pane_uuid {
                return Err(MuxRecoveryImageError::PaneUuidMismatch {
                    pane_id: pane.pane_id,
                    expected: pane.pane_uuid.clone(),
                    found: pane.checkpoint.pane_uuid.clone(),
                });
            }

            // Checkpoint binding field validation
            if pane.checkpoint.registration_wire_identity == [0; 16] {
                return Err(MuxRecoveryImageError::ZeroRegistrationWireIdentity(
                    pane.pane_id,
                ));
            }

            let chk_ref = &pane.checkpoint.checkpoint_ref;
            if chk_ref.object_id.is_empty() {
                return Err(MuxRecoveryImageError::InvalidCheckpointRef {
                    pane_id: pane.pane_id,
                    reason: "object_id must not be empty",
                });
            }
            if chk_ref.byte_length == 0 {
                return Err(MuxRecoveryImageError::InvalidCheckpointRef {
                    pane_id: pane.pane_id,
                    reason: "byte_length must be greater than zero",
                });
            }
            if chk_ref.byte_length > MAX_RECOVERY_IMAGE_BYTES as u64 {
                return Err(MuxRecoveryImageError::InvalidCheckpointRef {
                    pane_id: pane.pane_id,
                    reason: "byte_length exceeds maximum allowed object size",
                });
            }
            if chk_ref.payload_digest == [0u8; 32] {
                return Err(MuxRecoveryImageError::InvalidCheckpointRef {
                    pane_id: pane.pane_id,
                    reason: "payload_digest must not be all zeros",
                });
            }
            if chk_ref.schema_version == 0 {
                return Err(MuxRecoveryImageError::InvalidCheckpointRef {
                    pane_id: pane.pane_id,
                    reason: "schema_version must be non-zero",
                });
            }

            // Authority validation
            if let RecoverySpawnCustody::Original(custody) = &pane.spawn_custody {
                let RecoveryOriginalCustody {
                    broker_lineage,
                    guardian_incarnation,
                    original_mux_incarnation,
                    broker_build,
                    guardian_build,
                    original_mux_build,
                    pane_id,
                    spawn_effect_id,
                    current_mux_incarnation,
                    current_lease_generation,
                    acknowledged_successor,
                } = custody.as_ref();
                if [
                    broker_lineage,
                    guardian_incarnation,
                    original_mux_incarnation,
                    pane_id,
                    spawn_effect_id,
                    current_mux_incarnation,
                ]
                .iter()
                .any(|id| id.is_nil())
                    || [broker_build, guardian_build, original_mux_build]
                        .iter()
                        .any(|build| **build == [0; 32])
                    || pane_id.to_string() != pane.pane_uuid
                    // Captured topology encodes the incarnation as compact hex,
                    // whereas UUID Display inserts hyphens. Compare using the
                    // topology encoding so a valid live owner is not rejected.
                    || hex::encode(current_mux_incarnation.as_bytes())
                        != self.header.mux_incarnation_id
                    || *current_lease_generation == 0
                    || (*current_lease_generation == 1
                        && current_mux_incarnation != original_mux_incarnation)
                    || !matches!(&pane.checkpoint.authority,
                        CheckpointAuthority::Guardian { guardian_generation, publication, .. }
                        if guardian_generation == current_lease_generation
                            && publication.guardian_incarnation == *guardian_incarnation
                            && publication.mux_incarnation == *current_mux_incarnation)
                {
                    return Err(MuxRecoveryImageError::InvalidAuthority {
                        pane_id: pane.pane_id,
                        reason: "original Spawn provenance does not match captured pane and current owner",
                    });
                }
                if let Some(c) = acknowledged_successor {
                    if self.header.schema_version < 4
                        || c.broker_incarnation.is_nil()
                        || c.handoff_id.is_nil()
                        || c.ack_id.is_nil()
                        || c.broker_lineage != *broker_lineage
                        || c.broker_build != *broker_build
                        || c.pane_id != *pane_id
                        || c.lease_generation != *current_lease_generation
                        || c.lease_generation <= 1
                        || c.successor.mux_incarnation != *current_mux_incarnation
                        || c.successor.guardian_incarnation != *guardian_incarnation
                        || c.successor.guardian_build != *guardian_build
                        || c.successor.connection_id.is_nil()
                        || c.successor.mux_build == [0; 32]
                        || c.predecessor.guardian_incarnation != *guardian_incarnation
                        || c.predecessor.guardian_build != *guardian_build
                        || c.predecessor.connection_id.is_nil()
                        || c.predecessor.mux_incarnation.is_nil()
                        || c.predecessor.mux_build == [0; 32]
                        || c.predecessor.mux_incarnation == c.successor.mux_incarnation
                    {
                        return Err(MuxRecoveryImageError::InvalidAuthority {
                            pane_id: pane.pane_id,
                            reason: "successor custody differs from captured current owner",
                        });
                    }
                } else if self.header.schema_version >= 4 && *current_lease_generation > 1 {
                    return Err(MuxRecoveryImageError::InvalidAuthority {
                        pane_id: pane.pane_id,
                        reason: "successor capture requires acknowledged custody selector",
                    });
                }
            }
            match &pane.checkpoint.authority {
                CheckpointAuthority::ModelOnly {
                    captured_at_epoch_ms,
                    parser_seqno,
                } => {
                    if *captured_at_epoch_ms == 0 {
                        return Err(MuxRecoveryImageError::InvalidAuthority {
                            pane_id: pane.pane_id,
                            reason: "captured_at_epoch_ms must be greater than zero",
                        });
                    }
                    if pane.checkpoint.parser_capture.parser_seqno != *parser_seqno {
                        return Err(MuxRecoveryImageError::InvalidAuthority {
                            pane_id: pane.pane_id,
                            reason: "parser_capture.parser_seqno does not match model authority parser_seqno",
                        });
                    }
                }
                CheckpointAuthority::Guardian {
                    guardian_generation,
                    publication,
                    catalog_generation,
                } => {
                    if *guardian_generation == 0 {
                        return Err(MuxRecoveryImageError::InvalidAuthority {
                            pane_id: pane.pane_id,
                            reason: "guardian_generation must be greater than zero",
                        });
                    }
                    if *catalog_generation == 0 {
                        return Err(MuxRecoveryImageError::InvalidAuthority {
                            pane_id: pane.pane_id,
                            reason: "catalog_generation must be greater than zero",
                        });
                    }
                    if publication.guardian_incarnation.is_nil()
                        || publication.mux_incarnation.is_nil()
                        || publication.effect_id.is_nil()
                        || publication.checkpoint_identity == [0; 32]
                        || publication.output_boundary_identity == [0; 32]
                    {
                        return Err(MuxRecoveryImageError::InvalidAuthority {
                            pane_id: pane.pane_id,
                            reason: "published guardian receipt identities must be nonzero",
                        });
                    }
                }
            }
        }

        // 5. Windows, tabs, split trees, floating panes, and stacks
        let mut seen_window_ids = HashSet::new();
        let mut seen_stable_window_ids = HashSet::new();
        let mut seen_tab_ids = HashSet::new();
        let mut seen_stable_tab_ids = HashSet::new();
        let mut global_placed_pane_ids = HashSet::new();

        for window in &self.topology.windows {
            // WindowOrderRevision reserves MAX as an exhaustion sentinel;
            // MAX - 1 remains a valid captured state even though it cannot
            // admit another ordered-window mutation.
            if window.order_revision == u64::MAX {
                return Err(MuxRecoveryImageError::InvalidCapturedTopology(
                    "window order revision is the reserved exhaustion sentinel",
                ));
            }
            if !seen_window_ids.insert(window.window_id) {
                return Err(MuxRecoveryImageError::DuplicateWindowId(window.window_id));
            }
            validate_durable_identity("window.stable_window_id", &window.stable_window_id)?;
            if !seen_stable_window_ids.insert(&window.stable_window_id) {
                return Err(MuxRecoveryImageError::DuplicateStableWindowId(
                    window.stable_window_id.clone(),
                ));
            }

            if !window.tabs.is_empty() && window.active_tab_index >= window.tabs.len() {
                return Err(MuxRecoveryImageError::InvalidActiveTabIndex {
                    window_id: window.window_id,
                    index: window.active_tab_index,
                    count: window.tabs.len(),
                });
            }

            let window_tab_ids: HashSet<_> = window.tabs.iter().map(|tab| tab.tab_id).collect();
            if window
                .last_active_tab_id
                .is_some_and(|id| !window_tab_ids.contains(&id))
            {
                return Err(MuxRecoveryImageError::InvalidCapturedTopology(
                    "last active tab is absent from its window",
                ));
            }
            let mut previous_stack_id = None;
            let mut stacked_tabs = HashSet::new();
            for stack in &window.tab_stacks {
                if previous_stack_id.is_some_and(|id| id >= stack.stack_id)
                    || stack.tab_ids.is_empty()
                    || !stack.tab_ids.contains(&stack.visible_tab_id)
                    || stack
                        .tab_ids
                        .iter()
                        .any(|id| !window_tab_ids.contains(id) || !stacked_tabs.insert(*id))
                {
                    return Err(MuxRecoveryImageError::InvalidCapturedTopology(
                        "invalid window tab stack membership, visibility, or canonical order",
                    ));
                }
                previous_stack_id = Some(stack.stack_id);
            }

            for tab in &window.tabs {
                validate_durable_identity("tab.stable_tab_id", &tab.stable_tab_id)?;
                if !seen_tab_ids.insert(tab.tab_id) {
                    return Err(MuxRecoveryImageError::DuplicateTabId(tab.tab_id));
                }
                if !seen_stable_tab_ids.insert(&tab.stable_tab_id) {
                    return Err(MuxRecoveryImageError::DuplicateStableTabId(
                        tab.stable_tab_id.clone(),
                    ));
                }

                // Collect and validate all panes in this tab (split tree + floating + hidden stack members)
                let mut tab_panes = HashSet::new();

                if let Some(ref root_split) = tab.root_split {
                    validate_split_node(
                        root_split,
                        0,
                        &catalog_pane_uuids_by_id,
                        &mut tab_panes,
                        &mut global_placed_pane_ids,
                    )?;
                }

                let mut floating_pane_ids = HashSet::new();
                for fp in &tab.floating_panes {
                    let catalog_uuid = match catalog_pane_uuids_by_id.get(&fp.pane_id) {
                        Some(u) => *u,
                        None => return Err(MuxRecoveryImageError::MissingPane(fp.pane_id)),
                    };
                    if catalog_uuid != fp.pane_uuid.as_str() {
                        return Err(MuxRecoveryImageError::PaneUuidMismatchCatalog {
                            pane_id: fp.pane_id,
                            catalog_uuid: catalog_uuid.to_string(),
                            placed_uuid: fp.pane_uuid.clone(),
                        });
                    }
                    if !tab_panes.insert(fp.pane_id) {
                        return Err(MuxRecoveryImageError::DuplicatePanePlacement(fp.pane_id));
                    }
                    if !global_placed_pane_ids.insert(fp.pane_id) {
                        return Err(MuxRecoveryImageError::DuplicatePanePlacement(fp.pane_id));
                    }
                    floating_pane_ids.insert(fp.pane_id);
                }

                // Validate pane stacks: slot_index must map 1-to-1 to split tree leaves in in-order traversal
                let tree_leaves = tab
                    .root_split
                    .as_ref()
                    .map(|rs| rs.leaves())
                    .unwrap_or_default();
                let mut seen_stack_slots = HashSet::new();

                for stack in &tab.pane_stacks {
                    if stack.pane_ids.is_empty() {
                        return Err(MuxRecoveryImageError::MalformedSplit(
                            "pane stack must not be empty",
                        ));
                    }
                    if stack.active_index >= stack.pane_ids.len() {
                        return Err(MuxRecoveryImageError::MalformedSplit(
                            "pane stack active index out of bounds",
                        ));
                    }
                    if !seen_stack_slots.insert(stack.slot_index) {
                        return Err(MuxRecoveryImageError::DuplicateStackSlot {
                            tab_id: tab.tab_id,
                            slot_index: stack.slot_index,
                        });
                    }

                    // Slot index must correspond to an actual tree leaf in the tab's split tree
                    let (expected_leaf_pane_id, _) = tree_leaves
                        .get(stack.slot_index)
                        .copied()
                        .ok_or(MuxRecoveryImageError::StackSlotOutOfBounds {
                            tab_id: tab.tab_id,
                            slot_index: stack.slot_index,
                            leaf_count: tree_leaves.len(),
                        })?;

                    let active_id = stack.pane_ids[stack.active_index];
                    if active_id != expected_leaf_pane_id {
                        return Err(MuxRecoveryImageError::StackActiveMemberMismatch {
                            tab_id: tab.tab_id,
                            slot_index: stack.slot_index,
                            expected_pane_id: expected_leaf_pane_id,
                            found_pane_id: active_id,
                        });
                    }

                    for (idx, &pane_id) in stack.pane_ids.iter().enumerate() {
                        if idx == stack.active_index {
                            continue; // Active pane is already counted in tab_panes and global_placed_pane_ids
                        }
                        // Hidden stack pane:
                        if !catalog_pane_ids.contains(&pane_id) {
                            return Err(MuxRecoveryImageError::MissingPane(pane_id));
                        }
                        // Hidden pane must not be in tab_panes (not tiled leaf or floating pane)
                        if tab_panes.contains(&pane_id) {
                            return Err(MuxRecoveryImageError::DuplicatePanePlacement(pane_id));
                        }
                        // Insert into global_placed_pane_ids to ensure uniqueness
                        if !global_placed_pane_ids.insert(pane_id) {
                            return Err(MuxRecoveryImageError::DuplicatePanePlacement(pane_id));
                        }
                    }
                }

                // Active pane must exist in tab_panes or pane_stacks
                let all_tab_pane_ids = tab.all_pane_ids();
                if let Some(state) = &tab.runtime_state {
                    state.validate_membership(&all_tab_pane_ids)?;
                }
                if !all_tab_pane_ids.is_empty() && !all_tab_pane_ids.contains(&tab.active_pane_id) {
                    return Err(MuxRecoveryImageError::InvalidActivePaneId {
                        tab_id: tab.tab_id,
                        pane_id: tab.active_pane_id,
                    });
                }

                // Underlying tiled active pane if present must exist in split tree carrier leaves
                // (matching raw_tree_active_pane which only indexes into tree leaves, never hidden stack members)
                if let Some(tiled_id) = tab.underlying_tiled_active_pane_id {
                    let is_in_tree = tree_leaves.iter().any(|(id, _)| *id == tiled_id);
                    if !is_in_tree {
                        return Err(MuxRecoveryImageError::InvalidUnderlyingTiledActivePaneId {
                            tab_id: tab.tab_id,
                            pane_id: tiled_id,
                        });
                    }
                }

                // Floating focus if present must exist in tab.floating_panes
                if let Some(ff_id) = tab.floating_focus {
                    if !floating_pane_ids.contains(&ff_id) {
                        return Err(MuxRecoveryImageError::InvalidFloatingFocusId {
                            tab_id: tab.tab_id,
                            pane_id: ff_id,
                        });
                    }
                }

                // Zoomed pane if present must exist in tab_panes
                if let Some(z_id) = tab.zoomed_pane_id {
                    if !all_tab_pane_ids.contains(&z_id) {
                        return Err(MuxRecoveryImageError::InvalidZoomedPaneId {
                            tab_id: tab.tab_id,
                            pane_id: z_id,
                        });
                    }
                }

                // Enforce coherence between effective active pane, floating focus, zoom, and tiled active pane
                if let Some(z_id) = tab.zoomed_pane_id {
                    if tab.active_pane_id != z_id {
                        return Err(MuxRecoveryImageError::ContradictoryActivePaneState {
                            tab_id: tab.tab_id,
                            reason: "tab active pane id does not match zoomed pane id",
                        });
                    }
                } else if let Some(ff_id) = tab.floating_focus {
                    if tab.active_pane_id != ff_id {
                        return Err(MuxRecoveryImageError::ContradictoryActivePaneState {
                            tab_id: tab.tab_id,
                            reason: "tab active pane id does not match floating focus pane id",
                        });
                    }
                } else {
                    // Floating focus is absent and no zoom:
                    // Active pane cannot be a floating pane
                    if floating_pane_ids.contains(&tab.active_pane_id) {
                        return Err(MuxRecoveryImageError::ContradictoryActivePaneState {
                            tab_id: tab.tab_id,
                            reason: "tab active pane id is a floating pane but floating focus is absent",
                        });
                    }
                    // If split tree leaves exist, active pane must be a split tree leaf (cannot be hidden stack member)
                    if !tree_leaves.is_empty()
                        && !tree_leaves.iter().any(|(id, _)| *id == tab.active_pane_id)
                    {
                        return Err(MuxRecoveryImageError::ContradictoryActivePaneState {
                            tab_id: tab.tab_id,
                            reason: "tab active pane id must be a split tree leaf when floating focus and zoom are absent",
                        });
                    }
                    // If split tree leaves exist, underlying_tiled_active_pane_id (if specified) must match active_pane_id
                    if !tree_leaves.is_empty() {
                        if let Some(tiled_id) = tab.underlying_tiled_active_pane_id {
                            if tiled_id != tab.active_pane_id {
                                return Err(MuxRecoveryImageError::ContradictoryActivePaneState {
                                    tab_id: tab.tab_id,
                                    reason: "tab underlying tiled active pane id does not match active pane id when floating focus is absent",
                                });
                            }
                        }
                    }
                }
            }
        }

        // 6. Enforce global placement bijection: no orphan catalog panes
        for pane in &self.panes {
            if !global_placed_pane_ids.contains(&pane.pane_id) {
                return Err(MuxRecoveryImageError::OrphanCatalogPane(pane.pane_id));
            }
        }

        // 7. Focused window validation
        if let Some(fw_id) = self.topology.focused_window_id {
            if !seen_window_ids.contains(&fw_id) {
                return Err(MuxRecoveryImageError::InvalidFocusedWindowId(fw_id));
            }
        }

        // 8. Content digest check
        self.validate_workspace_state()?;
        let computed_digest = self.compute_digest()?;
        if computed_digest != self.image_digest {
            return Err(MuxRecoveryImageError::DigestMismatch {
                computed: hex::encode(computed_digest),
                recorded: hex::encode(self.image_digest),
            });
        }

        Ok(())
    }

    /// Expose complete workspace metadata only after full image validation.
    /// Historical images remain verifiable but cannot invent the omitted state.
    pub fn require_live_workspace_state(
        &self,
    ) -> Result<&RecoveryWorkspaceState, MuxRecoveryImageError> {
        self.validate()?;
        self.topology
            .workspace_state
            .as_ref()
            .ok_or(MuxRecoveryImageError::InvalidWorkspaceState(
                "historical image lacks complete live workspace metadata",
            ))
    }

    /// Admit complete live topology only when the authenticated schema carries
    /// both workspace state and the tab policies that drive future behavior.
    pub fn require_live_topology_state(&self) -> Result<(), MuxRecoveryImageError> {
        self.require_live_workspace_state()?;
        if self.header.schema_version < 7 {
            return Err(MuxRecoveryImageError::InvalidTabRuntimeState(
                "historical image lacks complete live tab/domain runtime metadata",
            ));
        }
        Ok(())
    }

    fn validate_workspace_state(&self) -> Result<(), MuxRecoveryImageError> {
        let Some(state) = &self.topology.workspace_state else {
            return Ok(());
        };
        let windows: HashMap<_, _> = self
            .topology
            .windows
            .iter()
            .map(|window| (window.window_id, window.workspace.as_str()))
            .collect();
        let mut names = HashSet::new();
        let mut members = HashSet::new();
        for workspace in &state.workspaces {
            if !names.insert(&workspace.name) {
                return Err(MuxRecoveryImageError::InvalidWorkspaceState(
                    "duplicate workspace",
                ));
            }
            for id in &workspace.window_ids {
                if windows.get(id).copied() != Some(workspace.name.as_str()) || !members.insert(*id)
                {
                    return Err(MuxRecoveryImageError::InvalidWorkspaceState(
                        "window membership is missing, duplicated, or belongs to another workspace",
                    ));
                }
            }
            if workspace
                .active_window_id
                .is_some_and(|id| !workspace.window_ids.contains(&id))
            {
                return Err(MuxRecoveryImageError::InvalidWorkspaceState(
                    "active window is not a member of its workspace",
                ));
            }
        }
        if members.len() != windows.len() {
            return Err(MuxRecoveryImageError::InvalidWorkspaceState(
                "orphan workspace window",
            ));
        }
        Ok(())
    }

    /// Computes the canonical SHA-256 digest over image content (domain-separated).
    ///
    /// Streams JSON serialization directly into Sha256 using an `io::Write` adapter
    /// without creating intermediate unbounded heap Strings, enforcing a hard 16 MiB
    /// byte limit.
    pub fn compute_digest(&self) -> Result<[u8; 32], MuxRecoveryImageError> {
        self.validate_bounds()?;

        let mut hasher = Sha256::new();
        hasher.update(HASH_DOMAIN_IMAGE);

        let sink = Sha256Sink::new(&mut hasher);
        let mut writer = BoundedWriter::new(sink, MAX_RECOVERY_IMAGE_BYTES);

        writer
            .write_all(b"\x00HEADER:\x00")
            .map_err(|_| MuxRecoveryImageError::TooLarge {
                bytes: writer.written(),
                limit: MAX_RECOVERY_IMAGE_BYTES,
            })?;
        serde_json::to_writer(&mut writer, &self.header).map_err(|e| {
            if writer.exceeded() {
                MuxRecoveryImageError::TooLarge {
                    bytes: writer.written(),
                    limit: MAX_RECOVERY_IMAGE_BYTES,
                }
            } else {
                MuxRecoveryImageError::Serialization(e.to_string())
            }
        })?;

        writer
            .write_all(b"\x00TOPOLOGY:\x00")
            .map_err(|_| MuxRecoveryImageError::TooLarge {
                bytes: writer.written(),
                limit: MAX_RECOVERY_IMAGE_BYTES,
            })?;
        serde_json::to_writer(&mut writer, &self.topology).map_err(|e| {
            if writer.exceeded() {
                MuxRecoveryImageError::TooLarge {
                    bytes: writer.written(),
                    limit: MAX_RECOVERY_IMAGE_BYTES,
                }
            } else {
                MuxRecoveryImageError::Serialization(e.to_string())
            }
        })?;

        writer
            .write_all(b"\x00PANES:\x00")
            .map_err(|_| MuxRecoveryImageError::TooLarge {
                bytes: writer.written(),
                limit: MAX_RECOVERY_IMAGE_BYTES,
            })?;
        serde_json::to_writer(&mut writer, &self.panes).map_err(|e| {
            if writer.exceeded() {
                MuxRecoveryImageError::TooLarge {
                    bytes: writer.written(),
                    limit: MAX_RECOVERY_IMAGE_BYTES,
                }
            } else {
                MuxRecoveryImageError::Serialization(e.to_string())
            }
        })?;

        Ok(hasher.finalize().into())
    }

    /// Serializes image to canonical JSON bytes after full validation.
    ///
    /// Bounds serialized output to [`MAX_RECOVERY_IMAGE_BYTES`].
    pub fn to_canonical_json(&self) -> Result<Vec<u8>, MuxRecoveryImageError> {
        self.validate()?;
        let mut writer = BoundedWriter::new(Vec::with_capacity(1024), MAX_RECOVERY_IMAGE_BYTES);
        serde_json::to_writer(&mut writer, self).map_err(|e| {
            if writer.exceeded() {
                MuxRecoveryImageError::TooLarge {
                    bytes: writer.written(),
                    limit: MAX_RECOVERY_IMAGE_BYTES,
                }
            } else {
                MuxRecoveryImageError::Serialization(e.to_string())
            }
        })?;
        Ok(writer.into_inner())
    }

    /// Parses recovery image from raw JSON slice with strict pre-parse size limit
    /// and full structural validation.
    pub fn from_json_slice(slice: &[u8]) -> Result<Self, MuxRecoveryImageError> {
        if slice.len() > MAX_RECOVERY_IMAGE_BYTES {
            return Err(MuxRecoveryImageError::TooLarge {
                bytes: slice.len(),
                limit: MAX_RECOVERY_IMAGE_BYTES,
            });
        }

        let image: Self = serde_json::from_slice(slice)
            .map_err(|e| MuxRecoveryImageError::Deserialization(e.to_string()))?;

        image.validate()?;
        Ok(image)
    }
}

// =============================================================================
// Live Mux Production Converter (feature = "frankenterm-deps")
// =============================================================================

#[cfg(feature = "frankenterm-deps")]
#[derive(Clone, Copy, Debug)]
pub enum RecoveryParserCheckpoint<'a> {
    Model(&'a mux::ModelParserCheckpointAck),
    Guardian(&'a mux::guardian_checkpoint::PublishedGuardianCheckpoint),
}

#[cfg(feature = "frankenterm-deps")]
impl<'a> RecoveryParserCheckpoint<'a> {
    #[must_use]
    pub fn terminal_checkpoint(&self) -> &'a frankenterm_term::RecoveryTerminalCheckpointV3 {
        match self {
            Self::Model(ack) => &ack.terminal_checkpoint,
            Self::Guardian(published) => published.capture().terminal_checkpoint(),
        }
    }

    #[must_use]
    pub fn registration_wire_identity(&self) -> [u8; 16] {
        match self {
            Self::Model(ack) => ack.registration_wire_identity,
            Self::Guardian(published) => published.capture().registration_wire_identity(),
        }
    }

    #[must_use]
    pub fn durable_pane_id(&self) -> uuid::Uuid {
        match self {
            Self::Model(ack) => ack.durable_pane_id,
            Self::Guardian(published) => published.capture().durable_pane_id(),
        }
    }

    #[must_use]
    pub fn parser_stream_bytes(&self) -> u64 {
        match self {
            Self::Model(ack) => ack.parser_stream_bytes,
            Self::Guardian(published) => published.capture().parser_stream_bytes(),
        }
    }
}

#[cfg(feature = "frankenterm-deps")]
struct ImageParserCapture<'a> {
    registration_wire_identity: [u8; 16],
    durable_pane_id: uuid::Uuid,
    parser_stream_bytes: u64,
    semantic_generation: u64,
    terminal_checkpoint: &'a frankenterm_term::RecoveryTerminalCheckpointV3,
}

#[cfg(feature = "frankenterm-deps")]
impl MuxRecoveryImage {
    /// Converts a coherent live [`mux::MuxCapturedTopology`] and per-pane [`mux::ModelParserCheckpointAck`]s
    /// into a validated [`MuxRecoveryImage`]. The publisher authenticates its encrypted envelope.
    ///
    /// # Invariants Enforced:
    /// - Exact membership bijection: every pane in `captured.pane_bindings` must appear in
    ///   `checkpoint_acks` and `checkpoint_object_refs`, with zero extraneous entries.
    /// - Registration wire identity match: `binding.registration_wire_identity == ack.registration_wire_identity`.
    /// - Registration wire identity non-zero: rejects all-zero registration wire identities.
    /// - Durable UUID match: `binding.pane_uuid == ack.durable_pane_id.to_string()`, rejects nil UUIDs.
    /// - Preserves the actual parser stream watermark, including an empty stream.
    /// - Preserves `pane_stacks` (hidden stack panes) and verifies exact membership.
    /// - Binds live topology incarnation without fabricated or default identifiers.
    /// - Computes canonical SHA-256 digest and validates the resulting image before returning.
    pub fn from_mux_captured(
        meta: RecoveryImageGenerationMeta,
        captured: &mux::MuxCapturedTopology,
        checkpoint_acks: &HashMap<usize, &mux::ModelParserCheckpointAck>,
        checkpoint_object_refs: &HashMap<usize, RecoveryObjectRef>,
    ) -> Result<Self, MuxRecoveryImageError> {
        let parser_checkpoints = checkpoint_acks
            .iter()
            .map(|(pane, ack)| (*pane, RecoveryParserCheckpoint::Model(ack)))
            .collect();
        Self::from_mux_captured_checkpoints(
            meta,
            captured,
            &parser_checkpoints,
            checkpoint_object_refs,
        )
    }

    /// Convert exact model captures and durably published guardian captures.
    /// Original Spawn provenance remains separate from checkpoint authority.
    pub fn from_mux_captured_checkpoints(
        meta: RecoveryImageGenerationMeta,
        captured: &mux::MuxCapturedTopology,
        checkpoint_acks: &HashMap<usize, RecoveryParserCheckpoint<'_>>,
        checkpoint_object_refs: &HashMap<usize, RecoveryObjectRef>,
    ) -> Result<Self, MuxRecoveryImageError> {
        // 1. Metadata validation
        if meta.generation == 0 {
            return Err(MuxRecoveryImageError::InvalidHeader(
                "generation must be greater than zero",
            ));
        }
        if meta.created_at_epoch_ms == 0 {
            return Err(MuxRecoveryImageError::InvalidHeader(
                "created_at_epoch_ms must be greater than zero",
            ));
        }
        if meta.ft_version.is_empty() {
            return Err(MuxRecoveryImageError::InvalidHeader(
                "ft_version must not be empty",
            ));
        }
        if meta.session_id.is_empty() {
            return Err(MuxRecoveryImageError::InvalidHeader(
                "session_id must not be empty",
            ));
        }

        // 2. Incarnation identity
        let incarnation_bytes = captured.session_incarnation.as_bytes();
        if incarnation_bytes == [0u8; 16] {
            return Err(MuxRecoveryImageError::InvalidHeader(
                "session incarnation is all zeros",
            ));
        }
        let topology_incarnation_id = hex::encode(incarnation_bytes);

        // 3. Exact membership bijection
        let captured_pane_ids: HashSet<usize> =
            captured.pane_bindings.iter().map(|b| b.pane_id).collect();

        for &pane_id in checkpoint_acks.keys() {
            if !captured_pane_ids.contains(&pane_id) {
                return Err(MuxRecoveryImageError::ExtraneousCheckpointAck(pane_id));
            }
        }
        for &pane_id in checkpoint_object_refs.keys() {
            if !captured_pane_ids.contains(&pane_id) {
                return Err(MuxRecoveryImageError::ExtraneousCheckpointObjectRef(
                    pane_id,
                ));
            }
        }
        for &pane_id in &captured_pane_ids {
            if !checkpoint_acks.contains_key(&pane_id) {
                return Err(MuxRecoveryImageError::MissingCheckpointAck(pane_id));
            }
            if !checkpoint_object_refs.contains_key(&pane_id) {
                return Err(MuxRecoveryImageError::MissingCheckpointObjectRef(pane_id));
            }
        }

        // 4. Build pane UUID map and validate pane checkpoints
        let mut pane_uuid_map = HashMap::new();
        let mut panes = Vec::with_capacity(captured.pane_bindings.len());

        for binding in &captured.pane_bindings {
            if binding.registration_wire_identity == [0u8; 16] {
                return Err(MuxRecoveryImageError::ZeroRegistrationWireIdentity(
                    binding.pane_id,
                ));
            }
            let ack = checkpoint_acks.get(&binding.pane_id).unwrap();
            let (ack, authority) = match ack {
                RecoveryParserCheckpoint::Model(ack) => {
                    if binding.spawn_custody.is_some() {
                        return Err(MuxRecoveryImageError::InvalidAuthority {
                            pane_id: binding.pane_id,
                            reason: "guardian provenance requires a published guardian checkpoint",
                        });
                    }
                    (
                        ImageParserCapture {
                            registration_wire_identity: ack.registration_wire_identity,
                            durable_pane_id: ack.durable_pane_id,
                            parser_stream_bytes: ack.parser_stream_bytes,
                            semantic_generation: ack.semantic_generation,
                            terminal_checkpoint: &ack.terminal_checkpoint,
                        },
                        CheckpointAuthority::ModelOnly {
                            captured_at_epoch_ms: captured.captured_at_epoch_ms,
                            parser_seqno: None,
                        },
                    )
                }
                RecoveryParserCheckpoint::Guardian(published) => {
                    let ack = published.capture();
                    let receipt = published.receipt();
                    let provenance =
                        binding
                            .spawn_custody
                            .ok_or(MuxRecoveryImageError::InvalidAuthority {
                                pane_id: binding.pane_id,
                                reason: "guardian capture lacks original Spawn provenance",
                            })?;
                    if provenance.original.pane_id != receipt.pane_id()
                        || provenance.original.guardian_incarnation
                            != published.owner().guardian_incarnation()
                        || provenance.current_mux_incarnation != published.owner().mux_incarnation()
                        || provenance.current_lease_generation != receipt.generation()
                        || provenance.current_mux_incarnation.as_bytes()
                            != &captured.session_incarnation.as_bytes()
                    {
                        return Err(MuxRecoveryImageError::InvalidAuthority {
                            pane_id: binding.pane_id,
                            reason: "published capture differs from captured guardian lease",
                        });
                    }
                    let checkpoint = ack.terminal_checkpoint();
                    (
                        ImageParserCapture {
                            registration_wire_identity: ack.registration_wire_identity(),
                            durable_pane_id: ack.durable_pane_id(),
                            parser_stream_bytes: ack.parser_stream_bytes(),
                            semantic_generation: ack.semantic_generation(),
                            terminal_checkpoint: checkpoint,
                        },
                        CheckpointAuthority::Guardian {
                            guardian_generation: receipt.generation(),
                            publication: RecoveryGuardianPublication {
                                guardian_incarnation: provenance.original.guardian_incarnation,
                                mux_incarnation: provenance.current_mux_incarnation,
                                effect_id: receipt.effect_id(),
                                checkpoint_identity: receipt
                                    .intent()
                                    .checkpoint_identity()
                                    .into_bytes(),
                                output_boundary_identity: receipt
                                    .intent()
                                    .output_boundary_identity()
                                    .into_bytes(),
                            },
                            catalog_generation: receipt.sequence(),
                        },
                    )
                }
            };
            if binding.registration_wire_identity != ack.registration_wire_identity {
                return Err(MuxRecoveryImageError::RegistrationWireIdentityMismatch {
                    pane_id: binding.pane_id,
                    captured: binding.registration_wire_identity,
                    ack: ack.registration_wire_identity,
                });
            }
            if *ack.durable_pane_id.as_bytes() == [0u8; 16] {
                return Err(MuxRecoveryImageError::NilDurablePaneId(binding.pane_id));
            }
            let ack_uuid = ack.durable_pane_id.to_string();
            if binding.pane_uuid != ack_uuid {
                return Err(MuxRecoveryImageError::DurablePaneUuidMismatch {
                    pane_id: binding.pane_id,
                    captured: binding.pane_uuid.clone(),
                    ack: ack_uuid,
                });
            }
            if ack.parser_stream_bytes != ack.terminal_checkpoint.parser_stream_bytes() {
                return Err(MuxRecoveryImageError::ParserStreamWatermarkMismatch {
                    pane_id: binding.pane_id,
                    ack: ack.parser_stream_bytes,
                    checkpoint: ack.terminal_checkpoint.parser_stream_bytes(),
                });
            }
            let invalid_model = |reason| MuxRecoveryImageError::InvalidCheckpointModel {
                pane_id: binding.pane_id,
                reason,
            };
            let validated = frankenterm_term::terminalstate::checkpoint::TerminalCheckpointV3::decode_canonical_json(
                ack.terminal_checkpoint.canonical_payload(),
                frankenterm_term::terminalstate::checkpoint::TerminalCheckpointLimits::default(),
            ).map_err(|_| invalid_model("canonical checkpoint validation failed"))?;
            let model = validated.checkpoint();
            if model.semantic_generation() != ack.semantic_generation {
                return Err(invalid_model("semantic generation differs from ACK"));
            }
            if model.primary_rows() != ack.terminal_checkpoint.rows()
                || model.primary_cols() != ack.terminal_checkpoint.cols()
            {
                return Err(invalid_model(
                    "checkpoint dimensions differ from ACK wrapper",
                ));
            }
            let (pixel_width, pixel_height, dpi) = model.pixel_dimensions_and_dpi();
            let (cursor_row, cursor_col) = model.cursor_position();
            let size = TerminalSize {
                rows: model.primary_rows(),
                cols: model.primary_cols(),
                pixel_width: usize::try_from(pixel_width)
                    .map_err(|_| invalid_model("pixel width exceeds host address space"))?,
                pixel_height: usize::try_from(pixel_height)
                    .map_err(|_| invalid_model("pixel height exceeds host address space"))?,
                dpi,
            };
            let cursor_position = (
                usize::try_from(cursor_col)
                    .map_err(|_| invalid_model("cursor column exceeds host address space"))?,
                usize::try_from(cursor_row)
                    .map_err(|_| invalid_model("cursor row is outside supported coordinates"))?,
            );
            let chk_ref = checkpoint_object_refs
                .get(&binding.pane_id)
                .unwrap()
                .clone();

            pane_uuid_map.insert(binding.pane_id, binding.pane_uuid.clone());

            panes.push(RecoveryPane {
                pane_id: binding.pane_id,
                pane_uuid: binding.pane_uuid.clone(),
                spawn_custody: binding.spawn_custody.into(),
                domain_name: binding.domain_name.clone(),
                title: model.title().to_owned(),
                cwd: model.current_directory().map(str::to_owned),
                size,
                cursor_position,
                alt_screen_active: model.is_alternate_screen_active(),
                checkpoint: PaneCheckpointBinding {
                    topology_incarnation_id: topology_incarnation_id.clone(),
                    pane_uuid: binding.pane_uuid.clone(),
                    registration_wire_identity: binding.registration_wire_identity,
                    parser_capture: ParserCaptureIdentity {
                        watermark_bytes: ack.parser_stream_bytes,
                        segment_id: None,
                        parser_seqno: None,
                    },
                    checkpoint_ref: chk_ref,
                    authority,
                },
            });
        }

        // 5. Build domains
        if captured.domains.len() > MAX_RECOVERY_DOMAINS {
            return Err(MuxRecoveryImageError::InvalidCapturedTopology(
                "captured domain census exceeds limit",
            ));
        }
        let mut domains = Vec::with_capacity(captured.domains.len());
        let mut policy_bytes = 0usize;
        for domain in &captured.domains {
            validate_string_len("domain.domain_name", &domain.name, MAX_STRING_BYTES)?;
            let policy = RecoveryDomainPolicy::from_mux(&domain.policy)?;
            policy_bytes = policy_bytes
                .checked_add(policy.validate()?)
                .and_then(|bytes| bytes.checked_add(domain.name.len()))
                .ok_or(MuxRecoveryImageError::InvalidCapturedTopology(
                    "domain policy budget overflow",
                ))?;
            if policy_bytes > MAX_RECOVERY_IMAGE_BYTES {
                return Err(MuxRecoveryImageError::InvalidCapturedTopology(
                    "aggregate domain policy budget exceeded",
                ));
            }
            domains.push(RecoveryDomain {
                incarnation_domain_id: domain.domain_id,
                domain_name: domain.name.clone(),
                is_attached: domain.state == mux::domain::DomainState::Attached,
                recovery_policy: Some(policy),
            });
        }
        for binding in &captured.pane_bindings {
            if !domains.iter().any(|domain| {
                domain.incarnation_domain_id == binding.domain_id
                    && domain.domain_name == binding.domain_name
                    && domain.is_attached
            }) {
                return Err(MuxRecoveryImageError::InvalidCapturedTopology(
                    "pane binding differs from captured domain census",
                ));
            }
        }

        // 6. Build windows and tabs
        let mut captured_tabs_by_id = HashMap::new();
        for tab in &captured.tabs {
            if tab.durable_tab_id.is_nil() {
                return Err(MuxRecoveryImageError::InvalidCapturedTopology(
                    "captured tab has no durable identity",
                ));
            }
            if captured_tabs_by_id.insert(tab.tab_id, tab).is_some() {
                return Err(MuxRecoveryImageError::DuplicateTabId(tab.tab_id));
            }
        }

        let mut windows = Vec::with_capacity(captured.windows.len());
        for win in &captured.windows {
            if win.durable_window_id.is_nil() {
                return Err(MuxRecoveryImageError::InvalidCapturedTopology(
                    "captured window has no durable identity",
                ));
            }
            let mut tabs = Vec::with_capacity(win.ordered_tab_ids.len());
            for &tab_id in &win.ordered_tab_ids {
                {
                    let tab = captured_tabs_by_id.remove(&tab_id).ok_or(
                        MuxRecoveryImageError::InvalidCapturedTopology(
                            "ordered tab is missing or repeated",
                        ),
                    )?;
                    if tab.window_id != win.window_id {
                        return Err(MuxRecoveryImageError::InvalidCapturedTopology(
                            "tab window binding mismatch",
                        ));
                    }
                    let root_split = convert_mux_pane_node(&tab.split_tree, &pane_uuid_map)?;
                    let floating_panes = tab
                        .floating_panes
                        .iter()
                        .map(|fp| {
                            let pane_uuid = pane_uuid_map
                                .get(&fp.pane_id)
                                .cloned()
                                .ok_or(MuxRecoveryImageError::MissingPane(fp.pane_id))?;
                            Ok(RecoveryFloatingPane::new(
                                fp.pane_id,
                                pane_uuid,
                                FloatingPaneRect {
                                    left: fp.rect.left,
                                    top: fp.rect.top,
                                    width: fp.rect.width,
                                    height: fp.rect.height,
                                },
                                fp.z_order,
                                fp.visible,
                                fp.pinned,
                                fp.opacity,
                            ))
                        })
                        .collect::<Result<Vec<_>, MuxRecoveryImageError>>()?;

                    let tab_stacks: Vec<RecoveryPaneStack> = tab
                        .pane_stacks
                        .iter()
                        .map(|s| RecoveryPaneStack {
                            slot_index: s.slot_index,
                            pane_ids: s.pane_ids.clone(),
                            active_index: s.active_index,
                        })
                        .collect();

                    let active_pane_id = match tab.active_pane_id {
                        Some(id) => id,
                        None if root_split.is_none()
                            && floating_panes.is_empty()
                            && tab.pane_stacks.is_empty() =>
                        {
                            0
                        }
                        None => {
                            return Err(MuxRecoveryImageError::InvalidCapturedTopology(
                                "nonempty tab has no active pane",
                            ));
                        }
                    };

                    let mut runtime_members: HashSet<usize> = root_split
                        .as_ref()
                        .map(|root| root.leaves().into_iter().map(|(id, _)| id).collect())
                        .unwrap_or_default();
                    runtime_members.extend(floating_panes.iter().map(|pane| pane.pane_id));
                    for stack in &tab_stacks {
                        runtime_members.extend(stack.pane_ids.iter().copied());
                    }
                    let runtime_state =
                        RecoveryTabRuntime::from_mux(&tab.runtime_state, &runtime_members)?;
                    tabs.push(RecoveryTab {
                        tab_id: tab.tab_id,
                        stable_tab_id: tab.durable_tab_id.to_string(),
                        title: tab.title.clone(),
                        working_dir: None,
                        size: TerminalSize {
                            rows: tab.size.rows,
                            cols: tab.size.cols,
                            pixel_width: tab.size.pixel_width,
                            pixel_height: tab.size.pixel_height,
                            dpi: tab.size.dpi,
                        },
                        size_before_zoom: TerminalSize {
                            rows: tab.size_before_zoom.rows,
                            cols: tab.size_before_zoom.cols,
                            pixel_width: tab.size_before_zoom.pixel_width,
                            pixel_height: tab.size_before_zoom.pixel_height,
                            dpi: tab.size_before_zoom.dpi,
                        },
                        zoomed_pane_id: tab.zoomed_pane_id,
                        root_split,
                        floating_panes,
                        floating_focus: tab.floating_focus,
                        active_pane_id,
                        pane_stacks: tab_stacks,
                        underlying_tiled_active_pane_id: tab.underlying_tiled_active_pane_id,
                        runtime_state: Some(runtime_state),
                    });
                }
            }

            let active_tab_index = match win.active_tab_index {
                Some(index) if index < tabs.len() => index,
                None if tabs.is_empty() && win.active_tab_id.is_none() => 0,
                _ => {
                    return Err(MuxRecoveryImageError::InvalidCapturedTopology(
                        "active tab index is missing or invalid",
                    ));
                }
            };
            if win.active_tab_id != tabs.get(active_tab_index).map(|tab| tab.tab_id) {
                return Err(MuxRecoveryImageError::InvalidCapturedTopology(
                    "active tab id and index disagree",
                ));
            }

            windows.push(RecoveryWindow {
                window_id: win.window_id,
                stable_window_id: win.durable_window_id.to_string(),
                workspace: win.workspace.clone(),
                title: win.title.clone(),
                order_revision: win.order_revision.get(),
                gui_position: win.position.as_ref().map(RecoveryGuiPosition::from),
                tabs,
                active_tab_index,
                last_active_tab_id: win.last_active_tab_id,
                tab_stacks: convert_mux_window_tab_stacks(&win.tab_stacks)?,
            });
        }
        if !captured_tabs_by_id.is_empty() {
            return Err(MuxRecoveryImageError::InvalidCapturedTopology(
                "captured tab is absent from window order",
            ));
        }

        // 7. Topology container
        let focused_window_id = captured.client_workspace.as_ref().and_then(|client| {
            captured
                .workspaces
                .iter()
                .find(|workspace| workspace.name == client.active_workspace)
                .and_then(|workspace| workspace.active_window_id)
        });

        let client_workspace =
            captured
                .client_workspace
                .as_ref()
                .map(|cw| ClientWorkspaceBinding {
                    client_id: cw.client_id.clone(),
                    active_workspace: cw.active_workspace.clone(),
                });

        let topology = RecoveryTopology {
            domains,
            domain_state: Some(RecoveryDomainState {
                default_domain_id: captured.default_domain_id,
            }),
            windows,
            focused_window_id,
            client_workspace,
            workspace_state: Some(RecoveryWorkspaceState {
                default_workspace: captured.default_workspace.clone(),
                workspaces: captured
                    .workspaces
                    .iter()
                    .map(|workspace| RecoveryWorkspace {
                        name: workspace.name.clone(),
                        window_ids: workspace.window_ids.clone(),
                        active_window_id: workspace.active_window_id,
                    })
                    .collect(),
            }),
        };

        // 8. Envelope assembly, digest calculation, and full validation
        let header = RecoveryImageHeader {
            magic: MUX_RECOVERY_IMAGE_MAGIC,
            schema_version: MUX_RECOVERY_IMAGE_SCHEMA_VERSION,
            generation: meta.generation,
            predecessor_digest: meta.predecessor_digest,
            created_at_epoch_ms: meta.created_at_epoch_ms,
            mux_incarnation_id: topology_incarnation_id,
            ft_version: meta.ft_version,
            session_id: meta.session_id,
        };

        let mut image = MuxRecoveryImage {
            header,
            topology,
            panes,
            image_digest: [0u8; 32],
        };

        let digest = image.compute_digest()?;
        image.image_digest = digest;
        image.validate()?;
        Ok(image)
    }
}

#[cfg(feature = "frankenterm-deps")]
fn convert_mux_window_tab_stacks(
    entries: &[mux::tab::TabStackEntry],
) -> Result<Vec<RecoveryTabStack>, MuxRecoveryImageError> {
    if entries.len() > MAX_RECOVERY_TABS_PER_WINDOW {
        return Err(MuxRecoveryImageError::ResourceLimit {
            resource: "window_tab_stack_members",
            count: entries.len(),
            limit: MAX_RECOVERY_TABS_PER_WINDOW,
        });
    }
    let invalid = || {
        MuxRecoveryImageError::InvalidCapturedTopology(
            "window tab stack entries require ordered contiguous positions and one visible member",
        )
    };
    let mut stacks = Vec::new();
    let mut remaining = entries;
    while let Some(first) = remaining.first() {
        if stacks
            .last()
            .is_some_and(|stack: &RecoveryTabStack| stack.stack_id >= first.stack_id.0)
        {
            return Err(invalid());
        }
        let count = remaining
            .iter()
            .take_while(|entry| entry.stack_id == first.stack_id)
            .count();
        let mut visible_tab_id = None;
        let mut tab_ids = Vec::with_capacity(count);
        for (position, entry) in remaining[..count].iter().enumerate() {
            if entry.position != position {
                return Err(invalid());
            }
            if entry.is_visible && visible_tab_id.replace(entry.tab_id).is_some() {
                return Err(invalid());
            }
            tab_ids.push(entry.tab_id);
        }
        stacks.push(RecoveryTabStack {
            stack_id: first.stack_id.0,
            tab_ids,
            visible_tab_id: visible_tab_id.ok_or_else(invalid)?,
        });
        remaining = &remaining[count..];
    }
    Ok(stacks)
}

#[cfg(feature = "frankenterm-deps")]
fn convert_mux_pane_node(
    node: &mux::tab::PaneNode,
    pane_uuid_map: &HashMap<usize, String>,
) -> Result<Option<RecoverySplitNode>, MuxRecoveryImageError> {
    match node {
        mux::tab::PaneNode::Empty => Ok(None),
        mux::tab::PaneNode::Leaf(entry) => {
            let pane_uuid = pane_uuid_map
                .get(&entry.pane_id)
                .cloned()
                .ok_or(MuxRecoveryImageError::MissingPane(entry.pane_id))?;
            Ok(Some(RecoverySplitNode::Leaf {
                pane_id: entry.pane_id,
                pane_uuid,
            }))
        }
        mux::tab::PaneNode::Split { left, right, node } => {
            let left_conv = convert_mux_pane_node(left, pane_uuid_map)?.ok_or(
                MuxRecoveryImageError::MalformedSplit("left split child is empty"),
            )?;
            let right_conv = convert_mux_pane_node(right, pane_uuid_map)?.ok_or(
                MuxRecoveryImageError::MalformedSplit("right split child is empty"),
            )?;
            let direction = match node.direction {
                mux::tab::SplitDirection::Horizontal => SplitDirection::Horizontal,
                mux::tab::SplitDirection::Vertical => SplitDirection::Vertical,
            };
            let split = SplitDirectionAndSize {
                direction,
                first: TerminalSize {
                    rows: node.first.rows,
                    cols: node.first.cols,
                    pixel_width: node.first.pixel_width,
                    pixel_height: node.first.pixel_height,
                    dpi: node.first.dpi,
                },
                second: TerminalSize {
                    rows: node.second.rows,
                    cols: node.second.cols,
                    pixel_width: node.second.pixel_width,
                    pixel_height: node.second.pixel_height,
                    dpi: node.second.dpi,
                },
            };
            Ok(Some(RecoverySplitNode::Split {
                split,
                left: Box::new(left_conv),
                right: Box::new(right_conv),
            }))
        }
    }
}

// =============================================================================
// Helper Functions
// =============================================================================

fn validate_durable_identity(
    field: &'static str,
    value: &str,
) -> Result<(), MuxRecoveryImageError> {
    let identity = uuid::Uuid::parse_str(value)
        .map_err(|_| MuxRecoveryImageError::InvalidDurableIdentity { field })?;
    if identity.is_nil() || identity.to_string() != value {
        return Err(MuxRecoveryImageError::InvalidDurableIdentity { field });
    }
    Ok(())
}

fn validate_string_len(
    field: &'static str,
    val: &str,
    limit: usize,
) -> Result<(), MuxRecoveryImageError> {
    if val.len() > limit {
        Err(MuxRecoveryImageError::StringTooLong {
            field,
            len: val.len(),
            limit,
        })
    } else {
        Ok(())
    }
}

fn check_split_tree_bounds(
    node: &RecoverySplitNode,
    depth: usize,
) -> Result<(), MuxRecoveryImageError> {
    if depth > MAX_SPLIT_TREE_DEPTH {
        return Err(MuxRecoveryImageError::TooDeep {
            depth,
            limit: MAX_SPLIT_TREE_DEPTH,
        });
    }
    match node {
        RecoverySplitNode::Leaf { pane_uuid, .. } => {
            validate_string_len("split.leaf.pane_uuid", pane_uuid, MAX_ID_STRING_BYTES)?;
        }
        RecoverySplitNode::Split { left, right, .. } => {
            check_split_tree_bounds(left, depth + 1)?;
            check_split_tree_bounds(right, depth + 1)?;
        }
    }
    Ok(())
}

fn validate_split_node(
    node: &RecoverySplitNode,
    depth: usize,
    catalog_pane_uuids_by_id: &HashMap<usize, &str>,
    tab_panes: &mut HashSet<usize>,
    global_placed_panes: &mut HashSet<usize>,
) -> Result<(), MuxRecoveryImageError> {
    if depth > MAX_SPLIT_TREE_DEPTH {
        return Err(MuxRecoveryImageError::TooDeep {
            depth,
            limit: MAX_SPLIT_TREE_DEPTH,
        });
    }

    match node {
        RecoverySplitNode::Leaf { pane_id, pane_uuid } => {
            validate_string_len("split.leaf.pane_uuid", pane_uuid, MAX_ID_STRING_BYTES)?;
            let catalog_uuid = match catalog_pane_uuids_by_id.get(pane_id) {
                Some(u) => *u,
                None => return Err(MuxRecoveryImageError::MissingPane(*pane_id)),
            };
            if catalog_uuid != pane_uuid.as_str() {
                return Err(MuxRecoveryImageError::PaneUuidMismatchCatalog {
                    pane_id: *pane_id,
                    catalog_uuid: catalog_uuid.to_string(),
                    placed_uuid: pane_uuid.clone(),
                });
            }
            if !tab_panes.insert(*pane_id) {
                return Err(MuxRecoveryImageError::DuplicatePanePlacement(*pane_id));
            }
            if !global_placed_panes.insert(*pane_id) {
                return Err(MuxRecoveryImageError::DuplicatePanePlacement(*pane_id));
            }
        }
        RecoverySplitNode::Split { split, left, right } => {
            if split.first.cols == 0
                && split.first.rows == 0
                && split.second.cols == 0
                && split.second.rows == 0
            {
                return Err(MuxRecoveryImageError::MalformedSplit(
                    "split node children both have zero terminal dimensions",
                ));
            }
            validate_split_node(
                left,
                depth + 1,
                catalog_pane_uuids_by_id,
                tab_panes,
                global_placed_panes,
            )?;
            validate_split_node(
                right,
                depth + 1,
                catalog_pane_uuids_by_id,
                tab_panes,
                global_placed_panes,
            )?;
        }
    }

    Ok(())
}

// =============================================================================
// Unit & Causal Regression Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn test_guardian_publication() -> RecoveryGuardianPublication {
        RecoveryGuardianPublication {
            guardian_incarnation: uuid::Uuid::from_u128(1),
            mux_incarnation: uuid::Uuid::from_u128(2),
            effect_id: uuid::Uuid::from_u128(3),
            checkpoint_identity: [4; 32],
            output_boundary_identity: [5; 32],
        }
    }

    #[test]
    fn original_spawn_authority_uses_captured_compact_mux_identity() {
        assert!(
            std::mem::size_of::<RecoverySpawnCustody>() <= 2 * std::mem::size_of::<usize>(),
            "absent custody must not reserve the full provenance payload"
        );
        let mut image = make_valid_test_image();
        let publication = test_guardian_publication();
        let incarnation = hex::encode(publication.mux_incarnation.as_bytes());
        image.header.mux_incarnation_id = incarnation.clone();
        for pane in &mut image.panes {
            pane.checkpoint.topology_incarnation_id = incarnation.clone();
        }
        let durable_id = uuid::Uuid::from_u128(9);
        let pane = &mut image.panes[0];
        pane.pane_uuid = durable_id.to_string();
        pane.checkpoint.pane_uuid = durable_id.to_string();
        pane.spawn_custody = RecoverySpawnCustody::Original(Box::new(RecoveryOriginalCustody {
            broker_lineage: uuid::Uuid::from_u128(10),
            guardian_incarnation: publication.guardian_incarnation,
            original_mux_incarnation: publication.mux_incarnation,
            broker_build: [1; 32],
            guardian_build: [2; 32],
            original_mux_build: [3; 32],
            pane_id: durable_id,
            spawn_effect_id: uuid::Uuid::from_u128(11),
            current_mux_incarnation: publication.mux_incarnation,
            current_lease_generation: 1,
            acknowledged_successor: None,
        }));
        pane.checkpoint.authority = CheckpointAuthority::Guardian {
            guardian_generation: 1,
            publication: publication.clone(),
            catalog_generation: 1,
        };
        let Some(RecoverySplitNode::Split { left, .. }) =
            image.topology.windows[0].tabs[0].root_split.as_mut()
        else {
            panic!("fixture must contain the first pane in a split");
        };
        let RecoverySplitNode::Leaf { pane_uuid, .. } = left.as_mut() else {
            panic!("fixture must contain a leaf");
        };
        *pane_uuid = durable_id.to_string();
        image.image_digest = image.compute_digest().unwrap();
        let bytes = image.to_canonical_json().unwrap();
        assert_eq!(MuxRecoveryImage::from_json_slice(&bytes).unwrap(), image);

        // Schema 3 had no successor field. Its already-signed bytes must not
        // acquire even a JSON null on decode/re-encode. Check a generation-two
        // image, which is readable history but lacks repeat-rotation authority.
        let mut legacy = image.clone();
        legacy.header.schema_version = 3;
        legacy.topology.domain_state = None;
        for domain in &mut legacy.topology.domains {
            domain.recovery_policy = None;
        }
        legacy.topology.workspace_state = None;
        for tab in legacy
            .topology
            .windows
            .iter_mut()
            .flat_map(|window| &mut window.tabs)
        {
            tab.runtime_state = None;
        }
        if let RecoverySpawnCustody::Original(custody) = &mut legacy.panes[0].spawn_custody {
            custody.original_mux_incarnation = uuid::Uuid::from_u128(13);
            custody.current_lease_generation = 2;
        }
        if let CheckpointAuthority::Guardian {
            guardian_generation,
            ..
        } = &mut legacy.panes[0].checkpoint.authority
        {
            *guardian_generation = 2;
        }
        // Freeze the schema-3 custody shape independently of the current enum.
        // Adding, reordering, or emitting null for the new field must change
        // this comparison rather than silently regenerating historical bytes.
        #[derive(Serialize)]
        enum Schema3Custody {
            Original {
                broker_lineage: uuid::Uuid,
                guardian_incarnation: uuid::Uuid,
                original_mux_incarnation: uuid::Uuid,
                broker_build: [u8; 32],
                guardian_build: [u8; 32],
                original_mux_build: [u8; 32],
                pane_id: uuid::Uuid,
                spawn_effect_id: uuid::Uuid,
                current_mux_incarnation: uuid::Uuid,
                current_lease_generation: u64,
            },
        }
        let old_custody_bytes = serde_json::to_vec(&Schema3Custody::Original {
            broker_lineage: uuid::Uuid::from_u128(10),
            guardian_incarnation: uuid::Uuid::from_u128(1),
            original_mux_incarnation: uuid::Uuid::from_u128(13),
            broker_build: [1; 32],
            guardian_build: [2; 32],
            original_mux_build: [3; 32],
            pane_id: uuid::Uuid::from_u128(9),
            spawn_effect_id: uuid::Uuid::from_u128(11),
            current_mux_incarnation: uuid::Uuid::from_u128(2),
            current_lease_generation: 2,
        })
        .unwrap();
        assert_eq!(
            serde_json::to_vec(&legacy.panes[0].spawn_custody).unwrap(),
            old_custody_bytes
        );
        legacy.image_digest = legacy.compute_digest().unwrap();
        let historical_bytes = legacy.to_canonical_json().unwrap();
        assert!(
            !std::str::from_utf8(&historical_bytes)
                .unwrap()
                .contains("acknowledged_successor")
        );
        let reopened = MuxRecoveryImage::from_json_slice(&historical_bytes).unwrap();
        assert_eq!(reopened.image_digest, legacy.image_digest);
        assert_eq!(reopened.compute_digest().unwrap(), legacy.image_digest);
        assert_eq!(reopened.to_canonical_json().unwrap(), historical_bytes);
        legacy.header.schema_version = 4;
        legacy.image_digest = legacy.compute_digest().unwrap();
        assert!(matches!(
            legacy.validate(),
            Err(MuxRecoveryImageError::InvalidAuthority { .. })
        ));
        let owner = RecoveryCustodyOwner {
            guardian_incarnation: publication.guardian_incarnation,
            connection_id: uuid::Uuid::from_u128(14),
            mux_incarnation: publication.mux_incarnation,
            guardian_build: [2; 32],
            mux_build: [3; 32],
        };
        if let RecoverySpawnCustody::Original(custody) = &mut legacy.panes[0].spawn_custody {
            custody.acknowledged_successor = Some(RecoverySuccessorCustody {
                broker_incarnation: uuid::Uuid::from_u128(15),
                broker_lineage: uuid::Uuid::from_u128(10),
                broker_build: [1; 32],
                predecessor: RecoveryCustodyOwner {
                    connection_id: uuid::Uuid::from_u128(16),
                    mux_incarnation: uuid::Uuid::from_u128(13),
                    ..owner
                },
                successor: owner,
                pane_id: durable_id,
                handoff_id: uuid::Uuid::from_u128(17),
                ack_id: uuid::Uuid::from_u128(18),
                lease_generation: 2,
                rebind_from_connection: uuid::Uuid::nil(),
            });
        }
        legacy.image_digest = legacy.compute_digest().unwrap();
        legacy.validate().unwrap();
        legacy.header.schema_version = 3;
        legacy.image_digest = legacy.compute_digest().unwrap();
        assert!(matches!(
            legacy.validate(),
            Err(MuxRecoveryImageError::InvalidAuthority { .. })
        ));

        if let RecoverySpawnCustody::Original(custody) = &mut image.panes[0].spawn_custody {
            custody.current_mux_incarnation = uuid::Uuid::from_u128(12);
        }
        image.image_digest = image.compute_digest().unwrap();
        assert!(matches!(
            image.validate(),
            Err(MuxRecoveryImageError::InvalidAuthority { pane_id: 1, .. })
        ));
    }

    #[test]
    fn durable_window_and_tab_ids_require_canonical_nonnil_uuids() {
        for window_identity in [true, false] {
            for invalid in [
                "",
                "window-1",
                "00000000-0000-0000-0000-000000000000",
                "000000000000000000000000000000ab",
                "00000000-0000-0000-0000-0000000000AB",
                "urn:uuid:00000000-0000-0000-0000-0000000000ab",
            ] {
                let mut image = make_valid_test_image();
                let field = if window_identity {
                    image.topology.windows[0].stable_window_id = invalid.into();
                    "window.stable_window_id"
                } else {
                    image.topology.windows[0].tabs[0].stable_tab_id = invalid.into();
                    "tab.stable_tab_id"
                };
                image.image_digest = image.compute_digest().unwrap();
                assert_eq!(
                    image.validate().unwrap_err(),
                    MuxRecoveryImageError::InvalidDurableIdentity { field }
                );
                assert!(matches!(
                    MuxRecoveryImage::from_json_slice(&serde_json::to_vec(&image).unwrap()),
                    Err(MuxRecoveryImageError::InvalidDurableIdentity { field: actual })
                        if actual == field
                ));
            }
        }
        let mut image = make_valid_test_image();
        image.topology.windows[0].stable_window_id = "00000000-0000-0000-0000-0000000000ab".into();
        image.topology.windows[0].tabs[0].stable_tab_id =
            "00000000-0000-0000-0000-0000000000cd".into();
        image.image_digest = image.compute_digest().unwrap();
        image.validate().unwrap();
    }

    #[test]
    fn canonical_durable_id_duplicates_are_rejected_across_numeric_identities() {
        let mut image = make_valid_test_image();
        let mut duplicate = image.topology.windows[0].clone();
        duplicate.window_id += 1;
        duplicate.tabs.clear();
        let expected = duplicate.stable_window_id.clone();
        image.topology.windows.push(duplicate);
        image.image_digest = image.compute_digest().unwrap();
        assert_eq!(
            image.validate().unwrap_err(),
            MuxRecoveryImageError::DuplicateStableWindowId(expected)
        );

        let mut image = make_valid_test_image();
        let mut duplicate = image.topology.windows[0].tabs[0].clone();
        duplicate.tab_id += 1;
        let expected = duplicate.stable_tab_id.clone();
        image.topology.windows[0].tabs.push(duplicate);
        image.image_digest = image.compute_digest().unwrap();
        assert_eq!(
            image.validate().unwrap_err(),
            MuxRecoveryImageError::DuplicateStableTabId(expected)
        );
    }

    #[test]
    fn window_recovery_order_revision_rejects_only_reserved_exhaustion_sentinel() {
        let mut image = make_valid_test_image();
        for revision in [0, 1, u64::MAX - 1] {
            image.topology.windows[0].order_revision = revision;
            image.image_digest = image.compute_digest().unwrap();
            image.validate().unwrap();
        }

        image.topology.windows[0].order_revision = u64::MAX;
        image.image_digest = image.compute_digest().unwrap();
        assert_eq!(
            image.validate().unwrap_err(),
            MuxRecoveryImageError::InvalidCapturedTopology(
                "window order revision is the reserved exhaustion sentinel"
            )
        );
    }

    #[test]
    fn window_recovery_metadata_is_required_and_prior_schema_is_rejected() {
        let image = make_valid_test_image();
        for field in ["title", "tab_stacks", "last_active_tab_id"] {
            let mut value = serde_json::to_value(&image).unwrap();
            value["topology"]["windows"][0]
                .as_object_mut()
                .unwrap()
                .remove(field);
            assert!(
                serde_json::from_value::<MuxRecoveryImage>(value).is_err(),
                "{field}"
            );
        }
        let mut without_provenance = serde_json::to_value(&image).unwrap();
        without_provenance["panes"][0]
            .as_object_mut()
            .unwrap()
            .remove("spawn_custody");
        assert!(serde_json::from_value::<MuxRecoveryImage>(without_provenance).is_err());
        for version in [1, 2] {
            let mut old = image.clone();
            old.header.schema_version = version;
            assert_eq!(
                old.validate().unwrap_err(),
                MuxRecoveryImageError::UnsupportedSchemaVersion(version)
            );
        }
    }

    #[test]
    fn window_recovery_bounds_report_the_offending_field_and_count() {
        let mut image = make_valid_test_image();
        image.topology.windows[0].title = "x".repeat(MAX_STRING_BYTES + 1);
        assert_eq!(
            image.validate_bounds().unwrap_err(),
            MuxRecoveryImageError::StringTooLong {
                field: "window.title",
                len: MAX_STRING_BYTES + 1,
                limit: MAX_STRING_BYTES,
            }
        );
        image.topology.windows[0].title.clear();
        let empty_stack = RecoveryTabStack {
            stack_id: 0,
            tab_ids: vec![],
            visible_tab_id: 0,
        };
        image.topology.windows[0].tab_stacks =
            vec![empty_stack.clone(); MAX_RECOVERY_TABS_PER_WINDOW + 1];
        assert_eq!(
            image.validate_bounds().unwrap_err(),
            MuxRecoveryImageError::ResourceLimit {
                resource: "window_tab_stacks",
                count: MAX_RECOVERY_TABS_PER_WINDOW + 1,
                limit: MAX_RECOVERY_TABS_PER_WINDOW,
            }
        );
        image.topology.windows[0].tab_stacks = vec![RecoveryTabStack {
            tab_ids: vec![0; MAX_RECOVERY_TABS_PER_WINDOW + 1],
            ..empty_stack
        }];
        assert_eq!(
            image.validate_bounds().unwrap_err(),
            MuxRecoveryImageError::ResourceLimit {
                resource: "window_tab_stack_members",
                count: MAX_RECOVERY_TABS_PER_WINDOW + 1,
                limit: MAX_RECOVERY_TABS_PER_WINDOW,
            }
        );
    }

    #[test]
    fn window_recovery_rejects_malformed_stacks_and_dangling_history() {
        let base = make_valid_test_image();
        let tab_id = base.topology.windows[0].tabs[0].tab_id;
        let valid_stack = RecoveryTabStack {
            stack_id: 3,
            tab_ids: vec![tab_id],
            visible_tab_id: tab_id,
        };
        let mut valid = base.clone();
        valid.topology.windows[0].tab_stacks = vec![valid_stack.clone()];
        valid.topology.windows[0].last_active_tab_id = Some(tab_id);
        valid.image_digest = valid.compute_digest().unwrap();
        valid.validate().unwrap();
        for stacks in [
            vec![RecoveryTabStack {
                tab_ids: vec![],
                ..valid_stack.clone()
            }],
            vec![RecoveryTabStack {
                tab_ids: vec![tab_id, tab_id],
                ..valid_stack.clone()
            }],
            vec![RecoveryTabStack {
                tab_ids: vec![999],
                visible_tab_id: 999,
                ..valid_stack.clone()
            }],
            vec![RecoveryTabStack {
                visible_tab_id: 999,
                ..valid_stack.clone()
            }],
            vec![valid_stack.clone(), valid_stack],
        ] {
            let mut image = base.clone();
            image.topology.windows[0].tab_stacks = stacks;
            image.image_digest = image.compute_digest().unwrap();
            assert!(matches!(
                image.validate(),
                Err(MuxRecoveryImageError::InvalidCapturedTopology(_))
            ));
        }
        let mut image = base;
        image.topology.windows[0].last_active_tab_id = Some(999);
        image.image_digest = image.compute_digest().unwrap();
        assert_eq!(
            image.validate().unwrap_err(),
            MuxRecoveryImageError::InvalidCapturedTopology(
                "last active tab is absent from its window"
            )
        );
    }

    fn make_test_pane(pane_id: usize, pane_uuid: &str, incarnation_id: &str) -> RecoveryPane {
        RecoveryPane {
            spawn_custody: RecoverySpawnCustody::Absent,
            pane_id,
            pane_uuid: pane_uuid.to_string(),
            domain_name: "local".to_string(),
            title: format!("bash_{pane_id}"),
            cwd: Some("/project".to_string()),
            size: TerminalSize {
                rows: 24,
                cols: 80,
                pixel_width: 800,
                pixel_height: 480,
                dpi: 96,
            },
            cursor_position: (0, 0),
            alt_screen_active: false,
            checkpoint: PaneCheckpointBinding {
                topology_incarnation_id: incarnation_id.to_string(),
                pane_uuid: pane_uuid.to_string(),
                registration_wire_identity: [1; 16],
                parser_capture: ParserCaptureIdentity {
                    watermark_bytes: 1024,
                    segment_id: None,
                    parser_seqno: Some(42),
                },
                checkpoint_ref: RecoveryObjectRef {
                    object_id: format!("obj_{pane_id}"),
                    byte_length: 512,
                    payload_digest: [1u8; 32],
                    schema_version: 3,
                },
                authority: CheckpointAuthority::ModelOnly {
                    captured_at_epoch_ms: 1747371642000,
                    parser_seqno: Some(42),
                },
            },
        }
    }

    fn make_valid_test_image() -> MuxRecoveryImage {
        let incarnation_id = "inc-20260914-1800";
        let pane1 = make_test_pane(1, "uuid-pane-1", incarnation_id);
        let pane2 = make_test_pane(2, "uuid-pane-2", incarnation_id);
        let pane3 = make_test_pane(3, "uuid-pane-3", incarnation_id);

        let split_tree = RecoverySplitNode::Split {
            split: SplitDirectionAndSize {
                direction: SplitDirection::Horizontal,
                first: TerminalSize {
                    rows: 24,
                    cols: 40,
                    pixel_width: 400,
                    pixel_height: 480,
                    dpi: 96,
                },
                second: TerminalSize {
                    rows: 24,
                    cols: 40,
                    pixel_width: 400,
                    pixel_height: 480,
                    dpi: 96,
                },
            },
            left: Box::new(RecoverySplitNode::Leaf {
                pane_id: 1,
                pane_uuid: "uuid-pane-1".to_string(),
            }),
            right: Box::new(RecoverySplitNode::Leaf {
                pane_id: 2,
                pane_uuid: "uuid-pane-2".to_string(),
            }),
        };

        let floating_pane = RecoveryFloatingPane::new(
            3,
            "uuid-pane-3".to_string(),
            FloatingPaneRect {
                left: 10,
                top: 5,
                width: 60,
                height: 15,
            },
            1,
            true,
            false,
            0.9,
        );

        let tab = RecoveryTab {
            tab_id: 10,
            stable_tab_id: "00000000-0000-0000-0000-000000000010".to_string(),
            title: "main_tab".to_string(),
            working_dir: Some("/project".to_string()),
            size: TerminalSize {
                rows: 24,
                cols: 80,
                pixel_width: 800,
                pixel_height: 480,
                dpi: 96,
            },
            size_before_zoom: TerminalSize {
                rows: 24,
                cols: 80,
                pixel_width: 800,
                pixel_height: 480,
                dpi: 96,
            },
            zoomed_pane_id: None,
            root_split: Some(split_tree),
            floating_panes: vec![floating_pane],
            floating_focus: None,
            active_pane_id: 1,
            pane_stacks: vec![],
            underlying_tiled_active_pane_id: Some(1),
            runtime_state: Some(Default::default()),
        };

        let window = RecoveryWindow {
            window_id: 100,
            stable_window_id: "00000000-0000-0000-0000-000000000100".to_string(),
            workspace: "default".to_string(),
            title: "recovery window".to_string(),
            last_active_tab_id: None,
            tab_stacks: vec![],
            order_revision: 1,
            gui_position: Some(RecoveryGuiPosition {
                x: RecoveryGuiDimension::Pixels(100.0_f32.to_bits()),
                y: RecoveryGuiDimension::Pixels(100.0_f32.to_bits()),
                origin: RecoveryGeometryOrigin::ScreenCoordinateSystem,
            }),
            tabs: vec![tab],
            active_tab_index: 0,
        };

        let topology = RecoveryTopology {
            domains: vec![RecoveryDomain {
                incarnation_domain_id: 0,
                domain_name: "local".to_string(),
                is_attached: true,
                recovery_policy: Some(RecoveryDomainPolicy::local_test_fixture()),
            }],
            domain_state: Some(Default::default()),
            windows: vec![window],
            focused_window_id: Some(100),
            client_workspace: Some(ClientWorkspaceBinding {
                client_id: "client-gui-1".to_string(),
                active_workspace: "default".to_string(),
            }),
            workspace_state: Some(RecoveryWorkspaceState {
                default_workspace: "default".into(),
                workspaces: vec![RecoveryWorkspace {
                    name: "default".into(),
                    window_ids: vec![100],
                    active_window_id: Some(100),
                }],
            }),
        };

        let header = RecoveryImageHeader {
            magic: MUX_RECOVERY_IMAGE_MAGIC,
            schema_version: MUX_RECOVERY_IMAGE_SCHEMA_VERSION,
            generation: 1,
            predecessor_digest: None,
            created_at_epoch_ms: 1747371642000,
            mux_incarnation_id: incarnation_id.to_string(),
            ft_version: "0.2.0".to_string(),
            session_id: "session-xyz".to_string(),
        };

        let mut image = MuxRecoveryImage {
            header,
            topology,
            panes: vec![pane1, pane2, pane3],
            image_digest: [0u8; 32],
        };

        let digest = image.compute_digest().unwrap();
        image.image_digest = digest;
        image
    }

    #[test]
    fn domain_authority_requires_schema_seven_and_preserves_historical_bytes() {
        let image = make_valid_test_image();
        image.require_live_topology_state().unwrap();
        for version in [3, 4, 5, 6] {
            let mut historical = image.clone();
            historical.header.schema_version = version;
            historical.topology.domain_state = None;
            for domain in &mut historical.topology.domains {
                domain.recovery_policy = None;
            }
            if version < 5 {
                historical.topology.workspace_state = None;
            }
            if version < 6 {
                for tab in historical
                    .topology
                    .windows
                    .iter_mut()
                    .flat_map(|window| &mut window.tabs)
                {
                    tab.runtime_state = None;
                }
            }
            historical.image_digest = historical.compute_digest().unwrap();
            let bytes = historical.to_canonical_json().unwrap();
            let text = std::str::from_utf8(&bytes).unwrap();
            assert!(!text.contains("domain_state") && !text.contains("recovery_policy"));
            let reopened = MuxRecoveryImage::from_json_slice(&bytes).unwrap();
            assert_eq!(reopened.to_canonical_json().unwrap(), bytes);
            assert!(reopened.require_live_topology_state().is_err());
            historical.topology.domain_state = Some(Default::default());
            assert!(historical.validate().is_err());
        }
        let mut missing = image.clone();
        missing.topology.domain_state = None;
        assert!(missing.validate().is_err());
        missing = image.clone();
        missing.topology.domains[0].recovery_policy = None;
        assert!(missing.validate().is_err());
        let mut wire = serde_json::to_value(&image).unwrap();
        wire["topology"]["domain_state"]
            .as_object_mut()
            .unwrap()
            .remove("default_domain_id");
        assert!(serde_json::from_value::<MuxRecoveryImage>(wire).is_err());
    }

    #[test]
    fn domain_authority_binds_empty_domains_default_and_command_policy() {
        let mut image = make_valid_test_image();
        let commands = RecoveryLocalDomainPolicy {
            default_prog: Some(vec!["/bin/sh".into(), "-l".into()]),
            default_cwd: Some("/var/tmp".into()),
            environment: std::collections::BTreeMap::from([("LANG".into(), "C.UTF-8".into())]),
            term: "xterm-256color".into(),
        };
        image.topology.domains.push(RecoveryDomain {
            incarnation_domain_id: 77,
            domain_name: "empty-guardian".into(),
            is_attached: true,
            recovery_policy: Some(RecoveryDomainPolicy::GuardianLocal {
                commands,
                socket_path: "/var/tmp/g.sock".into(),
                token_path: "/var/tmp/g.token".into(),
            }),
        });
        image
            .topology
            .domain_state
            .as_mut()
            .unwrap()
            .default_domain_id = Some(77);
        image.image_digest = image.compute_digest().unwrap();
        image.validate().unwrap();
        let reopened =
            MuxRecoveryImage::from_json_slice(&image.to_canonical_json().unwrap()).unwrap();
        assert_eq!(reopened.topology, image.topology);
        let mut changed = image.clone();
        changed
            .topology
            .domain_state
            .as_mut()
            .unwrap()
            .default_domain_id = Some(0);
        assert_ne!(changed.compute_digest().unwrap(), image.image_digest);
        assert!(changed.validate().is_err());
        for case in 0..10 {
            let mut invalid = image.clone();
            let RecoveryDomainPolicy::GuardianLocal {
                commands,
                socket_path,
                ..
            } = invalid.topology.domains[1]
                .recovery_policy
                .as_mut()
                .unwrap()
            else {
                unreachable!()
            };
            match case {
                0 => commands.default_prog = Some(vec![]),
                1 => commands.default_cwd = Some("bad\0cwd".into()),
                2 => {
                    commands.environment.insert("bad=key".into(), "x".into());
                }
                3 => commands.term = "x".repeat(65_537),
                4 => *socket_path = "relative/socket".into(),
                5 => {
                    invalid
                        .topology
                        .domain_state
                        .as_mut()
                        .unwrap()
                        .default_domain_id = Some(999)
                }
                6 => invalid.topology.domains.swap(0, 1),
                7 => invalid.topology.domains[1].is_attached = false,
                8 => invalid.topology.domains[1].incarnation_domain_id = usize::MAX,
                _ => invalid.topology.domains[1].domain_name = "bad\0domain".into(),
            }
            if let Ok(digest) = invalid.compute_digest() {
                invalid.image_digest = digest;
            }
            assert!(invalid.validate().is_err(), "case {case}");
        }
    }

    #[test]
    fn tab_runtime_state_requires_schema_six_and_preserves_historical_bytes() {
        let image = make_valid_test_image();
        image.require_live_topology_state().unwrap();
        let mut missing = image.clone();
        missing.topology.windows[0].tabs[0].runtime_state = None;
        assert!(matches!(
            missing.validate(),
            Err(MuxRecoveryImageError::InvalidTabRuntimeState(_))
        ));
        for version in [3, 4, 5] {
            let mut historical = missing.clone();
            historical.header.schema_version = version;
            historical.topology.domain_state = None;
            for domain in &mut historical.topology.domains {
                domain.recovery_policy = None;
            }
            if version < 5 {
                historical.topology.workspace_state = None;
            }
            historical.image_digest = historical.compute_digest().unwrap();
            historical.validate().unwrap();
            let bytes = historical.to_canonical_json().unwrap();
            assert!(
                !std::str::from_utf8(&bytes)
                    .unwrap()
                    .contains("runtime_state")
            );
            let reopened = MuxRecoveryImage::from_json_slice(&bytes).unwrap();
            assert_eq!(reopened.to_canonical_json().unwrap(), bytes);
            assert!(reopened.require_live_topology_state().is_err());
            historical.topology.windows[0].tabs[0].runtime_state = Some(Default::default());
            assert!(matches!(
                historical.validate(),
                Err(MuxRecoveryImageError::InvalidTabRuntimeState(_))
            ));
        }
        for field in [
            "recency_count",
            "recency_by_index",
            "layout_cycle",
            "constraint_overrides",
            "collapsed_panes",
        ] {
            let mut value = serde_json::to_value(&image).unwrap();
            value["topology"]["windows"][0]["tabs"][0]["runtime_state"]
                .as_object_mut()
                .unwrap()
                .remove(field);
            assert!(
                serde_json::from_value::<MuxRecoveryImage>(value).is_err(),
                "missing {field}"
            );
        }
    }

    #[test]
    fn tab_runtime_state_authenticates_policy_and_rejects_invalid_entries() {
        let mut image = make_valid_test_image();
        let state = RecoveryTabRuntime {
            recency_count: 12,
            // Stale indices are legitimate: removal does not renumber recency.
            recency_by_index: vec![(0, 11), (9000, 2)],
            constraint_overrides: vec![(
                1,
                RecoveryPaneConstraints {
                    min_width: 5,
                    min_height: 3,
                    max_width: Some(80),
                    max_height: None,
                    preferred_width: Some(40),
                    preferred_height: None,
                    fixed: false,
                },
            )],
            collapsed_panes: vec![1],
            layout_cycle: Some(RecoveryLayoutCycle {
                current_index: 1,
                layouts: vec![
                    RecoverySwapLayout {
                        name: "one".into(),
                        description: None,
                        arrangement: RecoveryLayoutArrangement::Slot { is_main: true },
                    },
                    RecoverySwapLayout {
                        name: "two".into(),
                        description: Some("split".into()),
                        arrangement: RecoveryLayoutArrangement::Split {
                            direction: SplitDirection::Horizontal,
                            ratio_bits: (-0.0f64).to_bits(),
                            first: Box::new(RecoveryLayoutArrangement::Slot { is_main: false }),
                            second: Box::new(RecoveryLayoutArrangement::Slot { is_main: true }),
                        },
                    },
                ],
            }),
        };
        image.topology.windows[0].tabs[0].runtime_state = Some(state.clone());
        image.image_digest = image.compute_digest().unwrap();
        image.require_live_topology_state().unwrap();
        let reopened =
            MuxRecoveryImage::from_json_slice(&image.to_canonical_json().unwrap()).unwrap();
        assert_eq!(
            reopened.topology.windows[0].tabs[0].runtime_state,
            Some(state.clone())
        );
        let mut tampered = image.clone();
        tampered.topology.windows[0].tabs[0]
            .runtime_state
            .as_mut()
            .unwrap()
            .recency_count += 1;
        assert!(matches!(
            tampered.validate(),
            Err(MuxRecoveryImageError::DigestMismatch { .. })
        ));
        for case in 0..11 {
            let mut invalid = state.clone();
            match case {
                0 => invalid.recency_by_index.push((0, 1)),
                1 => invalid.recency_by_index[0].1 = 13,
                2 => invalid.constraint_overrides[0].0 = 999,
                3 => invalid.constraint_overrides[0].1.min_width = 0,
                4 => invalid.constraint_overrides[0].1.preferred_width = Some(81),
                5 => invalid.collapsed_panes.push(1),
                6 => invalid.collapsed_panes = vec![999],
                7 => invalid.layout_cycle.as_mut().unwrap().current_index = 2,
                8 => invalid.layout_cycle.as_mut().unwrap().layouts.clear(),
                9 => invalid.layout_cycle.as_mut().unwrap().layouts[0].name = "x".repeat(1025),
                _ => {
                    if let RecoveryLayoutArrangement::Split { ratio_bits, .. } =
                        &mut invalid.layout_cycle.as_mut().unwrap().layouts[1].arrangement
                    {
                        *ratio_bits = f64::NAN.to_bits();
                    }
                }
            }
            assert!(
                invalid
                    .validate_bounds()
                    .and_then(|()| invalid.validate_membership(&[1]))
                    .is_err(),
                "case {case}"
            );
        }
    }

    #[test]
    fn workspace_state_requires_explicit_current_authority_and_preserves_history() {
        let image = make_valid_test_image();
        image.require_live_workspace_state().unwrap();
        let mut missing = image.clone();
        missing.topology.workspace_state = None;
        assert!(matches!(
            missing.validate(),
            Err(MuxRecoveryImageError::InvalidWorkspaceState(_))
        ));
        for version in [3, 4] {
            let mut historical = missing.clone();
            historical.header.schema_version = version;
            historical.topology.domain_state = None;
            for domain in &mut historical.topology.domains {
                domain.recovery_policy = None;
            }
            for tab in historical
                .topology
                .windows
                .iter_mut()
                .flat_map(|window| &mut window.tabs)
            {
                tab.runtime_state = None;
            }
            historical.image_digest = historical.compute_digest().unwrap();
            historical.validate().unwrap();
            let bytes = historical.to_canonical_json().unwrap();
            assert!(
                !std::str::from_utf8(&bytes)
                    .unwrap()
                    .contains("workspace_state")
            );
            let reopened = MuxRecoveryImage::from_json_slice(&bytes).unwrap();
            assert_eq!(reopened.to_canonical_json().unwrap(), bytes);
            assert!(matches!(
                reopened.require_live_workspace_state(),
                Err(MuxRecoveryImageError::InvalidWorkspaceState(_))
            ));
            historical.topology.workspace_state = image.topology.workspace_state.clone();
            assert!(matches!(
                historical.validate(),
                Err(MuxRecoveryImageError::InvalidWorkspaceState(_))
            ));
        }
        for field in ["default_workspace", "workspaces"] {
            let mut value = serde_json::to_value(&image).unwrap();
            value["topology"]["workspace_state"]
                .as_object_mut()
                .unwrap()
                .remove(field);
            assert!(serde_json::from_value::<MuxRecoveryImage>(value).is_err());
        }
        let mut value = serde_json::to_value(&image).unwrap();
        value["topology"]["workspace_state"]["workspaces"][0]
            .as_object_mut()
            .unwrap()
            .remove("active_window_id");
        assert!(serde_json::from_value::<MuxRecoveryImage>(value).is_err());
    }

    #[test]
    fn workspace_state_membership_and_digest_reject_tampering() {
        for fault in 0..5 {
            let mut image = make_valid_test_image();
            let state = image.topology.workspace_state.as_mut().unwrap();
            match fault {
                0 => state.workspaces[0].active_window_id = Some(999),
                1 => state.workspaces[0].window_ids.push(100),
                2 => state.workspaces[0].window_ids.clear(),
                3 => state.workspaces[0].name = "wrong-workspace".into(),
                _ => state.workspaces.push(state.workspaces[0].clone()),
            }
            image.image_digest = image.compute_digest().unwrap();
            assert!(
                matches!(
                    image.validate(),
                    Err(MuxRecoveryImageError::InvalidWorkspaceState(_))
                ),
                "fault={fault}"
            );
        }
        let mut image = make_valid_test_image();
        image
            .topology
            .workspace_state
            .as_mut()
            .unwrap()
            .default_workspace = "changed".into();
        assert!(matches!(
            image.validate(),
            Err(MuxRecoveryImageError::DigestMismatch { .. })
        ));
        image
            .topology
            .workspace_state
            .as_mut()
            .unwrap()
            .default_workspace = "x".repeat(MAX_STRING_BYTES + 1);
        assert!(matches!(
            image.validate_bounds(),
            Err(MuxRecoveryImageError::StringTooLong {
                field: "workspace.default",
                ..
            })
        ));
    }

    #[test]
    fn test_positive_full_roundtrip_preserves_topology_and_digest() {
        let image = make_valid_test_image();
        assert!(image.validate().is_ok());

        let json_bytes = image.to_canonical_json().unwrap();
        let decoded = MuxRecoveryImage::from_json_slice(&json_bytes).unwrap();

        assert_eq!(image, decoded);
        assert_eq!(decoded.pane_count(), 3);

        let refs = decoded.pane_terminal_refs();
        assert_eq!(refs.len(), 3);
        assert_eq!(refs[0].pane_id, 1);
        assert_eq!(refs[0].stable_pane_uuid, "uuid-pane-1");
        assert_eq!(refs[0].terminal_checkpoint_object_id, "obj_1");
    }

    #[test]
    fn test_positive_lookup_helpers_and_leaves() {
        let image = make_valid_test_image();
        assert!(image.find_pane(1).is_some());
        assert!(image.find_pane(999).is_none());
        assert!(image.find_pane_by_uuid("uuid-pane-2").is_some());
        assert!(image.find_pane_by_uuid("nonexistent").is_none());

        assert!(image.find_window(100).is_some());
        assert!(
            image
                .find_window_by_stable_id("00000000-0000-0000-0000-000000000100")
                .is_some()
        );
        assert!(image.find_window(999).is_none());

        assert!(image.find_tab(10).is_some());
        assert!(
            image
                .find_tab_by_stable_id("00000000-0000-0000-0000-000000000010")
                .is_some()
        );
        assert!(image.find_tab(999).is_none());

        let tab = image.find_tab(10).unwrap();
        let split_leaves = tab.root_split.as_ref().unwrap().leaves();
        assert_eq!(split_leaves.len(), 2);
        assert_eq!(split_leaves[0], (1, "uuid-pane-1"));
        assert_eq!(split_leaves[1], (2, "uuid-pane-2"));

        let all_panes = tab.all_pane_ids();
        assert_eq!(all_panes, vec![1, 2, 3]);
    }

    #[test]
    fn test_positive_floating_pane_active_focus() {
        let mut image = make_valid_test_image();
        image.topology.windows[0].tabs[0].active_pane_id = 3;
        image.topology.windows[0].tabs[0].floating_focus = Some(3);
        image.image_digest = image.compute_digest().unwrap();

        assert!(image.validate().is_ok());
    }

    #[test]
    fn test_positive_guardian_authority() {
        let mut image = make_valid_test_image();
        image.panes[0].checkpoint.authority = CheckpointAuthority::Guardian {
            guardian_generation: 5,
            publication: test_guardian_publication(),
            catalog_generation: 3,
        };
        image.image_digest = image.compute_digest().unwrap();
        assert!(image.validate().is_ok());
    }

    // --- Paired Tests: Pane Stacks ---

    #[test]
    fn test_positive_tab_with_pane_stacks() {
        let mut image = make_valid_test_image();
        // Add pane 4 as hidden stack member sharing slot with pane 1
        let pane4 = make_test_pane(4, "uuid-pane-4", &image.header.mux_incarnation_id);
        image.panes.push(pane4);
        image.topology.windows[0].tabs[0]
            .pane_stacks
            .push(RecoveryPaneStack {
                slot_index: 0,
                pane_ids: vec![1, 4],
                active_index: 0,
            });
        image.image_digest = image.compute_digest().unwrap();

        assert!(image.validate().is_ok());
        let all_ids = image.topology.windows[0].tabs[0].all_pane_ids();
        assert_eq!(all_ids, vec![1, 2, 4, 3]);
    }

    #[test]
    fn test_negative_pane_stack_missing_active_in_tree() {
        let mut image = make_valid_test_image();
        // Pane stack active pane is 99, which is not in split tree (expected leaf at slot 0 is pane 1)
        image.topology.windows[0].tabs[0]
            .pane_stacks
            .push(RecoveryPaneStack {
                slot_index: 0,
                pane_ids: vec![99, 1],
                active_index: 0,
            });
        image.image_digest = image.compute_digest().unwrap();

        let err = image.validate().unwrap_err();
        assert_eq!(
            err,
            MuxRecoveryImageError::StackActiveMemberMismatch {
                tab_id: 10,
                slot_index: 0,
                expected_pane_id: 1,
                found_pane_id: 99,
            }
        );
    }

    #[test]
    fn test_negative_pane_stack_wrong_slot_active_member() {
        let mut image = make_valid_test_image();
        // Split tree has leaf 0 = pane 1, leaf 1 = pane 2.
        // Add pane 4 as hidden stack member, but associate stack with slot 1 (leaf 2)
        // while specifying pane 1 as active member.
        let pane4 = make_test_pane(4, "uuid-pane-4", &image.header.mux_incarnation_id);
        image.panes.push(pane4);
        image.topology.windows[0].tabs[0]
            .pane_stacks
            .push(RecoveryPaneStack {
                slot_index: 1,
                pane_ids: vec![1, 4],
                active_index: 0,
            });
        image.image_digest = image.compute_digest().unwrap();

        let err = image.validate().unwrap_err();
        assert_eq!(
            err,
            MuxRecoveryImageError::StackActiveMemberMismatch {
                tab_id: 10,
                slot_index: 1,
                expected_pane_id: 2,
                found_pane_id: 1,
            }
        );
    }

    #[test]
    fn test_negative_pane_stack_slot_out_of_bounds() {
        let mut image = make_valid_test_image();
        // Split tree has 2 leaves (indices 0 and 1). Slot 5 is out of bounds.
        let pane4 = make_test_pane(4, "uuid-pane-4", &image.header.mux_incarnation_id);
        image.panes.push(pane4);
        image.topology.windows[0].tabs[0]
            .pane_stacks
            .push(RecoveryPaneStack {
                slot_index: 5,
                pane_ids: vec![1, 4],
                active_index: 0,
            });
        image.image_digest = image.compute_digest().unwrap();

        let err = image.validate().unwrap_err();
        assert_eq!(
            err,
            MuxRecoveryImageError::StackSlotOutOfBounds {
                tab_id: 10,
                slot_index: 5,
                leaf_count: 2,
            }
        );
    }

    #[test]
    fn test_negative_pane_stack_duplicate_slot() {
        let mut image = make_valid_test_image();
        let pane4 = make_test_pane(4, "uuid-pane-4", &image.header.mux_incarnation_id);
        let pane5 = make_test_pane(5, "uuid-pane-5", &image.header.mux_incarnation_id);
        image.panes.push(pane4);
        image.panes.push(pane5);
        image.topology.windows[0].tabs[0]
            .pane_stacks
            .push(RecoveryPaneStack {
                slot_index: 0,
                pane_ids: vec![1, 4],
                active_index: 0,
            });
        image.topology.windows[0].tabs[0]
            .pane_stacks
            .push(RecoveryPaneStack {
                slot_index: 0,
                pane_ids: vec![1, 5],
                active_index: 0,
            });
        image.image_digest = image.compute_digest().unwrap();

        let err = image.validate().unwrap_err();
        assert_eq!(
            err,
            MuxRecoveryImageError::DuplicateStackSlot {
                tab_id: 10,
                slot_index: 0,
            }
        );
    }

    #[test]
    fn test_negative_pane_stack_floating_pane_as_active_member() {
        let mut image = make_valid_test_image();
        // Pane 3 is a floating pane, not a tiled split leaf
        let pane4 = make_test_pane(4, "uuid-pane-4", &image.header.mux_incarnation_id);
        image.panes.push(pane4);
        image.topology.windows[0].tabs[0]
            .pane_stacks
            .push(RecoveryPaneStack {
                slot_index: 0,
                pane_ids: vec![3, 4],
                active_index: 0,
            });
        image.image_digest = image.compute_digest().unwrap();

        let err = image.validate().unwrap_err();
        assert_eq!(
            err,
            MuxRecoveryImageError::StackActiveMemberMismatch {
                tab_id: 10,
                slot_index: 0,
                expected_pane_id: 1,
                found_pane_id: 3,
            }
        );
    }

    #[test]
    fn test_positive_multi_slot_pane_stacks() {
        let mut image = make_valid_test_image();
        // Slot 0 has leaf 1, stacked with hidden pane 4
        // Slot 1 has leaf 2, stacked with hidden pane 5
        let pane4 = make_test_pane(4, "uuid-pane-4", &image.header.mux_incarnation_id);
        let pane5 = make_test_pane(5, "uuid-pane-5", &image.header.mux_incarnation_id);
        image.panes.push(pane4);
        image.panes.push(pane5);
        image.topology.windows[0].tabs[0]
            .pane_stacks
            .push(RecoveryPaneStack {
                slot_index: 0,
                pane_ids: vec![1, 4],
                active_index: 0,
            });
        image.topology.windows[0].tabs[0]
            .pane_stacks
            .push(RecoveryPaneStack {
                slot_index: 1,
                pane_ids: vec![2, 5],
                active_index: 0,
            });
        image.image_digest = image.compute_digest().unwrap();

        assert!(image.validate().is_ok());
        let all_ids = image.topology.windows[0].tabs[0].all_pane_ids();
        assert_eq!(all_ids, vec![1, 2, 4, 5, 3]);
    }

    #[test]
    fn test_negative_pane_stack_hidden_pane_also_in_tree() {
        let mut image = make_valid_test_image();
        // Hidden stack pane is pane 2, which already occupies a split leaf
        image.topology.windows[0].tabs[0]
            .pane_stacks
            .push(RecoveryPaneStack {
                slot_index: 0,
                pane_ids: vec![1, 2],
                active_index: 0,
            });
        image.image_digest = image.compute_digest().unwrap();

        let err = image.validate().unwrap_err();
        assert_eq!(err, MuxRecoveryImageError::DuplicatePanePlacement(2));
    }

    // --- Paired Tests: UUID Matching ---

    #[test]
    fn test_positive_near_identical_placed_uuid_match() {
        let image = make_valid_test_image();
        assert!(image.validate().is_ok());
    }

    #[test]
    fn test_negative_placed_leaf_uuid_mismatch_vs_catalog() {
        let mut image = make_valid_test_image();
        if let Some(RecoverySplitNode::Split { ref mut left, .. }) =
            image.topology.windows[0].tabs[0].root_split
        {
            if let RecoverySplitNode::Leaf {
                ref mut pane_uuid, ..
            } = **left
            {
                *pane_uuid = "uuid-pane-2".to_string();
            }
        }
        image.image_digest = image.compute_digest().unwrap();

        let err = image.validate().unwrap_err();
        assert_eq!(
            err,
            MuxRecoveryImageError::PaneUuidMismatchCatalog {
                pane_id: 1,
                catalog_uuid: "uuid-pane-1".to_string(),
                placed_uuid: "uuid-pane-2".to_string(),
            }
        );
    }

    #[test]
    fn test_negative_placed_floating_uuid_mismatch_vs_catalog() {
        let mut image = make_valid_test_image();
        image.topology.windows[0].tabs[0].floating_panes[0].pane_uuid = "uuid-swapped".to_string();
        image.image_digest = image.compute_digest().unwrap();

        let err = image.validate().unwrap_err();
        assert_eq!(
            err,
            MuxRecoveryImageError::PaneUuidMismatchCatalog {
                pane_id: 3,
                catalog_uuid: "uuid-pane-3".to_string(),
                placed_uuid: "uuid-swapped".to_string(),
            }
        );
    }

    // --- Paired Tests: Global Placement Bijection ---

    #[test]
    fn test_positive_near_identical_exact_placement() {
        let image = make_valid_test_image();
        assert!(image.validate().is_ok());
    }

    #[test]
    fn test_negative_duplicate_pane_placement_across_tabs() {
        let mut image = make_valid_test_image();
        let mut dup_tab = image.topology.windows[0].tabs[0].clone();
        dup_tab.tab_id = 11;
        dup_tab.stable_tab_id = "00000000-0000-0000-0000-000000000011".to_string();
        dup_tab.floating_panes.clear();
        dup_tab.floating_focus = None;
        dup_tab.pane_stacks.clear();
        dup_tab.root_split = Some(RecoverySplitNode::Leaf {
            pane_id: 1,
            pane_uuid: "uuid-pane-1".to_string(),
        });
        dup_tab.active_pane_id = 1;
        image.topology.windows[0].tabs.push(dup_tab);
        image.image_digest = image.compute_digest().unwrap();

        let err = image.validate().unwrap_err();
        assert_eq!(err, MuxRecoveryImageError::DuplicatePanePlacement(1));
    }

    #[test]
    fn test_negative_duplicate_pane_placement_within_same_tab() {
        let mut image = make_valid_test_image();
        image.topology.windows[0].tabs[0]
            .floating_panes
            .push(RecoveryFloatingPane::new(
                1,
                "uuid-pane-1".to_string(),
                FloatingPaneRect {
                    left: 0,
                    top: 0,
                    width: 20,
                    height: 10,
                },
                2,
                true,
                false,
                1.0,
            ));
        image.image_digest = image.compute_digest().unwrap();

        let err = image.validate().unwrap_err();
        assert_eq!(err, MuxRecoveryImageError::DuplicatePanePlacement(1));
    }

    #[test]
    fn test_negative_orphan_catalog_pane_unplaced() {
        let mut image = make_valid_test_image();
        let pane4 = make_test_pane(4, "uuid-pane-4", &image.header.mux_incarnation_id);
        image.panes.push(pane4);
        image.image_digest = image.compute_digest().unwrap();

        let err = image.validate().unwrap_err();
        assert_eq!(err, MuxRecoveryImageError::OrphanCatalogPane(4));
    }

    // --- Paired Tests: Registration & Authority Fields ---

    #[test]
    fn test_negative_registration_wire_identity_zero() {
        let mut image = make_valid_test_image();
        image.panes[0].checkpoint.registration_wire_identity = [0; 16];
        image.image_digest = image.compute_digest().unwrap();

        let err = image.validate().unwrap_err();
        assert_eq!(err, MuxRecoveryImageError::ZeroRegistrationWireIdentity(1));
    }

    #[test]
    fn test_negative_checkpoint_byte_length_zero() {
        let mut image = make_valid_test_image();
        image.panes[0].checkpoint.checkpoint_ref.byte_length = 0;
        image.image_digest = image.compute_digest().unwrap();

        let err = image.validate().unwrap_err();
        assert!(matches!(
            err,
            MuxRecoveryImageError::InvalidCheckpointRef {
                pane_id: 1,
                reason: "byte_length must be greater than zero"
            }
        ));
    }

    #[test]
    fn test_negative_checkpoint_digest_all_zeros() {
        let mut image = make_valid_test_image();
        image.panes[0].checkpoint.checkpoint_ref.payload_digest = [0u8; 32];
        image.image_digest = image.compute_digest().unwrap();

        let err = image.validate().unwrap_err();
        assert!(matches!(
            err,
            MuxRecoveryImageError::InvalidCheckpointRef {
                pane_id: 1,
                reason: "payload_digest must not be all zeros"
            }
        ));
    }

    #[test]
    fn test_negative_checkpoint_schema_version_zero() {
        let mut image = make_valid_test_image();
        image.panes[0].checkpoint.checkpoint_ref.schema_version = 0;
        image.image_digest = image.compute_digest().unwrap();

        let err = image.validate().unwrap_err();
        assert!(matches!(
            err,
            MuxRecoveryImageError::InvalidCheckpointRef {
                pane_id: 1,
                reason: "schema_version must be non-zero"
            }
        ));
    }

    #[test]
    fn test_negative_model_only_zero_timestamp() {
        let mut image = make_valid_test_image();
        image.panes[0].checkpoint.authority = CheckpointAuthority::ModelOnly {
            captured_at_epoch_ms: 0,
            parser_seqno: Some(42),
        };
        image.image_digest = image.compute_digest().unwrap();

        let err = image.validate().unwrap_err();
        assert!(matches!(
            err,
            MuxRecoveryImageError::InvalidAuthority {
                pane_id: 1,
                reason: "captured_at_epoch_ms must be greater than zero"
            }
        ));
    }

    #[test]
    fn test_model_only_zero_seqno_and_empty_stream() {
        let mut image = make_valid_test_image();
        image.panes[0].checkpoint.parser_capture.parser_seqno = Some(0);
        image.panes[0].checkpoint.parser_capture.watermark_bytes = 0;
        image.panes[0].checkpoint.authority = CheckpointAuthority::ModelOnly {
            captured_at_epoch_ms: 1747371642000,
            parser_seqno: Some(0),
        };
        image.image_digest = image.compute_digest().unwrap();

        image.validate().unwrap();
    }

    #[test]
    fn test_negative_model_only_parser_seqno_mismatch() {
        let mut image = make_valid_test_image();
        image.panes[0].checkpoint.parser_capture.parser_seqno = Some(99);
        image.panes[0].checkpoint.authority = CheckpointAuthority::ModelOnly {
            captured_at_epoch_ms: 1747371642000,
            parser_seqno: Some(42),
        };
        image.image_digest = image.compute_digest().unwrap();

        let err = image.validate().unwrap_err();
        assert!(matches!(
            err,
            MuxRecoveryImageError::InvalidAuthority {
                pane_id: 1,
                reason: "parser_capture.parser_seqno does not match model authority parser_seqno"
            }
        ));
    }

    #[test]
    fn test_negative_guardian_authority_zero_generation() {
        let mut image = make_valid_test_image();
        image.panes[0].checkpoint.authority = CheckpointAuthority::Guardian {
            guardian_generation: 0,
            publication: test_guardian_publication(),
            catalog_generation: 1,
        };
        image.image_digest = image.compute_digest().unwrap();

        let err = image.validate().unwrap_err();
        assert!(matches!(
            err,
            MuxRecoveryImageError::InvalidAuthority {
                pane_id: 1,
                reason: "guardian_generation must be greater than zero"
            }
        ));
    }

    #[test]
    fn test_negative_guardian_authority_zero_publication_identity() {
        let mut image = make_valid_test_image();
        image.panes[0].checkpoint.authority = CheckpointAuthority::Guardian {
            guardian_generation: 1,
            publication: RecoveryGuardianPublication {
                checkpoint_identity: [0; 32],
                ..test_guardian_publication()
            },
            catalog_generation: 1,
        };
        image.image_digest = image.compute_digest().unwrap();

        let err = image.validate().unwrap_err();
        assert!(matches!(
            err,
            MuxRecoveryImageError::InvalidAuthority {
                pane_id: 1,
                reason: "published guardian receipt identities must be nonzero"
            }
        ));
    }

    // --- Paired Tests: Split Tree Depth Limit ---

    #[test]
    fn test_positive_near_identical_tree_depth_at_limit() {
        let incarnation_id = "inc-20260914-1800";
        let mut panes = Vec::new();
        for id in 1..=6 {
            panes.push(make_test_pane(
                id,
                &format!("uuid-pane-{id}"),
                incarnation_id,
            ));
        }

        let mut node = RecoverySplitNode::Leaf {
            pane_id: 1,
            pane_uuid: "uuid-pane-1".to_string(),
        };
        for id in 2..=6 {
            node = RecoverySplitNode::Split {
                split: SplitDirectionAndSize {
                    direction: SplitDirection::Vertical,
                    first: TerminalSize {
                        rows: 24,
                        cols: 80,
                        pixel_width: 800,
                        pixel_height: 480,
                        dpi: 96,
                    },
                    second: TerminalSize {
                        rows: 24,
                        cols: 80,
                        pixel_width: 800,
                        pixel_height: 480,
                        dpi: 96,
                    },
                },
                left: Box::new(node),
                right: Box::new(RecoverySplitNode::Leaf {
                    pane_id: id,
                    pane_uuid: format!("uuid-pane-{id}"),
                }),
            };
        }

        let tab = RecoveryTab {
            tab_id: 10,
            stable_tab_id: "00000000-0000-0000-0000-000000000010".to_string(),
            title: "tree_tab".to_string(),
            working_dir: None,
            size: TerminalSize::default(),
            size_before_zoom: TerminalSize::default(),
            zoomed_pane_id: None,
            root_split: Some(node),
            floating_panes: vec![],
            floating_focus: None,
            active_pane_id: 1,
            pane_stacks: vec![],
            underlying_tiled_active_pane_id: Some(1),
            runtime_state: Some(Default::default()),
        };

        let mut image = MuxRecoveryImage {
            header: RecoveryImageHeader {
                magic: MUX_RECOVERY_IMAGE_MAGIC,
                schema_version: MUX_RECOVERY_IMAGE_SCHEMA_VERSION,
                generation: 1,
                predecessor_digest: None,
                created_at_epoch_ms: 1747371642000,
                mux_incarnation_id: incarnation_id.to_string(),
                ft_version: "0.2.0".to_string(),
                session_id: "sess".to_string(),
            },
            topology: RecoveryTopology {
                domains: vec![RecoveryDomain {
                    incarnation_domain_id: 0,
                    domain_name: "local".to_string(),
                    is_attached: true,
                    recovery_policy: Some(RecoveryDomainPolicy::local_test_fixture()),
                }],
                domain_state: Some(Default::default()),
                windows: vec![RecoveryWindow {
                    window_id: 100,
                    stable_window_id: "00000000-0000-0000-0000-000000000100".to_string(),
                    workspace: "default".to_string(),
                    title: String::new(),
                    last_active_tab_id: None,
                    tab_stacks: vec![],
                    order_revision: 1,
                    gui_position: None,
                    tabs: vec![tab],
                    active_tab_index: 0,
                }],
                focused_window_id: Some(100),
                client_workspace: None,
                workspace_state: Some(RecoveryWorkspaceState {
                    default_workspace: "default".into(),
                    workspaces: vec![RecoveryWorkspace {
                        name: "default".into(),
                        window_ids: vec![100],
                        active_window_id: Some(100),
                    }],
                }),
            },
            panes,
            image_digest: [0u8; 32],
        };

        let digest = image.compute_digest().unwrap();
        image.image_digest = digest;
        assert!(image.validate().is_ok());
    }

    #[test]
    fn test_negative_split_tree_depth_exceeded() {
        let mut image = make_valid_test_image();
        let mut node = RecoverySplitNode::Leaf {
            pane_id: 1,
            pane_uuid: "uuid-pane-1".to_string(),
        };
        for _ in 0..35 {
            node = RecoverySplitNode::Split {
                split: SplitDirectionAndSize {
                    direction: SplitDirection::Vertical,
                    first: TerminalSize {
                        rows: 24,
                        cols: 80,
                        pixel_width: 800,
                        pixel_height: 480,
                        dpi: 96,
                    },
                    second: TerminalSize {
                        rows: 24,
                        cols: 80,
                        pixel_width: 800,
                        pixel_height: 480,
                        dpi: 96,
                    },
                },
                left: Box::new(node),
                right: Box::new(RecoverySplitNode::Leaf {
                    pane_id: 2,
                    pane_uuid: "uuid-pane-2".to_string(),
                }),
            };
        }
        image.topology.windows[0].tabs[0].root_split = Some(node);

        let err_digest = image.compute_digest().unwrap_err();
        assert!(matches!(err_digest, MuxRecoveryImageError::TooDeep { .. }));

        let err_validate = image.validate().unwrap_err();
        assert!(matches!(
            err_validate,
            MuxRecoveryImageError::TooDeep { .. }
        ));
    }

    // --- Paired Tests: Oversize Limits & Bounded Writers ---

    #[test]
    fn test_negative_oversized_json_slice_rejected() {
        let dummy = vec![b' '; MAX_RECOVERY_IMAGE_BYTES + 1];
        let err = MuxRecoveryImage::from_json_slice(&dummy).unwrap_err();
        assert!(matches!(err, MuxRecoveryImageError::TooLarge { .. }));
    }

    #[test]
    fn test_negative_bounded_writer_rejects_overflow() {
        let mut buf = Vec::new();
        let mut writer = BoundedWriter::new(&mut buf, 10);
        assert!(writer.write_all(b"12345").is_ok());
        assert_eq!(writer.written(), 5);
        assert!(!writer.exceeded());

        let overflow_err = writer.write_all(b"123456");
        assert!(overflow_err.is_err());
        assert!(writer.exceeded());
    }

    #[test]
    fn test_to_canonical_json_validates_first() {
        let mut image = make_valid_test_image();
        image.header.schema_version = 999;
        assert!(matches!(
            image.to_canonical_json().unwrap_err(),
            MuxRecoveryImageError::UnsupportedSchemaVersion(999)
        ));
    }

    // --- Structural Validation Tests ---

    #[test]
    fn test_negative_duplicate_window_id() {
        let mut image = make_valid_test_image();
        let mut dup_win = image.topology.windows[0].clone();
        dup_win.stable_window_id = "00000000-0000-0000-0000-000000000101".to_string();
        image.topology.windows.push(dup_win);
        image.image_digest = image.compute_digest().unwrap();

        let err = image.validate().unwrap_err();
        assert_eq!(err, MuxRecoveryImageError::DuplicateWindowId(100));
    }

    #[test]
    fn test_negative_duplicate_tab_id() {
        let mut image = make_valid_test_image();
        let mut dup_tab = image.topology.windows[0].tabs[0].clone();
        dup_tab.stable_tab_id = "00000000-0000-0000-0000-000000000011".to_string();
        dup_tab.root_split = None;
        dup_tab.floating_panes.clear();
        dup_tab.pane_stacks.clear();
        image.topology.windows[0].tabs.push(dup_tab);
        image.image_digest = image.compute_digest().unwrap();

        let err = image.validate().unwrap_err();
        assert_eq!(err, MuxRecoveryImageError::DuplicateTabId(10));
    }

    #[test]
    fn test_negative_duplicate_pane_id() {
        let mut image = make_valid_test_image();
        let mut dup_pane = image.panes[0].clone();
        dup_pane.pane_uuid = "uuid-pane-other".to_string();
        dup_pane.checkpoint.pane_uuid = "uuid-pane-other".to_string();
        image.panes.push(dup_pane);
        image.image_digest = image.compute_digest().unwrap();

        let err = image.validate().unwrap_err();
        assert_eq!(err, MuxRecoveryImageError::DuplicatePaneId(1));
    }

    #[test]
    fn test_negative_missing_pane_reference() {
        let mut image = make_valid_test_image();
        image.panes.retain(|p| p.pane_id != 2);
        image.image_digest = image.compute_digest().unwrap();

        let err = image.validate().unwrap_err();
        assert_eq!(err, MuxRecoveryImageError::MissingPane(2));
    }

    #[test]
    fn test_negative_missing_domain_reference() {
        let mut image = make_valid_test_image();
        image.panes[0].domain_name = "unknown_domain".to_string();
        image.image_digest = image.compute_digest().unwrap();

        let err = image.validate().unwrap_err();
        assert_eq!(
            err,
            MuxRecoveryImageError::MissingDomain("unknown_domain".to_string())
        );
    }

    #[test]
    fn test_negative_invalid_active_tab_index() {
        let mut image = make_valid_test_image();
        image.topology.windows[0].active_tab_index = 5;
        image.image_digest = image.compute_digest().unwrap();

        let err = image.validate().unwrap_err();
        assert!(matches!(
            err,
            MuxRecoveryImageError::InvalidActiveTabIndex { .. }
        ));
    }

    #[test]
    fn test_negative_invalid_active_pane_id() {
        let mut image = make_valid_test_image();
        image.topology.windows[0].tabs[0].active_pane_id = 999;
        image.image_digest = image.compute_digest().unwrap();

        let err = image.validate().unwrap_err();
        assert!(matches!(
            err,
            MuxRecoveryImageError::InvalidActivePaneId { .. }
        ));
    }

    #[test]
    fn test_negative_digest_mismatch() {
        let mut image = make_valid_test_image();
        image.image_digest = [99u8; 32];

        let err = image.validate().unwrap_err();
        assert!(matches!(err, MuxRecoveryImageError::DigestMismatch { .. }));
    }

    #[test]
    fn test_negative_incarnation_mismatch_in_checkpoint_binding() {
        let mut image = make_valid_test_image();
        image.panes[0].checkpoint.topology_incarnation_id = "inc-foreign-old".to_string();
        image.image_digest = image.compute_digest().unwrap();

        let err = image.validate().unwrap_err();
        assert!(matches!(
            err,
            MuxRecoveryImageError::IncarnationMismatch { .. }
        ));
    }

    #[test]
    fn test_negative_pane_uuid_mismatch_in_checkpoint_binding() {
        let mut image = make_valid_test_image();
        image.panes[0].checkpoint.pane_uuid = "uuid-swapped-other".to_string();
        image.image_digest = image.compute_digest().unwrap();

        let err = image.validate().unwrap_err();
        assert!(matches!(
            err,
            MuxRecoveryImageError::PaneUuidMismatch { .. }
        ));
    }

    #[test]
    fn test_negative_invalid_magic_rejected() {
        let mut image = make_valid_test_image();
        image.header.magic = *b"BADM";
        assert!(matches!(
            image.compute_digest(),
            Err(MuxRecoveryImageError::InvalidMagic { .. })
        ));

        let err = image.validate().unwrap_err();
        assert!(matches!(err, MuxRecoveryImageError::InvalidMagic { .. }));
    }

    #[test]
    fn test_negative_unsupported_schema_version() {
        let mut image = make_valid_test_image();
        image.header.schema_version = 999;
        assert_eq!(
            image.compute_digest(),
            Err(MuxRecoveryImageError::UnsupportedSchemaVersion(999))
        );

        let err = image.validate().unwrap_err();
        assert_eq!(err, MuxRecoveryImageError::UnsupportedSchemaVersion(999));
    }

    #[test]
    fn test_negative_header_generation_zero() {
        let mut image = make_valid_test_image();
        image.header.generation = 0;
        image.image_digest = image.compute_digest().unwrap();

        let err = image.validate().unwrap_err();
        assert!(matches!(
            err,
            MuxRecoveryImageError::InvalidHeader("generation must be greater than zero")
        ));
    }

    #[test]
    fn test_recovery_tab_underlying_tiled_active_pane_id_serde_roundtrip() {
        let tab = RecoveryTab {
            tab_id: 1,
            stable_tab_id: "00000000-0000-0000-0000-000000000001".to_string(),
            title: "roundtrip".to_string(),
            working_dir: None,
            size: TerminalSize::default(),
            size_before_zoom: TerminalSize::default(),
            zoomed_pane_id: None,
            root_split: Some(RecoverySplitNode::Leaf {
                pane_id: 20,
                pane_uuid: "uuid-pane-20".to_string(),
            }),
            floating_panes: vec![RecoveryFloatingPane::new(
                99,
                "uuid-pane-99".to_string(),
                FloatingPaneRect {
                    left: 0,
                    top: 0,
                    width: 40,
                    height: 20,
                },
                1,
                true,
                false,
                1.0,
            )],
            floating_focus: Some(99),
            active_pane_id: 99,
            pane_stacks: vec![],
            underlying_tiled_active_pane_id: Some(20),
            runtime_state: Some(Default::default()),
        };

        // Standard serialization roundtrip
        let serialized = serde_json::to_string(&tab).unwrap();
        assert!(serialized.contains("\"underlying_tiled_active_pane_id\":20"));
        let deserialized: RecoveryTab = serde_json::from_str(&serialized).unwrap();
        assert_eq!(deserialized, tab);
        assert_eq!(deserialized.underlying_tiled_active_pane_id, Some(20));

        // When None, field is omitted from serialized JSON
        let mut tab_none = tab.clone();
        tab_none.underlying_tiled_active_pane_id = None;
        let serialized_none = serde_json::to_string(&tab_none).unwrap();
        assert!(!serialized_none.contains("underlying_tiled_active_pane_id"));
        let from_none: RecoveryTab = serde_json::from_str(&serialized_none).unwrap();
        assert_eq!(from_none.underlying_tiled_active_pane_id, None);
    }

    #[test]
    fn test_recovery_tab_underlying_tiled_active_pane_id_validation_rejects_nonexistent_pane() {
        let mut image = make_valid_test_image();
        // Pane 999 is not in the split tree leaves:
        image.topology.windows[0].tabs[0].underlying_tiled_active_pane_id = Some(999);
        let err = image.validate().unwrap_err();
        assert_eq!(
            err,
            MuxRecoveryImageError::InvalidUnderlyingTiledActivePaneId {
                tab_id: 10,
                pane_id: 999,
            }
        );
    }

    #[test]
    fn test_recovery_tab_underlying_tiled_active_pane_id_rejects_hidden_stack_member() {
        let mut image = make_valid_test_image();
        // Add pane 4 as hidden stack member sharing slot with pane 1
        let pane4 = make_test_pane(4, "uuid-pane-4", &image.header.mux_incarnation_id);
        image.panes.push(pane4);
        image.topology.windows[0].tabs[0]
            .pane_stacks
            .push(RecoveryPaneStack {
                slot_index: 0,
                pane_ids: vec![1, 4],
                active_index: 0,
            });
        // Pane 4 is a hidden stack member, not the carrier in the split tree.
        // raw_tree_active_pane only indexes into tree leaves, so hidden stack members must be rejected:
        image.topology.windows[0].tabs[0].underlying_tiled_active_pane_id = Some(4);
        let err = image.validate().unwrap_err();
        assert_eq!(
            err,
            MuxRecoveryImageError::InvalidUnderlyingTiledActivePaneId {
                tab_id: 10,
                pane_id: 4,
            }
        );
    }

    #[test]
    fn test_recovery_tab_rejects_contradictory_floating_focus_and_active_pane() {
        let mut image = make_valid_test_image();
        // Floating focus is set to floating pane 3, but active_pane_id claims tiled pane 1
        image.topology.windows[0].tabs[0].floating_focus = Some(3);
        image.topology.windows[0].tabs[0].active_pane_id = 1;
        let err = image.validate().unwrap_err();
        assert_eq!(
            err,
            MuxRecoveryImageError::ContradictoryActivePaneState {
                tab_id: 10,
                reason: "tab active pane id does not match floating focus pane id",
            }
        );
    }

    #[test]
    fn test_recovery_tab_rejects_floating_active_pane_when_floating_focus_absent() {
        let mut image = make_valid_test_image();
        // Floating focus is None, but active_pane_id is set to floating pane 3
        image.topology.windows[0].tabs[0].floating_focus = None;
        image.topology.windows[0].tabs[0].active_pane_id = 3;
        let err = image.validate().unwrap_err();
        assert_eq!(
            err,
            MuxRecoveryImageError::ContradictoryActivePaneState {
                tab_id: 10,
                reason: "tab active pane id is a floating pane but floating focus is absent",
            }
        );
    }

    #[test]
    fn test_recovery_tab_rejects_hidden_stack_active_pane_when_unzoomed_and_unfloating() {
        let mut image = make_valid_test_image();
        // Add pane 4 as hidden stack member sharing slot with pane 1
        let pane4 = make_test_pane(4, "uuid-pane-4", &image.header.mux_incarnation_id);
        image.panes.push(pane4);
        image.topology.windows[0].tabs[0]
            .pane_stacks
            .push(RecoveryPaneStack {
                slot_index: 0,
                pane_ids: vec![1, 4],
                active_index: 0,
            });
        // Pane 4 is a hidden stack member. Without zoom or floating focus, active pane MUST be a tree leaf:
        image.topology.windows[0].tabs[0].active_pane_id = 4;
        image.topology.windows[0].tabs[0].underlying_tiled_active_pane_id = None;
        let err = image.validate().unwrap_err();
        assert_eq!(
            err,
            MuxRecoveryImageError::ContradictoryActivePaneState {
                tab_id: 10,
                reason: "tab active pane id must be a split tree leaf when floating focus and zoom are absent",
            }
        );
    }

    #[test]
    fn test_recovery_tab_allows_hidden_stack_active_pane_when_zoomed() {
        let mut image = make_valid_test_image();
        let pane4 = make_test_pane(4, "uuid-pane-4", &image.header.mux_incarnation_id);
        image.panes.push(pane4);
        image.topology.windows[0].tabs[0]
            .pane_stacks
            .push(RecoveryPaneStack {
                slot_index: 0,
                pane_ids: vec![1, 4],
                active_index: 0,
            });
        // Pane 4 is a hidden stack member, but is explicitly zoomed:
        image.topology.windows[0].tabs[0].zoomed_pane_id = Some(4);
        image.topology.windows[0].tabs[0].active_pane_id = 4;
        image.topology.windows[0].tabs[0].underlying_tiled_active_pane_id = Some(1);
        image.image_digest = image.compute_digest().unwrap();
        assert!(image.validate().is_ok());
    }

    #[test]
    fn test_recovery_tab_allows_zoomed_pane_differing_from_underlying_tiled_active_pane() {
        let mut image = make_valid_test_image();
        // Pane 2 is zoomed (and therefore effective active), while pane 1 remains underlying tiled active:
        image.topology.windows[0].tabs[0].zoomed_pane_id = Some(2);
        image.topology.windows[0].tabs[0].active_pane_id = 2;
        image.topology.windows[0].tabs[0].underlying_tiled_active_pane_id = Some(1);
        image.image_digest = image.compute_digest().unwrap();
        assert!(image.validate().is_ok());
    }

    #[test]
    fn test_recovery_tab_rejects_contradictory_underlying_and_effective_active_pane() {
        let mut image = make_valid_test_image();
        // Floating focus is None, effective active is pane 1, but underlying tiled active claims pane 2
        image.topology.windows[0].tabs[0].floating_focus = None;
        image.topology.windows[0].tabs[0].active_pane_id = 1;
        image.topology.windows[0].tabs[0].underlying_tiled_active_pane_id = Some(2);
        let err = image.validate().unwrap_err();
        assert_eq!(
            err,
            MuxRecoveryImageError::ContradictoryActivePaneState {
                tab_id: 10,
                reason: "tab underlying tiled active pane id does not match active pane id when floating focus is absent",
            }
        );
    }

    #[test]
    fn test_recovery_tab_rejects_contradictory_zoomed_and_active_pane() {
        let mut image = make_valid_test_image();
        // Zoomed pane is pane 2, but active_pane_id claims pane 1
        image.topology.windows[0].tabs[0].zoomed_pane_id = Some(2);
        image.topology.windows[0].tabs[0].active_pane_id = 1;
        let err = image.validate().unwrap_err();
        assert_eq!(
            err,
            MuxRecoveryImageError::ContradictoryActivePaneState {
                tab_id: 10,
                reason: "tab active pane id does not match zoomed pane id",
            }
        );
    }
}

// =============================================================================
// Live Converter Integration & Causal Mismatch Tests (feature = "frankenterm-deps")
// =============================================================================

#[cfg(all(test, feature = "frankenterm-deps"))]
mod converter_tests {
    use super::*;
    use frankenterm_term::color::ColorPalette;
    use frankenterm_term::config::TerminalConfiguration;
    use frankenterm_term::terminalstate::checkpoint::TerminalCheckpointLimits;
    use frankenterm_term::{Terminal, TerminalSize as TermTerminalSize};
    use std::sync::Arc;

    #[test]
    fn converter_rejects_malformed_window_stack_entries_and_raw_history() {
        let entry = mux::tab::TabStackEntry {
            stack_id: mux::tab::TabStackId(4),
            tab_id: 20,
            position: 0,
            is_visible: true,
        };
        for entries in [
            vec![mux::tab::TabStackEntry {
                position: 1,
                ..entry.clone()
            }],
            vec![mux::tab::TabStackEntry {
                is_visible: false,
                ..entry.clone()
            }],
            vec![
                entry.clone(),
                mux::tab::TabStackEntry {
                    position: 1,
                    ..entry.clone()
                },
            ],
            vec![entry.clone(), entry.clone()],
            vec![mux::tab::TabStackEntry {
                tab_id: 999,
                ..entry.clone()
            }],
            vec![
                entry.clone(),
                mux::tab::TabStackEntry {
                    stack_id: mux::tab::TabStackId(3),
                    ..entry
                },
            ],
        ] {
            let (meta, mut captured, acks, refs) = make_test_fixture();
            captured.windows[0].tab_stacks = entries;
            assert!(matches!(
                MuxRecoveryImage::from_mux_captured(meta, &captured, &borrowed_acks(&acks), &refs),
                Err(MuxRecoveryImageError::InvalidCapturedTopology(_))
            ));
        }
        let (meta, mut captured, acks, refs) = make_test_fixture();
        captured.windows[0].last_active_tab_id = Some(999);
        assert_eq!(
            MuxRecoveryImage::from_mux_captured(meta, &captured, &borrowed_acks(&acks), &refs)
                .unwrap_err(),
            MuxRecoveryImageError::InvalidCapturedTopology(
                "last active tab is absent from its window"
            )
        );
    }

    fn borrowed_acks(
        acks: &HashMap<usize, mux::ModelParserCheckpointAck>,
    ) -> HashMap<usize, &mux::ModelParserCheckpointAck> {
        acks.iter().map(|(&pane_id, ack)| (pane_id, ack)).collect()
    }

    #[derive(Debug)]
    struct TestConfig;
    impl TerminalConfiguration for TestConfig {
        fn color_palette(&self) -> ColorPalette {
            ColorPalette::default()
        }
    }

    fn make_test_checkpoint(
        rows: usize,
        cols: usize,
        stream: &[u8],
    ) -> frankenterm_term::RecoveryTerminalCheckpointV3 {
        let size = TermTerminalSize {
            rows,
            cols,
            pixel_width: cols * 10,
            pixel_height: rows * 20,
            dpi: 96,
        };
        let mut term = Terminal::new(
            size,
            Arc::new(TestConfig),
            "FrankenTerm",
            "converter-test",
            Box::new(Vec::<u8>::new()),
        );
        term.advance_bytes(stream);
        term.capture_recovery_checkpoint(TerminalCheckpointLimits::default())
            .expect("capture test checkpoint")
    }

    fn make_test_ack(
        registration_wire_identity: [u8; 16],
        durable_pane_id: uuid::Uuid,
        terminal_checkpoint: frankenterm_term::RecoveryTerminalCheckpointV3,
    ) -> mux::ModelParserCheckpointAck {
        let semantic_generation = frankenterm_term::terminalstate::checkpoint::TerminalCheckpointV3::decode_canonical_json(
            terminal_checkpoint.canonical_payload(), TerminalCheckpointLimits::default(),
        ).unwrap().checkpoint().semantic_generation();
        mux::ModelParserCheckpointAck {
            registration_wire_identity,
            durable_pane_id,
            parser_stream_bytes: terminal_checkpoint.parser_stream_bytes(),
            semantic_generation,
            terminal_checkpoint,
        }
    }

    fn make_test_fixture() -> (
        RecoveryImageGenerationMeta,
        mux::MuxCapturedTopology,
        HashMap<usize, mux::ModelParserCheckpointAck>,
        HashMap<usize, RecoveryObjectRef>,
    ) {
        let meta = RecoveryImageGenerationMeta {
            generation: 1,
            predecessor_digest: None,
            created_at_epoch_ms: 1773500000000,
            ft_version: "0.2.0".to_string(),
            session_id: "sess-test".to_string(),
        };

        let incarnation_bytes = [0x5au8; 16];
        let session_incarnation = mux::MuxSessionIncarnation::from_bytes(incarnation_bytes);

        let wire1 = [1u8; 16];
        let wire2 = [2u8; 16];
        let uuid1 = *uuid::Uuid::new_v4().as_bytes();
        let uuid2 = *uuid::Uuid::new_v4().as_bytes();

        let binding1 = mux::MuxCapturedPaneBinding {
            pane_id: 101,
            pane_uuid: uuid::Uuid::from_bytes(uuid1).to_string(),
            spawn_custody: None,
            registration_wire_identity: wire1,
            domain_id: 1,
            domain_name: "local".to_string(),
            window_id: 10,
            tab_id: 20,
            lane: mux::MuxCapturedPaneLane::Tiled,
            title: "bash_101".to_string(),
            cwd: Some("/app".to_string()),
            size: frankenterm_term::TerminalSize {
                rows: 24,
                cols: 40,
                pixel_width: 400,
                pixel_height: 480,
                dpi: 96,
            },
            alt_screen_active: false,
            cursor_pos: (0, 0),
            is_active_in_tab: true,
            is_zoomed_in_tab: false,
        };

        let binding2 = mux::MuxCapturedPaneBinding {
            pane_id: 102,
            pane_uuid: uuid::Uuid::from_bytes(uuid2).to_string(),
            spawn_custody: None,
            registration_wire_identity: wire2,
            domain_id: 1,
            domain_name: "local".to_string(),
            window_id: 10,
            tab_id: 20,
            lane: mux::MuxCapturedPaneLane::Tiled,
            title: "bash_102".to_string(),
            cwd: Some("/app".to_string()),
            size: frankenterm_term::TerminalSize {
                rows: 24,
                cols: 40,
                pixel_width: 400,
                pixel_height: 480,
                dpi: 96,
            },
            alt_screen_active: false,
            cursor_pos: (0, 0),
            is_active_in_tab: false,
            is_zoomed_in_tab: false,
        };

        let pane_entry1 = mux::tab::PaneEntry {
            window_id: 10,
            tab_id: 20,
            pane_id: 101,
            title: "bash_101".to_string(),
            size: frankenterm_term::TerminalSize {
                rows: 24,
                cols: 40,
                pixel_width: 400,
                pixel_height: 480,
                dpi: 96,
            },
            working_dir: None,
            alt_screen_active: false,
            is_active_pane: true,
            is_zoomed_pane: false,
            workspace: "default".to_string(),
            cursor_pos: Default::default(),
            physical_top: 0,
            top_row: 0,
            left_col: 0,
            tty_name: None,
        };

        let pane_entry2 = mux::tab::PaneEntry {
            window_id: 10,
            tab_id: 20,
            pane_id: 102,
            title: "bash_102".to_string(),
            size: frankenterm_term::TerminalSize {
                rows: 24,
                cols: 40,
                pixel_width: 400,
                pixel_height: 480,
                dpi: 96,
            },
            working_dir: None,
            alt_screen_active: false,
            is_active_pane: false,
            is_zoomed_pane: false,
            workspace: "default".to_string(),
            cursor_pos: Default::default(),
            physical_top: 0,
            top_row: 0,
            left_col: 0,
            tty_name: None,
        };

        let split_tree = mux::tab::PaneNode::Split {
            left: Box::new(mux::tab::PaneNode::Leaf(pane_entry1)),
            right: Box::new(mux::tab::PaneNode::Leaf(pane_entry2)),
            node: mux::tab::SplitDirectionAndSize {
                direction: mux::tab::SplitDirection::Horizontal,
                first: frankenterm_term::TerminalSize {
                    rows: 24,
                    cols: 40,
                    pixel_width: 400,
                    pixel_height: 480,
                    dpi: 96,
                },
                second: frankenterm_term::TerminalSize {
                    rows: 24,
                    cols: 40,
                    pixel_width: 400,
                    pixel_height: 480,
                    dpi: 96,
                },
            },
        };

        let tab = mux::MuxCapturedTab {
            tab_id: 20,
            durable_tab_id: uuid::Uuid::from_u128(0x7420),
            window_id: 10,
            title: "main".to_string(),
            size: frankenterm_term::TerminalSize {
                rows: 24,
                cols: 80,
                pixel_width: 800,
                pixel_height: 480,
                dpi: 96,
            },
            size_before_zoom: frankenterm_term::TerminalSize {
                rows: 24,
                cols: 80,
                pixel_width: 800,
                pixel_height: 480,
                dpi: 96,
            },
            active_pane_id: Some(101),
            zoomed_pane_id: None,
            split_tree,
            floating_panes: vec![],
            floating_focus: None,
            pane_stacks: vec![],
            underlying_tiled_active_pane_id: Some(101),
            runtime_state: Default::default(),
        };

        let window = mux::MuxCapturedWindow {
            window_id: 10,
            durable_window_id: uuid::Uuid::from_u128(0x7710),
            workspace: "default".to_string(),
            title: "win".to_string(),
            order_revision: mux::window::WindowOrderRevision::new(1),
            ordered_tab_ids: vec![20],
            active_tab_id: Some(20),
            active_tab_index: Some(0),
            last_active_tab_id: None,
            tab_stacks: vec![],
            position: None,
            structural_pane_count: 2,
        };

        let captured = mux::MuxCapturedTopology {
            session_incarnation,
            default_domain_id: Some(1),
            domains: vec![mux::MuxCapturedDomain {
                domain_id: 1,
                name: "local".into(),
                state: mux::domain::DomainState::Attached,
                policy: RecoveryDomainPolicy::local_test_fixture().to_mux(),
            }],
            topology_revision: mux::TopologyRevision::new(1),
            captured_at_epoch_ms: 1773500000000,
            client_workspace: None,
            default_workspace: "default".to_string(),
            workspaces: vec![mux::MuxCapturedWorkspace {
                name: "default".to_string(),
                window_ids: vec![10],
                active_window_id: Some(10),
                pane_count: 2,
            }],
            windows: vec![window],
            tabs: vec![tab],
            pane_bindings: vec![binding1, binding2],
        };

        let mut checkpoint_acks = HashMap::new();
        checkpoint_acks.insert(
            101,
            make_test_ack(
                wire1,
                uuid::Uuid::from_bytes(uuid1),
                make_test_checkpoint(24, 40, b"one\n"),
            ),
        );
        checkpoint_acks.insert(
            102,
            make_test_ack(
                wire2,
                uuid::Uuid::from_bytes(uuid2),
                make_test_checkpoint(24, 40, b"two\n"),
            ),
        );

        let mut checkpoint_object_refs = HashMap::new();
        checkpoint_object_refs.insert(
            101,
            RecoveryObjectRef {
                object_id: "obj-101".to_string(),
                byte_length: 512,
                payload_digest: [1u8; 32],
                schema_version: 3,
            },
        );
        checkpoint_object_refs.insert(
            102,
            RecoveryObjectRef {
                object_id: "obj-102".to_string(),
                byte_length: 512,
                payload_digest: [2u8; 32],
                schema_version: 3,
            },
        );

        (meta, captured, checkpoint_acks, checkpoint_object_refs)
    }

    #[test]
    fn converter_preserves_distinct_default_client_and_workspace_window_choices() {
        let (meta, mut captured, acks, refs) = make_test_fixture();
        captured.default_workspace = "future-default-with-no-windows".into();
        captured.client_workspace = Some(mux::MuxCapturedClientWorkspaceBinding {
            client_id: "client-a".into(),
            active_workspace: "other".into(),
        });
        let mut other = captured.windows[0].clone();
        other.window_id = 20;
        other.durable_window_id = uuid::Uuid::from_u128(0x8820);
        other.workspace = "other".into();
        other.ordered_tab_ids.clear();
        other.active_tab_id = None;
        other.active_tab_index = None;
        other.last_active_tab_id = None;
        other.tab_stacks.clear();
        other.structural_pane_count = 0;
        captured.windows.push(other);
        captured.workspaces.push(mux::MuxCapturedWorkspace {
            name: "other".into(),
            window_ids: vec![20],
            active_window_id: Some(20),
            pane_count: 0,
        });
        let image =
            MuxRecoveryImage::from_mux_captured(meta, &captured, &borrowed_acks(&acks), &refs)
                .unwrap();
        let restored =
            MuxRecoveryImage::from_json_slice(&image.to_canonical_json().unwrap()).unwrap();
        let state = restored.require_live_workspace_state().unwrap();
        assert_eq!(state.default_workspace, captured.default_workspace);
        assert_eq!(state.workspaces.len(), captured.workspaces.len());
        for (actual, expected) in state.workspaces.iter().zip(&captured.workspaces) {
            assert_eq!(actual.name, expected.name);
            assert_eq!(actual.window_ids, expected.window_ids);
            assert_eq!(actual.active_window_id, expected.active_window_id);
        }
        assert_eq!(restored.topology.focused_window_id, Some(20));
        assert_eq!(
            restored.topology.client_workspace.unwrap().active_workspace,
            "other"
        );
    }

    #[test]
    fn converter_preserves_tab_runtime_bits_and_policy() {
        let (meta, mut captured, acks, refs) = make_test_fixture();
        let runtime = &mut captured.tabs[0].runtime_state;
        runtime.recency_count = 7;
        runtime.recency_by_index = vec![(0, 6), (90, 1)];
        runtime.constraint_overrides = vec![(
            101,
            mux::pane::PaneConstraints {
                preferred_width: Some(12),
                ..Default::default()
            },
        )];
        runtime.collapsed_panes = vec![102];
        runtime.layout_cycle = Some(mux::tab::MuxCapturedLayoutCycle {
            current_index: 0,
            layouts: vec![mux::layout::SwapLayout {
                name: "signed-zero".into(),
                description: None,
                arrangement: mux::layout::LayoutArrangement::Split {
                    direction: mux::tab::SplitDirection::Vertical,
                    ratio: -0.0,
                    first: Box::new(mux::layout::LayoutArrangement::Slot { is_main: true }),
                    second: Box::new(mux::layout::LayoutArrangement::Slot { is_main: false }),
                },
            }],
        });
        let image =
            MuxRecoveryImage::from_mux_captured(meta, &captured, &borrowed_acks(&acks), &refs)
                .unwrap();
        image.require_live_topology_state().unwrap();
        let bytes = image.to_canonical_json().unwrap();
        let reopened = MuxRecoveryImage::from_json_slice(&bytes).unwrap();
        let restored = reopened.topology.windows[0].tabs[0]
            .runtime_state
            .as_ref()
            .unwrap()
            .to_mux();
        assert_eq!(restored, captured.tabs[0].runtime_state);
        let mux::layout::LayoutArrangement::Split { ratio, .. } =
            &restored.layout_cycle.as_ref().unwrap().layouts[0].arrangement
        else {
            panic!("split lost")
        };
        assert_eq!(ratio.to_bits(), (-0.0f64).to_bits());
    }

    #[test]
    fn converter_preserves_full_domain_census_and_rejects_binding_substitution() {
        let (meta, mut captured, acks, refs) = make_test_fixture();
        captured.domains.push(mux::MuxCapturedDomain {
            domain_id: 77,
            name: "empty".into(),
            state: mux::domain::DomainState::Attached,
            policy: RecoveryDomainPolicy::local_test_fixture().to_mux(),
        });
        captured.default_domain_id = Some(77);
        let image = MuxRecoveryImage::from_mux_captured(
            meta.clone(),
            &captured,
            &borrowed_acks(&acks),
            &refs,
        )
        .unwrap();
        assert_eq!(image.topology.domains.len(), 2);
        assert_eq!(
            image
                .topology
                .domain_state
                .as_ref()
                .unwrap()
                .default_domain_id,
            Some(77)
        );
        assert!(image.topology.domains[1].is_attached);
        assert_eq!(
            image.topology.domains[1]
                .recovery_policy
                .as_ref()
                .unwrap()
                .to_mux(),
            captured.domains[1].policy
        );
        for change in 0..3 {
            let mut invalid = captured.clone();
            match change {
                0 => {
                    invalid.domains.remove(0);
                }
                1 => invalid.domains[0].name = "substituted".into(),
                _ => invalid.domains[0].state = mux::domain::DomainState::Detached,
            }
            assert!(
                MuxRecoveryImage::from_mux_captured(
                    meta.clone(),
                    &invalid,
                    &borrowed_acks(&acks),
                    &refs
                )
                .is_err()
            );
        }
    }

    #[test]
    fn test_converter_positive_full_happy_path() {
        let (meta, captured, acks, refs) = make_test_fixture();
        let image =
            MuxRecoveryImage::from_mux_captured(meta, &captured, &borrowed_acks(&acks), &refs)
                .expect("must convert live captured topology successfully");

        assert_eq!(image.pane_count(), 2);
        assert_eq!(
            image.header.mux_incarnation_id,
            hex::encode(captured.session_incarnation.as_bytes())
        );
        assert_eq!(image.header.generation, 1);
        assert_eq!(image.topology.windows.len(), 1);
        assert_eq!(image.topology.windows[0].tabs.len(), 1);

        let tab = &image.topology.windows[0].tabs[0];
        assert_eq!(tab.active_pane_id, 101);
        assert!(tab.root_split.is_some());
        assert_eq!(tab.all_pane_ids(), vec![101, 102]);
    }

    #[test]
    fn converter_uses_durable_object_identity_despite_reused_numeric_ids() {
        let (meta, mut captured, acks, refs) = make_test_fixture();
        let first = MuxRecoveryImage::from_mux_captured(
            meta.clone(),
            &captured,
            &borrowed_acks(&acks),
            &refs,
        )
        .unwrap();
        assert_eq!(
            first.topology.windows[0].stable_window_id,
            captured.windows[0].durable_window_id.to_string()
        );
        assert_eq!(
            first.topology.windows[0].tabs[0].stable_tab_id,
            captured.tabs[0].durable_tab_id.to_string()
        );

        // A subsequent process may reuse both numeric counters. Its different
        // object identities must not alias the prior recovery image.
        captured.windows[0].durable_window_id = uuid::Uuid::from_u128(0x8810);
        captured.tabs[0].durable_tab_id = uuid::Uuid::from_u128(0x8820);
        let second = MuxRecoveryImage::from_mux_captured(
            meta.clone(),
            &captured,
            &borrowed_acks(&acks),
            &refs,
        )
        .unwrap();
        assert_eq!(
            first.topology.windows[0].window_id,
            second.topology.windows[0].window_id
        );
        assert_eq!(
            first.topology.windows[0].tabs[0].tab_id,
            second.topology.windows[0].tabs[0].tab_id
        );
        assert_ne!(
            first.topology.windows[0].stable_window_id,
            second.topology.windows[0].stable_window_id
        );
        assert_ne!(
            first.topology.windows[0].tabs[0].stable_tab_id,
            second.topology.windows[0].tabs[0].stable_tab_id
        );
        let reopened =
            MuxRecoveryImage::from_json_slice(&second.to_canonical_json().unwrap()).unwrap();
        assert_eq!(reopened.topology, second.topology);

        for missing_window in [true, false] {
            let mut invalid = captured.clone();
            if missing_window {
                invalid.windows[0].durable_window_id = uuid::Uuid::nil();
            } else {
                invalid.tabs[0].durable_tab_id = uuid::Uuid::nil();
            }
            assert!(matches!(
                MuxRecoveryImage::from_mux_captured(
                    meta.clone(),
                    &invalid,
                    &borrowed_acks(&acks),
                    &refs
                ),
                Err(MuxRecoveryImageError::InvalidCapturedTopology(_))
            ));
        }
    }

    #[test]
    fn test_converter_projects_terminal_metadata_from_checkpoint_not_sampled_callbacks() {
        let (meta, mut captured, mut acks, refs) = make_test_fixture();
        let prior = acks.get(&101).unwrap();
        let replacement = make_test_ack(
            prior.registration_wire_identity,
            prior.durable_pane_id,
            make_test_checkpoint(
                24,
                40,
                b"\x1b]0;checkpoint-title\x07\x1b]7;file:///model-cwd\x07\x1b[?1049h\x1b[3;5H",
            ),
        );
        acks.insert(101, replacement);
        let sampled = captured
            .pane_bindings
            .iter_mut()
            .find(|pane| pane.pane_id == 101)
            .unwrap();
        sampled.title = "sampled-process-title".to_owned();
        sampled.cwd = Some("/sampled-process-cwd".to_owned());
        sampled.cursor_pos = (19, 17);
        sampled.alt_screen_active = false;
        sampled.size.pixel_width = 999;
        sampled.size.pixel_height = 998;
        sampled.size.dpi = 144;
        let image =
            MuxRecoveryImage::from_mux_captured(meta, &captured, &borrowed_acks(&acks), &refs)
                .unwrap();
        let pane = image.panes.iter().find(|pane| pane.pane_id == 101).unwrap();
        assert_eq!(pane.title, "checkpoint-title");
        assert_eq!(pane.cwd.as_deref(), Some("file:///model-cwd"));
        assert_eq!(pane.cursor_position, (4, 2));
        assert!(pane.alt_screen_active);
        assert_eq!(
            pane.size,
            TerminalSize {
                rows: 24,
                cols: 40,
                pixel_width: 400,
                pixel_height: 480,
                dpi: 96
            }
        );
    }

    #[test]
    fn test_converter_rejects_checkpoint_ack_semantic_generation_mismatch() {
        let (meta, captured, mut acks, refs) = make_test_fixture();
        acks.get_mut(&101).unwrap().semantic_generation += 1;
        let error =
            MuxRecoveryImage::from_mux_captured(meta, &captured, &borrowed_acks(&acks), &refs)
                .unwrap_err();
        assert_eq!(
            error,
            MuxRecoveryImageError::InvalidCheckpointModel {
                pane_id: 101,
                reason: "semantic generation differs from ACK",
            }
        );
    }

    #[test]
    fn test_converter_positive_with_pane_stacks() {
        let (meta, mut captured, mut acks, mut refs) = make_test_fixture();

        // Add pane 103 as a hidden stack member sharing slot with pane 101
        let wire3 = [3u8; 16];
        let uuid3 = *uuid::Uuid::new_v4().as_bytes();
        captured.pane_bindings.push(mux::MuxCapturedPaneBinding {
            pane_id: 103,
            pane_uuid: uuid::Uuid::from_bytes(uuid3).to_string(),
            spawn_custody: None,
            registration_wire_identity: wire3,
            domain_id: 1,
            domain_name: "local".to_string(),
            window_id: 10,
            tab_id: 20,
            lane: mux::MuxCapturedPaneLane::Tiled,
            title: "bash_103_hidden".to_string(),
            cwd: None,
            size: frankenterm_term::TerminalSize {
                rows: 24,
                cols: 40,
                pixel_width: 400,
                pixel_height: 480,
                dpi: 96,
            },
            alt_screen_active: false,
            cursor_pos: (0, 0),
            is_active_in_tab: false,
            is_zoomed_in_tab: false,
        });

        acks.insert(
            103,
            make_test_ack(
                wire3,
                uuid::Uuid::from_bytes(uuid3),
                make_test_checkpoint(24, 40, b"three\n"),
            ),
        );

        refs.insert(
            103,
            RecoveryObjectRef {
                object_id: "obj-103".to_string(),
                byte_length: 512,
                payload_digest: [3u8; 32],
                schema_version: 3,
            },
        );

        captured.tabs[0].pane_stacks = vec![mux::MuxCapturedPaneStack {
            slot_index: 0,
            pane_ids: vec![101, 103],
            active_index: 0,
        }];

        let image =
            MuxRecoveryImage::from_mux_captured(meta, &captured, &borrowed_acks(&acks), &refs)
                .expect("must convert topology with pane stacks");

        assert_eq!(image.pane_count(), 3);
        let tab = &image.topology.windows[0].tabs[0];
        assert_eq!(tab.pane_stacks.len(), 1);
        assert_eq!(tab.all_pane_ids(), vec![101, 102, 103]);
    }

    #[test]
    fn test_converter_negative_mismatched_ack_wire_identity() {
        let (meta, captured, mut acks, refs) = make_test_fixture();
        // Mutate ack wire identity
        acks.get_mut(&101).unwrap().registration_wire_identity = [0x99u8; 16];

        let err =
            MuxRecoveryImage::from_mux_captured(meta, &captured, &borrowed_acks(&acks), &refs)
                .unwrap_err();
        assert!(matches!(
            err,
            MuxRecoveryImageError::RegistrationWireIdentityMismatch { pane_id: 101, .. }
        ));
    }

    #[test]
    fn test_converter_negative_mismatched_ack_durable_uuid() {
        let (meta, captured, mut acks, refs) = make_test_fixture();
        // Mutate ack durable uuid
        acks.get_mut(&101).unwrap().durable_pane_id = uuid::Uuid::new_v4();

        let err =
            MuxRecoveryImage::from_mux_captured(meta, &captured, &borrowed_acks(&acks), &refs)
                .unwrap_err();
        assert!(matches!(
            err,
            MuxRecoveryImageError::DurablePaneUuidMismatch { pane_id: 101, .. }
        ));
    }

    #[test]
    fn test_converter_negative_missing_ack() {
        let (meta, captured, mut acks, refs) = make_test_fixture();
        acks.remove(&102);

        let err =
            MuxRecoveryImage::from_mux_captured(meta, &captured, &borrowed_acks(&acks), &refs)
                .unwrap_err();
        assert_eq!(err, MuxRecoveryImageError::MissingCheckpointAck(102));
    }

    #[test]
    fn test_converter_negative_extraneous_ack() {
        let (meta, captured, mut acks, refs) = make_test_fixture();
        acks.insert(
            999,
            make_test_ack(
                [9u8; 16],
                uuid::Uuid::new_v4(),
                make_test_checkpoint(24, 80, b"extra\n"),
            ),
        );

        let err =
            MuxRecoveryImage::from_mux_captured(meta, &captured, &borrowed_acks(&acks), &refs)
                .unwrap_err();
        assert_eq!(err, MuxRecoveryImageError::ExtraneousCheckpointAck(999));
    }

    #[test]
    fn test_converter_negative_missing_object_ref() {
        let (meta, captured, acks, mut refs) = make_test_fixture();
        refs.remove(&101);

        let err =
            MuxRecoveryImage::from_mux_captured(meta, &captured, &borrowed_acks(&acks), &refs)
                .unwrap_err();
        assert_eq!(err, MuxRecoveryImageError::MissingCheckpointObjectRef(101));
    }

    #[test]
    fn test_converter_negative_extraneous_object_ref() {
        let (meta, captured, acks, mut refs) = make_test_fixture();
        refs.insert(
            999,
            RecoveryObjectRef {
                object_id: "extra".to_string(),
                byte_length: 128,
                payload_digest: [9u8; 32],
                schema_version: 3,
            },
        );

        let err =
            MuxRecoveryImage::from_mux_captured(meta, &captured, &borrowed_acks(&acks), &refs)
                .unwrap_err();
        assert_eq!(
            err,
            MuxRecoveryImageError::ExtraneousCheckpointObjectRef(999)
        );
    }

    #[test]
    fn test_converter_negative_zero_registration_wire_identity() {
        let (meta, mut captured, acks, refs) = make_test_fixture();
        captured.pane_bindings[0].registration_wire_identity = [0u8; 16];

        let err =
            MuxRecoveryImage::from_mux_captured(meta, &captured, &borrowed_acks(&acks), &refs)
                .unwrap_err();
        assert_eq!(
            err,
            MuxRecoveryImageError::ZeroRegistrationWireIdentity(101)
        );
    }

    #[test]
    fn test_converter_negative_nil_durable_pane_id() {
        let (meta, captured, mut acks, refs) = make_test_fixture();
        acks.get_mut(&101).unwrap().durable_pane_id = uuid::Uuid::nil();

        let err =
            MuxRecoveryImage::from_mux_captured(meta, &captured, &borrowed_acks(&acks), &refs)
                .unwrap_err();
        assert_eq!(err, MuxRecoveryImageError::NilDurablePaneId(101));
    }

    #[test]
    fn test_converter_preserves_zero_parser_stream_bytes_without_invented_receipts() {
        let (meta, captured, mut acks, refs) = make_test_fixture();
        let ack = acks.get_mut(&101).unwrap();
        *ack = make_test_ack(
            ack.registration_wire_identity,
            ack.durable_pane_id,
            make_test_checkpoint(24, 40, b""),
        );

        let image =
            MuxRecoveryImage::from_mux_captured(meta, &captured, &borrowed_acks(&acks), &refs)
                .unwrap();
        let binding = &image
            .panes
            .iter()
            .find(|pane| pane.pane_id == 101)
            .unwrap()
            .checkpoint;
        assert_eq!(binding.parser_capture.watermark_bytes, 0);
        assert_eq!(binding.parser_capture.segment_id, None);
        assert_eq!(binding.parser_capture.parser_seqno, None);
        assert!(matches!(
            binding.authority,
            CheckpointAuthority::ModelOnly {
                parser_seqno: None,
                ..
            }
        ));
    }

    #[test]
    fn test_converter_preserves_all_registration_identity_bits() {
        let (meta, mut captured, mut acks, refs) = make_test_fixture();
        let mut wire_identity = [0; 16];
        wire_identity[15] = 7;
        captured.pane_bindings[0].registration_wire_identity = wire_identity;
        acks.get_mut(&101).unwrap().registration_wire_identity = wire_identity;
        let image =
            MuxRecoveryImage::from_mux_captured(meta, &captured, &borrowed_acks(&acks), &refs)
                .unwrap();
        assert_eq!(
            image
                .panes
                .iter()
                .find(|pane| pane.pane_id == 101)
                .unwrap()
                .checkpoint
                .registration_wire_identity,
            wire_identity
        );
    }

    #[test]
    fn test_converter_rejects_ack_checkpoint_watermark_mismatch() {
        let (meta, captured, mut acks, refs) = make_test_fixture();
        acks.get_mut(&101).unwrap().parser_stream_bytes = 5;
        let error =
            MuxRecoveryImage::from_mux_captured(meta, &captured, &borrowed_acks(&acks), &refs)
                .unwrap_err();
        assert_eq!(
            error,
            MuxRecoveryImageError::ParserStreamWatermarkMismatch {
                pane_id: 101,
                ack: 5,
                checkpoint: 4
            }
        );
    }

    #[test]
    fn test_converter_rejects_missing_ordered_tab_instead_of_dropping_it() {
        let (meta, mut captured, acks, refs) = make_test_fixture();
        captured.windows[0].ordered_tab_ids.push(999);
        let error =
            MuxRecoveryImage::from_mux_captured(meta, &captured, &borrowed_acks(&acks), &refs)
                .unwrap_err();
        assert_eq!(
            error,
            MuxRecoveryImageError::InvalidCapturedTopology("ordered tab is missing or repeated")
        );
    }

    #[test]
    fn test_converter_rejects_invalid_active_tab_instead_of_selecting_first() {
        let (meta, mut captured, acks, refs) = make_test_fixture();
        captured.windows[0].active_tab_index = Some(99);
        let error =
            MuxRecoveryImage::from_mux_captured(meta, &captured, &borrowed_acks(&acks), &refs)
                .unwrap_err();
        assert_eq!(
            error,
            MuxRecoveryImageError::InvalidCapturedTopology(
                "active tab index is missing or invalid"
            )
        );
    }

    #[test]
    fn test_converter_rejects_missing_active_pane_instead_of_selecting_first() {
        let (meta, mut captured, acks, refs) = make_test_fixture();
        captured.tabs[0].active_pane_id = None;
        let error =
            MuxRecoveryImage::from_mux_captured(meta, &captured, &borrowed_acks(&acks), &refs)
                .unwrap_err();
        assert_eq!(
            error,
            MuxRecoveryImageError::InvalidCapturedTopology("nonempty tab has no active pane")
        );
    }

    #[test]
    fn test_converter_preserves_position_units_origin_and_absent_focus() {
        let (meta, mut captured, acks, refs) = make_test_fixture();
        captured.windows[0].position = Some(config::GuiPosition {
            x: config::Dimension::Percent(0.25),
            y: config::Dimension::Cells(-2.5),
            origin: config::GeometryOrigin::Named("external-display".to_owned()),
        });
        let image =
            MuxRecoveryImage::from_mux_captured(meta, &captured, &borrowed_acks(&acks), &refs)
                .unwrap();
        assert_eq!(image.topology.focused_window_id, None);
        assert_eq!(
            image.topology.windows[0].gui_position,
            Some(RecoveryGuiPosition {
                x: RecoveryGuiDimension::Percent(0.25_f32.to_bits()),
                y: RecoveryGuiDimension::Cells((-2.5_f32).to_bits()),
                origin: RecoveryGeometryOrigin::Named("external-display".to_owned()),
            })
        );
        let decoded =
            MuxRecoveryImage::from_json_slice(&image.to_canonical_json().unwrap()).unwrap();
        assert_eq!(
            decoded.topology.windows[0].gui_position,
            image.topology.windows[0].gui_position
        );
    }
}
