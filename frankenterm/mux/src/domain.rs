//! A Domain represents an instance of a multiplexer.
//! For example, the gui frontend has its own domain,
//! and we can connect to a domain hosted by a mux server
//! that may be local, running "remotely" inside a WSL
//! container or actually remote, running on the other end
//! of an ssh session somewhere.

use crate::client::ClientId;
use crate::localpane::LocalPane;
use crate::pane::{alloc_pane_id, Pane, PaneId};
use crate::tab::{SplitRequest, Tab};
use crate::window::WindowId;
use crate::{
    MoveCommitReceipt, Mux, PaneOperationGuard, PaneRegistrationHandle, SplitCommitReceipt,
};
use anyhow::{bail, Context, Error};
use async_trait::async_trait;
use config::keyassignment::{SpawnCommand, SpawnTabDomain};
use config::{configuration, ExecDomain, SerialDomain, ValueOrFunc, WslDomain};
use downcast_rs::{impl_downcast, Downcast};
use frankenterm_sigpipe::{catch_recoverable, RecoverablePanicSite};
use frankenterm_term::TerminalSize;
use parking_lot::Mutex;
use portable_pty::{
    native_pty_system, CommandBuilder, ExitStatus, MasterPty, PtyPair, PtySize, PtySystem,
};
use std::collections::HashMap;
use std::ffi::OsString;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

static DOMAIN_ID: ::std::sync::atomic::AtomicUsize = ::std::sync::atomic::AtomicUsize::new(0);
pub(crate) fn reserve_recovered_domain_ids(maximum: usize) -> anyhow::Result<()> {
    crate::reserve_recovered_ids(&DOMAIN_ID, maximum, "domain")
}
pub type DomainId = usize;

/// A supported policy cannot be read while its live admission or lock is held.
/// This marker must not be used for unsupported policy or unresolved custody.
#[derive(Debug, thiserror::Error)]
#[error("domain recovery policy is temporarily busy")]
pub struct DomainRecoveryPolicyBusy;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DomainState {
    Detached,
    Attached,
}

pub fn alloc_domain_id() -> DomainId {
    crate::next_unique_usize_id(&DOMAIN_ID, "mux domain")
}

pub(crate) fn register_spawned_pane_or_rollback(
    mux: &Arc<Mux>,
    pane: &Arc<dyn Pane>,
) -> anyhow::Result<()> {
    if let Err(error) = mux.add_pane(pane) {
        let rollback = catch_recoverable(
            RecoverablePanicSite::MuxRegistrationRollback,
            std::panic::AssertUnwindSafe(|| pane.kill()),
        );
        if rollback.is_err() {
            log::error!(
                "spawned-pane registration rollback panicked for exact pane identity {:p}",
                Arc::as_ptr(pane)
            );
        }
        return Err(error);
    }
    Ok(())
}

/// An exact pane process/PTY that has not yet been published in a mux.
///
/// Native construction remains crate-private. Guardian construction consumes
/// an owned LocalPane and checks its private lease and replay facets. Dropping
/// the reservation kills a native child, but retires only a guardian lease;
/// a failed publication must not close an independently owned guardian child.
#[must_use = "an unpublished pane must be published or allowed to roll back"]
pub struct UnpublishedPane {
    pane: Option<Arc<dyn Pane>>,
    rollback: UnpublishedPaneRollback,
    guardian_publication: Option<Arc<AtomicBool>>,
}

/// Private restored topology and its still-armed pane rollback custody.
///
/// This is construction only, not a live registration capability. In particular
/// no reader is taken, no thread is started, no global IDs are reserved, and no
/// callback or topology notification is delivered. Dropping a failed or unused
/// preparation retains the normal guardian lease-only rollback behavior.
pub struct UnpublishedRecoveredTopology {
    // Drop topology references before the last pane custody references.
    pub(crate) windows: Vec<crate::window::Window>,
    pub(crate) tabs: HashMap<crate::tab::TabId, Arc<Tab>>,
    pub(crate) panes: Vec<UnpublishedPane>,
    captured_windows: Vec<crate::MuxCapturedWindow>,
    captured_tabs: Vec<crate::tab::MuxCapturedTab>,
    captured_domains: Vec<crate::MuxCapturedDomain>,
    default_domain_id: Option<DomainId>,
    default_workspace: String,
}

impl UnpublishedRecoveredTopology {
    /// Assemble already-authenticated metadata without granting its caller
    /// publication authority. The core recovery adapter retains the typed root
    /// verification alongside this value; these DTOs alone are not that proof.
    pub fn prepare(
        captured_windows: Vec<crate::MuxCapturedWindow>,
        captured_tabs: Vec<crate::tab::MuxCapturedTab>,
        captured_domains: Vec<crate::MuxCapturedDomain>,
        default_domain_id: Option<DomainId>,
        default_workspace: String,
        expected_panes: &HashMap<PaneId, ([u8; 16], DomainId)>,
        panes: Vec<UnpublishedPane>,
    ) -> anyhow::Result<Self> {
        anyhow::ensure!(
            panes.len() <= 4096
                && captured_windows.len() <= 4096
                && captured_tabs.len() <= 4096
                && captured_domains.len() <= 256,
            "recovered topology exceeds construction bounds"
        );
        anyhow::ensure!(
            panes.len() == expected_panes.len(),
            "recovered pane custody count mismatch"
        );
        anyhow::ensure!(
            !default_workspace.is_empty() && default_workspace.len() <= 65536,
            "invalid recovered default workspace"
        );
        let mut domain_ids = std::collections::HashSet::new();
        let mut domain_names = std::collections::HashSet::new();
        let mut policy_bytes = 0usize;
        for domain in &captured_domains {
            domain.policy.validate()?;
            anyhow::ensure!(
                domain.domain_id != usize::MAX
                    && !domain.name.is_empty()
                    && domain.name.len() <= 65536
                    && !domain.name.contains('\0')
                    && domain_ids.insert(domain.domain_id)
                    && domain_names.insert(domain.name.as_str()),
                "invalid or duplicate recovered domain identity"
            );
            anyhow::ensure!(
                domain.state == DomainState::Attached,
                "supported recovered local domain must be attached"
            );
            policy_bytes = policy_bytes
                .checked_add(
                    domain
                        .policy
                        .payload_bytes()
                        .context("recovered domain policy size overflow")?,
                )
                .and_then(|bytes| bytes.checked_add(domain.name.len()))
                .filter(|bytes| *bytes <= 16 * 1024 * 1024)
                .context("recovered domain policy aggregate exceeds byte budget")?;
        }
        anyhow::ensure!(
            default_domain_id.is_none_or(|id| domain_ids.contains(&id)),
            "recovered default domain is absent from domain census"
        );
        anyhow::ensure!(
            captured_domains.is_empty() == default_domain_id.is_none(),
            "nonempty recovered domain census requires its exact default"
        );
        anyhow::ensure!(
            expected_panes
                .values()
                .all(|(_, domain)| domain_ids.contains(domain)),
            "recovered pane domain is absent from domain census"
        );
        let mut pane_map = HashMap::new();
        for owned in &panes {
            let pane = owned.pane();
            let id = pane.pane_id();
            let expected = expected_panes
                .get(&id)
                .context("unexpected recovered pane custody")?;
            anyhow::ensure!(
                pane.durable_pane_id() == Some(expected.0) && pane.domain_id() == expected.1,
                "recovered pane custody identity mismatch"
            );
            anyhow::ensure!(
                pane.mux_registration_slot().load().is_none(),
                "recovered pane is already registered"
            );
            anyhow::ensure!(
                pane_map.insert(id, Arc::clone(pane)).is_none(),
                "duplicate recovered pane custody"
            );
        }
        let mut tabs = HashMap::new();
        let pane_ids: HashMap<_, _> = pane_map
            .iter()
            .map(|(id, pane)| (Arc::as_ptr(pane).cast::<()>(), *id))
            .collect();
        let mut durable_tabs = std::collections::HashSet::new();
        let mut placed_panes = std::collections::HashSet::new();
        for captured in &captured_tabs {
            anyhow::ensure!(
                durable_tabs.insert(captured.durable_tab_id),
                "duplicate durable recovered tab"
            );
            let tab = Arc::new(Tab::from_recovery_capture(captured, &pane_map)?);
            // The snapshot is callback-free and includes hidden stack members.
            for pane in tab.snapshot_panes_callback_free() {
                let id = *pane_ids
                    .get(&Arc::as_ptr(&pane).cast::<()>())
                    .context("unowned recovered tab pane")?;
                anyhow::ensure!(
                    placed_panes.insert(id),
                    "recovered pane has multiple tab placements"
                );
            }
            anyhow::ensure!(
                tabs.insert(captured.tab_id, tab).is_none(),
                "duplicate recovered tab"
            );
        }
        anyhow::ensure!(
            placed_panes.len() == pane_map.len(),
            "unplaced recovered pane custody"
        );
        let mut windows = Vec::new();
        let mut window_ids = std::collections::HashSet::new();
        let mut durable_windows = std::collections::HashSet::new();
        let mut placed_tabs = std::collections::HashSet::new();
        for captured in &captured_windows {
            anyhow::ensure!(
                window_ids.insert(captured.window_id)
                    && durable_windows.insert(captured.durable_window_id),
                "duplicate recovered window"
            );
            let mut pane_count = 0usize;
            for id in &captured.ordered_tab_ids {
                anyhow::ensure!(
                    placed_tabs.insert(*id),
                    "recovered tab has multiple window placements"
                );
                let metadata = captured_tabs
                    .iter()
                    .find(|tab| tab.tab_id == *id)
                    .context("missing recovered tab metadata")?;
                anyhow::ensure!(
                    metadata.window_id == captured.window_id,
                    "recovered tab parent mismatch"
                );
                pane_count = pane_count
                    .checked_add(
                        tabs.get(id)
                            .context("missing recovered tab")?
                            .snapshot_panes_callback_free()
                            .len(),
                    )
                    .context("recovered pane count overflow")?;
            }
            anyhow::ensure!(
                pane_count == captured.structural_pane_count,
                "recovered structural pane count mismatch"
            );
            windows.push(crate::window::Window::from_recovery_capture(
                captured, &tabs,
            )?);
        }
        anyhow::ensure!(placed_tabs.len() == tabs.len(), "unplaced recovered tab");
        Ok(Self {
            windows,
            tabs,
            panes,
            captured_windows,
            captured_tabs,
            captured_domains,
            default_domain_id,
            default_workspace,
        })
    }

    pub fn counts(&self) -> (usize, usize, usize) {
        (self.windows.len(), self.tabs.len(), self.panes.len())
    }

    pub fn captured_windows(&self) -> &[crate::MuxCapturedWindow] {
        &self.captured_windows
    }

    pub fn captured_tabs(&self) -> &[crate::tab::MuxCapturedTab] {
        &self.captured_tabs
    }

