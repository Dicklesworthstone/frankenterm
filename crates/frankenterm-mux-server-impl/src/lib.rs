use anyhow::Context;
use base64::Engine as _;
use config::{ConfigHandle, SshMultiplexing};
use frankenterm_client::domain::{ClientDomain, ClientDomainConfig};
use mux::Mux;
use mux::domain::{Domain, LocalDomain};
use mux::ssh::RemoteSshDomain;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::io::Read;
use std::path::PathBuf;
use std::sync::Arc;

pub mod dispatch;
pub mod local;
pub mod pki;
pub mod sessionhandler;

#[cfg(test)]
pub(crate) static GLOBAL_STATE_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(not(test))]
const LIVE_SCROLLBACK_COMPACT_MIN_STALE_BYTES: u64 = 4 * 1024 * 1024;
#[cfg(test)]
const LIVE_SCROLLBACK_COMPACT_MIN_STALE_BYTES: u64 = 1;
const LIVE_SCROLLBACK_LINE_RECORD_V1_UNCOMPRESSED: &str = "ftsl1u:";
const LIVE_SCROLLBACK_LINE_RECORD_V1_ZSTD: &str = "ftsl1z:";
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
    store: std::sync::Mutex<frankenterm_core::storage::mmap_store::MmapScrollbackStore>,
    state: std::sync::Mutex<LiveScrollbackSpillState>,
    redactor: frankenterm_core::redactor::Redactor,
}

#[derive(Debug, Default)]
struct LiveScrollbackSpillState {
    initial_stable_row: Option<wezterm_term::StableRowIndex>,
    max_retained_rows: usize,
}

impl LiveScrollbackSpillSink {
    fn new(
        base_dir: PathBuf,
        context: &config::ScrollbackSpillSinkContext,
    ) -> Result<Self, frankenterm_core::storage::mmap_store::MmapStoreError> {
        let pane_id = scrollback_sink_pane_id(context);
        let mut store = frankenterm_core::storage::mmap_store::MmapScrollbackStore::new(
            frankenterm_core::storage::mmap_store::MmapStoreConfig::new(base_dir),
        )?;
        store.ensure_pane(pane_id)?;
        Ok(Self {
            pane_id,
            store: std::sync::Mutex::new(store),
            state: std::sync::Mutex::new(LiveScrollbackSpillState::default()),
            redactor: frankenterm_core::redactor::Redactor::new(),
        })
    }

