//! Canonical bounded whole-mux recovery image types and validation.
//!
//! Bead: `ft-interactive-swarm-product-convergence-7xqz4.8.14.1.2`
//!
//! This module defines the canonical, versioned root and bounded immutable
//! logical-object graph representing a whole-mux snapshot. It preserves:
//! - Exact ordered domain, window, and tab identities.
//! - Authoritative user tab order from live `WindowOrderRevision`.
//! - Un-lossy split trees with physical first/second `TerminalSize` (matching `tab.rs:2714`).
//! - Floating panes, floating focus, and zoom state.
//! - Per-client active workspace bindings (mirroring `mux/lib.rs:11710`) without inventing
//!   global workspace truth.
//! - Cryptographic binding of terminal checkpoint object references to the exact
//!   mux incarnation and pane registration generation, preventing snapshot-swapping attacks.
//! - Explicit authority distinction between `ModelOnly` parser ground captures and
//!   `Guardian` protocol leases (never promoting a raw digest to authority).
//!
//! # Invariants and Resource Limits
//! - Hard byte ceiling: inputs exceeding [`MAX_RECOVERY_IMAGE_BYTES`] (16 MiB) fail pre-parse.
//! - Split tree recursion depth is bounded to [`MAX_SPLIT_TREE_DEPTH`] (32).
//! - Window, tab, and pane counts are capped at 4,096.
//! - All string lengths are strictly bounded to prevent memory exhaustion.
//! - Duplicate IDs (incarnation numeric IDs or stable UUIDs) fail validation closed.
//! - Exact 1-to-1 global placement bijection: every pane in the catalog must be placed
//!   in exactly one split leaf or floating pane across all windows and tabs. Duplicate
//!   placements and orphan catalog panes are strictly rejected.
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
pub const MUX_RECOVERY_IMAGE_SCHEMA_VERSION: u32 = 1;

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

/// Validation and decoding errors for whole-mux recovery images.
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

    #[error("invalid header: {0}")]
    InvalidHeader(&'static str),

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

    #[error("orphan pane in catalog: pane id {0} is never placed in any tab split tree or floating list")]
    OrphanCatalogPane(usize),

    #[error("duplicate domain name '{0}'")]
    DuplicateDomainName(String),

    #[error("duplicate incarnation domain id {0}")]
    DuplicateDomainId(usize),

    #[error("referenced domain '{0}' not found in image domain catalog")]
    MissingDomain(String),

    #[error("pane id {0} referenced in tab split tree or floating list not found in pane catalog")]
    MissingPane(usize),

    #[error("placed pane {pane_id} uuid '{placed_uuid}' does not match catalog uuid '{catalog_uuid}'")]
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

    #[error("tab {tab_id} active pane id {pane_id} not found in split tree or floating panes")]
    InvalidActivePaneId { tab_id: usize, pane_id: usize },

    #[error("tab {tab_id} floating focus pane id {pane_id} not found in tab floating panes")]
    InvalidFloatingFocusId { tab_id: usize, pane_id: usize },

    #[error("tab {tab_id} zoomed pane id {pane_id} not found in tab panes")]
    InvalidZoomedPaneId { tab_id: usize, pane_id: usize },

    #[error("focused window id {0} not found in window catalog")]
    InvalidFocusedWindowId(usize),

    #[error("checkpoint binding for pane {pane_id} has mismatched incarnation: expected '{expected}', found '{found}'")]
    IncarnationMismatch {
        pane_id: usize,
        expected: String,
        found: String,
    },

    #[error("checkpoint binding for pane {pane_id} has mismatched pane uuid: expected '{expected}', found '{found}'")]
    PaneUuidMismatch {
        pane_id: usize,
        expected: String,
        found: String,
    },

    #[error("pane {0} has invalid registration generation: must be greater than zero")]
    InvalidRegistrationGeneration(usize),

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

impl<'a> Write for Sha256Sink<'a> {
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
            return Err(std::io::Error::new(
                std::io::ErrorKind::Other,
                format!(
                    "output byte limit exceeded: attempted {} bytes with limit {}",
                    self.written.saturating_add(buf.len()),
                    self.limit
                ),
            ));
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
}

/// Window placement and dimensions on host desktop.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecoveryGuiPosition {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

/// Ordered window representation preserving exact user tab order.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecoveryWindow {
    pub window_id: usize,
    pub stable_window_id: String,
    pub workspace: String,
    pub order_revision: u64,
    pub gui_position: Option<RecoveryGuiPosition>,
    pub tabs: Vec<RecoveryTab>,
    pub active_tab_index: usize,
}