    pub fn captured_domains(&self) -> &[crate::MuxCapturedDomain] {
        &self.captured_domains
    }
    pub fn default_domain_id(&self) -> Option<DomainId> {
        self.default_domain_id
    }
    pub fn default_workspace(&self) -> &str {
        &self.default_workspace
    }
}

/// Read-only evidence that a particular guardian pane completed mux publication.
/// The receipt remains true after the registered pane is removed.
#[derive(Clone)]
pub struct GuardianPanePublicationReceipt {
    published: Arc<AtomicBool>,
}

impl GuardianPanePublicationReceipt {
    #[must_use]
    pub fn was_published(&self) -> bool {
        self.published.load(Ordering::Acquire)
    }
}

enum UnpublishedPaneRollback {
    KillNative,
    RetireGuardian,
}

impl UnpublishedPane {
    pub(crate) fn new(pane: Arc<dyn Pane>) -> Self {
        Self {
            pane: Some(pane),
            rollback: UnpublishedPaneRollback::KillNative,
            guardian_publication: None,
        }
    }

    /// Admit an owned, unregistered guardian proxy without exposing a raw
    /// pane constructor or weakening its lease-only cancellation behavior.
    pub fn from_guardian_proxy(pane: LocalPane) -> anyhow::Result<Self> {
        pane.validate_unpublished_guardian_proxy()?;
        Ok(Self {
            pane: Some(Arc::new(pane)),
            rollback: UnpublishedPaneRollback::RetireGuardian,
            guardian_publication: Some(Arc::new(AtomicBool::new(false))),
        })
    }

    /// Observe publication without acquiring pane ownership or mutation authority.
    #[must_use]
    pub fn guardian_publication_receipt(&self) -> Option<GuardianPanePublicationReceipt> {
        self.guardian_publication
            .as_ref()
            .map(|published| GuardianPanePublicationReceipt {
                published: Arc::clone(published),
            })
    }

    /// Publish into the exact mux, keeping rollback armed until registration
    /// succeeds. The registered pane is returned only after that transition.
    pub fn publish(self, mux: &Arc<Mux>) -> anyhow::Result<Arc<dyn Pane>> {
        mux.add_pane(self.pane())?;
        Ok(self.into_pane())
    }

    pub(crate) fn pane(&self) -> &Arc<dyn Pane> {
        self.pane
            .as_ref()
            .expect("unpublished pane accessed after it was consumed")
    }

    // Call only after the exact registration has committed: publish uses
    // add_pane, and the tab transaction calls this after its commit guard.
    pub(crate) fn into_pane(mut self) -> Arc<dyn Pane> {
        let pane = self
            .pane
            .take()
            .expect("unpublished pane consumed more than once");
        if let Some(published) = self.guardian_publication.as_ref() {
            published.store(true, Ordering::Release);
        }
        pane
    }
}

impl Drop for UnpublishedPane {
    fn drop(&mut self) {
        let Some(pane) = self.pane.take() else {
            return;
        };
        if matches!(self.rollback, UnpublishedPaneRollback::RetireGuardian) {
            // This Arc is created only from an owned LocalPane, and never
            // escapes before publication. Its Drop retires the exact lease.
            drop(pane);
            return;
        }
        let rollback = catch_recoverable(
            RecoverablePanicSite::MuxRegistrationRollback,
            std::panic::AssertUnwindSafe(|| pane.kill()),
        );
        if rollback.is_err() {
            log::error!(
                "unpublished-pane rollback panicked for exact pane identity {:p}",
                Arc::as_ptr(&pane)
            );
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum SplitSource {
    Spawn {
        command: Option<CommandBuilder>,
        command_dir: Option<String>,
    },
    MovePane(PaneId),
}

pub(super) struct PreparedPane {
    pane: Arc<dyn Pane>,
    registration: PaneRegistrationHandle,
    armed: bool,
}

impl PreparedPane {
    pub(super) fn new(pane: Arc<dyn Pane>, registration: PaneRegistrationHandle) -> Self {
        Self {
            pane,
            registration,
            armed: true,
        }
    }

    fn commit_split(
        mut self,
        tab: Arc<Tab>,
        window_id: WindowId,
        size: TerminalSize,
    ) -> SplitCommitReceipt {
        self.armed = false;
        SplitCommitReceipt::from_exact_parts(
            Arc::clone(&self.pane),
            self.registration.clone(),
            tab,
            window_id,
            size,
        )
    }

    fn commit_tab(mut self, tab: Arc<Tab>) -> Arc<Tab> {
        self.armed = false;
        tab
    }

    fn disarm(mut self) {
        self.armed = false;
    }
}

fn prepare_registered_pane(mux: &Arc<Mux>, pane: &Arc<dyn Pane>) -> anyhow::Result<PreparedPane> {
    let registration = match mux.capture_pane_registration(pane) {
        Some(registration) => registration,
        None => {
            let rollback = catch_recoverable(
                RecoverablePanicSite::MuxRegistrationRollback,
                std::panic::AssertUnwindSafe(|| pane.kill()),
            );
            if rollback.is_err() {
                log::error!(
                    "unregistered spawned-pane rollback panicked for exact pane identity {:p}",
                    Arc::as_ptr(pane)
                );
            }
            anyhow::bail!(
                "spawned pane has no exact mux registration; rolled back the unregistered pane"
            );
        }
    };
    Ok(PreparedPane::new(Arc::clone(pane), registration))
}

impl Drop for PreparedPane {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let rollback = catch_recoverable(
            RecoverablePanicSite::MuxRegistrationRollback,
            std::panic::AssertUnwindSafe(|| self.registration.retire_if_current()),
        );
        match rollback {
            Ok(true) => {}
            Ok(false) => {
                log::warn!(
                    "prepared pane {} lost exact registration before rollback",
                    self.registration.pane_id()
                );
            }
            Err(_) => {
                log::error!(
                    "prepared pane {} rollback panicked",
                    self.registration.pane_id()
                );
            }
        }
    }
}

// `async_trait` must synthesize boxed `Future` return values to keep this
// trait object-safe. Those futures are already `#[must_use]`; the macro also
// annotates each generated trait method, which newer Clippy diagnoses as
// `double_must_use` even though there is no source-level attribute to remove.
// Scope the compatibility allowance to this one macro-generated trait surface;
// placing `#[expect]` outside the macro expansion is itself unfulfillable.
#[allow(
    clippy::double_must_use,
    reason = "async_trait duplicates the intrinsic must-use contract of its generated boxed futures"
)]
#[async_trait(?Send)]
pub trait Domain: Downcast + Send + Sync {
    /// Durable construction policy, never a live connection or registration
    /// capability. Unsupported transports must opt in with a real restore
    /// contract before whole-mux recovery can capture them.
    fn recovery_policy(
        &self,
        _config: &config::ConfigHandle,
    ) -> anyhow::Result<DomainRecoveryPolicy> {
        bail!("domain transport has no durable recovery policy")
    }

    /// Spawn a new command within this domain on the exact originating mux.
    async fn spawn(
        &self,
        mux: &Arc<Mux>,
        size: TerminalSize,
        command: Option<CommandBuilder>,
        command_dir: Option<String>,
        window: WindowId,
    ) -> anyhow::Result<Arc<Tab>> {
        let pane = self
            .spawn_pane(mux, size, command, command_dir)
            .await
            .context("spawn")?;
        let prepared = prepare_registered_pane(mux, &pane)?;

        let tab = Arc::new(Tab::new(&size));
        tab.assign_pane(&pane);

        mux.add_tab_and_active_pane(&tab)?;
        if let Err(error) = mux.add_tab_to_window(&tab, window) {
            if mux
                .remove_tab_internal_if_same_with_pane_disposition(&tab, true)
                .is_some()
            {
                prepared.disarm();
            }
            return Err(error);
        }

        Ok(prepared.commit_tab(tab))
    }

    /// Spawn a new pane and commit it beside the exact admitted target.
    async fn split_pane_spawned(
        &self,
        mux: &Arc<Mux>,
        target: &PaneOperationGuard,
        split_request: SplitRequest,
        command: Option<CommandBuilder>,
        command_dir: Option<String>,
    ) -> anyhow::Result<SplitCommitReceipt> {
        anyhow::ensure!(
            target.belongs_to(mux),
            "split target belongs to another mux registration"
        );
        let (_domain_id, window_id, tab) = target.exact_location()?;
        let pane_index = match tab
            .iter_panes_ignoring_zoom()
            .iter()
            .find(|positioned| Arc::ptr_eq(&positioned.pane, target.pane()))
        {
            Some(p) => p.index,
            None => anyhow::bail!(
                "exact split target registration {} is not tiled",
                target.pane_id()
            ),
        };

        let split_size = match tab.compute_split_size(pane_index, split_request) {
            Some(s) => s,
            None => anyhow::bail!("invalid pane index {}", pane_index),
        };

        let target_config = target.with_pane(|pane| pane.get_config());
        let pane = self
            .spawn_pane(mux, split_size.second, command, command_dir)
            .await?;
        let prepared = prepare_registered_pane(mux, &pane)?;
        if let Some(config) = target_config {
            pane.set_config(config);
        }
        let dims = pane.get_dimensions();
        let size = TerminalSize {
            cols: dims.cols,
            rows: dims.viewport_rows,
            pixel_height: dims.pixel_height,
            pixel_width: dims.pixel_width,
            dpi: dims.dpi,
        };
        tab.split_and_insert(pane_index, split_request, pane)?;
        Ok(prepared.commit_split(tab, window_id, size))
    }

    /// Move another exact registration into a split beside the target.
    async fn split_pane_moved(
        &self,
        mux: &Arc<Mux>,
        target: &PaneOperationGuard,
        source: &PaneOperationGuard,
        split_request: SplitRequest,
    ) -> anyhow::Result<SplitCommitReceipt> {
        anyhow::ensure!(
            target.belongs_to(mux) && source.belongs_to(mux),
            "split source and target must belong to the originating mux"
        );
        anyhow::ensure!(
            !target.same_registration(source),
            "cannot move pane {} into a split of itself",
            target.pane_id()
        );
        mux.commit_guarded_moved_split(target, source, split_request)
    }

    /// Spawn and register a pane on the exact originating mux.
    async fn spawn_pane(
        &self,
        mux: &Arc<Mux>,
        size: TerminalSize,
        command: Option<CommandBuilder>,
        command_dir: Option<String>,
    ) -> anyhow::Result<Arc<dyn Pane>>;

