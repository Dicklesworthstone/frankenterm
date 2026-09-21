//! Opt-in local whole-mux publication. This does not restore a mux, move a
//! guardian lease, replicate a root, or preserve OS processes after power loss.
//!
//! Operators must separately enroll the recovery key and prepare an empty
//! publication store with `SnapshotPublicationStore::open`. Runtime admission
//! only reopens existing authority; it never creates replacement keys/stores.

use anyhow::Context as _;
use clap::Args;
use frankenterm_core::cx::{self, Cx};
use frankenterm_core::runtime_async;
use frankenterm_core::session_restore::{
    WholeMuxRecoveryVerifier, WholeMuxTrustedIdentityConfig, select_verified_recovery_roots_with_cx,
};
use frankenterm_core::snapshot_engine::{
    WholeMuxCaptureError, WholeMuxPublicationIdentity, capture_and_publish_whole_mux_recovery,
    open_recovery_key_enrollment,
};
use frankenterm_core::snapshot_publication::{
    PredecessorBinding, PublicationLimits, SnapshotPublicationStore,
};
use frankenterm_core::snapshot_representation::{RecoveryKey, RecoveryWrapContext};
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

#[derive(Args, Clone, Debug, Default)]
pub struct RecoveryOptions {
    /// Enable periodic local snapshots in an already prepared private store.
    #[arg(long, requires_all = ["recovery_enrollment", "recovery_kek", "recovery_namespace", "recovery_policy", "recovery_session", "recovery_root_id"])]
    pub recovery_store: Option<PathBuf>,
    /// Existing recovery-key enrollment directory. Never auto-created.
    #[arg(long, requires = "recovery_store")]
    pub recovery_enrollment: Option<PathBuf>,
    /// Existing private 32-byte wrapping key file; key bytes never enter argv.
    #[arg(long, requires = "recovery_store")]
    pub recovery_kek: Option<PathBuf>,
    /// Independently trusted namespace identity, 64 hex digits.
    #[arg(long, requires = "recovery_store", value_parser = parse_identity)]
    pub recovery_namespace: Option<[u8; 32]>,
    /// Independently trusted key policy identity, 64 hex digits.
    #[arg(long, requires = "recovery_store", value_parser = parse_identity)]
    pub recovery_policy: Option<[u8; 32]>,
    /// Independently trusted session identity.
    #[arg(long, requires = "recovery_store")]
    pub recovery_session: Option<String>,
    /// Independently trusted root object identity, 64 hex digits.
    #[arg(long, requires = "recovery_store", value_parser = parse_identity)]
    pub recovery_root_id: Option<[u8; 32]>,
    /// Delay after each completed attempt (seconds; default 60).
    #[arg(long, requires = "recovery_store", value_parser = clap::value_parser!(u64).range(1..=86400))]
    pub recovery_interval_seconds: Option<u64>,
    /// Local publication age target (seconds; default 120). No replica claim.
    #[arg(long, requires = "recovery_store", value_parser = clap::value_parser!(u64).range(1..=604800))]
    pub recovery_rpo_seconds: Option<u64>,
    /// Cooperative capture deadline (seconds; default 30, maximum 30).
    #[arg(long, requires = "recovery_store", value_parser = clap::value_parser!(u64).range(1..=30))]
    pub recovery_timeout_seconds: Option<u64>,
}

fn parse_identity(value: &str) -> Result<[u8; 32], String> {
    let mut identity = [0; 32];
    hex::decode_to_slice(value, &mut identity).map_err(|_| "expected 64 hex digits".to_owned())?;
    Ok(identity)
}

impl RecoveryOptions {
    /// Preserve every explicit option across daemon re-exec, before `--`.
    pub fn child_args(&self) -> Vec<std::ffi::OsString> {
        let mut args = Vec::new();
        for (flag, value) in [
            ("--recovery-store", self.recovery_store.as_ref()),
            ("--recovery-enrollment", self.recovery_enrollment.as_ref()),
            ("--recovery-kek", self.recovery_kek.as_ref()),
        ] {
            if let Some(value) = value {
                args.push(flag.into());
                args.push(value.as_os_str().to_owned());
            }
        }
        for (flag, value) in [
            (
                "--recovery-namespace",
                self.recovery_namespace.map(hex::encode),
            ),
            ("--recovery-policy", self.recovery_policy.map(hex::encode)),
            ("--recovery-session", self.recovery_session.clone()),
            ("--recovery-root-id", self.recovery_root_id.map(hex::encode)),
            (
                "--recovery-interval-seconds",
                self.recovery_interval_seconds.map(|v| v.to_string()),
            ),
            (
                "--recovery-rpo-seconds",
                self.recovery_rpo_seconds.map(|v| v.to_string()),
            ),
            (
                "--recovery-timeout-seconds",
                self.recovery_timeout_seconds.map(|v| v.to_string()),
            ),
        ] {
            if let Some(value) = value {
                args.push(flag.into());
                args.push(value.into());
            }
        }
        args
    }
}

struct CaptureState {
    store: SnapshotPublicationStore,
    key: Arc<RecoveryKey>,
    identity: WholeMuxPublicationIdentity,
}

#[derive(Clone)]
struct RestoredPredecessor {
    incarnation: String,
    generation: u64,
    envelope_hash: [u8; 32],
    image_digest: [u8; 32],
}