/// Ordered tab representation preserving exact split hierarchy, sizes, and floating panes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecoveryTab {
    pub tab_id: usize,
    pub stable_tab_id: String,
    pub title: String,
    pub working_dir: Option<String>,
    pub size: TerminalSize,
    pub size_before_zoom: TerminalSize,
    pub zoomed_pane_id: Option<usize>,
    pub root_split: Option<RecoverySplitNode>,
    pub floating_panes: Vec<RecoveryFloatingPane>,
    pub floating_focus: Option<usize>,
    pub active_pane_id: usize,
}

impl RecoveryTab {
    /// Returns all unique pane IDs contained in this tab (split tree leaves plus floating panes).
    #[must_use]
    pub fn all_pane_ids(&self) -> Vec<usize> {
        let mut panes = Vec::new();
        if let Some(ref root) = self.root_split {
            for (id, _) in root.leaves() {
                panes.push(id);
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
    pub domain_name: String,
    pub title: String,
    pub cwd: Option<String>,
    pub size: TerminalSize,
    pub cursor_position: (usize, usize),
    pub alt_screen_active: bool,
    pub checkpoint: PaneCheckpointBinding,
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
            .field("checkpoint_obj_id", &self.checkpoint.checkpoint_ref.object_id)
            .finish()
    }
}

/// Binds a decoupled terminal checkpoint object reference to the exact topology
/// incarnation, pane registration generation, and external parser capture boundary.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneCheckpointBinding {
    pub topology_incarnation_id: String,
    pub pane_uuid: String,
    pub registration_generation: u64,
    pub parser_capture: ParserCaptureIdentity,
    pub checkpoint_ref: RecoveryObjectRef,
    pub authority: CheckpointAuthority,
}

/// Identifiers captured at the external parser barrier.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ParserCaptureIdentity {
    pub watermark_bytes: u64,
    pub segment_id: u64,
    pub parser_seqno: u64,
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
        parser_seqno: u64,
    },
    /// Cryptographically signed or verified under guardian lease and catalog protocol.
    Guardian {
        guardian_generation: u64,
        lease_verifier: String,
        catalog_generation: u64,
    },
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
        self.topology.windows.iter().find(|w| w.window_id == window_id)
    }

    /// Looks up a window by its stable window ID.
    #[must_use]
    pub fn find_window_by_stable_id(&self, stable_id: &str) -> Option<&RecoveryWindow> {
        self.topology.windows.iter().find(|w| w.stable_window_id == stable_id)
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
        if self.header.schema_version != MUX_RECOVERY_IMAGE_SCHEMA_VERSION {
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

        for domain in &self.topology.domains {
            validate_string_len("domain.domain_name", &domain.domain_name, MAX_STRING_BYTES)?;
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
            if let CheckpointAuthority::Guardian {
                ref lease_verifier, ..
            } = pane.checkpoint.authority
            {
                validate_string_len(
                    "pane.checkpoint.authority.lease_verifier",
                    lease_verifier,
                    MAX_ID_STRING_BYTES,
                )?;
            }
        }

        for window in &self.topology.windows {
            validate_string_len(
                "window.stable_window_id",
                &window.stable_window_id,
                MAX_ID_STRING_BYTES,
            )?;
            validate_string_len("window.workspace", &window.workspace, MAX_STRING_BYTES)?;

            if window.tabs.len() > MAX_RECOVERY_TABS_PER_WINDOW {
                return Err(MuxRecoveryImageError::ResourceLimit {
                    resource: "tabs_per_window",
                    count: window.tabs.len(),
                    limit: MAX_RECOVERY_TABS_PER_WINDOW,
                });
            }

            for tab in &window.tabs {
                validate_string_len(
                    "tab.stable_tab_id",
                    &tab.stable_tab_id,
                    MAX_ID_STRING_BYTES,
                )?;
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
    ///   in exactly one split leaf or floating pane across all windows and tabs. Duplicate
    ///   placements and orphan catalog panes fail closed.
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
                return Err(MuxRecoveryImageError::MissingDomain(pane.domain_name.clone()));
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
            if pane.checkpoint.registration_generation == 0 {
                return Err(MuxRecoveryImageError::InvalidRegistrationGeneration(
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
                    if *parser_seqno == 0 {
                        return Err(MuxRecoveryImageError::InvalidAuthority {
                            pane_id: pane.pane_id,
                            reason: "parser_seqno must be greater than zero",
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
                    lease_verifier,
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
                    if lease_verifier.is_empty() {
                        return Err(MuxRecoveryImageError::InvalidAuthority {
                            pane_id: pane.pane_id,
                            reason: "lease_verifier must not be empty",
                        });
                    }
                }
            }
        }

        // 5. Windows, tabs, split trees, and global placement bijection
        let mut seen_window_ids = HashSet::new();
        let mut seen_stable_window_ids = HashSet::new();
        let mut seen_tab_ids = HashSet::new();
        let mut seen_stable_tab_ids = HashSet::new();
        let mut global_placed_pane_ids = HashSet::new();

        for window in &self.topology.windows {
            if !seen_window_ids.insert(window.window_id) {
                return Err(MuxRecoveryImageError::DuplicateWindowId(window.window_id));
            }
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

            for tab in &window.tabs {
                if !seen_tab_ids.insert(tab.tab_id) {
                    return Err(MuxRecoveryImageError::DuplicateTabId(tab.tab_id));
                }
                if !seen_stable_tab_ids.insert(&tab.stable_tab_id) {
                    return Err(MuxRecoveryImageError::DuplicateStableTabId(
                        tab.stable_tab_id.clone(),
                    ));
                }

                // Collect and validate all panes in this tab (split tree + floating)
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

                // Active pane must exist in tab_panes (either in split tree OR floating panes)
                if !tab_panes.is_empty() && !tab_panes.contains(&tab.active_pane_id) {
                    return Err(MuxRecoveryImageError::InvalidActivePaneId {
                        tab_id: tab.tab_id,
                        pane_id: tab.active_pane_id,
                    });
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
                    if !tab_panes.contains(&z_id) {
                        return Err(MuxRecoveryImageError::InvalidZoomedPaneId {
                            tab_id: tab.tab_id,
                            pane_id: z_id,
                        });
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
        let computed_digest = self.compute_digest()?;
        if computed_digest != self.image_digest {
            return Err(MuxRecoveryImageError::DigestMismatch {
                computed: hex::encode(computed_digest),
                recorded: hex::encode(self.image_digest),
            });
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
// Helper Functions
// =============================================================================

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

    fn make_test_pane(pane_id: usize, pane_uuid: &str, incarnation_id: &str) -> RecoveryPane {
        RecoveryPane {
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
                registration_generation: 1,
                parser_capture: ParserCaptureIdentity {
                    watermark_bytes: 1024,
                    segment_id: 1,
                    parser_seqno: 42,
                },
                checkpoint_ref: RecoveryObjectRef {
                    object_id: format!("obj_{pane_id}"),
                    byte_length: 512,
                    payload_digest: [1u8; 32],
                    schema_version: 2,
                },
                authority: CheckpointAuthority::ModelOnly {
                    captured_at_epoch_ms: 1747371642000,
                    parser_seqno: 42,
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
            stable_tab_id: "uuid-tab-10".to_string(),
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
            floating_focus: Some(3),
            active_pane_id: 1,
        };

        let window = RecoveryWindow {
            window_id: 100,
            stable_window_id: "uuid-window-100".to_string(),
            workspace: "default".to_string(),
            order_revision: 1,
            gui_position: Some(RecoveryGuiPosition {
                x: 100,
                y: 100,
                width: 800,
                height: 600,
            }),
            tabs: vec![tab],
            active_tab_index: 0,
        };

        let topology = RecoveryTopology {
            domains: vec![RecoveryDomain {
                incarnation_domain_id: 0,
                domain_name: "local".to_string(),
                is_attached: true,
            }],
            windows: vec![window],
            focused_window_id: Some(100),
            client_workspace: Some(ClientWorkspaceBinding {
                client_id: "client-gui-1".to_string(),
                active_workspace: "default".to_string(),
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
        assert!(image.find_window_by_stable_id("uuid-window-100").is_some());
        assert!(image.find_window(999).is_none());

        assert!(image.find_tab(10).is_some());
        assert!(image.find_tab_by_stable_id("uuid-tab-10").is_some());
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
        image.image_digest = image.compute_digest().unwrap();

        assert!(image.validate().is_ok());
    }

    #[test]
    fn test_positive_guardian_authority() {
        let mut image = make_valid_test_image();
        image.panes[0].checkpoint.authority = CheckpointAuthority::Guardian {
            guardian_generation: 5,
            lease_verifier: "lease-sig-ok".to_string(),
            catalog_generation: 3,
        };
        image.image_digest = image.compute_digest().unwrap();
        assert!(image.validate().is_ok());
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
        // Leaf 1 has pane_id 1, but its placed pane_uuid is swapped to "uuid-pane-2"
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
        // Floating pane 3 placed uuid is changed
        image.topology.windows[0].tabs[0].floating_panes[0].pane_uuid =
            "uuid-swapped".to_string();
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
        // Add a second tab that also places pane 1
        let mut dup_tab = image.topology.windows[0].tabs[0].clone();
        dup_tab.tab_id = 11;
        dup_tab.stable_tab_id = "uuid-tab-11".to_string();
        dup_tab.floating_panes.clear();
        dup_tab.floating_focus = None;
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
        // Place pane 1 as a floating pane while it is already in the split tree
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
        // Add pane 4 to catalog, but do not place it in any tab
        let pane4 = make_test_pane(4, "uuid-pane-4", &image.header.mux_incarnation_id);
        image.panes.push(pane4);
        image.image_digest = image.compute_digest().unwrap();

        let err = image.validate().unwrap_err();
        assert_eq!(err, MuxRecoveryImageError::OrphanCatalogPane(4));
    }

    // --- Paired Tests: Registration & Authority Fields ---

    #[test]
    fn test_negative_registration_generation_zero() {
        let mut image = make_valid_test_image();
        image.panes[0].checkpoint.registration_generation = 0;
        image.image_digest = image.compute_digest().unwrap();

        let err = image.validate().unwrap_err();
        assert_eq!(
            err,
            MuxRecoveryImageError::InvalidRegistrationGeneration(1)
        );
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
            parser_seqno: 42,
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
    fn test_negative_model_only_zero_seqno() {
        let mut image = make_valid_test_image();
        image.panes[0].checkpoint.authority = CheckpointAuthority::ModelOnly {
            captured_at_epoch_ms: 1747371642000,
            parser_seqno: 0,
        };
        image.image_digest = image.compute_digest().unwrap();

        let err = image.validate().unwrap_err();
        assert!(matches!(
            err,
            MuxRecoveryImageError::InvalidAuthority {
                pane_id: 1,
                reason: "parser_seqno must be greater than zero"
            }
        ));
    }

    #[test]
    fn test_negative_model_only_parser_seqno_mismatch() {
        let mut image = make_valid_test_image();
        image.panes[0].checkpoint.parser_capture.parser_seqno = 99;
        image.panes[0].checkpoint.authority = CheckpointAuthority::ModelOnly {
            captured_at_epoch_ms: 1747371642000,
            parser_seqno: 42,
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
            lease_verifier: "lease-sig".to_string(),
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
    fn test_negative_guardian_authority_empty_lease_verifier() {
        let mut image = make_valid_test_image();
        image.panes[0].checkpoint.authority = CheckpointAuthority::Guardian {
            guardian_generation: 1,
            lease_verifier: "".to_string(),
            catalog_generation: 1,
        };
        image.image_digest = image.compute_digest().unwrap();

        let err = image.validate().unwrap_err();
        assert!(matches!(
            err,
            MuxRecoveryImageError::InvalidAuthority {
                pane_id: 1,
                reason: "lease_verifier must not be empty"
            }
        ));
    }

    // --- Paired Tests: Split Tree Depth Limit ---

    #[test]
    fn test_positive_near_identical_tree_depth_at_limit() {
        let incarnation_id = "inc-20260914-1800";
        // Construct a tree of depth exactly 5 with 6 panes
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
            stable_tab_id: "uuid-tab-10".to_string(),
            title: "tree_tab".to_string(),
            working_dir: None,
            size: TerminalSize::default(),
            size_before_zoom: TerminalSize::default(),
            zoomed_pane_id: None,
            root_split: Some(node),
            floating_panes: vec![],
            floating_focus: None,
            active_pane_id: 1,
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
                }],
                windows: vec![RecoveryWindow {
                    window_id: 100,
                    stable_window_id: "win-100".to_string(),
                    workspace: "default".to_string(),
                    order_revision: 1,
                    gui_position: None,
                    tabs: vec![tab],
                    active_tab_index: 0,
                }],
                focused_window_id: Some(100),
                client_workspace: None,
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
        // Construct split tree deeper than 32
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

        // compute_digest bounds check also rejects deep split trees before hashing
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
        dup_win.stable_window_id = "uuid-window-other".to_string();
        image.topology.windows.push(dup_win);
        image.image_digest = image.compute_digest().unwrap();

        let err = image.validate().unwrap_err();
        assert_eq!(err, MuxRecoveryImageError::DuplicateWindowId(100));
    }

    #[test]
    fn test_negative_duplicate_tab_id() {
        let mut image = make_valid_test_image();
        let mut dup_tab = image.topology.windows[0].tabs[0].clone();
        dup_tab.stable_tab_id = "uuid-tab-other".to_string();
        dup_tab.root_split = None;
        dup_tab.floating_panes.clear();
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
        assert!(matches!(
            err,
            MuxRecoveryImageError::DigestMismatch { .. }
        ));
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
        image.image_digest = image.compute_digest().unwrap();

        let err = image.validate().unwrap_err();
        assert!(matches!(err, MuxRecoveryImageError::InvalidMagic { .. }));
    }

    #[test]
    fn test_negative_unsupported_schema_version() {
        let mut image = make_valid_test_image();
        image.header.schema_version = 999;
        image.image_digest = image.compute_digest().unwrap();

        let err = image.validate().unwrap_err();
        assert_eq!(
            err,
            MuxRecoveryImageError::UnsupportedSchemaVersion(999)
        );
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
}