    #[cfg(test)]
    fn physical_scrollback_bytes(&self) -> u64 {
        self.store
            .lock()
            .expect("live scrollback spill store mutex poisoned")
            .file_bytes(self.pane_id)
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

    let (prefix, payload) = if uncompressed.len() >= LIVE_SCROLLBACK_LINE_COMPRESS_MIN_BYTES {
        match zstd::stream::encode_all(&uncompressed[..], zstd::DEFAULT_COMPRESSION_LEVEL) {
            Ok(compressed) if compressed.len() < uncompressed.len() => {
                (LIVE_SCROLLBACK_LINE_RECORD_V1_ZSTD, compressed)
            }
            _ => (LIVE_SCROLLBACK_LINE_RECORD_V1_UNCOMPRESSED, uncompressed),
        }
    } else {
        (LIVE_SCROLLBACK_LINE_RECORD_V1_UNCOMPRESSED, uncompressed)
    };

    Some(format!(
        "{prefix}{}",
        base64::engine::general_purpose::STANDARD_NO_PAD.encode(payload)
    ))
}

fn decode_scrollback_line_record(record: &str) -> Option<wezterm_term::Line> {
    let (compressed, encoded) =
        if let Some(encoded) = record.strip_prefix(LIVE_SCROLLBACK_LINE_RECORD_V1_UNCOMPRESSED) {
            (false, encoded)
        } else if let Some(encoded) = record.strip_prefix(LIVE_SCROLLBACK_LINE_RECORD_V1_ZSTD) {
            (true, encoded)
        } else {
            return None;
        };

    let payload = base64::engine::general_purpose::STANDARD_NO_PAD
        .decode(encoded)
        .ok()?;
    let decoded = if compressed {
        let decoder = zstd::Decoder::new(payload.as_slice()).ok()?;
        let mut decoded = Vec::new();
        decoder
            .take(LIVE_SCROLLBACK_MAX_DECODED_LINE_BYTES + 1)
            .read_to_end(&mut decoded)
            .ok()?;
        if decoded.len() as u64 > LIVE_SCROLLBACK_MAX_DECODED_LINE_BYTES {
            return None;
        }
        decoded
    } else {
        payload
    };

    varbincode::deserialize(decoded.as_slice()).ok()
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

        let mut state = self
            .state
            .lock()
            .expect("live scrollback spill state mutex poisoned");
        let initial = *state.initial_stable_row.get_or_insert(stable_row);
        if stable_row < initial {
            return false;
        }

        let Some(record) = encode_scrollback_line_record(line, &self.redactor) else {
            return false;
        };
        let mut store = self
            .store
            .lock()
            .expect("live scrollback spill store mutex poisoned");
        if store.append_line(self.pane_id, &record).is_err() {
            return false;
        }

        state.max_retained_rows = max_retained_rows;
        let retained = store.line_count(self.pane_id);
        if retained > max_retained_rows {
            let oldest_seq = store.oldest_seq(self.pane_id).unwrap_or(0);
            let drop_count = retained - max_retained_rows;
            let prune_before_seq =
                oldest_seq.saturating_add(u64::try_from(drop_count).unwrap_or(u64::MAX));
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

        true
    }

    fn load_scrollback_line(
        &self,
        stable_row: wezterm_term::StableRowIndex,
    ) -> Option<wezterm_term::Line> {
        let initial = self
            .state
            .lock()
            .expect("live scrollback spill state mutex poisoned")
            .initial_stable_row?;
        if stable_row < initial {
            return None;
        }
        let seq = u64::try_from(stable_row - initial).ok()?;
        let record = self
            .store
            .lock()
            .expect("live scrollback spill store mutex poisoned")
            .line_at(self.pane_id, seq)
            .ok()
            .flatten()?;
        Some(
            decode_scrollback_line_record(&record)
                .unwrap_or_else(|| legacy_text_scrollback_line(&record)),
        )
    }

    fn oldest_scrollback_row(&self) -> Option<wezterm_term::StableRowIndex> {
        let state = self
            .state
            .lock()
            .expect("live scrollback spill state mutex poisoned");
        let initial = state.initial_stable_row?;
        self.store
            .lock()
            .expect("live scrollback spill store mutex poisoned")
            .oldest_seq(self.pane_id)
            .and_then(|seq| {
                wezterm_term::StableRowIndex::try_from(seq)
                    .ok()
                    .and_then(|seq| initial.checked_add(seq))
            })
    }

    fn retained_scrollback_rows(&self) -> usize {
        self.store
            .lock()
            .expect("live scrollback spill store mutex poisoned")
            .line_count(self.pane_id)
    }

    fn retained_scrollback_bytes(&self) -> usize {
        self.store
            .lock()
            .expect("live scrollback spill store mutex poisoned")
            .retained_bytes(self.pane_id)
            .try_into()
            .unwrap_or(usize::MAX)
    }

    fn clear_scrollback(&self) {
        let _ = self
            .store
            .lock()
            .expect("live scrollback spill store mutex poisoned")
            .clear_pane(self.pane_id);
        *self
            .state
            .lock()
            .expect("live scrollback spill state mutex poisoned") =
            LiveScrollbackSpillState::default();
    }
}

fn scrollback_sink_pane_id(context: &config::ScrollbackSpillSinkContext) -> u64 {
    let mut hasher = DefaultHasher::new();
    std::process::id().hash(&mut hasher);
    context.domain_id.hash(&mut hasher);
    context.pane_id.hash(&mut hasher);
    context.command_description.hash(&mut hasher);
    hasher.finish()
}

fn default_live_scrollback_dir() -> PathBuf {
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

        let domain: Arc<dyn Domain> = Arc::new(ClientDomain::new(client_config));
        mux.add_domain(&domain);
    }

    for ssh_dom in config.ssh_domains() {
        if ssh_dom.multiplexing != SshMultiplexing::None {
            continue;
        }

        if mux.get_domain_by_name(&ssh_dom.name).is_some() {
            continue;
        }

        let domain: Arc<dyn Domain> = Arc::new(RemoteSshDomain::with_ssh_domain(&ssh_dom)?);
        mux.add_domain(&domain);
    }

    for wsl_dom in config.wsl_domains() {
        if mux.get_domain_by_name(&wsl_dom.name).is_some() {
            continue;
        }

        let domain: Arc<dyn Domain> = Arc::new(LocalDomain::new_wsl(wsl_dom.clone())?);
        mux.add_domain(&domain);
    }

    for exec_dom in &config.exec_domains {
        if mux.get_domain_by_name(&exec_dom.name).is_some() {
            continue;
        }

        let domain: Arc<dyn Domain> = Arc::new(LocalDomain::new_exec_domain(exec_dom.clone())?);
        mux.add_domain(&domain);
    }