fn open_authority(
    options: &RecoveryOptions,
    cx: &Cx,
) -> anyhow::Result<(SnapshotPublicationStore, Arc<RecoveryKey>)> {
    cx.checkpoint()
        .map_err(|_| anyhow::anyhow!("recovery cancelled"))?;
    let (_, key) = open_recovery_key_enrollment(
        options
            .recovery_enrollment
            .as_ref()
            .context("missing enrollment")?,
        options
            .recovery_kek
            .as_ref()
            .context("missing wrapping key")?,
        &RecoveryWrapContext {
            namespace_id: options.recovery_namespace.context("missing namespace")?,
            policy_id: options.recovery_policy.context("missing policy")?,
        },
    )
    .inspect_err(|_| {
        log::error!("mux recovery authority rejection stage=key_enrollment");
    })?;
    let store = SnapshotPublicationStore::open_existing(
        options.recovery_store.as_ref().context("missing store")?,
        PublicationLimits::default(),
    )
    .inspect_err(|_| {
        log::error!("mux recovery authority rejection stage=store_reopen");
    })?;
    Ok((store, Arc::new(key)))
}

/// Authenticate an existing root before constructing or claiming any live pane.
/// Ordinary periodic startup never calls this explicit restoration entry point.
pub fn load_recovery_for_startup(
    options: &RecoveryOptions,
    custody: Option<PathBuf>,
    cx: &Cx,
) -> anyhow::Result<frankenterm_core::session_restore::ValidatedWholeMuxRecovery> {
    let session = options
        .recovery_session
        .as_deref()
        .context("missing session")?;
    anyhow::ensure!(
        !session.is_empty()
            && session.len() <= 256
            && (1..=86400).contains(&options.recovery_interval_seconds.unwrap_or(60))
            && (1..=604800).contains(&options.recovery_rpo_seconds.unwrap_or(120))
            && (1..=30).contains(&options.recovery_timeout_seconds.unwrap_or(30)),
        "invalid startup recovery session or timing bounds"
    );
    let (store, key) = open_authority(options, cx)?;
    let trusted = WholeMuxTrustedIdentityConfig::new(
        options.recovery_root_id.context("missing root identity")?,
    )
    .with_session_id(session);
    let verifier = WholeMuxRecoveryVerifier::new_production(key, trusted);
    #[cfg(unix)]
    let verifier = match custody {
        Some(path) => verifier.with_existing_guardian_custody(path),
        None => verifier,
    };
    #[cfg(not(unix))]
    anyhow::ensure!(custody.is_none(), "guardian custody requires Unix");
    let selected = select_verified_recovery_roots_with_cx(cx, &store, &verifier)?;
    anyhow::ensure!(
        !selected.has_unresolved_authority(),
        "recovery root authority requires reconciliation"
    );
    let current = selected.current.context("no authenticated recovery root")?;
    current.image().require_live_topology_state()?;
    Ok(current)
}

impl CaptureState {
    fn open(
        options: &RecoveryOptions,
        mux: &mux::Mux,
        custody: Option<PathBuf>,
        cx: &Cx,
        restored: Option<&RestoredPredecessor>,
    ) -> anyhow::Result<Self> {
        let (store, key) = open_authority(options, cx)?;
        let topology = mux
            .capture_topology_coherent(Default::default())
            .inspect_err(|error| {
                // Never format error payloads: tab errors can contain arbitrary
                // messages and topology errors carry live object identities.
                let reason = match error {
                    mux::MuxTopologyCaptureError::UnsupportedDomainPolicy => "domain_policy",
                    mux::MuxTopologyCaptureError::AuthorityExhausted => "authority_exhausted",
                    mux::MuxTopologyCaptureError::ConcurrentMutation { .. } => {
                        "concurrent_mutation"
                    }
                    mux::MuxTopologyCaptureError::WindowOrder { .. } => "window_order",
                    mux::MuxTopologyCaptureError::TabCapture { .. } => "tab_capture",
                    mux::MuxTopologyCaptureError::TooManyWindows { .. } => "window_limit",
                    mux::MuxTopologyCaptureError::TooManyTabs { .. } => "tab_limit",
                    mux::MuxTopologyCaptureError::TooManyPanes { .. } => "pane_limit",
                    mux::MuxTopologyCaptureError::TreeDepthExceeded { .. } => "tree_depth",
                    mux::MuxTopologyCaptureError::MissingPaneRegistration(_) => "pane_registration",
                    mux::MuxTopologyCaptureError::MissingDomain(_) => "missing_domain",
                    mux::MuxTopologyCaptureError::MissingDurablePaneId(_) => {
                        "missing_pane_identity"
                    }
                    mux::MuxTopologyCaptureError::NilDurablePaneId(_) => "nil_pane_identity",
                };
                log::error!("mux recovery authority rejection stage=topology reason={reason}");
            })?;
        let mut state = Self {
            store,
            key,
            identity: WholeMuxPublicationIdentity {
                generation: 1,
                session_id: options
                    .recovery_session
                    .clone()
                    .context("missing session")?,
                mux_incarnation_id: hex::encode(topology.session_incarnation.as_bytes()),
                root_object_id: options.recovery_root_id.context("missing root identity")?,
                publisher_id: "mux-periodic-local".to_owned(),
                ft_version: env!("CARGO_PKG_VERSION").to_owned(),
                predecessor: None,
                predecessor_image_digest: None,
                existing_guardian_custody: custody,
            },
        };
        state
            .select_predecessor_with_restore(cx, None, restored)
            .inspect_err(|_| {
                log::error!("mux recovery authority rejection stage=predecessor_selection");
            })?;
        Ok(state)
    }

