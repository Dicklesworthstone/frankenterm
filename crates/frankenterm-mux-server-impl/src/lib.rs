use anyhow::Context;
use base64::Engine as _;
use config::{ConfigHandle, SshMultiplexing};
use frankenterm_client::domain::{ClientDomain, ClientDomainConfig};
use mux::{DomainRegistrationError, Mux};
use mux::domain::{Domain, LocalDomain};
use mux::ssh::RemoteSshDomain;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::{Arc, MutexGuard};
use zeroize::Zeroizing;

pub mod delivery_ledger;
pub mod delivery_scheduler;
pub mod guardian_output_keys;
pub mod dispatch;
pub mod local;
pub mod pki;
pub mod sessionhandler;

#[cfg(test)]
pub(crate) struct RecoveringTestMutex(std::sync::Mutex<()>);

#[cfg(test)]
impl RecoveringTestMutex {
    const fn new() -> Self {
        Self(std::sync::Mutex::new(()))
    }

    fn lock(&self) -> std::sync::LockResult<std::sync::MutexGuard<'_, ()>> {
        match self.0.lock() {
            Ok(guard) => Ok(guard),
            Err(poisoned) => {
                let guard = poisoned.into_inner();
                self.0.clear_poison();
                Ok(guard)
            }
        }
    }
}

#[cfg(test)]
pub(crate) static GLOBAL_STATE_TEST_LOCK: RecoveringTestMutex = RecoveringTestMutex::new();

#[cfg(not(test))]
const LIVE_SCROLLBACK_COMPACT_MIN_STALE_BYTES: u64 = 4 * 1024 * 1024;
#[cfg(test)]
const LIVE_SCROLLBACK_COMPACT_MIN_STALE_BYTES: u64 = 1;
const LIVE_SCROLLBACK_LINE_RECORD_V1_UNCOMPRESSED: &str = "ftsl1u:";
const LIVE_SCROLLBACK_LINE_RECORD_V1_ZSTD: &str = "ftsl1z:";
const LIVE_SCROLLBACK_LINE_RECORD_V2_UNCOMPRESSED: &str = "ftsl2u:";
const LIVE_SCROLLBACK_LINE_RECORD_V2_ZSTD: &str = "ftsl2z:";
#[cfg(test)]
const LIVE_SCROLLBACK_LINE_COMPRESS_MIN_BYTES: usize = 256;
const LIVE_SCROLLBACK_MAX_DECODED_LINE_BYTES: u64 = 16 * 1024 * 1024;
const LIVE_SCROLLBACK_MAX_DECODED_LINE_BYTES_USIZE: usize = 16 * 1024 * 1024;
const LIVE_SCROLLBACK_MANIFEST_SCHEMA_V1: &str = "frankenterm.live-scrollback-manifest.v1";
const LIVE_SCROLLBACK_MANIFEST_SCHEMA_V2: &str = "frankenterm.live-scrollback-manifest.v2";
const LIVE_SCROLLBACK_MANIFEST_SCHEMA_V3: &str = "frankenterm.live-scrollback-manifest.v3";
const LIVE_SCROLLBACK_MANIFEST_SCHEMA_V4: &str = "frankenterm.live-scrollback-manifest.v4";
const LIVE_SCROLLBACK_REPLACEMENT_MAX_COMMITTED_BYTES: u64 = 1024 * 1024 * 1024;
const LIVE_SCROLLBACK_MAX_SEQUENCE_JOURNAL_BYTES: u64 = 1024 * 1024;
const LIVE_SCROLLBACK_REPLACEMENT_LEDGER_DOMAIN: &[u8] =
    b"frankenterm.live-scrollback-replacement-ledger.v1\0";
const LIVE_SCROLLBACK_CLEAR_EPOCH_DOMAIN: &[u8] =
    b"frankenterm.live-scrollback-clear-epoch.v1\0";
const LIVE_SCROLLBACK_LOGICAL_LEDGER_DIGEST_DOMAIN: &[u8] =
    b"frankenterm.live-scrollback-logical-ledger.v3\0";
const LIVE_SCROLLBACK_INCREMENTAL_CHAIN_DOMAIN: &[u8] =
    b"frankenterm.live-scrollback-incremental-chain.v4\0";
const LIVE_SCROLLBACK_APPEND_WAL_SCHEMA_V1: &str =
    "frankenterm.live-scrollback-append-wal.v1";
const LIVE_SCROLLBACK_APPEND_WAL_SCHEMA_V2: &str =
    "frankenterm.live-scrollback-append-wal.v2";
const LIVE_SCROLLBACK_APPEND_WAL_NAME: &str = ".append-wal.v1.json";
const LIVE_SCROLLBACK_APPEND_WAL_STAGE_NAME: &str = ".append-wal.v1.installing";
const LIVE_SCROLLBACK_APPEND_WAL_RECORD_DIGEST_DOMAIN: &[u8] =
    b"frankenterm.live-scrollback-append-wal-record.v1\0";
const LIVE_SCROLLBACK_APPEND_WAL_TARGET_DIGEST_DOMAIN: &[u8] =
    b"frankenterm.live-scrollback-append-wal-target.v1\0";
const LIVE_SCROLLBACK_APPEND_WAL_MAX_BYTES: u64 = 32 * 1024 * 1024;
const LIVE_SCROLLBACK_MANIFEST_MAX_BYTES: u64 = 1024 * 1024;
const LIVE_SCROLLBACK_MUTATION_LOCK_NAME: &str = ".mutation-lock.v3";

#[cfg(test)]
std::thread_local! {
    static LIVE_SCROLLBACK_AUTHORITY_RECORD_READS: std::cell::Cell<u64> = const {
        std::cell::Cell::new(0)
    };
}

fn configured_ssh_domains(config: &ConfigHandle) -> Vec<config::SshDomain> {
    config
        .ssh_domains()
        .into_iter()
        .map(|mut domain| {
            // Freeze the top-level fallback into each exact generation.
            // Otherwise a reload that changes only `ssh_backend` leaves
            // `None == None` in the domain snapshot while changing the
            // effective transport used by both raw and multiplexed SSH.
            domain.ssh_backend.get_or_insert(config.ssh_backend);
            domain
        })
        .collect()
}

pub fn configured_client_domains(config: &config::ConfigHandle) -> Vec<ClientDomainConfig> {
    let mut domains = vec![];
    for unix_dom in &config.unix_domains {
        domains.push(ClientDomainConfig::Unix(unix_dom.clone()));
    }

    for ssh_dom in configured_ssh_domains(config) {
        if ssh_dom.multiplexing == SshMultiplexing::WezTerm {
            domains.push(ClientDomainConfig::Ssh(ssh_dom));
        }
    }

    for tls_client in &config.tls_clients {
        domains.push(ClientDomainConfig::Tls(tls_client.clone()));
    }
    domains
}

#[derive(Clone)]
enum ConfiguredRawDomain {
    Ssh(config::SshDomain),
    Wsl(config::WslDomain),
    Exec(config::ExecDomain),
    Serial(config::SerialDomain),
}

impl ConfiguredRawDomain {
    fn name(&self) -> &str {
        match self {
            Self::Ssh(domain) => &domain.name,
            Self::Wsl(domain) => &domain.name,
            Self::Exec(domain) => &domain.name,
            Self::Serial(domain) => &domain.name,
        }
    }

    fn kind(&self) -> &'static str {
        match self {
            Self::Ssh(_) => "raw SSH",
            Self::Wsl(_) => "WSL",
            Self::Exec(_) => "exec",
            Self::Serial(_) => "serial",
        }
    }

    fn matches_registration(&self, registered: &mux::DomainOperationGuard) -> bool {
        match self {
            Self::Ssh(expected) => registered
                .downcast_ref::<RemoteSshDomain>()
                .is_some_and(|current| current.matches_configuration(expected)),
            Self::Wsl(expected) => registered
                .downcast_ref::<LocalDomain>()
                .is_some_and(|current| current.matches_wsl_configuration(expected)),
            Self::Exec(expected) => registered
                .downcast_ref::<LocalDomain>()
                .is_some_and(|current| current.matches_exec_configuration(expected)),
            Self::Serial(expected) => registered
                .downcast_ref::<LocalDomain>()
                .is_some_and(|current| current.matches_serial_configuration(expected)),
        }
    }

    fn instantiate(&self) -> anyhow::Result<Arc<dyn Domain>> {
        match self {
            Self::Ssh(domain) => Ok(Arc::new(RemoteSshDomain::with_configured_ssh_domain(
                domain,
            )?)),
            Self::Wsl(domain) => Ok(Arc::new(LocalDomain::new_wsl(domain.clone())?)),
            Self::Exec(domain) => Ok(Arc::new(LocalDomain::new_exec_domain(domain.clone())?)),
            Self::Serial(domain) => {
                Ok(Arc::new(LocalDomain::new_configured_serial_domain(
                    domain.clone(),
                )?))
            }
        }
    }
}

#[derive(Default)]
struct ConfiguredRawDomains {
    ordered: Vec<ConfiguredRawDomain>,
    by_name: BTreeMap<String, usize>,
}

impl ConfiguredRawDomains {
    fn insert(&mut self, domain: ConfiguredRawDomain) -> anyhow::Result<()> {
        let name = domain.name().to_string();
        anyhow::ensure!(
            !self.by_name.contains_key(&name),
            "configured domain name {name:?} is duplicated across raw transport entries"
        );
        let index = self.ordered.len();
        self.ordered.push(domain);
        self.by_name.insert(name, index);
        Ok(())
    }

    fn contains_key(&self, name: &str) -> bool {
        self.by_name.contains_key(name)
    }

    fn get(&self, name: &str) -> Option<&ConfiguredRawDomain> {
        self.by_name
            .get(name)
            .and_then(|index| self.ordered.get(*index))
    }

    fn iter(&self) -> impl Iterator<Item = &ConfiguredRawDomain> {
        self.ordered.iter()
    }
}

fn configured_raw_domains(config: &config::ConfigHandle) -> anyhow::Result<ConfiguredRawDomains> {
    let mut domains = ConfiguredRawDomains::default();
    for ssh in configured_ssh_domains(config) {
        if ssh.multiplexing == SshMultiplexing::None {
            domains.insert(ConfiguredRawDomain::Ssh(ssh))?;
        }
    }
    for wsl in config.wsl_domains() {
        domains.insert(ConfiguredRawDomain::Wsl(wsl))?;
    }
    for exec in &config.exec_domains {
        domains.insert(ConfiguredRawDomain::Exec(exec.clone()))?;
    }
    for serial in &config.serial_ports {
        domains.insert(ConfiguredRawDomain::Serial(serial.clone()))?;
    }
    Ok(domains)
}

fn is_configured_raw_registration(registered: &mux::DomainOperationGuard) -> bool {
    registered
        .downcast_ref::<RemoteSshDomain>()
        .is_some_and(RemoteSshDomain::is_configuration_owned)
        || registered
            .downcast_ref::<LocalDomain>()
            .is_some_and(LocalDomain::is_configuration_owned)
}

fn retire_configured_registration(
    mux: &Arc<Mux>,
    registered: &mux::DomainOperationGuard,
) -> anyhow::Result<()> {
    let domain_id = registered.domain_id();
    let domain_name = registered.domain_name().to_string();
    if let Some(client) = registered.downcast_ref::<ClientDomain>() {
        client.perform_detach();
    } else {
        anyhow::ensure!(
            is_configured_raw_registration(registered),
            "refusing to retire runtime-owned domain {domain_name:?} during configuration reconciliation"
        );
        if mux.domain_was_detached_if_guard(registered) {
            return Ok(());
        }
    }

    match mux.get_domain_by_name(&domain_name) {
        None => Ok(()),
        Some(current) if current.domain_id() != domain_id => Ok(()),
        Some(_) => anyhow::bail!(
            "configured domain {domain_name:?} remained live after exact-generation retirement"
        ),
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConfiguredClientDomainReconcileOutcome {
    Current,
    Registered,
    PendingRetirement,
    NotConfigured,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MuxDomainUpdateOutcome {
    Converged,
    PendingRetirements { domain_names: Vec<String> },
}

impl MuxDomainUpdateOutcome {
    #[must_use]
    pub const fn is_converged(&self) -> bool {
        matches!(self, Self::Converged)
    }

    #[must_use]
    pub fn pending_retirements(&self) -> &[String] {
        match self {
            Self::Converged => &[],
            Self::PendingRetirements { domain_names } => domain_names,
        }
    }
}

/// Make one configured client-domain name addressable by this exact mux.
///
/// A changed configuration first retires the old exact registration. Mux
/// retirement is deliberately asynchronous while admitted operations drain,
/// so callers must treat `PendingRetirement` as retryable and invoke this
/// function again before dialing. `NotConfigured` is terminal for an old retry
/// snapshot and must never recreate a domain removed by a configuration reload.
pub fn reconcile_configured_client_domain(
    config: &config::ConfigHandle,
    mux: &Arc<Mux>,
    domain_name: &str,
) -> anyhow::Result<ConfiguredClientDomainReconcileOutcome> {
    let Some(expected) = configured_client_domains(config)
        .into_iter()
        .find(|candidate| candidate.name() == domain_name)
    else {
        return Ok(ConfiguredClientDomainReconcileOutcome::NotConfigured);
    };

    reconcile_client_domain_config(mux, &expected)
}

pub fn reconcile_client_domain_config(
    mux: &Arc<Mux>,
    expected: &ClientDomainConfig,
) -> anyhow::Result<ConfiguredClientDomainReconcileOutcome> {
    let domain_name = expected.name();

    if let Some(registered) = mux.get_domain_by_name(domain_name) {
        if let Some(client) = registered.downcast_ref::<ClientDomain>() {
            if client.reconcile_configuration(expected) {
                return Ok(ConfiguredClientDomainReconcileOutcome::Current);
            }

            // `perform_detach` retires only this admitted registration. A newer
            // same-name generation cannot be removed by the stale guard.
            retire_configured_registration(mux, &registered)?;
            return Ok(ConfiguredClientDomainReconcileOutcome::PendingRetirement);
        }

        if is_configured_raw_registration(&registered) {
            retire_configured_registration(mux, &registered)?;
            return Ok(ConfiguredClientDomainReconcileOutcome::PendingRetirement);
        }

        anyhow::bail!(
            "configured client domain {domain_name:?} collides with a live runtime-owned domain"
        );
    }

    let domain: Arc<dyn Domain> = Arc::new(ClientDomain::new(expected.clone(), mux)?);
    match mux.add_domain(&domain) {
        Ok(()) => Ok(ConfiguredClientDomainReconcileOutcome::Registered),
        Err(
            DomainRegistrationError::RetiredIdentifier { .. }
            | DomainRegistrationError::IdentifierInUse { .. }
            | DomainRegistrationError::NameInUse { .. },
        ) => Ok(ConfiguredClientDomainReconcileOutcome::PendingRetirement),
        Err(error) => Err(anyhow::Error::new(error)),
    }
}

struct LiveScrollbackSpillSink {
    pane_id: u64,
    active_ledger_pane_id: std::sync::atomic::AtomicU64,
    durable_pane_id: [u8; 16],
    source_pane_id: usize,
    source_domain_id: usize,
    command_description: String,
    manifest_path: PathBuf,
    mutation_gate: std::sync::Mutex<()>,
    store: std::sync::Mutex<frankenterm_core::storage::mmap_store::MmapScrollbackStore>,
    state: std::sync::Mutex<LiveScrollbackSpillState>,
    keyring: Arc<std::sync::Mutex<guardian_output_keys::GuardianOutputKeyring>>,
}

impl std::fmt::Debug for LiveScrollbackSpillSink {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("LiveScrollbackSpillSink")
            .field("pane_id", &self.pane_id)
            .field("source_pane_id", &self.source_pane_id)
            .field("source_domain_id", &self.source_domain_id)
            .field("durable_pane_id", &"[REDACTED]")
            .field("command_description", &"[REDACTED]")
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Copy)]
struct LiveScrollbackSpillState {
    initial_stable_row: Option<wezterm_term::StableRowIndex>,
    newest_stable_row_exclusive: Option<wezterm_term::StableRowIndex>,
    max_retained_rows: usize,
    content_epoch: [u8; 16],
    revision: u64,
    authenticated_manifest: bool,
    predecessor_generation: Option<wezterm_term::config::ScrollbackSnapshotGeneration>,
    clear_manifest_published: bool,
    clear_pending_physical_reclamation: bool,
    transaction_quarantined: bool,
    /// Private, scan-minted authority for the exact retained interval. The
    /// type has no public constructor: ordinary mutations may only derive a
    /// successor from an already verified predecessor.
    verified_ledger: Option<VerifiedLedgerState>,
}

impl std::fmt::Debug for LiveScrollbackSpillState {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("LiveScrollbackSpillState")
            .field("initial_stable_row", &self.initial_stable_row)
            .field(
                "newest_stable_row_exclusive",
                &self.newest_stable_row_exclusive,
            )
            .field("max_retained_rows", &self.max_retained_rows)
            .field("content_epoch", &"[REDACTED]")
            .field("revision", &self.revision)
            .field("authenticated_manifest", &self.authenticated_manifest)
            .field("predecessor_generation", &self.predecessor_generation)
            .field("clear_manifest_published", &self.clear_manifest_published)
            .field(
                "clear_pending_physical_reclamation",
                &self.clear_pending_physical_reclamation,
            )
            .field("transaction_quarantined", &self.transaction_quarantined)
            .field("verified_ledger", &self.verified_ledger)
            .finish()
    }
}

impl LiveScrollbackSpillState {
    fn empty(content_epoch: [u8; 16], authenticated_manifest: bool) -> Self {
        Self {
            initial_stable_row: None,
            newest_stable_row_exclusive: None,
            max_retained_rows: 0,
            content_epoch,
            revision: 0,
            authenticated_manifest,
            predecessor_generation: None,
            clear_manifest_published: false,
            clear_pending_physical_reclamation: false,
            transaction_quarantined: false,
            verified_ledger: None,
        }
    }

    fn snapshot_generation(&self) -> wezterm_term::config::ScrollbackSnapshotGeneration {
        wezterm_term::config::ScrollbackSnapshotGeneration::new(
            self.content_epoch,
            self.revision,
        )
    }

    fn advance_revision(&mut self) -> Result<(), wezterm_term::config::ScrollbackSpillError> {
        let predecessor = self.snapshot_generation();
        self.revision = self
            .revision
            .checked_add(1)
            .ok_or(wezterm_term::config::ScrollbackSpillError::RevisionExhausted)?;
        self.predecessor_generation = Some(predecessor);
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct LiveScrollbackManifestV1 {
    schema: String,
    publication_state: String,
    durable_pane_id: String,
    source_pane_id: u64,
    source_domain_id: u64,
    command_description: String,
    initial_stable_row: Option<wezterm_term::StableRowIndex>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    newest_stable_row_exclusive: Option<wezterm_term::StableRowIndex>,
    max_retained_rows: u64,
    oldest_seq: Option<u64>,
    retained_rows: u64,
    next_seq: u64,
    content_log: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    content_sequence: Option<String>,
    /// Present and mandatory in schema v2. These optional fields allow a
    /// checksum-preserving read of v1 so it can be validated and immediately
    /// republished as v2 without treating it as generation-aware state.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    content_epoch: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    revision: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    predecessor_content_epoch: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    predecessor_revision: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    retained_record_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    committed_log_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    committed_sequence_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    logical_ledger_sha256: Option<String>,
    /// Schema v4 authenticates an incrementally maintainable retained-range
    /// chain. `anchor` is the commitment immediately before `oldest_seq` and
    /// `tail` is the commitment after the final retained record.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    chain_anchor_sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    chain_tail_sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    guardian_manifest_authentication: Option<String>,
    manifest_sha256: String,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct LiveScrollbackAppendWalV1 {
    schema: String,
    durable_pane_id: String,
    ledger_pane_id: u64,
    predecessor_content_epoch: String,
    predecessor_revision: u64,
    predecessor_manifest_sha256: String,
    target_content_epoch: String,
    target_revision: u64,
    initial_stable_row: wezterm_term::StableRowIndex,
    newest_stable_row_exclusive: wezterm_term::StableRowIndex,
    appended_stable_row: wezterm_term::StableRowIndex,
    appended_sequence: u64,
    max_retained_rows: u64,
    target_oldest_sequence: u64,
    target_next_sequence: u64,
    target_record_count: u64,
    target_retained_record_bytes: u64,
    encrypted_record_bytes: u64,
    encrypted_record_sha256: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    target_record_set_sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    predecessor_chain_anchor_sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    predecessor_chain_tail_sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    target_chain_anchor_sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    target_chain_tail_sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    evicted_record_count: Option<u64>,
    /// A consumed WAL is retained as bounded crash evidence. Replacement and
    /// clear generations advance this authenticated pointer instead of
    /// deleting the evidence or mistaking it for an unconsumed transaction.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    superseding_content_epoch: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    superseding_revision: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    superseding_ledger_pane_id: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    superseding_manifest_sha256: Option<String>,
    encrypted_record: String,
    guardian_authentication: Option<String>,
    wal_sha256: String,
}

impl std::fmt::Debug for LiveScrollbackAppendWalV1 {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("LiveScrollbackAppendWalV1")
            .field("schema", &self.schema)
            .field("durable_pane_id", &"[REDACTED]")
            .field("ledger_pane_id", &self.ledger_pane_id)
            .field("predecessor_content_epoch", &"[REDACTED]")
            .field("predecessor_revision", &self.predecessor_revision)
            .field("predecessor_manifest_sha256", &"[REDACTED]")
            .field("target_content_epoch", &"[REDACTED]")
            .field("target_revision", &self.target_revision)
            .field("initial_stable_row", &self.initial_stable_row)
            .field(
                "newest_stable_row_exclusive",
                &self.newest_stable_row_exclusive,
            )
            .field("appended_stable_row", &self.appended_stable_row)
            .field("appended_sequence", &self.appended_sequence)
            .field("max_retained_rows", &self.max_retained_rows)
            .field("target_oldest_sequence", &self.target_oldest_sequence)
            .field("target_next_sequence", &self.target_next_sequence)
            .field("target_record_count", &self.target_record_count)
            .field(
                "target_retained_record_bytes",
                &self.target_retained_record_bytes,
            )
            .field("encrypted_record_bytes", &self.encrypted_record_bytes)
            .field("encrypted_record_sha256", &"[REDACTED]")
            .field("target_record_set_sha256", &"[REDACTED]")
            .field("predecessor_chain_anchor_sha256", &"[REDACTED]")
            .field("predecessor_chain_tail_sha256", &"[REDACTED]")
            .field("target_chain_anchor_sha256", &"[REDACTED]")
            .field("target_chain_tail_sha256", &"[REDACTED]")
            .field("evicted_record_count", &self.evicted_record_count)
            .field("superseding_content_epoch", &"[REDACTED]")
            .field("superseding_revision", &self.superseding_revision)
            .field(
                "superseding_ledger_pane_id",
                &self.superseding_ledger_pane_id,
            )
            .field("superseding_manifest_sha256", &"[REDACTED]")
            .field("encrypted_record", &"[REDACTED]")
            .field("guardian_authentication", &"[REDACTED]")
            .field("wal_sha256", &"[REDACTED]")
            .finish()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct LiveScrollbackAppendWalSupersession {
    generation: wezterm_term::config::ScrollbackSnapshotGeneration,
    ledger_pane_id: u64,
    manifest_sha256: [u8; 32],
}

/// In-memory proof that one exact retained interval was authenticated by a
/// complete cold scan. Fields are private and the only constructors below
/// either scan every retained record or derive a successor from an existing
/// verified value. This is deliberately not serializable: the signed v4
/// manifest remains the durable authority.
#[derive(Clone, Copy, PartialEq, Eq)]
struct VerifiedLedgerState {
    ledger_pane_id: u64,
    oldest_sequence: Option<u64>,
    next_sequence: u64,
    record_count: u64,
    retained_record_bytes: u64,
    chain_anchor: [u8; 32],
    chain_tail: [u8; 32],
}

impl std::fmt::Debug for VerifiedLedgerState {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("VerifiedLedgerState")
            .field("ledger_pane_id", &self.ledger_pane_id)
            .field("oldest_sequence", &self.oldest_sequence)
            .field("next_sequence", &self.next_sequence)
            .field("record_count", &self.record_count)
            .field("retained_record_bytes", &self.retained_record_bytes)
            .field("chain_anchor", &"[REDACTED]")
            .field("chain_tail", &"[REDACTED]")
            .finish()
    }
}

fn live_scrollback_manifest_is_authenticated(manifest: &LiveScrollbackManifestV1) -> bool {
    matches!(
        manifest.schema.as_str(),
        LIVE_SCROLLBACK_MANIFEST_SCHEMA_V3 | LIVE_SCROLLBACK_MANIFEST_SCHEMA_V4
    )
}

fn live_scrollback_incremental_chain_next(
    predecessor: [u8; 32],
    ledger_pane_id: u64,
    sequence: u64,
    record: &str,
) -> anyhow::Result<[u8; 32]> {
    let mut hasher = Sha256::new();
    hasher.update(LIVE_SCROLLBACK_INCREMENTAL_CHAIN_DOMAIN);
    hasher.update(predecessor);
    hasher.update(ledger_pane_id.to_le_bytes());
    hasher.update(sequence.to_le_bytes());
    hasher.update(
        u64::try_from(record.len())
            .map_err(|_| anyhow::anyhow!("incremental-chain record length exceeds u64"))?
            .to_le_bytes(),
    );
    hasher.update(record.as_bytes());
    Ok(hasher.finalize().into())
}

fn live_scrollback_authority_record_at(
    store: &frankenterm_core::storage::mmap_store::MmapScrollbackStore,
    ledger_pane_id: u64,
    sequence: u64,
) -> anyhow::Result<String> {
    #[cfg(test)]
    LIVE_SCROLLBACK_AUTHORITY_RECORD_READS.with(|reads| reads.set(reads.get().saturating_add(1)));
    store
        .line_at(ledger_pane_id, sequence)?
        .ok_or_else(|| anyhow::anyhow!("authenticated ledger is missing sequence {sequence}"))
}

impl VerifiedLedgerState {
    fn empty(ledger_pane_id: u64) -> Self {
        Self {
            ledger_pane_id,
            oldest_sequence: None,
            next_sequence: 0,
            record_count: 0,
            retained_record_bytes: 0,
            chain_anchor: [0; 32],
            chain_tail: [0; 32],
        }
    }

    fn scan_store(
        ledger_pane_id: u64,
        store: &frankenterm_core::storage::mmap_store::MmapScrollbackStore,
        chain_anchor: [u8; 32],
    ) -> anyhow::Result<Self> {
        let oldest_sequence = store.oldest_seq(ledger_pane_id);
        let next_sequence = store.next_seq(ledger_pane_id)?;
        let record_count = u64::try_from(store.line_count(ledger_pane_id))
            .map_err(|_| anyhow::anyhow!("authenticated ledger row count exceeds u64"))?;
        match (oldest_sequence, record_count) {
            (None, 0) => {}
            (Some(oldest), count) if count != 0 => anyhow::ensure!(
                oldest.checked_add(count) == Some(next_sequence),
                "authenticated ledger sequence interval is not contiguous"
            ),
            _ => anyhow::bail!("authenticated ledger oldest-sequence identity is inconsistent"),
        }

        let mut chain_tail = chain_anchor;
        let mut retained_record_bytes = 0_u64;
        if let Some(oldest) = oldest_sequence {
            for offset in 0..record_count {
                let sequence = oldest
                    .checked_add(offset)
                    .ok_or_else(|| anyhow::anyhow!("authenticated ledger sequence overflows"))?;
                let record =
                    live_scrollback_authority_record_at(store, ledger_pane_id, sequence)?;
                retained_record_bytes = retained_record_bytes
                    .checked_add(u64::try_from(record.len())?)
                    .and_then(|bytes| bytes.checked_add(1))
                    .ok_or_else(|| {
                        anyhow::anyhow!("authenticated ledger retained bytes overflow")
                    })?;
                chain_tail = live_scrollback_incremental_chain_next(
                    chain_tail,
                    ledger_pane_id,
                    sequence,
                    &record,
                )?;
            }
        }
        anyhow::ensure!(
            retained_record_bytes == store.retained_record_bytes(ledger_pane_id),
            "authenticated ledger retained-byte accounting mismatch"
        );
        Ok(Self {
            ledger_pane_id,
            oldest_sequence,
            next_sequence,
            record_count,
            retained_record_bytes,
            chain_anchor,
            chain_tail,
        })
    }

    fn from_records(ledger_pane_id: u64, records: &[String]) -> anyhow::Result<Self> {
        let mut state = Self::empty(ledger_pane_id);
        for (offset, record) in records.iter().enumerate() {
            let sequence = u64::try_from(offset)
                .map_err(|_| anyhow::anyhow!("replacement ledger sequence exceeds u64"))?;
            state.chain_tail = live_scrollback_incremental_chain_next(
                state.chain_tail,
                ledger_pane_id,
                sequence,
                record,
            )?;
            state.retained_record_bytes = state
                .retained_record_bytes
                .checked_add(u64::try_from(record.len())?)
                .and_then(|bytes| bytes.checked_add(1))
                .ok_or_else(|| anyhow::anyhow!("replacement ledger retained bytes overflow"))?;
            state.record_count = state
                .record_count
                .checked_add(1)
                .ok_or_else(|| anyhow::anyhow!("replacement ledger row count overflows"))?;
            state.next_sequence = sequence
                .checked_add(1)
                .ok_or_else(|| anyhow::anyhow!("replacement ledger sequence overflows"))?;
        }
        state.oldest_sequence = (!records.is_empty()).then_some(0);
        Ok(state)
    }

    fn project_append(
        self,
        desired_sequence: u64,
        record: &str,
        max_retained_rows: usize,
        store: &frankenterm_core::storage::mmap_store::MmapScrollbackStore,
    ) -> anyhow::Result<(Self, u64)> {
        anyhow::ensure!(
            self.next_sequence == desired_sequence && max_retained_rows != 0,
            "incremental append predecessor authority is inconsistent"
        );
        let max_retained_rows = u64::try_from(max_retained_rows)?;
        let target_next_sequence = desired_sequence
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("incremental append sequence is exhausted"))?;
        let target_record_count = self
            .record_count
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("incremental append row count overflows"))?
            .min(max_retained_rows);
        let target_oldest_sequence = target_next_sequence
            .checked_sub(target_record_count)
            .ok_or_else(|| anyhow::anyhow!("incremental append range underflows"))?;
        let previous_oldest = self.oldest_sequence.unwrap_or(desired_sequence);
        anyhow::ensure!(
            self.record_count != 0 || self.oldest_sequence.is_none(),
            "incremental append empty predecessor has an oldest sequence"
        );
        let evicted_record_count = target_oldest_sequence
            .checked_sub(previous_oldest)
            .ok_or_else(|| anyhow::anyhow!("incremental append retention moves backwards"))?;

        let mut chain_anchor = self.chain_anchor;
        let mut retained_record_bytes = self
            .retained_record_bytes
            .checked_add(u64::try_from(record.len())?)
            .and_then(|bytes| bytes.checked_add(1))
            .ok_or_else(|| anyhow::anyhow!("incremental append retained bytes overflow"))?;
        for offset in 0..evicted_record_count {
            let sequence = previous_oldest
                .checked_add(offset)
                .ok_or_else(|| anyhow::anyhow!("incremental eviction sequence overflows"))?;
            let evicted =
                live_scrollback_authority_record_at(store, self.ledger_pane_id, sequence)?;
            chain_anchor = live_scrollback_incremental_chain_next(
                chain_anchor,
                self.ledger_pane_id,
                sequence,
                &evicted,
            )?;
            let evicted_bytes = u64::try_from(evicted.len())?
                .checked_add(1)
                .ok_or_else(|| anyhow::anyhow!("incremental eviction bytes overflow"))?;
            retained_record_bytes = retained_record_bytes
                .checked_sub(evicted_bytes)
                .ok_or_else(|| anyhow::anyhow!("incremental eviction byte count underflows"))?;
        }
        let chain_tail = live_scrollback_incremental_chain_next(
            self.chain_tail,
            self.ledger_pane_id,
            desired_sequence,
            record,
        )?;
        Ok((
            Self {
                ledger_pane_id: self.ledger_pane_id,
                oldest_sequence: Some(target_oldest_sequence),
                next_sequence: target_next_sequence,
                record_count: target_record_count,
                retained_record_bytes,
                chain_anchor,
                chain_tail,
            },
            evicted_record_count,
        ))
    }

    fn matches_store_facts(
        self,
        store: &frankenterm_core::storage::mmap_store::MmapScrollbackStore,
    ) -> anyhow::Result<bool> {
        Ok(store.oldest_seq(self.ledger_pane_id) == self.oldest_sequence
            && store.next_seq(self.ledger_pane_id)? == self.next_sequence
            && u64::try_from(store.line_count(self.ledger_pane_id))? == self.record_count
            && store.retained_record_bytes(self.ledger_pane_id) == self.retained_record_bytes)
    }
}

fn live_scrollback_manifest_generation(
    manifest: &LiveScrollbackManifestV1,
) -> anyhow::Result<Option<([u8; 16], u64)>> {
    match manifest.schema.as_str() {
        LIVE_SCROLLBACK_MANIFEST_SCHEMA_V1 => {
            anyhow::ensure!(
                manifest.content_epoch.is_none() && manifest.revision.is_none(),
                "legacy scrollback manifest contains generation fields"
            );
            Ok(None)
        }
        LIVE_SCROLLBACK_MANIFEST_SCHEMA_V2
        | LIVE_SCROLLBACK_MANIFEST_SCHEMA_V3
        | LIVE_SCROLLBACK_MANIFEST_SCHEMA_V4 => {
            let encoded_epoch = manifest
                .content_epoch
                .as_deref()
                .ok_or_else(|| anyhow::anyhow!("scrollback manifest content epoch is missing"))?;
            anyhow::ensure!(
                encoded_epoch.len() == 32
                    && encoded_epoch
                        .bytes()
                        .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f')),
                "scrollback manifest content epoch is not canonical"
            );
            let mut content_epoch = [0u8; 16];
            hex::decode_to_slice(encoded_epoch, &mut content_epoch)
                .map_err(|_| anyhow::anyhow!("scrollback manifest content epoch is invalid"))?;
            anyhow::ensure!(
                content_epoch != [0; 16],
                "scrollback manifest content epoch is invalid"
            );
            let revision = manifest
                .revision
                .ok_or_else(|| anyhow::anyhow!("scrollback manifest revision is missing"))?;
            Ok(Some((content_epoch, revision)))
        }
        _ => anyhow::bail!("unsupported live scrollback manifest schema"),
    }
}

fn live_scrollback_manifest_predecessor(
    manifest: &LiveScrollbackManifestV1,
) -> anyhow::Result<Option<wezterm_term::config::ScrollbackSnapshotGeneration>> {
    match (
        manifest.predecessor_content_epoch.as_deref(),
        manifest.predecessor_revision,
    ) {
        (None, None) => Ok(None),
        (Some(encoded_epoch), Some(revision)) => {
            anyhow::ensure!(
                encoded_epoch.len() == 32
                    && encoded_epoch
                        .bytes()
                        .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f')),
                "scrollback manifest predecessor epoch is not canonical"
            );
            let mut content_epoch = [0; 16];
            hex::decode_to_slice(encoded_epoch, &mut content_epoch)
                .map_err(|_| anyhow::anyhow!("scrollback manifest predecessor epoch is invalid"))?;
            anyhow::ensure!(
                content_epoch != [0; 16],
                "scrollback manifest predecessor epoch is invalid"
            );
            Ok(Some(
                wezterm_term::config::ScrollbackSnapshotGeneration::new(
                    content_epoch,
                    revision,
                ),
            ))
        }
        _ => anyhow::bail!("scrollback manifest predecessor generation is incomplete"),
    }
}

struct LiveScrollbackLogicalLedgerHasher {
    hasher: Sha256,
    next_sequence: Option<u64>,
    declared_next_sequence: u64,
    remaining_records: u64,
}

impl LiveScrollbackLogicalLedgerHasher {
    fn new(
        manifest: &LiveScrollbackManifestV1,
        ledger_pane_id: u64,
        oldest_sequence: Option<u64>,
        next_sequence: u64,
        record_count: u64,
        retained_record_bytes: u64,
        committed_log_bytes: u64,
        committed_sequence_bytes: u64,
    ) -> anyhow::Result<Self> {
        anyhow::ensure!(
            manifest.schema == LIVE_SCROLLBACK_MANIFEST_SCHEMA_V3,
            "logical ledger digest requires a v3 manifest"
        );
        anyhow::ensure!(
            manifest.oldest_seq == oldest_sequence
                && manifest.next_seq == next_sequence
                && manifest.retained_rows == record_count
                && manifest.retained_record_bytes == Some(retained_record_bytes)
                && manifest.committed_log_bytes == Some(committed_log_bytes)
                && manifest.committed_sequence_bytes == Some(committed_sequence_bytes),
            "logical ledger facts disagree with the authenticated manifest"
        );
        match (oldest_sequence, record_count) {
            (None, 0) => {}
            (Some(oldest), count) if count != 0 => {
                anyhow::ensure!(
                    oldest.checked_add(count) == Some(next_sequence),
                    "logical ledger sequence interval is not contiguous"
                );
            }
            _ => anyhow::bail!("logical ledger oldest-sequence identity is inconsistent"),
        }

        let mut durable_pane_id = [0; 16];
        hex::decode_to_slice(&manifest.durable_pane_id, &mut durable_pane_id)
            .map_err(|_| anyhow::anyhow!("logical ledger durable pane identity is invalid"))?;
        let (content_epoch, revision) = live_scrollback_manifest_generation(manifest)?
            .ok_or_else(|| anyhow::anyhow!("logical ledger generation is missing"))?;
        let predecessor = live_scrollback_manifest_predecessor(manifest)?;
        let mut hasher = Sha256::new();
        hasher.update(LIVE_SCROLLBACK_LOGICAL_LEDGER_DIGEST_DOMAIN);
        hasher.update(3_u32.to_le_bytes());
        hasher.update(durable_pane_id);
        hasher.update(ledger_pane_id.to_le_bytes());
        hasher.update(content_epoch);
        hasher.update(revision.to_le_bytes());
        update_scrollback_digest_generation(&mut hasher, predecessor);
        update_scrollback_digest_bytes(&mut hasher, manifest.publication_state.as_bytes())?;
        update_scrollback_digest_stable_row(&mut hasher, manifest.initial_stable_row)?;
        update_scrollback_digest_stable_row(
            &mut hasher,
            manifest.newest_stable_row_exclusive,
        )?;
        hasher.update(manifest.max_retained_rows.to_le_bytes());
        update_scrollback_digest_u64(&mut hasher, oldest_sequence);
        hasher.update(next_sequence.to_le_bytes());
        hasher.update(record_count.to_le_bytes());
        hasher.update(retained_record_bytes.to_le_bytes());
        hasher.update(committed_log_bytes.to_le_bytes());
        hasher.update(committed_sequence_bytes.to_le_bytes());
        Ok(Self {
            hasher,
            next_sequence: oldest_sequence,
            declared_next_sequence: next_sequence,
            remaining_records: record_count,
        })
    }

    fn observe(&mut self, sequence: u64, record: &str) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.remaining_records != 0 && self.next_sequence == Some(sequence),
            "logical ledger record order or sequence is inconsistent"
        );
        self.hasher.update(sequence.to_le_bytes());
        update_scrollback_digest_bytes(&mut self.hasher, record.as_bytes())?;
        self.remaining_records -= 1;
        self.next_sequence = sequence.checked_add(1);
        Ok(())
    }

    fn finish(self) -> anyhow::Result<[u8; 32]> {
        anyhow::ensure!(
            self.remaining_records == 0,
            "logical ledger digest omitted one or more records"
        );
        if self.next_sequence.is_some() {
            anyhow::ensure!(
                self.next_sequence == Some(self.declared_next_sequence),
                "logical ledger digest ended at the wrong sequence"
            );
        }
        Ok(self.hasher.finalize().into())
    }
}

fn update_scrollback_digest_bytes(hasher: &mut Sha256, bytes: &[u8]) -> anyhow::Result<()> {
    hasher.update(
        u64::try_from(bytes.len())
            .map_err(|_| anyhow::anyhow!("logical ledger field length exceeds u64"))?
            .to_le_bytes(),
    );
    hasher.update(bytes);
    Ok(())
}

fn update_scrollback_digest_u64(hasher: &mut Sha256, value: Option<u64>) {
    match value {
        Some(value) => {
            hasher.update([1]);
            hasher.update(value.to_le_bytes());
        }
        None => hasher.update([0]),
    }
}

fn update_scrollback_digest_stable_row(
    hasher: &mut Sha256,
    value: Option<wezterm_term::StableRowIndex>,
) -> anyhow::Result<()> {
    match value {
        Some(value) => {
            hasher.update([1]);
            hasher.update(
                i64::try_from(value)
                    .map_err(|_| anyhow::anyhow!("logical ledger stable row exceeds i64"))?
                    .to_le_bytes(),
            );
        }
        None => hasher.update([0]),
    }
    Ok(())
}

fn update_scrollback_digest_generation(
    hasher: &mut Sha256,
    value: Option<wezterm_term::config::ScrollbackSnapshotGeneration>,
) {
    match value {
        Some(value) => {
            hasher.update([1]);
            hasher.update(value.content_epoch());
            hasher.update(value.revision().to_le_bytes());
        }
        None => hasher.update([0]),
    }
}

fn expected_live_scrollback_logical_ledger_digest(
    manifest: &LiveScrollbackManifestV1,
) -> anyhow::Result<[u8; 32]> {
    let encoded = manifest
        .logical_ledger_sha256
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("v3 logical ledger digest is missing"))?;
    anyhow::ensure!(
        encoded.len() == 64
            && encoded
                .bytes()
                .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f')),
        "v3 logical ledger digest is not canonical lowercase hex"
    );
    let mut digest = [0; 32];
    hex::decode_to_slice(encoded, &mut digest)
        .map_err(|_| anyhow::anyhow!("v3 logical ledger digest is invalid"))?;
    Ok(digest)
}

fn expected_live_scrollback_v4_chain(
    manifest: &LiveScrollbackManifestV1,
) -> anyhow::Result<([u8; 32], [u8; 32])> {
    anyhow::ensure!(
        manifest.schema == LIVE_SCROLLBACK_MANIFEST_SCHEMA_V4,
        "incremental chain requires a v4 manifest"
    );
    let anchor = decode_live_scrollback_canonical_digest(
        manifest
            .chain_anchor_sha256
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("v4 chain anchor is missing"))?,
        "v4 chain anchor",
    )?;
    let tail = decode_live_scrollback_canonical_digest(
        manifest
            .chain_tail_sha256
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("v4 chain tail is missing"))?,
        "v4 chain tail",
    )?;
    anyhow::ensure!(
        manifest.retained_rows != 0 || anchor == tail,
        "empty v4 retained interval has unequal anchor and tail commitments"
    );
    Ok((anchor, tail))
}

fn live_scrollback_logical_ledger_digest_from_records(
    manifest: &LiveScrollbackManifestV1,
    ledger_pane_id: u64,
    oldest_sequence: Option<u64>,
    next_sequence: u64,
    retained_record_bytes: u64,
    committed_log_bytes: u64,
    committed_sequence_bytes: u64,
    records: &[String],
) -> anyhow::Result<[u8; 32]> {
    let record_count = u64::try_from(records.len())
        .map_err(|_| anyhow::anyhow!("logical ledger record count exceeds u64"))?;
    let mut hasher = LiveScrollbackLogicalLedgerHasher::new(
        manifest,
        ledger_pane_id,
        oldest_sequence,
        next_sequence,
        record_count,
        retained_record_bytes,
        committed_log_bytes,
        committed_sequence_bytes,
    )?;
    if let Some(oldest_sequence) = oldest_sequence {
        for (offset, record) in records.iter().enumerate() {
            let sequence = oldest_sequence
                .checked_add(
                    u64::try_from(offset)
                        .map_err(|_| anyhow::anyhow!("logical ledger offset exceeds u64"))?,
                )
                .ok_or_else(|| anyhow::anyhow!("logical ledger sequence overflows"))?;
            hasher.observe(sequence, record)?;
        }
    }
    hasher.finish()
}

fn verify_live_scrollback_logical_ledger_digest_from_snapshot(
    manifest: &LiveScrollbackManifestV1,
    ledger_pane_id: u64,
    snapshot: &frankenterm_core::storage::mmap_store::MmapPaneReadSnapshot,
) -> anyhow::Result<[u8; 32]> {
    let (oldest, next, retained_bytes, committed_bytes, sequence_bytes, records) =
        if manifest.publication_state == "cleared" {
            (None, 0, 0, 0, 0, &[][..])
        } else {
            (
                snapshot.oldest_seq,
                snapshot.next_seq,
                snapshot.retained_record_bytes,
                snapshot.committed_bytes,
                snapshot.sequence_bytes,
                snapshot.records.as_slice(),
            )
        };
    if manifest.schema == LIVE_SCROLLBACK_MANIFEST_SCHEMA_V3 {
        let observed = live_scrollback_logical_ledger_digest_from_records(
            manifest,
            ledger_pane_id,
            oldest,
            next,
            retained_bytes,
            committed_bytes,
            sequence_bytes,
            records,
        )?;
        anyhow::ensure!(
            observed == expected_live_scrollback_logical_ledger_digest(manifest)?,
            "authenticated v3 logical ledger digest mismatch"
        );
        return Ok(observed);
    }

    anyhow::ensure!(
        manifest.schema == LIVE_SCROLLBACK_MANIFEST_SCHEMA_V4
            && manifest.oldest_seq == oldest
            && manifest.next_seq == next
            && manifest.retained_rows == u64::try_from(records.len())?
            && manifest.retained_record_bytes == Some(retained_bytes)
            && manifest.committed_log_bytes == Some(committed_bytes)
            && manifest.committed_sequence_bytes == Some(sequence_bytes),
        "v4 ledger facts disagree with the authenticated manifest"
    );
    let (anchor, expected_tail) = expected_live_scrollback_v4_chain(manifest)?;
    let mut observed_tail = anchor;
    if let Some(oldest) = oldest {
        for (offset, record) in records.iter().enumerate() {
            let sequence = oldest
                .checked_add(u64::try_from(offset)?)
                .ok_or_else(|| anyhow::anyhow!("v4 snapshot sequence overflows"))?;
            observed_tail = live_scrollback_incremental_chain_next(
                observed_tail,
                ledger_pane_id,
                sequence,
                record,
            )?;
        }
    }
    anyhow::ensure!(
        observed_tail == expected_tail,
        "authenticated v4 incremental-chain mismatch"
    );
    Ok(observed_tail)
}

fn live_scrollback_cleared_manifest_is_canonical(
    manifest: &LiveScrollbackManifestV1,
) -> bool {
    manifest.initial_stable_row.is_none()
        && manifest.newest_stable_row_exclusive.is_none()
        && manifest.max_retained_rows == 0
        && manifest.oldest_seq.is_none()
        && manifest.retained_rows == 0
        && manifest.next_seq == 0
}

fn live_scrollback_append_wal_record_digest(record: &str) -> anyhow::Result<[u8; 32]> {
    let mut hasher = Sha256::new();
    hasher.update(LIVE_SCROLLBACK_APPEND_WAL_RECORD_DIGEST_DOMAIN);
    update_scrollback_digest_bytes(&mut hasher, record.as_bytes())?;
    Ok(hasher.finalize().into())
}

fn decode_live_scrollback_canonical_digest(
    encoded: &str,
    field: &'static str,
) -> anyhow::Result<[u8; 32]> {
    anyhow::ensure!(
        encoded.len() == 64
            && encoded
                .bytes()
                .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f')),
        "{field} is not canonical lowercase hex"
    );
    let mut digest = [0; 32];
    hex::decode_to_slice(encoded, &mut digest)
        .with_context(|| format!("decode {field}"))?;
    Ok(digest)
}

fn decode_live_scrollback_epoch(
    encoded: &str,
    field: &'static str,
) -> anyhow::Result<[u8; 16]> {
    anyhow::ensure!(
        encoded.len() == 32
            && encoded
                .bytes()
                .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f')),
        "{field} is not canonical lowercase hex"
    );
    let mut epoch = [0; 16];
    hex::decode_to_slice(encoded, &mut epoch).with_context(|| format!("decode {field}"))?;
    anyhow::ensure!(epoch != [0; 16], "{field} is the reserved zero epoch");
    Ok(epoch)
}

fn live_scrollback_append_wal_target_digest<F>(
    wal: &LiveScrollbackAppendWalV1,
    mut record_at: F,
) -> anyhow::Result<([u8; 32], u64)>
where
    F: FnMut(u64) -> anyhow::Result<String>,
{
    let mut durable_pane_id = [0; 16];
    hex::decode_to_slice(&wal.durable_pane_id, &mut durable_pane_id)
        .context("decode append WAL durable pane identity")?;
    let mut predecessor_epoch = [0; 16];
    hex::decode_to_slice(
        &wal.predecessor_content_epoch,
        &mut predecessor_epoch,
    )
    .context("decode append WAL predecessor epoch")?;
    let mut target_epoch = [0; 16];
    hex::decode_to_slice(&wal.target_content_epoch, &mut target_epoch)
        .context("decode append WAL target epoch")?;
    let mut hasher = Sha256::new();
    hasher.update(LIVE_SCROLLBACK_APPEND_WAL_TARGET_DIGEST_DOMAIN);
    hasher.update(1_u32.to_le_bytes());
    hasher.update(durable_pane_id);
    hasher.update(wal.ledger_pane_id.to_le_bytes());
    hasher.update(predecessor_epoch);
    hasher.update(wal.predecessor_revision.to_le_bytes());
    hasher.update(target_epoch);
    hasher.update(wal.target_revision.to_le_bytes());
    hasher.update(
        i64::try_from(wal.initial_stable_row)
            .map_err(|_| anyhow::anyhow!("append WAL initial stable row exceeds i64"))?
            .to_le_bytes(),
    );
    hasher.update(
        i64::try_from(wal.newest_stable_row_exclusive)
            .map_err(|_| anyhow::anyhow!("append WAL newest stable row exceeds i64"))?
            .to_le_bytes(),
    );
    hasher.update(wal.target_oldest_sequence.to_le_bytes());
    hasher.update(wal.target_next_sequence.to_le_bytes());
    hasher.update(wal.target_record_count.to_le_bytes());
    hasher.update(wal.max_retained_rows.to_le_bytes());

    let expected_next = wal
        .target_oldest_sequence
        .checked_add(wal.target_record_count)
        .ok_or_else(|| anyhow::anyhow!("append WAL target sequence range overflows"))?;
    anyhow::ensure!(
        expected_next == wal.target_next_sequence,
        "append WAL target sequence range is inconsistent"
    );
    let mut retained_record_bytes = 0_u64;
    for sequence in wal.target_oldest_sequence..wal.target_next_sequence {
        let record = record_at(sequence)?;
        retained_record_bytes = retained_record_bytes
            .checked_add(
                u64::try_from(record.len())
                    .map_err(|_| anyhow::anyhow!("append WAL record length exceeds u64"))?,
            )
            .and_then(|bytes| bytes.checked_add(1))
            .ok_or_else(|| anyhow::anyhow!("append WAL target byte count overflows"))?;
        hasher.update(sequence.to_le_bytes());
        update_scrollback_digest_bytes(&mut hasher, record.as_bytes())?;
    }
    hasher.update(retained_record_bytes.to_le_bytes());
    Ok((hasher.finalize().into(), retained_record_bytes))
}

struct LiveScrollbackFilesystemMutationLease {
    file: std::fs::File,
}

impl Drop for LiveScrollbackFilesystemMutationLease {
    fn drop(&mut self) {
        let _ = fs2::FileExt::unlock(&self.file);
    }
}

fn acquire_live_scrollback_filesystem_mutation_lease(
    pane_dir: &std::path::Path,
    synchronize_authority: bool,
) -> anyhow::Result<LiveScrollbackFilesystemMutationLease> {
    let lock_path = pane_dir.join(LIVE_SCROLLBACK_MUTATION_LOCK_NAME);
    let open_lock = |create_new: bool| -> std::io::Result<std::fs::File> {
        let mut options = std::fs::OpenOptions::new();
        options.read(true).write(true).create_new(create_new);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;

            options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
        }
        options.open(&lock_path)
    };
    let file = match open_lock(true) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            open_lock(false)?
        }
        Err(error) => return Err(error.into()),
    };
    fs2::FileExt::lock_exclusive(&file)?;
    let metadata = file.metadata()?;
    anyhow::ensure!(
        metadata.is_file() && metadata.len() == 0,
        "scrollback mutation-lock authority is not an empty regular file"
    );
    let path_metadata = std::fs::symlink_metadata(&lock_path)?;
    anyhow::ensure!(
        path_metadata.file_type().is_file(),
        "scrollback mutation-lock path is not a regular file"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};

        let directory_metadata = std::fs::symlink_metadata(pane_dir)?;
        anyhow::ensure!(
            metadata.permissions().mode() & 0o7777 == 0o600
                && metadata.nlink() == 1
                && metadata.uid() == directory_metadata.uid(),
            "scrollback mutation-lock authority is not private"
        );
        anyhow::ensure!(
            path_metadata.dev() == metadata.dev()
                && path_metadata.ino() == metadata.ino(),
            "scrollback mutation-lock authority changed identity"
        );
    }
    if synchronize_authority {
        // Synchronize both entries even when another process created the
        // file: a creator can crash after publication but before its own
        // directory sync, and each newly constructed sink must close that
        // durability gap before accepting this authority. Later mutations on
        // the same sink can acquire the already-established lease cheaply.
        file.sync_all()?;
        #[cfg(not(windows))]
        std::fs::File::open(pane_dir)?.sync_all()?;
    }
    Ok(LiveScrollbackFilesystemMutationLease { file })
}

#[derive(Debug)]
struct LiveScrollbackManifestPublishError {
    outcome_indeterminate: bool,
    source: anyhow::Error,
}

#[derive(Debug)]
struct LiveScrollbackAppendWalPublishError {
    outcome_indeterminate: bool,
    source: anyhow::Error,
}

impl LiveScrollbackAppendWalPublishError {
    fn outcome_indeterminate(&self) -> bool {
        self.outcome_indeterminate
    }
}

impl std::fmt::Display for LiveScrollbackAppendWalPublishError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.outcome_indeterminate {
            formatter.write_str("scrollback append WAL publication outcome is indeterminate")
        } else {
            formatter.write_str("scrollback append WAL was not published")
        }
    }
}

impl std::error::Error for LiveScrollbackAppendWalPublishError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.source.as_ref())
    }
}

impl LiveScrollbackManifestPublishError {
    fn before_publication(source: anyhow::Error) -> Self {
        Self {
            outcome_indeterminate: false,
            source,
        }
    }

    fn after_publication(source: anyhow::Error) -> Self {
        Self {
            outcome_indeterminate: true,
            source,
        }
    }

    fn outcome_indeterminate(&self) -> bool {
        self.outcome_indeterminate
    }
}

impl std::fmt::Display for LiveScrollbackManifestPublishError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.outcome_indeterminate {
            formatter.write_str("scrollback manifest publication outcome is indeterminate")
        } else {
            formatter.write_str("scrollback manifest was not published")
        }
    }
}

impl std::error::Error for LiveScrollbackManifestPublishError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.source.as_ref())
    }
}

/// Discovery metadata for one continuously persisted mux pane.
#[derive(Debug, Clone, Serialize)]
pub struct LiveScrollbackDurablePane {
    pub durable_pane_id: String,
    pub state: String,
    pub source_pane_id: Option<u64>,
    pub source_domain_id: Option<u64>,
    pub command_description: Option<String>,
    pub retained_rows: Option<u64>,
    pub next_seq: Option<u64>,
    pub path: PathBuf,
    pub error: Option<String>,
}

/// Authenticated identity of one complete v3/v4 scrollback ledger publication.
///
/// Content-bearing identities and digests remain absent from `Debug`; explicit
/// accessors are intended for recovery/checkpoint protocol binding.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct LiveScrollbackCommittedLedgerIdentity {
    durable_pane_id: [u8; 16],
    generation: wezterm_term::config::ScrollbackSnapshotGeneration,
    predecessor: Option<wezterm_term::config::ScrollbackSnapshotGeneration>,
    manifest_digest: [u8; 32],
    manifest_authentication_key_id: [u8; 8],
    logical_ledger_digest: [u8; 32],
    ledger_pane_id: u64,
    oldest_sequence: Option<u64>,
    next_sequence: u64,
    oldest_stable_row: Option<wezterm_term::StableRowIndex>,
    newest_stable_row_exclusive: wezterm_term::StableRowIndex,
    record_count: u64,
    retained_record_bytes: u64,
    committed_log_bytes: u64,
    committed_sequence_bytes: u64,
}

impl LiveScrollbackCommittedLedgerIdentity {
    #[must_use]
    pub const fn durable_pane_id(self) -> [u8; 16] {
        self.durable_pane_id
    }

    #[must_use]
    pub const fn generation(self) -> wezterm_term::config::ScrollbackSnapshotGeneration {
        self.generation
    }

    #[must_use]
    pub const fn predecessor(
        self,
    ) -> Option<wezterm_term::config::ScrollbackSnapshotGeneration> {
        self.predecessor
    }

    #[must_use]
    pub const fn manifest_digest(self) -> [u8; 32] {
        self.manifest_digest
    }

    #[must_use]
    pub const fn manifest_authentication_key_id(self) -> [u8; 8] {
        self.manifest_authentication_key_id
    }

    #[must_use]
    pub const fn logical_ledger_digest(self) -> [u8; 32] {
        self.logical_ledger_digest
    }

    #[must_use]
    pub const fn ledger_pane_id(self) -> u64 {
        self.ledger_pane_id
    }

    #[must_use]
    pub const fn oldest_sequence(self) -> Option<u64> {
        self.oldest_sequence
    }

    #[must_use]
    pub const fn next_sequence(self) -> u64 {
        self.next_sequence
    }

    #[must_use]
    pub const fn oldest_stable_row(self) -> Option<wezterm_term::StableRowIndex> {
        self.oldest_stable_row
    }

    #[must_use]
    pub const fn newest_stable_row_exclusive(self) -> wezterm_term::StableRowIndex {
        self.newest_stable_row_exclusive
    }

    #[must_use]
    pub const fn record_count(self) -> u64 {
        self.record_count
    }

    #[must_use]
    pub const fn retained_record_bytes(self) -> u64 {
        self.retained_record_bytes
    }

    #[must_use]
    pub const fn committed_log_bytes(self) -> u64 {
        self.committed_log_bytes
    }

    #[must_use]
    pub const fn committed_sequence_bytes(self) -> u64 {
        self.committed_sequence_bytes
    }
}

impl std::fmt::Debug for LiveScrollbackCommittedLedgerIdentity {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("LiveScrollbackCommittedLedgerIdentity")
            .field("durable_pane_id", &"[REDACTED]")
            .field("generation", &self.generation)
            .field("predecessor", &self.predecessor)
            .field("manifest_digest", &"[REDACTED]")
            .field("manifest_authentication_key_id", &"[REDACTED]")
            .field("logical_ledger_digest", &"[REDACTED]")
            .field("ledger_pane_id", &self.ledger_pane_id)
            .field("oldest_sequence", &self.oldest_sequence)
            .field("next_sequence", &self.next_sequence)
            .field("oldest_stable_row", &self.oldest_stable_row)
            .field(
                "newest_stable_row_exclusive",
                &self.newest_stable_row_exclusive,
            )
            .field("record_count", &self.record_count)
            .field("retained_record_bytes", &self.retained_record_bytes)
            .field("committed_log_bytes", &self.committed_log_bytes)
            .field("committed_sequence_bytes", &self.committed_sequence_bytes)
            .finish()
    }
}

/// Bounded read-only transcript exported from continuously persisted mux data.
#[derive(Debug, Clone, Serialize)]
pub struct LiveScrollbackTranscriptExport {
    pub durable_pane_id: String,
    pub publication_state: String,
    pub source_pane_id: u64,
    pub source_domain_id: u64,
    pub command_description: String,
    pub initial_stable_row: Option<wezterm_term::StableRowIndex>,
    pub oldest_seq: Option<u64>,
    pub next_seq: u64,
    pub retained_rows: usize,
    pub transcript: String,
    pub transcript_bytes: usize,
    pub committed_log_bytes: u64,
    pub physical_log_bytes: u64,
    pub trailing_uncommitted_bytes: u64,
    /// Number of authenticated exact-semantic rows decrypted before export
    /// redaction was applied.
    pub exact_semantic_records: usize,
    /// Number of legacy rows that cannot participate in exact-semantic
    /// recovery. Their bytes do not prove whether redaction happened before
    /// persistence.
    pub legacy_non_recovery_grade_records: usize,
    /// Exact-semantic rows intentionally persisted without redaction under
    /// guardian encryption. Redaction would destroy terminal recovery state.
    pub pre_persistence_redaction_not_applied_records: usize,
    /// Legacy ftsl1/ftsl2 frames produced by the historical
    /// redact-before-encode path. The framing attests intent but is not
    /// cryptographically authenticated like v3.
    pub legacy_redaction_attested_but_unauthenticated_records: usize,
    /// Raw legacy rows whose persisted bytes cannot establish whether
    /// redaction happened before persistence.
    pub raw_legacy_redaction_unknown_records: usize,
    pub redaction_applied_during_export: bool,
    pub source_content_mutated: bool,
    pub source_path: PathBuf,
}

pub const LIVE_SCROLLBACK_EXPORT_MAX_PANES: usize = 65_536;
pub const LIVE_SCROLLBACK_EXPORT_MAX_ROWS: usize = 4_000_000;
pub const LIVE_SCROLLBACK_EXPORT_MAX_TRANSCRIPT_BYTES: usize = 256 * 1024 * 1024;
pub const LIVE_SCROLLBACK_EXPORT_MAX_PHYSICAL_BYTES: u64 = 1024 * 1024 * 1024;
const LIVE_SCROLLBACK_DISCOVERY_EXTRA_ENTRIES: usize = 4096;

fn is_canonical_live_scrollback_id(value: &str) -> bool {
    value.len() == 32
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
}

fn filesystem_metadata_changed(
    before: &std::fs::Metadata,
    after: &std::fs::Metadata,
) -> anyhow::Result<bool> {
    if before.len() != after.len() || before.modified()? != after.modified()? {
        return Ok(true);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;

        if before.dev() != after.dev()
            || before.ino() != after.ino()
            || before.ctime() != after.ctime()
            || before.ctime_nsec() != after.ctime_nsec()
        {
            return Ok(true);
        }
    }
    Ok(false)
}

impl LiveScrollbackSpillSink {
    fn active_ledger_pane_id(&self) -> u64 {
        self.active_ledger_pane_id
            .load(std::sync::atomic::Ordering::Acquire)
    }

    fn clear_content_epoch(
        &self,
        predecessor: wezterm_term::config::ScrollbackSnapshotGeneration,
    ) -> [u8; 16] {
        let mut hasher = Sha256::new();
        hasher.update(LIVE_SCROLLBACK_CLEAR_EPOCH_DOMAIN);
        hasher.update(self.durable_pane_id);
        hasher.update(predecessor.content_epoch());
        hasher.update(predecessor.revision().to_le_bytes());
        let digest = hasher.finalize();
        let mut epoch = [0; 16];
        epoch.copy_from_slice(&digest[..16]);
        if epoch == [0; 16] {
            epoch[15] = 1;
        }
        if epoch == predecessor.content_epoch() {
            epoch[0] ^= 0x80;
        }
        if epoch == [0; 16] {
            epoch[15] = 1;
        }
        epoch
    }

    fn replacement_ledger_pane_id(
        &self,
        generation: wezterm_term::config::ScrollbackSnapshotGeneration,
        predecessor: Option<wezterm_term::config::ScrollbackSnapshotGeneration>,
        oldest_stable_row: Option<wezterm_term::StableRowIndex>,
        newest_stable_row_exclusive: wezterm_term::StableRowIndex,
        row_count: usize,
        max_retained_rows: usize,
    ) -> Result<u64, wezterm_term::config::ScrollbackSpillError> {
        use sha2::{Digest as _, Sha256};

        let mut hasher = Sha256::new();
        hasher.update(LIVE_SCROLLBACK_REPLACEMENT_LEDGER_DOMAIN);
        hasher.update(self.durable_pane_id);
        hasher.update(generation.content_epoch());
        hasher.update(generation.revision().to_le_bytes());
        match predecessor {
            Some(predecessor) => {
                hasher.update([1]);
                hasher.update(predecessor.content_epoch());
                hasher.update(predecessor.revision().to_le_bytes());
            }
            None => hasher.update([0]),
        }
        match oldest_stable_row {
            Some(oldest) => {
                hasher.update([1]);
                hasher.update(
                    i64::try_from(oldest)
                        .map_err(|_| {
                            wezterm_term::config::ScrollbackSpillError::ArithmeticOverflow(
                                "stable_row_identity",
                            )
                        })?
                        .to_le_bytes(),
                );
            }
            None => hasher.update([0]),
        }
        hasher.update(
            i64::try_from(newest_stable_row_exclusive)
                .map_err(|_| {
                    wezterm_term::config::ScrollbackSpillError::ArithmeticOverflow(
                        "stable_row_identity",
                    )
                })?
                .to_le_bytes(),
        );
        hasher.update(
            u64::try_from(row_count)
                .map_err(|_| {
                    wezterm_term::config::ScrollbackSpillError::ArithmeticOverflow("row_count")
                })?
                .to_le_bytes(),
        );
        hasher.update(
            u64::try_from(max_retained_rows)
                .map_err(|_| {
                    wezterm_term::config::ScrollbackSpillError::ArithmeticOverflow(
                        "retention",
                    )
                })?
                .to_le_bytes(),
        );
        let digest = hasher.finalize();
        let mut pane_id_bytes = [0; 8];
        pane_id_bytes.copy_from_slice(&digest[..8]);
        Ok(u64::from_le_bytes(pane_id_bytes) | (1_u64 << 63))
    }

    fn manifest_checksum(manifest: &LiveScrollbackManifestV1) -> anyhow::Result<String> {
        use sha2::{Digest as _, Sha256};

        let mut canonical = manifest.clone();
        canonical.manifest_sha256.clear();
        let bytes = serde_json::to_vec(&canonical)?;
        Ok(hex::encode(Sha256::digest(bytes)))
    }

    fn manifest_authentication_bytes(
        manifest: &LiveScrollbackManifestV1,
    ) -> anyhow::Result<Vec<u8>> {
        let mut canonical = manifest.clone();
        canonical.guardian_manifest_authentication = None;
        canonical.manifest_sha256.clear();
        serde_json::to_vec(&canonical).context("serialize canonical scrollback manifest")
    }

    fn append_wal_path(manifest_path: &std::path::Path) -> anyhow::Result<PathBuf> {
        Ok(manifest_path
            .parent()
            .ok_or_else(|| anyhow::anyhow!("scrollback manifest path has no parent"))?
            .join(LIVE_SCROLLBACK_APPEND_WAL_NAME))
    }

    fn append_wal_stage_path(manifest_path: &std::path::Path) -> anyhow::Result<PathBuf> {
        Ok(manifest_path
            .parent()
            .ok_or_else(|| anyhow::anyhow!("scrollback manifest path has no parent"))?
            .join(LIVE_SCROLLBACK_APPEND_WAL_STAGE_NAME))
    }

    fn append_wal_checksum(wal: &LiveScrollbackAppendWalV1) -> anyhow::Result<String> {
        let mut canonical = wal.clone();
        canonical.wal_sha256.clear();
        Ok(hex::encode(Sha256::digest(serde_json::to_vec(&canonical)?)))
    }

    fn append_wal_authentication_bytes(
        wal: &LiveScrollbackAppendWalV1,
    ) -> anyhow::Result<Vec<u8>> {
        let mut canonical = wal.clone();
        canonical.encrypted_record.clear();
        canonical.guardian_authentication = None;
        canonical.wal_sha256.clear();
        serde_json::to_vec(&canonical).context("serialize canonical scrollback append WAL")
    }

    fn append_wal_target_generation(
        wal: &LiveScrollbackAppendWalV1,
    ) -> anyhow::Result<wezterm_term::config::ScrollbackSnapshotGeneration> {
        Ok(wezterm_term::config::ScrollbackSnapshotGeneration::new(
            decode_live_scrollback_epoch(
                &wal.target_content_epoch,
                "append WAL target epoch",
            )?,
            wal.target_revision,
        ))
    }

    fn append_wal_supersession(
        wal: &LiveScrollbackAppendWalV1,
    ) -> anyhow::Result<Option<LiveScrollbackAppendWalSupersession>> {
        match (
            wal.superseding_content_epoch.as_deref(),
            wal.superseding_revision,
            wal.superseding_ledger_pane_id,
            wal.superseding_manifest_sha256.as_deref(),
        ) {
            (None, None, None, None) => Ok(None),
            (Some(encoded_epoch), Some(revision), Some(ledger_pane_id), Some(encoded_digest)) => {
                Ok(Some(LiveScrollbackAppendWalSupersession {
                    generation: wezterm_term::config::ScrollbackSnapshotGeneration::new(
                        decode_live_scrollback_epoch(
                            encoded_epoch,
                            "append WAL superseding epoch",
                        )?,
                        revision,
                    ),
                    ledger_pane_id,
                    manifest_sha256: decode_live_scrollback_canonical_digest(
                        encoded_digest,
                        "append WAL superseding manifest digest",
                    )?,
                }))
            }
            _ => anyhow::bail!("append WAL supersession authority is incomplete"),
        }
    }

    fn append_wal_effective_generation(
        wal: &LiveScrollbackAppendWalV1,
    ) -> anyhow::Result<wezterm_term::config::ScrollbackSnapshotGeneration> {
        if let Some(supersession) = Self::append_wal_supersession(wal)? {
            Ok(supersession.generation)
        } else {
            Self::append_wal_target_generation(wal)
        }
    }

    fn validate_append_wal_identity(
        wal: &LiveScrollbackAppendWalV1,
        durable_pane_id: [u8; 16],
    ) -> anyhow::Result<()> {
        anyhow::ensure!(
            matches!(
                wal.schema.as_str(),
                LIVE_SCROLLBACK_APPEND_WAL_SCHEMA_V1 | LIVE_SCROLLBACK_APPEND_WAL_SCHEMA_V2
            ),
            "unsupported live scrollback append WAL schema"
        );
        let expected_durable_pane_id = uuid::Uuid::from_bytes(durable_pane_id)
            .simple()
            .to_string();
        anyhow::ensure!(
            wal.durable_pane_id == expected_durable_pane_id,
            "live scrollback append WAL belongs to another durable pane"
        );
        let predecessor_epoch = decode_live_scrollback_epoch(
            &wal.predecessor_content_epoch,
            "append WAL predecessor epoch",
        )?;
        let target_epoch =
            decode_live_scrollback_epoch(&wal.target_content_epoch, "append WAL target epoch")?;
        Self::append_wal_supersession(wal)?;
        anyhow::ensure!(
            target_epoch == predecessor_epoch
                && wal.target_revision
                    == wal
                        .predecessor_revision
                        .checked_add(1)
                        .ok_or_else(|| anyhow::anyhow!("append WAL predecessor is exhausted"))?,
            "append WAL target is not the exact predecessor successor"
        );
        decode_live_scrollback_canonical_digest(
            &wal.predecessor_manifest_sha256,
            "append WAL predecessor manifest digest",
        )?;
        let record_digest = decode_live_scrollback_canonical_digest(
            &wal.encrypted_record_sha256,
            "append WAL encrypted-record digest",
        )?;
        match wal.schema.as_str() {
            LIVE_SCROLLBACK_APPEND_WAL_SCHEMA_V1 => {
                decode_live_scrollback_canonical_digest(
                    wal.target_record_set_sha256.as_deref().ok_or_else(|| {
                        anyhow::anyhow!("v1 append WAL target record-set digest is missing")
                    })?,
                    "append WAL target record-set digest",
                )?;
                anyhow::ensure!(
                    wal.predecessor_chain_anchor_sha256.is_none()
                        && wal.predecessor_chain_tail_sha256.is_none()
                        && wal.target_chain_anchor_sha256.is_none()
                        && wal.target_chain_tail_sha256.is_none()
                        && wal.evicted_record_count.is_none(),
                    "v1 append WAL contains v2 incremental authority"
                );
            }
            LIVE_SCROLLBACK_APPEND_WAL_SCHEMA_V2 => {
                anyhow::ensure!(
                    wal.target_record_set_sha256.is_none(),
                    "v2 append WAL contains a quadratic target-set digest"
                );
                let predecessor_anchor = decode_live_scrollback_canonical_digest(
                    wal.predecessor_chain_anchor_sha256
                        .as_deref()
                        .ok_or_else(|| anyhow::anyhow!("v2 predecessor chain anchor is missing"))?,
                    "v2 predecessor chain anchor",
                )?;
                let predecessor_tail = decode_live_scrollback_canonical_digest(
                    wal.predecessor_chain_tail_sha256
                        .as_deref()
                        .ok_or_else(|| anyhow::anyhow!("v2 predecessor chain tail is missing"))?,
                    "v2 predecessor chain tail",
                )?;
                let target_anchor = decode_live_scrollback_canonical_digest(
                    wal.target_chain_anchor_sha256
                        .as_deref()
                        .ok_or_else(|| anyhow::anyhow!("v2 target chain anchor is missing"))?,
                    "v2 target chain anchor",
                )?;
                let target_tail = decode_live_scrollback_canonical_digest(
                    wal.target_chain_tail_sha256
                        .as_deref()
                        .ok_or_else(|| anyhow::anyhow!("v2 target chain tail is missing"))?,
                    "v2 target chain tail",
                )?;
                let expected_tail = live_scrollback_incremental_chain_next(
                    predecessor_tail,
                    wal.ledger_pane_id,
                    wal.appended_sequence,
                    &wal.encrypted_record,
                )?;
                anyhow::ensure!(
                    expected_tail == target_tail
                        && (wal.evicted_record_count != Some(0)
                            || predecessor_anchor == target_anchor),
                    "v2 append WAL chain successor is inconsistent"
                );
            }
            _ => unreachable!("schema checked above"),
        }
        anyhow::ensure!(
            wal.encrypted_record_bytes == u64::try_from(wal.encrypted_record.len())?
                && wal.encrypted_record_bytes != 0
                && wal.encrypted_record_bytes <= LIVE_SCROLLBACK_APPEND_WAL_MAX_BYTES,
            "append WAL encrypted-record length is invalid"
        );
        anyhow::ensure!(
            live_scrollback_append_wal_record_digest(&wal.encrypted_record)? == record_digest,
            "append WAL encrypted-record digest mismatch"
        );
        anyhow::ensure!(
            wal.max_retained_rows != 0
                && wal.target_record_count != 0
                && wal.target_record_count <= wal.max_retained_rows
                && wal.target_retained_record_bytes != 0
                && wal.target_record_count <= wal.target_retained_record_bytes
                && wal.target_retained_record_bytes
                    <= LIVE_SCROLLBACK_REPLACEMENT_MAX_COMMITTED_BYTES,
            "append WAL target retention bounds are invalid"
        );
        let target_next_sequence = wal
            .appended_sequence
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("append WAL sequence is exhausted"))?;
        anyhow::ensure!(
            wal.target_next_sequence == target_next_sequence
                && wal.target_oldest_sequence
                    .checked_add(wal.target_record_count)
                    == Some(wal.target_next_sequence),
            "append WAL target sequence interval is inconsistent"
        );
        if wal.schema == LIVE_SCROLLBACK_APPEND_WAL_SCHEMA_V2 {
            let evicted = wal
                .evicted_record_count
                .ok_or_else(|| anyhow::anyhow!("v2 append WAL eviction count is missing"))?;
            anyhow::ensure!(
                evicted <= wal.appended_sequence
                    && wal.target_oldest_sequence
                        == wal
                            .appended_sequence
                            .checked_add(1)
                            .and_then(|next| next.checked_sub(wal.target_record_count))
                            .ok_or_else(|| anyhow::anyhow!("v2 append WAL target underflows"))?,
                "v2 append WAL eviction accounting is inconsistent"
            );
        }
        let stable_offset = wezterm_term::StableRowIndex::try_from(wal.appended_sequence)
            .map_err(|_| anyhow::anyhow!("append WAL sequence exceeds stable-row range"))?;
        let appended_stable_row = wal
            .initial_stable_row
            .checked_add(stable_offset)
            .ok_or_else(|| anyhow::anyhow!("append WAL stable-row identity overflows"))?;
        let newest_offset = wezterm_term::StableRowIndex::try_from(wal.target_next_sequence)
            .map_err(|_| anyhow::anyhow!("append WAL endpoint exceeds stable-row range"))?;
        anyhow::ensure!(
            wal.appended_stable_row == appended_stable_row
                && wal.newest_stable_row_exclusive
                    == wal
                        .initial_stable_row
                        .checked_add(newest_offset)
                        .ok_or_else(|| anyhow::anyhow!("append WAL endpoint overflows"))?,
            "append WAL stable-row interval is inconsistent"
        );
        let parsed = mux::guardian_output_journal::GuardianEncryptedScrollbackRow::parse(
            &wal.encrypted_record,
        )
        .context("parse append WAL exact row")?;
        let identity = parsed.identity();
        anyhow::ensure!(
            identity.durable_pane_id() == durable_pane_id
                && identity.content_epoch() == target_epoch
                && identity.revision() == wal.target_revision
                && identity.stable_row() == i64::try_from(wal.appended_stable_row)?
                && identity.sequence() == wal.appended_sequence,
            "append WAL exact row has the wrong authenticated location"
        );
        anyhow::ensure!(
            wal.guardian_authentication.is_some(),
            "append WAL guardian authentication is missing"
        );
        Ok(())
    }

    fn authenticate_append_wal(
        wal: &LiveScrollbackAppendWalV1,
        keyring: &guardian_output_keys::GuardianOutputKeyring,
    ) -> anyhow::Result<()> {
        let authentication =
            mux::guardian_output_journal::GuardianScrollbackAppendWalAuthentication::parse(
                wal.guardian_authentication
                    .as_deref()
                    .ok_or_else(|| anyhow::anyhow!("append WAL authentication is missing"))?,
            )
            .context("parse append WAL guardian authentication")?;
        let cipher = keyring
            .cipher_for_key_id(authentication.key_id())
            .context("load historical append WAL guardian key")?;
        cipher
            .verify_scrollback_append_wal(
                &authentication,
                &Self::append_wal_authentication_bytes(wal)?,
            )
            .context("authenticate exact-row append WAL")
    }

    fn append_wal_matches_predecessor_manifest(
        wal: &LiveScrollbackAppendWalV1,
        manifest: &LiveScrollbackManifestV1,
    ) -> anyhow::Result<bool> {
        let generation = live_scrollback_manifest_generation(manifest)?;
        let chain_matches = if wal.schema == LIVE_SCROLLBACK_APPEND_WAL_SCHEMA_V2
            && manifest.schema == LIVE_SCROLLBACK_MANIFEST_SCHEMA_V4
        {
            let (anchor, tail) = expected_live_scrollback_v4_chain(manifest)?;
            decode_live_scrollback_canonical_digest(
                wal.predecessor_chain_anchor_sha256
                    .as_deref()
                    .ok_or_else(|| anyhow::anyhow!("v2 predecessor chain anchor is missing"))?,
                "v2 predecessor chain anchor",
            )? == anchor
                && decode_live_scrollback_canonical_digest(
                    wal.predecessor_chain_tail_sha256.as_deref().ok_or_else(|| {
                        anyhow::anyhow!("v2 predecessor chain tail is missing")
                    })?,
                    "v2 predecessor chain tail",
                )? == tail
        } else {
            true
        };
        Ok(live_scrollback_manifest_is_authenticated(manifest)
            && manifest.publication_state == "complete"
            && manifest.manifest_sha256 == wal.predecessor_manifest_sha256
            && generation
                == Some((
                    decode_live_scrollback_epoch(
                        &wal.predecessor_content_epoch,
                        "append WAL predecessor epoch",
                    )?,
                    wal.predecessor_revision,
                ))
            && Self::manifest_ledger_pane_id(manifest)? == wal.ledger_pane_id
            && chain_matches)
    }

    fn append_wal_matches_target_manifest(
        wal: &LiveScrollbackAppendWalV1,
        manifest: &LiveScrollbackManifestV1,
    ) -> anyhow::Result<bool> {
        let target_epoch = decode_live_scrollback_epoch(
            &wal.target_content_epoch,
            "append WAL target epoch",
        )?;
        let predecessor_epoch = decode_live_scrollback_epoch(
            &wal.predecessor_content_epoch,
            "append WAL predecessor epoch",
        )?;
        let schema_matches = (wal.schema == LIVE_SCROLLBACK_APPEND_WAL_SCHEMA_V1
            && matches!(
                manifest.schema.as_str(),
                LIVE_SCROLLBACK_MANIFEST_SCHEMA_V3 | LIVE_SCROLLBACK_MANIFEST_SCHEMA_V4
            ))
            || (wal.schema == LIVE_SCROLLBACK_APPEND_WAL_SCHEMA_V2
                && manifest.schema == LIVE_SCROLLBACK_MANIFEST_SCHEMA_V4);
        let chain_matches = if wal.schema == LIVE_SCROLLBACK_APPEND_WAL_SCHEMA_V2 {
            let (anchor, tail) = expected_live_scrollback_v4_chain(manifest)?;
            decode_live_scrollback_canonical_digest(
                wal.target_chain_anchor_sha256
                    .as_deref()
                    .ok_or_else(|| anyhow::anyhow!("v2 target chain anchor is missing"))?,
                "v2 target chain anchor",
            )? == anchor
                && decode_live_scrollback_canonical_digest(
                    wal.target_chain_tail_sha256
                        .as_deref()
                        .ok_or_else(|| anyhow::anyhow!("v2 target chain tail is missing"))?,
                    "v2 target chain tail",
                )? == tail
        } else {
            true
        };
        Ok(schema_matches
            && live_scrollback_manifest_is_authenticated(manifest)
            && manifest.publication_state == "complete"
            && live_scrollback_manifest_generation(manifest)?
                == Some((target_epoch, wal.target_revision))
            && live_scrollback_manifest_predecessor(manifest)?
                == Some(wezterm_term::config::ScrollbackSnapshotGeneration::new(
                    predecessor_epoch,
                    wal.predecessor_revision,
                ))
            && Self::manifest_ledger_pane_id(manifest)? == wal.ledger_pane_id
            && manifest.initial_stable_row == Some(wal.initial_stable_row)
            && manifest.newest_stable_row_exclusive == Some(wal.newest_stable_row_exclusive)
            && manifest.max_retained_rows == wal.max_retained_rows
            && manifest.oldest_seq == Some(wal.target_oldest_sequence)
            && manifest.retained_rows == wal.target_record_count
            && manifest.next_seq == wal.target_next_sequence
            && manifest.retained_record_bytes == Some(wal.target_retained_record_bytes)
            && chain_matches)
    }

    fn append_wal_is_v1_to_v4_target_migration(
        wal: &LiveScrollbackAppendWalV1,
        manifest: &LiveScrollbackManifestV1,
    ) -> bool {
        wal.schema == LIVE_SCROLLBACK_APPEND_WAL_SCHEMA_V1
            && manifest.schema == LIVE_SCROLLBACK_MANIFEST_SCHEMA_V4
    }

    /// A recovered v1 WAL may be republished as a v4 manifest for the same
    /// generation. Accept that one-time schema migration only after proving
    /// both durable commitments over the exact same store: the v1 target-set
    /// digest and the v4 incremental chain. Ordinary v2/v4 steady-state
    /// acceptance deliberately does not call this full-scan verifier.
    fn verify_v1_append_wal_v4_target_migration(
        wal: &LiveScrollbackAppendWalV1,
        manifest: &LiveScrollbackManifestV1,
        store: &frankenterm_core::storage::mmap_store::MmapScrollbackStore,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(
            Self::append_wal_is_v1_to_v4_target_migration(wal, manifest)
                && Self::append_wal_matches_target_manifest(wal, manifest)?,
            "append WAL is not an exact v1-to-v4 target migration"
        );
        Self::verify_append_wal_target_store(wal, store)
            .context("verify v1 target-set digest during v4 migration")?;
        Self::verify_logical_ledger_digest_from_store(manifest, wal.ledger_pane_id, store)
            .context("verify v4 incremental chain during v1 WAL migration")?;
        Ok(())
    }

    fn append_wal_supersession_matches_manifest(
        wal: &LiveScrollbackAppendWalV1,
        manifest: &LiveScrollbackManifestV1,
    ) -> anyhow::Result<bool> {
        let Some(supersession) = Self::append_wal_supersession(wal)? else {
            return Ok(false);
        };
        let manifest_generation = live_scrollback_manifest_generation(manifest)?.map(
            |(content_epoch, revision)| {
                wezterm_term::config::ScrollbackSnapshotGeneration::new(content_epoch, revision)
            },
        );
        Ok(live_scrollback_manifest_is_authenticated(manifest)
            && manifest_generation == Some(supersession.generation)
            && Self::manifest_ledger_pane_id(manifest)? == supersession.ledger_pane_id
            && decode_live_scrollback_canonical_digest(
                &manifest.manifest_sha256,
                "superseding scrollback manifest digest",
            )? == supersession.manifest_sha256)
    }

    /// True only when the current authenticated manifest directly extends
    /// the latest generation named by this retained WAL evidence. Advancing
    /// the marker one signed edge at a time prevents a stale or future WAL
    /// replay from being silently discarded across replacement or clear.
    fn append_wal_is_immediately_superseded_by_manifest(
        wal: &LiveScrollbackAppendWalV1,
        manifest: &LiveScrollbackManifestV1,
    ) -> anyhow::Result<bool> {
        Ok(live_scrollback_manifest_is_authenticated(manifest)
            && live_scrollback_manifest_predecessor(manifest)?
                == Some(Self::append_wal_effective_generation(wal)?))
    }

    fn append_wal_is_consumed_or_superseded(
        wal: &LiveScrollbackAppendWalV1,
        manifest: &LiveScrollbackManifestV1,
    ) -> anyhow::Result<bool> {
        Ok(Self::append_wal_matches_target_manifest(wal, manifest)?
            || Self::append_wal_supersession_matches_manifest(wal, manifest)?
            || Self::append_wal_is_immediately_superseded_by_manifest(wal, manifest)?)
    }

    fn verify_append_wal_target_store(
        wal: &LiveScrollbackAppendWalV1,
        store: &frankenterm_core::storage::mmap_store::MmapScrollbackStore,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(
            store.oldest_seq(wal.ledger_pane_id) == Some(wal.target_oldest_sequence)
                && store.next_seq(wal.ledger_pane_id)? == wal.target_next_sequence
                && u64::try_from(store.line_count(wal.ledger_pane_id))?
                    == wal.target_record_count,
            "append WAL target ledger range mismatch"
        );
        if wal.schema == LIVE_SCROLLBACK_APPEND_WAL_SCHEMA_V1 {
            let (digest, retained_record_bytes) = live_scrollback_append_wal_target_digest(
                wal,
                |sequence| live_scrollback_authority_record_at(store, wal.ledger_pane_id, sequence),
            )?;
            anyhow::ensure!(
                digest
                    == decode_live_scrollback_canonical_digest(
                        wal.target_record_set_sha256.as_deref().ok_or_else(|| {
                            anyhow::anyhow!("v1 append WAL target digest is missing")
                        })?,
                        "append WAL target record-set digest",
                    )?
                    && retained_record_bytes == wal.target_retained_record_bytes,
                "append WAL target ledger digest or byte count mismatch"
            );
        } else {
            anyhow::ensure!(
                live_scrollback_authority_record_at(
                    store,
                    wal.ledger_pane_id,
                    wal.appended_sequence,
                )? == wal.encrypted_record,
                "v2 append WAL exact target row mismatch"
            );
        }
        anyhow::ensure!(
            store.retained_record_bytes(wal.ledger_pane_id)
                == wal.target_retained_record_bytes,
            "append WAL target retained-byte count mismatch"
        );
        Ok(())
    }

    fn v2_append_wal_target_authority(
        wal: &LiveScrollbackAppendWalV1,
    ) -> anyhow::Result<VerifiedLedgerState> {
        anyhow::ensure!(
            wal.schema == LIVE_SCROLLBACK_APPEND_WAL_SCHEMA_V2,
            "incremental target authority requires a v2 append WAL"
        );
        Ok(VerifiedLedgerState {
            ledger_pane_id: wal.ledger_pane_id,
            oldest_sequence: Some(wal.target_oldest_sequence),
            next_sequence: wal.target_next_sequence,
            record_count: wal.target_record_count,
            retained_record_bytes: wal.target_retained_record_bytes,
            chain_anchor: decode_live_scrollback_canonical_digest(
                wal.target_chain_anchor_sha256
                    .as_deref()
                    .ok_or_else(|| anyhow::anyhow!("v2 WAL target chain anchor is missing"))?,
                "v2 WAL target chain anchor",
            )?,
            chain_tail: decode_live_scrollback_canonical_digest(
                wal.target_chain_tail_sha256
                    .as_deref()
                    .ok_or_else(|| anyhow::anyhow!("v2 WAL target chain tail is missing"))?,
                "v2 WAL target chain tail",
            )?,
        })
    }

    fn verify_v2_append_wal_target_chain_cold(
        wal: &LiveScrollbackAppendWalV1,
        store: &frankenterm_core::storage::mmap_store::MmapScrollbackStore,
    ) -> anyhow::Result<VerifiedLedgerState> {
        let target = Self::v2_append_wal_target_authority(wal)?;
        let scanned = VerifiedLedgerState::scan_store(
            wal.ledger_pane_id,
            store,
            target.chain_anchor,
        )?;
        anyhow::ensure!(
            scanned == target,
            "v2 append WAL target incremental chain mismatch"
        );
        Ok(scanned)
    }

    fn reconcile_authenticated_append_wal(
        wal: &LiveScrollbackAppendWalV1,
        manifest: &LiveScrollbackManifestV1,
        store: &mut frankenterm_core::storage::mmap_store::MmapScrollbackStore,
        keyring: &guardian_output_keys::GuardianOutputKeyring,
        durable_pane_id: [u8; 16],
    ) -> anyhow::Result<Option<LiveScrollbackSpillState>> {
        Self::validate_append_wal_identity(wal, durable_pane_id)?;
        Self::authenticate_append_wal(wal, keyring)?;

        if Self::append_wal_matches_target_manifest(wal, manifest)? {
            if Self::append_wal_is_v1_to_v4_target_migration(wal, manifest) {
                Self::verify_v1_append_wal_v4_target_migration(wal, manifest, store)?;
            } else {
                Self::verify_append_wal_target_store(wal, store)?;
                Self::verify_logical_ledger_digest_from_store(
                    manifest,
                    wal.ledger_pane_id,
                    store,
                )
                .context("bind consumed append WAL to its authenticated target manifest")?;
            }
            return Ok(None);
        }
        if Self::append_wal_supersession_matches_manifest(wal, manifest)?
            || Self::append_wal_is_immediately_superseded_by_manifest(wal, manifest)?
        {
            // The current signed manifest either is the exact generation
            // recorded by the retirement marker or extends it by one signed
            // predecessor edge. The WAL remains intact as bounded evidence;
            // it must not be replayed into the superseding ledger.
            return Ok(None);
        }
        anyhow::ensure!(
            Self::append_wal_matches_predecessor_manifest(wal, manifest)?,
            "append WAL does not follow the published manifest"
        );

        let observed_next = store.next_seq(wal.ledger_pane_id)?;
        match observed_next.cmp(&wal.appended_sequence) {
            std::cmp::Ordering::Equal => {
                Self::verify_logical_ledger_digest_from_store(
                    manifest,
                    wal.ledger_pane_id,
                    store,
                )
                .context("verify append WAL predecessor ledger before recovery")?;
                let appended = store
                    .append_line(wal.ledger_pane_id, &wal.encrypted_record)
                    .context("recover append WAL exact row")?;
                anyhow::ensure!(
                    appended == wal.appended_sequence,
                    "append WAL recovery wrote the wrong sequence"
                );
            }
            std::cmp::Ordering::Greater => {
                anyhow::ensure!(
                    observed_next == wal.target_next_sequence
                        && store.line_at(wal.ledger_pane_id, wal.appended_sequence)?.as_deref()
                            == Some(wal.encrypted_record.as_str()),
                    "append WAL recovery found conflicting synchronized content"
                );
            }
            std::cmp::Ordering::Less => {
                anyhow::bail!("append WAL recovery found a gap before its target sequence");
            }
        }

        let observed_oldest = store
            .oldest_seq(wal.ledger_pane_id)
            .ok_or_else(|| anyhow::anyhow!("append WAL recovery ledger is unexpectedly empty"))?;
        anyhow::ensure!(
            observed_oldest <= wal.target_oldest_sequence,
            "append WAL recovery ledger pruned beyond its authenticated target"
        );
        if observed_oldest < wal.target_oldest_sequence {
            store
                .prune_before(wal.ledger_pane_id, wal.target_oldest_sequence)
                .context("recover append WAL retention cut")?;
        }
        Self::verify_append_wal_target_store(wal, store)?;
        let verified_ledger = if wal.schema == LIVE_SCROLLBACK_APPEND_WAL_SCHEMA_V2 {
            Some(Self::verify_v2_append_wal_target_chain_cold(wal, store)?)
        } else {
            None
        };
        let target_epoch = decode_live_scrollback_epoch(
            &wal.target_content_epoch,
            "append WAL target epoch",
        )?;
        Self::validate_persisted_records(
            store,
            wal.ledger_pane_id,
            keyring,
            durable_pane_id,
            target_epoch,
            wal.initial_stable_row,
            true,
        )?;
        let predecessor_epoch = decode_live_scrollback_epoch(
            &wal.predecessor_content_epoch,
            "append WAL predecessor epoch",
        )?;
        Ok(Some(LiveScrollbackSpillState {
            initial_stable_row: Some(wal.initial_stable_row),
            newest_stable_row_exclusive: Some(wal.newest_stable_row_exclusive),
            max_retained_rows: usize::try_from(wal.max_retained_rows)?,
            content_epoch: target_epoch,
            revision: wal.target_revision,
            authenticated_manifest: true,
            predecessor_generation: Some(
                wezterm_term::config::ScrollbackSnapshotGeneration::new(
                    predecessor_epoch,
                    wal.predecessor_revision,
                ),
            ),
            clear_manifest_published: false,
            clear_pending_physical_reclamation: false,
            transaction_quarantined: false,
            verified_ledger,
        }))
    }

    fn prepare_authenticated_append_wal(
        &self,
        predecessor_manifest: &LiveScrollbackManifestV1,
        previous_state: LiveScrollbackSpillState,
        proposed_state: LiveScrollbackSpillState,
        ledger_pane_id: u64,
        stable_row: wezterm_term::StableRowIndex,
        desired_sequence: u64,
        max_retained_rows: usize,
        encrypted_record: &str,
        store: &frankenterm_core::storage::mmap_store::MmapScrollbackStore,
    ) -> anyhow::Result<(LiveScrollbackAppendWalV1, VerifiedLedgerState)> {
        anyhow::ensure!(
            predecessor_manifest.publication_state == "complete"
                && live_scrollback_manifest_is_authenticated(predecessor_manifest)
                && Self::manifest_ledger_pane_id(predecessor_manifest)? == ledger_pane_id
                && live_scrollback_manifest_generation(predecessor_manifest)?
                    == Some((previous_state.content_epoch, previous_state.revision)),
            "append WAL predecessor manifest does not match live state"
        );
        let predecessor_authority = previous_state
            .verified_ledger
            .ok_or_else(|| anyhow::anyhow!("append WAL has no scan-minted predecessor authority"))?;
        anyhow::ensure!(
            predecessor_authority.ledger_pane_id == ledger_pane_id
                && predecessor_authority.matches_store_facts(store)?,
            "append WAL predecessor authority disagrees with the live store"
        );
        if predecessor_manifest.schema == LIVE_SCROLLBACK_MANIFEST_SCHEMA_V3 {
            // The one-time v3 -> v4 transition remains cold/full-scan
            // verified. Every subsequent v4 append uses only the typed state.
            Self::verify_logical_ledger_digest_from_store(
                predecessor_manifest,
                ledger_pane_id,
                store,
            )?;
        } else {
            let (anchor, tail) = expected_live_scrollback_v4_chain(predecessor_manifest)?;
            anyhow::ensure!(
                predecessor_authority.chain_anchor == anchor
                    && predecessor_authority.chain_tail == tail,
                "v4 append predecessor chain disagrees with scan-minted authority"
            );
        }
        anyhow::ensure!(
            store.next_seq(ledger_pane_id)? == desired_sequence,
            "append WAL must be prepared at the exact next sequence"
        );
        let current_record_count = u64::try_from(store.line_count(ledger_pane_id))?;
        let max_retained_rows_u64 = u64::try_from(max_retained_rows)?;
        let (target_authority, evicted_record_count) = predecessor_authority.project_append(
            desired_sequence,
            encrypted_record,
            max_retained_rows,
            store,
        )?;
        anyhow::ensure!(
            current_record_count == predecessor_authority.record_count,
            "append WAL predecessor row count changed"
        );
        let initial_stable_row = proposed_state
            .initial_stable_row
            .ok_or_else(|| anyhow::anyhow!("append WAL target has no stable-row origin"))?;
        let newest_stable_row_exclusive = proposed_state
            .newest_stable_row_exclusive
            .ok_or_else(|| anyhow::anyhow!("append WAL target has no stable-row endpoint"))?;
        let mut wal = LiveScrollbackAppendWalV1 {
            schema: LIVE_SCROLLBACK_APPEND_WAL_SCHEMA_V2.to_string(),
            durable_pane_id: uuid::Uuid::from_bytes(self.durable_pane_id)
                .simple()
                .to_string(),
            ledger_pane_id,
            predecessor_content_epoch: hex::encode(previous_state.content_epoch),
            predecessor_revision: previous_state.revision,
            predecessor_manifest_sha256: predecessor_manifest.manifest_sha256.clone(),
            target_content_epoch: hex::encode(proposed_state.content_epoch),
            target_revision: proposed_state.revision,
            initial_stable_row,
            newest_stable_row_exclusive,
            appended_stable_row: stable_row,
            appended_sequence: desired_sequence,
            max_retained_rows: max_retained_rows_u64,
            target_oldest_sequence: target_authority
                .oldest_sequence
                .ok_or_else(|| anyhow::anyhow!("append WAL target unexpectedly empty"))?,
            target_next_sequence: target_authority.next_sequence,
            target_record_count: target_authority.record_count,
            target_retained_record_bytes: target_authority.retained_record_bytes,
            encrypted_record_bytes: u64::try_from(encrypted_record.len())?,
            encrypted_record_sha256: hex::encode(live_scrollback_append_wal_record_digest(
                encrypted_record,
            )?),
            target_record_set_sha256: None,
            predecessor_chain_anchor_sha256: Some(hex::encode(
                predecessor_authority.chain_anchor,
            )),
            predecessor_chain_tail_sha256: Some(hex::encode(predecessor_authority.chain_tail)),
            target_chain_anchor_sha256: Some(hex::encode(target_authority.chain_anchor)),
            target_chain_tail_sha256: Some(hex::encode(target_authority.chain_tail)),
            evicted_record_count: Some(evicted_record_count),
            superseding_content_epoch: None,
            superseding_revision: None,
            superseding_ledger_pane_id: None,
            superseding_manifest_sha256: None,
            encrypted_record: encrypted_record.to_string(),
            guardian_authentication: None,
            wal_sha256: String::new(),
        };
        // Authentication is intentionally absent until every other canonical
        // field is frozen. Validate the non-auth fields with a private marker,
        // then remove it before sealing the canonical authentication bytes.
        wal.guardian_authentication = Some("pending".to_string());
        Self::validate_append_wal_identity(&wal, self.durable_pane_id)?;
        wal.guardian_authentication = None;
        let canonical = Self::append_wal_authentication_bytes(&wal)?;
        let mut keyring = self
            .lock_keyring("prepare append WAL authentication")
            .map_err(anyhow::Error::new)?;
        let cipher = keyring
            .latest_active_cipher()
            .context("load guardian append-WAL authentication key")?;
        wal.guardian_authentication = Some(
            cipher
                .authenticate_scrollback_append_wal(&canonical)
                .context("authenticate append WAL target")?
                .encode(),
        );
        Self::validate_append_wal_identity(&wal, self.durable_pane_id)?;
        wal.wal_sha256 = Self::append_wal_checksum(&wal)?;
        Ok((wal, target_authority))
    }

    fn persist_authenticated_append_wal(
        &self,
        wal: &LiveScrollbackAppendWalV1,
    ) -> Result<(), LiveScrollbackAppendWalPublishError> {
        let mut publication_attempted = false;
        let result = (|| -> anyhow::Result<()> {
            Self::validate_append_wal_identity(wal, self.durable_pane_id)?;
            {
                let keyring = self
                    .lock_keyring("persist append WAL authentication")
                    .map_err(anyhow::Error::new)?;
                Self::authenticate_append_wal(wal, &keyring)?;
            }
            anyhow::ensure!(
                wal.wal_sha256 == Self::append_wal_checksum(wal)?,
                "append WAL checksum changed before publication"
            );
            let active_path = Self::append_wal_path(&self.manifest_path)?;
            let stage_path = Self::append_wal_stage_path(&self.manifest_path)?;
            let parent = active_path
                .parent()
                .ok_or_else(|| anyhow::anyhow!("append WAL path has no parent"))?;

            if let Some(active) = Self::read_append_wal(&active_path)? {
                let manifest = Self::read_manifest(&self.manifest_path)?
                    .ok_or_else(|| anyhow::anyhow!("active append WAL has no manifest"))?;
                let durable_pane_id = uuid::Uuid::from_bytes(self.durable_pane_id)
                    .simple()
                    .to_string();
                validate_live_scrollback_manifest_identity(
                    &manifest,
                    &durable_pane_id,
                    &self.manifest_path,
                )?;
                {
                    let keyring = self
                        .lock_keyring("replace consumed append WAL authentication")
                        .map_err(anyhow::Error::new)?;
                    Self::validate_append_wal_identity(&active, self.durable_pane_id)?;
                    Self::authenticate_append_wal(&active, &keyring)?;
                    anyhow::ensure!(
                        Self::authenticate_manifest(&manifest, &keyring)?,
                        "active append WAL supersession manifest is not authenticated"
                    );
                }
                if Self::append_wal_matches_target_manifest(&active, &manifest)? {
                    let store = self
                        .lock_store("replace consumed append WAL target")
                        .map_err(anyhow::Error::new)?;
                    if Self::append_wal_is_v1_to_v4_target_migration(&active, &manifest) {
                        Self::verify_v1_append_wal_v4_target_migration(
                            &active,
                            &manifest,
                            &store,
                        )?;
                    } else {
                        Self::verify_append_wal_target_store(&active, &store)?;
                    }
                    if active.schema == LIVE_SCROLLBACK_APPEND_WAL_SCHEMA_V2 {
                        let authority = Self::v2_append_wal_target_authority(&active)?;
                        anyhow::ensure!(
                            authority.matches_store_facts(&store)?
                                && expected_live_scrollback_v4_chain(&manifest)?
                                    == (authority.chain_anchor, authority.chain_tail),
                            "consumed v2 append WAL disagrees with its published v4 authority"
                        );
                    } else if !Self::append_wal_is_v1_to_v4_target_migration(
                        &active,
                        &manifest,
                    ) {
                        Self::verify_logical_ledger_digest_from_store(
                            &manifest,
                            active.ledger_pane_id,
                            &store,
                        )
                        .context("bind consumed append WAL before bounded-slot replacement")?;
                    }
                } else {
                    anyhow::ensure!(
                        Self::append_wal_supersession_matches_manifest(&active, &manifest)?
                            || Self::append_wal_is_immediately_superseded_by_manifest(
                                &active,
                                &manifest,
                            )?,
                        "refusing to replace an unconsumed or unlinked append WAL"
                    );
                }
            }

            let mut bytes = serde_json::to_vec_pretty(wal)?;
            bytes.push(b'\n');
            anyhow::ensure!(
                u64::try_from(bytes.len()).unwrap_or(u64::MAX)
                    <= LIVE_SCROLLBACK_APPEND_WAL_MAX_BYTES,
                "append WAL serialization exceeds its byte ceiling"
            );
            let stage_is_exact = match Self::read_append_wal(&stage_path) {
                Ok(Some(staged)) => {
                    anyhow::ensure!(
                        staged == *wal,
                        "deterministic append WAL stage belongs to another transaction"
                    );
                    true
                }
                Ok(None) => false,
                Err(_)
                    if Self::append_wal_stage_is_recoverably_incomplete(&stage_path)? =>
                {
                    false
                }
                Err(error) => return Err(error),
            };
            if stage_is_exact {
                let path_metadata = std::fs::symlink_metadata(&stage_path)?;
                let mut options = std::fs::OpenOptions::new();
                options.read(true);
                #[cfg(unix)]
                {
                    use std::os::unix::fs::OpenOptionsExt as _;

                    options.custom_flags(libc::O_NOFOLLOW);
                }
                let file = options.open(&stage_path)?;
                let handle_metadata = file.metadata()?;
                anyhow::ensure!(
                    path_metadata.file_type().is_file()
                        && path_metadata.len() <= LIVE_SCROLLBACK_APPEND_WAL_MAX_BYTES
                        && handle_metadata.is_file()
                        && handle_metadata.len() <= LIVE_SCROLLBACK_APPEND_WAL_MAX_BYTES,
                    "opened exact append WAL stage is not a bounded regular file"
                );
                #[cfg(unix)]
                {
                    use std::os::unix::fs::MetadataExt as _;

                    anyhow::ensure!(
                        handle_metadata.dev() == path_metadata.dev()
                            && handle_metadata.ino() == path_metadata.ino(),
                        "exact append WAL stage changed identity before synchronization"
                    );
                }
                file.sync_all()?;
            } else {
                let stage_exists = std::fs::symlink_metadata(&stage_path).is_ok();
                let mut options = std::fs::OpenOptions::new();
                options.read(true).write(true);
                let expected_stage_metadata = if stage_exists {
                    Some(std::fs::symlink_metadata(&stage_path)?)
                } else {
                    None
                };
                if !stage_exists {
                    options.create_new(true);
                }
                #[cfg(unix)]
                {
                    use std::os::unix::fs::OpenOptionsExt as _;

                    options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
                }
                let mut file = options.open(&stage_path).with_context(|| {
                    format!("open deterministic append WAL stage {}", stage_path.display())
                })?;
                anyhow::ensure!(
                    file.metadata()?.is_file(),
                    "opened append WAL stage is not a regular file"
                );
                if let Some(expected) = expected_stage_metadata {
                    let observed = file.metadata()?;
                    anyhow::ensure!(
                        expected.file_type().is_file()
                            && expected.len() <= LIVE_SCROLLBACK_APPEND_WAL_MAX_BYTES,
                        "incomplete append WAL stage is not a bounded regular file"
                    );
                    #[cfg(unix)]
                    {
                        use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};

                        let parent_metadata = std::fs::symlink_metadata(parent)?;
                        anyhow::ensure!(
                            expected.permissions().mode() & 0o077 == 0
                                && expected.nlink() == 1
                                && expected.uid() == parent_metadata.uid()
                                && observed.dev() == expected.dev()
                                && observed.ino() == expected.ino(),
                            "incomplete append WAL stage changed identity before rewrite"
                        );
                        let path_metadata = std::fs::symlink_metadata(&stage_path)?;
                        anyhow::ensure!(
                            path_metadata.file_type().is_file()
                                && path_metadata.dev() == observed.dev()
                                && path_metadata.ino() == observed.ino(),
                            "incomplete append WAL stage changed path before rewrite"
                        );
                    }
                    // Do not truncate during path resolution: only the exact
                    // private, single-link inode pinned above may be rewritten.
                    file.set_len(0)?;
                }
                file.write_all(&bytes)?;
                file.sync_all()?;
            }
            anyhow::ensure!(
                Self::read_append_wal(&stage_path)?.as_ref() == Some(wal),
                "deterministic append WAL stage changed before publication"
            );
            #[cfg(not(windows))]
            std::fs::File::open(parent)?.sync_all()?;

            publication_attempted = true;
            std::fs::rename(&stage_path, &active_path).with_context(|| {
                format!("publish append WAL {}", active_path.display())
            })?;
            #[cfg(not(windows))]
            std::fs::File::open(parent)?.sync_all()?;
            let published = Self::read_append_wal(&active_path)?
                .ok_or_else(|| anyhow::anyhow!("published append WAL disappeared"))?;
            anyhow::ensure!(published == *wal, "published append WAL changed");
            let keyring = self
                .lock_keyring("acknowledge append WAL authentication")
                .map_err(anyhow::Error::new)?;
            Self::authenticate_append_wal(&published, &keyring)?;
            Ok(())
        })();
        result.map_err(|source| LiveScrollbackAppendWalPublishError {
            outcome_indeterminate: publication_attempted,
            source,
        })
    }

    /// Advance retained, consumed WAL evidence across one authenticated
    /// manifest edge. This never deletes the exact-row evidence; it re-seals
    /// the same bounded WAL with the successor manifest's generation, ledger
    /// pointer, and digest. A crash before this acknowledgement leaves the
    /// immediately-adjacent predecessor relation recoverable.
    fn advance_authenticated_append_wal_supersession(&self) -> anyhow::Result<()> {
        let active_path = Self::append_wal_path(&self.manifest_path)?;
        let Some(active) = Self::read_append_wal(&active_path)? else {
            return Ok(());
        };
        let manifest = Self::read_manifest(&self.manifest_path)?
            .ok_or_else(|| anyhow::anyhow!("retained append WAL has no published manifest"))?;
        let durable_pane_id = uuid::Uuid::from_bytes(self.durable_pane_id)
            .simple()
            .to_string();
        validate_live_scrollback_manifest_identity(
            &manifest,
            &durable_pane_id,
            &self.manifest_path,
        )?;
        {
            let keyring = self
                .lock_keyring("advance append WAL supersession authentication")
                .map_err(anyhow::Error::new)?;
            Self::validate_append_wal_identity(&active, self.durable_pane_id)?;
            Self::authenticate_append_wal(&active, &keyring)?;
            anyhow::ensure!(
                Self::authenticate_manifest(&manifest, &keyring)?,
                "append WAL supersession requires an authenticated manifest"
            );
        }
        let matches_target = Self::append_wal_matches_target_manifest(&active, &manifest)?;
        let v1_to_v4_target = matches_target
            && Self::append_wal_is_v1_to_v4_target_migration(&active, &manifest);
        if matches_target && !v1_to_v4_target {
            return Ok(());
        }
        if Self::append_wal_supersession_matches_manifest(&active, &manifest)? {
            return Ok(());
        }
        if v1_to_v4_target {
            let store = self
                .lock_store("advance v1 append WAL v4 target migration")
                .map_err(anyhow::Error::new)?;
            Self::verify_v1_append_wal_v4_target_migration(&active, &manifest, &store)?;
        } else {
            anyhow::ensure!(
                Self::append_wal_is_immediately_superseded_by_manifest(&active, &manifest)?,
                "retained append WAL is not linked to the current manifest"
            );
        }

        let (content_epoch, revision) = live_scrollback_manifest_generation(&manifest)?
            .ok_or_else(|| anyhow::anyhow!("superseding manifest has no generation"))?;
        let mut advanced = active;
        advanced.superseding_content_epoch = Some(hex::encode(content_epoch));
        advanced.superseding_revision = Some(revision);
        advanced.superseding_ledger_pane_id =
            Some(Self::manifest_ledger_pane_id(&manifest)?);
        advanced.superseding_manifest_sha256 = Some(manifest.manifest_sha256.clone());
        advanced.guardian_authentication = Some("pending".to_string());
        advanced.wal_sha256.clear();
        Self::validate_append_wal_identity(&advanced, self.durable_pane_id)?;
        advanced.guardian_authentication = None;
        let canonical = Self::append_wal_authentication_bytes(&advanced)?;
        {
            let mut keyring = self
                .lock_keyring("seal append WAL supersession")
                .map_err(anyhow::Error::new)?;
            let cipher = keyring
                .latest_active_cipher()
                .context("load guardian key for append WAL supersession")?;
            advanced.guardian_authentication = Some(
                cipher
                    .authenticate_scrollback_append_wal(&canonical)
                    .context("authenticate append WAL supersession")?
                    .encode(),
            );
        }
        Self::validate_append_wal_identity(&advanced, self.durable_pane_id)?;
        advanced.wal_sha256 = Self::append_wal_checksum(&advanced)?;
        self.persist_authenticated_append_wal(&advanced)
            .map_err(anyhow::Error::new)
    }

    fn manifest_ledger_pane_id(manifest: &LiveScrollbackManifestV1) -> anyhow::Result<u64> {
        match manifest.schema.as_str() {
            LIVE_SCROLLBACK_MANIFEST_SCHEMA_V1 | LIVE_SCROLLBACK_MANIFEST_SCHEMA_V2 => {
                anyhow::ensure!(
                    manifest.content_log == "0.log" && manifest.content_sequence.is_none(),
                    "legacy scrollback manifest names an unsupported content ledger"
                );
                Ok(0)
            }
            LIVE_SCROLLBACK_MANIFEST_SCHEMA_V3 | LIVE_SCROLLBACK_MANIFEST_SCHEMA_V4 => {
                let encoded = manifest
                    .content_log
                    .strip_suffix(".log")
                    .ok_or_else(|| anyhow::anyhow!("invalid v3 scrollback content-log pointer"))?;
                anyhow::ensure!(
                    !encoded.is_empty() && encoded.bytes().all(|byte| byte.is_ascii_digit()),
                    "invalid v3 scrollback content-log pointer"
                );
                let pane_id: u64 = encoded
                    .parse()
                    .context("decode v3 scrollback content-log pointer")?;
                let expected_sequence = format!("{pane_id}.seq");
                anyhow::ensure!(
                    manifest.content_log == format!("{pane_id}.log")
                        && manifest.content_sequence.as_deref()
                            == Some(expected_sequence.as_str()),
                    "noncanonical or inconsistent v3 scrollback ledger pointer"
                );
                Ok(pane_id)
            }
            _ => anyhow::bail!("unsupported live scrollback manifest schema"),
        }
    }

    fn authenticate_manifest(
        manifest: &LiveScrollbackManifestV1,
        keyring: &guardian_output_keys::GuardianOutputKeyring,
    ) -> anyhow::Result<bool> {
        let _ledger_pane_id = Self::manifest_ledger_pane_id(manifest)?;
        if !live_scrollback_manifest_is_authenticated(manifest) {
            anyhow::ensure!(
                matches!(
                    manifest.schema.as_str(),
                    LIVE_SCROLLBACK_MANIFEST_SCHEMA_V1 | LIVE_SCROLLBACK_MANIFEST_SCHEMA_V2
                ),
                "unsupported legacy scrollback manifest schema"
            );
            anyhow::ensure!(
                manifest.predecessor_content_epoch.is_none()
                    && manifest.predecessor_revision.is_none()
                    && manifest.newest_stable_row_exclusive.is_none()
                    && manifest.retained_record_bytes.is_none()
                    && manifest.committed_log_bytes.is_none()
                    && manifest.committed_sequence_bytes.is_none()
                    && manifest.logical_ledger_sha256.is_none()
                    && manifest.chain_anchor_sha256.is_none()
                    && manifest.chain_tail_sha256.is_none()
                    && manifest.guardian_manifest_authentication.is_none(),
                "legacy scrollback manifest contains authenticated authority fields"
            );
            return Ok(false);
        }
        anyhow::ensure!(
            manifest.content_epoch.is_some()
                && manifest.revision.is_some()
                && manifest.retained_record_bytes.is_some()
                && manifest.committed_log_bytes.is_some()
                && manifest.committed_sequence_bytes.is_some(),
            "authenticated scrollback manifest is missing mandatory generation or byte bounds"
        );
        let retained_record_bytes = manifest
            .retained_record_bytes
            .ok_or_else(|| anyhow::anyhow!("v3 retained-record byte bound is missing"))?;
        let committed_log_bytes = manifest
            .committed_log_bytes
            .ok_or_else(|| anyhow::anyhow!("v3 committed-log byte bound is missing"))?;
        let committed_sequence_bytes = manifest
            .committed_sequence_bytes
            .ok_or_else(|| anyhow::anyhow!("v3 sequence-journal byte bound is missing"))?;
        anyhow::ensure!(
            manifest.retained_rows <= manifest.max_retained_rows
                && retained_record_bytes <= committed_log_bytes
                && committed_log_bytes <= LIVE_SCROLLBACK_EXPORT_MAX_PHYSICAL_BYTES
                && committed_sequence_bytes <= LIVE_SCROLLBACK_MAX_SEQUENCE_JOURNAL_BYTES,
            "authenticated scrollback manifest exceeds its canonical ledger bounds"
        );
        anyhow::ensure!(
            (manifest.retained_rows == 0) == (retained_record_bytes == 0),
            "authenticated scrollback manifest has inconsistent retained-record bytes"
        );
        match manifest.schema.as_str() {
            LIVE_SCROLLBACK_MANIFEST_SCHEMA_V3 => {
                expected_live_scrollback_logical_ledger_digest(manifest)?;
                anyhow::ensure!(
                    manifest.chain_anchor_sha256.is_none()
                        && manifest.chain_tail_sha256.is_none(),
                    "v3 manifest contains v4 incremental authority"
                );
            }
            LIVE_SCROLLBACK_MANIFEST_SCHEMA_V4 => {
                expected_live_scrollback_v4_chain(manifest)?;
                anyhow::ensure!(
                    manifest.logical_ledger_sha256.is_none(),
                    "v4 manifest contains a non-incremental logical ledger digest"
                );
            }
            _ => unreachable!("authenticated schema checked above"),
        }
        anyhow::ensure!(
            (manifest.publication_state == "cleared")
                == manifest.newest_stable_row_exclusive.is_none(),
            "authenticated scrollback manifest has an invalid stable-row endpoint"
        );
        anyhow::ensure!(
            manifest.predecessor_content_epoch.is_some()
                == manifest.predecessor_revision.is_some(),
            "authenticated scrollback manifest has an incomplete predecessor generation"
        );
        if let Some(predecessor_epoch) = manifest.predecessor_content_epoch.as_deref() {
            anyhow::ensure!(
                predecessor_epoch.len() == 32
                    && predecessor_epoch
                        .bytes()
                        .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f')),
                "authenticated scrollback manifest has an invalid predecessor epoch"
            );
        }
        let encoded_authentication = manifest
            .guardian_manifest_authentication
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("v3 scrollback manifest is not authenticated"))?;
        let authentication = mux::guardian_output_journal::GuardianScrollbackManifestAuthentication::parse(
            encoded_authentication,
        )
        .context("parse guardian scrollback manifest authentication")?;
        let cipher = keyring
            .cipher_for_key_id(authentication.key_id())
            .context("load historical guardian manifest-authentication key")?;
        let canonical = Self::manifest_authentication_bytes(manifest)?;
        cipher
            .verify_scrollback_manifest(&authentication, &canonical)
            .context("authenticate v3 scrollback generation and ledger pointer")?;
        Ok(true)
    }

    fn authenticate_manifest_for_read(
        base_dir: &std::path::Path,
        manifest: &LiveScrollbackManifestV1,
    ) -> anyhow::Result<bool> {
        if live_scrollback_manifest_is_authenticated(manifest) {
            let keyring =
                guardian_output_keys::GuardianOutputKeyring::open_existing_scrollback_sibling(
                    base_dir,
                )
                .context("open guardian output keyring for v3 scrollback manifest")?;
            anyhow::ensure!(
                Self::authenticate_manifest(manifest, &keyring)?,
                "v3 scrollback manifest did not authenticate"
            );
            return Ok(true);
        }

        Self::manifest_ledger_pane_id(manifest)?;
        anyhow::ensure!(
            matches!(
                manifest.schema.as_str(),
                LIVE_SCROLLBACK_MANIFEST_SCHEMA_V1 | LIVE_SCROLLBACK_MANIFEST_SCHEMA_V2
            ),
            "unsupported legacy scrollback manifest schema"
        );
        anyhow::ensure!(
            manifest.newest_stable_row_exclusive.is_none()
                && manifest.predecessor_content_epoch.is_none()
                && manifest.predecessor_revision.is_none()
                && manifest.retained_record_bytes.is_none()
                && manifest.committed_log_bytes.is_none()
                && manifest.committed_sequence_bytes.is_none()
                && manifest.logical_ledger_sha256.is_none()
                && manifest.chain_anchor_sha256.is_none()
                && manifest.chain_tail_sha256.is_none()
                && manifest.guardian_manifest_authentication.is_none(),
            "legacy scrollback manifest contains authenticated authority fields"
        );
        Ok(false)
    }

    fn replacement_manifest_matches(
        &self,
        manifest: &LiveScrollbackManifestV1,
        proposed_state: LiveScrollbackSpillState,
        expected_generation: Option<wezterm_term::config::ScrollbackSnapshotGeneration>,
        oldest_stable_row: Option<wezterm_term::StableRowIndex>,
        newest_stable_row_exclusive: wezterm_term::StableRowIndex,
        row_count: usize,
        staged: frankenterm_core::storage::mmap_store::MmapStagedPaneLedger,
    ) -> anyhow::Result<bool> {
        let keyring = self
            .keyring
            .lock()
            .map_err(|_| anyhow::anyhow!("guardian output keyring is poisoned"))?;
        anyhow::ensure!(
            Self::authenticate_manifest(manifest, &keyring)?,
            "replacement manifest is not an authenticated generation"
        );
        drop(keyring);
        {
            let store = self
                .store
                .lock()
                .map_err(|_| anyhow::anyhow!("scrollback store is poisoned"))?;
            Self::verify_logical_ledger_digest_from_store(
                manifest,
                staged.pane_id(),
                &store,
            )
            .context("verify replacement manifest logical ledger digest")?;
        }
        let durable_pane_id = uuid::Uuid::from_bytes(self.durable_pane_id)
            .simple()
            .to_string();
        validate_live_scrollback_manifest_identity(
            manifest,
            &durable_pane_id,
            &self.manifest_path,
        )?;
        let row_count_u64 = u64::try_from(row_count)
            .map_err(|_| anyhow::anyhow!("replacement row count exceeds u64"))?;
        let manifest_generation = live_scrollback_manifest_generation(manifest)?.map(
            |(content_epoch, revision)| {
                wezterm_term::config::ScrollbackSnapshotGeneration::new(content_epoch, revision)
            },
        );
        Ok(manifest.schema == LIVE_SCROLLBACK_MANIFEST_SCHEMA_V4
            && manifest.publication_state == "complete"
            && manifest_generation == Some(proposed_state.snapshot_generation())
            && live_scrollback_manifest_predecessor(manifest)? == expected_generation
            && Self::manifest_ledger_pane_id(manifest)? == staged.pane_id()
            && manifest.initial_stable_row == oldest_stable_row
            && manifest.newest_stable_row_exclusive == Some(newest_stable_row_exclusive)
            && manifest.max_retained_rows
                == u64::try_from(proposed_state.max_retained_rows)
                    .map_err(|_| anyhow::anyhow!("replacement retention exceeds u64"))?
            && manifest.oldest_seq == (row_count != 0).then_some(0)
            && manifest.retained_rows == row_count_u64
            && manifest.next_seq == row_count_u64
            && manifest.retained_record_bytes == Some(staged.committed_bytes())
            && manifest.committed_log_bytes == Some(staged.committed_bytes())
            && manifest.committed_sequence_bytes == Some(0))
    }

    fn reread_and_verify_replacement_manifest(
        &self,
        proposed_state: LiveScrollbackSpillState,
        expected_generation: Option<wezterm_term::config::ScrollbackSnapshotGeneration>,
        oldest_stable_row: Option<wezterm_term::StableRowIndex>,
        newest_stable_row_exclusive: wezterm_term::StableRowIndex,
        row_count: usize,
        staged: frankenterm_core::storage::mmap_store::MmapStagedPaneLedger,
    ) -> anyhow::Result<()> {
        let manifest = Self::read_manifest(&self.manifest_path)?
            .ok_or_else(|| anyhow::anyhow!("published replacement manifest is missing"))?;
        anyhow::ensure!(
            self.replacement_manifest_matches(
                &manifest,
                proposed_state,
                expected_generation,
                oldest_stable_row,
                newest_stable_row_exclusive,
                row_count,
                staged,
            )?,
            "published replacement manifest does not name the staged ledger"
        );
        Ok(())
    }

    fn validate_persisted_records(
        store: &frankenterm_core::storage::mmap_store::MmapScrollbackStore,
        pane_id: u64,
        keyring: &guardian_output_keys::GuardianOutputKeyring,
        durable_pane_id: [u8; 16],
        content_epoch: [u8; 16],
        initial_stable_row: wezterm_term::StableRowIndex,
        require_exact_semantic: bool,
    ) -> anyhow::Result<()> {
        let retained_rows = store.line_count(pane_id);
        if retained_rows == 0 {
            return Ok(());
        }
        let oldest_seq = store.oldest_seq(pane_id).ok_or_else(|| {
            anyhow::anyhow!("non-empty scrollback log has no oldest sequence")
        })?;
        let mut cipher_cache = GuardianScrollbackCipherCache::new(keyring);
        for offset in 0..retained_rows {
            let seq = oldest_seq
                .checked_add(
                    u64::try_from(offset)
                        .map_err(|_| anyhow::anyhow!("scrollback offset exceeds u64"))?,
                )
                .ok_or_else(|| anyhow::anyhow!("scrollback sequence overflow"))?;
            let record = store
                .line_at(pane_id, seq)?
                .ok_or_else(|| anyhow::anyhow!("scrollback log is missing sequence {seq}"))?;
            let stable_offset = wezterm_term::StableRowIndex::try_from(seq)
                .map_err(|_| anyhow::anyhow!("scrollback sequence exceeds stable row range"))?;
            let stable_row = initial_stable_row
                .checked_add(stable_offset)
                .ok_or_else(|| anyhow::anyhow!("scrollback stable row range overflow"))?;
            let (_line, _decoded_bytes, fidelity) = decode_persisted_scrollback_line_with_limit(
                &record,
                &mut cipher_cache,
                durable_pane_id,
                content_epoch,
                stable_row,
                seq,
                LIVE_SCROLLBACK_MAX_DECODED_LINE_BYTES_USIZE,
            )
            .with_context(|| format!("validate persisted scrollback record {seq}"))?;
            anyhow::ensure!(
                !require_exact_semantic
                    || fidelity == DecodedScrollbackRecordFidelity::ExactSemantic,
            "authenticated scrollback ledger contains a legacy/non-exact row"
            );
        }
        Ok(())
    }

    fn logical_ledger_digest_from_store(
        manifest: &LiveScrollbackManifestV1,
        pane_id: u64,
        store: &frankenterm_core::storage::mmap_store::MmapScrollbackStore,
    ) -> anyhow::Result<[u8; 32]> {
        if manifest.schema == LIVE_SCROLLBACK_MANIFEST_SCHEMA_V4 {
            let (anchor, expected_tail) = expected_live_scrollback_v4_chain(manifest)?;
            let scanned = if manifest.publication_state == "cleared" {
                VerifiedLedgerState::empty(pane_id)
            } else {
                VerifiedLedgerState::scan_store(pane_id, store, anchor)?
            };
            anyhow::ensure!(
                scanned.oldest_sequence == manifest.oldest_seq
                    && scanned.next_sequence == manifest.next_seq
                    && scanned.record_count == manifest.retained_rows
                    && scanned.retained_record_bytes == manifest.retained_record_bytes.unwrap_or(0)
                    && scanned.chain_anchor == anchor
                    && scanned.chain_tail == expected_tail
                    && (manifest.publication_state == "cleared"
                        || (manifest.committed_log_bytes == Some(store.file_bytes(pane_id))
                            && manifest.committed_sequence_bytes
                                == Some(store.sequence_file_bytes(pane_id)?))),
                "authenticated v4 incremental ledger authority mismatch"
            );
            return Ok(scanned.chain_tail);
        }
        let (oldest_sequence, next_sequence, record_count, retained_record_bytes, committed_log_bytes, committed_sequence_bytes) =
            if manifest.publication_state == "cleared" {
                (None, 0, 0, 0, 0, 0)
            } else {
                (
                    store.oldest_seq(pane_id),
                    store.next_seq(pane_id)?,
                    u64::try_from(store.line_count(pane_id))
                        .map_err(|_| anyhow::anyhow!("logical ledger row count exceeds u64"))?,
                    store.retained_record_bytes(pane_id),
                    store.file_bytes(pane_id),
                    store.sequence_file_bytes(pane_id)?,
                )
            };
        let mut hasher = LiveScrollbackLogicalLedgerHasher::new(
            manifest,
            pane_id,
            oldest_sequence,
            next_sequence,
            record_count,
            retained_record_bytes,
            committed_log_bytes,
            committed_sequence_bytes,
        )?;
        if let Some(oldest_sequence) = oldest_sequence {
            for offset in 0..record_count {
                let sequence = oldest_sequence
                    .checked_add(offset)
                    .ok_or_else(|| anyhow::anyhow!("logical ledger sequence overflows"))?;
                let record = live_scrollback_authority_record_at(store, pane_id, sequence)?;
                hasher.observe(sequence, &record)?;
            }
        }
        hasher.finish()
    }

    fn verify_logical_ledger_digest_from_store(
        manifest: &LiveScrollbackManifestV1,
        pane_id: u64,
        store: &frankenterm_core::storage::mmap_store::MmapScrollbackStore,
    ) -> anyhow::Result<[u8; 32]> {
        let observed = Self::logical_ledger_digest_from_store(manifest, pane_id, store)?;
        if manifest.schema == LIVE_SCROLLBACK_MANIFEST_SCHEMA_V3 {
            anyhow::ensure!(
                observed == expected_live_scrollback_logical_ledger_digest(manifest)?,
                "authenticated v3 logical ledger digest mismatch"
            );
        } else {
            anyhow::ensure!(
                manifest.schema == LIVE_SCROLLBACK_MANIFEST_SCHEMA_V4
                    && observed == expected_live_scrollback_v4_chain(manifest)?.1,
                "authenticated v4 incremental ledger digest mismatch"
            );
        }
        Ok(observed)
    }

    fn new(
        base_dir: PathBuf,
        context: &config::ScrollbackSpillSinkContext,
    ) -> anyhow::Result<Self> {
        let pane_id = 0;
        let content_epoch = *uuid::Uuid::new_v4().as_bytes();
        let durable_pane_id = uuid::Uuid::from_bytes(context.durable_pane_id)
            .simple()
            .to_string();
        let durable_pane_dir = base_dir.join(&durable_pane_id);
        let manifest_path = durable_pane_dir.join("manifest.json");
        std::fs::create_dir_all(&durable_pane_dir).with_context(|| {
            format!(
                "create private scrollback directory {}",
                durable_pane_dir.display()
            )
        })?;
        let directory_metadata_before = std::fs::symlink_metadata(&durable_pane_dir)
            .with_context(|| format!("inspect scrollback directory {}", durable_pane_dir.display()))?;
        if !directory_metadata_before.file_type().is_dir() {
            anyhow::bail!(
                "scrollback pane path is not a directory: {}",
                durable_pane_dir.display()
            );
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};

            std::fs::set_permissions(&durable_pane_dir, std::fs::Permissions::from_mode(0o700))
                .with_context(|| {
                    format!(
                        "harden scrollback directory permissions {}",
                        durable_pane_dir.display()
                    )
                })?;
            let directory_metadata_after = std::fs::symlink_metadata(&durable_pane_dir)
                .with_context(|| {
                    format!(
                        "revalidate scrollback directory {}",
                        durable_pane_dir.display()
                    )
                })?;
            if !directory_metadata_after.file_type().is_dir()
                || directory_metadata_before.dev() != directory_metadata_after.dev()
                || directory_metadata_before.ino() != directory_metadata_after.ino()
            {
                anyhow::bail!(
                    "scrollback pane directory changed identity during initialization: {}",
                    durable_pane_dir.display()
                );
            }
        }
        #[cfg(not(windows))]
        {
            std::fs::File::open(&base_dir)
                .and_then(|directory| directory.sync_all())
                .with_context(|| {
                    format!(
                        "synchronize durable scrollback pane parent {}",
                        base_dir.display()
                    )
                })?;
            if let Some(base_parent) = base_dir.parent() {
                std::fs::File::open(base_parent)
                    .and_then(|directory| directory.sync_all())
                    .with_context(|| {
                        format!(
                            "synchronize scrollback storage parent {}",
                            base_parent.display()
                        )
                    })?;
            }
        }
        let _filesystem_mutation_lease =
            acquire_live_scrollback_filesystem_mutation_lease(&durable_pane_dir, true).with_context(
                || {
                    format!(
                        "acquire scrollback mutation authority {}",
                        durable_pane_dir.display()
                    )
                },
            )?;
        let keyring = guardian_output_keys::GuardianOutputKeyring::shared_scrollback_sibling(
            &base_dir,
        )
        .context("open shared guardian output keyring for encrypted scrollback")?;
        let redactor = frankenterm_core::redactor::Redactor::new();
        let command_description = redactor.redact(&context.command_description);
        let mut persisted_manifest = Self::read_manifest(&manifest_path)?;
        let (ledger_pane_id, authenticated_manifest) = match persisted_manifest.as_ref() {
            Some(manifest) => {
                validate_live_scrollback_manifest_identity(
                    manifest,
                    &durable_pane_id,
                    &manifest_path,
                )
                .context("validate scrollback manifest before following its ledger")?;
                let keyring_guard = keyring
                    .lock()
                    .map_err(|_| anyhow::anyhow!("guardian output keyring is poisoned"))?;
                let authenticated = Self::authenticate_manifest(manifest, &keyring_guard)
                    .with_context(|| {
                        format!(
                            "authenticate scrollback manifest before following its ledger at {}",
                            manifest_path.display()
                        )
                    })?;
                drop(keyring_guard);
                (Self::manifest_ledger_pane_id(manifest)?, authenticated)
            }
            None => (pane_id, true),
        };
        let mut store = frankenterm_core::storage::mmap_store::MmapScrollbackStore::new(
            frankenterm_core::storage::mmap_store::MmapStoreConfig::new(durable_pane_dir),
        )?;
        match persisted_manifest.as_ref() {
            Some(manifest) if manifest.publication_state == "cleared" => {
                store.ensure_pane(ledger_pane_id)?;
            }
            Some(_) => store.open_existing_pane(ledger_pane_id)?,
            None => store.ensure_pane(ledger_pane_id)?,
        }
        let append_wal_path = Self::append_wal_path(&manifest_path)?;
        let append_wal_stage_path = Self::append_wal_stage_path(&manifest_path)?;
        let active_append_wal = Self::read_append_wal(&append_wal_path)?;
        if let Some(wal) = active_append_wal.as_ref() {
            let published_manifest = persisted_manifest.as_ref().ok_or_else(|| {
                anyhow::anyhow!("scrollback append WAL has no published predecessor manifest")
            })?;
            let keyring_guard = keyring
                .lock()
                .map_err(|_| anyhow::anyhow!("guardian output keyring is poisoned"))?;
            Self::validate_append_wal_identity(wal, context.durable_pane_id)?;
            Self::authenticate_append_wal(wal, &keyring_guard)?;
            if Self::append_wal_is_v1_to_v4_target_migration(wal, published_manifest)
                && Self::append_wal_matches_target_manifest(wal, published_manifest)?
            {
                Self::verify_v1_append_wal_v4_target_migration(
                    wal,
                    published_manifest,
                    &store,
                )?;
            }
            anyhow::ensure!(
                Self::append_wal_matches_predecessor_manifest(wal, published_manifest)?
                    || Self::append_wal_is_consumed_or_superseded(
                        wal,
                        published_manifest,
                    )?,
                "active append WAL is neither adjacent to nor authentically superseded by the published manifest"
            );
        }
        if authenticated_manifest {
            let stage_path = Self::deterministic_manifest_stage_path(&manifest_path)?;
            let retained_manifest_stage = match Self::read_manifest(&stage_path) {
                Ok(stage) => stage,
                Err(error) if Self::manifest_stage_is_recoverably_incomplete(&stage_path)? => {
                    log::warn!(
                        "retaining a securely pinned incomplete scrollback manifest stage for deterministic recovery: {error}"
                    );
                    None
                }
                Err(error) => return Err(error),
            };
            if let Some(staged_manifest) = retained_manifest_stage {
                if staged_manifest.publication_state == "complete" {
                    validate_live_scrollback_manifest_identity(
                        &staged_manifest,
                        &durable_pane_id,
                        &stage_path,
                    )
                    .context("validate retained complete scrollback manifest stage")?;
                    {
                        let keyring_guard = keyring
                            .lock()
                            .map_err(|_| anyhow::anyhow!("guardian output keyring is poisoned"))?;
                        anyhow::ensure!(
                            Self::authenticate_manifest(&staged_manifest, &keyring_guard)?,
                            "retained complete scrollback manifest stage is not authenticated"
                        );
                    }
                    let staged_ledger_pane_id =
                        Self::manifest_ledger_pane_id(&staged_manifest)?;
                    if staged_ledger_pane_id == ledger_pane_id {
                        let published_manifest = persisted_manifest.as_ref().ok_or_else(|| {
                            anyhow::anyhow!(
                                "complete scrollback manifest stage has no published predecessor"
                            )
                        })?;
                        let published_generation = live_scrollback_manifest_generation(
                            published_manifest,
                        )?
                        .map(|(epoch, revision)| {
                            wezterm_term::config::ScrollbackSnapshotGeneration::new(
                                epoch, revision,
                            )
                        })
                        .ok_or_else(|| {
                            anyhow::anyhow!(
                                "complete scrollback manifest stage follows a legacy generation"
                            )
                        })?;
                        let staged_generation =
                            live_scrollback_manifest_generation(&staged_manifest)?
                                .map(|(epoch, revision)| {
                                    wezterm_term::config::ScrollbackSnapshotGeneration::new(
                                        epoch, revision,
                                    )
                                })
                                .ok_or_else(|| {
                                    anyhow::anyhow!(
                                        "complete scrollback manifest stage has no generation"
                                    )
                                })?;
                        let staged_predecessor =
                            live_scrollback_manifest_predecessor(&staged_manifest)?;
                        let same_prepared_generation =
                            published_manifest.publication_state == "prepared"
                                && staged_generation == published_generation
                                && staged_predecessor
                                    == live_scrollback_manifest_predecessor(published_manifest)?;
                        let exact_complete_successor =
                            published_manifest.publication_state == "complete"
                                && staged_generation.content_epoch()
                                    == published_generation.content_epoch()
                                && staged_generation.revision()
                                    == published_generation.revision().checked_add(1).ok_or_else(
                                        || {
                                            anyhow::anyhow!(
                                                "published scrollback revision is exhausted"
                                            )
                                        },
                                    )?
                                && staged_predecessor == Some(published_generation);
                        anyhow::ensure!(
                            same_prepared_generation || exact_complete_successor,
                            "retained complete scrollback manifest stage is not the exact same-ledger successor"
                        );
                        Self::verify_logical_ledger_digest_from_store(
                            &staged_manifest,
                            staged_ledger_pane_id,
                            &store,
                        )
                        .context("verify retained complete scrollback ledger stage")?;
                        if staged_manifest.retained_rows != 0 {
                            let initial_stable_row = staged_manifest.initial_stable_row.ok_or_else(
                                || {
                                    anyhow::anyhow!(
                                        "retained complete scrollback stage has no stable-row origin"
                                    )
                                },
                            )?;
                            let keyring_guard = keyring.lock().map_err(|_| {
                                anyhow::anyhow!("guardian output keyring is poisoned")
                            })?;
                            Self::validate_persisted_records(
                                &store,
                                staged_ledger_pane_id,
                                &keyring_guard,
                                context.durable_pane_id,
                                staged_generation.content_epoch(),
                                initial_stable_row,
                                true,
                            )
                            .context("authenticate retained complete scrollback rows")?;
                        }

                        std::fs::rename(&stage_path, &manifest_path).with_context(|| {
                            format!(
                                "publish retained complete scrollback manifest {}",
                                manifest_path.display()
                            )
                        })?;
                        #[cfg(not(windows))]
                        std::fs::File::open(
                            manifest_path.parent().ok_or_else(|| {
                                anyhow::anyhow!("scrollback manifest path has no parent")
                            })?,
                        )?
                        .sync_all()?;
                        let recovered = Self::read_manifest(&manifest_path)?.ok_or_else(|| {
                            anyhow::anyhow!("recovered scrollback manifest disappeared")
                        })?;
                        anyhow::ensure!(
                            recovered == staged_manifest,
                            "recovered scrollback manifest changed during publication"
                        );
                        {
                            let keyring_guard = keyring.lock().map_err(|_| {
                                anyhow::anyhow!("guardian output keyring is poisoned")
                            })?;
                            anyhow::ensure!(
                                Self::authenticate_manifest(&recovered, &keyring_guard)?,
                                "recovered scrollback manifest lost guardian authority"
                            );
                        }
                        Self::verify_logical_ledger_digest_from_store(
                            &recovered,
                            staged_ledger_pane_id,
                            &store,
                        )
                        .context("reverify recovered complete scrollback ledger")?;
                        persisted_manifest = Some(recovered);
                    }
                }
            }
        }
        let mut wal_recovered_state = match (
            active_append_wal.as_ref(),
            persisted_manifest.as_ref(),
        ) {
            (Some(wal), Some(manifest)) => {
                let keyring_guard = keyring
                    .lock()
                    .map_err(|_| anyhow::anyhow!("guardian output keyring is poisoned"))?;
                Self::reconcile_authenticated_append_wal(
                    wal,
                    manifest,
                    &mut store,
                    &keyring_guard,
                    context.durable_pane_id,
                )?
            }
            (Some(_), None) => {
                anyhow::bail!("scrollback append WAL has no published predecessor manifest")
            }
            (None, _) => None,
        };

        if wal_recovered_state.is_some() {
            anyhow::ensure!(
                std::fs::symlink_metadata(&append_wal_stage_path)
                    .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound),
                "an unconsumed append WAL conflicts with a second staged transaction"
            );
        } else {
            let staged_append_wal = match Self::read_append_wal(&append_wal_stage_path) {
                Ok(wal) => wal,
                Err(error)
                    if Self::append_wal_stage_is_recoverably_incomplete(
                        &append_wal_stage_path,
                    )? =>
                {
                    let authoritative_state_is_intact = match persisted_manifest.as_ref() {
                        Some(manifest)
                            if live_scrollback_manifest_is_authenticated(manifest) =>
                        {
                            Self::verify_logical_ledger_digest_from_store(
                                manifest,
                                ledger_pane_id,
                                &store,
                            )
                            .is_ok()
                        }
                        None => {
                            store.line_count(ledger_pane_id) == 0
                                && store.oldest_seq(ledger_pane_id).is_none()
                                && store.next_seq(ledger_pane_id)? == 0
                        }
                        _ => false,
                    };
                    anyhow::ensure!(
                        authoritative_state_is_intact,
                        "an incomplete append WAL stage accompanies non-authoritative ledger state: {error}"
                    );
                    log::warn!(
                        "ignoring a securely pinned incomplete scrollback append-WAL stage; no ledger mutation was authorized"
                    );
                    None
                }
                Err(error) => return Err(error),
            };
            if let Some(staged_wal) = staged_append_wal {
                let published_manifest = persisted_manifest.as_ref().ok_or_else(|| {
                    anyhow::anyhow!("staged scrollback append WAL has no predecessor manifest")
                })?;
                {
                    let keyring_guard = keyring
                        .lock()
                        .map_err(|_| anyhow::anyhow!("guardian output keyring is poisoned"))?;
                    Self::validate_append_wal_identity(&staged_wal, context.durable_pane_id)?;
                    Self::authenticate_append_wal(&staged_wal, &keyring_guard)?;
                    if Self::append_wal_is_v1_to_v4_target_migration(
                        &staged_wal,
                        published_manifest,
                    ) && Self::append_wal_matches_target_manifest(
                        &staged_wal,
                        published_manifest,
                    )? {
                        Self::verify_v1_append_wal_v4_target_migration(
                            &staged_wal,
                            published_manifest,
                            &store,
                        )?;
                    }
                    if let Some(active_wal) = active_append_wal.as_ref() {
                        if Self::append_wal_matches_target_manifest(
                            active_wal,
                            published_manifest,
                        )? {
                            if Self::append_wal_is_v1_to_v4_target_migration(
                                active_wal,
                                published_manifest,
                            ) {
                                Self::verify_v1_append_wal_v4_target_migration(
                                    active_wal,
                                    published_manifest,
                                    &store,
                                )?;
                            } else {
                                Self::verify_append_wal_target_store(active_wal, &store)?;
                                Self::verify_logical_ledger_digest_from_store(
                                    published_manifest,
                                    active_wal.ledger_pane_id,
                                    &store,
                                )
                                .context(
                                    "bind consumed append WAL before staged successor recovery",
                                )?;
                            }
                        } else {
                            anyhow::ensure!(
                                Self::append_wal_supersession_matches_manifest(
                                    active_wal,
                                    published_manifest,
                                )? || Self::append_wal_is_immediately_superseded_by_manifest(
                                    active_wal,
                                    published_manifest,
                                )?,
                                "staged append WAL conflicts with an unconsumed active transaction"
                            );
                        }
                    }
                    anyhow::ensure!(
                        Self::append_wal_matches_predecessor_manifest(
                            &staged_wal,
                            published_manifest,
                        )? || Self::append_wal_is_consumed_or_superseded(
                            &staged_wal,
                            published_manifest,
                        )?,
                        "staged append WAL is neither adjacent to nor authentically superseded by the published generation"
                    );
                }
                match std::fs::rename(&append_wal_stage_path, &append_wal_path) {
                    Ok(()) => {}
                    Err(rename_error) => {
                        let observed = Self::read_append_wal(&append_wal_path)?;
                        anyhow::ensure!(
                            observed.as_ref() == Some(&staged_wal),
                            "publish retained append WAL stage: {rename_error}"
                        );
                    }
                }
                #[cfg(not(windows))]
                std::fs::File::open(
                    append_wal_path.parent().ok_or_else(|| {
                        anyhow::anyhow!("scrollback append WAL has no parent")
                    })?,
                )?
                .sync_all()?;
                let published_wal = Self::read_append_wal(&append_wal_path)?
                    .ok_or_else(|| anyhow::anyhow!("published append WAL disappeared"))?;
                anyhow::ensure!(
                    published_wal == staged_wal,
                    "published append WAL changed during acknowledgement"
                );
                let keyring_guard = keyring
                    .lock()
                    .map_err(|_| anyhow::anyhow!("guardian output keyring is poisoned"))?;
                wal_recovered_state = Self::reconcile_authenticated_append_wal(
                    &published_wal,
                    published_manifest,
                    &mut store,
                    &keyring_guard,
                    context.durable_pane_id,
                )?;
            }
        }

        let (mut state, repair_manifest_publication) = if let Some(state) = wal_recovered_state {
            (state, Some("complete"))
        } else {
            match persisted_manifest {
            Some(manifest) => {
                let persisted_generation = live_scrollback_manifest_generation(&manifest)
                    .with_context(|| {
                        format!(
                            "validate scrollback manifest generation at {}",
                            manifest_path.display()
                        )
                    })?;
                let (state_content_epoch, mut state_revision, _legacy_generation_missing) =
                    persisted_generation
                        .map(|(epoch, revision)| (epoch, revision, false))
                        .unwrap_or((content_epoch, 0, true));
                if manifest.durable_pane_id != durable_pane_id
                    || !matches!(
                        manifest.publication_state.as_str(),
                        "prepared" | "complete" | "cleared"
                    )
                {
                    anyhow::bail!(
                        "scrollback manifest identity/schema mismatch at {}",
                        manifest_path.display()
                    );
                }
                if manifest.publication_state == "cleared" {
                    if authenticated_manifest {
                        Self::verify_logical_ledger_digest_from_store(
                            &manifest,
                            ledger_pane_id,
                            &store,
                        )
                        .with_context(|| {
                            format!(
                                "verify cleared logical ledger digest for {}",
                                manifest_path.display()
                            )
                        })?;
                        store.clear_pane(ledger_pane_id).with_context(|| {
                            format!(
                                "finish authenticated interrupted scrollback clear for {}",
                                manifest_path.display()
                            )
                        })?;
                    } else {
                        anyhow::ensure!(
                            store.line_count(ledger_pane_id) == 0
                                && store.oldest_seq(ledger_pane_id).is_none()
                                && store.next_seq(ledger_pane_id)? == 0,
                            "legacy cleared manifest cannot authorize reclamation of residual scrollback bytes at {}",
                            manifest_path.display()
                        );
                    }
                    let mut state = LiveScrollbackSpillState::empty(state_content_epoch, true);
                    state.revision = state_revision;
                    state.authenticated_manifest = authenticated_manifest;
                    state.predecessor_generation = live_scrollback_manifest_predecessor(&manifest)?;
                    state.clear_manifest_published = true;
                    // A checksum-only legacy clear is readable historical
                    // metadata, never authority to truncate retained bytes or
                    // republish them under a stronger label.
                    (state, None)
                } else {
                    let actual_retained_rows = u64::try_from(store.line_count(ledger_pane_id))
                        .map_err(|_| anyhow::anyhow!("scrollback row count exceeds u64"))?;
                    let actual_oldest_seq = store.oldest_seq(ledger_pane_id);
                    let actual_next_seq = store.next_seq(ledger_pane_id)?;
                    let actual_retained_record_bytes =
                        store.retained_record_bytes(ledger_pane_id);
                    let actual_committed_log_bytes = store.file_bytes(ledger_pane_id);
                    let actual_committed_sequence_bytes =
                        store.sequence_file_bytes(ledger_pane_id)?;
                    let initial_stable_row = manifest.initial_stable_row;
                    if authenticated_manifest {
                        let digest_verified = Self::verify_logical_ledger_digest_from_store(
                            &manifest,
                            ledger_pane_id,
                            &store,
                        )
                        .is_ok();
                        let narrowly_prepared_content_ahead =
                            manifest.publication_state == "prepared"
                                && actual_next_seq > manifest.next_seq
                                && actual_retained_rows > manifest.retained_rows
                                && actual_committed_log_bytes
                                    > manifest.committed_log_bytes.unwrap_or(0);
                        anyhow::ensure!(
                            digest_verified || narrowly_prepared_content_ahead,
                            "authenticated logical ledger digest mismatch for {}",
                            manifest_path.display()
                        );
                    }
                    if actual_retained_rows != 0 {
                        let initial_stable_row = initial_stable_row.ok_or_else(|| {
                            anyhow::anyhow!(
                                "published scrollback rows are missing their initial stable row"
                            )
                        })?;
                        let keyring_guard = keyring
                            .lock()
                            .map_err(|_| anyhow::anyhow!("guardian output keyring is poisoned"))?;
                        Self::validate_persisted_records(
                            &store,
                            ledger_pane_id,
                            &keyring_guard,
                            context.durable_pane_id,
                            state_content_epoch,
                            initial_stable_row,
                            authenticated_manifest,
                        )
                        .with_context(|| {
                            format!(
                                "validate persisted scrollback records for {}",
                                manifest_path.display()
                            )
                        })?;
                    }
                    anyhow::ensure!(
                        (manifest.retained_rows == 0) == manifest.oldest_seq.is_none(),
                        "scrollback manifest has inconsistent retained-row bounds at {}",
                        manifest_path.display()
                    );
                    anyhow::ensure!(
                        (actual_retained_rows == 0) == actual_oldest_seq.is_none(),
                        "scrollback content has inconsistent retained-row bounds at {}",
                        manifest_path.display()
                    );
                    let manifest_interval_end = match manifest.oldest_seq {
                        Some(oldest) => oldest.checked_add(manifest.retained_rows).ok_or_else(|| {
                            anyhow::anyhow!("scrollback manifest sequence range overflows u64")
                        })?,
                        None => manifest.next_seq,
                    };
                    let actual_interval_end = match actual_oldest_seq {
                        Some(oldest) => oldest.checked_add(actual_retained_rows).ok_or_else(|| {
                            anyhow::anyhow!("scrollback content sequence range overflows u64")
                        })?,
                        None => actual_next_seq,
                    };
                    anyhow::ensure!(
                        manifest_interval_end == manifest.next_seq,
                        "scrollback manifest next sequence is inconsistent at {}",
                        manifest_path.display()
                    );
                    anyhow::ensure!(
                        actual_interval_end == actual_next_seq,
                        "scrollback content next sequence is inconsistent at {}",
                        manifest_path.display()
                    );
                    let actual_newest_stable_row_exclusive = match initial_stable_row {
                        Some(initial) => {
                            let next_offset = wezterm_term::StableRowIndex::try_from(actual_next_seq)
                                .map_err(|_| {
                                    anyhow::anyhow!(
                                        "scrollback next sequence exceeds stable-row range"
                                    )
                                })?;
                            Some(initial.checked_add(next_offset).ok_or_else(|| {
                                anyhow::anyhow!("scrollback stable-row range overflows")
                            })?)
                        }
                        None if actual_next_seq == 0 => None,
                        None => anyhow::bail!(
                            "scrollback content has no initial stable-row identity at {}",
                            manifest_path.display()
                        ),
                    };
                    if authenticated_manifest
                        && manifest.publication_state == "complete"
                        && actual_retained_rows != 0
                    {
                        anyhow::ensure!(
                            manifest.newest_stable_row_exclusive
                                == actual_newest_stable_row_exclusive,
                            "authenticated scrollback stable-row endpoint disagrees with its ledger at {}",
                            manifest_path.display()
                        );
                    }
                    if actual_next_seq < manifest.next_seq
                        || matches!(
                            (actual_oldest_seq, manifest.oldest_seq),
                            (Some(actual), Some(recorded)) if actual < recorded
                        )
                    {
                        anyhow::bail!(
                            "scrollback content rolled back behind its published manifest at {}",
                            manifest_path.display()
                        );
                    }
                    let content_repair_required = actual_retained_rows != manifest.retained_rows
                        || actual_oldest_seq != manifest.oldest_seq
                        || actual_next_seq != manifest.next_seq
                        || (authenticated_manifest
                            && manifest.publication_state == "complete"
                            && actual_retained_rows != 0
                            && manifest.newest_stable_row_exclusive
                                != actual_newest_stable_row_exclusive)
                        || (authenticated_manifest
                            && (manifest.retained_record_bytes
                                != Some(actual_retained_record_bytes)
                                || manifest.committed_log_bytes
                                    != Some(actual_committed_log_bytes)
                                || manifest.committed_sequence_bytes
                                    != Some(actual_committed_sequence_bytes)))
                        || (manifest.publication_state == "prepared"
                            && actual_retained_rows > 0);
                    let mut repaired_predecessor =
                        live_scrollback_manifest_predecessor(&manifest)?;
                    if authenticated_manifest
                        && persisted_generation.is_some()
                        && manifest.publication_state == "complete"
                        && content_repair_required
                    {
                        repaired_predecessor = Some(
                            wezterm_term::config::ScrollbackSnapshotGeneration::new(
                                state_content_epoch,
                                state_revision,
                            ),
                        );
                        state_revision = state_revision.checked_add(1).ok_or_else(|| {
                            anyhow::anyhow!("scrollback manifest revision exhausted during repair")
                        })?;
                    }
                    let repair_complete_manifest =
                        authenticated_manifest && content_repair_required;
                    (
                        LiveScrollbackSpillState {
                            initial_stable_row,
                            newest_stable_row_exclusive: if authenticated_manifest {
                                manifest.newest_stable_row_exclusive
                            } else {
                                actual_newest_stable_row_exclusive
                            },
                            max_retained_rows: usize::try_from(manifest.max_retained_rows).map_err(
                                |_| anyhow::anyhow!("scrollback retention exceeds platform usize"),
                            )?,
                            content_epoch: state_content_epoch,
                            revision: state_revision,
                            authenticated_manifest,
                            predecessor_generation: repaired_predecessor,
                            clear_manifest_published: false,
                            clear_pending_physical_reclamation: false,
                            transaction_quarantined: false,
                            verified_ledger: None,
                        },
                        repair_complete_manifest.then_some("complete"),
                    )
                }
            }
            None => {
                if store.line_count(ledger_pane_id) != 0 {
                    anyhow::bail!(
                        "scrollback content exists without an identity manifest at {}",
                        manifest_path.display()
                    );
                }
                (LiveScrollbackSpillState::empty(content_epoch, true), None)
            }
            }
        };
        if state.authenticated_manifest && state.verified_ledger.is_none() {
            state.verified_ledger = if state.clear_manifest_published {
                Some(VerifiedLedgerState::empty(ledger_pane_id))
            } else {
                let published = Self::read_manifest(&manifest_path)?;
                let anchor = published
                    .as_ref()
                    .filter(|manifest| manifest.schema == LIVE_SCROLLBACK_MANIFEST_SCHEMA_V4)
                    .map(expected_live_scrollback_v4_chain)
                    .transpose()?
                    .map_or([0; 32], |(anchor, _tail)| anchor);
                Some(VerifiedLedgerState::scan_store(
                    ledger_pane_id,
                    &store,
                    anchor,
                )?)
            };
        }
        let sink = Self {
            pane_id,
            active_ledger_pane_id: std::sync::atomic::AtomicU64::new(ledger_pane_id),
            durable_pane_id: context.durable_pane_id,
            source_pane_id: context.pane_id,
            source_domain_id: context.domain_id,
            command_description,
            manifest_path,
            mutation_gate: std::sync::Mutex::new(()),
            store: std::sync::Mutex::new(store),
            state: std::sync::Mutex::new(state),
            keyring,
        };
        if let Some(publication_state) = repair_manifest_publication {
            sink.persist_manifest(publication_state).with_context(|| {
                format!(
                    "repair interrupted scrollback manifest publication {}",
                    sink.manifest_path.display()
                )
            })?;
        }
        if let Err(error) = sink.advance_authenticated_append_wal_supersession() {
            log::warn!(
                "deferred retained append WAL supersession acknowledgement during cold open: {error:#}"
            );
        }
        Ok(sink)
    }

    fn read_manifest(path: &std::path::Path) -> anyhow::Result<Option<LiveScrollbackManifestV1>> {
        let path_metadata_before = match std::fs::symlink_metadata(path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("inspect scrollback manifest {}", path.display()));
            }
        };
        if !path_metadata_before.file_type().is_file() {
            anyhow::bail!("scrollback manifest is not a regular file: {}", path.display());
        }
        if path_metadata_before.len() > LIVE_SCROLLBACK_MANIFEST_MAX_BYTES {
            anyhow::bail!("scrollback manifest exceeds 1 MiB: {}", path.display());
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;

            if path_metadata_before.permissions().mode() & 0o077 != 0 {
                anyhow::bail!("scrollback manifest is not private: {}", path.display());
            }
        }
        let mut options = std::fs::OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;

            options.custom_flags(libc::O_NOFOLLOW);
        }
        let file = options
            .open(path)
            .with_context(|| format!("open scrollback manifest {}", path.display()))?;
        let handle_metadata_before = file
            .metadata()
            .with_context(|| format!("inspect opened scrollback manifest {}", path.display()))?;
        if !handle_metadata_before.is_file() {
            anyhow::bail!("opened scrollback manifest is not a regular file: {}", path.display());
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;

            if handle_metadata_before.permissions().mode() & 0o077 != 0 {
                anyhow::bail!("opened scrollback manifest is not private: {}", path.display());
            }
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt as _;

            if handle_metadata_before.dev() != path_metadata_before.dev()
                || handle_metadata_before.ino() != path_metadata_before.ino()
            {
                anyhow::bail!("scrollback manifest changed identity before read: {}", path.display());
            }
        }
        let mut bytes = Vec::new();
        (&file)
            .take(LIVE_SCROLLBACK_MANIFEST_MAX_BYTES.saturating_add(1))
            .read_to_end(&mut bytes)
            .with_context(|| format!("read scrollback manifest {}", path.display()))?;
        if u64::try_from(bytes.len()).unwrap_or(u64::MAX)
            > LIVE_SCROLLBACK_MANIFEST_MAX_BYTES
        {
            anyhow::bail!("scrollback manifest exceeds 1 MiB: {}", path.display());
        }
        let handle_metadata_after = file
            .metadata()
            .with_context(|| format!("reinspect scrollback manifest {}", path.display()))?;
        if filesystem_metadata_changed(&handle_metadata_before, &handle_metadata_after)? {
            anyhow::bail!("scrollback manifest changed while being read: {}", path.display());
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt as _;

            let path_metadata_after = std::fs::symlink_metadata(path).with_context(|| {
                format!("revalidate scrollback manifest {}", path.display())
            })?;
            if !path_metadata_after.file_type().is_file()
                || path_metadata_after.dev() != handle_metadata_before.dev()
                || path_metadata_after.ino() != handle_metadata_before.ino()
            {
                anyhow::bail!("scrollback manifest changed identity during read: {}", path.display());
            }
        }
        let manifest: LiveScrollbackManifestV1 = serde_json::from_slice(&bytes)
            .with_context(|| format!("decode scrollback manifest {}", path.display()))?;
        let expected_checksum = Self::manifest_checksum(&manifest)?;
        if manifest.manifest_sha256.len() != 64
            || !manifest
                .manifest_sha256
                .bytes()
                .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
            || manifest.manifest_sha256 != expected_checksum
        {
            anyhow::bail!("scrollback manifest checksum failed at {}", path.display());
        }
        Ok(Some(manifest))
    }

    fn read_append_wal(
        path: &std::path::Path,
    ) -> anyhow::Result<Option<LiveScrollbackAppendWalV1>> {
        let path_metadata_before = match std::fs::symlink_metadata(path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("inspect scrollback append WAL {}", path.display()));
            }
        };
        anyhow::ensure!(
            path_metadata_before.file_type().is_file(),
            "scrollback append WAL is not a regular file: {}",
            path.display()
        );
        anyhow::ensure!(
            path_metadata_before.len() <= LIVE_SCROLLBACK_APPEND_WAL_MAX_BYTES,
            "scrollback append WAL exceeds its byte ceiling: {}",
            path.display()
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};

            let parent = path.parent().ok_or_else(|| {
                anyhow::anyhow!("scrollback append WAL path has no parent")
            })?;
            let parent_metadata = std::fs::symlink_metadata(parent)?;
            anyhow::ensure!(
                path_metadata_before.permissions().mode() & 0o077 == 0
                    && path_metadata_before.nlink() == 1
                    && path_metadata_before.uid() == parent_metadata.uid(),
                "scrollback append WAL is not private: {}",
                path.display()
            );
        }
        let mut options = std::fs::OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;

            options.custom_flags(libc::O_NOFOLLOW);
        }
        let file = options
            .open(path)
            .with_context(|| format!("open scrollback append WAL {}", path.display()))?;
        let handle_metadata_before = file
            .metadata()
            .with_context(|| format!("inspect opened scrollback append WAL {}", path.display()))?;
        anyhow::ensure!(
            handle_metadata_before.is_file(),
            "opened scrollback append WAL is not a regular file: {}",
            path.display()
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt as _;

            anyhow::ensure!(
                handle_metadata_before.dev() == path_metadata_before.dev()
                    && handle_metadata_before.ino() == path_metadata_before.ino(),
                "scrollback append WAL changed identity before read: {}",
                path.display()
            );
        }
        let mut bytes = Vec::new();
        (&file)
            .take(LIVE_SCROLLBACK_APPEND_WAL_MAX_BYTES.saturating_add(1))
            .read_to_end(&mut bytes)
            .with_context(|| format!("read scrollback append WAL {}", path.display()))?;
        anyhow::ensure!(
            u64::try_from(bytes.len()).unwrap_or(u64::MAX)
                <= LIVE_SCROLLBACK_APPEND_WAL_MAX_BYTES,
            "scrollback append WAL exceeds its byte ceiling: {}",
            path.display()
        );
        let handle_metadata_after = file.metadata().with_context(|| {
            format!("reinspect opened scrollback append WAL {}", path.display())
        })?;
        anyhow::ensure!(
            !filesystem_metadata_changed(&handle_metadata_before, &handle_metadata_after)?,
            "scrollback append WAL changed while being read: {}",
            path.display()
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt as _;

            let path_metadata_after = std::fs::symlink_metadata(path)?;
            anyhow::ensure!(
                path_metadata_after.file_type().is_file()
                    && path_metadata_after.dev() == handle_metadata_before.dev()
                    && path_metadata_after.ino() == handle_metadata_before.ino(),
                "scrollback append WAL changed identity during read: {}",
                path.display()
            );
        }
        let wal: LiveScrollbackAppendWalV1 = serde_json::from_slice(&bytes)
            .with_context(|| format!("decode scrollback append WAL {}", path.display()))?;
        anyhow::ensure!(
            wal.wal_sha256.len() == 64
                && wal
                    .wal_sha256
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
                && wal.wal_sha256 == Self::append_wal_checksum(&wal)?,
            "scrollback append WAL checksum failed at {}",
            path.display()
        );
        Ok(Some(wal))
    }

    /// A deterministic stage is not authoritative until its complete,
    /// authenticated bytes are renamed to the active WAL slot. A crash may
    /// leave a short/invalid JSON prefix here. Accept only a securely pinned,
    /// private regular file as a recoverably incomplete stage; unsafe path
    /// types and metadata still fail closed.
    fn append_wal_stage_is_recoverably_incomplete(
        path: &std::path::Path,
    ) -> anyhow::Result<bool> {
        let path_metadata = match std::fs::symlink_metadata(path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(error.into()),
        };
        anyhow::ensure!(
            path_metadata.file_type().is_file()
                && path_metadata.len() <= LIVE_SCROLLBACK_APPEND_WAL_MAX_BYTES,
            "incomplete append WAL stage is not a bounded regular file"
        );
        let mut options = std::fs::OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::{MetadataExt as _, OpenOptionsExt as _, PermissionsExt as _};

            let parent = path
                .parent()
                .ok_or_else(|| anyhow::anyhow!("append WAL stage has no parent"))?;
            let parent_metadata = std::fs::symlink_metadata(parent)?;
            anyhow::ensure!(
                path_metadata.permissions().mode() & 0o077 == 0
                    && path_metadata.nlink() == 1
                    && path_metadata.uid() == parent_metadata.uid(),
                "incomplete append WAL stage is not private"
            );
            options.custom_flags(libc::O_NOFOLLOW);
        }
        let file = options.open(path)?;
        let handle_metadata = file.metadata()?;
        anyhow::ensure!(handle_metadata.is_file(), "append WAL stage handle is not a file");
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt as _;

            anyhow::ensure!(
                handle_metadata.dev() == path_metadata.dev()
                    && handle_metadata.ino() == path_metadata.ino(),
                "incomplete append WAL stage changed identity"
            );
        }
        Ok(true)
    }

    /// A deterministic manifest stage is not authoritative until its complete
    /// bytes have passed checksum and guardian authentication and are renamed
    /// over the published manifest. A crash may leave an arbitrary prefix in
    /// this bounded slot. Only a private, single-link regular inode owned by
    /// the surrounding capability directory is safe to retain and rewrite.
    fn manifest_stage_is_recoverably_incomplete(
        path: &std::path::Path,
    ) -> anyhow::Result<bool> {
        let path_metadata = match std::fs::symlink_metadata(path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(error.into()),
        };
        anyhow::ensure!(
            path_metadata.file_type().is_file()
                && path_metadata.len() <= LIVE_SCROLLBACK_MANIFEST_MAX_BYTES,
            "incomplete manifest stage is not a bounded regular file"
        );
        let mut options = std::fs::OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::{MetadataExt as _, OpenOptionsExt as _, PermissionsExt as _};

            let parent = path
                .parent()
                .ok_or_else(|| anyhow::anyhow!("manifest stage has no parent"))?;
            let parent_metadata = std::fs::symlink_metadata(parent)?;
            anyhow::ensure!(
                path_metadata.permissions().mode() & 0o077 == 0
                    && path_metadata.nlink() == 1
                    && path_metadata.uid() == parent_metadata.uid(),
                "incomplete manifest stage is not private"
            );
            options.custom_flags(libc::O_NOFOLLOW);
        }
        let file = options.open(path)?;
        let handle_metadata = file.metadata()?;
        anyhow::ensure!(
            handle_metadata.is_file()
                && handle_metadata.len() <= LIVE_SCROLLBACK_MANIFEST_MAX_BYTES,
            "manifest stage handle is not a bounded file"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt as _;

            anyhow::ensure!(
                handle_metadata.dev() == path_metadata.dev()
                    && handle_metadata.ino() == path_metadata.ino(),
                "incomplete manifest stage changed identity"
            );
        }
        Ok(true)
    }

    fn rewrite_incomplete_manifest_stage(
        path: &std::path::Path,
        bytes: &[u8],
    ) -> anyhow::Result<()> {
        anyhow::ensure!(
            u64::try_from(bytes.len()).unwrap_or(u64::MAX)
                <= LIVE_SCROLLBACK_MANIFEST_MAX_BYTES,
            "manifest stage serialization exceeds its byte ceiling"
        );
        anyhow::ensure!(
            Self::manifest_stage_is_recoverably_incomplete(path)?,
            "manifest stage disappeared before deterministic rewrite"
        );
        let expected = std::fs::symlink_metadata(path)?;
        let mut options = std::fs::OpenOptions::new();
        options.read(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;

            options.custom_flags(libc::O_NOFOLLOW);
        }
        let mut file = options.open(path)?;
        let observed = file.metadata()?;
        anyhow::ensure!(
            expected.file_type().is_file()
                && expected.len() <= LIVE_SCROLLBACK_MANIFEST_MAX_BYTES
                && observed.is_file()
                && observed.len() <= LIVE_SCROLLBACK_MANIFEST_MAX_BYTES,
            "manifest stage handle is not a bounded file"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};

            let parent = path
                .parent()
                .ok_or_else(|| anyhow::anyhow!("manifest stage has no parent"))?;
            let parent_metadata = std::fs::symlink_metadata(parent)?;
            anyhow::ensure!(
                expected.file_type().is_file()
                    && expected.permissions().mode() & 0o077 == 0
                    && expected.nlink() == 1
                    && expected.uid() == parent_metadata.uid()
                    && observed.dev() == expected.dev()
                    && observed.ino() == expected.ino(),
                "manifest stage changed identity before deterministic rewrite"
            );
            let path_metadata = std::fs::symlink_metadata(path)?;
            anyhow::ensure!(
                path_metadata.file_type().is_file()
                    && path_metadata.dev() == observed.dev()
                    && path_metadata.ino() == observed.ino(),
                "manifest stage changed path before deterministic rewrite"
            );
        }
        file.set_len(0)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        Ok(())
    }

    fn sync_private_manifest_stage(path: &std::path::Path) -> anyhow::Result<()> {
        anyhow::ensure!(
            Self::manifest_stage_is_recoverably_incomplete(path)?,
            "manifest stage disappeared before synchronization"
        );
        let expected = std::fs::symlink_metadata(path)?;
        let mut options = std::fs::OpenOptions::new();
        options.read(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;

            options.custom_flags(libc::O_NOFOLLOW);
        }
        let file = options.open(path)?;
        let observed = file.metadata()?;
        anyhow::ensure!(
            expected.file_type().is_file()
                && expected.len() <= LIVE_SCROLLBACK_MANIFEST_MAX_BYTES
                && observed.is_file()
                && observed.len() <= LIVE_SCROLLBACK_MANIFEST_MAX_BYTES,
            "manifest stage handle is not a bounded file"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt as _;

            anyhow::ensure!(
                observed.dev() == expected.dev() && observed.ino() == expected.ino(),
                "manifest stage changed identity before synchronization"
            );
        }
        file.sync_all()?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt as _;

            let path_metadata = std::fs::symlink_metadata(path)?;
            anyhow::ensure!(
                path_metadata.file_type().is_file()
                    && path_metadata.dev() == observed.dev()
                    && path_metadata.ino() == observed.ino(),
                "manifest stage changed path during synchronization"
            );
        }
        Ok(())
    }

    fn deterministic_manifest_stage_path(
        manifest_path: &std::path::Path,
    ) -> anyhow::Result<PathBuf> {
        let parent = manifest_path
            .parent()
            .ok_or_else(|| anyhow::anyhow!("scrollback manifest path has no parent"))?;
        Ok(parent.join("manifest.json.installing-v3"))
    }

    fn persist_manifest(
        &self,
        publication_state: &'static str,
    ) -> Result<(), LiveScrollbackManifestPublishError> {
        let mut published = false;
        let result = (|| -> anyhow::Result<()> {
        anyhow::ensure!(
            matches!(publication_state, "prepared" | "complete" | "cleared"),
            "invalid scrollback manifest publication state"
        );
        let state = self
            .lock_state("persist_manifest state")
            .map_err(anyhow::Error::new)?;
        let initial_stable_row = state.initial_stable_row;
        let newest_stable_row_exclusive = state.newest_stable_row_exclusive;
        let max_retained_rows = state.max_retained_rows;
        let content_epoch = state.content_epoch;
        let revision = state.revision;
        let authenticated_manifest = state.authenticated_manifest;
        let predecessor_generation = state.predecessor_generation;
        let verified_ledger = state.verified_ledger;
        drop(state);
        let ledger_pane_id = self.active_ledger_pane_id();

        let (
            oldest_seq,
            retained_rows,
            next_seq,
            retained_record_bytes,
            committed_log_bytes,
            committed_sequence_bytes,
        ) = if publication_state == "cleared" {
            // The committed clear manifest describes the logical generation,
            // not residual bytes awaiting best-effort physical reclamation.
            // Avoid consulting the old store so a poisoned store cannot make
            // already-cleared content reachable through manifest metadata.
            (None, 0, 0, 0, 0, 0)
        } else {
            let store = self
                .lock_store("persist_manifest store")
                .map_err(anyhow::Error::new)?;
            let retained_rows = u64::try_from(store.line_count(ledger_pane_id))
                .map_err(|_| anyhow::anyhow!("scrollback row count exceeds u64"))?;
            let ledger = (
                store.oldest_seq(ledger_pane_id),
                retained_rows,
                store.next_seq(ledger_pane_id)?,
                store.retained_record_bytes(ledger_pane_id),
                store.file_bytes(ledger_pane_id),
                store.sequence_file_bytes(ledger_pane_id)?,
            );
            drop(store);
            ledger
        };
        if !authenticated_manifest && ledger_pane_id != 0 {
            anyhow::bail!("legacy scrollback manifest cannot name a versioned ledger");
        }
        let incremental_authority = if authenticated_manifest {
            let authority = verified_ledger.ok_or_else(|| {
                anyhow::anyhow!("authenticated manifest has no scan-minted ledger authority")
            })?;
            anyhow::ensure!(
                authority.ledger_pane_id == ledger_pane_id
                    && authority.oldest_sequence == oldest_seq
                    && authority.next_sequence == next_seq
                    && authority.record_count == retained_rows
                    && authority.retained_record_bytes == retained_record_bytes,
                "incremental authority disagrees with manifest ledger facts"
            );
            Some(authority)
        } else {
            None
        };
        let predecessor_content_epoch = predecessor_generation
            .map(|generation| hex::encode(generation.content_epoch()));
        let predecessor_revision = predecessor_generation.map(|generation| generation.revision());
        let mut manifest = LiveScrollbackManifestV1 {
            schema: if authenticated_manifest {
                LIVE_SCROLLBACK_MANIFEST_SCHEMA_V4
            } else {
                LIVE_SCROLLBACK_MANIFEST_SCHEMA_V2
            }
            .to_string(),
            publication_state: publication_state.to_string(),
            durable_pane_id: uuid::Uuid::from_bytes(self.durable_pane_id)
                .simple()
                .to_string(),
            source_pane_id: u64::try_from(self.source_pane_id)
                .map_err(|_| anyhow::anyhow!("source pane id exceeds u64"))?,
            source_domain_id: u64::try_from(self.source_domain_id)
                .map_err(|_| anyhow::anyhow!("source domain id exceeds u64"))?,
            command_description: self.command_description.clone(),
            initial_stable_row,
            newest_stable_row_exclusive: if authenticated_manifest {
                newest_stable_row_exclusive
            } else {
                None
            },
            max_retained_rows: u64::try_from(max_retained_rows)
                .map_err(|_| anyhow::anyhow!("scrollback retention exceeds u64"))?,
            oldest_seq,
            retained_rows,
            next_seq,
            content_log: format!("{ledger_pane_id}.log"),
            content_sequence: authenticated_manifest
                .then(|| format!("{ledger_pane_id}.seq")),
            content_epoch: Some(hex::encode(content_epoch)),
            revision: Some(revision),
            predecessor_content_epoch: if authenticated_manifest {
                predecessor_content_epoch
            } else {
                None
            },
            predecessor_revision: if authenticated_manifest {
                predecessor_revision
            } else {
                None
            },
            retained_record_bytes: authenticated_manifest.then_some(retained_record_bytes),
            committed_log_bytes: authenticated_manifest.then_some(committed_log_bytes),
            committed_sequence_bytes: authenticated_manifest
                .then_some(committed_sequence_bytes),
            logical_ledger_sha256: None,
            chain_anchor_sha256: incremental_authority
                .map(|authority| hex::encode(authority.chain_anchor)),
            chain_tail_sha256: incremental_authority
                .map(|authority| hex::encode(authority.chain_tail)),
            guardian_manifest_authentication: None,
            manifest_sha256: String::new(),
        };

        if authenticated_manifest {
            expected_live_scrollback_v4_chain(&manifest)?;
            let canonical = Self::manifest_authentication_bytes(&manifest)?;
            let mut keyring = self
                .lock_keyring("persist_manifest authentication")
                .map_err(anyhow::Error::new)?;
            let cipher = keyring
                .latest_active_cipher()
                .context("load guardian manifest-authentication key")?;
            manifest.guardian_manifest_authentication = Some(
                cipher
                    .authenticate_scrollback_manifest(&canonical)
                    .context("authenticate scrollback manifest generation and ledger pointer")?
                    .encode(),
            );
        }

        manifest.manifest_sha256 = Self::manifest_checksum(&manifest)?;

        let mut bytes = serde_json::to_vec_pretty(&manifest)?;
        bytes.push(b'\n');
        anyhow::ensure!(
            u64::try_from(bytes.len()).unwrap_or(u64::MAX)
                <= LIVE_SCROLLBACK_MANIFEST_MAX_BYTES,
            "scrollback manifest serialization exceeds its byte ceiling"
        );
        let parent = self
            .manifest_path
            .parent()
            .ok_or_else(|| anyhow::anyhow!("scrollback manifest path has no parent"))?;
        // One deterministic stage slot bounds pre-publication failures. A
        // retry reuses the exact authenticated target; a different target
        // fails closed behind the retained diagnostic rather than allocating
        // an unbounded series of `installing-*` files.
        let temp_path = Self::deterministic_manifest_stage_path(&self.manifest_path)?;
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
        }
        match options.open(&temp_path) {
            Ok(mut file) => {
                file.write_all(&bytes)
                    .and_then(|()| file.sync_all())
                    .with_context(|| {
                        format!("persist scrollback manifest stage {}", temp_path.display())
                    })?;
                drop(file);
                #[cfg(not(windows))]
                std::fs::File::open(parent)?.sync_all()?;
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                let staged = match Self::read_manifest(&temp_path) {
                    Ok(Some(staged)) => staged,
                    Ok(None) => anyhow::bail!(
                        "deterministic scrollback manifest stage disappeared"
                    ),
                    Err(read_error)
                        if Self::manifest_stage_is_recoverably_incomplete(&temp_path)? =>
                    {
                        Self::rewrite_incomplete_manifest_stage(&temp_path, &bytes).with_context(
                            || {
                                format!(
                                    "rewrite incomplete scrollback manifest stage {} after {read_error}",
                                    temp_path.display()
                                )
                            },
                        )?;
                        Self::read_manifest(&temp_path)?.ok_or_else(|| {
                            anyhow::anyhow!(
                                "rewritten deterministic scrollback manifest stage disappeared"
                            )
                        })?
                    }
                    Err(read_error) => return Err(read_error),
                };
                anyhow::ensure!(
                    Self::manifest_authentication_bytes(&staged)?
                        == Self::manifest_authentication_bytes(&manifest)?,
                    "deterministic scrollback manifest stage belongs to a different transaction"
                );
                if live_scrollback_manifest_is_authenticated(&staged) {
                    let keyring = self
                        .lock_keyring("persist_manifest staged authentication")
                        .map_err(anyhow::Error::new)?;
                    anyhow::ensure!(
                        Self::authenticate_manifest(&staged, &keyring)?,
                        "deterministic authenticated scrollback manifest stage did not authenticate"
                    );
                }
                // The existing stage may carry a different randomized AEAD
                // nonce/tag for the same canonical transaction. Publish and
                // acknowledge those exact already-synchronized bytes rather
                // than comparing the renamed stage against the newly sealed
                // equivalent constructed for this retry.
                manifest = staged;
                Self::sync_private_manifest_stage(&temp_path)?;
                #[cfg(not(windows))]
                std::fs::File::open(parent)?.sync_all()?;
            }
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("create scrollback manifest stage {}", temp_path.display())
                });
            }
        }
        std::fs::rename(&temp_path, &self.manifest_path).with_context(|| {
            format!(
                "publish scrollback manifest {}",
                self.manifest_path.display()
            )
        })?;
        published = true;
        #[cfg(not(windows))]
        {
            let directory = std::fs::File::open(parent).with_context(|| {
                format!("open scrollback manifest directory {}", parent.display())
            })?;
            directory.sync_all().with_context(|| {
                format!(
                    "synchronize scrollback manifest directory {}",
                    parent.display()
                )
            })?;
        }
        let published_manifest = Self::read_manifest(&self.manifest_path)?
            .ok_or_else(|| anyhow::anyhow!("published scrollback manifest disappeared"))?;
        anyhow::ensure!(
            published_manifest == manifest,
            "published scrollback manifest changed during acknowledgement"
        );
        if authenticated_manifest {
            let keyring = self
                .lock_keyring("persist_manifest published authentication")
                .map_err(anyhow::Error::new)?;
            anyhow::ensure!(
                Self::authenticate_manifest(&published_manifest, &keyring)?,
                "published v4 scrollback manifest lost guardian authority"
            );
            drop(keyring);
            if publication_state != "cleared" {
                let store = self
                    .lock_store("persist_manifest published logical ledger")
                    .map_err(anyhow::Error::new)?;
                let authority = incremental_authority.ok_or_else(|| {
                    anyhow::anyhow!("published v4 manifest lost incremental authority")
                })?;
                anyhow::ensure!(
                    authority.matches_store_facts(&store)?
                        && expected_live_scrollback_v4_chain(&published_manifest)?
                            == (authority.chain_anchor, authority.chain_tail),
                    "published v4 manifest disagrees with incremental ledger authority"
                );
            }
        }
        Ok(())
        })();

        result.map_err(|source| {
            if published {
                LiveScrollbackManifestPublishError::after_publication(source)
            } else {
                LiveScrollbackManifestPublishError::before_publication(source)
            }
        })
    }

    #[cfg(test)]
    fn physical_scrollback_bytes(&self) -> u64 {
        let ledger_pane_id = self.active_ledger_pane_id();
        self.lock_store("physical_scrollback_bytes")
            .map(|store| store.file_bytes(ledger_pane_id))
            .unwrap_or(0)
    }

    fn lock_mutation_gate(
        &self,
        context: &str,
    ) -> Result<MutexGuard<'_, ()>, wezterm_term::config::ScrollbackSpillError> {
        match self.mutation_gate.lock() {
            Ok(gate) => Ok(gate),
            Err(_) => {
                log::error!("live scrollback mutation gate poisoned during {context}");
                Err(wezterm_term::config::ScrollbackSpillError::StorageUnavailable)
            }
        }
    }

    fn lock_filesystem_mutation(
        &self,
        context: &str,
    ) -> Result<LiveScrollbackFilesystemMutationLease, wezterm_term::config::ScrollbackSpillError>
    {
        let Some(pane_dir) = self.manifest_path.parent() else {
            log::error!("live scrollback manifest has no parent during {context}");
            return Err(wezterm_term::config::ScrollbackSpillError::StorageUnavailable);
        };
        acquire_live_scrollback_filesystem_mutation_lease(pane_dir, false).map_err(|error| {
            log::error!("live scrollback filesystem mutation lease failed during {context}: {error}");
            wezterm_term::config::ScrollbackSpillError::StorageUnavailable
        })
    }

    fn authenticated_manifest_for_snapshot(
        &self,
        state: LiveScrollbackSpillState,
        ledger_pane_id: u64,
    ) -> Result<Option<LiveScrollbackManifestV1>, wezterm_term::config::ScrollbackSpillError> {
        use wezterm_term::config::ScrollbackSpillError;

        if !state.authenticated_manifest {
            return Ok(None);
        }
        let manifest = Self::read_manifest(&self.manifest_path)
            .map_err(|_| ScrollbackSpillError::StorageUnavailable)?;
        let Some(manifest) = manifest else {
            let pristine = state.revision == 0
                && state.predecessor_generation.is_none()
                && state.initial_stable_row.is_none()
                && state.newest_stable_row_exclusive.is_none()
                && !state.clear_manifest_published
                && !state.clear_pending_physical_reclamation;
            return if pristine {
                Ok(None)
            } else {
                Err(ScrollbackSpillError::StorageUnavailable)
            };
        };
        let durable_pane_id = uuid::Uuid::from_bytes(self.durable_pane_id)
            .simple()
            .to_string();
        validate_live_scrollback_manifest_identity(
            &manifest,
            &durable_pane_id,
            &self.manifest_path,
        )
        .map_err(|_| ScrollbackSpillError::StorageUnavailable)?;
        {
            let keyring = self.lock_keyring("snapshot_scrollback manifest authentication")?;
            if !Self::authenticate_manifest(&manifest, &keyring)
                .map_err(|_| ScrollbackSpillError::StorageUnavailable)?
            {
                return Err(ScrollbackSpillError::StorageUnavailable);
            }
        }
        let generation = live_scrollback_manifest_generation(&manifest)
            .map_err(|_| ScrollbackSpillError::StorageUnavailable)?
            .map(|(epoch, revision)| {
                wezterm_term::config::ScrollbackSnapshotGeneration::new(epoch, revision)
            });
        if generation != Some(state.snapshot_generation())
            || live_scrollback_manifest_predecessor(&manifest)
                .map_err(|_| ScrollbackSpillError::StorageUnavailable)?
                != state.predecessor_generation
            || Self::manifest_ledger_pane_id(&manifest)
                .map_err(|_| ScrollbackSpillError::StorageUnavailable)?
                != ledger_pane_id
            || (state.clear_manifest_published
                && manifest.publication_state != "cleared")
            || (!state.clear_manifest_published
                && manifest.publication_state != "complete")
        {
            return Err(ScrollbackSpillError::StorageUnavailable);
        }
        Ok(Some(manifest))
    }

    fn revalidate_snapshot_manifest(
        &self,
        before: &LiveScrollbackManifestV1,
    ) -> Result<(), wezterm_term::config::ScrollbackSpillError> {
        use wezterm_term::config::ScrollbackSpillError;

        let after = Self::read_manifest(&self.manifest_path)
            .map_err(|_| ScrollbackSpillError::StorageUnavailable)?
            .ok_or(ScrollbackSpillError::StorageUnavailable)?;
        if &after != before {
            return Err(ScrollbackSpillError::StorageUnavailable);
        }
        let keyring = self.lock_keyring("snapshot_scrollback manifest reauthentication")?;
        if !Self::authenticate_manifest(&after, &keyring)
            .map_err(|_| ScrollbackSpillError::StorageUnavailable)?
        {
            return Err(ScrollbackSpillError::StorageUnavailable);
        }
        Ok(())
    }

    fn verify_current_published_state_before_mutation(
        &self,
        state: LiveScrollbackSpillState,
        ledger_pane_id: u64,
        allow_prepared_content_ahead: bool,
    ) -> Result<(), wezterm_term::config::ScrollbackSpillError> {
        use wezterm_term::config::ScrollbackSpillError;

        if !state.authenticated_manifest {
            return Ok(());
        }
        let manifest = Self::read_manifest(&self.manifest_path)
            .map_err(|_| ScrollbackSpillError::StorageUnavailable)?;
        let Some(manifest) = manifest else {
            let pristine = state.revision == 0
                && state.predecessor_generation.is_none()
                && state.initial_stable_row.is_none()
                && state.newest_stable_row_exclusive.is_none()
                && !state.clear_manifest_published
                && !state.clear_pending_physical_reclamation;
            return if pristine {
                Ok(())
            } else {
                Err(ScrollbackSpillError::SnapshotGenerationMismatch)
            };
        };
        let durable_pane_id = uuid::Uuid::from_bytes(self.durable_pane_id)
            .simple()
            .to_string();
        validate_live_scrollback_manifest_identity(
            &manifest,
            &durable_pane_id,
            &self.manifest_path,
        )
        .map_err(|_| ScrollbackSpillError::StorageUnavailable)?;
        {
            let keyring = self.lock_keyring("pre-mutation manifest authentication")?;
            if !Self::authenticate_manifest(&manifest, &keyring)
                .map_err(|_| ScrollbackSpillError::StorageUnavailable)?
            {
                return Err(ScrollbackSpillError::StorageUnavailable);
            }
        }
        let generation = live_scrollback_manifest_generation(&manifest)
            .map_err(|_| ScrollbackSpillError::StorageUnavailable)?
            .map(|(epoch, revision)| {
                wezterm_term::config::ScrollbackSnapshotGeneration::new(epoch, revision)
            });
        if generation != Some(state.snapshot_generation())
            || live_scrollback_manifest_predecessor(&manifest)
                .map_err(|_| ScrollbackSpillError::StorageUnavailable)?
                != state.predecessor_generation
            || Self::manifest_ledger_pane_id(&manifest)
                .map_err(|_| ScrollbackSpillError::StorageUnavailable)?
                != ledger_pane_id
        {
            return Err(ScrollbackSpillError::SnapshotGenerationMismatch);
        }
        let publication_matches = if state.clear_manifest_published {
            manifest.publication_state == "cleared"
        } else {
            manifest.publication_state == "complete"
                || (allow_prepared_content_ahead
                    && manifest.publication_state == "prepared")
        };
        if !publication_matches {
            return Err(ScrollbackSpillError::StorageUnavailable);
        }
        let store = self.lock_store("pre-mutation logical ledger")?;
        let digest_verified = if manifest.schema == LIVE_SCROLLBACK_MANIFEST_SCHEMA_V4 {
            state.verified_ledger.is_some_and(|authority| {
                let chain = expected_live_scrollback_v4_chain(&manifest).ok();
                let manifest_facts_match = authority.ledger_pane_id == ledger_pane_id
                    && authority.oldest_sequence == manifest.oldest_seq
                    && authority.next_sequence == manifest.next_seq
                    && authority.record_count == manifest.retained_rows
                    && Some(authority.retained_record_bytes) == manifest.retained_record_bytes
                    && chain == Some((authority.chain_anchor, authority.chain_tail));
                if manifest.publication_state == "cleared" {
                    manifest_facts_match && authority == VerifiedLedgerState::empty(ledger_pane_id)
                } else {
                    manifest_facts_match
                        && authority.matches_store_facts(&store).unwrap_or(false)
                        && manifest.committed_log_bytes == Some(store.file_bytes(ledger_pane_id))
                        && manifest.committed_sequence_bytes
                            == store.sequence_file_bytes(ledger_pane_id).ok()
                }
            })
        } else {
            Self::verify_logical_ledger_digest_from_store(&manifest, ledger_pane_id, &store).is_ok()
        };
        if !digest_verified {
            if !(allow_prepared_content_ahead && manifest.publication_state == "prepared") {
                return Err(ScrollbackSpillError::StorageUnavailable);
            }
            let actual_rows = u64::try_from(store.line_count(ledger_pane_id))
                .map_err(|_| ScrollbackSpillError::ArithmeticOverflow("row_count"))?;
            let actual_next_sequence = store
                .next_seq(ledger_pane_id)
                .map_err(|_| ScrollbackSpillError::StorageUnavailable)?;
            let narrowly_prepared_content_ahead = actual_rows > manifest.retained_rows
                && actual_next_sequence > manifest.next_seq
                && store.file_bytes(ledger_pane_id)
                    > manifest.committed_log_bytes.unwrap_or(0);
            if !narrowly_prepared_content_ahead {
                return Err(ScrollbackSpillError::StorageUnavailable);
            }
            if store.line_count(ledger_pane_id) != 0 {
                let initial_stable_row = state
                    .initial_stable_row
                    .ok_or(ScrollbackSpillError::SnapshotRangeMismatch)?;
                let keyring = self.lock_keyring("pre-mutation prepared-ledger validation")?;
                Self::validate_persisted_records(
                    &store,
                    ledger_pane_id,
                    &keyring,
                    self.durable_pane_id,
                    state.content_epoch,
                    initial_stable_row,
                    true,
                )
                .map_err(|_| ScrollbackSpillError::StorageUnavailable)?;
            }
        }
        drop(store);
        self.revalidate_snapshot_manifest(&manifest)
    }

    fn lock_state(
        &self,
        context: &str,
    ) -> Result<
        MutexGuard<'_, LiveScrollbackSpillState>,
        wezterm_term::config::ScrollbackSpillError,
    > {
        match self.state.lock() {
            Ok(state) => Ok(state),
            Err(_) => {
                log::error!("live scrollback spill state mutex poisoned during {context}");
                Err(wezterm_term::config::ScrollbackSpillError::StorageUnavailable)
            }
        }
    }

    fn lock_store(
        &self,
        context: &str,
    ) -> Result<
        MutexGuard<'_, frankenterm_core::storage::mmap_store::MmapScrollbackStore>,
        wezterm_term::config::ScrollbackSpillError,
    > {
        match self.store.lock() {
            Ok(store) => Ok(store),
            Err(_) => {
                log::error!("live scrollback spill store mutex poisoned during {context}");
                Err(wezterm_term::config::ScrollbackSpillError::StorageUnavailable)
            }
        }
    }

    fn lock_keyring(
        &self,
        context: &str,
    ) -> Result<
        MutexGuard<'_, guardian_output_keys::GuardianOutputKeyring>,
        wezterm_term::config::ScrollbackSpillError,
    > {
        match self.keyring.lock() {
            Ok(keyring) => Ok(keyring),
            Err(_) => {
                log::error!("guardian output keyring mutex poisoned during {context}");
                Err(wezterm_term::config::ScrollbackSpillError::StorageUnavailable)
            }
        }
    }
}

#[cfg(test)]
fn first_visible_cell_attrs(line: &wezterm_term::Line) -> termwiz::cell::CellAttributes {
    line.visible_cells()
        .next()
        .map(|cell| cell.attrs().clone())
        .unwrap_or_else(termwiz::cell::CellAttributes::blank)
}

#[cfg(test)]
fn prepare_scrollback_line_record(
    line: &wezterm_term::Line,
    redacted_text: &str,
) -> wezterm_term::Line {
    let original_text = line.as_str();
    let mut record_line = if redacted_text == original_text.as_ref() {
        line.clone()
    } else {
        let attrs = first_visible_cell_attrs(line);
        let mut sanitized =
            wezterm_term::Line::from_text(redacted_text, &attrs, line.current_seqno(), None);
        if line.last_cell_was_wrapped() {
            sanitized.set_last_cell_was_wrapped(true, line.current_seqno());
        }
        sanitized
    };
    record_line.compress_for_scrollback();
    record_line
}

#[cfg(test)]
fn encode_scrollback_line_record(
    line: &wezterm_term::Line,
    redactor: &frankenterm_core::redactor::Redactor,
) -> Option<String> {
    let original_text = line.as_str();
    let redacted_text = redactor.redact(original_text.as_ref());
    let record_line = prepare_scrollback_line_record(line, &redacted_text);
    let uncompressed = varbincode::serialize(&record_line).ok()?;
    if u64::try_from(uncompressed.len()).ok()? > LIVE_SCROLLBACK_MAX_DECODED_LINE_BYTES {
        return None;
    }

    let (prefix, payload) = if uncompressed.len() >= LIVE_SCROLLBACK_LINE_COMPRESS_MIN_BYTES {
        match zstd::stream::encode_all(&uncompressed[..], zstd::DEFAULT_COMPRESSION_LEVEL) {
            Ok(compressed) if compressed.len() < uncompressed.len() => {
                (LIVE_SCROLLBACK_LINE_RECORD_V2_ZSTD, compressed)
            }
            _ => (LIVE_SCROLLBACK_LINE_RECORD_V2_UNCOMPRESSED, uncompressed),
        }
    } else {
        (LIVE_SCROLLBACK_LINE_RECORD_V2_UNCOMPRESSED, uncompressed)
    };
    let payload_sha256 = {
        use sha2::{Digest as _, Sha256};

        hex::encode(Sha256::digest(&payload))
    };

    Some(format!(
        "{prefix}{payload_sha256}:{}",
        base64::engine::general_purpose::STANDARD_NO_PAD.encode(payload)
    ))
}

fn encode_exact_scrollback_line_record(
    line: &wezterm_term::Line,
    cipher: &mux::guardian_output_journal::GuardianOutputCipher,
    identity: mux::guardian_output_journal::GuardianScrollbackRowIdentity,
) -> Option<String> {
    let plaintext = serialize_exact_semantic_scrollback_line(line)?;
    cipher
        .seal_scrollback_row(identity, &plaintext)
        .ok()?
        .encode()
        .ok()
}

fn serialize_exact_semantic_scrollback_line(
    line: &wezterm_term::Line,
) -> Option<Zeroizing<Vec<u8>>> {
    let mut semantic_line = line.clone();
    let cells = semantic_line.cells_mut();
    if cells.len() > LIVE_SCROLLBACK_MAX_DECODED_LINE_BYTES_USIZE {
        return None;
    }
    let mut cell_widths = Vec::new();
    cell_widths.try_reserve_exact(cells.len()).ok()?;
    for cell in cells {
        let width = u8::try_from(cell.width()).ok()?;
        if !matches!(width, 1 | 2) {
            return None;
        }
        cell_widths.push(width);
    }
    let semantic = ExactSemanticScrollbackLineV1 {
        schema: 1,
        line: semantic_line,
        cell_widths,
    };
    let mut plaintext = BoundedScrollbackPlaintext::new(
        LIVE_SCROLLBACK_MAX_DECODED_LINE_BYTES_USIZE,
    );
    let serialization = {
        let mut serializer = varbincode::Serializer::new(&mut plaintext);
        semantic.serialize(&mut serializer)
    };
    if plaintext.exceeded || serialization.is_err() || plaintext.bytes.is_empty() {
        return None;
    }
    Some(plaintext.bytes)
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ExactSemanticScrollbackLineV1 {
    schema: u32,
    line: wezterm_term::Line,
    cell_widths: Vec<u8>,
}

struct BoundedScrollbackPlaintext {
    bytes: Zeroizing<Vec<u8>>,
    max_bytes: usize,
    exceeded: bool,
}

impl BoundedScrollbackPlaintext {
    fn new(max_bytes: usize) -> Self {
        Self {
            bytes: Zeroizing::new(Vec::new()),
            max_bytes,
            exceeded: false,
        }
    }
}

impl Write for BoundedScrollbackPlaintext {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        let Some(next_len) = self.bytes.len().checked_add(buffer.len()) else {
            self.exceeded = true;
            return Err(std::io::Error::other(
                "semantic scrollback serialization length overflow",
            ));
        };
        if next_len > self.max_bytes {
            self.exceeded = true;
            return Err(std::io::Error::other(
                "semantic scrollback serialization exceeds its hard limit",
            ));
        }
        self.bytes
            .try_reserve(buffer.len())
            .map_err(std::io::Error::other)?;
        self.bytes.extend_from_slice(buffer);
        Ok(buffer.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn decode_scrollback_line_record_with_limit(
    record: &str,
    max_decoded_bytes: u64,
) -> Option<(wezterm_term::Line, usize)> {
    let max_decoded_bytes = max_decoded_bytes.min(LIVE_SCROLLBACK_MAX_DECODED_LINE_BYTES);
    let (compressed, expected_sha256, encoded) = if let Some(record) =
        record.strip_prefix(LIVE_SCROLLBACK_LINE_RECORD_V2_UNCOMPRESSED)
    {
        let (expected_sha256, encoded) = record.split_once(':')?;
        (false, Some(expected_sha256), encoded)
    } else if let Some(record) = record.strip_prefix(LIVE_SCROLLBACK_LINE_RECORD_V2_ZSTD) {
        let (expected_sha256, encoded) = record.split_once(':')?;
        (true, Some(expected_sha256), encoded)
    } else if let Some(encoded) =
        record.strip_prefix(LIVE_SCROLLBACK_LINE_RECORD_V1_UNCOMPRESSED)
    {
        (false, None, encoded)
    } else {
        let encoded = record.strip_prefix(LIVE_SCROLLBACK_LINE_RECORD_V1_ZSTD)?;
        (true, None, encoded)
    };

    let hard_max_encoded_bytes = LIVE_SCROLLBACK_MAX_DECODED_LINE_BYTES
        .checked_add(2)?
        .checked_div(3)?
        .checked_mul(4)?;
    if u64::try_from(encoded.len()).ok()? > hard_max_encoded_bytes {
        return None;
    }

    // Uncompressed base64 expands by at most four bytes for each three input
    // bytes. Reject an impossible-to-fit record before the decoder allocates.
    if !compressed {
        let max_encoded_bytes = max_decoded_bytes
            .checked_add(2)?
            .checked_div(3)?
            .checked_mul(4)?;
        if u64::try_from(encoded.len()).ok()? > max_encoded_bytes {
            return None;
        }
    }

    let payload = base64::engine::general_purpose::STANDARD_NO_PAD
        .decode(encoded)
        .ok()?;
    if !compressed && u64::try_from(payload.len()).ok()? > max_decoded_bytes {
        return None;
    }
    if u64::try_from(payload.len()).ok()? > LIVE_SCROLLBACK_MAX_DECODED_LINE_BYTES {
        return None;
    }
    if let Some(expected_sha256) = expected_sha256 {
        use sha2::{Digest as _, Sha256};

        let expected_sha256 = hex::decode(expected_sha256).ok()?;
        let actual_sha256 = Sha256::digest(&payload);
        // Bind the digest through an explicit `&[u8]` view: bare `.as_ref()`
        // is ambiguous here (E0283) because `bytes`/`asupersync` add extra
        // `PartialEq<_>` impls for `[u8]` to the candidate set.
        let actual_sha256: &[u8] = actual_sha256.as_ref();
        if expected_sha256.len() != 32 || expected_sha256.as_slice() != actual_sha256 {
            return None;
        }
    }
    let decoded_payload = if compressed {
        let decoder = zstd::Decoder::new(payload.as_slice()).ok()?;
        let mut decompressed = Vec::new();
        decoder
            .take(max_decoded_bytes.checked_add(1)?)
            .read_to_end(&mut decompressed)
            .ok()?;
        if u64::try_from(decompressed.len()).ok()? > max_decoded_bytes {
            return None;
        }
        decompressed
    } else {
        payload
    };

    // FND-013: decode untrusted scrollback through codec's BOUNDED varbincode
    // (caps container length/bytes + size_hint) instead of raw varbincode, so a
    // malicious length prefix in a Line cannot drive an unbounded preallocation.
    // The 16 MB input cap above bounds bytes; this bounds the decoded structure
    // independently of serde-version `size_hint::cautious` behavior. Wire-format
    // compatible with the `varbincode::serialize` on the encode side.
    let decoded_bytes = decoded_payload.len();
    let mut reader = decoded_payload.as_slice();
    let line = codec::bounded_varbincode_deserialize(&mut reader).ok()?;
    if !reader.is_empty() {
        return None;
    }
    Some((line, decoded_bytes))
}

fn decode_scrollback_line_record(record: &str) -> Option<wezterm_term::Line> {
    decode_scrollback_line_record_with_limit(record, LIVE_SCROLLBACK_MAX_DECODED_LINE_BYTES)
        .map(|(line, _decoded_bytes)| line)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DecodedScrollbackRecordFidelity {
    ExactSemantic,
    LegacyRedacted,
}

struct GuardianScrollbackCipherCache<'a> {
    keyring: &'a guardian_output_keys::GuardianOutputKeyring,
    ciphers: std::collections::HashMap<
        [u8; 8],
        mux::guardian_output_journal::GuardianOutputCipher,
    >,
}

impl<'a> GuardianScrollbackCipherCache<'a> {
    fn new(keyring: &'a guardian_output_keys::GuardianOutputKeyring) -> Self {
        Self {
            keyring,
            ciphers: std::collections::HashMap::new(),
        }
    }

    fn cipher_for_key_id(
        &mut self,
        key_id: [u8; 8],
    ) -> anyhow::Result<&mux::guardian_output_journal::GuardianOutputCipher> {
        if self.ciphers.contains_key(&key_id) {
            return Ok(self
                .ciphers
                .get(&key_id)
                .expect("cipher cache membership was just established"));
        }
        self.ciphers
            .try_reserve(1)
            .map_err(|_| anyhow::anyhow!("guardian scrollback cipher cache allocation failed"))?;
        let keyring = self.keyring;
        match self.ciphers.entry(key_id) {
            std::collections::hash_map::Entry::Occupied(entry) => Ok(entry.into_mut()),
            std::collections::hash_map::Entry::Vacant(entry) => {
                let cipher = keyring
                    .cipher_for_key_id(key_id)
                    .context("load guardian scrollback key")?;
                Ok(entry.insert(cipher))
            }
        }
    }
}

fn decode_persisted_scrollback_line_with_limit(
    record: &str,
    cipher_cache: &mut GuardianScrollbackCipherCache<'_>,
    durable_pane_id: [u8; 16],
    content_epoch: [u8; 16],
    stable_row: wezterm_term::StableRowIndex,
    sequence: u64,
    max_decoded_bytes: usize,
) -> anyhow::Result<(
    wezterm_term::Line,
    usize,
    DecodedScrollbackRecordFidelity,
)> {
    use mux::guardian_output_journal::GuardianEncryptedScrollbackRow;

    if GuardianEncryptedScrollbackRow::has_encrypted_prefix(record) {
        let parsed = GuardianEncryptedScrollbackRow::parse(record)
            .context("parse encrypted semantic scrollback row")?;
        let plaintext_bytes = usize::try_from(parsed.plaintext_bytes())
            .map_err(|_| anyhow::anyhow!("encrypted scrollback row size exceeds usize"))?;
        anyhow::ensure!(
            plaintext_bytes <= max_decoded_bytes,
            "encrypted scrollback row exceeds the remaining decoded-byte limit"
        );
        let stable_row = i64::try_from(stable_row)
            .map_err(|_| anyhow::anyhow!("stable row does not fit encrypted row identity"))?;
        let cipher = cipher_cache
            .cipher_for_key_id(parsed.key_id())
            .context("load historical guardian key for semantic scrollback row")?;
        let plaintext = cipher
            .open_scrollback_row(
                &parsed,
                durable_pane_id,
                content_epoch,
                stable_row,
                sequence,
                u32::try_from(max_decoded_bytes.min(LIVE_SCROLLBACK_MAX_DECODED_LINE_BYTES_USIZE))
                    .unwrap_or(u32::MAX),
            )
            .context("authenticate semantic scrollback row at durable location")?;
        let decoded_bytes = plaintext.len();
        let mut reader = plaintext.as_slice();
        let mut semantic: ExactSemanticScrollbackLineV1 =
            codec::bounded_varbincode_deserialize(&mut reader)
            .context("bounded decode of semantic scrollback row")?;
        anyhow::ensure!(
            reader.is_empty(),
            "semantic scrollback row contains trailing plaintext"
        );
        anyhow::ensure!(semantic.schema == 1, "unsupported semantic scrollback row schema");
        let cells = semantic.line.cells_mut();
        anyhow::ensure!(
            cells.len() == semantic.cell_widths.len(),
            "semantic scrollback cell-width sidecar length mismatch"
        );
        for (cell, width) in cells.iter_mut().zip(semantic.cell_widths) {
            anyhow::ensure!(matches!(width, 1 | 2), "invalid semantic scrollback cell width");
            let restored = termwiz::cell::Cell::new_grapheme_with_width(
                cell.str(),
                usize::from(width),
                cell.attrs().clone(),
            );
            *cell = restored;
        }
        return Ok((
            semantic.line,
            decoded_bytes,
            DecodedScrollbackRecordFidelity::ExactSemantic,
        ));
    }

    if record.starts_with("ftsl3") {
        anyhow::bail!("encrypted scrollback row has a corrupt reserved prefix");
    }
    if record.starts_with(LIVE_SCROLLBACK_LINE_RECORD_V1_UNCOMPRESSED)
        || record.starts_with(LIVE_SCROLLBACK_LINE_RECORD_V1_ZSTD)
        || record.starts_with(LIVE_SCROLLBACK_LINE_RECORD_V2_UNCOMPRESSED)
        || record.starts_with(LIVE_SCROLLBACK_LINE_RECORD_V2_ZSTD)
    {
        let (line, decoded_bytes) = decode_scrollback_line_record_with_limit(
            record,
            u64::try_from(max_decoded_bytes).unwrap_or(u64::MAX),
        )
        .ok_or_else(|| anyhow::anyhow!("legacy scrollback row failed bounded decoding"))?;
        return Ok((
            line,
            decoded_bytes,
            DecodedScrollbackRecordFidelity::LegacyRedacted,
        ));
    }
    if record.starts_with("ftsl") {
        anyhow::bail!("scrollback row has an unrecognized reserved record prefix");
    }
    anyhow::ensure!(
        record.len() <= max_decoded_bytes,
        "legacy text scrollback row exceeds the remaining decoded-byte limit"
    );
    Ok((
        legacy_text_scrollback_line(record),
        record.len(),
        DecodedScrollbackRecordFidelity::LegacyRedacted,
    ))
}

fn exact_scrollback_line_record_is_equivalent(
    existing: &str,
    line: &wezterm_term::Line,
    keyring: &guardian_output_keys::GuardianOutputKeyring,
    durable_pane_id: [u8; 16],
    content_epoch: [u8; 16],
    stable_row: wezterm_term::StableRowIndex,
    sequence: u64,
) -> bool {
    let Ok((decoded, _decoded_bytes, fidelity)) = decode_persisted_scrollback_line_with_limit(
        existing,
        &mut GuardianScrollbackCipherCache::new(keyring),
        durable_pane_id,
        content_epoch,
        stable_row,
        sequence,
        LIVE_SCROLLBACK_MAX_DECODED_LINE_BYTES_USIZE,
    ) else {
        return false;
    };
    if fidelity != DecodedScrollbackRecordFidelity::ExactSemantic {
        return false;
    }
    match (
        serialize_exact_semantic_scrollback_line(&decoded),
        serialize_exact_semantic_scrollback_line(line),
    ) {
        (Some(decoded), Some(expected)) => decoded == expected,
        _ => false,
    }
}

fn legacy_text_scrollback_line(text: &str) -> wezterm_term::Line {
    wezterm_term::Line::from_text(text, &termwiz::cell::CellAttributes::blank(), 0, None)
}

impl wezterm_term::config::ScrollbackSpillSink for LiveScrollbackSpillSink {
    fn store_scrollback_line(
        &self,
        stable_row: wezterm_term::StableRowIndex,
        line: &wezterm_term::Line,
        max_retained_rows: usize,
    ) -> bool {
        if max_retained_rows == 0 {
            return false;
        }

        let Ok(_mutation_gate) = self.lock_mutation_gate("store_scrollback_line") else {
            return false;
        };
        let Ok(_filesystem_mutation_lease) =
            self.lock_filesystem_mutation("store_scrollback_line")
        else {
            return false;
        };
        let ledger_pane_id = self.active_ledger_pane_id();
        let current_state = match self.lock_state("store_scrollback_line pending clear") {
            Ok(state) if state.transaction_quarantined => return false,
            Ok(state) => *state,
            Err(_) => return false,
        };
        // A checksum-only v1/v2 manifest cannot authorize appending newly
        // encrypted rows. Mixing an exact row into that unauthenticated
        // lineage would leave its location metadata mutable and could later
        // be mistaken for a recovery-grade generation. Existing legacy rows
        // remain readable/exportable; an explicit authenticated clear is the
        // only operation here that establishes a fresh exact lineage.
        if !current_state.authenticated_manifest {
            return false;
        }
        let stage_path = match Self::deterministic_manifest_stage_path(&self.manifest_path) {
            Ok(path) => path,
            Err(_) => return false,
        };
        match Self::read_manifest(&stage_path) {
            Ok(Some(stage)) if stage.publication_state == "complete" => {
                // Content can be synchronized before the final manifest
                // rename.  Reuse only the retained, authenticated complete
                // transaction that exactly matches this sink's current state;
                // `persist_manifest` performs that canonical comparison. A
                // replacement stage names a different ledger/state and is
                // therefore rejected rather than auto-promoted.
                if let Err(error) = self.persist_manifest("complete") {
                    if error.outcome_indeterminate() {
                        if let Ok(mut state) =
                            self.lock_state("store_scrollback_line staged retry quarantine")
                        {
                            state.transaction_quarantined = true;
                        }
                    }
                    return false;
                }
            }
            Ok(Some(stage)) if stage.publication_state == "prepared" => {
                // A first-row prepare that failed before publication is safe
                // to recreate from pristine revision zero. Any other staged
                // prepare is a conflicting retained transaction.
                if current_state.revision != 0
                    || current_state.initial_stable_row.is_some()
                    || current_state.newest_stable_row_exclusive.is_some()
                {
                    return false;
                }
            }
            Ok(Some(_)) => return false,
            Ok(None) => {}
            Err(error) => match Self::manifest_stage_is_recoverably_incomplete(&stage_path) {
                Ok(true) => log::warn!(
                    "retaining a securely pinned incomplete manifest stage for deterministic retry: {error}"
                ),
                Ok(false) | Err(_) => return false,
            },
        }
        if self
            .verify_current_published_state_before_mutation(
                current_state,
                ledger_pane_id,
                true,
            )
            .is_err()
        {
            return false;
        }
        if self.advance_authenticated_append_wal_supersession().is_err() {
            return false;
        }
        let clear_pending = current_state.clear_pending_physical_reclamation;
        if clear_pending {
            let Ok(mut store) = self.lock_store("store_scrollback_line reclaim committed clear")
            else {
                return false;
            };
            if store.clear_pane(ledger_pane_id).is_err() {
                return false;
            }
            drop(store);
            let Ok(mut state) = self.lock_state("store_scrollback_line complete deferred clear")
            else {
                return false;
            };
            state.clear_pending_physical_reclamation = false;
        }

        // Resolve exact retries before minting a successor generation. A row
        // that is already durable (or has already aged out) is an
        // acknowledgement of the existing generation, not a new mutation.
        let retry_state = match self.lock_state("store_scrollback_line retry preflight") {
            Ok(state) => *state,
            Err(_) => return false,
        };
        let retry_initial = retry_state.initial_stable_row.unwrap_or(stable_row);
        if stable_row < retry_initial {
            return false;
        }
        let Ok(retry_sequence) = u64::try_from(stable_row - retry_initial) else {
            return false;
        };
        {
            let Ok(store) = self.lock_store("store_scrollback_line retry preflight") else {
                return false;
            };
            let Ok(next_sequence) = store.next_seq(ledger_pane_id) else {
                return false;
            };
            if retry_sequence < next_sequence {
                match store.line_at(ledger_pane_id, retry_sequence) {
                    Ok(Some(existing)) => {
                        let Ok(keyring) =
                            self.lock_keyring("store_scrollback_line exact retry")
                        else {
                            return false;
                        };
                        return exact_scrollback_line_record_is_equivalent(
                            &existing,
                            line,
                            &keyring,
                            self.durable_pane_id,
                            retry_state.content_epoch,
                            stable_row,
                            retry_sequence,
                        );
                    }
                    Ok(None)
                        if store
                            .oldest_seq(ledger_pane_id)
                            .is_some_and(|oldest| retry_sequence < oldest) =>
                    {
                        return true;
                    }
                    _ => return false,
                }
            }
            if retry_sequence > next_sequence {
                return false;
            }
        }

        let (
            previous_state,
            proposed_state,
            manifest_prepare_required,
            desired_seq,
            row_identity,
        ) = {
            let Ok(state) = self.lock_state("store_scrollback_line initial row") else {
                return false;
            };
            let manifest_prepare_required = state.initial_stable_row.is_none();
            let initial = state.initial_stable_row.unwrap_or(stable_row);
            if stable_row < initial {
                return false;
            }
            let Ok(desired_seq) = u64::try_from(stable_row - initial) else {
                return false;
            };
            let previous_state = *state;
            let mut proposed_state = previous_state;
            if proposed_state.advance_revision().is_err() {
                return false;
            }
            proposed_state.clear_manifest_published = false;
            proposed_state.initial_stable_row = Some(initial);
            let Some(newest_stable_row_exclusive) = stable_row.checked_add(1) else {
                return false;
            };
            proposed_state.newest_stable_row_exclusive = Some(
                proposed_state
                    .newest_stable_row_exclusive
                    .map_or(newest_stable_row_exclusive, |current| {
                        current.max(newest_stable_row_exclusive)
                    }),
            );
            proposed_state.max_retained_rows = max_retained_rows;
            let Ok(stable_row_identity) = i64::try_from(stable_row) else {
                return false;
            };
            let Ok(row_identity) =
                mux::guardian_output_journal::GuardianScrollbackRowIdentity::new(
                    self.durable_pane_id,
                    proposed_state.content_epoch,
                    proposed_state.revision,
                    stable_row_identity,
                    desired_seq,
                )
            else {
                return false;
            };
            (
                previous_state,
                proposed_state,
                manifest_prepare_required,
                desired_seq,
                row_identity,
            )
        };
        let record = {
            let Ok(mut keyring) = self.lock_keyring("store_scrollback_line active key") else {
                return false;
            };
            let Ok(cipher) = keyring.latest_active_cipher() else {
                return false;
            };
            let Some(record) = encode_exact_scrollback_line_record(line, &cipher, row_identity)
            else {
                return false;
            };
            record
        };
        let (append_wal, target_authority) = if manifest_prepare_required {
            let target = {
                let Ok(store) = self.lock_store("store_scrollback_line prepare first authority")
                else {
                    return false;
                };
                let Some(predecessor) = previous_state.verified_ledger else {
                    return false;
                };
                match predecessor.project_append(
                    desired_seq,
                    &record,
                    max_retained_rows,
                    &store,
                ) {
                    Ok((target, 0)) => target,
                    _ => return false,
                }
            };
            (None, target)
        } else {
            let predecessor_manifest = match Self::read_manifest(&self.manifest_path) {
                Ok(Some(manifest)) => manifest,
                _ => return false,
            };
            let (wal, target) = {
                let Ok(store) = self.lock_store("store_scrollback_line prepare append WAL") else {
                    return false;
                };
                match self.prepare_authenticated_append_wal(
                    &predecessor_manifest,
                    previous_state,
                    proposed_state,
                    ledger_pane_id,
                    stable_row,
                    desired_seq,
                    max_retained_rows,
                    &record,
                    &store,
                ) {
                    Ok(prepared) => prepared,
                    Err(_) => return false,
                }
            };
            if let Err(error) = self.persist_authenticated_append_wal(&wal) {
                if error.outcome_indeterminate() {
                    if let Ok(mut state) =
                        self.lock_state("store_scrollback_line append WAL quarantine")
                    {
                        state.transaction_quarantined = true;
                    }
                }
                return false;
            }
            (Some(wal), target)
        };
        let Ok(mut state) = self.lock_state("store_scrollback_line publish proposed state") else {
            return false;
        };
        *state = proposed_state;
        drop(state);
        if manifest_prepare_required {
            if let Err(error) = self.persist_manifest("prepared") {
                let Ok(mut state) = self.lock_state("store_scrollback_line prepare failure") else {
                    return false;
                };
                if error.outcome_indeterminate() {
                    state.transaction_quarantined = true;
                } else {
                    *state = previous_state;
                }
                return false;
            }
        }

        let content_result = (|| -> anyhow::Result<()> {
            let mut store = self
                .lock_store("store_scrollback_line append")
                .map_err(anyhow::Error::new)?;
            anyhow::ensure!(
                store.next_seq(ledger_pane_id)? == desired_seq,
                "scrollback append sequence changed after serialized preflight"
            );
            let appended_seq = store.append_line(ledger_pane_id, &record)?;
            anyhow::ensure!(
                appended_seq == desired_seq,
                "scrollback append returned the wrong sequence"
            );

            let retained = store.line_count(ledger_pane_id);
            if retained > max_retained_rows {
                let oldest_seq = store.oldest_seq(ledger_pane_id).ok_or_else(|| {
                    anyhow::anyhow!("nonempty scrollback ledger has no oldest sequence")
                })?;
                let drop_count = u64::try_from(retained - max_retained_rows)?;
                let prune_before_seq = oldest_seq
                    .checked_add(drop_count)
                    .ok_or_else(|| anyhow::anyhow!("scrollback retention cut overflows"))?;
                store.prune_before(ledger_pane_id, prune_before_seq)?;
                store.compact_pane_if_stale(
                    ledger_pane_id,
                    LIVE_SCROLLBACK_COMPACT_MIN_STALE_BYTES,
                )?;
            }
            if let Some(wal) = append_wal.as_ref() {
                Self::verify_append_wal_target_store(wal, &store)?;
            }
            anyhow::ensure!(
                target_authority.matches_store_facts(&store)?,
                "incremental append receipt disagrees with synchronized store facts"
            );
            Ok(())
        })();
        if content_result.is_err() {
            if let Ok(mut state) = self.lock_state("store_scrollback_line content quarantine") {
                // A synchronized prepared manifest or active WAL may already
                // authorize recovery. Never roll the generation back after
                // entering either durable transaction.
                state.transaction_quarantined = true;
            }
            return false;
        }

        let Ok(mut state) = self.lock_state("store_scrollback_line retention") else {
            return false;
        };
        state.max_retained_rows = max_retained_rows;
        state.verified_ledger = Some(target_authority);
        drop(state);

        match self.persist_manifest("complete") {
            Ok(()) => {
                if let Err(error) = self.advance_authenticated_append_wal_supersession() {
                    log::warn!(
                        "deferred append WAL supersession acknowledgement after committed scrollback append: {error:#}"
                    );
                }
                true
            }
            Err(error) => {
                if append_wal.is_some() || error.outcome_indeterminate() {
                    if let Ok(mut state) =
                        self.lock_state("store_scrollback_line quarantine publication")
                    {
                        state.transaction_quarantined = true;
                    }
                }
                false
            }
        }
    }

    fn load_scrollback_line(
        &self,
        stable_row: wezterm_term::StableRowIndex,
    ) -> Option<wezterm_term::Line> {
        let _mutation_gate = self.lock_mutation_gate("load_scrollback_line").ok()?;
        let _filesystem_mutation_lease =
            self.lock_filesystem_mutation("load_scrollback_line").ok()?;
        let state = *self.lock_state("load_scrollback_line logical state").ok()?;
        if state.clear_manifest_published || state.transaction_quarantined {
            return None;
        }
        let ledger_pane_id = self.active_ledger_pane_id();
        self.verify_current_published_state_before_mutation(state, ledger_pane_id, false)
            .ok()?;
        let initial = state.initial_stable_row;
        let content_epoch = state.content_epoch;
        let initial = initial?;
        if stable_row < initial {
            return None;
        }
        let seq = u64::try_from(stable_row - initial).ok()?;
        let record = self
            .lock_store("load_scrollback_line read")
            .ok()?
            .line_at(ledger_pane_id, seq)
            .ok()
            .flatten()?;
        let keyring = self.lock_keyring("load_scrollback_line decrypt").ok()?;
        let mut cipher_cache = GuardianScrollbackCipherCache::new(&keyring);
        decode_persisted_scrollback_line_with_limit(
            &record,
            &mut cipher_cache,
            self.durable_pane_id,
            content_epoch,
            stable_row,
            seq,
            LIVE_SCROLLBACK_MAX_DECODED_LINE_BYTES_USIZE,
        )
        .ok()
        .map(|(line, _decoded_bytes, _fidelity)| line)
    }

    fn oldest_scrollback_row(&self) -> Option<wezterm_term::StableRowIndex> {
        let _mutation_gate = self.lock_mutation_gate("oldest_scrollback_row").ok()?;
        let state = self.lock_state("oldest_scrollback_row initial row").ok()?;
        if state.clear_manifest_published || state.transaction_quarantined {
            return None;
        }
        let initial = state.initial_stable_row?;
        drop(state);
        let ledger_pane_id = self.active_ledger_pane_id();
        self.lock_store("oldest_scrollback_row oldest seq")
            .ok()?
            .oldest_seq(ledger_pane_id)
            .and_then(|seq| {
                wezterm_term::StableRowIndex::try_from(seq)
                    .ok()
                    .and_then(|seq| initial.checked_add(seq))
            })
    }

    fn retained_scrollback_rows(&self) -> usize {
        let Ok(_mutation_gate) = self.lock_mutation_gate("retained_scrollback_rows") else {
            return 0;
        };
        let Ok(state) = self.lock_state("retained_scrollback_rows logical state") else {
            return 0;
        };
        if state.clear_manifest_published || state.transaction_quarantined {
            return 0;
        }
        drop(state);
        let ledger_pane_id = self.active_ledger_pane_id();
        self.lock_store("retained_scrollback_rows")
            .map(|store| store.line_count(ledger_pane_id))
            .unwrap_or(0)
    }

    fn retained_scrollback_bytes(&self) -> usize {
        let Ok(_mutation_gate) = self.lock_mutation_gate("retained_scrollback_bytes") else {
            return 0;
        };
        let Ok(state) = self.lock_state("retained_scrollback_bytes logical state") else {
            return 0;
        };
        if state.clear_manifest_published || state.transaction_quarantined {
            return 0;
        }
        drop(state);
        let ledger_pane_id = self.active_ledger_pane_id();
        self.lock_store("retained_scrollback_bytes")
            .map(|store| store.retained_bytes(ledger_pane_id))
            .unwrap_or(0)
            .try_into()
            .unwrap_or(usize::MAX)
    }

    fn snapshot_scrollback(
        &self,
        expected_newest_exclusive: wezterm_term::StableRowIndex,
        limits: wezterm_term::config::ScrollbackSnapshotLimits,
    ) -> Result<
        wezterm_term::config::ScrollbackSnapshot,
        wezterm_term::config::ScrollbackSpillError,
    > {
        use wezterm_term::config::{
            ScrollbackSnapshot, ScrollbackSnapshotFidelity, ScrollbackSpillError,
        };

        let _mutation_gate = self.lock_mutation_gate("snapshot_scrollback")?;
        let _filesystem_mutation_lease = self.lock_filesystem_mutation("snapshot_scrollback")?;
        let state = *self.lock_state("snapshot_scrollback state")?;

        if state.transaction_quarantined {
            return Err(ScrollbackSpillError::CommitOutcomeIndeterminate);
        }
        if state
            .newest_stable_row_exclusive
            .is_some_and(|newest| newest != expected_newest_exclusive)
        {
            return Err(ScrollbackSpillError::SnapshotRangeMismatch);
        }
        let ledger_pane_id = self.active_ledger_pane_id();
        let manifest_before =
            self.authenticated_manifest_for_snapshot(state, ledger_pane_id)?;
        if state.clear_manifest_published {
            if let Some(manifest) = manifest_before.as_ref() {
                let store = self.lock_store("snapshot_scrollback cleared digest")?;
                Self::verify_logical_ledger_digest_from_store(
                    manifest,
                    ledger_pane_id,
                    &store,
                )
                .map_err(|_| ScrollbackSpillError::StorageUnavailable)?;
                drop(store);
                self.revalidate_snapshot_manifest(manifest)?;
            }
            return ScrollbackSnapshot::from_contiguous_rows(
                state.snapshot_generation(),
                if state.authenticated_manifest {
                    ScrollbackSnapshotFidelity::ExactSemantic
                } else {
                    ScrollbackSnapshotFidelity::LegacyRedacted
                },
                None,
                expected_newest_exclusive,
                0,
                0,
                Vec::new(),
            );
        }
        let store = self.lock_store("snapshot_scrollback store")?;
        if let Some(manifest) = manifest_before.as_ref() {
            Self::verify_logical_ledger_digest_from_store(manifest, ledger_pane_id, &store)
                .map_err(|_| ScrollbackSpillError::StorageUnavailable)?;
        }

        let retained_rows = store.line_count(ledger_pane_id);
        let retained_rows_u64 = u64::try_from(retained_rows)
            .map_err(|_| ScrollbackSpillError::ArithmeticOverflow("row_count"))?;
        if retained_rows > limits.max_rows {
            return Err(ScrollbackSpillError::ResourceLimit {
                resource: "rows",
                observed: retained_rows_u64,
                maximum: u64::try_from(limits.max_rows).unwrap_or(u64::MAX),
            });
        }
        let stored_bytes = store.retained_bytes(ledger_pane_id);
        if stored_bytes > limits.max_stored_bytes {
            return Err(ScrollbackSpillError::ResourceLimit {
                resource: "stored_bytes",
                observed: stored_bytes,
                maximum: limits.max_stored_bytes,
            });
        }
        let physical_bytes = store.file_bytes(ledger_pane_id);
        if physical_bytes > limits.max_physical_bytes {
            return Err(ScrollbackSpillError::ResourceLimit {
                resource: "physical_bytes",
                observed: physical_bytes,
                maximum: limits.max_physical_bytes,
            });
        }

        let next_seq = store
            .next_seq(ledger_pane_id)
            .map_err(|_| ScrollbackSpillError::StorageUnavailable)?;
        let Some(initial_stable_row) = state.initial_stable_row else {
            if retained_rows != 0 || store.oldest_seq(ledger_pane_id).is_some() || next_seq != 0 {
                return Err(ScrollbackSpillError::SnapshotRangeMismatch);
            }
            let snapshot = ScrollbackSnapshot::from_contiguous_rows(
                state.snapshot_generation(),
                if state.authenticated_manifest {
                    ScrollbackSnapshotFidelity::ExactSemantic
                } else {
                    ScrollbackSnapshotFidelity::LegacyRedacted
                },
                None,
                expected_newest_exclusive,
                stored_bytes,
                0,
                Vec::new(),
            )?;
            drop(store);
            if let Some(manifest) = manifest_before.as_ref() {
                self.revalidate_snapshot_manifest(manifest)?;
            }
            return Ok(snapshot);
        };
        let next_stable_offset = wezterm_term::StableRowIndex::try_from(next_seq)
            .map_err(|_| ScrollbackSpillError::ArithmeticOverflow("stable_row_range"))?;
        let observed_newest_exclusive = initial_stable_row
            .checked_add(next_stable_offset)
            .ok_or(ScrollbackSpillError::ArithmeticOverflow("stable_row_range"))?;
        if observed_newest_exclusive != expected_newest_exclusive {
            return Err(ScrollbackSpillError::SnapshotRangeMismatch);
        }

        let oldest_seq = store.oldest_seq(ledger_pane_id);
        if (retained_rows == 0) != oldest_seq.is_none() {
            return Err(ScrollbackSpillError::SnapshotRangeMismatch);
        }
        let oldest_stable_row = match oldest_seq {
            Some(oldest_seq) => {
                let oldest_offset = wezterm_term::StableRowIndex::try_from(oldest_seq)
                    .map_err(|_| ScrollbackSpillError::ArithmeticOverflow("stable_row_range"))?;
                Some(
                    initial_stable_row
                        .checked_add(oldest_offset)
                        .ok_or(ScrollbackSpillError::ArithmeticOverflow("stable_row_range"))?,
                )
            }
            None => None,
        };

        let mut rows = Vec::new();
        rows.try_reserve_exact(retained_rows)
            .map_err(|_| ScrollbackSpillError::StorageUnavailable)?;
        let mut decoded_bytes = 0usize;
        let mut fidelity = if state.authenticated_manifest {
            ScrollbackSnapshotFidelity::ExactSemantic
        } else {
            ScrollbackSnapshotFidelity::LegacyRedacted
        };
        let keyring = self.lock_keyring("snapshot_scrollback decrypt")?;
        let mut cipher_cache = GuardianScrollbackCipherCache::new(&keyring);
        if let Some(oldest_seq) = oldest_seq {
            for offset in 0..retained_rows_u64 {
                let seq = oldest_seq
                    .checked_add(offset)
                    .ok_or(ScrollbackSpillError::ArithmeticOverflow("sequence"))?;
                let record = store
                    .line_at(ledger_pane_id, seq)
                    .map_err(|_| ScrollbackSpillError::StorageUnavailable)?
                    .ok_or(ScrollbackSpillError::SnapshotRowMissing)?;
                let remaining_decoded_bytes = limits
                    .max_decoded_bytes
                    .checked_sub(decoded_bytes)
                    .ok_or(ScrollbackSpillError::ResourceLimit {
                        resource: "decoded_bytes",
                        observed: u64::try_from(decoded_bytes).unwrap_or(u64::MAX),
                        maximum: u64::try_from(limits.max_decoded_bytes).unwrap_or(u64::MAX),
                    })?;
                if mux::guardian_output_journal::GuardianEncryptedScrollbackRow::has_encrypted_prefix(
                    &record,
                ) {
                    let parsed = mux::guardian_output_journal::GuardianEncryptedScrollbackRow::parse(
                        &record,
                    )
                    .map_err(|_| ScrollbackSpillError::StorageUnavailable)?;
                    let line_bytes = usize::try_from(parsed.plaintext_bytes())
                        .map_err(|_| ScrollbackSpillError::StorageUnavailable)?;
                    if line_bytes > remaining_decoded_bytes {
                        return Err(ScrollbackSpillError::ResourceLimit {
                            resource: "decoded_bytes",
                            observed: u64::try_from(decoded_bytes.saturating_add(line_bytes))
                                .unwrap_or(u64::MAX),
                            maximum: u64::try_from(limits.max_decoded_bytes)
                                .unwrap_or(u64::MAX),
                        });
                    }
                }
                let stable_offset = wezterm_term::StableRowIndex::try_from(seq)
                    .map_err(|_| ScrollbackSpillError::ArithmeticOverflow("stable_row_range"))?;
                let stable_row = initial_stable_row
                    .checked_add(stable_offset)
                    .ok_or(ScrollbackSpillError::ArithmeticOverflow("stable_row_range"))?;
                let (line, line_decoded_bytes, row_fidelity) =
                    decode_persisted_scrollback_line_with_limit(
                        &record,
                        &mut cipher_cache,
                        self.durable_pane_id,
                        state.content_epoch,
                        stable_row,
                        seq,
                        remaining_decoded_bytes,
                    )
                    .map_err(|_| ScrollbackSpillError::StorageUnavailable)?;
                if row_fidelity == DecodedScrollbackRecordFidelity::LegacyRedacted {
                    fidelity = ScrollbackSnapshotFidelity::LegacyRedacted;
                }
                decoded_bytes = decoded_bytes.checked_add(line_decoded_bytes).ok_or(
                    ScrollbackSpillError::ArithmeticOverflow("decoded_bytes"),
                )?;
                rows.push(line);
            }
        }

        let snapshot = ScrollbackSnapshot::from_contiguous_rows(
            state.snapshot_generation(),
            fidelity,
            oldest_stable_row,
            expected_newest_exclusive,
            stored_bytes,
            decoded_bytes,
            rows,
        )?;
        drop(store);
        if let Some(manifest) = manifest_before.as_ref() {
            self.revalidate_snapshot_manifest(manifest)?;
        }
        Ok(snapshot)
    }

    fn replace_scrollback_prefix(
        &self,
        expected_generation: Option<wezterm_term::config::ScrollbackSnapshotGeneration>,
        prefix: wezterm_term::config::ScrollbackPrefix<'_>,
        max_retained_rows: usize,
    ) -> Result<wezterm_term::config::ScrollbackReplaceCommit, wezterm_term::config::ScrollbackSpillError>
    {
        use wezterm_term::config::{
            ScrollbackReplaceCommit, ScrollbackSnapshotGeneration, ScrollbackSpillError,
        };

        let _mutation_gate = self.lock_mutation_gate("replace_scrollback_prefix")?;
        let _filesystem_mutation_lease =
            self.lock_filesystem_mutation("replace_scrollback_prefix")?;
        let row_count = prefix.row_count();
        if row_count > max_retained_rows {
            return Err(ScrollbackSpillError::ResourceLimit {
                resource: "rows",
                observed: u64::try_from(row_count).unwrap_or(u64::MAX),
                maximum: u64::try_from(max_retained_rows).unwrap_or(u64::MAX),
            });
        }
        let previous_state = *self.lock_state("replace_scrollback_prefix CAS state")?;
        if previous_state.transaction_quarantined {
            return Err(ScrollbackSpillError::CommitOutcomeIndeterminate);
        }
        if !previous_state.authenticated_manifest {
            return Err(ScrollbackSpillError::SnapshotGenerationMismatch);
        }
        let previous_ledger_pane_id = self.active_ledger_pane_id();
        self.verify_current_published_state_before_mutation(
            previous_state,
            previous_ledger_pane_id,
            false,
        )?;
        self.advance_authenticated_append_wal_supersession()
            .map_err(|_| ScrollbackSpillError::StorageUnavailable)?;
        let logical_row_count = if previous_state.clear_manifest_published {
            0
        } else {
            self.lock_store("replace_scrollback_prefix CAS rows")?
                .line_count(previous_ledger_pane_id)
        };
        let successor_generation = match expected_generation {
            Some(expected) => {
                if previous_state.snapshot_generation() != expected {
                    return Err(ScrollbackSpillError::SnapshotGenerationMismatch);
                }
                ScrollbackSnapshotGeneration::new(
                    expected.content_epoch(),
                    expected
                        .revision()
                        .checked_add(1)
                        .ok_or(ScrollbackSpillError::RevisionExhausted)?,
                )
            }
            None => {
                let pristine = previous_state.authenticated_manifest
                    && previous_state.revision == 0
                    && previous_state.predecessor_generation.is_none()
                    && previous_state.initial_stable_row.is_none()
                    && previous_state.newest_stable_row_exclusive.is_none()
                    && !previous_state.clear_manifest_published
                    && !previous_state.clear_pending_physical_reclamation
                    && logical_row_count == 0;
                if !pristine {
                    return Err(ScrollbackSpillError::SnapshotGenerationMismatch);
                }
                ScrollbackSnapshotGeneration::new(previous_state.content_epoch, 1)
            }
        };
        let mut proposed_state = LiveScrollbackSpillState {
            initial_stable_row: prefix.oldest_stable_row(),
            newest_stable_row_exclusive: Some(prefix.newest_stable_row_exclusive()),
            max_retained_rows,
            content_epoch: successor_generation.content_epoch(),
            revision: successor_generation.revision(),
            authenticated_manifest: true,
            predecessor_generation: expected_generation,
            clear_manifest_published: false,
            clear_pending_physical_reclamation: false,
            transaction_quarantined: false,
            verified_ledger: None,
        };
        let replacement_ledger_pane_id = self.replacement_ledger_pane_id(
            successor_generation,
            expected_generation,
            prefix.oldest_stable_row(),
            prefix.newest_stable_row_exclusive(),
            row_count,
            max_retained_rows,
        )?;

        let mut records = Vec::new();
        records
            .try_reserve_exact(row_count)
            .map_err(|_| ScrollbackSpillError::StorageUnavailable)?;
        let mut committed_record_bytes = 0_u64;
        let cipher = {
            let mut keyring = self.lock_keyring("replace_scrollback_prefix active key")?;
            keyring
                .latest_active_cipher()
                .map_err(|_| ScrollbackSpillError::StorageUnavailable)?
        };
        for (index, line) in prefix.rows().enumerate() {
            let oldest_stable_row = prefix
                .oldest_stable_row()
                .ok_or(ScrollbackSpillError::SnapshotRangeMismatch)?;
            let stable_offset = wezterm_term::StableRowIndex::try_from(index)
                .map_err(|_| ScrollbackSpillError::ArithmeticOverflow("stable_row_range"))?;
            let stable_row = oldest_stable_row
                .checked_add(stable_offset)
                .ok_or(ScrollbackSpillError::ArithmeticOverflow("stable_row_range"))?;
            let sequence = u64::try_from(index)
                .map_err(|_| ScrollbackSpillError::ArithmeticOverflow("sequence"))?;
            let identity = mux::guardian_output_journal::GuardianScrollbackRowIdentity::new(
                self.durable_pane_id,
                proposed_state.content_epoch,
                proposed_state.revision,
                i64::try_from(stable_row).map_err(|_| {
                    ScrollbackSpillError::ArithmeticOverflow("stable_row_identity")
                })?,
                sequence,
            )
            .map_err(|_| ScrollbackSpillError::StorageUnavailable)?;
            let record = encode_exact_scrollback_line_record(line, &cipher, identity)
                .ok_or(ScrollbackSpillError::StorageUnavailable)?;
            let record_bytes = u64::try_from(record.len())
                .map_err(|_| ScrollbackSpillError::ArithmeticOverflow("stored_bytes"))?;
            committed_record_bytes = committed_record_bytes
                .checked_add(record_bytes)
                .and_then(|bytes| bytes.checked_add(1))
                .ok_or(ScrollbackSpillError::ArithmeticOverflow("stored_bytes"))?;
            if committed_record_bytes > LIVE_SCROLLBACK_REPLACEMENT_MAX_COMMITTED_BYTES {
                return Err(ScrollbackSpillError::ResourceLimit {
                    resource: "stored_bytes",
                    observed: committed_record_bytes,
                    maximum: LIVE_SCROLLBACK_REPLACEMENT_MAX_COMMITTED_BYTES,
                });
            }
            records.push(record);
        }

        let staged = self
            .lock_store("replace_scrollback_prefix stage ledger")?
            .stage_versioned_pane_replacement(
                replacement_ledger_pane_id,
                &records,
                max_retained_rows,
                LIVE_SCROLLBACK_REPLACEMENT_MAX_COMMITTED_BYTES,
            )
            .map_err(|_| ScrollbackSpillError::StorageUnavailable)?;
        if staged.record_count() != row_count
            || staged.committed_bytes() != committed_record_bytes
        {
            return Err(ScrollbackSpillError::StorageUnavailable);
        }
        if staged.reused_existing() {
            let existing_records = {
                let store = self.lock_store("replace_scrollback_prefix reopen staged ledger")?;
                let mut existing = Vec::new();
                existing
                    .try_reserve_exact(row_count)
                    .map_err(|_| ScrollbackSpillError::StorageUnavailable)?;
                for sequence in 0..u64::try_from(row_count)
                    .map_err(|_| ScrollbackSpillError::ArithmeticOverflow("sequence"))?
                {
                    existing.push(
                        store
                            .line_at(staged.pane_id(), sequence)
                            .map_err(|_| ScrollbackSpillError::StorageUnavailable)?
                            .ok_or(ScrollbackSpillError::SnapshotRowMissing)?,
                    );
                }
                existing
            };
            let keyring = self.lock_keyring("replace_scrollback_prefix verify staged retry")?;
            for (index, (line, record)) in
                prefix.rows().zip(existing_records.iter()).enumerate()
            {
                let sequence = u64::try_from(index)
                    .map_err(|_| ScrollbackSpillError::ArithmeticOverflow("sequence"))?;
                let oldest_stable_row = prefix
                    .oldest_stable_row()
                    .ok_or(ScrollbackSpillError::SnapshotRangeMismatch)?;
                let stable_row = oldest_stable_row
                    .checked_add(wezterm_term::StableRowIndex::try_from(index).map_err(|_| {
                        ScrollbackSpillError::ArithmeticOverflow("stable_row_range")
                    })?)
                    .ok_or(ScrollbackSpillError::ArithmeticOverflow("stable_row_range"))?;
                let parsed =
                    mux::guardian_output_journal::GuardianEncryptedScrollbackRow::parse(record)
                        .map_err(|_| ScrollbackSpillError::StorageUnavailable)?;
                let expected_identity =
                    mux::guardian_output_journal::GuardianScrollbackRowIdentity::new(
                        self.durable_pane_id,
                        proposed_state.content_epoch,
                        proposed_state.revision,
                        i64::try_from(stable_row).map_err(|_| {
                            ScrollbackSpillError::ArithmeticOverflow("stable_row_identity")
                        })?,
                        sequence,
                    )
                    .map_err(|_| ScrollbackSpillError::StorageUnavailable)?;
                if parsed.identity() != expected_identity
                    || !exact_scrollback_line_record_is_equivalent(
                        record,
                        line,
                        &keyring,
                        self.durable_pane_id,
                        proposed_state.content_epoch,
                        stable_row,
                        sequence,
                    )
                {
                    return Err(ScrollbackSpillError::StorageUnavailable);
                }
            }
            drop(keyring);
            records = existing_records;
        }

        let replacement_authority = VerifiedLedgerState::from_records(
            staged.pane_id(),
            &records,
        )
        .map_err(|_| ScrollbackSpillError::StorageUnavailable)?;
        if replacement_authority.record_count
            != u64::try_from(row_count)
                .map_err(|_| ScrollbackSpillError::ArithmeticOverflow("row_count"))?
            || replacement_authority.retained_record_bytes != committed_record_bytes
        {
            return Err(ScrollbackSpillError::StorageUnavailable);
        }
        proposed_state.verified_ledger = Some(replacement_authority);

        {
            let mut state = self.lock_state("replace_scrollback_prefix publish state")?;
            *state = proposed_state;
        }
        self.active_ledger_pane_id
            .store(staged.pane_id(), std::sync::atomic::Ordering::Release);

        if let Err(error) = self.persist_manifest("complete") {
            if !error.outcome_indeterminate() {
                self.active_ledger_pane_id
                    .store(previous_ledger_pane_id, std::sync::atomic::Ordering::Release);
                *self.lock_state("replace_scrollback_prefix publication rollback")? =
                    previous_state;
                return Err(ScrollbackSpillError::StorageUnavailable);
            }

            let recovered_publication = self
                .reread_and_verify_replacement_manifest(
                    proposed_state,
                    expected_generation,
                    prefix.oldest_stable_row(),
                    prefix.newest_stable_row_exclusive(),
                    row_count,
                    staged,
                )
                .and_then(|()| {
                    #[cfg(not(windows))]
                    {
                        let parent = self.manifest_path.parent().ok_or_else(|| {
                            anyhow::anyhow!("scrollback manifest path has no parent")
                        })?;
                        std::fs::File::open(parent)?.sync_all()?;
                    }
                    self.reread_and_verify_replacement_manifest(
                        proposed_state,
                        expected_generation,
                        prefix.oldest_stable_row(),
                        prefix.newest_stable_row_exclusive(),
                        row_count,
                        staged,
                    )
                })
                .is_ok();
            if !recovered_publication {
                if let Ok(mut state) =
                    self.lock_state("replace_scrollback_prefix quarantine publication")
                {
                    state.transaction_quarantined = true;
                }
                return Err(ScrollbackSpillError::CommitOutcomeIndeterminate);
            }
        }

        let ledger_verified = (|| -> anyhow::Result<()> {
            let mut store = self
                .store
                .lock()
                .map_err(|_| anyhow::anyhow!("scrollback store is poisoned"))?;
            store
                .verify_staged_pane_ledger(staged, &records)
                .context("reopen and verify published replacement ledger")?;
            anyhow::ensure!(
                store.line_count(staged.pane_id()) == row_count
                    && store.oldest_seq(staged.pane_id()) == (row_count != 0).then_some(0)
                    && store.next_seq(staged.pane_id())? == u64::try_from(row_count)?,
                "published replacement ledger range changed after publication"
            );
            if let Some(initial_stable_row) = prefix.oldest_stable_row() {
                let keyring = self
                    .keyring
                    .lock()
                    .map_err(|_| anyhow::anyhow!("guardian output keyring is poisoned"))?;
                Self::validate_persisted_records(
                    &store,
                    staged.pane_id(),
                    &keyring,
                    self.durable_pane_id,
                    proposed_state.content_epoch,
                    initial_stable_row,
                    true,
                )
                .context("authenticate every published replacement row")?;
            }
            drop(store);
            self.reread_and_verify_replacement_manifest(
                proposed_state,
                expected_generation,
                prefix.oldest_stable_row(),
                prefix.newest_stable_row_exclusive(),
                row_count,
                staged,
            )
        })();
        if ledger_verified.is_err() {
            if let Ok(mut state) =
                self.lock_state("replace_scrollback_prefix quarantine verification")
            {
                state.transaction_quarantined = true;
            }
            return Err(ScrollbackSpillError::CommitOutcomeIndeterminate);
        }

        if let Err(error) = self.advance_authenticated_append_wal_supersession() {
            log::warn!(
                "deferred append WAL supersession acknowledgement after committed scrollback replacement: {error:#}"
            );
        }

        // The previously published immutable ledger remains an unreachable
        // archival recovery generation. Do not overwrite or delete historical
        // ledgers until a separately authorized, authenticated GC policy can
        // prove that no checkpoint or rollback authority still names them.
        Ok(ScrollbackReplaceCommit::new(
            successor_generation,
            prefix.oldest_stable_row(),
            prefix.newest_stable_row_exclusive(),
        ))
    }

    fn clear_scrollback(
        &self,
    ) -> Result<
        wezterm_term::config::ScrollbackClearCommit,
        wezterm_term::config::ScrollbackSpillError,
    > {
        use wezterm_term::config::{ScrollbackClearCommit, ScrollbackSpillError};

        let _mutation_gate = self.lock_mutation_gate("clear_scrollback")?;
        let _filesystem_mutation_lease = self.lock_filesystem_mutation("clear_scrollback")?;
        let ledger_pane_id = self.active_ledger_pane_id();
        let current_state = *self.lock_state("clear_scrollback current state")?;
        if current_state.transaction_quarantined {
            return Err(ScrollbackSpillError::CommitOutcomeIndeterminate);
        }
        self.verify_current_published_state_before_mutation(
            current_state,
            ledger_pane_id,
            true,
        )?;
        self.advance_authenticated_append_wal_supersession()
            .map_err(|_| ScrollbackSpillError::StorageUnavailable)?;
        // Prove the physical store is available before publishing a logical
        // clear. Holding this guard through publication also prevents a clear
        // manifest from becoming durable after store-lock poisoning has made
        // reclamation impossible to even attempt.
        let mut store = self.lock_store("clear_scrollback store availability")?;
        let (previous, clear_generation) = {
            let mut state = self.lock_state("clear_scrollback state reset")?;
            let previous = *state;
            let predecessor = previous.snapshot_generation();
            *state = LiveScrollbackSpillState {
                initial_stable_row: None,
                newest_stable_row_exclusive: None,
                max_retained_rows: 0,
                content_epoch: self.clear_content_epoch(predecessor),
                revision: 0,
                authenticated_manifest: true,
                predecessor_generation: Some(predecessor),
                clear_manifest_published: true,
                clear_pending_physical_reclamation: true,
                transaction_quarantined: false,
                verified_ledger: Some(VerifiedLedgerState::empty(ledger_pane_id)),
            };
            (previous, state.snapshot_generation())
        };
        // Publish the clear intent before truncating the content log. If the
        // process dies between these operations, constructor recovery observes
        // `cleared` and completes the idempotent clear before accepting data.
        if let Err(error) = self.persist_manifest("cleared") {
            if error.outcome_indeterminate() {
                if let Ok(mut state) = self.lock_state("clear_scrollback quarantine publication") {
                    state.transaction_quarantined = true;
                }
                log::error!("scrollback clear publication outcome is indeterminate");
                return Err(ScrollbackSpillError::CommitOutcomeIndeterminate);
            }
            *self.lock_state("clear_scrollback manifest rollback")? = previous;
            log::error!("failed to publish scrollback clear intent");
            return Err(ScrollbackSpillError::StorageUnavailable);
        }
        if let Err(error) = self.advance_authenticated_append_wal_supersession() {
            log::warn!(
                "deferred append WAL supersession acknowledgement after committed scrollback clear: {error:#}"
            );
        }
        let physical_reclaimed = store.clear_pane(ledger_pane_id).is_ok();
        drop(store);
        if physical_reclaimed {
            if let Ok(mut state) = self.lock_state("clear_scrollback physical completion") {
                state.clear_pending_physical_reclamation = false;
            } else {
                log::error!("scrollback state unavailable after committed physical clear");
            }
        } else {
            // The `cleared` manifest is already durable, so the logical clear
            // is committed. Keep the old bytes unreachable and retry their
            // physical reclamation before accepting a later row.
            log::error!("deferred physical reclamation after committed scrollback clear");
        }
        Ok(ScrollbackClearCommit::new(clear_generation))
    }
}

fn validate_live_scrollback_manifest_identity(
    manifest: &LiveScrollbackManifestV1,
    durable_pane_id: &str,
    manifest_path: &std::path::Path,
) -> anyhow::Result<()> {
    let generation = live_scrollback_manifest_generation(manifest).with_context(|| {
        format!(
            "validate live scrollback manifest generation at {}",
            manifest_path.display()
        )
    })?;
    anyhow::ensure!(
        manifest.durable_pane_id == durable_pane_id,
        "live scrollback manifest identity mismatch at {}",
        manifest_path.display()
    );
    let _ledger_pane_id = LiveScrollbackSpillSink::manifest_ledger_pane_id(manifest)
        .with_context(|| {
            format!(
                "validate live scrollback ledger pointer at {}",
                manifest_path.display()
            )
        })?;
    anyhow::ensure!(
        matches!(
            manifest.publication_state.as_str(),
            "prepared" | "complete" | "cleared"
        ),
        "invalid live scrollback publication state at {}",
        manifest_path.display()
    );
    anyhow::ensure!(
        generation.is_none()
            || manifest.publication_state != "cleared"
            || live_scrollback_cleared_manifest_is_canonical(manifest),
        "generation-aware cleared scrollback manifest has a non-empty logical ledger at {}",
        manifest_path.display()
    );
    anyhow::ensure!(
        (manifest.retained_rows == 0) == manifest.oldest_seq.is_none(),
        "live scrollback manifest has inconsistent retained-row bounds at {}",
        manifest_path.display()
    );
    anyhow::ensure!(
        manifest.retained_rows <= manifest.max_retained_rows,
        "live scrollback manifest exceeds its authenticated retention bound at {}",
        manifest_path.display()
    );
    let manifest_interval_end = match manifest.oldest_seq {
        Some(oldest) => oldest
            .checked_add(manifest.retained_rows)
            .ok_or_else(|| anyhow::anyhow!("live scrollback manifest sequence overflow"))?,
        None => manifest.next_seq,
    };
    anyhow::ensure!(
        manifest_interval_end == manifest.next_seq,
        "live scrollback manifest next sequence is inconsistent at {}",
        manifest_path.display()
    );
    if live_scrollback_manifest_is_authenticated(manifest)
        && manifest.publication_state != "cleared"
    {
        let newest = manifest.newest_stable_row_exclusive.ok_or_else(|| {
            anyhow::anyhow!(
                "authenticated live scrollback manifest is missing its stable-row endpoint at {}",
                manifest_path.display()
            )
        })?;
        if manifest.retained_rows != 0
            || manifest.next_seq != 0
            || manifest.publication_state == "prepared"
        {
            let initial = manifest.initial_stable_row.ok_or_else(|| {
                anyhow::anyhow!(
                    "authenticated live scrollback manifest is missing its initial stable row at {}",
                    manifest_path.display()
                )
            })?;
            let next_offset = wezterm_term::StableRowIndex::try_from(manifest.next_seq)
                .map_err(|_| anyhow::anyhow!("live scrollback stable-row range exceeds platform limits"))?;
            let ledger_newest = initial.checked_add(next_offset).ok_or_else(|| {
                anyhow::anyhow!("live scrollback stable-row endpoint overflows")
            })?;
            anyhow::ensure!(
                (manifest.publication_state == "prepared" && newest >= ledger_newest)
                    || newest == ledger_newest,
                "authenticated live scrollback stable-row endpoint is inconsistent at {}",
                manifest_path.display()
            );
        } else {
            anyhow::ensure!(
                manifest.initial_stable_row.is_none(),
                "authenticated empty replacement has a nonempty stable-row origin at {}",
                manifest_path.display()
            );
        }
    }
    Ok(())
}

fn validate_live_scrollback_directory(
    path: &std::path::Path,
    require_private: bool,
) -> anyhow::Result<std::fs::Metadata> {
    let metadata = std::fs::symlink_metadata(path)
        .with_context(|| format!("inspect live scrollback pane directory {}", path.display()))?;
    anyhow::ensure!(
        metadata.file_type().is_dir(),
        "live scrollback pane path is not a directory: {}",
        path.display()
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;

        anyhow::ensure!(
            !require_private || metadata.permissions().mode() & 0o077 == 0,
            "live scrollback pane directory is not private: {}",
            path.display()
        );
    }
    Ok(metadata)
}

fn validate_live_scrollback_content_file(
    path: &std::path::Path,
    label: &'static str,
    allow_missing: bool,
) -> anyhow::Result<()> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if allow_missing && error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(())
        }
        Err(error) => return Err(error).with_context(|| format!("inspect {label} {}", path.display())),
    };
    anyhow::ensure!(
        metadata.file_type().is_file(),
        "{label} is not a regular file: {}",
        path.display()
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;

        anyhow::ensure!(
            metadata.permissions().mode() & 0o077 == 0,
            "{label} is not private: {}",
            path.display()
        );
    }
    Ok(())
}

/// Read and authenticate the exact v3/v4 generation/pointer identity without
/// opening the selected ledger for mutation.
pub fn read_live_scrollback_committed_ledger_identity(
    base_dir: &std::path::Path,
    durable_pane_id: &str,
) -> anyhow::Result<LiveScrollbackCommittedLedgerIdentity> {
    anyhow::ensure!(
        is_canonical_live_scrollback_id(durable_pane_id),
        "invalid durable pane ID '{durable_pane_id}' (expected 32 lowercase hex characters)"
    );
    let pane_path = base_dir.join(durable_pane_id);
    let pane_metadata_before = validate_live_scrollback_directory(&pane_path, true)?;
    let manifest_path = pane_path.join("manifest.json");
    let manifest_before = LiveScrollbackSpillSink::read_manifest(&manifest_path)?
        .ok_or_else(|| anyhow::anyhow!("live scrollback manifest is missing"))?;
    validate_live_scrollback_manifest_identity(
        &manifest_before,
        durable_pane_id,
        &manifest_path,
    )?;
    anyhow::ensure!(
        live_scrollback_manifest_is_authenticated(&manifest_before)
            && manifest_before.publication_state == "complete"
            && LiveScrollbackSpillSink::authenticate_manifest_for_read(
                base_dir,
                &manifest_before,
            )?,
        "scrollback ledger identity requires one authenticated complete manifest"
    );
    let ledger_pane_id = LiveScrollbackSpillSink::manifest_ledger_pane_id(&manifest_before)?;
    validate_live_scrollback_content_file(
        &pane_path.join(format!("{ledger_pane_id}.log")),
        "live scrollback log",
        false,
    )?;
    validate_live_scrollback_content_file(
        &pane_path.join(format!("{ledger_pane_id}.seq")),
        "live scrollback sequence journal",
        false,
    )?;
    let identity_max_rows = usize::try_from(manifest_before.retained_rows)
        .map_err(|_| anyhow::anyhow!("v3 retained row count exceeds platform limits"))?;
    anyhow::ensure!(
        identity_max_rows <= LIVE_SCROLLBACK_EXPORT_MAX_ROWS,
        "v3 retained row count exceeds the hard identity-read limit"
    );
    let snapshot = frankenterm_core::storage::mmap_store::read_pane_snapshot(
        &pane_path,
        ledger_pane_id,
        identity_max_rows,
        LIVE_SCROLLBACK_EXPORT_MAX_PHYSICAL_BYTES,
        LIVE_SCROLLBACK_EXPORT_MAX_PHYSICAL_BYTES,
    )
    .context("read authenticated ledger for committed identity")?;
    let manifest_after = LiveScrollbackSpillSink::read_manifest(&manifest_path)?
        .ok_or_else(|| anyhow::anyhow!("live scrollback manifest disappeared during read"))?;
    anyhow::ensure!(
        manifest_before == manifest_after
            && LiveScrollbackSpillSink::authenticate_manifest_for_read(
                base_dir,
                &manifest_after,
            )?,
        "live scrollback manifest changed or lost authority during identity read"
    );
    let logical_ledger_digest = verify_live_scrollback_logical_ledger_digest_from_snapshot(
        &manifest_after,
        ledger_pane_id,
        &snapshot,
    )
    .context("verify logical ledger for committed identity")?;
    let pane_metadata_after = std::fs::symlink_metadata(&pane_path)?;
    anyhow::ensure!(
        !filesystem_metadata_changed(&pane_metadata_before, &pane_metadata_after)?,
        "live scrollback pane directory changed during identity read"
    );

    let (content_epoch, revision) = live_scrollback_manifest_generation(&manifest_after)?
        .ok_or_else(|| anyhow::anyhow!("v3 scrollback generation is missing"))?;
    let generation = wezterm_term::config::ScrollbackSnapshotGeneration::new(
        content_epoch,
        revision,
    );
    let initial_stable_row = manifest_after.initial_stable_row;
    let oldest_stable_row = match (initial_stable_row, manifest_after.oldest_seq) {
        (Some(initial), Some(oldest_sequence)) => {
            let offset = wezterm_term::StableRowIndex::try_from(oldest_sequence)
                .map_err(|_| anyhow::anyhow!("scrollback oldest sequence exceeds stable-row range"))?;
            Some(initial.checked_add(offset).ok_or_else(|| {
                anyhow::anyhow!("scrollback oldest stable-row identity overflows")
            })?)
        }
        (Some(_), None) if manifest_after.retained_rows == 0 => None,
        (None, None) if manifest_after.retained_rows == 0 && manifest_after.next_seq == 0 => None,
        _ => anyhow::bail!("scrollback stable-row range is incomplete"),
    };
    let newest_stable_row_exclusive = manifest_after
        .newest_stable_row_exclusive
        .ok_or_else(|| anyhow::anyhow!("v3 scrollback stable-row endpoint is missing"))?;
    let authentication =
        mux::guardian_output_journal::GuardianScrollbackManifestAuthentication::parse(
            manifest_after
                .guardian_manifest_authentication
                .as_deref()
                .ok_or_else(|| anyhow::anyhow!("manifest authentication is missing"))?,
        )?;
    let mut durable_pane_bytes = [0; 16];
    hex::decode_to_slice(durable_pane_id, &mut durable_pane_bytes)
        .map_err(|_| anyhow::anyhow!("invalid durable pane identity"))?;
    let mut manifest_digest = [0; 32];
    hex::decode_to_slice(&manifest_after.manifest_sha256, &mut manifest_digest)
        .map_err(|_| anyhow::anyhow!("invalid scrollback manifest digest"))?;
    Ok(LiveScrollbackCommittedLedgerIdentity {
        durable_pane_id: durable_pane_bytes,
        generation,
        predecessor: live_scrollback_manifest_predecessor(&manifest_after)?,
        manifest_digest,
        manifest_authentication_key_id: authentication.key_id(),
        logical_ledger_digest,
        ledger_pane_id,
        oldest_sequence: manifest_after.oldest_seq,
        next_sequence: manifest_after.next_seq,
        oldest_stable_row,
        newest_stable_row_exclusive,
        record_count: manifest_after.retained_rows,
        retained_record_bytes: manifest_after
            .retained_record_bytes
            .ok_or_else(|| anyhow::anyhow!("v3 retained-record byte bound is missing"))?,
        committed_log_bytes: manifest_after
            .committed_log_bytes
            .ok_or_else(|| anyhow::anyhow!("v3 committed-log byte bound is missing"))?,
        committed_sequence_bytes: manifest_after
            .committed_sequence_bytes
            .ok_or_else(|| anyhow::anyhow!("v3 sequence byte bound is missing"))?,
    })
}

/// Enumerate continuously persisted pane stores without opening their content
/// logs for write or attempting repair.
pub fn list_live_scrollback_panes(
    base_dir: &std::path::Path,
    max_entries: usize,
) -> anyhow::Result<Vec<LiveScrollbackDurablePane>> {
    anyhow::ensure!(
        (1..=LIVE_SCROLLBACK_EXPORT_MAX_PANES).contains(&max_entries),
        "max_entries must be in 1..={LIVE_SCROLLBACK_EXPORT_MAX_PANES}"
    );
    let base_metadata_before = match std::fs::symlink_metadata(base_dir) {
        Ok(_) => validate_live_scrollback_directory(base_dir, false)?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("inspect live scrollback directory {}", base_dir.display()));
        }
    };

    let mut pane_paths = Vec::new();
    let max_scanned_entries = max_entries.saturating_add(LIVE_SCROLLBACK_DISCOVERY_EXTRA_ENTRIES);
    let mut scanned_entries = 0usize;
    for entry in std::fs::read_dir(base_dir)
        .with_context(|| format!("read live scrollback directory {}", base_dir.display()))?
    {
        let entry = entry?;
        scanned_entries = scanned_entries.saturating_add(1);
        anyhow::ensure!(
            scanned_entries <= max_scanned_entries,
            "live scrollback discovery exceeded the {max_scanned_entries}-entry scan limit"
        );
        let Some(name) = entry.file_name().to_str().map(ToString::to_string) else {
            continue;
        };
        if !is_canonical_live_scrollback_id(&name) {
            continue;
        }
        if pane_paths.len() >= max_entries {
            anyhow::bail!(
                "live scrollback directory contains more than the configured {max_entries} panes"
            );
        }
        pane_paths.push((name, entry.path()));
    }
    pane_paths.sort_by(|left, right| left.0.cmp(&right.0));

    let mut panes = Vec::with_capacity(pane_paths.len());
    for (durable_pane_id, pane_path) in pane_paths {
        let manifest_path = pane_path.join("manifest.json");
        let result = (|| -> anyhow::Result<LiveScrollbackManifestV1> {
            let metadata_before = validate_live_scrollback_directory(&pane_path, true)?;
            let manifest_before = LiveScrollbackSpillSink::read_manifest(&manifest_path)?
                .ok_or_else(|| anyhow::anyhow!("live scrollback manifest is missing"))?;
            validate_live_scrollback_manifest_identity(
                &manifest_before,
                &durable_pane_id,
                &manifest_path,
            )?;
            let authenticated = LiveScrollbackSpillSink::authenticate_manifest_for_read(
                base_dir,
                &manifest_before,
            )?;
            let ledger_pane_id =
                LiveScrollbackSpillSink::manifest_ledger_pane_id(&manifest_before)?;
            validate_live_scrollback_content_file(
                &pane_path.join(format!("{ledger_pane_id}.log")),
                "live scrollback log",
                !authenticated && manifest_before.publication_state == "cleared",
            )?;
            validate_live_scrollback_content_file(
                &pane_path.join(format!("{ledger_pane_id}.seq")),
                "live scrollback sequence journal",
                !authenticated,
            )?;
            let authenticated_snapshot = if authenticated
                && manifest_before.publication_state != "cleared"
            {
                let retained_rows = usize::try_from(manifest_before.retained_rows)
                    .map_err(|_| anyhow::anyhow!("v3 retained rows exceed platform limits"))?;
                anyhow::ensure!(
                    retained_rows <= LIVE_SCROLLBACK_EXPORT_MAX_ROWS,
                    "v3 retained rows exceed the hard discovery limit"
                );
                Some(
                    frankenterm_core::storage::mmap_store::read_pane_snapshot(
                        &pane_path,
                        ledger_pane_id,
                        retained_rows,
                        LIVE_SCROLLBACK_EXPORT_MAX_PHYSICAL_BYTES,
                        LIVE_SCROLLBACK_EXPORT_MAX_PHYSICAL_BYTES,
                    )
                    .context("read authenticated logical ledger during discovery")?,
                )
            } else {
                None
            };
            let manifest_after = LiveScrollbackSpillSink::read_manifest(&manifest_path)?
                .ok_or_else(|| {
                    anyhow::anyhow!("live scrollback manifest disappeared during discovery")
                })?;
            anyhow::ensure!(
                manifest_before == manifest_after,
                "live scrollback manifest changed during discovery"
            );
            validate_live_scrollback_manifest_identity(
                &manifest_after,
                &durable_pane_id,
                &manifest_path,
            )?;
            anyhow::ensure!(
                LiveScrollbackSpillSink::authenticate_manifest_for_read(
                    base_dir,
                    &manifest_after,
                )? == authenticated,
                "live scrollback manifest authority changed during discovery"
            );
            if authenticated {
                if let Some(snapshot) = authenticated_snapshot.as_ref() {
                    verify_live_scrollback_logical_ledger_digest_from_snapshot(
                        &manifest_after,
                        ledger_pane_id,
                        snapshot,
                    )
                    .context("verify authenticated logical ledger during discovery")?;
                } else {
                    if manifest_after.schema == LIVE_SCROLLBACK_MANIFEST_SCHEMA_V3 {
                        let observed = live_scrollback_logical_ledger_digest_from_records(
                            &manifest_after,
                            ledger_pane_id,
                            None,
                            0,
                            0,
                            0,
                            0,
                            &[],
                        )?;
                        anyhow::ensure!(
                            observed
                                == expected_live_scrollback_logical_ledger_digest(&manifest_after)?,
                            "cleared authenticated v3 logical ledger digest mismatch"
                        );
                    } else {
                        let (anchor, tail) =
                            expected_live_scrollback_v4_chain(&manifest_after)?;
                        anyhow::ensure!(
                            anchor == tail,
                            "cleared authenticated v4 chain is nonempty"
                        );
                    }
                }
            }
            let metadata_after = std::fs::symlink_metadata(&pane_path)?;
            anyhow::ensure!(
                !filesystem_metadata_changed(&metadata_before, &metadata_after)?,
                "live scrollback pane directory changed during discovery"
            );
            Ok(manifest_after)
        })();
        match result {
            Ok(manifest) => panes.push(LiveScrollbackDurablePane {
                durable_pane_id,
                state: manifest.publication_state,
                source_pane_id: Some(manifest.source_pane_id),
                source_domain_id: Some(manifest.source_domain_id),
                command_description: Some(manifest.command_description),
                retained_rows: Some(manifest.retained_rows),
                next_seq: Some(manifest.next_seq),
                path: pane_path,
                error: None,
            }),
            Err(error) => panes.push(LiveScrollbackDurablePane {
                durable_pane_id,
                state: "corrupt".to_string(),
                source_pane_id: None,
                source_domain_id: None,
                command_description: None,
                retained_rows: None,
                next_seq: None,
                path: pane_path,
                error: Some(format!("{error:#}")),
            }),
        }
    }
    let base_metadata_after = std::fs::symlink_metadata(base_dir)?;
    anyhow::ensure!(
        !filesystem_metadata_changed(&base_metadata_before, &base_metadata_after)?,
        "live scrollback base directory changed during discovery"
    );
    Ok(panes)
}

/// Export a continuously persisted pane as plain text without opening source
/// files for write or sending bytes into a live PTY.
pub fn export_live_scrollback_transcript(
    base_dir: &std::path::Path,
    durable_pane_id: &str,
    max_rows: usize,
    max_transcript_bytes: usize,
    max_physical_bytes: u64,
) -> anyhow::Result<LiveScrollbackTranscriptExport> {
    anyhow::ensure!(
        is_canonical_live_scrollback_id(durable_pane_id),
        "invalid durable pane ID '{durable_pane_id}' (expected 32 lowercase hex characters)"
    );
    anyhow::ensure!(max_rows > 0, "max_rows must be greater than zero");
    anyhow::ensure!(
        max_rows <= LIVE_SCROLLBACK_EXPORT_MAX_ROWS,
        "max_rows exceeds hard limit {LIVE_SCROLLBACK_EXPORT_MAX_ROWS}"
    );
    anyhow::ensure!(
        max_transcript_bytes > 0,
        "max_transcript_bytes must be greater than zero"
    );
    anyhow::ensure!(
        max_transcript_bytes <= LIVE_SCROLLBACK_EXPORT_MAX_TRANSCRIPT_BYTES,
        "max_transcript_bytes exceeds hard limit {LIVE_SCROLLBACK_EXPORT_MAX_TRANSCRIPT_BYTES}"
    );
    anyhow::ensure!(
        max_physical_bytes > 0,
        "max_physical_bytes must be greater than zero"
    );
    anyhow::ensure!(
        max_physical_bytes <= LIVE_SCROLLBACK_EXPORT_MAX_PHYSICAL_BYTES,
        "max_physical_bytes exceeds hard limit {LIVE_SCROLLBACK_EXPORT_MAX_PHYSICAL_BYTES}"
    );

    let pane_path = base_dir.join(durable_pane_id);
    let pane_metadata_before = validate_live_scrollback_directory(&pane_path, true)?;
    let manifest_path = pane_path.join("manifest.json");
    let manifest_before = LiveScrollbackSpillSink::read_manifest(&manifest_path)?
        .ok_or_else(|| anyhow::anyhow!("live scrollback manifest is missing"))?;
    validate_live_scrollback_manifest_identity(
        &manifest_before,
        durable_pane_id,
        &manifest_path,
    )?;
    let authenticated_manifest = LiveScrollbackSpillSink::authenticate_manifest_for_read(
        base_dir,
        &manifest_before,
    )?;
    let ledger_pane_id = LiveScrollbackSpillSink::manifest_ledger_pane_id(&manifest_before)?;
    validate_live_scrollback_content_file(
        &pane_path.join(format!("{ledger_pane_id}.log")),
        "live scrollback log",
        !authenticated_manifest && manifest_before.publication_state == "cleared",
    )?;
    validate_live_scrollback_content_file(
        &pane_path.join(format!("{ledger_pane_id}.seq")),
        "live scrollback sequence journal",
        !authenticated_manifest,
    )?;

    let snapshot = frankenterm_core::storage::mmap_store::read_pane_snapshot(
        &pane_path,
        ledger_pane_id,
        max_rows,
        max_physical_bytes,
        max_physical_bytes,
    )
    .with_context(|| format!("read live scrollback content from {}", pane_path.display()))?;
    let manifest_after = LiveScrollbackSpillSink::read_manifest(&manifest_path)?
        .ok_or_else(|| anyhow::anyhow!("live scrollback manifest disappeared during export"))?;
    anyhow::ensure!(
        manifest_before == manifest_after,
        "live scrollback manifest changed during export; retry against a stable source"
    );
    validate_live_scrollback_manifest_identity(
        &manifest_after,
        durable_pane_id,
        &manifest_path,
    )?;
    anyhow::ensure!(
        LiveScrollbackSpillSink::authenticate_manifest_for_read(base_dir, &manifest_after)?
            == authenticated_manifest,
        "live scrollback manifest authority changed during export"
    );
    if authenticated_manifest {
        verify_live_scrollback_logical_ledger_digest_from_snapshot(
            &manifest_after,
            ledger_pane_id,
            &snapshot,
        )
        .context("verify authenticated logical ledger during export")?;
    }
    let pane_metadata_after = std::fs::symlink_metadata(&pane_path)?;
    anyhow::ensure!(
        !filesystem_metadata_changed(&pane_metadata_before, &pane_metadata_after)?,
        "live scrollback pane directory changed during export; retry against a stable source"
    );

    if authenticated_manifest && manifest_before.publication_state != "cleared" {
        anyhow::ensure!(
            snapshot.oldest_seq == manifest_before.oldest_seq
                && snapshot.next_seq == manifest_before.next_seq
                && u64::try_from(snapshot.records.len())? == manifest_before.retained_rows
                && Some(snapshot.retained_record_bytes)
                    == manifest_before.retained_record_bytes
                && Some(snapshot.committed_bytes) == manifest_before.committed_log_bytes
                && Some(snapshot.sequence_bytes) == manifest_before.committed_sequence_bytes,
            "authenticated live scrollback ledger disagrees with its manifest"
        );
    }

    if manifest_before.publication_state != "cleared" {
        anyhow::ensure!(
            manifest_before.retained_rows == 0 || !snapshot.records.is_empty(),
            "live scrollback content is empty behind a manifest with retained rows"
        );
        anyhow::ensure!(
            snapshot.next_seq >= manifest_before.next_seq,
            "live scrollback content rolled back behind its published manifest"
        );
        if let Some(recorded) = manifest_before.oldest_seq {
            let actual = snapshot.oldest_seq.ok_or_else(|| {
                anyhow::anyhow!(
                    "live scrollback content has no oldest sequence behind a retained manifest"
                )
            })?;
            anyhow::ensure!(
                actual >= recorded,
                "live scrollback content starts before its published retention boundary"
            );
        }
    }

    let records = if manifest_before.publication_state == "cleared" {
        &[][..]
    } else {
        snapshot.records.as_slice()
    };
    if !records.is_empty() {
        anyhow::ensure!(
            manifest_before.initial_stable_row.is_some(),
            "live scrollback records have no initial stable-row identity"
        );
    }

    let exact_semantic_records = records
        .iter()
        .filter(|record| {
            mux::guardian_output_journal::GuardianEncryptedScrollbackRow::has_encrypted_prefix(
                record,
            )
        })
        .count();
    let legacy_non_recovery_grade_records = records
        .len()
        .checked_sub(exact_semantic_records)
        .ok_or_else(|| anyhow::anyhow!("scrollback export record accounting underflow"))?;
    let legacy_redaction_attested_but_unauthenticated_records = records
        .iter()
        .filter(|record| {
            record.starts_with(LIVE_SCROLLBACK_LINE_RECORD_V1_UNCOMPRESSED)
                || record.starts_with(LIVE_SCROLLBACK_LINE_RECORD_V1_ZSTD)
                || record.starts_with(LIVE_SCROLLBACK_LINE_RECORD_V2_UNCOMPRESSED)
                || record.starts_with(LIVE_SCROLLBACK_LINE_RECORD_V2_ZSTD)
        })
        .count();
    let raw_legacy_redaction_unknown_records = legacy_non_recovery_grade_records
        .checked_sub(legacy_redaction_attested_but_unauthenticated_records)
        .ok_or_else(|| anyhow::anyhow!("scrollback export provenance accounting underflow"))?;
    anyhow::ensure!(
        !authenticated_manifest || legacy_non_recovery_grade_records == 0,
        "authenticated scrollback ledger contains legacy/non-exact rows"
    );
    let contains_exact_records = exact_semantic_records != 0;
    let keyring = if contains_exact_records {
        Some(
            guardian_output_keys::GuardianOutputKeyring::open_existing_scrollback_sibling(base_dir)
                .context("open guardian output keyring for scrollback transcript export")?,
        )
    } else {
        None
    };
    let mut cipher_cache = keyring
        .as_ref()
        .map(GuardianScrollbackCipherCache::new);
    let content_epoch = live_scrollback_manifest_generation(&manifest_before)?
        .map(|(epoch, _revision)| epoch);
    let mut durable_pane_bytes = [0; 16];
    if contains_exact_records {
        hex::decode_to_slice(durable_pane_id, &mut durable_pane_bytes)
            .map_err(|_| anyhow::anyhow!("invalid durable pane identity"))?;
    }
    let initial_stable_row = manifest_before.initial_stable_row;
    let oldest_seq = snapshot.oldest_seq;
    let redactor = frankenterm_core::redactor::Redactor::new();
    let mut transcript = String::new();
    for (index, record) in records.iter().enumerate() {
        let line = if mux::guardian_output_journal::GuardianEncryptedScrollbackRow::has_encrypted_prefix(
            record,
        ) {
            let sequence = oldest_seq
                .and_then(|oldest| oldest.checked_add(u64::try_from(index).ok()?))
                .ok_or_else(|| anyhow::anyhow!("live scrollback export sequence overflow"))?;
            let stable_row = initial_stable_row
                .and_then(|initial| {
                    wezterm_term::StableRowIndex::try_from(sequence)
                        .ok()
                        .and_then(|offset| initial.checked_add(offset))
                })
                .ok_or_else(|| anyhow::anyhow!("live scrollback export row identity overflow"))?;
            let cipher_cache = cipher_cache
                .as_mut()
                .ok_or_else(|| anyhow::anyhow!("encrypted scrollback keyring is unavailable"))?;
            let content_epoch = content_epoch.ok_or_else(|| {
                anyhow::anyhow!("encrypted scrollback record has no content epoch")
            })?;
            decode_persisted_scrollback_line_with_limit(
                record,
                cipher_cache,
                durable_pane_bytes,
                content_epoch,
                stable_row,
                sequence,
                LIVE_SCROLLBACK_MAX_DECODED_LINE_BYTES_USIZE,
            )
            .with_context(|| format!("decrypt live scrollback record {index}"))?
            .0
        } else if record.starts_with(LIVE_SCROLLBACK_LINE_RECORD_V1_UNCOMPRESSED)
            || record.starts_with(LIVE_SCROLLBACK_LINE_RECORD_V1_ZSTD)
            || record.starts_with(LIVE_SCROLLBACK_LINE_RECORD_V2_UNCOMPRESSED)
            || record.starts_with(LIVE_SCROLLBACK_LINE_RECORD_V2_ZSTD)
        {
            decode_scrollback_line_record(record).ok_or_else(|| {
                anyhow::anyhow!("live scrollback record {index} failed bounded integrity decoding")
            })?
        } else if record.starts_with("ftsl") {
            anyhow::bail!(
                "live scrollback record {index} has an unrecognized reserved record prefix"
            );
        } else {
            anyhow::ensure!(
                record.len() <= LIVE_SCROLLBACK_MAX_DECODED_LINE_BYTES_USIZE,
                "legacy text scrollback record {index} exceeds the hard decoded-byte limit"
            );
            legacy_text_scrollback_line(record)
        };
        let text = line.as_str();
        let redacted_text = redactor.redact(text.as_ref());
        let delimiter_bytes = usize::from(!line.last_cell_was_wrapped());
        let next_len = transcript
            .len()
            .checked_add(redacted_text.len())
            .and_then(|len| len.checked_add(delimiter_bytes))
            .ok_or_else(|| anyhow::anyhow!("live scrollback transcript length overflow"))?;
        anyhow::ensure!(
            next_len <= max_transcript_bytes,
            "live scrollback transcript exceeds configured {max_transcript_bytes}-byte limit"
        );
        transcript.push_str(&redacted_text);
        if !line.last_cell_was_wrapped() {
            transcript.push('\n');
        }
    }

    Ok(LiveScrollbackTranscriptExport {
        durable_pane_id: durable_pane_id.to_string(),
        publication_state: manifest_before.publication_state,
        source_pane_id: manifest_before.source_pane_id,
        source_domain_id: manifest_before.source_domain_id,
        command_description: manifest_before.command_description,
        initial_stable_row: manifest_before.initial_stable_row,
        oldest_seq: if records.is_empty() {
            None
        } else {
            snapshot.oldest_seq
        },
        next_seq: if records.is_empty() && manifest_after.publication_state == "cleared" {
            manifest_after.next_seq
        } else {
            snapshot.next_seq
        },
        retained_rows: records.len(),
        transcript_bytes: transcript.len(),
        transcript,
        committed_log_bytes: snapshot.committed_bytes,
        physical_log_bytes: snapshot.physical_bytes,
        trailing_uncommitted_bytes: snapshot.trailing_uncommitted_bytes,
        exact_semantic_records,
        legacy_non_recovery_grade_records,
        pre_persistence_redaction_not_applied_records: exact_semantic_records,
        legacy_redaction_attested_but_unauthenticated_records,
        raw_legacy_redaction_unknown_records,
        redaction_applied_during_export: !records.is_empty(),
        source_content_mutated: false,
        source_path: pane_path,
    })
}

#[must_use]
pub fn default_live_scrollback_dir() -> PathBuf {
    if let Some(base) = std::env::var_os("XDG_DATA_HOME").filter(|value| !value.is_empty()) {
        return PathBuf::from(base).join("ft").join("scrollback-lines");
    }
    if let Some(home) = std::env::var_os("HOME").filter(|value| !value.is_empty()) {
        return PathBuf::from(home)
            .join(".local")
            .join("share")
            .join("ft")
            .join("scrollback-lines");
    }
    PathBuf::from(".").join(".ft").join("scrollback-lines")
}

pub fn install_scrollback_spill_sink_factory() {
    let base_dir = Arc::new(default_live_scrollback_dir());
    config::set_scrollback_spill_sink_factory(Some(Arc::new(move |context| {
        match LiveScrollbackSpillSink::new((*base_dir).clone(), &context) {
            Ok(sink) => Some(Arc::new(sink)),
            Err(error) => {
                log::warn!(
                    "failed to initialize live scrollback spill sink for pane {} domain {}: {}",
                    context.pane_id,
                    context.domain_id,
                    error
                );
                None
            }
        }
    })));
}

pub fn update_mux_domains(config: &ConfigHandle) -> anyhow::Result<()> {
    match reconcile_mux_domains(config)? {
        MuxDomainUpdateOutcome::Converged => Ok(()),
        MuxDomainUpdateOutcome::PendingRetirements { domain_names } => {
            anyhow::bail!(
                "configured domains are awaiting exact-generation retirement before replacement: {domain_names:?}"
            )
        }
    }
}

pub fn reconcile_mux_domains(config: &ConfigHandle) -> anyhow::Result<MuxDomainUpdateOutcome> {
    install_scrollback_spill_sink_factory();
    update_mux_domains_impl(config, false)
}

pub fn update_mux_domains_for_server(config: &ConfigHandle) -> anyhow::Result<()> {
    match reconcile_mux_domains_for_server(config)? {
        MuxDomainUpdateOutcome::Converged => Ok(()),
        MuxDomainUpdateOutcome::PendingRetirements { domain_names } => {
            anyhow::bail!(
                "configured domains are awaiting exact-generation retirement before replacement: {domain_names:?}"
            )
        }
    }
}

pub fn reconcile_mux_domains_for_server(
    config: &ConfigHandle,
) -> anyhow::Result<MuxDomainUpdateOutcome> {
    install_scrollback_spill_sink_factory();
    update_mux_domains_impl(config, true)
}

fn add_configured_domain_or_defer(
    mux: &Arc<Mux>,
    domain: &Arc<dyn Domain>,
    pending_retirements: &mut Vec<String>,
) -> anyhow::Result<()> {
    match mux.add_domain(domain) {
        Ok(()) => Ok(()),
        Err(
            DomainRegistrationError::RetiredIdentifier { .. }
            | DomainRegistrationError::IdentifierInUse { .. }
            | DomainRegistrationError::NameInUse { .. },
        ) => {
            pending_retirements.push(domain.domain_name().to_string());
            Ok(())
        }
        Err(error) => Err(anyhow::Error::new(error)),
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ConfiguredRawDomainReconcileOutcome {
    Current,
    Registered,
    PendingRetirement,
}

fn reconcile_raw_domain_config(
    mux: &Arc<Mux>,
    expected: &ConfiguredRawDomain,
) -> anyhow::Result<ConfiguredRawDomainReconcileOutcome> {
    let domain_name = expected.name();
    if let Some(registered) = mux.get_domain_by_name(domain_name) {
        if expected.matches_registration(&registered) {
            return Ok(ConfiguredRawDomainReconcileOutcome::Current);
        }
        if registered.is::<ClientDomain>() || is_configured_raw_registration(&registered) {
            retire_configured_registration(mux, &registered)?;
            return Ok(ConfiguredRawDomainReconcileOutcome::PendingRetirement);
        }
        anyhow::bail!(
            "configured {} domain {domain_name:?} collides with a live runtime-owned domain",
            expected.kind(),
        );
    }

    let domain = expected.instantiate()?;
    let mut pending = Vec::new();
    add_configured_domain_or_defer(mux, &domain, &mut pending)?;
    if pending.is_empty() {
        Ok(ConfiguredRawDomainReconcileOutcome::Registered)
    } else {
        Ok(ConfiguredRawDomainReconcileOutcome::PendingRetirement)
    }
}

fn validate_desired_domain_collisions(
    mux: &Arc<Mux>,
    desired_client_names: &BTreeSet<String>,
    desired_raw_domains: &ConfiguredRawDomains,
) -> anyhow::Result<()> {
    for registered in mux.iter_domains() {
        let name = registered.domain_name();
        if !desired_client_names.contains(name) && !desired_raw_domains.contains_key(name) {
            continue;
        }
        anyhow::ensure!(
            registered.is::<ClientDomain>() || is_configured_raw_registration(&registered),
            "configured domain {name:?} collides with a live runtime-owned domain"
        );
    }
    Ok(())
}

fn validate_requested_default_domain(
    mux: &Arc<Mux>,
    desired_client_names: &BTreeSet<String>,
    desired_raw_domains: &ConfiguredRawDomains,
    default_name: Option<&String>,
    is_standalone_mux: bool,
) -> anyhow::Result<()> {
    let Some(name) = default_name else {
        return Ok(());
    };
    let key = if is_standalone_mux {
        "default_mux_server_domain"
    } else {
        "default_domain"
    };

    if desired_client_names.contains(name) {
        anyhow::ensure!(
            !is_standalone_mux,
            "default_mux_server_domain cannot be set to a client domain!"
        );
        return Ok(());
    }
    if desired_raw_domains.contains_key(name) {
        return Ok(());
    }

    let Some(registered) = mux.get_domain_by_name(name) else {
        anyhow::bail!("configured {key}={name:?} does not match any registered domain");
    };
    anyhow::ensure!(
        !registered.is::<ClientDomain>() && !is_configured_raw_registration(&registered),
        "configured {key}={name:?} names a configuration-owned domain absent from the desired configuration"
    );
    Ok(())
}

fn update_mux_domains_impl(
    config: &ConfigHandle,
    is_standalone_mux: bool,
) -> anyhow::Result<MuxDomainUpdateOutcome> {
    let mux = Mux::try_get().context("mux singleton is not available")?;
    let client_configs = configured_client_domains(config);
    let desired_client_names = client_configs
        .iter()
        .map(|client| client.name().to_string())
        .collect::<BTreeSet<_>>();
    anyhow::ensure!(
        desired_client_names.len() == client_configs.len(),
        "configured client domain names must be unique"
    );
    let desired_raw_domains = configured_raw_domains(config)?;
    if let Some(duplicate) = desired_client_names
        .iter()
        .find(|name| desired_raw_domains.contains_key(name))
    {
        anyhow::bail!(
            "configured domain name {duplicate:?} is duplicated across client and raw transports"
        );
    }

    // Fail before mutation for a desired configured name that is occupied by
    // a runtime-created domain. Only domains carrying explicit configuration
    // ownership may participate in transport replacement.
    validate_desired_domain_collisions(&mux, &desired_client_names, &desired_raw_domains)?;
    validate_requested_default_domain(
        &mux,
        &desired_client_names,
        &desired_raw_domains,
        if is_standalone_mux {
            config.default_mux_server_domain.as_ref()
        } else {
            config.default_domain.as_ref()
        },
        is_standalone_mux,
    )?;

    let mut pending_retirements = Vec::new();

    // Reconcile every configuration-owned transport class symmetrically.
    // Logical retirement closes name-based admission immediately; the exact
    // operation guard retained by this sweep fences destructive cleanup until
    // this iteration ends. Runtime-created domains are deliberately excluded.
    for registered in mux.iter_domains() {
        let domain_name = registered.domain_name().to_string();
        if registered.is::<ClientDomain>() {
            if desired_client_names.contains(&domain_name) {
                continue;
            }
        } else if is_configured_raw_registration(&registered) {
            if desired_raw_domains
                .get(&domain_name)
                .is_some_and(|expected| expected.matches_registration(&registered))
            {
                continue;
            }
        } else {
            continue;
        }

        retire_configured_registration(&mux, &registered)?;
        if desired_client_names.contains(&domain_name)
            || desired_raw_domains.contains_key(&domain_name)
        {
            pending_retirements.push(domain_name);
        }
    }

    for client_config in &client_configs {
        let domain_name = client_config.name().to_string();
        match reconcile_client_domain_config(&mux, client_config)? {
            ConfiguredClientDomainReconcileOutcome::Current
            | ConfiguredClientDomainReconcileOutcome::Registered => {}
            ConfiguredClientDomainReconcileOutcome::PendingRetirement => {
                pending_retirements.push(domain_name);
            }
            ConfiguredClientDomainReconcileOutcome::NotConfigured => {
                unreachable!("a directly supplied client configuration cannot be absent")
            }
        }
    }

    for raw_config in desired_raw_domains.iter() {
        match reconcile_raw_domain_config(&mux, raw_config)? {
            ConfiguredRawDomainReconcileOutcome::Current
            | ConfiguredRawDomainReconcileOutcome::Registered => {}
            ConfiguredRawDomainReconcileOutcome::PendingRetirement => {
                pending_retirements.push(raw_config.name().to_string());
            }
        }
    }

    if is_standalone_mux {
        if let Some(name) = &config.default_mux_server_domain {
            if desired_client_names.contains(name) {
                anyhow::bail!("default_mux_server_domain cannot be set to a client domain!");
            }
            let Some(dom) = mux.get_domain_by_name(name) else {
                anyhow::ensure!(
                    pending_retirements.iter().any(|pending| pending == name),
                    "configured default_mux_server_domain={name:?} does not match any registered domain"
                );
                pending_retirements.sort();
                pending_retirements.dedup();
                return Ok(MuxDomainUpdateOutcome::PendingRetirements {
                    domain_names: pending_retirements,
                });
            };
            if dom.is::<ClientDomain>() {
                anyhow::bail!("default_mux_server_domain cannot be set to a client domain!");
            }
            mux.set_default_domain_guard(&dom)?;
        }
    } else if let Some(name) = &config.default_domain {
        match mux.get_domain_by_name(name) {
            Some(dom) => mux.set_default_domain_guard(&dom)?,
            None => anyhow::ensure!(
                pending_retirements.iter().any(|pending| pending == name),
                "configured default_domain={name:?} does not match any registered domain"
            ),
        }
    }

    pending_retirements.sort();
    pending_retirements.dedup();
    if pending_retirements.is_empty() {
        Ok(MuxDomainUpdateOutcome::Converged)
    } else {
        Ok(MuxDomainUpdateOutcome::PendingRetirements {
            domain_names: pending_retirements,
        })
    }
}

pub static PKI: std::sync::LazyLock<pki::Pki> =
    std::sync::LazyLock::new(|| pki::Pki::init().expect("failed to initialize PKI"));

#[cfg(test)]
mod tests {
    use super::*;
    use config::{Config, ExecDomain, SerialDomain, SshDomain, WslDomain};
    use std::sync::MutexGuard;
    use termwiz::cell::CellAttributes;
    use wezterm_term::Line;
    use wezterm_term::config::ScrollbackSpillSink;

    fn test_lock() -> &'static RecoveringTestMutex {
        &GLOBAL_STATE_TEST_LOCK
    }

    struct ScopedTestState {
        _lock: MutexGuard<'static, ()>,
    }

    impl ScopedTestState {
        fn acquire() -> Self {
            let lock = test_lock()
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            reset_test_state();
            Self { _lock: lock }
        }
    }

    #[test]
    fn global_test_state_lock_recovers_without_cascading_failures() {
        let lock = Arc::new(RecoveringTestMutex::new());
        let poisoner = Arc::clone(&lock);
        let result = std::thread::spawn(move || {
            let _guard = poisoner.lock().expect("fresh test lock");
            panic!("deliberately poison the private test lock");
        })
        .join();
        assert!(
            result.is_err(),
            "the poisoner must exercise unwind recovery"
        );

        let _guard = lock
            .lock()
            .expect("the next test owner must recover poisoned state");
    }

    impl Drop for ScopedTestState {
        fn drop(&mut self) {
            reset_test_state();
        }
    }

    fn make_test_handle(ssh_domains: Vec<SshDomain>) -> ConfigHandle {
        make_test_handle_with(ssh_domains, |_| {})
    }

    fn make_test_handle_with(
        ssh_domains: Vec<SshDomain>,
        configure: impl FnOnce(&mut Config),
    ) -> ConfigHandle {
        let mut config = Config::default_config();
        config.unix_domains.clear();
        config.tls_clients.clear();
        config.ssh_domains = Some(ssh_domains);
        config.wsl_domains = Some(Vec::new());
        config.exec_domains.clear();
        config.serial_ports.clear();
        configure(&mut config);
        config::use_this_configuration(config);
        config::configuration()
    }

    fn wait_for_domain_reconciliation(config: &ConfigHandle) -> anyhow::Result<()> {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            match reconcile_mux_domains(config)? {
                MuxDomainUpdateOutcome::Converged => return Ok(()),
                MuxDomainUpdateOutcome::PendingRetirements { .. } => {
                    anyhow::ensure!(
                        std::time::Instant::now() < deadline,
                        "configured domain replacement did not converge after exact guards drained"
                    );
                    std::thread::yield_now();
                }
            }
        }
    }

    fn reset_test_state() {
        config::use_test_configuration();
        config::set_scrollback_spill_sink_factory(None);
        Mux::shutdown();
    }

    fn prepare_later_row_append_wal_for_test(
        sink: &LiveScrollbackSpillSink,
        stable_row: wezterm_term::StableRowIndex,
        line: &Line,
        max_retained_rows: usize,
    ) -> (LiveScrollbackAppendWalV1, LiveScrollbackSpillState) {
        let previous_state = *sink
            .lock_state("prepare test append WAL predecessor")
            .expect("lock test append WAL predecessor state");
        let initial_stable_row = previous_state
            .initial_stable_row
            .expect("later-row WAL fixture has a stable-row origin");
        let desired_sequence = u64::try_from(
            stable_row
                .checked_sub(initial_stable_row)
                .expect("test later row follows the stable-row origin"),
        )
        .expect("test stable-row offset fits u64");
        let mut proposed_state = previous_state;
        proposed_state
            .advance_revision()
            .expect("advance test append WAL generation");
        proposed_state.clear_manifest_published = false;
        proposed_state.newest_stable_row_exclusive = Some(
            stable_row
                .checked_add(1)
                .expect("test append WAL stable-row endpoint fits"),
        );
        proposed_state.max_retained_rows = max_retained_rows;
        let row_identity =
            mux::guardian_output_journal::GuardianScrollbackRowIdentity::new(
                sink.durable_pane_id,
                proposed_state.content_epoch,
                proposed_state.revision,
                i64::try_from(stable_row).expect("test stable row fits i64"),
                desired_sequence,
            )
            .expect("construct test append WAL row identity");
        let record = {
            let mut keyring = sink
                .lock_keyring("prepare test append WAL row")
                .expect("lock test append WAL keyring");
            let cipher = keyring
                .latest_active_cipher()
                .expect("load test append WAL key");
            encode_exact_scrollback_line_record(line, &cipher, row_identity)
                .expect("seal test append WAL row")
        };
        let predecessor_manifest = LiveScrollbackSpillSink::read_manifest(&sink.manifest_path)
            .expect("read test append WAL predecessor")
            .expect("test append WAL predecessor exists");
        let store = sink
            .lock_store("prepare test append WAL target")
            .expect("lock test append WAL store");
        let (wal, target_authority) = sink
            .prepare_authenticated_append_wal(
                &predecessor_manifest,
                previous_state,
                proposed_state,
                sink.active_ledger_pane_id(),
                stable_row,
                desired_sequence,
                max_retained_rows,
                &record,
                &store,
            )
            .expect("prepare authenticated test append WAL");
        proposed_state.verified_ledger = Some(target_authority);
        (wal, proposed_state)
    }

    fn append_wal_fixture(
        identity_byte: u8,
        max_retained_rows: usize,
    ) -> (
        tempfile::TempDir,
        config::ScrollbackSpillSinkContext,
        LiveScrollbackSpillSink,
        LiveScrollbackAppendWalV1,
        Line,
    ) {
        let dir = tempfile::tempdir().expect("create append WAL fixture directory");
        let context = config::ScrollbackSpillSinkContext {
            pane_id: u64::from(identity_byte) + 20_000,
            domain_id: 3,
            durable_pane_id: [identity_byte; 16],
            command_description: "append-wal-crash-fixture".to_string(),
        };
        let sink = LiveScrollbackSpillSink::new(dir.path().to_path_buf(), &context)
            .expect("create append WAL fixture sink");
        let attrs = CellAttributes::blank();
        assert!(sink.store_scrollback_line(
            10,
            &Line::from_text("append-wal-predecessor", &attrs, 1, None),
            max_retained_rows.max(1),
        ));
        let appended = Line::from_text("append-wal-recovered-target", &attrs, 2, None);
        let (wal, _) =
            prepare_later_row_append_wal_for_test(&sink, 11, &appended, max_retained_rows);
        (dir, context, sink, wal, appended)
    }

    fn publish_authenticated_v3_predecessor_fixture(
        sink: &LiveScrollbackSpillSink,
    ) -> LiveScrollbackManifestV1 {
        let mut manifest = LiveScrollbackSpillSink::read_manifest(&sink.manifest_path)
            .expect("read v1-WAL predecessor manifest")
            .expect("v1-WAL predecessor manifest exists");
        manifest.schema = LIVE_SCROLLBACK_MANIFEST_SCHEMA_V3.to_string();
        manifest.chain_anchor_sha256 = None;
        manifest.chain_tail_sha256 = None;
        manifest.logical_ledger_sha256 = None;
        manifest.guardian_manifest_authentication = None;
        manifest.manifest_sha256.clear();
        let ledger_pane_id = sink.active_ledger_pane_id();
        let digest = {
            let store = sink
                .lock_store("construct v1-WAL v3 predecessor digest")
                .expect("lock v1-WAL v3 predecessor store");
            LiveScrollbackSpillSink::logical_ledger_digest_from_store(
                &manifest,
                ledger_pane_id,
                &store,
            )
            .expect("construct v1-WAL v3 predecessor digest")
        };
        manifest.logical_ledger_sha256 = Some(hex::encode(digest));
        let canonical = LiveScrollbackSpillSink::manifest_authentication_bytes(&manifest)
            .expect("serialize v1-WAL v3 predecessor authority");
        let mut keyring = sink
            .lock_keyring("authenticate v1-WAL v3 predecessor")
            .expect("lock v1-WAL v3 predecessor keyring");
        let cipher = keyring
            .latest_active_cipher()
            .expect("load v1-WAL v3 predecessor key");
        manifest.guardian_manifest_authentication = Some(
            cipher
                .authenticate_scrollback_manifest(&canonical)
                .expect("authenticate v1-WAL v3 predecessor")
                .encode(),
        );
        drop(keyring);
        manifest.manifest_sha256 = LiveScrollbackSpillSink::manifest_checksum(&manifest)
            .expect("checksum v1-WAL v3 predecessor");
        overwrite_private_manifest_fixture(&sink.manifest_path, &manifest);
        manifest
    }

    fn seal_append_wal_fixture(
        sink: &LiveScrollbackSpillSink,
        wal: &mut LiveScrollbackAppendWalV1,
    ) {
        wal.guardian_authentication = Some("pending".to_string());
        wal.wal_sha256.clear();
        LiveScrollbackSpillSink::validate_append_wal_identity(wal, sink.durable_pane_id)
            .expect("validate unsealed append WAL fixture");
        wal.guardian_authentication = None;
        let canonical = LiveScrollbackSpillSink::append_wal_authentication_bytes(wal)
            .expect("serialize append WAL fixture authority");
        let mut keyring = sink
            .lock_keyring("seal append WAL fixture")
            .expect("lock append WAL fixture keyring");
        let cipher = keyring
            .latest_active_cipher()
            .expect("load append WAL fixture key");
        wal.guardian_authentication = Some(
            cipher
                .authenticate_scrollback_append_wal(&canonical)
                .expect("authenticate append WAL fixture")
                .encode(),
        );
        drop(keyring);
        LiveScrollbackSpillSink::validate_append_wal_identity(wal, sink.durable_pane_id)
            .expect("validate sealed append WAL fixture");
        wal.wal_sha256 = LiveScrollbackSpillSink::append_wal_checksum(wal)
            .expect("checksum append WAL fixture");
    }

    fn legacy_v1_append_wal_fixture(
        identity_byte: u8,
    ) -> (
        tempfile::TempDir,
        config::ScrollbackSpillSinkContext,
        LiveScrollbackSpillSink,
        LiveScrollbackAppendWalV1,
        Line,
    ) {
        let (dir, context, sink, mut wal, appended) =
            append_wal_fixture(identity_byte, 1);
        let predecessor = publish_authenticated_v3_predecessor_fixture(&sink);
        wal.schema = LIVE_SCROLLBACK_APPEND_WAL_SCHEMA_V1.to_string();
        wal.predecessor_manifest_sha256 = predecessor.manifest_sha256;
        wal.predecessor_chain_anchor_sha256 = None;
        wal.predecessor_chain_tail_sha256 = None;
        wal.target_chain_anchor_sha256 = None;
        wal.target_chain_tail_sha256 = None;
        wal.evicted_record_count = None;
        let (target_digest, retained_record_bytes) =
            live_scrollback_append_wal_target_digest(&wal, |sequence| {
                anyhow::ensure!(
                    sequence == wal.appended_sequence,
                    "single-row v1 WAL target requested an unexpected sequence"
                );
                Ok(wal.encrypted_record.clone())
            })
            .expect("construct v1 append WAL target-set digest");
        assert_eq!(retained_record_bytes, wal.target_retained_record_bytes);
        wal.target_record_set_sha256 = Some(hex::encode(target_digest));
        seal_append_wal_fixture(&sink, &mut wal);
        (dir, context, sink, wal, appended)
    }

    fn reset_authority_record_reads() {
        LIVE_SCROLLBACK_AUTHORITY_RECORD_READS.with(|reads| reads.set(0));
    }

    fn authority_record_reads() -> u64 {
        LIVE_SCROLLBACK_AUTHORITY_RECORD_READS.with(std::cell::Cell::get)
    }

    fn later_append_authority_reads(retained_rows: usize, target_retention: usize) -> u64 {
        let dir = tempfile::tempdir().expect("create incremental-authority fixture");
        let identity_byte = u8::try_from(retained_rows).unwrap_or(201);
        let context = config::ScrollbackSpillSinkContext {
            pane_id: 40_000_u64
                .checked_add(u64::try_from(retained_rows).expect("fixture row count fits u64"))
                .expect("fixture pane identity fits u64"),
            domain_id: 3,
            durable_pane_id: [identity_byte; 16],
            command_description: "incremental-authority-counter".to_string(),
        };
        let sink = LiveScrollbackSpillSink::new(dir.path().to_path_buf(), &context)
            .expect("create incremental-authority sink");
        let attributes = CellAttributes::blank();
        for row in 0..retained_rows {
            assert!(sink.store_scrollback_line(
                isize::try_from(row).expect("fixture stable row fits isize"),
                &Line::from_text(
                    &format!("incremental-authority-row-{row}"),
                    &attributes,
                    u64::try_from(row).expect("fixture row sequence fits u64"),
                    None,
                ),
                retained_rows,
            ));
        }
        reset_authority_record_reads();
        assert!(sink.store_scrollback_line(
            isize::try_from(retained_rows).expect("fixture stable row fits isize"),
            &Line::from_text(
                "incremental-authority-measured-row",
                &attributes,
                u64::try_from(retained_rows).expect("fixture row sequence fits u64"),
                None,
            ),
            target_retention,
        ));
        let reads = authority_record_reads();
        let manifest = LiveScrollbackSpillSink::read_manifest(&sink.manifest_path)
            .expect("read measured v4 manifest")
            .expect("measured v4 manifest exists");
        assert_eq!(manifest.schema, LIVE_SCROLLBACK_MANIFEST_SCHEMA_V4);
        let wal = LiveScrollbackSpillSink::read_append_wal(
            &LiveScrollbackSpillSink::append_wal_path(&sink.manifest_path)
                .expect("derive measured append WAL path"),
        )
        .expect("read measured v2 append WAL")
        .expect("measured v2 append WAL exists");
        assert_eq!(wal.schema, LIVE_SCROLLBACK_APPEND_WAL_SCHEMA_V2);
        reads
    }

    #[test]
    fn v4_later_append_authority_reads_are_constant_independent_of_retained_count() {
        let short = later_append_authority_reads(8, 16);
        let long = later_append_authority_reads(128, 256);
        assert_eq!(short, 2, "later append reads only prior/current WAL target rows");
        assert_eq!(long, short, "retained row count must not affect steady append reads");
    }

    #[test]
    fn v4_prefix_eviction_reads_exactly_the_affected_prefix_plus_constant_receipts() {
        let retained_rows = 128_usize;
        let target_retention = 16_usize;
        let evicted = retained_rows
            .checked_add(1)
            .and_then(|rows| rows.checked_sub(target_retention))
            .expect("fixture eviction arithmetic is valid");
        let reads = later_append_authority_reads(retained_rows, target_retention);
        assert_eq!(
            reads,
            u64::try_from(evicted).expect("fixture eviction count fits u64") + 2,
            "prefix eviction may read each evicted row once and only constant receipt rows"
        );
    }

    #[test]
    fn authenticated_v3_cold_open_migrates_to_v4_only_on_successor_publication() {
        let dir = tempfile::tempdir().expect("create v3 migration fixture");
        let context = config::ScrollbackSpillSinkContext {
            pane_id: 40_500,
            domain_id: 3,
            durable_pane_id: [205; 16],
            command_description: "v3-to-v4-cold-migration".to_string(),
        };
        let sink = LiveScrollbackSpillSink::new(dir.path().to_path_buf(), &context)
            .expect("create v3 migration sink");
        let attributes = CellAttributes::blank();
        assert!(sink.store_scrollback_line(
            10,
            &Line::from_text("v3-predecessor", &attributes, 1, None),
            8,
        ));
        let mut legacy = LiveScrollbackSpillSink::read_manifest(&sink.manifest_path)
            .expect("read v4 fixture manifest")
            .expect("v4 fixture manifest exists");
        legacy.schema = LIVE_SCROLLBACK_MANIFEST_SCHEMA_V3.to_string();
        legacy.chain_anchor_sha256 = None;
        legacy.chain_tail_sha256 = None;
        legacy.logical_ledger_sha256 = None;
        legacy.guardian_manifest_authentication = None;
        legacy.manifest_sha256.clear();
        let ledger_pane_id = sink.active_ledger_pane_id();
        let digest = {
            let store = sink
                .lock_store("construct v3 migration digest")
                .expect("lock v3 migration store");
            LiveScrollbackSpillSink::logical_ledger_digest_from_store(
                &legacy,
                ledger_pane_id,
                &store,
            )
            .expect("construct exact v3 logical digest")
        };
        legacy.logical_ledger_sha256 = Some(hex::encode(digest));
        let canonical = LiveScrollbackSpillSink::manifest_authentication_bytes(&legacy)
            .expect("serialize v3 migration authority");
        let mut keyring = sink
            .lock_keyring("authenticate v3 migration fixture")
            .expect("lock v3 migration keyring");
        let cipher = keyring
            .latest_active_cipher()
            .expect("load v3 migration key");
        legacy.guardian_manifest_authentication = Some(
            cipher
                .authenticate_scrollback_manifest(&canonical)
                .expect("authenticate v3 migration fixture")
                .encode(),
        );
        drop(keyring);
        legacy.manifest_sha256 = LiveScrollbackSpillSink::manifest_checksum(&legacy)
            .expect("checksum v3 migration fixture");
        overwrite_private_manifest_fixture(&sink.manifest_path, &legacy);
        drop(sink);

        let reopened = LiveScrollbackSpillSink::new(dir.path().to_path_buf(), &context)
            .expect("cold-open authenticated v3 fixture");
        let still_v3 = LiveScrollbackSpillSink::read_manifest(&reopened.manifest_path)
            .expect("reread cold-open v3 manifest")
            .expect("cold-open v3 manifest exists");
        assert_eq!(still_v3.schema, LIVE_SCROLLBACK_MANIFEST_SCHEMA_V3);
        assert!(reopened.store_scrollback_line(
            11,
            &Line::from_text("v4-successor", &attributes, 2, None),
            8,
        ));
        let migrated = LiveScrollbackSpillSink::read_manifest(&reopened.manifest_path)
            .expect("read migrated v4 manifest")
            .expect("migrated v4 manifest exists");
        assert_eq!(migrated.schema, LIVE_SCROLLBACK_MANIFEST_SCHEMA_V4);
        assert!(migrated.logical_ledger_sha256.is_none());
        assert!(migrated.chain_anchor_sha256.is_some());
        assert!(migrated.chain_tail_sha256.is_some());
    }

    fn write_private_stage_fixture(path: &std::path::Path, bytes: &[u8]) {
        let mut options = std::fs::OpenOptions::new();
        options.create_new(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;

            options.mode(0o600);
        }
        let mut file = options
            .open(path)
            .expect("create private stage fixture");
        file.write_all(bytes)
            .expect("write private stage fixture");
        file.sync_all()
            .expect("synchronize private stage fixture");
        #[cfg(not(windows))]
        std::fs::File::open(
            path.parent()
                .expect("stage fixture path has a parent"),
        )
        .expect("open stage fixture parent")
        .sync_all()
        .expect("synchronize stage fixture parent");
    }

    fn write_complete_append_wal_fixture(
        path: &std::path::Path,
        wal: &LiveScrollbackAppendWalV1,
    ) {
        let mut bytes = serde_json::to_vec_pretty(wal)
            .expect("serialize complete append WAL fixture");
        bytes.push(b'\n');
        write_private_stage_fixture(path, &bytes);
    }

    fn overwrite_private_append_wal_fixture(
        path: &std::path::Path,
        wal: &LiveScrollbackAppendWalV1,
    ) {
        let mut bytes = serde_json::to_vec_pretty(wal)
            .expect("serialize private append WAL fixture");
        bytes.push(b'\n');
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .truncate(true)
            .open(path)
            .expect("open private append WAL fixture for overwrite");
        file.write_all(&bytes)
            .expect("overwrite private append WAL fixture");
        file.sync_all()
            .expect("synchronize private append WAL fixture");
        #[cfg(not(windows))]
        std::fs::File::open(
            path.parent()
                .expect("append WAL fixture path has a parent"),
        )
        .expect("open append WAL fixture parent")
        .sync_all()
        .expect("synchronize append WAL fixture parent");
    }

    fn materialize_append_wal_target_for_test(
        sink: &LiveScrollbackSpillSink,
        wal: &LiveScrollbackAppendWalV1,
    ) -> LiveScrollbackSpillState {
        let predecessor = LiveScrollbackSpillSink::read_manifest(&sink.manifest_path)
            .expect("read append WAL predecessor manifest")
            .expect("append WAL predecessor manifest exists");
        let mut store = sink
            .lock_store("materialize append WAL test target")
            .expect("lock append WAL test store");
        let keyring = sink
            .lock_keyring("materialize append WAL test authentication")
            .expect("lock append WAL test keyring");
        LiveScrollbackSpillSink::reconcile_authenticated_append_wal(
            wal,
            &predecessor,
            &mut store,
            &keyring,
            sink.durable_pane_id,
        )
        .expect("materialize exact append WAL test target")
        .expect("predecessor manifest requires append WAL recovery")
    }

    fn overwrite_private_manifest_fixture(
        path: &std::path::Path,
        manifest: &LiveScrollbackManifestV1,
    ) {
        let mut bytes = serde_json::to_vec_pretty(manifest)
            .expect("serialize private manifest fixture");
        bytes.push(b'\n');
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .truncate(true)
            .open(path)
            .expect("open private manifest fixture for overwrite");
        file.write_all(&bytes)
            .expect("overwrite private manifest fixture");
        file.sync_all()
            .expect("synchronize private manifest fixture");
        #[cfg(not(windows))]
        std::fs::File::open(
            path.parent()
                .expect("manifest fixture path has a parent"),
        )
        .expect("open manifest fixture parent")
        .sync_all()
        .expect("synchronize manifest fixture parent");
    }

    #[test]
    fn live_scrollback_atomic_replacement_cas_reopen_and_empty_range_are_exact() {
        let dir = tempfile::tempdir().expect("temp scrollback dir");
        let context = config::ScrollbackSpillSinkContext {
            pane_id: 799,
            domain_id: 3,
            durable_pane_id: [79; 16],
            command_description: "atomic-replacement-shell".to_string(),
        };
        let attrs = CellAttributes::blank();
        let rows = vec![
            Line::from_text("replacement-row-zero", &attrs, 1, None),
            Line::from_text("replacement-row-one", &attrs, 2, None),
        ];
        let sink = LiveScrollbackSpillSink::new(dir.path().to_path_buf(), &context)
            .expect("create pristine replacement sink");
        let first_prefix = wezterm_term::config::ScrollbackPrefix::from_slices(
            Some(40),
            42,
            &rows,
            &[],
        )
        .expect("construct exact replacement prefix");
        let first = sink
            .replace_scrollback_prefix(None, first_prefix, 2)
            .expect("publish pristine replacement");
        assert_eq!(first.generation().revision(), 1);
        assert_eq!(first.oldest_stable_row(), Some(40));
        assert_eq!(first.newest_stable_row_exclusive(), 42);
        assert_ne!(sink.active_ledger_pane_id(), 0);
        assert_eq!(
            sink.load_scrollback_line(40)
                .expect("load first replacement row")
                .as_str()
                .as_ref(),
            "replacement-row-zero"
        );

        let successor_rows = vec![Line::from_text(
            "replacement-row-one-successor",
            &attrs,
            3,
            None,
        )];
        let successor_prefix = wezterm_term::config::ScrollbackPrefix::from_slices(
            Some(41),
            42,
            &successor_rows,
            &[],
        )
        .expect("construct successor replacement prefix");
        let successor = sink
            .replace_scrollback_prefix(Some(first.generation()), successor_prefix, 1)
            .expect("publish successor replacement");
        assert_eq!(
            successor.generation().content_epoch(),
            first.generation().content_epoch()
        );
        assert_eq!(successor.generation().revision(), 2);
        assert!(sink.load_scrollback_line(40).is_none());
        assert_eq!(sink.oldest_scrollback_row(), Some(41));

        let empty_prefix = wezterm_term::config::ScrollbackPrefix::from_slices(
            None,
            42,
            &[],
            &[],
        )
        .expect("construct atomic empty prefix");
        let empty = sink
            .replace_scrollback_prefix(Some(successor.generation()), empty_prefix, 0)
            .expect("publish atomic empty replacement");
        assert_eq!(empty.generation().revision(), 3);
        assert_eq!(empty.oldest_stable_row(), None);
        assert_eq!(empty.newest_stable_row_exclusive(), 42);
        assert_eq!(sink.retained_scrollback_rows(), 0);
        let none_retry = wezterm_term::config::ScrollbackPrefix::from_slices(
            None,
            42,
            &[],
            &[],
        )
        .expect("construct stale pristine retry");
        assert!(matches!(
            sink.replace_scrollback_prefix(None, none_retry, 0),
            Err(wezterm_term::config::ScrollbackSpillError::SnapshotGenerationMismatch)
        ));
        drop(sink);

        let reopened = LiveScrollbackSpillSink::new(dir.path().to_path_buf(), &context)
            .expect("reopen exact empty replacement");
        let snapshot = reopened
            .snapshot_scrollback(
                42,
                wezterm_term::config::ScrollbackSnapshotLimits {
                    max_rows: 1,
                    max_stored_bytes: 1024,
                    max_decoded_bytes: 1024,
                    max_physical_bytes: 1024,
                },
            )
            .expect("snapshot reopened exact empty replacement");
        assert_eq!(
            snapshot.fidelity(),
            wezterm_term::config::ScrollbackSnapshotFidelity::ExactSemantic
        );
        assert_eq!(snapshot.generation(), empty.generation());
        assert!(snapshot.rows().is_empty());

        let durable_pane_id = uuid::Uuid::from_bytes(context.durable_pane_id)
            .simple()
            .to_string();
        let identity = read_live_scrollback_committed_ledger_identity(
            dir.path(),
            &durable_pane_id,
        )
        .expect("read authenticated committed ledger identity");
        assert_eq!(identity.generation(), empty.generation());
        assert_eq!(identity.predecessor(), Some(successor.generation()));
        assert_eq!(identity.oldest_sequence(), None);
        assert_eq!(identity.next_sequence(), 0);
        assert_eq!(identity.oldest_stable_row(), None);
        assert_eq!(identity.newest_stable_row_exclusive(), 42);
        assert_eq!(identity.record_count(), 0);
        assert!(!format!("{identity:?}").contains(&hex::encode(identity.manifest_digest())));
        assert!(!format!("{identity:?}").contains(&hex::encode(identity.logical_ledger_digest())));
    }

    #[test]
    fn authenticated_incremental_chain_rejects_reorder_duplication_and_omission() {
        let dir = tempfile::tempdir().expect("temp logical-ledger digest dir");
        let context = config::ScrollbackSpillSinkContext {
            pane_id: 797,
            domain_id: 3,
            durable_pane_id: [77; 16],
            command_description: "logical-ledger-order-shell".to_string(),
        };
        let sink = LiveScrollbackSpillSink::new(dir.path().to_path_buf(), &context)
            .expect("create logical-ledger digest sink");
        let attrs = CellAttributes::blank();
        for (stable_row, text) in [
            (30_isize, "order-row-a"),
            (31_isize, "order-row-b"),
            (32_isize, "order-row-c"),
        ] {
            assert!(sink.store_scrollback_line(
                stable_row,
                &Line::from_text(
                    text,
                    &attrs,
                    u64::try_from(stable_row).expect("fixture stable row fits u64"),
                    None,
                ),
                8,
            ));
        }
        let manifest = LiveScrollbackSpillSink::read_manifest(&sink.manifest_path)
            .expect("read logical-ledger manifest")
            .expect("logical-ledger manifest exists");
        let ledger_pane_id = LiveScrollbackSpillSink::manifest_ledger_pane_id(&manifest)
            .expect("decode logical-ledger pointer");
        let store = sink
            .lock_store("test logical-ledger canonical order")
            .expect("test logical-ledger store lock");
        let records = (0..3)
            .map(|sequence| {
                store
                    .line_at(ledger_pane_id, sequence)
                    .expect("read exact logical-ledger row")
                    .expect("exact logical-ledger row exists")
            })
            .collect::<Vec<_>>();
        drop(store);
        assert_eq!(records[0].len(), records[1].len());
        assert_eq!(records[1].len(), records[2].len());
        let (anchor, expected) = expected_live_scrollback_v4_chain(&manifest)
            .expect("decode authenticated incremental chain");
        let digest_for = |candidate_records: &[String]| -> anyhow::Result<[u8; 32]> {
            anyhow::ensure!(
                u64::try_from(candidate_records.len())? == manifest.retained_rows,
                "candidate record count disagrees with v4 manifest"
            );
            let oldest = manifest
                .oldest_seq
                .ok_or_else(|| anyhow::anyhow!("v4 test ledger has no oldest sequence"))?;
            let mut tail = anchor;
            for (offset, record) in candidate_records.iter().enumerate() {
                let sequence = oldest
                    .checked_add(u64::try_from(offset)?)
                    .ok_or_else(|| anyhow::anyhow!("v4 test sequence overflows"))?;
                tail = live_scrollback_incremental_chain_next(
                    tail,
                    ledger_pane_id,
                    sequence,
                    record,
                )?;
            }
            Ok(tail)
        };
        assert_eq!(digest_for(&records).expect("hash canonical ledger"), expected);

        let mut reordered = records.clone();
        reordered.swap(0, 2);
        assert_ne!(
            digest_for(&reordered).expect("hash reordered same-length ledger"),
            expected
        );
        let mut duplicated = records.clone();
        duplicated[1] = duplicated[0].clone();
        assert_ne!(
            digest_for(&duplicated).expect("hash duplicated same-length ledger"),
            expected
        );
        assert!(
            digest_for(&records[..2]).is_err(),
            "an omitted record must not satisfy the manifest record-count identity"
        );
    }

    #[test]
    fn filesystem_mutation_lease_rejects_a_stale_second_sink_cas() {
        let dir = tempfile::tempdir().expect("temp cross-process CAS dir");
        let context = config::ScrollbackSpillSinkContext {
            pane_id: 794,
            domain_id: 3,
            durable_pane_id: [74; 16],
            command_description: "cross-process-cas-shell".to_string(),
        };
        let first_sink = LiveScrollbackSpillSink::new(dir.path().to_path_buf(), &context)
            .expect("create first CAS sink");
        let stale_sink = LiveScrollbackSpillSink::new(dir.path().to_path_buf(), &context)
            .expect("create second sink before publication");
        let rows = vec![Line::from_text(
            "authoritative-row",
            &CellAttributes::blank(),
            1,
            None,
        )];
        let authoritative_prefix = wezterm_term::config::ScrollbackPrefix::from_slices(
            Some(70),
            71,
            &rows,
            &[],
        )
        .expect("construct authoritative CAS prefix");
        first_sink
            .replace_scrollback_prefix(None, authoritative_prefix, 1)
            .expect("publish authoritative CAS generation");

        let stale_prefix = wezterm_term::config::ScrollbackPrefix::from_slices(
            Some(70),
            71,
            &rows,
            &[],
        )
        .expect("construct stale CAS prefix");
        assert!(matches!(
            stale_sink.replace_scrollback_prefix(None, stale_prefix, 1),
            Err(wezterm_term::config::ScrollbackSpillError::SnapshotGenerationMismatch)
        ));
        assert!(
            !stale_sink.store_scrollback_line(70, &rows[0], 1),
            "a stale ordinary writer must not overwrite the newer authenticated manifest"
        );
        assert_eq!(
            first_sink
                .load_scrollback_line(70)
                .expect("authoritative row survives stale writer rejection")
                .as_str()
                .as_ref(),
            "authoritative-row"
        );
        drop(stale_sink);
        drop(first_sink);
        let durable_pane_id = uuid::Uuid::from_bytes(context.durable_pane_id)
            .simple()
            .to_string();
        let lock_path = dir
            .path()
            .join(durable_pane_id)
            .join(LIVE_SCROLLBACK_MUTATION_LOCK_NAME);
        std::fs::write(&lock_path, b"nonzero-lock-content")
            .expect("persist invalid lock-authority fixture");
        let lock_error = LiveScrollbackSpillSink::new(dir.path().to_path_buf(), &context)
            .expect_err("nonempty filesystem mutation authority must fail closed");
        assert!(format!("{lock_error:#}").contains("empty regular file"));
    }

    #[test]
    fn archived_valid_revision_splice_fails_the_authenticated_logical_ledger_digest() {
        use std::io::{Seek as _, SeekFrom, Write as _};

        let dir = tempfile::tempdir().expect("temp archived-revision splice dir");
        let context = config::ScrollbackSpillSinkContext {
            pane_id: 796,
            domain_id: 3,
            durable_pane_id: [76; 16],
            command_description: "archived-revision-splice-shell".to_string(),
        };
        let durable_pane_id = uuid::Uuid::from_bytes(context.durable_pane_id)
            .simple()
            .to_string();
        let sink = LiveScrollbackSpillSink::new(dir.path().to_path_buf(), &context)
            .expect("create archived-revision splice sink");
        let row = vec![Line::from_text(
            "same-semantic-row",
            &CellAttributes::blank(),
            1,
            None,
        )];
        let first_prefix = wezterm_term::config::ScrollbackPrefix::from_slices(
            Some(50),
            51,
            &row,
            &[],
        )
        .expect("construct first archived-revision prefix");
        let first = sink
            .replace_scrollback_prefix(None, first_prefix, 1)
            .expect("publish first archived-revision ledger");
        let first_ledger = sink.active_ledger_pane_id();
        let first_record = sink
            .lock_store("read first archived revision")
            .expect("first archived-revision store lock")
            .line_at(first_ledger, 0)
            .expect("read first archived revision")
            .expect("first archived revision exists");

        let successor_prefix = wezterm_term::config::ScrollbackPrefix::from_slices(
            Some(50),
            51,
            &row,
            &[],
        )
        .expect("construct successor archived-revision prefix");
        let successor = sink
            .replace_scrollback_prefix(Some(first.generation()), successor_prefix, 1)
            .expect("publish successor archived-revision ledger");
        let successor_ledger = sink.active_ledger_pane_id();
        assert_ne!(successor_ledger, first_ledger);
        let successor_record = sink
            .lock_store("read successor archived revision")
            .expect("successor archived-revision store lock")
            .line_at(successor_ledger, 0)
            .expect("read successor archived revision")
            .expect("successor archived revision exists");
        assert_eq!(first_record.len(), successor_record.len());
        let first_envelope =
            mux::guardian_output_journal::GuardianEncryptedScrollbackRow::parse(&first_record)
                .expect("parse first archived revision");
        let successor_envelope =
            mux::guardian_output_journal::GuardianEncryptedScrollbackRow::parse(&successor_record)
                .expect("parse successor archived revision");
        assert_eq!(first_envelope.identity().revision(), 1);
        assert_eq!(successor_envelope.identity().revision(), 2);
        assert_eq!(successor.generation().revision(), 2);

        let successor_path = dir
            .path()
            .join(&durable_pane_id)
            .join(format!("{successor_ledger}.log"));
        let mut successor_file = std::fs::OpenOptions::new()
            .write(true)
            .open(&successor_path)
            .expect("open successor ledger for same-length archived splice");
        successor_file
            .seek(SeekFrom::Start(0))
            .expect("seek successor ledger");
        successor_file
            .write_all(first_record.as_bytes())
            .and_then(|()| successor_file.write_all(b"\n"))
            .expect("splice old valid revision at the same durable location");
        successor_file
            .sync_all()
            .expect("synchronize archived-revision splice");
        drop(successor_file);

        let export_error = export_live_scrollback_transcript(
            dir.path(),
            &durable_pane_id,
            8,
            1024 * 1024,
            1024 * 1024,
        )
        .expect_err("export must reject a valid older-revision splice");
        assert!(format!("{export_error:#}").contains("logical ledger"));
        let listed = list_live_scrollback_panes(dir.path(), 8)
            .expect("listing reports corrupt authority without following it");
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].state, "corrupt");
        assert!(listed[0]
            .error
            .as_deref()
            .is_some_and(|error| error.contains("logical ledger")));
        drop(sink);

        let reopen_error = LiveScrollbackSpillSink::new(dir.path().to_path_buf(), &context)
            .expect_err("reopen must reject a valid older-revision splice");
        assert!(format!("{reopen_error:#}").contains("logical ledger"));
    }

    #[test]
    fn recomputed_public_manifest_checksum_cannot_replace_guardian_authentication() {
        let dir = tempfile::tempdir().expect("temp recomputed-manifest-checksum dir");
        let context = config::ScrollbackSpillSinkContext {
            pane_id: 795,
            domain_id: 3,
            durable_pane_id: [75; 16],
            command_description: "guardian-authenticated-shell".to_string(),
        };
        let manifest_path = {
            let sink = LiveScrollbackSpillSink::new(dir.path().to_path_buf(), &context)
                .expect("create guardian-authenticated manifest sink");
            assert!(sink.store_scrollback_line(
                0,
                &Line::from_text("guardian-authenticated-row", &CellAttributes::blank(), 1, None),
                8,
            ));
            sink.manifest_path.clone()
        };
        let mut manifest = LiveScrollbackSpillSink::read_manifest(&manifest_path)
            .expect("read guardian-authenticated manifest")
            .expect("guardian-authenticated manifest exists");
        let original_authentication = manifest.guardian_manifest_authentication.clone();
        manifest.command_description = "attacker-recomputed-shell".to_string();
        manifest.manifest_sha256 = LiveScrollbackSpillSink::manifest_checksum(&manifest)
            .expect("recompute public manifest checksum");
        let mut bytes = serde_json::to_vec_pretty(&manifest)
            .expect("encode recomputed public-checksum manifest");
        bytes.push(b'\n');
        std::fs::write(&manifest_path, bytes)
            .expect("persist recomputed public-checksum manifest fixture");
        let checksum_valid = LiveScrollbackSpillSink::read_manifest(&manifest_path)
            .expect("public checksum must be internally consistent")
            .expect("recomputed public-checksum manifest exists");
        assert_eq!(
            checksum_valid.guardian_manifest_authentication,
            original_authentication,
            "the attacker did not possess guardian authentication authority"
        );

        let error = LiveScrollbackSpillSink::new(dir.path().to_path_buf(), &context)
            .expect_err("stale guardian authentication must reject a recomputed public checksum");
        assert!(format!("{error:#}").contains("authentication"));
    }

    #[test]
    fn repeated_manifest_publication_failure_reuses_one_deterministic_stage() {
        let dir = tempfile::tempdir().expect("temp scrollback dir");
        let context = config::ScrollbackSpillSinkContext {
            pane_id: 798,
            domain_id: 3,
            durable_pane_id: [78; 16],
            command_description: "bounded-manifest-stage-shell".to_string(),
        };
        let sink = LiveScrollbackSpillSink::new(dir.path().to_path_buf(), &context)
            .expect("create bounded-stage sink");
        std::fs::create_dir(&sink.manifest_path)
            .expect("block manifest rename with a destination directory");
        let line = Line::from_text("never-published", &CellAttributes::blank(), 1, None);
        assert!(!sink.store_scrollback_line(0, &line, 8));
        assert!(!sink.store_scrollback_line(0, &line, 8));
        let parent = sink
            .manifest_path
            .parent()
            .expect("manifest has parent directory");
        let installing = std::fs::read_dir(parent)
            .expect("read pane directory")
            .filter_map(Result::ok)
            .filter(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with("manifest.json.installing-")
            })
            .count();
        assert_eq!(installing, 1, "retries must reuse one bounded stage slot");
        std::fs::rename(
            &sink.manifest_path,
            parent.join("retained-rename-blocker"),
        )
        .expect("move the test blocker without deleting crash evidence");
        assert!(
            sink.store_scrollback_line(0, &line, 8),
            "the synchronized deterministic stage must remain publishable after the blocker clears"
        );
        assert_eq!(
            sink.load_scrollback_line(0)
                .expect("row committed through reused deterministic stage")
                .as_str()
                .as_ref(),
            "never-published"
        );
    }

    #[test]
    fn reopen_publishes_only_the_authenticated_same_ledger_complete_stage() {
        let dir = tempfile::tempdir().expect("temp complete-stage recovery dir");
        let context = config::ScrollbackSpillSinkContext {
            pane_id: 793,
            domain_id: 3,
            durable_pane_id: [73; 16],
            command_description: "complete-stage-recovery-shell".to_string(),
        };
        let attrs = CellAttributes::blank();
        {
            let sink = LiveScrollbackSpillSink::new(dir.path().to_path_buf(), &context)
                .expect("create complete-stage recovery sink");
            assert!(sink.store_scrollback_line(
                10,
                &Line::from_text("published-before-stage", &attrs, 1, None),
                8,
            ));
            let previous_state = *sink
                .lock_state("test complete-stage previous state")
                .expect("test complete-stage state lock");
            let mut proposed_state = previous_state;
            proposed_state
                .advance_revision()
                .expect("advance staged complete revision");
            proposed_state.newest_stable_row_exclusive = Some(12);
            proposed_state.max_retained_rows = 8;
            let second = Line::from_text("durable-complete-stage", &attrs, 2, None);
            let row_identity =
                mux::guardian_output_journal::GuardianScrollbackRowIdentity::new(
                    context.durable_pane_id,
                    proposed_state.content_epoch,
                    proposed_state.revision,
                    11,
                    1,
                )
                .expect("construct staged complete row identity");
            let cipher = sink
                .lock_keyring("test complete-stage active key")
                .expect("test complete-stage keyring lock")
                .latest_active_cipher()
                .expect("load complete-stage active key");
            let record = encode_exact_scrollback_line_record(&second, &cipher, row_identity)
                .expect("seal complete-stage exact row");
            assert_eq!(
                sink.lock_store("test complete-stage append")
                    .expect("test complete-stage store lock")
                    .append_line(sink.active_ledger_pane_id(), &record)
                    .expect("append durable complete-stage row"),
                1
            );
            *sink
                .lock_state("test complete-stage publish state")
                .expect("test complete-stage state lock") = proposed_state;

            let parent = sink
                .manifest_path
                .parent()
                .expect("complete-stage manifest has parent");
            let published_backup = parent.join("manifest-before-complete-stage");
            std::fs::rename(&sink.manifest_path, &published_backup)
                .expect("retain published predecessor manifest");
            std::fs::create_dir(&sink.manifest_path)
                .expect("block complete-stage manifest rename");
            let error = sink
                .persist_manifest("complete")
                .expect_err("complete-stage publication must fail before rename");
            assert!(!error.outcome_indeterminate());
            std::fs::rename(
                &sink.manifest_path,
                parent.join("retained-complete-stage-blocker"),
            )
            .expect("retain complete-stage blocker without deleting it");
            std::fs::rename(&published_backup, &sink.manifest_path)
                .expect("restore published predecessor manifest");
        }

        let reopened = LiveScrollbackSpillSink::new(dir.path().to_path_buf(), &context)
            .expect("recover authenticated same-ledger complete stage");
        assert_eq!(
            reopened
                .load_scrollback_line(11)
                .expect("recovered complete-stage row")
                .as_str()
                .as_ref(),
            "durable-complete-stage"
        );
        let manifest = LiveScrollbackSpillSink::read_manifest(&reopened.manifest_path)
            .expect("read recovered complete-stage manifest")
            .expect("recovered complete-stage manifest exists");
        assert_eq!(manifest.publication_state, "complete");
        assert_eq!(manifest.revision, Some(2));
        assert_eq!(manifest.retained_rows, 2);
        assert!(
            !LiveScrollbackSpillSink::deterministic_manifest_stage_path(
                &reopened.manifest_path,
            )
            .expect("derive deterministic stage path")
            .exists(),
            "the exact recovered stage must become the one published manifest"
        );
    }

    #[test]
    fn live_scrollback_spill_sink_hydrates_exact_rows_without_plaintext_on_disk() {
        let dir = tempfile::tempdir().expect("temp scrollback dir");
        let context = config::ScrollbackSpillSinkContext {
            pane_id: 7,
            domain_id: 3,
            durable_pane_id: [7; 16],
            command_description: "test-shell".to_string(),
        };
        let sink = LiveScrollbackSpillSink::new(dir.path().to_path_buf(), &context)
            .expect("create live spill sink");
        let attrs = CellAttributes::blank();

        for row in 0..4 {
            let text = if row == 2 {
                "line-2 sk-abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMN".to_string()
            } else {
                format!("line-{row}")
            };
            let line = Line::from_text(&text, &attrs, row, None);
            assert!(sink.store_scrollback_line(row as wezterm_term::StableRowIndex, &line, 2));
        }

        assert_eq!(sink.retained_scrollback_rows(), 2);
        assert_eq!(
            sink.physical_scrollback_bytes(),
            sink.retained_scrollback_bytes() as u64,
            "test-threshold compaction should rewrite the stale prefix away"
        );
        assert_eq!(sink.oldest_scrollback_row(), Some(2));
        assert!(sink.load_scrollback_line(1).is_none());
        let hydrated = sink
            .load_scrollback_line(2)
            .expect("retained row should hydrate");
        let hydrated_text = hydrated.as_str();
        assert!(
            hydrated_text.contains("sk-abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMN"),
            "recovery hydration must retain exact unredacted semantic content"
        );
        let durable_id = uuid::Uuid::from_bytes(context.durable_pane_id)
            .simple()
            .to_string();
        let log_bytes = std::fs::read(dir.path().join(durable_id).join("0.log"))
            .expect("read encrypted scrollback log");
        assert!(!log_bytes
            .windows(b"sk-abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMN".len())
            .any(|window| window == b"sk-abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMN"));
        assert!(!format!("{sink:?}").contains("abcdefghijklmnopqrstuvwxyz"));
    }

    #[test]
    fn live_scrollback_spill_sink_preserves_styled_line_records() {
        let dir = tempfile::tempdir().expect("temp scrollback dir");
        let context = config::ScrollbackSpillSinkContext {
            pane_id: 8,
            domain_id: 3,
            durable_pane_id: [8; 16],
            command_description: "styled-shell".to_string(),
        };
        let sink = LiveScrollbackSpillSink::new(dir.path().to_path_buf(), &context)
            .expect("create live spill sink");
        let mut attrs = CellAttributes::blank();
        attrs.set_italic(true);
        attrs.set_hyperlink(Some(Arc::new(termwiz::cell::Hyperlink::new_with_id(
            "https://checkpoint.example/row",
            "exact-link",
        ))));
        let mut line = Line::from_text("A styled-row", &attrs, 42, None);
        line.set_cell_grapheme(0, "A", 2, attrs.clone(), 42);
        let source_text = line.as_str().into_owned();

        assert!(sink.store_scrollback_line(0, &line, 4));

        let hydrated = sink
            .load_scrollback_line(0)
            .expect("styled row should hydrate");
        assert_eq!(hydrated.as_str().as_ref(), source_text);
        assert_eq!(hydrated.current_seqno(), 42);
        assert!(
            hydrated.visible_cells().all(|cell| cell.attrs().italic()),
            "serialized cold row should preserve cell attributes"
        );
        let first = hydrated
            .visible_cells()
            .next()
            .expect("explicit-width linked cell survives hydration");
        assert_eq!(first.width(), 2);
        assert_eq!(first.str(), "A");
        assert_eq!(
            first.attrs().hyperlink().map(|link| link.uri()),
            Some("https://checkpoint.example/row")
        );
        assert_eq!(
            varbincode::serialize(&hydrated).expect("serialize hydrated semantic row"),
            varbincode::serialize(&line).expect("serialize source semantic row")
        );
    }

    #[test]
    fn live_scrollback_spill_sink_reopens_compacted_rows_with_stable_identity() {
        let dir = tempfile::tempdir().expect("temp scrollback dir");
        let context = config::ScrollbackSpillSinkContext {
            pane_id: 81,
            domain_id: 3,
            durable_pane_id: [81; 16],
            command_description: "restartable-shell".to_string(),
        };
        let attrs = CellAttributes::blank();
        {
            let sink = LiveScrollbackSpillSink::new(dir.path().to_path_buf(), &context)
                .expect("create live spill sink");
            for stable_row in 10..15 {
                let line = Line::from_text(
                    &format!("stable-row-{stable_row}"),
                    &attrs,
                    stable_row,
                    None,
                );
                let stable_row = isize::try_from(stable_row).expect("stable row fits isize");
                assert!(sink.store_scrollback_line(stable_row, &line, 2));
            }
            assert_eq!(sink.oldest_scrollback_row(), Some(13));
        }

        let reopened = LiveScrollbackSpillSink::new(dir.path().to_path_buf(), &context)
            .expect("reopen live spill sink");
        assert_eq!(reopened.oldest_scrollback_row(), Some(13));
        assert_eq!(
            reopened
                .load_scrollback_line(13)
                .expect("reopened oldest row")
                .as_str()
                .as_ref(),
            "stable-row-13"
        );
        assert_eq!(
            reopened
                .load_scrollback_line(14)
                .expect("reopened newest row")
                .as_str()
                .as_ref(),
            "stable-row-14"
        );
    }

    #[test]
    fn live_scrollback_sinks_share_rotation_authority_without_reconstruction() {
        let dir = tempfile::tempdir().expect("temp scrollback dir");
        let first_context = config::ScrollbackSpillSinkContext {
            pane_id: 821,
            domain_id: 3,
            durable_pane_id: [91; 16],
            command_description: "first-live-rotation-shell".to_string(),
        };
        let second_context = config::ScrollbackSpillSinkContext {
            pane_id: 822,
            domain_id: 4,
            durable_pane_id: [92; 16],
            command_description: "second-live-rotation-shell".to_string(),
        };
        let first = LiveScrollbackSpillSink::new(dir.path().to_path_buf(), &first_context)
            .expect("create first live spill sink");
        let second = LiveScrollbackSpillSink::new(dir.path().to_path_buf(), &second_context)
            .expect("create second live spill sink");
        assert!(
            Arc::ptr_eq(&first.keyring, &second.keyring),
            "all panes under one scrollback authority must share one live keyring"
        );

        let attrs = CellAttributes::blank();
        assert!(second.store_scrollback_line(
            30,
            &Line::from_text("second-pane-before-rotation", &attrs, 1, None),
            8,
        ));
        let previous_key = first
            .lock_keyring("test shared live key rotation")
            .expect("lock first sink keyring")
            .active_key_id();
        let rotated_key = first
            .lock_keyring("test shared live key rotation")
            .expect("lock first sink keyring")
            .rotate()
            .expect("rotate shared live keyring");
        assert_ne!(previous_key, rotated_key);

        assert!(second.store_scrollback_line(
            31,
            &Line::from_text("second-pane-after-rotation", &attrs, 2, None),
            8,
        ));
        assert_eq!(
            second
                .load_scrollback_line(30)
                .expect("historical second-pane row")
                .as_str()
                .as_ref(),
            "second-pane-before-rotation"
        );
        assert_eq!(
            second
                .load_scrollback_line(31)
                .expect("rotated second-pane row")
                .as_str()
                .as_ref(),
            "second-pane-after-rotation"
        );
    }

    #[test]
    fn live_scrollback_reopens_rows_across_guardian_key_rotation() {
        let dir = tempfile::tempdir().expect("temp scrollback dir");
        let context = config::ScrollbackSpillSinkContext {
            pane_id: 811,
            domain_id: 3,
            durable_pane_id: [80; 16],
            command_description: "rotated-key-shell".to_string(),
        };
        {
            let sink = LiveScrollbackSpillSink::new(dir.path().to_path_buf(), &context)
                .expect("create live spill sink");
            let attrs = CellAttributes::blank();
            assert!(sink.store_scrollback_line(
                10,
                &Line::from_text("before-key-rotation", &attrs, 1, None),
                8,
            ));
            let original_key = sink
                .lock_keyring("test key rotation")
                .expect("lock shared guardian keyring")
                .active_key_id();
            let rotated_key = sink
                .lock_keyring("test key rotation")
                .expect("lock shared guardian keyring")
                .rotate()
                .expect("rotate shared guardian keyring");
            assert_ne!(original_key, rotated_key);
            assert!(sink.store_scrollback_line(
                11,
                &Line::from_text("after-key-rotation", &attrs, 2, None),
                8,
            ));
        }

        let reopened = LiveScrollbackSpillSink::new(dir.path().to_path_buf(), &context)
            .expect("reopen spill sink after key rotation");
        assert_eq!(
            reopened
                .load_scrollback_line(10)
                .expect("historical-key row survives reopen")
                .as_str()
                .as_ref(),
            "before-key-rotation"
        );
        assert_eq!(
            reopened
                .load_scrollback_line(11)
                .expect("active-key row survives reopen")
                .as_str()
                .as_ref(),
            "after-key-rotation"
        );
    }

    #[test]
    fn live_scrollback_spill_sink_repairs_interrupted_prepared_manifest() {
        let dir = tempfile::tempdir().expect("temp scrollback dir");
        let context = config::ScrollbackSpillSinkContext {
            pane_id: 812,
            domain_id: 3,
            durable_pane_id: [86; 16],
            command_description: "prepared-recovery-shell".to_string(),
        };
        {
            let sink = LiveScrollbackSpillSink::new(dir.path().to_path_buf(), &context)
                .expect("create live spill sink");
            let line = Line::from_text("durable-before-manifest", &CellAttributes::blank(), 1, None);
            assert!(sink.store_scrollback_line(10, &line, 8));
            sink.persist_manifest("prepared")
                .expect("simulate interrupted complete publication");
        }

        let reopened = LiveScrollbackSpillSink::new(dir.path().to_path_buf(), &context)
            .expect("repair prepared manifest on reopen");
        assert_eq!(
            reopened
                .load_scrollback_line(10)
                .expect("durable row survives interrupted publication")
                .as_str()
                .as_ref(),
            "durable-before-manifest"
        );
        let manifest = LiveScrollbackSpillSink::read_manifest(&reopened.manifest_path)
            .expect("read repaired manifest")
            .expect("repaired manifest exists");
        assert_eq!(manifest.publication_state, "complete");
        assert_eq!(manifest.oldest_seq, Some(0));
        assert_eq!(manifest.retained_rows, 1);
        assert_eq!(manifest.next_seq, 1);
    }

    #[derive(Clone, Copy, Debug)]
    enum AppendWalCrashCut {
        CompleteStageBeforeRename,
        ActiveBeforeRow,
        ActiveAfterRowBeforeRetention,
        ActiveAfterRetentionBeforeManifest,
        IncompleteManifestStageAfterRetention,
        CompleteManifestStageBeforeRename,
        CompleteManifestBeforeWalRetirement,
    }

    #[derive(Clone, Copy, Debug)]
    enum LegacyV1AppendWalCrashCut {
        ActiveBeforeRow,
        ActiveAfterRowBeforeRetention,
        ActiveAfterRetentionBeforeManifest,
    }

    #[test]
    fn authenticated_append_wal_recovers_every_durable_later_row_crash_cut() {
        let cuts = [
            AppendWalCrashCut::CompleteStageBeforeRename,
            AppendWalCrashCut::ActiveBeforeRow,
            AppendWalCrashCut::ActiveAfterRowBeforeRetention,
            AppendWalCrashCut::ActiveAfterRetentionBeforeManifest,
            AppendWalCrashCut::IncompleteManifestStageAfterRetention,
            AppendWalCrashCut::CompleteManifestStageBeforeRename,
            AppendWalCrashCut::CompleteManifestBeforeWalRetirement,
        ];
        for (index, cut) in cuts.into_iter().enumerate() {
            let identity_byte = u8::try_from(120 + index).expect("fixture identity fits u8");
            let (dir, context, sink, wal, appended) = append_wal_fixture(identity_byte, 1);
            let ledger_pane_id = sink.active_ledger_pane_id();
            match cut {
                AppendWalCrashCut::CompleteStageBeforeRename => {
                    let stage_path = LiveScrollbackSpillSink::append_wal_stage_path(
                        &sink.manifest_path,
                    )
                    .expect("derive complete append WAL stage path");
                    write_complete_append_wal_fixture(&stage_path, &wal);
                }
                AppendWalCrashCut::ActiveBeforeRow => sink
                    .persist_authenticated_append_wal(&wal)
                    .expect("publish active append WAL before row"),
                AppendWalCrashCut::ActiveAfterRowBeforeRetention => {
                    sink.persist_authenticated_append_wal(&wal)
                        .expect("publish active append WAL before row");
                    assert_eq!(
                        sink.lock_store("append WAL row-before-retention cut")
                            .expect("lock row-before-retention store")
                            .append_line(ledger_pane_id, &wal.encrypted_record)
                            .expect("synchronize row-before-retention cut"),
                        wal.appended_sequence
                    );
                }
                AppendWalCrashCut::ActiveAfterRetentionBeforeManifest => {
                    sink.persist_authenticated_append_wal(&wal)
                        .expect("publish active append WAL before target ledger");
                    let mut store = sink
                        .lock_store("append WAL post-retention cut")
                        .expect("lock post-retention store");
                    assert_eq!(
                        store
                            .append_line(ledger_pane_id, &wal.encrypted_record)
                            .expect("synchronize post-retention row"),
                        wal.appended_sequence
                    );
                    store
                        .prune_before(ledger_pane_id, wal.target_oldest_sequence)
                        .expect("synchronize append WAL retention cut");
                    LiveScrollbackSpillSink::verify_append_wal_target_store(&wal, &store)
                        .expect("retained append WAL target is exact");
                }
                AppendWalCrashCut::IncompleteManifestStageAfterRetention => {
                    sink.persist_authenticated_append_wal(&wal)
                        .expect("publish append WAL before incomplete manifest stage");
                    let recovered_state = materialize_append_wal_target_for_test(&sink, &wal);
                    *sink
                        .lock_state("incomplete manifest-stage test state")
                        .expect("lock incomplete manifest-stage state") = recovered_state;
                    let stage_path = LiveScrollbackSpillSink::deterministic_manifest_stage_path(
                        &sink.manifest_path,
                    )
                    .expect("derive incomplete manifest-stage path");
                    write_private_stage_fixture(&stage_path, br#"{"schema":"#);
                }
                AppendWalCrashCut::CompleteManifestStageBeforeRename => {
                    let predecessor = LiveScrollbackSpillSink::read_manifest(&sink.manifest_path)
                        .expect("read complete-stage predecessor")
                        .expect("complete-stage predecessor exists");
                    sink.persist_authenticated_append_wal(&wal)
                        .expect("publish append WAL before complete manifest stage");
                    let recovered_state = materialize_append_wal_target_for_test(&sink, &wal);
                    *sink
                        .lock_state("complete manifest-stage test state")
                        .expect("lock complete manifest-stage state") = recovered_state;
                    sink.persist_manifest("complete")
                        .expect("construct authenticated complete manifest target");
                    let target = LiveScrollbackSpillSink::read_manifest(&sink.manifest_path)
                        .expect("read complete manifest target")
                        .expect("complete manifest target exists");
                    overwrite_private_manifest_fixture(&sink.manifest_path, &predecessor);
                    let stage_path = LiveScrollbackSpillSink::deterministic_manifest_stage_path(
                        &sink.manifest_path,
                    )
                    .expect("derive complete manifest-stage path");
                    let mut target_bytes = serde_json::to_vec_pretty(&target)
                        .expect("serialize complete manifest-stage target");
                    target_bytes.push(b'\n');
                    write_private_stage_fixture(&stage_path, &target_bytes);
                }
                AppendWalCrashCut::CompleteManifestBeforeWalRetirement => {
                    sink.persist_authenticated_append_wal(&wal)
                        .expect("publish append WAL before complete manifest");
                    let recovered_state = materialize_append_wal_target_for_test(&sink, &wal);
                    *sink
                        .lock_state("append WAL complete-manifest state")
                        .expect("lock complete-manifest state") = recovered_state;
                    sink.persist_manifest("complete")
                        .expect("publish complete manifest while retaining consumed WAL");
                }
            }
            drop(sink);

            let reopened = LiveScrollbackSpillSink::new(dir.path().to_path_buf(), &context)
                .unwrap_or_else(|error| panic!("recover {cut:?}: {error:#}"));
            assert_eq!(
                reopened
                    .load_scrollback_line(11)
                    .unwrap_or_else(|| panic!("{cut:?} must recover the exact appended row"))
                    .as_str()
                    .as_ref(),
                appended.as_str().as_ref(),
                "{cut:?} changed the recovered semantic row"
            );
            assert!(
                reopened.load_scrollback_line(10).is_none(),
                "{cut:?} did not finish the authenticated retention cut"
            );
            assert_eq!(reopened.retained_scrollback_rows(), 1);
            let manifest = LiveScrollbackSpillSink::read_manifest(&reopened.manifest_path)
                .expect("read recovered append WAL manifest")
                .expect("recovered append WAL manifest exists");
            assert_eq!(manifest.publication_state, "complete");
            assert_eq!(manifest.revision, Some(wal.target_revision));
            assert_eq!(manifest.oldest_seq, Some(wal.target_oldest_sequence));
            assert_eq!(manifest.next_seq, wal.target_next_sequence);
            assert_eq!(manifest.retained_rows, wal.target_record_count);
            assert!(
                std::fs::symlink_metadata(
                    LiveScrollbackSpillSink::deterministic_manifest_stage_path(
                        &reopened.manifest_path,
                    )
                    .expect("derive recovered manifest stage path")
                )
                .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound),
                "{cut:?} left an unfinalized manifest stage"
            );
            assert!(
                std::fs::symlink_metadata(
                    LiveScrollbackSpillSink::append_wal_stage_path(&reopened.manifest_path)
                        .expect("derive recovered append WAL stage path")
                )
                .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound),
                "{cut:?} left an unfinalized append WAL stage"
            );
            assert_eq!(
                reopened
                    .lock_store("verify append WAL reopen range")
                    .expect("lock recovered append WAL store")
                    .next_seq(reopened.active_ledger_pane_id())
                    .expect("read recovered append WAL next sequence"),
                wal.target_next_sequence,
                "{cut:?} duplicated or omitted the authenticated append"
            );
        }
    }

    #[test]
    fn legacy_v1_append_wal_recovers_to_v4_across_crash_cuts_and_cold_reopens() {
        let cuts = [
            LegacyV1AppendWalCrashCut::ActiveBeforeRow,
            LegacyV1AppendWalCrashCut::ActiveAfterRowBeforeRetention,
            LegacyV1AppendWalCrashCut::ActiveAfterRetentionBeforeManifest,
        ];
        for (index, cut) in cuts.into_iter().enumerate() {
            let identity_byte = u8::try_from(220 + index).expect("fixture identity fits u8");
            let (dir, context, sink, wal, appended) =
                legacy_v1_append_wal_fixture(identity_byte);
            let ledger_pane_id = sink.active_ledger_pane_id();
            sink.persist_authenticated_append_wal(&wal)
                .expect("publish legacy v1 append WAL");
            match cut {
                LegacyV1AppendWalCrashCut::ActiveBeforeRow => {}
                LegacyV1AppendWalCrashCut::ActiveAfterRowBeforeRetention => {
                    assert_eq!(
                        sink.lock_store("v1 WAL row-before-retention cut")
                            .expect("lock v1 WAL row-before-retention store")
                            .append_line(ledger_pane_id, &wal.encrypted_record)
                            .expect("synchronize v1 WAL row-before-retention cut"),
                        wal.appended_sequence
                    );
                }
                LegacyV1AppendWalCrashCut::ActiveAfterRetentionBeforeManifest => {
                    let mut store = sink
                        .lock_store("v1 WAL post-retention cut")
                        .expect("lock v1 WAL post-retention store");
                    assert_eq!(
                        store
                            .append_line(ledger_pane_id, &wal.encrypted_record)
                            .expect("synchronize v1 WAL post-retention row"),
                        wal.appended_sequence
                    );
                    store
                        .prune_before(ledger_pane_id, wal.target_oldest_sequence)
                        .expect("synchronize v1 WAL retention cut");
                    LiveScrollbackSpillSink::verify_append_wal_target_store(&wal, &store)
                        .expect("v1 WAL retained target is exact");
                }
            }
            let wal_path = LiveScrollbackSpillSink::append_wal_path(&sink.manifest_path)
                .expect("derive legacy v1 WAL path");
            drop(sink);

            let first_reopen = LiveScrollbackSpillSink::new(dir.path().to_path_buf(), &context)
                .unwrap_or_else(|error| panic!("recover {cut:?} legacy v1 WAL: {error:#}"));
            assert_eq!(
                first_reopen
                    .load_scrollback_line(11)
                    .unwrap_or_else(|| panic!("{cut:?} must recover the exact v1 WAL target"))
                    .as_str()
                    .as_ref(),
                appended.as_str().as_ref()
            );
            assert!(first_reopen.load_scrollback_line(10).is_none());
            let migrated = LiveScrollbackSpillSink::read_manifest(&first_reopen.manifest_path)
                .expect("read v1-WAL migrated manifest")
                .expect("v1-WAL migrated manifest exists");
            assert_eq!(migrated.schema, LIVE_SCROLLBACK_MANIFEST_SCHEMA_V4);
            assert_eq!(migrated.revision, Some(wal.target_revision));
            let acknowledged = LiveScrollbackSpillSink::read_append_wal(&wal_path)
                .expect("read acknowledged legacy v1 WAL")
                .expect("acknowledged legacy v1 WAL remains present");
            assert_eq!(acknowledged.schema, LIVE_SCROLLBACK_APPEND_WAL_SCHEMA_V1);
            assert!(LiveScrollbackSpillSink::append_wal_supersession_matches_manifest(
                &acknowledged,
                &migrated,
            )
            .expect("bind legacy v1 WAL migration acknowledgement"));

            // Crash cut: the v4 target is durable but the same-generation
            // migration acknowledgement did not replace the original v1 WAL.
            overwrite_private_append_wal_fixture(&wal_path, &wal);
            drop(first_reopen);
            let second_reopen = LiveScrollbackSpillSink::new(dir.path().to_path_buf(), &context)
                .unwrap_or_else(|error| {
                    panic!("cold-reopen {cut:?} unacknowledged v1-to-v4 target: {error:#}")
                });
            assert_eq!(
                second_reopen
                    .load_scrollback_line(11)
                    .expect("second cold reopen retains exact v1 WAL target")
                    .as_str()
                    .as_ref(),
                appended.as_str().as_ref()
            );
            let reacknowledged = LiveScrollbackSpillSink::read_append_wal(&wal_path)
                .expect("read reacknowledged legacy v1 WAL")
                .expect("reacknowledged legacy v1 WAL remains present");
            let current = LiveScrollbackSpillSink::read_manifest(&second_reopen.manifest_path)
                .expect("read second-reopen v4 manifest")
                .expect("second-reopen v4 manifest exists");
            assert!(LiveScrollbackSpillSink::append_wal_supersession_matches_manifest(
                &reacknowledged,
                &current,
            )
            .expect("bind second-reopen v1 WAL migration acknowledgement"));

            let successor = Line::from_text(
                "v2-successor-after-v1-migration",
                &CellAttributes::blank(),
                3,
                None,
            );
            assert!(
                second_reopen.store_scrollback_line(12, &successor, 1),
                "a recovered v1 WAL must not strand later append authority"
            );
            let successor_wal = LiveScrollbackSpillSink::read_append_wal(&wal_path)
                .expect("read successor v2 WAL")
                .expect("successor v2 WAL exists");
            assert_eq!(successor_wal.schema, LIVE_SCROLLBACK_APPEND_WAL_SCHEMA_V2);
            drop(second_reopen);

            let third_reopen = LiveScrollbackSpillSink::new(dir.path().to_path_buf(), &context)
                .expect("reopen after replacing legacy v1 WAL with v2");
            assert_eq!(
                third_reopen
                    .load_scrollback_line(12)
                    .expect("v2 successor survives cold reopen")
                    .as_str()
                    .as_ref(),
                successor.as_str().as_ref()
            );
            assert!(third_reopen.load_scrollback_line(11).is_none());
        }
    }

    #[test]
    fn authenticated_v1_append_wal_wrong_target_digest_fails_closed_before_v4_publication() {
        let (dir, context, sink, mut wal, _appended) = legacy_v1_append_wal_fixture(219);
        let predecessor = LiveScrollbackSpillSink::read_manifest(&sink.manifest_path)
            .expect("read wrong-digest v3 predecessor")
            .expect("wrong-digest v3 predecessor exists");
        let wrong_digest = hex::encode([0x5a; 32]);
        assert_ne!(
            wal.target_record_set_sha256.as_deref(),
            Some(wrong_digest.as_str())
        );
        wal.target_record_set_sha256 = Some(wrong_digest);
        seal_append_wal_fixture(&sink, &mut wal);
        sink.persist_authenticated_append_wal(&wal)
            .expect("publish authenticated wrong-digest v1 WAL fixture");
        drop(sink);

        let error = LiveScrollbackSpillSink::new(dir.path().to_path_buf(), &context)
            .expect_err("authenticated v1 WAL with the wrong target set must fail closed");
        assert!(
            format!("{error:#}").contains("digest"),
            "wrong v1 target-set failure remains authority-specific: {error:#}"
        );
        let retained = LiveScrollbackSpillSink::read_manifest(
            &dir.path()
                .join(uuid::Uuid::from_bytes(context.durable_pane_id).simple().to_string())
                .join("manifest.json"),
        )
        .expect("read retained wrong-digest predecessor")
        .expect("wrong-digest predecessor remains present");
        assert_eq!(retained, predecessor);
        assert_eq!(retained.schema, LIVE_SCROLLBACK_MANIFEST_SCHEMA_V3);
    }

    #[test]
    fn authenticated_append_wal_supersession_recovers_ack_crash_and_advances_generations() {
        let dir = tempfile::tempdir().expect("create WAL supersession fixture");
        let context = config::ScrollbackSpillSinkContext {
            pane_id: 41_000,
            domain_id: 3,
            durable_pane_id: [206; 16],
            command_description: "append-wal-supersession".to_string(),
        };
        let sink = LiveScrollbackSpillSink::new(dir.path().to_path_buf(), &context)
            .expect("create WAL supersession sink");
        let attributes = CellAttributes::blank();
        let rows = vec![
            Line::from_text("supersession-row-a", &attributes, 1, None),
            Line::from_text("supersession-row-b", &attributes, 2, None),
        ];
        assert!(sink.store_scrollback_line(10, &rows[0], 8));
        assert!(sink.store_scrollback_line(11, &rows[1], 8));
        let wal_path = LiveScrollbackSpillSink::append_wal_path(&sink.manifest_path)
            .expect("derive supersession WAL path");
        let original = LiveScrollbackSpillSink::read_append_wal(&wal_path)
            .expect("read original consumed WAL")
            .expect("original consumed WAL exists");
        assert!(LiveScrollbackSpillSink::append_wal_supersession(&original)
            .expect("parse original WAL supersession")
            .is_none());
        let original_generation = sink
            .lock_state("read original supersession generation")
            .expect("lock original supersession state")
            .snapshot_generation();
        let first_prefix = wezterm_term::config::ScrollbackPrefix::from_slices(
            Some(10),
            12,
            &rows,
            &[],
        )
        .expect("construct first WAL supersession replacement");
        let first_replacement = sink
            .replace_scrollback_prefix(Some(original_generation), first_prefix, 8)
            .expect("publish first WAL supersession replacement");
        let first_manifest = LiveScrollbackSpillSink::read_manifest(&sink.manifest_path)
            .expect("read first superseding manifest")
            .expect("first superseding manifest exists");
        let first_retired = LiveScrollbackSpillSink::read_append_wal(&wal_path)
            .expect("read first retired WAL")
            .expect("first retired WAL remains present");
        assert!(LiveScrollbackSpillSink::append_wal_supersession_matches_manifest(
            &first_retired,
            &first_manifest,
        )
        .expect("bind first WAL retirement marker"));
        assert_eq!(first_retired.encrypted_record, original.encrypted_record);
        assert_eq!(
            first_retired.encrypted_record_sha256,
            original.encrypted_record_sha256
        );

        // Crash cut: the replacement manifest is durable but acknowledgement
        // rewrites have not advanced the retained WAL evidence yet.
        overwrite_private_append_wal_fixture(&wal_path, &original);
        drop(sink);
        let reopened = LiveScrollbackSpillSink::new(dir.path().to_path_buf(), &context)
            .expect("recover replacement-before-WAL-acknowledgement crash cut");
        let reopened_manifest = LiveScrollbackSpillSink::read_manifest(&reopened.manifest_path)
            .expect("read crash-cut successor manifest")
            .expect("crash-cut successor manifest exists");
        assert_eq!(
            reopened.active_ledger_pane_id(),
            LiveScrollbackSpillSink::manifest_ledger_pane_id(&reopened_manifest)
                .expect("decode crash-cut successor ledger")
        );
        assert_ne!(
            reopened.active_ledger_pane_id(),
            original.ledger_pane_id,
            "retained superseded WAL must not become current ledger authority"
        );
        let second_prefix = wezterm_term::config::ScrollbackPrefix::from_slices(
            Some(10),
            12,
            &rows,
            &[],
        )
        .expect("construct successive WAL supersession replacement");
        let second_replacement = reopened
            .replace_scrollback_prefix(
                Some(first_replacement.generation()),
                second_prefix,
                8,
            )
            .expect("publish successive WAL supersession replacement");
        let second_manifest = LiveScrollbackSpillSink::read_manifest(&reopened.manifest_path)
            .expect("read successive superseding manifest")
            .expect("successive superseding manifest exists");
        let second_retired = LiveScrollbackSpillSink::read_append_wal(&wal_path)
            .expect("read successively retired WAL")
            .expect("successively retired WAL remains present");
        assert_eq!(
            reopened.active_ledger_pane_id(),
            LiveScrollbackSpillSink::manifest_ledger_pane_id(&second_manifest)
                .expect("decode successive superseding ledger")
        );
        assert_ne!(reopened.active_ledger_pane_id(), original.ledger_pane_id);
        assert!(LiveScrollbackSpillSink::append_wal_supersession_matches_manifest(
            &second_retired,
            &second_manifest,
        )
        .expect("bind successive WAL retirement marker"));
        assert_eq!(
            second_replacement.generation(),
            reopened
                .lock_state("read successive supersession generation")
                .expect("lock successive supersession state")
                .snapshot_generation()
        );
        assert_eq!(second_retired.encrypted_record, original.encrypted_record);

        drop(reopened);
        let reopened = LiveScrollbackSpillSink::new(dir.path().to_path_buf(), &context)
            .expect("cold-reopen successive WAL supersession replacement");
        let cold_second_manifest =
            LiveScrollbackSpillSink::read_manifest(&reopened.manifest_path)
                .expect("read cold-reopened successive manifest")
                .expect("cold-reopened successive manifest exists");
        assert_eq!(cold_second_manifest, second_manifest);
        assert_eq!(
            reopened.active_ledger_pane_id(),
            LiveScrollbackSpillSink::manifest_ledger_pane_id(&cold_second_manifest)
                .expect("decode cold-reopened successive ledger")
        );
        assert_ne!(reopened.active_ledger_pane_id(), original.ledger_pane_id);

        reopened
            .clear_scrollback()
            .expect("publish clear across retained WAL evidence");
        let cleared_manifest = LiveScrollbackSpillSink::read_manifest(&reopened.manifest_path)
            .expect("read WAL-superseding clear manifest")
            .expect("WAL-superseding clear manifest exists");
        let cleared_retired = LiveScrollbackSpillSink::read_append_wal(&wal_path)
            .expect("read clear-retired WAL")
            .expect("clear-retired WAL evidence remains present");
        assert!(LiveScrollbackSpillSink::append_wal_supersession_matches_manifest(
            &cleared_retired,
            &cleared_manifest,
        )
        .expect("bind clear WAL retirement marker"));
        assert_eq!(cleared_retired.encrypted_record, original.encrypted_record);
        assert!(std::fs::symlink_metadata(&wal_path)
            .expect("retained WAL evidence path remains published")
            .is_file());

        drop(reopened);
        let reopened = LiveScrollbackSpillSink::new(dir.path().to_path_buf(), &context)
            .expect("cold-reopen clear across retained WAL evidence");
        let cold_cleared_manifest =
            LiveScrollbackSpillSink::read_manifest(&reopened.manifest_path)
                .expect("read cold-reopened clear manifest")
                .expect("cold-reopened clear manifest exists");
        assert_eq!(cold_cleared_manifest, cleared_manifest);
        assert_eq!(reopened.retained_scrollback_rows(), 0);
        assert!(reopened.load_scrollback_line(10).is_none());

        let post_clear_successor =
            Line::from_text("post-clear-successor", &attributes, 3, None);
        assert!(reopened.store_scrollback_line(20, &post_clear_successor, 8));
        let post_clear_manifest = LiveScrollbackSpillSink::read_manifest(&reopened.manifest_path)
            .expect("read post-clear successor manifest")
            .expect("post-clear successor manifest exists");
        let post_clear_retired = LiveScrollbackSpillSink::read_append_wal(&wal_path)
            .expect("read post-clear retired WAL")
            .expect("post-clear retired WAL evidence remains present");
        assert!(LiveScrollbackSpillSink::append_wal_supersession_matches_manifest(
            &post_clear_retired,
            &post_clear_manifest,
        )
        .expect("bind post-clear WAL retirement marker"));
        assert_eq!(post_clear_retired.encrypted_record, original.encrypted_record);

        drop(reopened);
        let final_reopen = LiveScrollbackSpillSink::new(dir.path().to_path_buf(), &context)
            .expect("cold-reopen post-clear WAL successor");
        let cold_post_clear_manifest =
            LiveScrollbackSpillSink::read_manifest(&final_reopen.manifest_path)
                .expect("read cold-reopened post-clear manifest")
                .expect("cold-reopened post-clear manifest exists");
        assert_eq!(cold_post_clear_manifest, post_clear_manifest);
        assert_eq!(
            final_reopen
                .load_scrollback_line(20)
                .expect("post-clear successor survives cold reopen")
                .as_str()
                .as_ref(),
            post_clear_successor.as_str().as_ref()
        );
        assert!(final_reopen.load_scrollback_line(10).is_none());
    }

    #[test]
    fn retained_append_wal_cannot_skip_two_authenticated_manifest_generations() {
        let dir = tempfile::tempdir().expect("create two-generation WAL skip fixture");
        let context = config::ScrollbackSpillSinkContext {
            pane_id: 41_002,
            domain_id: 3,
            durable_pane_id: [208; 16],
            command_description: "append-wal-two-generation-skip".to_string(),
        };
        let sink = LiveScrollbackSpillSink::new(dir.path().to_path_buf(), &context)
            .expect("create two-generation WAL skip sink");
        let attributes = CellAttributes::blank();
        let rows = vec![
            Line::from_text("two-generation-row-a", &attributes, 1, None),
            Line::from_text("two-generation-row-b", &attributes, 2, None),
        ];
        assert!(sink.store_scrollback_line(10, &rows[0], 8));
        assert!(sink.store_scrollback_line(11, &rows[1], 8));
        let wal_path = LiveScrollbackSpillSink::append_wal_path(&sink.manifest_path)
            .expect("derive two-generation WAL path");
        let original = LiveScrollbackSpillSink::read_append_wal(&wal_path)
            .expect("read two-generation original WAL")
            .expect("two-generation original WAL exists");
        let original_generation = sink
            .lock_state("read two-generation original state")
            .expect("lock two-generation original state")
            .snapshot_generation();

        let first_prefix = wezterm_term::config::ScrollbackPrefix::from_slices(
            Some(10),
            12,
            &rows,
            &[],
        )
        .expect("construct first two-generation replacement");
        let first = sink
            .replace_scrollback_prefix(Some(original_generation), first_prefix, 8)
            .expect("publish first two-generation replacement");
        let second_prefix = wezterm_term::config::ScrollbackPrefix::from_slices(
            Some(10),
            12,
            &rows,
            &[],
        )
        .expect("construct second two-generation replacement");
        sink.replace_scrollback_prefix(Some(first.generation()), second_prefix, 8)
            .expect("publish second two-generation replacement");
        let current = LiveScrollbackSpillSink::read_manifest(&sink.manifest_path)
            .expect("read two-generation current manifest")
            .expect("two-generation current manifest exists");
        assert_ne!(
            live_scrollback_manifest_predecessor(&current)
                .expect("decode two-generation current predecessor"),
            Some(
                LiveScrollbackSpillSink::append_wal_effective_generation(&original)
                    .expect("decode two-generation original WAL target")
            )
        );
        let manifest_bytes_before =
            std::fs::read(&sink.manifest_path).expect("read two-generation manifest bytes");
        let pane_dir = sink
            .manifest_path
            .parent()
            .expect("two-generation manifest has parent");
        let log_path = pane_dir.join(&current.content_log);
        let sequence_path = pane_dir.join(
            current
                .content_sequence
                .as_deref()
                .expect("two-generation current manifest names a sequence journal"),
        );
        let log_bytes_before =
            std::fs::read(&log_path).expect("read two-generation current log bytes");
        let sequence_bytes_before = std::fs::read(&sequence_path)
            .expect("read two-generation current sequence bytes");

        // Restore valid but two-generations-stale WAL evidence. The current
        // manifest is not its target, marker, or immediate successor.
        overwrite_private_append_wal_fixture(&wal_path, &original);
        drop(sink);
        let error = LiveScrollbackSpillSink::new(dir.path().to_path_buf(), &context)
            .expect_err("a retained WAL must not skip two authenticated generations");
        assert!(
            format!("{error:#}").contains("neither adjacent"),
            "two-generation skip rejection remains chain-specific: {error:#}"
        );
        assert_eq!(
            std::fs::read(
                dir.path()
                    .join(uuid::Uuid::from_bytes(context.durable_pane_id).simple().to_string())
                    .join("manifest.json")
            )
            .expect("re-read two-generation manifest bytes"),
            manifest_bytes_before
        );
        assert_eq!(
            std::fs::read(&log_path).expect("re-read two-generation current log bytes"),
            log_bytes_before
        );
        assert_eq!(
            std::fs::read(&sequence_path)
                .expect("re-read two-generation current sequence bytes"),
            sequence_bytes_before
        );
    }

    #[test]
    fn append_wal_supersession_metadata_requires_guardian_authentication() {
        let dir = tempfile::tempdir().expect("create WAL supersession tamper fixture");
        let context = config::ScrollbackSpillSinkContext {
            pane_id: 41_001,
            domain_id: 3,
            durable_pane_id: [207; 16],
            command_description: "append-wal-supersession-tamper".to_string(),
        };
        let sink = LiveScrollbackSpillSink::new(dir.path().to_path_buf(), &context)
            .expect("create WAL supersession tamper sink");
        let attributes = CellAttributes::blank();
        let rows = vec![
            Line::from_text("supersession-tamper-a", &attributes, 1, None),
            Line::from_text("supersession-tamper-b", &attributes, 2, None),
        ];
        assert!(sink.store_scrollback_line(10, &rows[0], 8));
        assert!(sink.store_scrollback_line(11, &rows[1], 8));
        let generation = sink
            .lock_state("read supersession tamper generation")
            .expect("lock supersession tamper state")
            .snapshot_generation();
        let prefix = wezterm_term::config::ScrollbackPrefix::from_slices(
            Some(10),
            12,
            &rows,
            &[],
        )
        .expect("construct supersession tamper replacement");
        sink.replace_scrollback_prefix(Some(generation), prefix, 8)
            .expect("publish supersession tamper replacement");
        let wal_path = LiveScrollbackSpillSink::append_wal_path(&sink.manifest_path)
            .expect("derive supersession tamper WAL path");
        let mut tampered = LiveScrollbackSpillSink::read_append_wal(&wal_path)
            .expect("read supersession tamper WAL")
            .expect("supersession tamper WAL exists");
        tampered.superseding_revision = Some(
            tampered
                .superseding_revision
                .expect("supersession marker has a revision")
                .checked_add(1)
                .expect("supersession tamper revision fits"),
        );
        tampered.wal_sha256 = LiveScrollbackSpillSink::append_wal_checksum(&tampered)
            .expect("recompute public checksum after supersession mutation");
        overwrite_private_append_wal_fixture(&wal_path, &tampered);
        drop(sink);
        let error = LiveScrollbackSpillSink::new(dir.path().to_path_buf(), &context)
            .expect_err("unauthenticated WAL supersession metadata must fail closed");
        assert!(
            format!("{error:#}").contains("append WAL"),
            "supersession tamper error remains source-specific: {error:#}"
        );
    }

    #[test]
    fn authenticated_append_wal_recovers_with_its_historical_guardian_key() {
        let (dir, context, sink, wal, appended) = append_wal_fixture(133, 1);
        let original_key_id = sink
            .lock_keyring("inspect append WAL guardian key")
            .expect("lock append WAL keyring")
            .active_key_id();
        let authentication =
            mux::guardian_output_journal::GuardianScrollbackAppendWalAuthentication::parse(
                wal.guardian_authentication
                    .as_deref()
                    .expect("append WAL carries guardian authentication"),
            )
            .expect("parse append WAL authentication");
        assert_eq!(authentication.key_id(), original_key_id);
        sink.persist_authenticated_append_wal(&wal)
            .expect("publish append WAL under original guardian key");
        let persisted_wal_bytes = std::fs::read(
            LiveScrollbackSpillSink::append_wal_path(&sink.manifest_path)
                .expect("derive persisted append WAL path"),
        )
        .expect("read persisted encrypted append WAL");
        assert!(
            !persisted_wal_bytes
                .windows(b"append-wal-recovered-target".len())
                .any(|window| window == b"append-wal-recovered-target"),
            "the recovery WAL must never persist exact semantic plaintext"
        );
        let rotated_key_id = sink
            .lock_keyring("rotate after append WAL publication")
            .expect("lock append WAL keyring for rotation")
            .rotate()
            .expect("rotate guardian key after durable WAL publication");
        assert_ne!(rotated_key_id, original_key_id);
        drop(sink);

        let reopened = LiveScrollbackSpillSink::new(dir.path().to_path_buf(), &context)
            .expect("recover append WAL through historical guardian key lookup");
        assert_eq!(
            reopened
                .load_scrollback_line(11)
                .expect("historical-key WAL restores its exact row")
                .as_str()
                .as_ref(),
            appended.as_str().as_ref()
        );
        assert!(reopened.load_scrollback_line(10).is_none());
        let persisted = LiveScrollbackSpillSink::read_append_wal(
            &LiveScrollbackSpillSink::append_wal_path(&reopened.manifest_path)
                .expect("derive historical-key append WAL path"),
        )
        .expect("read historical-key append WAL")
        .expect("consumed historical-key append WAL remains in its bounded slot");
        let keyring = reopened
            .lock_keyring("verify historical append WAL after reopen")
            .expect("lock reopened append WAL keyring");
        assert_eq!(keyring.active_key_id(), rotated_key_id);
        LiveScrollbackSpillSink::authenticate_append_wal(&persisted, &keyring)
            .expect("historical append WAL remains guardian-authenticated after rotation");
    }

    #[cfg(unix)]
    #[test]
    fn append_wal_publication_rejects_symlink_stage_without_mutation() {
        use std::os::unix::fs::symlink;

        let (dir, _context, sink, wal, _appended) = append_wal_fixture(134, 1);
        let attacker_target = dir.path().join("append-wal-attacker-target");
        std::fs::write(&attacker_target, b"attacker-content")
            .expect("write append-WAL attacker target");
        let stage_path = LiveScrollbackSpillSink::append_wal_stage_path(&sink.manifest_path)
            .expect("derive unsafe append WAL stage path");
        symlink(&attacker_target, &stage_path).expect("create append WAL stage symlink");

        let error = sink
            .persist_authenticated_append_wal(&wal)
            .expect_err("append WAL publication must reject a symlink stage");
        assert!(
            !error.outcome_indeterminate(),
            "stage-path rejection occurs before any WAL publication attempt"
        );
        assert_eq!(
            std::fs::read(&attacker_target).expect("reread append-WAL attacker target"),
            b"attacker-content"
        );
        assert!(
            LiveScrollbackSpillSink::read_append_wal(
                &LiveScrollbackSpillSink::append_wal_path(&sink.manifest_path)
                    .expect("derive active append WAL path")
            )
            .expect("inspect active append WAL slot")
            .is_none()
        );
        assert!(sink.load_scrollback_line(11).is_none());
    }

    #[cfg(unix)]
    #[test]
    fn incomplete_manifest_stage_rewrite_rejects_unsafe_path_authority() {
        use std::os::unix::fs::{PermissionsExt as _, symlink};

        let directory = tempfile::tempdir().expect("create unsafe manifest-stage fixtures");
        let attacker_target = directory.path().join("attacker-target");
        std::fs::write(&attacker_target, b"attacker-content")
            .expect("write manifest-stage attacker target");
        let symlink_stage = directory.path().join("symlink-stage");
        symlink(&attacker_target, &symlink_stage).expect("create manifest-stage symlink");
        assert!(
            LiveScrollbackSpillSink::rewrite_incomplete_manifest_stage(
                &symlink_stage,
                b"replacement"
            )
            .is_err(),
            "a symlink must never become deterministic manifest-stage authority"
        );
        assert_eq!(
            std::fs::read(&attacker_target).expect("reread symlink target"),
            b"attacker-content"
        );

        let hard_link_target = directory.path().join("hard-link-target");
        std::fs::write(&hard_link_target, b"hard-link-content")
            .expect("write manifest-stage hard-link target");
        std::fs::set_permissions(&hard_link_target, std::fs::Permissions::from_mode(0o600))
            .expect("make hard-link fixture otherwise private");
        let hard_link_stage = directory.path().join("hard-link-stage");
        std::fs::hard_link(&hard_link_target, &hard_link_stage)
            .expect("create manifest-stage hard link");
        assert!(
            LiveScrollbackSpillSink::rewrite_incomplete_manifest_stage(
                &hard_link_stage,
                b"replacement"
            )
            .is_err(),
            "a multiply linked inode must never be truncated as a manifest stage"
        );
        assert_eq!(
            std::fs::read(&hard_link_target).expect("reread hard-link target"),
            b"hard-link-content"
        );

        let public_stage = directory.path().join("public-stage");
        std::fs::write(&public_stage, b"public-content")
            .expect("write non-private manifest stage");
        std::fs::set_permissions(&public_stage, std::fs::Permissions::from_mode(0o644))
            .expect("make manifest-stage fixture non-private");
        assert!(
            LiveScrollbackSpillSink::rewrite_incomplete_manifest_stage(
                &public_stage,
                b"replacement"
            )
            .is_err(),
            "a non-private file must never become manifest-stage authority"
        );
        assert_eq!(
            std::fs::read(&public_stage).expect("reread non-private stage"),
            b"public-content"
        );
    }

    #[test]
    fn incomplete_append_wal_stage_is_non_authoritative_bounded_and_reusable() {
        let (dir, context, sink, _wal, appended) = append_wal_fixture(131, 1);
        let stage_path =
            LiveScrollbackSpillSink::append_wal_stage_path(&sink.manifest_path)
                .expect("derive incomplete append WAL stage path");
        write_private_stage_fixture(&stage_path, br#"{"schema":"#);
        drop(sink);

        let reopened = LiveScrollbackSpillSink::new(dir.path().to_path_buf(), &context)
            .expect("ignore securely pinned incomplete append WAL stage");
        assert_eq!(
            reopened
                .load_scrollback_line(10)
                .expect("predecessor survives incomplete append WAL stage")
                .as_str()
                .as_ref(),
            "append-wal-predecessor"
        );
        assert!(reopened.load_scrollback_line(11).is_none());
        assert!(
            reopened.store_scrollback_line(11, &appended, 1),
            "the deterministic incomplete stage must be safely reusable"
        );
        assert_eq!(
            reopened
                .load_scrollback_line(11)
                .expect("row stored through reused append WAL stage")
                .as_str()
                .as_ref(),
            appended.as_str().as_ref()
        );
        let pane_dir = reopened
            .manifest_path
            .parent()
            .expect("append WAL manifest has a parent");
        let wal_entries = std::fs::read_dir(pane_dir)
            .expect("list bounded append WAL directory")
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().contains("append-wal"))
            .count();
        assert_eq!(
            wal_entries, 1,
            "retries must retain only the one deterministic active/stage WAL slot"
        );
    }

    #[test]
    fn append_wal_authentication_rejects_same_length_ciphertext_and_metadata_mutation() {
        let (_dir, _context, sink, wal, _appended) = append_wal_fixture(132, 8);
        let mut tampered = wal.clone();
        let mut record_bytes = tampered.encrypted_record.into_bytes();
        let mutation_index = record_bytes
            .len()
            .checked_sub(8)
            .expect("encrypted row has a complete interior base64 quantum");
        record_bytes[mutation_index] = if record_bytes[mutation_index] == b'A' {
            b'B'
        } else {
            b'A'
        };
        tampered.encrypted_record =
            String::from_utf8(record_bytes).expect("ciphertext mutation stays ASCII");
        tampered.encrypted_record_sha256 = hex::encode(
            live_scrollback_append_wal_record_digest(&tampered.encrypted_record)
                .expect("digest mutated encrypted record"),
        );
        let predecessor_tail = decode_live_scrollback_canonical_digest(
            tampered
                .predecessor_chain_tail_sha256
                .as_deref()
                .expect("v2 WAL predecessor tail exists"),
            "test predecessor tail",
        )
        .expect("decode v2 WAL predecessor tail");
        tampered.target_chain_tail_sha256 = Some(hex::encode(
            live_scrollback_incremental_chain_next(
                predecessor_tail,
                tampered.ledger_pane_id,
                tampered.appended_sequence,
                &tampered.encrypted_record,
            )
            .expect("recompute attacker-controlled target tail"),
        ));
        tampered.wal_sha256 = LiveScrollbackSpillSink::append_wal_checksum(&tampered)
            .expect("recompute attacker-controlled public WAL checksum");
        LiveScrollbackSpillSink::validate_append_wal_identity(
            &tampered,
            sink.durable_pane_id,
        )
        .expect("same-length ciphertext mutation remains structurally canonical");
        let keyring = sink
            .lock_keyring("authenticate tampered append WAL")
            .expect("lock tampered append WAL keyring");
        assert!(
            LiveScrollbackSpillSink::authenticate_append_wal(&tampered, &keyring).is_err(),
            "recomputed public digests cannot replace guardian authentication"
        );

        let mut wrong_row = wal.clone();
        wrong_row.appended_stable_row = wrong_row
            .appended_stable_row
            .checked_add(1)
            .expect("wrong-row test value fits");
        wrong_row.wal_sha256 = LiveScrollbackSpillSink::append_wal_checksum(&wrong_row)
            .expect("recompute wrong-row public checksum");
        assert!(
            LiveScrollbackSpillSink::validate_append_wal_identity(
                &wrong_row,
                sink.durable_pane_id,
            )
            .is_err(),
            "a WAL cannot move its exact row identity"
        );

        let mut impossible_record_bound = wal.clone();
        impossible_record_bound.target_retained_record_bytes = impossible_record_bound
            .target_record_count
            .checked_sub(1)
            .expect("multi-row WAL fixture has a smaller nonzero byte bound");
        impossible_record_bound.wal_sha256 =
            LiveScrollbackSpillSink::append_wal_checksum(&impossible_record_bound)
                .expect("recompute impossible-bound public checksum");
        assert!(
            LiveScrollbackSpillSink::validate_append_wal_identity(
                &impossible_record_bound,
                sink.durable_pane_id,
            )
            .is_err(),
            "every retained record must consume at least one authenticated stored byte"
        );
        let debug = format!("{wal:?}");
        assert!(!debug.contains("append-wal-recovered-target"));
        assert!(!debug.contains(&wal.encrypted_record));
        assert!(!debug.contains(&wal.guardian_authentication.clone().unwrap()));
    }

    #[test]
    fn live_scrollback_spill_sink_reads_legacy_content_ahead_without_republishing_it() {
        let dir = tempfile::tempdir().expect("temp scrollback dir");
        let context = config::ScrollbackSpillSinkContext {
            pane_id: 813,
            domain_id: 3,
            durable_pane_id: [87; 16],
            command_description: "complete-recovery-shell".to_string(),
        };
        {
            let sink = LiveScrollbackSpillSink::new(dir.path().to_path_buf(), &context)
                .expect("create live spill sink");
            let attrs = CellAttributes::blank();
            let first = Line::from_text("published-row", &attrs, 1, None);
            assert!(sink.store_scrollback_line(20, &first, 8));
            {
                // Model an existing v2 authority before appending the mixed
                // legacy rows. A v3 authority must reject, never relabel, a
                // mixed exact/legacy ledger.
                let mut state = sink
                    .lock_state("test legacy mixed manifest")
                    .expect("test state lock");
                state.authenticated_manifest = false;
                state.newest_stable_row_exclusive = None;
                state.predecessor_generation = None;
            }
            sink.persist_manifest("complete")
                .expect("publish legacy mixed-manifest fixture");

            let second = Line::from_text("durable-unpublished-row", &attrs, 2, None);
            let redactor = frankenterm_core::redactor::Redactor::new();
            let record = encode_scrollback_line_record(&second, &redactor)
                .expect("encode legacy durable unpublished row");
            assert_eq!(
                sink.lock_store("test interrupted complete publication")
                    .expect("test store lock")
                    .append_line(sink.pane_id, &record)
                    .expect("durably append unpublished row"),
                1
            );
            assert_eq!(
                sink.lock_store("test raw legacy publication")
                    .expect("test store lock")
                    .append_line(sink.pane_id, "raw-legacy-unpublished-row")
                    .expect("durably append raw legacy row"),
                2
            );
        }

        let reopened = LiveScrollbackSpillSink::new(dir.path().to_path_buf(), &context)
            .expect("read stale legacy manifest without rewriting it");
        assert_eq!(
            reopened
                .load_scrollback_line(21)
                .expect("content ahead of manifest survives reopen")
                .as_str()
                .as_ref(),
            "durable-unpublished-row"
        );
        assert_eq!(
            reopened
                .load_scrollback_line(22)
                .expect("raw legacy content ahead of manifest survives reopen")
                .as_str()
                .as_ref(),
            "raw-legacy-unpublished-row"
        );
        let manifest = LiveScrollbackSpillSink::read_manifest(&reopened.manifest_path)
            .expect("read retained legacy manifest")
            .expect("retained legacy manifest exists");
        assert_eq!(manifest.publication_state, "complete");
        assert_eq!(manifest.oldest_seq, Some(0));
        assert_eq!(manifest.retained_rows, 1);
        assert_eq!(manifest.next_seq, 1);
        let rejected = Line::from_text("must-not-mix-into-legacy", &CellAttributes::blank(), 3, None);
        assert!(
            !reopened.store_scrollback_line(23, &rejected, 8),
            "a checksum-only legacy lineage must remain read-only"
        );
        assert_eq!(
            LiveScrollbackSpillSink::read_manifest(&reopened.manifest_path)
                .expect("re-read retained legacy manifest")
                .expect("retained legacy manifest still exists"),
            manifest,
            "a rejected legacy append must not republish or relabel the manifest"
        );
        assert_eq!(
            reopened
                .lock_store("test rejected legacy append")
                .expect("test legacy store lock")
                .line_count(reopened.pane_id),
            3,
            "a rejected legacy append must not mutate stored rows"
        );
        let mixed_snapshot = reopened
            .snapshot_scrollback(
                23,
                wezterm_term::config::ScrollbackSnapshotLimits {
                    max_rows: 8,
                    max_stored_bytes: 1024 * 1024,
                    max_decoded_bytes: 1024 * 1024,
                    max_physical_bytes: 1024 * 1024,
                },
            )
            .expect("capture mixed exact and legacy snapshot");
        assert_eq!(
            mixed_snapshot.fidelity(),
            wezterm_term::config::ScrollbackSnapshotFidelity::LegacyRedacted,
            "one legacy row keeps the complete mixed snapshot non-recovery-grade"
        );

        let durable_pane_id = uuid::Uuid::from_bytes(context.durable_pane_id)
            .simple()
            .to_string();
        let export = export_live_scrollback_transcript(
            dir.path(),
            &durable_pane_id,
            8,
            1024 * 1024,
            1024 * 1024,
        )
        .expect("export mixed versioned and raw legacy records");
        assert!(export.transcript.contains("published-row\n"));
        assert!(export.transcript.contains("durable-unpublished-row\n"));
        assert!(export.transcript.contains("raw-legacy-unpublished-row\n"));
        assert_eq!(export.exact_semantic_records, 1);
        assert_eq!(export.legacy_non_recovery_grade_records, 2);
        assert_eq!(export.pre_persistence_redaction_not_applied_records, 1);
        assert_eq!(
            export.legacy_redaction_attested_but_unauthenticated_records,
            1
        );
        assert_eq!(export.raw_legacy_redaction_unknown_records, 1);
        assert_eq!(
            export.pre_persistence_redaction_not_applied_records
                + export.legacy_redaction_attested_but_unauthenticated_records
                + export.raw_legacy_redaction_unknown_records,
            export.retained_rows
        );
        assert_eq!(
            export.legacy_redaction_attested_but_unauthenticated_records
                + export.raw_legacy_redaction_unknown_records,
            export.legacy_non_recovery_grade_records
        );
    }

    #[test]
    fn legacy_clear_manifest_cannot_authorize_physical_reclamation_on_reopen() {
        let dir = tempfile::tempdir().expect("temp legacy-clear authority dir");
        let context = config::ScrollbackSpillSinkContext {
            pane_id: 793,
            domain_id: 3,
            durable_pane_id: [73; 16],
            command_description: "legacy-clear-authority-shell".to_string(),
        };
        let durable_pane_id = uuid::Uuid::from_bytes(context.durable_pane_id)
            .simple()
            .to_string();
        let log_path = dir.path().join(&durable_pane_id).join("0.log");
        {
            let sink = LiveScrollbackSpillSink::new(dir.path().to_path_buf(), &context)
                .expect("create legacy-clear authority sink");
            assert!(sink.store_scrollback_line(
                0,
                &Line::from_text("must-not-be-reclaimed", &CellAttributes::blank(), 1, None),
                8,
            ));
            let mut state = sink
                .lock_state("test legacy clear authority")
                .expect("test legacy-clear state lock");
            state.initial_stable_row = None;
            state.newest_stable_row_exclusive = None;
            state.max_retained_rows = 0;
            state.authenticated_manifest = false;
            state.predecessor_generation = None;
            drop(state);
            sink.persist_manifest("cleared")
                .expect("publish checksum-only historical clear fixture");
        }
        let log_before = std::fs::read(&log_path).expect("read residual legacy-clear bytes");
        let error = LiveScrollbackSpillSink::new(dir.path().to_path_buf(), &context)
            .expect_err("legacy clear cannot authorize destructive recovery");
        assert!(format!("{error:#}").contains("cannot authorize reclamation"));
        assert_eq!(
            std::fs::read(&log_path).expect("re-read residual legacy-clear bytes"),
            log_before,
            "legacy clear recovery must leave residual bytes untouched"
        );
    }

    #[test]
    fn live_scrollback_spill_sink_reopens_authenticated_empty_forward_retention_state() {
        let dir = tempfile::tempdir().expect("temp scrollback dir");
        let context = config::ScrollbackSpillSinkContext {
            pane_id: 814,
            domain_id: 3,
            durable_pane_id: [88; 16],
            command_description: "empty-retention-recovery-shell".to_string(),
        };
        {
            let sink = LiveScrollbackSpillSink::new(dir.path().to_path_buf(), &context)
                .expect("create live spill sink");
            let attrs = CellAttributes::blank();
            for stable_row in 30..34 {
                let line = Line::from_text(
                    &format!("retained-row-{stable_row}"),
                    &attrs,
                    stable_row,
                    None,
                );
                let stable_row = isize::try_from(stable_row).expect("stable row fits isize");
                assert!(sink.store_scrollback_line(stable_row, &line, 8));
            }
            let prior_authority = sink
                .lock_state("test empty forward authority predecessor")
                .expect("test state lock")
                .verified_ledger
                .expect("populated v4 ledger has verified authority");
            let mut store = sink
                .lock_store("test empty forward retention state")
                .expect("test store lock");
            store.prune_before(sink.pane_id, 4).expect("persist full logical prune");
            assert!(
                store
                    .compact_pane_if_stale(sink.pane_id, 1)
                    .expect("compact empty retained set")
            );
            drop(store);
            sink.lock_state("test empty forward authority target")
                .expect("test state lock")
                .verified_ledger = Some(VerifiedLedgerState {
                ledger_pane_id: prior_authority.ledger_pane_id,
                oldest_sequence: None,
                next_sequence: 4,
                record_count: 0,
                retained_record_bytes: 0,
                chain_anchor: prior_authority.chain_tail,
                chain_tail: prior_authority.chain_tail,
            });
            sink.persist_manifest("complete")
                .expect("authenticate empty forward-retention state");
        }

        let reopened = LiveScrollbackSpillSink::new(dir.path().to_path_buf(), &context)
            .expect("reopen authenticated empty forward retention state");
        assert_eq!(reopened.retained_scrollback_rows(), 0);
        assert_eq!(reopened.oldest_scrollback_row(), None);
        let manifest = LiveScrollbackSpillSink::read_manifest(&reopened.manifest_path)
            .expect("read repaired manifest")
            .expect("repaired manifest exists");
        assert_eq!(manifest.publication_state, "complete");
        assert_eq!(manifest.oldest_seq, None);
        assert_eq!(manifest.retained_rows, 0);
        assert_eq!(manifest.next_seq, 4);
        let durable_pane_id = uuid::Uuid::from_bytes(context.durable_pane_id)
            .simple()
            .to_string();
        let identity = read_live_scrollback_committed_ledger_identity(
            dir.path(),
            &durable_pane_id,
        )
        .expect("read authenticated empty forward-retention identity");
        assert_eq!(identity.oldest_sequence(), None);
        assert_eq!(identity.next_sequence(), 4);
        assert_eq!(identity.oldest_stable_row(), None);
        assert_eq!(identity.newest_stable_row_exclusive(), 34);
        assert_eq!(identity.record_count(), 0);
        let resumed = Line::from_text("retained-row-34", &CellAttributes::blank(), 34, None);
        assert!(reopened.store_scrollback_line(34, &resumed, 8));
        assert_eq!(
            reopened
                .load_scrollback_line(34)
                .expect("sequence resumes after empty retained interval")
                .as_str()
                .as_ref(),
            "retained-row-34"
        );
    }

    #[test]
    fn live_scrollback_export_is_read_only_bounded_and_discoverable() {
        let dir = tempfile::tempdir().expect("temp scrollback dir");
        let zero_limit_error = list_live_scrollback_panes(dir.path(), 0)
            .expect_err("public discovery must reject an unbounded zero-entry policy");
        assert!(zero_limit_error.to_string().contains("max_entries must be in"));
        let context = config::ScrollbackSpillSinkContext {
            pane_id: 815,
            domain_id: 31,
            durable_pane_id: [89; 16],
            command_description: "recoverable-shell".to_string(),
        };
        let durable_pane_id = uuid::Uuid::from_bytes(context.durable_pane_id)
            .simple()
            .to_string();
        {
            let sink = LiveScrollbackSpillSink::new(dir.path().to_path_buf(), &context)
                .expect("create live spill sink");
            let attrs = CellAttributes::blank();
            assert!(sink.store_scrollback_line(
                40,
                &Line::from_text("recoverable-one", &attrs, 1, None),
                8
            ));
            assert!(sink.store_scrollback_line(
                41,
                &Line::from_text(
                    "recoverable-two sk-abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMN",
                    &attrs,
                    2,
                    None,
                ),
                8
            ));
        }

        let pane_dir = dir.path().join(&durable_pane_id);
        let log_path = pane_dir.join("0.log");
        let manifest_path = pane_dir.join("manifest.json");
        let log_before = std::fs::metadata(&log_path).expect("log metadata before export");
        let manifest_before =
            std::fs::metadata(&manifest_path).expect("manifest metadata before export");

        let panes = list_live_scrollback_panes(dir.path(), 8).expect("list durable panes");
        assert_eq!(panes.len(), 1);
        assert_eq!(panes[0].durable_pane_id, durable_pane_id);
        assert_eq!(panes[0].state, "complete");
        assert_eq!(panes[0].retained_rows, Some(2));
        assert!(panes[0].error.is_none());

        let export = export_live_scrollback_transcript(
            dir.path(),
            &durable_pane_id,
            8,
            1024,
            1024 * 1024,
        )
        .expect("export durable transcript");
        assert!(export.transcript.starts_with("recoverable-one\nrecoverable-two "));
        assert!(export.transcript.contains("[REDACTED]"));
        assert!(!export.transcript.contains("sk-abcdefghijklmnopqrstuvwxyz"));
        assert_eq!(export.retained_rows, 2);
        assert_eq!(export.oldest_seq, Some(0));
        assert_eq!(export.next_seq, 2);
        assert!(!export.source_content_mutated);
        assert_eq!(export.exact_semantic_records, 2);
        assert_eq!(export.legacy_non_recovery_grade_records, 0);
        assert_eq!(export.pre_persistence_redaction_not_applied_records, 2);
        assert_eq!(
            export.legacy_redaction_attested_but_unauthenticated_records,
            0
        );
        assert_eq!(export.raw_legacy_redaction_unknown_records, 0);
        assert_eq!(
            export.pre_persistence_redaction_not_applied_records
                + export.legacy_redaction_attested_but_unauthenticated_records
                + export.raw_legacy_redaction_unknown_records,
            export.retained_rows
        );
        assert_eq!(
            export.legacy_redaction_attested_but_unauthenticated_records
                + export.raw_legacy_redaction_unknown_records,
            export.legacy_non_recovery_grade_records
        );
        assert!(export.redaction_applied_during_export);

        let log_after = std::fs::metadata(&log_path).expect("log metadata after export");
        let manifest_after =
            std::fs::metadata(&manifest_path).expect("manifest metadata after export");
        assert_eq!(log_after.len(), log_before.len());
        assert_eq!(log_after.modified().ok(), log_before.modified().ok());
        assert_eq!(manifest_after.len(), manifest_before.len());
        assert_eq!(manifest_after.modified().ok(), manifest_before.modified().ok());

        let error = export_live_scrollback_transcript(
            dir.path(),
            &durable_pane_id,
            1,
            1024,
            1024 * 1024,
        )
        .expect_err("row limit must fail closed");
        assert!(error.to_string().contains("records limit 1"));
    }

    #[test]
    fn live_scrollback_export_ignores_uncommitted_torn_tail() {
        let dir = tempfile::tempdir().expect("temp scrollback dir");
        let context = config::ScrollbackSpillSinkContext {
            pane_id: 816,
            domain_id: 32,
            durable_pane_id: [90; 16],
            command_description: "torn-tail-shell".to_string(),
        };
        let durable_pane_id = uuid::Uuid::from_bytes(context.durable_pane_id)
            .simple()
            .to_string();
        {
            let sink = LiveScrollbackSpillSink::new(dir.path().to_path_buf(), &context)
                .expect("create live spill sink");
            assert!(sink.store_scrollback_line(
                50,
                &Line::from_text("committed", &CellAttributes::blank(), 1, None),
                8
            ));
        }
        let log_path = dir.path().join(&durable_pane_id).join("0.log");
        let mut log = std::fs::OpenOptions::new()
            .append(true)
            .open(&log_path)
            .expect("open log for torn-tail fixture");
        std::io::Write::write_all(&mut log, b"torn").expect("append torn tail fixture");
        log.sync_all().expect("persist torn tail fixture");
        drop(log);

        let export = export_live_scrollback_transcript(
            dir.path(),
            &durable_pane_id,
            8,
            1024,
            1024 * 1024,
        )
        .expect("export committed prefix");
        assert_eq!(export.transcript, "committed\n");
        assert_eq!(export.retained_rows, 1);
        assert_eq!(export.trailing_uncommitted_bytes, 4);
    }

    #[test]
    fn live_scrollback_export_rejects_content_pruned_behind_manifest() {
        let dir = tempfile::tempdir().expect("temp scrollback dir");
        let context = config::ScrollbackSpillSinkContext {
            pane_id: 817,
            domain_id: 33,
            durable_pane_id: [91; 16],
            command_description: "manifest-ahead-shell".to_string(),
        };
        let durable_pane_id = uuid::Uuid::from_bytes(context.durable_pane_id)
            .simple()
            .to_string();
        {
            let sink = LiveScrollbackSpillSink::new(dir.path().to_path_buf(), &context)
                .expect("create live spill sink");
            assert!(sink.store_scrollback_line(
                60,
                &Line::from_text("published", &CellAttributes::blank(), 1, None),
                8
            ));
        }
        let sequence_path = dir.path().join(&durable_pane_id).join("0.seq");
        std::fs::OpenOptions::new()
            .append(true)
            .open(sequence_path)
            .and_then(|mut file| {
                std::io::Write::write_all(&mut file, b"FTSEQ1:1\n")?;
                file.sync_all()
            })
            .expect("persist a content boundary ahead of the manifest fixture");

        let error = export_live_scrollback_transcript(
            dir.path(),
            &durable_pane_id,
            8,
            1024,
            1024 * 1024,
        )
        .expect_err("an empty content snapshot must not satisfy a retained manifest");
        assert!(
            format!("{error:#}").contains("logical ledger"),
            "authenticated content mutation must fail at the logical-ledger authority: {error:#}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn live_scrollback_export_rejects_non_private_content_log() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = tempfile::tempdir().expect("temp scrollback dir");
        let context = config::ScrollbackSpillSinkContext {
            pane_id: 818,
            domain_id: 34,
            durable_pane_id: [92; 16],
            command_description: "private-log-shell".to_string(),
        };
        let durable_pane_id = uuid::Uuid::from_bytes(context.durable_pane_id)
            .simple()
            .to_string();
        {
            let sink = LiveScrollbackSpillSink::new(dir.path().to_path_buf(), &context)
                .expect("create live spill sink");
            assert!(sink.store_scrollback_line(
                70,
                &Line::from_text("private", &CellAttributes::blank(), 1, None),
                8
            ));
        }
        let log_path = dir.path().join(&durable_pane_id).join("0.log");
        std::fs::set_permissions(&log_path, std::fs::Permissions::from_mode(0o644))
            .expect("relax fixture permissions");

        let error = export_live_scrollback_transcript(
            dir.path(),
            &durable_pane_id,
            8,
            1024,
            1024 * 1024,
        )
        .expect_err("export must reject non-private source content");
        assert!(error.to_string().contains("log is not private"));
    }

    #[cfg(unix)]
    #[test]
    fn live_scrollback_listing_rejects_dangling_base_symlink() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().expect("temp scrollback dir");
        let alias = dir.path().join("scrollback-lines");
        symlink(dir.path().join("missing-target"), &alias).expect("create dangling base symlink");

        let error = list_live_scrollback_panes(&alias, 8)
            .expect_err("a dangling storage symlink must not look like an empty store");
        assert!(error.to_string().contains("not a directory"));
    }

    #[test]
    fn live_scrollback_spill_sink_is_idempotent_and_rejects_sequence_gaps() {
        let dir = tempfile::tempdir().expect("temp scrollback dir");
        let context = config::ScrollbackSpillSinkContext {
            pane_id: 811,
            domain_id: 3,
            durable_pane_id: [83; 16],
            command_description: "idempotent-shell".to_string(),
        };
        let sink = LiveScrollbackSpillSink::new(dir.path().to_path_buf(), &context)
            .expect("create live spill sink");
        let attrs = CellAttributes::blank();
        let first = Line::from_text("stable-row-10", &attrs, 10, None);
        assert!(sink.store_scrollback_line(10, &first, 100));
        assert!(sink.store_scrollback_line(10, &first, 100));
        assert_eq!(sink.retained_scrollback_rows(), 1);

        let conflicting = Line::from_text("different-row-10", &attrs, 10, None);
        assert!(!sink.store_scrollback_line(10, &conflicting, 100));
        assert_eq!(
            sink.load_scrollback_line(10)
                .expect("original row remains authoritative after conflicting retry")
                .as_str()
                .as_ref(),
            "stable-row-10"
        );

        let gap = Line::from_text("stable-row-12", &attrs, 12, None);
        assert!(!sink.store_scrollback_line(12, &gap, 100));
        assert_eq!(sink.retained_scrollback_rows(), 1);
        assert!(sink.load_scrollback_line(11).is_none());
    }

    #[test]
    fn live_scrollback_spill_sink_rejects_corrupt_prefixed_record_as_content() {
        let dir = tempfile::tempdir().expect("temp scrollback dir");
        let context = config::ScrollbackSpillSinkContext {
            pane_id: 82,
            domain_id: 3,
            durable_pane_id: [82; 16],
            command_description: "corrupt-shell".to_string(),
        };
        let sink = LiveScrollbackSpillSink::new(dir.path().to_path_buf(), &context)
            .expect("create live spill sink");
        sink.lock_state("test corrupt record")
            .expect("test state lock")
            .initial_stable_row = Some(0);
        sink.lock_store("test corrupt record")
            .expect("test store lock")
            .append_line(sink.pane_id, "ftsl1u:not-valid-base64")
            .expect("append corrupt fixture");
        assert!(sink.load_scrollback_line(0).is_none());
    }

    #[test]
    fn live_scrollback_spill_sink_rejects_valid_json_manifest_tamper() {
        let dir = tempfile::tempdir().expect("temp scrollback dir");
        let context = config::ScrollbackSpillSinkContext {
            pane_id: 815,
            domain_id: 3,
            durable_pane_id: [89; 16],
            command_description: "manifest-tamper-shell".to_string(),
        };
        let manifest_path = {
            let sink = LiveScrollbackSpillSink::new(dir.path().to_path_buf(), &context)
                .expect("create live spill sink");
            let line = Line::from_text("checksum-bound-row", &CellAttributes::blank(), 1, None);
            assert!(sink.store_scrollback_line(0, &line, 8));
            sink.manifest_path.clone()
        };
        let mut manifest: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&manifest_path).unwrap()).unwrap();
        manifest["max_retained_rows"] = serde_json::json!(9);
        let mut tampered = serde_json::to_vec_pretty(&manifest).unwrap();
        tampered.push(b'\n');
        std::fs::write(&manifest_path, tampered).unwrap();

        let error = LiveScrollbackSpillSink::new(dir.path().to_path_buf(), &context)
            .expect_err("valid JSON with stale checksum must fail closed");
        assert!(error.to_string().contains("manifest checksum failed"));
    }

    #[cfg(unix)]
    #[test]
    fn live_scrollback_spill_sink_rejects_symlink_manifest() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().expect("temp scrollback dir");
        let context = config::ScrollbackSpillSinkContext {
            pane_id: 84,
            domain_id: 3,
            durable_pane_id: [84; 16],
            command_description: "symlink-shell".to_string(),
        };
        let durable_id = uuid::Uuid::from_bytes(context.durable_pane_id)
            .simple()
            .to_string();
        let pane_dir = dir.path().join(durable_id);
        std::fs::create_dir_all(&pane_dir).expect("create pane dir");
        let target = dir.path().join("attacker-manifest.json");
        std::fs::write(&target, b"{}\n").expect("write symlink target");
        symlink(&target, pane_dir.join("manifest.json")).expect("create manifest symlink");

        let error = LiveScrollbackSpillSink::new(dir.path().to_path_buf(), &context)
            .expect_err("manifest symlink must fail closed");
        assert!(error.to_string().contains("not a regular file"));
    }

    #[cfg(unix)]
    #[test]
    fn live_scrollback_spill_sink_rejects_symlink_pane_directory() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().expect("temp scrollback dir");
        let context = config::ScrollbackSpillSinkContext {
            pane_id: 85,
            domain_id: 3,
            durable_pane_id: [85; 16],
            command_description: "symlink-directory-shell".to_string(),
        };
        let durable_id = uuid::Uuid::from_bytes(context.durable_pane_id)
            .simple()
            .to_string();
        let target = dir.path().join("attacker-pane-directory");
        std::fs::create_dir(&target).expect("create symlink target dir");
        symlink(&target, dir.path().join(durable_id)).expect("create pane directory symlink");

        let error = LiveScrollbackSpillSink::new(dir.path().to_path_buf(), &context)
            .expect_err("pane directory symlink must fail closed");
        assert!(
            error.to_string().contains("not a directory")
                || error.to_string().contains("create private scrollback directory")
        );
    }

    #[test]
    fn live_scrollback_spill_sink_bounds_physical_storage_under_sustained_output() {
        let dir = tempfile::tempdir().expect("temp scrollback dir");
        let context = config::ScrollbackSpillSinkContext {
            pane_id: 9,
            domain_id: 3,
            durable_pane_id: [9; 16],
            command_description: "busy-shell".to_string(),
        };
        let sink = LiveScrollbackSpillSink::new(dir.path().to_path_buf(), &context)
            .expect("create live spill sink");
        let attrs = CellAttributes::blank();
        let max_retained_rows = 32;

        for row in 0..1024 {
            let text = format!("busy-row-{row:04}-{}", "payload".repeat(24));
            let line = Line::from_text(&text, &attrs, row, None);
            assert!(sink.store_scrollback_line(
                row as wezterm_term::StableRowIndex,
                &line,
                max_retained_rows,
            ));
        }

        assert_eq!(sink.retained_scrollback_rows(), max_retained_rows);
        let oldest_retained_row = (1024 - max_retained_rows) as wezterm_term::StableRowIndex;
        assert_eq!(sink.oldest_scrollback_row(), Some(oldest_retained_row));
        assert_eq!(
            sink.physical_scrollback_bytes(),
            sink.retained_scrollback_bytes() as u64,
            "test-threshold compaction should keep physical bytes to retained records"
        );
        assert!(
            sink.physical_scrollback_bytes() < 256 * 1024,
            "physical cold store should remain bounded after sustained output"
        );

        let oldest = sink
            .load_scrollback_line(oldest_retained_row)
            .expect("oldest retained row should hydrate");
        assert!(oldest.as_str().starts_with("busy-row-0992-"));
        assert!(sink.load_scrollback_line(991).is_none());
    }

    #[test]
    fn live_scrollback_spill_sink_clear_resets_retained_rows() {
        let dir = tempfile::tempdir().expect("temp scrollback dir");
        let context = config::ScrollbackSpillSinkContext {
            pane_id: 10,
            domain_id: 3,
            durable_pane_id: [10; 16],
            command_description: "clear-shell".to_string(),
        };
        let sink = LiveScrollbackSpillSink::new(dir.path().to_path_buf(), &context)
            .expect("create live spill sink");
        let attrs = CellAttributes::blank();
        let before = Line::from_text("before-clear", &attrs, 1, None);

        assert!(sink.store_scrollback_line(4, &before, 8));
        assert!(sink.load_scrollback_line(4).is_some());

        sink.clear_scrollback()
            .expect("logical scrollback clear should commit");

        assert_eq!(sink.retained_scrollback_rows(), 0);
        assert_eq!(sink.oldest_scrollback_row(), None);
        assert!(sink.load_scrollback_line(4).is_none());

        let after = Line::from_text("after-clear", &attrs, 2, None);
        assert!(sink.store_scrollback_line(20, &after, 8));

        assert!(sink.load_scrollback_line(4).is_none());
        assert_eq!(
            sink.load_scrollback_line(20)
                .expect("row stored after clear should hydrate")
                .as_str()
                .as_ref(),
            "after-clear"
        );
    }

    #[test]
    fn clear_retry_reuses_the_exact_deterministic_staged_generation() {
        let dir = tempfile::tempdir().expect("temp clear-stage retry dir");
        let context = config::ScrollbackSpillSinkContext {
            pane_id: 792,
            domain_id: 3,
            durable_pane_id: [72; 16],
            command_description: "clear-stage-retry-shell".to_string(),
        };
        let sink = LiveScrollbackSpillSink::new(dir.path().to_path_buf(), &context)
            .expect("create clear-stage retry sink");
        assert!(sink.store_scrollback_line(
            4,
            &Line::from_text("before-staged-clear", &CellAttributes::blank(), 1, None),
            8,
        ));
        let parent = sink
            .manifest_path
            .parent()
            .expect("clear-stage manifest has parent");
        let published_backup = parent.join("manifest-before-staged-clear");
        std::fs::rename(&sink.manifest_path, &published_backup)
            .expect("retain pre-clear published manifest");
        std::fs::create_dir(&sink.manifest_path).expect("block clear manifest rename");
        assert!(matches!(
            sink.clear_scrollback(),
            Err(wezterm_term::config::ScrollbackSpillError::StorageUnavailable)
        ));
        let staged = LiveScrollbackSpillSink::read_manifest(
            &LiveScrollbackSpillSink::deterministic_manifest_stage_path(&sink.manifest_path)
                .expect("derive clear-stage path"),
        )
        .expect("read deterministic clear stage")
        .expect("deterministic clear stage exists");
        assert_eq!(staged.publication_state, "cleared");
        let staged_epoch = staged.content_epoch.clone();
        std::fs::rename(
            &sink.manifest_path,
            parent.join("retained-clear-stage-blocker"),
        )
        .expect("retain clear-stage blocker without deleting it");
        std::fs::rename(&published_backup, &sink.manifest_path)
            .expect("restore pre-clear published manifest");

        sink.clear_scrollback()
            .expect("retry exact deterministic staged clear");
        let published = LiveScrollbackSpillSink::read_manifest(&sink.manifest_path)
            .expect("read retried clear manifest")
            .expect("retried clear manifest exists");
        assert_eq!(published.publication_state, "cleared");
        assert_eq!(published.content_epoch, staged_epoch);
        assert_eq!(sink.retained_scrollback_rows(), 0);
        assert!(sink.load_scrollback_line(4).is_none());
    }

    #[test]
    fn live_scrollback_snapshot_is_contiguous_bounded_and_exact_semantic() {
        let dir = tempfile::tempdir().expect("temp scrollback dir");
        let context = config::ScrollbackSpillSinkContext {
            pane_id: 13,
            domain_id: 3,
            durable_pane_id: [13; 16],
            command_description: "snapshot-shell".to_string(),
        };
        let sink = LiveScrollbackSpillSink::new(dir.path().to_path_buf(), &context)
            .expect("create live spill sink");
        let attrs = CellAttributes::blank();
        assert!(sink.store_scrollback_line(
            4,
            &Line::from_text("snapshot-row-four", &attrs, 1, None),
            8,
        ));
        assert!(sink.store_scrollback_line(
            5,
            &Line::from_text("snapshot-row-five", &attrs, 2, None),
            8,
        ));

        let limits = wezterm_term::config::ScrollbackSnapshotLimits {
            max_rows: 8,
            max_stored_bytes: 1024 * 1024,
            max_decoded_bytes: 1024 * 1024,
            max_physical_bytes: 1024 * 1024,
        };
        let snapshot = sink
            .snapshot_scrollback(6, limits)
            .expect("capture coherent exact snapshot");
        assert_eq!(
            snapshot.fidelity(),
            wezterm_term::config::ScrollbackSnapshotFidelity::ExactSemantic
        );
        assert_eq!(snapshot.oldest_stable_row(), Some(4));
        assert_eq!(snapshot.newest_stable_row_exclusive(), 6);
        assert_eq!(snapshot.rows().len(), 2);
        assert_eq!(snapshot.rows()[0].as_str().as_ref(), "snapshot-row-four");
        assert_eq!(snapshot.rows()[1].as_str().as_ref(), "snapshot-row-five");
        assert!(!format!("{snapshot:?}").contains("snapshot-row"));
        let bounded_error = sink
            .snapshot_scrollback(
                6,
                wezterm_term::config::ScrollbackSnapshotLimits {
                    max_decoded_bytes: 1,
                    ..limits
                },
            )
            .expect_err("exact row plaintext must obey the decoded-byte ceiling");
        assert!(matches!(
            bounded_error,
            wezterm_term::config::ScrollbackSpillError::ResourceLimit {
                resource: "decoded_bytes",
                ..
            }
        ));

        let generation_before_clear = snapshot.generation();
        let clear_commit = sink
            .clear_scrollback()
            .expect("logical scrollback clear should commit");
        assert_ne!(clear_commit.generation(), generation_before_clear);
        let empty = sink
            .snapshot_scrollback(6, limits)
            .expect("snapshot committed empty generation");
        assert!(empty.rows().is_empty());
        assert_eq!(empty.oldest_stable_row(), None);
        assert_eq!(
            empty.fidelity(),
            wezterm_term::config::ScrollbackSnapshotFidelity::ExactSemantic
        );

        let manifest = LiveScrollbackSpillSink::read_manifest(&sink.manifest_path)
            .expect("read authenticated cleared manifest")
            .expect("authenticated cleared manifest exists");
        assert_eq!(manifest.schema, LIVE_SCROLLBACK_MANIFEST_SCHEMA_V4);
        assert!(manifest.content_epoch.is_some());
        assert_eq!(manifest.revision, Some(0));
        assert_eq!(manifest.initial_stable_row, None);
        assert_eq!(manifest.max_retained_rows, 0);
        assert_eq!(manifest.oldest_seq, None);
        assert_eq!(manifest.retained_rows, 0);
        assert_eq!(manifest.next_seq, 0);
        assert!(manifest.logical_ledger_sha256.is_none());
        assert_eq!(manifest.chain_anchor_sha256, manifest.chain_tail_sha256);
        assert!(manifest.chain_anchor_sha256.is_some());
        assert!(manifest.guardian_manifest_authentication.is_some());
    }

    #[test]
    fn live_scrollback_spill_sink_fails_closed_on_poisoned_locks() {
        let dir = tempfile::tempdir().expect("temp scrollback dir");
        let context = config::ScrollbackSpillSinkContext {
            pane_id: 11,
            domain_id: 3,
            durable_pane_id: [11; 16],
            command_description: "poisoned-lock-shell".to_string(),
        };
        let sink = LiveScrollbackSpillSink::new(dir.path().to_path_buf(), &context)
            .expect("create live spill sink");
        let attrs = CellAttributes::blank();
        let first = Line::from_text("before-poison", &attrs, 1, None);

        let state_poisoned = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _state = sink.state.lock().expect("state lock for poison test");
            panic!("poison state lock for regression coverage");
        }));
        assert!(state_poisoned.is_err());
        assert!(sink.state.is_poisoned());
        assert!(!sink.store_scrollback_line(0, &first, 8));
        assert!(sink.state.is_poisoned());
        assert!(sink.load_scrollback_line(0).is_none());
        assert_eq!(sink.retained_scrollback_rows(), 0);
        assert!(sink.clear_scrollback().is_err());

        let store_context = config::ScrollbackSpillSinkContext {
            pane_id: 12,
            domain_id: 3,
            durable_pane_id: [12; 16],
            command_description: "poisoned-store-shell".to_string(),
        };
        let store_sink = LiveScrollbackSpillSink::new(dir.path().to_path_buf(), &store_context)
            .expect("create store-poison live spill sink");
        assert!(store_sink.store_scrollback_line(0, &first, 8));
        let manifest_before_store_poison = std::fs::read(&store_sink.manifest_path)
            .expect("read manifest before store poison");

        let store_poisoned = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _store = store_sink.store.lock().expect("store lock for poison test");
            panic!("poison store lock for regression coverage");
        }));
        assert!(store_poisoned.is_err());
        assert!(store_sink.store.is_poisoned());
        assert_eq!(store_sink.retained_scrollback_rows(), 0);
        assert!(store_sink.store.is_poisoned());
        assert!(store_sink.load_scrollback_line(0).is_none());
        assert!(store_sink.clear_scrollback().is_err());
        assert_eq!(
            std::fs::read(&store_sink.manifest_path)
                .expect("read manifest after rejected poisoned-store clear"),
            manifest_before_store_poison,
            "store unavailability must be detected before a clear manifest is published"
        );
    }

    #[test]
    fn scrollback_line_record_uses_zstd_for_repetitive_large_rows() {
        let attrs = CellAttributes::blank();
        let text = "z".repeat(4096);
        let line = Line::from_text(&text, &attrs, 77, None);
        let redactor = frankenterm_core::redactor::Redactor::new();

        let record =
            encode_scrollback_line_record(&line, &redactor).expect("encode scrollback record");

        assert!(
            record.starts_with(LIVE_SCROLLBACK_LINE_RECORD_V2_ZSTD),
            "large repetitive rows should use compressed record encoding"
        );
        let decoded =
            decode_scrollback_line_record(&record).expect("decode compressed scrollback record");
        assert_eq!(decoded.as_str().as_ref(), text);
        assert_eq!(decoded.current_seqno(), 77);
    }

    #[test]
    fn scrollback_line_record_rejects_checksum_mutation() {
        let attrs = CellAttributes::blank();
        let line = Line::from_text("checksum-bound-row", &attrs, 91, None);
        let redactor = frankenterm_core::redactor::Redactor::new();
        let mut record =
            encode_scrollback_line_record(&line, &redactor).expect("encode scrollback record");
        let checksum_index = LIVE_SCROLLBACK_LINE_RECORD_V2_UNCOMPRESSED.len();
        let replacement = if record.as_bytes()[checksum_index] == b'0' {
            "1"
        } else {
            "0"
        };
        record.replace_range(checksum_index..checksum_index + 1, replacement);
        assert!(decode_scrollback_line_record(&record).is_none());
    }

    #[test]
    fn client_domains_include_only_wezterm_ssh_domains() {
        let _state = ScopedTestState::acquire();

        let raw_ssh = SshDomain {
            name: "raw-ssh".to_string(),
            remote_address: "raw.example:22".to_string(),
            multiplexing: SshMultiplexing::None,
            ..SshDomain::default()
        };
        let mux_ssh = SshDomain {
            name: "mux-ssh".to_string(),
            remote_address: "mux.example:22".to_string(),
            multiplexing: SshMultiplexing::WezTerm,
            ..SshDomain::default()
        };

        let handle = make_test_handle(vec![raw_ssh, mux_ssh]);
        let domains = configured_client_domains(&handle);

        assert_eq!(
            domains.len(),
            1,
            "only multiplexed SSH should be client domains"
        );
        match &domains[0] {
            ClientDomainConfig::Ssh(ssh) => assert_eq!(ssh.name, "mux-ssh"),
            other => panic!("expected SSH client domain, got {other:?}"),
        }
    }

    #[test]
    fn update_mux_domains_registers_muxed_and_raw_ssh_domains() -> anyhow::Result<()> {
        let _state = ScopedTestState::acquire();

        let raw_ssh = SshDomain {
            name: "raw-ssh".to_string(),
            remote_address: "raw.example:22".to_string(),
            multiplexing: SshMultiplexing::None,
            ..SshDomain::default()
        };
        let mux_ssh = SshDomain {
            name: "mux-ssh".to_string(),
            remote_address: "mux.example:22".to_string(),
            multiplexing: SshMultiplexing::WezTerm,
            ..SshDomain::default()
        };
        let handle = make_test_handle(vec![raw_ssh, mux_ssh]);

        let local_domain: Arc<dyn Domain> = Arc::new(LocalDomain::new("local")?);
        let mux = Arc::new(Mux::new(Some(local_domain)));
        Mux::set_mux(&mux);

        update_mux_domains(&handle)?;

        let client_domain = mux
            .get_domain_by_name("mux-ssh")
            .expect("wezterm-multiplexed ssh domain should be registered");
        assert!(
            client_domain.is::<ClientDomain>(),
            "multiplexed SSH domain should use ClientDomain"
        );

        let raw_domain = mux
            .get_domain_by_name("raw-ssh")
            .expect("raw ssh domain should be registered");
        assert!(
            raw_domain.is::<RemoteSshDomain>(),
            "non-multiplexed SSH domain should use RemoteSshDomain"
        );

        Ok(())
    }

    #[test]
    fn client_domains_empty_config_returns_empty() {
        let _state = ScopedTestState::acquire();
        let handle = make_test_handle(vec![]);
        let domains = configured_client_domains(&handle);
        assert!(domains.is_empty(), "no ssh domains means no client domains");
    }

    #[test]
    fn update_mux_domains_with_no_ssh_registers_only_local() -> anyhow::Result<()> {
        let _state = ScopedTestState::acquire();
        let handle = make_test_handle(vec![]);

        let local_domain: Arc<dyn Domain> = Arc::new(LocalDomain::new("local")?);
        let mux = Arc::new(Mux::new(Some(local_domain)));
        Mux::set_mux(&mux);

        update_mux_domains(&handle)?;

        // Only the local domain should exist (no SSH domains registered)
        assert!(
            mux.get_domain_by_name("local").is_some(),
            "local domain should still be present"
        );

        Ok(())
    }

    #[test]
    fn update_mux_domains_idempotent_on_second_call() -> anyhow::Result<()> {
        let _state = ScopedTestState::acquire();

        let mux_ssh = SshDomain {
            name: "mux-ssh".to_string(),
            remote_address: "mux.example:22".to_string(),
            multiplexing: SshMultiplexing::WezTerm,
            ..SshDomain::default()
        };
        let handle = make_test_handle(vec![mux_ssh]);

        let local_domain: Arc<dyn Domain> = Arc::new(LocalDomain::new("local")?);
        let mux = Arc::new(Mux::new(Some(local_domain)));
        Mux::set_mux(&mux);

        update_mux_domains(&handle)?;
        let domain_first = mux
            .get_domain_by_name("mux-ssh")
            .expect("domain should exist after first call");

        // Call again — should not add a duplicate
        update_mux_domains(&handle)?;
        let domain_second = mux
            .get_domain_by_name("mux-ssh")
            .expect("domain should still exist after second call");

        // Same domain object (not re-created)
        assert_eq!(
            domain_first.domain_id(),
            domain_second.domain_id(),
            "second call should not create a new domain"
        );

        Ok(())
    }

    #[test]
    fn configured_client_policy_reload_preserves_exact_transport_generation()
    -> anyhow::Result<()> {
        let _state = ScopedTestState::acquire();

        let initial = SshDomain {
            name: "policy-only-client".to_string(),
            remote_address: "policy.example:22".to_string(),
            multiplexing: SshMultiplexing::WezTerm,
            connect_automatically: false,
            local_echo_threshold_ms: Some(15),
            overlay_lag_indicator: false,
            ..SshDomain::default()
        };
        let initial_handle = make_test_handle(vec![initial.clone()]);
        let local_domain: Arc<dyn Domain> = Arc::new(LocalDomain::new("local")?);
        let mux = Arc::new(Mux::new(Some(local_domain)));
        Mux::set_mux(&mux);
        update_mux_domains(&initial_handle)?;
        let initial_id = mux
            .get_domain_by_name("policy-only-client")
            .expect("initial policy domain exists")
            .domain_id();

        let changed_policy = SshDomain {
            connect_automatically: true,
            local_echo_threshold_ms: Some(75),
            overlay_lag_indicator: true,
            ..initial
        };
        let changed_handle = make_test_handle(vec![changed_policy]);
        assert_eq!(
            reconcile_configured_client_domain(
                &changed_handle,
                &mux,
                "policy-only-client",
            )?,
            ConfiguredClientDomainReconcileOutcome::Current,
        );
        let current = mux
            .get_domain_by_name("policy-only-client")
            .expect("policy update preserves client registration");
        assert_eq!(current.domain_id(), initial_id);
        assert!(
            current
                .downcast_ref::<ClientDomain>()
                .expect("policy domain remains a ClientDomain")
                .connect_automatically(),
        );
        Ok(())
    }

    #[test]
    fn configured_client_reconciliation_retries_exact_retirement_then_registers_fresh_config()
    -> anyhow::Result<()> {
        let _state = ScopedTestState::acquire();

        let first_ssh = SshDomain {
            name: "reconfigured-client".to_string(),
            remote_address: "old.example:22".to_string(),
            multiplexing: SshMultiplexing::WezTerm,
            ..SshDomain::default()
        };
        let first_handle = make_test_handle(vec![first_ssh]);
        let local_domain: Arc<dyn Domain> = Arc::new(LocalDomain::new("local")?);
        let mux = Arc::new(Mux::new(Some(local_domain)));
        Mux::set_mux(&mux);
        update_mux_domains(&first_handle)?;

        let retirement_barrier = mux
            .get_domain_by_name("reconfigured-client")
            .expect("first client generation is registered");
        let first_id = retirement_barrier.domain_id();
        let replacement_ssh = SshDomain {
            name: "reconfigured-client".to_string(),
            remote_address: "new.example:22".to_string(),
            multiplexing: SshMultiplexing::WezTerm,
            ..SshDomain::default()
        };
        let replacement_handle = make_test_handle(vec![replacement_ssh]);

        assert_eq!(
            reconcile_configured_client_domain(
                &replacement_handle,
                &mux,
                "reconfigured-client",
            )?,
            ConfiguredClientDomainReconcileOutcome::PendingRetirement,
            "changed configuration must retire the admitted old generation",
        );
        assert!(
            mux.get_domain_by_name("reconfigured-client").is_none(),
            "logical retirement must close stale-domain admission immediately",
        );
        assert_eq!(
            reconcile_configured_client_domain(
                &replacement_handle,
                &mux,
                "reconfigured-client",
            )?,
            ConfiguredClientDomainReconcileOutcome::PendingRetirement,
            "the same-name fence must remain retryable while an old guard drains",
        );

        drop(retirement_barrier);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            match reconcile_configured_client_domain(
                &replacement_handle,
                &mux,
                "reconfigured-client",
            )? {
                ConfiguredClientDomainReconcileOutcome::Registered => break,
                ConfiguredClientDomainReconcileOutcome::PendingRetirement => {
                    assert!(
                        std::time::Instant::now() < deadline,
                        "retired client generation did not release its same-name fence",
                    );
                    std::thread::yield_now();
                }
                outcome => panic!("unexpected replacement outcome: {outcome:?}"),
            }
        }

        let replacement = mux
            .get_domain_by_name("reconfigured-client")
            .expect("fresh configured client generation is registered");
        assert_ne!(replacement.domain_id(), first_id);
        let replacement_client = replacement
            .downcast_ref::<ClientDomain>()
            .expect("replacement uses ClientDomain");
        let expected = configured_client_domains(&replacement_handle)
            .into_iter()
            .next()
            .expect("replacement config exists");
        assert!(replacement_client.reconcile_configuration(&expected));
        Ok(())
    }

    #[test]
    fn aggregate_reconciliation_publishes_independent_domain_and_default_while_client_retires()
    -> anyhow::Result<()> {
        let _state = ScopedTestState::acquire();

        let first_client = SshDomain {
            name: "retiring-client".to_string(),
            remote_address: "old.example:22".to_string(),
            multiplexing: SshMultiplexing::WezTerm,
            ..SshDomain::default()
        };
        let first_handle = make_test_handle(vec![first_client]);
        let local_domain: Arc<dyn Domain> = Arc::new(LocalDomain::new("local")?);
        let mux = Arc::new(Mux::new(Some(local_domain)));
        Mux::set_mux(&mux);
        update_mux_domains(&first_handle)?;
        let retirement_barrier = mux
            .get_domain_by_name("retiring-client")
            .expect("first client generation is registered");
        let retired_id = retirement_barrier.domain_id();

        let replacement_client = SshDomain {
            name: "retiring-client".to_string(),
            remote_address: "new.example:22".to_string(),
            multiplexing: SshMultiplexing::WezTerm,
            ..SshDomain::default()
        };
        let independent_raw = SshDomain {
            name: "new-raw-default".to_string(),
            remote_address: "raw.example:22".to_string(),
            multiplexing: SshMultiplexing::None,
            ..SshDomain::default()
        };
        let replacement_handle =
            make_test_handle_with(vec![replacement_client, independent_raw], |config| {
                config.default_domain = Some("new-raw-default".to_string());
            });

        assert_eq!(
            reconcile_mux_domains(&replacement_handle)?,
            MuxDomainUpdateOutcome::PendingRetirements {
                domain_names: vec!["retiring-client".to_string()],
            }
        );
        assert!(
            mux.get_domain_by_name("new-raw-default").is_some(),
            "a pending client retirement must not suppress a later independent domain"
        );
        assert_eq!(
            mux.default_domain()?.domain_name(),
            "new-raw-default",
            "a safe independent default must publish during the pending pass"
        );

        drop(retirement_barrier);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            match reconcile_mux_domains(&replacement_handle)? {
                MuxDomainUpdateOutcome::Converged => break,
                MuxDomainUpdateOutcome::PendingRetirements { .. } => {
                    assert!(
                        std::time::Instant::now() < deadline,
                        "replacement client did not converge after its exact guard drained"
                    );
                    std::thread::yield_now();
                }
            }
        }
        let successor = mux
            .get_domain_by_name("retiring-client")
            .expect("fresh client generation is registered after retirement");
        assert_ne!(successor.domain_id(), retired_id);
        Ok(())
    }

    #[test]
    fn aggregate_reconciliation_retires_client_deleted_from_configuration()
    -> anyhow::Result<()> {
        let _state = ScopedTestState::acquire();

        let deleted_client = SshDomain {
            name: "deleted-client".to_string(),
            remote_address: "deleted.example:22".to_string(),
            multiplexing: SshMultiplexing::WezTerm,
            ..SshDomain::default()
        };
        let initial_handle = make_test_handle(vec![deleted_client]);
        let local_domain: Arc<dyn Domain> = Arc::new(LocalDomain::new("local")?);
        let mux = Arc::new(Mux::new(Some(local_domain)));
        Mux::set_mux(&mux);
        update_mux_domains(&initial_handle)?;
        let deleted_generation = mux
            .get_domain_by_name("deleted-client")
            .expect("configured client generation is registered");

        let deleted_handle = make_test_handle(vec![]);
        assert_eq!(
            reconcile_mux_domains(&deleted_handle)?,
            MuxDomainUpdateOutcome::Converged,
        );
        assert!(
            mux.get_domain_by_name("deleted-client").is_none(),
            "configuration deletion must close name-based admission immediately"
        );
        assert_eq!(deleted_generation.state(), mux::domain::DomainState::Detached);
        Ok(())
    }

    #[test]
    fn aggregate_reconciliation_retries_raw_to_client_same_name_transition()
    -> anyhow::Result<()> {
        let _state = ScopedTestState::acquire();

        let initial_raw = SshDomain {
            name: "raw-to-client".to_string(),
            remote_address: "shell.example:22".to_string(),
            multiplexing: SshMultiplexing::None,
            ..SshDomain::default()
        };
        let initial_handle = make_test_handle(vec![initial_raw]);
        let local_domain: Arc<dyn Domain> = Arc::new(LocalDomain::new("local")?);
        let mux = Arc::new(Mux::new(Some(local_domain)));
        Mux::set_mux(&mux);
        update_mux_domains(&initial_handle)?;
        let retirement_barrier = mux
            .get_domain_by_name("raw-to-client")
            .expect("initial raw generation is registered");
        let retired_id = retirement_barrier.domain_id();

        let replacement_client = SshDomain {
            name: "raw-to-client".to_string(),
            remote_address: "mux.example:22".to_string(),
            multiplexing: SshMultiplexing::WezTerm,
            ..SshDomain::default()
        };
        let replacement_handle = make_test_handle(vec![replacement_client]);
        assert_eq!(
            reconcile_configured_client_domain(
                &replacement_handle,
                &mux,
                "raw-to-client",
            )?,
            ConfiguredClientDomainReconcileOutcome::PendingRetirement,
            "raw-to-client replacement must retire instead of colliding",
        );
        assert!(
            mux.get_domain_by_name("raw-to-client").is_none(),
            "logical retirement must close stale raw transport admission"
        );

        drop(retirement_barrier);
        wait_for_domain_reconciliation(&replacement_handle)?;
        let replacement = mux
            .get_domain_by_name("raw-to-client")
            .expect("client replacement is registered");
        assert_ne!(replacement.domain_id(), retired_id);
        assert!(replacement.is::<ClientDomain>());
        assert!(replacement.downcast_ref::<RemoteSshDomain>().is_none());
        Ok(())
    }

    #[test]
    fn aggregate_reconciliation_replaces_changed_config_for_every_raw_domain_class()
    -> anyhow::Result<()> {
        let _state = ScopedTestState::acquire();

        let first_ssh = SshDomain {
            name: "changed-raw-ssh".to_string(),
            remote_address: "old-ssh.example:22".to_string(),
            multiplexing: SshMultiplexing::None,
            ..SshDomain::default()
        };
        let first_wsl = WslDomain {
            name: "changed-wsl".to_string(),
            distribution: Some("OldDistro".to_string()),
            username: Some("old-user".to_string()),
            default_cwd: Some("/old".into()),
            default_prog: Some(vec!["old-shell".to_string()]),
        };
        let first_exec = ExecDomain {
            name: "changed-exec".to_string(),
            fixup_command: "old-fixup".to_string(),
            label: None,
        };
        let first_serial = SerialDomain {
            name: "changed-serial".to_string(),
            port: Some("test-old-port".to_string()),
            baud: Some(9_600),
        };
        let first_handle = make_test_handle_with(vec![first_ssh], |config| {
            config.wsl_domains = Some(vec![first_wsl]);
            config.exec_domains = vec![first_exec];
            config.serial_ports = vec![first_serial];
        });
        let local_domain: Arc<dyn Domain> = Arc::new(LocalDomain::new("local")?);
        let mux = Arc::new(Mux::new(Some(local_domain)));
        Mux::set_mux(&mux);
        update_mux_domains(&first_handle)?;

        let barriers = [
            "changed-raw-ssh",
            "changed-wsl",
            "changed-exec",
            "changed-serial",
        ]
        .into_iter()
        .map(|name| {
            mux.get_domain_by_name(name)
                .unwrap_or_else(|| panic!("initial {name} generation is registered"))
        })
        .collect::<Vec<_>>();
        let retired_ids = barriers
            .iter()
            .map(|domain| (domain.domain_name().to_string(), domain.domain_id()))
            .collect::<BTreeMap<_, _>>();

        let replacement_ssh = SshDomain {
            name: "changed-raw-ssh".to_string(),
            remote_address: "new-ssh.example:2200".to_string(),
            multiplexing: SshMultiplexing::None,
            timeout: std::time::Duration::from_secs(17),
            ..SshDomain::default()
        };
        let replacement_wsl = WslDomain {
            name: "changed-wsl".to_string(),
            distribution: Some("NewDistro".to_string()),
            username: Some("new-user".to_string()),
            default_cwd: Some("/new".into()),
            default_prog: Some(vec!["new-shell".to_string()]),
        };
        let replacement_exec = ExecDomain {
            name: "changed-exec".to_string(),
            fixup_command: "new-fixup".to_string(),
            label: Some(config::ValueOrFunc::Func("new-label".to_string())),
        };
        let replacement_serial = SerialDomain {
            name: "changed-serial".to_string(),
            port: Some("test-new-port".to_string()),
            baud: Some(115_200),
        };
        let expected_ssh = replacement_ssh.clone();
        let expected_wsl = replacement_wsl.clone();
        let expected_exec = replacement_exec.clone();
        let expected_serial = replacement_serial.clone();
        let replacement_handle = make_test_handle_with(vec![replacement_ssh], |config| {
            config.wsl_domains = Some(vec![replacement_wsl]);
            config.exec_domains = vec![replacement_exec];
            config.serial_ports = vec![replacement_serial];
        });

        assert_eq!(
            reconcile_mux_domains(&replacement_handle)?,
            MuxDomainUpdateOutcome::PendingRetirements {
                domain_names: vec![
                    "changed-exec".to_string(),
                    "changed-raw-ssh".to_string(),
                    "changed-serial".to_string(),
                    "changed-wsl".to_string(),
                ],
            }
        );
        for name in retired_ids.keys() {
            assert!(
                mux.get_domain_by_name(name).is_none(),
                "stale {name} generation must become inadmissible immediately"
            );
        }

        drop(barriers);
        wait_for_domain_reconciliation(&replacement_handle)?;

        let ssh = mux
            .get_domain_by_name("changed-raw-ssh")
            .expect("replacement raw SSH generation exists");
        assert_ne!(ssh.domain_id(), retired_ids["changed-raw-ssh"]);
        assert!(
            ssh.downcast_ref::<RemoteSshDomain>()
                .is_some_and(|domain| domain.matches_configuration(&expected_ssh))
        );

        let wsl = mux
            .get_domain_by_name("changed-wsl")
            .expect("replacement WSL generation exists");
        assert_ne!(wsl.domain_id(), retired_ids["changed-wsl"]);
        assert!(
            wsl.downcast_ref::<LocalDomain>()
                .is_some_and(|domain| domain.matches_wsl_configuration(&expected_wsl))
        );

        let exec = mux
            .get_domain_by_name("changed-exec")
            .expect("replacement exec generation exists");
        assert_ne!(exec.domain_id(), retired_ids["changed-exec"]);
        assert!(
            exec.downcast_ref::<LocalDomain>()
                .is_some_and(|domain| domain.matches_exec_configuration(&expected_exec))
        );

        let serial = mux
            .get_domain_by_name("changed-serial")
            .expect("replacement serial generation exists");
        assert_ne!(serial.domain_id(), retired_ids["changed-serial"]);
        assert!(
            serial
                .downcast_ref::<LocalDomain>()
                .is_some_and(|domain| domain.matches_serial_configuration(&expected_serial))
        );
        Ok(())
    }

    #[test]
    fn aggregate_reconciliation_replaces_ssh_domains_when_global_backend_changes()
    -> anyhow::Result<()> {
        let _state = ScopedTestState::acquire();

        let raw = SshDomain {
            name: "backend-raw".to_string(),
            remote_address: "raw-backend.example:22".to_string(),
            multiplexing: SshMultiplexing::None,
            ssh_backend: None,
            ..SshDomain::default()
        };
        let client = SshDomain {
            name: "backend-client".to_string(),
            remote_address: "client-backend.example:22".to_string(),
            multiplexing: SshMultiplexing::WezTerm,
            ssh_backend: None,
            ..SshDomain::default()
        };
        let first_handle =
            make_test_handle_with(vec![raw.clone(), client.clone()], |config| {
                config.ssh_backend = config::SshBackend::LibSsh;
            });
        let local_domain: Arc<dyn Domain> = Arc::new(LocalDomain::new("local")?);
        let mux = Arc::new(Mux::new(Some(local_domain)));
        Mux::set_mux(&mux);
        update_mux_domains(&first_handle)?;

        let barriers = ["backend-raw", "backend-client"]
            .into_iter()
            .map(|name| {
                mux.get_domain_by_name(name)
                    .unwrap_or_else(|| panic!("initial {name} generation exists"))
            })
            .collect::<Vec<_>>();
        let retired_ids = barriers
            .iter()
            .map(|domain| (domain.domain_name().to_string(), domain.domain_id()))
            .collect::<BTreeMap<_, _>>();

        let replacement_handle = make_test_handle_with(vec![raw.clone(), client], |config| {
            config.ssh_backend = config::SshBackend::Ssh2;
        });
        assert_eq!(
            reconcile_mux_domains(&replacement_handle)?,
            MuxDomainUpdateOutcome::PendingRetirements {
                domain_names: vec!["backend-client".to_string(), "backend-raw".to_string()],
            }
        );
        assert!(mux.get_domain_by_name("backend-raw").is_none());
        assert!(mux.get_domain_by_name("backend-client").is_none());

        drop(barriers);
        wait_for_domain_reconciliation(&replacement_handle)?;

        let mut expected_raw = raw;
        expected_raw.ssh_backend = Some(config::SshBackend::Ssh2);
        let replacement_raw = mux
            .get_domain_by_name("backend-raw")
            .expect("raw SSH backend successor exists");
        assert_ne!(replacement_raw.domain_id(), retired_ids["backend-raw"]);
        assert!(
            replacement_raw
                .downcast_ref::<RemoteSshDomain>()
                .is_some_and(|domain| domain.matches_configuration(&expected_raw))
        );

        let expected_client = configured_client_domains(&replacement_handle)
            .into_iter()
            .find(|domain| domain.name() == "backend-client")
            .expect("normalized client SSH configuration exists");
        let replacement_client = mux
            .get_domain_by_name("backend-client")
            .expect("client SSH backend successor exists");
        assert_ne!(
            replacement_client.domain_id(),
            retired_ids["backend-client"]
        );
        assert!(
            replacement_client
                .downcast_ref::<ClientDomain>()
                .is_some_and(|domain| domain.reconcile_configuration(&expected_client))
        );
        Ok(())
    }

    #[test]
    fn aggregate_reconciliation_retries_cross_raw_kind_transition()
    -> anyhow::Result<()> {
        let _state = ScopedTestState::acquire();

        let initial_wsl = WslDomain {
            name: "cross-raw-kind".to_string(),
            distribution: Some("OldDistro".to_string()),
            username: None,
            default_cwd: None,
            default_prog: None,
        };
        let initial_handle = make_test_handle_with(Vec::new(), |config| {
            config.wsl_domains = Some(vec![initial_wsl]);
        });
        let local_domain: Arc<dyn Domain> = Arc::new(LocalDomain::new("local")?);
        let mux = Arc::new(Mux::new(Some(local_domain)));
        Mux::set_mux(&mux);
        update_mux_domains(&initial_handle)?;
        let retirement_barrier = mux
            .get_domain_by_name("cross-raw-kind")
            .expect("initial WSL generation exists");
        let retired_id = retirement_barrier.domain_id();

        let replacement_exec = ExecDomain {
            name: "cross-raw-kind".to_string(),
            fixup_command: "exec-replacement".to_string(),
            label: None,
        };
        let expected_exec = replacement_exec.clone();
        let replacement_handle = make_test_handle_with(Vec::new(), |config| {
            config.exec_domains = vec![replacement_exec];
        });
        assert_eq!(
            reconcile_mux_domains(&replacement_handle)?,
            MuxDomainUpdateOutcome::PendingRetirements {
                domain_names: vec!["cross-raw-kind".to_string()],
            }
        );
        assert!(mux.get_domain_by_name("cross-raw-kind").is_none());

        drop(retirement_barrier);
        wait_for_domain_reconciliation(&replacement_handle)?;
        let replacement = mux
            .get_domain_by_name("cross-raw-kind")
            .expect("exec successor exists");
        assert_ne!(replacement.domain_id(), retired_id);
        let local = replacement
            .downcast_ref::<LocalDomain>()
            .expect("exec successor is a local domain");
        assert!(local.matches_exec_configuration(&expected_exec));
        assert!(!local.matches_wsl_configuration(&WslDomain {
            name: "cross-raw-kind".to_string(),
            distribution: Some("OldDistro".to_string()),
            username: None,
            default_cwd: None,
            default_prog: None,
        }));
        Ok(())
    }

    #[test]
    fn aggregate_reconciliation_logically_removes_deleted_raw_domain_classes()
    -> anyhow::Result<()> {
        let _state = ScopedTestState::acquire();

        let raw_ssh = SshDomain {
            name: "deleted-raw-ssh".to_string(),
            remote_address: "deleted.example:22".to_string(),
            multiplexing: SshMultiplexing::None,
            ..SshDomain::default()
        };
        let wsl = WslDomain {
            name: "deleted-wsl".to_string(),
            distribution: Some("DeletedDistro".to_string()),
            username: None,
            default_cwd: None,
            default_prog: None,
        };
        let exec = ExecDomain {
            name: "deleted-exec".to_string(),
            fixup_command: "deleted-fixup".to_string(),
            label: None,
        };
        let serial = SerialDomain {
            name: "deleted-serial".to_string(),
            port: Some("deleted-test-port".to_string()),
            baud: Some(9_600),
        };
        let initial_handle = make_test_handle_with(vec![raw_ssh], |config| {
            config.wsl_domains = Some(vec![wsl]);
            config.exec_domains = vec![exec];
            config.serial_ports = vec![serial];
        });
        let local_domain: Arc<dyn Domain> = Arc::new(LocalDomain::new("local")?);
        let local_id = local_domain.domain_id();
        let mux = Arc::new(Mux::new(Some(local_domain)));
        Mux::set_mux(&mux);
        update_mux_domains(&initial_handle)?;

        let deleted_names = [
            "deleted-raw-ssh",
            "deleted-wsl",
            "deleted-exec",
            "deleted-serial",
        ];
        let retirement_barriers = deleted_names
            .iter()
            .map(|name| {
                mux.get_domain_by_name(name)
                    .unwrap_or_else(|| panic!("initial {name} generation exists"))
            })
            .collect::<Vec<_>>();
        let empty_handle = make_test_handle(Vec::new());
        assert_eq!(
            reconcile_mux_domains(&empty_handle)?,
            MuxDomainUpdateOutcome::Converged,
            "deletion needs no same-name successor and converges after logical retirement"
        );
        for name in deleted_names {
            assert!(
                mux.get_domain_by_name(name).is_none(),
                "deleted configured domain {name} must not remain addressable"
            );
        }
        assert_eq!(
            mux.get_domain_by_name("local")
                .expect("runtime local domain is preserved")
                .domain_id(),
            local_id
        );
        drop(retirement_barriers);
        Ok(())
    }

    #[test]
    fn aggregate_reconciliation_preserves_runtime_created_raw_domains()
    -> anyhow::Result<()> {
        let _state = ScopedTestState::acquire();

        let local_domain: Arc<dyn Domain> = Arc::new(LocalDomain::new("local")?);
        let mux = Arc::new(Mux::new(Some(local_domain)));
        Mux::set_mux(&mux);

        let runtime_ssh_config = SshDomain {
            name: "runtime-raw-ssh".to_string(),
            remote_address: "runtime.example:22".to_string(),
            multiplexing: SshMultiplexing::None,
            ..SshDomain::default()
        };
        let runtime_ssh: Arc<dyn Domain> = Arc::new(RemoteSshDomain::with_ssh_domain(
            &runtime_ssh_config,
        )?);
        let runtime_ssh_id = runtime_ssh.domain_id();
        mux.add_domain(&runtime_ssh)?;

        let runtime_serial_config = SerialDomain {
            name: "runtime-serial".to_string(),
            port: Some("runtime-test-port".to_string()),
            baud: Some(57_600),
        };
        let runtime_serial: Arc<dyn Domain> = Arc::new(LocalDomain::new_serial_domain(
            runtime_serial_config,
        )?);
        let runtime_serial_id = runtime_serial.domain_id();
        mux.add_domain(&runtime_serial)?;

        let unrelated_reload = make_test_handle(Vec::new());
        assert_eq!(
            reconcile_mux_domains(&unrelated_reload)?,
            MuxDomainUpdateOutcome::Converged
        );
        assert_eq!(
            mux.get_domain_by_name("runtime-raw-ssh")
                .expect("unrelated reload preserves ad-hoc SSH domain")
                .domain_id(),
            runtime_ssh_id
        );
        assert_eq!(
            mux.get_domain_by_name("runtime-serial")
                .expect("unrelated reload preserves ad-hoc serial domain")
                .domain_id(),
            runtime_serial_id
        );
        Ok(())
    }

    #[test]
    fn aggregate_reconciliation_refuses_to_adopt_runtime_raw_ssh_as_configured()
    -> anyhow::Result<()> {
        let _state = ScopedTestState::acquire();

        let runtime_ssh_config = SshDomain {
            name: "runtime-raw-collision".to_string(),
            remote_address: "runtime.example:22".to_string(),
            multiplexing: SshMultiplexing::None,
            ..SshDomain::default()
        };
        let local_domain: Arc<dyn Domain> = Arc::new(LocalDomain::new("local")?);
        let mux = Arc::new(Mux::new(Some(local_domain)));
        Mux::set_mux(&mux);
        let runtime_ssh: Arc<dyn Domain> = Arc::new(RemoteSshDomain::with_ssh_domain(
            &runtime_ssh_config,
        )?);
        let runtime_ssh_id = runtime_ssh.domain_id();
        mux.add_domain(&runtime_ssh)?;

        let conflicting_reload = make_test_handle(vec![runtime_ssh_config]);
        let error = reconcile_mux_domains(&conflicting_reload)
            .expect_err("runtime raw SSH must not be adopted as configuration-owned");
        assert!(format!("{error:#}").contains("runtime-owned"));
        let preserved = mux
            .get_domain_by_name("runtime-raw-collision")
            .expect("runtime raw SSH remains live after rejected reload");
        assert_eq!(preserved.domain_id(), runtime_ssh_id);
        assert!(
            preserved
                .downcast_ref::<RemoteSshDomain>()
                .is_some_and(|domain| !domain.is_configuration_owned())
        );
        Ok(())
    }

    #[test]
    fn aggregate_reconciliation_refuses_to_replace_runtime_owned_local_domain()
    -> anyhow::Result<()> {
        let _state = ScopedTestState::acquire();

        let local_domain: Arc<dyn Domain> = Arc::new(LocalDomain::new("local")?);
        let local_id = local_domain.domain_id();
        let mux = Arc::new(Mux::new(Some(local_domain)));
        Mux::set_mux(&mux);
        let conflicting_wsl = WslDomain {
            name: "local".to_string(),
            distribution: Some("MustNotReplaceRuntime".to_string()),
            username: None,
            default_cwd: None,
            default_prog: None,
        };
        let handle = make_test_handle_with(Vec::new(), |config| {
            config.wsl_domains = Some(vec![conflicting_wsl]);
        });

        let error = reconcile_mux_domains(&handle)
            .expect_err("runtime-owned local domain collision must fail before retirement");
        assert!(format!("{error:#}").contains("runtime-owned"));
        assert_eq!(
            mux.get_domain_by_name("local")
                .expect("runtime local domain remains live")
                .domain_id(),
            local_id
        );
        Ok(())
    }

    #[test]
    fn aggregate_reconciliation_retries_client_to_raw_same_name_transition()
    -> anyhow::Result<()> {
        let _state = ScopedTestState::acquire();

        let initial_client = SshDomain {
            name: "client-to-raw".to_string(),
            remote_address: "mux.example:22".to_string(),
            multiplexing: SshMultiplexing::WezTerm,
            ..SshDomain::default()
        };
        let initial_handle = make_test_handle(vec![initial_client]);
        let local_domain: Arc<dyn Domain> = Arc::new(LocalDomain::new("local")?);
        let mux = Arc::new(Mux::new(Some(local_domain)));
        Mux::set_mux(&mux);
        update_mux_domains(&initial_handle)?;
        let retirement_barrier = mux
            .get_domain_by_name("client-to-raw")
            .expect("initial client generation is registered");
        let retired_id = retirement_barrier.domain_id();

        let replacement_raw = SshDomain {
            name: "client-to-raw".to_string(),
            remote_address: "shell.example:22".to_string(),
            multiplexing: SshMultiplexing::None,
            ..SshDomain::default()
        };
        let expected_raw = replacement_raw.clone();
        let replacement_handle =
            make_test_handle_with(vec![replacement_raw], |config| {
                config.default_domain = Some("client-to-raw".to_string());
            });
        assert_eq!(
            reconcile_mux_domains(&replacement_handle)?,
            MuxDomainUpdateOutcome::PendingRetirements {
                domain_names: vec!["client-to-raw".to_string()],
            },
            "the old exact name fence is retryable, not a terminal reload error",
        );
        assert!(mux.get_domain_by_name("client-to-raw").is_none());

        drop(retirement_barrier);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            match reconcile_mux_domains(&replacement_handle)? {
                MuxDomainUpdateOutcome::Converged => break,
                MuxDomainUpdateOutcome::PendingRetirements { .. } => {
                    assert!(
                        std::time::Instant::now() < deadline,
                        "raw replacement did not converge after the client name fence drained",
                    );
                    std::thread::yield_now();
                }
            }
        }
        let replacement = mux
            .get_domain_by_name("client-to-raw")
            .expect("raw replacement is registered");
        assert_ne!(replacement.domain_id(), retired_id);
        assert!(replacement.is::<RemoteSshDomain>());
        assert!(replacement.downcast_ref::<ClientDomain>().is_none());
        assert!(
            replacement
                .downcast_ref::<RemoteSshDomain>()
                .is_some_and(|domain| domain.matches_configuration(&expected_raw)),
            "replacement must capture the desired raw transport configuration"
        );
        assert_eq!(mux.default_domain()?.domain_name(), "client-to-raw");
        Ok(())
    }

    #[test]
    fn client_domains_with_only_raw_ssh_returns_empty() {
        let _state = ScopedTestState::acquire();

        let raw_ssh = SshDomain {
            name: "raw-only".to_string(),
            remote_address: "raw.example:22".to_string(),
            multiplexing: SshMultiplexing::None,
            ..SshDomain::default()
        };
        let handle = make_test_handle(vec![raw_ssh]);
        let domains = configured_client_domains(&handle);

        assert!(
            domains.is_empty(),
            "raw SSH domains should not appear in client_domains"
        );
    }

    #[test]
    fn update_mux_domains_for_server_respects_mux_server_domain() -> anyhow::Result<()> {
        let _state = ScopedTestState::acquire();

        let raw_ssh = SshDomain {
            name: "raw-ssh".to_string(),
            remote_address: "raw.example:22".to_string(),
            multiplexing: SshMultiplexing::None,
            ..SshDomain::default()
        };
        let handle = make_test_handle(vec![raw_ssh]);

        let local_domain: Arc<dyn Domain> = Arc::new(LocalDomain::new("local")?);
        let mux = Arc::new(Mux::new(Some(local_domain)));
        Mux::set_mux(&mux);

        // update_mux_domains_for_server should work the same as update_mux_domains
        // for domain registration (the difference is in default_domain handling)
        update_mux_domains_for_server(&handle)?;

        let domain = mux
            .get_domain_by_name("raw-ssh")
            .expect("raw SSH domain should be registered by server variant");
        assert!(
            domain.is::<RemoteSshDomain>(),
            "should use RemoteSshDomain for non-multiplexed SSH"
        );

        Ok(())
    }

    #[test]
    fn update_mux_domains_honors_explicit_default_domain() -> anyhow::Result<()> {
        let _state = ScopedTestState::acquire();
        let raw_ssh = SshDomain {
            name: "configured-default".to_string(),
            remote_address: "default.example:22".to_string(),
            multiplexing: SshMultiplexing::None,
            ..SshDomain::default()
        };
        let handle = make_test_handle_with(vec![raw_ssh], |config| {
            config.default_domain = Some("configured-default".to_string());
        });
        let local_domain: Arc<dyn Domain> = Arc::new(LocalDomain::new("local")?);
        let mux = Arc::new(Mux::new(Some(local_domain)));
        Mux::set_mux(&mux);

        update_mux_domains(&handle)?;

        assert_eq!(mux.default_domain()?.domain_name(), "configured-default");
        Ok(())
    }

    #[test]
    fn update_mux_domains_rejects_missing_default_domain_with_key_and_value() -> anyhow::Result<()>
    {
        let _state = ScopedTestState::acquire();
        let handle = make_test_handle_with(vec![], |config| {
            config.default_domain = Some("missing-client-default".to_string());
        });
        let local_domain: Arc<dyn Domain> = Arc::new(LocalDomain::new("local")?);
        let mux = Arc::new(Mux::new(Some(local_domain)));
        Mux::set_mux(&mux);

        let error = update_mux_domains(&handle).expect_err("missing configured default must fail");
        let rendered = format!("{error:#}");
        assert!(rendered.contains("default_domain"));
        assert!(rendered.contains("missing-client-default"));
        Ok(())
    }

    #[test]
    fn update_mux_domains_reload_reports_missing_default_instead_of_stale_success()
    -> anyhow::Result<()> {
        let _state = ScopedTestState::acquire();
        let first = SshDomain {
            name: "first-default".to_string(),
            remote_address: "first.example:22".to_string(),
            multiplexing: SshMultiplexing::None,
            ..SshDomain::default()
        };
        let first_handle = make_test_handle_with(vec![first], |config| {
            config.default_domain = Some("first-default".to_string());
        });
        let local_domain: Arc<dyn Domain> = Arc::new(LocalDomain::new("local")?);
        let mux = Arc::new(Mux::new(Some(local_domain)));
        Mux::set_mux(&mux);
        update_mux_domains(&first_handle)?;
        assert_eq!(mux.default_domain()?.domain_name(), "first-default");

        let reloaded_handle = make_test_handle_with(vec![], |config| {
            config.default_domain = Some("missing-after-reload".to_string());
        });
        let error = update_mux_domains(&reloaded_handle)
            .expect_err("reload must report an explicit missing default");
        let rendered = format!("{error:#}");
        assert!(rendered.contains("default_domain"));
        assert!(rendered.contains("missing-after-reload"));
        assert_eq!(
            mux.default_domain()?.domain_name(),
            "first-default",
            "failed reload may retain prior state only behind an explicit error"
        );
        Ok(())
    }

    #[test]
    fn update_mux_domains_for_server_honors_explicit_default_mux_server_domain()
    -> anyhow::Result<()> {
        let _state = ScopedTestState::acquire();
        let raw_ssh = SshDomain {
            name: "server-default".to_string(),
            remote_address: "server.example:22".to_string(),
            multiplexing: SshMultiplexing::None,
            ..SshDomain::default()
        };
        let handle = make_test_handle_with(vec![raw_ssh], |config| {
            config.default_mux_server_domain = Some("server-default".to_string());
        });
        let local_domain: Arc<dyn Domain> = Arc::new(LocalDomain::new("local")?);
        let mux = Arc::new(Mux::new(Some(local_domain)));
        Mux::set_mux(&mux);

        update_mux_domains_for_server(&handle)?;

        assert_eq!(mux.default_domain()?.domain_name(), "server-default");
        Ok(())
    }

    #[test]
    fn update_mux_domains_for_server_rejects_missing_default_with_key_and_value()
    -> anyhow::Result<()> {
        let _state = ScopedTestState::acquire();
        let handle = make_test_handle_with(vec![], |config| {
            config.default_mux_server_domain = Some("missing-server-default".to_string());
        });
        let local_domain: Arc<dyn Domain> = Arc::new(LocalDomain::new("local")?);
        let mux = Arc::new(Mux::new(Some(local_domain)));
        Mux::set_mux(&mux);

        let error = update_mux_domains_for_server(&handle)
            .expect_err("missing configured mux-server default must fail");
        let rendered = format!("{error:#}");
        assert!(rendered.contains("default_mux_server_domain"));
        assert!(rendered.contains("missing-server-default"));
        Ok(())
    }

    #[test]
    fn update_mux_domains_for_server_reload_reports_missing_default_instead_of_stale_success()
    -> anyhow::Result<()> {
        let _state = ScopedTestState::acquire();
        let first = SshDomain {
            name: "first-server-default".to_string(),
            remote_address: "first-server.example:22".to_string(),
            multiplexing: SshMultiplexing::None,
            ..SshDomain::default()
        };
        let first_handle = make_test_handle_with(vec![first], |config| {
            config.default_mux_server_domain = Some("first-server-default".to_string());
        });
        let local_domain: Arc<dyn Domain> = Arc::new(LocalDomain::new("local")?);
        let mux = Arc::new(Mux::new(Some(local_domain)));
        Mux::set_mux(&mux);
        update_mux_domains_for_server(&first_handle)?;
        assert_eq!(mux.default_domain()?.domain_name(), "first-server-default");

        let reloaded_handle = make_test_handle_with(vec![], |config| {
            config.default_mux_server_domain = Some("missing-server-after-reload".to_string());
        });
        let error = update_mux_domains_for_server(&reloaded_handle)
            .expect_err("server reload must report an explicit missing default");
        let rendered = format!("{error:#}");
        assert!(rendered.contains("default_mux_server_domain"));
        assert!(rendered.contains("missing-server-after-reload"));
        assert_eq!(
            mux.default_domain()?.domain_name(),
            "first-server-default",
            "failed server reload may retain prior state only behind an explicit error"
        );
        Ok(())
    }

    #[test]
    fn update_mux_domains_for_server_preserves_client_domain_rejection() -> anyhow::Result<()> {
        let _state = ScopedTestState::acquire();
        let client_ssh = SshDomain {
            name: "client-domain".to_string(),
            remote_address: "client.example:22".to_string(),
            multiplexing: SshMultiplexing::WezTerm,
            ..SshDomain::default()
        };
        let handle = make_test_handle_with(vec![client_ssh], |config| {
            config.default_mux_server_domain = Some("client-domain".to_string());
        });
        let local_domain: Arc<dyn Domain> = Arc::new(LocalDomain::new("local")?);
        let mux = Arc::new(Mux::new(Some(local_domain)));
        Mux::set_mux(&mux);

        let error = update_mux_domains_for_server(&handle)
            .expect_err("a client domain cannot become the standalone server default");
        let rendered = format!("{error:#}");
        assert!(rendered.contains("cannot be set to a client domain"));
        Ok(())
    }

    #[test]
    fn client_domains_multiple_mux_ssh() {
        let _state = ScopedTestState::acquire();

        let mux1 = SshDomain {
            name: "mux-1".to_string(),
            remote_address: "host1:22".to_string(),
            multiplexing: SshMultiplexing::WezTerm,
            ..SshDomain::default()
        };
        let mux2 = SshDomain {
            name: "mux-2".to_string(),
            remote_address: "host2:22".to_string(),
            multiplexing: SshMultiplexing::WezTerm,
            ..SshDomain::default()
        };
        let raw = SshDomain {
            name: "raw".to_string(),
            remote_address: "host3:22".to_string(),
            multiplexing: SshMultiplexing::None,
            ..SshDomain::default()
        };

        let handle = make_test_handle(vec![mux1, mux2, raw]);
        let domains = configured_client_domains(&handle);

        assert_eq!(
            domains.len(),
            2,
            "should have 2 multiplexed SSH client domains"
        );

        let names: Vec<&str> = domains.iter().map(|d| d.name()).collect();
        assert!(names.contains(&"mux-1"));
        assert!(names.contains(&"mux-2"));
    }
}
