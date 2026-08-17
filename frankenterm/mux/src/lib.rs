#![allow(clippy::bool_assert_comparison)]
#![allow(clippy::borrow_deref_ref)]
#![allow(clippy::box_collection)]
#![allow(clippy::boxed_local)]
#![allow(clippy::clone_on_copy)]
#![allow(clippy::collapsible_else_if)]
#![allow(clippy::derivable_impls)]
#![allow(clippy::empty_line_after_doc_comments)]
#![allow(clippy::explicit_auto_deref)]
#![allow(clippy::extra_unused_type_parameters)]
#![allow(clippy::field_reassign_with_default)]
#![allow(clippy::from_over_into)]
#![allow(clippy::get_first)]
#![allow(clippy::into_iter_on_ref)]
#![allow(clippy::io_other_error)]
#![allow(clippy::iter_kv_map)]
#![allow(clippy::iter_nth)]
#![allow(clippy::let_unit_value)]
#![allow(clippy::manual_flatten)]
#![allow(clippy::manual_map)]
#![allow(clippy::map_clone)]
#![allow(clippy::map_entry)]
#![allow(clippy::match_like_matches_macro)]
#![allow(clippy::needless_borrow)]
#![allow(clippy::needless_borrowed_reference)]
#![allow(clippy::needless_borrows_for_generic_args)]
#![allow(clippy::needless_return)]
#![allow(clippy::neg_multiply)]
#![allow(clippy::new_ret_no_self)]
#![allow(clippy::new_without_default)]
#![allow(clippy::nonminimal_bool)]
#![allow(clippy::option_as_ref_deref)]
#![allow(clippy::option_map_unit_fn)]
#![allow(clippy::redundant_field_names)]
#![allow(clippy::redundant_guards)]
#![allow(clippy::redundant_pattern)]
#![allow(clippy::redundant_pattern_matching)]
#![allow(clippy::result_large_err)]
#![allow(clippy::search_is_some)]
#![allow(clippy::single_char_add_str)]
#![allow(clippy::single_match)]
#![allow(clippy::too_many_arguments)]
#![allow(clippy::type_complexity)]
#![allow(clippy::unnecessary_cast)]
#![allow(clippy::unnecessary_get_then_check)]
#![allow(clippy::unnecessary_lazy_evaluations)]
#![allow(clippy::unnecessary_map_or)]
#![allow(clippy::useless_conversion)]
#![allow(clippy::useless_format)]
#![allow(clippy::wildcard_in_or_patterns)]
#[cfg(all(feature = "async-smol", feature = "async-asupersync"))]
compile_error!(
    "mux async runtime features are mutually exclusive; enable only one of \"async-smol\" or \"async-asupersync\""
);
#[cfg(not(any(feature = "async-smol", feature = "async-asupersync")))]
compile_error!(
    "mux requires one async runtime feature: \"async-asupersync\" (preferred) or \"async-smol\""
);

use crate::client::{ClientId, ClientInfo};
use crate::pane::{CachePolicy, CloseReason, Pane, PaneId};
use crate::ssh_agent::AgentProxy;
use crate::tab::{FloatingPaneRect, SplitRequest, Tab, TabId};
use crate::tmux::TmuxDomain;
use crate::window::{
    FrozenWindowOrder, PrepareWindowOrderError, PreparedWindowState, Window, WindowId,
    WindowOrderRevision, WindowOrderSnapshotError, MAX_TABS_PER_ORDERED_WINDOW,
};
use anyhow::{anyhow, Context, Error};
use config::keyassignment::SpawnTabDomain;
use config::{configuration, ExitBehavior, GuiPosition, TermConfig};
use domain::{Domain, DomainId, DomainState, SplitSource};
use filedescriptor::{poll, pollfd, socketpair, AsRawSocketDescriptor, FileDescriptor, POLLIN};
use frankenterm_sigpipe::{catch_recoverable, RecoverablePanicSite};
use frankenterm_term::{Alert, Clipboard, ClipboardSelection, DownloadHandler, TerminalSize};
#[cfg(unix)]
use libc::{c_int, SOL_SOCKET, SO_RCVBUF, SO_SNDBUF};
use log::error;
use metrics::histogram;
use parking_lot::{
    Condvar, MappedRwLockReadGuard, MappedRwLockWriteGuard, Mutex, RwLock, RwLockReadGuard,
    RwLockWriteGuard,
};
use percent_encoding::percent_decode_str;
use portable_pty::{CommandBuilder, ExitStatus, PtySize};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use std::collections::{HashMap, HashSet, VecDeque};
use std::convert::{TryFrom, TryInto};
use std::io::{Read, Write};
use std::num::NonZeroU64;
#[cfg(windows)]
use std::os::raw::c_int;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Weak};
use std::thread;
use std::time::{Duration, Instant};
use termwiz::escape::csi::{DecPrivateMode, DecPrivateModeCode, Device, Mode};
use termwiz::escape::{Action, CSI};
use termwiz::input::KeyEvent;
use thiserror::*;
#[cfg(windows)]
use winapi::um::winsock2::{SOL_SOCKET, SO_RCVBUF, SO_SNDBUF};

pub mod activity;
pub mod client;
pub mod connui;
pub mod domain;
pub mod events;
pub mod layout;
pub mod localpane;
pub mod pane;
pub mod renderable;
pub mod ssh;
pub mod ssh_agent;
pub mod tab;
mod terminfo_renderer;
pub mod termwiztermtab;
pub mod tmux;
pub mod tmux_commands;
mod tmux_pty;
pub mod unify;
pub mod window;

use crate::activity::{Activity, ActivityPruneState};

pub const DEFAULT_WORKSPACE: &str = "default";

/// Unpredictable identity for one live mux-session incarnation.
///
/// Numeric pane, tab, and window identifiers are meaningful only inside this
/// scope. A newly constructed mux always receives a fresh incarnation so a
/// reconnect can distinguish a surviving session from a restarted server even
/// when process-local numeric identifiers happen to repeat.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
pub struct MuxSessionIncarnation([u8; 16]);

impl MuxSessionIncarnation {
    pub fn new() -> Self {
        Self(*uuid::Uuid::new_v4().as_bytes())
    }

    pub const fn from_bytes(bytes: [u8; 16]) -> Self {
        Self(bytes)
    }

    pub const fn as_bytes(self) -> [u8; 16] {
        self.0
    }
}

impl Default for MuxSessionIncarnation {
    fn default() -> Self {
        Self::new()
    }
}

/// Session-global revision of the mux topology publication stream.
///
/// Revision zero names the initial empty state. Every topology notification
/// publication reserves the next value with checked addition. Exhaustion is a
/// terminal authority state; the counter never wraps, saturates, resets, or
/// reuses a prior value.
#[derive(
    Clone, Copy, Debug, Default, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize,
)]
pub struct TopologyRevision(u64);

impl TopologyRevision {
    pub const INITIAL: Self = Self(0);

    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    pub const fn get(self) -> u64 {
        self.0
    }
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
#[error(
    "mux topology revision space is exhausted; refusing to wrap, saturate, reset, or reuse a revision"
)]
pub struct TopologyRevisionExhausted;

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum TopologySubscriptionError {
    #[error(transparent)]
    RevisionExhausted(#[from] TopologyRevisionExhausted),
    #[error(transparent)]
    IdentifierAllocation(#[from] IdAllocationError),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MuxTopologyStamp {
    NonTopology,
    Revision(TopologyRevision),
    Exhausted,
}

#[derive(Clone, Debug)]
pub struct MuxNotificationEnvelope {
    pub notification: MuxNotification,
    pub topology: MuxTopologyStamp,
}

#[derive(Debug)]
struct MuxTopologyAuthority {
    session_incarnation: MuxSessionIncarnation,
    revision: TopologyRevision,
    exhausted: bool,
}

impl MuxTopologyAuthority {
    fn new() -> Self {
        Self {
            session_incarnation: MuxSessionIncarnation::new(),
            revision: TopologyRevision::INITIAL,
            exhausted: false,
        }
    }

    fn reserve_revision(&mut self) -> Result<TopologyRevision, TopologyRevisionExhausted> {
        if self.exhausted {
            return Err(TopologyRevisionExhausted);
        }
        let Some(next) = self.revision.0.checked_add(1) else {
            self.exhausted = true;
            return Err(TopologyRevisionExhausted);
        };
        if next == u64::MAX {
            self.exhausted = true;
            return Err(TopologyRevisionExhausted);
        }
        self.revision = TopologyRevision(next);
        Ok(self.revision)
    }

    fn reserve_revisions(
        &mut self,
        count: usize,
    ) -> Result<TopologyRevision, TopologyRevisionExhausted> {
        if count == 0 {
            return Ok(self.revision);
        }
        if self.exhausted {
            return Err(TopologyRevisionExhausted);
        }
        let count = u64::try_from(count).map_err(|_| TopologyRevisionExhausted)?;
        let Some(first) = self.revision.0.checked_add(1) else {
            self.exhausted = true;
            return Err(TopologyRevisionExhausted);
        };
        let Some(last) = self.revision.0.checked_add(count) else {
            self.exhausted = true;
            return Err(TopologyRevisionExhausted);
        };
        if last == u64::MAX {
            self.exhausted = true;
            return Err(TopologyRevisionExhausted);
        }
        self.revision = TopologyRevision(last);
        Ok(TopologyRevision(first))
    }

    fn snapshot(
        &self,
    ) -> Result<(MuxSessionIncarnation, TopologyRevision), TopologyRevisionExhausted> {
        if self.exhausted || self.revision.0 == u64::MAX {
            Err(TopologyRevisionExhausted)
        } else {
            Ok((self.session_incarnation, self.revision))
        }
    }

    const fn current_revision(&self) -> TopologyRevision {
        self.revision
    }
}

/// Maximum number of idempotency receipts retained by one mux incarnation.
///
/// The ledger is insertion-ordered rather than access-ordered: adversarial
/// retries cannot keep old identities resident forever by touching them.
pub const MAX_WINDOW_ORDER_RECEIPTS: usize = 4_096;
/// Aggregate compact tab identities retained across reorder receipts.
///
/// This prevents the count bound from becoming a hidden 4096 x 4096 memory
/// multiplier when every recent request targeted a very large window.
pub const MAX_WINDOW_ORDER_RECEIPT_TAB_IDS: usize = 65_536;

/// Canonical semantic version mixed into every v1 reorder digest.
///
/// This lives with mux authority so the wire codec and the mutation boundary
/// cannot silently drift to different digest grammars.
pub const WINDOW_REORDER_PROTOCOL_VERSION_V1: u16 = 1;
/// Domain separator for the mux-authoritative v1 reorder digest.
pub const WINDOW_REORDER_DIGEST_DOMAIN_V1: &[u8] = b"frankenterm.window-reorder.v1\0";

/// Idempotency identity unique within one client-owned random namespace.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct WindowOrderMutationId {
    pub namespace: [u8; 16],
    pub sequence: u64,
}

impl WindowOrderMutationId {
    pub const fn new(namespace: [u8; 16], sequence: u64) -> Self {
        Self {
            namespace,
            sequence,
        }
    }

    fn is_valid(self) -> bool {
        self.namespace != [0; 16] && self.sequence != 0 && self.sequence != u64::MAX
    }
}

/// Canonical digest of one frozen reorder request, derived by mux authority.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct WindowReorderDigest([u8; 32]);

impl WindowReorderDigest {
    pub const fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    pub const fn as_bytes(self) -> [u8; 32] {
        self.0
    }
}

/// Fixed-width fields in the canonical v1 reorder digest preimage.
///
/// `desired_tab_ids` is supplied separately as an exact-size iterator so the
/// codec can hash its stable `u64` wire identities without allocating a second
/// vector. The topology stream identity is intentionally absent: reconnect
/// rotates streams while an exact idempotent retry must retain its digest.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WindowReorderDigestInputV1 {
    pub protocol_version: u16,
    pub domain_binding_id: [u8; 16],
    pub session_incarnation: MuxSessionIncarnation,
    pub window_id: u64,
    pub expected_order_revision: u64,
    pub desired_active_tab_id: Option<u64>,
    pub mutation_id: WindowOrderMutationId,
}

/// Derive the single canonical digest shared by codec validation and mux
/// mutation authority.
#[must_use]
pub fn canonical_window_reorder_digest_v1(
    input: WindowReorderDigestInputV1,
    desired_tab_ids: impl ExactSizeIterator<Item = u64>,
) -> WindowReorderDigest {
    let mut hasher = Sha256::new();
    hasher.update(WINDOW_REORDER_DIGEST_DOMAIN_V1);
    hasher.update(input.protocol_version.to_be_bytes());
    hasher.update(input.domain_binding_id);
    hasher.update(input.session_incarnation.as_bytes());
    hasher.update(input.window_id.to_be_bytes());
    hasher.update(input.expected_order_revision.to_be_bytes());
    hasher.update(
        u64::try_from(desired_tab_ids.len())
            .unwrap_or(u64::MAX)
            .to_be_bytes(),
    );
    for tab_id in desired_tab_ids {
        hasher.update(tab_id.to_be_bytes());
    }
    match input.desired_active_tab_id {
        None => hasher.update([0]),
        Some(tab_id) => {
            hasher.update([1]);
            hasher.update(tab_id.to_be_bytes());
        }
    }
    hasher.update(input.mutation_id.namespace);
    hasher.update(input.mutation_id.sequence.to_be_bytes());
    WindowReorderDigest::from_bytes(hasher.finalize().into())
}

/// Opaque mux-domain form of one decoded reorder request.
///
/// Callers provide semantic fields to [`ReorderWindowTabsRequest::try_new_v1`];
/// mux authority validates their cycle-free representation and derives the
/// digest itself. Private fields prevent a server adapter from pairing a
/// mutation identity with unrelated digest bytes.
///
/// ```compile_fail
/// use mux::{ReorderWindowTabsRequest, WindowReorderDigest};
///
/// fn forge(mut request: ReorderWindowTabsRequest) {
///     request.request_digest = WindowReorderDigest::from_bytes([0xff; 32]);
/// }
/// ```
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReorderWindowTabsRequest {
    protocol_version: u16,
    domain_binding_id: [u8; 16],
    session_incarnation: MuxSessionIncarnation,
    window_id: WindowId,
    expected_order_revision: WindowOrderRevision,
    desired_tab_ids: Vec<TabId>,
    desired_active_tab_id: Option<TabId>,
    mutation_id: WindowOrderMutationId,
    request_digest: WindowReorderDigest,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WindowReorderMalformed {
    UnsupportedProtocolVersion {
        actual: u16,
    },
    InvalidDomainBindingIdentity,
    InvalidSessionIncarnation,
    InvalidMutationIdentity,
    WireIdDoesNotFitU64 {
        field: &'static str,
        value: usize,
    },
    ReservedWireId {
        field: &'static str,
        value: u64,
    },
    ExpectedRevisionExhausted,
    TooManyTabs {
        count: usize,
        max: usize,
    },
    DuplicateTabId {
        tab_id: TabId,
    },
    ActiveTabRequired,
    ActiveTabNotInDesiredOrder {
        tab_id: TabId,
    },
    InvalidCurrentState,
    MissingTabId {
        tab_id: TabId,
    },
    ForeignTabId {
        tab_id: TabId,
    },
    ActiveTabChanged {
        current_active_tab_id: Option<TabId>,
        desired_active_tab_id: Option<TabId>,
    },
    DigestMismatch {
        expected: WindowReorderDigest,
        actual: WindowReorderDigest,
    },
}

fn reorder_wire_id(field: &'static str, value: usize) -> Result<u64, WindowReorderMalformed> {
    let wire_value = u64::try_from(value)
        .map_err(|_| WindowReorderMalformed::WireIdDoesNotFitU64 { field, value })?;
    if wire_value == u64::MAX {
        return Err(WindowReorderMalformed::ReservedWireId {
            field,
            value: wire_value,
        });
    }
    Ok(wire_value)
}

impl ReorderWindowTabsRequest {
    /// Validate and freeze one v1 reorder intent, deriving its digest inside
    /// mux authority before the request can reach the receipt ledger.
    pub fn try_new_v1(
        domain_binding_id: [u8; 16],
        session_incarnation: MuxSessionIncarnation,
        window_id: WindowId,
        expected_order_revision: WindowOrderRevision,
        desired_tab_ids: Vec<TabId>,
        desired_active_tab_id: Option<TabId>,
        mutation_id: WindowOrderMutationId,
    ) -> Result<Self, WindowReorderMalformed> {
        let mut request = Self {
            protocol_version: WINDOW_REORDER_PROTOCOL_VERSION_V1,
            domain_binding_id,
            session_incarnation,
            window_id,
            expected_order_revision,
            desired_tab_ids,
            desired_active_tab_id,
            mutation_id,
            request_digest: WindowReorderDigest::from_bytes([0; 32]),
        };
        request.request_digest = derive_reorder_window_tabs_digest(&request)?;
        Ok(request)
    }

    pub const fn session_incarnation(&self) -> MuxSessionIncarnation {
        self.session_incarnation
    }

    pub const fn window_id(&self) -> WindowId {
        self.window_id
    }

    pub const fn expected_order_revision(&self) -> WindowOrderRevision {
        self.expected_order_revision
    }

    pub fn desired_tab_ids(&self) -> &[TabId] {
        &self.desired_tab_ids
    }

    pub const fn desired_active_tab_id(&self) -> Option<TabId> {
        self.desired_active_tab_id
    }

    pub const fn mutation_id(&self) -> WindowOrderMutationId {
        self.mutation_id
    }

    pub const fn request_digest(&self) -> WindowReorderDigest {
        self.request_digest
    }
}

/// Compact immutable identity state used in replies and retained receipts.
/// Unlike [`FrozenWindowOrder`], this never keeps a live tab or pane alive.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WindowOrderState {
    pub window_id: WindowId,
    pub order_revision: WindowOrderRevision,
    pub ordered_tab_ids: Arc<[TabId]>,
    pub active_tab_id: Option<TabId>,
}

impl WindowOrderState {
    fn from_frozen(window: &FrozenWindowOrder) -> Self {
        Self {
            window_id: window.window_id(),
            order_revision: window.order_revision(),
            ordered_tab_ids: Arc::from(window.ordered_tab_ids().collect::<Vec<_>>()),
            active_tab_id: window.active_tab_id(),
        }
    }

    fn from_semantically_validated_window(window: &Window) -> Self {
        Self {
            window_id: window.window_id(),
            order_revision: window.order_revision(),
            ordered_tab_ids: Arc::from(window.iter().map(|tab| tab.tab_id()).collect::<Vec<_>>()),
            active_tab_id: window.get_active().map(|tab| tab.tab_id()),
        }
    }
}

/// Frozen result of an applied or conflicting compare-and-set decision.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WindowOrderCommit {
    pub topology_revision: TopologyRevision,
    pub window: WindowOrderState,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum WindowReorderTerminalOutcome {
    Applied(WindowOrderCommit),
    Conflict(WindowOrderCommit),
    StaleIncarnation,
    MissingWindow { window_id: WindowId },
    Malformed(WindowReorderMalformed),
    Exhausted,
}

impl WindowReorderTerminalOutcome {
    fn retained_tab_id_count(&self) -> usize {
        match self {
            Self::Applied(commit) | Self::Conflict(commit) => commit.window.ordered_tab_ids.len(),
            Self::StaleIncarnation
            | Self::MissingWindow { .. }
            | Self::Malformed(_)
            | Self::Exhausted => 0,
        }
    }
}

/// Exact retry and same-identity/different-digest equivocation are distinct
/// from a first terminal decision.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ReorderWindowTabsResult {
    Decision(WindowReorderTerminalOutcome),
    Replay(WindowReorderTerminalOutcome),
    Equivocation {
        mutation_id: WindowOrderMutationId,
        retained_digest: WindowReorderDigest,
        attempted_digest: WindowReorderDigest,
    },
}

fn derive_reorder_window_tabs_digest(
    request: &ReorderWindowTabsRequest,
) -> Result<WindowReorderDigest, WindowReorderMalformed> {
    if request.protocol_version != WINDOW_REORDER_PROTOCOL_VERSION_V1 {
        return Err(WindowReorderMalformed::UnsupportedProtocolVersion {
            actual: request.protocol_version,
        });
    }
    if request.domain_binding_id == [0; 16] {
        return Err(WindowReorderMalformed::InvalidDomainBindingIdentity);
    }
    if request.session_incarnation.as_bytes() == [0; 16] {
        return Err(WindowReorderMalformed::InvalidSessionIncarnation);
    }
    let window_id = reorder_wire_id("window_id", request.window_id)?;
    if request.expected_order_revision.get() == u64::MAX {
        return Err(WindowReorderMalformed::ExpectedRevisionExhausted);
    }
    if request.desired_tab_ids.len() > MAX_TABS_PER_ORDERED_WINDOW {
        return Err(WindowReorderMalformed::TooManyTabs {
            count: request.desired_tab_ids.len(),
            max: MAX_TABS_PER_ORDERED_WINDOW,
        });
    }
    for &tab_id in &request.desired_tab_ids {
        reorder_wire_id("tab_id", tab_id)?;
    }
    let desired_active_tab_id = request
        .desired_active_tab_id
        .map(|tab_id| reorder_wire_id("active_tab_id", tab_id))
        .transpose()?;
    if !request.mutation_id.is_valid() {
        return Err(WindowReorderMalformed::InvalidMutationIdentity);
    }
    Ok(canonical_window_reorder_digest_v1(
        WindowReorderDigestInputV1 {
            protocol_version: request.protocol_version,
            domain_binding_id: request.domain_binding_id,
            session_incarnation: request.session_incarnation,
            window_id,
            expected_order_revision: request.expected_order_revision.get(),
            desired_active_tab_id,
            mutation_id: request.mutation_id,
        },
        request.desired_tab_ids.iter().map(|&tab_id| {
            u64::try_from(tab_id).expect("validated reorder tab id must fit the wire width")
        }),
    ))
}

fn validate_reorder_window_tabs_request(
    request: &ReorderWindowTabsRequest,
) -> Result<(), WindowReorderMalformed> {
    let expected = derive_reorder_window_tabs_digest(request)?;
    if request.request_digest != expected {
        return Err(WindowReorderMalformed::DigestMismatch {
            expected,
            actual: request.request_digest,
        });
    }
    Ok(())
}

/// Immutable result of one mux-owned window topology transaction.
#[derive(Clone, Debug)]
pub struct FrozenWindowTopologyChange {
    windows: Arc<[FrozenWindowOrder]>,
    attached_tabs: Arc<[(TabId, WindowId)]>,
    created_windows: Arc<[WindowId]>,
    removed_windows: Arc<[WindowId]>,
}

impl FrozenWindowTopologyChange {
    fn from_prepared(
        mut windows: Vec<FrozenWindowOrder>,
        mut attached_tabs: Vec<(TabId, WindowId)>,
        mut created_windows: Vec<WindowId>,
        mut removed_windows: Vec<WindowId>,
    ) -> anyhow::Result<Self> {
        windows.sort_unstable_by_key(FrozenWindowOrder::window_id);
        anyhow::ensure!(
            windows
                .windows(2)
                .all(|pair| pair[0].window_id() != pair[1].window_id()),
            "a frozen window topology change cannot contain one window twice"
        );
        let frozen_tab_count = windows.iter().try_fold(0usize, |count, window| {
            count
                .checked_add(window.ordered_tabs().len())
                .context("counting frozen window transaction tabs")
        })?;
        let mut frozen_tab_ids = HashSet::new();
        frozen_tab_ids
            .try_reserve(frozen_tab_count)
            .map_err(|error| anyhow!("reserve frozen window tab identities: {error}"))?;
        for window in &windows {
            for tab in window.ordered_tabs() {
                anyhow::ensure!(
                    frozen_tab_ids.insert(tab.tab_id()),
                    "a frozen window topology change cannot retain tab id {} in multiple windows",
                    tab.tab_id(),
                );
            }
        }
        attached_tabs.sort_unstable();
        anyhow::ensure!(
            attached_tabs.windows(2).all(|pair| pair[0].0 != pair[1].0),
            "a frozen window topology change cannot attach one tab twice"
        );
        for &(tab_id, window_id) in &attached_tabs {
            let window = windows
                .binary_search_by_key(&window_id, FrozenWindowOrder::window_id)
                .ok()
                .and_then(|index| windows.get(index))
                .ok_or_else(|| {
                    anyhow!(
                        "a frozen window topology change attaches tab {tab_id} to absent window {window_id}"
                    )
                })?;
            anyhow::ensure!(
                window
                    .ordered_tabs()
                    .iter()
                    .any(|tab| tab.tab_id() == tab_id),
                "a frozen window topology change attaches tab {tab_id} without retaining it in window {window_id}"
            );
        }
        removed_windows.sort_unstable();
        created_windows.sort_unstable();
        anyhow::ensure!(
            created_windows.windows(2).all(|pair| pair[0] != pair[1]),
            "a frozen window topology change cannot create one window twice"
        );
        anyhow::ensure!(
            removed_windows.windows(2).all(|pair| pair[0] != pair[1]),
            "a frozen window topology change cannot remove one window twice"
        );
        anyhow::ensure!(
            removed_windows
                .iter()
                .all(|removed| windows.iter().all(|window| window.window_id() != *removed)),
            "a frozen window topology change cannot both retain and remove one window"
        );
        anyhow::ensure!(
            created_windows
                .iter()
                .all(|created| removed_windows.binary_search(created).is_err()),
            "a frozen window topology change cannot both create and remove one window"
        );
        anyhow::ensure!(
            created_windows.iter().all(|created| windows
                .binary_search_by_key(created, FrozenWindowOrder::window_id)
                .is_ok()),
            "a frozen window topology change must retain every created window"
        );
        Ok(Self {
            windows: Arc::from(windows),
            attached_tabs: Arc::from(attached_tabs),
            created_windows: Arc::from(created_windows),
            removed_windows: Arc::from(removed_windows),
        })
    }

    pub fn windows(&self) -> &[FrozenWindowOrder] {
        &self.windows
    }

    /// Return whether this transaction changed or retired `window_id`.
    ///
    /// Retired windows are deliberately affected but absent from
    /// [`Self::windows`], which contains only frozen post-commit survivors.
    pub fn affects_window(&self, window_id: WindowId) -> bool {
        self.windows
            .binary_search_by_key(&window_id, FrozenWindowOrder::window_id)
            .is_ok()
            || self.removed_windows.binary_search(&window_id).is_ok()
    }

    pub fn attached_tabs(&self) -> &[(TabId, WindowId)] {
        &self.attached_tabs
    }

    pub fn removed_windows(&self) -> &[WindowId] {
        &self.removed_windows
    }

    pub fn created_windows(&self) -> &[WindowId] {
        &self.created_windows
    }

    pub fn legacy_resync_tab_id(&self) -> TabId {
        self.windows
            .iter()
            .find_map(FrozenWindowOrder::active_tab_id)
            .unwrap_or(0)
    }
}

#[derive(Clone, Debug)]
pub enum MuxNotification {
    PaneOutput(PaneId),
    SynchronizedOutput {
        pane_id: PaneId,
        event: SynchronizedOutputEvent,
    },
    PaneAdded(PaneId),
    /// One exact floating-pane registration and structural attachment,
    /// frozen at the topology revision carried by the enclosing envelope.
    ///
    /// This replaces the lossy `PaneAdded` + `TabResized` + `PaneFocused`
    /// sequence for mux-owned floating spawns.  Delayed subscribers consume
    /// the exact committed identities and geometry carried here rather than
    /// re-reading a potentially newer numeric pane registration.
    FloatingPaneSpawnCommitted(FrozenFloatingPaneSpawn),
    PaneRemoved(PaneId),
    WindowCreated(WindowId),
    WindowRemoved(WindowId),
    WindowInvalidated(WindowId),
    /// One allocation-prepared mutation of one or more exact mux windows.
    ///
    /// Every included window was committed under the same window-map lock and
    /// the enclosing envelope's single topology revision.  The windows are in
    /// ascending `WindowId` order, so delayed consumers never need to re-read
    /// mutable mux state or infer which half of a cross-window move they saw.
    WindowTopologyChanged(FrozenWindowTopologyChange),
    /// One frozen, pointer-preserving pure-window reorder. The enclosing
    /// envelope carries the single topology revision reserved by the same
    /// transaction; delayed subscribers must consume this state directly and
    /// must not re-read a potentially newer window.
    WindowOrderChanged {
        mutation_id: WindowOrderMutationId,
        request_digest: WindowReorderDigest,
        window: FrozenWindowOrder,
    },
    /// Workspace payload frozen at the same mutation point as its topology
    /// revision; delayed subscribers must never re-read a later window state.
    WindowWorkspaceChanged {
        window_id: WindowId,
        workspace: String,
    },
    ActiveWorkspaceChanged(Arc<ClientId>),
    Alert {
        pane_id: PaneId,
        alert: frankenterm_term::Alert,
    },
    Empty,
    AssignClipboard {
        pane_id: PaneId,
        selection: ClipboardSelection,
        clipboard: Option<String>,
    },
    SaveToDownloads {
        name: Option<String>,
        data: Arc<Vec<u8>>,
    },
    TabAddedToWindow {
        tab_id: TabId,
        window_id: WindowId,
    },
    PaneFocused(PaneId),
    TabResized(TabId),
    TabTitleChanged {
        tab_id: TabId,
        title: String,
    },
    WindowTitleChanged {
        window_id: WindowId,
        title: String,
    },
    WorkspaceRenamed {
        old_workspace: String,
        new_workspace: String,
    },
}

impl MuxNotification {
    /// Whether publishing this variant requires the mux's private pane
    /// lifecycle queue authority.
    ///
    /// A numeric pane ID is not proof that an add/remove transition actually
    /// occurred.  These variants must therefore never enter through the
    /// generic [`Mux::notify`] surface, where an arbitrary caller could forge
    /// topology and cleanup state.
    const fn requires_pane_lifecycle_authority(&self) -> bool {
        matches!(
            self,
            Self::PaneAdded(_) | Self::FloatingPaneSpawnCommitted(_) | Self::PaneRemoved(_)
        )
    }

    /// Whether this notification describes state represented by a mux
    /// topology snapshot and therefore participates in the revision stream.
    pub const fn is_topology(&self) -> bool {
        matches!(
            self,
            Self::PaneAdded(_)
                | Self::FloatingPaneSpawnCommitted(_)
                | Self::PaneRemoved(_)
                | Self::WindowCreated(_)
                | Self::WindowRemoved(_)
                | Self::WindowInvalidated(_)
                | Self::WindowTopologyChanged(_)
                | Self::WindowOrderChanged { .. }
                | Self::WindowWorkspaceChanged { .. }
                | Self::Empty
                | Self::TabAddedToWindow { .. }
                | Self::PaneFocused(_)
                | Self::TabResized(_)
                | Self::TabTitleChanged { .. }
                | Self::WindowTitleChanged { .. }
                | Self::WorkspaceRenamed { .. }
        )
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SynchronizedOutputEvent {
    Depth {
        outcome: SynchronizedOutputDepthOutcome,
        max_depth: u32,
    },
    Admission {
        decision: SynchronizedOutputAdmissionDecision,
        bytes: u64,
    },
    Drain {
        cause: SynchronizedOutputDrainCause,
        bytes: u64,
        depth_outcome: Option<SynchronizedOutputDepthOutcome>,
        max_depth: u32,
    },
    ModeQuery,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SynchronizedOutputDepthOutcome {
    Opened { new_depth: u32 },
    Closed { new_depth: u32 },
    Flushed,
    Underflow,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SynchronizedOutputAdmissionDecision {
    Accepted,
    Truncated { dropped_bytes: u64 },
    Refused,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SynchronizedOutputDrainCause {
    Esu,
    Watchdog,
    LiveResizeForce,
    Operator,
}

static SUB_ID: AtomicUsize = AtomicUsize::new(0);

/// A process-local identifier namespace cannot satisfy a requested reservation.
///
/// `usize::MAX` is an exhausted sentinel, not an identifier that this
/// allocator will ever publish.  Refusing the allocation is essential:
/// saturating at the last value would silently issue the same identifier to
/// multiple live objects.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
#[error(
    "{namespace} identifier space has insufficient remaining capacity for a reservation of {requested} identifier(s); refusing to wrap, saturate, reset, or reuse an identifier"
)]
pub struct IdAllocationError {
    namespace: &'static str,
    requested: usize,
}

impl IdAllocationError {
    pub fn namespace(self) -> &'static str {
        self.namespace
    }

    pub fn requested(self) -> usize {
        self.requested
    }
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
#[error("pane identifier {pane_id} is already registered to a different pane instance")]
pub struct PaneIdCollision {
    pub pane_id: PaneId,
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
#[error(
    "pane identifier {pane_id} is already being prepared for this pane instance; retry after the in-flight registration completes"
)]
pub struct PanePreparationInProgress {
    pub pane_id: PaneId,
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
#[error("pane identifier {pane_id} registration was cancelled before publication")]
pub struct PanePreparationCancelled {
    pub pane_id: PaneId,
}

#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum DomainRegistrationError {
    #[error("domain identifier {domain_id} ({domain_name}) has been retired and cannot be reused")]
    RetiredIdentifier {
        domain_id: DomainId,
        domain_name: String,
    },
    #[error(
        "domain identifier {domain_id} is already registered to {registered_name}; refusing different instance {requested_name}"
    )]
    IdentifierInUse {
        domain_id: DomainId,
        registered_name: String,
        requested_name: String,
    },
    #[error(
        "domain name {domain_name} is already registered to identifier {registered_id}; refusing identifier {requested_id}"
    )]
    NameInUse {
        domain_name: String,
        registered_id: DomainId,
        requested_id: DomainId,
    },
    #[error("domain registry indexes are inconsistent: {detail}")]
    RegistryInconsistent { detail: String },
    #[error(
        "domain identifier {domain_id} ({domain_name}) is not the exact live registration and cannot become default"
    )]
    DefaultNotRegistered {
        domain_id: DomainId,
        domain_name: String,
    },
}

pub(crate) fn try_reserve_usize_ids(
    counter: &AtomicUsize,
    count: usize,
    namespace: &'static str,
) -> Result<std::ops::Range<usize>, IdAllocationError> {
    // The atomic orders only the uniqueness counter. The locks that publish
    // the resulting objects provide the required visibility ordering, so
    // stronger atomic ordering would add coherence cost without correctness.
    let mut current = counter.load(Ordering::Relaxed);
    if count == 0 {
        return Ok(current..current);
    }

    loop {
        let next = current.checked_add(count).ok_or(IdAllocationError {
            namespace,
            requested: count,
        })?;
        match counter.compare_exchange_weak(current, next, Ordering::Relaxed, Ordering::Relaxed) {
            Ok(_) => return Ok(current..next),
            Err(actual) => current = actual,
        }
    }
}

/// Allocate one process-local identifier without ever reusing the terminal
/// value after exhaustion.
///
/// The remaining infallible domain, tab, window, and client constructors
/// cannot propagate [`IdAllocationError`]. Exhaustion is therefore an
/// invariant failure: panicking before publication is strictly safer than
/// returning `usize::MAX` repeatedly and aliasing live mux objects.
#[track_caller]
pub(crate) fn next_unique_usize_id(counter: &AtomicUsize, namespace: &'static str) -> usize {
    let mut current = counter.load(Ordering::Relaxed);
    loop {
        let Some(next) = current.checked_add(1) else {
            panic!(
                "{} identifier space exhausted; refusing to reuse an identifier",
                namespace
            );
        };
        match counter.compare_exchange_weak(current, next, Ordering::Relaxed, Ordering::Relaxed) {
            Ok(_) => return current,
            Err(actual) => current = actual,
        }
    }
}

fn try_increment_atomic_count(counter: &AtomicUsize) -> bool {
    let mut current = counter.load(Ordering::Acquire);
    loop {
        let Some(next) = current.checked_add(1) else {
            return false;
        };
        match counter.compare_exchange_weak(current, next, Ordering::AcqRel, Ordering::Acquire) {
            Ok(_) => return true,
            Err(observed) => current = observed,
        }
    }
}

fn try_decrement_atomic_count(counter: &AtomicUsize) -> bool {
    let mut current = counter.load(Ordering::Acquire);
    loop {
        let Some(next) = current.checked_sub(1) else {
            return false;
        };
        match counter.compare_exchange_weak(current, next, Ordering::AcqRel, Ordering::Acquire) {
            Ok(_) => return true,
            Err(observed) => current = observed,
        }
    }
}

type MuxSubscriber = dyn Fn(MuxNotificationEnvelope) -> bool + Send + Sync;

struct PreparedPaneRegistration {
    pane_id: PaneId,
    reader: Option<Box<dyn std::io::Read + Send>>,
    registration_reservation: pane_registration_handle::PaneRegistrationReservation,
}

/// Process-local, non-forgeable identity for one live registration of a pane.
///
/// Pane identifiers and even the exact same `Arc<dyn Pane>` may be observed
/// again after removal.  Reader tasks therefore carry this capability so that
/// delayed output, startup gates, and EOF cleanup from an earlier registration
/// cannot act on a later registration of the same pane value.
#[derive(Default)]
struct PaneRetirementTracker {
    generations: Mutex<HashMap<PaneId, Arc<PaneRegistrationGeneration>>>,
}

/// Pointer-identity token for one authoritative `PaneRemoved` subscriber
/// fanout.  A GUI cleanup lease retains this exact token so a delayed callback
/// can never release the reuse fence for a later removal of the same numeric
/// pane ID.
#[derive(Debug)]
struct PaneRemovalCleanupToken {
    state: Mutex<PaneRemovalCleanupState>,
    cleanup_complete: Option<Arc<AtomicBool>>,
    created_at: Instant,
}

#[derive(Debug)]
struct PaneRemovalCleanupState {
    accepting_leases: bool,
    leases: usize,
    finalized: bool,
}

impl PaneRemovalCleanupToken {
    fn new(cleanup_complete: Option<Arc<AtomicBool>>) -> Self {
        Self {
            state: Mutex::new(PaneRemovalCleanupState {
                accepting_leases: true,
                leases: 0,
                finalized: false,
            }),
            cleanup_complete,
            created_at: Instant::now(),
        }
    }

    fn try_acquire(&self) -> bool {
        let mut state = self.state.lock();
        if !state.accepting_leases || state.finalized {
            return false;
        }
        let Some(leases) = state.leases.checked_add(1) else {
            return false;
        };
        state.leases = leases;
        true
    }

    fn close(&self) -> bool {
        let mut state = self.state.lock();
        state.accepting_leases = false;
        if state.leases == 0 && !state.finalized {
            state.finalized = true;
            true
        } else {
            false
        }
    }

    fn release(&self) -> Option<bool> {
        let mut state = self.state.lock();
        let leases = state.leases.checked_sub(1)?;
        state.leases = leases;
        if !state.accepting_leases && leases == 0 && !state.finalized {
            state.finalized = true;
            Some(true)
        } else {
            Some(false)
        }
    }

    fn mark_cleanup_complete(&self) {
        if let Some(cleanup_complete) = &self.cleanup_complete {
            cleanup_complete.store(true, Ordering::Release);
        }
    }
}

/// Diagnostic view of deferred `PaneRemoved` cleanup authority.
///
/// This snapshot is deliberately observational: age is never treated as
/// permission to release a fence.  A same-ID pane remains fenced until every
/// exact lease is completed or dropped by its owner.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct PaneRemovalCleanupSnapshot {
    pub active_fences: usize,
    pub outstanding_leases: usize,
    pub oldest_fence_age: Duration,
}

/// Keeps a removed pane's numeric registry slot fenced while a subscriber
/// performs deferred, generation-specific cleanup.
///
/// This lease can be acquired only synchronously from the authoritative
/// `PaneRemoved` subscriber callback. Dropping it acknowledges that the
/// deferred cleanup either ran or was abandoned, allowing same-ID registration
/// once every subscriber lease has completed.
pub struct PaneRemovalCleanupLease {
    owner: Weak<Mux>,
    pane_id: PaneId,
    token: Arc<PaneRemovalCleanupToken>,
    acquired_at: Instant,
    completed: bool,
}

impl std::fmt::Debug for PaneRemovalCleanupLease {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PaneRemovalCleanupLease")
            .field("pane_id", &self.pane_id)
            .field(
                "generation",
                &(Arc::as_ptr(&self.token) as *const () as usize),
            )
            .finish_non_exhaustive()
    }
}

impl Drop for PaneRemovalCleanupLease {
    fn drop(&mut self) {
        histogram!("mux.notifications.pane_removed.deferred_cleanup_ms")
            .record(self.acquired_at.elapsed().as_secs_f64() * 1_000.0);
        if !self.completed {
            metrics::counter!("mux.notifications.pane_removed.deferred_cleanup_abandoned")
                .increment(1);
        }
        let owner = self.owner.upgrade();
        let release = self.token.release();
        if release.is_some() {
            if let Some(owner) = &owner {
                if !try_decrement_atomic_count(&owner.pane_removal_cleanup_outstanding_leases) {
                    log::error!(
                        "PaneRemoved global cleanup observability count underflow for pane {}; exact token release remains authoritative",
                        self.pane_id
                    );
                }
            }
        }
        match release {
            Some(true) => {
                if let Some(owner) = &owner {
                    owner.finalize_pane_removal_cleanup(self.pane_id, &self.token);
                } else {
                    self.token.mark_cleanup_complete();
                }
            }
            Some(false) => {}
            None => {
                log::error!(
                    "PaneRemoved cleanup lease count underflow for pane {}; retaining generation completion fence",
                    self.pane_id
                );
            }
        }
        if let Some(owner) = owner {
            owner.record_pane_removal_cleanup_counts();
        }
    }
}

impl PaneRemovalCleanupLease {
    /// Acknowledge that the deferred subscriber cleanup ran to completion.
    /// Consuming the lease releases its share of the same-ID reuse fence.
    pub fn complete(mut self) {
        self.completed = true;
    }
}

impl PaneRetirementTracker {
    fn retire(&self, generation: &Arc<PaneRegistrationGeneration>) {
        let pane_id = generation.pane_id;
        let mut generations = self.generations.lock();
        generations.insert(pane_id, Arc::clone(generation));
        if pane_registration_is_quiescent(generation.operation_state.load(Ordering::Acquire))
            && generations
                .get(&pane_id)
                .is_some_and(|retired| Arc::ptr_eq(retired, generation))
        {
            generations.remove(&pane_id);
        }
    }

    fn reap(&self, generation: &Arc<PaneRegistrationGeneration>) {
        if !pane_registration_is_quiescent(generation.operation_state.load(Ordering::Acquire)) {
            return;
        }
        let pane_id = generation.pane_id;
        let mut generations = self.generations.lock();
        if generations
            .get(&pane_id)
            .is_some_and(|retired| Arc::ptr_eq(retired, generation))
        {
            generations.remove(&pane_id);
        }
    }

    /// Return whether a retired generation still has a side effect in flight.
    ///
    /// The caller holds `pane_registration`, so a false result is serialized
    /// with publication of the next generation for this pane ID.
    fn has_in_flight_retirement(&self, pane_id: PaneId) -> bool {
        let mut generations = self.generations.lock();
        let Some(generation) = generations.get(&pane_id) else {
            return false;
        };
        if pane_registration_is_quiescent(generation.operation_state.load(Ordering::Acquire)) {
            generations.remove(&pane_id);
            false
        } else {
            true
        }
    }
}

enum PaneRetirementCleanupState {
    Unattached,
    Pending(PaneRetirementCompletion),
    Claimed,
}

struct PaneRetirementCompletion {
    pane_id: PaneId,
    pane: Arc<dyn Pane>,
    kill: bool,
    lifecycle_notification: PaneLifecycleNotificationTicket,
    cleanup_complete: Arc<AtomicBool>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PaneRemovalFollowUp {
    None,
    PruneDeadWindowsIgnoringActivity,
}

#[derive(Clone, Copy)]
enum PaneRetirementExecution {
    Inline,
    MainThreadIfConfigured,
}

/// Process-local, non-forgeable identity for one live registration of a pane.
///
/// The operation counter closes the check-then-act race between a parser and
/// removal. Removal retires the generation without waiting for external pane
/// callbacks; a same-ID registration remains fenced in `PaneRetirementTracker`
/// until every operation that linearized before retirement has returned.
struct PaneRegistrationGeneration {
    pane_id: PaneId,
    operation_state: AtomicUsize,
    reader_dead: Arc<AtomicBool>,
    retirement_tracker: Weak<PaneRetirementTracker>,
    owner: Weak<Mux>,
    cleanup: Mutex<PaneRetirementCleanupState>,
    cleanup_complete: Arc<AtomicBool>,
}

const PANE_REGISTRATION_RETIRED: usize = 1usize << (usize::BITS - 1);
const PANE_REGISTRATION_DEFERRED_RETIREMENT: usize = 1usize << (usize::BITS - 2);
const PANE_REGISTRATION_OPERATION_MASK: usize = PANE_REGISTRATION_DEFERRED_RETIREMENT - 1;

fn pane_registration_is_quiescent(state: usize) -> bool {
    state & PANE_REGISTRATION_RETIRED != 0 && state & PANE_REGISTRATION_OPERATION_MASK == 0
}

impl PaneRegistrationGeneration {
    fn new(
        pane_id: PaneId,
        retirement_tracker: &Arc<PaneRetirementTracker>,
        owner: Weak<Mux>,
    ) -> Arc<Self> {
        Arc::new(Self {
            pane_id,
            operation_state: AtomicUsize::new(0),
            reader_dead: Arc::new(AtomicBool::new(false)),
            retirement_tracker: Arc::downgrade(retirement_tracker),
            owner,
            cleanup: Mutex::new(PaneRetirementCleanupState::Unattached),
            cleanup_complete: Arc::new(AtomicBool::new(false)),
        })
    }

    fn try_acquire(self: &Arc<Self>) -> Option<PaneRegistrationOperationLease> {
        let mut state = self.operation_state.load(Ordering::Acquire);
        loop {
            if state & PANE_REGISTRATION_RETIRED != 0 {
                return None;
            }
            let active = state & PANE_REGISTRATION_OPERATION_MASK;
            let next = active.checked_add(1)?;
            if next > PANE_REGISTRATION_OPERATION_MASK {
                return None;
            }
            match self.operation_state.compare_exchange_weak(
                state,
                next,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    return Some(PaneRegistrationOperationLease {
                        generation: Arc::clone(self),
                    });
                }
                Err(actual) => state = actual,
            }
        }
    }

    fn retire(self: &Arc<Self>) {
        self.reader_dead.store(true, Ordering::Release);
        let mut state = self.operation_state.load(Ordering::Acquire);
        loop {
            if state & PANE_REGISTRATION_RETIRED != 0 {
                break;
            }
            let deferred = if state & PANE_REGISTRATION_OPERATION_MASK == 0 {
                0
            } else {
                PANE_REGISTRATION_DEFERRED_RETIREMENT
            };
            match self.operation_state.compare_exchange_weak(
                state,
                state | PANE_REGISTRATION_RETIRED | deferred,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    if let Some(tracker) = self.retirement_tracker.upgrade() {
                        tracker.retire(self);
                    }
                    break;
                }
                Err(actual) => state = actual,
            }
        }
        self.try_claim_quiescent_cleanup();
    }

    fn release_operation(self: &Arc<Self>) {
        let previous = self.operation_state.fetch_sub(1, Ordering::AcqRel);
        debug_assert!(
            previous & PANE_REGISTRATION_OPERATION_MASK > 0,
            "pane operation count must not underflow"
        );
        if previous & PANE_REGISTRATION_RETIRED != 0
            && previous & PANE_REGISTRATION_OPERATION_MASK == 1
        {
            if let Some(tracker) = self.retirement_tracker.upgrade() {
                tracker.reap(self);
            }
            self.try_claim_quiescent_cleanup();
        }
    }

    fn attach_cleanup(self: &Arc<Self>, completion: PaneRetirementCompletion) {
        {
            let mut cleanup = self.cleanup.lock();
            match &*cleanup {
                PaneRetirementCleanupState::Unattached => {
                    *cleanup = PaneRetirementCleanupState::Pending(completion);
                }
                PaneRetirementCleanupState::Pending(_) | PaneRetirementCleanupState::Claimed => {
                    debug_assert!(false, "pane retirement cleanup may only be attached once");
                    return;
                }
            }
        }
        self.try_claim_quiescent_cleanup();
    }

    fn try_claim_quiescent_cleanup(self: &Arc<Self>) {
        let state = self.operation_state.load(Ordering::Acquire);
        if !pane_registration_is_quiescent(state) {
            return;
        }
        let completion = {
            let mut cleanup = self.cleanup.lock();
            if !pane_registration_is_quiescent(self.operation_state.load(Ordering::Acquire)) {
                return;
            }
            match std::mem::replace(&mut *cleanup, PaneRetirementCleanupState::Claimed) {
                PaneRetirementCleanupState::Pending(completion) => Some(completion),
                state => {
                    *cleanup = state;
                    None
                }
            }
        };
        if let Some(completion) = completion {
            let execution = if state & PANE_REGISTRATION_DEFERRED_RETIREMENT == 0 {
                PaneRetirementExecution::Inline
            } else {
                PaneRetirementExecution::MainThreadIfConfigured
            };
            completion.run(self.owner.clone(), execution);
        }
    }
}

struct PaneRegistrationOperationLease {
    generation: Arc<PaneRegistrationGeneration>,
}

impl Drop for PaneRegistrationOperationLease {
    fn drop(&mut self) {
        self.generation.release_operation();
    }
}

mod pane_registration_handle {
    use super::*;

    /// Process-local authority for one exact pane registration.
    ///
    /// A numeric [`PaneId`] names a reusable slot, not a durable capability.
    /// Deferred work must carry this handle so that a stale callback cannot
    /// re-resolve the slot and act on a later registration.  The handle deliberately
    /// keeps only a weak pane reference and a weak mux owner; retaining queued work
    /// therefore cannot keep either object alive.
    #[derive(Clone)]
    pub struct PaneRegistrationHandle {
        pane: Weak<dyn Pane>,
        generation: Arc<PaneRegistrationGeneration>,
    }

    impl std::fmt::Debug for PaneRegistrationHandle {
        fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter
                .debug_struct("PaneRegistrationHandle")
                .field("pane_id", &self.generation.pane_id)
                .field(
                    "generation",
                    &(Arc::as_ptr(&self.generation) as *const () as usize),
                )
                .finish_non_exhaustive()
        }
    }

    /// Restricted view of one exact live pane registration.
    ///
    /// The underlying mux and pane references are intentionally private.  In
    /// particular, this type has no `Deref`, downcast, or raw getter that could
    /// mint an `Arc<dyn Pane>` and let registration authority escape the
    /// generation lease.
    ///
    /// ```compile_fail
    /// # fn cannot_escape_raw_pane(current: &mux::CurrentPane<'_>) {
    /// current.with_pane(|pane| pane);
    /// # }
    /// ```
    pub struct CurrentPane<'a> {
        owner: &'a Mux,
        pane: &'a Arc<dyn Pane>,
        registration: &'a PaneRegistrationHandle,
        pane_id: PaneId,
    }

    impl CurrentPane<'_> {
        pub fn pane_id(&self) -> PaneId {
            self.pane_id
        }

        pub fn write_all(&self, data: &[u8]) -> std::io::Result<()> {
            self.pane.writer().write_all(data)
        }

        pub fn write_all_and_flush(&self, data: &[u8]) -> std::io::Result<()> {
            let mut writer = self.pane.writer();
            writer.write_all(data)?;
            writer.flush()
        }

        pub fn erase_scrollback(&self, erase_mode: config::keyassignment::ScrollbackEraseMode) {
            self.pane.erase_scrollback(erase_mode);
        }

        pub fn send_paste(&self, text: &str) -> anyhow::Result<()> {
            self.pane.send_paste(text)
        }

        pub fn key_down(
            &self,
            key: frankenterm_term::KeyCode,
            modifiers: frankenterm_term::KeyModifiers,
        ) -> anyhow::Result<()> {
            self.pane.key_down(key, modifiers)
        }

        pub fn key_up(
            &self,
            key: frankenterm_term::KeyCode,
            modifiers: frankenterm_term::KeyModifiers,
        ) -> anyhow::Result<()> {
            self.pane.key_up(key, modifiers)
        }

        pub fn mouse_event(&self, event: frankenterm_term::MouseEvent) -> anyhow::Result<()> {
            self.pane.mouse_event(event)
        }

        pub fn is_mouse_grabbed(&self) -> bool {
            self.pane.is_mouse_grabbed()
        }

        pub fn is_alt_screen_active(&self) -> bool {
            self.pane.is_alt_screen_active()
        }

        pub fn get_dimensions(&self) -> crate::renderable::RenderableDimensions {
            self.pane.get_dimensions()
        }

        pub fn get_tiered_scrollback_status(
            &self,
        ) -> Option<crate::renderable::PaneTieredScrollbackStatus> {
            self.pane.get_tiered_scrollback_status()
        }

        pub fn get_cursor_position(&self) -> crate::renderable::StableCursorPosition {
            self.pane.get_cursor_position()
        }

        pub fn get_title(&self) -> String {
            self.pane.get_title()
        }

        pub fn get_current_working_dir(&self, policy: CachePolicy) -> Option<url::Url> {
            self.pane.get_current_working_dir(policy)
        }

        pub fn get_current_seqno(&self) -> termwiz::surface::SequenceNo {
            self.pane.get_current_seqno()
        }

        pub fn get_changed_since(
            &self,
            range: std::ops::Range<frankenterm_term::StableRowIndex>,
            seqno: termwiz::surface::SequenceNo,
        ) -> rangeset::RangeSet<frankenterm_term::StableRowIndex> {
            self.pane.get_changed_since(range, seqno)
        }

        pub fn get_changed_since_with_source_fence(
            &self,
            range: std::ops::Range<frankenterm_term::StableRowIndex>,
            last_observed_source_end: termwiz::surface::SequenceNo,
        ) -> (
            termwiz::surface::SequenceNo,
            rangeset::RangeSet<frankenterm_term::StableRowIndex>,
        ) {
            self.pane
                .get_changed_since_with_source_fence(range, last_observed_source_end)
        }

        pub fn get_lines(
            &self,
            range: std::ops::Range<frankenterm_term::StableRowIndex>,
        ) -> (
            frankenterm_term::StableRowIndex,
            Vec<termwiz::surface::Line>,
        ) {
            self.pane.get_lines(range)
        }

        pub fn palette(&self) -> frankenterm_term::color::ColorPalette {
            self.pane.palette()
        }

        pub fn semantic_snapshot(
            &self,
        ) -> anyhow::Result<(
            Vec<frankenterm_term::SemanticZone>,
            Vec<String>,
            Option<i32>,
        )> {
            let zones = self.pane.get_semantic_zones()?;
            let zone_texts = zones
                .iter()
                .copied()
                .map(|zone| self.pane.get_text_from_semantic_zone(zone))
                .collect::<anyhow::Result<Vec<_>>>()?;
            let last_exit_code = self.pane.get_semantic_exit_code()?;
            Ok((zones, zone_texts, last_exit_code))
        }

        pub fn set_client_palette(&self, palette: frankenterm_term::color::ColorPalette) {
            match self.pane.get_config() {
                Some(config) => match config.downcast_ref::<config::TermConfig>() {
                    Some(term_config) => term_config.set_client_palette(palette),
                    None => {
                        log::error!(
                            "pane {} does not have TermConfig as its configuration; \
                             ignoring client palette update",
                            self.pane_id,
                        );
                    }
                },
                None => {
                    let config = config::TermConfig::new();
                    config.set_client_palette(palette);
                    self.pane.set_config(Arc::new(config));
                }
            }
            self.owner.notify(MuxNotification::Alert {
                pane_id: self.pane_id,
                alert: Alert::PaletteChanged,
            });
        }

        fn tab_contains_exact_pane(&self, tab: &Tab) -> bool {
            tab.iter_all_panes()
                .iter()
                .any(|candidate| Arc::ptr_eq(candidate, self.pane))
        }

        fn tab_has_exact_tiled_pane(&self, tab: &Tab) -> bool {
            tab.iter_panes_ignoring_zoom()
                .iter()
                .any(|candidate| Arc::ptr_eq(&candidate.pane, self.pane))
        }

        fn tab_has_exact_floating_pane(&self, tab: &Tab) -> bool {
            tab.iter_floating_panes()
                .iter()
                .any(|candidate| Arc::ptr_eq(&candidate.pane, self.pane))
        }

        fn exact_tab(&self, tab_id: TabId) -> anyhow::Result<Arc<Tab>> {
            let tab = self
                .owner
                .get_tab(tab_id)
                .ok_or_else(|| anyhow!("no such tab {tab_id}"))?;
            anyhow::ensure!(
                self.tab_contains_exact_pane(&tab),
                "tab {tab_id} does not contain exact pane registration {}",
                self.pane_id,
            );
            Ok(tab)
        }

        fn containing_exact_tab(&self) -> anyhow::Result<Arc<Tab>> {
            for window_id in self.owner.iter_windows() {
                let Some(window) = self.owner.get_window(window_id) else {
                    continue;
                };
                for tab in window.iter() {
                    if self.tab_contains_exact_pane(tab) {
                        return Ok(Arc::clone(tab));
                    }
                }
            }
            Err(anyhow!(
                "exact pane registration {} is not attached to a tab",
                self.pane_id,
            ))
        }

        fn containing_exact_floating_tab(&self) -> anyhow::Result<Arc<Tab>> {
            for window_id in self.owner.iter_windows() {
                let Some(window) = self.owner.get_window(window_id) else {
                    continue;
                };
                for tab in window.iter() {
                    if self.tab_has_exact_floating_pane(tab) {
                        return Ok(Arc::clone(tab));
                    }
                }
            }
            Err(anyhow!(
                "exact floating pane registration {} is not attached to a tab",
                self.pane_id,
            ))
        }

        pub fn set_zoomed_in_tab(&self, tab_id: TabId, zoomed: bool) -> anyhow::Result<()> {
            let tab = self.exact_tab(tab_id)?;
            match tab.get_zoomed_pane() {
                Some(current) => {
                    let is_zoomed = Arc::ptr_eq(&current, self.pane);
                    if is_zoomed != zoomed {
                        tab.set_zoomed(false);
                        if zoomed {
                            anyhow::ensure!(
                                tab.set_active_pane_for_mux(self.pane, self.owner),
                                "exact pane {} was not accepted as active by tab {tab_id}",
                                self.pane_id,
                            );
                            tab.set_zoomed(true);
                        }
                    }
                }
                None if zoomed => {
                    anyhow::ensure!(
                        tab.set_active_pane_for_mux(self.pane, self.owner),
                        "exact pane {} was not accepted as active by tab {tab_id}",
                        self.pane_id,
                    );
                    tab.set_zoomed(true);
                }
                None => {}
            }
            Ok(())
        }

        pub fn pane_in_direction(
            &self,
            direction: config::keyassignment::PaneDirection,
        ) -> anyhow::Result<Option<PaneId>> {
            let tab = self.containing_exact_tab()?;
            let panes = tab.iter_panes_ignoring_zoom();
            Ok(tab
                .get_pane_direction(direction, true)
                .and_then(|pane_index| panes.get(pane_index))
                .map(|positioned| positioned.pane.pane_id()))
        }

        pub fn activate_pane_direction(
            &self,
            direction: config::keyassignment::PaneDirection,
        ) -> anyhow::Result<()> {
            self.containing_exact_tab()?
                .activate_pane_direction(direction);
            Ok(())
        }

        pub fn resize_in_tab(&self, tab_id: TabId, size: TerminalSize) -> anyhow::Result<()> {
            let tab = self.exact_tab(tab_id)?;
            self.pane.resize(size)?;
            tab.rebuild_splits_sizes_from_contained_panes();
            Ok(())
        }

        pub fn adjust_pane_size(
            &self,
            direction: config::keyassignment::PaneDirection,
            amount: usize,
        ) -> anyhow::Result<()> {
            self.containing_exact_tab()?
                .adjust_pane_size(direction, amount);
            Ok(())
        }

        pub fn create_floating_pane(
            &self,
            tab_id: TabId,
            rect: crate::tab::FloatingPaneRect,
        ) -> anyhow::Result<()> {
            let tab = self
                .owner
                .get_tab(tab_id)
                .ok_or_else(|| anyhow!("no such tab {tab_id}"))?;
            if self.tab_has_exact_floating_pane(&tab) {
                tab.set_floating_pane_rect(self.pane_id, rect);
                tab.set_floating_pane_focus(self.pane_id);
                return Ok(());
            }
            anyhow::ensure!(
                !self.tab_has_exact_tiled_pane(&tab),
                "pane {} is already tiled in tab {tab_id}; floating create expects a detached pane",
                self.pane_id,
            );
            anyhow::ensure!(
                !self.tab_contains_exact_pane(&tab),
                "pane {} is already attached to tab {tab_id}",
                self.pane_id,
            );
            tab.add_floating_pane(Arc::clone(self.pane), rect)?;
            Ok(())
        }

        pub fn move_floating_pane(&self, rect: crate::tab::FloatingPaneRect) -> anyhow::Result<()> {
            self.containing_exact_floating_tab()?
                .set_floating_pane_rect(self.pane_id, rect)
                .ok_or_else(|| anyhow!("floating pane {} not found", self.pane_id))?;
            Ok(())
        }

        pub fn set_floating_pane_z_order(&self, z_order: u32) -> anyhow::Result<()> {
            anyhow::ensure!(
                self.containing_exact_floating_tab()?
                    .set_floating_pane_z_order(self.pane_id, z_order),
                "floating pane {} not found",
                self.pane_id,
            );
            Ok(())
        }

        pub fn set_floating_pane_visible(&self, visible: bool) -> anyhow::Result<()> {
            anyhow::ensure!(
                self.containing_exact_floating_tab()?
                    .set_floating_pane_visible(self.pane_id, visible),
                "floating pane {} not found",
                self.pane_id,
            );
            Ok(())
        }

        pub fn remove_floating_pane(&self) -> anyhow::Result<()> {
            let removed = self
                .containing_exact_floating_tab()?
                .remove_floating_pane(self.pane_id)
                .ok_or_else(|| anyhow!("floating pane {} not found", self.pane_id))?;
            anyhow::ensure!(
                Arc::ptr_eq(&removed, self.pane),
                "floating pane {} changed registration during removal",
                self.pane_id,
            );
            Ok(())
        }

        pub fn update_pane_constraints(
            &self,
            min_width: Option<usize>,
            max_width: Option<usize>,
            min_height: Option<usize>,
            max_height: Option<usize>,
        ) -> anyhow::Result<()> {
            self.containing_exact_tab()?
                .update_pane_constraints(self.pane_id, min_width, max_width, min_height, max_height)
                .ok_or_else(|| anyhow!("pane {} not found in tab", self.pane_id))?;
            Ok(())
        }

        pub fn is_same_pane(&self, pane: &Arc<dyn Pane>) -> bool {
            std::ptr::eq(
                Arc::as_ptr(self.pane) as *const (),
                Arc::as_ptr(pane) as *const (),
            )
        }

        /// Compare this exact registration with a borrowed pane object without
        /// allowing the registered `Arc` to escape its generation lease.
        pub fn is_same_pane_ref(&self, pane: &dyn Pane) -> bool {
            // Compare the allocation address, not the wide-pointer metadata.
            // Separately formed trait-object views of the same allocation may
            // carry equivalent but non-identical vtable pointers.
            std::ptr::eq(
                Arc::as_ptr(self.pane) as *const (),
                pane as *const dyn Pane as *const (),
            )
        }

        pub fn register_domain(
            &self,
            domain: &Arc<dyn Domain>,
        ) -> Result<(), DomainRegistrationError> {
            self.owner.add_domain(domain)
        }

        pub fn dispatch_alert(&self, alert: Alert) {
            match &alert {
                Alert::WindowTitleChanged(title) => {
                    if let Some((_domain_id, window_id, _tab_id)) =
                        self.owner.resolve_pane_id(self.pane_id)
                    {
                        self.owner.set_window_title(window_id, title);
                    }
                }
                Alert::TabTitleChanged(title) => {
                    if let Some((_domain_id, _window_id, tab_id)) =
                        self.owner.resolve_pane_id(self.pane_id)
                    {
                        self.owner
                            .set_tab_title(tab_id, title.as_deref().unwrap_or(""));
                    }
                }
                _ => {}
            }
            self.owner.notify(MuxNotification::Alert {
                pane_id: self.pane_id,
                alert,
            });
        }

        pub fn prune_dead_windows(&self) {
            self.owner.prune_dead_windows();
        }

        pub fn focus_pane_and_containing_tab(&self) -> anyhow::Result<()> {
            self.owner
                .focus_exact_pane_and_containing_tab_registered(self.pane_id, self.pane)
        }

        /// Focus this exact pane and attribute focus to an exact client
        /// registration when one is supplied.
        pub fn focus_for_client_if_same(
            &self,
            client_id: Option<&Arc<ClientId>>,
        ) -> anyhow::Result<()> {
            if let Some(client_id) = client_id {
                if !self.owner.client_registration_is_current(client_id) {
                    anyhow::bail!("client registration is no longer current");
                }
            }
            self.focus_pane_and_containing_tab()?;
            if let Some(client_id) = client_id {
                if self
                    .owner
                    .replace_client_focus_metadata_for_registration_if_same(
                        client_id,
                        self.pane_id,
                        self.registration,
                    )
                    .is_none()
                {
                    metrics::counter!("mux.focus.client_attribution_lost").increment(1);
                    log::warn!(
                        "client registration retired after pane {} focus committed; \
                         skipping stale client attribution",
                        self.pane_id,
                    );
                }
            }
            Ok(())
        }

        pub(super) fn record_focus_for_client(&self, client_id: &Arc<ClientId>) -> bool {
            self.owner.record_focus_for_client_registration_if_same(
                client_id,
                self.registration,
                self.pane.as_ref(),
            )
        }

        pub(super) fn focus_changed(&self, focused: bool) {
            self.pane.focus_changed(focused);
        }

        pub fn record_input_for_current_identity(&self) {
            self.owner.record_input_for_current_identity();
        }

        /// Query whether a pane slot is populated in this handle's exact mux.
        ///
        /// This is intentionally a boolean projection rather than a pane
        /// getter: clients can expire numeric ID mappings without allowing an
        /// `Arc<dyn Pane>` to escape this generation-scoped operation.
        pub fn contains_pane_id(&self, pane_id: PaneId) -> bool {
            self.owner.get_pane(pane_id).is_some()
        }

        /// Query tab membership against this handle's exact mux authority.
        pub fn tab_has_panes_in_domain(&self, tab_id: TabId, domain_id: DomainId) -> bool {
            self.owner
                .get_tab(tab_id)
                .is_some_and(|tab| tab.has_panes_in_domain(domain_id))
        }

        /// Query window membership against this handle's exact mux authority.
        pub fn window_has_panes_in_domain(&self, window_id: WindowId, domain_id: DomainId) -> bool {
            self.owner.window_has_panes_in_domain(window_id, domain_id)
        }

        pub(super) fn assign_clipboard(
            &self,
            selection: ClipboardSelection,
            clipboard: Option<String>,
        ) {
            self.owner.notify(MuxNotification::AssignClipboard {
                pane_id: self.pane_id(),
                selection,
                clipboard,
            });
        }

        pub(super) fn save_to_downloads(&self, name: Option<String>, data: Vec<u8>) {
            self.owner.notify(MuxNotification::SaveToDownloads {
                name,
                data: Arc::new(data),
            });
        }
    }

    /// Restricted output-mutation view of one exact live pane registration.
    ///
    /// Construction also reserves the pane's ordered output continuation.  The
    /// continuation is finished before the registration lease is released, even
    /// when the caller unwinds.
    pub struct CurrentPaneOutput<'a> {
        current: CurrentPane<'a>,
    }

    impl CurrentPaneOutput<'_> {
        pub fn pane_id(&self) -> PaneId {
            self.current.pane_id()
        }

        pub fn is_same_pane_ref(&self, pane: &dyn Pane) -> bool {
            self.current.is_same_pane_ref(pane)
        }

        pub fn perform_actions(&self, actions: Vec<Action>) {
            self.current.pane.perform_actions(actions);
        }

        /// Dispatch an alert while retaining the same exact-generation output
        /// authority that ordered the pane state mutation which produced it.
        pub fn dispatch_alert(&self, alert: Alert) {
            self.current.dispatch_alert(alert);
        }
    }

    /// Non-cloneable authority for one admitted pane operation.
    ///
    /// Unlike [`PaneRegistrationHandle`], this guard owns the exact pane and
    /// mux for the complete operation and holds the generation lease until it
    /// is dropped.  Retirement may remove the pane's numeric registry slot
    /// after admission, but it cannot redirect this guard to a successor or
    /// finish exact-generation cleanup while the operation is in flight.
    pub struct PaneOperationGuard {
        owner: Arc<Mux>,
        pane: Arc<dyn Pane>,
        generation: Arc<PaneRegistrationGeneration>,
        retirement: Arc<PaneRetirementTracker>,
        _operation: PaneRegistrationOperationLease,
    }

    impl std::fmt::Debug for PaneOperationGuard {
        fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter
                .debug_struct("PaneOperationGuard")
                .field("pane_id", &self.generation.pane_id)
                .field(
                    "generation",
                    &(Arc::as_ptr(&self.generation) as *const () as usize),
                )
                .finish_non_exhaustive()
        }
    }

    impl PaneOperationGuard {
        pub fn pane_id(&self) -> PaneId {
            self.generation.pane_id
        }

        pub fn same_registration(&self, other: &Self) -> bool {
            Arc::ptr_eq(&self.owner, &other.owner)
                && Arc::ptr_eq(&self.pane, &other.pane)
                && Arc::ptr_eq(&self.generation, &other.generation)
                && Arc::ptr_eq(&self.retirement, &other.retirement)
        }

        pub fn belongs_to(&self, owner: &Arc<Mux>) -> bool {
            Arc::ptr_eq(&self.owner, owner)
        }

        pub fn is_same_pane(&self, pane: &Arc<dyn Pane>) -> bool {
            Arc::ptr_eq(&self.pane, pane)
        }

        /// Borrow the exact pane only for the duration of `f`.
        ///
        /// The closure cannot retain the pane `Arc`, so exact authority remains
        /// scoped to this non-cloneable guard.
        pub fn with_pane<R>(&self, f: impl FnOnce(&dyn Pane) -> R) -> R {
            f(self.pane.as_ref())
        }

        pub fn registration(&self) -> PaneRegistrationHandle {
            PaneRegistrationHandle::new(&self.pane, &self.generation)
        }

        pub(crate) fn owner(&self) -> &Arc<Mux> {
            &self.owner
        }

        pub(crate) fn pane(&self) -> &Arc<dyn Pane> {
            &self.pane
        }

        pub(crate) fn exact_location(&self) -> anyhow::Result<(DomainId, WindowId, Arc<Tab>)> {
            let domain_id = self.pane.domain_id();
            for window_id in self.owner.iter_windows() {
                let Some(window) = self.owner.get_window(window_id) else {
                    continue;
                };
                for tab in window.iter() {
                    if tab
                        .iter_all_panes()
                        .iter()
                        .any(|candidate| Arc::ptr_eq(candidate, &self.pane))
                    {
                        return Ok((domain_id, window_id, Arc::clone(tab)));
                    }
                }
            }
            Err(anyhow!(
                "exact pane registration {} is not attached to a tab",
                self.pane_id()
            ))
        }

        pub fn capture_split_receipt(
            &self,
            pane: Arc<dyn Pane>,
            tab: Arc<Tab>,
            window_id: WindowId,
            size: TerminalSize,
        ) -> anyhow::Result<SplitCommitReceipt> {
            anyhow::ensure!(
                self.owner.get_window(window_id).is_some_and(|window| window
                    .iter()
                    .any(|candidate| Arc::ptr_eq(candidate, &tab))),
                "window {window_id} does not contain exact split tab {}",
                tab.tab_id()
            );
            let panes = tab.iter_all_panes();
            anyhow::ensure!(
                panes
                    .iter()
                    .any(|candidate| Arc::ptr_eq(candidate, &self.pane)),
                "split tab {} no longer contains exact target registration {}",
                tab.tab_id(),
                self.pane_id()
            );
            anyhow::ensure!(
                panes.iter().any(|candidate| Arc::ptr_eq(candidate, &pane)),
                "split tab {} does not contain exact committed pane {}",
                tab.tab_id(),
                pane.pane_id()
            );
            let registration = self.owner.capture_pane_registration(&pane).ok_or_else(|| {
                anyhow!(
                    "committed split pane {} has no exact mux registration",
                    pane.pane_id()
                )
            })?;
            Ok(SplitCommitReceipt::from_exact_parts(
                pane,
                registration,
                tab,
                window_id,
                size,
            ))
        }

        pub fn capture_move_receipt(
            &self,
            tab: Arc<Tab>,
            window_id: WindowId,
        ) -> anyhow::Result<MoveCommitReceipt> {
            anyhow::ensure!(
                self.owner.get_window(window_id).is_some_and(|window| window
                    .iter()
                    .any(|candidate| Arc::ptr_eq(candidate, &tab))),
                "window {window_id} does not contain exact moved tab {}",
                tab.tab_id()
            );
            anyhow::ensure!(
                tab.iter_all_panes()
                    .iter()
                    .any(|candidate| Arc::ptr_eq(candidate, &self.pane)),
                "moved tab {} does not contain exact pane registration {}",
                tab.tab_id(),
                self.pane_id()
            );
            let size = tab.get_size();
            Ok(MoveCommitReceipt::from_exact_parts(
                Arc::clone(&self.pane),
                self.registration(),
                tab,
                window_id,
                size,
            ))
        }
    }

    /// Admission authority for one floating-pane spawn.
    ///
    /// The exact destination tab and its active pane generation are captured
    /// together before any domain attach or spawn await.  The handle does not
    /// retain an operation lease across slow domain work: final commit
    /// reacquires exact-generation authority, so closing the target can retire
    /// promptly rather than waiting for a process spawn to finish.
    pub struct FloatingSpawnTarget {
        target: PaneRegistrationHandle,
        tab: Arc<Tab>,
        window_id: WindowId,
    }

    impl std::fmt::Debug for FloatingSpawnTarget {
        fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter
                .debug_struct("FloatingSpawnTarget")
                .field("target_pane_id", &self.target.pane_id())
                .field("tab_id", &self.tab.tab_id())
                .field("window_id", &self.window_id)
                .finish_non_exhaustive()
        }
    }

    impl FloatingSpawnTarget {
        pub(crate) fn from_exact_parts(
            target: PaneRegistrationHandle,
            tab: Arc<Tab>,
            window_id: WindowId,
        ) -> Self {
            Self {
                target,
                tab,
                window_id,
            }
        }

        pub fn target_pane_id(&self) -> PaneId {
            self.target.pane_id()
        }

        pub fn tab_id(&self) -> TabId {
            self.tab.tab_id()
        }

        pub fn window_id(&self) -> WindowId {
            self.window_id
        }

        pub(crate) fn belongs_to(&self, mux: &Arc<Mux>) -> bool {
            self.target
                .owner()
                .is_some_and(|owner| Arc::ptr_eq(&owner, mux))
        }

        pub(crate) fn tab_arc(&self) -> Arc<Tab> {
            Arc::clone(&self.tab)
        }

        pub(crate) fn target(&self) -> &PaneRegistrationHandle {
            &self.target
        }
    }

    /// Compact immutable result carried by the single floating-spawn topology
    /// event.
    ///
    /// The payload deliberately retains no pane, tab, registration, or mux
    /// allocation. A delayed/backpressured subscriber therefore cannot extend
    /// process lifetime or pin a retired topology generation. Local consumers
    /// use it for invalidation only; exact authority remains in the commit
    /// receipt and pane lifecycle queue.
    #[derive(Clone, Copy, Debug, PartialEq)]
    pub struct FrozenFloatingPaneSpawn {
        pane_id: PaneId,
        tab_id: TabId,
        window_id: WindowId,
        rect: crate::tab::FloatingPaneRect,
        z_order: u32,
        visible: bool,
        pinned: bool,
        opacity: f32,
        focused: bool,
    }

    impl FrozenFloatingPaneSpawn {
        pub(crate) fn from_exact_parts(
            registration: PaneRegistrationHandle,
            tab_id: TabId,
            window_id: WindowId,
            positioned: &crate::tab::PositionedFloatingPane,
        ) -> Self {
            debug_assert_eq!(positioned.pane_id, registration.pane_id());
            Self {
                pane_id: registration.pane_id(),
                tab_id,
                window_id,
                rect: crate::tab::FloatingPaneRect {
                    left: positioned.left,
                    top: positioned.top,
                    width: positioned.width,
                    height: positioned.height,
                },
                z_order: positioned.z_order,
                visible: positioned.visible,
                pinned: positioned.pinned,
                opacity: positioned.opacity,
                focused: positioned.is_focused,
            }
        }

        pub fn pane_id(&self) -> PaneId {
            self.pane_id
        }

        pub fn tab_id(&self) -> TabId {
            self.tab_id
        }

        pub fn window_id(&self) -> WindowId {
            self.window_id
        }

        pub fn rect(&self) -> crate::tab::FloatingPaneRect {
            self.rect
        }

        pub fn z_order(&self) -> u32 {
            self.z_order
        }

        pub fn visible(&self) -> bool {
            self.visible
        }

        pub fn pinned(&self) -> bool {
            self.pinned
        }

        pub fn opacity(&self) -> f32 {
            self.opacity
        }

        pub fn focused(&self) -> bool {
            self.focused
        }
    }

    /// Exact local result of a committed floating-pane spawn.
    pub struct FloatingPaneCommitReceipt {
        pane: Arc<dyn Pane>,
        registration: PaneRegistrationHandle,
        tab: Arc<Tab>,
        window_id: WindowId,
        rect: crate::tab::FloatingPaneRect,
        z_order: u32,
        focused: bool,
    }

    impl std::fmt::Debug for FloatingPaneCommitReceipt {
        fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter
                .debug_struct("FloatingPaneCommitReceipt")
                .field("pane_id", &self.pane_id())
                .field("tab_id", &self.tab_id())
                .field("window_id", &self.window_id)
                .field("rect", &self.rect)
                .field("z_order", &self.z_order)
                .field("focused", &self.focused)
                .finish_non_exhaustive()
        }
    }

    impl FloatingPaneCommitReceipt {
        pub(crate) fn from_exact_parts(
            pane: Arc<dyn Pane>,
            registration: PaneRegistrationHandle,
            tab: Arc<Tab>,
            window_id: WindowId,
            positioned: crate::tab::PositionedFloatingPane,
        ) -> Self {
            debug_assert_eq!(positioned.pane_id, registration.pane_id());
            debug_assert!(Arc::ptr_eq(&pane, &positioned.pane));
            Self {
                pane,
                registration,
                tab,
                window_id,
                rect: crate::tab::FloatingPaneRect {
                    left: positioned.left,
                    top: positioned.top,
                    width: positioned.width,
                    height: positioned.height,
                },
                z_order: positioned.z_order,
                focused: positioned.is_focused,
            }
        }

        pub fn pane_id(&self) -> PaneId {
            self.registration.pane_id()
        }

        pub fn tab_id(&self) -> TabId {
            self.tab.tab_id()
        }

        pub fn window_id(&self) -> WindowId {
            self.window_id
        }

        pub fn rect(&self) -> crate::tab::FloatingPaneRect {
            self.rect
        }

        pub fn z_order(&self) -> u32 {
            self.z_order
        }

        pub fn is_focused(&self) -> bool {
            self.focused
        }

        pub fn registration(&self) -> PaneRegistrationHandle {
            self.registration.clone()
        }

        pub fn with_pane<R>(&self, f: impl FnOnce(&dyn Pane) -> R) -> R {
            f(self.pane.as_ref())
        }
    }

    /// Exact local result of a committed split.
    pub struct SplitCommitReceipt {
        pane: Arc<dyn Pane>,
        registration: PaneRegistrationHandle,
        tab: Arc<Tab>,
        window_id: WindowId,
        size: TerminalSize,
    }

    impl std::fmt::Debug for SplitCommitReceipt {
        fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter
                .debug_struct("SplitCommitReceipt")
                .field("pane_id", &self.pane_id())
                .field("tab_id", &self.tab_id())
                .field("window_id", &self.window_id)
                .field("size", &self.size)
                .finish_non_exhaustive()
        }
    }

    impl SplitCommitReceipt {
        pub(crate) fn from_exact_parts(
            pane: Arc<dyn Pane>,
            registration: PaneRegistrationHandle,
            tab: Arc<Tab>,
            window_id: WindowId,
            size: TerminalSize,
        ) -> Self {
            debug_assert!(registration.is_same_pane(&pane));
            Self {
                pane,
                registration,
                tab,
                window_id,
                size,
            }
        }

        pub fn pane_id(&self) -> PaneId {
            self.registration.pane_id()
        }

        pub fn tab_id(&self) -> TabId {
            self.tab.tab_id()
        }

        pub fn window_id(&self) -> WindowId {
            self.window_id
        }

        pub fn size(&self) -> TerminalSize {
            self.size
        }

        pub fn registration(&self) -> PaneRegistrationHandle {
            self.registration.clone()
        }

        pub fn with_pane<R>(&self, f: impl FnOnce(&dyn Pane) -> R) -> R {
            f(self.pane.as_ref())
        }

        pub(crate) fn into_legacy_parts(self) -> (Arc<dyn Pane>, TerminalSize, WindowId, TabId) {
            (self.pane, self.size, self.window_id, self.tab.tab_id())
        }
    }

    /// Exact local result of a committed move to a tab.
    pub struct MoveCommitReceipt {
        pane: Arc<dyn Pane>,
        registration: PaneRegistrationHandle,
        tab: Arc<Tab>,
        window_id: WindowId,
        size: TerminalSize,
    }

    impl std::fmt::Debug for MoveCommitReceipt {
        fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter
                .debug_struct("MoveCommitReceipt")
                .field("pane_id", &self.pane_id())
                .field("tab_id", &self.tab_id())
                .field("window_id", &self.window_id)
                .field("size", &self.size)
                .finish_non_exhaustive()
        }
    }

    impl MoveCommitReceipt {
        pub(crate) fn from_exact_parts(
            pane: Arc<dyn Pane>,
            registration: PaneRegistrationHandle,
            tab: Arc<Tab>,
            window_id: WindowId,
            size: TerminalSize,
        ) -> Self {
            debug_assert!(registration.is_same_pane(&pane));
            Self {
                pane,
                registration,
                tab,
                window_id,
                size,
            }
        }

        pub fn pane_id(&self) -> PaneId {
            self.registration.pane_id()
        }

        pub fn tab_id(&self) -> TabId {
            self.tab.tab_id()
        }

        pub fn window_id(&self) -> WindowId {
            self.window_id
        }

        pub fn size(&self) -> TerminalSize {
            self.size
        }

        pub fn registration(&self) -> PaneRegistrationHandle {
            self.registration.clone()
        }

        pub fn with_pane<R>(&self, f: impl FnOnce(&dyn Pane) -> R) -> R {
            f(self.pane.as_ref())
        }

        pub(crate) fn into_legacy_parts(self) -> (Arc<Tab>, WindowId) {
            (self.tab, self.window_id)
        }
    }

    impl PaneRegistrationHandle {
        pub(super) fn new(
            pane: &Arc<dyn Pane>,
            generation: &Arc<PaneRegistrationGeneration>,
        ) -> Self {
            Self {
                pane: Arc::downgrade(pane),
                generation: Arc::clone(generation),
            }
        }

        pub fn pane_id(&self) -> PaneId {
            self.generation.pane_id
        }

        pub fn same_registration(&self, other: &Self) -> bool {
            Weak::ptr_eq(&self.pane, &other.pane)
                && Arc::ptr_eq(&self.generation, &other.generation)
        }

        /// Compare one exact pane allocation without acquiring a retirement
        /// lease. Internal transactions use this only for preflight and then
        /// revalidate the generation while holding `pane_registration`.
        pub(crate) fn is_same_pane(&self, pane: &Arc<dyn Pane>) -> bool {
            Weak::ptr_eq(&self.pane, &Arc::downgrade(pane))
        }

        /// Validate this exact weak pane/generation against a live registry
        /// entry. The caller must hold the owning mux's registration serializer.
        pub(crate) fn matches_live_registration(&self, registered: &LivePaneRegistration) -> bool {
            Weak::ptr_eq(&self.pane, &Arc::downgrade(&registered.pane))
                && Arc::ptr_eq(&self.generation, &registered.generation)
        }

        /// Acquire a callback-free retirement fence for a cached location.
        /// The cache's topology revision separately validates structural
        /// ownership. Keeping this lease through the final registry/revision
        /// checks closes the check-then-retire race without taking the global
        /// pane-registration serializer on every stable key event.
        pub(super) fn cached_location_lease_for_owner(
            &self,
            expected_owner: &Mux,
        ) -> Option<PaneRegistrationOperationLease> {
            let lease = self.generation.try_acquire()?;
            let Some(owner) = self.generation.owner.upgrade() else {
                return None;
            };
            if !std::ptr::eq(owner.as_ref(), expected_owner) || self.pane.upgrade().is_none() {
                return None;
            }
            Some(lease)
        }

        /// Acquire non-cloneable exact authority for a complete pane operation.
        pub fn operation_guard(&self, expected_owner: &Arc<Mux>) -> Option<PaneOperationGuard> {
            let operation = self.generation.try_acquire()?;
            let pane = self.pane.upgrade()?;
            let owner = self.generation.owner.upgrade()?;
            if !Arc::ptr_eq(&owner, expected_owner) {
                return None;
            }
            let retirement = self.generation.retirement_tracker.upgrade()?;
            if !Arc::ptr_eq(&retirement, &owner.pane_retirements) {
                return None;
            }
            let is_current = {
                let _registration = owner.pane_registration.lock();
                owner
                    .panes
                    .read()
                    .get(&self.generation.pane_id)
                    .is_some_and(|registered| {
                        Arc::ptr_eq(&registered.pane, &pane)
                            && Arc::ptr_eq(&registered.generation, &self.generation)
                    })
            };
            is_current.then_some(PaneOperationGuard {
                owner,
                pane,
                generation: Arc::clone(&self.generation),
                retirement,
                _operation: operation,
            })
        }

        /// Resolve the exact owner for mux-internal topology transactions.
        ///
        /// This remains crate-private so external consumers cannot bypass the
        /// closure-only authority surface or retain raw mux authority.
        pub(crate) fn owner(&self) -> Option<Arc<Mux>> {
            self.generation.owner.upgrade()
        }

        fn is_rebindable_retirement(&self) -> bool {
            pane_registration_is_quiescent(self.generation.operation_state.load(Ordering::Acquire))
                && self.generation.cleanup_complete.load(Ordering::Acquire)
        }

        pub(crate) fn guards_detached_topology(
            &self,
            expected_owner: &Mux,
            expected_pane: &Arc<dyn Pane>,
        ) -> bool {
            let state = self.generation.operation_state.load(Ordering::Acquire);
            if state & PANE_REGISTRATION_RETIRED == 0
                || state & PANE_REGISTRATION_OPERATION_MASK == 0
            {
                return false;
            }
            let Some(pane) = self.pane.upgrade() else {
                return false;
            };
            if !Arc::ptr_eq(&pane, expected_pane) {
                return false;
            }
            self.generation
                .owner
                .upgrade()
                .is_some_and(|owner| std::ptr::eq(owner.as_ref(), expected_owner))
        }

        #[cfg(test)]
        pub(super) fn active_operation_count(&self) -> usize {
            self.generation.operation_state.load(Ordering::Acquire)
                & PANE_REGISTRATION_OPERATION_MASK
        }

        /// Run `f` only if this exact owner, pane, and generation still has
        /// authority.
        ///
        /// Admission is serialized with retirement.  Registry locks are released
        /// before calling external code, while the generation operation lease
        /// remains live until `f` returns (or unwinds).
        pub fn try_with_current<R>(&self, f: impl FnOnce(CurrentPane<'_>) -> R) -> Option<R> {
            let (operation, pane, mux) = self.resolve_current()?;
            let result = f(CurrentPane {
                owner: mux.as_ref(),
                pane: &pane,
                registration: self,
                pane_id: self.generation.pane_id,
            });
            drop(operation);
            Some(result)
        }

        /// Search only this exact pane registration.
        ///
        /// The generation operation lease remains held across the search so a
        /// same-ID replacement cannot satisfy a request admitted for this pane.
        /// Dropping the future releases the lease through ordinary RAII.
        pub async fn search_if_current(
            &self,
            expected_owner: &Arc<Mux>,
            pattern: crate::pane::Pattern,
            range: std::ops::Range<frankenterm_term::StableRowIndex>,
            limit: Option<u32>,
        ) -> Option<anyhow::Result<Vec<crate::pane::SearchResult>>> {
            let (operation, pane, mux) = self.resolve_current()?;
            if !Arc::ptr_eq(&mux, expected_owner) {
                return None;
            }
            let result = pane.search(pattern, range, limit).await;
            drop(mux);
            drop(operation);
            Some(result)
        }

        /// Spawn a split beside this exact pane registration.
        pub async fn split_spawned_if_current(
            &self,
            expected_owner: &Arc<Mux>,
            request: SplitRequest,
            command: Option<CommandBuilder>,
            command_dir: Option<String>,
            domain: config::keyassignment::SpawnTabDomain,
            client_id: Option<Arc<ClientId>>,
        ) -> Option<anyhow::Result<SplitCommitReceipt>> {
            let target = self.operation_guard(expected_owner)?;
            Some(
                expected_owner
                    .split_pane_spawned(target, request, command, command_dir, domain, client_id)
                    .await,
            )
        }

        /// Move another exact pane registration into a split beside this one.
        pub async fn split_moved_if_current(
            &self,
            expected_owner: &Arc<Mux>,
            source_registration: &Self,
            request: SplitRequest,
            domain: config::keyassignment::SpawnTabDomain,
            client_id: Option<Arc<ClientId>>,
        ) -> Option<anyhow::Result<SplitCommitReceipt>> {
            let target = self.operation_guard(expected_owner)?;
            let source = match source_registration.operation_guard(expected_owner) {
                Some(source) => source,
                None => {
                    return Some(Err(anyhow!(
                        "split source pane registration {} is no longer current",
                        source_registration.pane_id()
                    )));
                }
            };
            Some(
                expected_owner
                    .split_pane_moved(target, source, request, domain, client_id)
                    .await,
            )
        }

        /// Move only this exact pane registration to a new tab.
        pub async fn move_to_new_tab_if_current(
            &self,
            expected_owner: &Arc<Mux>,
            window_id: Option<WindowId>,
            workspace_for_new_window: Option<String>,
            client_id: Option<Arc<ClientId>>,
        ) -> Option<anyhow::Result<MoveCommitReceipt>> {
            let target = self.operation_guard(expected_owner)?;
            Some(
                expected_owner
                    .move_pane_to_new_tab_guarded(
                        target,
                        window_id,
                        workspace_for_new_window,
                        client_id,
                    )
                    .await,
            )
        }

        /// Reserve exact-generation output authority before mutating the pane.
        ///
        /// The output continuation is established before `f` runs, so accepted
        /// state mutation is ordered before a racing `PaneRemoved`.  Its drop path
        /// finishes the producer and releases the operation lease even if `f`
        /// panics.
        pub fn try_with_current_output<R>(
            &self,
            f: impl FnOnce(CurrentPaneOutput<'_>) -> R,
        ) -> Option<R> {
            let pane = self.pane.upgrade()?;
            let mux = self.generation.owner.upgrade()?;
            let output = mux.reserve_pane_output_for_reader(
                &pane,
                &self.generation,
                promise::spawn::is_scheduler_configured(),
            )?;
            let result = f(CurrentPaneOutput {
                current: CurrentPane {
                    owner: mux.as_ref(),
                    pane: &pane,
                    registration: self,
                    pane_id: self.generation.pane_id,
                },
            });
            output.finish();
            Some(result)
        }

        /// Remove and kill only the registration represented by this handle.
        ///
        /// Callers must not hold mux topology or tab locks; pane cleanup and
        /// lifecycle subscribers may re-enter those surfaces synchronously.
        pub fn retire_if_current(&self) -> bool {
            let Some(owner) =
                self.remove_if_current_without_recompute(true, PaneRemovalFollowUp::None)
            else {
                return false;
            };
            owner.recompute_pane_count();
            true
        }

        /// Retire this exact registration and force its dead tab/window sweep
        /// after `Pane::kill`, even while an unrelated activity token is live.
        ///
        /// TermWiz applets run under an activity token, so ordinary pruning is
        /// intentionally suppressed while they are active. The follow-up runs
        /// before the generation's replacement fence is released.
        pub fn retire_and_prune_if_current(&self) -> bool {
            let Some(owner) = self.remove_if_current_without_recompute(
                true,
                PaneRemovalFollowUp::PruneDeadWindowsIgnoringActivity,
            ) else {
                return false;
            };
            owner.recompute_pane_count();
            true
        }

        /// Remove only the local registration represented by this handle.
        ///
        /// Callers must not hold mux topology or tab locks.
        pub fn detach_local_if_current(&self) -> bool {
            let Some(owner) =
                self.remove_if_current_without_recompute(false, PaneRemovalFollowUp::None)
            else {
                return false;
            };
            owner.recompute_pane_count();
            true
        }

        pub(crate) fn retire_batch_if_current(registrations: Vec<Self>) -> usize {
            struct Candidate {
                pane: Arc<dyn Pane>,
                generation: Arc<PaneRegistrationGeneration>,
                operation: PaneRegistrationOperationLease,
            }

            struct OwnerBatch {
                owner: Arc<Mux>,
                candidates: Vec<Candidate>,
            }

            let mut owners = Vec::<OwnerBatch>::new();
            for registration in registrations {
                let Some(operation) = registration.generation.try_acquire() else {
                    continue;
                };
                let Some(pane) = registration.pane.upgrade() else {
                    continue;
                };
                let Some(owner) = registration.generation.owner.upgrade() else {
                    continue;
                };
                let candidate = Candidate {
                    pane,
                    generation: registration.generation,
                    operation,
                };
                if let Some(batch) = owners
                    .iter_mut()
                    .find(|batch| Arc::ptr_eq(&batch.owner, &owner))
                {
                    batch.candidates.push(candidate);
                } else {
                    owners.push(OwnerBatch {
                        owner,
                        candidates: vec![candidate],
                    });
                }
            }

            let mut retired = 0;
            for OwnerBatch { owner, candidates } in owners {
                let (removed, rejected, output_batches) = {
                    let _registration = owner.pane_registration.lock();
                    let (removed, rejected) = {
                        let mut panes = owner.panes.write();
                        let mut removed = Vec::new();
                        let mut rejected = Vec::new();
                        for candidate in candidates {
                            let pane_id = candidate.generation.pane_id;
                            let matches = panes.get(&pane_id).is_some_and(|registered| {
                                Arc::ptr_eq(&registered.pane, &candidate.pane)
                                    && Arc::ptr_eq(&registered.generation, &candidate.generation)
                            });
                            if !matches {
                                rejected.push(candidate);
                                continue;
                            }
                            let registered = panes
                                .remove(&pane_id)
                                .expect("exact batch candidate was just validated");
                            let pane = Arc::clone(&registered.pane);
                            let generation = Arc::clone(&registered.generation);
                            drop(registered);
                            removed.push((pane_id, pane, generation, candidate.operation));
                        }
                        (removed, rejected)
                    };

                    {
                        let mut retiring = owner.retiring_pane_ids.lock();
                        for (pane_id, _, _, _) in &removed {
                            retiring.insert(*pane_id);
                        }
                    }

                    let output_batches = {
                        let mut pending = owner.pending_pane_output.lock();
                        removed
                            .iter()
                            .filter_map(|(pane_id, _, generation, _)| {
                                let matches = pending.queued.get(pane_id).is_some_and(|batch| {
                                    Arc::ptr_eq(&batch.generation, generation)
                                });
                                matches.then(|| pending.queued.remove(pane_id)).flatten()
                            })
                            .collect::<Vec<_>>()
                    };

                    let removal_topology = removed
                        .iter()
                        .map(|(pane_id, _, _, _)| {
                            owner
                                .envelope_notification(MuxNotification::PaneRemoved(*pane_id))
                                .topology
                        })
                        .collect::<Vec<_>>();
                    let tickets = {
                        let mut pending = owner.pending_pane_lifecycle.lock();
                        removed
                            .iter()
                            .zip(removal_topology)
                            .map(|((pane_id, _, generation, _), topology)| {
                                let ready = Arc::new(AtomicBool::new(false));
                                pending.by_pane.entry(*pane_id).or_default().push_back(
                                    PendingPaneLifecycleNotification {
                                        notification: PaneLifecycleNotification::Removed(*pane_id),
                                        topology,
                                        ready: Arc::clone(&ready),
                                        reader_start_gate: None,
                                        cleanup_complete: Some(Arc::clone(
                                            &generation.cleanup_complete,
                                        )),
                                        removal_follow_up: PaneRemovalFollowUp::None,
                                    },
                                );
                                PaneLifecycleNotificationTicket {
                                    pane_id: *pane_id,
                                    ready,
                                }
                            })
                            .collect::<Vec<_>>()
                    };

                    let removed = removed
                        .into_iter()
                        .zip(tickets)
                        .map(
                            |((pane_id, pane, generation, operation), lifecycle_notification)| {
                                (
                                    RemovedPaneRegistration {
                                        pane_id,
                                        pane,
                                        generation,
                                        lifecycle_notification,
                                    },
                                    operation,
                                )
                            },
                        )
                        .collect::<Vec<_>>();
                    (removed, rejected, output_batches)
                };

                // Rejected candidates may own the final operation lease of an
                // already-retired generation. Dropping them can synchronously
                // run pane cleanup and lifecycle subscribers, so it must happen
                // after every registry and topology guard has been released.
                drop(rejected);
                for output_batch in output_batches {
                    histogram!("mux.notifications.pane_output.removal_forced_seal_rate").record(1.);
                    output_batch.seal();
                }
                let pane_ids = removed
                    .iter()
                    .map(|(removed, _)| removed.pane_id)
                    .collect::<Vec<_>>();
                owner.discard_removed_pane_states(&pane_ids);
                retired += removed.len();
                for (removed, operation) in removed {
                    owner.finish_pane_removal(removed, true);
                    drop(operation);
                }
                owner.recompute_pane_count();
            }
            retired
        }

        fn resolve_current(
            &self,
        ) -> Option<(PaneRegistrationOperationLease, Arc<dyn Pane>, Arc<Mux>)> {
            let operation = self.generation.try_acquire()?;
            let pane = self.pane.upgrade()?;
            let mux = self.generation.owner.upgrade()?;
            let is_current = {
                let _registration = mux.pane_registration.lock();
                mux.panes
                    .read()
                    .get(&self.generation.pane_id)
                    .is_some_and(|registered| {
                        Arc::ptr_eq(&registered.pane, &pane)
                            && Arc::ptr_eq(&registered.generation, &self.generation)
                    })
            };
            is_current.then_some((operation, pane, mux))
        }

        fn remove_if_current_without_recompute(
            &self,
            kill: bool,
            removal_follow_up: PaneRemovalFollowUp,
        ) -> Option<Arc<Mux>> {
            let operation = self.generation.try_acquire()?;
            let pane = self.pane.upgrade()?;
            let mux = self.generation.owner.upgrade()?;
            let removed = mux.take_pane_for_removal(
                self.generation.pane_id,
                Some(&pane),
                Some(&self.generation),
                removal_follow_up,
            )?;
            mux.finish_pane_removal(removed, kill);
            drop(operation);
            Some(mux)
        }
    }

    enum PaneRegistrationSlotState {
        Unbound,
        Reserved { token: Arc<()> },
        Bound(PaneRegistrationHandle),
    }

    impl Default for PaneRegistrationSlotState {
        fn default() -> Self {
            Self::Unbound
        }
    }

    /// Exclusive pre-publication claim on a pane-owned registration slot.
    ///
    /// The reservation is acquired before fallible pane preparation. Dropping it
    /// rolls the slot back to `Unbound`; committing it is an allocation-free,
    /// callback-free state transition that the mux performs while serializing
    /// registry publication.
    #[must_use = "dropping a pane registration reservation cancels the claim"]
    pub(super) struct PaneRegistrationReservation {
        slot: Arc<PaneRegistrationSlot>,
        token: Arc<()>,
        registration: Option<PaneRegistrationHandle>,
        active: bool,
    }

    #[must_use = "pane registration commits must be finalized after registry publication"]
    pub(super) struct PaneRegistrationCommitGuard {
        slot: Arc<PaneRegistrationSlot>,
        registration: Option<PaneRegistrationHandle>,
        active: bool,
    }

    impl PaneRegistrationReservation {
        pub(super) fn commit(mut self) -> Result<PaneRegistrationCommitGuard, Error> {
            let registration = self
                .registration
                .take()
                .expect("active pane registration reservation retains its handle");
            {
                let mut state = self.slot.current.write();
                match &*state {
                    PaneRegistrationSlotState::Reserved { token }
                        if Arc::ptr_eq(token, &self.token) =>
                    {
                        *state = PaneRegistrationSlotState::Bound(registration.clone());
                    }
                    _ => {
                        return Err(anyhow!(
                            "pane registration slot reservation was lost before publication"
                        ));
                    }
                }
            }
            self.active = false;
            Ok(PaneRegistrationCommitGuard {
                slot: Arc::clone(&self.slot),
                registration: Some(registration),
                active: true,
            })
        }
    }

    impl Drop for PaneRegistrationReservation {
        fn drop(&mut self) {
            if !self.active {
                return;
            }
            let mut state = self.slot.current.write();
            if matches!(
                &*state,
                PaneRegistrationSlotState::Reserved { token }
                    if Arc::ptr_eq(token, &self.token)
            ) {
                *state = PaneRegistrationSlotState::Unbound;
            }
        }
    }

    impl PaneRegistrationCommitGuard {
        pub(super) fn finalize(mut self) -> PaneRegistrationHandle {
            self.active = false;
            self.registration
                .take()
                .expect("active pane registration commit retains its handle")
        }
    }

    impl Drop for PaneRegistrationCommitGuard {
        fn drop(&mut self) {
            if !self.active {
                return;
            }
            let Some(registration) = self.registration.as_ref() else {
                return;
            };
            let mut state = self.slot.current.write();
            if matches!(
                &*state,
                PaneRegistrationSlotState::Bound(current)
                    if current.same_registration(registration)
            ) {
                *state = PaneRegistrationSlotState::Unbound;
            }
        }
    }

    /// Shared binding point for pane-owned deferred callbacks.
    ///
    /// A pane may be registered more than once during its lifetime. Loading the
    /// slot clones the exact current-generation handle; already queued clones stay
    /// bound to their original generation and fail closed after replacement.
    ///
    /// Publication first reserves this slot. A different live registration or
    /// concurrent reservation is rejected, which prevents one pane object from
    /// silently acquiring two mux owners. While the owner remains alive, a
    /// binding may be replaced only after its leases, pane cleanup, and removal
    /// lifecycle have all completed. Owner destruction alone deliberately does
    /// not authorize rebinding: subscriber work can escape into external
    /// executors, so a weak-owner check is not a complete teardown barrier.
    #[derive(Default)]
    pub struct PaneRegistrationSlot {
        current: RwLock<PaneRegistrationSlotState>,
    }

    impl PaneRegistrationSlot {
        pub fn load(&self) -> Option<PaneRegistrationHandle> {
            match &*self.current.read() {
                PaneRegistrationSlotState::Bound(registration) => Some(registration.clone()),
                PaneRegistrationSlotState::Unbound | PaneRegistrationSlotState::Reserved { .. } => {
                    None
                }
            }
        }

        pub(super) fn reserve(
            self: &Arc<Self>,
            registration: PaneRegistrationHandle,
        ) -> Result<PaneRegistrationReservation, Error> {
            let token = Arc::new(());
            {
                let mut state = self.current.write();
                match &*state {
                    PaneRegistrationSlotState::Unbound => {}
                    PaneRegistrationSlotState::Bound(current)
                        if current.is_rebindable_retirement() => {}
                    PaneRegistrationSlotState::Bound(current) => {
                        return Err(anyhow!(
                            "pane {} is already bound to a live or draining mux registration",
                            current.pane_id()
                        ));
                    }
                    PaneRegistrationSlotState::Reserved { .. } => {
                        return Err(anyhow!(
                            "pane {} already has a registration publication in progress",
                            registration.pane_id()
                        ));
                    }
                }
                *state = PaneRegistrationSlotState::Reserved {
                    token: Arc::clone(&token),
                };
            }
            Ok(PaneRegistrationReservation {
                slot: Arc::clone(self),
                token,
                registration: Some(registration),
                active: true,
            })
        }
    }
}

pub use pane_registration_handle::{
    CurrentPane, CurrentPaneOutput, FloatingPaneCommitReceipt, FloatingSpawnTarget,
    FrozenFloatingPaneSpawn, MoveCommitReceipt, PaneOperationGuard, PaneRegistrationHandle,
    PaneRegistrationSlot, SplitCommitReceipt,
};

struct PanePreparation {
    pane: Weak<dyn Pane>,
    generation: Arc<PaneRegistrationGeneration>,
    cancelled: bool,
}

struct LivePaneRegistration {
    pane: Arc<dyn Pane>,
    generation: Arc<PaneRegistrationGeneration>,
    /// Stable domain identity observed before publication. Registry and
    /// topology transactions use this callback-free value while holding mux
    /// locks; invoking `Pane::domain_id` there would permit reentrant deadlock.
    domain_id: DomainId,
}

impl Drop for LivePaneRegistration {
    fn drop(&mut self) {
        self.generation.retire();
    }
}

struct PanePreparationClaim<'a> {
    registration: &'a Mutex<()>,
    claims: &'a Mutex<HashMap<PaneId, PanePreparation>>,
    pane_id: PaneId,
    domain_id: DomainId,
    pane: Weak<dyn Pane>,
    generation: Arc<PaneRegistrationGeneration>,
    active: bool,
}

impl PanePreparationClaim<'_> {
    /// Return whether this claim still owns the current preparation generation.
    ///
    /// The caller must hold `pane_registration`, which serializes this check
    /// with removal, a subsequent claim, and final publication.
    fn is_authoritative_locked(&self) -> bool {
        self.active
            && self
                .claims
                .lock()
                .get(&self.pane_id)
                .is_some_and(|preparing| {
                    !preparing.cancelled
                        && Weak::ptr_eq(&preparing.pane, &self.pane)
                        && Arc::ptr_eq(&preparing.generation, &self.generation)
                })
    }

    /// Retire this exact preparation generation without disturbing a newer
    /// claim for the same pane instance.
    ///
    /// The caller must hold `pane_registration`.
    fn retire_locked(&mut self) -> bool {
        if !self.active {
            return false;
        }
        let mut claims = self.claims.lock();
        let owns_generation = claims.get(&self.pane_id).is_some_and(|preparing| {
            Weak::ptr_eq(&preparing.pane, &self.pane)
                && Arc::ptr_eq(&preparing.generation, &self.generation)
        });
        if owns_generation {
            claims.remove(&self.pane_id);
        }
        self.active = false;
        owns_generation
    }
}

impl Drop for PanePreparationClaim<'_> {
    fn drop(&mut self) {
        if self.active {
            let registration = self.registration;
            let _registration = registration.lock();
            self.retire_locked();
        }
    }
}

/// Releases a successfully spawned pane reader only after publication and its
/// synchronous `PaneAdded` notification are complete.
///
/// Dropping this gate without releasing it closes the channel. The waiting
/// thread then exits without upgrading the pane or invoking the reader.
#[derive(Clone)]
enum PaneReaderStartDecision {
    Released,
    Cancelled,
}

#[derive(Debug, Clone, Copy, Eq, Hash, PartialEq)]
enum PaneReaderWorker {
    Parser,
    Reader,
}

#[cfg(test)]
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum PaneReaderPreparationFault {
    Socketpair,
    ParserSpawn,
    ReaderSpawn,
    ParserReady,
    ReaderReady,
}

const PANE_READER_READY_TIMEOUT: Duration = Duration::from_secs(30);

struct PaneReaderStartCoordinator {
    decision: Mutex<Option<PaneReaderStartDecision>>,
    changed: Condvar,
}

impl PaneReaderStartCoordinator {
    fn new() -> Self {
        Self {
            decision: Mutex::new(None),
            changed: Condvar::new(),
        }
    }

    fn wait(&self) -> PaneReaderStartDecision {
        let mut decision = self.decision.lock();
        while decision.is_none() {
            self.changed.wait(&mut decision);
        }
        decision
            .as_ref()
            .expect("pane reader decision is present after wait")
            .clone()
    }

    fn release(&self) {
        let mut decision = self.decision.lock();
        if decision.is_none() {
            *decision = Some(PaneReaderStartDecision::Released);
            self.changed.notify_all();
        }
    }

    fn cancel(&self) -> bool {
        let mut decision = self.decision.lock();
        if decision.is_none() {
            *decision = Some(PaneReaderStartDecision::Cancelled);
            self.changed.notify_all();
            true
        } else {
            false
        }
    }
}

struct PaneReaderStartGate {
    coordinator: Arc<PaneReaderStartCoordinator>,
    pane_id: PaneId,
    pane: Weak<dyn Pane>,
    generation: Arc<PaneRegistrationGeneration>,
}

impl PaneReaderStartGate {
    fn release_if_registered(self, mux: &Mux) {
        let owns_generation = self
            .generation
            .owner
            .upgrade()
            .is_some_and(|owner| std::ptr::eq::<Mux>(owner.as_ref(), mux));
        let is_registered = owns_generation && {
            let _registration = mux.pane_registration.lock();
            mux.panes
                .read()
                .get(&self.pane_id)
                .is_some_and(|registered| {
                    Weak::ptr_eq(&Arc::downgrade(&registered.pane), &self.pane)
                        && Arc::ptr_eq(&registered.generation, &self.generation)
                })
        };
        if !is_registered {
            return;
        }
        self.coordinator.release();
    }
}

impl Drop for PaneReaderStartGate {
    fn drop(&mut self) {
        if self.coordinator.cancel() {
            self.generation.reader_dead.store(true, Ordering::Release);
        }
    }
}

struct PaneOutputBatch {
    pane_id: PaneId,
    generation: Arc<PaneRegistrationGeneration>,
    lifecycle_notification: PaneLifecycleNotificationTicket,
    owner: Weak<Mux>,
    state: AtomicUsize,
    dispatch_on_main: bool,
    reserved_at: Instant,
}

struct PaneOutputContinuation {
    batch: Arc<PaneOutputBatch>,
    operation: Option<PaneRegistrationOperationLease>,
}

impl PaneOutputContinuation {
    fn finish(mut self) {
        self.finish_inner();
    }

    fn finish_inner(&mut self) {
        let Some(operation) = self.operation.take() else {
            return;
        };
        self.batch.finish_producer();
        // Arrange lifecycle publication before releasing the generation
        // operation. A concurrent retirement may then enqueue PaneRemoved,
        // but the earlier unready PaneOutput ticket still orders delivery.
        drop(operation);
    }
}

impl Drop for PaneOutputContinuation {
    fn drop(&mut self) {
        // `Pane::perform_actions` is external code and may unwind after
        // partially mutating terminal state. Conservatively complete the
        // reserved continuation so retirement cannot wedge or hide that state.
        self.finish_inner();
    }
}

#[derive(Default)]
struct PendingPaneOutputNotifications {
    notifications: Vec<Arc<PaneOutputBatch>>,
    queued: HashMap<PaneId, Arc<PaneOutputBatch>>,
}

#[derive(Clone)]
enum PaneLifecycleNotification {
    Added(PaneId),
    FloatingSpawnCommitted(FrozenFloatingPaneSpawn),
    Removed(PaneId),
    Output(PaneId),
}

impl PaneLifecycleNotification {
    fn pane_id(&self) -> PaneId {
        match self {
            Self::Added(pane_id) | Self::Removed(pane_id) | Self::Output(pane_id) => *pane_id,
            Self::FloatingSpawnCommitted(spawn) => spawn.pane_id(),
        }
    }
}

impl From<PaneLifecycleNotification> for MuxNotification {
    fn from(notification: PaneLifecycleNotification) -> Self {
        match notification {
            PaneLifecycleNotification::Added(pane_id) => Self::PaneAdded(pane_id),
            PaneLifecycleNotification::FloatingSpawnCommitted(spawn) => {
                Self::FloatingPaneSpawnCommitted(spawn)
            }
            PaneLifecycleNotification::Removed(pane_id) => Self::PaneRemoved(pane_id),
            PaneLifecycleNotification::Output(pane_id) => Self::PaneOutput(pane_id),
        }
    }
}

struct PendingPaneLifecycleNotification {
    notification: PaneLifecycleNotification,
    topology: MuxTopologyStamp,
    ready: Arc<AtomicBool>,
    reader_start_gate: Option<PaneReaderStartGate>,
    cleanup_complete: Option<Arc<AtomicBool>>,
    removal_follow_up: PaneRemovalFollowUp,
}

#[derive(Default)]
struct PendingPaneLifecycleNotifications {
    by_pane: HashMap<PaneId, VecDeque<PendingPaneLifecycleNotification>>,
    retirements: HashMap<PaneId, VecDeque<PaneRetirementCompletion>>,
    ready_panes: VecDeque<PaneId>,
    ready_set: HashSet<PaneId>,
    draining: bool,
}

/// Allocation-complete authority to append one lifecycle event while retaining
/// the lifecycle queue lock through a structural commit.
///
/// The final append itself is infallible: map, per-pane queue, ready queue, and
/// ready-set capacity are all reserved before the caller acquires topology
/// revision authority or mutates mux state.
struct PreparedPaneLifecycleEnqueue<'a> {
    pending: parking_lot::MutexGuard<'a, PendingPaneLifecycleNotifications>,
    pane_id: PaneId,
    ready: Arc<AtomicBool>,
    vacant_queue: Option<VecDeque<PendingPaneLifecycleNotification>>,
}

struct PreparedPaneLifecycleBatchEntry {
    pane_id: PaneId,
    ready: Arc<AtomicBool>,
    vacant_queue: Option<VecDeque<PendingPaneLifecycleNotification>>,
}

/// Allocation-complete authority for appending several independent pane
/// lifecycle edges in one structural transaction.
///
/// The queue guard remains held through the caller's final commit cut. Every
/// map, queue, ready-set, and result-vector allocation is reserved up front,
/// so [`PreparedPaneLifecycleBatchEnqueue::enqueue`] only moves prepared
/// values into already-owned storage.
struct PreparedPaneLifecycleBatchEnqueue<'a> {
    pending: parking_lot::MutexGuard<'a, PendingPaneLifecycleNotifications>,
    entries: Vec<PreparedPaneLifecycleBatchEntry>,
    tickets: Vec<PaneLifecycleNotificationTicket>,
}

struct PreparedPaneLifecycleBatchNotification {
    notification: PaneLifecycleNotification,
    topology: MuxTopologyStamp,
    reader_start_gate: Option<PaneReaderStartGate>,
    cleanup_complete: Option<Arc<AtomicBool>>,
    removal_follow_up: PaneRemovalFollowUp,
}

impl PreparedPaneLifecycleEnqueue<'_> {
    fn enqueue(
        mut self,
        notification: PaneLifecycleNotification,
        topology: MuxTopologyStamp,
        reader_start_gate: Option<PaneReaderStartGate>,
        cleanup_complete: Option<Arc<AtomicBool>>,
        removal_follow_up: PaneRemovalFollowUp,
    ) -> PaneLifecycleNotificationTicket {
        debug_assert_eq!(notification.pane_id(), self.pane_id);
        let ticket_ready = Arc::clone(&self.ready);
        let pending_notification = PendingPaneLifecycleNotification {
            notification,
            topology,
            ready: self.ready,
            reader_start_gate,
            cleanup_complete,
            removal_follow_up,
        };
        if let Some(queue) = self.pending.by_pane.get_mut(&self.pane_id) {
            queue.push_back(pending_notification);
        } else {
            let mut queue = self
                .vacant_queue
                .take()
                .expect("a vacant lifecycle slot retains its prepared queue");
            queue.push_back(pending_notification);
            let prior = self.pending.by_pane.insert(self.pane_id, queue);
            debug_assert!(prior.is_none());
        }
        PaneLifecycleNotificationTicket {
            pane_id: self.pane_id,
            ready: ticket_ready,
        }
    }
}

impl PreparedPaneLifecycleBatchEnqueue<'_> {
    fn enqueue(
        mut self,
        notifications: Vec<PreparedPaneLifecycleBatchNotification>,
    ) -> Vec<PaneLifecycleNotificationTicket> {
        assert_eq!(
            notifications.len(),
            self.entries.len(),
            "prepared lifecycle batch cardinality changed before commit"
        );
        for (mut entry, notification) in self.entries.drain(..).zip(notifications) {
            debug_assert_eq!(notification.notification.pane_id(), entry.pane_id);
            let pending_notification = PendingPaneLifecycleNotification {
                notification: notification.notification,
                topology: notification.topology,
                ready: entry.ready,
                reader_start_gate: notification.reader_start_gate,
                cleanup_complete: notification.cleanup_complete,
                removal_follow_up: notification.removal_follow_up,
            };
            if let Some(queue) = self.pending.by_pane.get_mut(&entry.pane_id) {
                queue.push_back(pending_notification);
            } else {
                let mut queue = entry
                    .vacant_queue
                    .take()
                    .expect("a vacant lifecycle batch slot retains its prepared queue");
                queue.push_back(pending_notification);
                let prior = self.pending.by_pane.insert(entry.pane_id, queue);
                debug_assert!(prior.is_none());
            }
        }
        self.tickets
    }
}

impl PendingPaneLifecycleNotifications {
    fn pane_has_ready_action(&self, pane_id: PaneId) -> bool {
        let Some(notification) = self
            .by_pane
            .get(&pane_id)
            .and_then(|notifications| notifications.front())
        else {
            return false;
        };
        notification.ready.load(Ordering::Acquire)
            || self
                .retirements
                .get(&pane_id)
                .and_then(|retirements| retirements.front())
                .is_some_and(|retirement| {
                    Arc::ptr_eq(
                        &notification.ready,
                        &retirement.lifecycle_notification.ready,
                    )
                })
    }

    fn arm_pane_if_ready(&mut self, pane_id: PaneId) {
        if self.pane_has_ready_action(pane_id) && self.ready_set.insert(pane_id) {
            self.ready_panes.push_back(pane_id);
        }
    }

    fn begin_drain_if_ready(&mut self) -> bool {
        if self.draining || self.ready_panes.is_empty() {
            return false;
        }
        self.draining = true;
        true
    }
}

#[derive(Clone)]
struct PaneLifecycleNotificationTicket {
    pane_id: PaneId,
    ready: Arc<AtomicBool>,
}

enum PaneLifecycleDrainStep {
    Notification(PendingPaneLifecycleNotification),
    Retirement(PaneRetirementCompletion),
    Done,
}

const PANE_OUTPUT_BATCH_SEALED: usize = 1usize << (usize::BITS - 1);
const PANE_OUTPUT_BATCH_PRODUCER_MASK: usize = PANE_OUTPUT_BATCH_SEALED - 1;

struct PaneLifecycleTicketDispatch {
    owner: Weak<Mux>,
    ticket: Option<PaneLifecycleNotificationTicket>,
}

impl PaneLifecycleTicketDispatch {
    fn new(owner: Weak<Mux>, ticket: PaneLifecycleNotificationTicket) -> Self {
        Self {
            owner,
            ticket: Some(ticket),
        }
    }

    fn execute(mut self, scheduled: bool) {
        let ticket = self
            .ticket
            .take()
            .expect("pane lifecycle dispatch executes at most once");
        Self::finish(&self.owner, ticket, scheduled);
    }

    fn finish(owner: &Weak<Mux>, ticket: PaneLifecycleNotificationTicket, scheduled: bool) {
        if let Some(mux) = owner.upgrade() {
            if scheduled && !mux.is_main_thread() {
                log::error!(
                    "pane {} lifecycle scheduler ran away from the mux main thread; \
                     completing inline to preserve lifecycle liveness",
                    ticket.pane_id
                );
            }
            mux.complete_pane_lifecycle_notification(ticket);
        }
    }
}

impl Drop for PaneLifecycleTicketDispatch {
    fn drop(&mut self) {
        if let Some(ticket) = self.ticket.take() {
            Self::finish(&self.owner, ticket, false);
        }
    }
}

struct PaneOutputDrainDispatch {
    owner: Weak<Mux>,
    armed: bool,
}

impl PaneOutputDrainDispatch {
    fn new(owner: Weak<Mux>) -> Self {
        Self { owner, armed: true }
    }

    fn execute(mut self) {
        self.armed = false;
        if let Some(mux) = self.owner.upgrade() {
            mux.flush_pending_pane_output_notifications();
        }
    }
}

impl Drop for PaneOutputDrainDispatch {
    fn drop(&mut self) {
        if self.armed {
            if let Some(mux) = self.owner.upgrade() {
                mux.flush_pending_pane_output_notifications();
            }
        }
    }
}

impl PaneOutputBatch {
    fn new(
        pane_id: PaneId,
        generation: Arc<PaneRegistrationGeneration>,
        lifecycle_notification: PaneLifecycleNotificationTicket,
        producer_count: usize,
        dispatch_on_main: bool,
    ) -> Arc<Self> {
        debug_assert!(producer_count <= PANE_OUTPUT_BATCH_PRODUCER_MASK);
        let owner = generation.owner.clone();
        Arc::new(Self {
            pane_id,
            generation,
            lifecycle_notification,
            owner,
            state: AtomicUsize::new(producer_count),
            dispatch_on_main,
            reserved_at: Instant::now(),
        })
    }

    fn try_join_producer(&self) -> bool {
        let mut state = self.state.load(Ordering::Acquire);
        loop {
            if state & PANE_OUTPUT_BATCH_SEALED != 0 {
                return false;
            }
            let producers = state & PANE_OUTPUT_BATCH_PRODUCER_MASK;
            let Some(next) = producers.checked_add(1) else {
                return false;
            };
            if next > PANE_OUTPUT_BATCH_PRODUCER_MASK {
                return false;
            }
            match self
                .state
                .compare_exchange_weak(state, next, Ordering::AcqRel, Ordering::Acquire)
            {
                Ok(_) => return true,
                Err(actual) => state = actual,
            }
        }
    }

    fn finish_producer(&self) {
        let previous = self.state.fetch_sub(1, Ordering::AcqRel);
        debug_assert!(
            previous & PANE_OUTPUT_BATCH_PRODUCER_MASK > 0,
            "pane output producer count must not underflow"
        );
        if previous == (PANE_OUTPUT_BATCH_SEALED | 1) {
            self.publish();
        }
    }

    fn seal(&self) {
        let previous = self
            .state
            .fetch_or(PANE_OUTPUT_BATCH_SEALED, Ordering::AcqRel);
        if previous & PANE_OUTPUT_BATCH_SEALED != 0 {
            return;
        }
        if previous & PANE_OUTPUT_BATCH_PRODUCER_MASK == 0 {
            self.publish();
        }
    }

    fn publish(&self) {
        histogram!("mux.notifications.pane_output.reservation_to_dispatch")
            .record(self.reserved_at.elapsed());
        let Some(mux) = self.owner.upgrade() else {
            return;
        };
        let ticket = self.lifecycle_notification.clone();
        if !self.dispatch_on_main
            || mux.is_main_thread()
            || !promise::spawn::is_scheduler_configured()
        {
            mux.complete_pane_lifecycle_notification(ticket);
            return;
        }
        drop(mux);

        let dispatch = PaneLifecycleTicketDispatch::new(self.owner.clone(), ticket);
        promise::spawn::spawn_into_main_thread(async move {
            dispatch.execute(true);
        })
        .detach();
    }
}

struct PaneRetirementDispatch {
    owner: Weak<Mux>,
    completion: Option<PaneRetirementCompletion>,
}

impl PaneRetirementDispatch {
    fn new(owner: Weak<Mux>, completion: PaneRetirementCompletion) -> Self {
        Self {
            owner,
            completion: Some(completion),
        }
    }

    fn execute(mut self, scheduled: bool) {
        let completion = self
            .completion
            .take()
            .expect("pane retirement dispatch executes at most once");
        Self::finish(&self.owner, completion, scheduled);
    }

    fn finish(owner: &Weak<Mux>, completion: PaneRetirementCompletion, scheduled: bool) {
        if let Some(mux) = owner.upgrade() {
            if scheduled && !mux.is_main_thread() {
                log::error!(
                    "pane {} retirement scheduler ran away from the mux main thread; \
                     completing inline to preserve lifecycle liveness",
                    completion.pane_id
                );
            }
            mux.enqueue_pane_retirement(completion);
        } else {
            completion.complete(None);
        }
    }
}

impl Drop for PaneRetirementDispatch {
    fn drop(&mut self) {
        if let Some(completion) = self.completion.take() {
            // A scheduler is allowed to reject/drop its runnable. Once cleanup
            // is claimed there is no second owner to retry it, so fail open
            // for thread affinity but fail closed for lifecycle liveness.
            Self::finish(&self.owner, completion, false);
        }
    }
}

impl PaneRetirementCompletion {
    fn run(self, owner: Weak<Mux>, execution: PaneRetirementExecution) {
        if matches!(execution, PaneRetirementExecution::Inline) {
            PaneRetirementDispatch::finish(&owner, self, false);
            return;
        }

        let Some(mux) = owner.upgrade() else {
            self.complete(None);
            return;
        };
        if mux.is_main_thread() || !promise::spawn::is_scheduler_configured() {
            mux.enqueue_pane_retirement(self);
            return;
        }
        drop(mux);

        let dispatch = PaneRetirementDispatch::new(owner, self);
        promise::spawn::spawn_into_main_thread(async move {
            dispatch.execute(true);
        })
        .detach();
    }

    fn complete(self, mux: Option<&Mux>) {
        if self.kill {
            log::debug!("killing pane {}", self.pane_id);
            if catch_recoverable(
                RecoverablePanicSite::MuxPaneRetirement,
                std::panic::AssertUnwindSafe(|| self.pane.kill()),
            )
            .is_err()
            {
                log::error!(
                    "pane {} panicked while being killed; completing removal lifecycle",
                    self.pane_id
                );
            }
        }
        if let Some(mux) = mux {
            mux.complete_pane_lifecycle_notification(self.lifecycle_notification);
        } else {
            self.cleanup_complete.store(true, Ordering::Release);
        }
    }
}

struct RemovedPaneRegistration {
    pane_id: PaneId,
    pane: Arc<dyn Pane>,
    generation: Arc<PaneRegistrationGeneration>,
    lifecycle_notification: PaneLifecycleNotificationTicket,
}

struct RemovedTabRegistration {
    structural_panes: Vec<Arc<dyn Pane>>,
    removed_panes: Vec<RemovedPaneRegistration>,
    output_batches: Vec<Arc<PaneOutputBatch>>,
}

struct RemovedWindowRegistration {
    removed_tabs: Vec<RemovedTabRegistration>,
}

/// Exact structural parent of one registered tab.
///
/// The weak reference proves that a recycled numeric [`TabId`] cannot inherit
/// the prior tab generation's window.  Entries are committed under the same
/// window-map write guard as the ordered membership vectors, so readers never
/// need to scan unrelated windows to resolve a parent.
#[derive(Clone)]
struct TabParentRegistration {
    tab: Weak<Tab>,
    window_id: WindowId,
}

impl TabParentRegistration {
    fn new(tab: &Arc<Tab>, window_id: WindowId) -> Self {
        Self {
            tab: Arc::downgrade(tab),
            window_id,
        }
    }

    fn matches(&self, tab: &Arc<Tab>, window_id: WindowId) -> bool {
        self.window_id == window_id
            && self
                .tab
                .upgrade()
                .is_some_and(|registered| Arc::ptr_eq(&registered, tab))
    }

    fn is_same_tab(&self, tab: &Arc<Tab>) -> bool {
        self.tab
            .upgrade()
            .is_some_and(|registered| Arc::ptr_eq(&registered, tab))
    }
}

/// Revision-coherent exact location of one registered pane.
///
/// A stable session resolves the same pane for every key event.  Repeating the
/// historical all-tabs/all-panes scan on that path made input latency grow
/// with unrelated session size.  This cache is safe without mutation hooks in
/// every `Tab` method because a hit revalidates the exact registration, tab,
/// parent, and structural pane presence inside the same topology revision in
/// which the location was censused. A revision change causes one exact cold
/// refresh; subsequent stable key events avoid every unrelated tab.
#[derive(Clone)]
struct PaneLocationCacheEntry {
    registration: PaneRegistrationHandle,
    pane: Weak<dyn Pane>,
    tab: Weak<Tab>,
    domain_id: DomainId,
    window_id: WindowId,
    topology_revision: TopologyRevision,
}

/// Discriminant key for the high-rate Alert variants we dedupe per pane.
///
/// `CurrentWorkingDirectoryChanged` (OSC 7) re-emits on every shell prompt
/// under active agent output; `OutputSinceFocusLost` re-emits on every seqno
/// bump to an unfocused pane. Across N attached muxes these can saturate the
/// notify path with thousands of clones+box allocations per second. Progress
/// is deliberately excluded: Percentage(42) followed by Percentage(64) is a
/// state transition, not a duplicate, and timer-dropping the newer value can
/// leave a remote client stale indefinitely. See ft-18xgy and
/// ft-interactive-systems-performance-4tenz.5.5.1.
#[derive(Debug, Clone, Copy, Hash, PartialEq, Eq)]
enum HighRateAlertKind {
    CurrentWorkingDirectoryChanged,
    OutputSinceFocusLost,
}

/// Window during which a `(pane_id, kind)` repeat is dropped at the mux
/// fanout layer. ~1 frame at 60 Hz; below human perception for shell-prompt UI.
const HIGH_RATE_ALERT_DEDUPE_WINDOW: Duration = Duration::from_millis(16);
/// Stale entries older than this are pruned on each insert to keep the dedupe
/// map bounded regardless of pane churn.
const HIGH_RATE_ALERT_PRUNE_AFTER: Duration = Duration::from_secs(1);

#[derive(Default)]
struct PendingWindowNotifications {
    queue: VecDeque<PendingWindowAction>,
    draining: bool,
    scheduled: bool,
    owner: Weak<Mux>,
}

#[derive(Clone)]
struct WindowOrderReceipt {
    request_digest: WindowReorderDigest,
    outcome: WindowReorderTerminalOutcome,
    retained_tab_ids: usize,
}

struct WindowOrderReceiptLedger {
    receipts: HashMap<WindowOrderMutationId, WindowOrderReceipt>,
    insertion_order: VecDeque<WindowOrderMutationId>,
    retained_tab_ids: usize,
}

impl WindowOrderReceiptLedger {
    fn new() -> Self {
        Self {
            receipts: HashMap::new(),
            insertion_order: VecDeque::new(),
            retained_tab_ids: 0,
        }
    }

    fn lookup(
        &self,
        mutation_id: WindowOrderMutationId,
        request_digest: WindowReorderDigest,
    ) -> Option<ReorderWindowTabsResult> {
        let receipt = self.receipts.get(&mutation_id)?;
        Some(if receipt.request_digest == request_digest {
            ReorderWindowTabsResult::Replay(receipt.outcome.clone())
        } else {
            ReorderWindowTabsResult::Equivocation {
                mutation_id,
                retained_digest: receipt.request_digest,
                attempted_digest: request_digest,
            }
        })
    }

    fn retain(
        &mut self,
        mutation_id: WindowOrderMutationId,
        request_digest: WindowReorderDigest,
        outcome: WindowReorderTerminalOutcome,
    ) {
        debug_assert!(!self.receipts.contains_key(&mutation_id));
        let retained_tab_ids = outcome.retained_tab_id_count();
        debug_assert!(retained_tab_ids <= MAX_TABS_PER_ORDERED_WINDOW);
        while self.receipts.len() == MAX_WINDOW_ORDER_RECEIPTS
            || self.retained_tab_ids + retained_tab_ids > MAX_WINDOW_ORDER_RECEIPT_TAB_IDS
        {
            let expired = self
                .insertion_order
                .pop_front()
                .expect("an over-budget window-order receipt ledger has an insertion record");
            let removed = self
                .receipts
                .remove(&expired)
                .expect("a retained insertion identity has one receipt");
            self.retained_tab_ids -= removed.retained_tab_ids;
        }
        self.receipts.insert(
            mutation_id,
            WindowOrderReceipt {
                request_digest,
                outcome,
                retained_tab_ids,
            },
        );
        self.insertion_order.push_back(mutation_id);
        self.retained_tab_ids += retained_tab_ids;
    }
}

enum PendingWindowAction {
    Notification {
        envelope: MuxNotificationEnvelope,
        activity: Option<Activity>,
    },
    FocusLost(Arc<dyn Pane>),
}

struct WindowNotificationDispatch {
    owner: Weak<Mux>,
    armed: bool,
}

impl WindowNotificationDispatch {
    fn new(owner: Weak<Mux>) -> Self {
        Self { owner, armed: true }
    }

    fn execute(mut self) {
        if let Some(owner) = self.owner.upgrade() {
            owner.run_scheduled_window_notification_drain();
        }
        self.armed = false;
    }
}

impl Drop for WindowNotificationDispatch {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        if let Some(owner) = self.owner.upgrade() {
            owner.window_notification_dispatch_dropped();
        }
        self.armed = false;
    }
}

pub struct Mux {
    topology: Mutex<MuxTopologyAuthority>,
    tabs: RwLock<HashMap<TabId, Arc<Tab>>>,
    tab_parents: RwLock<HashMap<TabId, TabParentRegistration>>,
    #[cfg(test)]
    tab_parent_lookup_probes: AtomicUsize,
    #[cfg(test)]
    tab_parent_write_cuts: AtomicUsize,
    pane_location_cache: RwLock<HashMap<PaneId, PaneLocationCacheEntry>>,
    #[cfg(test)]
    pane_location_cache_hits: AtomicUsize,
    #[cfg(test)]
    pane_location_full_scans: AtomicUsize,
    #[cfg(test)]
    pane_location_scan_tab_probes: AtomicUsize,
    panes: RwLock<HashMap<PaneId, LivePaneRegistration>>,
    pane_retirements: Arc<PaneRetirementTracker>,
    pane_preparations: Mutex<HashMap<PaneId, PanePreparation>>,
    pane_registration: Mutex<()>,
    retiring_pane_ids: Mutex<HashSet<PaneId>>,
    pane_removal_cleanup_fences: Mutex<HashMap<PaneId, Arc<PaneRemovalCleanupToken>>>,
    pane_removal_cleanup_outstanding_leases: AtomicUsize,
    pending_pane_lifecycle: Mutex<PendingPaneLifecycleNotifications>,
    windows: RwLock<HashMap<WindowId, Window>>,
    provisional_windows: Mutex<HashSet<WindowId>>,
    pending_window_notifications: Mutex<PendingWindowNotifications>,
    window_order_receipts: Mutex<WindowOrderReceiptLedger>,
    activity_count: Arc<AtomicUsize>,
    activity_prune_state: Arc<ActivityPruneState>,
    default_domain: RwLock<Option<Arc<dyn Domain>>>,
    domains: RwLock<HashMap<DomainId, Arc<dyn Domain>>>,
    domains_by_name: RwLock<HashMap<String, Arc<dyn Domain>>>,
    domain_registration: Mutex<()>,
    retired_domain_ids: Mutex<HashSet<DomainId>>,
    subscribers: RwLock<HashMap<usize, Arc<MuxSubscriber>>>,
    pending_pane_output: Mutex<PendingPaneOutputNotifications>,
    pane_output_drain_scheduled: AtomicBool,
    #[cfg(test)]
    pane_reader_preparation_fault: Mutex<Option<PaneReaderPreparationFault>>,
    #[cfg(test)]
    pane_count_recomputes: AtomicUsize,
    banner: RwLock<Option<String>>,
    clients: RwLock<HashMap<ClientId, ClientInfo>>,
    reliable_input: Mutex<ReliableInputLedger>,
    identity: RwLock<Option<Arc<ClientId>>>,
    num_panes_by_workspace: RwLock<HashMap<String, usize>>,
    main_thread_id: std::thread::ThreadId,
    agent: Option<AgentProxy>,
    /// Per-(pane, alert-kind) timestamp of the most recently dispatched
    /// high-rate Alert. Used by `notify` to drop duplicate repeats within
    /// `HIGH_RATE_ALERT_DEDUPE_WINDOW`. ft-18xgy.
    last_high_rate_alert: Mutex<HashMap<(PaneId, HighRateAlertKind), Instant>>,
}

/// Maximum number of distinct client incarnations whose latest reliable input
/// identity is retained for one mux lifetime. Reconnects reuse the same
/// [`ClientId`] entry. A new identity beyond this bound is admitted as an
/// ordinary client but receives a typed reliable-input refusal rather than
/// growing mux memory without limit.
pub const MAX_RELIABLE_INPUT_CLIENTS: usize = 4_096;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReliableInputKeyKind {
    KeyDown,
    KeyUp,
}

#[derive(Debug, Clone)]
struct ReliableInputFingerprint {
    registration: PaneRegistrationHandle,
    input_serial: u64,
    kind: ReliableInputKeyKind,
    event: KeyEvent,
}

impl ReliableInputFingerprint {
    fn matches(
        &self,
        registration: &PaneRegistrationHandle,
        input_serial: u64,
        kind: ReliableInputKeyKind,
        event: &KeyEvent,
    ) -> bool {
        self.registration.same_registration(registration)
            && self.input_serial == input_serial
            && self.kind == kind
            && self.event == *event
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReliableInputTerminalOutcome {
    Applied,
    OutcomeUnknown,
}

#[derive(Debug)]
struct ReliableInputTerminal {
    fingerprint: ReliableInputFingerprint,
    outcome: ReliableInputTerminalOutcome,
}

#[derive(Debug)]
struct ReliableInputPending {
    claim_id: NonZeroU64,
    fingerprint: ReliableInputFingerprint,
    side_effect_started: bool,
}

#[derive(Debug)]
struct ReliableInputClientLedger {
    next_claim_id: Option<NonZeroU64>,
    pending: Option<ReliableInputPending>,
    terminal: Option<ReliableInputTerminal>,
}

impl ReliableInputClientLedger {
    fn new() -> Self {
        Self {
            next_claim_id: NonZeroU64::new(1),
            pending: None,
            terminal: None,
        }
    }
}

#[derive(Debug)]
struct ReliableInputLedger {
    clients: HashMap<ClientId, Arc<Mutex<ReliableInputClientLedger>>>,
}

impl ReliableInputLedger {
    fn new() -> Self {
        Self {
            clients: HashMap::with_capacity(MAX_RELIABLE_INPUT_CLIENTS),
        }
    }

    fn prepare_client(&mut self, client_id: &ClientId) -> bool {
        if self.clients.contains_key(client_id) {
            return true;
        }
        if self.clients.len() >= MAX_RELIABLE_INPUT_CLIENTS {
            return false;
        }
        self.clients.insert(
            client_id.clone(),
            Arc::new(Mutex::new(ReliableInputClientLedger::new())),
        );
        true
    }
}

#[derive(Debug)]
pub enum ReliableInputClaimOutcome {
    Execute(ReliableInputCommitPermit),
    DuplicateApplied,
    DuplicatePending,
    OutcomeUnknown,
    ClientNotRegistered,
    ClientRegistrationRetired,
    ClientLedgerUnavailable,
    IdentityAuthorityExhausted,
    StaleSerial,
    IdentityConflict,
}

/// Armed ownership of one client's sole pending reliable key transition.
///
/// Dropping before [`Self::begin_side_effect`] rolls the claim back so a retry
/// may execute. Dropping after the pane callback begins records an
/// outcome-unknown terminal state: replay is then refused rather than risking
/// a duplicate key effect after a panic or callback error.
pub struct ReliableInputCommitPermit {
    client: Arc<Mutex<ReliableInputClientLedger>>,
    claim_id: NonZeroU64,
    armed: bool,
}

impl std::fmt::Debug for ReliableInputCommitPermit {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ReliableInputCommitPermit")
            .field("claim_id", &self.claim_id)
            .field("armed", &self.armed)
            .finish_non_exhaustive()
    }
}

impl ReliableInputCommitPermit {
    /// Mark the exact point after which a callback failure cannot prove that no
    /// externally visible key effect occurred.
    pub fn begin_side_effect(&mut self) -> bool {
        let mut client = self.client.lock();
        let Some(pending) = client.pending.as_mut() else {
            return false;
        };
        if pending.claim_id != self.claim_id || pending.side_effect_started {
            return false;
        }
        pending.side_effect_started = true;
        true
    }

    /// Commit successful application without allocation.
    pub fn commit_applied(mut self) -> bool {
        let committed = {
            let mut client = self.client.lock();
            let Some(pending) = client.pending.take() else {
                return false;
            };
            if pending.claim_id != self.claim_id || !pending.side_effect_started {
                client.pending = Some(pending);
                return false;
            }
            client.terminal = Some(ReliableInputTerminal {
                fingerprint: pending.fingerprint,
                outcome: ReliableInputTerminalOutcome::Applied,
            });
            true
        };
        if committed {
            self.armed = false;
        }
        committed
    }
}

impl Drop for ReliableInputCommitPermit {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let mut client = self.client.lock();
        let Some(pending) = client.pending.take() else {
            return;
        };
        if pending.claim_id != self.claim_id {
            client.pending = Some(pending);
            return;
        }
        if pending.side_effect_started {
            client.terminal = Some(ReliableInputTerminal {
                fingerprint: pending.fingerprint,
                outcome: ReliableInputTerminalOutcome::OutcomeUnknown,
            });
        }
    }
}

fn mux_socket_buffer_size() -> usize {
    configuration().mux_socket_buffer_size
}

fn max_held_synchronized_output_bytes() -> usize {
    configuration().mux_max_synchronized_output_bytes
}

fn synchronized_output_decrqm_response(hold: bool) -> &'static [u8] {
    if hold {
        b"\x1b[?2026;1$y"
    } else {
        b"\x1b[?2026;2$y"
    }
}

fn respond_to_synchronized_output_query(
    pane: &Weak<dyn Pane>,
    generation: &Arc<PaneRegistrationGeneration>,
    hold: bool,
) {
    let Some(_operation) = generation.try_acquire() else {
        return;
    };
    let Some(pane) = pane.upgrade() else {
        return;
    };
    let Some(_mux) = resolve_pane_reader_mux(&pane, generation) else {
        return;
    };

    let mut writer = pane.writer();
    if let Err(err) = writer.write_all(synchronized_output_decrqm_response(hold)) {
        log::warn!("failed to answer DEC 2026 mode query: {err}");
        return;
    }
    if let Err(err) = writer.flush() {
        log::warn!("failed to flush DEC 2026 mode query response: {err}");
    }
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
struct SynchronizedOutputActionEffect {
    flush: bool,
    handled: bool,
    depth_outcome: Option<SynchronizedOutputDepthOutcome>,
}

#[derive(Debug, Clone, Copy, Default, Eq, PartialEq)]
struct SynchronizedOutputHold {
    depth: u32,
    max_depth: u32,
}

impl SynchronizedOutputHold {
    fn is_holding(self) -> bool {
        self.depth > 0
    }

    fn max_depth(self) -> u32 {
        self.max_depth
    }

    fn open_bsu(&mut self) -> SynchronizedOutputDepthOutcome {
        self.depth = self.depth.saturating_add(1);
        if self.depth > self.max_depth {
            self.max_depth = self.depth;
        }
        SynchronizedOutputDepthOutcome::Opened {
            new_depth: self.depth,
        }
    }

    fn close_esu(&mut self) -> SynchronizedOutputDepthOutcome {
        if self.depth == 0 {
            return SynchronizedOutputDepthOutcome::Underflow;
        }
        self.depth -= 1;
        if self.depth == 0 {
            SynchronizedOutputDepthOutcome::Flushed
        } else {
            SynchronizedOutputDepthOutcome::Closed {
                new_depth: self.depth,
            }
        }
    }

    fn force_reset(&mut self) -> bool {
        let was_holding = self.is_holding();
        self.depth = 0;
        was_holding
    }
}

fn handle_synchronized_output_action(
    action: &Action,
    hold: &mut SynchronizedOutputHold,
    respond_to_query: impl FnOnce(bool),
) -> SynchronizedOutputActionEffect {
    let mut effect = SynchronizedOutputActionEffect {
        flush: false,
        handled: false,
        depth_outcome: None,
    };

    match action {
        Action::CSI(CSI::Mode(Mode::SetDecPrivateMode(DecPrivateMode::Code(
            DecPrivateModeCode::SynchronizedOutput,
        )))) => {
            effect.depth_outcome = Some(hold.open_bsu());
        }
        Action::CSI(CSI::Mode(Mode::ResetDecPrivateMode(DecPrivateMode::Code(
            DecPrivateModeCode::SynchronizedOutput,
        )))) => {
            let outcome = hold.close_esu();
            effect.flush = matches!(outcome, SynchronizedOutputDepthOutcome::Flushed);
            effect.depth_outcome = Some(outcome);
        }
        Action::CSI(CSI::Device(dev)) if matches!(**dev, Device::SoftReset) => {
            effect.flush = hold.force_reset();
        }
        Action::CSI(CSI::Mode(Mode::QueryDecPrivateMode(DecPrivateMode::Code(
            DecPrivateModeCode::SynchronizedOutput,
        )))) => {
            respond_to_query(hold.is_holding());
            effect.handled = true;
        }
        _ => {}
    }

    effect
}

fn notify_synchronized_output_event(
    pane: &Weak<dyn Pane>,
    generation: &Arc<PaneRegistrationGeneration>,
    event: SynchronizedOutputEvent,
) {
    let Some(operation) = generation.try_acquire() else {
        return;
    };
    let Some(pane) = pane.upgrade() else {
        return;
    };
    let Some(mux) = resolve_pane_reader_mux(&pane, generation) else {
        return;
    };
    let notification = MuxNotification::SynchronizedOutput {
        pane_id: pane.pane_id(),
        event,
    };
    if mux.is_main_thread() || !promise::spawn::is_scheduler_configured() {
        mux.notify(notification);
        return;
    }

    let owner = Arc::downgrade(&mux);
    promise::spawn::spawn_into_main_thread(async move {
        let _operation = operation;
        let Some(mux) = owner.upgrade() else {
            return;
        };
        mux.notify(notification);
    })
    .detach();
}

/// This function applies parsed actions to the pane and notifies any
/// mux subscribers about the output event
fn resolve_pane_reader_mux(
    pane: &Arc<dyn Pane>,
    generation: &Arc<PaneRegistrationGeneration>,
) -> Option<Arc<Mux>> {
    let mux = generation.owner.upgrade()?;
    let is_registered = {
        let _registration = mux.pane_registration.lock();
        mux.panes
            .read()
            .get(&pane.pane_id())
            .is_some_and(|registered| {
                Arc::ptr_eq(&registered.pane, pane)
                    && Arc::ptr_eq(&registered.generation, generation)
            })
    };
    is_registered.then_some(mux)
}

fn send_actions_to_mux(
    pane: &Weak<dyn Pane>,
    generation: &Arc<PaneRegistrationGeneration>,
    dead: &Arc<AtomicBool>,
    actions: Vec<Action>,
) {
    send_actions_to_mux_with_scheduler_state(
        pane,
        generation,
        dead,
        actions,
        promise::spawn::is_scheduler_configured(),
    );
}

fn send_actions_to_mux_with_scheduler_state(
    pane: &Weak<dyn Pane>,
    generation: &Arc<PaneRegistrationGeneration>,
    dead: &Arc<AtomicBool>,
    actions: Vec<Action>,
    scheduler_configured: bool,
) {
    let start = Instant::now();
    let Some(pane) = pane.upgrade() else {
        dead.store(true, Ordering::Release);
        return;
    };
    let Some(mux) = generation.owner.upgrade() else {
        dead.store(true, Ordering::Release);
        return;
    };
    let Some(output) = mux.reserve_pane_output_for_reader(&pane, generation, scheduler_configured)
    else {
        dead.store(true, Ordering::Release);
        return;
    };

    pane.perform_actions(actions);
    histogram!("send_actions_to_mux.perform_actions.latency").record(start.elapsed());
    output.finish();
    histogram!("send_actions_to_mux.rate").record(1.);
}

fn parse_buffered_data(
    pane: Weak<dyn Pane>,
    generation: Arc<PaneRegistrationGeneration>,
    dead: &Arc<AtomicBool>,
    mut rx: FileDescriptor,
) {
    let mut buf = vec![0; configuration().mux_output_parser_buffer_size];
    let mut parser = termwiz::escape::parser::Parser::new();
    let mut actions = vec![];
    let mut hold = SynchronizedOutputHold::default();
    let mut action_size: usize = 0;
    let mut delay = Duration::from_millis(configuration().mux_output_parser_coalesce_delay_ms);
    let mut deadline = None;

    loop {
        match rx.read(&mut buf) {
            Ok(size) if size == 0 => {
                dead.store(true, Ordering::Release);
                break;
            }
            Err(_) => {
                dead.store(true, Ordering::Release);
                break;
            }
            Ok(size) => {
                let mut chunk_touched_hold = hold.is_holding();
                let mut chunk_admission_emitted = false;
                parser.parse(&buf[0..size], |action| {
                    let was_holding = hold.is_holding();
                    let effect = handle_synchronized_output_action(&action, &mut hold, |hold| {
                        respond_to_synchronized_output_query(&pane, &generation, hold);
                    });
                    if was_holding || hold.is_holding() {
                        chunk_touched_hold = true;
                    }
                    if let Some(depth_outcome) = effect.depth_outcome {
                        if effect.flush {
                            if chunk_touched_hold && !chunk_admission_emitted && size > 0 {
                                notify_synchronized_output_event(
                                    &pane,
                                    &generation,
                                    SynchronizedOutputEvent::Admission {
                                        decision: SynchronizedOutputAdmissionDecision::Accepted,
                                        bytes: size as u64,
                                    },
                                );
                                chunk_admission_emitted = true;
                            }
                            notify_synchronized_output_event(
                                &pane,
                                &generation,
                                SynchronizedOutputEvent::Drain {
                                    cause: SynchronizedOutputDrainCause::Esu,
                                    bytes: action_size.saturating_add(size) as u64,
                                    depth_outcome: Some(depth_outcome),
                                    max_depth: hold.max_depth(),
                                },
                            );
                        } else {
                            notify_synchronized_output_event(
                                &pane,
                                &generation,
                                SynchronizedOutputEvent::Depth {
                                    outcome: depth_outcome,
                                    max_depth: hold.max_depth(),
                                },
                            );
                        }
                    } else if effect.handled {
                        notify_synchronized_output_event(
                            &pane,
                            &generation,
                            SynchronizedOutputEvent::ModeQuery,
                        );
                    }
                    if !was_holding && hold.is_holding() && !actions.is_empty() {
                        // Flush prior actions before entering BSU hold.
                        send_actions_to_mux(&pane, &generation, dead, std::mem::take(&mut actions));
                        action_size = 0;
                    }
                    if !effect.handled {
                        action.append_to(&mut actions);
                    }

                    if effect.flush && !actions.is_empty() {
                        send_actions_to_mux(&pane, &generation, dead, std::mem::take(&mut actions));
                        action_size = 0;
                    }
                });
                if chunk_touched_hold && !chunk_admission_emitted && size > 0 {
                    notify_synchronized_output_event(
                        &pane,
                        &generation,
                        SynchronizedOutputEvent::Admission {
                            decision: SynchronizedOutputAdmissionDecision::Accepted,
                            bytes: size as u64,
                        },
                    );
                }
                action_size += size;
                if hold.is_holding() && action_size >= max_held_synchronized_output_bytes() {
                    // A buggy app can enter synchronized-output mode and never
                    // send the reset sequence. Bound buffered memory in that case.
                    log::warn!(
                        "forcing synchronized-output flush after {} buffered bytes without reset",
                        action_size
                    );
                    hold.force_reset();
                    notify_synchronized_output_event(
                        &pane,
                        &generation,
                        SynchronizedOutputEvent::Drain {
                            cause: SynchronizedOutputDrainCause::Watchdog,
                            bytes: action_size as u64,
                            depth_outcome: None,
                            max_depth: hold.max_depth(),
                        },
                    );
                    if !actions.is_empty() {
                        send_actions_to_mux(&pane, &generation, dead, std::mem::take(&mut actions));
                    }
                    deadline = None;
                    action_size = 0;
                }
                if !actions.is_empty() && !hold.is_holding() {
                    // If we haven't accumulated too much data,
                    // pause for a short while to increase the chances
                    // that we coalesce a full "frame" from an unoptimized
                    // TUI program
                    if action_size < buf.len() {
                        let poll_delay = match deadline {
                            None => {
                                if let Some(target) = Instant::now().checked_add(delay) {
                                    deadline.replace(target);
                                    Some(delay)
                                } else {
                                    log::warn!(
                                        "mux output parser coalesce delay is too large for Instant; flushing without delay"
                                    );
                                    None
                                }
                            }
                            Some(target) => target.checked_duration_since(Instant::now()),
                        };
                        if poll_delay.is_some() {
                            let mut pfd = [pollfd {
                                fd: rx.as_socket_descriptor(),
                                events: POLLIN,
                                revents: 0,
                            }];
                            if let Ok(1) = poll(&mut pfd, poll_delay) {
                                // We can read now without blocking, so accumulate
                                // more data into actions
                                continue;
                            }

                            // Not readable in time: let the data we have flow into
                            // the terminal model
                        }
                    }

                    send_actions_to_mux(&pane, &generation, dead, std::mem::take(&mut actions));
                    deadline = None;
                    action_size = 0;
                }

                let config = configuration();
                buf.resize(config.mux_output_parser_buffer_size, 0);
                delay = Duration::from_millis(config.mux_output_parser_coalesce_delay_ms);
            }
        }
    }

    // Don't forget to send anything that we might have buffered
    // to be displayed before we return from here; this is important
    // for very short lived commands so that we don't forget to
    // display what they displayed.
    if !actions.is_empty() {
        send_actions_to_mux(&pane, &generation, dead, std::mem::take(&mut actions));
    }
}

fn set_socket_buffer(fd: &mut FileDescriptor, option: i32, size: usize) -> anyhow::Result<()> {
    let size = size as c_int;
    let socklen = std::mem::size_of_val(&size);
    unsafe {
        let res = libc::setsockopt(
            fd.as_socket_descriptor(),
            SOL_SOCKET,
            option,
            &size as *const c_int as *const _,
            socklen as _,
        );
        if res == 0 {
            Ok(())
        } else {
            Err(std::io::Error::last_os_error()).context("setsockopt")
        }
    }
}

fn allocate_socketpair() -> anyhow::Result<(FileDescriptor, FileDescriptor)> {
    let (mut tx, mut rx) = socketpair().context("socketpair")?;
    set_socket_buffer(&mut tx, SO_SNDBUF, mux_socket_buffer_size())
        .context("SO_SNDBUF")
        .ok();
    set_socket_buffer(&mut rx, SO_RCVBUF, mux_socket_buffer_size())
        .context("SO_RCVBUF")
        .ok();
    Ok((tx, rx))
}

fn finish_pane_reader_eof(
    pane: &Weak<dyn Pane>,
    generation: &Arc<PaneRegistrationGeneration>,
    pane_id: PaneId,
    exit_behavior: ExitBehavior,
) {
    let Some(_operation) = generation.try_acquire() else {
        return;
    };
    let Some(expected) = pane.upgrade() else {
        return;
    };
    let Some(mux) = resolve_pane_reader_mux(&expected, generation) else {
        return;
    };

    match exit_behavior {
        ExitBehavior::Hold | ExitBehavior::CloseOnCleanExit => {
            log::trace!("checking for dead windows after EOF on pane {}", pane_id);
            mux.prune_dead_windows();
        }
        ExitBehavior::Close => {
            mux.remove_pane_if_same_generation(pane_id, &expected, generation);
            mux.prune_dead_windows();
        }
    }
}

/// This function is run in a separate thread; its purpose is to perform
/// blocking reads from the pty (non-blocking reads are not portable to
/// all platforms and pty/tty types), parse the escape sequences and
/// relay the actions to the mux thread to apply them to the pane.
fn read_from_pane_pty(
    pane: Weak<dyn Pane>,
    generation: Arc<PaneRegistrationGeneration>,
    banner: Option<String>,
    mut reader: Box<dyn std::io::Read>,
    mut tx: FileDescriptor,
    dead: Arc<AtomicBool>,
    parser_done: std::sync::mpsc::Receiver<()>,
) {
    let mut buf = vec![0; mux_socket_buffer_size()];

    let pane_for_lifecycle = Weak::clone(&pane);
    let (pane_id, exit_behavior) = match pane.upgrade() {
        Some(pane) => (pane.pane_id(), pane.exit_behavior()),
        None => return,
    };

    if let Some(banner) = banner {
        tx.write_all(banner.as_bytes()).ok();
    }

    while !dead.load(Ordering::Acquire) {
        match reader.read(&mut buf) {
            Ok(size) if size == 0 => {
                log::trace!("read_pty EOF: pane_id {}", pane_id);
                break;
            }
            Err(err) => {
                error!("read_pty failed: pane {} {:?}", pane_id, err);
                break;
            }
            Ok(size) => {
                histogram!("read_from_pane_pty.bytes.rate").record(size as f64);
                log::trace!("read_pty pane {pane_id} read {size} bytes");
                if let Err(err) = tx.write_all(&buf[..size]) {
                    error!(
                        "read_pty failed to write to parser: pane {} {:?}",
                        pane_id, err
                    );
                    break;
                }
            }
        }
    }

    // Closing the writer lets the parser observe EOF and apply its final
    // buffered actions. Do not retire the generation until that final flush
    // completes; otherwise very short-lived commands can lose their last
    // frame when EOF races exact-generation cleanup.
    drop(tx);
    let _ = parser_done.recv();

    let exit_behavior = exit_behavior.unwrap_or_else(|| configuration().exit_behavior);
    if promise::spawn::is_scheduler_configured() {
        promise::spawn::spawn_into_main_thread(async move {
            finish_pane_reader_eof(&pane_for_lifecycle, &generation, pane_id, exit_behavior);
        })
        .detach();
    } else {
        finish_pane_reader_eof(&pane_for_lifecycle, &generation, pane_id, exit_behavior);
    }

    dead.store(true, Ordering::Release);
}

lazy_static::lazy_static! {
    static ref MUX: Mutex<Option<Arc<Mux>>> = Mutex::new(None);
}

#[cfg(test)]
pub(crate) static MUX_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

pub struct MuxWindowBuilder {
    window_id: WindowId,
    owner: Weak<Mux>,
    activity: Option<Activity>,
    provisional: bool,
    notified: bool,
}

impl MuxWindowBuilder {
    /// Releases an empty provisional window without publishing `WindowCreated`.
    ///
    /// Tmux control-mode domains retain a builder while their remote window
    /// topology materializes. Terminal cleanup must be able to release that
    /// activity lease without making an abandoned empty window visible. If
    /// the window gained a tab before cancellation, the mux preserves it and
    /// publishes `WindowCreated`; rollback callers must first detach every tab
    /// they provisionally attached.
    pub fn cancel(mut self) {
        self.notified = true;
        if self.provisional {
            if let Some(owner) = self.owner.upgrade() {
                owner.cancel_provisional_window(self.window_id, self.activity.take());
            }
        }
        drop(self.activity.take());
    }

    fn notify(&mut self) {
        if self.notified {
            return;
        }
        self.notified = true;
        let Some(activity) = self.activity.take() else {
            return;
        };
        let window_id = self.window_id;
        let Some(mux) = self.owner.upgrade() else {
            return;
        };
        // Keep creation in the same exact-owner FIFO as mutations that may
        // already have been queued for this window.  Retaining the Activity in
        // the queue entry also prevents pruning until subscribers have
        // observed the creation.  `flush_window_notifications` still delivers
        // synchronously on the mux thread (important for Wayland), and uses
        // the existing main-thread scheduler everywhere else.
        if mux.enqueue_window_created_if_present(window_id, activity) {
            mux.flush_window_notifications();
        }
    }
}

impl Drop for MuxWindowBuilder {
    fn drop(&mut self) {
        self.notify();
    }
}

impl std::ops::Deref for MuxWindowBuilder {
    type Target = WindowId;

    fn deref(&self) -> &WindowId {
        &self.window_id
    }
}

pub struct MuxWindowWriteGuard<'a> {
    guard: Option<MappedRwLockWriteGuard<'a, Window>>,
    mux: &'a Mux,
}

impl std::ops::Deref for MuxWindowWriteGuard<'_> {
    type Target = Window;

    fn deref(&self) -> &Window {
        self.guard
            .as_deref()
            .expect("mux window write guard remains live until drop")
    }
}

impl std::ops::DerefMut for MuxWindowWriteGuard<'_> {
    fn deref_mut(&mut self) -> &mut Window {
        self.guard
            .as_deref_mut()
            .expect("mux window write guard remains live until drop")
    }
}

impl Drop for MuxWindowWriteGuard<'_> {
    fn drop(&mut self) {
        drop(self.guard.take());
        self.mux.flush_window_notifications();
    }
}

impl Mux {
    pub fn new(default_domain: Option<Arc<dyn Domain>>) -> Self {
        let mut domains = HashMap::new();
        let mut domains_by_name = HashMap::new();
        if let Some(default_domain) = default_domain.as_ref() {
            domains.insert(default_domain.domain_id(), Arc::clone(default_domain));

            domains_by_name.insert(
                default_domain.domain_name().to_string(),
                Arc::clone(default_domain),
            );
        }

        let agent = if config::configuration().mux_enable_ssh_agent {
            Some(AgentProxy::new())
        } else {
            None
        };

        Self {
            topology: Mutex::new(MuxTopologyAuthority::new()),
            tabs: RwLock::new(HashMap::new()),
            tab_parents: RwLock::new(HashMap::new()),
            #[cfg(test)]
            tab_parent_lookup_probes: AtomicUsize::new(0),
            #[cfg(test)]
            tab_parent_write_cuts: AtomicUsize::new(0),
            pane_location_cache: RwLock::new(HashMap::new()),
            #[cfg(test)]
            pane_location_cache_hits: AtomicUsize::new(0),
            #[cfg(test)]
            pane_location_full_scans: AtomicUsize::new(0),
            #[cfg(test)]
            pane_location_scan_tab_probes: AtomicUsize::new(0),
            panes: RwLock::new(HashMap::new()),
            pane_retirements: Arc::new(PaneRetirementTracker::default()),
            pane_preparations: Mutex::new(HashMap::new()),
            pane_registration: Mutex::new(()),
            retiring_pane_ids: Mutex::new(HashSet::new()),
            pane_removal_cleanup_fences: Mutex::new(HashMap::new()),
            pane_removal_cleanup_outstanding_leases: AtomicUsize::new(0),
            pending_pane_lifecycle: Mutex::new(PendingPaneLifecycleNotifications::default()),
            windows: RwLock::new(HashMap::new()),
            provisional_windows: Mutex::new(HashSet::new()),
            pending_window_notifications: Mutex::new(PendingWindowNotifications::default()),
            window_order_receipts: Mutex::new(WindowOrderReceiptLedger::new()),
            activity_count: Arc::new(AtomicUsize::new(0)),
            activity_prune_state: Arc::new(ActivityPruneState::default()),
            default_domain: RwLock::new(default_domain),
            domains_by_name: RwLock::new(domains_by_name),
            domains: RwLock::new(domains),
            domain_registration: Mutex::new(()),
            retired_domain_ids: Mutex::new(HashSet::new()),
            subscribers: RwLock::new(HashMap::new()),
            pending_pane_output: Mutex::new(PendingPaneOutputNotifications::default()),
            pane_output_drain_scheduled: AtomicBool::new(false),
            #[cfg(test)]
            pane_reader_preparation_fault: Mutex::new(None),
            #[cfg(test)]
            pane_count_recomputes: AtomicUsize::new(0),
            banner: RwLock::new(None),
            clients: RwLock::new(HashMap::new()),
            reliable_input: Mutex::new(ReliableInputLedger::new()),
            identity: RwLock::new(None),
            num_panes_by_workspace: RwLock::new(HashMap::new()),
            main_thread_id: std::thread::current().id(),
            agent,
            last_high_rate_alert: Mutex::new(HashMap::new()),
        }
    }

    fn get_default_workspace(&self) -> String {
        let config = configuration();
        config
            .default_workspace
            .as_deref()
            .unwrap_or(DEFAULT_WORKSPACE)
            .to_string()
    }

    pub fn is_main_thread(&self) -> bool {
        std::thread::current().id() == self.main_thread_id
    }

    fn recompute_pane_count(&self) {
        #[cfg(test)]
        self.pane_count_recomputes.fetch_add(1, Ordering::Relaxed);
        let mut count = HashMap::new();
        for window in self.windows.read().values() {
            let workspace = window.get_workspace();
            for tab in window.iter() {
                *count.entry(workspace.to_string()).or_insert(0) += match tab.count_panes() {
                    Some(n) => n,
                    None => {
                        // Busy: abort this and we'll retry later
                        return;
                    }
                };
            }
        }
        *self.num_panes_by_workspace.write() = count;
    }

    /// Record input only when `client_id` is the exact live registration.
    ///
    /// `ClientId` values are reusable across reconnects, so value equality is
    /// insufficient for deferred connection work. The `Arc` allocation is the
    /// process-local registration token.
    pub fn client_had_input_if_same(&self, client_id: &Arc<ClientId>) -> bool {
        let updated = {
            let mut clients = self.clients.write();
            clients
                .get_mut(client_id.as_ref())
                .filter(|info| Arc::ptr_eq(&info.client_id, client_id))
                .is_some_and(|info| {
                    info.update_last_input();
                    true
                })
        };
        if updated {
            if let Some(agent) = &self.agent {
                agent.update_target();
            }
        }
        updated
    }

    pub fn record_input_for_current_identity(&self) {
        if let Some(ident) = self.identity.read().clone() {
            let _ = self.client_had_input_if_same(&ident);
        }
    }

    pub fn record_focus_for_current_identity(&self, pane_id: PaneId) {
        if let Some(ident) = self.identity.read().clone() {
            let _ = self.record_focus_for_client(&ident, pane_id);
        }
    }

    pub fn resolve_focused_pane(
        &self,
        client_id: &Arc<ClientId>,
    ) -> Option<(DomainId, WindowId, TabId, PaneId)> {
        let registration = {
            self.clients
                .read()
                .get(client_id.as_ref())
                .filter(|info| Arc::ptr_eq(&info.client_id, client_id))?
                .focused_pane_registration()?
        };
        registration
            .try_with_current(|current| {
                let pane_id = current.pane_id();
                let (domain, window, tab) = self.resolve_pane_id(pane_id)?;
                Some((domain, window, tab, pane_id))
            })
            .flatten()
    }

    pub fn record_focus_for_client(&self, client_id: &Arc<ClientId>, pane_id: PaneId) -> bool {
        if let Some(registration) = self.capture_current_pane(pane_id) {
            return registration
                .try_with_current(|current| current.record_focus_for_client(client_id))
                .unwrap_or(false);
        }
        false
    }

    /// Record focus only for the exact live client and pane registrations.
    fn record_focus_for_client_registration_if_same(
        &self,
        client_id: &Arc<ClientId>,
        registration: &PaneRegistrationHandle,
        pane: &dyn Pane,
    ) -> bool {
        self.update_client_focus(
            client_id,
            registration.pane_id(),
            Some((registration, pane)),
        )
    }

    #[cfg(test)]
    fn record_focus_for_client_if_same(&self, client_id: &Arc<ClientId>, pane_id: PaneId) -> bool {
        let Some(registration) = self.capture_current_pane(pane_id) else {
            return false;
        };
        registration
            .try_with_current(|current| current.record_focus_for_client(client_id))
            .unwrap_or(false)
    }

    fn update_client_focus(
        &self,
        exact_client: &Arc<ClientId>,
        pane_id: PaneId,
        target: Option<(&PaneRegistrationHandle, &dyn Pane)>,
    ) -> bool {
        let Some((target_registration, target_pane)) = target else {
            return false;
        };
        let Some((prior, same_registration)) = self
            .replace_client_focus_metadata_for_registration_if_same(
                exact_client,
                pane_id,
                target_registration,
            )
        else {
            return false;
        };

        if same_registration {
            return true;
        }
        if let Some(prior) = prior {
            let _ = prior.try_with_current(|current| current.focus_changed(false));
        }
        target_pane.focus_changed(true);
        true
    }

    /// Replace client focus metadata only while one exact pane registration
    /// remains published.
    ///
    /// The pane-registration serializer closes the retirement/check/store
    /// window: removal either wins first and this fails, or wins afterward and
    /// its cleanup clears the metadata installed here.
    fn replace_client_focus_metadata_for_registration_if_same(
        &self,
        client_id: &Arc<ClientId>,
        pane_id: PaneId,
        target: &PaneRegistrationHandle,
    ) -> Option<(Option<PaneRegistrationHandle>, bool)> {
        let _registration = self.pane_registration.lock();
        let remains_current = self.panes.read().get(&pane_id).is_some_and(|registered| {
            target.same_registration(&PaneRegistrationHandle::new(
                &registered.pane,
                &registered.generation,
            ))
        });
        if !remains_current {
            return None;
        }
        self.replace_client_focus_metadata_if_same(client_id, pane_id, Some(target))
    }

    /// Replace only the process-local client focus projection.
    ///
    /// No pane callback or mux notification is emitted here. Callers that
    /// commit topology focus execute those effects exactly once through the
    /// topology transition; metadata-only callers layer their own callbacks
    /// after this lock has been released.
    fn replace_client_focus_metadata_if_same(
        &self,
        client_id: &Arc<ClientId>,
        pane_id: PaneId,
        target: Option<&PaneRegistrationHandle>,
    ) -> Option<(Option<PaneRegistrationHandle>, bool)> {
        let mut clients = self.clients.write();
        let info = clients
            .get_mut(client_id.as_ref())
            .filter(|info| Arc::ptr_eq(&info.client_id, client_id))?;
        let prior = info.focused_pane_registration();
        let same_registration = prior
            .as_ref()
            .zip(target)
            .is_some_and(|(prior, target)| prior.same_registration(target));
        info.replace_focused_pane(pane_id, target.cloned());
        Some((prior, same_registration))
    }

    /// Called by PaneFocused event handlers to reconcile a remote
    /// pane focus event and apply its effects locally
    pub fn focus_pane_and_containing_tab(&self, pane_id: PaneId) -> anyhow::Result<()> {
        let registration = self
            .capture_current_pane(pane_id)
            .ok_or_else(|| anyhow!("pane {pane_id} not found"))?;
        registration
            .try_with_current(|current| current.focus_pane_and_containing_tab())
            .ok_or_else(|| anyhow!("pane registration {pane_id} is no longer current"))?
    }

    /// Focus one exact pane instance and the exact tab that currently owns it.
    ///
    /// This identity-preserving form is intended for callers that already
    /// resolved an `Arc<dyn Pane>` and must not be redirected to a same-id
    /// successor before the focus operation reaches mux authority.
    pub fn focus_exact_pane_and_containing_tab(&self, pane: &Arc<dyn Pane>) -> anyhow::Result<()> {
        let pane_id = pane.pane_id();
        let registration = self
            .capture_current_pane(pane_id)
            .ok_or_else(|| anyhow!("pane {pane_id} is not registered"))?;
        registration
            .try_with_current(|current| {
                anyhow::ensure!(
                    current.is_same_pane(pane),
                    "pane {pane_id} was replaced before exact focus"
                );
                self.focus_exact_pane_and_containing_tab_registered(pane_id, pane)
            })
            .ok_or_else(|| anyhow!("pane registration {pane_id} is no longer current"))?
    }

    fn focus_exact_pane_and_containing_tab_registered(
        &self,
        pane_id: PaneId,
        pane: &Arc<dyn Pane>,
    ) -> anyhow::Result<()> {
        let (window_id, tab) = {
            let windows = self.windows.read();
            windows
                .iter()
                .find_map(|(window_id, window)| {
                    window
                        .iter()
                        .find(|tab| {
                            tab.iter_all_panes()
                                .iter()
                                .any(|candidate| Arc::ptr_eq(candidate, pane))
                        })
                        .map(|tab| (*window_id, Arc::clone(tab)))
                })
                .ok_or_else(|| anyhow!("can't find exact pane {pane_id} in the mux topology"))?
        };

        self.activate_tab_exact_in_window(window_id, &tab, true)?;

        // Focus/activate the pane locally
        anyhow::ensure!(
            tab.set_active_pane_for_mux(pane, self),
            "exact pane {pane_id} was not accepted as active by tab {}",
            tab.tab_id(),
        );

        Ok(())
    }

    pub fn register_client(&self, client_id: Arc<ClientId>) {
        let reliable_input_prepared = self.reliable_input.lock().prepare_client(&client_id);
        if !reliable_input_prepared {
            metrics::counter!("mux.reliable_input.client_admission", "outcome" => "capacity")
                .increment(1);
        }
        let replaced = {
            let mut clients = self.clients.write();
            if clients
                .get(client_id.as_ref())
                .is_some_and(|info| Arc::ptr_eq(&info.client_id, &client_id))
            {
                return;
            }
            clients.insert(
                (*client_id).clone(),
                ClientInfo::new(Arc::clone(&client_id)),
            )
        };
        let Some(replaced) = replaced else {
            return;
        };

        let mut identity = self.identity.write();
        if identity
            .as_ref()
            .is_some_and(|current| Arc::ptr_eq(current, &replaced.client_id))
        {
            *identity = None;
        }
    }

    pub fn client_registration_is_current(&self, client_id: &Arc<ClientId>) -> bool {
        self.clients
            .read()
            .get(client_id.as_ref())
            .is_some_and(|info| Arc::ptr_eq(&info.client_id, client_id))
    }

    /// Claim the sole pending reliable key transition for an exact registered
    /// client and pane generation. This method invokes no pane callback and
    /// retains no mux lock in the returned permit.
    pub fn claim_reliable_key_event(
        &self,
        client_id: Option<&Arc<ClientId>>,
        registration: &PaneRegistrationHandle,
        input_serial: u64,
        kind: ReliableInputKeyKind,
        event: &KeyEvent,
    ) -> ReliableInputClaimOutcome {
        let Some(client_id) = client_id else {
            return ReliableInputClaimOutcome::ClientNotRegistered;
        };
        if !self.client_registration_is_current(client_id) {
            return ReliableInputClaimOutcome::ClientRegistrationRetired;
        }
        let Some(client_ledger) = self
            .reliable_input
            .lock()
            .clients
            .get(client_id.as_ref())
            .cloned()
        else {
            return ReliableInputClaimOutcome::ClientLedgerUnavailable;
        };
        let mut ledger = client_ledger.lock();
        if let Some(pending) = &ledger.pending {
            return if pending.fingerprint.input_serial < input_serial {
                ReliableInputClaimOutcome::DuplicatePending
            } else if pending.fingerprint.input_serial > input_serial {
                ReliableInputClaimOutcome::StaleSerial
            } else if pending
                .fingerprint
                .matches(registration, input_serial, kind, event)
            {
                ReliableInputClaimOutcome::DuplicatePending
            } else {
                ReliableInputClaimOutcome::IdentityConflict
            };
        }
        if let Some(terminal) = &ledger.terminal {
            if terminal.fingerprint.input_serial > input_serial {
                return ReliableInputClaimOutcome::StaleSerial;
            }
            if terminal.fingerprint.input_serial == input_serial {
                if !terminal
                    .fingerprint
                    .matches(registration, input_serial, kind, event)
                {
                    return ReliableInputClaimOutcome::IdentityConflict;
                }
                return match terminal.outcome {
                    ReliableInputTerminalOutcome::Applied => {
                        ReliableInputClaimOutcome::DuplicateApplied
                    }
                    ReliableInputTerminalOutcome::OutcomeUnknown => {
                        ReliableInputClaimOutcome::OutcomeUnknown
                    }
                };
            }
        }
        let Some(claim_id) = ledger.next_claim_id else {
            return ReliableInputClaimOutcome::IdentityAuthorityExhausted;
        };
        ledger.next_claim_id = claim_id.get().checked_add(1).and_then(NonZeroU64::new);
        ledger.pending = Some(ReliableInputPending {
            claim_id,
            fingerprint: ReliableInputFingerprint {
                registration: registration.clone(),
                input_serial,
                kind,
                event: event.clone(),
            },
            side_effect_started: false,
        });
        drop(ledger);
        ReliableInputClaimOutcome::Execute(ReliableInputCommitPermit {
            client: client_ledger,
            claim_id,
            armed: true,
        })
    }

    pub fn iter_clients(&self) -> Vec<ClientInfo> {
        self.clients
            .read()
            .values()
            .map(ClientInfo::wire_snapshot)
            .collect()
    }

    /// Returns a list of the unique workspace names known to the mux.
    /// This is taken from all known windows.
    pub fn iter_workspaces(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .windows
            .read()
            .values()
            .map(|w| w.get_workspace().to_string())
            .collect();
        names.sort();
        names.dedup();
        names
    }

    /// Generate a new unique workspace name
    pub fn generate_workspace_name(&self) -> String {
        let used = self.iter_workspaces();
        for candidate in names::Generator::default() {
            if !used.contains(&candidate) {
                return candidate;
            }
        }
        unreachable!();
    }

    /// Returns the effective active workspace name
    pub fn active_workspace(&self) -> String {
        self.identity
            .read()
            .clone()
            .and_then(|ident| self.active_workspace_for_client_if_same(&ident))
            .unwrap_or_else(|| self.get_default_workspace())
    }

    /// Returns the effective active workspace name for a given client
    pub fn active_workspace_for_client(&self, ident: &Arc<ClientId>) -> String {
        self.active_workspace_for_client_if_same(ident)
            .unwrap_or_else(|| self.get_default_workspace())
    }

    fn active_workspace_for_client_if_same(&self, ident: &Arc<ClientId>) -> Option<String> {
        self.clients
            .read()
            .get(ident.as_ref())
            .filter(|info| Arc::ptr_eq(&info.client_id, ident))
            .and_then(|info| info.active_workspace.clone())
    }

    pub fn set_active_workspace_for_client(&self, ident: &Arc<ClientId>, workspace: &str) {
        let _ = self.set_active_workspace_for_client_if_same(ident, workspace);
    }

    /// Assign a workspace only when `ident` is the exact live registration.
    pub fn set_active_workspace_for_client_if_same(
        &self,
        ident: &Arc<ClientId>,
        workspace: &str,
    ) -> bool {
        let changed = {
            let mut clients = self.clients.write();
            clients
                .get_mut(ident.as_ref())
                .filter(|info| Arc::ptr_eq(&info.client_id, ident))
                .is_some_and(|info| {
                    info.active_workspace.replace(workspace.to_string());
                    true
                })
        };
        if changed {
            self.notify(MuxNotification::ActiveWorkspaceChanged(ident.clone()));
        }
        changed
    }

    /// Assigns the active workspace name for the current identity
    pub fn set_active_workspace(&self, workspace: &str) {
        if let Some(ident) = self.identity.read().clone() {
            let _ = self.set_active_workspace_for_client_if_same(&ident, workspace);
        }
    }

    pub fn rename_workspace(&self, old_workspace: &str, new_workspace: &str) {
        if old_workspace == new_workspace {
            return;
        }

        let rename_notification = {
            let mut windows = self.windows.write();
            for window in windows.values_mut() {
                if window.get_workspace() == old_workspace {
                    window.set_workspace(new_workspace);
                }
            }
            self.envelope_notification(MuxNotification::WorkspaceRenamed {
                old_workspace: old_workspace.to_string(),
                new_workspace: new_workspace.to_string(),
            })
        };
        self.flush_window_notifications();
        self.dispatch_notification_envelope(rename_notification);
        self.recompute_pane_count();
        let changed_clients = {
            let mut clients = self.clients.write();
            clients
                .values_mut()
                .filter_map(|client| {
                    if client.active_workspace.as_deref() == Some(old_workspace) {
                        client.active_workspace.replace(new_workspace.to_string());
                        Some(Arc::clone(&client.client_id))
                    } else {
                        None
                    }
                })
                .collect::<Vec<_>>()
        };
        for client_id in changed_clients {
            self.notify(MuxNotification::ActiveWorkspaceChanged(client_id));
        }
    }

    /// Overrides the current client identity.
    /// Returns `IdentityHolder` which will restore the prior identity
    /// when it is dropped.
    /// This can be used to change the identity for the duration of a block.
    pub fn with_identity(self: &Arc<Self>, id: Option<Arc<ClientId>>) -> IdentityHolder {
        let prior = self.replace_identity(id);
        IdentityHolder {
            owner: Arc::downgrade(self),
            prior,
        }
    }

    /// Replace the identity, returning the prior identity
    pub fn replace_identity(&self, id: Option<Arc<ClientId>>) -> Option<Arc<ClientId>> {
        std::mem::replace(&mut *self.identity.write(), id)
    }

    /// Returns the active identity
    pub fn active_identity(&self) -> Option<Arc<ClientId>> {
        self.identity.read().clone()
    }

    pub fn unregister_client(&self, client_id: &Arc<ClientId>) {
        let _ = self.unregister_client_if_same(client_id);
    }

    pub fn unregister_client_if_same(&self, client_id: &Arc<ClientId>) -> bool {
        let removed = {
            let mut clients = self.clients.write();
            let owns_registration = clients
                .get(client_id.as_ref())
                .is_some_and(|info| Arc::ptr_eq(&info.client_id, client_id));
            if owns_registration {
                clients.remove(client_id.as_ref());
                true
            } else {
                false
            }
        };
        if !removed {
            return false;
        }

        let mut identity = self.identity.write();
        if identity
            .as_ref()
            .is_some_and(|current| Arc::ptr_eq(current, client_id))
        {
            *identity = None;
        }
        true
    }

    pub fn subscribe<F>(&self, subscriber: F) -> Result<usize, IdAllocationError>
    where
        F: Fn(MuxNotification) -> bool + 'static + Send + Sync,
    {
        self.subscribe_with_topology(move |envelope| subscriber(envelope.notification))
    }

    /// Subscribe with an exact cleanup lease for each authoritative
    /// `PaneRemoved` notification.
    ///
    /// The lease is minted inside the synchronous subscriber wrapper while the
    /// mux still owns that removal generation's reuse fence. Consumers that
    /// defer cleanup must move the lease with their queued work; dropping it
    /// releases that consumer's share of the fence. While any lease remains,
    /// same-ID registration fails with `PaneIdCollision` rather than waiting;
    /// callers performing deliberate hot replacement must retry after cleanup.
    pub fn subscribe_with_pane_removal_cleanup<F>(
        self: &Arc<Self>,
        subscriber: F,
    ) -> Result<usize, IdAllocationError>
    where
        F: Fn(MuxNotification, Option<PaneRemovalCleanupLease>) -> bool + 'static + Send + Sync,
    {
        let owner = Arc::downgrade(self);
        self.subscribe(move |notification| {
            let cleanup = match &notification {
                MuxNotification::PaneRemoved(pane_id) => owner
                    .upgrade()
                    .and_then(|mux| mux.acquire_pane_removal_cleanup_lease(*pane_id)),
                _ => None,
            };
            subscriber(notification, cleanup)
        })
    }

    /// Subscribe to mux notifications together with their topology authority.
    ///
    /// Existing in-process consumers should normally use [`Self::subscribe`].
    /// Protocol bridges use this form so topology events retain the exact
    /// revision reserved at publication rather than reconstructing order from
    /// callback timing.
    pub fn subscribe_with_topology<F>(&self, subscriber: F) -> Result<usize, IdAllocationError>
    where
        F: Fn(MuxNotificationEnvelope) -> bool + 'static + Send + Sync,
    {
        let sub_id = try_reserve_usize_ids(&SUB_ID, 1, "mux subscriber")?.start;
        self.subscribers
            .write()
            .insert(sub_id, Arc::new(subscriber));
        Ok(sub_id)
    }

    /// Atomically subscribe to the topology stream and capture its baseline.
    ///
    /// Holding the topology authority while publishing the subscriber prevents
    /// a revision from being reserved after the returned baseline without the
    /// new subscriber being visible to that publication. An older envelope
    /// that was reserved before this call may still arrive afterward; protocol
    /// consumers must discard revisions at or below the returned baseline.
    pub fn subscribe_with_topology_fence<F>(
        &self,
        subscriber: F,
    ) -> Result<(usize, MuxSessionIncarnation, TopologyRevision), TopologySubscriptionError>
    where
        F: Fn(MuxNotificationEnvelope) -> bool + 'static + Send + Sync,
    {
        let topology = self.topology.lock();
        let (session_incarnation, baseline_revision) = topology.snapshot()?;
        let sub_id = try_reserve_usize_ids(&SUB_ID, 1, "mux subscriber")?.start;
        self.subscribers
            .write()
            .insert(sub_id, Arc::new(subscriber));
        drop(topology);
        Ok((sub_id, session_incarnation, baseline_revision))
    }

    pub fn unsubscribe(&self, sub_id: usize) -> bool {
        self.subscribers.write().remove(&sub_id).is_some()
    }

    pub fn topology_snapshot_authority(
        &self,
    ) -> Result<(MuxSessionIncarnation, TopologyRevision), TopologyRevisionExhausted> {
        self.topology.lock().snapshot()
    }

    /// Reserve the authoritative topology stamp for `notification`.
    ///
    /// Topology mutations that must defer subscriber callbacks call this while
    /// their state lock is still held, then dispatch the returned envelope
    /// after releasing that lock. This keeps the revision reservation before
    /// state visibility without permitting callback re-entry into the
    /// mutation's critical section.
    pub(crate) fn envelope_notification(
        &self,
        notification: MuxNotification,
    ) -> MuxNotificationEnvelope {
        let topology = if notification.is_topology() {
            match self.topology.lock().reserve_revision() {
                Ok(revision) => MuxTopologyStamp::Revision(revision),
                Err(_) => MuxTopologyStamp::Exhausted,
            }
        } else {
            MuxTopologyStamp::NonTopology
        };
        MuxNotificationEnvelope {
            notification,
            topology,
        }
    }

    /// Publish a generic mux notification.
    ///
    /// Pane add/remove transitions are deliberately rejected here: only the
    /// serialized pane-registration lifecycle can mint their topology and
    /// deferred-cleanup authority.
    pub fn notify(&self, notification: MuxNotification) {
        if notification.requires_pane_lifecycle_authority() {
            metrics::counter!(
                "mux.notifications.pane_lifecycle.generic_publish_rejected",
                "variant" => match &notification {
                    MuxNotification::PaneAdded(_) => "pane_added",
                    MuxNotification::FloatingPaneSpawnCommitted(_) => {
                        "floating_pane_spawn_committed"
                    }
                    MuxNotification::PaneRemoved(_) => "pane_removed",
                    _ => unreachable!("lifecycle-authority predicate admitted another variant"),
                }
            )
            .increment(1);
            log::error!(
                "refusing forged pane lifecycle publication through generic Mux::notify; use the authoritative pane registration/removal path"
            );
            return;
        }

        // Dedupe high-rate Alert variants per (pane, kind) within
        // HIGH_RATE_ALERT_DEDUPE_WINDOW. Saves N_subscribers × clone +
        // box-allocation per dropped notification under bursty agent output.
        // See ft-18xgy.
        if let MuxNotification::Alert { pane_id, alert } = &notification {
            let kind = match alert {
                frankenterm_term::Alert::CurrentWorkingDirectoryChanged => {
                    Some(HighRateAlertKind::CurrentWorkingDirectoryChanged)
                }
                frankenterm_term::Alert::OutputSinceFocusLost => {
                    Some(HighRateAlertKind::OutputSinceFocusLost)
                }
                _ => None,
            };
            if let Some(kind) = kind {
                let now = Instant::now();
                let key = (*pane_id, kind);
                let mut last = self.last_high_rate_alert.lock();
                // Dedup check first so the deduped path doesn't pay the
                // O(map_size) prune cost. `saturating_duration_since` keeps
                // the comparison safe under non-monotonic clock anomalies
                // (rare, but Instant on Windows isn't strictly monotonic).
                if let Some(prev) = last.get(&key) {
                    if now.saturating_duration_since(*prev) < HIGH_RATE_ALERT_DEDUPE_WINDOW {
                        histogram!("mux.notifications.high_rate_alert.deduped").record(1.);
                        return;
                    }
                }
                // Only on the insert path: best-effort prune of stale entries.
                // With <100 panes per host the map stays trivially small.
                last.retain(|_, ts| {
                    now.saturating_duration_since(*ts) < HIGH_RATE_ALERT_PRUNE_AFTER
                });
                last.insert(key, now);
            }
        }

        match notification {
            MuxNotification::PaneOutput(pane_id) => self.enqueue_pane_output_notification(pane_id),
            notification => self.dispatch_notification(notification),
        }
    }

    fn dispatch_notification(&self, notification: MuxNotification) {
        let envelope = self.envelope_notification(notification);
        self.dispatch_notification_envelope(envelope);
    }

    fn bind_window_notification_owner(self: &Arc<Self>) {
        let mut pending = self.pending_window_notifications.lock();
        if let Some(owner) = pending.owner.upgrade() {
            assert!(
                Arc::ptr_eq(&owner, self),
                "a mux window-notification queue must retain one exact owner",
            );
            return;
        }
        pending.owner = Arc::downgrade(self);
    }

    pub(crate) fn enqueue_window_notification(self: &Arc<Self>, notification: MuxNotification) {
        self.enqueue_window_notification_entry(notification, None);
    }

    #[cfg(test)]
    pub(crate) fn enqueue_window_focus_lost(self: &Arc<Self>, pane: Arc<dyn Pane>) {
        self.bind_window_notification_owner();
        let mut pending = self.pending_window_notifications.lock();
        pending
            .queue
            .push_back(PendingWindowAction::FocusLost(pane));
    }

    fn enqueue_window_created_if_present(
        self: &Arc<Self>,
        window_id: WindowId,
        activity: Activity,
    ) -> bool {
        self.bind_window_notification_owner();
        let windows = self.windows.read();
        if !windows.contains_key(&window_id) {
            self.provisional_windows.lock().remove(&window_id);
            return false;
        }
        if !self.provisional_windows.lock().remove(&window_id) {
            return false;
        }
        // Queue while the window map read lock still prevents an explicit
        // remover from publishing WindowRemoved first. If removal already won,
        // suppress both sides of the unpublished provisional lifecycle.
        self.queue_window_notification_entry(
            MuxNotification::WindowCreated(window_id),
            Some(activity),
        );
        true
    }

    fn enqueue_window_notification_entry(
        self: &Arc<Self>,
        notification: MuxNotification,
        activity: Option<Activity>,
    ) {
        self.bind_window_notification_owner();
        self.queue_window_notification_entry(notification, activity);
    }

    /// Queue a window event after the owning [`Arc<Mux>`] has been bound.
    ///
    /// A number of mux APIs intentionally accept `&Mux` because exact pane
    /// generation handles borrow the mux without retaining it.  Every window
    /// is nevertheless created by `new_empty_window`, which binds this queue
    /// before publishing the window.  Keeping the non-retaining enqueue path
    /// lets those APIs participate in the same FIFO without consulting the
    /// mutable process-global mux singleton.
    fn queue_window_notification(&self, notification: MuxNotification) {
        self.queue_window_notification_entry(notification, None);
    }

    fn queue_window_notification_entry(
        &self,
        notification: MuxNotification,
        activity: Option<Activity>,
    ) {
        let envelope = self.envelope_notification(notification);
        self.queue_window_notification_envelope(envelope, activity);
    }

    /// Queue an envelope whose topology revision was reserved by the same
    /// transaction that froze its payload.
    fn queue_window_notification_envelope(
        &self,
        envelope: MuxNotificationEnvelope,
        activity: Option<Activity>,
    ) {
        let mut pending = self.pending_window_notifications.lock();
        assert!(
            std::ptr::eq(pending.owner.as_ptr(), self as *const Self),
            "window notifications require the exact owner to be bound before publication",
        );
        pending
            .queue
            .push_back(PendingWindowAction::Notification { envelope, activity });
    }

    /// Commit one allocation-prepared mutation spanning one or more windows.
    ///
    /// Lock order is `windows -> tab_parents -> topology ->
    /// pending_window_notifications -> provisional_windows` when
    /// created/removed window receipts require the final lock. Callers already
    /// hold the window-map write guard. A caller retaining
    /// `provisional_windows` may commit a state-only transaction only when both
    /// receipt lists are empty. Every fallible allocation and revision check
    /// completes before the first window or parent-index field changes; the
    /// post-reservation suffix contains only exact lookups, swaps, scalar
    /// assignments, and pushes into pre-reserved capacity.
    fn commit_prepared_window_states_locked(
        &self,
        windows: &mut HashMap<WindowId, Window>,
        mut prepared: Vec<(WindowId, PreparedWindowState)>,
        attached_tabs: Vec<(TabId, WindowId)>,
        mut created_windows: Vec<WindowId>,
        mut removed_windows: Vec<WindowId>,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(!prepared.is_empty(), "window transaction is empty");
        prepared.sort_unstable_by_key(|(window_id, _)| *window_id);
        anyhow::ensure!(
            prepared.windows(2).all(|pair| pair[0].0 != pair[1].0),
            "window transaction names one window more than once"
        );
        for (window_id, state) in &prepared {
            anyhow::ensure!(
                windows.contains_key(window_id),
                "window {window_id} left the mux before prepared commit"
            );
            anyhow::ensure!(
                state.frozen().window_id() == *window_id,
                "prepared window state identity mismatch"
            );
        }
        removed_windows.sort_unstable();
        created_windows.sort_unstable();
        anyhow::ensure!(
            created_windows.windows(2).all(|pair| pair[0] != pair[1]),
            "window transaction creates one window twice"
        );
        anyhow::ensure!(
            removed_windows.windows(2).all(|pair| pair[0] != pair[1]),
            "window transaction removes one window twice"
        );
        anyhow::ensure!(
            removed_windows.iter().all(|window_id| prepared
                .binary_search_by_key(window_id, |(prepared_id, _)| *prepared_id)
                .is_ok()),
            "removed windows must retain one prepared final state"
        );
        anyhow::ensure!(
            removed_windows.iter().all(|window_id| prepared
                .binary_search_by_key(window_id, |(prepared_id, _)| *prepared_id)
                .ok()
                .and_then(|index| prepared.get(index))
                .is_some_and(|(_, state)| state.frozen().ordered_tabs().is_empty())),
            "removed windows must have an empty prepared final state"
        );
        anyhow::ensure!(
            created_windows.iter().all(|window_id| prepared
                .binary_search_by_key(window_id, |(prepared_id, _)| *prepared_id)
                .is_ok()),
            "created windows must retain one prepared final state"
        );
        let parentage_changed = !removed_windows.is_empty()
            || prepared.iter().any(|(_, state)| state.membership_changed());
        anyhow::ensure!(
            parentage_changed || attached_tabs.is_empty(),
            "membership-preserving window transaction cannot publish tab attachments"
        );
        let mut prepared_window_ids = HashSet::new();
        if parentage_changed {
            prepared_window_ids
                .try_reserve(prepared.len())
                .map_err(|error| anyhow!("reserve prepared window identities: {error}"))?;
            for (window_id, _) in &prepared {
                debug_assert!(prepared_window_ids.insert(*window_id));
            }
        }

        let (prior_tab_count, final_tab_count) = if parentage_changed {
            let prior = prepared.iter().try_fold(0usize, |count, (window_id, _)| {
                count
                    .checked_add(
                        windows
                            .get(window_id)
                            .expect("prepared window presence validated above")
                            .len(),
                    )
                    .ok_or_else(|| anyhow!("prior window tab count overflow"))
            })?;
            let final_count = prepared
                .iter()
                .try_fold(0usize, |count, (window_id, state)| {
                    if removed_windows.binary_search(window_id).is_ok() {
                        Ok(count)
                    } else {
                        count
                            .checked_add(state.frozen().ordered_tabs().len())
                            .ok_or_else(|| anyhow!("final window tab count overflow"))
                    }
                })?;
            (prior, final_count)
        } else {
            (0, 0)
        };
        let mut prior_tabs = Vec::new();
        prior_tabs
            .try_reserve_exact(prior_tab_count)
            .map_err(|error| anyhow!("reserve prior tab-parent transaction: {error}"))?;
        let mut prior_tab_memberships = HashSet::new();
        prior_tab_memberships
            .try_reserve(prior_tab_count)
            .map_err(|error| anyhow!("reserve exact prior tab memberships: {error}"))?;
        let mut final_tab_ids = HashSet::new();
        final_tab_ids
            .try_reserve(final_tab_count)
            .map_err(|error| anyhow!("reserve final tab-parent identities: {error}"))?;
        let mut final_tab_memberships = HashSet::new();
        final_tab_memberships
            .try_reserve(final_tab_count)
            .map_err(|error| anyhow!("reserve exact final tab memberships: {error}"))?;
        if parentage_changed {
            for (window_id, _) in &prepared {
                prior_tabs.extend(
                    windows
                        .get(window_id)
                        .expect("prepared window presence validated above")
                        .iter()
                        .map(|tab| (*window_id, Arc::clone(tab))),
                );
            }
            for (window_id, tab) in &prior_tabs {
                anyhow::ensure!(
                    prior_tab_memberships.insert((*window_id, Arc::as_ptr(tab) as usize)),
                    "window {window_id} contains exact tab {} more than once",
                    tab.tab_id(),
                );
            }
            for (window_id, state) in &prepared {
                if removed_windows.binary_search(window_id).is_ok() {
                    continue;
                }
                for tab in state.frozen().ordered_tabs() {
                    anyhow::ensure!(
                        final_tab_ids.insert(tab.tab_id()),
                        "window transaction gives tab {} more than one final parent",
                        tab.tab_id()
                    );
                    debug_assert!(
                        final_tab_memberships.insert((*window_id, Arc::as_ptr(tab) as usize))
                    );
                }
            }
        }

        let mut frozen_states = Vec::new();
        frozen_states
            .try_reserve_exact(prepared.len())
            .map_err(|error| anyhow!("reserve frozen window states: {error}"))?;
        frozen_states.extend(
            prepared
                .iter()
                .filter(|(window_id, _)| removed_windows.binary_search(window_id).is_err())
                .map(|(_, state)| state.frozen().clone()),
        );
        let frozen = FrozenWindowTopologyChange::from_prepared(
            frozen_states,
            attached_tabs,
            created_windows.clone(),
            removed_windows.clone(),
        )?;
        let mut focus_lost = Vec::new();
        focus_lost
            .try_reserve_exact(prepared.len())
            .map_err(|error| anyhow!("reserve window focus callbacks: {error}"))?;

        #[cfg(test)]
        if parentage_changed {
            self.tab_parent_write_cuts.fetch_add(1, Ordering::Relaxed);
        }
        let mut tab_parents = parentage_changed.then(|| self.tab_parents.write());
        if let Some(tab_parents) = tab_parents.as_mut() {
            let parent_index_growth = final_tab_count.saturating_sub(prior_tab_count);
            tab_parents
                .try_reserve(parent_index_growth)
                .map_err(|error| anyhow!("reserve tab-parent index transaction: {error}"))?;
            for (window_id, tab) in &prior_tabs {
                anyhow::ensure!(
                    tab_parents
                        .get(&tab.tab_id())
                        .is_some_and(|parent| parent.matches(tab, *window_id)),
                    "tab {} parent index does not match exact membership in window {window_id}",
                    tab.tab_id()
                );
            }
            for (window_id, state) in &prepared {
                if removed_windows.binary_search(window_id).is_ok() {
                    continue;
                }
                for tab in state.frozen().ordered_tabs() {
                    if let Some(parent) = tab_parents.get(&tab.tab_id()) {
                        anyhow::ensure!(
                            parent.is_same_tab(tab)
                                && prepared_window_ids.contains(&parent.window_id)
                                && prior_tab_memberships
                                    .contains(&(parent.window_id, Arc::as_ptr(tab) as usize)),
                            "tab {} already has a different or untouched window parent",
                            tab.tab_id()
                        );
                    }
                }
            }
        }
        let mut topology = self.topology.lock();
        let mut pending = self.pending_window_notifications.lock();
        anyhow::ensure!(
            std::ptr::eq(pending.owner.as_ptr(), self as *const Self),
            "window topology transactions require the exact owner to be bound"
        );
        pending
            .queue
            .try_reserve(prepared.len().saturating_add(1))
            .map_err(|error| anyhow!("reserve frozen window transaction delivery: {error}"))?;
        let revision = topology.reserve_revision().map_err(anyhow::Error::new)?;

        for (window_id, state) in prepared {
            let window = windows
                .get_mut(&window_id)
                .expect("prepared window presence was validated under the same write guard");
            let (_committed, lost) = window.commit_prepared_state(state);
            if let Some(pane) = lost {
                focus_lost.push(pane);
            }
        }
        if let Some(tab_parents) = tab_parents.as_mut() {
            for (window_id, tab) in &prior_tabs {
                if !final_tab_memberships.contains(&(*window_id, Arc::as_ptr(tab) as usize)) {
                    let removed = tab_parents.remove(&tab.tab_id());
                    debug_assert!(removed.is_some_and(|parent| parent.matches(tab, *window_id)));
                }
            }
            for state in frozen.windows() {
                let window_id = state.window_id();
                for tab in state.ordered_tabs() {
                    if !prior_tab_memberships.contains(&(window_id, Arc::as_ptr(tab) as usize)) {
                        let replaced = tab_parents
                            .insert(tab.tab_id(), TabParentRegistration::new(tab, window_id));
                        debug_assert!(replaced.is_none());
                    }
                }
            }
        }
        if !removed_windows.is_empty() || !created_windows.is_empty() {
            let mut provisional = self.provisional_windows.lock();
            for window_id in removed_windows {
                let removed = windows.remove(&window_id);
                debug_assert!(removed.is_some());
                provisional.remove(&window_id);
            }
            for window_id in created_windows {
                provisional.remove(&window_id);
            }
        }
        for pane in focus_lost {
            pending
                .queue
                .push_back(PendingWindowAction::FocusLost(pane));
        }
        pending.queue.push_back(PendingWindowAction::Notification {
            envelope: MuxNotificationEnvelope {
                notification: MuxNotification::WindowTopologyChanged(frozen),
                topology: MuxTopologyStamp::Revision(revision),
            },
            activity: None,
        });
        Ok(())
    }

    fn flush_window_notifications(&self) {
        if !promise::spawn::is_scheduler_configured() || self.is_main_thread() {
            self.drain_window_notifications_inline();
            return;
        }

        let dispatch = {
            let mut pending = self.pending_window_notifications.lock();
            if pending.scheduled || pending.draining || pending.queue.is_empty() {
                return;
            }
            let Some(owner) = pending.owner.upgrade() else {
                pending.queue.clear();
                return;
            };
            pending.scheduled = true;
            WindowNotificationDispatch::new(Arc::downgrade(&owner))
        };
        promise::spawn::spawn_into_main_thread(async move {
            dispatch.execute();
        })
        .detach();
    }

    fn run_scheduled_window_notification_drain(&self) {
        self.pending_window_notifications.lock().scheduled = false;
        self.drain_window_notifications_inline();
    }

    fn window_notification_dispatch_dropped(&self) {
        self.pending_window_notifications.lock().scheduled = false;
        // The dispatch is dropped only after the mutation's window-map guard
        // has been released. Inline fallback preserves delivery if an
        // executor rejects or abandons the queued runnable.
        self.drain_window_notifications_inline();
    }

    fn drain_window_notifications_inline(&self) {
        let should_drain = {
            let mut pending = self.pending_window_notifications.lock();
            if pending.draining || pending.queue.is_empty() {
                false
            } else {
                pending.draining = true;
                true
            }
        };
        if !should_drain {
            return;
        }

        struct DrainReset<'a> {
            pending: &'a Mutex<PendingWindowNotifications>,
            armed: bool,
        }

        impl Drop for DrainReset<'_> {
            fn drop(&mut self) {
                if self.armed {
                    self.pending.lock().draining = false;
                }
            }
        }

        let mut reset = DrainReset {
            pending: &self.pending_window_notifications,
            armed: true,
        };
        loop {
            let action = {
                let mut pending = self.pending_window_notifications.lock();
                let action = pending.queue.pop_front();
                if action.is_none() {
                    pending.draining = false;
                    reset.armed = false;
                }
                action
            };
            let Some(action) = action else {
                return;
            };
            match action {
                PendingWindowAction::Notification { envelope, activity } => {
                    self.dispatch_notification_envelope(envelope);
                    // The optional Activity deliberately remains live through
                    // subscriber fanout, then releases only after the
                    // corresponding WindowCreated event has been observed.
                    drop(activity);
                }
                PendingWindowAction::FocusLost(pane) => {
                    if catch_recoverable(
                        RecoverablePanicSite::MuxPaneCallback,
                        std::panic::AssertUnwindSafe(|| pane.focus_changed(false)),
                    )
                    .is_err()
                    {
                        log::error!(
                            "pane focus-loss callback panicked for exact pane identity {:p}",
                            Arc::as_ptr(&pane)
                        );
                    }
                }
            }
        }
    }

    pub fn notify_from_any_thread(notification: MuxNotification) {
        if let MuxNotification::PaneOutput(pane_id) = notification {
            if let Some(mux) = Mux::try_get() {
                mux.enqueue_pane_output_notification(pane_id);
            }
            return;
        }
        if let Some(mux) = Mux::try_get() {
            if mux.is_main_thread() {
                mux.notify(notification);
                return;
            }
        }
        if promise::spawn::is_scheduler_configured() {
            promise::spawn::spawn_into_main_thread(async {
                if let Some(mux) = Mux::try_get() {
                    mux.notify(notification);
                }
            })
            .detach();
        }
    }

    // Callbacks are invoked without holding the subscribers lock. A callback
    // removed concurrently may still observe the current notification if it
    // was present in the snapshot; removals only affect future notifications.
    pub(crate) fn dispatch_notification_envelope(&self, envelope: MuxNotificationEnvelope) {
        if let MuxNotification::PaneRemoved(pane_id) = &envelope.notification {
            // Numeric pane identifiers may eventually be reused.  Drop the
            // retired generation's location promptly so long-running churn is
            // bounded by currently relevant pane IDs rather than historical
            // session cardinality.  Exact-Arc and revision checks already make
            // a retained entry safe; this is the memory-bound half.
            self.pane_location_cache.write().remove(pane_id);
        }
        let subscribers = self
            .subscribers
            .read()
            .iter()
            .map(|(id, subscriber)| (*id, Arc::clone(subscriber)))
            .collect::<Vec<_>>();
        histogram!("mux.notifications.subscriber_fanout").record(subscribers.len() as f64);

        let mut dead_subscribers = Vec::new();
        for (id, subscriber) in subscribers {
            match catch_recoverable(
                RecoverablePanicSite::MuxSubscriber,
                std::panic::AssertUnwindSafe(|| subscriber(envelope.clone())),
            ) {
                Ok(true) => {} // subscriber still alive
                Ok(false) => dead_subscribers.push(id),
                Err(_) => {
                    log::error!("mux subscriber {id} panicked — removing");
                    dead_subscribers.push(id);
                }
            }
        }

        if !dead_subscribers.is_empty() {
            let mut subscribers = self.subscribers.write();
            for id in dead_subscribers {
                subscribers.remove(&id);
            }
        }
    }

    /// Enqueue a pane lifecycle transition at the same linearization point as
    /// its topology mutation. The caller must hold `pane_registration`.
    ///
    /// The returned ticket keeps the event undispatchable until any required
    /// outside-lock work (notably `Pane::kill`) has completed.
    fn enqueue_pane_lifecycle_notification_locked(
        &self,
        notification: PaneLifecycleNotification,
        reader_start_gate: Option<PaneReaderStartGate>,
    ) -> PaneLifecycleNotificationTicket {
        self.enqueue_pane_lifecycle_notification_with_cleanup_locked(
            notification,
            reader_start_gate,
            None,
            PaneRemovalFollowUp::None,
        )
    }

    fn enqueue_pane_removal_notification_locked(
        &self,
        pane_id: PaneId,
        generation: &Arc<PaneRegistrationGeneration>,
        removal_follow_up: PaneRemovalFollowUp,
    ) -> PaneLifecycleNotificationTicket {
        self.enqueue_pane_lifecycle_notification_with_cleanup_locked(
            PaneLifecycleNotification::Removed(pane_id),
            None,
            Some(Arc::clone(&generation.cleanup_complete)),
            removal_follow_up,
        )
    }

    fn enqueue_pane_lifecycle_notification_with_cleanup_locked(
        &self,
        notification: PaneLifecycleNotification,
        reader_start_gate: Option<PaneReaderStartGate>,
        cleanup_complete: Option<Arc<AtomicBool>>,
        removal_follow_up: PaneRemovalFollowUp,
    ) -> PaneLifecycleNotificationTicket {
        let topology = self
            .envelope_notification(notification.clone().into())
            .topology;
        self.enqueue_pane_lifecycle_notification_at_topology_locked(
            notification,
            topology,
            reader_start_gate,
            cleanup_complete,
            removal_follow_up,
        )
    }

    /// Reserve every recoverably fallible allocation required to append one
    /// pane lifecycle event, retaining the queue lock until the caller commits
    /// and appends it.
    fn prepare_pane_lifecycle_enqueue(
        &self,
        pane_id: PaneId,
    ) -> anyhow::Result<PreparedPaneLifecycleEnqueue<'_>> {
        let ready = Arc::new(AtomicBool::new(false));
        let mut pending = self.pending_pane_lifecycle.lock();
        let vacant_queue = if let Some(queue) = pending.by_pane.get_mut(&pane_id) {
            queue
                .try_reserve(1)
                .map_err(|error| anyhow!("reserve pane {pane_id} lifecycle queue: {error}"))?;
            None
        } else {
            pending
                .by_pane
                .try_reserve(1)
                .map_err(|error| anyhow!("reserve pane lifecycle map: {error}"))?;
            let mut queue = VecDeque::new();
            queue
                .try_reserve(1)
                .map_err(|error| anyhow!("reserve pane {pane_id} lifecycle queue: {error}"))?;
            Some(queue)
        };
        if !pending.ready_set.contains(&pane_id) {
            pending
                .ready_panes
                .try_reserve(1)
                .map_err(|error| anyhow!("reserve pane lifecycle ready queue: {error}"))?;
            pending
                .ready_set
                .try_reserve(1)
                .map_err(|error| anyhow!("reserve pane lifecycle ready set: {error}"))?;
        }
        Ok(PreparedPaneLifecycleEnqueue {
            pending,
            pane_id,
            ready,
            vacant_queue,
        })
    }

    /// Reserve every recoverably fallible allocation needed to append one
    /// lifecycle edge for each distinct pane ID in `pane_ids`.
    ///
    /// The returned guard intentionally retains `pending_pane_lifecycle` so a
    /// concurrent producer cannot consume the reserved map or queue capacity
    /// before the enclosing multi-pane topology transaction commits.
    fn prepare_pane_lifecycle_batch_enqueue(
        &self,
        pane_ids: &[PaneId],
    ) -> anyhow::Result<PreparedPaneLifecycleBatchEnqueue<'_>> {
        let mut unique = HashSet::new();
        unique
            .try_reserve(pane_ids.len())
            .map_err(|error| anyhow!("reserve lifecycle batch identity set: {error}"))?;
        for &pane_id in pane_ids {
            anyhow::ensure!(
                unique.insert(pane_id),
                "pane {pane_id} appears more than once in one lifecycle batch"
            );
        }

        let mut ready_tokens = Vec::new();
        ready_tokens
            .try_reserve_exact(pane_ids.len())
            .map_err(|error| anyhow!("reserve lifecycle batch readiness tokens: {error}"))?;
        ready_tokens.extend(pane_ids.iter().map(|_| Arc::new(AtomicBool::new(false))));

        let mut entries = Vec::new();
        entries
            .try_reserve_exact(pane_ids.len())
            .map_err(|error| anyhow!("reserve lifecycle batch entries: {error}"))?;
        let mut tickets = Vec::new();
        tickets
            .try_reserve_exact(pane_ids.len())
            .map_err(|error| anyhow!("reserve lifecycle batch tickets: {error}"))?;

        let mut pending = self.pending_pane_lifecycle.lock();
        let absent = pane_ids
            .iter()
            .filter(|pane_id| !pending.by_pane.contains_key(pane_id))
            .count();
        pending
            .by_pane
            .try_reserve(absent)
            .map_err(|error| anyhow!("reserve lifecycle batch pane map: {error}"))?;
        let ready_needed = pane_ids
            .iter()
            .filter(|pane_id| !pending.ready_set.contains(pane_id))
            .count();
        pending
            .ready_panes
            .try_reserve(ready_needed)
            .map_err(|error| anyhow!("reserve lifecycle batch ready queue: {error}"))?;
        pending
            .ready_set
            .try_reserve(ready_needed)
            .map_err(|error| anyhow!("reserve lifecycle batch ready set: {error}"))?;

        for (&pane_id, ready) in pane_ids.iter().zip(ready_tokens) {
            let vacant_queue = if let Some(queue) = pending.by_pane.get_mut(&pane_id) {
                queue.try_reserve(1).map_err(|error| {
                    anyhow!("reserve pane {pane_id} lifecycle batch queue: {error}")
                })?;
                None
            } else {
                let mut queue = VecDeque::new();
                queue.try_reserve(1).map_err(|error| {
                    anyhow!("reserve pane {pane_id} lifecycle batch queue: {error}")
                })?;
                Some(queue)
            };
            tickets.push(PaneLifecycleNotificationTicket {
                pane_id,
                ready: Arc::clone(&ready),
            });
            entries.push(PreparedPaneLifecycleBatchEntry {
                pane_id,
                ready,
                vacant_queue,
            });
        }

        Ok(PreparedPaneLifecycleBatchEnqueue {
            pending,
            entries,
            tickets,
        })
    }

    /// Enqueue a lifecycle event using topology authority already reserved by
    /// the enclosing structural transaction. The caller holds
    /// `pane_registration`; `topology` must have been reserved before any
    /// infallible commit step and while the transaction's state locks remain
    /// held.
    fn enqueue_pane_lifecycle_notification_at_topology_locked(
        &self,
        notification: PaneLifecycleNotification,
        topology: MuxTopologyStamp,
        reader_start_gate: Option<PaneReaderStartGate>,
        cleanup_complete: Option<Arc<AtomicBool>>,
        removal_follow_up: PaneRemovalFollowUp,
    ) -> PaneLifecycleNotificationTicket {
        let pane_id = notification.pane_id();
        let ready = Arc::new(AtomicBool::new(false));
        self.pending_pane_lifecycle
            .lock()
            .by_pane
            .entry(pane_id)
            .or_default()
            .push_back(PendingPaneLifecycleNotification {
                notification,
                topology,
                ready: Arc::clone(&ready),
                reader_start_gate,
                cleanup_complete,
                removal_follow_up,
            });
        PaneLifecycleNotificationTicket { pane_id, ready }
    }

    /// Make a quiescent pane retirement eligible to run immediately before its
    /// corresponding `PaneRemoved` notification.
    ///
    /// Retirement is part of the per-pane lifecycle queue rather than an
    /// independent task: this prevents a concurrent removal from killing a
    /// pane before an earlier `PaneAdded` callback and reader-start decision
    /// have completed. Different pane IDs remain independently drainable.
    fn enqueue_pane_retirement(&self, completion: PaneRetirementCompletion) {
        let pane_id = completion.pane_id;
        debug_assert_eq!(
            pane_id, completion.lifecycle_notification.pane_id,
            "pane retirement must complete its own lifecycle ticket"
        );
        let should_drain = {
            let mut pending = self.pending_pane_lifecycle.lock();
            pending
                .retirements
                .entry(pane_id)
                .or_default()
                .push_back(completion);
            pending.arm_pane_if_ready(pane_id);
            pending.begin_drain_if_ready()
        };
        if should_drain {
            self.drain_pane_lifecycle_notifications();
        }
    }

    /// Mark one queued lifecycle transition ready and arrange for exactly one
    /// caller to drain all ready transitions in topology-mutation order.
    ///
    /// This method must be called without `pane_registration` or a subscriber
    /// lock held; `dispatch_notification` snapshots subscribers before invoking
    /// any callback.
    fn complete_pane_lifecycle_notification(&self, ticket: PaneLifecycleNotificationTicket) {
        ticket.ready.store(true, Ordering::Release);
        let should_drain = {
            let mut pending = self.pending_pane_lifecycle.lock();
            pending.arm_pane_if_ready(ticket.pane_id);
            pending.begin_drain_if_ready()
        };
        if should_drain {
            self.drain_pane_lifecycle_notifications();
        }
    }

    fn begin_pane_removal_cleanup_fanout(
        &self,
        pane_id: PaneId,
        cleanup_complete: Option<Arc<AtomicBool>>,
    ) -> Option<Arc<PaneRemovalCleanupToken>> {
        let _registration = self.pane_registration.lock();
        let retiring = self.retiring_pane_ids.lock();
        if !retiring.contains(&pane_id) {
            log::error!(
                "refusing to publish an unfenced PaneRemoved cleanup generation for pane {pane_id}"
            );
            return None;
        }

        let mut fences = self.pane_removal_cleanup_fences.lock();
        if fences.contains_key(&pane_id) {
            log::error!(
                "refusing to replace the active PaneRemoved cleanup generation for pane {pane_id}"
            );
            return None;
        }
        let token = Arc::new(PaneRemovalCleanupToken::new(cleanup_complete));
        fences.insert(pane_id, Arc::clone(&token));
        drop(fences);
        drop(retiring);
        drop(_registration);
        self.record_pane_removal_cleanup_counts();
        Some(token)
    }

    fn acquire_pane_removal_cleanup_lease(
        self: &Arc<Self>,
        pane_id: PaneId,
    ) -> Option<PaneRemovalCleanupLease> {
        let _registration = self.pane_registration.lock();
        let retiring = self.retiring_pane_ids.lock();
        if !retiring.contains(&pane_id) {
            return None;
        }
        let token = Arc::clone(self.pane_removal_cleanup_fences.lock().get(&pane_id)?);
        if !token.try_acquire() {
            log::error!(
                "PaneRemoved cleanup lease was requested outside active fanout or its count exhausted for pane {pane_id}"
            );
            return None;
        }
        if !try_increment_atomic_count(&self.pane_removal_cleanup_outstanding_leases) {
            let released = token.release();
            debug_assert_eq!(released, Some(false));
            log::error!(
                "PaneRemoved global cleanup lease count exhausted for pane {pane_id}; refusing new deferred authority"
            );
            return None;
        }
        let lease = PaneRemovalCleanupLease {
            owner: Arc::downgrade(self),
            pane_id,
            token,
            acquired_at: Instant::now(),
            completed: false,
        };
        drop(retiring);
        drop(_registration);
        self.record_pane_removal_cleanup_counts();
        Some(lease)
    }

    fn finish_pane_removal_cleanup_fanout(
        &self,
        pane_id: PaneId,
        token: &Arc<PaneRemovalCleanupToken>,
    ) {
        if token.close() {
            self.finalize_pane_removal_cleanup(pane_id, token);
        }
    }

    fn finalize_pane_removal_cleanup(&self, pane_id: PaneId, token: &Arc<PaneRemovalCleanupToken>) {
        let _registration = self.pane_registration.lock();
        let mut retiring = self.retiring_pane_ids.lock();
        let mut fences = self.pane_removal_cleanup_fences.lock();
        let Some(active) = fences.get(&pane_id) else {
            token.mark_cleanup_complete();
            return;
        };
        if !Arc::ptr_eq(active, token) {
            log::error!(
                "stale PaneRemoved cleanup generation attempted to release pane {pane_id}; retaining current reuse fence"
            );
            token.mark_cleanup_complete();
            return;
        }
        fences.remove(&pane_id);
        retiring.remove(&pane_id);
        token.mark_cleanup_complete();
        histogram!("mux.notifications.pane_removed.cleanup_fence_lifetime_ms")
            .record(token.created_at.elapsed().as_secs_f64() * 1_000.0);
        drop(fences);
        drop(retiring);
        drop(_registration);
        self.record_pane_removal_cleanup_counts();
    }

    /// Return current deferred-cleanup pressure without weakening its
    /// generation fences.
    ///
    /// The scan is bounded by the active removal-fence map and allocates
    /// nothing. In particular, `oldest_fence_age` is diagnostic only: no age
    /// threshold can make a pane ID reusable while a lease remains live.
    #[must_use]
    pub fn pane_removal_cleanup_snapshot(&self) -> PaneRemovalCleanupSnapshot {
        let now = Instant::now();
        let fences = self.pane_removal_cleanup_fences.lock();
        let mut snapshot = PaneRemovalCleanupSnapshot {
            active_fences: fences.len(),
            outstanding_leases: self
                .pane_removal_cleanup_outstanding_leases
                .load(Ordering::Acquire),
            ..PaneRemovalCleanupSnapshot::default()
        };
        for token in fences.values() {
            snapshot.oldest_fence_age = snapshot
                .oldest_fence_age
                .max(now.saturating_duration_since(token.created_at));
        }
        snapshot
    }

    fn record_pane_removal_cleanup_counts(&self) {
        let active_fences = self.pane_removal_cleanup_fences.lock().len();
        let outstanding_leases = self
            .pane_removal_cleanup_outstanding_leases
            .load(Ordering::Acquire);
        metrics::gauge!("mux.notifications.pane_removed.cleanup_fences_active")
            .set(active_fences as f64);
        metrics::gauge!("mux.notifications.pane_removed.cleanup_leases_outstanding")
            .set(outstanding_leases as f64);
    }

    fn drain_pane_lifecycle_notifications(&self) {
        loop {
            let step = {
                let mut pending = self.pending_pane_lifecycle.lock();
                loop {
                    let Some(pane_id) = pending.ready_panes.pop_front() else {
                        pending.draining = false;
                        break PaneLifecycleDrainStep::Done;
                    };
                    pending.ready_set.remove(&pane_id);

                    let notification_ready = pending
                        .by_pane
                        .get(&pane_id)
                        .and_then(|notifications| notifications.front())
                        .is_some_and(|notification| notification.ready.load(Ordering::Acquire));
                    if notification_ready {
                        let notification = pending
                            .by_pane
                            .get_mut(&pane_id)
                            .and_then(VecDeque::pop_front)
                            .expect("a ready pane must retain its lifecycle notification");
                        if pending
                            .by_pane
                            .get(&pane_id)
                            .is_some_and(VecDeque::is_empty)
                        {
                            pending.by_pane.remove(&pane_id);
                        }
                        pending.arm_pane_if_ready(pane_id);
                        break PaneLifecycleDrainStep::Notification(notification);
                    }

                    let retirement_ready = pending
                        .by_pane
                        .get(&pane_id)
                        .and_then(|notifications| notifications.front())
                        .zip(
                            pending
                                .retirements
                                .get(&pane_id)
                                .and_then(|retirements| retirements.front()),
                        )
                        .is_some_and(|(notification, retirement)| {
                            Arc::ptr_eq(
                                &notification.ready,
                                &retirement.lifecycle_notification.ready,
                            )
                        });
                    if retirement_ready {
                        let retirement = pending
                            .retirements
                            .get_mut(&pane_id)
                            .and_then(VecDeque::pop_front)
                            .expect("a retirement-ready pane must retain its completion");
                        if pending
                            .retirements
                            .get(&pane_id)
                            .is_some_and(VecDeque::is_empty)
                        {
                            pending.retirements.remove(&pane_id);
                        }
                        break PaneLifecycleDrainStep::Retirement(retirement);
                    }

                    debug_assert!(
                        false,
                        "pane {} was armed without a ready lifecycle action",
                        pane_id
                    );
                }
            };

            match step {
                PaneLifecycleDrainStep::Notification(pending_notification) => {
                    let PendingPaneLifecycleNotification {
                        notification,
                        topology,
                        ready: _,
                        reader_start_gate,
                        cleanup_complete,
                        removal_follow_up,
                    } = pending_notification;
                    let removed_pane_id = match &notification {
                        PaneLifecycleNotification::Removed(pane_id) => Some(*pane_id),
                        PaneLifecycleNotification::Added(_)
                        | PaneLifecycleNotification::FloatingSpawnCommitted(_)
                        | PaneLifecycleNotification::Output(_) => None,
                    };
                    let removal_cleanup_token = if let Some(pane_id) = removed_pane_id {
                        self.begin_pane_removal_cleanup_fanout(pane_id, cleanup_complete)
                    } else {
                        debug_assert!(
                            cleanup_complete.is_none(),
                            "only pane removal notifications complete retirement cleanup",
                        );
                        None
                    };
                    self.dispatch_notification_envelope(MuxNotificationEnvelope {
                        notification: notification.into(),
                        topology,
                    });
                    if let Some(reader_start_gate) = reader_start_gate {
                        reader_start_gate.release_if_registered(self);
                    }
                    if let Some(pane_id) = removed_pane_id {
                        if removal_follow_up
                            == PaneRemovalFollowUp::PruneDeadWindowsIgnoringActivity
                        {
                            // The exact retired generation remains fenced while
                            // pruning. No same-ID publication can race the
                            // topology sweep between Pane::kill and removal of
                            // its now-dead tab/window.
                            self.prune_dead_windows_ignoring_activity();
                        }
                        // The removal queue entry owns the retirement fence. A
                        // caller may complete a ticket reentrantly while an
                        // earlier callback is draining. Deferred subscribers
                        // may extend this exact fence through their queued GUI
                        // cleanup, preventing a replacement generation from
                        // being registered and then erased by a bare-ID task.
                        if let Some(token) = removal_cleanup_token {
                            self.finish_pane_removal_cleanup_fanout(pane_id, &token);
                        }
                    } else {
                        debug_assert_eq!(removal_follow_up, PaneRemovalFollowUp::None);
                    }
                }
                PaneLifecycleDrainStep::Retirement(retirement) => {
                    retirement.complete(Some(self));
                }
                PaneLifecycleDrainStep::Done => return,
            }
        }
    }

    fn enqueue_pane_output_notification(&self, pane_id: PaneId) {
        let Some(pane) = self.get_pane(pane_id) else {
            return;
        };
        let _ = self.enqueue_pane_output_notification_for_pane_with_scheduler_state(
            &pane,
            promise::spawn::is_scheduler_configured(),
        );
    }

    fn enqueue_pane_output_notification_for_pane_with_scheduler_state(
        &self,
        pane: &Arc<dyn Pane>,
        scheduler_configured: bool,
    ) -> bool {
        let pane_id = pane.pane_id();
        let (should_schedule, owner) = {
            let _registration = self.pane_registration.lock();
            let generation = {
                let panes = self.panes.read();
                let Some(current) = panes.get(&pane_id) else {
                    return false;
                };
                if !Arc::ptr_eq(&current.pane, pane) {
                    return false;
                }
                Arc::clone(&current.generation)
            };
            let owner = generation.owner.clone();
            let mut pending = self.pending_pane_output.lock();
            let already_queued = pending
                .queued
                .get(&pane_id)
                .is_some_and(|queued| Arc::ptr_eq(&queued.generation, &generation));
            if !already_queued {
                if pending.queued.contains_key(&pane_id) {
                    return false;
                }
                let lifecycle_notification = self.enqueue_pane_lifecycle_notification_locked(
                    PaneLifecycleNotification::Output(pane_id),
                    None,
                );
                let batch = PaneOutputBatch::new(
                    pane_id,
                    generation,
                    lifecycle_notification,
                    0,
                    scheduler_configured,
                );
                pending.queued.insert(pane_id, Arc::clone(&batch));
                pending.notifications.push(batch);
                histogram!("mux.notifications.pane_output.unique_enqueue_rate").record(1.);
            }
            (
                !self
                    .pane_output_drain_scheduled
                    .swap(true, Ordering::AcqRel),
                owner,
            )
        };

        self.schedule_pane_output_drain(should_schedule, owner, scheduler_configured);
        true
    }

    fn reserve_pane_output_for_reader(
        &self,
        pane: &Arc<dyn Pane>,
        generation: &Arc<PaneRegistrationGeneration>,
        scheduler_configured: bool,
    ) -> Option<PaneOutputContinuation> {
        let pane_id = pane.pane_id();
        let (batch, operation, should_schedule, owner) = {
            let _registration = self.pane_registration.lock();
            let is_registered = self.panes.read().get(&pane_id).is_some_and(|registered| {
                Arc::ptr_eq(&registered.pane, pane)
                    && Arc::ptr_eq(&registered.generation, generation)
            });
            if !is_registered {
                return None;
            }
            let operation = generation.try_acquire()?;
            let owner = generation.owner.clone();
            let mut pending = self.pending_pane_output.lock();
            let batch = match pending.queued.get(&pane_id) {
                Some(batch) if Arc::ptr_eq(&batch.generation, generation) => {
                    if !batch.try_join_producer() {
                        return None;
                    }
                    histogram!("mux.notifications.pane_output.joined_producer_rate").record(1.);
                    Arc::clone(batch)
                }
                Some(_) => return None,
                None => {
                    let lifecycle_notification = self.enqueue_pane_lifecycle_notification_locked(
                        PaneLifecycleNotification::Output(pane_id),
                        None,
                    );
                    let batch = PaneOutputBatch::new(
                        pane_id,
                        Arc::clone(generation),
                        lifecycle_notification,
                        1,
                        scheduler_configured,
                    );
                    pending.queued.insert(pane_id, Arc::clone(&batch));
                    pending.notifications.push(Arc::clone(&batch));
                    histogram!("mux.notifications.pane_output.unique_enqueue_rate").record(1.);
                    batch
                }
            };
            (
                batch,
                operation,
                !self
                    .pane_output_drain_scheduled
                    .swap(true, Ordering::AcqRel),
                owner,
            )
        };

        self.schedule_pane_output_drain(should_schedule, owner, scheduler_configured);
        Some(PaneOutputContinuation {
            batch,
            operation: Some(operation),
        })
    }

    fn schedule_pane_output_drain(
        &self,
        should_schedule: bool,
        owner: Weak<Mux>,
        scheduler_configured: bool,
    ) {
        if !should_schedule {
            return;
        }

        let exact_mux = scheduler_configured
            .then(|| owner.upgrade())
            .flatten()
            .filter(|mux| std::ptr::eq::<Mux>(mux.as_ref(), self));
        if let Some(exact_mux) = exact_mux {
            let dispatch = PaneOutputDrainDispatch::new(Arc::downgrade(&exact_mux));
            promise::spawn::spawn_into_main_thread(async move {
                dispatch.execute();
            })
            .detach();
        } else {
            // Standalone/headless embedders may intentionally construct a mux
            // without configuring the GUI scheduler or without installing it
            // as the global mux. Preserve the historical direct-notify
            // contract by draining synchronously in either case.
            self.flush_pending_pane_output_notifications();
        }
    }

    /// Detach the open output batch for an exact retiring generation.
    ///
    /// The caller holds `pane_registration`. The batch remains in the
    /// scheduled vector so an already-queued drain can encounter it safely,
    /// but exact `Arc` comparison prevents that delayed drain from erasing a
    /// replacement generation's marker.
    fn take_pending_pane_output_batch_locked(
        &self,
        pane_id: PaneId,
        generation: &Arc<PaneRegistrationGeneration>,
    ) -> Option<Arc<PaneOutputBatch>> {
        let mut pending = self.pending_pane_output.lock();
        let matches = pending
            .queued
            .get(&pane_id)
            .is_some_and(|batch| Arc::ptr_eq(&batch.generation, generation));
        if matches {
            pending.queued.remove(&pane_id)
        } else {
            None
        }
    }

    fn discard_removed_pane_states(&self, pane_ids: &[PaneId]) {
        if pane_ids.is_empty() {
            return;
        }
        let pane_ids = pane_ids.iter().copied().collect::<HashSet<_>>();
        self.discard_removed_pane_states_set(&pane_ids);
    }

    fn discard_removed_pane_states_set(&self, pane_ids: &HashSet<PaneId>) {
        if pane_ids.is_empty() {
            return;
        }
        self.last_high_rate_alert
            .lock()
            .retain(|(pane_id, _), _| !pane_ids.contains(pane_id));
        for client in self.clients.write().values_mut() {
            let removed_registration = client
                .focused_pane_registration()
                .is_some_and(|registration| pane_ids.contains(&registration.pane_id()));
            let removed_projection = client
                .focused_pane_id
                .is_some_and(|pane_id| pane_ids.contains(&pane_id));
            if removed_registration || removed_projection {
                client.clear_focused_pane();
            }
        }
    }

    fn flush_pending_pane_output_notifications(&self) {
        loop {
            let batches = {
                let mut pending = self.pending_pane_output.lock();
                if pending.notifications.is_empty() {
                    self.pane_output_drain_scheduled
                        .store(false, Ordering::Release);
                    return;
                }
                let batches = std::mem::take(&mut pending.notifications);
                for batch in &batches {
                    let is_current = pending
                        .queued
                        .get(&batch.pane_id)
                        .is_some_and(|queued| Arc::ptr_eq(queued, batch));
                    if is_current {
                        pending.queued.remove(&batch.pane_id);
                    }
                }
                batches
            };

            histogram!("mux.notifications.pane_output.batch_size").record(batches.len() as f64);
            for batch in batches {
                batch.seal();
            }
        }
    }

    pub fn default_domain(&self) -> Arc<dyn Domain> {
        self.default_domain.read().as_ref().map(Arc::clone).unwrap()
    }

    fn resolve_default_domain(&self) -> anyhow::Result<Arc<dyn Domain>> {
        self.default_domain
            .read()
            .as_ref()
            .map(Arc::clone)
            .ok_or_else(|| anyhow!("no default domain configured"))
    }

    pub fn set_default_domain(
        &self,
        domain: &Arc<dyn Domain>,
    ) -> Result<(), DomainRegistrationError> {
        let domain_id = domain.domain_id();
        let domain_name = domain.domain_name().to_string();
        let _registration = self.domain_registration.lock();
        let exact_id = self
            .domains
            .read()
            .get(&domain_id)
            .is_some_and(|registered| Arc::ptr_eq(registered, domain));
        let exact_name = self
            .domains_by_name
            .read()
            .get(&domain_name)
            .is_some_and(|registered| Arc::ptr_eq(registered, domain));
        if self.retired_domain_ids.lock().contains(&domain_id) || !exact_id || !exact_name {
            return Err(DomainRegistrationError::DefaultNotRegistered {
                domain_id,
                domain_name,
            });
        }
        *self.default_domain.write() = Some(Arc::clone(domain));
        Ok(())
    }

    pub fn get_domain(&self, id: DomainId) -> Option<Arc<dyn Domain>> {
        self.domains.read().get(&id).cloned()
    }

    pub fn get_domain_by_name(&self, name: &str) -> Option<Arc<dyn Domain>> {
        self.domains_by_name.read().get(name).cloned()
    }

    pub fn add_domain(&self, domain: &Arc<dyn Domain>) -> Result<(), DomainRegistrationError> {
        // Domain implementations are external callbacks. Resolve requested
        // metadata before taking registry locks so a reentrant or blocking
        // implementation cannot freeze every domain reader/writer.
        let domain_id = domain.domain_id();
        let domain_name = domain.domain_name().to_string();
        let domain_arc = Arc::clone(domain);
        let _registration = self.domain_registration.lock();

        if self.retired_domain_ids.lock().contains(&domain_id) {
            return Err(DomainRegistrationError::RetiredIdentifier {
                domain_id,
                domain_name,
            });
        }

        {
            let mut domains = self.domains.write();
            let mut domains_by_name = self.domains_by_name.write();
            if let Some(existing) = domains.get(&domain_id) {
                if Arc::ptr_eq(existing, domain) {
                    return Ok(());
                }
                let registered_name = domains_by_name
                    .iter()
                    .find_map(|(name, registered)| {
                        Arc::ptr_eq(registered, existing).then(|| name.clone())
                    })
                    .ok_or_else(|| DomainRegistrationError::RegistryInconsistent {
                        detail: format!(
                            "identifier {domain_id} has no exact name-index registration"
                        ),
                    })?;
                return Err(DomainRegistrationError::IdentifierInUse {
                    domain_id,
                    registered_name,
                    requested_name: domain_name,
                });
            }
            if let Some(existing) = domains_by_name.get(&domain_name) {
                if !Arc::ptr_eq(existing, domain) {
                    let registered_id = domains
                        .iter()
                        .find_map(|(id, registered)| {
                            Arc::ptr_eq(registered, existing).then_some(*id)
                        })
                        .ok_or_else(|| DomainRegistrationError::RegistryInconsistent {
                            detail: format!(
                                "name {domain_name} has no exact identifier-index registration"
                            ),
                        })?;
                    return Err(DomainRegistrationError::NameInUse {
                        domain_name,
                        registered_id,
                        requested_id: domain_id,
                    });
                }
                return Ok(());
            }
            domains_by_name.insert(domain_name, Arc::clone(&domain_arc));
            domains.insert(domain_id, Arc::clone(&domain_arc));
        }

        let mut default_domain = self.default_domain.write();
        if default_domain.is_none() {
            *default_domain = Some(domain_arc);
        }
        Ok(())
    }

    pub fn set_mux(mux: &Arc<Mux>) {
        // Drop the replaced mux only after releasing the singleton lock. The
        // old mux's last Arc may run ClientDomain teardown, which calls back
        // into `Mux::try_get`.
        let replaced = MUX.lock().replace(Arc::clone(mux));
        drop(replaced);
    }

    pub fn shutdown() {
        // Important: bind the taken Arc<Mux> to a `let` so the MutexGuard
        // returned by MUX.lock() is dropped at the end of the *statement*
        // (i.e., right here), BEFORE `taken` itself is dropped at end of
        // function. Without the let-binding, a temporary-drop-order
        // deadlock fires:
        //
        //   MUX.lock().take();           // as one statement, temporaries
        //                                // dropped reverse-of-construction
        //   ── drops Option<Arc<Mux>> first  ⇨ Mux::drop ⇨ ClientDomain::drop
        //         which calls Mux::try_get  ⇨ tries to acquire MUX.lock
        //   ── while MutexGuard STILL HELD                ⇨ deadlock
        //                                                   (main thread
        //                                                   parked in
        //                                                   parking_lot::
        //                                                   RawMutex::
        //                                                   lock_slow,
        //                                                   beachball)
        //
        // Reproduces reliably by closing the last GUI tab on macOS when
        // a remote ClientDomain is registered: gui-startup spawns the
        // domain which adds a mux notification subscriber holding a weak
        // ref to ClientDomain; on app exit the FnOnce subscriber drops,
        // which drops ClientDomain, whose Drop calls Mux::try_get(). With
        // the implicit-temp form we deadlock on the same lock the outer
        // shutdown() is holding.
        let _taken = MUX.lock().take();
    }

    pub fn get() -> Arc<Mux> {
        Self::try_get().unwrap()
    }

    pub fn try_get() -> Option<Arc<Mux>> {
        MUX.lock().as_ref().map(Arc::clone)
    }

    pub fn get_pane(&self, pane_id: PaneId) -> Option<Arc<dyn Pane>> {
        self.panes
            .read()
            .get(&pane_id)
            .map(|registered| Arc::clone(&registered.pane))
    }

    pub fn get_tab(&self, tab_id: TabId) -> Option<Arc<Tab>> {
        self.tabs.read().get(&tab_id).map(Arc::clone)
    }

    pub fn capture_pane_registration(
        &self,
        pane: &Arc<dyn Pane>,
    ) -> Option<PaneRegistrationHandle> {
        let pane_id = pane.pane_id();
        let _registration = self.pane_registration.lock();
        self.panes.read().get(&pane_id).and_then(|registered| {
            Arc::ptr_eq(&registered.pane, pane)
                .then(|| PaneRegistrationHandle::new(pane, &registered.generation))
        })
    }

    pub fn capture_pane_registrations(
        &self,
        panes: &[Arc<dyn Pane>],
    ) -> Vec<PaneRegistrationHandle> {
        let candidates = panes
            .iter()
            .map(|pane| (pane.pane_id(), pane))
            .collect::<Vec<_>>();
        let _registration = self.pane_registration.lock();
        let registered = self.panes.read();
        candidates
            .into_iter()
            .filter_map(|(pane_id, pane)| {
                registered.get(&pane_id).and_then(|current| {
                    Arc::ptr_eq(&current.pane, pane)
                        .then(|| PaneRegistrationHandle::new(pane, &current.generation))
                })
            })
            .collect()
    }

    pub fn capture_current_pane(&self, pane_id: PaneId) -> Option<PaneRegistrationHandle> {
        let _registration = self.pane_registration.lock();
        self.panes
            .read()
            .get(&pane_id)
            .map(|registered| PaneRegistrationHandle::new(&registered.pane, &registered.generation))
    }

    /// Admit one complete operation against the exact current registration.
    ///
    /// The returned guard owns the pane and mux and cannot be cloned.  Callers
    /// should capture it before scheduling or awaiting deferred mutation work.
    pub fn capture_pane_operation(self: &Arc<Self>, pane_id: PaneId) -> Option<PaneOperationGuard> {
        self.capture_current_pane(pane_id)?.operation_guard(self)
    }

    #[cfg(test)]
    fn remove_pane_registration_if_same(&self, pane_id: PaneId, expected: &Arc<dyn Pane>) -> bool {
        let mut panes = self.panes.write();
        if panes
            .get(&pane_id)
            .is_some_and(|registered| Arc::ptr_eq(&registered.pane, expected))
        {
            panes.remove(&pane_id);
            true
        } else {
            false
        }
    }

    #[cfg(test)]
    fn remove_tab_registration_if_same(&self, tab_id: TabId, expected: &Arc<Tab>) -> bool {
        let mut tabs = self.tabs.write();
        if tabs
            .get(&tab_id)
            .is_some_and(|registered| Arc::ptr_eq(registered, expected))
        {
            tabs.remove(&tab_id);
            true
        } else {
            false
        }
    }

    /// Claim one pane ID for fallible external preparation.
    ///
    /// The claim prevents concurrent callers from consuming the same pane
    /// reader while allowing unrelated pane registrations to proceed. Public
    /// `Pane` callbacks are deliberately invoked only after this short map
    /// critical section has ended.
    fn claim_pane_preparation(
        self: &Arc<Self>,
        pane: &Arc<dyn Pane>,
    ) -> Result<Option<PanePreparationClaim<'_>>, Error> {
        let pane_id = pane.pane_id();
        let domain_id = pane.domain_id();
        let weak_pane = Arc::downgrade(pane);
        let generation =
            PaneRegistrationGeneration::new(pane_id, &self.pane_retirements, Arc::downgrade(self));
        let _registration = self.pane_registration.lock();
        if self.retiring_pane_ids.lock().contains(&pane_id)
            || self.pane_retirements.has_in_flight_retirement(pane_id)
        {
            return Err(PaneIdCollision { pane_id }.into());
        }
        let mut claims = self.pane_preparations.lock();
        if let Some(preparing) = claims.get(&pane_id) {
            if Weak::ptr_eq(&preparing.pane, &weak_pane) {
                return Err(PanePreparationInProgress { pane_id }.into());
            }
            return Err(PaneIdCollision { pane_id }.into());
        }

        if let Some(existing) = self.panes.read().get(&pane_id) {
            if Arc::ptr_eq(&existing.pane, pane) {
                return Ok(None);
            }
            return Err(PaneIdCollision { pane_id }.into());
        }

        claims.insert(
            pane_id,
            PanePreparation {
                pane: weak_pane.clone(),
                generation: Arc::clone(&generation),
                cancelled: false,
            },
        );
        Ok(Some(PanePreparationClaim {
            registration: &self.pane_registration,
            claims: &self.pane_preparations,
            pane_id,
            domain_id,
            pane: weak_pane,
            generation,
            active: true,
        }))
    }

    /// Cancel the current preparation for this ID, optionally requiring exact
    /// pane-instance identity. The caller must hold `pane_registration`.
    fn cancel_pane_preparation_locked(
        &self,
        pane_id: PaneId,
        expected: Option<&Arc<dyn Pane>>,
    ) -> bool {
        let mut claims = self.pane_preparations.lock();
        let Some(preparing) = claims.get_mut(&pane_id) else {
            return false;
        };
        if !expected.is_none_or(|expected| Weak::ptr_eq(&preparing.pane, &Arc::downgrade(expected)))
        {
            return false;
        }
        preparing.cancelled = true;
        true
    }

    /// Perform fallible pane callbacks after the caller owns the per-ID
    /// preparation claim and without holding the topology publication lock.
    fn prepare_claimed_pane_registration(
        &self,
        pane: &Arc<dyn Pane>,
        pane_id: PaneId,
        generation: &Arc<PaneRegistrationGeneration>,
    ) -> Result<PreparedPaneRegistration, Error> {
        let callback_target = PaneRegistrationHandle::new(pane, generation);
        let registration_reservation = pane
            .mux_registration_slot()
            .reserve(callback_target.clone())?;
        let clipboard: Arc<dyn Clipboard> = Arc::new(MuxClipboard {
            target: callback_target.clone(),
        });
        pane.set_clipboard(&clipboard);

        let downloader: Arc<dyn DownloadHandler> = Arc::new(MuxDownloader {
            target: callback_target,
        });
        pane.set_download_handler(&downloader);

        let reader = pane.reader()?;
        Ok(PreparedPaneRegistration {
            pane_id,
            reader,
            registration_reservation,
        })
    }

    fn insert_pane_registration_locked(
        &self,
        pane_id: PaneId,
        domain_id: DomainId,
        pane: &Arc<dyn Pane>,
        generation: &Arc<PaneRegistrationGeneration>,
    ) -> Result<(), Error> {
        if self.retiring_pane_ids.lock().contains(&pane_id)
            || self.pane_retirements.has_in_flight_retirement(pane_id)
        {
            return Err(PaneIdCollision { pane_id }.into());
        }
        let mut panes = self.panes.write();
        if let Some(existing) = panes.get(&pane_id) {
            if Arc::ptr_eq(&existing.pane, pane) {
                return Err(anyhow!(
                    "pane identifier {pane_id} became registered during a serialized preparation"
                ));
            }
            return Err(PaneIdCollision { pane_id }.into());
        }
        panes.insert(
            pane_id,
            LivePaneRegistration {
                pane: Arc::clone(pane),
                generation: Arc::clone(generation),
                domain_id,
            },
        );
        Ok(())
    }

    fn insert_tab_registration_locked(&self, tab: &Arc<Tab>) -> Result<bool, Error> {
        let tab_id = tab.tab_id();
        let mut tabs = self.tabs.write();
        if let Some(existing) = tabs.get(&tab_id) {
            if Arc::ptr_eq(existing, tab) {
                return Ok(false);
            }
            return Err(anyhow!(
                "tab identifier {tab_id} is already registered to a different tab instance"
            ));
        }
        tabs.insert(tab_id, Arc::clone(tab));
        Ok(true)
    }

    fn tab_registration_needs_insert_locked(&self, tab: &Arc<Tab>) -> Result<bool, Error> {
        let tab_id = tab.tab_id();
        if let Some(existing) = self.tabs.read().get(&tab_id) {
            if Arc::ptr_eq(existing, tab) {
                return Ok(false);
            }
            return Err(anyhow!(
                "tab identifier {tab_id} is already registered to a different tab instance"
            ));
        }
        Ok(true)
    }

    #[cfg(test)]
    fn fail_next_pane_reader_preparation(&self, fault: PaneReaderPreparationFault) {
        *self.pane_reader_preparation_fault.lock() = Some(fault);
    }

    fn spawn_prepared_pane_reader(
        &self,
        pane: &Arc<dyn Pane>,
        prepared: PreparedPaneRegistration,
        generation: &Arc<PaneRegistrationGeneration>,
    ) -> Result<
        (
            Option<PaneReaderStartGate>,
            pane_registration_handle::PaneRegistrationReservation,
        ),
        Error,
    > {
        let PreparedPaneRegistration {
            pane_id,
            reader,
            registration_reservation,
        } = prepared;
        if let Some(reader) = reader {
            #[cfg(test)]
            let fault = self.pane_reader_preparation_fault.lock().take();
            #[cfg(test)]
            if fault == Some(PaneReaderPreparationFault::Socketpair) {
                generation.reader_dead.store(true, Ordering::Release);
                return Err(anyhow!(
                    "injected pane reader socketpair failure for pane {pane_id}"
                ));
            }
            let (tx, rx) = allocate_socketpair().with_context(|| {
                format!("failed to allocate pane reader socketpair for pane {pane_id}")
            })?;
            let banner = self.banner.read().clone();
            let weak_pane = Arc::downgrade(pane);
            let coordinator = Arc::new(PaneReaderStartCoordinator::new());
            let dead = Arc::clone(&generation.reader_dead);
            let (ready_tx, ready_rx) =
                std::sync::mpsc::channel::<Result<PaneReaderWorker, String>>();
            let (parser_done_tx, parser_done_rx) = std::sync::mpsc::channel();

            let parser_coordinator = Arc::clone(&coordinator);
            let parser_ready = ready_tx.clone();
            let parser_pane = weak_pane.clone();
            let parser_generation = Arc::clone(generation);
            let parser_dead = Arc::clone(&dead);
            #[cfg(test)]
            let fail_parser_spawn = fault == Some(PaneReaderPreparationFault::ParserSpawn);
            #[cfg(not(test))]
            let fail_parser_spawn = false;
            #[cfg(test)]
            let fail_parser_ready = fault == Some(PaneReaderPreparationFault::ParserReady);
            #[cfg(not(test))]
            let fail_parser_ready = false;
            let parser_spawn_result = if fail_parser_spawn {
                Err(std::io::Error::other("injected pane parser spawn failure"))
            } else {
                thread::Builder::new()
                    .name(format!("mux-parse-pane-{pane_id}"))
                    .spawn(move || {
                        if fail_parser_ready {
                            let _ = parser_ready
                                .send(Err("injected pane parser readiness failure".to_string()));
                            let _ = parser_coordinator.cancel();
                            return;
                        }
                        if parser_ready.send(Ok(PaneReaderWorker::Parser)).is_err() {
                            let _ = parser_coordinator.cancel();
                            return;
                        }
                        match parser_coordinator.wait() {
                            PaneReaderStartDecision::Released => {
                                parse_buffered_data(
                                    parser_pane,
                                    parser_generation,
                                    &parser_dead,
                                    rx,
                                );
                                let _ = parser_done_tx.send(());
                            }
                            PaneReaderStartDecision::Cancelled => {
                                let _ = parser_done_tx.send(());
                            }
                        }
                    })
            };
            let parser_thread = parser_spawn_result.map_err(|err| {
                dead.store(true, Ordering::Release);
                let _ = coordinator.cancel();
                anyhow!("failed to spawn pane parser thread for pane {pane_id}: {err}")
            })?;

            let reader_coordinator = Arc::clone(&coordinator);
            let reader_ready = ready_tx.clone();
            let reader_pane = weak_pane.clone();
            let reader_generation = Arc::clone(generation);
            let reader_dead = Arc::clone(&dead);
            #[cfg(test)]
            let fail_reader_spawn = fault == Some(PaneReaderPreparationFault::ReaderSpawn);
            #[cfg(not(test))]
            let fail_reader_spawn = false;
            #[cfg(test)]
            let fail_reader_ready = fault == Some(PaneReaderPreparationFault::ReaderReady);
            #[cfg(not(test))]
            let fail_reader_ready = false;
            let reader_spawn_result = if fail_reader_spawn {
                Err(std::io::Error::other("injected pane reader spawn failure"))
            } else {
                thread::Builder::new()
                    .name(format!("mux-read-pane-{pane_id}"))
                    .spawn(move || {
                        if fail_reader_ready {
                            let _ = reader_ready
                                .send(Err("injected pane reader readiness failure".to_string()));
                            let _ = reader_coordinator.cancel();
                            return;
                        }
                        if reader_ready.send(Ok(PaneReaderWorker::Reader)).is_err() {
                            let _ = reader_coordinator.cancel();
                            return;
                        }
                        match reader_coordinator.wait() {
                            PaneReaderStartDecision::Released => {
                                read_from_pane_pty(
                                    reader_pane,
                                    reader_generation,
                                    banner,
                                    reader,
                                    tx,
                                    reader_dead,
                                    parser_done_rx,
                                );
                            }
                            PaneReaderStartDecision::Cancelled => {}
                        }
                    })
            };
            let reader_thread = match reader_spawn_result {
                Ok(thread) => thread,
                Err(err) => {
                    dead.store(true, Ordering::Release);
                    let _ = coordinator.cancel();
                    drop(ready_tx);
                    let _ = parser_thread.join();
                    return Err(anyhow!(
                        "failed to spawn pane reader thread for pane {pane_id}: {err}"
                    ));
                }
            };
            drop(ready_tx);

            let mut ready_workers = HashSet::new();
            let readiness = (|| -> Result<(), Error> {
                for _ in 0..2 {
                    match ready_rx.recv_timeout(PANE_READER_READY_TIMEOUT) {
                        Ok(Ok(worker)) => {
                            if !ready_workers.insert(worker) {
                                return Err(anyhow!(
                                    "pane {pane_id} reader preparation reported duplicate \
                                     {worker:?} readiness"
                                ));
                            }
                        }
                        Ok(Err(reason)) => {
                            return Err(anyhow!(
                                "pane {pane_id} reader preparation failed: {reason}"
                            ));
                        }
                        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                            return Err(anyhow!(
                                "timed out after {PANE_READER_READY_TIMEOUT:?} waiting for pane \
                                 {pane_id} reader workers to become ready"
                            ));
                        }
                        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                            return Err(anyhow!(
                                "pane {pane_id} reader worker exited before reporting readiness"
                            ));
                        }
                    }
                }
                Ok(())
            })();
            if let Err(err) = readiness {
                dead.store(true, Ordering::Release);
                let _ = coordinator.cancel();
                let _ = parser_thread.join();
                let _ = reader_thread.join();
                return Err(err);
            }

            Ok((
                Some(PaneReaderStartGate {
                    coordinator,
                    pane_id,
                    pane: weak_pane,
                    generation: Arc::clone(generation),
                }),
                registration_reservation,
            ))
        } else {
            Ok((None, registration_reservation))
        }
    }

    /// Notify a pane that one exact registration is live without allowing
    /// arbitrary pane code to unwind across the publication boundary.
    ///
    /// The registration check and callback run without mux registry/topology
    /// locks. A reentrant removal may retire the generation, in which case the
    /// callback is simply skipped. Callback panic is quarantined so callers
    /// holding a construction-time kill guard cannot kill an already-published
    /// pane during unwinding.
    fn notify_pane_registration_did_bind(
        &self,
        pane: &Arc<dyn Pane>,
        registration: &PaneRegistrationHandle,
    ) {
        let hook_registration = registration.clone();
        if catch_recoverable(
            RecoverablePanicSite::MuxPaneCallback,
            std::panic::AssertUnwindSafe(|| {
                let _ = registration.try_with_current(|current| {
                    if current.is_same_pane(pane) {
                        pane.mux_registration_did_bind(hook_registration);
                    }
                });
            }),
        )
        .is_err()
        {
            log::error!(
                "pane bind callback panicked for exact pane identity {:p}",
                Arc::as_ptr(pane)
            );
        }
    }

    pub fn add_pane(self: &Arc<Self>, pane: &Arc<dyn Pane>) -> Result<(), Error> {
        let Some(mut preparation_claim) = self.claim_pane_preparation(pane)? else {
            return Ok(());
        };
        let prepared = self.prepare_claimed_pane_registration(
            pane,
            preparation_claim.pane_id,
            &preparation_claim.generation,
        )?;
        let pane_id = prepared.pane_id;
        // Spawning is the last fallible external operation and therefore must
        // precede publication. The thread waits on its start gate.
        let (mut reader_start_gate, registration_reservation) =
            self.spawn_prepared_pane_reader(pane, prepared, &preparation_claim.generation)?;
        let publication_result = {
            let _registration = self.pane_registration.lock();
            let result = (|| -> Result<_, Error> {
                if !preparation_claim.is_authoritative_locked() {
                    return Err(PanePreparationCancelled { pane_id }.into());
                }
                let commit_guard = registration_reservation.commit()?;
                self.insert_pane_registration_locked(
                    pane_id,
                    preparation_claim.domain_id,
                    pane,
                    &preparation_claim.generation,
                )?;
                let lifecycle_notification = self.enqueue_pane_lifecycle_notification_locked(
                    PaneLifecycleNotification::Added(pane_id),
                    reader_start_gate.take(),
                );
                let registration = commit_guard.finalize();
                Ok((lifecycle_notification, registration))
            })();
            preparation_claim.retire_locked();
            result
        };
        let (lifecycle_notification, registration) = publication_result?;

        self.complete_pane_lifecycle_notification(lifecycle_notification);
        self.notify_pane_registration_did_bind(pane, &registration);
        self.recompute_pane_count();
        Ok(())
    }

    pub fn add_tab_no_panes(&self, tab: &Arc<Tab>) -> Result<(), Error> {
        let inserted = {
            let _registration = self.pane_registration.lock();
            self.insert_tab_registration_locked(tab)?
        };
        if inserted {
            self.recompute_pane_count();
        }
        Ok(())
    }

    pub fn add_tab_and_active_pane(
        self: &Arc<Self>,
        tab: &Arc<Tab>,
    ) -> Result<Option<PaneRegistrationHandle>, Error> {
        let pane = tab
            .get_active_pane()
            .ok_or_else(|| anyhow!("tab MUST have an active pane"))?;
        let mut preparation_claim = self.claim_pane_preparation(&pane)?;
        let prepared = match preparation_claim.as_ref() {
            Some(claim) => Some(self.prepare_claimed_pane_registration(
                &pane,
                claim.pane_id,
                &claim.generation,
            )?),
            None => None,
        };
        let (mut reader_start_gate, mut registration_reservation) = match prepared {
            Some(prepared) => {
                let (reader_start_gate, registration_reservation) = self
                    .spawn_prepared_pane_reader(
                        &pane,
                        prepared,
                        &preparation_claim
                            .as_ref()
                            .expect("prepared panes retain their preparation claim")
                            .generation,
                    )?;
                (reader_start_gate, Some(registration_reservation))
            }
            None => (None, None),
        };
        let publication_result = {
            let _registration = self.pane_registration.lock();
            let result = (|| -> Result<Option<_>, Error> {
                let tab_needs_insert = self.tab_registration_needs_insert_locked(tab)?;

                match preparation_claim.as_ref() {
                    None => {
                        let tab_was_inserted = self.insert_tab_registration_locked(tab)?;
                        debug_assert_eq!(tab_was_inserted, tab_needs_insert);
                        Ok(None)
                    }
                    Some(claim) => {
                        let pane_id = claim.pane_id;
                        if !claim.is_authoritative_locked() {
                            return Err(PanePreparationCancelled { pane_id }.into());
                        }
                        if self.retiring_pane_ids.lock().contains(&pane_id)
                            || self.pane_retirements.has_in_flight_retirement(pane_id)
                        {
                            return Err(PaneIdCollision { pane_id }.into());
                        }

                        // Keep raw tab/pane readers from observing a half
                        // transaction. The registry serializer is outermost;
                        // tab-map then pane-map is the mux topology lock order.
                        let tab_id = tab.tab_id();
                        let mut tabs = self.tabs.write();
                        let tab_needs_insert = match tabs.get(&tab_id) {
                            Some(existing) if Arc::ptr_eq(existing, tab) => false,
                            Some(_) => {
                                return Err(anyhow!(
                                    "tab identifier {tab_id} is already registered to a different tab instance"
                                ));
                            }
                            None => true,
                        };
                        let mut panes = self.panes.write();
                        if let Some(existing) = panes.get(&pane_id) {
                            if Arc::ptr_eq(&existing.pane, &pane) {
                                return Err(anyhow!(
                                    "pane identifier {pane_id} became registered during a serialized preparation"
                                ));
                            }
                            return Err(PaneIdCollision { pane_id }.into());
                        }

                        let commit_guard = registration_reservation
                            .take()
                            .expect("a prepared pane retains its registration reservation")
                            .commit()?;
                        panes.insert(
                            pane_id,
                            LivePaneRegistration {
                                pane: Arc::clone(&pane),
                                generation: Arc::clone(&claim.generation),
                                domain_id: claim.domain_id,
                            },
                        );
                        if tab_needs_insert {
                            tabs.insert(tab_id, Arc::clone(tab));
                        }
                        drop(panes);
                        drop(tabs);

                        let lifecycle_notification = self
                            .enqueue_pane_lifecycle_notification_locked(
                                PaneLifecycleNotification::Added(pane_id),
                                reader_start_gate.take(),
                            );
                        let registration = commit_guard.finalize();
                        Ok(Some((lifecycle_notification, registration)))
                    }
                }
            })();
            if let Some(claim) = preparation_claim.as_mut() {
                claim.retire_locked();
            }
            result
        };
        let published_pane = publication_result?;

        let registration = if let Some((lifecycle_notification, registration)) = published_pane {
            self.complete_pane_lifecycle_notification(lifecycle_notification);
            self.notify_pane_registration_did_bind(&pane, &registration);
            Some(registration)
        } else {
            self.capture_pane_registration(&pane)
        };
        self.recompute_pane_count();
        Ok(registration)
    }

    fn take_pane_for_removal(
        &self,
        pane_id: PaneId,
        expected: Option<&Arc<dyn Pane>>,
        expected_generation: Option<&Arc<PaneRegistrationGeneration>>,
        removal_follow_up: PaneRemovalFollowUp,
    ) -> Option<RemovedPaneRegistration> {
        let (removed, needs_cleanup, cleanup_only_fence_owned, output_batch) = {
            let _registration = self.pane_registration.lock();
            let preparation_cancelled = if expected_generation.is_none() {
                self.cancel_pane_preparation_locked(pane_id, expected)
            } else {
                false
            };
            let registration = {
                let mut panes = self.panes.write();
                let matches = panes.get(&pane_id).is_some_and(|registered| {
                    expected.is_none_or(|expected| Arc::ptr_eq(&registered.pane, expected))
                        && expected_generation.is_none_or(|generation| {
                            Arc::ptr_eq(&registered.generation, generation)
                        })
                });
                if matches {
                    panes.remove(&pane_id).map(|registered| {
                        let pane = Arc::clone(&registered.pane);
                        let generation = Arc::clone(&registered.generation);
                        drop(registered);
                        (pane, generation)
                    })
                } else {
                    None
                }
            };
            // An unqualified removal remains the authoritative stale-state
            // sweep even when no registry entry survives. Fence that cleanup
            // so it cannot erase state belonging to a concurrent replacement.
            let needs_cleanup =
                expected.is_none() || preparation_cancelled || registration.is_some();
            let fence_inserted = needs_cleanup && self.retiring_pane_ids.lock().insert(pane_id);
            let cleanup_only_fence_owned = fence_inserted && registration.is_none();
            let output_batch = registration.as_ref().and_then(|(_, generation)| {
                self.take_pending_pane_output_batch_locked(pane_id, generation)
            });
            let removed = registration.map(|(pane, generation)| {
                let lifecycle_notification = self.enqueue_pane_removal_notification_locked(
                    pane_id,
                    &generation,
                    removal_follow_up,
                );
                RemovedPaneRegistration {
                    pane_id,
                    pane,
                    generation,
                    lifecycle_notification,
                }
            });
            (
                removed,
                needs_cleanup,
                cleanup_only_fence_owned,
                output_batch,
            )
        };

        if let Some(output_batch) = output_batch {
            histogram!("mux.notifications.pane_output.removal_forced_seal_rate").record(1.);
            output_batch.seal();
        }
        if needs_cleanup {
            self.discard_removed_pane_states(&[pane_id]);
        }
        if cleanup_only_fence_owned {
            let _registration = self.pane_registration.lock();
            self.retiring_pane_ids.lock().remove(&pane_id);
        }
        removed
    }

    fn finish_pane_removal(&self, removed: RemovedPaneRegistration, kill: bool) {
        let RemovedPaneRegistration {
            pane_id,
            pane,
            generation,
            lifecycle_notification,
        } = removed;
        generation.attach_cleanup(PaneRetirementCompletion {
            pane_id,
            pane,
            kill,
            lifecycle_notification,
            cleanup_complete: Arc::clone(&generation.cleanup_complete),
        });
    }

    fn remove_pane_internal(&self, pane_id: PaneId) {
        log::debug!("removing pane {}", pane_id);
        if let Some(removed) =
            self.take_pane_for_removal(pane_id, None, None, PaneRemovalFollowUp::None)
        {
            self.finish_pane_removal(removed, true);
            self.recompute_pane_count();
        }
    }

    #[cfg(test)]
    fn remove_pane_if_same(&self, pane_id: PaneId, expected: &Arc<dyn Pane>) {
        log::debug!("removing exact pane instance {}", pane_id);
        if let Some(removed) =
            self.take_pane_for_removal(pane_id, Some(expected), None, PaneRemovalFollowUp::None)
        {
            self.finish_pane_removal(removed, true);
            self.recompute_pane_count();
        }
    }

    fn remove_pane_if_same_generation(
        &self,
        pane_id: PaneId,
        expected: &Arc<dyn Pane>,
        generation: &Arc<PaneRegistrationGeneration>,
    ) {
        log::debug!("removing exact pane registration generation {}", pane_id);
        if let Some(removed) = self.take_pane_for_removal(
            pane_id,
            Some(expected),
            Some(generation),
            PaneRemovalFollowUp::None,
        ) {
            self.finish_pane_removal(removed, true);
            self.recompute_pane_count();
        }
    }

    fn take_tab_and_panes_for_removal(
        &self,
        tab_id: TabId,
        expected: Option<&Arc<Tab>>,
    ) -> Option<(Arc<Tab>, Vec<RemovedPaneRegistration>)> {
        let tab = self.tabs.read().get(&tab_id).map(Arc::clone)?;
        if expected.is_some_and(|expected| !Arc::ptr_eq(expected, &tab)) {
            return None;
        }

        let (removed_panes, output_batches) = {
            let _registration = self.pane_registration.lock();
            let mut tabs = self.tabs.write();
            if !tabs
                .get(&tab_id)
                .is_some_and(|registered| Arc::ptr_eq(registered, &tab))
            {
                return None;
            }
            let mut windows = self.windows.write();
            let prepared_windows = match self.prepare_exact_tab_detach_locked(
                &windows,
                std::slice::from_ref(&tab),
                None,
                false,
                "tab retirement",
            ) {
                Ok(prepared) => prepared,
                Err(error) => {
                    log::error!("refusing tab retirement before structural commit: {error:#}");
                    return None;
                }
            };
            let mut removed_windows = Vec::new();
            if let Err(error) = removed_windows.try_reserve_exact(prepared_windows.len()) {
                log::error!(
                    "refusing tab retirement before structural commit: cannot reserve exact empty-window receipts: {error}"
                );
                return None;
            }
            {
                let provisional = self.provisional_windows.lock();
                removed_windows.extend(prepared_windows.iter().filter_map(|(window_id, state)| {
                    (state.frozen().ordered_tabs().is_empty() && !provisional.contains(window_id))
                        .then_some(*window_id)
                }));
            }
            let result = tab.with_pane_snapshot_callback_free(|pane_snapshot| {
                let pane_candidates = self.resolve_tab_pane_candidates_locked(pane_snapshot);
                if !prepared_windows.is_empty() {
                    if let Err(error) = self.commit_prepared_window_states_locked(
                        &mut windows,
                        prepared_windows,
                        Vec::new(),
                        Vec::new(),
                        removed_windows,
                    ) {
                        log::error!("refusing tab retirement window commit: {error:#}");
                        return None;
                    }
                }
                tabs.remove(&tab_id);
                Some(self.take_tab_pane_candidates_for_removal_locked(pane_candidates))
            })?;
            result
        };
        self.finish_taken_tab_pane_state(&removed_panes, output_batches);
        Some((tab, removed_panes))
    }

    /// Resolve callback-free structural pane pointers to the numeric slots
    /// already owned by the pane registry or an in-flight preparation. The
    /// caller holds `pane_registration`, so the two maps cannot change between
    /// this census and retirement.
    fn resolve_tab_pane_candidates_locked(
        &self,
        pane_snapshot: Vec<Arc<dyn Pane>>,
    ) -> Vec<(PaneId, Arc<dyn Pane>)> {
        self.resolve_tab_pane_candidate_batches_locked(std::slice::from_ref(&pane_snapshot))
            .pop()
            .expect("one pane snapshot produces one candidate batch")
    }

    /// Batch variant of [`Self::resolve_tab_pane_candidates_locked`]. A
    /// window can own many tabs, so scanning the global pane registries once
    /// avoids turning close latency into `tabs * registered_panes` work.
    fn resolve_tab_pane_candidate_batches_locked(
        &self,
        pane_snapshots: &[Vec<Arc<dyn Pane>>],
    ) -> Vec<Vec<(PaneId, Arc<dyn Pane>)>> {
        if pane_snapshots.iter().all(Vec::is_empty) {
            return pane_snapshots.iter().map(|_| Vec::new()).collect();
        }
        let preparations = self.pane_preparations.lock();
        let panes = self.panes.read();
        let structural_identities = pane_snapshots
            .iter()
            .flatten()
            .map(|pane| Arc::as_ptr(pane) as *const () as usize)
            .collect::<HashSet<_>>();
        let mut pane_ids_by_identity = HashMap::with_capacity(structural_identities.len());
        for (pane_id, registered) in panes.iter() {
            let identity = Arc::as_ptr(&registered.pane) as *const () as usize;
            if structural_identities.contains(&identity) {
                pane_ids_by_identity.insert(identity, *pane_id);
            }
        }
        for (pane_id, preparing) in preparations.iter() {
            let identity = Weak::as_ptr(&preparing.pane) as *const () as usize;
            if structural_identities.contains(&identity) {
                pane_ids_by_identity.entry(identity).or_insert(*pane_id);
            }
        }
        pane_snapshots
            .iter()
            .map(|snapshot| {
                snapshot
                    .iter()
                    .filter_map(|pane| {
                        let identity = Arc::as_ptr(pane) as *const () as usize;
                        let pane_id = pane_ids_by_identity.remove(&identity)?;
                        Some((pane_id, Arc::clone(pane)))
                    })
                    .collect()
            })
            .collect()
    }

    /// Remove the pane-registry entries in one already-authorized tab
    /// snapshot. The caller holds `pane_registration` and has already removed
    /// the exact tab from `tabs`.
    fn take_tab_pane_candidates_for_removal_locked(
        &self,
        pane_candidates: Vec<(PaneId, Arc<dyn Pane>)>,
    ) -> (Vec<RemovedPaneRegistration>, Vec<Arc<PaneOutputBatch>>) {
        for (pane_id, expected) in &pane_candidates {
            self.cancel_pane_preparation_locked(*pane_id, Some(expected));
        }
        let removed_panes = {
            let mut panes = self.panes.write();
            pane_candidates
                .into_iter()
                .filter_map(|(pane_id, expected)| {
                    if panes
                        .get(&pane_id)
                        .is_some_and(|registered| Arc::ptr_eq(&registered.pane, &expected))
                    {
                        panes.remove(&pane_id).map(|registered| {
                            let pane = Arc::clone(&registered.pane);
                            let generation = Arc::clone(&registered.generation);
                            drop(registered);
                            (pane_id, pane, generation)
                        })
                    } else {
                        None
                    }
                })
                .collect::<Vec<_>>()
        };
        {
            let mut retiring = self.retiring_pane_ids.lock();
            for (pane_id, _, _) in &removed_panes {
                retiring.insert(*pane_id);
            }
        }
        let output_batches = removed_panes
            .iter()
            .filter_map(|(pane_id, _, generation)| {
                self.take_pending_pane_output_batch_locked(*pane_id, generation)
            })
            .collect::<Vec<_>>();
        let removed_panes = removed_panes
            .into_iter()
            .map(|(pane_id, pane, generation)| {
                let lifecycle_notification = self.enqueue_pane_removal_notification_locked(
                    pane_id,
                    &generation,
                    PaneRemovalFollowUp::None,
                );
                RemovedPaneRegistration {
                    pane_id,
                    pane,
                    generation,
                    lifecycle_notification,
                }
            })
            .collect::<Vec<_>>();
        (removed_panes, output_batches)
    }

    fn finish_taken_tab_pane_state(
        &self,
        removed_panes: &[RemovedPaneRegistration],
        output_batches: Vec<Arc<PaneOutputBatch>>,
    ) {
        for output_batch in output_batches {
            histogram!("mux.notifications.pane_output.removal_forced_seal_rate").record(1.);
            output_batch.seal();
        }
        let pane_ids = removed_panes
            .iter()
            .map(|removed| removed.pane_id)
            .collect::<Vec<_>>();
        self.discard_removed_pane_states(&pane_ids);
    }

    /// Allocation-prepare the complete exact-parentage sweep before any tab,
    /// pane, or window registry changes.
    fn prepare_exact_tab_detach_locked(
        &self,
        windows: &HashMap<WindowId, Window>,
        removals: &[Arc<Tab>],
        excluded_window: Option<WindowId>,
        allow_same_id_successor: bool,
        operation: &'static str,
    ) -> anyhow::Result<Vec<(WindowId, PreparedWindowState)>> {
        #[cfg(test)]
        self.validate_tab_parent_index_matches_windows_locked(windows)
            .with_context(|| format!("validate tab-parent index before {operation}"))?;

        let parents = self.tab_parents.read();
        let mut removals_by_window: HashMap<WindowId, HashSet<usize>> = HashMap::new();
        removals_by_window
            .try_reserve(removals.len())
            .map_err(|error| anyhow!("reserve {operation} indexed parents: {error}"))?;
        for tab in removals {
            let Some(parent) = parents.get(&tab.tab_id()) else {
                continue;
            };
            if !parent.is_same_tab(tab) {
                anyhow::ensure!(
                    allow_same_id_successor,
                    "{operation}: tab {} parent index names a different exact generation",
                    tab.tab_id()
                );
                continue;
            }
            if excluded_window == Some(parent.window_id) {
                continue;
            }
            let identities = removals_by_window.entry(parent.window_id).or_default();
            identities
                .try_reserve(1)
                .map_err(|error| anyhow!("reserve {operation} tab identity: {error}"))?;
            identities.insert(Arc::as_ptr(tab) as usize);
        }
        drop(parents);

        let mut prepared = Vec::new();
        prepared
            .try_reserve_exact(removals_by_window.len())
            .map_err(|error| anyhow!("reserve {operation} window transaction: {error}"))?;
        for (window_id, removals) in removals_by_window {
            let window = windows.get(&window_id).ok_or_else(|| {
                anyhow!("{operation}: indexed parent window {window_id} is absent")
            })?;
            if let Some(state) = window
                .prepare_remove_exact_identity_set(&removals)
                .with_context(|| format!("prepare {operation} in window {window_id}"))?
            {
                prepared.push((window_id, state));
            } else {
                anyhow::bail!(
                    "{operation}: indexed exact tab membership is absent from window {window_id}"
                );
            }
        }
        prepared.sort_unstable_by_key(|(window_id, _)| *window_id);
        Ok(prepared)
    }

    /// Atomically validate a delayed pane witness against an exact tab and
    /// commit that tab's registry retirement while its topology lock remains
    /// held. Pane IDs are resolved through the authoritative pane registry,
    /// avoiding re-entrant pane callbacks under the tab lock.
    fn take_tab_and_panes_for_removal_with_operation(
        &self,
        expected: &Arc<Tab>,
        operation: &PaneOperationGuard,
    ) -> Option<(Arc<Tab>, Vec<RemovedPaneRegistration>)> {
        let tab_id = expected.tab_id();
        let (removed_panes, output_batches) = {
            let _registration = self.pane_registration.lock();
            let mut tabs = self.tabs.write();
            if !tabs
                .get(&tab_id)
                .is_some_and(|registered| Arc::ptr_eq(registered, expected))
            {
                return None;
            }
            let mut windows = self.windows.write();
            let prepared_windows = match self.prepare_exact_tab_detach_locked(
                &windows,
                std::slice::from_ref(expected),
                None,
                false,
                "witnessed tab retirement",
            ) {
                Ok(prepared) => prepared,
                Err(error) => {
                    log::error!(
                        "refusing witnessed tab retirement before structural commit: {error:#}"
                    );
                    return None;
                }
            };
            let mut removed_windows = Vec::new();
            if let Err(error) = removed_windows.try_reserve_exact(prepared_windows.len()) {
                log::error!(
                    "refusing witnessed tab retirement before structural commit: cannot reserve exact empty-window receipts: {error}"
                );
                return None;
            }
            {
                let provisional = self.provisional_windows.lock();
                removed_windows.extend(prepared_windows.iter().filter_map(|(window_id, state)| {
                    (state.frozen().ordered_tabs().is_empty() && !provisional.contains(window_id))
                        .then_some(*window_id)
                }));
            }
            let result = expected.with_exact_pane_operation(operation, |pane_snapshot| {
                let pane_candidates = self.resolve_tab_pane_candidates_locked(pane_snapshot);
                if !prepared_windows.is_empty() {
                    if let Err(error) = self.commit_prepared_window_states_locked(
                        &mut windows,
                        prepared_windows,
                        Vec::new(),
                        Vec::new(),
                        removed_windows,
                    ) {
                        log::error!("refusing witnessed tab retirement window commit: {error:#}");
                        return None;
                    }
                }
                tabs.remove(&tab_id);
                Some(self.take_tab_pane_candidates_for_removal_locked(pane_candidates))
            })?;
            result?
        };
        self.finish_taken_tab_pane_state(&removed_panes, output_batches);
        Some((Arc::clone(expected), removed_panes))
    }

    fn remove_tab_internal(&self, tab_id: TabId) -> Option<Arc<Tab>> {
        log::debug!("remove_tab_internal tab {}", tab_id);

        let (tab, removed_panes) = self.take_tab_and_panes_for_removal(tab_id, None)?;

        self.flush_window_notifications();

        let pane_ids: Vec<PaneId> = removed_panes
            .iter()
            .map(|removed| removed.pane_id)
            .collect();
        log::debug!("panes to remove: {pane_ids:?}");
        for removed in removed_panes {
            self.finish_pane_removal(removed, true);
        }
        self.recompute_pane_count();

        Some(tab)
    }

    fn remove_tab_internal_if_same_with_pane_disposition(
        &self,
        expected: &Arc<Tab>,
        kill_remote_panes: bool,
    ) -> Option<Arc<Tab>> {
        let tab_id = expected.tab_id();
        log::debug!("remove exact tab instance {}", tab_id);

        let (tab, removed_panes) = self.take_tab_and_panes_for_removal(tab_id, Some(expected))?;

        self.flush_window_notifications();

        log::debug!(
            "removing {} panes from exact tab {tab_id}",
            removed_panes.len()
        );
        for removed in removed_panes {
            self.finish_pane_removal(removed, kill_remote_panes);
        }
        self.recompute_pane_count();

        Some(tab)
    }

    fn remove_empty_tab_internal_if_same(&self, expected: &Arc<Tab>) -> Option<Arc<Tab>> {
        let tab_id = expected.tab_id();
        let tab = {
            // Map authority precedes `Tab::inner`, matching window mutation
            // paths that retain the tab map while reconciling active panes.
            // Holding both through removal closes the final "dead tab gained a
            // pane" race without invoking pane callbacks in this scope.
            let _registration = self.pane_registration.lock();
            let mut tabs = self.tabs.write();
            if !tabs
                .get(&tab_id)
                .is_some_and(|current| Arc::ptr_eq(current, expected))
            {
                return None;
            }
            let mut windows = self.windows.write();
            let prepared_windows = match self.prepare_exact_tab_detach_locked(
                &windows,
                std::slice::from_ref(expected),
                None,
                false,
                "empty-tab retirement",
            ) {
                Ok(prepared) => prepared,
                Err(error) => {
                    log::error!(
                        "refusing empty-tab retirement before structural commit: {error:#}"
                    );
                    return None;
                }
            };
            let mut removed_windows = Vec::new();
            if let Err(error) = removed_windows.try_reserve_exact(prepared_windows.len()) {
                log::error!(
                    "refusing empty-tab retirement before commit: cannot reserve window-retirement payload: {error}"
                );
                return None;
            }
            {
                let provisional = self.provisional_windows.lock();
                removed_windows.extend(prepared_windows.iter().filter_map(|(window_id, state)| {
                    (state.frozen().ordered_tabs().is_empty() && !provisional.contains(window_id))
                        .then_some(*window_id)
                }));
            }
            let tab = expected.with_structurally_empty(|| {
                if !prepared_windows.is_empty() {
                    if let Err(error) = self.commit_prepared_window_states_locked(
                        &mut windows,
                        prepared_windows,
                        Vec::new(),
                        Vec::new(),
                        removed_windows,
                    ) {
                        log::error!("refusing empty-tab retirement window commit: {error:#}");
                        return None;
                    }
                }
                tabs.remove(&tab_id)
            })??;
            tab
        };
        self.flush_window_notifications();
        self.recompute_pane_count();
        Some(tab)
    }

    fn remove_window_internal(&self, window_id: WindowId) {
        self.remove_window_internal_with_notification(window_id, true);
    }

    /// Stage a complete window retirement before releasing registration
    /// authority. All exact tabs owned by the window remain topology-locked
    /// from their pane census through window, tab, and pane registry commit.
    /// An optional delayed-operation witness is validated in that same frozen
    /// structural cut.
    fn take_window_and_tabs_for_removal(
        &self,
        window_id: WindowId,
        expected: Option<(&Arc<Tab>, &PaneOperationGuard)>,
    ) -> Option<RemovedWindowRegistration> {
        let mut removed_tabs = {
            let _registration = self.pane_registration.lock();
            let mut tabs = self.tabs.write();
            if expected.is_some_and(|(expected_tab, _)| {
                !tabs
                    .get(&expected_tab.tab_id())
                    .is_some_and(|registered| Arc::ptr_eq(registered, expected_tab))
            }) {
                return None;
            }

            let mut windows = self.windows.write();
            let window = windows.get(&window_id)?;
            if expected.is_some_and(|(expected_tab, _)| {
                !window
                    .iter()
                    .any(|candidate| Arc::ptr_eq(candidate, expected_tab))
            }) {
                return None;
            }

            let mut seen = HashSet::new();
            let retired_tabs = window
                .iter()
                .filter(|tab| {
                    tabs.get(&tab.tab_id())
                        .is_some_and(|registered| Arc::ptr_eq(registered, tab))
                        && seen.insert(Arc::as_ptr(tab) as usize)
                })
                .cloned()
                .collect::<Vec<_>>();
            let mut prepared_windows = match self.prepare_exact_tab_detach_locked(
                &windows,
                &retired_tabs,
                None,
                false,
                "window retirement",
            ) {
                Ok(prepared) => prepared,
                Err(error) => {
                    log::error!(
                        "refusing window {window_id} retirement before structural commit: {error:#}"
                    );
                    return None;
                }
            };
            let was_provisional = self.provisional_windows.lock().contains(&window_id);
            if was_provisional {
                prepared_windows.retain(|(prepared_id, _)| *prepared_id != window_id);
            } else if !prepared_windows
                .iter()
                .any(|(prepared_id, _)| *prepared_id == window_id)
            {
                let retirement = windows
                    .get(&window_id)
                    .expect("validated window remains present")
                    .prepare_retirement_marker()
                    .map_err(|error| {
                        log::error!(
                            "refusing empty window {window_id} retirement before commit: {error:#}"
                        );
                    })
                    .ok()?;
                prepared_windows.push((window_id, retirement));
            }

            let removed_tabs = Tab::with_pane_snapshots_callback_free(
                &retired_tabs,
                expected,
                |pane_snapshots| {
                    if !prepared_windows.is_empty() {
                        let removed_windows = if was_provisional {
                            Vec::new()
                        } else {
                            let mut removed = Vec::new();
                            if removed.try_reserve_exact(1).is_err() {
                                return None;
                            }
                            removed.push(window_id);
                            removed
                        };
                        if let Err(error) = self.commit_prepared_window_states_locked(
                            &mut windows,
                            prepared_windows,
                            Vec::new(),
                            Vec::new(),
                            removed_windows,
                        ) {
                            log::error!("refusing window retirement commit: {error:#}");
                            return None;
                        }
                    }
                    if was_provisional {
                        windows
                            .remove(&window_id)
                            .expect("validated provisional window remains present");
                        self.provisional_windows.lock().remove(&window_id);
                    }
                    for tab in &retired_tabs {
                        tabs.remove(&tab.tab_id());
                    }

                    let pane_candidate_batches =
                        self.resolve_tab_pane_candidate_batches_locked(&pane_snapshots);
                    Some(
                        pane_snapshots
                            .into_iter()
                            .zip(pane_candidate_batches)
                            .map(|(structural_panes, pane_candidates)| {
                                let (removed_panes, output_batches) = self
                                    .take_tab_pane_candidates_for_removal_locked(pane_candidates);
                                RemovedTabRegistration {
                                    structural_panes,
                                    removed_panes,
                                    output_batches,
                                }
                            })
                            .collect::<Vec<_>>(),
                    )
                },
            )??;
            removed_tabs
        };

        for removed in &mut removed_tabs {
            self.finish_taken_tab_pane_state(
                &removed.removed_panes,
                std::mem::take(&mut removed.output_batches),
            );
        }
        Some(RemovedWindowRegistration { removed_tabs })
    }

    /// Finish a structurally committed window retirement. Every callback runs
    /// after the window, tab, and pane registries have rejected the doomed
    /// identities, so re-entrant topology work cannot resurrect them.
    fn finish_removed_window(&self, removed_window: RemovedWindowRegistration) {
        // Pane retirement can synchronously dispatch PaneRemoved. Drain the
        // already-queued parent removal first on the established inline path;
        // protocol bridges additionally enforce the reserved revision order
        // when a configured scheduler requires main-thread window delivery.
        self.flush_window_notifications();

        let mut domains_of_window = HashSet::new();
        for removed_tab in &removed_window.removed_tabs {
            for pane in &removed_tab.structural_panes {
                match catch_recoverable(
                    RecoverablePanicSite::MuxPaneCallback,
                    std::panic::AssertUnwindSafe(|| pane.domain_id()),
                ) {
                    Ok(domain_id) => {
                        domains_of_window.insert(domain_id);
                    }
                    Err(_) => {
                        log::error!(
                            "pane domain callback panicked during window retirement; continuing cleanup"
                        );
                    }
                }
            }
        }

        for domain_id in domains_of_window {
            if let Some(domain) = self.get_domain(domain_id) {
                let detach = catch_recoverable(
                    RecoverablePanicSite::MuxWindowCallback,
                    std::panic::AssertUnwindSafe(|| {
                        if !domain.detachable() {
                            return Ok(());
                        }
                        log::info!("detaching domain {domain_id}");
                        domain.detach().map_err(|err| format!("{err:#}"))
                    }),
                );
                match detach {
                    Ok(Ok(())) => {}
                    Ok(Err(err)) => {
                        log::error!("while detaching domain {domain_id}: {err}");
                    }
                    Err(_) => {
                        log::error!(
                            "domain {domain_id} callback panicked during window retirement; continuing cleanup"
                        );
                    }
                }
            }
        }

        for removed_tab in removed_window.removed_tabs {
            for removed in removed_tab.removed_panes {
                self.finish_pane_removal(removed, true);
            }
        }
    }

    fn cancel_provisional_window(&self, window_id: WindowId, mut activity: Option<Activity>) {
        let (removed, preserved) = {
            let mut windows = self.windows.write();
            if !self.provisional_windows.lock().remove(&window_id) {
                return;
            }
            match windows.get(&window_id) {
                Some(window) if window.is_empty() => {
                    windows.remove(&window_id);
                    (true, false)
                }
                Some(_) => {
                    // Linearize publication while the map lock still protects
                    // the surviving topology. Any later removal queues behind
                    // this creation event in the same exact-owner FIFO.
                    self.queue_window_notification_entry(
                        MuxNotification::WindowCreated(window_id),
                        activity.take(),
                    );
                    (false, true)
                }
                None => (false, false),
            }
        };
        if removed {
            self.recompute_pane_count();
        }
        if preserved {
            self.flush_window_notifications();
        }
    }

    fn remove_window_if_empty_internal(&self, window_id: WindowId) -> bool {
        let removed = {
            let mut windows = self.windows.write();
            if self.provisional_windows.lock().contains(&window_id) {
                // Only the exact MuxWindowBuilder cancellation/publication
                // transaction may retire an unpublished window. An activity
                // count is an admission hint, not a topology lock; a prune
                // admitted just before new_empty_window can otherwise reap
                // the newly published provisional window.
                false
            } else if let Some(window) = windows.get(&window_id).filter(|window| window.is_empty())
            {
                let state = match window.prepare_retirement_marker() {
                    Ok(state) => state,
                    Err(error) => {
                        log::error!(
                            "refusing empty window {window_id} retirement before commit: {error:#}"
                        );
                        return false;
                    }
                };
                let mut prepared = Vec::new();
                let mut removed_windows = Vec::new();
                if prepared.try_reserve_exact(1).is_err()
                    || removed_windows.try_reserve_exact(1).is_err()
                {
                    log::error!("refusing empty window {window_id} retirement: allocation failed");
                    return false;
                }
                prepared.push((window_id, state));
                removed_windows.push(window_id);
                match self.commit_prepared_window_states_locked(
                    &mut windows,
                    prepared,
                    Vec::new(),
                    Vec::new(),
                    removed_windows,
                ) {
                    Ok(()) => true,
                    Err(error) => {
                        log::error!("refusing empty window {window_id} retirement: {error:#}");
                        false
                    }
                }
            } else {
                false
            }
        };
        if removed {
            self.recompute_pane_count();
            self.flush_window_notifications();
        }
        removed
    }

    fn remove_window_internal_with_notification(&self, window_id: WindowId, notify_removed: bool) {
        log::debug!("remove_window_internal {}", window_id);

        if !notify_removed {
            // A cancelled builder represents an unpublished, provisional
            // window.  Remove it only while it is still empty.  If another
            // topology operation raced a live tab into the window, silently
            // tearing it down here would detach that tab's domain without
            // ever publishing a corresponding window lifecycle event.
            let removed = {
                let mut windows = self.windows.write();
                if windows
                    .get(&window_id)
                    .is_some_and(|window| window.is_empty())
                {
                    let removed = windows.remove(&window_id).is_some();
                    self.provisional_windows.lock().remove(&window_id);
                    removed
                } else {
                    false
                }
            };
            if removed {
                self.recompute_pane_count();
            } else {
                log::debug!(
                    "preserving provisional window {window_id}: it is absent or acquired live topology"
                );
            }
            return;
        }

        if let Some(removed_window) = self.take_window_and_tabs_for_removal(window_id, None) {
            self.finish_removed_window(removed_window);
        }
        self.recompute_pane_count();
        self.flush_window_notifications();
    }

    pub fn remove_pane(&self, pane_id: PaneId) {
        self.remove_pane_internal(pane_id);
        self.prune_dead_windows();
    }

    pub fn remove_tab(&self, tab_id: TabId) -> Option<Arc<Tab>> {
        let tab = self.remove_tab_internal(tab_id);
        self.prune_dead_windows();
        tab
    }

    /// Remove and kill only the exact tab generation supplied by the caller.
    ///
    /// The pane-registration witness prevents an old `Arc<Tab>` from
    /// authorizing removal after that same tab object was removed and
    /// re-registered. Deferred GUI confirmation must retain both authorities
    /// rather than re-resolving numeric IDs after user think time.
    pub fn remove_tab_if_same(
        self: &Arc<Self>,
        expected: &Arc<Tab>,
        witness: &PaneRegistrationHandle,
    ) -> bool {
        let Some(operation) = witness.operation_guard(self) else {
            return false;
        };
        let Some((tab, removed_panes)) =
            self.take_tab_and_panes_for_removal_with_operation(expected, &operation)
        else {
            return false;
        };
        // The delayed-operation lease authorizes only the frozen structural
        // commit above, which already included exact affected-window cleanup.
        // Release it before attaching pane-retirement cleanup so PaneRemoved
        // lifecycle delivery cannot unnecessarily retain the operation lease;
        // the retired-ID fence still prevents same-ID reuse through subscriber
        // cleanup.
        drop(operation);
        self.flush_window_notifications();

        log::debug!(
            "removing {} panes from exact witnessed tab {}",
            removed_panes.len(),
            tab.tab_id()
        );
        for removed in removed_panes {
            self.finish_pane_removal(removed, true);
        }
        self.recompute_pane_count();
        true
    }

    /// Drop the LOCAL mirror of a tab without disturbing the remote session.
    ///
    /// Mirrors [`Mux::remove_tab`] (registry removal, detach from every window,
    /// drop the tab's panes, prune now-empty windows) except that its batched
    /// pane removal does not call [`Pane::kill`], so no `Pdu::KillPane` is sent.
    /// This is the safety crux of the window-unify feature: when two local tabs
    /// mirror the same remote session, the duplicate's mirror is dropped here
    /// while the canonical window keeps its mirror and the remote session stays
    /// alive.
    pub fn remove_tab_local_only(&self, tab_id: TabId) -> Option<Arc<Tab>> {
        log::debug!("remove_tab_local_only tab {}", tab_id);

        let (tab, removed_panes) = self.take_tab_and_panes_for_removal(tab_id, None)?;

        self.flush_window_notifications();

        log::debug!(
            "dropping {} panes from local-only tab {tab_id}",
            removed_panes.len()
        );
        for removed in removed_panes {
            self.finish_pane_removal(removed, false);
        }
        self.recompute_pane_count();
        self.prune_dead_windows();

        Some(tab)
    }

    /// Roll back one exact local tab mirror, including an already-installed
    /// pane tree, without sending remote pane termination requests.
    ///
    /// Exact Arc identity prevents an exhausted/reused numeric tab ID from
    /// removing a replacement. This is the populated counterpart to
    /// [`Mux::remove_empty_tab_local_only_if_same`] for transactions that own
    /// the staged tab from registration through publication. An empty
    /// provisional parent remains under its exact [`MuxWindowBuilder`]'s
    /// authority and is retired when that transaction calls
    /// [`MuxWindowBuilder::cancel`]; a general prune must not publish or reap
    /// another transaction's provisional window. Once attachment has
    /// published that window, the exact detach receipt may synchronously
    /// retire it if this rollback left it empty. The receipt also avoids an
    /// unrelated O(all windows) maintenance sweep on this rollback path.
    pub fn remove_tab_local_only_if_same(&self, expected: &Arc<Tab>) -> bool {
        self.remove_tab_internal_if_same_with_pane_disposition(expected, false)
            .is_some()
    }

    /// Roll back an exact, still-empty local tab registration without risking
    /// removal of a concurrent replacement that happens to carry the same ID.
    ///
    /// This is intentionally narrower than [`Mux::remove_tab_local_only`]: it
    /// refuses to remove a tab once any pane tree has been installed. That
    /// makes it suitable for unwinding fallible topology staging without ever
    /// killing a remote pane or tearing down topology published after the
    /// staging handle became stale.
    pub fn remove_empty_tab_local_only_if_same(&self, expected: &Arc<Tab>) -> bool {
        if self.remove_empty_tab_internal_if_same(expected).is_none() {
            return false;
        }
        true
    }

    pub fn prune_dead_windows(&self) {
        self.prune_dead_windows_impl(false);
    }

    fn prune_dead_windows_ignoring_activity(&self) {
        self.prune_dead_windows_impl(true);
    }

    fn prune_dead_windows_impl(&self, ignore_activity: bool) {
        let activity_count = Activity::count_for_mux(self);
        if !ignore_activity && activity_count > 0 {
            log::trace!("prune_dead_windows: exact activity count={activity_count}");
            return;
        }
        // Snapshot exact tab identities before invoking pane callbacks. A tab
        // may be retained by a window after leaving the mux registry, and a
        // later tab may reuse its numeric ID in adversarial/remote scenarios.
        // Pointer keys remain valid for this pass because every key has a
        // retained Arc in `live_tabs` or `window_tabs`.
        let live_tabs = self.tabs.read().clone();
        let window_tabs = self
            .windows
            .read()
            .values()
            .flat_map(|window| window.iter().cloned())
            .collect::<Vec<_>>();
        let mut tab_candidates = live_tabs.values().cloned().collect::<Vec<_>>();
        let mut candidate_identities = tab_candidates
            .iter()
            .map(|tab| Arc::as_ptr(tab) as usize)
            .collect::<HashSet<_>>();
        for tab in window_tabs {
            if candidate_identities.insert(Arc::as_ptr(&tab) as usize) {
                tab_candidates.push(tab);
            }
        }

        let mut dead_pane_registrations = Vec::new();
        let mut dead_tabs = Vec::new();
        let mut stale_window_tabs = Vec::new();
        for tab in tab_candidates {
            let is_registered = live_tabs
                .get(&tab.tab_id())
                .is_some_and(|registered| Arc::ptr_eq(registered, &tab));
            if !is_registered {
                stale_window_tabs.push(tab);
                continue;
            }
            let (_, mut registrations) = tab.prune_dead_panes_deferred(self);
            dead_pane_registrations.append(&mut registrations);
            if tab.with_structurally_empty(|| ()).is_some() {
                dead_tabs.push(tab);
            }
        }

        PaneRegistrationHandle::retire_batch_if_current(dead_pane_registrations);

        for tab in dead_tabs {
            log::trace!("exact tab {} is dead", tab.tab_id());
            self.remove_empty_tab_internal_if_same(&tab);
        }

        // Revalidate stale exact tab Arcs against the live registry while
        // holding its read lock through the window mutation. This prevents a
        // same-ID exact re-registration from being stripped after callbacks.
        let dead_windows = {
            let tabs = self.tabs.read();
            let stale_tabs = stale_window_tabs
                .iter()
                .filter(|stale| {
                    tabs.get(&stale.tab_id())
                        .is_none_or(|current| !Arc::ptr_eq(current, stale))
                })
                .cloned()
                .collect::<Vec<_>>();
            let mut windows = self.windows.write();
            let provisional_windows = self.provisional_windows.lock();
            let prepared = match self.prepare_exact_tab_detach_locked(
                &windows,
                &stale_tabs,
                None,
                true,
                "stale-parent prune",
            ) {
                Ok(prepared) => prepared,
                Err(error) => {
                    log::error!("refusing stale-parent prune before commit: {error:#}");
                    Vec::new()
                }
            };
            if !prepared.is_empty() {
                if let Err(error) = self.commit_prepared_window_states_locked(
                    &mut windows,
                    prepared,
                    Vec::new(),
                    Vec::new(),
                    Vec::new(),
                ) {
                    log::error!("refusing stale-parent prune window commit: {error:#}");
                }
            }
            let dead_windows = windows
                .iter()
                .filter_map(|(window_id, window)| {
                    (window.is_empty() && !provisional_windows.contains(window_id))
                        .then_some(*window_id)
                })
                .collect::<Vec<_>>();
            dead_windows
        };
        self.flush_window_notifications();

        for window_id in dead_windows {
            log::trace!("window {} is dead", window_id);
            self.remove_window_if_empty_internal(window_id);
        }

        if self.notify_empty_if_current() {
            log::trace!("prune_dead_windows: is_empty, send MuxNotification::Empty");
        } else {
            log::trace!("prune_dead_windows: not empty");
        }
    }

    pub fn kill_window(&self, window_id: WindowId) {
        self.remove_window_internal(window_id);
        self.prune_dead_windows();
    }

    /// Kill a window only while it still contains the exact tab generation
    /// observed by a delayed caller.
    ///
    /// The pane witness binds the tab to the originating mux generation; the
    /// window-map transaction binds the final removal to exact tab identity.
    /// This is the deferred-confirmation counterpart to [`Self::kill_window`].
    pub fn kill_window_if_contains_exact_tab(
        self: &Arc<Self>,
        window_id: WindowId,
        expected_tab: &Arc<Tab>,
        witness: &PaneRegistrationHandle,
    ) -> bool {
        let Some(operation) = witness.operation_guard(self) else {
            return false;
        };
        let Some(removed_window) =
            self.take_window_and_tabs_for_removal(window_id, Some((expected_tab, &operation)))
        else {
            return false;
        };
        // Structural authority has committed. Do not retain the witness lease
        // through domain callbacks and pruning; pane retirement retains its
        // separate same-ID reuse fence until cleanup fanout is complete.
        drop(operation);
        self.finish_removed_window(removed_window);
        self.recompute_pane_count();
        self.flush_window_notifications();
        self.prune_dead_windows();
        true
    }

    pub fn get_window(&self, window_id: WindowId) -> Option<MappedRwLockReadGuard<'_, Window>> {
        RwLockReadGuard::try_map(self.windows.read(), |windows| windows.get(&window_id)).ok()
    }

    pub fn get_window_mut(&self, window_id: WindowId) -> Option<MuxWindowWriteGuard<'_>> {
        let guard =
            RwLockWriteGuard::try_map(self.windows.write(), |windows| windows.get_mut(&window_id))
                .ok()?;
        Some(MuxWindowWriteGuard {
            guard: Some(guard),
            mux: self,
        })
    }

    /// Non-blocking variant of [`Self::get_window_mut`].
    ///
    /// Returns `None` when the window registry is contended or the window is
    /// absent.
    pub fn try_get_window_mut(&self, window_id: WindowId) -> Option<MuxWindowWriteGuard<'_>> {
        let guard = RwLockWriteGuard::try_map(self.windows.try_write()?, |windows| {
            windows.get_mut(&window_id)
        })
        .ok()?;
        Some(MuxWindowWriteGuard {
            guard: Some(guard),
            mux: self,
        })
    }

    pub fn set_window_title(&self, window_id: WindowId, title: &str) -> bool {
        let changed = {
            let mut windows = self.windows.write();
            let Some(window) = windows.get_mut(&window_id) else {
                return false;
            };
            let changed = window.set_title_without_notify(title);
            if changed {
                self.queue_window_notification(MuxNotification::WindowTitleChanged {
                    window_id,
                    title: title.to_string(),
                });
            }
            changed
        };
        self.flush_window_notifications();
        changed
    }

    pub fn set_tab_title(&self, tab_id: TabId, title: &str) -> bool {
        let Some(tab) = self.get_tab(tab_id) else {
            return false;
        };
        let (changed, notification) = tab.set_title_for_mux(title, Some(self));
        if let Some(notification) = notification {
            self.dispatch_notification_envelope(notification);
        }
        changed
    }

    /// Freeze one window's exact ordered tab pointers and active identity.
    pub fn window_order_snapshot(
        &self,
        window_id: WindowId,
    ) -> Result<Option<FrozenWindowOrder>, WindowOrderSnapshotError> {
        let windows = self.windows.read();
        windows
            .get(&window_id)
            .map(Window::order_snapshot)
            .transpose()
    }

    /// Atomically compare-and-set one complete same-window tab permutation.
    ///
    /// All request, membership, active-identity, window-revision, and global
    /// topology-revision checks finish before the ordered vector changes. A
    /// successful transaction queues exactly one frozen topology event while
    /// the window lock is still held, then invokes subscribers only after all
    /// mux locks and the idempotency ledger have been released.
    pub fn reorder_window_tabs(
        &self,
        request: ReorderWindowTabsRequest,
    ) -> ReorderWindowTabsResult {
        if let Err(malformed) = validate_reorder_window_tabs_request(&request) {
            return ReorderWindowTabsResult::Decision(WindowReorderTerminalOutcome::Malformed(
                malformed,
            ));
        }
        let mut notification_queued = false;
        let outcome = {
            let mut windows = self.windows.write();
            let mut topology = self.topology.lock();
            if request.session_incarnation != topology.session_incarnation {
                return ReorderWindowTabsResult::Decision(
                    WindowReorderTerminalOutcome::StaleIncarnation,
                );
            }
            let Some(window) = windows.get_mut(&request.window_id) else {
                return ReorderWindowTabsResult::Decision(
                    WindowReorderTerminalOutcome::MissingWindow {
                        window_id: request.window_id,
                    },
                );
            };

            let mut receipts = self.window_order_receipts.lock();
            if let Some(replay_or_equivocation) =
                receipts.lookup(request.mutation_id, request.request_digest)
            {
                return replay_or_equivocation;
            }

            let outcome = match window
                .validate_exact_order(&request.desired_tab_ids, request.desired_active_tab_id)
            {
                Err(PrepareWindowOrderError::RevisionExhausted(_)) => {
                    WindowReorderTerminalOutcome::Exhausted
                }
                Err(PrepareWindowOrderError::InvalidCurrentState(_)) => {
                    WindowReorderTerminalOutcome::Malformed(
                        WindowReorderMalformed::InvalidCurrentState,
                    )
                }
                Err(PrepareWindowOrderError::DuplicateTabId { tab_id }) => {
                    WindowReorderTerminalOutcome::Malformed(
                        WindowReorderMalformed::DuplicateTabId { tab_id },
                    )
                }
                Err(PrepareWindowOrderError::MissingTabId { tab_id }) => {
                    WindowReorderTerminalOutcome::Malformed(WindowReorderMalformed::MissingTabId {
                        tab_id,
                    })
                }
                Err(PrepareWindowOrderError::ForeignTabId { tab_id }) => {
                    WindowReorderTerminalOutcome::Malformed(WindowReorderMalformed::ForeignTabId {
                        tab_id,
                    })
                }
                Err(PrepareWindowOrderError::ActiveTabChanged {
                    current_active_tab_id,
                    desired_active_tab_id,
                }) => WindowReorderTerminalOutcome::Malformed(
                    WindowReorderMalformed::ActiveTabChanged {
                        current_active_tab_id,
                        desired_active_tab_id,
                    },
                ),
                Ok(_) if window.order_revision() != request.expected_order_revision => {
                    WindowReorderTerminalOutcome::Conflict(WindowOrderCommit {
                        topology_revision: topology.current_revision(),
                        window: WindowOrderState::from_semantically_validated_window(window),
                    })
                }
                Ok(validated) => match window.prepare_validated_order(validated) {
                    Err(_) => WindowReorderTerminalOutcome::Exhausted,
                    Ok(prepared) => match topology.reserve_revision() {
                        Err(_) => WindowReorderTerminalOutcome::Exhausted,
                        Ok(topology_revision) => {
                            let window = window.commit_prepared_order(prepared);
                            let commit = WindowOrderCommit {
                                topology_revision,
                                window: WindowOrderState::from_frozen(&window),
                            };
                            self.queue_window_notification_envelope(
                                MuxNotificationEnvelope {
                                    notification: MuxNotification::WindowOrderChanged {
                                        mutation_id: request.mutation_id,
                                        request_digest: request.request_digest,
                                        window,
                                    },
                                    topology: MuxTopologyStamp::Revision(topology_revision),
                                },
                                None,
                            );
                            notification_queued = true;
                            WindowReorderTerminalOutcome::Applied(commit)
                        }
                    },
                },
            };
            receipts.retain(request.mutation_id, request.request_digest, outcome.clone());
            outcome
        };
        if notification_queued {
            self.flush_window_notifications();
        }
        ReorderWindowTabsResult::Decision(outcome)
    }

    pub fn get_active_tab_for_window(&self, window_id: WindowId) -> Option<Arc<Tab>> {
        let window = self.get_window(window_id)?;
        window.get_active().map(Arc::clone)
    }

    /// Change one window's active tab through the frozen topology stream.
    ///
    /// Invalid indices and either revision-counter exhaustion fail before the
    /// active identity, last-active identity, focus callbacks, or notification
    /// queue changes.
    pub fn activate_tab_at_index(
        &self,
        window_id: WindowId,
        tab_index: usize,
        save_last_active: bool,
    ) -> anyhow::Result<bool> {
        let changed = {
            let tabs = self.tabs.read();
            let mut windows = self.windows.write();
            let window = windows
                .get(&window_id)
                .ok_or_else(|| anyhow!("activate_tab_at_index: no such window {window_id}"))?;
            let selected = window.get_by_idx(tab_index).ok_or_else(|| {
                anyhow!(
                    "activate_tab_at_index: index {tab_index} is out of range for window {window_id}"
                )
            })?;
            let selected_id = selected.tab_id();
            anyhow::ensure!(
                tabs.get(&selected_id)
                    .is_some_and(|registered| Arc::ptr_eq(registered, selected)),
                "activate_tab_at_index: tab {selected_id} is not the current registered instance"
            );
            let Some(state) = window.prepare_set_active(tab_index, save_last_active)? else {
                return Ok(false);
            };
            let mut prepared = Vec::new();
            prepared
                .try_reserve_exact(1)
                .map_err(|error| anyhow!("reserve active-tab transaction: {error}"))?;
            prepared.push((window_id, state));
            self.commit_prepared_window_states_locked(
                &mut windows,
                prepared,
                Vec::new(),
                Vec::new(),
                Vec::new(),
            )?;
            true
        };
        self.flush_window_notifications();
        Ok(changed)
    }

    /// Activate one exact registered tab in one exact window.
    ///
    /// Unlike an index captured through `get_window`, the tab identity is
    /// resolved while the tab-registry read guard and window-map write guard
    /// are both held. A concurrent reorder or same-id tab replacement can
    /// therefore only make this operation fail; it can never redirect focus
    /// to a different tab.
    pub fn activate_tab_exact_in_window(
        &self,
        window_id: WindowId,
        expected: &Arc<Tab>,
        save_last_active: bool,
    ) -> anyhow::Result<bool> {
        let tab_id = expected.tab_id();
        let changed = {
            let tabs = self.tabs.read();
            anyhow::ensure!(
                tabs.get(&tab_id)
                    .is_some_and(|registered| Arc::ptr_eq(registered, expected)),
                "activate exact tab: tab {tab_id} is not the current registered instance"
            );
            let mut windows = self.windows.write();
            let window = windows
                .get(&window_id)
                .ok_or_else(|| anyhow!("activate exact tab: no such window {window_id}"))?;
            let tab_index = window
                .iter()
                .position(|candidate| Arc::ptr_eq(candidate, expected))
                .ok_or_else(|| {
                    anyhow!(
                        "activate exact tab: tab {tab_id} is not attached to window {window_id}"
                    )
                })?;
            let Some(state) = window.prepare_set_active(tab_index, save_last_active)? else {
                return Ok(false);
            };
            let mut prepared = Vec::new();
            prepared
                .try_reserve_exact(1)
                .map_err(|error| anyhow!("reserve exact active-tab transaction: {error}"))?;
            prepared.push((window_id, state));
            self.commit_prepared_window_states_locked(
                &mut windows,
                prepared,
                Vec::new(),
                Vec::new(),
                Vec::new(),
            )?;
            true
        };
        self.flush_window_notifications();
        Ok(changed)
    }

    pub fn window_can_close_without_prompting(&self, window_id: WindowId) -> Option<bool> {
        let tabs = {
            let window = self.get_window(window_id)?;
            window.iter().cloned().collect::<Vec<_>>()
        };
        Some(
            tabs.into_iter()
                .all(|tab| tab.can_close_without_prompting(CloseReason::Window)),
        )
    }

    pub fn window_has_panes_in_domain(&self, window_id: WindowId, domain_id: DomainId) -> bool {
        let Some(window) = self.get_window(window_id) else {
            return false;
        };

        for tab in window.iter() {
            if tab.has_panes_in_domain(domain_id) {
                return true;
            }
        }

        false
    }

    pub fn new_empty_window(
        self: &Arc<Self>,
        workspace: Option<String>,
        position: Option<GuiPosition>,
    ) -> MuxWindowBuilder {
        // Bind the deferred-event queue before the empty provisional window is
        // published.  Even a cancellation or global-mux swap before its first
        // mutation must retain this exact mux as notification authority.
        self.bind_window_notification_owner();
        let workspace = Some(workspace.unwrap_or_else(|| self.active_workspace()));
        let window = Window::new_for_owner(workspace, position, Arc::downgrade(self));
        let window_id = window.window_id();
        // Acquire the pruning lease before publication. Otherwise an
        // off-thread prune can remove the empty provisional window between
        // insertion and builder construction, producing Created-after-Removed.
        let activity = Activity::new_for_mux(self);
        {
            let mut windows = self.windows.write();
            windows.insert(window_id, window);
            self.provisional_windows.lock().insert(window_id);
        }
        MuxWindowBuilder {
            window_id,
            owner: Arc::downgrade(self),
            activity: Some(activity),
            provisional: true,
            notified: false,
        }
    }

    pub fn add_tab_to_window(&self, tab: &Arc<Tab>, window_id: WindowId) -> anyhow::Result<()> {
        let tab_id = tab.tab_id();
        {
            let tabs = self.tabs.read();
            anyhow::ensure!(
                tabs.get(&tab_id)
                    .is_some_and(|registered| Arc::ptr_eq(registered, tab)),
                "add_tab_to_window: tab {tab_id} is not the exact registered tab instance"
            );
            let mut windows = self.windows.write();
            let existing_parent = self.tab_parents.read().get(&tab_id).cloned();
            if let Some(existing_parent) = existing_parent {
                let existing_window = existing_parent.window_id;
                let identity = if existing_parent.is_same_tab(tab) {
                    "exact instance"
                } else {
                    "different same-id instance"
                };
                return Err(anyhow!(
                    "add_tab_to_window: tab {tab_id} ({identity}) is already attached to window \
                     {existing_window}"
                ));
            }
            let window = windows
                .get_mut(&window_id)
                .ok_or_else(|| anyhow!("add_tab_to_window: no such window_id {}", window_id))?;
            anyhow::ensure!(
                window.idx_by_id(tab_id).is_none(),
                "add_tab_to_window: window {window_id} already contains tab id {tab_id}"
            );
            let prepared_state = window.prepare_insert(window.len(), tab)?;
            let mut prepared = Vec::new();
            prepared
                .try_reserve_exact(1)
                .map_err(|error| anyhow!("reserve tab attachment transaction: {error}"))?;
            prepared.push((window_id, prepared_state));
            let mut attached_tabs = Vec::new();
            attached_tabs
                .try_reserve_exact(1)
                .map_err(|error| anyhow!("reserve tab attachment payload: {error}"))?;
            attached_tabs.push((tab_id, window_id));
            let mut created_windows = Vec::new();
            if self.provisional_windows.lock().contains(&window_id) {
                created_windows
                    .try_reserve_exact(1)
                    .map_err(|error| anyhow!("reserve window creation payload: {error}"))?;
                created_windows.push(window_id);
            }
            self.commit_prepared_window_states_locked(
                &mut windows,
                prepared,
                attached_tabs,
                created_windows,
                Vec::new(),
            )?;
        }
        self.recompute_pane_count();
        self.flush_window_notifications();
        Ok(())
    }

    pub fn window_containing_tab(&self, tab_id: TabId) -> Option<WindowId> {
        #[cfg(test)]
        self.tab_parent_lookup_probes
            .fetch_add(1, Ordering::Relaxed);
        // Keep the parent guard through the weak-generation upgrade. Membership
        // commits hold this index's write guard across both the window-vector
        // and parent-entry swaps, so the result is one linearizable cut. Do not
        // also lock the tab registry here: Tab methods can resolve their window
        // while holding Tab::inner, whereas retirement takes tabs before inner.
        let parents = self.tab_parents.read();
        let parent = parents.get(&tab_id)?;
        parent.tab.upgrade().map(|_| parent.window_id)
    }

    #[cfg(test)]
    fn assert_tab_parent_index_matches_windows(&self) {
        let windows = self.windows.read();
        self.validate_tab_parent_index_matches_windows_locked(&windows)
            .expect("tab-parent index must match exact window membership");
    }

    #[cfg(test)]
    fn validate_tab_parent_index_matches_windows_locked(
        &self,
        windows: &HashMap<WindowId, Window>,
    ) -> anyhow::Result<()> {
        let parents = self.tab_parents.read();
        let mut observed = HashMap::new();
        observed
            .try_reserve(parents.len())
            .map_err(|error| anyhow!("reserve tab-parent validation: {error}"))?;
        for (window_id, window) in windows.iter() {
            for tab in window.iter() {
                let prior = observed.insert(tab.tab_id(), (*window_id, Arc::clone(tab)));
                anyhow::ensure!(
                    prior.is_none(),
                    "tab {} has multiple window parents",
                    tab.tab_id()
                );
            }
        }
        anyhow::ensure!(
            parents.len() == observed.len(),
            "tab-parent index cardinality {} differs from window membership {}",
            parents.len(),
            observed.len(),
        );
        for (tab_id, (window_id, tab)) in observed {
            anyhow::ensure!(
                parents
                    .get(&tab_id)
                    .is_some_and(|parent| parent.matches(&tab, window_id)),
                "tab {tab_id} lacks its exact indexed parent {window_id}"
            );
        }
        Ok(())
    }

    /// Move a tab from whichever window currently contains it into
    /// `dst_window` at position `idx` (appended when `idx` is `None`).
    ///
    /// This is a pure *metadata* move: the live `Arc<Tab>` (and all of its
    /// panes) is preserved in the mux tab registry and merely re-parented
    /// between the windows' ordered tab lists. No pane is killed and no
    /// `Pdu::KillPane` is sent -- this is the mechanism the window-unify
    /// feature uses to relocate non-duplicate tabs onto the canonical window.
    /// Existing destination active identity is preserved; an empty destination
    /// activates the moved tab. Same-window reorders preserve active identity
    /// and tab-stack membership without transient focus loss. An explicit
    /// `idx` outside the final-index range fails before either window mutates.
    ///
    /// Window-lifecycle decisions (closing a now-empty source window) are left
    /// to the caller; this primitive does not prune. Workspace policy
    /// (same-workspace-only) is likewise enforced by the planner, not here.
    pub fn move_tab_between_windows(
        &self,
        tab_id: TabId,
        dst_window: WindowId,
        idx: Option<usize>,
    ) -> anyhow::Result<()> {
        let tabs = self.tabs.read();
        let tab = tabs
            .get(&tab_id)
            .map(Arc::clone)
            .ok_or_else(|| anyhow!("move_tab_between_windows: tab {tab_id} not found in mux"))?;
        let (changed, src_window) = {
            let mut windows = self.windows.write();
            let src_window = self
                .tab_parents
                .read()
                .get(&tab_id)
                .filter(|parent| parent.is_same_tab(&tab))
                .map(|parent| parent.window_id);
            anyhow::ensure!(
                src_window.is_some(),
                "move_tab_between_windows: tab {tab_id} has no exact indexed window parent"
            );
            let src_window = src_window
                .expect("one exact indexed parent was validated while holding the window map");
            let source = windows.get(&src_window).ok_or_else(|| {
                anyhow!("move_tab_between_windows: source window {src_window} not found")
            })?;
            let source_index = source
                .iter()
                .position(|candidate| Arc::ptr_eq(candidate, &tab))
                .ok_or_else(|| {
                    anyhow!(
                        "move_tab_between_windows: exact tab {tab_id} left source window \
                         {src_window}"
                    )
                })?;
            let source_len = source.len();
            let destination = windows.get(&dst_window).ok_or_else(|| {
                anyhow!("move_tab_between_windows: destination window {dst_window} not found")
            })?;
            let destination_len = destination.len();
            let pos = match (src_window == dst_window, idx) {
                (true, Some(pos)) => {
                    anyhow::ensure!(
                        pos < source_len,
                        "move_tab_between_windows: destination index {pos} is out of range for \
                         window {dst_window} with {source_len} tabs"
                    );
                    pos
                }
                (true, None) => source_len - 1,
                (false, Some(pos)) => {
                    anyhow::ensure!(
                        pos <= destination_len,
                        "move_tab_between_windows: destination index {pos} is out of range for \
                         window {dst_window} with {destination_len} tabs"
                    );
                    pos
                }
                (false, None) => destination_len,
            };
            if src_window != dst_window {
                anyhow::ensure!(
                    !destination
                        .iter()
                        .any(|candidate| Arc::ptr_eq(candidate, &tab)),
                    "move_tab_between_windows: destination window {dst_window} already contains \
                     exact tab {tab_id}"
                );
                anyhow::ensure!(
                    destination.idx_by_id(tab_id).is_none(),
                    "move_tab_between_windows: destination window {dst_window} already contains \
                     tab id {tab_id}"
                );
                // `remove_tab_if_same` and `insert` are individually
                // fail-before-mutation, but a cross-window transfer must
                // preflight both revisions and destination capacity before
                // detaching the source. The write guard keeps those checks
                // stable through the two infallible commit steps.
                source.next_order_revision().with_context(|| {
                    format!(
                        "move_tab_between_windows: source window {src_window} order revision is exhausted"
                    )
                })?;
                destination.ensure_tab_insert_available().with_context(|| {
                    format!(
                        "move_tab_between_windows: destination window {dst_window} cannot accept tab {tab_id}"
                    )
                })?;
            }

            let changed = if src_window == dst_window && source_index == pos {
                false
            } else {
                let mut prepared = Vec::new();
                prepared
                    .try_reserve_exact(if src_window == dst_window { 1 } else { 2 })
                    .map_err(|error| anyhow!("reserve tab move transaction: {error}"))?;
                let mut attached_tabs = Vec::new();
                if src_window == dst_window {
                    let state = windows
                        .get(&src_window)
                        .expect("source window presence checked above")
                        .prepare_reorder_exact(&tab, source_index, pos)?
                        .ok_or_else(|| {
                            anyhow!(
                                "move_tab_between_windows: exact tab {tab_id} left source window \
                                 {src_window}"
                            )
                        })?;
                    prepared.push((src_window, state));
                } else {
                    let source_state = windows
                        .get(&src_window)
                        .expect("source window presence checked above")
                        .prepare_remove_exact(&tab)?
                        .ok_or_else(|| {
                            anyhow!(
                                "move_tab_between_windows: exact tab {tab_id} left source window \
                                 {src_window}"
                            )
                        })?;
                    let destination_state = windows
                        .get(&dst_window)
                        .expect("destination window presence checked above")
                        .prepare_insert(pos, &tab)?;
                    prepared.push((src_window, source_state));
                    prepared.push((dst_window, destination_state));
                    attached_tabs.try_reserve_exact(1).map_err(|error| {
                        anyhow!("reserve cross-window attachment payload: {error}")
                    })?;
                    attached_tabs.push((tab_id, dst_window));
                }
                let mut created_windows = Vec::new();
                if src_window != dst_window && self.provisional_windows.lock().contains(&dst_window)
                {
                    created_windows.try_reserve_exact(1).map_err(|error| {
                        anyhow!("reserve destination-window creation payload: {error}")
                    })?;
                    created_windows.push(dst_window);
                }
                self.commit_prepared_window_states_locked(
                    &mut windows,
                    prepared,
                    attached_tabs,
                    created_windows,
                    Vec::new(),
                )?;
                true
            };
            (changed, src_window)
        };
        drop(tabs);

        // Pane count is unchanged for a within-workspace move; recompute keeps
        // the per-workspace tallies correct if a caller ever moves across
        // workspaces.
        if changed && src_window != dst_window {
            self.recompute_pane_count();
        }
        self.flush_window_notifications();
        Ok(())
    }

    pub fn is_empty(&self) -> bool {
        self.panes.read().is_empty()
    }

    /// Publish `Empty` only if the pane registry is still empty, reserving the
    /// topology revision before a new pane can acquire the registry write
    /// lock. Dispatch remains outside the registry lock so subscribers may
    /// safely re-enter mux state.
    fn notify_empty_if_current(&self) -> bool {
        let notification = {
            let panes = self.panes.read();
            if !panes.is_empty() {
                return false;
            }
            self.envelope_notification(MuxNotification::Empty)
        };
        self.dispatch_notification_envelope(notification);
        true
    }

    pub fn is_workspace_empty(&self, workspace: &str) -> bool {
        *self
            .num_panes_by_workspace
            .read()
            .get(workspace)
            .unwrap_or(&0)
            == 0
    }

    pub fn is_active_workspace_empty(&self) -> bool {
        let workspace = self.active_workspace();
        self.is_workspace_empty(&workspace)
    }

    pub fn iter_panes(&self) -> Vec<Arc<dyn Pane>> {
        self.panes
            .read()
            .iter()
            .map(|(_, registered)| Arc::clone(&registered.pane))
            .collect()
    }

    pub fn iter_windows_in_workspace(&self, workspace: &str) -> Vec<WindowId> {
        let mut windows: Vec<WindowId> = self
            .windows
            .read()
            .iter()
            .filter_map(|(k, w)| {
                if w.get_workspace() == workspace {
                    Some(k)
                } else {
                    None
                }
            })
            .cloned()
            .collect();
        windows.sort();
        windows
    }

    pub fn iter_windows(&self) -> Vec<WindowId> {
        self.windows.read().keys().cloned().collect()
    }

    pub fn iter_domains(&self) -> Vec<Arc<dyn Domain>> {
        self.domains.read().values().cloned().collect()
    }

    pub fn resolve_pane_id(&self, pane_id: PaneId) -> Option<(DomainId, WindowId, TabId)> {
        const COHERENCE_ATTEMPTS: usize = 3;

        for _ in 0..COHERENCE_ATTEMPTS {
            let revision = self.topology.lock().current_revision();
            if let Some(location) = self.cached_pane_location(pane_id, revision) {
                if self.topology.lock().current_revision() == revision {
                    #[cfg(test)]
                    self.pane_location_cache_hits
                        .fetch_add(1, Ordering::Relaxed);
                    return Some(location);
                }
                continue;
            }
            if self.topology.lock().current_revision() != revision {
                continue;
            }

            #[cfg(test)]
            self.pane_location_full_scans
                .fetch_add(1, Ordering::Relaxed);

            // Resolve the numeric slot once, then compare only exact Arc
            // identities while walking callback-free tab snapshots.  This is
            // both safer and cheaper than invoking Pane::pane_id/domain_id for
            // every pane in the session, and it rejects duplicate structural
            // ownership instead of returning HashMap iteration's first match.
            // The registration serializer closes the small publication gap in
            // which a new pane/tab map entry exists but its lifecycle topology
            // revision has not yet been reserved.
            let _registration = self.pane_registration.lock();
            let (pane, domain_id, exact_registration) = {
                let panes = self.panes.read();
                let registered = panes.get(&pane_id)?;
                (
                    Arc::clone(&registered.pane),
                    registered.domain_id,
                    PaneRegistrationHandle::new(&registered.pane, &registered.generation),
                )
            };
            let tabs = {
                let registered = self.tabs.read();
                let mut snapshot = Vec::new();
                snapshot.try_reserve_exact(registered.len()).ok()?;
                snapshot.extend(registered.values().cloned());
                snapshot
            };
            let mut owner = None;
            for tab in tabs {
                #[cfg(test)]
                self.pane_location_scan_tab_probes
                    .fetch_add(1, Ordering::Relaxed);
                if tab.contains_exact_pane_callback_free(&pane) {
                    if owner.is_some() {
                        drop(_registration);
                        self.pane_location_cache.write().remove(&pane_id);
                        log::error!(
                            "refusing ambiguous pane {pane_id} lookup: its exact allocation has multiple structural tab owners"
                        );
                        return None;
                    }
                    owner = Some(tab);
                }
            }
            let tab = owner?;
            let tab_id = tab.tab_id();
            let window_id = {
                let parents = self.tab_parents.read();
                let parent = parents.get(&tab_id)?;
                if !parent.matches(&tab, parent.window_id) {
                    return None;
                }
                parent.window_id
            };

            // The topology authority is the cache's generation fence.  If a
            // pane, tab, or window moved during the cold scan, retry from a new
            // cut rather than publishing a stale location.
            if self.topology.lock().current_revision() != revision {
                continue;
            }
            let still_registered = self.panes.read().get(&pane_id).is_some_and(|registered| {
                Arc::ptr_eq(&registered.pane, &pane) && registered.domain_id == domain_id
            });
            let tab_still_registered = self
                .tabs
                .read()
                .get(&tab_id)
                .is_some_and(|registered| Arc::ptr_eq(registered, &tab));
            let parent_still_registered = self
                .tab_parents
                .read()
                .get(&tab_id)
                .is_some_and(|parent| parent.matches(&tab, window_id));
            if !still_registered
                || !tab_still_registered
                || !parent_still_registered
                || self.topology.lock().current_revision() != revision
            {
                continue;
            }

            let entry = PaneLocationCacheEntry {
                registration: exact_registration,
                pane: Arc::downgrade(&pane),
                tab: Arc::downgrade(&tab),
                domain_id,
                window_id,
                topology_revision: revision,
            };
            let mut cache = self.pane_location_cache.write();
            if cache.try_reserve(1).is_ok() {
                cache.insert(pane_id, entry);
            }
            return Some((domain_id, window_id, tab_id));
        }

        None
    }

    fn cached_pane_location(
        &self,
        pane_id: PaneId,
        topology_revision: TopologyRevision,
    ) -> Option<(DomainId, WindowId, TabId)> {
        let entry = self.pane_location_cache.read().get(&pane_id).cloned()?;
        if entry.topology_revision != topology_revision {
            return None;
        }
        let _operation = entry.registration.cached_location_lease_for_owner(self)?;
        let pane = entry.pane.upgrade()?;
        let tab = entry.tab.upgrade()?;
        let tab_id = tab.tab_id();
        if entry.registration.pane_id() != pane_id {
            return None;
        }
        if !self
            .tabs
            .read()
            .get(&tab_id)
            .is_some_and(|registered| Arc::ptr_eq(registered, &tab))
        {
            return None;
        }
        if !self
            .tab_parents
            .read()
            .get(&tab_id)
            .is_some_and(|parent| parent.matches(&tab, entry.window_id))
        {
            return None;
        }
        if !tab.contains_exact_pane_callback_free(&pane) {
            return None;
        }
        if !self.panes.read().get(&pane_id).is_some_and(|registered| {
            entry.registration.matches_live_registration(registered)
                && registered.domain_id == entry.domain_id
        }) {
            return None;
        }
        Some((entry.domain_id, entry.window_id, tab_id))
    }

    pub fn domain_was_detached(&self, domain: DomainId) {
        let Some(expected) = self.get_domain(domain) else {
            return;
        };
        let _ = self.domain_was_detached_if_same(&expected);
    }

    /// Remove one exact domain instance and all of its topology.
    ///
    /// Domain identifiers are retired before topology callbacks run, but the
    /// exact detached domain remains discoverable until its panes have been
    /// killed. `ClientPane::kill` relies on that detached registration to
    /// suppress an unintended remote `KillPane` during transport teardown.
    pub fn domain_was_detached_if_same(&self, expected: &Arc<dyn Domain>) -> bool {
        let domain = expected.domain_id();
        let removed = {
            let _registration = self.domain_registration.lock();
            let Some(registered) = self.domains.read().get(&domain).cloned() else {
                return false;
            };
            if !Arc::ptr_eq(&registered, expected) {
                return false;
            }
            if !self.retired_domain_ids.lock().insert(domain) {
                // Another exact teardown already owns this retired
                // registration. Do not duplicate pane kills or callbacks.
                return false;
            }
            registered
        };

        // Tmux domains install mux notification subscriptions that should be
        // removed eagerly when the domain is detached. Waiting for the next
        // notification to lazily retain-drop stale callbacks can leak
        // subscribers in long-idle sessions.
        if let Some(tmux_domain) = removed.downcast_ref::<TmuxDomain>() {
            let sub_id = tmux_domain.inner.notification_sub_id.lock().take();
            if let Some(sub_id) = sub_id {
                let _ = self.unsubscribe(sub_id);
            }
        }

        let domain_panes = self
            .iter_panes()
            .into_iter()
            .filter(|pane| pane.domain_id() == domain)
            .collect::<Vec<_>>();
        let dead_panes = domain_panes
            .iter()
            .map(|pane| pane.pane_id())
            .collect::<HashSet<_>>();
        let mut dead_pane_registrations = self.capture_pane_registrations(&domain_panes);

        // Snapshot exact tab Arcs before structural work. Pane callbacks,
        // resize/focus effects, and registration retirement must never run
        // while the mux window registry is locked.
        let mut tabs = self.tabs.read().values().cloned().collect::<Vec<_>>();
        let window_tabs = self
            .windows
            .read()
            .values()
            .flat_map(|window| window.iter().cloned())
            .collect::<Vec<_>>();
        for tab in window_tabs {
            if !tabs.iter().any(|candidate| Arc::ptr_eq(candidate, &tab)) {
                tabs.push(tab);
            }
        }
        for tab in tabs {
            let (_, registrations) = tab.remove_exact_panes_deferred(self, &domain_panes);
            for registration in registrations {
                if !dead_pane_registrations
                    .iter()
                    .any(|current| current.same_registration(&registration))
                {
                    dead_pane_registrations.push(registration);
                }
            }
        }

        log::info!("domain detached panes: {:?}", dead_panes);
        PaneRegistrationHandle::retire_batch_if_current(dead_pane_registrations);

        self.prune_dead_windows();

        {
            let _registration = self.domain_registration.lock();
            let mut domains = self.domains.write();
            if !domains
                .get(&domain)
                .is_some_and(|current| Arc::ptr_eq(current, &removed))
            {
                log::error!(
                    "retired domain {domain} changed identity during exact teardown; preserving \
                     the unexpected registration"
                );
                return false;
            }
            domains.remove(&domain);
            drop(domains);

            self.domains_by_name
                .write()
                .retain(|_, current| !Arc::ptr_eq(current, &removed));

            let should_replace_default = self
                .default_domain
                .read()
                .as_ref()
                .is_some_and(|current| Arc::ptr_eq(current, &removed));
            if should_replace_default {
                let replacement = self.domains.read().values().next().cloned();
                *self.default_domain.write() = replacement;
            }
        }
        true
    }

    pub fn set_banner(&self, banner: Option<String>) {
        *self.banner.write() = banner;
    }

    pub fn resolve_spawn_tab_domain(
        &self,
        source_pane_id: Option<PaneId>,
        domain: &config::keyassignment::SpawnTabDomain,
    ) -> anyhow::Result<Arc<dyn Domain>> {
        let domain = match domain {
            SpawnTabDomain::DefaultDomain => self.resolve_default_domain()?,
            SpawnTabDomain::CurrentPaneDomain => match source_pane_id {
                Some(pane_id) => {
                    let (pane_domain_id, _window_id, _tab_id) = self
                        .resolve_pane_id(pane_id)
                        .ok_or_else(|| anyhow!("pane_id {} invalid", pane_id))?;
                    self.get_domain(pane_domain_id).ok_or_else(|| {
                        anyhow!("pane_id {pane_id} resolved to missing domain id {pane_domain_id}")
                    })?
                }
                None => self.resolve_default_domain()?,
            },
            SpawnTabDomain::DomainId(domain_id) => self
                .get_domain(*domain_id)
                .ok_or_else(|| anyhow!("domain id {} is invalid", domain_id))?,
            SpawnTabDomain::DomainName(name) => {
                self.get_domain_by_name(&name).ok_or_else(|| {
                    let names: Vec<String> = self
                        .domains_by_name
                        .read()
                        .keys()
                        .map(|name| format!("\"{name}\""))
                        .collect();
                    anyhow!(
                        "domain name \"{name}\" is invalid. Possible names are {}.",
                        names.join(", ")
                    )
                })?
            }
        };
        Ok(domain)
    }

    fn resolve_spawn_tab_domain_for_operation(
        &self,
        target: &PaneOperationGuard,
        domain: &config::keyassignment::SpawnTabDomain,
    ) -> anyhow::Result<Arc<dyn Domain>> {
        if !std::ptr::eq(target.owner().as_ref(), self) {
            anyhow::bail!(
                "pane operation guard {} belongs to another mux",
                target.pane_id()
            );
        }
        match domain {
            SpawnTabDomain::CurrentPaneDomain => {
                let domain_id = target.with_pane(|pane| pane.domain_id());
                self.get_domain(domain_id).ok_or_else(|| {
                    anyhow!(
                        "exact pane registration {} resolved to missing domain id {domain_id}",
                        target.pane_id()
                    )
                })
            }
            _ => self.resolve_spawn_tab_domain(None, domain),
        }
    }

    fn resolve_cwd(
        &self,
        command_dir: Option<String>,
        pane: Option<Arc<dyn Pane>>,
        target_domain: DomainId,
        policy: CachePolicy,
    ) -> Option<String> {
        command_dir.or_else(|| {
            match pane {
                Some(pane) if pane.domain_id() == target_domain => pane
                    .get_current_working_dir(policy)
                    .and_then(|url| {
                        percent_decode_str(url.path())
                            .decode_utf8()
                            .ok()
                            .map(|path| path.into_owned())
                    })
                    .map(|path| {
                        // On Windows the file URI can produce a path like:
                        // `/C:\Users` which is valid in a file URI, but the leading slash
                        // is not liked by the windows file APIs, so we strip it off here.
                        let bytes = path.as_bytes();
                        if bytes.len() > 2 && bytes[0] == b'/' && bytes[2] == b':' {
                            path[1..].to_owned()
                        } else {
                            path
                        }
                    }),
                _ => None,
            }
        })
    }

    /// Capture the exact destination for a floating-pane spawn before any
    /// asynchronous domain work begins.
    pub fn capture_floating_spawn_target(
        self: &Arc<Self>,
        window_id: WindowId,
    ) -> anyhow::Result<FloatingSpawnTarget> {
        let (tab, pane) = {
            let window = self
                .get_window(window_id)
                .ok_or_else(|| anyhow!("window {window_id} is not registered"))?;
            let tab = window
                .get_active()
                .cloned()
                .ok_or_else(|| anyhow!("window {window_id} has no active tab"))?;
            let pane = tab.get_active_pane().ok_or_else(|| {
                anyhow!(
                    "active tab {} in window {window_id} has no active pane",
                    tab.tab_id()
                )
            })?;
            (tab, pane)
        };
        let registration = self.capture_pane_registration(&pane).ok_or_else(|| {
            anyhow!(
                "active pane in tab {} has no exact current registration",
                tab.tab_id()
            )
        })?;
        let target_operation = registration.operation_guard(self).ok_or_else(|| {
            anyhow!(
                "active pane registration in tab {} retired during floating-spawn admission",
                tab.tab_id()
            )
        })?;
        anyhow::ensure!(
            target_operation.is_same_pane(&pane),
            "floating-spawn admission resolved a different exact pane allocation"
        );
        let (_domain_id, exact_window_id, exact_tab) = target_operation.exact_location()?;
        anyhow::ensure!(
            exact_window_id == window_id && Arc::ptr_eq(&exact_tab, &tab),
            "floating-spawn target moved out of its admitted tab/window"
        );
        drop(target_operation);
        Ok(FloatingSpawnTarget::from_exact_parts(
            registration,
            tab,
            window_id,
        ))
    }

    /// Spawn one pane directly into a previously captured floating layer.
    ///
    /// Unsupported remote domains are rejected before attach or spawn. The
    /// destination is never re-resolved from current UI state after an await.
    pub async fn spawn_floating_pane(
        self: &Arc<Self>,
        target: FloatingSpawnTarget,
        rect: FloatingPaneRect,
        command: Option<CommandBuilder>,
        command_dir: Option<String>,
        domain: SpawnTabDomain,
        term_config: Arc<TermConfig>,
        owner_client_id: Option<Arc<ClientId>>,
    ) -> anyhow::Result<FloatingPaneCommitReceipt> {
        anyhow::ensure!(
            target.belongs_to(self),
            "floating-pane target belongs to another mux"
        );
        if owner_client_id
            .as_ref()
            .is_some_and(|client_id| !self.client_registration_is_current(client_id))
        {
            anyhow::bail!("client registration is no longer current");
        }
        let target_operation = target
            .target()
            .operation_guard(self)
            .ok_or_else(|| anyhow!("floating-pane target registration retired before spawn"))?;
        let target_pane = Arc::clone(target_operation.pane());
        let domain = self
            .resolve_spawn_tab_domain_for_operation(&target_operation, &domain)
            .context("resolve floating-pane spawn domain")?;
        anyhow::ensure!(
            domain.supports_floating_pane_spawn(),
            "domain `{}` has no authoritative floating-pane spawn transaction; refusing before spawn",
            domain.domain_name(),
        );
        let domain_id = domain.domain_id();
        let command_dir = self.resolve_cwd(
            command_dir,
            Some(target_pane),
            domain_id,
            CachePolicy::FetchImmediate,
        );
        let tab = target.tab_arc();
        let geometry = tab.prepare_floating_spawn_geometry(rect);
        // Do not pin the target generation while domain attach or process
        // spawn is outstanding. Final commit reacquires exact authority.
        drop(target_operation);

        if domain.state() == DomainState::Detached {
            domain
                .attach(self, owner_client_id.clone(), Some(target.window_id()))
                .await?;
        }
        if owner_client_id
            .as_ref()
            .is_some_and(|client_id| !self.client_registration_is_current(client_id))
        {
            anyhow::bail!("client registration retired while attaching floating-pane domain");
        }

        let unpublished = domain
            .spawn_unpublished_pane(self, geometry.size(), command, command_dir)
            .await
            .context("spawn unpublished floating pane")?;
        {
            let pane = unpublished.pane();
            let pane_domain_id = catch_recoverable(
                RecoverablePanicSite::MuxPaneCallback,
                std::panic::AssertUnwindSafe(|| pane.domain_id()),
            )
            .map_err(|_| anyhow!("floating-pane domain callback panicked"))?;
            anyhow::ensure!(
                pane_domain_id == domain_id,
                "resolved domain {domain_id} returned unpublished pane for domain id {pane_domain_id}",
            );
            catch_recoverable(
                RecoverablePanicSite::MuxPaneCallback,
                std::panic::AssertUnwindSafe(|| pane.set_config(term_config)),
            )
            .map_err(|_| anyhow!("floating-pane configuration callback panicked"))?;
            let resize_result = catch_recoverable(
                RecoverablePanicSite::MuxPaneCallback,
                std::panic::AssertUnwindSafe(|| pane.resize(geometry.size())),
            )
            .map_err(|_| anyhow!("floating-pane resize callback panicked"))?;
            resize_result.context("pre-size unpublished floating pane")?;
        }

        let (pane, positioned, registration) = tab.commit_unpublished_floating_pane(
            self,
            &domain,
            domain_id,
            target.window_id(),
            target.target(),
            unpublished,
            geometry,
            owner_client_id.as_ref(),
        )?;
        Ok(FloatingPaneCommitReceipt::from_exact_parts(
            pane,
            registration,
            tab,
            target.window_id(),
            positioned,
        ))
    }

    pub async fn split_pane(
        self: &Arc<Self>,
        source_pane_id: PaneId,
        request: SplitRequest,
        source: SplitSource,
        domain: config::keyassignment::SpawnTabDomain,
        owner_client_id: Option<Arc<ClientId>>,
    ) -> anyhow::Result<(Arc<dyn Pane>, TerminalSize, WindowId, TabId)> {
        let target = self
            .capture_pane_operation(source_pane_id)
            .ok_or_else(|| anyhow!("pane_id {source_pane_id} is not a current registration"))?;
        let receipt = match source {
            SplitSource::Spawn {
                command,
                command_dir,
            } => {
                self.split_pane_spawned(
                    target,
                    request,
                    command,
                    command_dir,
                    domain,
                    owner_client_id,
                )
                .await?
            }
            SplitSource::MovePane(move_pane_id) => {
                let source = self.capture_pane_operation(move_pane_id).ok_or_else(|| {
                    anyhow!("move pane_id {move_pane_id} is not a current registration")
                })?;
                self.split_pane_moved(target, source, request, domain, owner_client_id)
                    .await?
            }
        };
        Ok(receipt.into_legacy_parts())
    }

    pub async fn split_pane_spawned(
        self: &Arc<Self>,
        target: PaneOperationGuard,
        request: SplitRequest,
        command: Option<CommandBuilder>,
        command_dir: Option<String>,
        domain: config::keyassignment::SpawnTabDomain,
        owner_client_id: Option<Arc<ClientId>>,
    ) -> anyhow::Result<SplitCommitReceipt> {
        anyhow::ensure!(
            target.belongs_to(self),
            "split target registration {} belongs to another mux",
            target.pane_id()
        );
        if owner_client_id
            .as_ref()
            .is_some_and(|client_id| !self.client_registration_is_current(client_id))
        {
            anyhow::bail!("client registration is no longer current");
        }
        let (_pane_domain_id, window_id, _tab) = target.exact_location()?;
        let domain = self
            .resolve_spawn_tab_domain_for_operation(&target, &domain)
            .context("resolve_spawn_tab_domain")?;

        if domain.state() == DomainState::Detached {
            domain
                .attach(self, owner_client_id, Some(window_id))
                .await?;
        }

        let command_dir = self.resolve_cwd(
            command_dir,
            Some(Arc::clone(target.pane())),
            domain.domain_id(),
            CachePolicy::FetchImmediate,
        );
        domain
            .split_pane_spawned(self, &target, request, command, command_dir)
            .await
    }

    pub async fn split_pane_moved(
        self: &Arc<Self>,
        target: PaneOperationGuard,
        source: PaneOperationGuard,
        request: SplitRequest,
        domain: config::keyassignment::SpawnTabDomain,
        owner_client_id: Option<Arc<ClientId>>,
    ) -> anyhow::Result<SplitCommitReceipt> {
        anyhow::ensure!(
            target.belongs_to(self) && source.belongs_to(self),
            "split source and target must belong to the originating mux"
        );
        anyhow::ensure!(
            !target.same_registration(&source),
            "cannot move pane {} into a split of itself",
            target.pane_id()
        );
        if owner_client_id
            .as_ref()
            .is_some_and(|client_id| !self.client_registration_is_current(client_id))
        {
            anyhow::bail!("client registration is no longer current");
        }
        let (_pane_domain_id, window_id, _tab) = target.exact_location()?;
        let domain = self
            .resolve_spawn_tab_domain_for_operation(&target, &domain)
            .context("resolve_spawn_tab_domain")?;
        if domain.state() == DomainState::Detached {
            domain
                .attach(self, owner_client_id, Some(window_id))
                .await?;
        }
        domain
            .split_pane_moved(self, &target, &source, request)
            .await
    }

    pub async fn move_pane_to_new_tab(
        self: &Arc<Self>,
        pane_id: PaneId,
        window_id: Option<WindowId>,
        workspace_for_new_window: Option<String>,
    ) -> anyhow::Result<(Arc<Tab>, WindowId)> {
        let target = self
            .capture_pane_operation(pane_id)
            .ok_or_else(|| anyhow!("pane {pane_id} is not a current registration"))?;
        Ok(self
            .move_pane_to_new_tab_guarded(target, window_id, workspace_for_new_window, None)
            .await?
            .into_legacy_parts())
    }

    pub async fn move_pane_to_new_tab_guarded(
        self: &Arc<Self>,
        target: PaneOperationGuard,
        window_id: Option<WindowId>,
        workspace_for_new_window: Option<String>,
        owner_client_id: Option<Arc<ClientId>>,
    ) -> anyhow::Result<MoveCommitReceipt> {
        anyhow::ensure!(
            target.belongs_to(self),
            "move target registration {} belongs to another mux",
            target.pane_id()
        );
        if owner_client_id
            .as_ref()
            .is_some_and(|client_id| !self.client_registration_is_current(client_id))
        {
            anyhow::bail!("client registration is no longer current");
        }
        let (domain_id, _src_window, src_tab) = target.exact_location()?;
        let domain = self.get_domain(domain_id).ok_or_else(|| {
            anyhow!(
                "domain {domain_id} of exact pane registration {} not found",
                target.pane_id()
            )
        })?;

        if let Some(receipt) = domain
            .move_pane_to_new_tab(self, &target, window_id, workspace_for_new_window.clone())
            .await?
        {
            return Ok(receipt);
        }

        let window_builder;
        let (window_id, size) = if let Some(window_id) = window_id {
            let window = self
                .get_window_mut(window_id)
                .ok_or_else(|| anyhow!("window_id {} not found on this server", window_id))?;
            let tab = window
                .get_active()
                .ok_or_else(|| anyhow!("window {} has no tabs", window_id))?;
            let size = tab.get_size();

            (window_id, size)
        } else {
            window_builder = self.new_empty_window(workspace_for_new_window, None);
            (*window_builder, src_tab.get_size())
        };

        let pane = src_tab
            .remove_exact_pane_for_move(self, target.pane())
            .ok_or_else(|| {
                anyhow!(
                    "exact pane registration {} was not in its containing tab",
                    target.pane_id()
                )
            })?;

        let tab = Arc::new(Tab::new(&size));
        tab.assign_pane(&pane);
        pane.resize(size)?;
        // The pane already has exact operation authority. Its numeric registry
        // slot may have been retired after admission, so committing the new
        // tab must not attempt to re-register or re-authorize it.
        self.add_tab_no_panes(&tab)?;
        self.add_tab_to_window(&tab, window_id)?;

        if src_tab.is_dead() {
            self.remove_tab(src_tab.tab_id());
        }

        Ok(MoveCommitReceipt::from_exact_parts(
            pane,
            target.registration(),
            tab,
            window_id,
            size,
        ))
    }

    pub async fn spawn_tab_or_window(
        self: &Arc<Self>,
        window_id: Option<WindowId>,
        domain: SpawnTabDomain,
        command: Option<CommandBuilder>,
        command_dir: Option<String>,
        size: TerminalSize,
        current_pane_id: Option<PaneId>,
        workspace_for_new_window: String,
        window_position: Option<GuiPosition>,
        owner_client_id: Option<Arc<ClientId>>,
    ) -> anyhow::Result<(Arc<Tab>, Arc<dyn Pane>, WindowId)> {
        if owner_client_id
            .as_ref()
            .is_some_and(|client_id| !self.client_registration_is_current(client_id))
        {
            anyhow::bail!("client registration is no longer current");
        }
        let domain = self
            .resolve_spawn_tab_domain(current_pane_id, &domain)
            .context("resolve_spawn_tab_domain")?;

        let window_builder;
        let term_config;

        let (window_id, size) = if let Some(window_id) = window_id {
            let window = self
                .get_window_mut(window_id)
                .ok_or_else(|| anyhow!("window_id {} not found on this server", window_id))?;
            let tab = window
                .get_active()
                .ok_or_else(|| anyhow!("window {} has no tabs", window_id))?;
            let pane = tab
                .get_active_pane()
                .ok_or_else(|| anyhow!("active tab in window {} has no panes", window_id))?;
            term_config = pane.get_config();

            let size = tab.get_size();

            (window_id, size)
        } else {
            term_config = None;
            window_builder = self.new_empty_window(Some(workspace_for_new_window), window_position);
            (*window_builder, size)
        };

        if domain.state() == DomainState::Detached {
            domain
                .attach(self, owner_client_id, Some(window_id))
                .await?;
        }

        let cwd = self.resolve_cwd(
            command_dir,
            match current_pane_id {
                Some(id) => {
                    // Only use the cwd from the current pane if the domain
                    // is the same as the one we are spawning into
                    let (current_domain_id, _, _) = self
                        .resolve_pane_id(id)
                        .ok_or_else(|| anyhow!("pane_id {} invalid", id))?;
                    if current_domain_id == domain.domain_id() {
                        self.get_pane(id)
                    } else {
                        None
                    }
                }
                None => None,
            },
            domain.domain_id(),
            CachePolicy::FetchImmediate,
        );

        let tab = domain
            .spawn(self, size, command.clone(), cwd.clone(), window_id)
            .await
            .with_context(|| {
                format!(
                    "Spawning in domain `{}`: {size:?} command={command:?} cwd={cwd:?}",
                    domain.domain_name()
                )
            })?;

        let pane = tab
            .get_active_pane()
            .ok_or_else(|| anyhow!("missing active pane on tab!?"))?;

        if let Some(config) = term_config {
            pane.set_config(config);
        }

        let tab_index = self
            .get_window(window_id)
            .ok_or_else(|| anyhow!("no such window!?"))?
            .idx_by_id(tab.tab_id());
        if let Some(tab_index) = tab_index {
            self.activate_tab_at_index(window_id, tab_index, true)?;
        }

        Ok((tab, pane, window_id))
    }
}

pub struct IdentityHolder {
    owner: Weak<Mux>,
    prior: Option<Arc<ClientId>>,
}

impl Drop for IdentityHolder {
    fn drop(&mut self) {
        if let Some(mux) = self.owner.upgrade() {
            mux.replace_identity(self.prior.take());
        }
    }
}

#[derive(Debug, Error)]
#[allow(dead_code)]
pub enum SessionTerminated {
    #[error("Process exited: {:?}", status)]
    ProcessStatus { status: ExitStatus },
    #[error("Error: {:?}", err)]
    Error { err: Error },
    #[error("Window Closed")]
    WindowClosed,
}

pub(crate) fn terminal_size_to_pty_size(size: TerminalSize) -> anyhow::Result<PtySize> {
    Ok(PtySize {
        rows: size.rows.try_into()?,
        cols: size.cols.try_into()?,
        pixel_height: size.pixel_height.try_into()?,
        pixel_width: size.pixel_width.try_into()?,
    })
}

struct MuxClipboard {
    target: PaneRegistrationHandle,
}

impl Clipboard for MuxClipboard {
    fn set_contents(
        &self,
        selection: ClipboardSelection,
        clipboard: Option<String>,
    ) -> anyhow::Result<()> {
        self.target
            .try_with_current(|current| {
                current.assign_clipboard(selection, clipboard);
            })
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "MuxClipboard::set_contents: pane registration is no longer current"
                )
            })
    }
}

struct MuxDownloader {
    target: PaneRegistrationHandle,
}

impl frankenterm_term::DownloadHandler for MuxDownloader {
    fn save_to_downloads(&self, name: Option<String>, data: Vec<u8>) {
        let _ = self.target.try_with_current(|current| {
            current.save_to_downloads(name, data);
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pane::{ForEachPaneLogicalLine, LogicalLine, WithPaneLines};
    use crate::renderable::{RenderableDimensions, StableCursorPosition};
    use crate::tab::{DomainFloatingPaneReconcileReceipt, DomainFloatingPaneState};
    use frankenterm_term::color::ColorPalette;
    use frankenterm_term::{KeyCode, KeyModifiers, MouseEvent, StableRowIndex, TerminalSize};
    use parking_lot::{MappedMutexGuard, MutexGuard};
    use proptest::prelude::*;
    use rangeset::RangeSet;
    use std::ops::Range;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Mutex as StdMutex, MutexGuard as StdMutexGuard};
    use termwiz::surface::{Line, SequenceNo};

    fn global_test_lock() -> StdMutexGuard<'static, ()> {
        crate::MUX_TEST_LOCK
            .lock()
            .unwrap_or_else(|err| err.into_inner())
    }

    struct ScopedMuxOverride {
        prior: Option<Arc<Mux>>,
    }

    impl ScopedMuxOverride {
        fn install(mux: &Arc<Mux>) -> Self {
            let prior = Mux::try_get();
            Mux::set_mux(mux);
            Self { prior }
        }
    }

    impl Drop for ScopedMuxOverride {
        fn drop(&mut self) {
            if let Some(prior) = self.prior.take() {
                Mux::set_mux(&prior);
            } else {
                Mux::shutdown();
            }
        }
    }

    struct BoundedTestExecutor {
        receiver: std::sync::mpsc::Receiver<promise::spawn::Runnable>,
    }

    impl BoundedTestExecutor {
        fn new() -> Self {
            let (sender, receiver) = std::sync::mpsc::channel();
            let low_priority_sender = sender.clone();
            promise::spawn::set_schedulers(
                Box::new(move |runnable| {
                    let _ = sender.send(runnable);
                }),
                Box::new(move |runnable| {
                    let _ = low_priority_sender.send(runnable);
                }),
            );
            Self { receiver }
        }

        fn run_until(&self, timeout: Duration, mut completed: impl FnMut() -> bool) {
            let started = Instant::now();
            while !completed() {
                // The promise scheduler is process-global. A worker from an
                // earlier test may outlive that test and enqueue a harmless
                // stale runnable after this executor is installed, so "tick
                // exactly once" does not identify this test's own runnable.
                let remaining = timeout.saturating_sub(started.elapsed());
                assert!(
                    !remaining.is_zero(),
                    "the bounded test scheduler did not reach its completion condition",
                );
                self.receiver
                    .recv_timeout(remaining)
                    .expect("the bounded test scheduler should reach its completion condition")
                    .run();
            }
        }
    }

    struct BlockingDropLatch {
        entered: std::sync::mpsc::SyncSender<()>,
        release: StdMutex<std::sync::mpsc::Receiver<()>>,
    }

    impl Drop for BlockingDropLatch {
        fn drop(&mut self) {
            let _ = self.entered.send(());
            let _ = self
                .release
                .lock()
                .unwrap_or_else(|err| err.into_inner())
                .recv_timeout(Duration::from_secs(30));
        }
    }

    struct KillCountingPane {
        id: PaneId,
        domain_id: DomainId,
        size: Mutex<TerminalSize>,
        kills: Arc<AtomicUsize>,
        actions: Arc<AtomicUsize>,
        binds: AtomicUsize,
        writes: Mutex<Vec<u8>>,
        reader: Mutex<Option<Box<dyn std::io::Read + Send>>>,
        on_reader: Mutex<Option<Box<dyn FnOnce() + Send>>>,
        on_actions: Mutex<Option<Box<dyn FnOnce() + Send>>>,
        on_kill: Mutex<Option<Box<dyn FnOnce() + Send>>>,
        on_domain_id: Mutex<Option<Box<dyn FnOnce() + Send>>>,
        focus_events: Arc<Mutex<Vec<bool>>>,
        pane_id_calls: Option<Arc<AtomicUsize>>,
        mux_registration: Arc<PaneRegistrationSlot>,
        fail_reader: bool,
        search_pending: bool,
    }

    impl KillCountingPane {
        fn new(id: PaneId, size: TerminalSize) -> (Arc<dyn Pane>, Arc<AtomicUsize>) {
            Self::new_with_reader(id, size, None, false)
        }

        fn new_with_reader(
            id: PaneId,
            size: TerminalSize,
            reader: Option<Box<dyn std::io::Read + Send>>,
            fail_reader: bool,
        ) -> (Arc<dyn Pane>, Arc<AtomicUsize>) {
            let kills = Arc::new(AtomicUsize::new(0));
            let pane: Arc<dyn Pane> = Arc::new(Self {
                id,
                domain_id: 1,
                size: Mutex::new(size),
                kills: Arc::clone(&kills),
                actions: Arc::new(AtomicUsize::new(0)),
                binds: AtomicUsize::new(0),
                writes: Mutex::new(Vec::new()),
                reader: Mutex::new(reader),
                on_reader: Mutex::new(None),
                on_actions: Mutex::new(None),
                on_kill: Mutex::new(None),
                on_domain_id: Mutex::new(None),
                focus_events: Arc::new(Mutex::new(Vec::new())),
                pane_id_calls: None,
                mux_registration: Arc::new(PaneRegistrationSlot::default()),
                fail_reader,
                search_pending: false,
            });
            (pane, kills)
        }

        fn new_with_domain(
            id: PaneId,
            size: TerminalSize,
            domain_id: DomainId,
        ) -> (Arc<dyn Pane>, Arc<AtomicUsize>) {
            let kills = Arc::new(AtomicUsize::new(0));
            let pane: Arc<dyn Pane> = Arc::new(Self {
                id,
                domain_id,
                size: Mutex::new(size),
                kills: Arc::clone(&kills),
                actions: Arc::new(AtomicUsize::new(0)),
                binds: AtomicUsize::new(0),
                writes: Mutex::new(Vec::new()),
                reader: Mutex::new(None),
                on_reader: Mutex::new(None),
                on_actions: Mutex::new(None),
                on_kill: Mutex::new(None),
                on_domain_id: Mutex::new(None),
                focus_events: Arc::new(Mutex::new(Vec::new())),
                pane_id_calls: None,
                mux_registration: Arc::new(PaneRegistrationSlot::default()),
                fail_reader: false,
                search_pending: false,
            });
            (pane, kills)
        }

        fn new_with_pending_search(
            id: PaneId,
            size: TerminalSize,
        ) -> (Arc<dyn Pane>, Arc<AtomicUsize>) {
            let kills = Arc::new(AtomicUsize::new(0));
            let pane: Arc<dyn Pane> = Arc::new(Self {
                id,
                domain_id: 1,
                size: Mutex::new(size),
                kills: Arc::clone(&kills),
                actions: Arc::new(AtomicUsize::new(0)),
                binds: AtomicUsize::new(0),
                writes: Mutex::new(Vec::new()),
                reader: Mutex::new(None),
                on_reader: Mutex::new(None),
                on_actions: Mutex::new(None),
                on_kill: Mutex::new(None),
                on_domain_id: Mutex::new(None),
                focus_events: Arc::new(Mutex::new(Vec::new())),
                pane_id_calls: None,
                mux_registration: Arc::new(PaneRegistrationSlot::default()),
                fail_reader: false,
                search_pending: true,
            });
            (pane, kills)
        }

        fn new_with_reader_callback(
            id: PaneId,
            size: TerminalSize,
            on_reader: impl FnOnce() + Send + 'static,
        ) -> (Arc<dyn Pane>, Arc<AtomicUsize>) {
            let kills = Arc::new(AtomicUsize::new(0));
            let pane: Arc<dyn Pane> = Arc::new(Self {
                id,
                domain_id: 1,
                size: Mutex::new(size),
                kills: Arc::clone(&kills),
                actions: Arc::new(AtomicUsize::new(0)),
                binds: AtomicUsize::new(0),
                writes: Mutex::new(Vec::new()),
                reader: Mutex::new(None),
                on_reader: Mutex::new(Some(Box::new(on_reader))),
                on_actions: Mutex::new(None),
                on_kill: Mutex::new(None),
                on_domain_id: Mutex::new(None),
                focus_events: Arc::new(Mutex::new(Vec::new())),
                pane_id_calls: None,
                mux_registration: Arc::new(PaneRegistrationSlot::default()),
                fail_reader: false,
                search_pending: false,
            });
            (pane, kills)
        }

        fn new_with_kill_callback(
            id: PaneId,
            size: TerminalSize,
            on_kill: impl FnOnce() + Send + 'static,
        ) -> (Arc<dyn Pane>, Arc<AtomicUsize>) {
            let kills = Arc::new(AtomicUsize::new(0));
            let pane: Arc<dyn Pane> = Arc::new(Self {
                id,
                domain_id: 1,
                size: Mutex::new(size),
                kills: Arc::clone(&kills),
                actions: Arc::new(AtomicUsize::new(0)),
                binds: AtomicUsize::new(0),
                writes: Mutex::new(Vec::new()),
                reader: Mutex::new(None),
                on_reader: Mutex::new(None),
                on_actions: Mutex::new(None),
                on_kill: Mutex::new(Some(Box::new(on_kill))),
                on_domain_id: Mutex::new(None),
                focus_events: Arc::new(Mutex::new(Vec::new())),
                pane_id_calls: None,
                mux_registration: Arc::new(PaneRegistrationSlot::default()),
                fail_reader: false,
                search_pending: false,
            });
            (pane, kills)
        }

        fn new_with_actions_callback(
            id: PaneId,
            size: TerminalSize,
            on_actions: impl FnOnce() + Send + 'static,
        ) -> (Arc<dyn Pane>, Arc<AtomicUsize>) {
            let kills = Arc::new(AtomicUsize::new(0));
            let pane: Arc<dyn Pane> = Arc::new(Self {
                id,
                domain_id: 1,
                size: Mutex::new(size),
                kills: Arc::clone(&kills),
                actions: Arc::new(AtomicUsize::new(0)),
                binds: AtomicUsize::new(0),
                writes: Mutex::new(Vec::new()),
                reader: Mutex::new(None),
                on_reader: Mutex::new(None),
                on_actions: Mutex::new(Some(Box::new(on_actions))),
                on_kill: Mutex::new(None),
                on_domain_id: Mutex::new(None),
                focus_events: Arc::new(Mutex::new(Vec::new())),
                pane_id_calls: None,
                mux_registration: Arc::new(PaneRegistrationSlot::default()),
                fail_reader: false,
                search_pending: false,
            });
            (pane, kills)
        }

        fn new_with_focus_counter(
            id: PaneId,
            size: TerminalSize,
        ) -> (Arc<dyn Pane>, Arc<Mutex<Vec<bool>>>) {
            let focus_events = Arc::new(Mutex::new(Vec::new()));
            let pane: Arc<dyn Pane> = Arc::new(Self {
                id,
                domain_id: 1,
                size: Mutex::new(size),
                kills: Arc::new(AtomicUsize::new(0)),
                actions: Arc::new(AtomicUsize::new(0)),
                binds: AtomicUsize::new(0),
                writes: Mutex::new(Vec::new()),
                reader: Mutex::new(None),
                on_reader: Mutex::new(None),
                on_actions: Mutex::new(None),
                on_kill: Mutex::new(None),
                on_domain_id: Mutex::new(None),
                focus_events: Arc::clone(&focus_events),
                pane_id_calls: None,
                mux_registration: Arc::new(PaneRegistrationSlot::default()),
                fail_reader: false,
                search_pending: false,
            });
            (pane, focus_events)
        }

        fn new_with_pane_id_counter(
            id: PaneId,
            size: TerminalSize,
        ) -> (Arc<dyn Pane>, Arc<AtomicUsize>, Arc<AtomicUsize>) {
            let kills = Arc::new(AtomicUsize::new(0));
            let pane_id_calls = Arc::new(AtomicUsize::new(0));
            let pane: Arc<dyn Pane> = Arc::new(Self {
                id,
                domain_id: 1,
                size: Mutex::new(size),
                kills: Arc::clone(&kills),
                actions: Arc::new(AtomicUsize::new(0)),
                binds: AtomicUsize::new(0),
                writes: Mutex::new(Vec::new()),
                reader: Mutex::new(None),
                on_reader: Mutex::new(None),
                on_actions: Mutex::new(None),
                on_kill: Mutex::new(None),
                on_domain_id: Mutex::new(None),
                focus_events: Arc::new(Mutex::new(Vec::new())),
                pane_id_calls: Some(Arc::clone(&pane_id_calls)),
                mux_registration: Arc::new(PaneRegistrationSlot::default()),
                fail_reader: false,
                search_pending: false,
            });
            (pane, kills, pane_id_calls)
        }
    }

    #[async_trait::async_trait(?Send)]
    impl Pane for KillCountingPane {
        fn pane_id(&self) -> PaneId {
            if let Some(calls) = &self.pane_id_calls {
                calls.fetch_add(1, Ordering::SeqCst);
            }
            self.id
        }

        fn mux_registration_slot(&self) -> &Arc<PaneRegistrationSlot> {
            &self.mux_registration
        }

        fn mux_registration_did_bind(&self, _registration: PaneRegistrationHandle) {
            self.binds.fetch_add(1, Ordering::SeqCst);
        }

        fn get_cursor_position(&self) -> StableCursorPosition {
            StableCursorPosition::default()
        }

        fn get_current_seqno(&self) -> SequenceNo {
            0
        }

        fn get_changed_since(
            &self,
            _lines: Range<StableRowIndex>,
            _seqno: SequenceNo,
        ) -> RangeSet<StableRowIndex> {
            RangeSet::new()
        }

        fn get_lines(&self, _lines: Range<StableRowIndex>) -> (StableRowIndex, Vec<Line>) {
            (0, Vec::new())
        }

        fn with_lines_mut(
            &self,
            _lines: Range<StableRowIndex>,
            _with_lines: &mut dyn WithPaneLines,
        ) {
        }

        fn for_each_logical_line_in_stable_range_mut(
            &self,
            _lines: Range<StableRowIndex>,
            _for_line: &mut dyn ForEachPaneLogicalLine,
        ) {
        }

        fn get_logical_lines(&self, _lines: Range<StableRowIndex>) -> Vec<LogicalLine> {
            Vec::new()
        }

        fn get_dimensions(&self) -> RenderableDimensions {
            let size = *self.size.lock();
            RenderableDimensions {
                cols: size.cols,
                viewport_rows: size.rows,
                scrollback_rows: size.rows,
                physical_top: 0,
                scrollback_top: 0,
                dpi: size.dpi,
                pixel_width: size.pixel_width,
                pixel_height: size.pixel_height,
                reverse_video: false,
            }
        }

        fn get_title(&self) -> String {
            format!("kill-counting-pane-{}", self.id)
        }

        async fn search(
            &self,
            _pattern: crate::pane::Pattern,
            _range: Range<StableRowIndex>,
            _limit: Option<u32>,
        ) -> anyhow::Result<Vec<crate::pane::SearchResult>> {
            if self.search_pending {
                std::future::pending::<()>().await;
            }
            Ok(Vec::new())
        }

        fn send_paste(&self, _text: &str) -> anyhow::Result<()> {
            Ok(())
        }

        fn reader(&self) -> anyhow::Result<Option<Box<dyn std::io::Read + Send>>> {
            let on_reader = self.on_reader.lock().take();
            if let Some(on_reader) = on_reader {
                on_reader();
            }
            if self.fail_reader {
                return Err(anyhow!("intentional test pane reader acquisition failure"));
            }
            Ok(self.reader.lock().take())
        }

        fn writer(&self) -> MappedMutexGuard<'_, dyn std::io::Write> {
            MutexGuard::map(self.writes.lock(), |writes| {
                let writer: &mut dyn std::io::Write = writes;
                writer
            })
        }

        fn resize(&self, size: TerminalSize) -> anyhow::Result<()> {
            *self.size.lock() = size;
            Ok(())
        }

        fn key_down(&self, _key: KeyCode, _mods: KeyModifiers) -> anyhow::Result<()> {
            Ok(())
        }

        fn key_up(&self, _key: KeyCode, _mods: KeyModifiers) -> anyhow::Result<()> {
            Ok(())
        }

        fn mouse_event(&self, _event: MouseEvent) -> anyhow::Result<()> {
            Ok(())
        }

        fn perform_actions(&self, actions: Vec<Action>) {
            let on_actions = self.on_actions.lock().take();
            if let Some(on_actions) = on_actions {
                on_actions();
            }
            self.actions.fetch_add(actions.len(), Ordering::SeqCst);
        }

        fn is_dead(&self) -> bool {
            false
        }

        fn kill(&self) {
            self.kills.fetch_add(1, Ordering::SeqCst);
            let on_kill = self.on_kill.lock().take();
            if let Some(on_kill) = on_kill {
                on_kill();
            }
        }

        fn focus_changed(&self, focused: bool) {
            self.focus_events.lock().push(focused);
        }

        fn palette(&self) -> ColorPalette {
            ColorPalette::default()
        }

        fn domain_id(&self) -> DomainId {
            let on_domain_id = self.on_domain_id.lock().take();
            if let Some(on_domain_id) = on_domain_id {
                on_domain_id();
            }
            self.domain_id
        }

        fn is_mouse_grabbed(&self) -> bool {
            false
        }

        fn is_alt_screen_active(&self) -> bool {
            false
        }

        fn get_current_working_dir(&self, _policy: CachePolicy) -> Option<url::Url> {
            None
        }
    }

    struct GuardedMutationTestDomain {
        domain_id: DomainId,
        spawned_panes: Mutex<VecDeque<Arc<dyn Pane>>>,
        after_registration:
            Mutex<Option<Box<dyn FnOnce(&Arc<Mux>, &Arc<dyn Pane>) + Send + 'static>>>,
        supports_floating_spawn: bool,
    }

    impl GuardedMutationTestDomain {
        fn new(next_spawned_pane: Option<Arc<dyn Pane>>) -> Self {
            Self {
                domain_id: 1,
                spawned_panes: Mutex::new(next_spawned_pane.into_iter().collect()),
                after_registration: Mutex::new(None),
                supports_floating_spawn: true,
            }
        }

        fn with_panes(spawned_panes: Vec<Arc<dyn Pane>>) -> Self {
            Self {
                domain_id: 1,
                spawned_panes: Mutex::new(spawned_panes.into()),
                after_registration: Mutex::new(None),
                supports_floating_spawn: true,
            }
        }

        fn unsupported_floating(next_spawned_pane: Arc<dyn Pane>) -> Self {
            Self {
                domain_id: 1,
                spawned_panes: Mutex::new(VecDeque::from([next_spawned_pane])),
                after_registration: Mutex::new(None),
                supports_floating_spawn: false,
            }
        }

        fn with_after_registration(
            next_spawned_pane: Arc<dyn Pane>,
            after_registration: impl FnOnce(&Arc<Mux>, &Arc<dyn Pane>) + Send + 'static,
        ) -> Self {
            Self {
                domain_id: 1,
                spawned_panes: Mutex::new(VecDeque::from([next_spawned_pane])),
                after_registration: Mutex::new(Some(Box::new(after_registration))),
                supports_floating_spawn: true,
            }
        }

        fn for_domain(domain_id: DomainId) -> Self {
            Self {
                domain_id,
                spawned_panes: Mutex::new(VecDeque::new()),
                after_registration: Mutex::new(None),
                supports_floating_spawn: true,
            }
        }
    }

    #[async_trait::async_trait(?Send)]
    impl Domain for GuardedMutationTestDomain {
        fn supports_floating_pane_spawn(&self) -> bool {
            self.supports_floating_spawn
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
            mux.add_pane(unpublished.pane())?;
            Ok(unpublished.into_pane())
        }

        async fn spawn_unpublished_pane(
            &self,
            mux: &Arc<Mux>,
            _size: TerminalSize,
            _command: Option<CommandBuilder>,
            _command_dir: Option<String>,
        ) -> anyhow::Result<domain::UnpublishedPane> {
            let pane = self
                .spawned_panes
                .lock()
                .pop_front()
                .ok_or_else(|| anyhow!("test domain has no prepared pane"))?;
            if let Some(after_registration) = self.after_registration.lock().take() {
                after_registration(mux, &pane);
            }
            Ok(domain::UnpublishedPane::new(pane))
        }

        fn detachable(&self) -> bool {
            false
        }

        fn domain_id(&self) -> DomainId {
            self.domain_id
        }

        fn domain_name(&self) -> &str {
            match self.domain_id {
                1 => "guarded-mutation-test",
                2 => "guarded-mutation-foreign-test",
                _ => "guarded-mutation-other-test",
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

        fn detach(&self) -> anyhow::Result<()> {
            Ok(())
        }

        fn state(&self) -> DomainState {
            DomainState::Attached
        }
    }

    struct BlockingDetachTestDomain {
        entered: std::sync::mpsc::SyncSender<()>,
        release: StdMutex<std::sync::mpsc::Receiver<()>>,
    }

    #[async_trait::async_trait(?Send)]
    impl Domain for BlockingDetachTestDomain {
        async fn spawn_pane(
            &self,
            _mux: &Arc<Mux>,
            _size: TerminalSize,
            _command: Option<CommandBuilder>,
            _command_dir: Option<String>,
        ) -> anyhow::Result<Arc<dyn Pane>> {
            Err(anyhow!("blocking detach test domain cannot spawn panes"))
        }

        fn detachable(&self) -> bool {
            true
        }

        fn domain_id(&self) -> DomainId {
            1
        }

        fn domain_name(&self) -> &str {
            "blocking-detach-test"
        }

        async fn attach(
            &self,
            _mux: &Arc<Mux>,
            _owner_client_id: Option<Arc<ClientId>>,
            _window_id: Option<WindowId>,
        ) -> anyhow::Result<()> {
            Ok(())
        }

        fn detach(&self) -> anyhow::Result<()> {
            self.entered
                .send(())
                .map_err(|_| anyhow!("window-removal test stopped observing detach"))?;
            self.release
                .lock()
                .unwrap_or_else(|err| err.into_inner())
                .recv_timeout(Duration::from_secs(30))
                .map_err(|err| anyhow!("window-removal test did not release detach: {err}"))?;
            Ok(())
        }

        fn state(&self) -> DomainState {
            DomainState::Attached
        }
    }

    struct PanicDetachTestDomain {
        detaches: Arc<AtomicUsize>,
    }

    #[async_trait::async_trait(?Send)]
    impl Domain for PanicDetachTestDomain {
        async fn spawn_pane(
            &self,
            _mux: &Arc<Mux>,
            _size: TerminalSize,
            _command: Option<CommandBuilder>,
            _command_dir: Option<String>,
        ) -> anyhow::Result<Arc<dyn Pane>> {
            Err(anyhow!("panic detach test domain cannot spawn panes"))
        }

        fn detachable(&self) -> bool {
            true
        }

        fn domain_id(&self) -> DomainId {
            1
        }

        fn domain_name(&self) -> &str {
            "panic-detach-test"
        }

        async fn attach(
            &self,
            _mux: &Arc<Mux>,
            _owner_client_id: Option<Arc<ClientId>>,
            _window_id: Option<WindowId>,
        ) -> anyhow::Result<()> {
            Ok(())
        }

        fn detach(&self) -> anyhow::Result<()> {
            self.detaches.fetch_add(1, Ordering::SeqCst);
            panic!("intentional domain detach panic during window retirement");
        }

        fn state(&self) -> DomainState {
            DomainState::Attached
        }
    }

    struct RegistrationObservingReader {
        mux: Arc<Mux>,
        pane_id: PaneId,
        tab_id: TabId,
        pane_added: Arc<AtomicBool>,
        result_tx: Option<std::sync::mpsc::Sender<(bool, bool, bool)>>,
    }

    impl std::io::Read for RegistrationObservingReader {
        fn read(&mut self, _buf: &mut [u8]) -> std::io::Result<usize> {
            if let Some(result_tx) = self.result_tx.take() {
                let pane_is_registered = self.mux.get_pane(self.pane_id).is_some();
                let tab_contains_pane = self
                    .mux
                    .get_tab(self.tab_id)
                    .and_then(|tab| tab.get_active_pane())
                    .is_some_and(|pane| pane.pane_id() == self.pane_id);
                let pane_added_was_emitted = self.pane_added.load(Ordering::SeqCst);
                let _ = result_tx.send((
                    pane_is_registered,
                    tab_contains_pane,
                    pane_added_was_emitted,
                ));
            }
            Ok(0)
        }
    }

    struct CancellationObservingReader {
        reads: Arc<AtomicUsize>,
        dropped_tx: Option<std::sync::mpsc::Sender<()>>,
    }

    impl std::io::Read for CancellationObservingReader {
        fn read(&mut self, _buf: &mut [u8]) -> std::io::Result<usize> {
            self.reads.fetch_add(1, Ordering::SeqCst);
            Ok(0)
        }
    }

    impl Drop for CancellationObservingReader {
        fn drop(&mut self) {
            if let Some(dropped_tx) = self.dropped_tx.take() {
                let _ = dropped_tx.send(());
            }
        }
    }

    fn test_size() -> TerminalSize {
        TerminalSize {
            rows: 24,
            cols: 80,
            pixel_width: 800,
            pixel_height: 600,
            dpi: 96,
        }
    }

    fn test_window_reorder_request(
        mux: &Mux,
        window_id: WindowId,
        desired_tab_ids: Vec<TabId>,
        desired_active_tab_id: Option<TabId>,
        mutation_sequence: u64,
    ) -> ReorderWindowTabsRequest {
        let (session_incarnation, _) = mux
            .topology_snapshot_authority()
            .expect("test mux topology authority");
        let expected_order_revision = mux
            .window_order_snapshot(window_id)
            .expect("test window snapshot validity")
            .expect("test window presence")
            .order_revision();
        test_window_reorder_request_for(
            session_incarnation,
            window_id,
            expected_order_revision,
            desired_tab_ids,
            desired_active_tab_id,
            mutation_sequence,
        )
    }

    fn test_window_reorder_request_for(
        session_incarnation: MuxSessionIncarnation,
        window_id: WindowId,
        expected_order_revision: WindowOrderRevision,
        desired_tab_ids: Vec<TabId>,
        desired_active_tab_id: Option<TabId>,
        mutation_sequence: u64,
    ) -> ReorderWindowTabsRequest {
        ReorderWindowTabsRequest::try_new_v1(
            [0x62; 16],
            session_incarnation,
            window_id,
            expected_order_revision,
            desired_tab_ids,
            desired_active_tab_id,
            WindowOrderMutationId::new([0x73; 16], mutation_sequence),
        )
        .expect("test reorder intent must be constructible")
    }

    fn register_test_pane(mux: &Arc<Mux>, pane_id: PaneId) -> Arc<dyn Pane> {
        let (pane, _) = KillCountingPane::new(pane_id, test_size());
        mux.add_pane(&pane)
            .expect("test pane should register with mux");
        pane
    }

    fn register_attached_test_pane(
        _global_scheduler_guard: &StdMutexGuard<'static, ()>,
        mux: &Arc<Mux>,
        pane: &Arc<dyn Pane>,
    ) -> (Arc<Tab>, WindowId) {
        let tab = Arc::new(Tab::new(&test_size()));
        tab.assign_pane(pane);
        mux.add_tab_and_active_pane(&tab)
            .expect("test pane and tab should register atomically");
        let window = mux.new_empty_window(None, None);
        let window_id = *window;
        mux.add_tab_to_window(&tab, window_id)
            .expect("test tab should attach to its exact window");
        (tab, window_id)
    }

    fn reliable_input_test_event(character: char) -> KeyEvent {
        KeyEvent {
            key: termwiz::input::KeyCode::Char(character),
            modifiers: termwiz::input::Modifiers::CTRL,
        }
    }

    #[test]
    fn reliable_input_ledger_deduplicates_commit_and_survives_client_reconnect() {
        let global_guard = global_test_lock();
        let mux = Arc::new(Mux::new(None));
        let (pane, _kills) = KillCountingPane::new(301, test_size());
        let (_tab, _window_id) = register_attached_test_pane(&global_guard, &mux, &pane);
        let registration = mux
            .capture_pane_registration(&pane)
            .expect("reliable input pane registration");
        let original_client = Arc::new(ClientId::new());
        mux.register_client(Arc::clone(&original_client));
        let event = reliable_input_test_event('x');

        let ReliableInputClaimOutcome::Execute(mut permit) = mux.claim_reliable_key_event(
            Some(&original_client),
            &registration,
            11,
            ReliableInputKeyKind::KeyDown,
            &event,
        ) else {
            panic!("first exact identity must own execution");
        };
        assert!(matches!(
            mux.claim_reliable_key_event(
                Some(&original_client),
                &registration,
                11,
                ReliableInputKeyKind::KeyDown,
                &event,
            ),
            ReliableInputClaimOutcome::DuplicatePending
        ));
        assert!(permit.begin_side_effect());
        assert!(permit.commit_applied());
        assert!(matches!(
            mux.claim_reliable_key_event(
                Some(&original_client),
                &registration,
                11,
                ReliableInputKeyKind::KeyDown,
                &event,
            ),
            ReliableInputClaimOutcome::DuplicateApplied
        ));

        let replacement_client = Arc::new(original_client.as_ref().clone());
        mux.register_client(Arc::clone(&replacement_client));
        assert!(matches!(
            mux.claim_reliable_key_event(
                Some(&original_client),
                &registration,
                11,
                ReliableInputKeyKind::KeyDown,
                &event,
            ),
            ReliableInputClaimOutcome::ClientRegistrationRetired
        ));
        assert!(matches!(
            mux.claim_reliable_key_event(
                Some(&replacement_client),
                &registration,
                11,
                ReliableInputKeyKind::KeyDown,
                &event,
            ),
            ReliableInputClaimOutcome::DuplicateApplied
        ));
    }

    #[test]
    fn reliable_input_ledger_rolls_back_before_callback_and_fails_closed_after_start() {
        let global_guard = global_test_lock();
        let mux = Arc::new(Mux::new(None));
        let (pane, _kills) = KillCountingPane::new(302, test_size());
        let (_tab, _window_id) = register_attached_test_pane(&global_guard, &mux, &pane);
        let registration = mux
            .capture_pane_registration(&pane)
            .expect("reliable input pane registration");
        let client = Arc::new(ClientId::new());
        mux.register_client(Arc::clone(&client));
        let event = reliable_input_test_event('y');

        let ReliableInputClaimOutcome::Execute(permit) = mux.claim_reliable_key_event(
            Some(&client),
            &registration,
            21,
            ReliableInputKeyKind::KeyDown,
            &event,
        ) else {
            panic!("first exact identity must own execution");
        };
        drop(permit);
        let ReliableInputClaimOutcome::Execute(mut retry) = mux.claim_reliable_key_event(
            Some(&client),
            &registration,
            21,
            ReliableInputKeyKind::KeyDown,
            &event,
        ) else {
            panic!("pre-callback cancellation must permit exact retry");
        };
        assert!(retry.begin_side_effect());
        drop(retry);
        assert!(matches!(
            mux.claim_reliable_key_event(
                Some(&client),
                &registration,
                21,
                ReliableInputKeyKind::KeyDown,
                &event,
            ),
            ReliableInputClaimOutcome::OutcomeUnknown
        ));
        assert!(matches!(
            mux.claim_reliable_key_event(
                Some(&client),
                &registration,
                20,
                ReliableInputKeyKind::KeyDown,
                &event,
            ),
            ReliableInputClaimOutcome::StaleSerial
        ));
        assert!(matches!(
            mux.claim_reliable_key_event(
                Some(&client),
                &registration,
                21,
                ReliableInputKeyKind::KeyUp,
                &event,
            ),
            ReliableInputClaimOutcome::IdentityConflict
        ));
        assert!(matches!(
            mux.claim_reliable_key_event(
                Some(&client),
                &registration,
                22,
                ReliableInputKeyKind::KeyUp,
                &event,
            ),
            ReliableInputClaimOutcome::Execute(_)
        ));
    }

    #[test]
    fn reliable_input_client_ledger_has_a_hard_cardinality_bound() {
        let mut ledger = ReliableInputLedger::new();
        let mut client_id = ClientId::new();
        for id in 0..MAX_RELIABLE_INPUT_CLIENTS {
            client_id.id = id;
            assert!(ledger.prepare_client(&client_id));
        }
        assert_eq!(ledger.clients.len(), MAX_RELIABLE_INPUT_CLIENTS);
        client_id.id = MAX_RELIABLE_INPUT_CLIENTS;
        assert!(!ledger.prepare_client(&client_id));
        assert_eq!(ledger.clients.len(), MAX_RELIABLE_INPUT_CLIENTS);
    }

    #[test]
    fn exact_receipt_construction_does_not_reenter_pane_identity_callbacks() {
        let global_guard = global_test_lock();
        let mux = Arc::new(Mux::new(None));
        let (pane, _kills, pane_id_calls) =
            KillCountingPane::new_with_pane_id_counter(216, test_size());
        let (tab, window_id) = register_attached_test_pane(&global_guard, &mux, &pane);
        let registration = mux
            .capture_pane_registration(&pane)
            .expect("receipt test pane registration");
        let calls_before_receipts = pane_id_calls.load(Ordering::SeqCst);

        let split = SplitCommitReceipt::from_exact_parts(
            Arc::clone(&pane),
            registration.clone(),
            Arc::clone(&tab),
            window_id,
            test_size(),
        );
        let moved = MoveCommitReceipt::from_exact_parts(
            Arc::clone(&pane),
            registration,
            tab,
            window_id,
            test_size(),
        );

        assert_eq!(split.pane_id(), 216);
        assert_eq!(moved.pane_id(), 216);
        assert_eq!(
            pane_id_calls.load(Ordering::SeqCst),
            calls_before_receipts,
            "receipt construction must use exact registration identity instead of Pane callbacks"
        );
    }

    fn pane_with_blocked_reader(
        pane_id: PaneId,
    ) -> (
        Arc<dyn Pane>,
        Arc<AtomicUsize>,
        std::sync::mpsc::Receiver<()>,
        std::sync::mpsc::Sender<()>,
    ) {
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let (pane, kills) =
            KillCountingPane::new_with_reader_callback(pane_id, test_size(), move || {
                entered_tx
                    .send(())
                    .expect("registration thread should report reader acquisition");
                release_rx
                    .recv_timeout(Duration::from_secs(30))
                    .expect("test should release blocked reader acquisition");
            });
        (pane, kills, entered_rx, release_tx)
    }

    fn tab_with_kill_counter(mux: &Arc<Mux>, pane_id: PaneId) -> (Arc<Tab>, Arc<AtomicUsize>) {
        let size = test_size();
        let tab = Arc::new(Tab::new(&size));
        let (pane, kills) = KillCountingPane::new(pane_id, size);
        tab.assign_pane(&pane);
        mux.add_tab_and_active_pane(&tab)
            .expect("test tab should register with mux");
        (tab, kills)
    }

    #[test]
    fn add_pane_rejects_a_different_instance_with_the_same_id() {
        let mux = Arc::new(Mux::new(None));
        let (first, _) = KillCountingPane::new(77, test_size());
        let (duplicate, _) = KillCountingPane::new(77, test_size());

        mux.add_pane(&first)
            .expect("first pane instance should register");
        let err = mux
            .add_pane(&duplicate)
            .expect_err("a different pane instance must not replace the registered pane");
        let collision = err
            .downcast_ref::<PaneIdCollision>()
            .expect("duplicate registration should preserve its typed error");
        assert_eq!(collision.pane_id, 77);

        let registered = mux
            .get_pane(77)
            .expect("first pane should remain registered");
        assert!(Arc::ptr_eq(&registered, &first));
        assert_eq!(mux.panes.read().len(), 1);
    }

    #[test]
    fn standalone_mux_reader_is_not_misclassified_as_stale_without_a_global_owner() {
        let _guard = global_test_lock();
        Mux::shutdown();
        let mux = Arc::new(Mux::new(None));
        let (pane, _) = KillCountingPane::new(75, test_size());
        mux.add_pane(&pane)
            .expect("standalone pane should register");
        let dead = Arc::new(AtomicBool::new(false));
        let generation = Arc::clone(
            &mux.panes
                .read()
                .get(&75)
                .expect("registered test pane has a generation")
                .generation,
        );

        send_actions_to_mux(&Arc::downgrade(&pane), &generation, &dead, Vec::new());

        assert!(
            !dead.load(Ordering::Acquire),
            "absence of a global mux must not terminate a valid standalone reader",
        );
    }

    #[test]
    fn pane_callbacks_remain_bound_to_exact_originating_registration() {
        let _guard = global_test_lock();
        Mux::shutdown();
        let originating_mux = Arc::new(Mux::new(None));
        let replacement_mux = Arc::new(Mux::new(None));
        let (originating_pane, _) = KillCountingPane::new(105, test_size());
        let (replacement_pane, _) = KillCountingPane::new(105, test_size());
        originating_mux
            .add_pane(&originating_pane)
            .expect("originating pane should register");
        replacement_mux
            .add_pane(&replacement_pane)
            .expect("replacement mux should register its distinct same-ID pane");
        Mux::set_mux(&replacement_mux);

        let generation = Arc::clone(
            &originating_mux
                .panes
                .read()
                .get(&105)
                .expect("originating generation should be live")
                .generation,
        );
        let target = PaneRegistrationHandle::new(&originating_pane, &generation);
        let clipboard_events = Arc::new(AtomicUsize::new(0));
        let download_events = Arc::new(AtomicUsize::new(0));
        let clipboard_events_for_subscriber = Arc::clone(&clipboard_events);
        let download_events_for_subscriber = Arc::clone(&download_events);
        originating_mux
            .subscribe(move |notification| {
                match notification {
                    MuxNotification::AssignClipboard { pane_id: 105, .. } => {
                        clipboard_events_for_subscriber.fetch_add(1, Ordering::SeqCst);
                    }
                    MuxNotification::SaveToDownloads { .. } => {
                        download_events_for_subscriber.fetch_add(1, Ordering::SeqCst);
                    }
                    _ => {}
                }
                true
            })
            .expect("originating mux subscription should allocate an identifier");
        let replacement_events = Arc::new(AtomicUsize::new(0));
        let replacement_events_for_subscriber = Arc::clone(&replacement_events);
        replacement_mux
            .subscribe(move |notification| {
                if matches!(
                    notification,
                    MuxNotification::AssignClipboard { .. }
                        | MuxNotification::SaveToDownloads { .. }
                ) {
                    replacement_events_for_subscriber.fetch_add(1, Ordering::SeqCst);
                }
                true
            })
            .expect("replacement mux subscription should allocate an identifier");

        let clipboard = MuxClipboard {
            target: target.clone(),
        };
        let downloader = MuxDownloader { target };
        clipboard
            .set_contents(ClipboardSelection::Clipboard, Some("exact".to_string()))
            .expect("live exact callback should resolve");
        downloader.save_to_downloads(Some("exact.txt".to_string()), vec![1, 2, 3]);

        assert_eq!(clipboard_events.load(Ordering::SeqCst), 1);
        assert_eq!(download_events.load(Ordering::SeqCst), 1);
        assert_eq!(replacement_events.load(Ordering::SeqCst), 0);

        originating_mux.remove_pane_if_same_generation(105, &originating_pane, &generation);
        assert!(
            clipboard
                .set_contents(ClipboardSelection::Clipboard, Some("stale".to_string()))
                .is_err(),
            "retired callbacks must fail closed",
        );
        downloader.save_to_downloads(Some("stale.txt".to_string()), vec![4, 5, 6]);
        assert_eq!(clipboard_events.load(Ordering::SeqCst), 1);
        assert_eq!(download_events.load(Ordering::SeqCst), 1);
        assert_eq!(replacement_events.load(Ordering::SeqCst), 0);
        assert!(
            replacement_mux
                .get_pane(105)
                .is_some_and(|pane| Arc::ptr_eq(&pane, &replacement_pane)),
            "retiring the origin must preserve the replacement mux's same-ID pane",
        );
        Mux::shutdown();
    }

    #[test]
    fn retired_generation_fences_same_arc_readd_until_admitted_operations_finish() {
        let mux = Arc::new(Mux::new(None));
        let (pane, _) = KillCountingPane::new(101, test_size());
        let actions = Arc::clone(
            &pane
                .downcast_ref::<KillCountingPane>()
                .expect("test pane concrete type")
                .actions,
        );
        mux.add_pane(&pane)
            .expect("initial pane generation should register");
        let old_generation = Arc::clone(
            &mux.panes
                .read()
                .get(&101)
                .expect("initial generation should be live")
                .generation,
        );
        let admitted = old_generation
            .try_acquire()
            .expect("live generation should admit an operation");

        mux.remove_pane_if_same_generation(101, &pane, &old_generation);
        let err = mux
            .add_pane(&pane)
            .expect_err("same Arc must stay fenced while an old operation is in flight");
        assert!(err.downcast_ref::<PaneIdCollision>().is_some());

        drop(admitted);
        mux.add_pane(&pane)
            .expect("same Arc may register after the old operation quiesces");
        let new_generation = Arc::clone(
            &mux.panes
                .read()
                .get(&101)
                .expect("replacement generation should be live")
                .generation,
        );
        assert!(!Arc::ptr_eq(&old_generation, &new_generation));

        let stale_dead = Arc::new(AtomicBool::new(false));
        send_actions_to_mux(
            &Arc::downgrade(&pane),
            &old_generation,
            &stale_dead,
            vec![Action::Print('x')],
        );
        assert!(stale_dead.load(Ordering::Acquire));
        assert_eq!(
            actions.load(Ordering::SeqCst),
            0,
            "retired generation must not mutate the same re-registered Arc",
        );

        let live_dead = Arc::new(AtomicBool::new(false));
        send_actions_to_mux(
            &Arc::downgrade(&pane),
            &new_generation,
            &live_dead,
            vec![Action::Print('y')],
        );
        assert!(!live_dead.load(Ordering::Acquire));
        assert_eq!(actions.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn removal_defers_kill_and_notification_until_admitted_operation_quiesces() {
        let mux = Arc::new(Mux::new(None));
        let (pane, kills) = KillCountingPane::new(104, test_size());
        let removed = Arc::new(AtomicUsize::new(0));
        let removed_for_subscriber = Arc::clone(&removed);
        mux.subscribe(move |notification| {
            if matches!(notification, MuxNotification::PaneRemoved(104)) {
                removed_for_subscriber.fetch_add(1, Ordering::SeqCst);
            }
            true
        })
        .expect("test mux subscription should allocate an identifier");
        mux.add_pane(&pane)
            .expect("initial pane generation should register");
        let generation = Arc::clone(
            &mux.panes
                .read()
                .get(&104)
                .expect("initial generation should be live")
                .generation,
        );
        let admitted_first = generation
            .try_acquire()
            .expect("live generation should admit the first operation");
        let admitted_last = generation
            .try_acquire()
            .expect("live generation should admit the second operation");

        mux.remove_pane_if_same_generation(104, &pane, &generation);

        assert!(mux.get_pane(104).is_none());
        assert_eq!(
            kills.load(Ordering::SeqCst),
            0,
            "Pane::kill must not overtake admitted work",
        );
        assert_eq!(
            removed.load(Ordering::SeqCst),
            0,
            "PaneRemoved must not overtake admitted work",
        );
        assert!(
            mux.add_pane(&pane).is_err(),
            "same-ID reuse must remain fenced while cleanup is pending",
        );

        drop(admitted_first);
        assert_eq!(
            kills.load(Ordering::SeqCst),
            0,
            "cleanup must wait for every admitted operation",
        );
        assert_eq!(removed.load(Ordering::SeqCst), 0);

        drop(admitted_last);

        assert_eq!(kills.load(Ordering::SeqCst), 1);
        assert_eq!(removed.load(Ordering::SeqCst), 1);
        mux.add_pane(&pane)
            .expect("same-ID reuse may proceed after exact cleanup completes");
    }

    #[test]
    fn admitted_mutation_delivers_output_before_deferred_kill_and_removed() {
        let _guard = global_test_lock();
        let executor = BoundedTestExecutor::new();
        let mux = Arc::new(Mux::new(None));
        let (actions_entered_tx, actions_entered_rx) = std::sync::mpsc::channel();
        let (release_actions_tx, release_actions_rx) = std::sync::mpsc::channel();
        let (pane, kills) =
            KillCountingPane::new_with_actions_callback(105, test_size(), move || {
                actions_entered_tx
                    .send(())
                    .expect("perform_actions should report admission");
                release_actions_rx
                    .recv_timeout(Duration::from_secs(30))
                    .expect("test should release blocked perform_actions");
            });
        let events = Arc::new(Mutex::new(Vec::new()));
        let events_for_subscriber = Arc::clone(&events);
        let kills_for_subscriber = Arc::clone(&kills);
        let mux_main_thread = std::thread::current().id();
        mux.subscribe(move |notification| {
            match notification {
                MuxNotification::PaneOutput(105) => {
                    assert_eq!(
                        kills_for_subscriber.load(Ordering::SeqCst),
                        0,
                        "PaneOutput must precede kill",
                    );
                    events_for_subscriber.lock().push("output");
                }
                MuxNotification::PaneRemoved(105) => {
                    assert_eq!(
                        std::thread::current().id(),
                        mux_main_thread,
                        "deferred cleanup must return to the mux main thread",
                    );
                    assert_eq!(
                        kills_for_subscriber.load(Ordering::SeqCst),
                        1,
                        "Pane::kill must precede PaneRemoved",
                    );
                    events_for_subscriber.lock().push("removed");
                }
                _ => {}
            }
            true
        })
        .expect("test mux subscription should allocate an identifier");
        mux.add_pane(&pane).expect("test pane should register");
        let generation = Arc::clone(
            &mux.panes
                .read()
                .get(&105)
                .expect("test generation should be live")
                .generation,
        );
        let dead = Arc::new(AtomicBool::new(false));
        let pane_for_actions = Arc::downgrade(&pane);
        let generation_for_actions = Arc::clone(&generation);
        let dead_for_actions = Arc::clone(&dead);
        let actions_thread = std::thread::spawn(move || {
            send_actions_to_mux_with_scheduler_state(
                &pane_for_actions,
                &generation_for_actions,
                &dead_for_actions,
                vec![Action::Print('x')],
                false,
            );
        });

        actions_entered_rx
            .recv_timeout(Duration::from_secs(30))
            .expect("perform_actions should block after exact output reservation");
        assert!(
            mux.pane_registration.try_lock().is_some(),
            "external pane mutation must run without the topology lock",
        );
        mux.remove_pane_if_same_generation(105, &pane, &generation);
        assert!(mux.get_pane(105).is_none());
        assert_eq!(kills.load(Ordering::SeqCst), 0);
        assert!(events.lock().is_empty());
        assert!(
            mux.add_pane(&pane).is_err(),
            "same-ID reuse must remain fenced through accepted output",
        );

        release_actions_tx
            .send(())
            .expect("release blocked perform_actions");
        actions_thread
            .join()
            .expect("perform_actions thread should not panic");
        assert!(!dead.load(Ordering::Acquire));
        assert_eq!(&*events.lock(), &["output"]);
        assert_eq!(kills.load(Ordering::SeqCst), 0);

        executor.run_until(Duration::from_secs(30), || {
            kills.load(Ordering::SeqCst) == 1
        });
        assert_eq!(&*events.lock(), &["output", "removed"]);
        assert_eq!(kills.load(Ordering::SeqCst), 1);
        mux.add_pane(&pane)
            .expect("same-ID reuse may proceed after output and removal dispatch");
    }

    #[test]
    fn discarded_retirement_dispatch_cannot_strand_claimed_cleanup() {
        let mux = Arc::new(Mux::new(None));
        let (pane, kills) = KillCountingPane::new(106, test_size());
        let removed = Arc::new(AtomicUsize::new(0));
        let removed_for_subscriber = Arc::clone(&removed);
        mux.subscribe(move |notification| {
            if matches!(notification, MuxNotification::PaneRemoved(106)) {
                removed_for_subscriber.fetch_add(1, Ordering::SeqCst);
            }
            true
        })
        .expect("test mux subscription should allocate an identifier");

        let lifecycle_notification = {
            let _registration = mux.pane_registration.lock();
            assert!(mux.retiring_pane_ids.lock().insert(106));
            mux.enqueue_pane_lifecycle_notification_locked(
                PaneLifecycleNotification::Removed(106),
                None,
            )
        };
        let dispatch = PaneRetirementDispatch::new(
            Arc::downgrade(&mux),
            PaneRetirementCompletion {
                pane_id: 106,
                pane: Arc::clone(&pane),
                kill: true,
                lifecycle_notification,
                cleanup_complete: Arc::new(AtomicBool::new(false)),
            },
        );
        drop(dispatch);

        assert_eq!(
            kills.load(Ordering::SeqCst),
            1,
            "dispatch guard must recover cleanup when the runnable is dropped",
        );
        assert_eq!(removed.load(Ordering::SeqCst), 1);
        mux.add_pane(&pane)
            .expect("fallback cleanup must release the same-ID fence");
    }

    #[test]
    fn quiescent_removal_does_not_block_unrelated_pane_lifecycle() {
        let mux = Arc::new(Mux::new(None));
        let (pane_a, _) = KillCountingPane::new(106, test_size());
        let (pane_b, _) = KillCountingPane::new(107, test_size());
        let (pane_c, _) = KillCountingPane::new(108, test_size());
        mux.add_pane(&pane_a).expect("pane A should register");
        let generation_a = Arc::clone(
            &mux.panes
                .read()
                .get(&106)
                .expect("pane A generation should be live")
                .generation,
        );
        let admitted_a = generation_a
            .try_acquire()
            .expect("pane A should admit an operation");
        let events = Arc::new(Mutex::new(Vec::new()));
        let events_for_subscriber = Arc::clone(&events);
        mux.subscribe(move |notification| {
            match notification {
                MuxNotification::PaneAdded(107) => {
                    events_for_subscriber.lock().push("added-b");
                }
                MuxNotification::PaneAdded(108) => {
                    events_for_subscriber.lock().push("added-c");
                }
                MuxNotification::PaneOutput(108) => {
                    events_for_subscriber.lock().push("output-c");
                }
                MuxNotification::PaneRemoved(106) => {
                    events_for_subscriber.lock().push("removed-a");
                }
                _ => {}
            }
            true
        })
        .expect("test mux subscription should allocate an identifier");

        mux.remove_pane_if_same_generation(106, &pane_a, &generation_a);
        mux.add_pane(&pane_b)
            .expect("pane B registration must not block behind pane A cleanup");
        mux.add_pane(&pane_c)
            .expect("pane C registration must not block behind pane A cleanup");
        assert!(mux.enqueue_pane_output_notification_for_pane_with_scheduler_state(&pane_c, false));

        assert_eq!(
            &*events.lock(),
            &["added-b", "added-c", "output-c"],
            "unready lifecycle work may order only its own pane",
        );

        drop(admitted_a);
        assert_eq!(
            &*events.lock(),
            &["added-b", "added-c", "output-c", "removed-a"],
        );
    }

    #[test]
    fn removal_cannot_kill_before_an_earlier_pane_added_is_observed() {
        let mux = Arc::new(Mux::new(None));
        let (pane, kills) = KillCountingPane::new(109, test_size());
        let observed = Arc::new(Mutex::new(Vec::new()));
        let observed_for_subscriber = Arc::clone(&observed);
        mux.subscribe(move |notification| {
            match notification {
                MuxNotification::PaneAdded(109) => {
                    observed_for_subscriber.lock().push("added");
                }
                MuxNotification::PaneRemoved(109) => {
                    observed_for_subscriber.lock().push("removed");
                }
                _ => {}
            }
            true
        })
        .expect("test mux subscription should allocate an identifier");

        let generation =
            PaneRegistrationGeneration::new(109, &mux.pane_retirements, Arc::downgrade(&mux));
        let added = {
            let _registration = mux.pane_registration.lock();
            mux.insert_pane_registration_locked(109, pane.domain_id(), &pane, &generation)
                .expect("test pane registration should publish");
            mux.enqueue_pane_lifecycle_notification_locked(
                PaneLifecycleNotification::Added(109),
                None,
            )
        };

        mux.remove_pane_if_same_generation(109, &pane, &generation);
        assert_eq!(
            kills.load(Ordering::SeqCst),
            0,
            "retirement must wait behind the earlier unready PaneAdded",
        );
        assert!(
            observed.lock().is_empty(),
            "neither lifecycle event is ready before PaneAdded completion",
        );

        mux.complete_pane_lifecycle_notification(added);
        assert_eq!(&*observed.lock(), &["added", "removed"]);
        assert_eq!(kills.load(Ordering::SeqCst), 1);
        let pending = mux.pending_pane_lifecycle.lock();
        assert!(pending.by_pane.is_empty());
        assert!(pending.retirements.is_empty());
        assert!(pending.ready_panes.is_empty());
        assert!(pending.ready_set.is_empty());
        assert!(!pending.draining);
    }

    #[test]
    fn generation_acquire_retire_race_never_admits_post_retirement_work() {
        for pane_id in 200..300 {
            let tracker = Arc::new(PaneRetirementTracker::default());
            let generation = PaneRegistrationGeneration::new(pane_id, &tracker, Weak::new());
            let barrier = Arc::new(std::sync::Barrier::new(3));

            let generation_for_acquire = Arc::clone(&generation);
            let acquire_barrier = Arc::clone(&barrier);
            let acquire_thread = std::thread::spawn(move || {
                acquire_barrier.wait();
                generation_for_acquire.try_acquire()
            });
            let generation_for_retire = Arc::clone(&generation);
            let retire_barrier = Arc::clone(&barrier);
            let retire_thread = std::thread::spawn(move || {
                retire_barrier.wait();
                generation_for_retire.retire();
            });

            barrier.wait();
            let admitted = acquire_thread
                .join()
                .expect("acquire racer should not panic");
            retire_thread.join().expect("retire racer should not panic");
            assert!(
                generation.try_acquire().is_none(),
                "no operation may be admitted after retirement",
            );
            let retired_state = generation.operation_state.load(Ordering::Acquire);

            if let Some(admitted) = admitted {
                assert_ne!(
                    retired_state & PANE_REGISTRATION_DEFERRED_RETIREMENT,
                    0,
                    "retirement must remember that admitted work existed at linearization",
                );
                assert!(
                    tracker.has_in_flight_retirement(pane_id),
                    "a pre-retirement operation must fence same-ID reuse",
                );
                drop(admitted);
            } else {
                assert_eq!(
                    retired_state & PANE_REGISTRATION_DEFERRED_RETIREMENT,
                    0,
                    "retirement that wins admission must retain inline cleanup policy",
                );
            }
            assert!(!tracker.has_in_flight_retirement(pane_id));
        }
    }

    #[test]
    fn generation_operation_count_exhaustion_fails_closed_without_wrap() {
        let tracker = Arc::new(PaneRetirementTracker::default());
        let generation = PaneRegistrationGeneration::new(300, &tracker, Weak::new());
        generation
            .operation_state
            .store(PANE_REGISTRATION_OPERATION_MASK, Ordering::Release);

        assert!(generation.try_acquire().is_none());
        assert_eq!(
            generation.operation_state.load(Ordering::Acquire),
            PANE_REGISTRATION_OPERATION_MASK,
        );
    }

    #[test]
    fn delayed_exact_registration_removal_preserves_same_arc_replacement() {
        let mux = Arc::new(Mux::new(None));
        let (pane, _) = KillCountingPane::new(102, test_size());
        mux.add_pane(&pane)
            .expect("initial pane generation should register");
        let delayed = mux
            .capture_pane_registration(&pane)
            .expect("live pane should yield exact registration identity");

        mux.remove_pane_if_same(102, &pane);
        mux.add_pane(&pane)
            .expect("same Arc should receive a fresh generation");
        assert!(
            !delayed.retire_if_current(),
            "a retired handle must not remove a replacement generation",
        );

        assert!(
            mux.get_pane(102)
                .is_some_and(|registered| Arc::ptr_eq(&registered, &pane)),
            "delayed cleanup for the old generation must preserve the replacement",
        );
    }

    #[test]
    fn pane_registration_handle_fails_after_same_arc_reregistration() {
        let mux = Arc::new(Mux::new(None));
        let (pane, kills) = KillCountingPane::new(131, test_size());

        mux.add_pane(&pane).expect("initial registration");
        let stale = mux
            .capture_pane_registration(&pane)
            .expect("initial handle");

        mux.remove_pane_if_same(131, &pane);
        assert_eq!(kills.load(Ordering::SeqCst), 1);

        mux.add_pane(&pane).expect("same Arc replacement");
        let current = mux
            .capture_pane_registration(&pane)
            .expect("replacement handle");

        assert!(!stale.same_registration(&current));
        assert_eq!(stale.try_with_current(|_| ()), None);
        assert_eq!(stale.try_with_current_output(|_| ()), None);
        assert!(!stale.retire_if_current());

        assert_eq!(
            current.try_with_current(|resolved| {
                assert_eq!(resolved.pane_id(), 131);
            }),
            Some(()),
        );
    }

    #[test]
    fn cancelled_search_releases_exact_registration_lease() {
        let _guard = global_test_lock();
        let executor = BoundedTestExecutor::new();
        let mux = Arc::new(Mux::new(None));
        let (pane, kills) = KillCountingPane::new_with_pending_search(158, test_size());
        mux.add_pane(&pane)
            .expect("pending-search pane registration");
        let registration = mux
            .capture_pane_registration(&pane)
            .expect("pending-search pane should yield an exact handle");

        let mut search = Box::pin(registration.search_if_current(
            &mux,
            crate::pane::Pattern::default(),
            0..1,
            None,
        ));
        let waker = std::task::Waker::noop();
        let mut context = std::task::Context::from_waker(waker);
        assert!(
            std::future::Future::poll(search.as_mut(), &mut context).is_pending(),
            "the test pane must keep search pending after exact admission",
        );
        assert_eq!(registration.active_operation_count(), 1);

        assert!(
            registration.retire_if_current(),
            "retirement should claim the exact pending-search registration",
        );
        let (replacement, _) = KillCountingPane::new(158, test_size());
        assert!(
            mux.add_pane(&replacement).is_err(),
            "the retiring generation must fence a same-ID replacement",
        );

        drop(search);
        executor.run_until(Duration::from_secs(5), || kills.load(Ordering::SeqCst) == 1);
        assert_eq!(
            registration.active_operation_count(),
            0,
            "cancelling search must release its generation operation lease",
        );
        mux.add_pane(&replacement)
            .expect("replacement should register once cancellation completes retirement");
    }

    #[test]
    fn exact_split_rejects_moving_target_into_itself_without_mutation() {
        let mux = Arc::new(Mux::new(None));
        let (pane, kills) = KillCountingPane::new(159, test_size());
        mux.add_pane(&pane).expect("target pane registration");
        let registration = mux
            .capture_pane_registration(&pane)
            .expect("target pane should yield an exact handle");

        let raw_error = match promise::spawn::block_on(mux.split_pane(
            159,
            SplitRequest::default(),
            SplitSource::MovePane(159),
            SpawnTabDomain::CurrentPaneDomain,
            None,
        )) {
            Ok(_) => panic!("the raw primitive must reject a self-move before topology lookup"),
            Err(error) => error,
        };
        assert!(
            raw_error.to_string().contains("into a split of itself"),
            "unexpected raw split error: {:#}",
            raw_error,
        );

        let error = promise::spawn::block_on(registration.split_moved_if_current(
            &mux,
            &registration,
            SplitRequest::default(),
            SpawnTabDomain::CurrentPaneDomain,
            None,
        ))
        .expect("the target registration remains current")
        .expect_err("moving a split target into itself must fail before mutation");

        assert!(
            error.to_string().contains("into a split of itself"),
            "unexpected error: {:#}",
            error,
        );
        assert_eq!(kills.load(Ordering::SeqCst), 0);
        assert!(
            mux.get_pane(159)
                .is_some_and(|registered| Arc::ptr_eq(&registered, &pane)),
            "the rejected operation must leave the exact target registered",
        );
        assert_eq!(
            registration.try_with_current(|current| current.pane_id()),
            Some(159),
        );
    }

    #[test]
    fn pane_operation_guard_fences_same_and_different_arc_replacements_after_retirement() {
        let global_guard = global_test_lock();
        for (pane_id, reuse_same_arc) in [(160, true), (161, false)] {
            let mux = Arc::new(Mux::new(None));
            let (pane, kills) = KillCountingPane::new(pane_id, test_size());
            let (tab, window_id) = register_attached_test_pane(&global_guard, &mux, &pane);
            let registration = mux
                .capture_pane_registration(&pane)
                .expect("attached pane should yield an exact registration");
            let guard = registration
                .operation_guard(&mux)
                .expect("live registration should admit an operation guard");

            assert!(
                registration.retire_if_current(),
                "retirement must remove the numeric registry mapping after admission"
            );
            assert!(mux.get_pane(pane_id).is_none());
            assert_eq!(
                kills.load(Ordering::SeqCst),
                0,
                "retirement cleanup must not overtake the admitted guard"
            );
            assert_eq!(registration.active_operation_count(), 1);
            mux.prune_dead_windows();

            let (_domain_id, guarded_window_id, guarded_tab) = guard
                .exact_location()
                .expect("pruning must retain exact topology authority for an admitted guard");
            assert_eq!(guarded_window_id, window_id);
            assert!(Arc::ptr_eq(&guarded_tab, &tab));
            guard.with_pane(|current| {
                assert!(std::ptr::eq(current, pane.as_ref()));
                assert!(mux.pane_registration.try_lock().is_some());
                assert!(mux.panes.try_write().is_some());
                assert!(mux.windows.try_write().is_some());
                assert!(tab.topology_lock_is_available_for_test());
            });

            let (different_arc, different_kills) = KillCountingPane::new(pane_id, test_size());
            let replacement = if reuse_same_arc {
                Arc::clone(&pane)
            } else {
                Arc::clone(&different_arc)
            };
            assert!(
                mux.add_pane(&replacement).is_err(),
                "same-ID replacement must remain fenced while the guard is live"
            );

            drop(guard);
            assert_eq!(registration.active_operation_count(), 0);
            assert_eq!(
                kills.load(Ordering::SeqCst),
                1,
                "dropping the final guard must complete exact retirement"
            );
            mux.remove_tab(tab.tab_id());
            mux.add_pane(&replacement)
                .expect("replacement may register after the exact guard quiesces");
            if !reuse_same_arc {
                assert_eq!(
                    different_kills.load(Ordering::SeqCst),
                    0,
                    "old-generation cleanup must not touch a different-Arc successor"
                );
                assert!(mux
                    .get_pane(pane_id)
                    .is_some_and(|current| Arc::ptr_eq(&current, &different_arc)));
            }
        }
    }

    #[test]
    fn pane_operation_guard_uses_origin_mux_after_global_swap_and_allows_reentrancy() {
        let global_guard = global_test_lock();
        Mux::shutdown();

        let origin = Arc::new(Mux::new(None));
        let replacement_mux = Arc::new(Mux::new(None));
        let (origin_pane, origin_kills) = KillCountingPane::new(162, test_size());
        let (replacement_pane, replacement_kills) = KillCountingPane::new(162, test_size());
        let (origin_tab, origin_window_id) =
            register_attached_test_pane(&global_guard, &origin, &origin_pane);
        replacement_mux
            .add_pane(&replacement_pane)
            .expect("replacement mux may use the same numeric pane ID");
        let registration = origin
            .capture_pane_registration(&origin_pane)
            .expect("origin registration");
        let guard = registration
            .operation_guard(&origin)
            .expect("origin operation admission");

        Mux::set_mux(&replacement_mux);
        assert!(registration.retire_if_current());
        guard.with_pane(|current| {
            assert!(std::ptr::eq(current, origin_pane.as_ref()));
            assert!(origin.pane_registration.try_lock().is_some());
            assert!(origin.panes.try_write().is_some());
            assert!(origin.windows.try_write().is_some());
            assert!(origin_tab.topology_lock_is_available_for_test());
        });
        let (_domain_id, window_id, tab) = guard
            .exact_location()
            .expect("global mux replacement must not redirect exact topology");
        assert_eq!(window_id, origin_window_id);
        assert!(Arc::ptr_eq(&tab, &origin_tab));
        assert!(guard.belongs_to(&origin));
        assert!(!guard.belongs_to(&replacement_mux));
        assert!(replacement_mux
            .get_pane(162)
            .is_some_and(|pane| Arc::ptr_eq(&pane, &replacement_pane)));

        drop(guard);
        assert_eq!(origin_kills.load(Ordering::SeqCst), 1);
        assert_eq!(replacement_kills.load(Ordering::SeqCst), 0);
        origin.remove_tab(origin_tab.tab_id());
        Mux::shutdown();
    }

    #[test]
    fn cancelling_pending_operation_guard_future_releases_retirement_fence() {
        let _global = global_test_lock();
        let executor = BoundedTestExecutor::new();
        let mux = Arc::new(Mux::new(None));
        let (pane, kills) = KillCountingPane::new(163, test_size());
        mux.add_pane(&pane)
            .expect("guard cancellation registration");
        let registration = mux
            .capture_pane_registration(&pane)
            .expect("guard cancellation handle");
        let guard = registration
            .operation_guard(&mux)
            .expect("guard cancellation admission");
        let mut pending = Box::pin(async move {
            std::future::pending::<()>().await;
            drop(guard);
        });
        let waker = std::task::Waker::noop();
        let mut context = std::task::Context::from_waker(waker);
        assert!(std::future::Future::poll(pending.as_mut(), &mut context).is_pending());

        assert!(registration.retire_if_current());
        let (replacement, _) = KillCountingPane::new(163, test_size());
        assert!(mux.add_pane(&replacement).is_err());
        assert_eq!(registration.active_operation_count(), 1);

        drop(pending);
        executor.run_until(Duration::from_secs(5), || kills.load(Ordering::SeqCst) == 1);
        assert_eq!(registration.active_operation_count(), 0);
        mux.add_pane(&replacement)
            .expect("future cancellation must release the replacement fence");
    }

    #[test]
    fn panicking_operation_guard_scope_releases_retirement_fence() {
        let _global = global_test_lock();
        let executor = BoundedTestExecutor::new();
        let mux = Arc::new(Mux::new(None));
        let (pane, kills) = KillCountingPane::new(164, test_size());
        mux.add_pane(&pane).expect("guard panic registration");
        let registration = mux
            .capture_pane_registration(&pane)
            .expect("guard panic handle");
        let guard = registration
            .operation_guard(&mux)
            .expect("guard panic admission");
        assert!(registration.retire_if_current());

        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
            guard.with_pane::<()>(|_| panic!("intentional operation-guard panic"));
        }));
        assert!(result.is_err());
        executor.run_until(Duration::from_secs(5), || kills.load(Ordering::SeqCst) == 1);
        assert_eq!(registration.active_operation_count(), 0);

        let (replacement, _) = KillCountingPane::new(164, test_size());
        mux.add_pane(&replacement)
            .expect("unwind must release the replacement fence");
    }

    #[test]
    fn pane_operation_guard_retains_exact_objects_only_until_drop() {
        let global_guard = global_test_lock();
        let executor = BoundedTestExecutor::new();
        let mux = Arc::new(Mux::new(None));
        let weak_mux = Arc::downgrade(&mux);
        let (pane, kills) = KillCountingPane::new(165, test_size());
        let weak_pane = Arc::downgrade(&pane);
        let (tab, _window_id) = register_attached_test_pane(&global_guard, &mux, &pane);
        let registration = mux
            .capture_pane_registration(&pane)
            .expect("collectability registration");
        let guard = registration
            .operation_guard(&mux)
            .expect("collectability operation admission");
        assert!(registration.retire_if_current());
        mux.remove_tab(tab.tab_id());

        drop(tab);
        drop(pane);
        drop(mux);
        assert!(weak_mux.upgrade().is_some());
        assert!(weak_pane.upgrade().is_some());

        drop(guard);
        executor.run_until(Duration::from_secs(5), || {
            weak_mux.upgrade().is_none() && weak_pane.upgrade().is_none()
        });
        assert_eq!(kills.load(Ordering::SeqCst), 1);
        assert_eq!(registration.active_operation_count(), 0);
    }

    #[test]
    fn prepared_pane_rolls_back_exact_registration_without_locks() {
        let mux = Arc::new(Mux::new(None));
        let weak_mux = Arc::downgrade(&mux);
        let (pane, kills) = KillCountingPane::new_with_kill_callback(166, test_size(), move || {
            let mux = weak_mux
                .upgrade()
                .expect("rollback callback should observe the exact mux");
            assert!(mux.pane_registration.try_lock().is_some());
            assert!(mux.panes.try_write().is_some());
            assert!(mux.pending_pane_lifecycle.try_lock().is_some());
            assert!(mux.retiring_pane_ids.try_lock().is_some());
        });
        mux.add_pane(&pane).expect("prepared pane registration");
        let registration = mux
            .capture_pane_registration(&pane)
            .expect("prepared pane handle");
        let prepared = domain::PreparedPane::new(Arc::clone(&pane), registration.clone());

        drop(prepared);

        assert!(mux.get_pane(166).is_none());
        assert_eq!(kills.load(Ordering::SeqCst), 1);
        assert_eq!(registration.active_operation_count(), 0);
    }

    #[test]
    fn stale_prepared_pane_rollback_preserves_exact_successor() {
        let mux = Arc::new(Mux::new(None));
        let (original, original_kills) = KillCountingPane::new(167, test_size());
        let (replacement, replacement_kills) = KillCountingPane::new(167, test_size());
        mux.add_pane(&original)
            .expect("prepared original registration");
        let registration = mux
            .capture_pane_registration(&original)
            .expect("prepared original handle");
        let prepared = domain::PreparedPane::new(Arc::clone(&original), registration.clone());

        assert!(registration.detach_local_if_current());
        mux.add_pane(&replacement)
            .expect("successor registration after exact local detach");
        drop(prepared);

        assert_eq!(original_kills.load(Ordering::SeqCst), 0);
        assert_eq!(replacement_kills.load(Ordering::SeqCst), 0);
        assert!(mux
            .get_pane(167)
            .is_some_and(|pane| Arc::ptr_eq(&pane, &replacement)));
    }

    #[test]
    fn domain_spawn_missing_window_rolls_back_registered_tab_and_pane() {
        let mux = Arc::new(Mux::new(None));
        let (spawned, spawned_kills) = KillCountingPane::new(179, test_size());
        let domain: Arc<dyn Domain> =
            Arc::new(GuardedMutationTestDomain::new(Some(Arc::clone(&spawned))));

        let result =
            promise::spawn::block_on(domain.spawn(&mux, test_size(), None, None, usize::MAX));

        assert!(result.is_err());
        assert!(mux.get_pane(spawned.pane_id()).is_none());
        assert!(mux.tabs.read().is_empty());
        assert!(mux.windows.read().is_empty());
        assert_eq!(spawned_kills.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn floating_spawn_commits_without_any_tab_or_window_order_churn() {
        let global_guard = global_test_lock();
        let mux = Arc::new(Mux::new(None));
        let (target, target_kills) = KillCountingPane::new(180, test_size());
        let (tab, window_id) = register_attached_test_pane(&global_guard, &mux, &target);

        let mut spawned_panes = Vec::new();
        let mut spawned_kills = Vec::new();
        for pane_id in 181..189 {
            let (pane, kills) = KillCountingPane::new(pane_id, test_size());
            spawned_panes.push(pane);
            spawned_kills.push(kills);
        }
        let domain: Arc<dyn Domain> =
            Arc::new(GuardedMutationTestDomain::with_panes(spawned_panes.clone()));
        mux.add_domain(&domain)
            .expect("register floating test domain");

        let before = mux
            .window_order_snapshot(window_id)
            .expect("valid destination window")
            .expect("destination window present");
        assert_eq!(mux.tabs.read().len(), 1);

        for spawned in &spawned_panes {
            let target = mux
                .capture_floating_spawn_target(window_id)
                .expect("capture exact floating destination");
            let receipt = promise::spawn::block_on(mux.spawn_floating_pane(
                target,
                FloatingPaneRect {
                    left: 2,
                    top: 3,
                    width: 20,
                    height: 8,
                },
                None,
                None,
                SpawnTabDomain::DomainId(1),
                Arc::new(TermConfig::new()),
                None,
            ))
            .expect("floating spawn should commit");

            assert_eq!(receipt.pane_id(), spawned.pane_id());
            assert_eq!(receipt.tab_id(), tab.tab_id());
            assert_eq!(receipt.window_id(), window_id);
            receipt.with_pane(|pane| assert!(std::ptr::eq(pane, spawned.as_ref())));
        }

        let after = mux
            .window_order_snapshot(window_id)
            .expect("valid destination window after spawns")
            .expect("destination window remains present");
        assert_eq!(after.order_revision(), before.order_revision());
        assert_eq!(
            after.ordered_tab_ids().collect::<Vec<_>>(),
            before.ordered_tab_ids().collect::<Vec<_>>()
        );
        assert_eq!(after.active_tab_id(), before.active_tab_id());
        assert_eq!(mux.tabs.read().len(), 1, "no temporary tabs may register");
        assert_eq!(tab.iter_floating_panes().len(), spawned_panes.len());
        let all_panes = tab.iter_all_panes();
        assert_eq!(all_panes.len(), spawned_panes.len() + 1);
        for spawned in &spawned_panes {
            assert_eq!(
                all_panes
                    .iter()
                    .filter(|candidate| Arc::ptr_eq(candidate, spawned))
                    .count(),
                1,
                "each spawned allocation must have one structural owner"
            );
        }
        assert_eq!(target_kills.load(Ordering::SeqCst), 0);
        for kills in spawned_kills {
            assert_eq!(kills.load(Ordering::SeqCst), 0);
        }
    }

    #[test]
    fn floating_spawn_rolls_back_when_destination_retires_during_spawn() {
        let global_guard = global_test_lock();
        let mux = Arc::new(Mux::new(None));
        let (target, target_kills) = KillCountingPane::new(189, test_size());
        let (tab, window_id) = register_attached_test_pane(&global_guard, &mux, &target);
        let (spawned, spawned_kills) = KillCountingPane::new(190, test_size());
        let doomed_tab_id = tab.tab_id();
        let domain: Arc<dyn Domain> = Arc::new(GuardedMutationTestDomain::with_after_registration(
            Arc::clone(&spawned),
            move |mux, _spawned| {
                assert!(mux.remove_tab(doomed_tab_id).is_some());
            },
        ));
        mux.add_domain(&domain)
            .expect("register floating test domain");

        let target = mux
            .capture_floating_spawn_target(window_id)
            .expect("capture exact floating destination");
        let result = promise::spawn::block_on(mux.spawn_floating_pane(
            target,
            FloatingPaneRect {
                left: 0,
                top: 0,
                width: 10,
                height: 5,
            },
            None,
            None,
            SpawnTabDomain::DomainId(1),
            Arc::new(TermConfig::new()),
            None,
        ));

        assert!(result.is_err());
        assert!(mux.get_pane(spawned.pane_id()).is_none());
        assert!(mux.get_tab(doomed_tab_id).is_none());
        assert_eq!(
            mux.tabs.read().len(),
            0,
            "rollback must not leak a temp tab"
        );
        assert_eq!(spawned_kills.load(Ordering::SeqCst), 1);
        assert_eq!(target_kills.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn slow_floating_spawn_preserves_a_later_user_tab_switch() {
        let global_guard = global_test_lock();
        let mux = Arc::new(Mux::new(None));
        let (target, _target_kills) = KillCountingPane::new(191, test_size());
        let (destination, window_id) = register_attached_test_pane(&global_guard, &mux, &target);
        let (other_pane, _other_kills) = KillCountingPane::new(192, test_size());
        let other_tab = Arc::new(Tab::new(&test_size()));
        other_tab.assign_pane(&other_pane);
        mux.add_tab_and_active_pane(&other_tab)
            .expect("register other tab");
        mux.add_tab_to_window(&other_tab, window_id)
            .expect("attach other tab to destination window");
        let revision_before_switch = mux
            .window_order_snapshot(window_id)
            .expect("valid window")
            .expect("present window")
            .order_revision();

        let (spawned, spawned_kills) = KillCountingPane::new(193, test_size());
        let other_tab_for_hook = Arc::clone(&other_tab);
        let domain: Arc<dyn Domain> = Arc::new(GuardedMutationTestDomain::with_after_registration(
            Arc::clone(&spawned),
            move |mux, _spawned| {
                let mut window = mux.get_window_mut(window_id).expect("live test window");
                let other_idx = window
                    .iter()
                    .position(|tab| Arc::ptr_eq(tab, &other_tab_for_hook))
                    .expect("other tab remains attached");
                window.save_and_then_set_active(other_idx);
            },
        ));
        mux.add_domain(&domain)
            .expect("register floating test domain");

        let target = mux
            .capture_floating_spawn_target(window_id)
            .expect("capture destination before simulated slow spawn");
        let receipt = promise::spawn::block_on(mux.spawn_floating_pane(
            target,
            FloatingPaneRect {
                left: 1,
                top: 1,
                width: 12,
                height: 6,
            },
            None,
            None,
            SpawnTabDomain::DomainId(1),
            Arc::new(TermConfig::new()),
            None,
        ))
        .expect("spawn should attach to captured tab without stealing focus");

        assert_eq!(receipt.tab_id(), destination.tab_id());
        assert!(!receipt.is_focused());
        assert!(destination.has_floating_pane(spawned.pane_id()));
        let after = mux
            .window_order_snapshot(window_id)
            .expect("valid window after spawn")
            .expect("window remains present");
        assert_eq!(after.active_tab_id(), Some(other_tab.tab_id()));
        assert_eq!(
            after.order_revision().get(),
            revision_before_switch.get() + 1,
            "only the simulated user switch may advance window order"
        );
        assert_eq!(spawned_kills.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn unsupported_floating_domain_rejects_before_spawning_any_pane() {
        let global_guard = global_test_lock();
        let mux = Arc::new(Mux::new(None));
        let (target, _target_kills) = KillCountingPane::new(194, test_size());
        let (_tab, window_id) = register_attached_test_pane(&global_guard, &mux, &target);
        let (never_spawned, never_spawned_kills) = KillCountingPane::new(195, test_size());
        let domain: Arc<dyn Domain> = Arc::new(GuardedMutationTestDomain::unsupported_floating(
            Arc::clone(&never_spawned),
        ));
        mux.add_domain(&domain)
            .expect("register unsupported floating test domain");
        let panes_before = mux.panes.read().len();
        let tabs_before = mux.tabs.read().len();

        let target = mux
            .capture_floating_spawn_target(window_id)
            .expect("capture exact destination");
        let result = promise::spawn::block_on(mux.spawn_floating_pane(
            target,
            FloatingPaneRect {
                left: 0,
                top: 0,
                width: 10,
                height: 5,
            },
            None,
            None,
            SpawnTabDomain::DomainId(1),
            Arc::new(TermConfig::new()),
            None,
        ));

        assert!(result.is_err());
        assert_eq!(mux.panes.read().len(), panes_before);
        assert_eq!(mux.tabs.read().len(), tabs_before);
        assert!(mux.get_pane(never_spawned.pane_id()).is_none());
        assert_eq!(never_spawned_kills.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn floating_spawn_topology_exhaustion_rolls_back_before_attachment() {
        let global_guard = global_test_lock();
        let mux = Arc::new(Mux::new(None));
        let (target, target_kills) = KillCountingPane::new(200, test_size());
        let (destination, window_id) = register_attached_test_pane(&global_guard, &mux, &target);
        let before = mux
            .window_order_snapshot(window_id)
            .expect("valid destination window")
            .expect("destination window present");
        let (spawned, spawned_kills) = KillCountingPane::new(201, test_size());
        let domain: Arc<dyn Domain> =
            Arc::new(GuardedMutationTestDomain::new(Some(Arc::clone(&spawned))));
        mux.add_domain(&domain)
            .expect("register floating test domain");
        {
            let mut topology = mux.topology.lock();
            topology.revision = TopologyRevision(u64::MAX - 1);
            topology.exhausted = false;
        }

        let target = mux
            .capture_floating_spawn_target(window_id)
            .expect("capture exact destination");
        let result = promise::spawn::block_on(mux.spawn_floating_pane(
            target,
            FloatingPaneRect {
                left: 0,
                top: 0,
                width: 10,
                height: 5,
            },
            None,
            None,
            SpawnTabDomain::DomainId(1),
            Arc::new(TermConfig::new()),
            None,
        ));

        assert!(result.is_err());
        assert!(!destination.has_floating_pane(spawned.pane_id()));
        assert!(mux.get_pane(spawned.pane_id()).is_none());
        assert_eq!(mux.tabs.read().len(), 1);
        let after = mux
            .window_order_snapshot(window_id)
            .expect("valid destination after rollback")
            .expect("destination remains present");
        assert_eq!(after.order_revision(), before.order_revision());
        assert_eq!(after.active_tab_id(), before.active_tab_id());
        let topology = mux.topology.lock();
        assert!(topology.exhausted);
        assert_eq!(topology.revision, TopologyRevision(u64::MAX - 1));
        assert_eq!(target_kills.load(Ordering::SeqCst), 0);
        assert_eq!(spawned_kills.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn floating_spawn_target_detach_rolls_back_with_no_mux_or_tab_locks_held() {
        let global_guard = global_test_lock();
        let mux = Arc::new(Mux::new(None));
        let (target, _target_kills) = KillCountingPane::new(196, test_size());
        let (destination, window_id) = register_attached_test_pane(&global_guard, &mux, &target);
        let weak_mux = Arc::downgrade(&mux);
        let weak_destination = Arc::downgrade(&destination);
        let (spawned, spawned_kills) =
            KillCountingPane::new_with_kill_callback(197, test_size(), move || {
                let mux = weak_mux
                    .upgrade()
                    .expect("rollback callback should retain originating mux");
                let destination = weak_destination
                    .upgrade()
                    .expect("rollback callback should retain destination tab");
                assert!(mux.pane_registration.try_lock().is_some());
                assert!(mux.panes.try_write().is_some());
                assert!(mux.tabs.try_write().is_some());
                assert!(mux.windows.try_write().is_some());
                let _ = destination.iter_all_panes();
            });
        let destination_for_hook = Arc::clone(&destination);
        let target_id = target.pane_id();
        let domain: Arc<dyn Domain> = Arc::new(GuardedMutationTestDomain::with_after_registration(
            Arc::clone(&spawned),
            move |_mux, _spawned| {
                let removed = destination_for_hook.remove_pane(target_id);
                assert!(removed.is_some());
            },
        ));
        mux.add_domain(&domain)
            .expect("register floating test domain");

        let target = mux
            .capture_floating_spawn_target(window_id)
            .expect("capture exact destination");
        let result = promise::spawn::block_on(mux.spawn_floating_pane(
            target,
            FloatingPaneRect {
                left: 0,
                top: 0,
                width: 10,
                height: 5,
            },
            None,
            None,
            SpawnTabDomain::DomainId(1),
            Arc::new(TermConfig::new()),
            None,
        ));

        assert!(result.is_err());
        assert!(!destination.has_floating_pane(spawned.pane_id()));
        assert!(mux.get_pane(spawned.pane_id()).is_none());
        assert_eq!(spawned_kills.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn floating_spawn_uses_originating_mux_after_global_singleton_swap() {
        let global_guard = global_test_lock();
        let originating_mux = Arc::new(Mux::new(None));
        let _mux_override = ScopedMuxOverride::install(&originating_mux);
        let replacement_mux = Arc::new(Mux::new(None));
        let (target, _target_kills) = KillCountingPane::new(198, test_size());
        let (destination, window_id) =
            register_attached_test_pane(&global_guard, &originating_mux, &target);
        let (spawned, spawned_kills) = KillCountingPane::new(199, test_size());
        let replacement_for_hook = Arc::clone(&replacement_mux);
        let domain: Arc<dyn Domain> = Arc::new(GuardedMutationTestDomain::with_after_registration(
            Arc::clone(&spawned),
            move |_mux, _spawned| Mux::set_mux(&replacement_for_hook),
        ));
        originating_mux
            .add_domain(&domain)
            .expect("register floating test domain");

        let target = originating_mux
            .capture_floating_spawn_target(window_id)
            .expect("capture exact originating destination");
        let receipt = promise::spawn::block_on(originating_mux.spawn_floating_pane(
            target,
            FloatingPaneRect {
                left: 2,
                top: 2,
                width: 10,
                height: 5,
            },
            None,
            None,
            SpawnTabDomain::DomainId(1),
            Arc::new(TermConfig::new()),
            None,
        ))
        .expect("originating mux should retain explicit commit authority");

        assert_eq!(receipt.tab_id(), destination.tab_id());
        assert!(destination.has_floating_pane(spawned.pane_id()));
        assert!(originating_mux
            .get_pane(spawned.pane_id())
            .is_some_and(|pane| Arc::ptr_eq(&pane, &spawned)));
        assert!(replacement_mux.get_pane(spawned.pane_id()).is_none());
        assert!(replacement_mux.tabs.read().is_empty());
        assert!(replacement_mux.windows.read().is_empty());
        assert!(Mux::try_get().is_some_and(|mux| Arc::ptr_eq(&mux, &replacement_mux)));
        assert_eq!(spawned_kills.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn domain_floating_reconcile_publishes_replays_and_retires_exact_owner() {
        let global_guard = global_test_lock();
        let mux = Arc::new(Mux::new(None));
        let (tiled, tiled_kills) = KillCountingPane::new(202, test_size());
        let (tab, window_id) = register_attached_test_pane(&global_guard, &mux, &tiled);
        let domain: Arc<dyn Domain> = Arc::new(GuardedMutationTestDomain::new(None));
        mux.add_domain(&domain)
            .expect("register authoritative floating test domain");
        let (floating, floating_kills) = KillCountingPane::new(203, test_size());
        let rect = FloatingPaneRect {
            left: 4,
            top: 3,
            width: 20,
            height: 8,
        };
        let desired = || DomainFloatingPaneState {
            tab: Arc::clone(&tab),
            pane: Arc::clone(&floating),
            pane_id: 203,
            rect,
            z_order: 7,
            visible: true,
            pinned: true,
            opacity: 0.75,
            focused: false,
        };

        let receipt = mux
            .reconcile_domain_floating_panes(
                1,
                vec![Arc::clone(&tiled), Arc::clone(&floating)],
                vec![desired()],
            )
            .expect("new floating mirror should publish with its exact owner");
        assert_eq!(receipt.changed_tab_ids, vec![tab.tab_id()]);
        assert_eq!(receipt.invalidated_window_ids, vec![window_id]);
        assert_eq!(receipt.registered_pane_ids, vec![203]);
        assert!(receipt.retired_pane_ids.is_empty());
        assert!(mux
            .get_pane(203)
            .is_some_and(|pane| Arc::ptr_eq(&pane, &floating)));
        let positioned = tab.iter_floating_panes();
        assert_eq!(positioned.len(), 1);
        assert!(Arc::ptr_eq(&positioned[0].pane, &floating));
        assert_eq!(positioned[0].pane_id, 203);
        assert_eq!(
            (
                positioned[0].left,
                positioned[0].top,
                positioned[0].width,
                positioned[0].height,
            ),
            (4, 3, 20, 8),
        );
        assert_eq!(positioned[0].z_order, 7);
        assert!(positioned[0].visible);
        assert!(positioned[0].pinned);
        assert_eq!(positioned[0].opacity.to_bits(), 0.75_f32.to_bits());
        assert!(!positioned[0].is_focused);
        assert_eq!(floating_kills.load(Ordering::SeqCst), 0);

        let replay = mux
            .reconcile_domain_floating_panes(
                1,
                vec![Arc::clone(&tiled), Arc::clone(&floating)],
                vec![desired()],
            )
            .expect("identical authoritative replay should succeed");
        assert_eq!(replay, DomainFloatingPaneReconcileReceipt::default());
        let replayed = tab.iter_floating_panes();
        assert_eq!(replayed.len(), 1);
        assert!(Arc::ptr_eq(&replayed[0].pane, &floating));
        assert_eq!(floating_kills.load(Ordering::SeqCst), 0);

        let retired = mux
            .reconcile_domain_floating_panes(1, vec![Arc::clone(&tiled)], Vec::new())
            .expect("absent remote float should detach and retire without a pane callback");
        assert_eq!(retired.changed_tab_ids, vec![tab.tab_id()]);
        assert_eq!(retired.invalidated_window_ids, vec![window_id]);
        assert!(retired.registered_pane_ids.is_empty());
        assert_eq!(retired.retired_pane_ids, vec![203]);
        assert!(tab.iter_floating_panes().is_empty());
        assert!(mux.get_pane(203).is_none());
        assert_eq!(floating_kills.load(Ordering::SeqCst), 0);
        assert_eq!(tiled_kills.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn domain_floating_reconcile_rejects_tiled_alias_without_mutation() {
        let global_guard = global_test_lock();
        let mux = Arc::new(Mux::new(None));
        let (tiled, tiled_kills) = KillCountingPane::new(204, test_size());
        let (tab, window_id) = register_attached_test_pane(&global_guard, &mux, &tiled);
        let domain: Arc<dyn Domain> = Arc::new(GuardedMutationTestDomain::new(None));
        mux.add_domain(&domain)
            .expect("register authoritative floating test domain");
        let before = mux
            .window_order_snapshot(window_id)
            .expect("valid test window")
            .expect("attached test window");

        let error = mux
            .reconcile_domain_floating_panes(
                1,
                vec![Arc::clone(&tiled)],
                vec![DomainFloatingPaneState {
                    tab: Arc::clone(&tab),
                    pane: Arc::clone(&tiled),
                    pane_id: 204,
                    rect: FloatingPaneRect {
                        left: 0,
                        top: 0,
                        width: 20,
                        height: 8,
                    },
                    z_order: 0,
                    visible: true,
                    pinned: false,
                    opacity: 1.0,
                    focused: false,
                }],
            )
            .expect_err("one exact pane cannot be both tiled and floating");

        assert!(
            error.to_string().contains("also tiled"),
            "unexpected error: {error:#}",
            error = error,
        );
        assert!(tab.iter_floating_panes().is_empty());
        assert!(mux
            .get_pane(204)
            .is_some_and(|pane| Arc::ptr_eq(&pane, &tiled)));
        let after = mux
            .window_order_snapshot(window_id)
            .expect("valid test window after rejection")
            .expect("attached test window after rejection");
        assert_eq!(after.order_revision(), before.order_revision());
        assert_eq!(after.active_tab_id(), before.active_tab_id());
        assert_eq!(tiled_kills.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn domain_floating_reconcile_rejects_window_retained_stale_tab() {
        let global_guard = global_test_lock();
        let mux = Arc::new(Mux::new(None));
        let (tiled, tiled_kills) = KillCountingPane::new(209, test_size());
        let (tab, window_id) = register_attached_test_pane(&global_guard, &mux, &tiled);
        let domain: Arc<dyn Domain> = Arc::new(GuardedMutationTestDomain::new(None));
        mux.add_domain(&domain)
            .expect("register authoritative floating test domain");
        let (floating, floating_kills) = KillCountingPane::new(210, test_size());
        let before = mux
            .window_order_snapshot(window_id)
            .expect("valid test window")
            .expect("attached test window");

        let removed = mux
            .tabs
            .write()
            .remove(&tab.tab_id())
            .expect("seed a stale exact tab retained only by its window");
        assert!(Arc::ptr_eq(&removed, &tab));

        let error = mux
            .reconcile_domain_floating_panes(
                1,
                vec![Arc::clone(&tiled), Arc::clone(&floating)],
                vec![DomainFloatingPaneState {
                    tab: Arc::clone(&tab),
                    pane: Arc::clone(&floating),
                    pane_id: 210,
                    rect: FloatingPaneRect {
                        left: 1,
                        top: 1,
                        width: 20,
                        height: 8,
                    },
                    z_order: 0,
                    visible: true,
                    pinned: false,
                    opacity: 1.0,
                    focused: false,
                }],
            )
            .expect_err("a window-retained stale tab must not gain floating owner authority");

        assert!(
            error
                .to_string()
                .contains("is not an exact live mux registration"),
            "unexpected error: {:#}",
            error,
        );
        assert!(mux.get_pane(210).is_none());
        assert!(tab.iter_floating_panes().is_empty());
        let after = mux
            .window_order_snapshot(window_id)
            .expect("valid test window after rejection")
            .expect("stale tab remains window-owned after rejection");
        assert_eq!(after.order_revision(), before.order_revision());
        assert_eq!(after.active_tab_id(), before.active_tab_id());
        assert_eq!(tiled_kills.load(Ordering::SeqCst), 0);
        assert_eq!(floating_kills.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn domain_floating_reconcile_preserves_foreign_slots_and_replays_wire_order() {
        let global_guard = global_test_lock();
        let mux = Arc::new(Mux::new(None));
        let _mux_override = ScopedMuxOverride::install(&mux);
        let (tiled, tiled_kills) = KillCountingPane::new(205, test_size());
        let (tab, _window_id) = register_attached_test_pane(&global_guard, &mux, &tiled);
        let domain: Arc<dyn Domain> = Arc::new(GuardedMutationTestDomain::for_domain(1));
        let foreign_domain: Arc<dyn Domain> = Arc::new(GuardedMutationTestDomain::for_domain(2));
        mux.add_domain(&domain)
            .expect("register reconciled floating test domain");
        mux.add_domain(&foreign_domain)
            .expect("register preserved foreign floating test domain");

        let (foreign, foreign_kills) = KillCountingPane::new_with_domain(206, test_size(), 2);
        mux.add_pane(&foreign)
            .expect("register the pre-existing foreign float");
        tab.add_floating_pane(
            Arc::clone(&foreign),
            FloatingPaneRect {
                left: 1,
                top: 1,
                width: 12,
                height: 6,
            },
        )
        .expect("attach the pre-existing foreign float");
        let foreign_before = tab.iter_floating_panes()[0].clone();

        let (first, first_kills) = KillCountingPane::new(207, test_size());
        let (second, second_kills) = KillCountingPane::new(208, test_size());
        let desired = |pane: &Arc<dyn Pane>, pane_id, left, z_order| DomainFloatingPaneState {
            tab: Arc::clone(&tab),
            pane: Arc::clone(pane),
            pane_id,
            rect: FloatingPaneRect {
                left,
                top: 3,
                width: 20,
                height: 8,
            },
            z_order,
            visible: true,
            pinned: false,
            opacity: 1.0,
            focused: false,
        };
        let authoritative = || vec![Arc::clone(&tiled), Arc::clone(&first), Arc::clone(&second)];

        mux.reconcile_domain_floating_panes(
            1,
            authoritative(),
            vec![desired(&first, 207, 3, 3), desired(&second, 208, 5, 3)],
        )
        .expect("initial domain floats should publish around the foreign slot");
        let initial = tab.iter_floating_panes();
        assert_eq!(initial.len(), 3);
        assert!(Arc::ptr_eq(&initial[0].pane, &foreign));
        assert!(Arc::ptr_eq(&initial[1].pane, &first));
        assert!(Arc::ptr_eq(&initial[2].pane, &second));

        let reordered = mux
            .reconcile_domain_floating_panes(
                1,
                authoritative(),
                vec![desired(&second, 208, 5, 3), desired(&first, 207, 3, 3)],
            )
            .expect("authoritative order should replace only the reconciled domain slots");
        assert_eq!(reordered.changed_tab_ids, vec![tab.tab_id()]);
        let current = tab.iter_floating_panes();
        assert_eq!(current.len(), 3);
        assert!(Arc::ptr_eq(&current[0].pane, &foreign));
        assert!(Arc::ptr_eq(&current[1].pane, &second));
        assert!(Arc::ptr_eq(&current[2].pane, &first));
        assert_eq!(current[0].pane_id, foreign_before.pane_id);
        assert_eq!(current[0].left, foreign_before.left);
        assert_eq!(current[0].top, foreign_before.top);
        assert_eq!(current[0].width, foreign_before.width);
        assert_eq!(current[0].height, foreign_before.height);
        assert_eq!(current[0].z_order, foreign_before.z_order);
        assert_eq!(current[0].visible, foreign_before.visible);
        assert_eq!(current[0].pinned, foreign_before.pinned);
        assert_eq!(
            current[0].opacity.to_bits(),
            foreign_before.opacity.to_bits()
        );
        assert_eq!(current[0].is_focused, foreign_before.is_focused);

        let replay = mux
            .reconcile_domain_floating_panes(
                1,
                authoritative(),
                vec![desired(&second, 208, 5, 3), desired(&first, 207, 3, 3)],
            )
            .expect("stable mixed-domain replay should be an exact no-op");
        assert_eq!(replay, DomainFloatingPaneReconcileReceipt::default());
        let replayed = tab.iter_floating_panes();
        assert_eq!(replayed.len(), 3);
        assert!(Arc::ptr_eq(&replayed[0].pane, &foreign));
        assert!(Arc::ptr_eq(&replayed[1].pane, &second));
        assert!(Arc::ptr_eq(&replayed[2].pane, &first));
        assert_eq!(tiled_kills.load(Ordering::SeqCst), 0);
        assert_eq!(foreign_kills.load(Ordering::SeqCst), 0);
        assert_eq!(first_kills.load(Ordering::SeqCst), 0);
        assert_eq!(second_kills.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn guarded_spawned_split_survives_target_registry_detach_after_admission() {
        let global_guard = global_test_lock();
        let mux = Arc::new(Mux::new(None));
        let (target, target_kills) = KillCountingPane::new(168, test_size());
        let (spawned, spawned_kills) = KillCountingPane::new(169, test_size());
        let (target_tab, target_window_id) =
            register_attached_test_pane(&global_guard, &mux, &target);
        let domain: Arc<dyn Domain> =
            Arc::new(GuardedMutationTestDomain::new(Some(Arc::clone(&spawned))));
        mux.add_domain(&domain).expect("register test domain");

        let target_registration = mux
            .capture_pane_registration(&target)
            .expect("target registration");
        let target_guard = target_registration
            .operation_guard(&mux)
            .expect("target operation admission");
        assert!(target_registration.detach_local_if_current());
        assert!(mux.get_pane(target.pane_id()).is_none());

        let receipt = promise::spawn::block_on(mux.split_pane_spawned(
            target_guard,
            SplitRequest::default(),
            None,
            None,
            SpawnTabDomain::CurrentPaneDomain,
            None,
        ))
        .expect("exact target authority must survive registry detach");

        assert_eq!(receipt.pane_id(), spawned.pane_id());
        assert_eq!(receipt.tab_id(), target_tab.tab_id());
        assert_eq!(receipt.window_id(), target_window_id);
        receipt.with_pane(|pane| assert!(std::ptr::eq(pane, spawned.as_ref())));
        assert!(target_tab
            .iter_all_panes()
            .iter()
            .any(|pane| Arc::ptr_eq(pane, &target)));
        assert!(target_tab
            .iter_all_panes()
            .iter()
            .any(|pane| Arc::ptr_eq(pane, &spawned)));
        assert_eq!(target_kills.load(Ordering::SeqCst), 0);
        assert_eq!(spawned_kills.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn guarded_moved_split_survives_both_registry_detaches_after_admission() {
        let global_guard = global_test_lock();
        let mux = Arc::new(Mux::new(None));
        let (target, target_kills) = KillCountingPane::new(170, test_size());
        let (source, source_kills) = KillCountingPane::new(171, test_size());
        let (target_tab, target_window_id) =
            register_attached_test_pane(&global_guard, &mux, &target);
        let (source_tab, _source_window_id) =
            register_attached_test_pane(&global_guard, &mux, &source);
        let domain: Arc<dyn Domain> = Arc::new(GuardedMutationTestDomain::new(None));
        mux.add_domain(&domain).expect("register test domain");

        let target_registration = mux
            .capture_pane_registration(&target)
            .expect("target registration");
        let source_registration = mux
            .capture_pane_registration(&source)
            .expect("source registration");
        let target_guard = target_registration
            .operation_guard(&mux)
            .expect("target operation admission");
        let source_guard = source_registration
            .operation_guard(&mux)
            .expect("source operation admission");
        assert!(target_registration.detach_local_if_current());
        assert!(source_registration.detach_local_if_current());

        let receipt = promise::spawn::block_on(mux.split_pane_moved(
            target_guard,
            source_guard,
            SplitRequest::default(),
            SpawnTabDomain::CurrentPaneDomain,
            None,
        ))
        .expect("exact source and target authority must survive registry detach");

        assert_eq!(receipt.pane_id(), source.pane_id());
        assert_eq!(receipt.tab_id(), target_tab.tab_id());
        assert_eq!(receipt.window_id(), target_window_id);
        assert!(receipt
            .registration()
            .same_registration(&source_registration));
        receipt.with_pane(|pane| assert!(std::ptr::eq(pane, source.as_ref())));
        assert!(mux.get_tab(source_tab.tab_id()).is_none());
        assert!(target_tab
            .iter_all_panes()
            .iter()
            .any(|pane| Arc::ptr_eq(pane, &target)));
        assert!(target_tab
            .iter_all_panes()
            .iter()
            .any(|pane| Arc::ptr_eq(pane, &source)));
        assert_eq!(target_kills.load(Ordering::SeqCst), 0);
        assert_eq!(source_kills.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn guarded_move_to_new_tab_never_reauthorizes_a_detached_registry_slot() {
        let global_guard = global_test_lock();
        let mux = Arc::new(Mux::new(None));
        let (pane, kills) = KillCountingPane::new(172, test_size());
        let (source_tab, _source_window_id) =
            register_attached_test_pane(&global_guard, &mux, &pane);
        let domain: Arc<dyn Domain> = Arc::new(GuardedMutationTestDomain::new(None));
        mux.add_domain(&domain).expect("register test domain");

        let registration = mux
            .capture_pane_registration(&pane)
            .expect("move registration");
        let guard = registration
            .operation_guard(&mux)
            .expect("move operation admission");
        assert!(registration.detach_local_if_current());
        assert!(mux.get_pane(pane.pane_id()).is_none());
        mux.prune_dead_windows();
        assert!(
            source_tab
                .iter_all_panes()
                .iter()
                .any(|current| Arc::ptr_eq(current, &pane)),
            "pruning must retain the detached pane while its move guard is live"
        );

        let receipt = promise::spawn::block_on(mux.move_pane_to_new_tab_guarded(
            guard,
            None,
            Some("guarded-move-test".to_string()),
            None,
        ))
        .expect("exact move authority must survive registry detach");

        assert_eq!(receipt.pane_id(), pane.pane_id());
        assert!(receipt.registration().same_registration(&registration));
        receipt.with_pane(|current| assert!(std::ptr::eq(current, pane.as_ref())));
        assert!(mux.get_tab(source_tab.tab_id()).is_none());
        assert!(mux.get_tab(receipt.tab_id()).is_some_and(|tab| {
            tab.iter_all_panes()
                .iter()
                .any(|current| Arc::ptr_eq(current, &pane))
        }));
        assert!(
            mux.get_pane(pane.pane_id()).is_none(),
            "commit must not reconstruct registry authority from the raw pane ID"
        );
        assert_eq!(kills.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn pane_registration_handle_preserves_originating_mux_after_global_replacement() {
        let _guard = global_test_lock();
        Mux::shutdown();

        let originating_mux = Arc::new(Mux::new(None));
        let replacement_mux = Arc::new(Mux::new(None));
        let (originating_pane, _) = KillCountingPane::new(132, test_size());
        let (replacement_pane, _) = KillCountingPane::new(132, test_size());

        originating_mux
            .add_pane(&originating_pane)
            .expect("origin registration");
        replacement_mux
            .add_pane(&replacement_pane)
            .expect("replacement mux should register its distinct same-ID pane");

        let handle = originating_mux
            .capture_pane_registration(&originating_pane)
            .expect("origin handle");

        let origin_outputs = Arc::new(AtomicUsize::new(0));
        let observed_origin = Arc::clone(&origin_outputs);
        originating_mux
            .subscribe(move |event| {
                if matches!(event, MuxNotification::PaneOutput(132)) {
                    observed_origin.fetch_add(1, Ordering::SeqCst);
                }
                true
            })
            .expect("origin subscription");

        let replacement_outputs = Arc::new(AtomicUsize::new(0));
        let observed_replacement = Arc::clone(&replacement_outputs);
        replacement_mux
            .subscribe(move |event| {
                if matches!(event, MuxNotification::PaneOutput(132)) {
                    observed_replacement.fetch_add(1, Ordering::SeqCst);
                }
                true
            })
            .expect("replacement subscription");

        Mux::set_mux(&originating_mux);
        Mux::set_mux(&replacement_mux);

        assert_eq!(
            handle.try_with_current_output(|output| {
                assert_eq!(output.pane_id(), 132);
                output.perform_actions(vec![Action::Print('x')]);
            }),
            Some(()),
        );
        originating_mux.flush_pending_pane_output_notifications();

        assert_eq!(origin_outputs.load(Ordering::SeqCst), 1);
        assert_eq!(replacement_outputs.load(Ordering::SeqCst), 0);
        assert!(
            Mux::try_get().is_some_and(|mux| Arc::ptr_eq(&mux, &replacement_mux)),
            "the handle must not replace or consult the global mux",
        );

        Mux::shutdown();
    }

    #[test]
    fn panicking_handle_output_closure_releases_lease_before_removal() {
        let _guard = global_test_lock();
        Mux::shutdown();

        let mux = Arc::new(Mux::new(None));
        let (pane, kills) = KillCountingPane::new(133, test_size());
        let events = Arc::new(Mutex::new(Vec::new()));
        let observed = Arc::clone(&events);
        let observed_kills = Arc::clone(&kills);

        mux.subscribe(move |event| {
            match event {
                MuxNotification::PaneOutput(133) => {
                    assert_eq!(observed_kills.load(Ordering::SeqCst), 0);
                    observed.lock().push("output");
                }
                MuxNotification::PaneRemoved(133) => {
                    assert_eq!(observed_kills.load(Ordering::SeqCst), 1);
                    observed.lock().push("removed");
                }
                _ => {}
            }
            true
        })
        .expect("subscription");

        mux.add_pane(&pane).expect("registration");
        let handle = mux
            .capture_pane_registration(&pane)
            .expect("registration handle");

        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = handle.try_with_current_output::<()>(|output| {
                output.perform_actions(vec![Action::Print('x')]);
                panic!("intentional output-closure panic");
            });
        }));
        assert!(result.is_err());
        assert_eq!(
            handle.active_operation_count(),
            0,
            "unwind must release the output operation lease",
        );

        mux.remove_pane_if_same(133, &pane);
        mux.flush_pending_pane_output_notifications();

        assert_eq!(&*events.lock(), &["output", "removed"]);
        assert_eq!(kills.load(Ordering::SeqCst), 1);
        assert!(mux.pending_pane_output.lock().queued.is_empty());
        assert!(mux.pending_pane_output.lock().notifications.is_empty());
        assert!(mux.pending_pane_lifecycle.lock().by_pane.is_empty());
        assert!(mux.pending_pane_lifecycle.lock().retirements.is_empty());

        Mux::shutdown();
    }

    #[test]
    fn pane_registration_handle_runs_external_output_closure_without_mux_locks() {
        let _guard = global_test_lock();
        Mux::shutdown();

        let mux = Arc::new(Mux::new(None));
        let (pane, _) = KillCountingPane::new(134, test_size());
        mux.add_pane(&pane).expect("registration");
        let handle = mux
            .capture_pane_registration(&pane)
            .expect("registration handle");

        assert_eq!(
            handle.try_with_current_output(|output| {
                assert!(mux.pane_registration.try_lock().is_some());
                assert!(mux.panes.try_write().is_some());
                assert!(mux.pending_pane_output.try_lock().is_some());
                assert!(mux.pending_pane_lifecycle.try_lock().is_some());
                assert!(mux.retiring_pane_ids.try_lock().is_some());
                assert!(mux.subscribers.try_write().is_some());
                output.perform_actions(vec![Action::Print('x')]);
            }),
            Some(()),
        );

        mux.flush_pending_pane_output_notifications();
        Mux::shutdown();
    }

    #[test]
    fn pane_registration_handle_does_not_retain_pane() {
        let mux = Arc::new(Mux::new(None));
        let (pane, kills) = KillCountingPane::new(135, test_size());
        let weak_pane = Arc::downgrade(&pane);

        mux.add_pane(&pane).expect("registration");
        let handle = mux
            .capture_pane_registration(&pane)
            .expect("registration handle");

        assert!(handle.detach_local_if_current());
        assert!(mux.get_pane(135).is_none());
        assert_eq!(
            kills.load(Ordering::SeqCst),
            0,
            "local detach must never call Pane::kill",
        );

        drop(pane);
        assert!(
            weak_pane.upgrade().is_none(),
            "the surviving handle must contain no strong pane reference",
        );
        assert_eq!(handle.try_with_current(|_| ()), None);
    }

    #[test]
    fn pane_registration_handle_fails_closed_after_different_arc_replacement() {
        let mux = Arc::new(Mux::new(None));
        let (original, original_kills) = KillCountingPane::new(136, test_size());
        let (replacement, replacement_kills) = KillCountingPane::new(136, test_size());

        mux.add_pane(&original).expect("original registration");
        let stale = mux
            .capture_pane_registration(&original)
            .expect("original handle");
        assert!(stale.retire_if_current());
        assert_eq!(original_kills.load(Ordering::SeqCst), 1);

        mux.add_pane(&replacement)
            .expect("different Arc replacement");
        let current = mux
            .capture_pane_registration(&replacement)
            .expect("replacement handle");

        assert!(!stale.same_registration(&current));
        assert_eq!(stale.try_with_current(|_| ()), None);
        assert_eq!(stale.try_with_current_output(|_| ()), None);
        assert!(!stale.retire_if_current());
        assert_eq!(replacement_kills.load(Ordering::SeqCst), 0);
        assert_eq!(current.try_with_current(|pane| pane.pane_id()), Some(136),);
    }

    #[test]
    fn pane_registration_slot_rebind_does_not_retarget_queued_clone() {
        let mux = Arc::new(Mux::new(None));
        let (pane, _) = KillCountingPane::new(137, test_size());
        let slot = Arc::new(PaneRegistrationSlot::default());

        mux.add_pane(&pane).expect("original registration");
        let original = mux
            .capture_pane_registration(&pane)
            .expect("original handle");
        slot.reserve(original.clone())
            .expect("unbound slot accepts original reservation")
            .commit()
            .expect("original reservation commits")
            .finalize();
        let queued = slot.load().expect("queued work captures original handle");

        assert!(original.detach_local_if_current());
        mux.add_pane(&pane).expect("same Arc replacement");
        let replacement = mux
            .capture_pane_registration(&pane)
            .expect("replacement handle");
        slot.reserve(replacement.clone())
            .expect("retired slot accepts replacement reservation")
            .commit()
            .expect("replacement reservation commits")
            .finalize();

        assert!(!queued.same_registration(&replacement));
        assert_eq!(queued.try_with_current(|_| ()), None);
        assert!(
            slot.load()
                .is_some_and(|bound| bound.same_registration(&replacement)),
            "rebinding changes only future admissions",
        );
    }

    #[test]
    fn pane_registration_slot_rejects_a_second_live_mux_owner() {
        let first_mux = Arc::new(Mux::new(None));
        let second_mux = Arc::new(Mux::new(None));
        let (pane, _) = KillCountingPane::new(151, test_size());

        first_mux
            .add_pane(&pane)
            .expect("first mux should publish the pane");
        let err = second_mux
            .add_pane(&pane)
            .expect_err("same pane object must not acquire a second live mux owner");

        assert!(err
            .to_string()
            .contains("already bound to a live or draining mux registration"));
        assert!(second_mux.get_pane(151).is_none());
        assert!(
            first_mux
                .capture_pane_registration(&pane)
                .is_some_and(|registration| {
                    registration.try_with_current(|pane| pane.pane_id()) == Some(151)
                }),
            "the rejected publication must leave the first owner authoritative",
        );
    }

    #[test]
    fn pane_registration_slot_rejects_rebind_after_owner_destruction_without_cleanup() {
        let first_mux = Arc::new(Mux::new(None));
        let weak_first_mux = Arc::downgrade(&first_mux);
        let second_mux = Arc::new(Mux::new(None));
        let (pane, kills) = KillCountingPane::new(155, test_size());

        first_mux
            .add_pane(&pane)
            .expect("first mux should publish the pane");
        let stale = first_mux
            .capture_pane_registration(&pane)
            .expect("first registration handle");

        drop(first_mux);
        assert!(
            weak_first_mux.upgrade().is_none(),
            "the first owner must be destroyed before the pane is rebound",
        );
        assert_eq!(stale.try_with_current(|_| ()), None);
        assert_eq!(stale.try_with_current_output(|_| ()), None);
        assert!(!stale.retire_if_current());

        let err = second_mux
            .add_pane(&pane)
            .expect_err("owner death cannot prove detached subscriber work is quiescent");
        assert!(err
            .to_string()
            .contains("already bound to a live or draining mux registration"));
        assert!(second_mux.get_pane(155).is_none());
        assert_eq!(
            kills.load(Ordering::SeqCst),
            0,
            "rejecting an unproven rebind must not kill the retained pane",
        );
    }

    #[test]
    fn pane_slot_rebind_waits_for_claimed_cleanup_after_owner_destruction() {
        let _guard = global_test_lock();
        let executor = BoundedTestExecutor::new();
        let first_mux = Arc::new(Mux::new(None));
        let weak_first_mux = Arc::downgrade(&first_mux);
        let second_mux = Arc::new(Mux::new(None));
        let (pane, kills) = KillCountingPane::new(156, test_size());

        first_mux
            .add_pane(&pane)
            .expect("first mux should publish the pane");
        let stale = first_mux
            .capture_pane_registration(&pane)
            .expect("first registration handle");
        let generation = Arc::clone(
            &first_mux
                .panes
                .read()
                .get(&156)
                .expect("first generation should be live")
                .generation,
        );
        let admitted = generation
            .try_acquire()
            .expect("the live generation should admit one operation");

        first_mux.remove_pane_if_same_generation(156, &pane, &generation);
        assert_eq!(kills.load(Ordering::SeqCst), 0);

        std::thread::spawn(move || drop(admitted))
            .join()
            .expect("operation release should not panic");
        drop(first_mux);
        assert!(weak_first_mux.upgrade().is_none());

        let err = second_mux
            .add_pane(&pane)
            .expect_err("a claimed old-generation kill must fence rebinding");
        assert!(err
            .to_string()
            .contains("already bound to a live or draining mux registration"));
        assert_eq!(kills.load(Ordering::SeqCst), 0);

        executor.run_until(Duration::from_secs(30), || {
            kills.load(Ordering::SeqCst) == 1
        });
        second_mux
            .add_pane(&pane)
            .expect("completed old-generation cleanup should release the slot");
        let current = second_mux
            .capture_pane_registration(&pane)
            .expect("replacement registration handle");

        assert!(!stale.same_registration(&current));
        assert_eq!(stale.try_with_current(|_| ()), None);
        assert!(!stale.retire_if_current());
        assert_eq!(kills.load(Ordering::SeqCst), 1);
        assert_eq!(current.try_with_current(|pane| pane.pane_id()), Some(156),);
    }

    #[test]
    fn pane_slot_rebind_remains_fenced_after_full_owner_field_teardown() {
        let first_mux = Arc::new(Mux::new(None));
        let weak_first_mux = Arc::downgrade(&first_mux);
        let second_mux = Arc::new(Mux::new(None));
        let (pane, kills) = KillCountingPane::new(157, test_size());
        let (drop_entered_tx, drop_entered_rx) = std::sync::mpsc::sync_channel(1);
        let (drop_release_tx, drop_release_rx) = std::sync::mpsc::sync_channel(1);
        let teardown_latch = BlockingDropLatch {
            entered: drop_entered_tx,
            release: StdMutex::new(drop_release_rx),
        };

        first_mux
            .subscribe(move |_| {
                let _keep_latch_alive = &teardown_latch;
                true
            })
            .expect("teardown latch subscriber should register");
        first_mux
            .add_pane(&pane)
            .expect("first mux should publish the pane");

        let drop_thread = std::thread::spawn(move || drop(first_mux));
        drop_entered_rx
            .recv_timeout(Duration::from_secs(30))
            .expect("owner teardown should reach the blocking subscriber");
        assert!(
            weak_first_mux.upgrade().is_none(),
            "Arc weak death is visible before owner field teardown completes",
        );

        let err = second_mux
            .add_pane(&pane)
            .expect_err("weak-owner death alone must not authorize rebinding");
        assert!(err
            .to_string()
            .contains("already bound to a live or draining mux registration"));
        assert_eq!(kills.load(Ordering::SeqCst), 0);

        drop_release_tx
            .send(())
            .expect("owner teardown latch should release");
        drop_thread.join().expect("owner teardown should not panic");
        let err = second_mux
            .add_pane(&pane)
            .expect_err("owner field teardown cannot prove detached work is quiescent");
        assert!(err
            .to_string()
            .contains("already bound to a live or draining mux registration"));
        assert_eq!(kills.load(Ordering::SeqCst), 0);
        assert!(second_mux.get_pane(157).is_none());
    }

    #[test]
    fn unfinalized_pane_registration_commit_rolls_back_the_slot() {
        let mux = Arc::new(Mux::new(None));
        let (pane, _) = KillCountingPane::new(153, test_size());
        mux.add_pane(&pane).expect("test pane registration");
        let registration = mux
            .capture_pane_registration(&pane)
            .expect("test registration handle");
        let slot = Arc::new(PaneRegistrationSlot::default());

        let commit = slot
            .reserve(registration.clone())
            .expect("unbound test slot accepts reservation")
            .commit()
            .expect("test reservation commits");
        assert!(slot
            .load()
            .is_some_and(|bound| bound.same_registration(&registration)));
        drop(commit);
        assert!(
            slot.load().is_none(),
            "dropping an unfinalized commit must hide the unpublished handle",
        );
    }

    #[test]
    fn pane_slot_rebind_waits_until_removed_subscribers_finish() {
        let first_mux = Arc::new(Mux::new(None));
        let second_mux = Arc::new(Mux::new(None));
        let (pane, _) = KillCountingPane::new(154, test_size());
        first_mux
            .add_pane(&pane)
            .expect("first mux should publish the pane");
        let registration = first_mux
            .capture_pane_registration(&pane)
            .expect("first registration handle");

        let attempted = Arc::new(AtomicBool::new(false));
        let rejected = Arc::new(AtomicBool::new(false));
        let attempted_for_subscriber = Arc::clone(&attempted);
        let rejected_for_subscriber = Arc::clone(&rejected);
        let second_mux_for_subscriber = Arc::clone(&second_mux);
        let pane_for_subscriber = Arc::clone(&pane);
        first_mux
            .subscribe(move |notification| {
                if matches!(notification, MuxNotification::PaneRemoved(154)) {
                    attempted_for_subscriber.store(true, Ordering::SeqCst);
                    rejected_for_subscriber.store(
                        second_mux_for_subscriber
                            .add_pane(&pane_for_subscriber)
                            .is_err(),
                        Ordering::SeqCst,
                    );
                }
                true
            })
            .expect("test subscriber identifier");

        assert!(registration.retire_if_current());
        assert!(attempted.load(Ordering::SeqCst));
        assert!(
            rejected.load(Ordering::SeqCst),
            "old removal subscribers must finish before another mux can bind the pane",
        );
        second_mux
            .add_pane(&pane)
            .expect("completed removal lifecycle should release the pane slot");
    }

    #[test]
    fn failed_pane_preparation_releases_its_slot_reservation() {
        let mux = Arc::new(Mux::new(None));
        let (pane, _) = KillCountingPane::new_with_reader(
            152,
            test_size(),
            Some(Box::new(std::io::empty())),
            false,
        );

        mux.fail_next_pane_reader_preparation(PaneReaderPreparationFault::Socketpair);
        let err = mux
            .add_pane(&pane)
            .expect_err("injected preparation failure should reject publication");
        assert!(err
            .to_string()
            .contains("injected pane reader socketpair failure"));
        assert!(mux.get_pane(152).is_none());

        mux.add_pane(&pane)
            .expect("dropped reservation must permit a later publication");
        assert!(mux.get_pane(152).is_some());
    }

    #[test]
    fn pane_registration_handle_does_not_retain_mux() {
        let handle = {
            let mux = Arc::new(Mux::new(None));
            let weak_mux = Arc::downgrade(&mux);
            let (pane, _) = KillCountingPane::new(138, test_size());
            mux.add_pane(&pane).expect("registration");
            let handle = mux
                .capture_pane_registration(&pane)
                .expect("registration handle");
            assert!(handle.detach_local_if_current());
            drop(pane);
            drop(mux);
            assert!(
                weak_mux.upgrade().is_none(),
                "a surviving handle must not keep its owner mux alive",
            );
            handle
        };

        assert_eq!(handle.try_with_current(|_| ()), None);
    }

    #[test]
    fn delayed_tab_close_stays_bound_to_origin_registration_across_mux_swap() {
        let _guard = global_test_lock();
        Mux::shutdown();

        let origin = Arc::new(Mux::new(None));
        let replacement_mux = Arc::new(Mux::new(None));
        let (origin_pane, origin_kills) = KillCountingPane::new(139, test_size());
        let (foreign_pane, foreign_kills) = KillCountingPane::new(139, test_size());
        let tab = Arc::new(Tab::new(&test_size()));
        tab.assign_pane(&origin_pane);
        origin
            .add_tab_and_active_pane(&tab)
            .expect("origin tab registration");
        replacement_mux
            .add_pane(&foreign_pane)
            .expect("independent mux may reuse the process-local pane value");
        let delayed = origin
            .capture_pane_registration(&origin_pane)
            .expect("pre-confirmation registration handle");

        Mux::set_mux(&replacement_mux);
        assert!(tab.kill_pane_registration(&delayed));
        assert_eq!(origin_kills.load(Ordering::SeqCst), 1);
        assert_eq!(foreign_kills.load(Ordering::SeqCst), 0);
        assert!(origin.get_pane(139).is_none());
        assert!(
            replacement_mux
                .get_pane(139)
                .is_some_and(|pane| Arc::ptr_eq(&pane, &foreign_pane)),
            "the current singleton's same-ID pane must remain untouched",
        );

        let (origin_successor, successor_kills) = KillCountingPane::new(139, test_size());
        tab.assign_pane(&origin_successor);
        origin
            .add_pane(&origin_successor)
            .expect("origin may publish a later same-ID registration");

        assert!(!tab.kill_pane_registration(&delayed));
        assert_eq!(successor_kills.load(Ordering::SeqCst), 0);
        assert!(tab.contains_pane(139));
        assert!(origin
            .get_pane(139)
            .is_some_and(|pane| Arc::ptr_eq(&pane, &origin_successor)),);

        Mux::shutdown();
    }

    #[test]
    fn delayed_exact_tab_removal_uses_origin_mux_after_global_swap() {
        let _guard = global_test_lock();
        Mux::shutdown();

        let origin = Arc::new(Mux::new(None));
        let replacement_mux = Arc::new(Mux::new(None));
        let origin_tab = Arc::new(Tab::new(&test_size()));
        let (origin_pane, _) = KillCountingPane::new(195, test_size());
        origin_tab.assign_pane(&origin_pane);
        let replacement_tab = Arc::new(Tab::new(&test_size()));
        let witness = origin
            .add_tab_and_active_pane(&origin_tab)
            .expect("origin tab registration")
            .expect("new origin pane registration witness");
        replacement_mux
            .add_tab_no_panes(&replacement_tab)
            .expect("replacement tab registration");

        Mux::set_mux(&replacement_mux);
        assert!(origin.remove_tab_if_same(&origin_tab, &witness));
        assert!(origin.get_tab(origin_tab.tab_id()).is_none());
        assert!(
            replacement_mux
                .get_tab(replacement_tab.tab_id())
                .is_some_and(|tab| Arc::ptr_eq(&tab, &replacement_tab)),
            "exact removal through the captured mux must preserve the global replacement's tab",
        );
        assert!(
            !origin.remove_tab_if_same(&origin_tab, &witness),
            "a stale exact tab Arc must not acquire removal authority twice",
        );

        let (successor_pane, successor_kills) = KillCountingPane::new(195, test_size());
        let detached_origin = origin_tab
            .remove_pane(195)
            .expect("removed tab retains its prior pane tree until explicitly repurposed");
        assert!(Arc::ptr_eq(&detached_origin, &origin_pane));
        origin_tab.assign_pane(&successor_pane);
        let successor_witness = origin
            .add_tab_and_active_pane(&origin_tab)
            .expect("same Arc tab may be registered as a later generation")
            .expect("successor pane registration witness");
        assert!(
            !origin.remove_tab_if_same(&origin_tab, &witness),
            "the old witness must not remove a later registration of the same Arc<Tab>",
        );
        assert!(origin
            .get_tab(origin_tab.tab_id())
            .is_some_and(|tab| Arc::ptr_eq(&tab, &origin_tab)));
        assert_eq!(successor_kills.load(Ordering::SeqCst), 0);
        assert!(origin.remove_tab_if_same(&origin_tab, &successor_witness));
        assert_eq!(successor_kills.load(Ordering::SeqCst), 1);

        Mux::shutdown();
    }

    #[test]
    fn exact_tab_removal_uses_callback_free_structural_census() {
        let _guard = global_test_lock();
        Mux::shutdown();

        let mux = Arc::new(Mux::new(None));
        let tab = Arc::new(Tab::new(&test_size()));
        let (pane, kills, pane_id_calls) =
            KillCountingPane::new_with_pane_id_counter(198, test_size());
        tab.assign_pane(&pane);
        let witness = mux
            .add_tab_and_active_pane(&tab)
            .expect("tab registration")
            .expect("pane registration witness");
        let calls_before_removal = pane_id_calls.load(Ordering::SeqCst);

        assert!(mux.remove_tab_if_same(&tab, &witness));
        assert_eq!(kills.load(Ordering::SeqCst), 1);
        assert_eq!(
            pane_id_calls.load(Ordering::SeqCst),
            calls_before_removal,
            "tab retirement must resolve registered pane IDs without invoking pane callbacks",
        );

        let ordinary_tab = Arc::new(Tab::new(&test_size()));
        let (ordinary_pane, ordinary_kills, ordinary_pane_id_calls) =
            KillCountingPane::new_with_pane_id_counter(199, test_size());
        ordinary_tab.assign_pane(&ordinary_pane);
        mux.add_tab_and_active_pane(&ordinary_tab)
            .expect("ordinary tab registration")
            .expect("ordinary pane registration witness");
        let ordinary_calls_before_removal = ordinary_pane_id_calls.load(Ordering::SeqCst);

        assert!(mux.remove_tab(ordinary_tab.tab_id()).is_some());
        assert_eq!(ordinary_kills.load(Ordering::SeqCst), 1);
        assert_eq!(
            ordinary_pane_id_calls.load(Ordering::SeqCst),
            ordinary_calls_before_removal,
            "ordinary tab retirement must use the same callback-free structural census",
        );

        Mux::shutdown();
    }

    #[test]
    fn delayed_exact_window_removal_rejects_replaced_contents() {
        let _guard = global_test_lock();
        Mux::shutdown();

        let mux = Arc::new(Mux::new(None));
        let origin_tab = Arc::new(Tab::new(&test_size()));
        let (origin_pane, origin_kills) = KillCountingPane::new(196, test_size());
        origin_tab.assign_pane(&origin_pane);
        let origin_witness = mux
            .add_tab_and_active_pane(&origin_tab)
            .expect("origin tab registration")
            .expect("origin pane registration witness");

        let replacement_tab = Arc::new(Tab::new(&test_size()));
        let (replacement_pane, replacement_kills) = KillCountingPane::new(197, test_size());
        replacement_tab.assign_pane(&replacement_pane);
        mux.add_tab_and_active_pane(&replacement_tab)
            .expect("replacement tab registration")
            .expect("replacement pane registration witness");

        let window = mux.new_empty_window(None, None);
        let window_id = *window;
        mux.add_tab_to_window(&origin_tab, window_id)
            .expect("origin tab window attachment");
        drop(window);

        let holding_window = mux.new_empty_window(None, None);
        let holding_window_id = *holding_window;
        mux.move_tab_between_windows(origin_tab.tab_id(), holding_window_id, None)
            .expect("move the origin tab without retiring its registration");
        mux.add_tab_to_window(&replacement_tab, window_id)
            .expect("replacement contents fit in origin window");

        assert!(
            !mux.kill_window_if_contains_exact_tab(window_id, &origin_tab, &origin_witness),
            "a delayed close must not kill a window after its exact originating tab left",
        );
        assert!(mux.get_window(window_id).is_some_and(|window| {
            window
                .iter()
                .any(|candidate| Arc::ptr_eq(candidate, &replacement_tab))
        }));
        assert_eq!(origin_kills.load(Ordering::SeqCst), 0);
        assert_eq!(replacement_kills.load(Ordering::SeqCst), 0);

        mux.move_tab_between_windows(origin_tab.tab_id(), window_id, None)
            .expect("origin tab may be moved back to its still-live window");
        assert!(mux.kill_window_if_contains_exact_tab(window_id, &origin_tab, &origin_witness));
        assert!(mux.get_window(window_id).is_none());
        assert_eq!(origin_kills.load(Ordering::SeqCst), 1);
        assert_eq!(replacement_kills.load(Ordering::SeqCst), 1);

        drop(holding_window);
        Mux::shutdown();
    }

    #[test]
    fn exact_window_retirement_rejects_reentrant_and_concurrent_tab_reattachment() {
        let _guard = global_test_lock();
        Mux::shutdown();

        let mux = Arc::new(Mux::new(None));
        let doomed_tab = Arc::new(Tab::new(&test_size()));
        let (doomed_pane, doomed_kills) = KillCountingPane::new(200, test_size());
        doomed_tab.assign_pane(&doomed_pane);
        let doomed_witness = mux
            .add_tab_and_active_pane(&doomed_tab)
            .expect("doomed tab registration")
            .expect("doomed pane registration witness");

        let survivor_tab = Arc::new(Tab::new(&test_size()));
        let (survivor_pane, survivor_kills) = KillCountingPane::new(201, test_size());
        survivor_tab.assign_pane(&survivor_pane);
        mux.add_tab_and_active_pane(&survivor_tab)
            .expect("survivor tab registration")
            .expect("survivor pane registration witness");

        let doomed_window = mux.new_empty_window(None, None);
        let doomed_window_id = *doomed_window;
        mux.add_tab_to_window(&doomed_tab, doomed_window_id)
            .expect("doomed window attachment");
        drop(doomed_window);

        let survivor_window = mux.new_empty_window(None, None);
        let survivor_window_id = *survivor_window;
        mux.add_tab_to_window(&survivor_tab, survivor_window_id)
            .expect("survivor window attachment");
        drop(survivor_window);

        let callback_ran = Arc::new(AtomicBool::new(false));
        let reattach_succeeded = Arc::new(AtomicBool::new(false));
        let concurrent_reattach_succeeded = Arc::new(AtomicBool::new(false));
        let callback_mux = Arc::clone(&mux);
        let callback_tab = Arc::clone(&doomed_tab);
        let observed_callback = Arc::clone(&callback_ran);
        let observed_reattach = Arc::clone(&reattach_succeeded);
        let observed_concurrent_reattach = Arc::clone(&concurrent_reattach_succeeded);
        doomed_pane
            .downcast_ref::<KillCountingPane>()
            .expect("test pane concrete type")
            .on_domain_id
            .lock()
            .replace(Box::new(move || {
                observed_callback.store(true, Ordering::SeqCst);
                if callback_mux
                    .add_tab_to_window(&callback_tab, survivor_window_id)
                    .is_ok()
                {
                    observed_reattach.store(true, Ordering::SeqCst);
                }
                let concurrent_mux = Arc::clone(&callback_mux);
                let concurrent_tab = Arc::clone(&callback_tab);
                let concurrent_reattached = std::thread::spawn(move || {
                    concurrent_mux
                        .add_tab_to_window(&concurrent_tab, survivor_window_id)
                        .is_ok()
                })
                .join()
                .expect("concurrent reattachment probe must not panic");
                observed_concurrent_reattach.store(concurrent_reattached, Ordering::SeqCst);
            }));

        assert!(mux.kill_window_if_contains_exact_tab(
            doomed_window_id,
            &doomed_tab,
            &doomed_witness,
        ));
        assert!(callback_ran.load(Ordering::SeqCst));
        assert!(
            !reattach_succeeded.load(Ordering::SeqCst),
            "all doomed tab registrations must retire before pane callbacks can re-enter the mux",
        );
        assert!(
            !concurrent_reattach_succeeded.load(Ordering::SeqCst),
            "a concurrent caller must not reattach a doomed tab after structural retirement",
        );
        assert_eq!(doomed_kills.load(Ordering::SeqCst), 1);
        assert_eq!(survivor_kills.load(Ordering::SeqCst), 0);
        assert!(mux.get_tab(doomed_tab.tab_id()).is_none());
        assert!(mux.get_pane(200).is_none());
        assert!(mux.get_window(survivor_window_id).is_some_and(|window| {
            window.len() == 1
                && window
                    .iter()
                    .any(|candidate| Arc::ptr_eq(candidate, &survivor_tab))
        }));

        Mux::shutdown();
    }

    #[test]
    fn ordinary_window_kill_retires_every_owned_tab_and_pane() {
        let _guard = global_test_lock();
        Mux::shutdown();

        let mux = Arc::new(Mux::new(None));
        let first_tab = Arc::new(Tab::new(&test_size()));
        let (first_pane, first_kills, first_pane_id_calls) =
            KillCountingPane::new_with_pane_id_counter(202, test_size());
        first_tab.assign_pane(&first_pane);
        mux.add_tab_and_active_pane(&first_tab)
            .expect("first tab registration")
            .expect("first pane registration witness");

        let second_tab = Arc::new(Tab::new(&test_size()));
        let (second_pane, second_kills, second_pane_id_calls) =
            KillCountingPane::new_with_pane_id_counter(203, test_size());
        second_tab.assign_pane(&second_pane);
        mux.add_tab_and_active_pane(&second_tab)
            .expect("second tab registration")
            .expect("second pane registration witness");

        let window = mux.new_empty_window(None, None);
        let window_id = *window;
        mux.add_tab_to_window(&first_tab, window_id)
            .expect("first tab window attachment");
        mux.add_tab_to_window(&second_tab, window_id)
            .expect("second tab window attachment");
        drop(window);
        let first_calls_before_removal = first_pane_id_calls.load(Ordering::SeqCst);
        let second_calls_before_removal = second_pane_id_calls.load(Ordering::SeqCst);

        mux.kill_window(window_id);

        assert!(mux.get_window(window_id).is_none());
        assert!(mux.get_tab(first_tab.tab_id()).is_none());
        assert!(mux.get_tab(second_tab.tab_id()).is_none());
        assert!(mux.get_pane(202).is_none());
        assert!(mux.get_pane(203).is_none());
        assert_eq!(first_kills.load(Ordering::SeqCst), 1);
        assert_eq!(second_kills.load(Ordering::SeqCst), 1);
        assert_eq!(
            first_pane_id_calls.load(Ordering::SeqCst),
            first_calls_before_removal,
            "window retirement must not invoke pane-id callbacks for its first tab",
        );
        assert_eq!(
            second_pane_id_calls.load(Ordering::SeqCst),
            second_calls_before_removal,
            "window retirement must freeze sibling tabs through the same callback-free census",
        );

        Mux::shutdown();
    }

    #[test]
    fn exhausted_parent_window_rejects_ordinary_and_witnessed_tab_retirement_atomically() {
        let _guard = global_test_lock();
        Mux::shutdown();

        let mux = Arc::new(Mux::new(None));
        let tab = Arc::new(Tab::new(&test_size()));
        let (pane, kills) = KillCountingPane::new(206, test_size());
        tab.assign_pane(&pane);
        let witness = mux
            .add_tab_and_active_pane(&tab)
            .expect("tab registration")
            .expect("pane witness");
        let window = mux.new_empty_window(None, None);
        let window_id = *window;
        mux.add_tab_to_window(&tab, window_id)
            .expect("tab window attachment");
        drop(window);
        mux.get_window_mut(window_id)
            .expect("window remains registered")
            .set_order_revision_for_test(WindowOrderRevision::new(u64::MAX - 1));

        assert!(
            !mux.remove_tab_if_same(&tab, &witness),
            "witnessed retirement must fail before changing any registry",
        );
        assert!(
            mux.remove_tab(tab.tab_id()).is_none(),
            "ordinary retirement must obey the same fail-closed preflight",
        );
        assert!(mux
            .get_tab(tab.tab_id())
            .is_some_and(|registered| Arc::ptr_eq(&registered, &tab)));
        assert!(mux
            .get_pane(206)
            .is_some_and(|registered| Arc::ptr_eq(&registered, &pane)));
        assert!(mux.get_window(window_id).is_some_and(|window| {
            window.order_revision().get() == u64::MAX - 1
                && window.iter().any(|candidate| Arc::ptr_eq(candidate, &tab))
        }));
        assert_eq!(kills.load(Ordering::SeqCst), 0);

        Mux::shutdown();
    }

    #[test]
    fn exhausted_parent_window_rejects_empty_tab_retirement_atomically() {
        let _guard = global_test_lock();
        Mux::shutdown();

        let mux = Arc::new(Mux::new(None));
        let tab = Arc::new(Tab::new(&test_size()));
        mux.add_tab_no_panes(&tab).expect("empty tab registration");
        let window = mux.new_empty_window(None, None);
        let window_id = *window;
        mux.add_tab_to_window(&tab, window_id)
            .expect("empty tab window attachment");
        drop(window);
        mux.get_window_mut(window_id)
            .expect("window remains registered")
            .set_order_revision_for_test(WindowOrderRevision::new(u64::MAX - 1));

        assert!(
            !mux.remove_empty_tab_local_only_if_same(&tab),
            "empty-tab cleanup must reject before removing the tab registry entry",
        );
        assert!(mux
            .get_tab(tab.tab_id())
            .is_some_and(|registered| Arc::ptr_eq(&registered, &tab)));
        assert!(mux.get_window(window_id).is_some_and(|window| {
            window.order_revision().get() == u64::MAX - 1
                && window.iter().any(|candidate| Arc::ptr_eq(candidate, &tab))
        }));

        Mux::shutdown();
    }

    #[test]
    fn exhausted_duplicate_parent_rejects_window_retirement_before_any_commit() {
        let _guard = global_test_lock();
        Mux::shutdown();

        let mux = Arc::new(Mux::new(None));
        let tab = Arc::new(Tab::new(&test_size()));
        let (pane, kills) = KillCountingPane::new(207, test_size());
        tab.assign_pane(&pane);
        let witness = mux
            .add_tab_and_active_pane(&tab)
            .expect("tab registration")
            .expect("pane witness");

        let doomed = mux.new_empty_window(None, None);
        let doomed_id = *doomed;
        mux.add_tab_to_window(&tab, doomed_id)
            .expect("canonical parent attachment");
        drop(doomed);

        let duplicate = mux.new_empty_window(None, None);
        let duplicate_id = *duplicate;
        {
            let mut windows = mux.windows.write();
            let duplicate = windows
                .get_mut(&duplicate_id)
                .expect("duplicate-parent fixture window");
            duplicate
                .push(&tab)
                .expect("a standalone window cannot detect another parent");
            duplicate.set_order_revision_for_test(WindowOrderRevision::new(u64::MAX - 1));
        }
        drop(duplicate);

        assert!(
            !mux.kill_window_if_contains_exact_tab(doomed_id, &tab, &witness),
            "an exhausted surviving duplicate parent must reject the whole retirement",
        );
        assert!(mux.get_window(doomed_id).is_some());
        assert!(mux.get_window(duplicate_id).is_some_and(|window| {
            window.order_revision().get() == u64::MAX - 1
                && window.iter().any(|candidate| Arc::ptr_eq(candidate, &tab))
        }));
        assert!(mux
            .get_tab(tab.tab_id())
            .is_some_and(|registered| Arc::ptr_eq(&registered, &tab)));
        assert!(mux
            .get_pane(207)
            .is_some_and(|registered| Arc::ptr_eq(&registered, &pane)));
        assert_eq!(kills.load(Ordering::SeqCst), 0);

        Mux::shutdown();
    }

    #[test]
    fn exhausted_stale_parent_prune_is_non_panicking_and_zero_mutation() {
        let _guard = global_test_lock();
        Mux::shutdown();

        let mux = Arc::new(Mux::new(None));
        let tab = Arc::new(Tab::new(&test_size()));
        mux.add_tab_no_panes(&tab).expect("stale tab registration");
        let window = mux.new_empty_window(None, None);
        let window_id = *window;
        mux.add_tab_to_window(&tab, window_id)
            .expect("stale tab window attachment");
        drop(window);
        assert!(mux.remove_tab_registration_if_same(tab.tab_id(), &tab));
        mux.get_window_mut(window_id)
            .expect("stale parent remains registered")
            .set_order_revision_for_test(WindowOrderRevision::new(u64::MAX - 1));

        mux.prune_dead_windows();

        assert!(mux.get_tab(tab.tab_id()).is_none());
        assert!(mux.get_window(window_id).is_some_and(|window| {
            window.order_revision().get() == u64::MAX - 1
                && window.iter().any(|candidate| Arc::ptr_eq(candidate, &tab))
        }));

        Mux::shutdown();
    }

    #[test]
    fn stale_parent_prune_commits_while_retaining_provisional_census_guard() {
        let _guard = global_test_lock();
        Mux::shutdown();

        let mux = Arc::new(Mux::new(None));
        let tab = Arc::new(Tab::new(&test_size()));
        mux.add_tab_no_panes(&tab).expect("stale tab registration");
        let window = mux.new_empty_window(None, None);
        let window_id = *window;
        mux.add_tab_to_window(&tab, window_id)
            .expect("stale tab window attachment");
        drop(window);
        assert!(mux.remove_tab_registration_if_same(tab.tab_id(), &tab));

        mux.prune_dead_windows();

        assert!(mux.get_tab(tab.tab_id()).is_none());
        assert!(
            mux.get_window(window_id).is_none(),
            "stale-parent pruning must detach the stale tab and retire its emptied window",
        );

        Mux::shutdown();
    }

    #[test]
    fn window_retirement_publishes_parent_before_panes_with_distinct_revisions() {
        let _guard = global_test_lock();
        Mux::shutdown();

        let mux = Arc::new(Mux::new(None));
        let tab = Arc::new(Tab::new(&test_size()));
        let (pane, kills) = KillCountingPane::new(208, test_size());
        tab.assign_pane(&pane);
        mux.add_tab_and_active_pane(&tab)
            .expect("tab registration")
            .expect("pane witness");
        let window = mux.new_empty_window(None, None);
        let window_id = *window;
        mux.add_tab_to_window(&tab, window_id)
            .expect("tab window attachment");
        drop(window);
        let observed = Arc::new(Mutex::new(Vec::new()));
        let observed_for_subscriber = Arc::clone(&observed);
        mux.subscribe_with_topology(move |envelope| {
            let kind = match envelope.notification {
                MuxNotification::WindowTopologyChanged(change)
                    if change.removed_windows().binary_search(&window_id).is_ok() =>
                {
                    Some("window")
                }
                MuxNotification::PaneRemoved(208) => Some("pane"),
                _ => None,
            };
            if let Some(kind) = kind {
                let MuxTopologyStamp::Revision(revision) = envelope.topology else {
                    panic!("retirement topology event must carry a live revision");
                };
                observed_for_subscriber.lock().push((kind, revision.get()));
            }
            true
        })
        .expect("topology subscription");

        mux.kill_window(window_id);

        let observed = observed.lock();
        assert_eq!(observed.len(), 2);
        assert_eq!(observed[0].0, "window");
        assert_eq!(observed[1].0, "pane");
        assert!(
            observed[0].1 < observed[1].1,
            "parent removal must reserve and publish before child removal: {:?}",
            *observed,
        );
        assert!(mux.get_window(window_id).is_none());
        assert!(mux.get_tab(tab.tab_id()).is_none());
        assert!(mux.get_pane(208).is_none());
        assert_eq!(kills.load(Ordering::SeqCst), 1);

        Mux::shutdown();
    }

    #[test]
    fn window_retirement_completes_cleanup_after_pane_domain_callback_panic() {
        let _guard = global_test_lock();
        Mux::shutdown();

        let mux = Arc::new(Mux::new(None));
        let tab = Arc::new(Tab::new(&test_size()));
        let (pane, kills) = KillCountingPane::new(204, test_size());
        tab.assign_pane(&pane);
        mux.add_tab_and_active_pane(&tab)
            .expect("panic test tab registration")
            .expect("panic test pane registration witness");

        let window = mux.new_empty_window(None, None);
        let window_id = *window;
        mux.add_tab_to_window(&tab, window_id)
            .expect("panic test window attachment");
        drop(window);

        pane.downcast_ref::<KillCountingPane>()
            .expect("test pane concrete type")
            .on_domain_id
            .lock()
            .replace(Box::new(|| {
                panic!("intentional pane domain callback panic during window retirement");
            }));

        mux.kill_window(window_id);

        assert!(mux.get_window(window_id).is_none());
        assert!(mux.get_tab(tab.tab_id()).is_none());
        assert!(mux.get_pane(204).is_none());
        assert_eq!(
            kills.load(Ordering::SeqCst),
            1,
            "a recovered domain callback panic must not strand pane retirement",
        );

        Mux::shutdown();
    }

    #[test]
    fn window_retirement_completes_cleanup_after_domain_detach_panic() {
        let _guard = global_test_lock();
        Mux::shutdown();

        let mux = Arc::new(Mux::new(None));
        let detaches = Arc::new(AtomicUsize::new(0));
        let domain: Arc<dyn Domain> = Arc::new(PanicDetachTestDomain {
            detaches: Arc::clone(&detaches),
        });
        mux.add_domain(&domain).expect("register panic test domain");

        let tab = Arc::new(Tab::new(&test_size()));
        let (pane, kills) = KillCountingPane::new(205, test_size());
        tab.assign_pane(&pane);
        mux.add_tab_and_active_pane(&tab)
            .expect("detach panic test tab registration")
            .expect("detach panic test pane registration witness");

        let window = mux.new_empty_window(None, None);
        let window_id = *window;
        mux.add_tab_to_window(&tab, window_id)
            .expect("detach panic test window attachment");
        drop(window);

        mux.kill_window(window_id);

        assert_eq!(detaches.load(Ordering::SeqCst), 1);
        assert!(mux.get_window(window_id).is_none());
        assert!(mux.get_tab(tab.tab_id()).is_none());
        assert!(mux.get_pane(205).is_none());
        assert_eq!(
            kills.load(Ordering::SeqCst),
            1,
            "a recovered domain detach panic must not strand pane retirement",
        );

        Mux::shutdown();
    }

    #[test]
    fn exact_title_updates_ignore_global_mux_and_allow_reentrant_getters() {
        let _guard = global_test_lock();
        Mux::shutdown();

        let origin = Arc::new(Mux::new(None));
        let replacement = Arc::new(Mux::new(None));
        let window = origin.new_empty_window(Some("titles".to_string()), None);
        let window_id = *window;
        let tab = Arc::new(Tab::new(&test_size()));
        origin
            .add_tab_no_panes(&tab)
            .expect("title test tab registration");
        origin
            .add_tab_to_window(&tab, window_id)
            .expect("title test window attachment");

        let origin_notifications = Arc::new(AtomicUsize::new(0));
        let observed_origin = Arc::clone(&origin_notifications);
        let weak_origin = Arc::downgrade(&origin);
        let tab_id = tab.tab_id();
        origin
            .subscribe(move |notification| {
                if matches!(
                    notification,
                    MuxNotification::WindowTitleChanged { .. }
                        | MuxNotification::TabTitleChanged { .. }
                ) {
                    let mux = weak_origin
                        .upgrade()
                        .expect("origin must remain live during callback");
                    assert!(mux.get_window(window_id).is_some());
                    assert!(mux.get_tab(tab_id).is_some());
                    observed_origin.fetch_add(1, Ordering::SeqCst);
                }
                true
            })
            .expect("origin subscription");

        let replacement_notifications = Arc::new(AtomicUsize::new(0));
        let observed_replacement = Arc::clone(&replacement_notifications);
        replacement
            .subscribe(move |notification| {
                if matches!(
                    notification,
                    MuxNotification::WindowTitleChanged { .. }
                        | MuxNotification::TabTitleChanged { .. }
                ) {
                    observed_replacement.fetch_add(1, Ordering::SeqCst);
                }
                true
            })
            .expect("replacement subscription");

        Mux::set_mux(&replacement);
        assert!(origin.set_window_title(window_id, "origin window"));
        assert!(origin.set_tab_title(tab_id, "origin tab"));
        assert_eq!(origin_notifications.load(Ordering::SeqCst), 2);
        assert_eq!(replacement_notifications.load(Ordering::SeqCst), 0);
        assert_eq!(
            origin
                .get_window(window_id)
                .expect("origin window")
                .get_title(),
            "origin window",
        );
        assert_eq!(tab.get_title(), "origin tab");

        drop(window);
        Mux::shutdown();
    }

    #[test]
    fn batch_registration_retirement_recomputes_pane_count_once() {
        let mux = Arc::new(Mux::new(None));
        let mut handles = Vec::new();
        let mut kill_counters = Vec::new();
        for pane_id in 142..150 {
            let (pane, kills) = KillCountingPane::new(pane_id, test_size());
            mux.add_pane(&pane).expect("batch pane registration");
            handles.push(
                mux.capture_pane_registration(&pane)
                    .expect("batch registration handle"),
            );
            kill_counters.push(kills);
        }
        mux.pane_count_recomputes.store(0, Ordering::Relaxed);

        assert_eq!(PaneRegistrationHandle::retire_batch_if_current(handles), 8,);
        assert_eq!(
            mux.pane_count_recomputes.load(Ordering::Relaxed),
            1,
            "one owner batch must scan topology only once",
        );
        for (offset, kills) in kill_counters.into_iter().enumerate() {
            let pane_id = 142 + offset;
            assert_eq!(kills.load(Ordering::SeqCst), 1);
            assert!(mux.get_pane(pane_id).is_none());
        }
    }

    #[test]
    fn stale_flush_local_output_cannot_suppress_same_arc_replacement_output() {
        let mux = Arc::new(Mux::new(None));
        let (pane, _) = KillCountingPane::new(103, test_size());
        mux.add_pane(&pane)
            .expect("initial pane generation should register");
        let old_generation = Arc::clone(
            &mux.panes
                .read()
                .get(&103)
                .expect("initial generation should be live")
                .generation,
        );
        let stale_batch = {
            let _registration = mux.pane_registration.lock();
            let lifecycle_notification = mux.enqueue_pane_lifecycle_notification_locked(
                PaneLifecycleNotification::Output(103),
                None,
            );
            let batch = PaneOutputBatch::new(
                103,
                Arc::clone(&old_generation),
                lifecycle_notification,
                0,
                false,
            );
            let mut pending = mux.pending_pane_output.lock();
            pending.queued.insert(103, Arc::clone(&batch));
            pending.notifications.push(Arc::clone(&batch));
            batch
        };

        mux.remove_pane_if_same_generation(103, &pane, &old_generation);
        mux.add_pane(&pane)
            .expect("same Arc should receive a replacement generation");
        let new_generation = Arc::clone(
            &mux.panes
                .read()
                .get(&103)
                .expect("replacement generation should be live")
                .generation,
        );
        let outputs = Arc::new(Mutex::new(Vec::new()));
        let observed_outputs = Arc::clone(&outputs);
        mux.subscribe(move |notification| {
            if let MuxNotification::PaneOutput(pane_id) = notification {
                observed_outputs.lock().push(pane_id);
            }
            true
        })
        .expect("test mux subscription should allocate an identifier");

        let new_batch = {
            let _registration = mux.pane_registration.lock();
            let lifecycle_notification = mux.enqueue_pane_lifecycle_notification_locked(
                PaneLifecycleNotification::Output(103),
                None,
            );
            let batch = PaneOutputBatch::new(
                103,
                Arc::clone(&new_generation),
                lifecycle_notification,
                0,
                false,
            );
            let mut pending = mux.pending_pane_output.lock();
            pending.queued.insert(103, Arc::clone(&batch));
            pending.notifications.push(Arc::clone(&batch));
            batch
        };
        assert!(
            mux.pending_pane_output
                .lock()
                .queued
                .get(&103)
                .is_some_and(|queued| Arc::ptr_eq(queued, &new_batch)),
            "replacement work must own the queued marker before delayed flush",
        );

        mux.flush_pending_pane_output_notifications();
        assert_eq!(&*outputs.lock(), &[103]);
        assert!(
            stale_batch.state.load(Ordering::Acquire) & PANE_OUTPUT_BATCH_SEALED != 0,
            "the delayed old batch remains harmlessly sealed",
        );
        assert!(mux.pending_pane_output.lock().queued.is_empty());
    }

    #[test]
    fn pane_reader_preparation_failures_are_prepublication_and_reusable() {
        let faults = [
            PaneReaderPreparationFault::Socketpair,
            PaneReaderPreparationFault::ParserSpawn,
            PaneReaderPreparationFault::ReaderSpawn,
            PaneReaderPreparationFault::ParserReady,
            PaneReaderPreparationFault::ReaderReady,
        ];

        for (offset, fault) in faults.iter().copied().enumerate() {
            let pane_id = 110 + offset;
            let mux = Arc::new(Mux::new(None));
            let reads = Arc::new(AtomicUsize::new(0));
            let (dropped_tx, dropped_rx) = std::sync::mpsc::channel();
            let reader = CancellationObservingReader {
                reads: Arc::clone(&reads),
                dropped_tx: Some(dropped_tx),
            };
            let (pane, _) = KillCountingPane::new_with_reader(
                pane_id,
                test_size(),
                Some(Box::new(reader)),
                false,
            );
            mux.fail_next_pane_reader_preparation(fault);

            let error = mux
                .add_pane(&pane)
                .expect_err("injected preparation fault must reject registration");
            assert!(
                error.to_string().contains("injected pane"),
                "unexpected {:?} diagnostic: {:#}",
                fault,
                error,
            );
            dropped_rx
                .recv_timeout(Duration::from_secs(30))
                .expect("failed preparation must drop the unread reader");
            assert_eq!(reads.load(Ordering::SeqCst), 0);
            assert!(mux.get_pane(pane_id).is_none());
            assert!(!mux.pane_preparations.lock().contains_key(&pane_id));
            let pending_lifecycle = mux.pending_pane_lifecycle.lock();
            assert!(
                pending_lifecycle.by_pane.is_empty() && pending_lifecycle.retirements.is_empty(),
                "failed preparation must not enqueue PaneAdded",
            );
            drop(pending_lifecycle);

            mux.add_pane(&pane)
                .expect("failed prepublication generation must leave the ID reusable");
            assert!(mux.get_pane(pane_id).is_some());
        }
    }

    #[test]
    fn ready_reader_workers_cannot_read_until_pane_added_callback_finishes() {
        let mux = Arc::new(Mux::new(None));
        let reads = Arc::new(AtomicUsize::new(0));
        let (dropped_tx, dropped_rx) = std::sync::mpsc::channel();
        let reader = CancellationObservingReader {
            reads: Arc::clone(&reads),
            dropped_tx: Some(dropped_tx),
        };
        let (pane, _) =
            KillCountingPane::new_with_reader(120, test_size(), Some(Box::new(reader)), false);
        let (added_entered_tx, added_entered_rx) = std::sync::mpsc::channel();
        let (release_added_tx, release_added_rx) = std::sync::mpsc::channel();
        let release_added_rx = Arc::new(Mutex::new(release_added_rx));
        mux.subscribe({
            let release_added_rx = Arc::clone(&release_added_rx);
            move |notification| {
                if matches!(notification, MuxNotification::PaneAdded(120)) {
                    let _ = added_entered_tx.send(());
                    release_added_rx
                        .lock()
                        .recv_timeout(Duration::from_secs(30))
                        .expect("test should release blocked PaneAdded subscriber");
                }
                true
            }
        })
        .expect("test mux subscription should allocate an identifier");

        let mux_for_add = Arc::clone(&mux);
        let pane_for_add = Arc::clone(&pane);
        let add_thread = std::thread::spawn(move || mux_for_add.add_pane(&pane_for_add));
        added_entered_rx
            .recv_timeout(Duration::from_secs(30))
            .expect("PaneAdded subscriber should block registration");
        assert_eq!(
            reads.load(Ordering::SeqCst),
            0,
            "both workers are ready but the outer reader must remain gated",
        );

        release_added_tx
            .send(())
            .expect("release PaneAdded subscriber");
        add_thread
            .join()
            .expect("registration thread should not panic")
            .expect("registration should succeed");
        dropped_rx
            .recv_timeout(Duration::from_secs(30))
            .expect("released EOF reader should finish");
        assert_eq!(reads.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn pane_added_reentrant_removal_cancels_both_ready_workers() {
        let mux = Arc::new(Mux::new(None));
        let reads = Arc::new(AtomicUsize::new(0));
        let (dropped_tx, dropped_rx) = std::sync::mpsc::channel();
        let reader = CancellationObservingReader {
            reads: Arc::clone(&reads),
            dropped_tx: Some(dropped_tx),
        };
        let (pane, _) =
            KillCountingPane::new_with_reader(121, test_size(), Some(Box::new(reader)), false);
        let events = Arc::new(Mutex::new(Vec::new()));
        let events_for_subscriber = Arc::clone(&events);
        let mux_for_subscriber = Arc::clone(&mux);
        mux.subscribe(move |notification| {
            match notification {
                MuxNotification::PaneAdded(121) => {
                    events_for_subscriber.lock().push("added");
                    mux_for_subscriber.remove_pane(121);
                }
                MuxNotification::PaneRemoved(121) => {
                    events_for_subscriber.lock().push("removed");
                }
                _ => {}
            }
            true
        })
        .expect("test mux subscription should allocate an identifier");

        mux.add_pane(&pane)
            .expect("publication succeeds even when subscriber immediately removes it");
        dropped_rx
            .recv_timeout(Duration::from_secs(30))
            .expect("cancelled workers must drop their unread reader");
        assert_eq!(reads.load(Ordering::SeqCst), 0);
        assert!(mux.get_pane(121).is_none());
        assert_eq!(&*events.lock(), &["added", "removed"]);
        assert_eq!(
            pane.downcast_ref::<KillCountingPane>()
                .expect("test pane should retain its concrete type")
                .binds
                .load(Ordering::SeqCst),
            0,
            "a PaneAdded subscriber that retires the generation must suppress its stale bind hook"
        );
    }

    #[test]
    fn standalone_shared_mux_reader_delivers_output_and_exact_eof_cleanup() {
        let _guard = global_test_lock();
        Mux::shutdown();
        let executor = BoundedTestExecutor::new();
        let mux = Arc::new(Mux::new(None));
        let reader = std::io::Cursor::new(b"x".to_vec());
        let (pane, _) =
            KillCountingPane::new_with_reader(122, test_size(), Some(Box::new(reader)), false);
        let actions = Arc::clone(
            &pane
                .downcast_ref::<KillCountingPane>()
                .expect("test pane concrete type")
                .actions,
        );
        let (output_tx, output_rx) = std::sync::mpsc::channel();
        let (removed_tx, removed_rx) = std::sync::mpsc::channel();
        mux.subscribe(move |notification| {
            match notification {
                MuxNotification::PaneOutput(122) => {
                    let _ = output_tx.send(());
                }
                MuxNotification::PaneRemoved(122) => {
                    let _ = removed_tx.send(());
                }
                _ => {}
            }
            true
        })
        .expect("test mux subscription should allocate an identifier");

        mux.add_pane(&pane)
            .expect("standalone shared mux should register a reader pane");
        executor.run_until(Duration::from_secs(30), || output_rx.try_recv().is_ok());
        assert_eq!(actions.load(Ordering::SeqCst), 1);

        executor.run_until(Duration::from_secs(30), || removed_rx.try_recv().is_ok());
        assert!(mux.get_pane(122).is_none());
        Mux::shutdown();
    }

    #[test]
    fn global_replacement_between_ready_and_release_cannot_redirect_reader() {
        let _guard = global_test_lock();
        Mux::shutdown();
        let executor = BoundedTestExecutor::new();
        let originating_mux = Arc::new(Mux::new(None));
        let replacement_mux = Arc::new(Mux::new(None));
        Mux::set_mux(&originating_mux);

        let reader = std::io::Cursor::new(b"o".to_vec());
        let (originating_pane, _) =
            KillCountingPane::new_with_reader(123, test_size(), Some(Box::new(reader)), false);
        let (replacement_pane, _) = KillCountingPane::new(123, test_size());

        let (added_entered_tx, added_entered_rx) = std::sync::mpsc::channel();
        let (release_added_tx, release_added_rx) = std::sync::mpsc::channel();
        let release_added_rx = Arc::new(Mutex::new(release_added_rx));
        let (origin_output_tx, origin_output_rx) = std::sync::mpsc::channel();
        originating_mux
            .subscribe({
                let release_added_rx = Arc::clone(&release_added_rx);
                move |notification| {
                    match notification {
                        MuxNotification::PaneAdded(123) => {
                            let _ = added_entered_tx.send(());
                            release_added_rx
                                .lock()
                                .recv_timeout(Duration::from_secs(30))
                                .expect("test should release PaneAdded callback");
                        }
                        MuxNotification::PaneOutput(123) => {
                            let _ = origin_output_tx.send(());
                        }
                        _ => {}
                    }
                    true
                }
            })
            .expect("originating mux subscription should allocate an identifier");

        let mux_for_add = Arc::clone(&originating_mux);
        let pane_for_add = Arc::clone(&originating_pane);
        let add_thread = std::thread::spawn(move || mux_for_add.add_pane(&pane_for_add));
        added_entered_rx
            .recv_timeout(Duration::from_secs(30))
            .expect("origin PaneAdded callback should pause release");
        replacement_mux
            .add_pane(&replacement_pane)
            .expect("replacement mux should register its distinct same-ID pane");
        Mux::set_mux(&replacement_mux);
        release_added_tx
            .send(())
            .expect("release originating PaneAdded callback");
        add_thread
            .join()
            .expect("origin registration thread should not panic")
            .expect("origin registration should succeed");

        executor.run_until(Duration::from_secs(30), || {
            origin_output_rx.try_recv().is_ok()
        });
        executor.run_until(Duration::from_secs(30), || {
            originating_mux.get_pane(123).is_none()
        });
        assert!(originating_mux.get_pane(123).is_none());
        assert!(
            replacement_mux
                .get_pane(123)
                .is_some_and(|pane| Arc::ptr_eq(&pane, &replacement_pane)),
            "originating cleanup must preserve the replacement mux pane",
        );
        Mux::shutdown();
    }

    #[test]
    fn same_instance_preparation_conflict_is_typed_and_retryable() {
        let mux = Arc::new(Mux::new(None));
        let (pane, _) = KillCountingPane::new(76, test_size());
        let preparation_claim = mux
            .claim_pane_preparation(&pane)
            .expect("first preparation claim should succeed")
            .expect("an unregistered pane should require preparation");

        let err = match mux.claim_pane_preparation(&pane) {
            Err(err) => err,
            Ok(_) => panic!("same-instance concurrent preparation must report busy"),
        };
        assert_eq!(
            err.downcast_ref::<PanePreparationInProgress>(),
            Some(&PanePreparationInProgress { pane_id: 76 })
        );
        assert!(mux.get_pane(76).is_none());
        drop(preparation_claim);
    }

    #[test]
    fn removal_cancels_only_the_claimed_generation_before_pane_publication() {
        let mux = Arc::new(Mux::new(None));
        let (pane, kills, reader_entered, release_reader) = pane_with_blocked_reader(86);
        let mux_for_add = Arc::clone(&mux);
        let pane_for_add = Arc::clone(&pane);
        let (result_tx, result_rx) = std::sync::mpsc::channel();
        let add_thread = std::thread::spawn(move || {
            let result = mux_for_add.add_pane(&pane_for_add);
            result_tx
                .send(result)
                .expect("test should still be waiting for registration result");
        });

        reader_entered
            .recv_timeout(Duration::from_secs(30))
            .expect("pane preparation should reach its blocking reader callback");
        mux.remove_pane(86);
        let retry_while_cancelled = match mux.claim_pane_preparation(&pane) {
            Err(err) => err,
            Ok(_) => panic!("cancelled preparation must retain its claim until it unwinds"),
        };
        assert_eq!(
            retry_while_cancelled.downcast_ref::<PanePreparationInProgress>(),
            Some(&PanePreparationInProgress { pane_id: 86 })
        );
        release_reader
            .send(())
            .expect("blocked reader callback should still be waiting");

        let err = result_rx
            .recv_timeout(Duration::from_secs(30))
            .expect("cancelled registration should finish")
            .expect_err("the stale preparation generation must not publish");
        assert_eq!(
            err.downcast_ref::<PanePreparationCancelled>(),
            Some(&PanePreparationCancelled { pane_id: 86 })
        );
        add_thread
            .join()
            .expect("registration thread should not panic");
        assert!(
            mux.pane_preparations.lock().is_empty(),
            "the cancelled preparation tombstone must retire with its exact claim",
        );
        assert!(
            mux.get_pane(86).is_none(),
            "removal must fence stale publication"
        );
        assert_eq!(kills.load(Ordering::SeqCst), 0);

        let replacement_claim = mux
            .claim_pane_preparation(&pane)
            .expect("the same pane may retry after cancelled work has unwound")
            .expect("the cancelled pane was never published");
        drop(replacement_claim);
        assert!(mux.pane_preparations.lock().is_empty());
    }

    #[test]
    fn exact_instance_removal_does_not_cancel_a_different_preparation() {
        let mux = Arc::new(Mux::new(None));
        let (pane, kills, reader_entered, release_reader) = pane_with_blocked_reader(87);
        let (different_instance, different_kills) = KillCountingPane::new(87, test_size());
        let mux_for_add = Arc::clone(&mux);
        let pane_for_add = Arc::clone(&pane);
        let (result_tx, result_rx) = std::sync::mpsc::channel();
        let add_thread = std::thread::spawn(move || {
            let result = mux_for_add.add_pane(&pane_for_add);
            result_tx
                .send(result)
                .expect("test should still be waiting for registration result");
        });

        reader_entered
            .recv_timeout(Duration::from_secs(30))
            .expect("pane preparation should reach its blocking reader callback");
        mux.remove_pane_if_same(87, &different_instance);
        release_reader
            .send(())
            .expect("blocked reader callback should still be waiting");
        result_rx
            .recv_timeout(Duration::from_secs(30))
            .expect("uncancelled registration should finish")
            .expect("identity-mismatched removal must not cancel preparation");
        add_thread
            .join()
            .expect("registration thread should not panic");

        let registered = mux
            .get_pane(87)
            .expect("the exact claimed pane should be published");
        assert!(Arc::ptr_eq(&registered, &pane));
        assert_eq!(kills.load(Ordering::SeqCst), 0);
        assert_eq!(different_kills.load(Ordering::SeqCst), 0);
        assert!(mux.pane_preparations.lock().is_empty());
    }

    #[test]
    fn pane_lifecycle_observers_follow_serialized_topology_order() {
        let mux = Arc::new(Mux::new(None));
        let observed = Arc::new(Mutex::new(Vec::new()));
        let observed_for_subscriber = Arc::clone(&observed);
        let mux_for_subscriber = Arc::downgrade(&mux);
        mux.subscribe(move |notification| {
            let transition = match notification {
                MuxNotification::PaneAdded(89) => Some("added"),
                MuxNotification::PaneRemoved(89) => Some("removed"),
                _ => None,
            };
            if let Some(transition) = transition {
                let mux = mux_for_subscriber
                    .upgrade()
                    .expect("test mux should outlive its subscriber");
                assert!(
                    mux.pane_registration.try_lock().is_some(),
                    "lifecycle observers must run outside pane_registration"
                );
                assert!(
                    mux.subscribers.try_write().is_some(),
                    "lifecycle observers must run outside the subscriber registry lock"
                );
                observed_for_subscriber.lock().push(transition);
            }
            true
        })
        .expect("test mux subscription should allocate an identifier");

        // Both mutators block in count recomputation after publishing their
        // lifecycle event. Count aggregation must not stall reader start or
        // later ordered lifecycle delivery.
        let pane_count_guard = mux.num_panes_by_workspace.read();
        let (pane, kills) = KillCountingPane::new(89, test_size());
        let mux_for_add = Arc::clone(&mux);
        let pane_for_add = Arc::clone(&pane);
        let (add_tx, add_rx) = std::sync::mpsc::channel();
        let add_thread = std::thread::spawn(move || {
            let result = mux_for_add.add_pane(&pane_for_add);
            add_tx
                .send(result)
                .expect("test should still be waiting for add result");
        });

        let publication_deadline = Instant::now() + Duration::from_secs(30);
        while mux.get_pane(89).is_none() {
            assert!(
                Instant::now() < publication_deadline,
                "add should publish before blocking in pane-count recomputation"
            );
            std::thread::yield_now();
        }
        // Registry insertion and lifecycle-ticket completion are separate
        // linearization points; the reader gate makes the latter authoritative
        // for external observation. Wait for that boundary explicitly.
        while observed.lock().as_slice() != ["added"] {
            assert!(
                Instant::now() < publication_deadline,
                "PaneAdded should publish before blocking in pane-count recomputation"
            );
            std::thread::yield_now();
        }
        assert_eq!(
            &*observed.lock(),
            &["added"],
            "PaneAdded must not wait behind count aggregation",
        );

        let mux_for_remove = Arc::clone(&mux);
        let (remove_tx, remove_rx) = std::sync::mpsc::channel();
        let remove_thread = std::thread::spawn(move || {
            mux_for_remove.remove_pane(89);
            remove_tx
                .send(())
                .expect("test should still be waiting for remove result");
        });
        let removal_deadline = Instant::now() + Duration::from_secs(30);
        while mux.get_pane(89).is_some() {
            assert!(
                Instant::now() < removal_deadline,
                "remove should mutate topology before blocking in pane-count recomputation"
            );
            std::thread::yield_now();
        }
        // Topology removal linearizes before the fallible/panicking Pane::kill
        // callback and before its lifecycle ticket becomes ready. Wait for the
        // distinct observer boundary rather than assuming that seeing the map
        // mutation means the removal thread has already completed both steps.
        while observed.lock().as_slice() != ["added", "removed"] {
            assert!(
                Instant::now() < removal_deadline,
                "PaneRemoved should publish before blocking in pane-count recomputation"
            );
            std::thread::yield_now();
        }
        assert_eq!(
            &*observed.lock(),
            &["added", "removed"],
            "a later removal must follow the earlier addition without waiting on count aggregation"
        );

        drop(pane_count_guard);
        add_rx
            .recv_timeout(Duration::from_secs(30))
            .expect("add should finish after pane-count barrier release")
            .expect("serialized add should succeed");
        remove_rx
            .recv_timeout(Duration::from_secs(30))
            .expect("remove should finish after pane-count barrier release");
        add_thread.join().expect("add thread should not panic");
        remove_thread
            .join()
            .expect("remove thread should not panic");

        assert_eq!(&*observed.lock(), &["added", "removed"]);
        assert!(mux.get_pane(89).is_none());
        assert_eq!(kills.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn panicking_pane_kill_cannot_wedge_later_lifecycle_delivery() {
        let mux = Arc::new(Mux::new(None));
        let observed = Arc::new(Mutex::new(Vec::new()));
        let observed_for_subscriber = Arc::clone(&observed);
        mux.subscribe(move |notification| {
            if let MuxNotification::PaneRemoved(pane_id) = notification {
                observed_for_subscriber.lock().push(pane_id);
            }
            true
        })
        .expect("test mux subscription should allocate an identifier");

        let (panicking, _) = KillCountingPane::new_with_kill_callback(89, test_size(), || {
            panic!("intentional pane kill panic");
        });
        let ordinary = register_test_pane(&mux, 90);
        mux.add_pane(&panicking)
            .expect("panicking test pane should register");

        mux.remove_pane_if_same(89, &panicking);
        mux.remove_pane_if_same(90, &ordinary);

        assert_eq!(
            &*observed.lock(),
            &[89, 90],
            "a panic must not leave an unready lifecycle ticket at the queue head",
        );
        let pending_lifecycle = mux.pending_pane_lifecycle.lock();
        assert!(pending_lifecycle.by_pane.is_empty());
        assert!(pending_lifecycle.retirements.is_empty());
    }

    #[test]
    fn same_id_replacement_is_fenced_through_removed_callback() {
        let mux = Arc::new(Mux::new(None));
        let replacement_attempted = Arc::new(AtomicBool::new(false));
        let replacement_rejected = Arc::new(AtomicBool::new(false));
        let replacement_attempted_from_kill = Arc::clone(&replacement_attempted);
        let replacement_rejected_from_kill = Arc::clone(&replacement_rejected);
        let replacement = KillCountingPane::new(91, test_size()).0;
        let replacement_from_kill = Arc::clone(&replacement);
        let mux_from_kill = Arc::clone(&mux);
        let (original, _) = KillCountingPane::new_with_kill_callback(91, test_size(), move || {
            replacement_attempted_from_kill.store(true, Ordering::SeqCst);
            replacement_rejected_from_kill.store(
                mux_from_kill.add_pane(&replacement_from_kill).is_err(),
                Ordering::SeqCst,
            );
        });
        mux.add_pane(&original)
            .expect("original test pane should register");

        let replacement_for_subscriber = Arc::clone(&replacement);
        let mux_for_subscriber = Arc::clone(&mux);
        mux.subscribe(move |notification| {
            if let MuxNotification::PaneRemoved(91) = notification {
                assert!(
                    mux_for_subscriber.get_pane(91).is_none(),
                    "PaneRemoved must not expose a same-id replacement",
                );
                assert!(
                    mux_for_subscriber
                        .add_pane(&replacement_for_subscriber)
                        .is_err(),
                    "the retiring ID must remain fenced through subscriber callbacks",
                );
            }
            true
        })
        .expect("test mux subscription should allocate an identifier");

        mux.remove_pane_if_same(91, &original);

        assert!(replacement_attempted.load(Ordering::SeqCst));
        assert!(replacement_rejected.load(Ordering::SeqCst));
        mux.add_pane(&replacement)
            .expect("same ID may register after the removal callback completes");
        assert!(mux
            .get_pane(91)
            .is_some_and(|pane| Arc::ptr_eq(&pane, &replacement)));
    }

    #[test]
    fn deferred_removed_cleanup_fences_replacement_after_subscriber_return() {
        let mux = Arc::new(Mux::new(None));
        let (original, _) = KillCountingPane::new(191, test_size());
        let (replacement, _) = KillCountingPane::new(191, test_size());
        let escaped_cleanup = Arc::new(Mutex::new(None));

        mux.add_pane(&original).expect("register original pane");
        assert!(
            mux.acquire_pane_removal_cleanup_lease(191).is_none(),
            "cleanup authority must not exist before PaneRemoved fanout",
        );

        let escaped_cleanup_for_subscriber = Arc::clone(&escaped_cleanup);
        mux.subscribe_with_pane_removal_cleanup(move |notification, cleanup| {
            if matches!(notification, MuxNotification::PaneRemoved(191)) {
                *escaped_cleanup_for_subscriber.lock() = cleanup;
            }
            true
        })
        .expect("test mux subscription should allocate an identifier");

        mux.remove_pane_if_same(191, &original);
        assert!(
            escaped_cleanup.lock().is_some(),
            "the subscriber must retain exact cleanup authority with its queued work",
        );
        let cleanup_snapshot = mux.pane_removal_cleanup_snapshot();
        assert_eq!(cleanup_snapshot.active_fences, 1);
        assert_eq!(cleanup_snapshot.outstanding_leases, 1);
        assert!(
            mux.acquire_pane_removal_cleanup_lease(191).is_none(),
            "new leases must not be minted after synchronous fanout closes",
        );
        assert!(
            mux.add_pane(&replacement).is_err(),
            "same-ID replacement must remain fenced after the subscriber returns",
        );

        escaped_cleanup
            .lock()
            .take()
            .expect("queued cleanup lease")
            .complete();
        assert_eq!(
            mux.pane_removal_cleanup_snapshot(),
            PaneRemovalCleanupSnapshot::default(),
            "completing the final exact lease must clear all cleanup pressure",
        );
        mux.add_pane(&replacement)
            .expect("replacement may register after deferred cleanup acknowledges completion");
    }

    #[test]
    fn batch_retirement_uses_the_same_deferred_cleanup_fence() {
        let mux = Arc::new(Mux::new(None));
        let (original, _) = KillCountingPane::new(195, test_size());
        let (replacement, _) = KillCountingPane::new(195, test_size());
        let escaped_cleanup = Arc::new(Mutex::new(None));

        mux.add_pane(&original).expect("register original pane");
        let registration = mux
            .capture_pane_registration(&original)
            .expect("capture the exact batch-retirement generation");
        let escaped_cleanup_for_subscriber = Arc::clone(&escaped_cleanup);
        mux.subscribe_with_pane_removal_cleanup(move |notification, cleanup| {
            if matches!(notification, MuxNotification::PaneRemoved(195)) {
                *escaped_cleanup_for_subscriber.lock() = cleanup;
            }
            true
        })
        .expect("test mux subscription should allocate an identifier");

        assert_eq!(
            PaneRegistrationHandle::retire_batch_if_current(vec![registration]),
            1,
            "the exact batch candidate must retire",
        );
        assert!(
            escaped_cleanup.lock().is_some(),
            "the hand-built batch removal notification must mint cleanup authority",
        );
        assert!(
            mux.add_pane(&replacement).is_err(),
            "batch retirement must retain the same-ID fence after subscriber return",
        );

        escaped_cleanup
            .lock()
            .take()
            .expect("batch cleanup lease")
            .complete();
        mux.add_pane(&replacement)
            .expect("batch-retired pane ID may be reused after deferred cleanup completes");
    }

    #[test]
    fn panicking_cleanup_subscriber_releases_its_lease_during_unwind() {
        let mux = Arc::new(Mux::new(None));
        let (original, _) = KillCountingPane::new(196, test_size());
        let (replacement, _) = KillCountingPane::new(196, test_size());

        mux.add_pane(&original).expect("register original pane");
        mux.subscribe_with_pane_removal_cleanup(|notification, cleanup| {
            if matches!(notification, MuxNotification::PaneRemoved(196)) {
                assert!(cleanup.is_some(), "panicking subscriber received its lease");
                panic!("intentional cleanup-subscriber unwind");
            }
            true
        })
        .expect("test mux subscription should allocate an identifier");

        mux.remove_pane_if_same(196, &original);
        mux.add_pane(&replacement).expect(
            "unwinding a cleanup subscriber must not strand its lease or the same-ID fence",
        );
    }

    #[test]
    fn final_deferred_removed_cleanup_lease_exclusively_releases_reuse_fence() {
        let mux = Arc::new(Mux::new(None));
        let (original, _) = KillCountingPane::new(192, test_size());
        let (replacement, _) = KillCountingPane::new(192, test_size());
        let cleanups = Arc::new(Mutex::new(Vec::new()));

        mux.add_pane(&original).expect("register original pane");
        for _ in 0..2 {
            let cleanups_for_subscriber = Arc::clone(&cleanups);
            mux.subscribe_with_pane_removal_cleanup(move |notification, cleanup| {
                if matches!(notification, MuxNotification::PaneRemoved(192)) {
                    cleanups_for_subscriber
                        .lock()
                        .push(cleanup.expect("PaneRemoved subscriber receives a cleanup lease"));
                }
                true
            })
            .expect("test mux subscription should allocate an identifier");
        }

        mux.remove_pane_if_same(192, &original);
        assert_eq!(cleanups.lock().len(), 2);

        cleanups.lock().remove(0).complete();
        assert!(
            mux.add_pane(&replacement).is_err(),
            "one completed window must not release another window's cleanup fence",
        );

        drop(cleanups.lock().remove(0));
        mux.add_pane(&replacement).expect(
            "dropping an abandoned final cleanup must release the fence without claiming success",
        );
    }

    #[test]
    fn old_mux_cleanup_token_cannot_fence_or_remove_same_id_in_new_mux() {
        let old_mux = Arc::new(Mux::new(None));
        let new_mux = Arc::new(Mux::new(None));
        let (old_pane, _) = KillCountingPane::new(193, test_size());
        let (new_pane, _) = KillCountingPane::new(193, test_size());
        let escaped_cleanup = Arc::new(Mutex::new(None));

        old_mux.add_pane(&old_pane).expect("register old pane");
        let escaped_cleanup_for_subscriber = Arc::clone(&escaped_cleanup);
        old_mux
            .subscribe_with_pane_removal_cleanup(move |notification, cleanup| {
                if matches!(notification, MuxNotification::PaneRemoved(193)) {
                    *escaped_cleanup_for_subscriber.lock() = cleanup;
                }
                true
            })
            .expect("old mux subscription should allocate an identifier");
        old_mux.remove_pane_if_same(193, &old_pane);

        new_mux
            .add_pane(&new_pane)
            .expect("a different mux owns an independent numeric namespace");
        escaped_cleanup
            .lock()
            .take()
            .expect("old cleanup lease")
            .complete();

        assert!(new_mux
            .get_pane(193)
            .is_some_and(|pane| Arc::ptr_eq(&pane, &new_pane)));
    }

    #[test]
    fn final_cleanup_lease_releases_same_arc_slot_after_mux_owner_destruction() {
        let old_mux = Arc::new(Mux::new(None));
        let weak_old_mux = Arc::downgrade(&old_mux);
        let new_mux = Arc::new(Mux::new(None));
        let (pane, _) = KillCountingPane::new(194, test_size());
        let escaped_cleanup = Arc::new(Mutex::new(None));

        old_mux
            .add_pane(&pane)
            .expect("register old pane generation");
        let escaped_cleanup_for_subscriber = Arc::clone(&escaped_cleanup);
        old_mux
            .subscribe_with_pane_removal_cleanup(move |notification, cleanup| {
                if matches!(notification, MuxNotification::PaneRemoved(194)) {
                    *escaped_cleanup_for_subscriber.lock() = cleanup;
                }
                true
            })
            .expect("old mux subscription should allocate an identifier");
        old_mux.remove_pane_if_same(194, &pane);
        drop(old_mux);
        assert!(weak_old_mux.upgrade().is_none());

        assert!(
            new_mux.add_pane(&pane).is_err(),
            "owner destruction must not bypass deferred cleanup authority",
        );
        escaped_cleanup
            .lock()
            .take()
            .expect("owner-independent cleanup token")
            .complete();
        new_mux
            .add_pane(&pane)
            .expect("the final lease must complete the old generation even after owner teardown");
    }

    #[test]
    fn reentrant_removal_keeps_same_id_fenced_until_queued_removed_dispatch() {
        let mux = Arc::new(Mux::new(None));
        let (original, kills) = KillCountingPane::new(92, test_size());
        let (replacement, _) = KillCountingPane::new(92, test_size());
        let first_add = Arc::new(AtomicBool::new(true));
        let replacement_rejected = Arc::new(AtomicBool::new(false));
        let observed = Arc::new(Mutex::new(Vec::new()));

        let mux_for_subscriber = Arc::clone(&mux);
        let original_for_subscriber = Arc::clone(&original);
        let replacement_for_subscriber = Arc::clone(&replacement);
        let first_add_for_subscriber = Arc::clone(&first_add);
        let rejected_for_subscriber = Arc::clone(&replacement_rejected);
        let observed_for_subscriber = Arc::clone(&observed);
        let kills_for_subscriber = Arc::clone(&kills);
        mux.subscribe(move |notification| {
            match notification {
                MuxNotification::PaneAdded(92)
                    if first_add_for_subscriber.swap(false, Ordering::SeqCst) =>
                {
                    observed_for_subscriber.lock().push("added");
                    mux_for_subscriber.remove_pane_if_same(92, &original_for_subscriber);
                    assert_eq!(
                        kills_for_subscriber.load(Ordering::SeqCst),
                        0,
                        "reentrant removal must not kill during PaneAdded fanout",
                    );
                    rejected_for_subscriber.store(
                        mux_for_subscriber
                            .add_pane(&replacement_for_subscriber)
                            .is_err(),
                        Ordering::SeqCst,
                    );
                }
                MuxNotification::PaneRemoved(92) => {
                    observed_for_subscriber.lock().push("removed");
                }
                _ => {}
            }
            true
        })
        .expect("test mux subscription should allocate an identifier");

        mux.add_pane(&original)
            .expect("original pane should publish before reentrant removal");

        assert!(
            replacement_rejected.load(Ordering::SeqCst),
            "completion during an active drain must not release the retiring ID early",
        );
        assert_eq!(&*observed.lock(), &["added", "removed"]);
        assert_eq!(kills.load(Ordering::SeqCst), 1);
        mux.add_pane(&replacement)
            .expect("replacement may register after queued PaneRemoved dispatch");
    }

    #[test]
    fn duplicate_unqualified_removal_cannot_release_an_inflight_fence() {
        let mux = Arc::new(Mux::new(None));
        let replacement: Arc<dyn Pane> = KillCountingPane::new(92, test_size()).0;
        let replacement_rejected = Arc::new(AtomicBool::new(false));
        let mux_from_kill = Arc::clone(&mux);
        let replacement_from_kill = Arc::clone(&replacement);
        let rejected_from_kill = Arc::clone(&replacement_rejected);
        let (original, _) = KillCountingPane::new_with_kill_callback(92, test_size(), move || {
            // This stale duplicate removal does not own the retirement
            // fence established by the outer removal.
            mux_from_kill.remove_pane(92);
            rejected_from_kill.store(
                mux_from_kill.add_pane(&replacement_from_kill).is_err(),
                Ordering::SeqCst,
            );
        });
        let original: Arc<dyn Pane> = original;
        mux.add_pane(&original).expect("register original pane");

        mux.remove_pane(92);

        assert!(
            replacement_rejected.load(Ordering::SeqCst),
            "a duplicate cleanup-only removal must not clear another removal's fence",
        );
        mux.add_pane(&replacement)
            .expect("fence should release after the authoritative Removed callback");
    }

    #[test]
    fn tab_removal_fences_ids_during_kill_and_removed_callbacks() {
        let mux = Arc::new(Mux::new(None));
        let (normal_replacement, _) = KillCountingPane::new(93, test_size());
        let (local_replacement, _) = KillCountingPane::new(94, test_size());
        let kill_rejected = Arc::new(AtomicBool::new(false));
        let normal_callback_rejected = Arc::new(AtomicBool::new(false));
        let local_callback_rejected = Arc::new(AtomicBool::new(false));

        let mux_for_kill = Arc::clone(&mux);
        let replacement_for_kill = Arc::clone(&normal_replacement);
        let kill_rejected_from_callback = Arc::clone(&kill_rejected);
        let (normal_pane, _) =
            KillCountingPane::new_with_kill_callback(93, test_size(), move || {
                kill_rejected_from_callback.store(
                    mux_for_kill.add_pane(&replacement_for_kill).is_err(),
                    Ordering::SeqCst,
                );
            });
        let normal_tab = Arc::new(Tab::new(&test_size()));
        normal_tab.assign_pane(&normal_pane);
        mux.add_tab_and_active_pane(&normal_tab)
            .expect("normal tab should register");

        let (local_pane, _) = KillCountingPane::new(94, test_size());
        let local_tab = Arc::new(Tab::new(&test_size()));
        local_tab.assign_pane(&local_pane);
        mux.add_tab_and_active_pane(&local_tab)
            .expect("local-only tab should register");

        let mux_for_subscriber = Arc::clone(&mux);
        let normal_replacement_for_subscriber = Arc::clone(&normal_replacement);
        let local_replacement_for_subscriber = Arc::clone(&local_replacement);
        let normal_rejected_for_subscriber = Arc::clone(&normal_callback_rejected);
        let local_rejected_for_subscriber = Arc::clone(&local_callback_rejected);
        mux.subscribe(move |notification| {
            match notification {
                MuxNotification::PaneRemoved(93) => {
                    normal_rejected_for_subscriber.store(
                        mux_for_subscriber
                            .add_pane(&normal_replacement_for_subscriber)
                            .is_err(),
                        Ordering::SeqCst,
                    );
                }
                MuxNotification::PaneRemoved(94) => {
                    local_rejected_for_subscriber.store(
                        mux_for_subscriber
                            .add_pane(&local_replacement_for_subscriber)
                            .is_err(),
                        Ordering::SeqCst,
                    );
                }
                _ => {}
            }
            true
        })
        .expect("test mux subscription should allocate an identifier");

        mux.remove_tab(normal_tab.tab_id())
            .expect("normal tab should be removed");
        mux.remove_tab_local_only(local_tab.tab_id())
            .expect("local-only tab should be removed");

        assert!(kill_rejected.load(Ordering::SeqCst));
        assert!(normal_callback_rejected.load(Ordering::SeqCst));
        assert!(local_callback_rejected.load(Ordering::SeqCst));
        mux.add_pane(&normal_replacement)
            .expect("normal replacement may register after PaneRemoved");
        mux.add_pane(&local_replacement)
            .expect("local replacement may register after PaneRemoved");
    }

    #[test]
    fn tab_removal_during_active_pane_preparation_fences_topology_publication() {
        let mux = Arc::new(Mux::new(None));
        let tab = Arc::new(Tab::new(&test_size()));
        let tab_id = tab.tab_id();
        let (pane, kills, reader_entered, release_reader) = pane_with_blocked_reader(88);
        tab.assign_pane(&pane);
        mux.add_tab_no_panes(&tab)
            .expect("test tab should be provisionally registered");
        let mux_for_add = Arc::clone(&mux);
        let tab_for_add = Arc::clone(&tab);
        let (result_tx, result_rx) = std::sync::mpsc::channel();
        let add_thread = std::thread::spawn(move || {
            let result = mux_for_add.add_tab_and_active_pane(&tab_for_add);
            result_tx
                .send(result)
                .expect("test should still be waiting for registration result");
        });

        reader_entered
            .recv_timeout(Duration::from_secs(30))
            .expect("pane preparation should reach its blocking reader callback");
        let removed_tab = mux
            .remove_tab(tab_id)
            .expect("exact provisionally registered tab should be removed");
        assert!(Arc::ptr_eq(&removed_tab, &tab));
        release_reader
            .send(())
            .expect("blocked reader callback should still be waiting");

        let err = result_rx
            .recv_timeout(Duration::from_secs(30))
            .expect("cancelled tab registration should finish")
            .expect_err("a cancelled active pane must prevent tab publication");
        assert_eq!(
            err.downcast_ref::<PanePreparationCancelled>(),
            Some(&PanePreparationCancelled { pane_id: 88 })
        );
        add_thread
            .join()
            .expect("registration thread should not panic");
        assert!(mux.get_pane(88).is_none());
        assert!(mux.get_tab(tab_id).is_none());
        assert_eq!(kills.load(Ordering::SeqCst), 0);
        assert!(mux.pane_preparations.lock().is_empty());
    }

    #[test]
    fn add_tab_does_not_publish_topology_when_its_pane_id_collides() {
        let mux = Arc::new(Mux::new(None));
        let first_tab = Arc::new(Tab::new(&test_size()));
        let (first, _) = KillCountingPane::new(78, test_size());
        first_tab.assign_pane(&first);
        mux.add_tab_and_active_pane(&first_tab)
            .expect("first tab and pane should register");

        let duplicate_tab = Arc::new(Tab::new(&test_size()));
        let duplicate_tab_id = duplicate_tab.tab_id();
        let (duplicate, _) = KillCountingPane::new(78, test_size());
        duplicate_tab.assign_pane(&duplicate);

        let err = mux
            .add_tab_and_active_pane(&duplicate_tab)
            .expect_err("colliding pane must reject its containing tab");
        assert!(err.downcast_ref::<PaneIdCollision>().is_some());
        assert!(
            mux.get_tab(duplicate_tab_id).is_none(),
            "a tab containing the rejected pane must not become observable"
        );
        let registered = mux
            .get_pane(78)
            .expect("first pane should remain registered");
        assert!(Arc::ptr_eq(&registered, &first));
    }

    #[test]
    fn add_tab_no_panes_rejects_a_different_instance_with_the_same_id() {
        let mux = Arc::new(Mux::new(None));
        let first = Arc::new(Tab::new(&test_size()));
        let duplicate = Arc::new(Tab::new(&test_size()));
        let colliding_id = duplicate.tab_id();
        mux.tabs.write().insert(colliding_id, Arc::clone(&first));

        let err = mux
            .add_tab_no_panes(&duplicate)
            .expect_err("a different tab instance must not replace the registered tab");
        assert!(err
            .to_string()
            .contains("already registered to a different tab instance"));
        let registered = mux
            .get_tab(colliding_id)
            .expect("first tab should remain registered");
        assert!(Arc::ptr_eq(&registered, &first));
    }

    #[test]
    fn add_tab_does_not_publish_topology_when_reader_acquisition_fails() {
        let mux = Arc::new(Mux::new(None));
        let tab = Arc::new(Tab::new(&test_size()));
        let tab_id = tab.tab_id();
        let (pane, _) = KillCountingPane::new_with_reader(79, test_size(), None, true);
        tab.assign_pane(&pane);

        let err = mux
            .add_tab_and_active_pane(&tab)
            .expect_err("reader acquisition failure must reject the tab and pane");
        assert!(err
            .to_string()
            .contains("intentional test pane reader acquisition failure"));
        assert!(mux.get_pane(79).is_none());
        assert!(mux.get_tab(tab_id).is_none());
    }

    #[test]
    fn add_tab_starts_reader_only_after_topology_and_pane_added_are_visible() {
        let mux = Arc::new(Mux::new(None));
        let tab = Arc::new(Tab::new(&test_size()));
        let tab_id = tab.tab_id();
        let pane_added = Arc::new(AtomicBool::new(false));
        let pane_added_for_subscriber = Arc::clone(&pane_added);
        mux.subscribe(move |notification| {
            if matches!(notification, MuxNotification::PaneAdded(80)) {
                pane_added_for_subscriber.store(true, Ordering::SeqCst);
            }
            true
        })
        .expect("test mux subscription should allocate an identifier");
        let (result_tx, result_rx) = std::sync::mpsc::channel();
        let reader = RegistrationObservingReader {
            mux: Arc::clone(&mux),
            pane_id: 80,
            tab_id,
            pane_added,
            result_tx: Some(result_tx),
        };
        let (pane, _) =
            KillCountingPane::new_with_reader(80, test_size(), Some(Box::new(reader)), false);
        tab.assign_pane(&pane);

        mux.add_tab_and_active_pane(&tab)
            .expect("tab, pane, and PaneAdded should publish before reader start");
        let (pane_was_registered, tab_contained_pane, pane_added_was_emitted) = result_rx
            .recv_timeout(Duration::from_secs(30))
            .expect("pane reader should report its initial publication state");
        assert!(pane_was_registered);
        assert!(tab_contained_pane);
        assert!(pane_added_was_emitted);
    }

    #[test]
    fn dropping_unreleased_reader_gate_exits_without_touching_pane_or_reading() {
        let mux = Arc::new(Mux::new(None));
        let reads = Arc::new(AtomicUsize::new(0));
        let (dropped_tx, dropped_rx) = std::sync::mpsc::channel();
        let reader = CancellationObservingReader {
            reads: Arc::clone(&reads),
            dropped_tx: Some(dropped_tx),
        };
        let (pane, _) =
            KillCountingPane::new_with_reader(81, test_size(), Some(Box::new(reader)), false);

        let reader_start_gate = {
            let preparation_claim = mux
                .claim_pane_preparation(&pane)
                .expect("pane preparation claim should succeed")
                .expect("new pane should require a preparation claim");
            let prepared = mux
                .prepare_claimed_pane_registration(
                    &pane,
                    pane.pane_id(),
                    &preparation_claim.generation,
                )
                .expect("pane preparation should succeed");
            mux.spawn_prepared_pane_reader(&pane, prepared, &preparation_claim.generation)
                .expect("reader thread should spawn")
                .0
                .expect("pane reader should produce a start gate")
        };

        assert!(
            mux.get_pane(81).is_none(),
            "spawning a gated reader must not publish the pane"
        );
        drop(reader_start_gate);
        dropped_rx
            .recv_timeout(Duration::from_secs(30))
            .expect("cancelled reader thread should drop its unread reader");
        assert_eq!(reads.load(Ordering::SeqCst), 0);
        assert!(mux.get_pane(81).is_none());
    }

    #[test]
    fn pane_reader_callback_can_reenter_registration_for_an_unrelated_pane() {
        let mux = Arc::new(Mux::new(None));
        let (nested, _) = KillCountingPane::new(83, test_size());
        let mux_for_reader = Arc::clone(&mux);
        let nested_for_reader = Arc::clone(&nested);
        let (outer, _) = KillCountingPane::new_with_reader_callback(82, test_size(), move || {
            assert!(
                mux_for_reader.pane_registration.try_lock().is_some(),
                "external Pane callbacks must not run under the publication mutex"
            );
            mux_for_reader
                .add_pane(&nested_for_reader)
                .expect("reader callback should reenter registration");
        });

        mux.add_pane(&outer)
            .expect("outer pane should register after its callback returns");

        let registered_outer = mux.get_pane(82).expect("outer pane should be registered");
        let registered_nested = mux.get_pane(83).expect("nested pane should be registered");
        assert!(Arc::ptr_eq(&registered_outer, &outer));
        assert!(Arc::ptr_eq(&registered_nested, &nested));
    }

    #[test]
    fn pane_kill_callback_can_reenter_registration_without_registry_lock() {
        let mux = Arc::new(Mux::new(None));
        let (nested, _) = KillCountingPane::new(85, test_size());
        let mux_for_kill = Arc::clone(&mux);
        let nested_for_kill = Arc::clone(&nested);
        let (removed, kills) =
            KillCountingPane::new_with_kill_callback(84, test_size(), move || {
                assert!(
                    mux_for_kill.panes.try_write().is_some(),
                    "Pane::kill must not run under the pane registry write lock"
                );
                mux_for_kill
                    .add_pane(&nested_for_kill)
                    .expect("kill callback should reenter registration");
            });
        mux.add_pane(&removed)
            .expect("pane under test should register");

        mux.remove_pane(84);

        assert_eq!(kills.load(Ordering::SeqCst), 1);
        assert!(mux.get_pane(84).is_none());
        let registered_nested = mux.get_pane(85).expect("nested pane should be registered");
        assert!(Arc::ptr_eq(&registered_nested, &nested));
    }

    #[test]
    fn pane_registration_rollback_never_removes_a_replacement_instance() {
        let mux = Arc::new(Mux::new(None));
        let (failed_registration, _) = KillCountingPane::new(91, test_size());
        let (replacement, _) = KillCountingPane::new(91, test_size());
        mux.panes.write().insert(
            91,
            LivePaneRegistration {
                pane: Arc::clone(&replacement),
                generation: PaneRegistrationGeneration::new(91, &mux.pane_retirements, Weak::new()),
                domain_id: replacement.domain_id(),
            },
        );

        assert!(!mux.remove_pane_registration_if_same(91, &failed_registration));
        let registered = mux
            .get_pane(91)
            .expect("pointer-mismatched rollback must preserve replacement pane");
        assert!(Arc::ptr_eq(&registered, &replacement));

        assert!(mux.remove_pane_registration_if_same(91, &replacement));
        assert!(mux.get_pane(91).is_none());

        let failed_tab = Arc::new(Tab::new(&test_size()));
        let replacement_tab = Arc::new(Tab::new(&test_size()));
        let replacement_tab_id = replacement_tab.tab_id();
        mux.tabs
            .write()
            .insert(replacement_tab_id, Arc::clone(&replacement_tab));

        assert!(!mux.remove_tab_registration_if_same(replacement_tab_id, &failed_tab));
        let registered_tab = mux
            .get_tab(replacement_tab_id)
            .expect("pointer-mismatched rollback must preserve replacement tab");
        assert!(Arc::ptr_eq(&registered_tab, &replacement_tab));

        assert!(mux.remove_tab_registration_if_same(replacement_tab_id, &replacement_tab));
        assert!(mux.get_tab(replacement_tab_id).is_none());
    }

    #[test]
    fn default_workspace_value() {
        assert_eq!(DEFAULT_WORKSPACE, "default");
    }

    #[test]
    fn synchronized_output_decrqm_response_reports_hold_state() {
        assert_eq!(synchronized_output_decrqm_response(true), b"\x1b[?2026;1$y");
        assert_eq!(
            synchronized_output_decrqm_response(false),
            b"\x1b[?2026;2$y"
        );
    }

    #[test]
    fn synchronized_output_query_is_answered_from_parser_hold_state() {
        let mut parser = termwiz::escape::parser::Parser::new();
        let mut hold = SynchronizedOutputHold::default();
        let mut responses = Vec::new();
        let mut forwarded_actions = Vec::new();
        let mut events = Vec::new();

        parser.parse(
            b"\x1b[?2026h\x1b[?2026$p\x1b[?2026l\x1b[?2026$p",
            |action| {
                let effect = handle_synchronized_output_action(&action, &mut hold, |hold| {
                    responses.push(synchronized_output_decrqm_response(hold).to_vec());
                });
                if let Some(outcome) = effect.depth_outcome {
                    events.push(SynchronizedOutputEvent::Depth {
                        outcome,
                        max_depth: hold.max_depth(),
                    });
                }
                if effect.handled {
                    events.push(SynchronizedOutputEvent::ModeQuery);
                }
                if !effect.handled {
                    forwarded_actions.push(action);
                }
            },
        );

        assert_eq!(
            responses,
            vec![b"\x1b[?2026;1$y".to_vec(), b"\x1b[?2026;2$y".to_vec()]
        );
        assert_eq!(
            forwarded_actions.len(),
            2,
            "mode-query actions must be answered directly, not forwarded into the held action buffer",
        );
        assert_eq!(
            events,
            vec![
                SynchronizedOutputEvent::Depth {
                    outcome: SynchronizedOutputDepthOutcome::Opened { new_depth: 1 },
                    max_depth: 1,
                },
                SynchronizedOutputEvent::ModeQuery,
                SynchronizedOutputEvent::Depth {
                    outcome: SynchronizedOutputDepthOutcome::Flushed,
                    max_depth: 1,
                },
                SynchronizedOutputEvent::ModeQuery,
            ],
        );
        assert!(matches!(
            &forwarded_actions[0],
            Action::CSI(CSI::Mode(Mode::SetDecPrivateMode(DecPrivateMode::Code(
                DecPrivateModeCode::SynchronizedOutput
            ))))
        ));
        assert!(matches!(
            &forwarded_actions[1],
            Action::CSI(CSI::Mode(Mode::ResetDecPrivateMode(DecPrivateMode::Code(
                DecPrivateModeCode::SynchronizedOutput
            ))))
        ));
    }

    #[test]
    fn synchronized_output_hold_tracks_nested_depth_and_underflow() {
        let mut parser = termwiz::escape::parser::Parser::new();
        let mut hold = SynchronizedOutputHold::default();
        let mut outcomes = Vec::new();
        let mut flushes = 0;

        parser.parse(
            b"\x1b[?2026h\x1b[?2026h\x1b[?2026l\x1b[?2026l\x1b[?2026l",
            |action| {
                let effect = handle_synchronized_output_action(&action, &mut hold, |_| {});
                if effect.flush {
                    flushes += 1;
                }
                if let Some(outcome) = effect.depth_outcome {
                    outcomes.push(outcome);
                }
            },
        );

        assert_eq!(
            outcomes,
            vec![
                SynchronizedOutputDepthOutcome::Opened { new_depth: 1 },
                SynchronizedOutputDepthOutcome::Opened { new_depth: 2 },
                SynchronizedOutputDepthOutcome::Closed { new_depth: 1 },
                SynchronizedOutputDepthOutcome::Flushed,
                SynchronizedOutputDepthOutcome::Underflow,
            ]
        );
        assert_eq!(flushes, 1, "only the ESU that closes depth to zero flushes");
        assert_eq!(hold.max_depth(), 2);
        assert!(!hold.is_holding());
    }

    #[test]
    fn synchronized_output_soft_reset_flushes_without_operator_attribution() {
        let mut parser = termwiz::escape::parser::Parser::new();
        let mut hold = SynchronizedOutputHold::default();
        let mut flushes = 0;

        parser.parse(b"\x1b[?2026h\x1b[!p", |action| {
            let effect = handle_synchronized_output_action(&action, &mut hold, |_| {});
            if effect.flush {
                flushes += 1;
            }
        });

        assert_eq!(flushes, 1);
        assert!(!hold.is_holding());
    }

    #[derive(Debug, Clone, Copy)]
    enum SynchronizedOutputWireOp {
        Set,
        Reset,
        Query,
    }

    fn synchronized_output_wire_op_strategy() -> impl Strategy<Value = SynchronizedOutputWireOp> {
        prop_oneof![
            Just(SynchronizedOutputWireOp::Set),
            Just(SynchronizedOutputWireOp::Reset),
            Just(SynchronizedOutputWireOp::Query),
        ]
    }

    fn append_synchronized_output_wire_op(bytes: &mut Vec<u8>, op: SynchronizedOutputWireOp) {
        match op {
            SynchronizedOutputWireOp::Set => bytes.extend_from_slice(b"\x1b[?2026h"),
            SynchronizedOutputWireOp::Reset => bytes.extend_from_slice(b"\x1b[?2026l"),
            SynchronizedOutputWireOp::Query => bytes.extend_from_slice(b"\x1b[?2026$p"),
        }
    }

    proptest! {
        #[test]
        fn synchronized_output_escape_stream_queries_follow_hold_state(
            ops in proptest::collection::vec(synchronized_output_wire_op_strategy(), 1..64),
            chunk_sizes in proptest::collection::vec(1usize..8, 1..128),
        ) {
            let mut expected_depth = 0_u32;
            let mut expected_responses = Vec::new();
            let mut expected_forwarded = 0usize;
            let mut input = Vec::new();

            for op in &ops {
                append_synchronized_output_wire_op(&mut input, *op);
                match op {
                    SynchronizedOutputWireOp::Set => {
                        expected_depth = expected_depth.saturating_add(1);
                        expected_forwarded += 1;
                    }
                    SynchronizedOutputWireOp::Reset => {
                        expected_depth = expected_depth.saturating_sub(1);
                        expected_forwarded += 1;
                    }
                    SynchronizedOutputWireOp::Query => {
                        expected_responses
                            .push(synchronized_output_decrqm_response(expected_depth > 0).to_vec());
                    }
                }
            }

            let mut parser = termwiz::escape::parser::Parser::new();
            let mut hold = SynchronizedOutputHold::default();
            let mut responses = Vec::new();
            let mut forwarded = 0usize;
            let mut offset = 0usize;
            let mut chunk_iter = chunk_sizes.iter().copied().cycle();

            while offset < input.len() {
                let chunk_len = chunk_iter.next().unwrap_or(input.len()).min(input.len() - offset);
                parser.parse(&input[offset..offset + chunk_len], |action| {
                    let effect = handle_synchronized_output_action(&action, &mut hold, |hold| {
                        responses.push(synchronized_output_decrqm_response(hold).to_vec());
                    });
                    if !effect.handled {
                        forwarded += 1;
                    }
                });
                offset += chunk_len;
            }

            prop_assert_eq!(responses, expected_responses);
            prop_assert_eq!(forwarded, expected_forwarded);
            prop_assert_eq!(hold.is_holding(), expected_depth > 0);
        }
    }

    #[test]
    fn mux_notification_pane_output_debug() {
        let n = MuxNotification::PaneOutput(42);
        let dbg = format!("{:?}", n);
        assert!(dbg.contains("PaneOutput"));
        assert!(dbg.contains("42"));
    }

    #[test]
    fn mux_notification_synchronized_output_debug_and_clone() {
        let n = MuxNotification::SynchronizedOutput {
            pane_id: 7,
            event: SynchronizedOutputEvent::Drain {
                cause: SynchronizedOutputDrainCause::Watchdog,
                bytes: 8192,
                depth_outcome: None,
                max_depth: 3,
            },
        };
        let dbg = format!("{:?}", n.clone());
        assert!(dbg.contains("SynchronizedOutput"));
        assert!(dbg.contains("Watchdog"));
        assert!(dbg.contains("7"));
    }

    #[test]
    fn mux_notification_pane_added_clone() {
        let n = MuxNotification::PaneAdded(1);
        let n2 = n.clone();
        let dbg = format!("{:?}", n2);
        assert!(dbg.contains("PaneAdded"));
    }

    #[test]
    fn mux_notification_pane_removed() {
        let n = MuxNotification::PaneRemoved(5);
        let dbg = format!("{:?}", n);
        assert!(dbg.contains("PaneRemoved"));
    }

    #[test]
    fn mux_notification_window_created() {
        let n = MuxNotification::WindowCreated(0);
        let dbg = format!("{:?}", n);
        assert!(dbg.contains("WindowCreated"));
    }

    #[test]
    fn mux_notification_window_removed() {
        let n = MuxNotification::WindowRemoved(1);
        let dbg = format!("{:?}", n);
        assert!(dbg.contains("WindowRemoved"));
    }

    #[test]
    fn mux_notification_empty() {
        let n = MuxNotification::Empty;
        let dbg = format!("{:?}", n);
        assert!(dbg.contains("Empty"));
    }

    #[test]
    fn subscribe_handle_can_unsubscribe() {
        let mux = Mux::new(None);
        let notifications = Arc::new(AtomicUsize::new(0));
        let observed = Arc::clone(&notifications);

        let sub_id = mux
            .subscribe(move |_| {
                observed.fetch_add(1, Ordering::Relaxed);
                true
            })
            .expect("test mux subscription should allocate an identifier");

        mux.notify(MuxNotification::Empty);
        assert_eq!(notifications.load(Ordering::Relaxed), 1);

        assert!(mux.unsubscribe(sub_id));
        mux.notify(MuxNotification::Empty);
        assert_eq!(notifications.load(Ordering::Relaxed), 1);
        assert!(!mux.unsubscribe(sub_id));
    }

    #[test]
    fn generic_notify_rejects_forged_pane_lifecycle_variants() {
        let mux = Mux::new(None);
        let observed = Arc::new(Mutex::new(Vec::new()));
        let observed_for_subscriber = Arc::clone(&observed);
        mux.subscribe_with_topology(move |envelope| {
            observed_for_subscriber.lock().push(envelope);
            true
        })
        .expect("test topology subscription should allocate an identifier");

        mux.notify(MuxNotification::PaneAdded(71));
        mux.notify(MuxNotification::PaneRemoved(71));

        assert!(
            observed.lock().is_empty(),
            "generic notification authority must not publish a forged pane lifecycle",
        );
        assert_eq!(
            mux.topology_snapshot_authority()
                .expect("rejected lifecycle variants cannot exhaust topology authority")
                .1,
            TopologyRevision::default(),
            "rejected lifecycle variants must not reserve topology revisions",
        );
    }

    #[test]
    fn topology_subscriber_receives_checked_revisions_and_non_topology_markers() {
        let mux = Mux::new(None);
        let observed = Arc::new(Mutex::new(Vec::new()));
        let observed_for_subscriber = Arc::clone(&observed);
        mux.subscribe_with_topology(move |envelope| {
            observed_for_subscriber.lock().push(envelope.topology);
            true
        })
        .expect("test topology subscription should allocate an identifier");

        mux.notify(MuxNotification::TabResized(17));
        mux.notify(MuxNotification::SynchronizedOutput {
            pane_id: 17,
            event: SynchronizedOutputEvent::ModeQuery,
        });
        mux.notify(MuxNotification::WindowTitleChanged {
            window_id: 23,
            title: "two".to_string(),
        });

        assert_eq!(
            *observed.lock(),
            vec![
                MuxTopologyStamp::Revision(TopologyRevision(1)),
                MuxTopologyStamp::NonTopology,
                MuxTopologyStamp::Revision(TopologyRevision(2)),
            ]
        );
        let (session, revision) = mux
            .topology_snapshot_authority()
            .expect("live topology authority should remain available");
        assert_ne!(session.as_bytes(), [0; 16]);
        assert_eq!(revision, TopologyRevision(2));
    }

    #[test]
    fn queued_window_notification_keeps_enqueue_revision_across_later_dispatch() {
        let mux = Arc::new(Mux::new(None));
        mux.bind_window_notification_owner();
        let observed = Arc::new(Mutex::new(Vec::new()));
        let observed_for_subscriber = Arc::clone(&observed);
        mux.subscribe_with_topology(move |envelope| {
            if let MuxTopologyStamp::Revision(revision) = envelope.topology {
                observed_for_subscriber.lock().push(revision);
            }
            true
        })
        .expect("test topology subscription should allocate an identifier");

        mux.queue_window_notification(MuxNotification::WindowTitleChanged {
            window_id: 31,
            title: "queued".to_string(),
        });
        assert_eq!(
            mux.topology_snapshot_authority()
                .expect("queued topology publication should retain authority")
                .1,
            TopologyRevision(1),
        );

        mux.notify(MuxNotification::TabTitleChanged {
            tab_id: 41,
            title: "direct".to_string(),
        });
        mux.flush_window_notifications();

        assert_eq!(
            *observed.lock(),
            vec![TopologyRevision(2), TopologyRevision(1)],
            "delivery timing may differ across internal queues, but each envelope must retain \
             the revision reserved at its mutation publication point",
        );
    }

    #[test]
    fn tab_title_reserves_revision_before_changed_state_becomes_dispatchable() {
        let mux = Arc::new(Mux::new(None));
        let tab = Arc::new(Tab::new(&test_size()));
        mux.add_tab_no_panes(&tab)
            .expect("test tab should register");
        let before = mux
            .topology_snapshot_authority()
            .expect("topology authority should be live")
            .1;
        let observed = Arc::new(Mutex::new(Vec::new()));
        let observed_for_subscriber = Arc::clone(&observed);
        mux.subscribe_with_topology(move |envelope| {
            observed_for_subscriber
                .lock()
                .push((envelope.notification, envelope.topology));
            true
        })
        .expect("test topology subscription should allocate an identifier");

        let (changed, notification) = tab.set_title_for_mux("reserved", Some(&mux));
        assert!(changed);
        assert_eq!(tab.get_title(), "reserved");
        assert!(
            observed.lock().is_empty(),
            "reservation must not invoke subscribers while the mutation owns its state lock",
        );
        let after_reservation = mux
            .topology_snapshot_authority()
            .expect("topology authority should remain live")
            .1;
        assert_eq!(after_reservation.get(), before.get() + 1);

        mux.dispatch_notification_envelope(
            notification.expect("a changed title on an exact mux reserves one envelope"),
        );
        assert!(matches!(
            observed.lock().as_slice(),
            [(
                MuxNotification::TabTitleChanged { tab_id, title },
                MuxTopologyStamp::Revision(revision),
            )] if *tab_id == tab.tab_id()
                && title == "reserved"
                && *revision == after_reservation
        ));
    }

    #[test]
    fn workspace_rename_dispatches_only_after_new_window_state_is_visible() {
        let mux = Arc::new(Mux::new(None));
        let window_builder = mux.new_empty_window(Some("old-workspace".to_string()), None);
        let window_id = *window_builder;
        drop(window_builder);
        let before = mux
            .topology_snapshot_authority()
            .expect("topology authority should be live")
            .1;
        let observed = Arc::new(Mutex::new(Vec::new()));
        let observed_for_subscriber = Arc::clone(&observed);
        let mux_for_subscriber = Arc::clone(&mux);
        mux.subscribe_with_topology(move |envelope| {
            let kind = match envelope.notification {
                MuxNotification::WindowWorkspaceChanged {
                    window_id: id,
                    workspace,
                } if id == window_id && workspace == "new-workspace" => Some("window"),
                MuxNotification::WorkspaceRenamed { .. } => Some("rename"),
                _ => None,
            };
            if let Some(kind) = kind {
                let workspace = mux_for_subscriber
                    .get_window(window_id)
                    .expect("renamed window must remain registered")
                    .get_workspace()
                    .to_string();
                observed_for_subscriber
                    .lock()
                    .push((kind, envelope.topology, workspace));
            }
            true
        })
        .expect("test topology subscription should allocate an identifier");

        mux.rename_workspace("old-workspace", "new-workspace");

        assert_eq!(
            observed.lock().as_slice(),
            &[
                (
                    "window",
                    MuxTopologyStamp::Revision(TopologyRevision::new(before.get() + 1)),
                    "new-workspace".to_string(),
                ),
                (
                    "rename",
                    MuxTopologyStamp::Revision(TopologyRevision::new(before.get() + 2)),
                    "new-workspace".to_string(),
                ),
            ],
            "both reserved events must observe the fully committed workspace mutation",
        );
    }

    #[test]
    fn window_removal_reserves_revision_before_detachable_domain_callback() {
        let _guard = global_test_lock();
        Mux::shutdown();

        let mux = Arc::new(Mux::new(None));
        Mux::set_mux(&mux);
        let (detach_entered_tx, detach_entered_rx) = std::sync::mpsc::sync_channel(1);
        let (detach_release_tx, detach_release_rx) = std::sync::mpsc::sync_channel(1);
        let domain: Arc<dyn Domain> = Arc::new(BlockingDetachTestDomain {
            entered: detach_entered_tx,
            release: StdMutex::new(detach_release_rx),
        });
        mux.add_domain(&domain)
            .expect("register blocking test domain");

        let window_builder = mux.new_empty_window(None, None);
        let window_id = *window_builder;
        let tab = Arc::new(Tab::new(&test_size()));
        let (pane, _) = KillCountingPane::new(211, test_size());
        tab.assign_pane(&pane);
        mux.add_tab_and_active_pane(&tab)
            .expect("test tab and pane should register");
        mux.add_tab_to_window(&tab, window_id)
            .expect("test tab should attach to its window");
        drop(window_builder);
        let before = mux
            .topology_snapshot_authority()
            .expect("topology authority should be live")
            .1;
        let removal_revisions = Arc::new(Mutex::new(Vec::new()));
        let removal_revisions_for_subscriber = Arc::clone(&removal_revisions);
        mux.subscribe_with_topology(move |envelope| {
            match envelope {
                MuxNotificationEnvelope {
                    notification: MuxNotification::WindowTopologyChanged(change),
                    topology: MuxTopologyStamp::Revision(revision),
                } if change.removed_windows().binary_search(&window_id).is_ok() => {
                    removal_revisions_for_subscriber.lock().push(revision);
                }
                _ => {}
            }
            true
        })
        .expect("window-removal subscriber should register");

        let mux_for_removal = Arc::clone(&mux);
        let remover = std::thread::spawn(move || mux_for_removal.kill_window(window_id));
        detach_entered_rx
            .recv_timeout(Duration::from_secs(30))
            .expect("window removal should enter the detachable domain callback");

        let window_was_removed = mux.get_window(window_id).is_none();
        let during_callback = mux
            .topology_snapshot_authority()
            .expect("topology authority should remain live")
            .1;
        let removal_revision = removal_revisions.lock().last().copied();

        detach_release_tx
            .send(())
            .expect("release detachable-domain callback");
        remover.join().expect("window removal should complete");
        assert!(
            window_was_removed,
            "the window registry mutation precedes the detachable-domain callback",
        );
        let removal_revision = removal_revision.expect(
            "the frozen window-retirement transaction must be published before the detachable callback starts",
        );
        assert_eq!(
            removal_revisions.lock().as_slice(),
            &[removal_revision],
            "last-window retirement must publish exactly one frozen removal transaction",
        );
        assert!(
            removal_revision > before && removal_revision <= during_callback,
            "the frozen retirement revision {} must be reserved after the pre-removal snapshot \
             {} and before the callback-visible authority {}",
            removal_revision.get(),
            before.get(),
            during_callback.get(),
        );
        Mux::shutdown();
    }

    #[test]
    fn topology_revision_exhaustion_is_terminal_and_never_wraps() {
        let mux = Mux::new(None);
        mux.topology.lock().revision = TopologyRevision(u64::MAX - 1);
        let observed = Arc::new(Mutex::new(Vec::new()));
        let observed_for_subscriber = Arc::clone(&observed);
        mux.subscribe_with_topology(move |envelope| {
            observed_for_subscriber.lock().push(envelope.topology);
            true
        })
        .expect("test topology subscription should allocate an identifier");

        mux.notify(MuxNotification::Empty);
        mux.notify(MuxNotification::WindowInvalidated(99));

        assert_eq!(
            *observed.lock(),
            vec![MuxTopologyStamp::Exhausted, MuxTopologyStamp::Exhausted],
        );
        assert_eq!(
            mux.topology.lock().revision,
            TopologyRevision(u64::MAX - 1),
            "the reserved terminal sentinel must never be published",
        );
        assert_eq!(
            mux.topology_snapshot_authority().unwrap_err(),
            TopologyRevisionExhausted,
        );
    }

    #[test]
    fn topology_fence_subscription_captures_an_atomic_baseline() {
        let mux = Mux::new(None);
        mux.notify(MuxNotification::TabResized(17));

        let observed = Arc::new(Mutex::new(Vec::new()));
        let observed_for_subscriber = Arc::clone(&observed);
        let (_sub_id, session, baseline) = mux
            .subscribe_with_topology_fence(move |envelope| {
                observed_for_subscriber.lock().push(envelope.topology);
                true
            })
            .expect("live topology authority should permit a fenced subscription");

        assert_ne!(session.as_bytes(), [0; 16]);
        assert_eq!(baseline, TopologyRevision(1));
        mux.notify(MuxNotification::WindowInvalidated(17));
        assert_eq!(
            *observed.lock(),
            vec![MuxTopologyStamp::Revision(TopologyRevision(2))],
        );
    }

    #[test]
    fn topology_fence_subscription_fails_closed_after_revision_exhaustion() {
        let mux = Mux::new(None);
        mux.topology.lock().revision = TopologyRevision(u64::MAX);

        assert!(matches!(
            mux.subscribe_with_topology_fence(|_| true),
            Err(TopologySubscriptionError::RevisionExhausted(
                TopologyRevisionExhausted
            )),
        ));
        assert!(mux.subscribers.read().is_empty());
    }

    #[test]
    fn infallible_allocator_issues_the_last_unreserved_identifier_once() {
        let counter = AtomicUsize::new(usize::MAX - 1);

        assert_eq!(next_unique_usize_id(&counter, "test"), usize::MAX - 1);
        assert_eq!(counter.load(Ordering::Relaxed), usize::MAX);
    }

    #[test]
    #[should_panic(expected = "test identifier space exhausted; refusing to reuse an identifier")]
    fn infallible_allocator_fails_closed_at_exhaustion() {
        let counter = AtomicUsize::new(usize::MAX);
        let _ = next_unique_usize_id(&counter, "test");
    }

    #[test]
    fn checked_atomic_count_updates_preserve_zero_and_maximum_bounds() {
        let counter = AtomicUsize::new(0);

        assert!(!try_decrement_atomic_count(&counter));
        assert_eq!(counter.load(Ordering::Acquire), 0);
        assert!(try_increment_atomic_count(&counter));
        assert_eq!(counter.load(Ordering::Acquire), 1);
        assert!(try_decrement_atomic_count(&counter));
        assert_eq!(counter.load(Ordering::Acquire), 0);

        counter.store(usize::MAX - 1, Ordering::Release);
        assert!(try_increment_atomic_count(&counter));
        assert_eq!(counter.load(Ordering::Acquire), usize::MAX);
        assert!(!try_increment_atomic_count(&counter));
        assert_eq!(counter.load(Ordering::Acquire), usize::MAX);
    }

    #[test]
    fn checked_id_reservation_uses_terminal_value_only_as_exhausted_sentinel() {
        let counter = AtomicUsize::new(usize::MAX - 2);

        assert_eq!(
            try_reserve_usize_ids(&counter, 2, "test").unwrap(),
            usize::MAX - 2..usize::MAX
        );
        assert_eq!(counter.load(Ordering::Relaxed), usize::MAX);

        let err = try_reserve_usize_ids(&counter, 1, "test").unwrap_err();
        assert_eq!(err.namespace(), "test");
        assert_eq!(err.requested(), 1);
        assert_eq!(counter.load(Ordering::Relaxed), usize::MAX);
    }

    #[test]
    fn checked_id_reservation_is_atomic_when_the_requested_range_will_not_fit() {
        let counter = AtomicUsize::new(usize::MAX - 1);

        let err = try_reserve_usize_ids(&counter, 2, "test").unwrap_err();
        assert_eq!(err.namespace(), "test");
        assert_eq!(err.requested(), 2);
        assert!(err
            .to_string()
            .contains("insufficient remaining capacity for a reservation of 2"));
        assert_eq!(counter.load(Ordering::Relaxed), usize::MAX - 1);
        assert_eq!(
            try_reserve_usize_ids(&counter, 1, "test").unwrap(),
            usize::MAX - 1..usize::MAX
        );
    }

    #[test]
    fn concurrent_checked_id_reservations_are_unique_and_gap_free() {
        const THREADS: usize = 8;
        const RESERVATIONS_PER_THREAD: usize = 64;
        const IDS_PER_RESERVATION: usize = 7;

        let counter = Arc::new(AtomicUsize::new(0));
        let barrier = Arc::new(std::sync::Barrier::new(THREADS));
        let observed = Arc::new(Mutex::new(Vec::new()));
        let mut workers = Vec::new();

        for _ in 0..THREADS {
            let counter = Arc::clone(&counter);
            let barrier = Arc::clone(&barrier);
            let observed = Arc::clone(&observed);
            workers.push(std::thread::spawn(move || {
                barrier.wait();
                for _ in 0..RESERVATIONS_PER_THREAD {
                    let reserved =
                        try_reserve_usize_ids(&counter, IDS_PER_RESERVATION, "test").unwrap();
                    observed.lock().extend(reserved);
                }
            }));
        }

        for worker in workers {
            worker.join().expect("reservation worker should not panic");
        }

        let expected = THREADS * RESERVATIONS_PER_THREAD * IDS_PER_RESERVATION;
        let mut observed = observed.lock();
        observed.sort_unstable();
        assert_eq!(&*observed, &(0..expected).collect::<Vec<_>>());
        assert_eq!(counter.load(Ordering::Relaxed), expected);
    }

    #[test]
    fn notification_callbacks_can_unsubscribe_without_lock_reentrancy() {
        let mux = Arc::new(Mux::new(None));
        let first_notifications = Arc::new(AtomicUsize::new(0));
        let second_notifications = Arc::new(AtomicUsize::new(0));
        let second_sub_id = Arc::new(Mutex::new(None));

        let mux_for_first = Arc::clone(&mux);
        let second_sub_id_for_first = Arc::clone(&second_sub_id);
        let first_notifications_for_first = Arc::clone(&first_notifications);
        mux.subscribe(move |_| {
            first_notifications_for_first.fetch_add(1, Ordering::Relaxed);
            if let Some(sub_id) = *second_sub_id_for_first.lock() {
                mux_for_first.unsubscribe(sub_id);
            }
            true
        })
        .expect("test mux subscription should allocate an identifier");

        let second_notifications_for_second = Arc::clone(&second_notifications);
        let second_id = mux
            .subscribe(move |_| {
                second_notifications_for_second.fetch_add(1, Ordering::Relaxed);
                true
            })
            .expect("test mux subscription should allocate an identifier");
        *second_sub_id.lock() = Some(second_id);

        mux.dispatch_notification(MuxNotification::Empty);
        assert_eq!(first_notifications.load(Ordering::Relaxed), 1);
        assert_eq!(
            second_notifications.load(Ordering::Relaxed),
            1,
            "snapshot fanout allows already-snapshotted subscribers to observe the current event",
        );

        mux.dispatch_notification(MuxNotification::Empty);
        assert_eq!(first_notifications.load(Ordering::Relaxed), 2);
        assert_eq!(
            second_notifications.load(Ordering::Relaxed),
            1,
            "unsubscribe during callback should remove the subscriber for future notifications",
        );
    }

    #[test]
    fn high_rate_alert_dedupe_preserves_value_bearing_progress_updates() {
        let mux = Mux::new(None);
        let notifications = Arc::new(AtomicUsize::new(0));
        let observed = Arc::clone(&notifications);
        mux.subscribe(move |notification| {
            if matches!(notification, MuxNotification::Alert { .. }) {
                observed.fetch_add(1, Ordering::Relaxed);
            }
            true
        })
        .expect("test mux subscription should allocate an identifier");

        let cwd = MuxNotification::Alert {
            pane_id: 7,
            alert: frankenterm_term::Alert::CurrentWorkingDirectoryChanged,
        };
        mux.notify(cwd.clone());
        mux.notify(cwd.clone());
        assert_eq!(
            notifications.load(Ordering::Relaxed),
            1,
            "idempotent same-pane alerts should dedupe inside the frame window",
        );

        mux.notify(MuxNotification::Alert {
            pane_id: 7,
            alert: frankenterm_term::Alert::Progress(frankenterm_term::Progress::Percentage(42)),
        });
        mux.notify(MuxNotification::Alert {
            pane_id: 7,
            alert: frankenterm_term::Alert::Progress(frankenterm_term::Progress::Percentage(64)),
        });
        assert_eq!(
            notifications.load(Ordering::Relaxed),
            3,
            "newer value-bearing progress state must never be timer-dropped",
        );

        mux.notify(MuxNotification::Alert {
            pane_id: 7,
            alert: frankenterm_term::Alert::OutputSinceFocusLost,
        });
        assert_eq!(
            notifications.load(Ordering::Relaxed),
            4,
            "different idempotent alert kinds should not dedupe each other",
        );

        {
            let mut last = mux.last_high_rate_alert.lock();
            *last
                .get_mut(&(7, HighRateAlertKind::CurrentWorkingDirectoryChanged))
                .expect("first cwd alert should populate the dedupe map") = Instant::now()
                .checked_sub(HIGH_RATE_ALERT_DEDUPE_WINDOW + Duration::from_millis(1))
                .expect("test duration is small enough to subtract from now");
        }
        mux.notify(cwd);
        assert_eq!(
            notifications.load(Ordering::Relaxed),
            5,
            "same-pane same-kind alert should dispatch again after the dedupe window",
        );
    }

    #[test]
    fn remove_pane_discards_high_rate_alert_state() {
        let mux = Mux::new(None);
        mux.notify(MuxNotification::Alert {
            pane_id: 7,
            alert: frankenterm_term::Alert::OutputSinceFocusLost,
        });
        mux.notify(MuxNotification::Alert {
            pane_id: 7,
            alert: frankenterm_term::Alert::CurrentWorkingDirectoryChanged,
        });
        mux.notify(MuxNotification::Alert {
            pane_id: 8,
            alert: frankenterm_term::Alert::OutputSinceFocusLost,
        });

        {
            let last = mux.last_high_rate_alert.lock();
            assert!(last.contains_key(&(7, HighRateAlertKind::OutputSinceFocusLost)));
            assert!(last.contains_key(&(7, HighRateAlertKind::CurrentWorkingDirectoryChanged)));
            assert!(last.contains_key(&(8, HighRateAlertKind::OutputSinceFocusLost)));
        }

        mux.remove_pane(7);

        let last = mux.last_high_rate_alert.lock();
        assert!(
            !last.keys().any(|(pane_id, _)| *pane_id == 7),
            "remove_pane must not leave high-rate alert dedupe entries for a dead pane",
        );
        assert!(
            last.contains_key(&(8, HighRateAlertKind::OutputSinceFocusLost)),
            "tearing down one pane must not clear dedupe state for unrelated live panes",
        );
    }

    #[test]
    fn remove_pane_discards_client_focus_for_removed_pane() {
        let mux = Arc::new(Mux::new(None));
        let removed_client = Arc::new(ClientId::new());
        let unrelated_client = Arc::new(ClientId::new());
        let (removed_pane, _) = KillCountingPane::new(7, test_size());
        let (unrelated_pane, _) = KillCountingPane::new(8, test_size());
        mux.add_pane(&removed_pane)
            .expect("removed pane registration");
        mux.add_pane(&unrelated_pane)
            .expect("unrelated pane registration");
        mux.register_client(Arc::clone(&removed_client));
        mux.register_client(Arc::clone(&unrelated_client));
        assert!(mux.record_focus_for_client(&removed_client, 7));
        assert!(mux.record_focus_for_client(&unrelated_client, 8));

        let (removed_focus, unrelated_focus) = {
            let clients = mux.clients.read();
            let removed = &clients[removed_client.as_ref()];
            let unrelated = &clients[unrelated_client.as_ref()];
            assert_eq!(removed.focused_pane_id, Some(7));
            assert_eq!(unrelated.focused_pane_id, Some(8));
            (
                removed
                    .focused_pane_registration()
                    .expect("removed client must retain exact pane authority"),
                unrelated
                    .focused_pane_registration()
                    .expect("unrelated client must retain exact pane authority"),
            )
        };

        mux.remove_pane(7);

        assert!(
            removed_focus.try_with_current(|_| ()).is_none(),
            "the removed pane registration must be retired",
        );
        let clients = mux.clients.read();
        let removed = &clients[removed_client.as_ref()];
        assert_eq!(
            removed.focused_pane_id, None,
            "remove_pane must clear per-client focus state for the removed pane",
        );
        assert!(
            removed.focused_pane_registration().is_none(),
            "remove_pane must clear the removed exact focus authority",
        );
        let unrelated = &clients[unrelated_client.as_ref()];
        assert_eq!(
            unrelated.focused_pane_id,
            Some(8),
            "removing one pane must not clear client focus for unrelated panes",
        );
        let surviving_focus = unrelated
            .focused_pane_registration()
            .expect("unrelated exact focus authority must survive");
        assert!(
            surviving_focus.same_registration(&unrelated_focus),
            "unrelated focus must retain the same exact pane generation",
        );
        assert_eq!(
            surviving_focus.try_with_current(|current| current.pane_id()),
            Some(8),
            "unrelated focus authority must remain live",
        );
    }

    #[test]
    fn record_focus_rejects_unregistered_numeric_pane_id() {
        let mux = Arc::new(Mux::new(None));
        let client = Arc::new(ClientId::new());
        mux.register_client(Arc::clone(&client));

        assert!(
            !mux.record_focus_for_client(&client, 7),
            "a raw numeric ID must not mint focus authority without a live pane registration",
        );

        let clients = mux.clients.read();
        let stored = &clients[client.as_ref()];
        assert_eq!(stored.focused_pane_id, None);
        assert!(stored.focused_pane_registration().is_none());
    }

    #[test]
    fn unregister_client_discards_removed_active_identity() {
        let mux = Mux::new(None);
        let removed_client = Arc::new(ClientId::new());
        let retained_client = Arc::new(ClientId::new());
        mux.register_client(Arc::clone(&removed_client));
        mux.register_client(Arc::clone(&retained_client));

        mux.replace_identity(Some(Arc::clone(&retained_client)));
        mux.unregister_client(&removed_client);
        assert_eq!(
            mux.active_identity().as_deref(),
            Some(retained_client.as_ref()),
            "unregistering one client must not clear an unrelated active identity",
        );

        mux.unregister_client(&retained_client);
        assert_eq!(
            mux.active_identity(),
            None,
            "unregister_client must not leave a dead client id as the active identity",
        );
    }

    #[test]
    fn unregister_client_if_same_preserves_equal_replacement_instance() {
        let mux = Mux::new(None);
        let stale_client = Arc::new(ClientId::new());
        let replacement_client = Arc::new(stale_client.as_ref().clone());

        mux.register_client(Arc::clone(&stale_client));
        mux.register_client(Arc::clone(&replacement_client));

        assert!(
            !mux.unregister_client_if_same(&stale_client),
            "stale cleanup must not remove an equal-valued replacement registration",
        );
        assert!(
            mux.clients
                .read()
                .get(replacement_client.as_ref())
                .is_some_and(|info| Arc::ptr_eq(&info.client_id, &replacement_client)),
            "the replacement registration must survive stale cleanup",
        );

        mux.replace_identity(Some(Arc::clone(&replacement_client)));
        assert!(mux.unregister_client_if_same(&replacement_client));
        assert!(
            mux.active_identity().is_none(),
            "exact cleanup must clear the exact active identity",
        );
    }

    #[test]
    fn register_client_is_idempotent_for_the_exact_registration() {
        let mux = Arc::new(Mux::new(None));
        let client = Arc::new(ClientId::new());
        let (pane, _) = KillCountingPane::new(776, test_size());
        mux.add_pane(&pane).expect("focus target registration");
        mux.register_client(Arc::clone(&client));
        assert!(mux.record_focus_for_client(&client, 776));
        assert!(mux.set_active_workspace_for_client_if_same(&client, "preserved"));

        let before = mux
            .clients
            .read()
            .get(client.as_ref())
            .cloned()
            .expect("client registration before idempotent refresh");
        let before_focus = before
            .focused_pane_registration()
            .expect("exact focus authority before idempotent refresh");

        mux.register_client(Arc::clone(&client));

        let after = mux
            .clients
            .read()
            .get(client.as_ref())
            .cloned()
            .expect("client registration after idempotent refresh");
        let after_focus = after
            .focused_pane_registration()
            .expect("exact focus authority after idempotent refresh");
        assert_eq!(
            after, before,
            "re-registering the exact Arc must preserve all client metadata",
        );
        assert!(
            Arc::ptr_eq(&after.client_id, &client),
            "idempotent registration must preserve the exact client token",
        );
        assert!(
            before_focus.same_registration(&after_focus),
            "idempotent registration must preserve exact pane authority",
        );
    }

    #[test]
    fn resolve_focused_pane_rejects_an_equal_valued_stale_client() {
        let mux = Arc::new(Mux::new(None));
        let stale_client = Arc::new(ClientId::new());
        let replacement_client = Arc::new(stale_client.as_ref().clone());
        let (pane, _) = KillCountingPane::new(777, test_size());
        let tab = Arc::new(Tab::new(&test_size()));
        tab.assign_pane(&pane);
        mux.add_tab_and_active_pane(&tab)
            .expect("tab and active pane registration");
        let window = mux.new_empty_window(None, None);
        mux.add_tab_to_window(&tab, *window)
            .expect("tab attachment");

        mux.register_client(Arc::clone(&stale_client));
        mux.register_client(Arc::clone(&replacement_client));
        assert!(mux.record_focus_for_client(&replacement_client, 777));

        assert!(
            mux.resolve_focused_pane(&stale_client).is_none(),
            "a stale equal-valued client must not borrow replacement focus authority",
        );
        assert!(
            mux.resolve_focused_pane(&replacement_client).is_some(),
            "the exact replacement client must retain its focused-pane projection",
        );
    }

    #[test]
    fn guarded_client_mutations_reject_equal_valued_replacement_token() {
        let mux = Mux::new(None);
        let stale_client = Arc::new(ClientId::new());
        let replacement_client = Arc::new(stale_client.as_ref().clone());

        mux.register_client(Arc::clone(&stale_client));
        mux.register_client(Arc::clone(&replacement_client));
        let default_workspace = mux.active_workspace();
        assert!(
            mux.set_active_workspace_for_client_if_same(
                &replacement_client,
                "replacement-workspace",
            )
        );
        assert_eq!(
            mux.active_workspace_for_client(&replacement_client),
            "replacement-workspace",
            "the exact replacement token must read its selected workspace",
        );
        assert_eq!(
            mux.active_workspace_for_client(&stale_client),
            default_workspace,
            "an equal-valued stale token must not borrow replacement workspace authority",
        );

        let replacement_before = mux
            .clients
            .read()
            .get(replacement_client.as_ref())
            .cloned()
            .expect("equal-valued replacement should be registered");

        assert!(
            !mux.client_had_input_if_same(&stale_client),
            "stale input must not mutate an equal-valued replacement",
        );
        assert!(
            !mux.record_focus_for_client_if_same(&stale_client, 777),
            "stale focus must not mutate an equal-valued replacement",
        );
        assert!(
            !mux.set_active_workspace_for_client_if_same(&stale_client, "stale"),
            "stale workspace selection must not mutate an equal-valued replacement",
        );

        let replacement_after = mux
            .clients
            .read()
            .get(replacement_client.as_ref())
            .cloned()
            .expect("stale mutations must preserve the replacement registration");
        assert_eq!(
            replacement_after, replacement_before,
            "all guarded mutations must leave the replacement client untouched",
        );
        assert!(
            Arc::ptr_eq(&replacement_after.client_id, &replacement_client),
            "stale mutations must preserve the exact replacement registration token",
        );
    }

    #[test]
    fn current_identity_never_retargets_an_equal_valued_client_replacement() {
        let mux = Arc::new(Mux::new(None));
        let stale_client = Arc::new(ClientId::new());
        let replacement_client = Arc::new(stale_client.as_ref().clone());
        let (pane, focus_events) = KillCountingPane::new_with_focus_counter(777, test_size());
        mux.add_pane(&pane).expect("focus target registration");
        mux.register_client(Arc::clone(&stale_client));
        mux.replace_identity(Some(Arc::clone(&stale_client)));

        mux.register_client(Arc::clone(&replacement_client));
        assert!(
            mux.active_identity().is_none(),
            "equal-valued replacement must retire the stale current-identity token",
        );

        {
            let mut clients = mux.clients.write();
            let replacement = clients
                .get_mut(replacement_client.as_ref())
                .expect("replacement client remains registered");
            replacement.last_input = chrono::Utc::now() - chrono::Duration::seconds(30);
        }
        let replacement_before = mux
            .clients
            .read()
            .get(replacement_client.as_ref())
            .cloned()
            .expect("replacement client snapshot");
        let workspace_events = Arc::new(AtomicUsize::new(0));
        let workspace_events_for_subscriber = Arc::clone(&workspace_events);
        mux.subscribe(move |notification| {
            if matches!(notification, MuxNotification::ActiveWorkspaceChanged(_)) {
                workspace_events_for_subscriber.fetch_add(1, Ordering::SeqCst);
            }
            true
        })
        .expect("workspace event subscription");

        // Reinstall the stale token to model delayed identity-bearing work that
        // outlived replacement. Every current-identity mutation must still
        // fail exact pointer validation.
        mux.replace_identity(Some(Arc::clone(&stale_client)));
        mux.record_input_for_current_identity();
        mux.record_focus_for_current_identity(777);
        mux.set_active_workspace("stale-workspace");

        let replacement_after_stale = mux
            .clients
            .read()
            .get(replacement_client.as_ref())
            .cloned()
            .expect("stale work must preserve the replacement");
        assert_eq!(replacement_after_stale, replacement_before);
        assert!(focus_events.lock().is_empty());
        assert_eq!(workspace_events.load(Ordering::SeqCst), 0);

        mux.replace_identity(Some(Arc::clone(&replacement_client)));
        mux.record_input_for_current_identity();
        mux.record_focus_for_current_identity(777);
        mux.set_active_workspace("replacement-workspace");

        let replacement_after_current = mux
            .clients
            .read()
            .get(replacement_client.as_ref())
            .cloned()
            .expect("current replacement remains registered");
        assert!(replacement_after_current.last_input > replacement_before.last_input);
        assert_eq!(replacement_after_current.focused_pane_id, Some(777),);
        assert!(replacement_after_current
            .focused_pane_registration()
            .is_some(),);
        assert_eq!(
            replacement_after_current.active_workspace.as_deref(),
            Some("replacement-workspace"),
        );
        assert_eq!(focus_events.lock().as_slice(), &[true]);
        assert_eq!(workspace_events.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn failed_topology_focus_preserves_client_state_and_callbacks() {
        let mux = Arc::new(Mux::new(None));
        let client = Arc::new(ClientId::new());
        let (prior_pane, prior_focus) = KillCountingPane::new_with_focus_counter(778, test_size());
        let (target_pane, target_focus) =
            KillCountingPane::new_with_focus_counter(779, test_size());
        mux.add_pane(&prior_pane).expect("prior pane registration");
        mux.add_pane(&target_pane)
            .expect("target pane registration");
        mux.register_client(Arc::clone(&client));
        mux.record_focus_for_client(&client, 778);
        prior_focus.lock().clear();
        target_focus.lock().clear();

        let target = mux
            .capture_pane_registration(&target_pane)
            .expect("target pane should yield an exact handle");
        let result = target
            .try_with_current(|current| current.focus_for_client_if_same(Some(&client)))
            .expect("target registration remains current");

        assert!(
            result.is_err(),
            "a pane outside mux topology must not report a focus commit",
        );
        assert_eq!(
            mux.clients
                .read()
                .get(client.as_ref())
                .expect("client remains registered")
                .focused_pane_id,
            Some(778),
            "failed topology validation must preserve client focus",
        );
        assert!(
            prior_focus.lock().is_empty(),
            "failed validation must not emit prior-pane focus loss",
        );
        assert!(
            target_focus.lock().is_empty(),
            "failed validation must not emit target-pane focus gain",
        );
    }

    #[test]
    fn focus_transition_does_not_notify_same_id_replacement() {
        let mux = Arc::new(Mux::new(None));
        let client = Arc::new(ClientId::new());
        let (original, original_focus) = KillCountingPane::new_with_focus_counter(780, test_size());
        let (replacement, replacement_focus) =
            KillCountingPane::new_with_focus_counter(780, test_size());
        let (target, target_focus) = KillCountingPane::new_with_focus_counter(781, test_size());

        mux.add_pane(&original).expect("original pane registration");
        mux.add_pane(&target).expect("target pane registration");
        mux.register_client(Arc::clone(&client));
        mux.record_focus_for_client(&client, 780);
        original_focus.lock().clear();
        target_focus.lock().clear();

        assert!(
            mux.remove_pane_registration_if_same(780, &original),
            "the test must retire only the original registry entry",
        );
        mux.add_pane(&replacement)
            .expect("same-id replacement registration");
        mux.record_focus_for_client(&client, 781);

        assert!(
            original_focus.lock().is_empty(),
            "a retired exact registration cannot receive a later focus callback",
        );
        assert!(
            replacement_focus.lock().is_empty(),
            "the same-id replacement was never focused and must not receive focus loss",
        );
        assert_eq!(
            target_focus.lock().as_slice(),
            &[true],
            "the exact target registration receives one focus-gain callback",
        );
    }

    #[test]
    fn exact_focus_emits_one_transition_only_on_the_originating_mux() {
        let _guard = global_test_lock();
        Mux::shutdown();

        let origin = Arc::new(Mux::new(None));
        let replacement = Arc::new(Mux::new(None));
        let client = Arc::new(ClientId::new());
        let (prior, prior_focus) = KillCountingPane::new_with_focus_counter(783, test_size());
        let (target, target_focus) = KillCountingPane::new_with_focus_counter(784, test_size());

        let window_builder = origin.new_empty_window(None, None);
        let window_id = *window_builder;
        let tab = Arc::new(Tab::new(&test_size()));
        tab.assign_pane(&prior);
        origin
            .add_tab_and_active_pane(&tab)
            .expect("origin tab and prior pane");
        origin
            .add_tab_to_window(&tab, window_id)
            .expect("origin tab attachment");
        tab.split_and_insert(0, SplitRequest::default(), Arc::clone(&target))
            .expect("target split");
        origin.add_pane(&target).expect("target registration");
        tab.set_active_pane_for_mux(&prior, &origin);
        origin.register_client(Arc::clone(&client));
        assert!(origin.record_focus_for_client(&client, 783));
        prior_focus.lock().clear();
        target_focus.lock().clear();

        let origin_notifications = Arc::new(AtomicUsize::new(0));
        let origin_notifications_for_subscriber = Arc::clone(&origin_notifications);
        origin
            .subscribe(move |notification| {
                if matches!(notification, MuxNotification::PaneFocused(784)) {
                    origin_notifications_for_subscriber.fetch_add(1, Ordering::SeqCst);
                }
                true
            })
            .expect("origin focus subscription");
        let replacement_notifications = Arc::new(AtomicUsize::new(0));
        let replacement_notifications_for_subscriber = Arc::clone(&replacement_notifications);
        replacement
            .subscribe(move |notification| {
                if matches!(notification, MuxNotification::PaneFocused(784)) {
                    replacement_notifications_for_subscriber.fetch_add(1, Ordering::SeqCst);
                }
                true
            })
            .expect("replacement focus subscription");

        Mux::set_mux(&replacement);
        let registration = origin
            .capture_pane_registration(&target)
            .expect("exact target registration");
        let result = registration
            .try_with_current(|current| current.focus_for_client_if_same(Some(&client)))
            .expect("target remains current");
        result.expect("exact focus commit");

        assert_eq!(prior_focus.lock().as_slice(), &[false]);
        assert_eq!(target_focus.lock().as_slice(), &[true]);
        assert_eq!(origin_notifications.load(Ordering::SeqCst), 1);
        assert_eq!(replacement_notifications.load(Ordering::SeqCst), 0);

        let repeated = registration
            .try_with_current(|current| current.focus_for_client_if_same(Some(&client)))
            .expect("target remains current");
        repeated.expect("repeated exact focus is an idempotent success");
        assert_eq!(prior_focus.lock().as_slice(), &[false]);
        assert_eq!(target_focus.lock().as_slice(), &[true]);
        assert_eq!(origin_notifications.load(Ordering::SeqCst), 1);
        assert_eq!(replacement_notifications.load(Ordering::SeqCst), 0);

        drop(window_builder);
        Mux::shutdown();
    }

    #[test]
    fn client_focus_wire_views_drop_process_local_pane_authority() {
        let mux = Arc::new(Mux::new(None));
        let client = Arc::new(ClientId::new());
        let (pane, _) = KillCountingPane::new(782, test_size());
        let weak_mux = Arc::downgrade(&mux);
        let weak_pane = Arc::downgrade(&pane);

        mux.add_pane(&pane).expect("pane registration");
        mux.register_client(Arc::clone(&client));
        mux.record_focus_for_client(&client, 782);

        let stored = mux
            .clients
            .read()
            .get(client.as_ref())
            .cloned()
            .expect("client remains registered");
        assert!(
            stored.focused_pane_registration().is_some(),
            "the process-local client record must retain exact focus authority",
        );

        let json = serde_json::to_value(&stored).expect("serialize focused client");
        let object = json
            .as_object()
            .expect("ClientInfo must serialize as a JSON object");
        let mut fields = object.keys().map(String::as_str).collect::<Vec<_>>();
        fields.sort_unstable();
        assert_eq!(
            fields,
            [
                "active_workspace",
                "client_id",
                "connected_at",
                "focused_pane_id",
                "last_input",
            ],
            "the wire schema must remain the five-field metadata projection",
        );
        assert!(
            json.get("focused_pane_registration").is_none(),
            "process-local pane authority must not enter the wire schema",
        );
        let decoded: ClientInfo = serde_json::from_value(json).expect("deserialize focused client");
        assert_eq!(decoded.focused_pane_id, Some(782));
        assert!(
            decoded.focused_pane_registration().is_none(),
            "wire metadata must not mint process-local pane authority",
        );
        assert_eq!(
            decoded, stored,
            "wire equality intentionally compares the serialized projection only",
        );

        let snapshots = mux.iter_clients();
        assert_eq!(snapshots.len(), 1);
        assert_eq!(snapshots[0].focused_pane_id, Some(782));
        assert!(
            !Arc::ptr_eq(&snapshots[0].client_id, &client),
            "wire snapshots must not leak the exact process-local client token",
        );
        assert!(
            !mux.client_registration_is_current(&snapshots[0].client_id),
            "wire metadata must not carry live client mutation authority",
        );
        assert!(
            snapshots[0].focused_pane_registration().is_none(),
            "iter_clients must return a wire-safe projection",
        );

        drop(mux);
        drop(pane);
        assert!(
            weak_mux.upgrade().is_none(),
            "focused client records and wire snapshots must not retain the mux",
        );
        assert!(
            weak_pane.upgrade().is_none(),
            "focused client records and wire snapshots must not retain the pane",
        );
    }

    #[test]
    fn identity_holder_restores_originating_mux_after_global_replacement() {
        let _guard = global_test_lock();
        Mux::shutdown();

        let originating_mux = Arc::new(Mux::new(None));
        let replacement_mux = Arc::new(Mux::new(None));
        let prior_identity = Arc::new(ClientId::new());
        let temporary_identity = Arc::new(ClientId::new());
        let replacement_identity = Arc::new(ClientId::new());

        originating_mux.replace_identity(Some(Arc::clone(&prior_identity)));
        replacement_mux.replace_identity(Some(Arc::clone(&replacement_identity)));
        Mux::set_mux(&originating_mux);

        let holder = originating_mux.with_identity(Some(Arc::clone(&temporary_identity)));
        assert!(
            originating_mux
                .active_identity()
                .as_ref()
                .is_some_and(|identity| Arc::ptr_eq(identity, &temporary_identity)),
            "with_identity must install the temporary identity on its owner",
        );

        Mux::set_mux(&replacement_mux);
        drop(holder);

        assert!(
            originating_mux
                .active_identity()
                .as_ref()
                .is_some_and(|identity| Arc::ptr_eq(identity, &prior_identity)),
            "dropping the holder must restore its originating mux",
        );
        assert!(
            replacement_mux
                .active_identity()
                .as_ref()
                .is_some_and(|identity| Arc::ptr_eq(identity, &replacement_identity)),
            "holder cleanup must not mutate the replacement global mux",
        );
        assert!(
            Mux::try_get().is_some_and(|mux| Arc::ptr_eq(&mux, &replacement_mux)),
            "holder cleanup must not replace the process-global mux",
        );

        Mux::shutdown();
    }

    #[test]
    fn pane_output_without_scheduler_preserves_synchronous_notify_contract() {
        let mux = Arc::new(Mux::new(None));
        let pane = register_test_pane(&mux, 7);
        let pane_outputs = Arc::new(Mutex::new(Vec::new()));
        let observed = Arc::clone(&pane_outputs);
        mux.subscribe(move |notification| {
            if let MuxNotification::PaneOutput(pane_id) = notification {
                observed.lock().push(pane_id);
            }
            true
        })
        .expect("test mux subscription should allocate an identifier");

        assert!(mux.enqueue_pane_output_notification_for_pane_with_scheduler_state(&pane, false));

        assert_eq!(&*pane_outputs.lock(), &[7]);
        assert!(mux.pending_pane_output.lock().notifications.is_empty());
        assert!(
            !mux.pane_output_drain_scheduled.load(Ordering::Relaxed),
            "synchronous fallback must leave no stranded drain lease",
        );
    }

    #[test]
    fn scheduled_pane_output_drain_remains_bound_to_originating_mux() {
        let _guard = global_test_lock();
        Mux::shutdown();
        let executor = promise::spawn::SimpleExecutor::new();
        let originating_mux = Arc::new(Mux::new(None));
        let replacement_mux = Arc::new(Mux::new(None));
        let _pane = register_test_pane(&originating_mux, 7);
        let pane_outputs = Arc::new(Mutex::new(Vec::new()));
        let observed = Arc::clone(&pane_outputs);
        originating_mux
            .subscribe(move |notification| {
                if let MuxNotification::PaneOutput(pane_id) = notification {
                    observed.lock().push(pane_id);
                }
                true
            })
            .expect("test mux subscription should allocate an identifier");

        Mux::set_mux(&originating_mux);
        originating_mux.enqueue_pane_output_notification(7);
        Mux::set_mux(&replacement_mux);
        executor
            .tick()
            .expect("scheduled pane-output drain should run");

        assert_eq!(
            &*pane_outputs.lock(),
            &[7],
            "mux replacement must not redirect an already-scheduled output drain",
        );
        assert!(originating_mux
            .pending_pane_output
            .lock()
            .notifications
            .is_empty());
        assert!(
            !originating_mux
                .pane_output_drain_scheduled
                .load(Ordering::Relaxed),
            "originating mux must not retain a permanently scheduled drain lease",
        );
        Mux::shutdown();
    }

    #[test]
    fn scheduled_pane_output_drain_does_not_retain_destroyed_mux() {
        let _guard = global_test_lock();
        Mux::shutdown();
        let executor = promise::spawn::SimpleExecutor::new();
        let mux = Arc::new(Mux::new(None));
        let _pane = register_test_pane(&mux, 7);
        let pane_outputs = Arc::new(Mutex::new(Vec::new()));
        let observed = Arc::clone(&pane_outputs);
        mux.subscribe(move |notification| {
            if let MuxNotification::PaneOutput(pane_id) = notification {
                observed.lock().push(pane_id);
            }
            true
        })
        .expect("test mux subscription should allocate an identifier");

        mux.enqueue_pane_output_notification(7);
        let weak_mux = Arc::downgrade(&mux);
        assert_eq!(
            Arc::strong_count(&mux),
            1,
            "a deferred pane-output drain must retain only a weak mux owner",
        );
        drop(mux);
        assert!(
            weak_mux.upgrade().is_none(),
            "pane registrations and deferred drain state must not form a strong mux cycle",
        );
        executor
            .tick()
            .expect("scheduled pane-output drain should run");

        assert!(
            pane_outputs.lock().is_empty(),
            "a deferred drain must not retain or notify a destroyed mux",
        );
    }

    #[test]
    fn pane_output_notifications_coalesce_until_flushed() {
        let _guard = global_test_lock();
        Mux::shutdown();
        let executor = promise::spawn::SimpleExecutor::new();
        let mux = Arc::new(Mux::new(None));
        Mux::set_mux(&mux);
        let _pane_7 = register_test_pane(&mux, 7);
        let _pane_8 = register_test_pane(&mux, 8);
        let pane_outputs = Arc::new(Mutex::new(Vec::new()));
        let observed = Arc::clone(&pane_outputs);
        mux.subscribe(move |notification| {
            if let MuxNotification::PaneOutput(pane_id) = notification {
                observed.lock().push(pane_id);
            }
            true
        })
        .expect("test mux subscription should allocate an identifier");

        mux.enqueue_pane_output_notification(7);
        mux.enqueue_pane_output_notification(7);
        mux.enqueue_pane_output_notification(8);

        {
            let pending = mux.pending_pane_output.lock();
            assert_eq!(
                pending
                    .notifications
                    .iter()
                    .map(|notification| notification.pane_id)
                    .collect::<Vec<_>>(),
                vec![7, 8]
            );
            assert!(pending.queued.contains_key(&7));
            assert!(pending.queued.contains_key(&8));
        }
        assert!(pane_outputs.lock().is_empty());

        mux.flush_pending_pane_output_notifications();

        assert_eq!(&*pane_outputs.lock(), &[7, 8]);
        let pending = mux.pending_pane_output.lock();
        assert!(pending.notifications.is_empty());
        assert!(pending.queued.is_empty());
        assert!(
            !mux.pane_output_drain_scheduled.load(Ordering::Relaxed),
            "flush should clear the scheduled flag once the queue is empty",
        );
        drop(pending);

        executor
            .tick()
            .expect("scheduled pane-output drain should run");
        Mux::shutdown();
    }

    #[test]
    fn removal_force_seals_accepted_output_without_erasing_other_batches() {
        let _guard = global_test_lock();
        Mux::shutdown();
        let executor = promise::spawn::SimpleExecutor::new();
        let mux = Arc::new(Mux::new(None));
        Mux::set_mux(&mux);
        let _pane_7 = register_test_pane(&mux, 7);
        let _pane_8 = register_test_pane(&mux, 8);
        let pane_outputs = Arc::new(Mutex::new(Vec::new()));
        let observed = Arc::clone(&pane_outputs);
        mux.subscribe(move |notification| {
            if let MuxNotification::PaneOutput(pane_id) = notification {
                observed.lock().push(pane_id);
            }
            true
        })
        .expect("test mux subscription should allocate an identifier");

        mux.enqueue_pane_output_notification(7);
        mux.enqueue_pane_output_notification(8);
        mux.remove_pane(7);

        {
            let pending = mux.pending_pane_output.lock();
            assert_eq!(
                pending
                    .notifications
                    .iter()
                    .map(|notification| notification.pane_id)
                    .collect::<Vec<_>>(),
                vec![7, 8]
            );
            assert!(!pending.queued.contains_key(&7));
            assert!(pending.queued.contains_key(&8));
        }
        assert_eq!(
            &*pane_outputs.lock(),
            &[7],
            "removal must publish output accepted before its lifecycle transition",
        );

        mux.flush_pending_pane_output_notifications();

        assert_eq!(
            &*pane_outputs.lock(),
            &[7, 8],
            "the unrelated open batch must remain independently drainable",
        );

        executor
            .tick()
            .expect("scheduled pane-output drain should run");
        Mux::shutdown();
    }

    #[test]
    fn tab_removal_force_seals_output_before_removed_and_preserves_unrelated_batch() {
        let _guard = global_test_lock();
        Mux::shutdown();
        let executor = promise::spawn::SimpleExecutor::new();
        let mux = Arc::new(Mux::new(None));
        Mux::set_mux(&mux);
        let (tab, tab_pane_kills) = tab_with_kill_counter(&mux, 140);
        let _unrelated_pane = register_test_pane(&mux, 141);
        let events = Arc::new(Mutex::new(Vec::new()));
        let observed = Arc::clone(&events);
        mux.subscribe(move |notification| {
            match notification {
                MuxNotification::PaneOutput(140) => observed.lock().push("tab-output"),
                MuxNotification::PaneRemoved(140) => observed.lock().push("tab-removed"),
                MuxNotification::PaneOutput(141) => observed.lock().push("unrelated-output"),
                _ => {}
            }
            true
        })
        .expect("test mux subscription should allocate an identifier");

        mux.enqueue_pane_output_notification(140);
        mux.enqueue_pane_output_notification(141);
        mux.remove_tab(tab.tab_id())
            .expect("tab and its exact pane registration should be removed");

        assert_eq!(
            &*events.lock(),
            &["tab-output", "tab-removed"],
            "accepted tab output must publish before its removal lifecycle",
        );
        assert_eq!(tab_pane_kills.load(Ordering::SeqCst), 1);
        {
            let pending = mux.pending_pane_output.lock();
            assert!(!pending.queued.contains_key(&140));
            assert!(pending.queued.contains_key(&141));
        }

        mux.flush_pending_pane_output_notifications();
        assert_eq!(
            &*events.lock(),
            &["tab-output", "tab-removed", "unrelated-output"],
            "tab cleanup must not erase another pane's independently open batch",
        );

        executor
            .tick()
            .expect("scheduled pane-output drain should run");
        Mux::shutdown();
    }

    #[test]
    fn remove_pane_preserves_already_accepted_output_before_removed() {
        let _guard = global_test_lock();
        Mux::shutdown();
        let executor = promise::spawn::SimpleExecutor::new();
        let mux = Arc::new(Mux::new(None));
        Mux::set_mux(&mux);
        let _pane = register_test_pane(&mux, 7);
        let pane_outputs = Arc::new(Mutex::new(Vec::new()));
        let observed = Arc::clone(&pane_outputs);
        mux.subscribe(move |notification| {
            if let MuxNotification::PaneOutput(pane_id) = notification {
                observed.lock().push(pane_id);
            }
            true
        })
        .expect("test mux subscription should allocate an identifier");

        mux.enqueue_pane_output_notification(7);
        mux.remove_pane(7);

        {
            let pending = mux.pending_pane_output.lock();
            assert!(
                !pending.notifications.is_empty(),
                "the scheduled vector may retain an already sealed exact batch",
            );
            assert!(
                pending.queued.is_empty(),
                "removal must close the exact generation's open batch",
            );
        }
        assert_eq!(&*pane_outputs.lock(), &[7]);

        mux.flush_pending_pane_output_notifications();
        executor
            .tick()
            .expect("scheduled pane-output drain should run");
        assert_eq!(
            pane_outputs.lock().as_slice(),
            [7],
            "an accepted output batch must publish exactly once before PaneRemoved",
        );
        Mux::shutdown();
    }

    #[test]
    fn pane_output_reentrant_enqueue_is_drained_before_returning() {
        let _guard = global_test_lock();
        Mux::shutdown();
        let executor = promise::spawn::SimpleExecutor::new();
        let mux = Arc::new(Mux::new(None));
        Mux::set_mux(&mux);
        let _pane_7 = register_test_pane(&mux, 7);
        let _pane_8 = register_test_pane(&mux, 8);
        let pane_outputs = Arc::new(Mutex::new(Vec::new()));
        let reentered = Arc::new(AtomicBool::new(false));

        let mux_for_subscriber = Arc::clone(&mux);
        let pane_outputs_for_subscriber = Arc::clone(&pane_outputs);
        let reentered_for_subscriber = Arc::clone(&reentered);
        mux.subscribe(move |notification| {
            if let MuxNotification::PaneOutput(pane_id) = notification {
                pane_outputs_for_subscriber.lock().push(pane_id);
                if pane_id == 7 && !reentered_for_subscriber.swap(true, Ordering::Relaxed) {
                    mux_for_subscriber.enqueue_pane_output_notification(8);
                }
            }
            true
        })
        .expect("test mux subscription should allocate an identifier");

        mux.enqueue_pane_output_notification(7);
        mux.flush_pending_pane_output_notifications();

        assert_eq!(
            &*pane_outputs.lock(),
            &[7, 8],
            "reentrant pane-output enqueue should be drained before the current flush returns",
        );
        let pending = mux.pending_pane_output.lock();
        assert!(pending.notifications.is_empty());
        assert!(pending.queued.is_empty());
        assert!(
            !mux.pane_output_drain_scheduled.load(Ordering::Relaxed),
            "flush should clear the scheduled flag after draining reentrant enqueues",
        );
        drop(pending);

        executor
            .tick()
            .expect("scheduled pane-output drain should run");
        Mux::shutdown();
    }

    #[test]
    fn pane_output_from_stale_same_id_instance_is_rejected() {
        let _guard = global_test_lock();
        Mux::shutdown();
        let executor = promise::spawn::SimpleExecutor::new();
        let mux = Arc::new(Mux::new(None));
        Mux::set_mux(&mux);
        let stale = register_test_pane(&mux, 7);
        let pane_outputs = Arc::new(Mutex::new(Vec::new()));
        let observed = Arc::clone(&pane_outputs);
        mux.subscribe(move |notification| {
            if let MuxNotification::PaneOutput(pane_id) = notification {
                observed.lock().push(pane_id);
            }
            true
        })
        .expect("test mux subscription should allocate an identifier");

        mux.remove_pane_if_same(7, &stale);
        let replacement = register_test_pane(&mux, 7);
        assert!(
            !mux.enqueue_pane_output_notification_for_pane_with_scheduler_state(&stale, true),
            "an old reader must not attribute output to a same-id replacement",
        );
        assert!(
            mux.enqueue_pane_output_notification_for_pane_with_scheduler_state(&replacement, true),
            "the exact replacement instance should retain output authority",
        );
        mux.flush_pending_pane_output_notifications();

        assert_eq!(&*pane_outputs.lock(), &[7]);
        executor
            .tick()
            .expect("scheduled pane-output drain should run");
        Mux::shutdown();
    }

    #[test]
    fn removal_during_output_batch_preserves_already_accepted_output() {
        let _guard = global_test_lock();
        Mux::shutdown();
        let executor = promise::spawn::SimpleExecutor::new();
        let mux = Arc::new(Mux::new(None));
        Mux::set_mux(&mux);
        let _pane_7 = register_test_pane(&mux, 7);
        let _pane_8 = register_test_pane(&mux, 8);
        let pane_outputs = Arc::new(Mutex::new(Vec::new()));
        let observed = Arc::clone(&pane_outputs);
        let mux_for_subscriber = Arc::clone(&mux);
        mux.subscribe(move |notification| {
            if let MuxNotification::PaneOutput(pane_id) = notification {
                observed.lock().push(pane_id);
                if pane_id == 7 {
                    mux_for_subscriber.remove_pane(8);
                }
            }
            true
        })
        .expect("test mux subscription should allocate an identifier");

        mux.enqueue_pane_output_notification(7);
        mux.enqueue_pane_output_notification(8);
        mux.flush_pending_pane_output_notifications();

        assert_eq!(
            &*pane_outputs.lock(),
            &[7, 8],
            "an output accepted before removal must precede PaneRemoved for that pane",
        );
        assert!(mux.get_pane(8).is_none());
        executor
            .tick()
            .expect("scheduled pane-output drain should run");
        Mux::shutdown();
    }

    #[test]
    fn resolve_spawn_tab_domain_reports_missing_default_domain() {
        let mux = Mux::new(None);

        assert_eq!(
            mux.resolve_spawn_tab_domain(None, &SpawnTabDomain::DefaultDomain)
                .map(|domain| domain.domain_id())
                .map_err(|error| error.to_string()),
            Err("no default domain configured".to_string()),
        );
        assert_eq!(
            mux.resolve_spawn_tab_domain(None, &SpawnTabDomain::CurrentPaneDomain)
                .map(|domain| domain.domain_id())
                .map_err(|error| error.to_string()),
            Err("no default domain configured".to_string()),
        );
    }

    #[test]
    fn window_builder_drop_after_mux_shutdown_does_not_panic() {
        let _guard = global_test_lock();
        Mux::shutdown();

        let mux = Arc::new(Mux::new(None));
        Mux::set_mux(&mux);
        let window_builder = mux.new_empty_window(None, None);

        Mux::shutdown();
        drop(window_builder);
    }

    #[test]
    fn explicit_removal_of_unpublished_window_suppresses_both_lifecycle_edges() {
        let _guard = global_test_lock();
        Mux::shutdown();

        let mux = Arc::new(Mux::new(None));
        Mux::set_mux(&mux);
        let lifecycles = Arc::new(Mutex::new(Vec::new()));
        let lifecycles_for_subscriber = Arc::clone(&lifecycles);
        mux.subscribe(move |notification| {
            match notification {
                MuxNotification::WindowCreated(window_id) => {
                    lifecycles_for_subscriber
                        .lock()
                        .push(("created", window_id));
                }
                MuxNotification::WindowRemoved(window_id) => {
                    lifecycles_for_subscriber
                        .lock()
                        .push(("removed", window_id));
                }
                _ => {}
            }
            true
        })
        .expect("provisional lifecycle subscriber should allocate an identifier");

        let window_builder = mux.new_empty_window(None, None);
        let window_id = *window_builder;
        mux.kill_window(window_id);
        drop(window_builder);

        assert!(
            lifecycles.lock().is_empty(),
            "an unpublished provisional window must emit neither Removed nor a later Created"
        );
        Mux::shutdown();
    }

    #[test]
    fn pruning_never_reaps_an_unpublished_provisional_window() {
        let _guard = global_test_lock();
        Mux::shutdown();

        let mux = Arc::new(Mux::new(None));
        Mux::set_mux(&mux);
        let window_builder = mux.new_empty_window(None, None);
        let window_id = *window_builder;

        mux.prune_dead_windows();
        assert!(
            mux.get_window(window_id).is_some(),
            "activity-aware pruning must preserve the provisional window",
        );

        mux.prune_dead_windows_ignoring_activity();
        assert!(
            mux.get_window(window_id).is_some(),
            "pane-retirement pruning must not bypass provisional publication authority",
        );
        assert!(
            mux.provisional_windows.lock().contains(&window_id),
            "pruning must not consume the builder's publication marker",
        );

        window_builder.cancel();
        assert!(
            mux.get_window(window_id).is_none(),
            "the exact builder must retain cancellation authority after both prune paths",
        );
        Mux::shutdown();
    }

    #[test]
    fn window_builder_non_main_drop_without_scheduler_notifies() {
        let _guard = global_test_lock();
        Mux::shutdown();
        if promise::spawn::is_scheduler_configured() {
            return;
        }

        let mux = Arc::new(Mux::new(None));
        Mux::set_mux(&mux);
        let seen = Arc::new(AtomicUsize::new(0));
        let seen_for_subscriber = Arc::clone(&seen);
        mux.subscribe(move |notification| {
            if matches!(notification, MuxNotification::WindowCreated(_)) {
                seen_for_subscriber.fetch_add(1, Ordering::SeqCst);
            }
            true
        })
        .expect("test mux subscription should allocate an identifier");

        let mux_for_thread = Arc::clone(&mux);
        let handle = std::thread::spawn(move || {
            let window_builder = mux_for_thread.new_empty_window(None, None);
            drop(window_builder);
        });
        handle
            .join()
            .expect("window builder thread should not panic");

        assert_eq!(seen.load(Ordering::SeqCst), 1);
        Mux::shutdown();
    }

    #[test]
    fn new_empty_window_without_global_mux_uses_instance_workspace() {
        let _guard = global_test_lock();
        Mux::shutdown();

        let mux = Arc::new(Mux::new(None));
        let window_builder = mux.new_empty_window(None, None);
        let window_id = *window_builder;

        {
            let mut window = mux
                .get_window_mut(window_id)
                .expect("new_empty_window should register the window");
            assert_eq!(window.get_workspace(), DEFAULT_WORKSPACE);
            window.set_workspace("workspace-without-global-mux");
            assert_eq!(window.get_workspace(), "workspace-without-global-mux");
        }

        drop(window_builder);
        Mux::shutdown();
    }

    #[test]
    fn window_topology_notification_runs_after_window_lock_released() {
        let _guard = global_test_lock();
        Mux::shutdown();

        let executor = BoundedTestExecutor::new();
        let mux = Arc::new(Mux::new(None));
        Mux::set_mux(&mux);

        let window_builder = mux.new_empty_window(None, None);
        let window_id = *window_builder;
        let observed = Arc::new(AtomicBool::new(false));
        let observed_for_subscriber = Arc::clone(&observed);
        let mux_for_subscriber = Arc::clone(&mux);
        mux.subscribe(move |notification| {
            if let MuxNotification::WindowTopologyChanged(change) = notification {
                if change.affects_window(window_id) {
                    assert!(mux_for_subscriber.get_window(window_id).is_some());
                    observed_for_subscriber.store(true, Ordering::Relaxed);
                }
            }
            true
        })
        .expect("test mux subscription should allocate an identifier");

        let size = frankenterm_term::TerminalSize {
            rows: 1,
            cols: 1,
            pixel_width: 1,
            pixel_height: 1,
            dpi: 96,
        };
        let tab = Arc::new(Tab::new(&size));
        mux.add_tab_no_panes(&tab)
            .expect("test tab should register");
        let mux_for_thread = Arc::clone(&mux);
        let tab_for_thread = Arc::clone(&tab);
        std::thread::spawn(move || {
            mux_for_thread
                .add_tab_to_window(&tab_for_thread, window_id)
                .expect("tab should be added to test window");
        })
        .join()
        .expect("off-main window mutation should not panic");

        assert!(!observed.load(Ordering::Relaxed));
        executor.run_until(Duration::from_secs(5), || observed.load(Ordering::Relaxed));
        assert!(observed.load(Ordering::Relaxed));

        drop(window_builder);
        Mux::shutdown();
    }

    #[test]
    fn window_notifications_are_fifo_and_allow_reentrant_window_mutation() {
        let _guard = global_test_lock();
        Mux::shutdown();

        let _executor = BoundedTestExecutor::new();
        let mux = Arc::new(Mux::new(None));
        Mux::set_mux(&mux);
        let window_builder = mux.new_empty_window(None, None);
        let window_id = *window_builder;

        let events = Arc::new(Mutex::new(Vec::new()));
        let events_for_subscriber = Arc::clone(&events);
        let did_reenter = Arc::new(AtomicBool::new(false));
        let did_reenter_for_subscriber = Arc::clone(&did_reenter);
        let mux_for_subscriber = Arc::clone(&mux);
        mux.subscribe(move |notification| {
            match notification {
                MuxNotification::WindowTopologyChanged(change)
                    if change.affects_window(window_id) =>
                {
                    events_for_subscriber.lock().push("topology");
                    if !did_reenter_for_subscriber.swap(true, Ordering::SeqCst) {
                        let mut window = mux_for_subscriber
                            .get_window_mut(window_id)
                            .expect("subscriber must re-enter after the map lock is released");
                        window.set_workspace("reentrant-workspace");
                    }
                }
                MuxNotification::WindowWorkspaceChanged {
                    window_id: id,
                    workspace,
                } if id == window_id && workspace == "reentrant-workspace" => {
                    events_for_subscriber.lock().push("workspace-changed");
                }
                _ => {}
            }
            true
        })
        .expect("test mux subscription should allocate an identifier");

        let tab = Arc::new(Tab::new(&test_size()));
        mux.add_tab_no_panes(&tab)
            .expect("test tab should register");
        mux.add_tab_to_window(&tab, window_id)
            .expect("tab should be added to test window");

        assert_eq!(
            &*events.lock(),
            &["topology", "workspace-changed"],
            "a reentrant event appends behind notifications already admitted at the mutation point",
        );
        assert_eq!(
            mux.get_window(window_id)
                .expect("test window should remain registered")
                .get_workspace(),
            "reentrant-workspace",
        );

        drop(window_builder);
        Mux::shutdown();
    }

    #[test]
    fn deferred_workspace_notifications_retain_each_mutation_payload() {
        let _guard = global_test_lock();
        Mux::shutdown();

        let executor = BoundedTestExecutor::new();
        let mux = Arc::new(Mux::new(None));
        Mux::set_mux(&mux);
        let window_builder = mux.new_empty_window(None, None);
        let window_id = *window_builder;

        let workspaces = Arc::new(Mutex::new(Vec::new()));
        let workspaces_for_subscriber = Arc::clone(&workspaces);
        mux.subscribe(move |notification| {
            if let MuxNotification::WindowWorkspaceChanged {
                window_id: id,
                workspace,
            } = notification
            {
                if id == window_id {
                    workspaces_for_subscriber.lock().push(workspace);
                }
            }
            true
        })
        .expect("test mux subscription should allocate an identifier");

        let mux_for_thread = Arc::clone(&mux);
        std::thread::spawn(move || {
            mux_for_thread
                .get_window_mut(window_id)
                .expect("test window should remain registered")
                .set_workspace("first-workspace");
            mux_for_thread
                .get_window_mut(window_id)
                .expect("test window should remain registered")
                .set_workspace("second-workspace");
        })
        .join()
        .expect("off-main workspace mutations should not panic");

        assert!(
            workspaces.lock().is_empty(),
            "the bounded executor must retain both callbacks before dispatch",
        );
        executor.run_until(Duration::from_secs(5), || workspaces.lock().len() == 2);
        assert_eq!(
            workspaces.lock().as_slice(),
            &[
                "first-workspace".to_string(),
                "second-workspace".to_string(),
            ],
            "each queued revision must retain the workspace written at its own mutation point",
        );

        drop(window_builder);
        Mux::shutdown();
    }

    #[test]
    fn window_notification_fifo_stays_bound_to_origin_across_global_mux_swap() {
        let _guard = global_test_lock();
        Mux::shutdown();

        let _executor = BoundedTestExecutor::new();
        let origin = Arc::new(Mux::new(None));
        let replacement = Arc::new(Mux::new(None));
        let window_builder = origin.new_empty_window(None, None);
        let window_id = *window_builder;

        let origin_events = Arc::new(Mutex::new(Vec::new()));
        let origin_events_for_subscriber = Arc::clone(&origin_events);
        origin
            .subscribe(move |notification| {
                match notification {
                    MuxNotification::WindowTopologyChanged(change)
                        if change.affects_window(window_id) =>
                    {
                        origin_events_for_subscriber.lock().push("topology");
                    }
                    _ => {}
                }
                true
            })
            .expect("origin subscription");

        let replacement_events = Arc::new(AtomicUsize::new(0));
        let replacement_events_for_subscriber = Arc::clone(&replacement_events);
        replacement
            .subscribe(move |notification| {
                if matches!(
                    notification,
                    MuxNotification::WindowInvalidated(_)
                        | MuxNotification::WindowTopologyChanged(_)
                        | MuxNotification::TabAddedToWindow { .. }
                        | MuxNotification::WindowCreated(_)
                ) {
                    replacement_events_for_subscriber.fetch_add(1, Ordering::SeqCst);
                }
                true
            })
            .expect("replacement subscription");

        Mux::set_mux(&replacement);
        let tab = Arc::new(Tab::new(&test_size()));
        origin
            .add_tab_no_panes(&tab)
            .expect("origin tab should register");
        origin
            .add_tab_to_window(&tab, window_id)
            .expect("origin window should accept the tab");
        drop(window_builder);

        assert_eq!(&*origin_events.lock(), &["topology"],);
        assert_eq!(replacement_events.load(Ordering::SeqCst), 0);
        Mux::shutdown();
    }

    #[test]
    fn deferred_window_focus_loss_targets_the_exact_pane_at_removal() {
        let _guard = global_test_lock();
        Mux::shutdown();

        let executor = BoundedTestExecutor::new();
        let mux = Arc::new(Mux::new(None));
        Mux::set_mux(&mux);
        let window_builder = mux.new_empty_window(None, None);
        let window_id = *window_builder;

        let first_tab = Arc::new(Tab::new(&test_size()));
        let (first_pane, first_focus) = KillCountingPane::new_with_focus_counter(451, test_size());
        first_tab.assign_pane(&first_pane);
        mux.add_tab_and_active_pane(&first_tab)
            .expect("first tab and pane should publish");

        let second_tab = Arc::new(Tab::new(&test_size()));
        let (second_pane, second_focus) =
            KillCountingPane::new_with_focus_counter(452, test_size());
        second_tab.assign_pane(&second_pane);
        mux.add_tab_and_active_pane(&second_tab)
            .expect("second tab and pane should publish");

        mux.add_tab_to_window(&first_tab, window_id)
            .expect("first tab should enter the window");
        mux.add_tab_to_window(&second_tab, window_id)
            .expect("second tab should enter the window");

        let mux_for_thread = Arc::clone(&mux);
        let first_tab_for_thread = Arc::clone(&first_tab);
        std::thread::spawn(move || {
            assert!(
                mux_for_thread.remove_tab_local_only_if_same(&first_tab_for_thread),
                "exact test tab should retire through the mux transaction"
            );
        })
        .join()
        .expect("off-main mux-owned active-tab removal should not panic");

        let (later_pane, later_focus) = KillCountingPane::new_with_focus_counter(453, test_size());
        first_tab
            .add_floating_pane(
                later_pane,
                crate::tab::FloatingPaneRect {
                    left: 1,
                    top: 1,
                    width: 20,
                    height: 10,
                },
            )
            .expect("removed tab should accept a later active pane");
        first_focus.lock().clear();
        later_focus.lock().clear();

        executor.run_until(Duration::from_secs(5), || !first_focus.lock().is_empty());
        assert_eq!(
            &*first_focus.lock(),
            &[false],
            "the pane active at removal linearization must receive one focus loss",
        );
        assert!(
            later_focus.lock().is_empty(),
            "changing the removed tab before drain must not retarget focus loss",
        );
        assert!(
            second_focus.lock().is_empty(),
            "the newly active tab must not receive the prior tab's focus loss",
        );

        drop(window_builder);
        Mux::shutdown();
    }

    #[test]
    fn provisional_cancel_publishes_topology_that_won_the_race() {
        let _guard = global_test_lock();
        Mux::shutdown();

        let _executor = BoundedTestExecutor::new();
        let mux = Arc::new(Mux::new(None));
        Mux::set_mux(&mux);
        let window_builder = mux.new_empty_window(None, None);
        let window_id = *window_builder;
        let created_events = Arc::new(AtomicUsize::new(0));
        let created_events_for_subscriber = Arc::clone(&created_events);
        let removed_events = Arc::new(AtomicUsize::new(0));
        let removed_events_for_subscriber = Arc::clone(&removed_events);
        mux.subscribe(move |notification| {
            match notification {
                MuxNotification::WindowTopologyChanged(change)
                    if change.created_windows().binary_search(&window_id).is_ok() =>
                {
                    created_events_for_subscriber.fetch_add(1, Ordering::SeqCst);
                }
                MuxNotification::WindowRemoved(id) if id == window_id => {
                    removed_events_for_subscriber.fetch_add(1, Ordering::SeqCst);
                }
                _ => {}
            }
            true
        })
        .expect("test mux subscription should allocate an identifier");

        let tab = Arc::new(Tab::new(&test_size()));
        mux.add_tab_no_panes(&tab)
            .expect("racing tab should register");
        mux.add_tab_to_window(&tab, window_id)
            .expect("racing tab should acquire the provisional window");
        window_builder.cancel();

        assert!(
            mux.get_window(window_id)
                .is_some_and(|window| window.iter().any(|candidate| Arc::ptr_eq(candidate, &tab))),
            "cancellation must not silently tear down topology that raced into the window",
        );
        assert_eq!(
            created_events.load(Ordering::SeqCst),
            1,
            "a surviving window must publish exactly one frozen creation transaction",
        );
        assert_eq!(
            removed_events.load(Ordering::SeqCst),
            0,
            "publishing raced live topology must not synthesize removal",
        );
        Mux::shutdown();
    }

    #[test]
    fn move_tab_between_windows_preserves_live_tab_and_pane() {
        let _guard = global_test_lock();
        Mux::shutdown();

        let mux = Arc::new(Mux::new(None));
        Mux::set_mux(&mux);
        let src_window = mux.new_empty_window(Some("winunify".to_string()), None);
        let src_window_id = *src_window;
        let dst_window = mux.new_empty_window(Some("winunify".to_string()), None);
        let dst_window_id = *dst_window;
        let (tab, kills) = tab_with_kill_counter(&mux, 401);
        let tab_id = tab.tab_id();
        mux.add_tab_to_window(&tab, src_window_id)
            .expect("tab should start in source window");
        mux.assert_tab_parent_index_matches_windows();

        let move_events = Arc::new(Mutex::new(Vec::new()));
        let move_events_for_subscriber = Arc::clone(&move_events);
        mux.subscribe_with_topology(move |envelope| {
            if matches!(
                &envelope.notification,
                MuxNotification::WindowTopologyChanged(_)
            ) {
                move_events_for_subscriber.lock().push(envelope);
            }
            true
        })
        .expect("cross-window transaction subscriber");

        mux.move_tab_between_windows(tab_id, dst_window_id, Some(0))
            .expect("metadata move should succeed");
        mux.assert_tab_parent_index_matches_windows();

        let events = move_events.lock();
        assert_eq!(
            events.len(),
            1,
            "a cross-window move must publish one frozen transaction"
        );
        let envelope = &events[0];
        assert!(matches!(envelope.topology, MuxTopologyStamp::Revision(_)));
        let MuxNotification::WindowTopologyChanged(change) = &envelope.notification else {
            unreachable!("subscriber retained only frozen window transactions");
        };
        assert_eq!(
            change
                .windows()
                .iter()
                .map(FrozenWindowOrder::window_id)
                .collect::<Vec<_>>(),
            {
                let mut ids = vec![src_window_id, dst_window_id];
                ids.sort_unstable();
                ids
            },
            "both post-commit windows must share one deterministic payload",
        );
        assert_eq!(change.attached_tabs(), &[(tab_id, dst_window_id)]);
        assert_eq!(change.created_windows(), &[dst_window_id]);
        assert!(change.removed_windows().is_empty());
        drop(events);

        assert_eq!(mux.window_containing_tab(tab_id), Some(dst_window_id));
        assert!(
            mux.get_tab(tab_id)
                .map(|stored| Arc::ptr_eq(&stored, &tab))
                .unwrap_or(false),
            "move must keep the same live Arc<Tab> in the mux registry",
        );
        assert!(
            mux.get_pane(401).is_some(),
            "move must keep the tab's pane registered",
        );
        assert_eq!(
            kills.load(Ordering::SeqCst),
            0,
            "metadata move must not kill the pane",
        );

        drop(dst_window);
        drop(src_window);
        Mux::shutdown();
    }

    #[test]
    fn same_window_tab_move_preserves_exact_active_and_stack_identity() {
        let _guard = global_test_lock();
        Mux::shutdown();

        let mux = Arc::new(Mux::new(None));
        Mux::set_mux(&mux);
        let window_builder = mux.new_empty_window(Some("same-window-order".to_string()), None);
        let window_id = *window_builder;
        let first = Arc::new(Tab::new(&test_size()));
        let active = Arc::new(Tab::new(&test_size()));
        let third = Arc::new(Tab::new(&test_size()));
        let stack_id = crate::tab::TabStackId(83);

        for tab in [&first, &active, &third] {
            mux.add_tab_no_panes(tab).expect("register exact tab");
            mux.add_tab_to_window(tab, window_id)
                .expect("attach exact tab");
        }
        {
            let mut window = mux
                .get_window_mut(window_id)
                .expect("window should remain registered");
            window.set_active_without_saving(1);
            window
                .create_tab_stack(
                    stack_id,
                    vec![first.tab_id(), active.tab_id(), third.tab_id()],
                )
                .expect("create stack before reorder");
        }
        let active_id = active.tab_id();
        let move_events = Arc::new(AtomicUsize::new(0));
        let move_events_for_subscriber = Arc::clone(&move_events);
        mux.subscribe(move |notification| {
            if matches!(
                notification,
                MuxNotification::WindowTopologyChanged(change)
                    if change.affects_window(window_id)
            ) {
                move_events_for_subscriber.fetch_add(1, Ordering::SeqCst);
            }
            true
        })
        .expect("subscribe to exact reorder event");

        mux.move_tab_between_windows(active_id, window_id, Some(1))
            .expect("same-index move is a successful no-op");
        assert_eq!(
            move_events.load(Ordering::SeqCst),
            0,
            "same-index move must not publish a false topology mutation",
        );

        mux.move_tab_between_windows(first.tab_id(), window_id, Some(2))
            .expect("move inactive tab right");
        {
            let window = mux
                .get_window(window_id)
                .expect("window should remain registered");
            assert_eq!(
                window.iter().map(Arc::as_ptr).collect::<Vec<_>>(),
                vec![
                    Arc::as_ptr(&active),
                    Arc::as_ptr(&third),
                    Arc::as_ptr(&first),
                ],
            );
            assert!(window
                .get_active()
                .is_some_and(|tab| Arc::ptr_eq(tab, &active)));
        }

        mux.move_tab_between_windows(active_id, window_id, Some(2))
            .expect("move active tab right");
        mux.move_tab_between_windows(active_id, window_id, Some(0))
            .expect("move active tab left");
        {
            let window = mux
                .get_window(window_id)
                .expect("window should remain registered");
            assert_eq!(
                window.iter().map(Arc::as_ptr).collect::<Vec<_>>(),
                vec![
                    Arc::as_ptr(&active),
                    Arc::as_ptr(&third),
                    Arc::as_ptr(&first),
                ],
            );
            assert_eq!(window.get_active_idx(), 0);
            assert!(window
                .get_active()
                .is_some_and(|tab| Arc::ptr_eq(tab, &active)));
            for tab in [&first, &active, &third] {
                assert_eq!(
                    window.tab_stack_for_tab(tab.tab_id()),
                    Some(stack_id),
                    "same-window reorder must retain stack membership",
                );
            }
        }

        drop(window_builder);
        Mux::shutdown();
    }

    #[test]
    fn reorder_request_derives_authority_without_mux_or_caller_digest() {
        let session_incarnation = MuxSessionIncarnation::from_bytes([0x33; 16]);
        let mutation_id = WindowOrderMutationId::new([0x44; 16], 9);
        let request = ReorderWindowTabsRequest::try_new_v1(
            [0x11; 16],
            session_incarnation,
            7,
            WindowOrderRevision::new(5),
            vec![9, 11, 13],
            Some(11),
            mutation_id,
        )
        .expect("bounded canonical intent constructs without a mux singleton");
        let expected = canonical_window_reorder_digest_v1(
            WindowReorderDigestInputV1 {
                protocol_version: WINDOW_REORDER_PROTOCOL_VERSION_V1,
                domain_binding_id: [0x11; 16],
                session_incarnation,
                window_id: 7,
                expected_order_revision: 5,
                desired_active_tab_id: Some(11),
                mutation_id,
            },
            IntoIterator::into_iter([9_u64, 11, 13]),
        );
        assert_eq!(request.request_digest(), expected);

        let changed = ReorderWindowTabsRequest::try_new_v1(
            [0x11; 16],
            session_incarnation,
            7,
            WindowOrderRevision::new(5),
            vec![11, 9, 13],
            Some(11),
            mutation_id,
        )
        .expect("changed semantic intent remains structurally valid");
        assert_ne!(changed.request_digest(), request.request_digest());
        assert_eq!(
            ReorderWindowTabsRequest::try_new_v1(
                [0; 16],
                session_incarnation,
                7,
                WindowOrderRevision::new(5),
                vec![9],
                Some(9),
                mutation_id,
            ),
            Err(WindowReorderMalformed::InvalidDomainBindingIdentity)
        );
        assert_eq!(
            ReorderWindowTabsRequest::try_new_v1(
                [0x11; 16],
                MuxSessionIncarnation::from_bytes([0; 16]),
                7,
                WindowOrderRevision::new(5),
                vec![9],
                Some(9),
                mutation_id,
            ),
            Err(WindowReorderMalformed::InvalidSessionIncarnation)
        );
        assert_eq!(
            ReorderWindowTabsRequest::try_new_v1(
                [0x11; 16],
                session_incarnation,
                7,
                WindowOrderRevision::new(u64::MAX),
                vec![9],
                Some(9),
                mutation_id,
            ),
            Err(WindowReorderMalformed::ExpectedRevisionExhausted)
        );

        let maximum_tabs = (0..MAX_TABS_PER_ORDERED_WINDOW).collect::<Vec<_>>();
        let maximum = ReorderWindowTabsRequest::try_new_v1(
            [0x11; 16],
            session_incarnation,
            7,
            WindowOrderRevision::new(5),
            maximum_tabs.clone(),
            Some(0),
            WindowOrderMutationId::new([0x44; 16], 10),
        )
        .expect("q4096 wire representation remains bounded");
        let maximum_expected = canonical_window_reorder_digest_v1(
            WindowReorderDigestInputV1 {
                protocol_version: WINDOW_REORDER_PROTOCOL_VERSION_V1,
                domain_binding_id: [0x11; 16],
                session_incarnation,
                window_id: 7,
                expected_order_revision: 5,
                desired_active_tab_id: Some(0),
                mutation_id: WindowOrderMutationId::new([0x44; 16], 10),
            },
            maximum_tabs
                .iter()
                .map(|tab_id| u64::try_from(*tab_id).expect("bounded test id fits u64")),
        );
        assert_eq!(maximum.request_digest(), maximum_expected);
    }

    #[test]
    fn reorder_window_tabs_applies_once_and_replays_without_republication() {
        let _guard = global_test_lock();
        Mux::shutdown();

        let mux = Arc::new(Mux::new(None));
        // `reorder_window_tabs` is exact-owner API and must not depend on the
        // mutable process singleton. Keeping this fixture local also prevents
        // an unrelated deferred global task in the parallel suite from
        // advancing this mux's topology authority.
        let window_builder = mux.new_empty_window(Some("authoritative-order".to_string()), None);
        let window_id = *window_builder;
        let first = Arc::new(Tab::new(&test_size()));
        let active = Arc::new(Tab::new(&test_size()));
        let third = Arc::new(Tab::new(&test_size()));
        let stack_id = crate::tab::TabStackId(113);
        for tab in [&first, &active, &third] {
            mux.add_tab_no_panes(tab).expect("register exact tab");
            mux.add_tab_to_window(tab, window_id)
                .expect("attach exact tab");
        }
        {
            let mut window = mux.get_window_mut(window_id).expect("test window");
            window.set_active_without_saving(1);
            window
                .create_tab_stack(
                    stack_id,
                    vec![first.tab_id(), active.tab_id(), third.tab_id()],
                )
                .expect("create stack before authoritative reorder");
        }
        let stack_before = mux
            .get_window(window_id)
            .expect("test window")
            .tab_stack_entries();
        let (_, topology_before) = mux
            .topology_snapshot_authority()
            .expect("topology before reorder");
        let order_before = mux
            .window_order_snapshot(window_id)
            .expect("valid order before reorder")
            .expect("window before reorder")
            .order_revision();

        let observed = Arc::new(Mutex::new(Vec::new()));
        let observed_for_subscriber = Arc::clone(&observed);
        mux.subscribe_with_topology(move |envelope| {
            if let MuxNotification::WindowOrderChanged {
                mutation_id,
                request_digest,
                window,
            } = envelope.notification
            {
                observed_for_subscriber.lock().push((
                    envelope.topology,
                    mutation_id,
                    request_digest,
                    window,
                ));
            }
            true
        })
        .expect("subscribe to frozen order event");

        let request = test_window_reorder_request(
            &mux,
            window_id,
            vec![third.tab_id(), first.tab_id(), active.tab_id()],
            Some(active.tab_id()),
            1,
        );
        let request_digest = request.request_digest();
        let first_result = mux.reorder_window_tabs(request.clone());
        let applied = match first_result {
            ReorderWindowTabsResult::Decision(WindowReorderTerminalOutcome::Applied(commit)) => {
                commit
            }
            other => panic!(
                "expected first authoritative apply, got {other:?}",
                other = other
            ),
        };
        assert_eq!(
            applied.topology_revision.get(),
            topology_before.get() + 1,
            "one logical reorder reserves one global topology revision"
        );
        assert_eq!(
            applied.window.order_revision.get(),
            order_before.get() + 1,
            "one logical reorder reserves one window order revision"
        );
        assert_eq!(
            applied.window.ordered_tab_ids.as_ref(),
            [third.tab_id(), first.tab_id(), active.tab_id()]
        );
        assert_eq!(applied.window.active_tab_id, Some(active.tab_id()));
        let live_applied = mux
            .window_order_snapshot(window_id)
            .expect("valid applied order")
            .expect("applied window");
        assert_eq!(
            live_applied
                .ordered_tabs()
                .iter()
                .map(Arc::as_ptr)
                .collect::<Vec<_>>(),
            vec![
                Arc::as_ptr(&third),
                Arc::as_ptr(&first),
                Arc::as_ptr(&active),
            ]
        );
        assert!(live_applied
            .active_tab()
            .is_some_and(|tab| Arc::ptr_eq(tab, &active)));
        assert_eq!(
            mux.get_window(window_id)
                .expect("reordered window")
                .tab_stack_entries(),
            stack_before,
            "exact permutation must not reconstruct or detach tab stacks"
        );

        mux.move_tab_between_windows(third.tab_id(), window_id, Some(2))
            .expect("a later legacy mutation should not rewrite the frozen receipt");
        assert_eq!(
            mux.window_order_snapshot(window_id)
                .expect("valid later order")
                .expect("later window")
                .ordered_tab_ids()
                .collect::<Vec<_>>(),
            vec![first.tab_id(), active.tab_id(), third.tab_id()]
        );

        let replay = mux.reorder_window_tabs(request.clone());
        match replay {
            ReorderWindowTabsResult::Replay(WindowReorderTerminalOutcome::Applied(commit)) => {
                assert_eq!(commit.topology_revision, applied.topology_revision);
                assert_eq!(commit.window.order_revision, applied.window.order_revision);
            }
            other => panic!(
                "expected exact applied replay, got {other:?}",
                other = other
            ),
        }
        let equivocation = test_window_reorder_request_for(
            request.session_incarnation(),
            window_id,
            order_before,
            vec![first.tab_id(), third.tab_id(), active.tab_id()],
            Some(active.tab_id()),
            1,
        );
        let attempted_digest = equivocation.request_digest();
        assert!(matches!(
            mux.reorder_window_tabs(equivocation),
            ReorderWindowTabsResult::Equivocation {
                retained_digest,
                attempted_digest: actual_attempted_digest,
                ..
            } if retained_digest == request_digest
                && actual_attempted_digest == attempted_digest
        ));

        let observed = observed.lock();
        assert_eq!(
            observed.len(),
            1,
            "apply, exact replay, and equivocation must publish one event total"
        );
        let (stamp, mutation_id, digest, frozen) = &observed[0];
        assert_eq!(
            *stamp,
            MuxTopologyStamp::Revision(applied.topology_revision)
        );
        assert_eq!(*mutation_id, WindowOrderMutationId::new([0x73; 16], 1));
        assert_eq!(*digest, request_digest);
        assert_eq!(
            frozen.ordered_tab_ids().collect::<Vec<_>>(),
            vec![third.tab_id(), first.tab_id(), active.tab_id()]
        );
        drop(observed);
        drop(window_builder);
        Mux::shutdown();
    }

    #[test]
    fn reorder_window_tabs_conflict_and_malformed_inputs_are_zero_mutation() {
        let _guard = global_test_lock();
        Mux::shutdown();

        let mux = Arc::new(Mux::new(None));
        // Keep the exact-owner authority under test isolated from deferred
        // tasks belonging to the process-global mux used by other tests.
        let window_builder = mux.new_empty_window(Some("order-conflicts".to_string()), None);
        let other_builder = mux.new_empty_window(Some("order-conflicts".to_string()), None);
        let window_id = *window_builder;
        let other_window_id = *other_builder;
        let first = Arc::new(Tab::new(&test_size()));
        let active = Arc::new(Tab::new(&test_size()));
        let third = Arc::new(Tab::new(&test_size()));
        let foreign = Arc::new(Tab::new(&test_size()));
        for tab in [&first, &active, &third, &foreign] {
            mux.add_tab_no_panes(tab).expect("register exact tab");
        }
        for tab in [&first, &active, &third] {
            mux.add_tab_to_window(tab, window_id)
                .expect("attach exact local member");
        }
        mux.add_tab_to_window(&foreign, other_window_id)
            .expect("attach exact foreign member");
        mux.get_window_mut(window_id)
            .expect("test window")
            .set_active_without_saving(1);

        let notifications = Arc::new(AtomicUsize::new(0));
        let notifications_for_subscriber = Arc::clone(&notifications);
        mux.subscribe(move |notification| {
            if matches!(notification, MuxNotification::WindowOrderChanged { .. }) {
                notifications_for_subscriber.fetch_add(1, Ordering::SeqCst);
            }
            true
        })
        .expect("subscribe to order events");
        let before = mux
            .window_order_snapshot(window_id)
            .expect("valid initial order")
            .expect("initial window");
        let topology_before = mux
            .topology_snapshot_authority()
            .expect("valid initial topology")
            .1;

        let session_incarnation = mux
            .topology_snapshot_authority()
            .expect("current mux authority")
            .0;
        let canonical = test_window_reorder_request(
            &mux,
            window_id,
            vec![third.tab_id(), first.tab_id(), active.tab_id()],
            Some(active.tab_id()),
            9,
        );
        let mut forged = canonical;
        forged.request_digest = WindowReorderDigest::from_bytes([0xff; 32]);
        assert!(matches!(
            mux.reorder_window_tabs(forged),
            ReorderWindowTabsResult::Decision(WindowReorderTerminalOutcome::Malformed(
                WindowReorderMalformed::DigestMismatch { .. }
            ))
        ));
        assert_eq!(
            mux.window_order_receipts.lock().receipts.len(),
            0,
            "forged internal requests must fail before receipt admission"
        );

        let duplicate_request = ReorderWindowTabsRequest::try_new_v1(
            [0x62; 16],
            session_incarnation,
            window_id,
            before.order_revision(),
            vec![first.tab_id(), first.tab_id(), active.tab_id()],
            Some(active.tab_id()),
            WindowOrderMutationId::new([0x73; 16], 8),
        )
        .expect("bounded duplicate permutation reaches mux semantic authority");
        assert!(matches!(
            mux.reorder_window_tabs(duplicate_request),
            ReorderWindowTabsResult::Decision(WindowReorderTerminalOutcome::Malformed(
                WindowReorderMalformed::DuplicateTabId { tab_id }
            )) if tab_id == first.tab_id()
        ));
        assert_eq!(
            mux.window_order_receipts.lock().receipts.len(),
            1,
            "semantic malformed decisions are retained only after exact authority lookup"
        );

        let reused_semantic_identity = ReorderWindowTabsRequest::try_new_v1(
            [0x62; 16],
            session_incarnation,
            window_id,
            before.order_revision(),
            vec![first.tab_id(), active.tab_id(), third.tab_id()],
            Some(active.tab_id()),
            WindowOrderMutationId::new([0x73; 16], 8),
        )
        .expect("same mutation identity can carry an alternate bounded payload");
        assert!(matches!(
            mux.reorder_window_tabs(reused_semantic_identity),
            ReorderWindowTabsResult::Equivocation { mutation_id, .. }
                if mutation_id == WindowOrderMutationId::new([0x73; 16], 8)
        ));

        let duplicate_with_stale_revision = ReorderWindowTabsRequest::try_new_v1(
            [0x62; 16],
            session_incarnation,
            window_id,
            WindowOrderRevision::new(before.order_revision().get().saturating_sub(1)),
            vec![first.tab_id(), first.tab_id(), active.tab_id()],
            Some(active.tab_id()),
            WindowOrderMutationId::new([0x73; 16], 9),
        )
        .expect("multi-fault request remains wire-representable");
        assert!(matches!(
            mux.reorder_window_tabs(duplicate_with_stale_revision),
            ReorderWindowTabsResult::Decision(WindowReorderTerminalOutcome::Malformed(
                WindowReorderMalformed::DuplicateTabId { tab_id }
            )) if tab_id == first.tab_id()
        ));

        let conflict = test_window_reorder_request_for(
            session_incarnation,
            window_id,
            WindowOrderRevision::new(before.order_revision().get().saturating_sub(1)),
            vec![third.tab_id(), first.tab_id(), active.tab_id()],
            Some(active.tab_id()),
            10,
        );
        match mux.reorder_window_tabs(conflict) {
            ReorderWindowTabsResult::Decision(WindowReorderTerminalOutcome::Conflict(commit)) => {
                assert_eq!(commit.topology_revision, topology_before);
                assert_eq!(commit.window.order_revision, before.order_revision());
            }
            other => panic!(
                "expected stale-revision conflict, got {other:?}",
                other = other
            ),
        }

        let cases = [
            (
                vec![first.tab_id(), active.tab_id()],
                Some(active.tab_id()),
                WindowReorderMalformed::MissingTabId {
                    tab_id: third.tab_id(),
                },
            ),
            (
                vec![first.tab_id(), active.tab_id(), foreign.tab_id()],
                Some(active.tab_id()),
                WindowReorderMalformed::ForeignTabId {
                    tab_id: foreign.tab_id(),
                },
            ),
            (
                vec![first.tab_id(), active.tab_id(), third.tab_id()],
                Some(first.tab_id()),
                WindowReorderMalformed::ActiveTabChanged {
                    current_active_tab_id: Some(active.tab_id()),
                    desired_active_tab_id: Some(first.tab_id()),
                },
            ),
        ];
        for (offset, (desired, desired_active, expected)) in
            IntoIterator::into_iter(cases).enumerate()
        {
            let request = test_window_reorder_request(
                &mux,
                window_id,
                desired,
                desired_active,
                11 + offset as u64,
            );
            match mux.reorder_window_tabs(request) {
                ReorderWindowTabsResult::Decision(WindowReorderTerminalOutcome::Malformed(
                    actual,
                )) => assert_eq!(actual, expected),
                other => panic!(
                    "expected malformed zero-mutation result, got {other:?}",
                    other = other
                ),
            }
        }
        let missing_window_id = usize::MAX - 1;
        let missing_window = test_window_reorder_request_for(
            session_incarnation,
            missing_window_id,
            before.order_revision(),
            vec![first.tab_id(), first.tab_id(), active.tab_id()],
            Some(active.tab_id()),
            20,
        );
        let receipt_count_before_identity_failures =
            mux.window_order_receipts.lock().receipts.len();
        assert!(matches!(
            mux.reorder_window_tabs(missing_window),
            ReorderWindowTabsResult::Decision(
                WindowReorderTerminalOutcome::MissingWindow { window_id }
            ) if window_id == missing_window_id
        ));
        let stale_session = test_window_reorder_request_for(
            MuxSessionIncarnation::from_bytes([0x99; 16]),
            window_id,
            before.order_revision(),
            vec![first.tab_id(), first.tab_id(), active.tab_id()],
            Some(active.tab_id()),
            21,
        );
        assert!(matches!(
            mux.reorder_window_tabs(stale_session),
            ReorderWindowTabsResult::Decision(WindowReorderTerminalOutcome::StaleIncarnation)
        ));
        assert_eq!(
            mux.window_order_receipts.lock().receipts.len(),
            receipt_count_before_identity_failures,
            "stale session and missing window precede receipt retention even with malformed permutations"
        );

        let after = mux
            .window_order_snapshot(window_id)
            .expect("valid final order")
            .expect("final window");
        assert_eq!(after.order_revision(), before.order_revision());
        assert_eq!(
            after
                .ordered_tabs()
                .iter()
                .map(Arc::as_ptr)
                .collect::<Vec<_>>(),
            before
                .ordered_tabs()
                .iter()
                .map(Arc::as_ptr)
                .collect::<Vec<_>>()
        );
        assert_eq!(
            after.active_tab().map(Arc::as_ptr),
            before.active_tab().map(Arc::as_ptr)
        );
        assert_eq!(
            mux.topology_snapshot_authority()
                .expect("topology after rejected requests")
                .1,
            topology_before
        );
        assert_eq!(notifications.load(Ordering::SeqCst), 0);

        drop(other_builder);
        drop(window_builder);
        Mux::shutdown();
    }

    #[test]
    fn reorder_window_tabs_revision_exhaustion_is_zero_mutation() {
        let _guard = global_test_lock();
        Mux::shutdown();

        let mux = Arc::new(Mux::new(None));
        Mux::set_mux(&mux);
        let window_builder = mux.new_empty_window(Some("order-exhaustion".to_string()), None);
        let window_id = *window_builder;
        let first = Arc::new(Tab::new(&test_size()));
        let active = Arc::new(Tab::new(&test_size()));
        for tab in [&first, &active] {
            mux.add_tab_no_panes(tab).expect("register exact tab");
            mux.add_tab_to_window(tab, window_id)
                .expect("attach exact tab");
        }
        mux.get_window_mut(window_id)
            .expect("test window")
            .set_active_without_saving(1);
        mux.get_window_mut(window_id)
            .expect("test window")
            .set_order_revision_for_test(WindowOrderRevision::new(u64::MAX - 1));
        let before = mux
            .window_order_snapshot(window_id)
            .expect("valid terminal-revision snapshot")
            .expect("test window snapshot");
        let notifications = Arc::new(AtomicUsize::new(0));
        let notifications_for_subscriber = Arc::clone(&notifications);
        mux.subscribe(move |notification| {
            if matches!(notification, MuxNotification::WindowOrderChanged { .. }) {
                notifications_for_subscriber.fetch_add(1, Ordering::SeqCst);
            }
            true
        })
        .expect("subscribe to order events");

        let stale_revision = test_window_reorder_request_for(
            mux.topology_snapshot_authority()
                .expect("current session remains available")
                .0,
            window_id,
            WindowOrderRevision::new(before.order_revision().get() - 1),
            vec![active.tab_id(), first.tab_id()],
            Some(active.tab_id()),
            29,
        );
        assert!(matches!(
            mux.reorder_window_tabs(stale_revision),
            ReorderWindowTabsResult::Decision(WindowReorderTerminalOutcome::Conflict(commit))
                if commit.window.order_revision == before.order_revision()
        ));

        let request = test_window_reorder_request(
            &mux,
            window_id,
            vec![active.tab_id(), first.tab_id()],
            Some(active.tab_id()),
            30,
        );
        assert!(matches!(
            mux.reorder_window_tabs(request),
            ReorderWindowTabsResult::Decision(WindowReorderTerminalOutcome::Exhausted)
        ));
        let after = mux
            .window_order_snapshot(window_id)
            .expect("valid order after exhaustion")
            .expect("test window after exhaustion");
        assert_eq!(after.order_revision(), before.order_revision());
        assert_eq!(
            after
                .ordered_tabs()
                .iter()
                .map(Arc::as_ptr)
                .collect::<Vec<_>>(),
            before
                .ordered_tabs()
                .iter()
                .map(Arc::as_ptr)
                .collect::<Vec<_>>()
        );
        assert_eq!(
            after.active_tab().map(Arc::as_ptr),
            before.active_tab().map(Arc::as_ptr)
        );
        assert_eq!(notifications.load(Ordering::SeqCst), 0);

        drop(window_builder);
        Mux::shutdown();
    }

    #[test]
    fn window_order_receipt_ledger_is_bounded_and_insertion_ordered() {
        let mut ledger = WindowOrderReceiptLedger::new();
        for sequence in 1..=MAX_WINDOW_ORDER_RECEIPTS as u64 + 1 {
            let mutation_id = WindowOrderMutationId::new([0x81; 16], sequence);
            ledger.retain(
                mutation_id,
                WindowReorderDigest::from_bytes([sequence as u8; 32]),
                WindowReorderTerminalOutcome::Exhausted,
            );
        }
        assert_eq!(ledger.receipts.len(), MAX_WINDOW_ORDER_RECEIPTS);
        assert_eq!(ledger.insertion_order.len(), MAX_WINDOW_ORDER_RECEIPTS);
        assert!(!ledger
            .receipts
            .contains_key(&WindowOrderMutationId::new([0x81; 16], 1)));
        assert!(ledger.receipts.contains_key(&WindowOrderMutationId::new(
            [0x81; 16],
            MAX_WINDOW_ORDER_RECEIPTS as u64 + 1,
        )));
    }

    #[test]
    fn cross_window_tab_move_preserves_both_active_identities() {
        let _guard = global_test_lock();
        Mux::shutdown();

        let mux = Arc::new(Mux::new(None));
        Mux::set_mux(&mux);
        let source_builder = mux.new_empty_window(Some("cross-window-order".to_string()), None);
        let source_id = *source_builder;
        let destination_builder =
            mux.new_empty_window(Some("cross-window-order".to_string()), None);
        let destination_id = *destination_builder;
        let empty_builder = mux.new_empty_window(Some("cross-window-order".to_string()), None);
        let empty_id = *empty_builder;
        let left_fallback_builder =
            mux.new_empty_window(Some("cross-window-order".to_string()), None);
        let left_fallback_id = *left_fallback_builder;

        let first = Arc::new(Tab::new(&test_size()));
        let source_active = Arc::new(Tab::new(&test_size()));
        let source_right = Arc::new(Tab::new(&test_size()));
        let destination_first = Arc::new(Tab::new(&test_size()));
        let destination_active = Arc::new(Tab::new(&test_size()));
        let source_left = Arc::new(Tab::new(&test_size()));
        let source_last_active = Arc::new(Tab::new(&test_size()));
        for tab in [
            &first,
            &source_active,
            &source_right,
            &destination_first,
            &destination_active,
            &source_left,
            &source_last_active,
        ] {
            mux.add_tab_no_panes(tab).expect("register exact tab");
        }
        for tab in [&first, &source_active, &source_right] {
            mux.add_tab_to_window(tab, source_id)
                .expect("attach source tab");
        }
        for tab in [&destination_first, &destination_active] {
            mux.add_tab_to_window(tab, destination_id)
                .expect("attach destination tab");
        }
        for tab in [&source_left, &source_last_active] {
            mux.add_tab_to_window(tab, left_fallback_id)
                .expect("attach left-fallback source tab");
        }
        mux.get_window_mut(source_id)
            .expect("source window")
            .set_active_without_saving(1);
        mux.get_window_mut(destination_id)
            .expect("destination window")
            .set_active_without_saving(1);
        mux.get_window_mut(left_fallback_id)
            .expect("left-fallback source window")
            .set_active_without_saving(1);

        mux.move_tab_between_windows(first.tab_id(), destination_id, Some(0))
            .expect("insert before destination active");
        {
            let source = mux.get_window(source_id).expect("source window");
            let destination = mux.get_window(destination_id).expect("destination window");
            assert!(source
                .get_active()
                .is_some_and(|tab| Arc::ptr_eq(tab, &source_active)));
            assert!(destination
                .get_active()
                .is_some_and(|tab| Arc::ptr_eq(tab, &destination_active)));
            assert_eq!(
                destination.iter().map(Arc::as_ptr).collect::<Vec<_>>(),
                vec![
                    Arc::as_ptr(&first),
                    Arc::as_ptr(&destination_first),
                    Arc::as_ptr(&destination_active),
                ],
            );
        }

        mux.move_tab_between_windows(source_active.tab_id(), empty_id, None)
            .expect("move active source tab into empty destination");
        {
            let source = mux.get_window(source_id).expect("source window");
            let empty = mux.get_window(empty_id).expect("empty destination");
            assert!(source
                .get_active()
                .is_some_and(|tab| Arc::ptr_eq(tab, &source_right)));
            assert!(empty
                .get_active()
                .is_some_and(|tab| Arc::ptr_eq(tab, &source_active)));
        }

        mux.move_tab_between_windows(source_last_active.tab_id(), destination_id, None)
            .expect("move last active source tab into nonempty destination");
        {
            let source = mux
                .get_window(left_fallback_id)
                .expect("left-fallback source window");
            let destination = mux.get_window(destination_id).expect("destination window");
            assert!(source
                .get_active()
                .is_some_and(|tab| Arc::ptr_eq(tab, &source_left)));
            assert!(destination
                .get_active()
                .is_some_and(|tab| Arc::ptr_eq(tab, &destination_active)));
        }

        drop(left_fallback_builder);
        drop(empty_builder);
        drop(destination_builder);
        drop(source_builder);
        Mux::shutdown();
    }

    #[test]
    fn invalid_tab_move_indices_fail_before_either_window_mutates() {
        let _guard = global_test_lock();
        Mux::shutdown();

        let mux = Arc::new(Mux::new(None));
        Mux::set_mux(&mux);
        let source_builder = mux.new_empty_window(Some("move-atomicity".to_string()), None);
        let source_id = *source_builder;
        let destination_builder = mux.new_empty_window(Some("move-atomicity".to_string()), None);
        let destination_id = *destination_builder;
        let source_tab = Arc::new(Tab::new(&test_size()));
        let destination_tab = Arc::new(Tab::new(&test_size()));
        for tab in [&source_tab, &destination_tab] {
            mux.add_tab_no_panes(tab).expect("register exact tab");
        }
        mux.add_tab_to_window(&source_tab, source_id)
            .expect("attach source tab");
        mux.add_tab_to_window(&destination_tab, destination_id)
            .expect("attach destination tab");

        let source_before = mux
            .get_window(source_id)
            .expect("source window")
            .iter()
            .map(Arc::as_ptr)
            .collect::<Vec<_>>();
        let destination_before = mux
            .get_window(destination_id)
            .expect("destination window")
            .iter()
            .map(Arc::as_ptr)
            .collect::<Vec<_>>();

        let cross_error = mux
            .move_tab_between_windows(source_tab.tab_id(), destination_id, Some(2))
            .expect_err("cross-window index beyond append must fail");
        assert!(cross_error.to_string().contains("out of range"));
        let same_error = mux
            .move_tab_between_windows(source_tab.tab_id(), source_id, Some(1))
            .expect_err("same-window index equal to length must fail");
        assert!(same_error.to_string().contains("out of range"));

        let source = mux.get_window(source_id).expect("source window");
        let destination = mux.get_window(destination_id).expect("destination window");
        assert_eq!(
            source.iter().map(Arc::as_ptr).collect::<Vec<_>>(),
            source_before,
        );
        assert_eq!(
            destination.iter().map(Arc::as_ptr).collect::<Vec<_>>(),
            destination_before,
        );
        assert!(source
            .get_active()
            .is_some_and(|tab| Arc::ptr_eq(tab, &source_tab)));
        assert!(destination
            .get_active()
            .is_some_and(|tab| Arc::ptr_eq(tab, &destination_tab)));

        drop(destination);
        drop(source);
        drop(destination_builder);
        drop(source_builder);
        Mux::shutdown();
    }

    #[test]
    fn exhausted_destination_rejects_attach_and_cross_window_move_before_detach() {
        let _guard = global_test_lock();
        Mux::shutdown();

        let mux = Arc::new(Mux::new(None));
        Mux::set_mux(&mux);
        let source_builder = mux.new_empty_window(Some("move-exhaustion".to_string()), None);
        let source_id = *source_builder;
        let destination_builder = mux.new_empty_window(Some("move-exhaustion".to_string()), None);
        let destination_id = *destination_builder;
        let source_tab = Arc::new(Tab::new(&test_size()));
        let destination_tab = Arc::new(Tab::new(&test_size()));
        let unattached_tab = Arc::new(Tab::new(&test_size()));
        for tab in [&source_tab, &destination_tab, &unattached_tab] {
            mux.add_tab_no_panes(tab).expect("register exact tab");
        }
        mux.add_tab_to_window(&source_tab, source_id)
            .expect("attach source tab");
        mux.add_tab_to_window(&destination_tab, destination_id)
            .expect("attach destination tab");
        mux.get_window_mut(destination_id)
            .expect("destination window")
            .set_order_revision_for_test(WindowOrderRevision::new(u64::MAX - 1));
        let source_revision_before = mux
            .get_window(source_id)
            .expect("source window")
            .order_revision();
        let events = Arc::new(AtomicUsize::new(0));
        let events_for_subscriber = Arc::clone(&events);
        mux.subscribe(move |notification| {
            if matches!(notification, MuxNotification::WindowTopologyChanged(_)) {
                events_for_subscriber.fetch_add(1, Ordering::SeqCst);
            }
            true
        })
        .expect("subscribe after setup");

        let attach_error = mux
            .add_tab_to_window(&unattached_tab, destination_id)
            .expect_err("an exhausted destination must reject a new attachment");
        assert!(
            format!("{attach_error:#}").contains("revision space is exhausted"),
            "unexpected exhausted-attachment error: {attach_error:#}",
            attach_error = attach_error,
        );
        let move_error = mux
            .move_tab_between_windows(source_tab.tab_id(), destination_id, None)
            .expect_err("an exhausted destination must reject before source detach");
        assert!(
            format!("{move_error:#}").contains("destination window"),
            "unexpected exhausted-move error: {move_error:#}",
            move_error = move_error,
        );

        let source = mux.get_window(source_id).expect("source window survives");
        let destination = mux
            .get_window(destination_id)
            .expect("destination window survives");
        assert_eq!(source.len(), 1);
        assert!(source
            .get_active()
            .is_some_and(|tab| Arc::ptr_eq(tab, &source_tab)));
        assert_eq!(destination.len(), 1);
        assert!(destination
            .get_active()
            .is_some_and(|tab| Arc::ptr_eq(tab, &destination_tab)));
        assert_eq!(destination.order_revision().get(), u64::MAX - 1);
        assert_eq!(source.order_revision(), source_revision_before);
        assert_eq!(
            events.load(Ordering::SeqCst),
            0,
            "destination exhaustion must publish no partial transaction",
        );
        drop(destination);
        drop(source);
        assert!(mux.window_containing_tab(unattached_tab.tab_id()).is_none());
        drop(destination_builder);
        drop(source_builder);
        Mux::shutdown();
    }

    #[test]
    fn exhausted_source_rejects_cross_window_move_before_destination_attach() {
        let _guard = global_test_lock();
        Mux::shutdown();

        let mux = Arc::new(Mux::new(None));
        Mux::set_mux(&mux);
        let source_builder = mux.new_empty_window(Some("source-exhaustion".to_string()), None);
        let source_id = *source_builder;
        let destination_builder = mux.new_empty_window(Some("source-exhaustion".to_string()), None);
        let destination_id = *destination_builder;
        let source_tab = Arc::new(Tab::new(&test_size()));
        let destination_tab = Arc::new(Tab::new(&test_size()));
        for tab in [&source_tab, &destination_tab] {
            mux.add_tab_no_panes(tab).expect("register exact tab");
        }
        mux.add_tab_to_window(&source_tab, source_id)
            .expect("attach source tab");
        mux.add_tab_to_window(&destination_tab, destination_id)
            .expect("attach destination tab");
        mux.get_window_mut(source_id)
            .expect("source window")
            .set_order_revision_for_test(WindowOrderRevision::new(u64::MAX - 1));
        let destination_revision_before = mux
            .get_window(destination_id)
            .expect("destination window")
            .order_revision();
        let events = Arc::new(AtomicUsize::new(0));
        let events_for_subscriber = Arc::clone(&events);
        mux.subscribe(move |notification| {
            if matches!(notification, MuxNotification::WindowTopologyChanged(_)) {
                events_for_subscriber.fetch_add(1, Ordering::SeqCst);
            }
            true
        })
        .expect("subscribe after setup");

        let error = mux
            .move_tab_between_windows(source_tab.tab_id(), destination_id, None)
            .expect_err("an exhausted source must reject before destination attachment");
        assert!(
            format!("{error:#}").contains("source window"),
            "unexpected exhausted-source error: {error:#}",
            error = error,
        );

        let source = mux.get_window(source_id).expect("source window survives");
        let destination = mux
            .get_window(destination_id)
            .expect("destination window survives");
        assert_eq!(source.order_revision().get(), u64::MAX - 1);
        assert_eq!(destination.order_revision(), destination_revision_before);
        assert_eq!(source.len(), 1);
        assert_eq!(destination.len(), 1);
        assert!(source
            .get_active()
            .is_some_and(|tab| Arc::ptr_eq(tab, &source_tab)));
        assert!(destination
            .get_active()
            .is_some_and(|tab| Arc::ptr_eq(tab, &destination_tab)));
        assert_eq!(
            events.load(Ordering::SeqCst),
            0,
            "source exhaustion must publish no partial transaction",
        );

        drop(destination);
        drop(source);
        drop(destination_builder);
        drop(source_builder);
        Mux::shutdown();
    }

    #[test]
    fn full_destination_rejects_cross_window_move_before_source_detach() {
        let _guard = global_test_lock();
        Mux::shutdown();

        let mux = Arc::new(Mux::new(None));
        Mux::set_mux(&mux);
        let source_builder = mux.new_empty_window(Some("full-destination".to_string()), None);
        let source_id = *source_builder;
        let source_tab = Arc::new(Tab::new(&test_size()));
        mux.add_tab_no_panes(&source_tab)
            .expect("register exact source tab");
        mux.add_tab_to_window(&source_tab, source_id)
            .expect("attach source tab");

        // Build the capacity boundary ownerlessly so fixture construction does
        // not publish 4,096 irrelevant topology events. The public mux move
        // still sees the exact same bounded Window representation.
        let mut destination = Window::new(Some("full-destination".to_string()), None);
        for _ in 0..MAX_TABS_PER_ORDERED_WINDOW {
            destination
                .push(&Arc::new(Tab::new(&test_size())))
                .expect("fill destination exactly to its bounded capacity");
        }
        let destination_id = destination.window_id();
        mux.windows.write().insert(destination_id, destination);
        let source_before = mux
            .window_order_snapshot(source_id)
            .expect("valid source")
            .expect("registered source");
        let destination_before = mux
            .window_order_snapshot(destination_id)
            .expect("valid destination")
            .expect("registered destination");
        let events = Arc::new(AtomicUsize::new(0));
        let events_for_subscriber = Arc::clone(&events);
        mux.subscribe(move |notification| {
            if matches!(notification, MuxNotification::WindowTopologyChanged(_)) {
                events_for_subscriber.fetch_add(1, Ordering::SeqCst);
            }
            true
        })
        .expect("subscribe after setup");

        let error = mux
            .move_tab_between_windows(source_tab.tab_id(), destination_id, None)
            .expect_err("full destination must reject before source detach");
        assert!(
            format!("{error:#}").contains("ordered-window limit"),
            "unexpected full-destination error: {error:#}",
            error = error,
        );
        let source_after = mux
            .window_order_snapshot(source_id)
            .expect("valid source")
            .expect("registered source");
        let destination_after = mux
            .window_order_snapshot(destination_id)
            .expect("valid destination")
            .expect("registered destination");
        assert_eq!(
            source_after.order_revision(),
            source_before.order_revision()
        );
        assert_eq!(source_after.active_tab_id(), source_before.active_tab_id());
        assert_eq!(
            source_after.ordered_tab_ids().collect::<Vec<_>>(),
            source_before.ordered_tab_ids().collect::<Vec<_>>(),
        );
        assert_eq!(
            destination_after.order_revision(),
            destination_before.order_revision()
        );
        assert_eq!(
            destination_after.ordered_tabs().len(),
            MAX_TABS_PER_ORDERED_WINDOW
        );
        assert_eq!(events.load(Ordering::SeqCst), 0);

        mux.windows.write().remove(&destination_id);
        drop(source_builder);
        Mux::shutdown();
    }

    #[test]
    fn global_topology_exhaustion_rejects_active_selection_without_window_mutation() {
        let _guard = global_test_lock();
        Mux::shutdown();

        let mux = Arc::new(Mux::new(None));
        Mux::set_mux(&mux);
        let window_builder = mux.new_empty_window(Some("topology-exhaustion".to_string()), None);
        let window_id = *window_builder;
        let first = Arc::new(Tab::new(&test_size()));
        let second = Arc::new(Tab::new(&test_size()));
        for tab in [&first, &second] {
            mux.add_tab_no_panes(tab).expect("register exact tab");
            mux.add_tab_to_window(tab, window_id)
                .expect("attach exact tab");
        }
        let before = mux
            .window_order_snapshot(window_id)
            .expect("valid window")
            .expect("registered window");
        let events = Arc::new(AtomicUsize::new(0));
        let events_for_subscriber = Arc::clone(&events);
        mux.subscribe(move |notification| {
            if matches!(notification, MuxNotification::WindowTopologyChanged(_)) {
                events_for_subscriber.fetch_add(1, Ordering::SeqCst);
            }
            true
        })
        .expect("subscribe after setup");
        mux.topology.lock().revision = TopologyRevision(u64::MAX - 1);

        let error = mux
            .activate_tab_at_index(window_id, 1, true)
            .expect_err("terminal topology revision must reject active selection");
        assert!(format!("{error:#}").contains("topology revision space is exhausted"));
        let after = mux
            .window_order_snapshot(window_id)
            .expect("valid window")
            .expect("registered window");
        assert_eq!(after.order_revision(), before.order_revision());
        assert_eq!(after.active_tab_id(), before.active_tab_id());
        assert_eq!(
            after.ordered_tab_ids().collect::<Vec<_>>(),
            before.ordered_tab_ids().collect::<Vec<_>>(),
        );
        assert_eq!(mux.topology.lock().revision, TopologyRevision(u64::MAX - 1));
        assert_eq!(events.load(Ordering::SeqCst), 0);

        drop(window_builder);
        Mux::shutdown();
    }

    #[test]
    fn window_transaction_rejects_nonempty_retirement_before_any_mutation() {
        let _guard = global_test_lock();
        Mux::shutdown();

        let mux = Arc::new(Mux::new(None));
        Mux::set_mux(&mux);
        let builder = mux.new_empty_window(Some("invalid-retirement".to_string()), None);
        let window_id = *builder;
        let tab = Arc::new(Tab::new(&test_size()));
        mux.add_tab_no_panes(&tab).expect("register exact tab");
        mux.add_tab_to_window(&tab, window_id)
            .expect("attach exact tab");
        drop(builder);

        let before = mux
            .window_order_snapshot(window_id)
            .expect("valid window")
            .expect("registered window");
        let events = Arc::new(AtomicUsize::new(0));
        let events_for_subscriber = Arc::clone(&events);
        mux.subscribe(move |notification| {
            if matches!(notification, MuxNotification::WindowTopologyChanged(_)) {
                events_for_subscriber.fetch_add(1, Ordering::SeqCst);
            }
            true
        })
        .expect("subscribe after setup");

        let error = {
            let mut windows = mux.windows.write();
            let state = windows
                .get(&window_id)
                .expect("window remains registered")
                .prepare_retirement_marker()
                .expect("prepare nonempty retirement probe");
            mux.commit_prepared_window_states_locked(
                &mut windows,
                vec![(window_id, state)],
                Vec::new(),
                Vec::new(),
                vec![window_id],
            )
            .expect_err("nonempty window retirement must fail closed")
        };
        assert!(
            format!("{error:#}").contains("empty prepared final state"),
            "unexpected nonempty-retirement error: {error:#}",
            error = error,
        );
        let after = mux
            .window_order_snapshot(window_id)
            .expect("valid window")
            .expect("registered window");
        assert_eq!(after.order_revision(), before.order_revision());
        assert_eq!(after.active_tab_id(), before.active_tab_id());
        assert_eq!(
            after.ordered_tab_ids().collect::<Vec<_>>(),
            before.ordered_tab_ids().collect::<Vec<_>>()
        );
        assert_eq!(events.load(Ordering::SeqCst), 0);

        Mux::shutdown();
    }

    #[test]
    fn exact_tab_activation_cannot_be_redirected_by_a_reorder_or_stale_registration() {
        let _guard = global_test_lock();
        Mux::shutdown();

        let mux = Arc::new(Mux::new(None));
        Mux::set_mux(&mux);
        let window_builder = mux.new_empty_window(Some("exact-activation".to_string()), None);
        let window_id = *window_builder;
        let first = Arc::new(Tab::new(&test_size()));
        let intended = Arc::new(Tab::new(&test_size()));
        let third = Arc::new(Tab::new(&test_size()));
        for tab in [&first, &intended, &third] {
            mux.add_tab_no_panes(tab).expect("register exact tab");
            mux.add_tab_to_window(tab, window_id)
                .expect("attach exact tab");
        }

        mux.move_tab_between_windows(intended.tab_id(), window_id, Some(0))
            .expect("reorder the intended tab away from its observed index");
        assert!(
            mux.activate_tab_exact_in_window(window_id, &intended, true)
                .expect("activate the exact tab after reorder"),
            "the reordered exact tab should become active",
        );
        assert!(
            mux.get_active_tab_for_window(window_id)
                .is_some_and(|active| Arc::ptr_eq(&active, &intended)),
            "exact activation must follow the tab identity rather than its prior index",
        );

        let topology_events = Arc::new(AtomicUsize::new(0));
        let topology_events_for_subscriber = Arc::clone(&topology_events);
        mux.subscribe(move |notification| {
            if matches!(notification, MuxNotification::WindowTopologyChanged(_)) {
                topology_events_for_subscriber.fetch_add(1, Ordering::SeqCst);
            }
            true
        })
        .expect("subscribe after successful activation");
        let removed = mux
            .tabs
            .write()
            .remove(&intended.tab_id())
            .expect("remove exact registration for stale-identity probe");
        let before = mux
            .window_order_snapshot(window_id)
            .expect("valid window")
            .expect("registered window");
        let error = mux
            .activate_tab_exact_in_window(window_id, &intended, true)
            .expect_err("an unregistered exact tab must be rejected");
        assert!(format!("{error:#}").contains("not the current registered instance"));
        let after = mux
            .window_order_snapshot(window_id)
            .expect("valid window")
            .expect("registered window");
        assert_eq!(after.order_revision(), before.order_revision());
        assert_eq!(after.active_tab_id(), before.active_tab_id());
        assert_eq!(topology_events.load(Ordering::SeqCst), 0);
        mux.tabs.write().insert(intended.tab_id(), removed);

        drop(window_builder);
        Mux::shutdown();
    }

    #[test]
    fn global_topology_exhaustion_rejects_add_move_and_remove_without_partial_state() {
        let _guard = global_test_lock();
        Mux::shutdown();

        let mux = Arc::new(Mux::new(None));
        Mux::set_mux(&mux);
        let source_builder = mux.new_empty_window(Some("global-exhaustion".to_string()), None);
        let source_id = *source_builder;
        let destination_builder = mux.new_empty_window(Some("global-exhaustion".to_string()), None);
        let destination_id = *destination_builder;
        let source_tab = Arc::new(Tab::new(&test_size()));
        let destination_tab = Arc::new(Tab::new(&test_size()));
        let unattached_tab = Arc::new(Tab::new(&test_size()));
        for tab in [&source_tab, &destination_tab, &unattached_tab] {
            mux.add_tab_no_panes(tab).expect("register exact tab");
        }
        mux.add_tab_to_window(&source_tab, source_id)
            .expect("attach source tab");
        mux.add_tab_to_window(&destination_tab, destination_id)
            .expect("attach destination tab");
        let source_before = mux
            .window_order_snapshot(source_id)
            .expect("valid source")
            .expect("registered source");
        let destination_before = mux
            .window_order_snapshot(destination_id)
            .expect("valid destination")
            .expect("registered destination");
        let events = Arc::new(AtomicUsize::new(0));
        let events_for_subscriber = Arc::clone(&events);
        mux.subscribe(move |notification| {
            if matches!(notification, MuxNotification::WindowTopologyChanged(_)) {
                events_for_subscriber.fetch_add(1, Ordering::SeqCst);
            }
            true
        })
        .expect("subscribe after setup");
        mux.topology.lock().revision = TopologyRevision(u64::MAX - 1);

        assert!(
            mux.move_tab_between_windows(source_tab.tab_id(), destination_id, None)
                .is_err(),
            "global exhaustion must reject a cross-window move",
        );
        assert!(
            mux.add_tab_to_window(&unattached_tab, destination_id)
                .is_err(),
            "global exhaustion must reject a new attachment",
        );
        assert!(
            !mux.remove_tab_local_only_if_same(&source_tab),
            "global exhaustion must reject exact tab retirement",
        );

        let source_after = mux
            .window_order_snapshot(source_id)
            .expect("valid source")
            .expect("registered source");
        let destination_after = mux
            .window_order_snapshot(destination_id)
            .expect("valid destination")
            .expect("registered destination");
        for (before, after) in [
            (&source_before, &source_after),
            (&destination_before, &destination_after),
        ] {
            assert_eq!(after.order_revision(), before.order_revision());
            assert_eq!(after.active_tab_id(), before.active_tab_id());
            assert_eq!(
                after.ordered_tab_ids().collect::<Vec<_>>(),
                before.ordered_tab_ids().collect::<Vec<_>>(),
            );
        }
        assert!(mux
            .get_tab(source_tab.tab_id())
            .is_some_and(|tab| Arc::ptr_eq(&tab, &source_tab)));
        assert!(mux.window_containing_tab(unattached_tab.tab_id()).is_none());
        mux.assert_tab_parent_index_matches_windows();
        assert_eq!(mux.topology.lock().revision, TopologyRevision(u64::MAX - 1));
        assert_eq!(events.load(Ordering::SeqCst), 0);

        drop(destination_builder);
        drop(source_builder);
        Mux::shutdown();
    }

    #[test]
    fn tab_parent_conflicts_fail_without_partial_window_mutation() {
        let _guard = global_test_lock();
        Mux::shutdown();

        let mux = Arc::new(Mux::new(None));
        Mux::set_mux(&mux);
        let src_window = mux.new_empty_window(Some("parent-check".to_string()), None);
        let src_window_id = *src_window;
        let dst_window = mux.new_empty_window(Some("parent-check".to_string()), None);
        let dst_window_id = *dst_window;
        let (tab, _kills) = tab_with_kill_counter(&mux, 454);

        mux.add_tab_to_window(&tab, src_window_id)
            .expect("tab should acquire its one parent");
        let duplicate_error = mux
            .add_tab_to_window(&tab, dst_window_id)
            .expect_err("one exact tab cannot acquire a second parent");
        assert!(
            duplicate_error.to_string().contains("already attached"),
            "unexpected parent-conflict error: {:#}",
            duplicate_error
        );
        assert!(
            mux.get_window(src_window_id)
                .is_some_and(|window| window.iter().any(|item| Arc::ptr_eq(item, &tab))),
            "failed attachment must preserve the source parent",
        );
        assert!(
            mux.get_window(dst_window_id)
                .is_some_and(|window| window.iter().all(|item| !Arc::ptr_eq(item, &tab))),
            "failed attachment must leave the destination unchanged",
        );

        // Construct legacy-corrupt multi-parent topology through the
        // test-only low-level Window surface. The indexed source remains the
        // original exact parent, and destination validation must refuse the
        // duplicate without scanning for a HashMap-order-dependent source.
        mux.get_window_mut(dst_window_id)
            .expect("destination should remain registered")
            .push(&tab)
            .expect("seed destination before same-window rejection");
        let move_error = mux
            .move_tab_between_windows(tab.tab_id(), dst_window_id, None)
            .expect_err("move must reject ambiguous multi-parent topology");
        assert!(
            move_error
                .to_string()
                .contains("already contains exact tab"),
            "unexpected ambiguous-parent error: {:#}",
            move_error
        );
        assert_eq!(
            mux.window_containing_tab(tab.tab_id()),
            Some(src_window_id),
            "corrupt unindexed membership must not replace exact parent authority",
        );
        for window_id in [src_window_id, dst_window_id] {
            assert!(
                mux.get_window(window_id)
                    .is_some_and(|window| window.iter().any(|item| Arc::ptr_eq(item, &tab))),
                "ambiguous move must preserve window {}",
                window_id
            );
        }

        drop(dst_window);
        drop(src_window);
        Mux::shutdown();
    }

    #[test]
    fn missing_tab_parent_index_rejects_move_without_window_mutation() {
        let _guard = global_test_lock();
        Mux::shutdown();

        let mux = Arc::new(Mux::new(None));
        Mux::set_mux(&mux);
        let source = mux.new_empty_window(Some("missing-parent-source".to_string()), None);
        let source_id = *source;
        let destination =
            mux.new_empty_window(Some("missing-parent-destination".to_string()), None);
        let destination_id = *destination;
        let (tab, _kills) = tab_with_kill_counter(&mux, 457);
        mux.add_tab_to_window(&tab, source_id)
            .expect("tab should acquire its source parent");
        let source_before = mux
            .window_order_snapshot(source_id)
            .expect("valid source")
            .expect("registered source");
        let destination_before = mux
            .window_order_snapshot(destination_id)
            .expect("valid destination")
            .expect("registered destination");

        let removed = mux.tab_parents.write().remove(&tab.tab_id());
        assert!(removed.is_some_and(|parent| parent.matches(&tab, source_id)));
        let error = mux
            .move_tab_between_windows(tab.tab_id(), destination_id, None)
            .expect_err("missing exact parent authority must reject a move");
        assert!(
            error.to_string().contains("no exact indexed window parent"),
            "unexpected missing-parent error: {error:#}",
            error = error,
        );
        for (before, window_id) in [
            (&source_before, source_id),
            (&destination_before, destination_id),
        ] {
            let after = mux
                .window_order_snapshot(window_id)
                .expect("valid window")
                .expect("registered window");
            assert_eq!(after.order_revision(), before.order_revision());
            assert_eq!(after.active_tab_id(), before.active_tab_id());
            assert_eq!(
                after.ordered_tab_ids().collect::<Vec<_>>(),
                before.ordered_tab_ids().collect::<Vec<_>>(),
            );
        }
        assert!(mux
            .tab_parents
            .write()
            .insert(tab.tab_id(), TabParentRegistration::new(&tab, source_id),)
            .is_none());
        mux.assert_tab_parent_index_matches_windows();

        drop(destination);
        drop(source);
        Mux::shutdown();
    }

    #[test]
    fn indexed_tab_parent_resolution_has_constant_work_at_large_session_counts() {
        for count in [1_024usize, 4_096, 16_384] {
            let mux = Mux::new(None);
            let mut expected = Vec::new();
            expected
                .try_reserve_exact(count)
                .expect("reserve indexed parent scale fixture");
            {
                let mut tabs = mux.tabs.write();
                let mut windows = mux.windows.write();
                let mut parents = mux.tab_parents.write();
                tabs.try_reserve(count).expect("reserve scale tab registry");
                windows
                    .try_reserve(count)
                    .expect("reserve scale window registry");
                parents
                    .try_reserve(count)
                    .expect("reserve scale parent index");
                for _ in 0..count {
                    let tab = Arc::new(Tab::new(&test_size()));
                    let tab_id = tab.tab_id();
                    let mut window = Window::new(Some("parent-scale".to_string()), None);
                    let window_id = window.window_id();
                    window.push(&tab).expect("seed exact window membership");
                    assert!(tabs.insert(tab_id, Arc::clone(&tab)).is_none());
                    assert!(windows.insert(window_id, window).is_none());
                    assert!(parents
                        .insert(tab_id, TabParentRegistration::new(&tab, window_id))
                        .is_none());
                    expected.push((tab_id, window_id));
                }
            }
            mux.assert_tab_parent_index_matches_windows();

            let probes_before = mux.tab_parent_lookup_probes.load(Ordering::Relaxed);
            for &(tab_id, window_id) in &expected {
                assert_eq!(mux.window_containing_tab(tab_id), Some(window_id));
            }
            let probes = mux
                .tab_parent_lookup_probes
                .load(Ordering::Relaxed)
                .saturating_sub(probes_before);
            assert_eq!(
                probes, count,
                "each successful parent resolution must use exactly one indexed probe",
            );
            eprintln!(
                "mux_tab_parent_lookup_work tab_count={count} lookups={count} hash_probes={probes}"
            );
        }
    }

    #[test]
    fn pane_location_cache_makes_stable_key_path_constant_after_one_large_session_scan() {
        let _guard = global_test_lock();
        Mux::shutdown();

        const UNRELATED_TABS: usize = 16_384;
        const STABLE_LOOKUPS: usize = 4_096;
        let mux = Arc::new(Mux::new(None));
        Mux::set_mux(&mux);
        let (pane, _kills) = KillCountingPane::new(912_001, test_size());
        let (target_tab, original_window_id) = register_attached_test_pane(&_guard, &mux, &pane);

        // Empty tabs are sufficient to reproduce the old session-cardinality
        // cost without starting PTYs or touching a product process.
        {
            let mut tabs = mux.tabs.write();
            let mut windows = mux.windows.write();
            let mut parents = mux.tab_parents.write();
            tabs.try_reserve(UNRELATED_TABS)
                .expect("reserve large-session tab registry");
            windows
                .try_reserve(UNRELATED_TABS)
                .expect("reserve large-session window registry");
            parents
                .try_reserve(UNRELATED_TABS)
                .expect("reserve large-session parent index");
            for _ in 0..UNRELATED_TABS {
                let tab = Arc::new(Tab::new(&test_size()));
                let tab_id = tab.tab_id();
                let mut window = Window::new(Some("pane-location-scale".to_string()), None);
                let window_id = window.window_id();
                window.push(&tab).expect("seed unrelated exact membership");
                assert!(tabs.insert(tab_id, Arc::clone(&tab)).is_none());
                assert!(windows.insert(window_id, window).is_none());
                assert!(parents
                    .insert(tab_id, TabParentRegistration::new(&tab, window_id))
                    .is_none());
            }
        }
        mux.assert_tab_parent_index_matches_windows();

        let scans_before = mux.pane_location_full_scans.load(Ordering::Relaxed);
        let probes_before = mux.pane_location_scan_tab_probes.load(Ordering::Relaxed);
        let hits_before = mux.pane_location_cache_hits.load(Ordering::Relaxed);
        assert_eq!(
            mux.resolve_pane_id(912_001),
            Some((pane.domain_id(), original_window_id, target_tab.tab_id()))
        );
        let scans_after_cold = mux.pane_location_full_scans.load(Ordering::Relaxed);
        let probes_after_cold = mux.pane_location_scan_tab_probes.load(Ordering::Relaxed);
        assert_eq!(scans_after_cold.saturating_sub(scans_before), 1);
        assert_eq!(
            probes_after_cold.saturating_sub(probes_before),
            UNRELATED_TABS + 1,
            "the planted cold miss must expose the former all-tab work"
        );

        assert!(mux.set_tab_title(target_tab.tab_id(), "cache-refreshes-after-title-revision"));

        for _ in 0..STABLE_LOOKUPS {
            assert_eq!(
                mux.resolve_pane_id(912_001),
                Some((pane.domain_id(), original_window_id, target_tab.tab_id()))
            );
        }
        assert_eq!(
            mux.pane_location_full_scans.load(Ordering::Relaxed),
            scans_after_cold + 1,
            "one revision change must cause one exact refresh, not one global scan per keypress"
        );
        assert_eq!(
            mux.pane_location_scan_tab_probes.load(Ordering::Relaxed),
            probes_after_cold + UNRELATED_TABS + 1,
            "one revision refresh must be the only additional unrelated-tab census"
        );
        assert_eq!(
            mux.pane_location_cache_hits
                .load(Ordering::Relaxed)
                .saturating_sub(hits_before),
            STABLE_LOOKUPS - 1,
        );

        // A real topology transaction invalidates the revision fence.  The
        // next lookup must perform exactly one new census and cache the new
        // parent rather than returning the old window.
        let destination = mux.new_empty_window(Some("pane-location-scale".to_string()), None);
        let destination_window_id = *destination;
        mux.move_tab_between_windows(target_tab.tab_id(), destination_window_id, None)
            .expect("move target tab to a new exact parent");
        assert_eq!(
            mux.resolve_pane_id(912_001),
            Some((pane.domain_id(), destination_window_id, target_tab.tab_id()))
        );
        assert_eq!(
            mux.pane_location_full_scans.load(Ordering::Relaxed),
            scans_after_cold + 2,
            "one topology change should cause one cold refresh"
        );

        drop(destination);
        Mux::shutdown();
    }

    #[test]
    fn pane_location_cache_rejects_duplicate_exact_structural_owners() {
        let _guard = global_test_lock();
        Mux::shutdown();

        let mux = Arc::new(Mux::new(None));
        Mux::set_mux(&mux);
        let (pane, _kills) = KillCountingPane::new(912_002, test_size());
        let (first_tab, first_window_id) = register_attached_test_pane(&_guard, &mux, &pane);
        assert_eq!(
            mux.resolve_pane_id(912_002),
            Some((pane.domain_id(), first_window_id, first_tab.tab_id()))
        );
        assert!(mux.pane_location_cache.read().contains_key(&912_002));

        // Seed the malformed topology through the ordinary public surfaces:
        // an already-registered exact pane must never become a cacheable
        // HashMap-order-dependent owner merely because a second tab points at
        // it.
        let duplicate_tab = Arc::new(Tab::new(&test_size()));
        duplicate_tab.assign_pane(&pane);
        mux.add_tab_and_active_pane(&duplicate_tab)
            .expect("the duplicate-owner fixture reuses the existing pane registration");
        let duplicate_window = mux.new_empty_window(None, None);
        mux.add_tab_to_window(&duplicate_tab, *duplicate_window)
            .expect("attach duplicate-owner fixture");

        assert_eq!(mux.resolve_pane_id(912_002), None);
        assert!(mux.pane_location_cache.read().get(&912_002).is_none());

        drop(duplicate_window);
        Mux::shutdown();
    }

    #[test]
    fn membership_preserving_commits_skip_parent_index_writes_at_large_tab_counts() {
        for count in [1_024usize, 4_096, 16_384] {
            let mux = Arc::new(Mux::new(None));
            mux.bind_window_notification_owner();
            let window_count = count.div_ceil(MAX_TABS_PER_ORDERED_WINDOW);
            let mut seeded_windows = Vec::new();
            seeded_windows
                .try_reserve_exact(window_count)
                .expect("reserve membership-preserving scale windows");
            let mut remaining = count;
            while remaining != 0 {
                let window_tab_count = remaining.min(MAX_TABS_PER_ORDERED_WINDOW);
                let mut window = Window::new(Some("parent-write-scale".to_string()), None);
                let mut tabs = Vec::new();
                tabs.try_reserve_exact(window_tab_count)
                    .expect("reserve membership-preserving scale tabs");
                for _ in 0..window_tab_count {
                    tabs.push(Arc::new(Tab::new(&test_size())));
                }
                window
                    .seed_tabs_for_scale_test(&tabs)
                    .expect("seed membership-preserving scale window");
                seeded_windows.push(window);
                remaining -= window_tab_count;
            }
            let target_window_id = seeded_windows[0].window_id();
            let target_tab_count = seeded_windows[0].len();
            {
                let mut registered_tabs = mux.tabs.write();
                let mut windows = mux.windows.write();
                let mut parents = mux.tab_parents.write();
                registered_tabs
                    .try_reserve(count)
                    .expect("reserve scale tab registry");
                windows
                    .try_reserve(window_count)
                    .expect("reserve scale window registry");
                parents
                    .try_reserve(count)
                    .expect("reserve scale parent index");
                for window in &seeded_windows {
                    for tab in window.iter() {
                        assert!(registered_tabs
                            .insert(tab.tab_id(), Arc::clone(tab))
                            .is_none());
                        assert!(parents
                            .insert(
                                tab.tab_id(),
                                TabParentRegistration::new(tab, window.window_id()),
                            )
                            .is_none());
                    }
                }
                for window in seeded_windows {
                    assert!(windows.insert(window.window_id(), window).is_none());
                }
            }
            mux.assert_tab_parent_index_matches_windows();

            let reorder_tab_id = mux
                .get_window(target_window_id)
                .and_then(|window| window.get_by_idx(0).map(|tab| tab.tab_id()))
                .expect("target window retains its first tab");
            let writes_before = mux.tab_parent_write_cuts.load(Ordering::Relaxed);
            assert!(mux
                .activate_tab_at_index(target_window_id, target_tab_count - 1, false)
                .expect("large membership-preserving activation must commit"));
            mux.move_tab_between_windows(
                reorder_tab_id,
                target_window_id,
                Some(target_tab_count - 1),
            )
            .expect("large same-window reorder must commit");
            let parent_write_cuts = mux
                .tab_parent_write_cuts
                .load(Ordering::Relaxed)
                .saturating_sub(writes_before);
            assert_eq!(
                parent_write_cuts, 0,
                "active-only and same-window reorder commits must not acquire the parent-index write cut",
            );
            mux.assert_tab_parent_index_matches_windows();
            eprintln!(
                "mux_membership_preserving_work tab_count={count} parent_index_write_cuts={parent_write_cuts}"
            );
        }
    }

    #[test]
    fn tab_parent_index_matches_window_model_across_mutation_sequence() {
        let _guard = global_test_lock();
        Mux::shutdown();

        let mux = Arc::new(Mux::new(None));
        Mux::set_mux(&mux);
        let windows = (0..4)
            .map(|ordinal| mux.new_empty_window(Some(format!("parent-model-{ordinal}")), None))
            .collect::<Vec<_>>();
        let window_ids = windows.iter().map(|window| **window).collect::<Vec<_>>();
        let tabs = (0..32)
            .map(|ordinal| {
                let tab = Arc::new(Tab::new(&test_size()));
                mux.add_tab_no_panes(&tab).expect("register model tab");
                mux.add_tab_to_window(&tab, window_ids[ordinal % window_ids.len()])
                    .expect("attach model tab");
                tab
            })
            .collect::<Vec<_>>();
        let mut expected_parent = tabs
            .iter()
            .enumerate()
            .map(|(ordinal, tab)| (tab.tab_id(), window_ids[ordinal % window_ids.len()]))
            .collect::<HashMap<_, _>>();

        for step in 0..256usize {
            let tab = &tabs[(step.saturating_mul(17).saturating_add(3)) % tabs.len()];
            let destination =
                window_ids[(step.saturating_mul(13).saturating_add(1)) % window_ids.len()];
            mux.move_tab_between_windows(tab.tab_id(), destination, None)
                .expect("model move must preserve exact parent authority");
            expected_parent.insert(tab.tab_id(), destination);
            mux.assert_tab_parent_index_matches_windows();
            for candidate in &tabs {
                assert_eq!(
                    mux.window_containing_tab(candidate.tab_id()),
                    expected_parent.get(&candidate.tab_id()).copied(),
                    "parent model diverged after step {step} for tab {}",
                    candidate.tab_id(),
                );
            }
        }

        drop(windows);
        Mux::shutdown();
    }

    #[test]
    fn stale_tab_parent_index_cannot_authorize_transfer_from_absent_membership() {
        let _guard = global_test_lock();
        Mux::shutdown();

        let mux = Arc::new(Mux::new(None));
        Mux::set_mux(&mux);
        let source = mux.new_empty_window(Some("stale-parent-source".to_string()), None);
        let source_id = *source;
        let destination = mux.new_empty_window(Some("stale-parent-destination".to_string()), None);
        let destination_id = *destination;
        let (tab, _kills) = tab_with_kill_counter(&mux, 455);
        mux.add_tab_to_window(&tab, source_id)
            .expect("tab should acquire its source parent");

        // Seed a legacy-corrupt cut: exact indexed authority still names the
        // source, but the source membership has disappeared. Merely including
        // that window in a prepared transaction must not launder the stale
        // index into a valid destination attachment.
        mux.get_window_mut(source_id)
            .expect("source remains registered")
            .remove_by_id(tab.tab_id());
        let destination_before = mux
            .window_order_snapshot(destination_id)
            .expect("valid destination")
            .expect("registered destination");
        let error = {
            let mut windows = mux.windows.write();
            let source_state = windows
                .get(&source_id)
                .expect("source remains registered")
                .prepare_retirement_marker()
                .expect("prepare empty source state");
            let destination_state = windows
                .get(&destination_id)
                .expect("destination remains registered")
                .prepare_insert(0, &tab)
                .expect("prepare destination insertion");
            mux.commit_prepared_window_states_locked(
                &mut windows,
                vec![
                    (source_id, source_state),
                    (destination_id, destination_state),
                ],
                vec![(tab.tab_id(), destination_id)],
                Vec::new(),
                Vec::new(),
            )
            .expect_err("stale indexed membership must fail before commit")
        };
        assert!(
            error
                .to_string()
                .contains("different or untouched window parent"),
            "unexpected stale-parent error: {error:#}",
            error = error,
        );
        let destination_after = mux
            .window_order_snapshot(destination_id)
            .expect("valid destination")
            .expect("registered destination");
        assert_eq!(
            destination_after.ordered_tab_ids().collect::<Vec<_>>(),
            destination_before.ordered_tab_ids().collect::<Vec<_>>(),
            "failed stale-parent admission must leave the destination unchanged",
        );
        assert_eq!(
            mux.window_containing_tab(tab.tab_id()),
            Some(source_id),
            "the failed transaction must not rewrite indexed authority",
        );

        drop(destination);
        drop(source);
        Mux::shutdown();
    }

    #[test]
    fn remove_tab_local_only_drops_mirror_without_killing_pane() {
        let _guard = global_test_lock();
        Mux::shutdown();

        let mux = Arc::new(Mux::new(None));
        Mux::set_mux(&mux);
        let local_only_window = mux.new_empty_window(Some("winunify".to_string()), None);
        let local_only_window_id = *local_only_window;
        let (local_only_tab, local_only_kills) = tab_with_kill_counter(&mux, 501);
        let local_only_tab_id = local_only_tab.tab_id();
        mux.add_tab_to_window(&local_only_tab, local_only_window_id)
            .expect("local-only tab should be attached to a window");

        let normal_window = mux.new_empty_window(Some("winunify".to_string()), None);
        let normal_window_id = *normal_window;
        let (normal_tab, normal_kills) = tab_with_kill_counter(&mux, 502);
        let normal_tab_id = normal_tab.tab_id();
        mux.add_tab_to_window(&normal_tab, normal_window_id)
            .expect("normal tab should be attached to a window");

        let removed = mux
            .remove_tab_local_only(local_only_tab_id)
            .expect("local-only tab should be removed");
        assert!(Arc::ptr_eq(&removed, &local_only_tab));
        assert!(mux.get_tab(local_only_tab_id).is_none());
        assert!(mux.get_pane(501).is_none());
        assert!(mux.window_containing_tab(local_only_tab_id).is_none());
        assert!(!mux.tab_parents.read().contains_key(&local_only_tab_id));
        mux.assert_tab_parent_index_matches_windows();
        assert_eq!(
            local_only_kills.load(Ordering::SeqCst),
            0,
            "local-only tab removal must not call Pane::kill / Pdu::KillPane path",
        );

        let normal_removed = mux
            .remove_tab(normal_tab_id)
            .expect("normal tab should be removed");
        assert!(Arc::ptr_eq(&normal_removed, &normal_tab));
        assert!(mux.window_containing_tab(normal_tab_id).is_none());
        assert!(!mux.tab_parents.read().contains_key(&normal_tab_id));
        mux.assert_tab_parent_index_matches_windows();
        assert_eq!(
            normal_kills.load(Ordering::SeqCst),
            1,
            "ordinary tab removal remains the killing path",
        );

        drop(normal_window);
        drop(local_only_window);
        Mux::shutdown();
    }

    #[test]
    fn remove_tab_local_only_if_same_prunes_published_window_before_stale_builder_cancel() {
        let _guard = global_test_lock();
        Mux::shutdown();

        let mux = Arc::new(Mux::new(None));
        Mux::set_mux(&mux);
        let window = mux.new_empty_window(Some("ordered-staging".to_string()), None);
        let window_id = *window;
        let (tab, kills) = tab_with_kill_counter(&mux, 503);
        let tab_id = tab.tab_id();
        mux.add_tab_to_window(&tab, window_id)
            .expect("populated staging tab should attach");

        assert!(mux.remove_tab_local_only_if_same(&tab));
        assert!(mux.get_tab(tab_id).is_none());
        assert!(mux.get_pane(503).is_none());
        assert!(mux.window_containing_tab(tab_id).is_none());
        assert!(!mux.tab_parents.read().contains_key(&tab_id));
        mux.assert_tab_parent_index_matches_windows();
        assert_eq!(
            kills.load(Ordering::SeqCst),
            0,
            "transaction rollback must not send a remote pane kill",
        );
        assert!(
            mux.get_window(window_id).is_none(),
            "exact tab rollback must synchronously prune the published window it emptied",
        );
        assert!(
            !mux.remove_tab_local_only_if_same(&tab),
            "a stale exact rollback handle must be inert",
        );

        window.cancel();
        assert!(
            mux.get_window(window_id).is_none(),
            "the stale builder must remain inert after exact published-window cleanup",
        );
        Mux::shutdown();
    }

    #[test]
    fn remove_empty_tab_local_only_if_same_rolls_back_exact_staging() {
        let _guard = global_test_lock();
        Mux::shutdown();

        let mux = Arc::new(Mux::new(None));
        Mux::set_mux(&mux);
        let window = mux.new_empty_window(Some("ordered-staging".to_string()), None);
        let window_id = *window;
        drop(window);
        let unrelated_window =
            mux.new_empty_window(Some("unrelated-empty-window".to_string()), None);
        let unrelated_window_id = *unrelated_window;
        drop(unrelated_window);
        let tab = Arc::new(Tab::new(&test_size()));
        let tab_id = tab.tab_id();
        mux.add_tab_no_panes(&tab)
            .expect("empty staging tab should register");
        mux.add_tab_to_window(&tab, window_id)
            .expect("empty staging tab should attach");

        assert!(mux.remove_empty_tab_local_only_if_same(&tab));
        assert!(mux.get_tab(tab_id).is_none());
        assert!(mux.get_window(window_id).is_none());
        assert!(
            mux.get_window(unrelated_window_id).is_some(),
            "exact rollback must not prune a pre-existing unrelated empty window"
        );
        assert!(
            !mux.remove_empty_tab_local_only_if_same(&tab),
            "a stale exact staging handle must be inert"
        );

        Mux::shutdown();
    }

    #[test]
    fn detached_domain_is_removed_from_domain_maps() {
        let default_domain: Arc<dyn Domain> =
            Arc::new(domain::LocalDomain::new("default-test-domain").unwrap());
        let mux = Mux::new(Some(Arc::clone(&default_domain)));

        let detached_domain: Arc<dyn Domain> =
            Arc::new(domain::LocalDomain::new("detached-test-domain").unwrap());
        let detached_id = detached_domain.domain_id();
        let detached_name = detached_domain.domain_name().to_string();

        mux.add_domain(&detached_domain)
            .expect("detached test domain should register");
        assert!(mux.get_domain(detached_id).is_some());
        assert!(mux.get_domain_by_name(&detached_name).is_some());

        mux.domain_was_detached(detached_id);

        assert!(mux.get_domain(detached_id).is_none());
        assert!(mux.get_domain_by_name(&detached_name).is_none());
        assert!(mux.get_domain(default_domain.domain_id()).is_some());
    }

    #[test]
    fn detaching_default_domain_promotes_remaining_domain() {
        let default_domain: Arc<dyn Domain> =
            Arc::new(domain::LocalDomain::new("default-domain-to-detach").unwrap());
        let mux = Mux::new(Some(Arc::clone(&default_domain)));

        let replacement_domain: Arc<dyn Domain> =
            Arc::new(domain::LocalDomain::new("replacement-domain").unwrap());
        let replacement_id = replacement_domain.domain_id();
        mux.add_domain(&replacement_domain)
            .expect("replacement test domain should register");

        mux.domain_was_detached(default_domain.domain_id());

        assert!(mux.get_domain(default_domain.domain_id()).is_none());
        assert!(mux.get_domain(replacement_id).is_some());
        assert_eq!(mux.default_domain().domain_id(), replacement_id);
    }

    #[test]
    fn detaching_tmux_domain_eagerly_removes_notification_subscriber() {
        let default_domain: Arc<dyn Domain> =
            Arc::new(domain::LocalDomain::new("default-domain-tmux-detach-test").unwrap());
        let mux = Mux::new(Some(default_domain));

        let tmux_domain =
            Arc::new(TmuxDomain::new(0).expect("start tmux test domain I/O supervisor"));
        let tmux_domain_dyn: Arc<dyn Domain> = tmux_domain.clone();
        let tmux_domain_id = tmux_domain_dyn.domain_id();
        mux.add_domain(&tmux_domain_dyn)
            .expect("tmux test domain should register");

        let sub_id = mux
            .subscribe(|_| true)
            .expect("test mux subscription should allocate an identifier");
        *tmux_domain.inner.notification_sub_id.lock() = Some(sub_id);

        mux.domain_was_detached(tmux_domain_id);

        assert!(mux.get_domain(tmux_domain_id).is_none());
        assert!(
            !mux.unsubscribe(sub_id),
            "tmux notification subscriber should be removed eagerly on detach"
        );
        assert!(tmux_domain.inner.notification_sub_id.lock().is_none());
    }

    #[test]
    fn add_domain_rejects_live_same_name_domain_without_half_detach() {
        let default_domain: Arc<dyn Domain> =
            Arc::new(domain::LocalDomain::new("default-domain-add-domain-test").unwrap());
        let mux = Mux::new(Some(default_domain));

        let first: Arc<dyn Domain> =
            Arc::new(domain::LocalDomain::new("duplicate-name-domain").unwrap());
        let second: Arc<dyn Domain> =
            Arc::new(domain::LocalDomain::new("duplicate-name-domain").unwrap());
        let first_id = first.domain_id();
        let second_id = second.domain_id();

        mux.add_domain(&first)
            .expect("first duplicate-name test domain should register");
        assert!(mux.get_domain(first_id).is_some());
        assert_eq!(
            mux.get_domain_by_name("duplicate-name-domain")
                .unwrap()
                .domain_id(),
            first_id
        );

        let error = mux
            .add_domain(&second)
            .expect_err("a live same-name domain must not be silently half-detached");

        assert_eq!(
            error,
            DomainRegistrationError::NameInUse {
                domain_name: "duplicate-name-domain".to_string(),
                registered_id: first_id,
                requested_id: second_id,
            }
        );
        assert!(mux.get_domain(first_id).is_some());
        assert!(mux.get_domain(second_id).is_none());
        assert_eq!(
            mux.get_domain_by_name("duplicate-name-domain")
                .unwrap()
                .domain_id(),
            first_id
        );
        assert!(
            !mux.retired_domain_ids.lock().contains(&first_id),
            "registration rejection must not retire or strand the live domain",
        );
    }

    #[test]
    fn mux_notification_tab_title_changed() {
        let n = MuxNotification::TabTitleChanged {
            tab_id: 3,
            title: "new title".to_string(),
        };
        let n2 = n.clone();
        let dbg = format!("{:?}", n2);
        assert!(dbg.contains("TabTitleChanged"));
        assert!(dbg.contains("new title"));
    }

    #[test]
    fn mux_notification_window_title_changed() {
        let n = MuxNotification::WindowTitleChanged {
            window_id: 1,
            title: "window title".to_string(),
        };
        let dbg = format!("{:?}", n);
        assert!(dbg.contains("WindowTitleChanged"));
    }

    #[test]
    fn mux_notification_workspace_renamed() {
        let n = MuxNotification::WorkspaceRenamed {
            old_workspace: "old".to_string(),
            new_workspace: "new".to_string(),
        };
        let dbg = format!("{:?}", n);
        assert!(dbg.contains("WorkspaceRenamed"));
        assert!(dbg.contains("old"));
        assert!(dbg.contains("new"));
    }

    #[test]
    fn mux_notification_pane_focused() {
        let n = MuxNotification::PaneFocused(7);
        let dbg = format!("{:?}", n);
        assert!(dbg.contains("PaneFocused"));
        assert!(dbg.contains("7"));
    }

    #[test]
    fn mux_notification_tab_resized() {
        let n = MuxNotification::TabResized(2);
        let dbg = format!("{:?}", n);
        assert!(dbg.contains("TabResized"));
    }

    #[test]
    fn mux_notification_save_to_downloads() {
        let n = MuxNotification::SaveToDownloads {
            name: Some("file.txt".to_string()),
            data: Arc::new(vec![1, 2, 3]),
        };
        let n2 = n.clone();
        let dbg = format!("{:?}", n2);
        assert!(dbg.contains("SaveToDownloads"));
        assert!(dbg.contains("file.txt"));
    }

    #[test]
    fn mux_notification_tab_added_to_window() {
        let n = MuxNotification::TabAddedToWindow {
            tab_id: 1,
            window_id: 2,
        };
        let dbg = format!("{:?}", n);
        assert!(dbg.contains("TabAddedToWindow"));
    }

    #[test]
    fn session_terminated_window_closed_display() {
        let err = SessionTerminated::WindowClosed;
        assert_eq!(format!("{}", err), "Window Closed");
    }

    #[test]
    fn session_terminated_window_closed_debug() {
        let err = SessionTerminated::WindowClosed;
        let dbg = format!("{:?}", err);
        assert!(dbg.contains("WindowClosed"));
    }

    #[test]
    fn session_terminated_is_error() {
        let err = SessionTerminated::WindowClosed;
        let error: &dyn std::error::Error = &err;
        assert_eq!(error.to_string(), "Window Closed");
    }

    #[test]
    fn terminal_size_to_pty_size_basic() {
        let size = TerminalSize {
            rows: 24,
            cols: 80,
            pixel_width: 800,
            pixel_height: 600,
            dpi: 96,
        };
        let pty_size = terminal_size_to_pty_size(size).unwrap();
        assert_eq!(pty_size.rows, 24);
        assert_eq!(pty_size.cols, 80);
        assert_eq!(pty_size.pixel_width, 800);
        assert_eq!(pty_size.pixel_height, 600);
    }

    #[test]
    fn terminal_size_to_pty_size_zero() {
        let size = TerminalSize {
            rows: 0,
            cols: 0,
            pixel_width: 0,
            pixel_height: 0,
            dpi: 0,
        };
        let pty_size = terminal_size_to_pty_size(size).unwrap();
        assert_eq!(pty_size.rows, 0);
        assert_eq!(pty_size.cols, 0);
    }

    #[test]
    fn panicking_subscriber_is_removed_and_does_not_poison_others() {
        let mux = Mux::new(None);
        let healthy_count = Arc::new(AtomicUsize::new(0));
        let observed = Arc::clone(&healthy_count);

        // Panicking subscriber
        mux.subscribe(move |_| {
            panic!("intentional test panic in subscriber");
        })
        .expect("test mux subscription should allocate an identifier");

        // Healthy subscriber registered after the panicker
        mux.subscribe(move |_| {
            observed.fetch_add(1, Ordering::Relaxed);
            true
        })
        .expect("test mux subscription should allocate an identifier");

        // First dispatch: panicker fires and is removed, healthy fires
        mux.dispatch_notification(MuxNotification::Empty);
        assert_eq!(healthy_count.load(Ordering::Relaxed), 1);

        // Second dispatch: panicker is gone, only healthy fires
        mux.dispatch_notification(MuxNotification::Empty);
        assert_eq!(healthy_count.load(Ordering::Relaxed), 2);

        // Only the healthy subscriber remains
        assert_eq!(mux.subscribers.read().len(), 1);
    }
}
