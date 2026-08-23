use anyhow::Context;
use base64::Engine as _;
use config::{ConfigHandle, SshMultiplexing};
use frankenterm_client::domain::{ClientDomain, ClientDomainConfig};
use mux::Mux;
use mux::domain::{Domain, LocalDomain};
use mux::ssh::RemoteSshDomain;
use serde::{Deserialize, Serialize};
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::{Arc, MutexGuard};

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
const LIVE_SCROLLBACK_LINE_COMPRESS_MIN_BYTES: usize = 256;
const LIVE_SCROLLBACK_MAX_DECODED_LINE_BYTES: u64 = 16 * 1024 * 1024;

fn client_domains(config: &config::ConfigHandle) -> Vec<ClientDomainConfig> {
    let mut domains = vec![];
    for unix_dom in &config.unix_domains {
        domains.push(ClientDomainConfig::Unix(unix_dom.clone()));
    }

    for ssh_dom in config.ssh_domains() {
        if ssh_dom.multiplexing == SshMultiplexing::WezTerm {
            domains.push(ClientDomainConfig::Ssh(ssh_dom.clone()));
        }
    }

    for tls_client in &config.tls_clients {
        domains.push(ClientDomainConfig::Tls(tls_client.clone()));
    }
    domains
}

#[derive(Debug)]
struct LiveScrollbackSpillSink {
    pane_id: u64,
    durable_pane_id: [u8; 16],
    source_pane_id: usize,
    source_domain_id: usize,
    command_description: String,
    manifest_path: PathBuf,
    store: std::sync::Mutex<frankenterm_core::storage::mmap_store::MmapScrollbackStore>,
    state: std::sync::Mutex<LiveScrollbackSpillState>,
    redactor: frankenterm_core::redactor::Redactor,
}

#[derive(Debug, Clone, Copy, Default)]
struct LiveScrollbackSpillState {
    initial_stable_row: Option<wezterm_term::StableRowIndex>,
    max_retained_rows: usize,
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
    max_retained_rows: u64,
    oldest_seq: Option<u64>,
    retained_rows: u64,
    next_seq: u64,
    content_log: String,
    manifest_sha256: String,
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
    pub redaction_applied_before_persistence: bool,
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
    fn manifest_checksum(manifest: &LiveScrollbackManifestV1) -> anyhow::Result<String> {
        use sha2::{Digest as _, Sha256};

        let mut canonical = manifest.clone();
        canonical.manifest_sha256.clear();
        let bytes = serde_json::to_vec(&canonical)?;
        Ok(hex::encode(Sha256::digest(bytes)))
    }