    fn select_predecessor(&mut self, cx: &Cx, receipt: Option<(u64, &str)>) -> anyhow::Result<()> {
        self.select_predecessor_with_restore(cx, receipt, None)
    }

    fn select_predecessor_with_restore(
        &mut self,
        cx: &Cx,
        receipt: Option<(u64, &str)>,
        restored: Option<&RestoredPredecessor>,
    ) -> anyhow::Result<()> {
        let trusted = WholeMuxTrustedIdentityConfig::new(self.identity.root_object_id)
            .with_session_id(self.identity.session_id.clone());
        // Verify both retained generations under the trusted key/root/session.
        // A successful restore leaves the old incarnation in the previous slot;
        // pinning every candidate to the successor would misclassify that valid
        // predecessor as corruption. Pin the selected current root below instead.
        let verifier = WholeMuxRecoveryVerifier::new_production(Arc::clone(&self.key), trusted);
        #[cfg(unix)]
        let verifier = match self.identity.existing_guardian_custody.as_ref() {
            Some(path) => verifier.with_existing_guardian_custody(path.clone()),
            None => verifier,
        };
        let selected = select_verified_recovery_roots_with_cx(cx, &self.store, &verifier)?;
        // Never silently fall back past corruption and overwrite a newer root.
        anyhow::ensure!(
            !selected.has_unresolved_authority(),
            "recovery root authority requires reconciliation"
        );
        match selected.current {
            Some(current) => {
                let expected_incarnation = restored
                    .map(|source| &source.incarnation)
                    .unwrap_or(&self.identity.mux_incarnation_id);
                anyhow::ensure!(
                    current.image().header.mux_incarnation_id == *expected_incarnation,
                    "selected recovery root belongs to another mux incarnation"
                );
                let hash = hex::encode(current.root_envelope_sha256());
                if let Some(restored) = restored {
                    anyhow::ensure!(
                        current.generation() == restored.generation
                            && current.root_envelope_sha256() == restored.envelope_hash
                            && current.image().image_digest == restored.image_digest,
                        "restored predecessor no longer names the selected recovery root"
                    );
                }
                if let Some((generation, expected_hash)) = receipt {
                    anyhow::ensure!(
                        current.generation() == generation && hash == expected_hash,
                        "published recovery root selection differs from receipt"
                    );
                }
                self.identity.generation = current
                    .generation()
                    .checked_add(1)
                    .context("recovery generation exhausted")?;
                self.identity.predecessor = Some(PredecessorBinding {
                    expected_generation: current.generation(),
                    expected_hash: hash,
                });
                self.identity.predecessor_image_digest = Some(current.image().image_digest);
            }
            None => anyhow::ensure!(
                receipt.is_none() && restored.is_none(),
                "published or restored recovery root is missing"
            ),
        }
        Ok(())
    }
}

struct AttemptResult {
    state: Option<CaptureState>,
    published_generation: Option<u64>,
    error: Option<&'static str>,
    terminal: bool,
}

type CaptureFuture = Pin<Box<dyn Future<Output = Result<AttemptResult, String>>>>;

/// Main-loop-owned maintenance task. `poll` performs no filesystem/codec work.
/// Its retained blocking future is never raced against cancellation: shutdown
/// cancels the operation Cx, then continues polling and ticking the executor
/// until the actual closure has returned, before shutting down the mux.
pub struct PeriodicRecovery {
    options: RecoveryOptions,
    mux: Arc<mux::Mux>,
    custody: Option<PathBuf>,
    restored: Option<RestoredPredecessor>,
    state: Option<CaptureState>,
    active: Option<CaptureFuture>,
    operation_cx: Option<Cx>,
    attempt_started: Option<Instant>,
    next_due: Instant,
    last_success: Option<Instant>,
    stopping: bool,
    failed: bool,
    interval: Duration,
    rpo: Duration,
    timeout: Duration,
    #[cfg(test)]
    before_capture: Option<Box<dyn FnOnce() + Send>>,
}

impl PeriodicRecovery {
    pub fn new(
        options: RecoveryOptions,
        mux: Arc<mux::Mux>,
        custody: Option<PathBuf>,
    ) -> anyhow::Result<Option<Self>> {
        if options.recovery_store.is_none() {
            return Ok(None);
        }
        let session = options
            .recovery_session
            .as_deref()
            .context("missing recovery session")?;
        anyhow::ensure!(
            !session.is_empty() && session.len() <= 256,
            "recovery session must contain 1..256 bytes"
        );
        anyhow::ensure!(
            options.recovery_enrollment.is_some()
                && options.recovery_kek.is_some()
                && options.recovery_namespace.is_some()
                && options.recovery_policy.is_some()
                && options.recovery_root_id.is_some(),
            "incomplete periodic recovery authority"
        );
        let interval = options.recovery_interval_seconds.unwrap_or(60);
        let rpo = options.recovery_rpo_seconds.unwrap_or(120);
        let timeout = options.recovery_timeout_seconds.unwrap_or(30);
        anyhow::ensure!(
            (1..=86400).contains(&interval)
                && (1..=604800).contains(&rpo)
                && (1..=30).contains(&timeout),
            "invalid periodic recovery timing bounds"
        );
        Ok(Some(Self {
            options,
            mux,
            custody,
            restored: None,
            state: None,
            active: None,
            operation_cx: None,
            attempt_started: None,
            next_due: Instant::now(),
            last_success: None,
            stopping: false,
            failed: false,
            interval: Duration::from_secs(interval),
            rpo: Duration::from_secs(rpo),
            timeout: Duration::from_secs(timeout),
            #[cfg(test)]
            before_capture: None,
        }))
    }

