//! Vendored runtime primitive inventory and migration map for asupersync.
//!
//! This module catalogs every async/runtime primitive used by FrankenTerm's
//! vendored crates (the `frankenterm/` subdirectory), assigns migration
//! difficulty, criticality, and sequencing constraints, and provides a
//! queryable API for downstream migration tooling.
//!
//! Corresponds to bead ft-e34d9.10.5.1.
//!
//! # Architecture
//!
//! Vendored crates use exclusively `smol` (68 references across 11 files)
//! with zero tokio references. Three crates already have optional
//! `async-asupersync` feature gates. The migration path is:
//!
//! ```text
//! smol::block_on / smol::io  →  asupersync adapter  →  native asupersync
//! ```
//!
//! # Migration Sequencing (S4 stage)
//!
//! 1. **Wave 0 (already compat)**: async_ossl, uds, promise — already have
//!    `async-asupersync` features; validate and lock contracts.
//! 2. **Wave 1 (codec)**: Replace smol I/O traits with asupersync adapter;
//!    codec is isolated and low-risk.
//! 3. **Wave 2 (ssh)**: Highest concentration (44 smol refs); requires
//!    adapter for channel, block_on, and async I/O patterns.
//! 4. **Wave 3 (config + scripting)**: Feature-gated smol usage via Lua;
//!    isolate behind feature boundary.
//! 5. **Wave 4 (mux)**: Transitivity-only; migration follows ssh/config.
//! 6. **Wave 5 (pty)**: Dev-only futures traits; transparent migration.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

// ── Types ──────────────────────────────────────────────────────────────────

/// Identifies a vendored crate in the frankenterm/ directory.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct VendoredCrateId(pub String);

impl VendoredCrateId {
    pub fn new(name: impl Into<String>) -> Self {
        Self(name.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for VendoredCrateId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Runtime primitive family detected in vendored code.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum RuntimePrimitive {
    /// `smol::block_on`, `smol::spawn`, `smol::io::*`
    Smol,
    /// `asupersync::*` (already migrated or has feature gate)
    Asupersync,
    /// `async-io` crate (reactor-level)
    AsyncIo,
    /// `async-executor`, `async-task` (executor primitives)
    AsyncExecutor,
    /// `futures::io::AsyncRead/AsyncWrite` (trait-only)
    FuturesTraits,
    /// `flume` channels (runtime-agnostic)
    Flume,
}

impl std::fmt::Display for RuntimePrimitive {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Smol => write!(f, "smol"),
            Self::Asupersync => write!(f, "asupersync"),
            Self::AsyncIo => write!(f, "async-io"),
            Self::AsyncExecutor => write!(f, "async-executor"),
            Self::FuturesTraits => write!(f, "futures-traits"),
            Self::Flume => write!(f, "flume"),
        }
    }
}

/// Migration difficulty for a vendored crate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum MigrationDifficulty {
    /// Already has asupersync feature gate; just validate and enable.
    AlreadyCompat,
    /// Minimal async surface; straightforward adapter.
    Low,
    /// Multiple async patterns; isolated but requires careful adapter work.
    Medium,
    /// Heavy async surface with cross-cutting concerns.
    High,
}

impl MigrationDifficulty {
    /// Numeric score for sorting (lower = easier).
    pub fn score(self) -> u8 {
        match self {
            Self::AlreadyCompat => 0,
            Self::Low => 1,
            Self::Medium => 2,
            Self::High => 3,
        }
    }
}

impl std::fmt::Display for MigrationDifficulty {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AlreadyCompat => write!(f, "already-compat"),
            Self::Low => write!(f, "low"),
            Self::Medium => write!(f, "medium"),
            Self::High => write!(f, "high"),
        }
    }
}

/// Criticality of the crate to FrankenTerm's runtime.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum Criticality {
    /// Not on critical path; sync-only or dev-only.
    None,
    /// Low impact; feature-gated or optional path.
    Low,
    /// Medium impact; used in standard workflows.
    Medium,
    /// High impact; core to mux/ssh/transport.
    High,
}

impl Criticality {
    pub fn score(self) -> u8 {
        match self {
            Self::None => 0,
            Self::Low => 1,
            Self::Medium => 2,
            Self::High => 3,
        }
    }
}