    fn validate_persisted_records(
        store: &frankenterm_core::storage::mmap_store::MmapScrollbackStore,
        pane_id: u64,
    ) -> anyhow::Result<()> {
        let retained_rows = store.line_count(pane_id);
        if retained_rows == 0 {
            return Ok(());
        }
        let oldest_seq = store.oldest_seq(pane_id).ok_or_else(|| {
            anyhow::anyhow!("non-empty scrollback log has no oldest sequence")
        })?;
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
            let recognized = record.starts_with(LIVE_SCROLLBACK_LINE_RECORD_V1_UNCOMPRESSED)
                || record.starts_with(LIVE_SCROLLBACK_LINE_RECORD_V1_ZSTD)
                || record.starts_with(LIVE_SCROLLBACK_LINE_RECORD_V2_UNCOMPRESSED)
                || record.starts_with(LIVE_SCROLLBACK_LINE_RECORD_V2_ZSTD);
            if !recognized || decode_scrollback_line_record(&record).is_none() {
                anyhow::bail!("scrollback record {seq} failed integrity validation");
            }
        }
        Ok(())
    }

    fn new(
        base_dir: PathBuf,
        context: &config::ScrollbackSpillSinkContext,
    ) -> anyhow::Result<Self> {
        let pane_id = 0;
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
        let mut store = frankenterm_core::storage::mmap_store::MmapScrollbackStore::new(
            frankenterm_core::storage::mmap_store::MmapStoreConfig::new(durable_pane_dir),
        )?;
        store.ensure_pane(pane_id)?;
        let redactor = frankenterm_core::redactor::Redactor::new();
        let command_description = redactor.redact(&context.command_description);
        let (state, repair_complete_manifest) = match Self::read_manifest(&manifest_path)? {
            Some(manifest) => {
                if manifest.schema != "frankenterm.live-scrollback-manifest.v1"
                    || manifest.durable_pane_id != durable_pane_id
                    || manifest.content_log != "0.log"
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
                    store.clear_pane(pane_id).with_context(|| {
                        format!(
                            "finish interrupted scrollback clear for {}",
                            manifest_path.display()
                        )
                    })?;
                    (LiveScrollbackSpillState::default(), false)
                } else {
                    let actual_retained_rows = u64::try_from(store.line_count(pane_id))
                        .map_err(|_| anyhow::anyhow!("scrollback row count exceeds u64"))?;
                    let actual_oldest_seq = store.oldest_seq(pane_id);
                    let actual_next_seq = store.next_seq(pane_id)?;
                    Self::validate_persisted_records(&store, pane_id).with_context(|| {
                        format!(
                            "validate persisted scrollback records for {}",
                            manifest_path.display()
                        )
                    })?;
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
                    let repair_complete_manifest = actual_retained_rows != manifest.retained_rows
                        || actual_oldest_seq != manifest.oldest_seq
                        || actual_next_seq != manifest.next_seq
                        || (manifest.publication_state == "prepared"
                            && actual_retained_rows > 0);
                    (
                        LiveScrollbackSpillState {
                            initial_stable_row: Some(manifest.initial_stable_row.ok_or_else(
                                || {
                                    anyhow::anyhow!(
                                        "published scrollback manifest is missing initial stable row"
                                    )
                                },
                            )?),
                            max_retained_rows: usize::try_from(manifest.max_retained_rows).map_err(
                                |_| anyhow::anyhow!("scrollback retention exceeds platform usize"),
                            )?,
                        },
                        repair_complete_manifest,
                    )
                }
            }
            None => {
                if store.line_count(pane_id) != 0 {
                    anyhow::bail!(
                        "scrollback content exists without an identity manifest at {}",
                        manifest_path.display()
                    );
                }
                (LiveScrollbackSpillState::default(), false)
            }
        };
        let sink = Self {
            pane_id,
            durable_pane_id: context.durable_pane_id,
            source_pane_id: context.pane_id,
            source_domain_id: context.domain_id,
            command_description,
            manifest_path,
            store: std::sync::Mutex::new(store),
            state: std::sync::Mutex::new(state),
            redactor,
        };
        if repair_complete_manifest {
            sink.persist_manifest("complete").with_context(|| {
                format!(
                    "repair interrupted scrollback manifest publication {}",
                    sink.manifest_path.display()
                )
            })?;
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
        if path_metadata_before.len() > 1024 * 1024 {
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
        (&file).take(1024 * 1024 + 1)
            .read_to_end(&mut bytes)
            .with_context(|| format!("read scrollback manifest {}", path.display()))?;
        if bytes.len() > 1024 * 1024 {
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

    fn persist_manifest(&self, publication_state: &'static str) -> anyhow::Result<()> {
        anyhow::ensure!(
            matches!(publication_state, "prepared" | "complete" | "cleared"),
            "invalid scrollback manifest publication state"
        );
        let state = self.lock_state("persist_manifest state");
        let initial_stable_row = state.initial_stable_row;
        let max_retained_rows = state.max_retained_rows;
        drop(state);

        let store = self.lock_store("persist_manifest store");
        let mut manifest = LiveScrollbackManifestV1 {
            schema: "frankenterm.live-scrollback-manifest.v1".to_string(),
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
            max_retained_rows: u64::try_from(max_retained_rows)
                .map_err(|_| anyhow::anyhow!("scrollback retention exceeds u64"))?,
            oldest_seq: store.oldest_seq(self.pane_id),
            retained_rows: u64::try_from(store.line_count(self.pane_id))
                .map_err(|_| anyhow::anyhow!("scrollback row count exceeds u64"))?,
            next_seq: store.next_seq(self.pane_id)?,
            content_log: "0.log".to_string(),
            manifest_sha256: String::new(),
        };
        drop(store);

        manifest.manifest_sha256 = Self::manifest_checksum(&manifest)?;

        let mut bytes = serde_json::to_vec_pretty(&manifest)?;
        bytes.push(b'\n');
        let parent = self
            .manifest_path
            .parent()
            .ok_or_else(|| anyhow::anyhow!("scrollback manifest path has no parent"))?;
        let temp_path = parent.join(format!(
            "manifest.json.installing-{}",
            uuid::Uuid::new_v4().simple()
        ));
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600);
        }
        let mut file = options
            .open(&temp_path)
            .with_context(|| format!("create scrollback manifest stage {}", temp_path.display()))?;
        file.write_all(&bytes)
            .and_then(|()| file.sync_all())
            .with_context(|| format!("persist scrollback manifest stage {}", temp_path.display()))?;
        drop(file);
        let staged_manifest = tempfile::TempPath::try_from_path(temp_path.clone())
            .with_context(|| format!("adopt scrollback manifest stage {}", temp_path.display()))?;
        if let Err(mut error) = staged_manifest.persist(&self.manifest_path) {
            // Preserve the fully synchronized stage for recovery and diagnosis;
            // a failed publication must never silently delete the only new copy.
            error.path.disable_cleanup(true);
            return Err(error.error).with_context(|| {
                format!(
                    "publish scrollback manifest {}",
                    self.manifest_path.display()
                )
            });
        }
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
        Ok(())
    }

    #[cfg(test)]
    fn physical_scrollback_bytes(&self) -> u64 {
        self.lock_store("physical_scrollback_bytes")
            .file_bytes(self.pane_id)
    }

    fn lock_state(&self, context: &str) -> MutexGuard<'_, LiveScrollbackSpillState> {
        match self.state.lock() {
            Ok(state) => state,
            Err(poisoned) => {
                log::error!("live scrollback spill state mutex poisoned during {context}");
                self.state.clear_poison();
                poisoned.into_inner()
            }
        }
    }

    fn lock_store(
        &self,
        context: &str,
    ) -> MutexGuard<'_, frankenterm_core::storage::mmap_store::MmapScrollbackStore> {
        match self.store.lock() {
            Ok(store) => store,
            Err(poisoned) => {
                log::error!("live scrollback spill store mutex poisoned during {context}");
                self.store.clear_poison();
                poisoned.into_inner()
            }
        }
    }
}