    /// Resume publication only after the authenticated graph was installed in
    /// this exact owner. The first attempt re-verifies the source root on disk;
    /// later attempts are pinned to the successor incarnation as usual.
    pub fn new_after_restore(
        options: RecoveryOptions,
        mux: Arc<mux::Mux>,
        custody: Option<PathBuf>,
        restored: &frankenterm_core::session_restore::PublishedWholeMuxTopology,
    ) -> anyhow::Result<Self> {
        anyhow::ensure!(restored.matches_owner(&mux), "restored mux owner mismatch");
        let source = restored.source();
        anyhow::ensure!(
            options.recovery_session.as_deref() == Some(source.image().header.session_id.as_str()),
            "restored recovery session mismatch"
        );
        let mut controller = Self::new(options, mux, custody)?
            .context("restored mux requires configured recovery publication")?;
        controller.restored = Some(RestoredPredecessor {
            incarnation: source.image().header.mux_incarnation_id.clone(),
            generation: source.generation(),
            envelope_hash: source.root_envelope_sha256(),
            image_digest: source.image().image_digest,
        });
        Ok(controller)
    }

    pub fn request_shutdown(&mut self) {
        self.stopping = true;
        if let Some(cx) = &self.operation_cx {
            cx.cancel_with(
                frankenterm_core::outcome::CancelKind::User,
                Some("mux periodic recovery shutdown"),
            );
        }
    }

    pub fn is_settled(&self) -> bool {
        self.active.is_none()
    }

    pub fn poll(&mut self, startup_complete: bool) {
        let now = Instant::now();
        if let Some(active) = self.active.as_mut() {
            let mut context = Context::from_waker(futures::task::noop_waker_ref());
            if let Poll::Ready(result) = active.as_mut().poll(&mut context) {
                self.active = None;
                self.operation_cx = None;
                self.next_due = Instant::now() + self.interval;
                match result {
                    Ok(result) => {
                        self.state = result.state;
                        self.failed = result.terminal;
                        if let Some(generation) = result.published_generation {
                            // The optimistic cut happened no earlier than the
                            // attempt start. Do not understate RPO age by
                            // refreshing it at the end of encode/verification.
                            self.last_success = self.attempt_started;
                            metrics::counter!("mux.recovery.publication", "outcome" => "published_local").increment(1);
                            log::info!(
                                "mux periodic recovery published local generation={generation}; replica durability unproven"
                            );
                        }
                        if let Some(error) = result.error {
                            metrics::counter!("mux.recovery.publication", "outcome" => error)
                                .increment(1);
                            log::error!(
                                "mux periodic recovery outcome={error} disabled={}",
                                result.terminal
                            );
                        }
                    }
                    Err(_) => {
                        self.failed = true;
                        log::error!(
                            "mux periodic recovery blocking task failed; publication lane disabled"
                        );
                        metrics::counter!("mux.recovery.publication", "outcome" => "worker_failed")
                            .increment(1);
                    }
                }
            }
        }
        let age = self
            .last_success
            .map(|success| now.saturating_duration_since(success));
        metrics::gauge!("mux.recovery.local_rpo_proven").set(
            if age.is_some_and(|age| age <= self.rpo) {
                1.0
            } else {
                0.0
            },
        );
        if let Some(age) = age {
            metrics::gauge!("mux.recovery.local_age_seconds").set(age.as_secs_f64());
        }
        if self.active.is_some()
            || self.stopping
            || self.failed
            || !startup_complete
            || now < self.next_due
        {
            return;
        }
        let cx = cx::for_request();
        self.operation_cx = Some(cx.clone());
        self.attempt_started = Some(now);
        let options = self.options.clone();
        let mux = Arc::clone(&self.mux);
        let custody = self.custody.clone();
        let restored = self.restored.clone();
        let prior = self.state.take();
        let timeout = self.timeout;
        #[cfg(test)]
        let before_capture = self.before_capture.take();
        self.active = Some(Box::pin(runtime_async::spawn_blocking(move || {
            let mut state = match prior.map(Ok).unwrap_or_else(|| {
                CaptureState::open(&options, &mux, custody, &cx, restored.as_ref())
            }) {
                Ok(state) => state,
                Err(_) => {
                    return AttemptResult {
                        state: None,
                        published_generation: None,
                        error: Some("authority_rejected"),
                        terminal: true,
                    };
                }
            };
            #[cfg(test)]
            if let Some(before_capture) = before_capture {
                before_capture();
            }
            let outcome = capture_and_publish_whole_mux_recovery(
                &cx,
                &mux,
                &state.store,
                Arc::clone(&state.key),
                &state.identity,
                timeout,
            );
            #[cfg(test)]
            if let Err(error) = &outcome {
                eprintln!("periodic recovery test capture failed: {error:?}");
            }
            let (published_generation, error, terminal) = match outcome {
                Ok(receipt) => {
                    // Verification settlement must finish even if shutdown
                    // arrived after durable publication. It never writes.
                    match state.select_predecessor(
                        &cx::for_request(),
                        Some((receipt.generation, &receipt.sha256)),
                    ) {
                        Ok(()) => (Some(receipt.generation), None, false),
                        Err(_) => (None, Some("published_root_verification_failed"), true),
                    }
                }
                Err(WholeMuxCaptureError::Busy) => (None, Some("busy"), false),
                Err(
                    WholeMuxCaptureError::CaptureDeadline
                    | WholeMuxCaptureError::StaleCapture
                    | WholeMuxCaptureError::Topology(_)
                    | WholeMuxCaptureError::Pane { .. }
                    | WholeMuxCaptureError::GuardianPane { .. },
                ) => (None, Some("capture_rejected"), false),
                Err(WholeMuxCaptureError::Context(_)) => (None, Some("cancelled"), false),
                Err(_) => (None, Some("publication_requires_reconciliation"), true),
            };
            AttemptResult {
                state: Some(state),
                published_generation,
                error,
                terminal,
            }
        })));
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use frankenterm_core::runtime_async::{CompatRuntime, RuntimeBuilder};
    use frankenterm_core::snapshot_engine::enroll_recovery_key;
    use mux::domain::{Domain, LocalDomain};
    use std::io::Write as _;
    use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};