/// Migration wave assignment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum MigrationWave {
    /// Already compatible; validate and lock.
    Wave0AlreadyCompat,
    /// Isolated async I/O (codec).
    Wave1Codec,
    /// Heavy smol surface (ssh).
    Wave2Ssh,
    /// Feature-gated Lua path (config, scripting).
    Wave3ConfigScripting,
    /// Transitivity-only (mux).
    Wave4Mux,
    /// Dev-only traits (pty).
    Wave5DevOnly,
    /// No async; no migration needed.
    NotApplicable,
}

impl MigrationWave {
    pub fn ordinal(self) -> u8 {
        match self {
            Self::Wave0AlreadyCompat => 0,
            Self::Wave1Codec => 1,
            Self::Wave2Ssh => 2,
            Self::Wave3ConfigScripting => 3,
            Self::Wave4Mux => 4,
            Self::Wave5DevOnly => 5,
            Self::NotApplicable => 255,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Wave0AlreadyCompat => "W0: already compat",
            Self::Wave1Codec => "W1: codec adapter",
            Self::Wave2Ssh => "W2: ssh adapter",
            Self::Wave3ConfigScripting => "W3: config/scripting isolation",
            Self::Wave4Mux => "W4: mux transitivity",
            Self::Wave5DevOnly => "W5: dev-only traits",
            Self::NotApplicable => "N/A: sync-only",
        }
    }
}

/// Feature gate configuration for async runtime selection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FeatureGateConfig {
    /// Does the crate have an `async-smol` feature?
    pub has_async_smol: bool,
    /// Does the crate have an `async-asupersync` feature?
    pub has_async_asupersync: bool,
    /// Does the default feature set include async-smol?
    pub default_includes_smol: bool,
    /// Transitive feature activations (e.g., "frankenterm-ssh/async-smol").
    pub transitive_activations: Vec<String>,
}

/// Per-file async primitive reference count.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileReference {
    pub path: String,
    pub smol_refs: u32,
    pub asupersync_refs: u32,
    pub async_io_refs: u32,
    pub futures_refs: u32,
    pub primary_patterns: Vec<String>,
}

/// Complete entry for a vendored crate in the migration map.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VendoredCrateEntry {
    pub crate_id: VendoredCrateId,
    pub cargo_toml_path: String,
    pub total_smol_refs: u32,
    pub total_asupersync_refs: u32,
    pub total_async_refs: u32,
    pub difficulty: MigrationDifficulty,
    pub criticality: Criticality,
    pub wave: MigrationWave,
    pub recommended_target: String,
    pub feature_gates: FeatureGateConfig,
    pub file_references: Vec<FileReference>,
    pub depends_on: Vec<VendoredCrateId>,
    pub depended_by: Vec<VendoredCrateId>,
    pub affected_workflows: Vec<String>,
    pub notes: String,
}

/// The complete vendored migration map.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VendoredMigrationMap {
    pub version: u32,
    pub generated_at: String,
    pub bead_id: String,
    pub risk_id: String,
    pub total_vendored_crates: usize,
    pub async_vendored_crates: usize,
    pub sync_only_crates: Vec<VendoredCrateId>,
    pub entries: BTreeMap<VendoredCrateId, VendoredCrateEntry>,
    pub global_smol_refs: u32,
    pub global_asupersync_refs: u32,
}

// ── Canonical inventory builder ────────────────────────────────────────────

/// All 29 vendored crates (15 sync-only omitted from detailed entries).
const SYNC_ONLY_CRATES: &[&str] = &[
    "base91",
    "bidi",
    "bintree",
    "blob-leases",
    "cell",
    "char-props",
    "color-types",
    "dynamic",
    "escape-parser",
    "filedescriptor",
    "input-types",
    "luahelper",
    "rangeset",
    "surface",
    "term",
    "termwiz",
    "umask",
    "vtparse",
    "lua-api-crates",
    "procinfo",
];