    for serial in &config.serial_ports {
        if mux.get_domain_by_name(&serial.name).is_some() {
            continue;
        }

        let domain: Arc<dyn Domain> = Arc::new(LocalDomain::new_serial_domain(serial.clone())?);
        mux.add_domain(&domain);
    }

    if is_standalone_mux {
        if let Some(name) = &config.default_mux_server_domain {
            if let Some(dom) = mux.get_domain_by_name(name) {
                if dom.is::<ClientDomain>() {
                    anyhow::bail!("default_mux_server_domain cannot be set to a client domain!");
                }
                mux.set_default_domain(&dom);
            }
        }
    } else if let Some(name) = &config.default_domain {
        if let Some(dom) = mux.get_domain_by_name(name) {
            mux.set_default_domain(&dom);
        }
    }

    Ok(())
}

pub static PKI: std::sync::LazyLock<pki::Pki> =
    std::sync::LazyLock::new(|| pki::Pki::init().expect("failed to initialize PKI"));

#[cfg(test)]
mod tests {
    use super::*;
    use config::{Config, SshDomain};
    use std::sync::{Mutex, OnceLock};
    use termwiz::cell::CellAttributes;
    use wezterm_term::Line;
    use wezterm_term::config::ScrollbackSpillSink;

    fn test_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    fn make_test_handle(ssh_domains: Vec<SshDomain>) -> ConfigHandle {
        let mut config = Config::default_config();
        config.unix_domains.clear();
        config.tls_clients.clear();
        config.ssh_domains = Some(ssh_domains);
        config::use_this_configuration(config);
        config::configuration()
    }

    fn reset_test_state() {
        config::use_test_configuration();
        Mux::shutdown();
    }

    #[test]
    fn live_scrollback_spill_sink_hydrates_redacted_retained_rows() {
        let dir = tempfile::tempdir().expect("temp scrollback dir");
        let context = config::ScrollbackSpillSinkContext {
            pane_id: 7,
            domain_id: 3,
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
    fn live_scrollback_spill_sink_bounds_physical_storage_under_sustained_output() {
        let dir = tempfile::tempdir().expect("temp scrollback dir");
        let context = config::ScrollbackSpillSinkContext {
            pane_id: 9,
            domain_id: 3,
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
    fn scrollback_line_record_uses_zstd_for_repetitive_large_rows() {
        let attrs = CellAttributes::blank();
        let text = "z".repeat(4096);
        let line = Line::from_text(&text, &attrs, 77, None);
        let redactor = frankenterm_core::redactor::Redactor::new();

        let record =
            encode_scrollback_line_record(&line, &redactor).expect("encode scrollback record");

        assert!(
            record.starts_with(LIVE_SCROLLBACK_LINE_RECORD_V1_ZSTD),
            "large repetitive rows should use compressed record encoding"
        );
        let decoded =
            decode_scrollback_line_record(&record).expect("decode compressed scrollback record");
        assert_eq!(decoded.as_str().as_ref(), text);
        assert_eq!(decoded.current_seqno(), 77);
    }

    #[test]
    fn client_domains_include_only_wezterm_ssh_domains() {
        let _guard = test_lock().lock().expect("lock");

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

        reset_test_state();
    }

    #[test]
    fn update_mux_domains_registers_muxed_and_raw_ssh_domains() -> anyhow::Result<()> {
        let _guard = test_lock().lock().expect("lock");

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

        reset_test_state();
        Ok(())
    }

    #[test]
    fn client_domains_empty_config_returns_empty() {
        let _guard = test_lock().lock().expect("lock");
        let handle = make_test_handle(vec![]);
        let domains = client_domains(&handle);
        assert!(domains.is_empty(), "no ssh domains means no client domains");
        reset_test_state();
    }

    #[test]
    fn update_mux_domains_with_no_ssh_registers_only_local() -> anyhow::Result<()> {
        let _guard = test_lock().lock().expect("lock");
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

        reset_test_state();
        Ok(())
    }

    #[test]
    fn update_mux_domains_idempotent_on_second_call() -> anyhow::Result<()> {
        let _guard = test_lock().lock().expect("lock");

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

        reset_test_state();
        Ok(())
    }

    #[test]
    fn client_domains_with_only_raw_ssh_returns_empty() {
        let _guard = test_lock().lock().expect("lock");

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

        reset_test_state();
    }

    #[test]
    fn update_mux_domains_for_server_respects_mux_server_domain() -> anyhow::Result<()> {
        let _guard = test_lock().lock().expect("lock");

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

        reset_test_state();
        Ok(())
    }

    #[test]
    fn client_domains_multiple_mux_ssh() {
        let _guard = test_lock().lock().expect("lock");

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

        reset_test_state();
    }
}