fn first_visible_cell_attrs(line: &wezterm_term::Line) -> termwiz::cell::CellAttributes {
    line.visible_cells()
        .next()
        .map(|cell| cell.attrs().clone())
        .unwrap_or_else(termwiz::cell::CellAttributes::blank)
}

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

fn decode_scrollback_line_record(record: &str) -> Option<wezterm_term::Line> {
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

    let payload = base64::engine::general_purpose::STANDARD_NO_PAD
        .decode(encoded)
        .ok()?;
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
            .take(LIVE_SCROLLBACK_MAX_DECODED_LINE_BYTES + 1)
            .read_to_end(&mut decompressed)
            .ok()?;
        if decompressed.len() as u64 > LIVE_SCROLLBACK_MAX_DECODED_LINE_BYTES {
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
    let mut reader = decoded_payload.as_slice();
    codec::bounded_varbincode_deserialize(&mut reader).ok()
}

fn scrollback_line_records_are_equivalent(left: &str, right: &str) -> bool {
    let Some(left) = decode_scrollback_line_record(left) else {
        return false;
    };
    let Some(right) = decode_scrollback_line_record(right) else {
        return false;
    };
    match (varbincode::serialize(&left), varbincode::serialize(&right)) {
        (Ok(left), Ok(right)) => left == right,
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
        let Some(record) = encode_scrollback_line_record(line, &self.redactor) else {
            return false;
        };

        let (manifest_prepare_required, desired_seq) = {
            let mut state = self.lock_state("store_scrollback_line initial row");
            let manifest_prepare_required = state.initial_stable_row.is_none();
            let initial = *state.initial_stable_row.get_or_insert(stable_row);
            if stable_row < initial {
                return false;
            }
            let Ok(desired_seq) = u64::try_from(stable_row - initial) else {
                return false;
            };
            state.max_retained_rows = max_retained_rows;
            (manifest_prepare_required, desired_seq)
        };
        if manifest_prepare_required && self.persist_manifest("prepared").is_err() {
            *self.lock_state("store_scrollback_line prepare rollback") =
                LiveScrollbackSpillState::default();
            return false;
        }

        {
            let mut store = self.lock_store("store_scrollback_line append");
            let Ok(next_seq) = store.next_seq(self.pane_id) else {
                return false;
            };
            if desired_seq < next_seq {
                match store.line_at(self.pane_id, desired_seq) {
                    Ok(Some(existing)) if scrollback_line_records_are_equivalent(&existing, &record) => {
                        // Idempotent retry after content durability succeeded
                        // but a later manifest publication failed.
                    }
                    Ok(None)
                        if store
                            .oldest_seq(self.pane_id)
                            .is_some_and(|oldest| desired_seq < oldest) =>
                    {
                        // The stable row was already acknowledged and has
                        // since aged out under the configured retention bound.
                        return true;
                    }
                    _ => return false,
                }
            } else if desired_seq == next_seq {
                match store.append_line(self.pane_id, &record) {
                    Ok(appended_seq) if appended_seq == desired_seq => {}
                    _ => return false,
                }
            } else {
                // Refuse to manufacture a gap: ordered stable-row identity is
                // more important than admitting a later line out of sequence.
                return false;
            }

            let retained = store.line_count(self.pane_id);
            if retained > max_retained_rows {
                let oldest_seq = store.oldest_seq(self.pane_id).unwrap_or(0);
                let drop_count = retained - max_retained_rows;
                let Ok(drop_count) = u64::try_from(drop_count) else {
                    return false;
                };
                let Some(prune_before_seq) = oldest_seq.checked_add(drop_count) else {
                    return false;
                };
                if store.prune_before(self.pane_id, prune_before_seq).is_err() {
                    return false;
                }
                if store
                    .compact_pane_if_stale(self.pane_id, LIVE_SCROLLBACK_COMPACT_MIN_STALE_BYTES)
                    .is_err()
                {
                    return false;
                }
            }
        }

        self.lock_state("store_scrollback_line retention")
            .max_retained_rows = max_retained_rows;

        self.persist_manifest("complete").is_ok()
    }

    fn load_scrollback_line(
        &self,
        stable_row: wezterm_term::StableRowIndex,
    ) -> Option<wezterm_term::Line> {
        let initial = self
            .lock_state("load_scrollback_line initial row")
            .initial_stable_row?;
        if stable_row < initial {
            return None;
        }
        let seq = u64::try_from(stable_row - initial).ok()?;
        let record = self
            .lock_store("load_scrollback_line read")
            .line_at(self.pane_id, seq)
            .ok()
            .flatten()?;
        if record.starts_with(LIVE_SCROLLBACK_LINE_RECORD_V1_UNCOMPRESSED)
            || record.starts_with(LIVE_SCROLLBACK_LINE_RECORD_V1_ZSTD)
            || record.starts_with(LIVE_SCROLLBACK_LINE_RECORD_V2_UNCOMPRESSED)
            || record.starts_with(LIVE_SCROLLBACK_LINE_RECORD_V2_ZSTD)
        {
            // A prefixed record that fails bounded decode is corrupt or
            // incomplete. Never reinterpret its encoded bytes as legacy user
            // text; that would manufacture plausible scrollback from damaged
            // storage and hide the recovery boundary.
            decode_scrollback_line_record(&record)
        } else {
            Some(legacy_text_scrollback_line(&record))
        }
    }

    fn oldest_scrollback_row(&self) -> Option<wezterm_term::StableRowIndex> {
        let initial = self
            .lock_state("oldest_scrollback_row initial row")
            .initial_stable_row?;
        self.lock_store("oldest_scrollback_row oldest seq")
            .oldest_seq(self.pane_id)
            .and_then(|seq| {
                wezterm_term::StableRowIndex::try_from(seq)
                    .ok()
                    .and_then(|seq| initial.checked_add(seq))
            })
    }

    fn retained_scrollback_rows(&self) -> usize {
        self.lock_store("retained_scrollback_rows")
            .line_count(self.pane_id)
    }

    fn retained_scrollback_bytes(&self) -> usize {
        self.lock_store("retained_scrollback_bytes")
            .retained_bytes(self.pane_id)
            .try_into()
            .unwrap_or(usize::MAX)
    }

    fn clear_scrollback(&self) {
        let previous = {
            let mut state = self.lock_state("clear_scrollback state reset");
            let previous = *state;
            *state = LiveScrollbackSpillState::default();
            previous
        };
        // Publish the clear intent before truncating the content log. If the
        // process dies between these operations, constructor recovery observes
        // `cleared` and completes the idempotent clear before accepting data.
        if let Err(error) = self.persist_manifest("cleared") {
            *self.lock_state("clear_scrollback manifest rollback") = previous;
            log::error!("failed to persist scrollback clear intent: {error:#}");
            return;
        }
        if let Err(error) = self
            .lock_store("clear_scrollback store reset")
            .clear_pane(self.pane_id)
        {
            log::error!("failed to complete persisted scrollback clear intent: {error}");
        }
    }
}

fn validate_live_scrollback_manifest_identity(
    manifest: &LiveScrollbackManifestV1,
    durable_pane_id: &str,
    manifest_path: &std::path::Path,
) -> anyhow::Result<()> {
    anyhow::ensure!(
        manifest.schema == "frankenterm.live-scrollback-manifest.v1",
        "unsupported live scrollback manifest schema at {}",
        manifest_path.display()
    );
    anyhow::ensure!(
        manifest.durable_pane_id == durable_pane_id,
        "live scrollback manifest identity mismatch at {}",
        manifest_path.display()
    );
    anyhow::ensure!(
        manifest.content_log == "0.log",
        "live scrollback manifest content path mismatch at {}",
        manifest_path.display()
    );
    anyhow::ensure!(
        matches!(
            manifest.publication_state.as_str(),
            "prepared" | "complete" | "cleared"
        ),
        "invalid live scrollback publication state at {}",
        manifest_path.display()
    );
    anyhow::ensure!(
        (manifest.retained_rows == 0) == manifest.oldest_seq.is_none(),
        "live scrollback manifest has inconsistent retained-row bounds at {}",
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
        let result = validate_live_scrollback_directory(&pane_path, true)
            .and_then(|metadata_before| {
                let manifest = LiveScrollbackSpillSink::read_manifest(&manifest_path)?;
                let metadata_after = std::fs::symlink_metadata(&pane_path)?;
                anyhow::ensure!(
                    !filesystem_metadata_changed(&metadata_before, &metadata_after)?,
                    "live scrollback pane directory changed during discovery"
                );
                Ok(manifest)
            })
            .and_then(|manifest| {
                manifest.ok_or_else(|| anyhow::anyhow!("live scrollback manifest is missing"))
            })
            .and_then(|manifest| {
                validate_live_scrollback_manifest_identity(
                    &manifest,
                    &durable_pane_id,
                    &manifest_path,
                )?;
                Ok(manifest)
            });
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
    validate_live_scrollback_content_file(&pane_path.join("0.log"), "live scrollback log", false)?;
    validate_live_scrollback_content_file(
        &pane_path.join("0.seq"),
        "live scrollback sequence journal",
        true,
    )?;

    let snapshot = frankenterm_core::storage::mmap_store::read_pane_snapshot(
        &pane_path,
        0,
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
    let pane_metadata_after = std::fs::symlink_metadata(&pane_path)?;
    anyhow::ensure!(
        !filesystem_metadata_changed(&pane_metadata_before, &pane_metadata_after)?,
        "live scrollback pane directory changed during export; retry against a stable source"
    );

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

    let mut transcript = String::new();
    for (index, record) in records.iter().enumerate() {
        let line = decode_scrollback_line_record(record).ok_or_else(|| {
            anyhow::anyhow!("live scrollback record {index} failed bounded integrity decoding")
        })?;
        let text = line.as_str();
        let delimiter_bytes = usize::from(!line.last_cell_was_wrapped());
        let next_len = transcript
            .len()
            .checked_add(text.len())
            .and_then(|len| len.checked_add(delimiter_bytes))
            .ok_or_else(|| anyhow::anyhow!("live scrollback transcript length overflow"))?;
        anyhow::ensure!(
            next_len <= max_transcript_bytes,
            "live scrollback transcript exceeds configured {max_transcript_bytes}-byte limit"
        );
        transcript.push_str(text.as_ref());
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
        redaction_applied_before_persistence: true,
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
    install_scrollback_spill_sink_factory();
    update_mux_domains_impl(config, false)
}

pub fn update_mux_domains_for_server(config: &ConfigHandle) -> anyhow::Result<()> {
    install_scrollback_spill_sink_factory();
    update_mux_domains_impl(config, true)
}

fn update_mux_domains_impl(config: &ConfigHandle, is_standalone_mux: bool) -> anyhow::Result<()> {
    let mux = Mux::try_get().context("mux singleton is not available")?;

    for client_config in client_domains(config) {
        if mux.get_domain_by_name(client_config.name()).is_some() {
            continue;
        }

        let domain: Arc<dyn Domain> = Arc::new(ClientDomain::new(client_config, &mux)?);
        mux.add_domain(&domain)?;
    }

    for ssh_dom in config.ssh_domains() {
        if ssh_dom.multiplexing != SshMultiplexing::None {
            continue;
        }

        if mux.get_domain_by_name(&ssh_dom.name).is_some() {
            continue;
        }

        let domain: Arc<dyn Domain> = Arc::new(RemoteSshDomain::with_ssh_domain(&ssh_dom)?);
        mux.add_domain(&domain)?;
    }

    for wsl_dom in config.wsl_domains() {
        if mux.get_domain_by_name(&wsl_dom.name).is_some() {
            continue;
        }

        let domain: Arc<dyn Domain> = Arc::new(LocalDomain::new_wsl(wsl_dom.clone())?);
        mux.add_domain(&domain)?;
    }

    for exec_dom in &config.exec_domains {
        if mux.get_domain_by_name(&exec_dom.name).is_some() {
            continue;
        }

        let domain: Arc<dyn Domain> = Arc::new(LocalDomain::new_exec_domain(exec_dom.clone())?);
        mux.add_domain(&domain)?;
    }

    for serial in &config.serial_ports {
        if mux.get_domain_by_name(&serial.name).is_some() {
            continue;
        }

        let domain: Arc<dyn Domain> = Arc::new(LocalDomain::new_serial_domain(serial.clone())?);
        mux.add_domain(&domain)?;
    }

    if is_standalone_mux {
        if let Some(name) = &config.default_mux_server_domain {
            let dom = mux.get_domain_by_name(name).ok_or_else(|| {
                anyhow::anyhow!(
                    "configured default_mux_server_domain={name:?} does not match any registered domain"
                )
            })?;
            if dom.is::<ClientDomain>() {
                anyhow::bail!("default_mux_server_domain cannot be set to a client domain!");
            }
            mux.set_default_domain_guard(&dom)?;
        }
    } else if let Some(name) = &config.default_domain {
        let dom = mux.get_domain_by_name(name).ok_or_else(|| {
            anyhow::anyhow!(
                "configured default_domain={name:?} does not match any registered domain"
            )
        })?;
        mux.set_default_domain_guard(&dom)?;
    }

    Ok(())
}

pub static PKI: std::sync::LazyLock<pki::Pki> =
    std::sync::LazyLock::new(|| pki::Pki::init().expect("failed to initialize PKI"));

#[cfg(test)]
mod tests {
    use super::*;
    use config::{Config, SshDomain};
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
        configure(&mut config);
        config::use_this_configuration(config);
        config::configuration()
    }

    fn reset_test_state() {
        config::use_test_configuration();
        config::set_scrollback_spill_sink_factory(None);
        Mux::shutdown();
    }

    #[test]
    fn live_scrollback_spill_sink_hydrates_redacted_retained_rows() {
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
        assert!(hydrated_text.contains("[REDACTED]"));
        assert!(!hydrated_text.contains("sk-"));
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
        let line = Line::from_text("styled-row", &attrs, 42, None);

        assert!(sink.store_scrollback_line(0, &line, 4));

        let hydrated = sink
            .load_scrollback_line(0)
            .expect("styled row should hydrate");
        assert_eq!(hydrated.as_str().as_ref(), "styled-row");
        assert_eq!(hydrated.current_seqno(), 42);
        assert!(
            hydrated.visible_cells().all(|cell| cell.attrs().italic()),
            "serialized cold row should preserve cell attributes"
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

    #[test]
    fn live_scrollback_spill_sink_repairs_content_ahead_of_complete_manifest() {
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

            let second = Line::from_text("durable-unpublished-row", &attrs, 2, None);
            let record = encode_scrollback_line_record(&second, &sink.redactor)
                .expect("encode durable unpublished row");
            assert_eq!(
                sink.lock_store("test interrupted complete publication")
                    .append_line(sink.pane_id, &record)
                    .expect("durably append unpublished row"),
                1
            );
        }

        let reopened = LiveScrollbackSpillSink::new(dir.path().to_path_buf(), &context)
            .expect("repair stale complete manifest on reopen");
        assert_eq!(
            reopened
                .load_scrollback_line(21)
                .expect("content ahead of manifest survives reopen")
                .as_str()
                .as_ref(),
            "durable-unpublished-row"
        );
        let manifest = LiveScrollbackSpillSink::read_manifest(&reopened.manifest_path)
            .expect("read repaired manifest")
            .expect("repaired manifest exists");
        assert_eq!(manifest.publication_state, "complete");
        assert_eq!(manifest.oldest_seq, Some(0));
        assert_eq!(manifest.retained_rows, 2);
        assert_eq!(manifest.next_seq, 2);
    }

    #[test]
    fn live_scrollback_spill_sink_repairs_empty_forward_retention_state() {
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
            let mut store = sink.lock_store("test empty forward retention state");
            store.prune_before(sink.pane_id, 4).expect("persist full logical prune");
            assert!(
                store
                    .compact_pane_if_stale(sink.pane_id, 1)
                    .expect("compact empty retained set")
            );
        }

        let reopened = LiveScrollbackSpillSink::new(dir.path().to_path_buf(), &context)
            .expect("repair empty forward retention state on reopen");
        assert_eq!(reopened.retained_scrollback_rows(), 0);
        assert_eq!(reopened.oldest_scrollback_row(), None);
        let manifest = LiveScrollbackSpillSink::read_manifest(&reopened.manifest_path)
            .expect("read repaired manifest")
            .expect("repaired manifest exists");
        assert_eq!(manifest.publication_state, "complete");
        assert_eq!(manifest.oldest_seq, None);
        assert_eq!(manifest.retained_rows, 0);
        assert_eq!(manifest.next_seq, 4);
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
                &Line::from_text("recoverable-two", &attrs, 2, None),
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
        assert_eq!(export.transcript, "recoverable-one\nrecoverable-two\n");
        assert_eq!(export.retained_rows, 2);
        assert_eq!(export.oldest_seq, Some(0));
        assert_eq!(export.next_seq, 2);
        assert!(!export.source_content_mutated);
        assert!(export.redaction_applied_before_persistence);

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
        assert!(error
            .to_string()
            .contains("content is empty behind a manifest with retained rows"));
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
        sink.lock_state("test corrupt record").initial_stable_row = Some(0);
        sink.lock_store("test corrupt record")
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

        sink.clear_scrollback();

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
    fn live_scrollback_spill_sink_recovers_poisoned_locks() {
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
        let second = Line::from_text("after-poison", &attrs, 2, None);

        let state_poisoned = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _state = sink.state.lock().expect("state lock for poison test");
            panic!("poison state lock for regression coverage");
        }));
        assert!(state_poisoned.is_err());
        assert!(sink.state.is_poisoned());
        assert!(sink.store_scrollback_line(0, &first, 8));
        assert!(!sink.state.is_poisoned());
        assert!(sink.load_scrollback_line(0).is_some());

        let store_poisoned = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _store = sink.store.lock().expect("store lock for poison test");
            panic!("poison store lock for regression coverage");
        }));
        assert!(store_poisoned.is_err());
        assert!(sink.store.is_poisoned());
        assert_eq!(sink.retained_scrollback_rows(), 1);
        assert!(!sink.store.is_poisoned());
        assert!(sink.store_scrollback_line(1, &second, 8));
        assert_eq!(sink.retained_scrollback_rows(), 2);
        assert_eq!(
            sink.load_scrollback_line(1)
                .expect("row stored after poison should hydrate")
                .as_str()
                .as_ref(),
            "after-poison"
        );

        sink.clear_scrollback();
        assert_eq!(sink.retained_scrollback_rows(), 0);
        assert_eq!(sink.oldest_scrollback_row(), None);
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
        let domains = client_domains(&handle);

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
        let domains = client_domains(&handle);
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
    fn client_domains_with_only_raw_ssh_returns_empty() {
        let _state = ScopedTestState::acquire();

        let raw_ssh = SshDomain {
            name: "raw-only".to_string(),
            remote_address: "raw.example:22".to_string(),
            multiplexing: SshMultiplexing::None,
            ..SshDomain::default()
        };
        let handle = make_test_handle(vec![raw_ssh]);
        let domains = client_domains(&handle);

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
        let domains = client_domains(&handle);

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