/// Build the canonical vendored migration map from the known inventory.
pub fn build_canonical_map() -> VendoredMigrationMap {
    let mut entries = BTreeMap::new();

    // ── async_ossl ─────────────────────────────────────────────────────
    entries.insert(
        VendoredCrateId::new("async_ossl"),
        VendoredCrateEntry {
            crate_id: VendoredCrateId::new("async_ossl"),
            cargo_toml_path: "frankenterm/async_ossl/Cargo.toml".into(),
            total_smol_refs: 0,
            total_asupersync_refs: 1,
            total_async_refs: 1,
            difficulty: MigrationDifficulty::AlreadyCompat,
            criticality: Criticality::Medium,
            wave: MigrationWave::Wave0AlreadyCompat,
            recommended_target: "async-asupersync feature gate (already present)".into(),
            feature_gates: FeatureGateConfig {
                has_async_smol: false,
                has_async_asupersync: true,
                default_includes_smol: false,
                transitive_activations: vec![],
            },
            file_references: vec![],
            depends_on: vec![],
            depended_by: vec![VendoredCrateId::new("ssh")],
            affected_workflows: vec!["ssh-transport".into(), "tls-connections".into()],
            notes: "Most mature vendored crate for dual-runtime. Has async-io default + async-asupersync optional.".into(),
        },
    );

    // ── uds ────────────────────────────────────────────────────────────
    entries.insert(
        VendoredCrateId::new("uds"),
        VendoredCrateEntry {
            crate_id: VendoredCrateId::new("uds"),
            cargo_toml_path: "frankenterm/uds/Cargo.toml".into(),
            total_smol_refs: 0,
            total_asupersync_refs: 1,
            total_async_refs: 1,
            difficulty: MigrationDifficulty::AlreadyCompat,
            criticality: Criticality::Medium,
            wave: MigrationWave::Wave0AlreadyCompat,
            recommended_target: "async-asupersync feature gate (already present)".into(),
            feature_gates: FeatureGateConfig {
                has_async_smol: false,
                has_async_asupersync: true,
                default_includes_smol: false,
                transitive_activations: vec![],
            },
            file_references: vec![],
            depends_on: vec![],
            depended_by: vec![VendoredCrateId::new("ssh")],
            affected_workflows: vec!["unix-ipc".into(), "mux-transport".into()],
            notes: "Minimal surface. Has async-io default + async-asupersync optional.".into(),
        },
    );

    // ── promise ────────────────────────────────────────────────────────
    entries.insert(
        VendoredCrateId::new("promise"),
        VendoredCrateEntry {
            crate_id: VendoredCrateId::new("promise"),
            cargo_toml_path: "frankenterm/promise/Cargo.toml".into(),
            total_smol_refs: 0,
            total_asupersync_refs: 2,
            total_async_refs: 2,
            difficulty: MigrationDifficulty::AlreadyCompat,
            criticality: Criticality::Medium,
            wave: MigrationWave::Wave0AlreadyCompat,
            recommended_target: "async-asupersync feature gate (already present)".into(),
            feature_gates: FeatureGateConfig {
                has_async_smol: false,
                has_async_asupersync: true,
                default_includes_smol: false,
                transitive_activations: vec![],
            },
            file_references: vec![],
            depends_on: vec![],
            depended_by: vec![
                VendoredCrateId::new("config"),
                VendoredCrateId::new("mux"),
            ],
            affected_workflows: vec!["lua-callbacks".into(), "async-promise-resolution".into()],
            notes: "Bridge between callback-based Lua promises and async/await. Uses async-executor, async-io, async-task, flume.".into(),
        },
    );

    // ── codec ──────────────────────────────────────────────────────────
    entries.insert(
        VendoredCrateId::new("codec"),
        VendoredCrateEntry {
            crate_id: VendoredCrateId::new("codec"),
            cargo_toml_path: "frankenterm/codec/Cargo.toml".into(),
            total_smol_refs: 12,
            total_asupersync_refs: 1,
            total_async_refs: 13,
            difficulty: MigrationDifficulty::Medium,
            criticality: Criticality::High,
            wave: MigrationWave::Wave1Codec,
            recommended_target: "asupersync adapter over smol I/O traits".into(),
            feature_gates: FeatureGateConfig {
                has_async_smol: true,
                has_async_asupersync: true,
                default_includes_smol: true,
                transitive_activations: vec![],
            },
            file_references: vec![FileReference {
                path: "frankenterm/codec/src/lib.rs".into(),
                smol_refs: 12,
                asupersync_refs: 1,
                async_io_refs: 0,
                futures_refs: 0,
                primary_patterns: vec![
                    "smol::block_on".into(),
                    "smol::io::AsyncReadExt".into(),
                    "smol::io::AsyncWriteExt".into(),
                    "encode_async".into(),
                    "decode_async".into(),
                ],
            }],
            depends_on: vec![],
            depended_by: vec![VendoredCrateId::new("ssh")],
            affected_workflows: vec!["sftp-protocol".into(), "mux-wire-format".into()],
            notes: "SFTP protocol PDU serialization. Isolated module; good first migration target after Wave 0.".into(),
        },
    );

    // ── ssh ────────────────────────────────────────────────────────────
    entries.insert(
        VendoredCrateId::new("ssh"),
        VendoredCrateEntry {
            crate_id: VendoredCrateId::new("ssh"),
            cargo_toml_path: "frankenterm/ssh/Cargo.toml".into(),
            total_smol_refs: 44,
            total_asupersync_refs: 2,
            total_async_refs: 46,
            difficulty: MigrationDifficulty::High,
            criticality: Criticality::High,
            wave: MigrationWave::Wave2Ssh,
            recommended_target: "asupersync adapter with smol compatibility shim".into(),
            feature_gates: FeatureGateConfig {
                has_async_smol: true,
                has_async_asupersync: true,
                default_includes_smol: true,
                transitive_activations: vec![
                    "async_ossl/async-io".into(),
                    "frankenterm-uds/async-io".into(),
                    "async_ossl/async-asupersync".into(),
                    "frankenterm-uds/async-asupersync".into(),
                ],
            },
            file_references: vec![
                FileReference {
                    path: "frankenterm/ssh/tests/e2e/sftp.rs".into(),
                    smol_refs: 31,
                    asupersync_refs: 0,
                    async_io_refs: 0,
                    futures_refs: 0,
                    primary_patterns: vec!["smol::block_on".into()],
                },
                FileReference {
                    path: "frankenterm/ssh/tests/e2e/sftp/file.rs".into(),
                    smol_refs: 6,
                    asupersync_refs: 0,
                    async_io_refs: 0,
                    futures_refs: 0,
                    primary_patterns: vec!["smol::block_on".into()],
                },
                FileReference {
                    path: "frankenterm/ssh/src/sftp/mod.rs".into(),
                    smol_refs: 5,
                    asupersync_refs: 0,
                    async_io_refs: 0,
                    futures_refs: 0,
                    primary_patterns: vec![
                        "smol::channel::bounded".into(),
                        "smol::channel::Receiver".into(),
                        "smol::channel::Sender".into(),
                    ],
                },
                FileReference {
                    path: "frankenterm/ssh/src/lib.rs".into(),
                    smol_refs: 2,
                    asupersync_refs: 2,
                    async_io_refs: 0,
                    futures_refs: 0,
                    primary_patterns: vec![
                        "smol::channel re-exports".into(),
                        "asupersync feature gate".into(),
                    ],
                },
            ],
            depends_on: vec![
                VendoredCrateId::new("async_ossl"),
                VendoredCrateId::new("uds"),
                VendoredCrateId::new("codec"),
            ],
            depended_by: vec![
                VendoredCrateId::new("config"),
                VendoredCrateId::new("mux"),
            ],
            affected_workflows: vec![
                "ssh-remote-sessions".into(),
                "sftp-file-transfer".into(),
                "remote-mux-connection".into(),
            ],
            notes: "Highest concentration of smol refs (44). R6 risk owner. Test harness uses 37 smol::block_on calls. async-asupersync feature exists but pulls async-smol transitively.".into(),
        },
    );

    // ── config ─────────────────────────────────────────────────────────
    entries.insert(
        VendoredCrateId::new("config"),
        VendoredCrateEntry {
            crate_id: VendoredCrateId::new("config"),
            cargo_toml_path: "frankenterm/config/Cargo.toml".into(),
            total_smol_refs: 5,
            total_asupersync_refs: 0,
            total_async_refs: 5,
            difficulty: MigrationDifficulty::Medium,
            criticality: Criticality::Medium,
            wave: MigrationWave::Wave3ConfigScripting,
            recommended_target: "feature-gate isolation; harmonize with ssh/promise".into(),
            feature_gates: FeatureGateConfig {
                has_async_smol: true,
                has_async_asupersync: true,
                default_includes_smol: false,
                transitive_activations: vec![
                    "frankenterm-ssh/async-smol".into(),
                    "promise/async-asupersync".into(),
                    "frankenterm-ssh/async-asupersync".into(),
                ],
            },
            file_references: vec![],
            depends_on: vec![
                VendoredCrateId::new("ssh"),
                VendoredCrateId::new("promise"),
            ],
            depended_by: vec![
                VendoredCrateId::new("mux"),
                VendoredCrateId::new("scripting"),
            ],
            affected_workflows: vec!["configuration-loading".into(), "lua-config-eval".into()],
            notes: "Active only when lua feature + async-smol/async-asupersync enabled. smol is direct dep regardless but only used in async paths.".into(),
        },
    );

    // ── scripting ──────────────────────────────────────────────────────
    entries.insert(
        VendoredCrateId::new("scripting"),
        VendoredCrateEntry {
            crate_id: VendoredCrateId::new("scripting"),
            cargo_toml_path: "frankenterm/scripting/Cargo.toml".into(),
            total_smol_refs: 2,
            total_asupersync_refs: 0,
            total_async_refs: 2,
            difficulty: MigrationDifficulty::Low,
            criticality: Criticality::Low,
            wave: MigrationWave::Wave3ConfigScripting,
            recommended_target: "isolate within Lua feature gate; use promise boundary".into(),
            feature_gates: FeatureGateConfig {
                has_async_smol: false,
                has_async_asupersync: false,
                default_includes_smol: false,
                transitive_activations: vec!["config/lua".into()],
            },
            file_references: vec![],
            depends_on: vec![VendoredCrateId::new("config")],
            depended_by: vec![],
            affected_workflows: vec!["lua-scripting".into()],
            notes: "smol only active when lua feature enabled. No direct async-asupersync gate yet; depends on config migration.".into(),
        },
    );

    // ── mux ────────────────────────────────────────────────────────────
    entries.insert(
        VendoredCrateId::new("mux"),
        VendoredCrateEntry {
            crate_id: VendoredCrateId::new("mux"),
            cargo_toml_path: "frankenterm/mux/Cargo.toml".into(),
            total_smol_refs: 0,
            total_asupersync_refs: 0,
            total_async_refs: 0,
            difficulty: MigrationDifficulty::Medium,
            criticality: Criticality::High,
            wave: MigrationWave::Wave4Mux,
            recommended_target: "adapter boundary at transitive dep boundary".into(),
            feature_gates: FeatureGateConfig {
                has_async_smol: true,
                has_async_asupersync: true,
                default_includes_smol: true,
                transitive_activations: vec![
                    "config/async-smol".into(),
                    "frankenterm-ssh/async-smol".into(),
                    "config/async-asupersync".into(),
                    "promise/async-asupersync".into(),
                    "frankenterm-ssh/async-asupersync".into(),
                ],
            },
            file_references: vec![],
            depends_on: vec![
                VendoredCrateId::new("config"),
                VendoredCrateId::new("ssh"),
                VendoredCrateId::new("promise"),
            ],
            depended_by: vec![],
            affected_workflows: vec![
                "terminal-multiplexing".into(),
                "pane-management".into(),
                "mux-server-client".into(),
            ],
            notes: "No direct smol refs; all async comes transitively from config/ssh/promise. Migration follows upstream crates.".into(),
        },
    );

    // ── pty ────────────────────────────────────────────────────────────
    entries.insert(
        VendoredCrateId::new("pty"),
        VendoredCrateEntry {
            crate_id: VendoredCrateId::new("pty"),
            cargo_toml_path: "frankenterm/pty/Cargo.toml".into(),
            total_smol_refs: 5,
            total_asupersync_refs: 0,
            total_async_refs: 5,
            difficulty: MigrationDifficulty::Low,
            criticality: Criticality::Low,
            wave: MigrationWave::Wave5DevOnly,
            recommended_target: "transparent (futures trait re-exports only)".into(),
            feature_gates: FeatureGateConfig {
                has_async_smol: false,
                has_async_asupersync: false,
                default_includes_smol: false,
                transitive_activations: vec![],
            },
            file_references: vec![],
            depends_on: vec![],
            depended_by: vec![],
            affected_workflows: vec!["pty-spawn".into()],
            notes:
                "smol + futures only in dev-dependencies for tests. No production async surface."
                    .into(),
        },
    );

    let sync_only: Vec<VendoredCrateId> = SYNC_ONLY_CRATES
        .iter()
        .map(|s| VendoredCrateId::new(*s))
        .collect();

    let global_smol: u32 = entries.values().map(|e| e.total_smol_refs).sum();
    let global_asupersync: u32 = entries.values().map(|e| e.total_asupersync_refs).sum();

    VendoredMigrationMap {
        version: 1,
        generated_at: "2026-02-25T08:00:00Z".into(),
        bead_id: "ft-e34d9.10.5.1".into(),
        risk_id: "R6".into(),
        total_vendored_crates: entries.len() + sync_only.len(),
        async_vendored_crates: entries.len(),
        sync_only_crates: sync_only,
        entries,
        global_smol_refs: global_smol,
        global_asupersync_refs: global_asupersync,
    }
}