    fn prepared() -> (tempfile::TempDir, RecoveryOptions) {
        // RCH's TMPDIR can sit beneath a group-writable checkout. Resolve the
        // platform /tmp instead; on macOS this also removes /var symlinks.
        // The production key opener must keep rejecting unsafe ancestors.
        let parent = std::fs::canonicalize("/tmp").unwrap();
        let directory = tempfile::Builder::new()
            .permissions(std::fs::Permissions::from_mode(0o700))
            .tempdir_in(parent)
            .unwrap();
        let kek = directory.path().join("wrapping-key");
        std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(&kek)
            .unwrap()
            .write_all(&[0x51; 32])
            .unwrap();
        let enrollment = directory.path().join("enrollment");
        let context = RecoveryWrapContext {
            namespace_id: [0x52; 32],
            policy_id: [0x53; 32],
        };
        enroll_recovery_key(&enrollment, &kek, &context).unwrap();
        let store = directory.path().join("publication");
        SnapshotPublicationStore::open(&store, PublicationLimits::default()).unwrap();
        (
            directory,
            RecoveryOptions {
                recovery_store: Some(store),
                recovery_enrollment: Some(enrollment),
                recovery_kek: Some(kek),
                recovery_namespace: Some(context.namespace_id),
                recovery_policy: Some(context.policy_id),
                recovery_root_id: Some([0x54; 32]),
                recovery_session: Some("periodic-real-mux".to_owned()),
                recovery_interval_seconds: Some(1),
                recovery_rpo_seconds: Some(2),
                recovery_timeout_seconds: Some(5),
            },
        )
    }