    /// Construct a pane process/PTY without publishing it in a mux.
    ///
    /// Only domains that can uphold the unpublished reservation contract may
    /// override this method. Until mux publication consumes the guard,
    /// cancellation or failure kills a native child but only retires a
    /// guardian-owned pane's lease, preserving its child for reconciliation.
    async fn spawn_unpublished_pane(
        &self,
        _mux: &Arc<Mux>,
        _size: TerminalSize,
        _command: Option<CommandBuilder>,
        _command_dir: Option<String>,
    ) -> anyhow::Result<UnpublishedPane> {
        bail!(
            "domain `{}` does not support unpublished pane construction",
            self.domain_name()
        )
    }

    /// The mux will call this method on the domain of the pane that
    /// is being moved to give the domain a chance to handle the movement.
    /// `mux` is the exact originating mux that admitted that movement.
    /// If this method returns Ok(None), then the mux will handle the
    /// movement itself by mutating its local Tabs and Windows.
    async fn move_pane_to_new_tab(
        &self,
        _mux: &Arc<Mux>,
        _pane: &PaneOperationGuard,
        _window_id: Option<WindowId>,
        _workspace_for_new_window: Option<String>,
    ) -> anyhow::Result<Option<MoveCommitReceipt>> {
        Ok(None)
    }

    /// Returns false if the `spawn` method will never succeed.
    /// There are some internal placeholder domains that are
    /// pre-created with local UI that we do not want to allow
    /// to show in the launcher/menu as launchable items.
    fn spawnable(&self) -> bool {
        true
    }

    /// Whether this domain can authoritatively spawn directly into a local
    /// floating layer without first publishing a normal tab.
    ///
    /// Client-backed and asynchronously materialized remote domains must leave
    /// this false until their protocol exposes an atomic remote operation. The
    /// mux checks this synchronously before attach or spawn.
    fn supports_floating_pane_spawn(&self) -> bool {
        false
    }

    /// Returns true if the `detach` method can be used
    /// to detach the domain, preserving the associated
    /// panes, or false if the `detach` method will never
    /// succeed
    fn detachable(&self) -> bool;

    /// Returns the domain id, which is useful for obtaining
    /// a handle on the domain later.
    fn domain_id(&self) -> DomainId;

    /// Returns the name of the domain.
    /// Should be a short identifier.
    fn domain_name(&self) -> &str;

    /// Returns a label describing the domain.
    async fn domain_label(&self) -> String {
        self.domain_name().to_string()
    }

    /// Re-attach to any tabs that might be pre-existing in this domain
    /// Attach to this domain using mux and client authority captured before
    /// any asynchronous connection work begins.
    async fn attach(
        &self,
        mux: &Arc<Mux>,
        owner_client_id: Option<Arc<ClientId>>,
        window_id: Option<WindowId>,
    ) -> anyhow::Result<()>;

    /// Detach all tabs
    fn detach(&self) -> anyhow::Result<()>;

    /// Indicates the state of the domain
    fn state(&self) -> DomainState;
}
impl_downcast!(Domain);

#[derive(Clone, PartialEq, Eq)]
pub struct LocalDomainRecoveryPolicy {
    pub default_prog: Option<Vec<String>>,
    pub default_cwd: Option<PathBuf>,
    pub environment: std::collections::BTreeMap<String, String>,
    pub term: String,
}

impl std::fmt::Debug for LocalDomainRecoveryPolicy {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("LocalDomainRecoveryPolicy")
            .field(
                "program_arguments",
                &self.default_prog.as_ref().map(Vec::len),
            )
            .field("has_working_directory", &self.default_cwd.is_some())
            .field("environment_entries", &self.environment.len())
            .finish_non_exhaustive()
    }
}