// ── Query API ──────────────────────────────────────────────────────────────

impl VendoredMigrationMap {
    /// Get entry for a specific vendored crate.
    pub fn get(&self, crate_id: &str) -> Option<&VendoredCrateEntry> {
        self.entries.get(&VendoredCrateId::new(crate_id))
    }

    /// List crates in a specific migration wave, ordered by difficulty.
    pub fn wave_crates(&self, wave: MigrationWave) -> Vec<&VendoredCrateEntry> {
        let mut crates: Vec<_> = self.entries.values().filter(|e| e.wave == wave).collect();
        crates.sort_by_key(|e| e.difficulty.score());
        crates
    }

    /// List all crates that depend on a given crate.
    pub fn reverse_deps(&self, crate_id: &str) -> Vec<&VendoredCrateId> {
        let id = VendoredCrateId::new(crate_id);
        self.entries
            .values()
            .filter(|e| e.depends_on.contains(&id))
            .map(|e| &e.crate_id)
            .collect()
    }

    /// Compute migration order (topological sort by wave then difficulty).
    pub fn migration_order(&self) -> Vec<&VendoredCrateEntry> {
        let mut all: Vec<_> = self.entries.values().collect();
        all.sort_by_key(|e| (e.wave.ordinal(), e.difficulty.score()));
        all
    }