    fn pump_until(
        controller: &mut PeriodicRecovery,
        executor: &promise::spawn::SimpleExecutor,
        done: impl Fn(&PeriodicRecovery) -> bool,
    ) {
        let deadline = Instant::now() + Duration::from_secs(15);
        while !done(controller) {
            controller.poll(true);
            executor.try_tick().unwrap();
            // Expected rejection tests satisfy `done` on terminal failure;
            // successful-publication tests must not turn rejection into a
            // misleading scheduling timeout.
            assert!(
                done(controller) || !controller.failed,
                "recovery became terminal before the requested state: settled={} next_generation={:?}",
                controller.is_settled(),
                controller
                    .state
                    .as_ref()
                    .map(|state| state.identity.generation),
            );
            assert!(Instant::now() < deadline, "owned capture did not settle");
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    fn selected(
        options: &RecoveryOptions,
    ) -> frankenterm_core::session_restore::ValidatedWholeMuxRecovery {
        let (_, key) = open_recovery_key_enrollment(
            options.recovery_enrollment.as_ref().unwrap(),
            options.recovery_kek.as_ref().unwrap(),
            &RecoveryWrapContext {
                namespace_id: options.recovery_namespace.unwrap(),
                policy_id: options.recovery_policy.unwrap(),
            },
        )
        .unwrap();
        let store = SnapshotPublicationStore::open_existing(
            options.recovery_store.as_ref().unwrap(),
            PublicationLimits::default(),
        )
        .unwrap();
        let verifier = WholeMuxRecoveryVerifier::new_production(
            Arc::new(key),
            WholeMuxTrustedIdentityConfig::new(options.recovery_root_id.unwrap())
                .with_session_id(options.recovery_session.as_ref().unwrap()),
        );
        let selection =
            select_verified_recovery_roots_with_cx(&cx::for_request(), &store, &verifier).unwrap();
        assert!(!selection.has_unresolved_authority());
        selection.current.unwrap()
    }

    #[test]
    fn periodic_recovery_real_pty_publishes_consecutive_authenticated_roots() {
        let _serial = crate::GLOBAL_STATE_TEST_LOCK.lock().unwrap();
        let executor = promise::spawn::SimpleExecutor::new();
        let (_directory, options) = prepared();
        let domain: Arc<dyn Domain> = Arc::new(LocalDomain::new("periodic-real-pty").unwrap());
        let mux = Arc::new(mux::Mux::new(Some(Arc::clone(&domain))));
        let window = mux.new_empty_window(None, None);
        let runtime = RuntimeBuilder::current_thread().build().unwrap();
        let script = r#"/bin/stty -echo || exit 1; printf '\033]2;first-root\007'; while IFS= read -r title; do printf '\033]2;%s\007' "$title"; done"#;
        let command = config::Config::default()
            .build_prog(
                Some(vec![
                    std::ffi::OsStr::new("/bin/sh"),
                    std::ffi::OsStr::new("-c"),
                    std::ffi::OsStr::new(script),
                ]),
                None,
                None,
            )
            .unwrap();
        let tab = runtime
            .block_on(domain.spawn(
                &mux,
                wezterm_term::TerminalSize {
                    rows: 24,
                    cols: 80,
                    pixel_width: 640,
                    pixel_height: 384,
                    dpi: 96,
                },
                Some(command),
                None,
                *window,
            ))
            .unwrap();
        struct OwnedPane {
            mux: Arc<mux::Mux>,
            pane: Arc<dyn mux::pane::Pane>,
        }
        impl Drop for OwnedPane {
            fn drop(&mut self) {
                self.pane.kill();
                self.mux.remove_pane(self.pane.pane_id());
            }
        }
        let owned = OwnedPane {
            mux: Arc::clone(&mux),
            pane: tab.get_active_pane().unwrap(),
        };
        let wait_title = |title: &str| {
            let deadline = Instant::now() + Duration::from_secs(5);
            while owned.pane.get_title() != title {
                executor.try_tick().unwrap();
                assert!(Instant::now() < deadline, "actual PTY title did not arrive");
                std::thread::sleep(Duration::from_millis(1));
            }
        };
        wait_title("first-root");
        let mut controller = PeriodicRecovery::new(options.clone(), Arc::clone(&mux), None)
            .unwrap()
            .unwrap();
        controller.poll(false);
        assert!(controller.active.is_none(), "startup has not completed");
        pump_until(&mut controller, &executor, |c| {
            c.state.as_ref().is_some_and(|s| s.identity.generation == 2)
        });
        let first = selected(&options);
        assert_eq!(first.generation(), 1);
        assert_eq!(first.pane_count(), 1);
        assert_eq!(first.image().panes[0].title, "first-root");
        assert!(
            controller.next_due > Instant::now(),
            "cadence starts after completion"
        );
        {
            let mut writer = owned.pane.writer();
            writeln!(writer, "second-root").unwrap();
            writer.flush().unwrap();
        }
        wait_title("second-root");
        pump_until(&mut controller, &executor, |c| {
            c.state.as_ref().is_some_and(|s| s.identity.generation == 3)
        });
        let second = selected(&options);
        assert_eq!(second.generation(), 2);
        assert_ne!(second.root_envelope_sha256(), first.root_envelope_sha256());
        assert_eq!(second.pane_count(), 1);
        assert_eq!(second.image().panes[0].title, "second-root");
        controller.request_shutdown();
        pump_until(&mut controller, &executor, PeriodicRecovery::is_settled);
        assert!(controller.last_success.is_some());

        let before = files(_directory.path());
        let replacement = Arc::new(mux::Mux::new(None));
        let mut restarted = PeriodicRecovery::new(options.clone(), replacement, None)
            .unwrap()
            .unwrap();
        pump_until(&mut restarted, &executor, |c| c.failed && c.is_settled());
        assert!(restarted.last_success.is_none());
        assert_eq!(files(_directory.path()), before);
        assert_eq!(
            selected(&options).root_envelope_sha256(),
            second.root_envelope_sha256()
        );
    }

    #[test]
    fn periodic_recovery_atomic_restore_advances_only_exact_predecessor() {
        use mux::domain::{Domain, LocalDomain};

        let _serial = crate::GLOBAL_STATE_TEST_LOCK.lock().unwrap();
        let executor = promise::spawn::SimpleExecutor::new();
        for (superseded, damaged) in [(false, false), (false, true), (true, false), (true, true)] {
            let (directory, options) = prepared();
            let domain: Arc<dyn Domain> = Arc::new(LocalDomain::new("retained-empty").unwrap());
            let original = Arc::new(mux::Mux::new(Some(domain)));
            let mut predecessor = PeriodicRecovery::new(options.clone(), original, None)
                .unwrap()
                .unwrap();
            pump_until(&mut predecessor, &executor, |c| {
                c.state.as_ref().is_some_and(|s| s.identity.generation == 2)
            });
            if damaged {
                let path = options
                    .recovery_store
                    .as_ref()
                    .unwrap()
                    .join("roots/slot_a.root");
                let mut bytes = std::fs::read(&path).unwrap();
                bytes[0] ^= 1;
                std::fs::write(&path, bytes).unwrap();
                let (store, key) = open_authority(&options, &cx::for_request()).unwrap();
                let verifier = WholeMuxRecoveryVerifier::new_production(
                    key,
                    WholeMuxTrustedIdentityConfig::new(options.recovery_root_id.unwrap())
                        .with_session_id(options.recovery_session.as_ref().unwrap()),
                );
                let selection =
                    select_verified_recovery_roots_with_cx(&cx::for_request(), &store, &verifier)
                        .unwrap();
                assert_eq!(selection.torn_or_rejected.len(), 1);
                assert_eq!(selection.torn_or_rejected[0].authority,
                    frankenterm_core::snapshot_publication::RootDiagnosticAuthority::AuthenticatedReconstruction);
                assert!(
                    selection.torn_or_rejected[0]
                        .reason
                        .starts_with("corrupt envelope:")
                );
                assert!(!selection.has_unresolved_authority());
                assert_eq!(selection.current.unwrap().generation(), 1);
                // Authenticated repair must not forgive a filesystem authority failure.
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
                assert!(load_recovery_for_startup(&options, None, &cx::for_request()).is_err());
                let zero_evidence_store = SnapshotPublicationStore::open_existing(
                    options.recovery_store.as_ref().unwrap(),
                    PublicationLimits {
                        max_error_records: 0,
                        ..PublicationLimits::default()
                    },
                )
                .unwrap();
                let rejected = select_verified_recovery_roots_with_cx(
                    &cx::for_request(),
                    &zero_evidence_store,
                    &verifier,
                )
                .unwrap();
                assert!(rejected.torn_or_rejected.is_empty());
                assert!(rejected.has_unresolved_authority());
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
                let repaired = select_verified_recovery_roots_with_cx(
                    &cx::for_request(),
                    &zero_evidence_store,
                    &verifier,
                )
                .unwrap();
                assert!(repaired.torn_or_rejected.is_empty());
                assert!(!repaired.has_unresolved_authority());
                assert_eq!(repaired.current.unwrap().generation(), 1);
                // Reject invalid discovery even though the repair symbols are intact.
                let discovery = options
                    .recovery_store
                    .as_ref()
                    .unwrap()
                    .join("generations/slot_a.discovery");
                let original = std::fs::read(&discovery).unwrap();
                let mut changed = original.clone();
                *changed.last_mut().unwrap() ^= 1;
                std::fs::write(&discovery, changed).unwrap();
                assert!(load_recovery_for_startup(&options, None, &cx::for_request()).is_err());
                // An intact ordinary root must not mask invalid discovery when
                // the caller requests zero retained diagnostic records.
                let damaged_root = std::fs::read(&path).unwrap();
                let mut intact_root = damaged_root.clone();
                intact_root[0] ^= 1;
                std::fs::write(&path, intact_root).unwrap();
                let rejected = select_verified_recovery_roots_with_cx(
                    &cx::for_request(),
                    &zero_evidence_store,
                    &verifier,
                )
                .unwrap();
                assert!(
                    rejected.torn_or_rejected.is_empty() && rejected.has_unresolved_authority()
                );
                std::fs::write(&path, &damaged_root).unwrap();
                std::fs::write(discovery, original).unwrap();
                // Make a readable generation-2 ordinary envelope containing the
                // generation-1 encrypted graph; keep valid generation-1 discovery.
                // The outer checksum is not authority: graph authentication must
                // reject generation 2 even though older discovery still repairs.
                use sha2::{Digest, Sha256};
                let mut intact_root = damaged_root.clone();
                intact_root[0] ^= 1;
                let magic_len = frankenterm_core::snapshot_publication::ENVELOPE_MAGIC.len();
                let header_start = magic_len + 4;
                let header_len =
                    u32::from_le_bytes(intact_root[magic_len..header_start].try_into().unwrap())
                        as usize;
                let header_end = header_start + header_len;
                let mut header: frankenterm_core::snapshot_publication::GenerationEnvelopeHeader =
                    serde_json::from_slice(&intact_root[header_start..header_end]).unwrap();
                header.generation = 2;
                let encoded_header = serde_json::to_vec(&header).unwrap();
                let mut newer = intact_root[..magic_len].to_vec();
                newer
                    .extend_from_slice(&u32::try_from(encoded_header.len()).unwrap().to_le_bytes());
                newer.extend_from_slice(&encoded_header);
                newer.extend_from_slice(&intact_root[header_end..intact_root.len() - 32]);
                let checksum = Sha256::digest(&newer);
                newer.extend_from_slice(&checksum);
                std::fs::write(&path, newer).unwrap();
                let rejected =
                    select_verified_recovery_roots_with_cx(&cx::for_request(), &store, &verifier)
                        .unwrap();
                assert_eq!(rejected.current.as_ref().unwrap().generation(), 1);
                assert!(rejected.has_unresolved_authority());
                assert!(
                    rejected
                        .torn_or_rejected
                        .iter()
                        .any(|diagnostic| diagnostic.generation == Some(2)
                            && diagnostic.reason == "root graph rejected")
                );
                assert!(load_recovery_for_startup(&options, None, &cx::for_request()).is_err());
                std::fs::write(&path, damaged_root).unwrap();
            }
            // The damaged case must authenticate real persisted repair data at
            // startup, then exercise CaptureState::open and two publications
            // below. Superseded cases must still refuse without writing.
            let before_load = files(directory.path());
            let source = load_recovery_for_startup(&options, None, &cx::for_request()).unwrap();
            assert_eq!(files(directory.path()), before_load);
            let source_digest = source.image().image_digest;
            let successor = Arc::new(mux::Mux::new(None));
            let published = crate::guardian_proxy::restore_from_options(
                Arc::clone(&successor),
                &options,
                None,
                wezterm_term::terminalstate::checkpoint::TerminalCheckpointLimits::default(),
            )
            .unwrap();
            assert!(
                PeriodicRecovery::new_after_restore(
                    options.clone(),
                    Arc::new(mux::Mux::new(None)),
                    None,
                    &published,
                )
                .is_err()
            );
            if superseded {
                // Change the durable root after the restored graph was installed.
                // The successor must refuse rather than overwrite that newer root.
                predecessor.next_due = Instant::now();
                pump_until(&mut predecessor, &executor, |c| {
                    c.state.as_ref().is_some_and(|s| s.identity.generation == 3)
                });
            }
            predecessor.request_shutdown();
            pump_until(&mut predecessor, &executor, PeriodicRecovery::is_settled);
            let before = files(directory.path());
            let mut controller = PeriodicRecovery::new_after_restore(
                options.clone(),
                Arc::clone(&successor),
                None,
                &published,
            )
            .unwrap();
            if superseded {
                pump_until(&mut controller, &executor, |c| c.failed && c.is_settled());
                assert!(controller.last_success.is_none());
                assert_eq!(files(directory.path()), before);
            } else {
                pump_until(&mut controller, &executor, |c| {
                    c.state.as_ref().is_some_and(|s| s.identity.generation == 3)
                });
                let next = selected(&options);
                assert_eq!(next.generation(), 2);
                assert_eq!(next.image().header.predecessor_digest, Some(source_digest));
                assert_eq!(
                    next.image().header.mux_incarnation_id,
                    hex::encode(
                        successor
                            .topology_snapshot_authority()
                            .unwrap()
                            .0
                            .as_bytes()
                    )
                );
                assert_eq!(next.image().topology.domains.len(), 1);
                assert_eq!(
                    next.image().topology.domains[0].domain_name,
                    "retained-empty"
                );
                controller.next_due = Instant::now();
                pump_until(&mut controller, &executor, |c| {
                    c.state.as_ref().is_some_and(|s| s.identity.generation == 4)
                });
                let following = selected(&options);
                assert_eq!(following.generation(), 3);
                assert_eq!(
                    following.image().header.predecessor_digest,
                    Some(next.image().image_digest)
                );
            }
            controller.request_shutdown();
            pump_until(&mut controller, &executor, PeriodicRecovery::is_settled);
        }
    }

    fn files(root: &std::path::Path) -> std::collections::BTreeMap<PathBuf, Vec<u8>> {
        let mut result = std::collections::BTreeMap::new();
        for entry in std::fs::read_dir(root).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                result.extend(files(&path));
            } else {
                result.insert(path.clone(), std::fs::read(path).unwrap());
            }
        }
        result
    }