impl LocalDomainRecoveryPolicy {
    pub fn payload_bytes(&self) -> Option<usize> {
        self.default_prog
            .iter()
            .flatten()
            .map(String::len)
            .chain(
                self.environment
                    .iter()
                    .flat_map(|(key, value)| [key.len(), value.len()]),
            )
            .chain(self.default_cwd.iter().map(|path| path.as_os_str().len()))
            .chain(std::iter::once(self.term.len()))
            .try_fold(0usize, usize::checked_add)
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.default_prog
                .as_ref()
                .is_none_or(|args| !args.is_empty() && args.len() <= 4096)
                && self.environment.len() <= 4096,
            "invalid recovered domain spawn policy entry count"
        );
        let cwd = self
            .default_cwd
            .as_ref()
            .map(|path| {
                path.to_str().ok_or_else(|| {
                    anyhow::anyhow!("recovered domain working directory is not UTF-8")
                })
            })
            .transpose()?;
        let values = self
            .default_prog
            .iter()
            .flatten()
            .map(String::as_str)
            .chain(
                self.environment
                    .iter()
                    .flat_map(|(k, v)| [k.as_str(), v.as_str()]),
            )
            .chain(cwd)
            .chain(std::iter::once(self.term.as_str()));
        let mut bytes = 0usize;
        for value in values {
            anyhow::ensure!(
                value.len() <= 65536 && !value.contains('\0'),
                "invalid recovered domain policy field"
            );
            bytes = bytes
                .checked_add(value.len())
                .ok_or_else(|| anyhow::anyhow!("recovered domain policy size overflow"))?;
        }
        anyhow::ensure!(
            bytes <= 2 * 1024 * 1024,
            "recovered domain policy exceeds byte budget"
        );
        anyhow::ensure!(
            self.environment
                .keys()
                .all(|key| !key.is_empty() && !key.contains('=')),
            "invalid recovered environment key"
        );
        Ok(())
    }

    fn command_config(&self) -> config::Config {
        config::Config {
            default_prog: self.default_prog.clone(),
            default_cwd: self.default_cwd.clone(),
            set_environment_variables: self
                .environment
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
            term: self.term.clone(),
            ..config::Config::default()
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DomainRecoveryPolicy {
    Local(LocalDomainRecoveryPolicy),
    GuardianLocal {
        commands: LocalDomainRecoveryPolicy,
        socket_path: PathBuf,
        token_path: PathBuf,
    },
}

impl DomainRecoveryPolicy {
    pub fn validate(&self) -> anyhow::Result<()> {
        match self {
            Self::Local(policy) => policy.validate(),
            Self::GuardianLocal {
                commands,
                socket_path,
                token_path,
            } => {
                commands.validate()?;
                anyhow::ensure!(
                    [socket_path, token_path].iter().all(|path| {
                        path.is_absolute()
                            && path
                                .to_str()
                                .is_some_and(|value| value.len() <= 65536 && !value.contains('\0'))
                    }),
                    "invalid recovered guardian endpoint path"
                );
                Ok(())
            }
        }
    }

    pub fn payload_bytes(&self) -> Option<usize> {
        match self {
            Self::Local(policy) => policy.payload_bytes(),
            Self::GuardianLocal {
                commands,
                socket_path,
                token_path,
            } => commands
                .payload_bytes()?
                .checked_add(socket_path.as_os_str().len())?
                .checked_add(token_path.as_os_str().len()),
        }
    }
}

pub struct LocalDomain {
    pty_system: Mutex<Box<dyn PtySystem + Send>>,
    id: DomainId,
    name: String,
    configuration: LocalDomainConfiguration,
    recovered_policy: Option<LocalDomainRecoveryPolicy>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum LocalDomainConfiguration {
    Runtime,
    Wsl(WslDomain),
    Exec(ExecDomain),
    Serial(SerialDomain),
}

impl LocalDomain {
    /// Private construction only: the caller must reserve durable IDs and
    /// publish the complete recovered domain cohort atomically.
    pub fn from_recovery_policy(
        id: DomainId,
        name: String,
        policy: LocalDomainRecoveryPolicy,
    ) -> anyhow::Result<Self> {
        anyhow::ensure!(
            cfg!(unix)
                && id != usize::MAX
                && !name.is_empty()
                && name.len() <= 65536
                && !name.contains('\0'),
            "invalid recovered local domain identity"
        );
        policy.validate()?;
        Ok(Self {
            pty_system: Mutex::new(native_pty_system()),
            id,
            name,
            configuration: LocalDomainConfiguration::Runtime,
            recovered_policy: Some(policy),
        })
    }

    pub fn capture_recovery_policy(
        &self,
        captured_config: &config::ConfigHandle,
    ) -> anyhow::Result<LocalDomainRecoveryPolicy> {
        #[cfg(unix)]
        let native_pty = {
            let backend = self.pty_system.try_lock().ok_or(DomainRecoveryPolicyBusy)?;
            let backend: &dyn PtySystem = &**backend;
            backend
                .downcast_ref::<portable_pty::unix::UnixPtySystem>()
                .is_some()
        };
        #[cfg(not(unix))]
        let native_pty = false;
        anyhow::ensure!(
            native_pty,
            "domain PTY backend has no supported recovery policy"
        );
        if let Some(policy) = &self.recovered_policy {
            return Ok(policy.clone());
        }
        anyhow::ensure!(
            cfg!(unix)
                && matches!(self.configuration, LocalDomainConfiguration::Runtime)
                && !captured_config
                    .exec_domains
                    .iter()
                    .any(|d| d.name == self.name)
                && !captured_config
                    .wsl_domains
                    .as_ref()
                    .is_some_and(|domains| domains.iter().any(|d| d.name == self.name)),
            "domain command callbacks or transport overrides cannot be restored"
        );
        let program = captured_config.default_prog.as_ref();
        anyhow::ensure!(
            program.is_none_or(|args| !args.is_empty() && args.len() <= 4096)
                && captured_config.set_environment_variables.len() <= 4096,
            "domain spawn policy exceeds bounded entry limits"
        );
        let cwd = captured_config
            .default_cwd
            .as_ref()
            .map(|path| {
                path.to_str().ok_or_else(|| {
                    anyhow::anyhow!("domain recovery requires a UTF-8 working directory")
                })
            })
            .transpose()?;
        let strings = program
            .into_iter()
            .flatten()
            .map(String::as_str)
            .chain(
                captured_config
                    .set_environment_variables
                    .iter()
                    .flat_map(|(k, v)| [k.as_str(), v.as_str()]),
            )
            .chain(cwd)
            .chain(std::iter::once(captured_config.term.as_str()));
        let mut bytes = 0usize;
        for value in strings {
            anyhow::ensure!(
                value.len() <= 65536,
                "domain policy field exceeds byte limit"
            );
            bytes = bytes
                .checked_add(value.len())
                .ok_or_else(|| anyhow::anyhow!("domain policy byte count overflow"))?;
        }
        anyhow::ensure!(
            bytes <= 2 * 1024 * 1024,
            "domain spawn policy exceeds byte budget"
        );
        let policy = LocalDomainRecoveryPolicy {
            default_prog: captured_config.default_prog.clone(),
            default_cwd: captured_config.default_cwd.clone(),
            environment: captured_config
                .set_environment_variables
                .iter()
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect(),
            term: captured_config.term.clone(),
        };
        policy.validate()?;
        Ok(policy)
    }

    pub fn new(name: &str) -> Result<Self, Error> {
        Ok(Self::with_pty_system(name, native_pty_system()))
    }

    fn resolve_exec_domain(&self) -> Option<ExecDomain> {
        if self.recovered_policy.is_some() {
            return None;
        }
        match &self.configuration {
            LocalDomainConfiguration::Exec(exec) => Some(exec.clone()),
            LocalDomainConfiguration::Runtime => config::configuration()
                .exec_domains
                .iter()
                .find(|exec| exec.name == self.name)
                .cloned(),
            LocalDomainConfiguration::Wsl(_) | LocalDomainConfiguration::Serial(_) => None,
        }
    }

    fn resolve_wsl_domain(&self) -> Option<WslDomain> {
        if self.recovered_policy.is_some() {
            return None;
        }
        match &self.configuration {
            LocalDomainConfiguration::Wsl(wsl) => Some(wsl.clone()),
            LocalDomainConfiguration::Runtime => config::configuration()
                .wsl_domains()
                .iter()
                .find(|wsl| wsl.name == self.name)
                .cloned(),
            LocalDomainConfiguration::Exec(_) | LocalDomainConfiguration::Serial(_) => None,
        }
    }

    pub fn with_pty_system(name: &str, pty_system: Box<dyn PtySystem + Send>) -> Self {
        Self::with_pty_system_and_configuration(
            name.to_string(),
            pty_system,
            LocalDomainConfiguration::Runtime,
        )
    }

    fn with_pty_system_and_configuration(
        name: String,
        pty_system: Box<dyn PtySystem + Send>,
        configuration: LocalDomainConfiguration,
    ) -> Self {
        let id = alloc_domain_id();
        Self {
            pty_system: Mutex::new(pty_system),
            id,
            name,
            configuration,
            recovered_policy: None,
        }
    }

    pub fn new_wsl(wsl: WslDomain) -> Result<Self, Error> {
        Ok(Self::with_pty_system_and_configuration(
            wsl.name.clone(),
            native_pty_system(),
            LocalDomainConfiguration::Wsl(wsl),
        ))
    }

    pub fn new_exec_domain(exec_domain: ExecDomain) -> anyhow::Result<Self> {
        Ok(Self::with_pty_system_and_configuration(
            exec_domain.name.clone(),
            native_pty_system(),
            LocalDomainConfiguration::Exec(exec_domain),
        ))
    }

    pub fn new_serial_domain(serial_domain: SerialDomain) -> anyhow::Result<Self> {
        Self::new_serial_domain_with_configuration(serial_domain, LocalDomainConfiguration::Runtime)
    }

    pub fn new_configured_serial_domain(serial_domain: SerialDomain) -> anyhow::Result<Self> {
        let configuration = LocalDomainConfiguration::Serial(serial_domain.clone());
        Self::new_serial_domain_with_configuration(serial_domain, configuration)
    }

    fn new_serial_domain_with_configuration(
        serial_domain: SerialDomain,
        configuration: LocalDomainConfiguration,
    ) -> anyhow::Result<Self> {
        let port = serial_domain.port.as_ref().unwrap_or(&serial_domain.name);
        let mut serial = portable_pty::serial::SerialTty::new(&port);
        if let Some(baud) = serial_domain.baud {
            serial.set_baud_rate(baud);
        }
        let pty_system = Box::new(serial);
        Ok(Self::with_pty_system_and_configuration(
            serial_domain.name.clone(),
            pty_system,
            configuration,
        ))
    }

    /// Whether this exact domain generation was constructed from a reloadable
    /// configuration entry rather than created by the mux at runtime.
    ///
    /// The distinction is intentionally retained on the domain itself so a
    /// configuration sweep cannot retire the built-in local domain or another
    /// runtime-created `LocalDomain` merely because its concrete Rust type and
    /// name happen to match a configured entry.
    pub fn is_configuration_owned(&self) -> bool {
        !matches!(&self.configuration, LocalDomainConfiguration::Runtime)
    }

    pub fn matches_wsl_configuration(&self, expected: &WslDomain) -> bool {
        matches!(
            &self.configuration,
            LocalDomainConfiguration::Wsl(current) if current == expected
        )
    }

    pub fn matches_exec_configuration(&self, expected: &ExecDomain) -> bool {
        matches!(
            &self.configuration,
            LocalDomainConfiguration::Exec(current) if current == expected
        )
    }

    pub fn matches_serial_configuration(&self, expected: &SerialDomain) -> bool {
        matches!(
            &self.configuration,
            LocalDomainConfiguration::Serial(current) if current == expected
        )
    }

    fn wslenv_entry_name(entry: &str) -> &str {
        entry.split_once('/').map(|(name, _)| name).unwrap_or(entry)
    }

    fn augment_wslenv_for_wsl_command(cmd: &mut CommandBuilder) {
        let mut wslenv_entries: Vec<String> = cmd
            .get_env("WSLENV")
            .map(|value| value.to_string_lossy().to_string())
            .map(|value| {
                value
                    .split(':')
                    .filter(|entry| !entry.is_empty())
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();

        for (key, _) in cmd.iter_extra_env_as_str() {
            if key == "WSLENV" {
                continue;
            }
            if wslenv_entries
                .iter()
                .any(|entry| Self::wslenv_entry_name(entry) == key)
            {
                continue;
            }
            wslenv_entries.push(key.to_string());
        }

        if !wslenv_entries.is_empty() {
            cmd.env("WSLENV", wslenv_entries.join(":"));
        }
    }

    fn rewrite_command_for_wsl(cmd: &mut CommandBuilder, wsl: &WslDomain) -> anyhow::Result<()> {
        let mut args: Vec<OsString> = cmd.get_argv().clone();

        if args.is_empty() {
            if let Some(def_prog) = &wsl.default_prog {
                for arg in def_prog {
                    args.push(arg.into());
                }
            }
        }

        let mut argv: Vec<OsString> = vec![
            "wsl.exe".into(),
            "--distribution".into(),
            wsl.distribution
                .as_deref()
                .unwrap_or(wsl.name.as_str())
                .into(),
        ];

        if let Some(cwd) = cmd.get_cwd() {
            argv.push("--cd".into());
            argv.push(cwd.into());
        }

        if let Some(user) = &wsl.username {
            argv.push("--user".into());
            argv.push(user.into());
        }

        if !args.is_empty() {
            argv.push("--exec".into());
            for arg in args {
                argv.push(arg);
            }
        }

        // WSL only imports Windows-side environment variables that are
        // listed in WSLENV, so copy explicit command env keys into the
        // existing WSLENV contract before we swap argv to `wsl.exe --exec`.
        Self::augment_wslenv_for_wsl_command(cmd);

        cmd.clear_cwd();
        *cmd.get_argv_mut() = argv;
        Ok(())
    }

    #[cfg(unix)]
    fn is_conpty(&self) -> bool {
        false
    }

    #[cfg(windows)]
    fn is_conpty(&self) -> bool {
        let pty_system = self.pty_system.lock();
        let pty_system: &dyn PtySystem = &**pty_system;
        pty_system
            .downcast_ref::<portable_pty::win::conpty::ConPtySystem>()
            .is_some()
    }

    async fn fixup_command(&self, cmd: &mut CommandBuilder) -> anyhow::Result<()> {
        if let Some(wsl) = self.resolve_wsl_domain() {
            Self::rewrite_command_for_wsl(cmd, &wsl)?;
        } else if let Some(ed) = self.resolve_exec_domain() {
            let mut args = vec![];
            let mut set_environment_variables = HashMap::new();
            for arg in cmd.get_argv() {
                args.push(
                    arg.to_str()
                        .ok_or_else(|| anyhow::anyhow!("command argument is not utf8"))?
                        .to_string(),
                );
            }
            for (k, v) in cmd.iter_full_env_as_str() {
                set_environment_variables.insert(k.to_string(), v.to_string());
            }
            let cwd = match cmd.get_cwd() {
                Some(cwd) => Some(PathBuf::from(cwd)),
                None => None,
            };
            let spawn_command = SpawnCommand {
                label: None,
                domain: SpawnTabDomain::DomainName(ed.name.clone()),
                args: if args.is_empty() { None } else { Some(args) },
                set_environment_variables,
                cwd,
                position: None,
            };

            #[cfg(feature = "lua")]
            let spawn_command = config::with_lua_config_on_main_thread(|lua| async {
                let lua = lua.ok_or_else(|| anyhow::anyhow!("missing lua context"))?;
                let value = config::lua::emit_async_callback(
                    &*lua,
                    (ed.fixup_command.clone(), (spawn_command.clone())),
                )
                .await?;
                let cmd: SpawnCommand =
                    luahelper::from_lua_value_dynamic(value).with_context(|| {
                        format!(
                            "interpreting SpawnCommand result from ExecDomain {}",
                            ed.name
                        )
                    })?;
                Ok(cmd)
            })
            .await
            .with_context(|| format!("calling ExecDomain {} function", ed.name))?;
            #[cfg(not(feature = "lua"))]
            let spawn_command = spawn_command;

            // Reinterpret the SpawnCommand into the builder

            cmd.get_argv_mut().clear();
            if let Some(args) = &spawn_command.args {
                for arg in args {
                    cmd.get_argv_mut().push(arg.into());
                }
            }
            cmd.env_clear();
            for (k, v) in &spawn_command.set_environment_variables {
                cmd.env(k, v);
            }
            cmd.clear_cwd();
            if let Some(cwd) = &spawn_command.cwd {
                cmd.cwd(cwd);
            }
        } else if Path::new("/.flatpak-info").exists() {
            // We're running inside a flatpak sandbox.
            // Run the command outside the sandbox via flatpak-spawn
            let mut args = vec![
                "flatpak-spawn".to_string(),
                "--host".to_string(),
                "--watch-bus".to_string(),
            ];
            if let Some(cwd) = cmd.get_cwd() {
                args.push(format!("--directory={}", Path::new(cwd).display()));
            }

            let is_default_prog = cmd.is_default_prog();

            // Note: WEZTERM_UNIX_SOCKET, WEZTERM_CONFIG_(FILE|DIR) and other env
            // vars are not included in this.
            // We can't include them: their paths are only meaningful in the sandbox
            // and cannot be reasonably accessed from outside it in the shell.
            for (k, v) in cmd.iter_extra_env_as_str() {
                args.push(format!("--env={k}={v}"));
            }

            for arg in cmd.get_argv() {
                args.push(
                    arg.to_str()
                        .ok_or_else(|| anyhow::anyhow!("command argument is not utf8"))?
                        .to_string(),
                );
            }

            if is_default_prog {
                // We can't read $SHELL from inside the sandbox, so ask the host.
                //
                // Guard both failure modes the old code swallowed silently:
                //   * non-zero exit from flatpak-spawn (missing host socket,
                //     --host denied, etc.) — the earlier code dropped stderr
                //     and still pushed stdout (often empty) as the shell
                //     argument, producing a malformed `flatpak-spawn` argv
                //     whose downstream spawn_command failure carried no
                //     context pointing at the real cause;
                //   * empty `$SHELL` — `echo $SHELL` can legitimately
                //     return just `\n` if the host shell has no SHELL set,
                //     in which case we'd push `""` as a program name and
                //     exec() would fail with an opaque ENOENT.
                let output = std::process::Command::new("flatpak-spawn")
                    .args(["--host", "sh", "-c", "echo $SHELL"])
                    .output()
                    .context("invoking flatpak-spawn --host sh -c 'echo $SHELL'")?;
                if !output.status.success() {
                    anyhow::bail!(
                        "flatpak-spawn --host sh -c 'echo $SHELL' failed (status={:?}): {}",
                        output.status.code(),
                        String::from_utf8_lossy(&output.stderr).trim(),
                    );
                }
                let shell = String::from_utf8_lossy(&output.stdout);
                let shell = shell.trim();
                if shell.is_empty() {
                    anyhow::bail!(
                        "flatpak-spawn --host reported an empty $SHELL; \
                         set SHELL on the host or spawn an explicit program"
                    );
                }

                args.push(shell.to_string());
                // Assume we can pass `-l` for a login shell
                args.push("-l".to_string());
            }

            // Avoid setting up the controlling tty as that is not compatible
            // with flatpak:
            // <https://github.com/flatpak/flatpak/issues/3697>
            // <https://github.com/flatpak/flatpak/issues/3285>
            cmd.set_controlling_tty(false);

            // Re-apply to the builder
            cmd.get_argv_mut().clear();
            for arg in args {
                cmd.get_argv_mut().push(arg.into());
            }
            cmd.clear_cwd();
            log::trace!("made: {cmd:#?}");
        } else if let Some(dir) = cmd.get_cwd() {
            // I'm not normally a fan of existence checking, but not checking here
            // can be painful; in the case where a tab is local but has connected
            // to a remote system and that remote has used OSC 7 to set a path
            // that doesn't exist on the local system, process spawning can fail.
            // Another situation is `sudo -i` has the pane with set to a cwd
            // that is not accessible to the user.
            if let Err(err) = Path::new(&dir).read_dir() {
                log::warn!(
                    "Directory {:?} is not readable and will not be \
                     used for the command we are spawning: {:#}",
                    dir,
                    err
                );
                cmd.clear_cwd();
            }
        }
        Ok(())
    }

    /// Prepare the configured command without creating a PTY or process.
    pub async fn build_command(
        &self,
        mux: &Arc<Mux>,
        command: Option<CommandBuilder>,
        command_dir: Option<String>,
        pane_id: PaneId,
    ) -> anyhow::Result<CommandBuilder> {
        let live_config = configuration();
        let recovered_config = self
            .recovered_policy
            .as_ref()
            .map(LocalDomainRecoveryPolicy::command_config);
        let config: &config::Config = recovered_config.as_ref().unwrap_or(&live_config);

        let wsl = self.resolve_wsl_domain();
        let default_prog = wsl
            .as_ref()
            .map(|wsl| wsl.default_prog.as_ref())
            .unwrap_or(config.default_prog.as_ref());

        let mut cmd = match command {
            Some(mut cmd) => {
                config.apply_cmd_defaults(&mut cmd, default_prog, config.default_cwd.as_ref());
                cmd
            }
            None => config.build_prog(
                None,
                default_prog,
                wsl.as_ref()
                    .map(|wsl| wsl.default_cwd.as_ref())
                    .unwrap_or(config.default_cwd.as_ref()),
            )?,
        };
        if let Some(dir) = command_dir {
            cmd.cwd(dir);
        }
        if let Ok(sock) = std::env::var("WEZTERM_UNIX_SOCKET") {
            cmd.env("WEZTERM_UNIX_SOCKET", sock);
        }
        cmd.env("WEZTERM_PANE", pane_id.to_string());
        if let Some(agent_path) = mux.agent.as_ref().map(|agent| agent.path().to_path_buf()) {
            cmd.env("SSH_AUTH_SOCK", agent_path);
        }
        self.fixup_command(&mut cmd).await?;
        Ok(cmd)
    }
}

/// Allows sharing the writer between the Pane and the Terminal.
/// This could potentially be eliminated in the future if we can
/// teach the Pane impl to reference the writer in the Termninal,
/// but the Pane trait returns a RefMut and that makes it a bit
/// awkward at the moment.
#[derive(Clone)]
pub(crate) struct WriterWrapper {
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
}

impl WriterWrapper {
    pub fn new(writer: Box<dyn Write + Send>) -> Self {
        Self {
            writer: Arc::new(Mutex::new(writer)),
        }
    }
}

impl std::io::Write for WriterWrapper {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.writer.lock().write(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.writer.lock().flush()
    }
}

/// Wraps the underlying pty; we use this as a marker for when
/// the spawn attempt failed in order to hold the pane open
pub(crate) struct FailedSpawnPty {
    inner: Mutex<Box<dyn MasterPty>>,
}

impl portable_pty::MasterPty for FailedSpawnPty {
    fn resize(&self, new_size: PtySize) -> anyhow::Result<()> {
        self.inner.lock().resize(new_size)
    }
    fn get_size(&self) -> anyhow::Result<PtySize> {
        self.inner.lock().get_size()
    }
    fn try_clone_reader(&self) -> anyhow::Result<Box<dyn std::io::Read + Send + 'static>> {
        self.inner.lock().try_clone_reader()
    }
    fn take_writer(&self) -> anyhow::Result<Box<dyn std::io::Write + Send + 'static>> {
        self.inner.lock().take_writer()
    }

    #[cfg(unix)]
    fn process_group_leader(&self) -> Option<i32> {
        None
    }

    #[cfg(unix)]
    fn as_raw_fd(&self) -> Option<std::os::fd::RawFd> {
        None
    }

    #[cfg(unix)]
    fn tty_name(&self) -> Option<std::path::PathBuf> {
        None
    }
}

/// A fake child process for the case where the spawn attempt
/// failed. It reports as immediately terminated.
#[derive(Debug)]
pub(crate) struct FailedProcessSpawn {}

impl portable_pty::Child for FailedProcessSpawn {
    fn try_wait(&mut self) -> std::io::Result<Option<ExitStatus>> {
        Ok(Some(ExitStatus::with_exit_code(1)))
    }

    fn wait(&mut self) -> std::io::Result<ExitStatus> {
        Ok(ExitStatus::with_exit_code(1))
    }

    fn process_id(&self) -> Option<u32> {
        None
    }

    #[cfg(windows)]
    fn as_raw_handle(&self) -> Option<std::os::windows::io::RawHandle> {
        None
    }
}

impl portable_pty::ChildKiller for FailedProcessSpawn {
    fn kill(&mut self) -> std::io::Result<()> {
        Ok(())
    }
    fn clone_killer(&self) -> Box<dyn portable_pty::ChildKiller + Send + Sync> {
        Box::new(FailedProcessSpawn {})
    }
}

/// Owns the result of the blocking process-spawn worker until the awaiting
/// future has actually claimed it.
///
/// Dropping the receiver side of `spawn_into_new_thread` does not stop its OS
/// thread. Without this guard, a successful child produced after async
/// cancellation would be dropped alive when the worker discovered that its
/// result receiver was gone. Keeping the result armed in the channel makes
/// cancellation kill that otherwise-orphaned child exactly once.
struct KillOnDropChildResult {
    result: Option<anyhow::Result<Box<dyn portable_pty::Child + Send + Sync>>>,
}

impl KillOnDropChildResult {
    fn new(result: anyhow::Result<Box<dyn portable_pty::Child + Send + Sync>>) -> Self {
        Self {
            result: Some(result),
        }
    }

    fn into_result(mut self) -> anyhow::Result<Box<dyn portable_pty::Child + Send + Sync>> {
        self.result
            .take()
            .expect("child spawn result consumed more than once")
    }
}

impl Drop for KillOnDropChildResult {
    fn drop(&mut self) {
        let Some(Ok(mut child)) = self.result.take() else {
            return;
        };
        let rollback = catch_recoverable(
            RecoverablePanicSite::MuxRegistrationRollback,
            std::panic::AssertUnwindSafe(|| child.kill()),
        );
        match rollback {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                log::warn!(
                    "cancelled local pane spawn could not kill its unclaimed child: {error}"
                );
            }
            Err(_) => {
                log::error!(
                    "cancelled local pane spawn panicked while killing its unclaimed child"
                );
            }
        }
    }
}

#[async_trait(?Send)]
impl Domain for LocalDomain {
    fn recovery_policy(
        &self,
        captured_config: &config::ConfigHandle,
    ) -> anyhow::Result<DomainRecoveryPolicy> {
        self.capture_recovery_policy(captured_config)
            .map(DomainRecoveryPolicy::Local)
    }

    async fn spawn_pane(
        &self,
        mux: &Arc<Mux>,
        size: TerminalSize,
        command: Option<CommandBuilder>,
        command_dir: Option<String>,
    ) -> anyhow::Result<Arc<dyn Pane>> {
        let unpublished = self
            .spawn_unpublished_pane(mux, size, command, command_dir)
            .await?;
        unpublished.publish(mux)
    }

    async fn spawn_unpublished_pane(
        &self,
        mux: &Arc<Mux>,
        size: TerminalSize,
        command: Option<CommandBuilder>,
        command_dir: Option<String>,
    ) -> anyhow::Result<UnpublishedPane> {
        let pane_id = alloc_pane_id()?;
        let cmd = self
            .build_command(mux, command, command_dir, pane_id)
            .await
            .context("build_command")?;
        let pair = self
            .pty_system
            .lock()
            .openpty(crate::terminal_size_to_pty_size(size)?)?;

        let command_line = cmd
            .as_unix_command_line()
            .unwrap_or_else(|err| format!("error rendering command line: {:?}", err));
        let command_description = format!(
            "\"{}\" in domain \"{}\"",
            if command_line.is_empty() {
                cmd.get_shell()
            } else {
                command_line
            },
            self.name
        );
        // [ft-s9oci] `SlavePty::spawn_command` wraps `fork + exec` on Unix
        // (and `CreateProcess` on Windows). Both are fully synchronous
        // syscall sequences that can stall for a long time — a slow
        // program load, a hung NSS lookup reaching /etc/nsswitch.conf,
        // an LD_PRELOAD hook that deadlocks in its own init. Calling it
        // directly from this async fn parks the executor thread until
        // the child is either up or the call fails; every other async
        // task scheduled on that thread (IPC handlers, tmux event
        // processing, UI repaint) stops making progress too.
        //
        // Hand the fork off to a dedicated OS thread via
        // `promise::spawn::spawn_into_new_thread`. The slave pty and
        // the CommandBuilder are both `Send`, and the slave is not
        // referenced anywhere else on this code path after spawn —
        // dropping it inside the worker thread once the spawn settles
        // is correct. The worker returns a kill-on-drop result reservation.
        // If this async future is cancelled while the syscall sequence is
        // still blocked, the channel eventually drops that reservation and
        // kills any child which materialized after cancellation.
        let PtyPair { slave, master } = pair;
        let guarded_child_result = promise::spawn::spawn_into_new_thread(move || {
            Ok(KillOnDropChildResult::new(slave.spawn_command(cmd)))
        })
        .await;
        let mut writer = WriterWrapper::new(master.take_writer()?);

        let durable_pane_id = *uuid::Uuid::new_v4().as_bytes();
        let term_config = config::TermConfig::new_for_pane(
            pane_id,
            self.id,
            durable_pane_id,
            command_description.clone(),
        );
        let mut terminal = frankenterm_term::Terminal::new(
            size,
            std::sync::Arc::new(term_config),
            "WezTerm",
            config::wezterm_version(),
            Box::new(writer.clone()),
        );
        if self.is_conpty() {
            terminal.enable_conpty_quirks();
        }

        // Keep the child result reservation armed through every fallible setup
        // step above. Only the immediate LocalPane construction below takes
        // ownership away from its kill-on-drop guard.
        let child_result = match guarded_child_result {
            Ok(result) => result.into_result(),
            Err(error) => Err(error),
        };

        let pane: Arc<dyn Pane> = match child_result {
            Ok(child) => Arc::new(LocalPane::new(
                pane_id,
                terminal,
                child,
                master,
                Box::new(writer),
                self.id,
                durable_pane_id,
                command_description,
            )),
            Err(err) => {
                // Show the error to the user in the new pane
                let display_result = write!(writer, "{err:#}").and_then(|_| writer.flush());
                if display_result.is_err() {
                    tracing::warn!(
                        spawn_error = ?err,
                        pane_error_display = ?display_result,
                        "failed to surface local pane spawn error"
                    );
                }

                // and return a dummy pane that has exited
                Arc::new(LocalPane::new(
                    pane_id,
                    terminal,
                    Box::new(FailedProcessSpawn {}),
                    Box::new(FailedSpawnPty {
                        inner: Mutex::new(master),
                    }),
                    Box::new(writer),
                    self.id,
                    durable_pane_id,
                    command_description,
                ))
            }
        };

        Ok(UnpublishedPane::new(pane))
    }

    fn supports_floating_pane_spawn(&self) -> bool {
        true
    }

    fn domain_id(&self) -> DomainId {
        self.id
    }

    fn domain_name(&self) -> &str {
        &self.name
    }

    async fn domain_label(&self) -> String {
        if let Some(ed) = self.resolve_exec_domain() {
            match &ed.label {
                Some(ValueOrFunc::Value(frankenterm_dynamic::Value::String(s))) => s.to_string(),
                Some(ValueOrFunc::Func(label_func)) => {
                    #[cfg(feature = "lua")]
                    {
                        let label = config::with_lua_config_on_main_thread(|lua| async {
                            let lua = lua.ok_or_else(|| anyhow::anyhow!("missing lua context"))?;
                            let value = config::lua::emit_async_callback(
                                &*lua,
                                (label_func.clone(), (self.name.clone())),
                            )
                            .await?;
                            let label: String = luahelper::from_lua_value_dynamic(value)
                                .with_context(|| {
                                    format!(
                                        "interpreting SpawnCommand result from ExecDomain {}",
                                        ed.name
                                    )
                                })?;
                            Ok(label)
                        })
                        .await;
                        match label {
                            Ok(label) => label,
                            Err(err) => {
                                log::error!(
                                    "Error while calling label function for ExecDomain `{}`: {err:#}",
                                    self.name
                                );
                                self.name.to_string()
                            }
                        }
                    }
                    #[cfg(not(feature = "lua"))]
                    {
                        let _ = label_func;
                        self.name.to_string()
                    }
                }
                _ => self.name.to_string(),
            }
        } else if let Some(wsl) = self.resolve_wsl_domain() {
            wsl.distribution.unwrap_or_else(|| self.name.to_string())
        } else {
            self.name.to_string()
        }
    }

    async fn attach(
        &self,
        _mux: &Arc<Mux>,
        _owner_client_id: Option<Arc<ClientId>>,
        _window_id: Option<WindowId>,
    ) -> anyhow::Result<()> {
        Ok(())
    }

    fn detachable(&self) -> bool {
        false
    }

    fn detach(&self) -> anyhow::Result<()> {
        bail!(
            "detach is unsupported for LocalDomain because local panes are owned by the current mux session"
        );
    }

    fn state(&self) -> DomainState {
        DomainState::Attached
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use portable_pty::{Child, ChildKiller, SlavePty};
    use std::future::{poll_fn, Future};
    use std::io::{Read, Result as IoResult, Write};
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex as StdMutex, MutexGuard as StdMutexGuard};
    use std::task::Poll;
    use std::time::{Duration, Instant};

    fn mux_test_lock() -> &'static StdMutex<()> {
        &crate::MUX_TEST_LOCK
    }

    struct ScopedMux {
        prior: Option<Arc<Mux>>,
        _guard: StdMutexGuard<'static, ()>,
    }

    impl ScopedMux {
        fn install(mux: Arc<Mux>) -> Self {
            let guard = mux_test_lock()
                .lock()
                .unwrap_or_else(|err| err.into_inner());
            let prior = Mux::try_get();
            Mux::set_mux(&mux);
            Self {
                prior,
                _guard: guard,
            }
        }
    }

    impl Drop for ScopedMux {
        fn drop(&mut self) {
            if let Some(prior) = self.prior.take() {
                Mux::set_mux(&prior);
            } else {
                Mux::shutdown();
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn recovered_local_spawn_policy_controls_actual_command() -> anyhow::Result<()> {
        let retained_cwd = std::env::current_dir()?;
        // Native command preparation deliberately discards unreadable paths.
        // Exercise restored policy with a directory that can actually be used.
        retained_cwd.read_dir()?;
        let policy = LocalDomainRecoveryPolicy {
            default_prog: Some(vec!["/bin/sh".into(), "-l".into()]),
            default_cwd: Some(retained_cwd.clone()),
            environment: [("FT_RECOVERED_POLICY".into(), "retained".into())].into(),
            term: "retained-terminal".into(),
        };
        let domain =
            LocalDomain::from_recovery_policy(817, "restored-local".into(), policy.clone())?;
        let mux = Arc::new(Mux::new(None));
        let command = promise::spawn::block_on(domain.build_command(&mux, None, None, 819))?;
        assert_eq!(
            command.get_argv().as_slice(),
            &[OsString::from("/bin/sh"), OsString::from("-l")]
        );
        assert_eq!(
            command.get_cwd().map(OsString::as_os_str),
            Some(retained_cwd.as_os_str())
        );
        assert_eq!(
            command.get_env("FT_RECOVERED_POLICY"),
            Some(std::ffi::OsStr::new("retained"))
        );
        assert_eq!(
            command.get_env("TERM"),
            Some(std::ffi::OsStr::new("retained-terminal"))
        );
        assert_eq!(domain.domain_id(), 817);
        assert_eq!(
            domain.capture_recovery_policy(&config::ConfigHandle::default_config())?,
            policy
        );
        assert!(domain.resolve_exec_domain().is_none());
        assert!(domain.resolve_wsl_domain().is_none());
        Ok(())
    }

    #[test]
    fn recovery_domain_policy_refuses_callbacks_and_invalid_environment() -> anyhow::Result<()> {
        let (pty, spawn_calls) = SlowSpawnPtySystem::new(Duration::ZERO);
        let custom = LocalDomain::with_pty_system("custom-backend", Box::new(pty));
        assert!(custom
            .capture_recovery_policy(&config::ConfigHandle::default_config())
            .is_err());
        assert_eq!(spawn_calls.load(Ordering::SeqCst), 0);
        #[cfg(unix)]
        {
            let native = Arc::new(LocalDomain::new("busy-backend")?);
            let mux = Mux::new(Some(native.clone()));
            let held = native.pty_system.lock();
            assert!(native
                .capture_recovery_policy(&config::ConfigHandle::default_config())
                .unwrap_err()
                .is::<DomainRecoveryPolicyBusy>());
            assert!(matches!(
                mux.capture_topology_coherent(Default::default()),
                Err(crate::MuxTopologyCaptureError::DomainPolicyBusy)
            ));
            drop(held);
            assert!(mux.capture_topology_coherent(Default::default()).is_ok());
        }
        let domain = LocalDomain::new_exec_domain(ExecDomain {
            name: "callback-domain".into(),
            fixup_command: "uncaptured-function".into(),
            label: None,
        })?;
        assert!(domain
            .capture_recovery_policy(&config::ConfigHandle::default_config())
            .unwrap_err()
            .downcast_ref::<DomainRecoveryPolicyBusy>()
            .is_none());
        let mux = Mux::new(Some(Arc::new(domain)));
        assert!(matches!(
            mux.capture_topology_coherent(Default::default()),
            Err(crate::MuxTopologyCaptureError::UnsupportedDomainPolicy)
        ));
        let policy = LocalDomainRecoveryPolicy {
            default_prog: None,
            default_cwd: None,
            environment: [("invalid=key".into(), "value".into())].into(),
            term: "xterm".into(),
        };
        assert!(policy.validate().is_err());
        assert!(LocalDomain::from_recovery_policy(817, "invalid".into(), policy).is_err());
        Ok(())
    }

    #[test]
    fn configured_local_generation_retains_exact_spawn_configuration() -> anyhow::Result<()> {
        let wsl = WslDomain {
            name: "retained-wsl".to_string(),
            distribution: Some("RetainedDistro".to_string()),
            username: Some("retained-user".to_string()),
            default_cwd: Some("/retained".into()),
            default_prog: Some(vec!["retained-shell".to_string()]),
        };
        let wsl_domain = LocalDomain::new_wsl(wsl.clone())?;
        assert_eq!(wsl_domain.resolve_wsl_domain(), Some(wsl));
        assert!(wsl_domain.resolve_exec_domain().is_none());

        let exec = ExecDomain {
            name: "retained-exec".to_string(),
            fixup_command: "retained-fixup".to_string(),
            label: Some(ValueOrFunc::Func("retained-label".to_string())),
        };
        let exec_domain = LocalDomain::new_exec_domain(exec.clone())?;
        assert_eq!(exec_domain.resolve_exec_domain(), Some(exec));
        assert!(exec_domain.resolve_wsl_domain().is_none());
        Ok(())
    }

    #[test]
    fn serial_domain_configuration_ownership_is_explicit() -> anyhow::Result<()> {
        let serial = SerialDomain {
            name: "provenance-serial".to_string(),
            port: Some("provenance-test-port".to_string()),
            baud: Some(u32::MAX),
        };
        let exact_backend_baud: u32 = serial.baud.expect("test baud is present");
        assert_eq!(exact_backend_baud, u32::MAX);
        let runtime = LocalDomain::new_serial_domain(serial.clone())?;
        assert!(!runtime.is_configuration_owned());
        assert!(!runtime.matches_serial_configuration(&serial));

        let configured = LocalDomain::new_configured_serial_domain(serial.clone())?;
        assert!(configured.is_configuration_owned());
        assert!(configured.matches_serial_configuration(&serial));
        Ok(())
    }

    #[derive(Debug, Clone)]
    struct TestChild {
        kill_calls: Option<Arc<AtomicUsize>>,
    }

    impl TestChild {
        fn untracked() -> Self {
            Self { kill_calls: None }
        }

        fn tracked(kill_calls: Arc<AtomicUsize>) -> Self {
            Self {
                kill_calls: Some(kill_calls),
            }
        }
    }

    impl ChildKiller for TestChild {
        fn kill(&mut self) -> IoResult<()> {
            if let Some(kill_calls) = &self.kill_calls {
                kill_calls.fetch_add(1, Ordering::SeqCst);
            }
            Ok(())
        }

        fn clone_killer(&self) -> Box<dyn ChildKiller + Send + Sync> {
            Box::new(self.clone())
        }
    }

    impl Child for TestChild {
        fn try_wait(&mut self) -> IoResult<Option<ExitStatus>> {
            Ok(None)
        }

        fn wait(&mut self) -> IoResult<ExitStatus> {
            Ok(ExitStatus::with_exit_code(0))
        }

        fn process_id(&self) -> Option<u32> {
            Some(4242)
        }
    }

    #[derive(Clone, Default)]
    struct BufferWriter {
        written: Arc<StdMutex<Vec<u8>>>,
    }

    impl Write for BufferWriter {
        fn write(&mut self, buf: &[u8]) -> IoResult<usize> {
            self.written
                .lock()
                .unwrap_or_else(|err| err.into_inner())
                .extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> IoResult<()> {
            Ok(())
        }
    }

    struct TestMasterPty {
        written: Arc<StdMutex<Vec<u8>>>,
        reader_requests: Arc<AtomicUsize>,
        resize_requests: Arc<AtomicUsize>,
    }

    impl TestMasterPty {
        fn new() -> Self {
            Self {
                written: Arc::new(StdMutex::new(Vec::new())),
                reader_requests: Arc::new(AtomicUsize::new(0)),
                resize_requests: Arc::new(AtomicUsize::new(0)),
            }
        }
    }

    impl MasterPty for TestMasterPty {
        fn resize(&self, _size: PtySize) -> Result<(), Error> {
            self.resize_requests.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }

        fn get_size(&self) -> Result<PtySize, Error> {
            Ok(PtySize::default())
        }

        fn try_clone_reader(&self) -> Result<Box<dyn Read + Send>, Error> {
            self.reader_requests.fetch_add(1, Ordering::SeqCst);
            struct BlockingReader;
            impl Read for BlockingReader {
                fn read(&mut self, _buf: &mut [u8]) -> IoResult<usize> {
                    std::thread::sleep(Duration::from_secs(86400));
                    Ok(0)
                }
            }
            Ok(Box::new(BlockingReader))
        }

        fn take_writer(&self) -> Result<Box<dyn Write + Send>, Error> {
            Ok(Box::new(BufferWriter {
                written: Arc::clone(&self.written),
            }))
        }

        #[cfg(unix)]
        fn process_group_leader(&self) -> Option<libc::pid_t> {
            None
        }

        #[cfg(unix)]
        fn as_raw_fd(&self) -> Option<std::os::fd::RawFd> {
            None
        }

        #[cfg(unix)]
        fn tty_name(&self) -> Option<std::path::PathBuf> {
            None
        }
    }

    struct SlowSpawnSlavePty {
        delay: Duration,
        spawn_calls: Arc<AtomicUsize>,
    }

    #[test]
    fn unpublished_recovered_topology_retains_metadata_without_reader_or_resize_activation() {
        use crate::tab::{MuxCapturedTab, PaneEntry, PaneNode};
        use crate::window::WindowOrderRevision;
        // Exercise the real LocalPane reader/resize endpoints with retained
        // PTY probes, rather than an inert closure pretending to be a reader.
        for invalid_revision in [false, true] {
            let id = alloc_pane_id().expect("allocate recovery fixture pane identity");
            let durable = uuid::Uuid::new_v4();
            let size = TerminalSize {
                rows: 4,
                cols: 12,
                pixel_width: 120,
                pixel_height: 40,
                dpi: 96,
            };
            let master = TestMasterPty::new();
            let reader_requests = Arc::clone(&master.reader_requests);
            let resize_requests = Arc::clone(&master.resize_requests);
            let written = Arc::clone(&master.written);
            let config =
                config::TermConfig::new_for_pane(id, 1, *durable.as_bytes(), String::new());
            let terminal = frankenterm_term::Terminal::new(
                size,
                Arc::new(config),
                "recovery-test",
                "test",
                Box::new(Vec::<u8>::new()),
            );
            let pane: Arc<dyn Pane> = Arc::new(LocalPane::new(
                id,
                terminal,
                Box::new(TestChild::untracked()),
                Box::new(master),
                Box::new(BufferWriter {
                    written: Arc::clone(&written),
                }),
                1,
                *durable.as_bytes(),
                String::new(),
            ));
            let registration = Arc::clone(pane.mux_registration_slot());
            let tab_id = 71;
            let window_id = 81;
            let tab = MuxCapturedTab {
                tab_id,
                durable_tab_id: uuid::Uuid::new_v4(),
                window_id,
                title: "restored tab title".to_string(),
                size,
                size_before_zoom: size,
                active_pane_id: Some(id),
                zoomed_pane_id: None,
                split_tree: PaneNode::Leaf(PaneEntry {
                    window_id,
                    tab_id,
                    pane_id: id,
                    title: pane.get_title(),
                    size,
                    working_dir: None,
                    alt_screen_active: false,
                    is_active_pane: true,
                    is_zoomed_pane: false,
                    workspace: "restored workspace".to_string(),
                    cursor_pos: pane.get_cursor_position(),
                    physical_top: 0,
                    top_row: 0,
                    left_col: 0,
                    tty_name: None,
                }),
                floating_panes: Vec::new(),
                floating_focus: None,
                pane_stacks: Vec::new(),
                underlying_tiled_active_pane_id: Some(id),
                runtime_state: Default::default(),
            };
            let window = crate::MuxCapturedWindow {
                window_id,
                durable_window_id: uuid::Uuid::new_v4(),
                workspace: "restored workspace".to_string(),
                title: "restored window title".to_string(),
                order_revision: WindowOrderRevision::new(if invalid_revision {
                    u64::MAX
                } else {
                    912
                }),
                ordered_tab_ids: vec![tab_id],
                active_tab_id: Some(tab_id),
                active_tab_index: Some(0),
                last_active_tab_id: Some(tab_id),
                tab_stacks: Vec::new(),
                position: None,
                structural_pane_count: 1,
            };
            let prepared = UnpublishedRecoveredTopology::prepare(
                vec![window.clone()],
                vec![tab.clone()],
                vec![crate::MuxCapturedDomain {
                    domain_id: 1,
                    name: "fixture-local".into(),
                    state: DomainState::Attached,
                    policy: DomainRecoveryPolicy::Local(LocalDomainRecoveryPolicy {
                        default_prog: None,
                        default_cwd: None,
                        environment: Default::default(),
                        term: "xterm".into(),
                    }),
                }],
                Some(1),
                "restored workspace".into(),
                &HashMap::from([(id, (*durable.as_bytes(), 1))]),
                vec![UnpublishedPane::new(pane)],
            );
            if invalid_revision {
                assert!(prepared.is_err());
            } else {
                let prepared = prepared.expect("valid private topology");
                assert_eq!(prepared.counts(), (1, 1, 1));
                assert_eq!(prepared.default_domain_id(), Some(1));
                assert_eq!(prepared.default_workspace(), "restored workspace");
                assert_eq!(prepared.captured_domains()[0].name, "fixture-local");
                let actual_window = &prepared.windows[0];
                assert_eq!(actual_window.durable_id(), window.durable_window_id);
                assert_eq!(actual_window.get_title(), window.title);
                assert_eq!(actual_window.get_workspace(), window.workspace);
                assert_eq!(
                    actual_window.order_snapshot().unwrap().order_revision(),
                    window.order_revision
                );
                let actual_tab = prepared
                    .tabs
                    .get(&tab_id)
                    .unwrap()
                    .capture_tab_topology(window_id, &window.workspace)
                    .unwrap();
                assert_eq!(actual_tab, tab);
                assert!(registration.load().is_none());
                drop(prepared);
            }
            assert_eq!(reader_requests.load(Ordering::SeqCst), 0);
            assert_eq!(resize_requests.load(Ordering::SeqCst), 0);
            assert!(written.lock().unwrap().is_empty());
            assert!(registration.load().is_none());
        }
    }

    impl SlavePty for SlowSpawnSlavePty {
        fn spawn_command(
            &self,
            _cmd: CommandBuilder,
        ) -> Result<Box<dyn Child + Send + Sync>, Error> {
            self.spawn_calls.fetch_add(1, Ordering::SeqCst);
            std::thread::sleep(self.delay);
            Ok(Box::new(TestChild::untracked()))
        }
    }

    struct SlowSpawnPtySystem {
        delay: Duration,
        spawn_calls: Arc<AtomicUsize>,
    }

    impl SlowSpawnPtySystem {
        fn new(delay: Duration) -> (Self, Arc<AtomicUsize>) {
            let spawn_calls = Arc::new(AtomicUsize::new(0));
            (
                Self {
                    delay,
                    spawn_calls: Arc::clone(&spawn_calls),
                },
                spawn_calls,
            )
        }
    }

    impl PtySystem for SlowSpawnPtySystem {
        fn openpty(&self, _size: PtySize) -> anyhow::Result<PtyPair> {
            Ok(PtyPair {
                slave: Box::new(SlowSpawnSlavePty {
                    delay: self.delay,
                    spawn_calls: Arc::clone(&self.spawn_calls),
                }),
                master: Box::new(TestMasterPty::new()),
            })
        }
    }

    struct CancellationSpawnSlavePty {
        started: Arc<AtomicBool>,
        release: Arc<AtomicBool>,
        kill_calls: Arc<AtomicUsize>,
    }

    impl SlavePty for CancellationSpawnSlavePty {
        fn spawn_command(
            &self,
            _cmd: CommandBuilder,
        ) -> Result<Box<dyn Child + Send + Sync>, Error> {
            self.started.store(true, Ordering::Release);
            while !self.release.load(Ordering::Acquire) {
                std::thread::sleep(Duration::from_millis(1));
            }
            Ok(Box::new(TestChild::tracked(Arc::clone(&self.kill_calls))))
        }
    }

    struct CancellationSpawnPtySystem {
        started: Arc<AtomicBool>,
        release: Arc<AtomicBool>,
        kill_calls: Arc<AtomicUsize>,
    }

    impl PtySystem for CancellationSpawnPtySystem {
        fn openpty(&self, _size: PtySize) -> anyhow::Result<PtyPair> {
            Ok(PtyPair {
                slave: Box::new(CancellationSpawnSlavePty {
                    started: Arc::clone(&self.started),
                    release: Arc::clone(&self.release),
                    kill_calls: Arc::clone(&self.kill_calls),
                }),
                master: Box::new(TestMasterPty::new()),
            })
        }
    }

    fn wslenv_entries(cmd: &CommandBuilder) -> Vec<String> {
        cmd.get_env("WSLENV")
            .map(|value| value.to_string_lossy().to_string())
            .map(|value| {
                value
                    .split(':')
                    .filter(|entry| !entry.is_empty())
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default()
    }

    #[test]
    fn domain_state_equality() {
        assert_eq!(DomainState::Detached, DomainState::Detached);
        assert_eq!(DomainState::Attached, DomainState::Attached);
        assert_ne!(DomainState::Detached, DomainState::Attached);
    }

    #[test]
    fn local_domain_detach_is_explicitly_unsupported() {
        let domain = LocalDomain::new("local-detach-test").expect("local domain");
        assert!(!domain.detachable());

        let err = domain
            .detach()
            .expect_err("local domain detach should fail");
        let err = err.to_string();
        assert!(err.contains("unsupported"), "{}", err);
        assert!(err.contains("LocalDomain"), "{}", err);
    }

    #[test]
    fn domain_state_clone_copy() {
        let s = DomainState::Attached;
        let s2 = s; // Copy
        let s3 = s.clone(); // Clone
        assert_eq!(s, s2);
        assert_eq!(s, s3);
    }

    #[test]
    fn domain_state_debug() {
        let dbg = format!("{:?}", DomainState::Detached);
        assert!(dbg.contains("Detached"));
        let dbg = format!("{:?}", DomainState::Attached);
        assert!(dbg.contains("Attached"));
    }

    #[test]
    fn split_source_move_pane() {
        let a = SplitSource::MovePane(42);
        let b = SplitSource::MovePane(42);
        assert_eq!(a, b);

        let c = SplitSource::MovePane(99);
        assert_ne!(a, c);
    }

    #[test]
    fn split_source_spawn_no_command() {
        let a = SplitSource::Spawn {
            command: None,
            command_dir: None,
        };
        let b = SplitSource::Spawn {
            command: None,
            command_dir: None,
        };
        assert_eq!(a, b);
    }

    #[test]
    fn split_source_spawn_with_dir() {
        let a = SplitSource::Spawn {
            command: None,
            command_dir: Some("/home/user".to_string()),
        };
        let b = SplitSource::Spawn {
            command: None,
            command_dir: Some("/home/user".to_string()),
        };
        assert_eq!(a, b);

        let c = SplitSource::Spawn {
            command: None,
            command_dir: Some("/tmp".to_string()),
        };
        assert_ne!(a, c);
    }

    #[test]
    fn split_source_debug() {
        let s = SplitSource::MovePane(5);
        let dbg = format!("{:?}", s);
        assert!(dbg.contains("MovePane"));
        assert!(dbg.contains("5"));
    }

    #[test]
    fn split_source_clone() {
        let a = SplitSource::MovePane(10);
        let b = a.clone();
        assert_eq!(a, b);
    }

    #[test]
    fn split_source_variants_inequality() {
        let spawn = SplitSource::Spawn {
            command: None,
            command_dir: None,
        };
        let mv = SplitSource::MovePane(0);
        assert_ne!(spawn, mv);
    }

    #[test]
    fn rewrite_command_for_wsl_adds_explicit_env_keys_to_wslenv() -> anyhow::Result<()> {
        let wsl = WslDomain {
            name: "WSL:Ubuntu".to_string(),
            distribution: Some("Ubuntu".to_string()),
            username: Some("alice".to_string()),
            default_cwd: None,
            default_prog: None,
        };

        let mut cmd = CommandBuilder::new("bash");
        cmd.cwd("/tmp/project");
        cmd.env("WSLENV", "TERM:COLORTERM");
        cmd.env("WEZTERM_PANE", "7");
        cmd.env("CUSTOM_KEY", "custom");

        LocalDomain::rewrite_command_for_wsl(&mut cmd, &wsl)?;

        let argv = cmd
            .get_argv()
            .iter()
            .map(|arg| arg.to_string_lossy().to_string())
            .collect::<Vec<_>>();
        assert_eq!(
            argv,
            vec![
                "wsl.exe",
                "--distribution",
                "Ubuntu",
                "--cd",
                "/tmp/project",
                "--user",
                "alice",
                "--exec",
                "bash",
            ]
        );
        assert!(cmd.get_cwd().is_none());

        let mut entries = wslenv_entries(&cmd);
        entries.sort();
        assert_eq!(
            entries,
            vec!["COLORTERM", "CUSTOM_KEY", "TERM", "WEZTERM_PANE"]
        );
        Ok(())
    }

    #[test]
    fn rewrite_command_for_wsl_preserves_existing_flagged_wslenv_entries() -> anyhow::Result<()> {
        let wsl = WslDomain {
            name: "WSL:Ubuntu".to_string(),
            distribution: Some("Ubuntu".to_string()),
            username: None,
            default_cwd: None,
            default_prog: None,
        };

        let mut cmd = CommandBuilder::new("env");
        cmd.env("WSLENV", "SSH_AUTH_SOCK/p:TERM");
        cmd.env("SSH_AUTH_SOCK", "/tmp/agent.sock");
        cmd.env("TERM", "xterm-256color");
        cmd.env("WEZTERM_PANE", "11");

        LocalDomain::rewrite_command_for_wsl(&mut cmd, &wsl)?;

        let entries = wslenv_entries(&cmd);
        assert!(entries.iter().any(|entry| entry == "SSH_AUTH_SOCK/p"));
        assert!(entries.iter().any(|entry| entry == "TERM"));
        assert!(entries.iter().any(|entry| entry == "WEZTERM_PANE"));
        assert_eq!(
            entries
                .iter()
                .filter(|entry| LocalDomain::wslenv_entry_name(entry) == "SSH_AUTH_SOCK")
                .count(),
            1
        );
        assert_eq!(
            entries
                .iter()
                .filter(|entry| LocalDomain::wslenv_entry_name(entry) == "TERM")
                .count(),
            1
        );
        Ok(())
    }

    #[test]
    fn local_domain_spawn_pane_first_poll_stays_non_blocking() {
        const SPAWN_DELAY: Duration = Duration::from_millis(200);

        let mux = Arc::new(Mux::new(None));
        let _guard = ScopedMux::install(Arc::clone(&mux));
        let exec = promise::spawn::ScopedExecutor::new();
        let (pty_system, spawn_calls) = SlowSpawnPtySystem::new(SPAWN_DELAY);
        let domain = LocalDomain::with_pty_system("slow-spawn-test", Box::new(pty_system));

        let pane_id = promise::spawn::block_on(exec.run(async {
            let mut spawn_pane = std::pin::pin!(domain.spawn_pane(
                &mux,
                TerminalSize::default(),
                Some(CommandBuilder::new("slow-spawn-test")),
                None,
            ));

            let first_poll = poll_fn(|cx| {
                Poll::Ready(match spawn_pane.as_mut().poll(cx) {
                    Poll::Ready(result) => Some(result),
                    Poll::Pending => None,
                })
            })
            .await;

            assert!(
                first_poll.is_none(),
                "[ft-odywh] spawn_pane completed during the first poll; \
                 spawn_command is likely running synchronously on the executor thread again"
            );

            spawn_pane
                .await
                .expect("spawn pane should succeed")
                .pane_id()
        }));

        assert_eq!(
            spawn_calls.load(Ordering::SeqCst),
            1,
            "[ft-odywh] fake PTY should be spawned exactly once"
        );
        assert!(
            mux.get_pane(pane_id).is_some(),
            "[ft-odywh] mux should register the pane returned by spawn_pane"
        );
    }

    #[test]
    fn cancelled_unpublished_spawn_kills_child_materialized_after_cancellation() {
        const WAIT_LIMIT: Duration = Duration::from_secs(2);

        struct ReleaseOnDrop(Arc<AtomicBool>);
        impl Drop for ReleaseOnDrop {
            fn drop(&mut self) {
                self.0.store(true, Ordering::Release);
            }
        }

        let mux = Arc::new(Mux::new(None));
        let _guard = ScopedMux::install(Arc::clone(&mux));
        let exec = promise::spawn::ScopedExecutor::new();
        let started = Arc::new(AtomicBool::new(false));
        let release = Arc::new(AtomicBool::new(false));
        let kill_calls = Arc::new(AtomicUsize::new(0));
        let release_on_drop = ReleaseOnDrop(Arc::clone(&release));
        let domain = LocalDomain::with_pty_system(
            "cancelled-spawn-test",
            Box::new(CancellationSpawnPtySystem {
                started: Arc::clone(&started),
                release: Arc::clone(&release),
                kill_calls: Arc::clone(&kill_calls),
            }),
        );

        promise::spawn::block_on(exec.run(async {
            let mut spawn = domain.spawn_unpublished_pane(
                &mux,
                TerminalSize::default(),
                Some(CommandBuilder::new("cancelled-spawn-test")),
                None,
            );

            let first_poll = poll_fn(|cx| {
                Poll::Ready(match spawn.as_mut().poll(cx) {
                    Poll::Ready(result) => Some(result),
                    Poll::Pending => None,
                })
            })
            .await;
            assert!(
                first_poll.is_none(),
                "unpublished spawn unexpectedly completed before its worker was released"
            );

            let deadline = Instant::now() + WAIT_LIMIT;
            while !started.load(Ordering::Acquire) && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(1));
            }
            assert!(
                started.load(Ordering::Acquire),
                "blocking spawn worker did not start within {wait_limit:?}",
                wait_limit = WAIT_LIMIT,
            );

            drop(spawn);
            release.store(true, Ordering::Release);
        }));
        drop(exec);
        drop(release_on_drop);

        let deadline = Instant::now() + WAIT_LIMIT;
        while kill_calls.load(Ordering::SeqCst) == 0 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(
            kill_calls.load(Ordering::SeqCst),
            1,
            "a child produced after spawn cancellation must be killed exactly once"
        );
    }
}