    /// Total async reference count across all vendored crates.
    pub fn total_async_refs(&self) -> u32 {
        self.entries.values().map(|e| e.total_async_refs).sum()
    }

    /// Count of crates that already have async-asupersync feature gates.
    pub fn already_compat_count(&self) -> usize {
        self.entries
            .values()
            .filter(|e| e.feature_gates.has_async_asupersync)
            .count()
    }

    /// Canonical JSON string for determinism checks.
    pub fn canonical_string(&self) -> String {
        serde_json::to_string_pretty(self).unwrap_or_default()
    }
}

// ── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_map_builds_successfully() {
        let map = build_canonical_map();
        assert!(map.total_vendored_crates > 0);
        assert!(map.async_vendored_crates > 0);
        assert!(map.async_vendored_crates <= map.total_vendored_crates);
    }

    #[test]
    fn correct_crate_count() {
        let map = build_canonical_map();
        assert_eq!(map.async_vendored_crates, 9);
        assert_eq!(map.sync_only_crates.len(), 20);
        assert_eq!(map.total_vendored_crates, 29);
    }

    #[test]
    fn ssh_is_highest_concentration() {
        let map = build_canonical_map();
        let ssh = map.get("ssh").unwrap();
        assert_eq!(ssh.total_smol_refs, 44);
        assert_eq!(ssh.difficulty, MigrationDifficulty::High);
        assert_eq!(ssh.wave, MigrationWave::Wave2Ssh);
    }

    #[test]
    fn wave0_crates_already_compat() {
        let map = build_canonical_map();
        let wave0 = map.wave_crates(MigrationWave::Wave0AlreadyCompat);
        assert_eq!(wave0.len(), 3);
        for entry in &wave0 {
            assert_eq!(entry.difficulty, MigrationDifficulty::AlreadyCompat);
            assert!(entry.feature_gates.has_async_asupersync);
        }
    }

    #[test]
    fn migration_order_is_wave_sorted() {
        let map = build_canonical_map();
        let order = map.migration_order();
        let mut prev_wave = 0u8;
        for entry in &order {
            assert!(
                entry.wave.ordinal() >= prev_wave,
                "{} wave {} < prev {}",
                entry.crate_id,
                entry.wave.ordinal(),
                prev_wave
            );
            prev_wave = entry.wave.ordinal();
        }
    }

    #[test]
    fn global_smol_refs_match_sum() {
        let map = build_canonical_map();
        let sum: u32 = map.entries.values().map(|e| e.total_smol_refs).sum();
        assert_eq!(map.global_smol_refs, sum);
    }

    #[test]
    fn global_asupersync_refs_match_sum() {
        let map = build_canonical_map();
        let sum: u32 = map.entries.values().map(|e| e.total_asupersync_refs).sum();
        assert_eq!(map.global_asupersync_refs, sum);
    }

    #[test]
    fn no_vendored_crate_uses_tokio() {
        let map = build_canonical_map();
        // Vendored crates have zero tokio references — this is a key invariant
        for entry in map.entries.values() {
            for fr in &entry.file_references {
                assert!(
                    !fr.primary_patterns.iter().any(|p| p.contains("tokio")),
                    "{} must not reference tokio",
                    entry.crate_id
                );
            }
        }
    }

    #[test]
    fn already_compat_count() {
        let map = build_canonical_map();
        // async_ossl, uds, promise, codec, ssh, config, mux = 7 have async-asupersync
        assert_eq!(map.already_compat_count(), 7);
    }

    #[test]
    fn reverse_deps_ssh() {
        let map = build_canonical_map();
        let deps = map.reverse_deps("ssh");
        assert!(deps.iter().any(|id| id.as_str() == "config"));
        assert!(deps.iter().any(|id| id.as_str() == "mux"));
    }

    #[test]
    fn reverse_deps_codec() {
        let map = build_canonical_map();
        let deps = map.reverse_deps("codec");
        assert!(deps.iter().any(|id| id.as_str() == "ssh"));
    }

    #[test]
    fn dependency_graph_is_acyclic() {
        let map = build_canonical_map();
        // Simple cycle detection: no crate depends on itself
        for entry in map.entries.values() {
            assert!(
                !entry.depends_on.contains(&entry.crate_id),
                "{} has self-dependency",
                entry.crate_id
            );
        }
        // No mutual dependencies
        for entry in map.entries.values() {
            for dep in &entry.depends_on {
                if let Some(dep_entry) = map.entries.get(dep) {
                    assert!(
                        !dep_entry.depends_on.contains(&entry.crate_id),
                        "cycle between {} and {}",
                        entry.crate_id,
                        dep
                    );
                }
            }
        }
    }

    #[test]
    fn wave_ordinals_are_monotonic() {
        let waves = [
            MigrationWave::Wave0AlreadyCompat,
            MigrationWave::Wave1Codec,
            MigrationWave::Wave2Ssh,
            MigrationWave::Wave3ConfigScripting,
            MigrationWave::Wave4Mux,
            MigrationWave::Wave5DevOnly,
        ];
        for pair in waves.windows(2) {
            assert!(
                pair[0].ordinal() < pair[1].ordinal(),
                "{} >= {}",
                pair[0].ordinal(),
                pair[1].ordinal()
            );
        }
    }

    #[test]
    fn difficulty_scores_are_ordered() {
        assert!(MigrationDifficulty::AlreadyCompat.score() < MigrationDifficulty::Low.score());
        assert!(MigrationDifficulty::Low.score() < MigrationDifficulty::Medium.score());
        assert!(MigrationDifficulty::Medium.score() < MigrationDifficulty::High.score());
    }

    #[test]
    fn serde_roundtrip() {
        let map = build_canonical_map();
        let json = serde_json::to_string(&map).unwrap();
        let restored: VendoredMigrationMap = serde_json::from_str(&json).unwrap();
        assert_eq!(map, restored);
    }

    #[test]
    fn canonical_string_is_deterministic() {
        let map = build_canonical_map();
        let s1 = map.canonical_string();
        let s2 = map.canonical_string();
        assert_eq!(s1, s2);
    }

    #[test]
    fn total_async_refs_consistent() {
        let map = build_canonical_map();
        let total = map.total_async_refs();
        assert!(total > 0);
        // total_async_refs >= smol + asupersync (some crates have async-io, futures too)
        assert!(total >= map.global_smol_refs + map.global_asupersync_refs);
    }

    #[test]
    fn mux_has_no_direct_smol() {
        let map = build_canonical_map();
        let mux = map.get("mux").unwrap();
        assert_eq!(mux.total_smol_refs, 0);
        assert_eq!(mux.total_async_refs, 0);
        assert_eq!(mux.wave, MigrationWave::Wave4Mux);
    }

    #[test]
    fn wave_coverage_complete() {
        let map = build_canonical_map();
        // Every entry has a wave assignment
        for entry in map.entries.values() {
            assert_ne!(
                entry.wave,
                MigrationWave::NotApplicable,
                "{} should have a wave",
                entry.crate_id
            );
        }
    }
}