    #[test]
    fn periodic_recovery_missing_or_corrupt_authority_never_publishes() {
        let _serial = crate::GLOBAL_STATE_TEST_LOCK.lock().unwrap();
        let executor = promise::spawn::SimpleExecutor::new();
        for fault in 0..5 {
            let (directory, mut options) = prepared();
            if fault == 0 {
                options.recovery_kek = Some(directory.path().join("missing-key"));
            } else if fault == 1 {
                options.recovery_store = Some(directory.path().join("missing-store"));
            } else if fault == 2 {
                options.recovery_policy = Some([0xff; 32]);
            } else if fault == 3 {
                let path = options
                    .recovery_store
                    .as_ref()
                    .unwrap()
                    .join("roots")
                    .join("slot_a.root");
                std::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .mode(0o600)
                    .open(path)
                    .unwrap()
                    .write_all(b"corrupt root authority")
                    .unwrap();
            } else {
                let path = options
                    .recovery_enrollment
                    .as_ref()
                    .unwrap()
                    .join("recovery-key.wrapped");
                let mut bytes = std::fs::read(&path).unwrap();
                bytes[0] ^= 1;
                std::fs::write(path, bytes).unwrap();
            }
            let before = files(directory.path());
            let mut controller =
                PeriodicRecovery::new(options, Arc::new(mux::Mux::new(None)), None)
                    .unwrap()
                    .unwrap();
            pump_until(&mut controller, &executor, |c| c.failed && c.is_settled());
            assert!(controller.last_success.is_none());
            assert_eq!(files(directory.path()), before);
        }
    }

    #[test]
    fn periodic_recovery_shutdown_waits_for_actual_blocking_capture() {
        let _serial = crate::GLOBAL_STATE_TEST_LOCK.lock().unwrap();
        let executor = promise::spawn::SimpleExecutor::new();
        let (directory, options) = prepared();
        let before = files(directory.path());
        let mut controller = PeriodicRecovery::new(options, Arc::new(mux::Mux::new(None)), None)
            .unwrap()
            .unwrap();
        let (entered_tx, entered_rx) = std::sync::mpsc::sync_channel(1);
        let (release_tx, release_rx) = std::sync::mpsc::sync_channel(1);
        controller.before_capture = Some(Box::new(move || {
            entered_tx.send(()).unwrap();
            release_rx
                .recv_timeout(Duration::from_secs(5))
                .expect("release real capture boundary");
        }));
        controller.poll(true);
        controller.poll(true); // Poll the owned future to start actual blocking work.
        entered_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        controller.request_shutdown();
        controller.poll(true);
        assert!(
            !controller.is_settled(),
            "cancellation is not task completion"
        );
        release_tx.send(()).unwrap();
        pump_until(&mut controller, &executor, PeriodicRecovery::is_settled);
        assert!(controller.last_success.is_none());
        assert_eq!(files(directory.path()), before);
        controller.poll(true);
        assert!(
            controller.is_settled(),
            "shutdown cannot schedule another capture"
        );
    }
}
